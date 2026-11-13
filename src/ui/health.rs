// SPDX-License-Identifier: GPL-3.0-only
//! Mic health hints: notices when the microphone is set too loud (it clips) or too
//! quiet (even the auto gain can't bring the voice up), so the window can say so
//! calmly. Sotto never changes the mic level itself; the hint says where to do it.
//!
//! It only reads counters the audio thread already keeps (`Stats`), on the window's
//! refresh timer while the window is visible. Pure logic: tested without audio.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use sotto::dsp::AGC_TARGET_DB;

/// Clipping counts over this much recent time…
const CLIP_WINDOW: Duration = Duration::from_secs(10);
/// …and this many clipped frames (10 ms each) in it shows the hint.
const CLIP_FRAMES: u64 = 3;
/// "Too quiet" needs this much speech in a row (frames of 10 ms): 10 s.
const QUIET_SPEECH_FRAMES: u64 = 1000;
/// Too quiet: the voice still lands this far under the auto gain's target after the
/// most boost allowed.
const QUIET_SHORT_DB: f32 = 8.0;

#[derive(Default)]
pub struct Health {
    /// Counter values at the last update; `None` until the first one.
    last: Option<(u64, u64)>,
    /// Recent clipped frames: (when, how many).
    clips: VecDeque<(Instant, u64)>,
    /// Speech in a row that was too quiet, frames.
    quiet_frames: u64,
}

/// What to show.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Hints {
    pub too_loud: bool,
    pub too_quiet: bool,
}

impl Health {
    /// Start over (the window was hidden, or the audio restarted).
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Feed the latest counters: total clipped frames and speech frames, the auto
    /// gain's measured voice level (dB) and the most boost it may add (0 when off).
    pub fn update(
        &mut self,
        now: Instant,
        clip_total: u64,
        speech_total: u64,
        voice_db: f32,
        boost_db: f32,
    ) -> Hints {
        let (clips, speech) = match self.last.replace((clip_total, speech_total)) {
            // A counter that went back (new streams) counts as a fresh start.
            Some((c, s)) if clip_total >= c && speech_total >= s => {
                (clip_total - c, speech_total - s)
            }
            _ => (0, 0),
        };
        if clips > 0 {
            self.clips.push_back((now, clips));
        }
        while self
            .clips
            .front()
            .is_some_and(|(t, _)| now.duration_since(*t) > CLIP_WINDOW)
        {
            self.clips.pop_front();
        }
        if speech > 0 {
            if voice_db.is_finite() && voice_db + boost_db < AGC_TARGET_DB - QUIET_SHORT_DB {
                self.quiet_frames += speech;
            } else {
                self.quiet_frames = 0;
            }
        }
        Hints {
            too_loud: self.clips.iter().map(|(_, n)| n).sum::<u64>() >= CLIP_FRAMES,
            too_quiet: self.quiet_frames >= QUIET_SPEECH_FRAMES,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipping_shows_then_fades_after_ten_seconds() {
        let t0 = Instant::now();
        let mut h = Health::default();
        assert!(!h.update(t0, 0, 0, -20.0, 12.0).too_loud);
        assert!(!h.update(t0, 2, 0, -20.0, 12.0).too_loud);
        assert!(
            h.update(t0 + Duration::from_secs(1), 4, 0, -20.0, 12.0)
                .too_loud
        );
        let later = t0 + Duration::from_secs(12);
        assert!(!h.update(later, 4, 0, -20.0, 12.0).too_loud);
    }

    #[test]
    fn quiet_needs_ten_seconds_of_quiet_speech() {
        let t = Instant::now();
        let mut h = Health::default();
        h.update(t, 0, 0, -45.0, 12.0);
        // −45 + 12 = −33, more than 8 dB under −23: quiet.
        assert!(!h.update(t, 0, 500, -45.0, 12.0).too_quiet);
        assert!(h.update(t, 0, 1000, -45.0, 12.0).too_quiet);
        // Loud enough speech clears it at once.
        assert!(!h.update(t, 0, 1100, -30.0, 12.0).too_quiet);
        // Silence (no new speech) changes nothing.
        assert!(!h.update(t, 0, 1100, -60.0, 0.0).too_quiet);
    }

    #[test]
    fn auto_gain_off_counts_as_no_boost() {
        let t = Instant::now();
        let mut h = Health::default();
        h.update(t, 0, 0, -33.0, 0.0);
        assert!(h.update(t, 0, 1000, -33.0, 0.0).too_quiet);
        let mut h = Health::default();
        h.update(t, 0, 0, -30.0, 12.0);
        assert!(!h.update(t, 0, 1000, -30.0, 12.0).too_quiet);
    }

    #[test]
    fn counters_going_back_start_fresh() {
        let t = Instant::now();
        let mut h = Health::default();
        h.update(t, 100, 100, -20.0, 12.0);
        assert_eq!(h.update(t, 0, 0, -20.0, 12.0), Hints::default());
    }
}
