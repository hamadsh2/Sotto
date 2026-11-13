// SPDX-License-Identifier: GPL-3.0-only
//! "Tune for my room": measures the room and the voice, then suggests a noise-gate
//! threshold and an auto-gain limit. Nothing is changed until the user applies it.
//!
//! Two steps of `STEP` seconds each:
//! 1. **Quiet** — the user stays silent with their usual background on. The worker
//!    records the gate-detector level of every frame (after denoise and EQ, which is
//!    exactly what the gate compares with its threshold), giving the noise left over.
//! 2. **Talk** — the user speaks normally. Frames clearly above that noise count as
//!    voice; their RMS gives the voice level.
//!
//! The audio thread only bumps atomic counters in `Stats` (see `Stats::record_chain`);
//! this module runs on the main thread. `suggest` is a pure function of the two
//! histograms, so it is tested without any audio.
//!
//! Python analogy: `Session` is a small state machine object you call `.tick()` on from
//! a timer; `suggest` is like a function over two `numpy.histogram` results.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::dsp::AGC_TARGET_DB;
use crate::params::{AGC_MAX_DB_MAX, AGC_MAX_DB_MIN, GATE_DB_MAX, GATE_DB_MIN};
use crate::stats::{Stats, TUNE_BINS};

/// Length of each step.
pub const STEP: Duration = Duration::from_secs(5);
/// The gate opens at least this far above the noise that's left…
const GATE_ABOVE_NOISE_DB: f32 = 10.0;
/// …and at least this close under the voice: clicks and other short sounds that slip
/// past the denoiser are usually quieter than that, so the gate still catches them
/// in a room the denoiser makes almost silent.
const GATE_UNDER_VOICE_DB: f32 = 18.0;
/// It never opens above the quieter part of the voice minus this.
const GATE_BELOW_VOICE_DB: f32 = 6.0;
/// Frames within this range of the loudest speech count as active speech; word tails
/// and breaths below it would make the voice look quieter than it is (the same idea
/// as the "active speech level" of ITU-T P.56).
const ACTIVE_RANGE_DB: f32 = 15.0;
/// Frames this far above the noise count as voice in the talk step.
const VOICE_ABOVE_NOISE_DB: f32 = 6.0;
/// Less than this gap between noise and voice: the room is too loud to separate well.
const NOISY_GAP_DB: f32 = 12.0;
/// At least this much speech (frames of 10 ms) is needed for a result: 1 s.
const MIN_VOICE_FRAMES: u32 = 100;
/// More clipped frames than this share of the voice: the mic level is too high.
const CLIP_SHARE: f32 = 0.01;

/// What a finished measurement suggests.
#[derive(Clone, Debug, PartialEq)]
pub struct Suggestion {
    /// Noise left after denoise (95th percentile of the quiet step), dB.
    pub noise_db: f32,
    /// Active speech level (power average of the talk step's louder speech), dB.
    pub voice_db: f32,
    /// Suggested gate threshold, dB (whole numbers).
    pub gate_db: f32,
    /// Suggested auto-gain limit, dB (whole numbers).
    pub agc_max_db: f32,
    /// Noise and voice are close: the gate can't separate them cleanly.
    pub noisy: bool,
    /// The voice needs more boost than the auto gain allows: the mic is set too low.
    pub too_quiet: bool,
    /// The mic clipped while talking: its level is set too high.
    pub clipping: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Failure {
    /// No audio arrived at all (the mic isn't running).
    NoAudio,
    /// Not enough speech in the talk step.
    NoVoice,
}

impl Failure {
    pub fn message(&self) -> &'static str {
        match self {
            Failure::NoAudio => "No sound came from your microphone. Check that it's running.",
            Failure::NoVoice => {
                "Sotto didn't hear enough of your voice. Try again and keep talking."
            }
        }
    }
}

/// The level (dB, bin upper edge) at or below which `share` of the counts fall.
/// `None` for an empty histogram.
fn percentile(h: &[u32; TUNE_BINS], share: f32) -> Option<f32> {
    let total: u64 = h.iter().map(|&c| c as u64).sum();
    if total == 0 {
        return None;
    }
    let want = ((total as f64) * share as f64).ceil().max(1.0) as u64;
    let mut seen = 0u64;
    for (i, &c) in h.iter().enumerate() {
        seen += c as u64;
        if seen >= want {
            return Some(bin_top(i));
        }
    }
    Some(0.0)
}

/// The upper edge of bin `i`, dB.
fn bin_top(i: usize) -> f32 {
    i as f32 - 100.0 + 1.0
}

/// Average power of a histogram (each bin at its centre), in dB.
fn power_mean_db(h: &[u32; TUNE_BINS]) -> Option<f32> {
    let (mut sum, mut n) = (0.0f64, 0u64);
    for (i, &c) in h.iter().enumerate() {
        let centre = bin_top(i) as f64 - 0.5;
        sum += c as f64 * 10f64.powf(centre / 10.0);
        n += c as u64;
    }
    (n > 0).then(|| (10.0 * (sum / n as f64).log10()) as f32)
}

/// Where the talk step should start counting voice, from the quiet step's histogram.
pub fn voice_threshold(quiet: &[u32; TUNE_BINS]) -> Option<f32> {
    percentile(quiet, 0.95).map(|n| n + VOICE_ABOVE_NOISE_DB)
}

/// The suggestion from the quiet step's level histogram, the talk step's voice-RMS
/// histogram, and how many voice and clipped frames the talk step had.
pub fn suggest(
    quiet: &[u32; TUNE_BINS],
    voice: &[u32; TUNE_BINS],
    clipped_frames: u64,
) -> Result<Suggestion, Failure> {
    let noise_db = percentile(quiet, 0.95).ok_or(Failure::NoAudio)?;
    let voice_frames: u32 = voice.iter().sum();
    if voice_frames < MIN_VOICE_FRAMES {
        return Err(Failure::NoVoice);
    }
    // Active speech: the bins within ACTIVE_RANGE_DB of the loud end (Some: the
    // histogram isn't empty).
    let loud = percentile(voice, 0.95).unwrap_or(noise_db);
    let mut active = [0u32; TUNE_BINS];
    for (i, (&c, a)) in voice.iter().zip(active.iter_mut()).enumerate() {
        if bin_top(i) > loud - ACTIVE_RANGE_DB {
            *a = c;
        }
    }
    let voice_db = power_mean_db(&active).unwrap_or(loud);
    let voice_low = percentile(&active, 0.1).unwrap_or(voice_db);

    let wanted = (noise_db + GATE_ABOVE_NOISE_DB).max(voice_db - GATE_UNDER_VOICE_DB);
    // Never above the quieter speech: cutting words is worse than letting some noise
    // through.
    let gate = wanted.min(voice_low - GATE_BELOW_VOICE_DB);
    let gate_db = gate.round().clamp(GATE_DB_MIN, GATE_DB_MAX);
    let noisy = voice_db - noise_db < NOISY_GAP_DB;

    let needed = AGC_TARGET_DB - voice_db;
    let agc_max_db = needed.round().clamp(AGC_MAX_DB_MIN, AGC_MAX_DB_MAX);
    Ok(Suggestion {
        noise_db,
        voice_db,
        gate_db,
        agc_max_db,
        noisy,
        too_quiet: needed > AGC_MAX_DB_MAX,
        clipping: clipped_frames as f32 > voice_frames as f32 * CLIP_SHARE,
    })
}

#[derive(Clone, Debug, PartialEq)]
pub enum Phase {
    Quiet,
    Talk,
    Done(Result<Suggestion, Failure>),
}

/// One measurement in progress. Dropping it stops the measuring.
pub struct Session {
    stats: Arc<Stats>,
    phase: Phase,
    step_started: Instant,
    quiet: [u32; TUNE_BINS],
    clips_at_talk: u64,
}

impl Session {
    pub fn start(stats: Arc<Stats>) -> Self {
        // Nothing counts as voice in the quiet step.
        stats.tune_begin(f32::INFINITY);
        Self {
            stats,
            phase: Phase::Quiet,
            step_started: Instant::now(),
            quiet: [0; TUNE_BINS],
            clips_at_talk: 0,
        }
    }

    pub fn phase(&self) -> &Phase {
        &self.phase
    }

    /// 0..=1 through the current step (1 when done).
    pub fn progress(&self) -> f32 {
        match self.phase {
            Phase::Done(_) => 1.0,
            _ => (self.step_started.elapsed().as_secs_f32() / STEP.as_secs_f32()).min(1.0),
        }
    }

    /// Move on when a step's time is up. Call it a few times a second.
    pub fn tick(&mut self) -> &Phase {
        if self.step_started.elapsed() < STEP {
            return &self.phase;
        }
        match self.phase {
            Phase::Quiet => {
                self.quiet = self.stats.tune_take().0;
                match voice_threshold(&self.quiet) {
                    Some(min) => {
                        self.stats.tune_begin(min);
                        self.clips_at_talk = self.clip_count();
                        self.phase = Phase::Talk;
                        self.step_started = Instant::now();
                    }
                    None => self.finish(Err(Failure::NoAudio)),
                }
            }
            Phase::Talk => {
                let (_, voice) = self.stats.tune_take();
                let clips = self.clip_count().saturating_sub(self.clips_at_talk);
                self.finish(suggest(&self.quiet, &voice, clips));
            }
            Phase::Done(_) => {}
        }
        &self.phase
    }

    fn clip_count(&self) -> u64 {
        self.stats
            .clip_frames
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    fn finish(&mut self, r: Result<Suggestion, Failure>) {
        self.stats.tune_end();
        self.phase = Phase::Done(r);
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.stats.tune_end();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stats::tune_bin;

    /// A histogram with `n` frames at each given level.
    fn hist(levels: &[(f32, u32)]) -> [u32; TUNE_BINS] {
        let mut h = [0; TUNE_BINS];
        for &(db, n) in levels {
            h[tune_bin(db)] += n;
        }
        h
    }

    #[test]
    fn quiet_room_and_clear_voice() {
        // Noise around −70 dB; speech mostly −26…−22 dB, with word tails and breaths
        // down to −50 that must not drag the level down.
        let quiet = hist(&[(-72.0, 300), (-70.0, 170), (-68.5, 30)]);
        let voice = hist(&[
            (-49.5, 80),
            (-44.5, 80),
            (-38.5, 60),
            (-25.5, 150),
            (-23.5, 150),
            (-21.5, 60),
        ]);
        let s = suggest(&quiet, &voice, 0).unwrap();
        assert_eq!(s.noise_db, -68.0);
        assert!((s.voice_db - -23.9).abs() < 0.2, "{}", s.voice_db);
        // 18 dB under the voice, well above the noise.
        assert_eq!(s.gate_db, -42.0);
        // Only what the voice needs to reach −23 dB.
        assert_eq!(s.agc_max_db, 1.0);
        assert!(!s.noisy && !s.too_quiet && !s.clipping);
    }

    #[test]
    fn an_almost_silent_room_still_gets_a_useful_gate() {
        // High quality can leave nearly nothing: the gate follows the voice instead of
        // dropping to −80 dB, so clicks that slip through are still cut.
        let quiet = hist(&[(-99.5, 500)]);
        let voice = hist(&[(-28.5, 300)]);
        let s = suggest(&quiet, &voice, 0).unwrap();
        assert_eq!(s.gate_db, -47.0); // −28.5 − 18, rounded
        assert_eq!(s.agc_max_db, 6.0); // −23 − (−28.5), rounded
    }

    #[test]
    fn noisy_room_keeps_the_gate_under_the_voice() {
        let quiet = hist(&[(-40.5, 500)]); // loud leftover noise
        let voice = hist(&[(-34.5, 100), (-30.5, 300)]);
        let s = suggest(&quiet, &voice, 0).unwrap();
        assert!(s.noisy);
        // Not 10 dB above the noise (−30, inside the voice): 6 dB under its quieter
        // part (−34).
        assert_eq!(s.gate_db, -40.0);
    }

    #[test]
    fn values_stay_in_the_sliders_range() {
        let quiet = hist(&[(-99.5, 500)]); // digital silence
        let voice = hist(&[(-60.5, 200)]); // very quiet mic
        let s = suggest(&quiet, &voice, 0).unwrap();
        assert_eq!(s.gate_db, -79.0); // follows the voice down, inside the range
        assert_eq!(s.agc_max_db, AGC_MAX_DB_MAX);
        assert!(s.too_quiet);

        let loud = hist(&[(-8.5, 200)]);
        let s = suggest(&quiet, &loud, 0).unwrap();
        assert_eq!(s.agc_max_db, AGC_MAX_DB_MIN);
        assert!(s.gate_db <= GATE_DB_MAX);
    }

    #[test]
    fn clipping_is_reported() {
        let quiet = hist(&[(-70.5, 500)]);
        let voice = hist(&[(-12.5, 400)]);
        assert!(suggest(&quiet, &voice, 10).unwrap().clipping);
        assert!(!suggest(&quiet, &voice, 2).unwrap().clipping);
    }

    #[test]
    fn missing_audio_or_voice_is_a_failure() {
        let empty = [0; TUNE_BINS];
        let quiet = hist(&[(-70.5, 500)]);
        assert_eq!(suggest(&empty, &empty, 0), Err(Failure::NoAudio));
        assert_eq!(
            suggest(&quiet, &hist(&[(-30.5, 50)]), 0),
            Err(Failure::NoVoice)
        );
    }

    #[test]
    fn voice_starts_6_db_above_the_noise() {
        assert_eq!(voice_threshold(&hist(&[(-70.5, 100)])), Some(-64.0));
        assert_eq!(voice_threshold(&[0; TUNE_BINS]), None);
    }
}
