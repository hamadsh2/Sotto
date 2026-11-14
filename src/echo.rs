// SPDX-License-Identifier: GPL-3.0-only
//! Echo removal for open-back headphones.
//!
//! Open-back headphones leak what you hear (game, friends' voices, music) into the mic.
//! To remove that leak, the canceller needs to know what was played. Sotto listens to
//! the headphones device the user picked through a **read-only WASAPI loopback capture**
//! (it only reads the mix Windows sends there; nothing is ever played to that device and
//! none of its settings change) and hands that "reference" to WebRTC's AEC3 echo
//! canceller (the `sonora` crate, a pure-Rust port). AEC3 learns the path from the
//! headphones to the mic (delay, loudness, room) and subtracts it.
//!
//! ```text
//!  headphones loopback ─(cpal thread)─► LoopbackTap: downmix to mono ─► ref ring ─┐
//!                                                                                 ▼
//!  mic frame (after high-pass) ──► EchoRef::process (AEC3, on the worker) ──► denoise
//! ```
//!
//! The reference stays at the device's own rate (44.1, 48, 96 kHz, …); AEC3 resamples
//! it internally. Windows sends no loopback packets while nothing plays, so an empty
//! ring simply means "silence" and AEC3 gets zeros.
//!
//! Python analogy: `LoopbackTap` is the producer side of a `queue.Queue` filled from the
//! audio library's thread; `EchoRef` is the consumer that takes 10 ms at a time.

use ringbuf::traits::{Consumer, Observer, Producer};
use ringbuf::{HeapCons, HeapProd};
use sonora::config::EchoCanceller;
use sonora::{AudioProcessing, Config, StreamConfig};

use crate::denoise::FRAME;
use crate::engine::SAMPLE_RATE;

/// Ring capacity: 200 ms of reference at the device rate (a hard ceiling).
pub fn ring_capacity(rate: u32) -> usize {
    rate as usize / 5
}

/// Reference frames kept queued when (re)starting, so normal thread jitter between the
/// loopback and mic callbacks doesn't starve AEC3 every other frame.
const START_FRAMES: usize = 2;
/// More than this queued (the headphones' clock runs a little faster than the mic's)
/// → drop the oldest down to `START_FRAMES`, so the reference never lags far behind.
const MAX_FRAMES: usize = 8;

/// Loopback callback side: downmix to mono and queue. No allocation, no locks.
pub struct LoopbackTap {
    prod: HeapProd<f32>,
    channels: usize,
}

impl LoopbackTap {
    pub fn new(prod: HeapProd<f32>, channels: usize) -> Self {
        Self {
            prod,
            channels: channels.max(1),
        }
    }

    /// Called from the loopback stream's callback with interleaved samples.
    pub fn push(&mut self, data: &[f32]) {
        let scale = 1.0 / self.channels as f32;
        for f in data.chunks_exact(self.channels) {
            let m: f32 = f.iter().sum::<f32>() * scale;
            let m = if m.is_finite() {
                m.clamp(-1.0, 1.0)
            } else {
                0.0
            };
            // Full ring: the worker isn't taking frames (e.g. stopped); drop the sample.
            let _ = self.prod.try_push(m);
        }
    }
}

/// Worker side: AEC3 plus the reference ring. Built on the main thread (AEC3 allocates
/// its state up front) and then moved to the worker, which never allocates with it.
pub struct EchoRef {
    apm: AudioProcessing,
    cons: HeapCons<f32>,
    /// Reference samples per 10 ms at the device rate.
    ref_frame: usize,
    ref_buf: Box<[f32]>,
    ref_out: Box<[f32]>,
    cap: [f32; FRAME],
    /// Waiting for `START_FRAMES` of reference before consuming (after silence/underrun).
    priming: bool,
}

impl EchoRef {
    /// `rate`: the loopback device's sample rate. AEC3 takes 10 ms chunks, so it must be
    /// a whole number of samples per 10 ms (all usual rates are).
    pub fn new(cons: HeapCons<f32>, rate: u32) -> Result<Self, String> {
        if !(8_000..=384_000).contains(&rate) || !rate.is_multiple_of(100) {
            return Err(format!(
                "the headphones run at {rate} Hz, which echo removal can't use"
            ));
        }
        let ref_frame = rate as usize / 100;
        let apm = AudioProcessing::builder()
            .config(Config {
                echo_canceller: Some(EchoCanceller::default()),
                ..Default::default()
            })
            .capture_config(StreamConfig::new(SAMPLE_RATE, 1))
            .render_config(StreamConfig::new(rate, 1))
            .build();
        Ok(Self {
            apm,
            cons,
            ref_frame,
            ref_buf: vec![0.0; ref_frame].into_boxed_slice(),
            ref_out: vec![0.0; ref_frame].into_boxed_slice(),
            cap: [0.0; FRAME],
            priming: true,
        })
    }

    /// Take the next 10 ms of reference into `ref_buf` (zeros if none is due).
    fn next_ref(&mut self) {
        let n = self.ref_frame;
        let queued = self.cons.occupied_len();
        if queued > MAX_FRAMES * n {
            self.cons.skip(queued - START_FRAMES * n);
        }
        if self.priming && self.cons.occupied_len() >= START_FRAMES * n {
            self.priming = false;
        }
        if !self.priming && self.cons.occupied_len() >= n {
            self.cons.pop_slice(&mut self.ref_buf);
        } else {
            // Nothing playing (or a gap): silence, and re-prime before resuming.
            self.priming = true;
            self.ref_buf.fill(0.0);
        }
    }

    /// Remove the echo from one 48 kHz mic frame, in place. No allocation.
    pub fn process(&mut self, x: &mut [f32; FRAME]) {
        self.next_ref();
        let _ = self
            .apm
            .process_render_f32(&[&self.ref_buf], &mut [&mut self.ref_out]);
        self.cap = *x;
        if self
            .apm
            .process_capture_f32(&[&self.cap], &mut [&mut x[..]])
            .is_err()
        {
            *x = self.cap; // keep the voice if AEC3 refuses a frame
        }
        for s in x.iter_mut() {
            if !s.is_finite() {
                *s = 0.0;
            }
        }
    }

    /// How much echo AEC3 is removing right now (ERLE), in dB, when it knows.
    pub fn erle_db(&self) -> Option<f64> {
        self.apm.statistics().echo_return_loss_enhancement
    }

    /// AEC3's estimate of the headphones → mic delay, in ms, when it knows.
    pub fn delay_ms(&self) -> Option<i32> {
        self.apm.statistics().delay_ms
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ringbuf::HeapRb;
    use ringbuf::traits::Split;

    fn rms(v: &[f32]) -> f32 {
        (v.iter().map(|x| x * x).sum::<f32>() / v.len() as f32).sqrt()
    }

    /// Speech-like test signal: low-passed noise with a syllable-rate envelope.
    fn far_end(n: usize, rate: u32, seed: u32) -> Vec<f32> {
        let mut s = seed;
        let mut lp = 0.0f32;
        (0..n)
            .map(|i| {
                s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let r = (s >> 8) as f32 / 8_388_608.0 - 1.0;
                lp = 0.9 * lp + 0.1 * r;
                let t = i as f32 / rate as f32;
                lp * 1.5 * (0.5 + 0.5 * (t * std::f32::consts::TAU * 3.0).sin()).powi(2)
            })
            .collect()
    }

    /// Runs `secs` of a 35 ms, -14 dB leak through EchoRef with the reference fed in
    /// 10 ms packets at `rate`. Returns (mic, out) at 48 kHz.
    fn run_leak(rate: u32, secs: usize, near: impl Fn(usize) -> f32) -> (Vec<f32>, Vec<f32>) {
        let (prod, cons) = HeapRb::<f32>::new(ring_capacity(rate)).split();
        let mut tap = LoopbackTap::new(prod, 2);
        let mut aec = EchoRef::new(cons, rate).expect("rate");
        let far = far_end(rate as usize * secs, rate, 7);
        let n = SAMPLE_RATE as usize * secs;
        let delay = 48 * 35;
        let mic: Vec<f32> = (0..n)
            .map(|i| {
                let echo = i
                    .checked_sub(delay)
                    .map_or(0.0, |j| 0.2 * far[j * rate as usize / SAMPLE_RATE as usize]);
                echo + near(i)
            })
            .collect();
        let rf = rate as usize / 100;
        let mut out = mic.clone();
        let mut stereo = vec![0.0f32; rf * 2];
        for (f, frame) in out.as_chunks_mut::<FRAME>().0.iter_mut().enumerate() {
            for (k, s) in far[f * rf..(f + 1) * rf].iter().enumerate() {
                stereo[2 * k] = *s;
                stereo[2 * k + 1] = *s;
            }
            tap.push(&stereo);
            aec.process(frame);
        }
        (mic, out)
    }

    #[test]
    fn tap_downmixes_and_sanitizes() {
        let (prod, mut cons) = HeapRb::<f32>::new(16).split();
        let mut tap = LoopbackTap::new(prod, 2);
        tap.push(&[0.5, 0.25, f32::NAN, 1.0, 4.0, 4.0]);
        let mut got = [0.0f32; 3];
        assert_eq!(cons.pop_slice(&mut got), 3);
        assert_eq!(got, [0.375, 0.0, 1.0]);
    }

    #[test]
    fn removes_a_leak_at_48k_and_44k() {
        for rate in [48_000, 44_100] {
            let (mic, out) = run_leak(rate, 6, |_| 0.0);
            let r = 3 * 48_000..6 * 48_000;
            let removed = 20.0 * (rms(&mic[r.clone()]) / rms(&out[r])).log10();
            assert!(removed > 20.0, "{rate} Hz: only {removed:.1} dB removed");
        }
    }

    #[test]
    fn keeps_the_voice_while_talking_over_the_leak() {
        let near = far_end(48_000 * 8, 48_000, 99);
        let talk = 6 * 48_000..8 * 48_000;
        let t2 = talk.clone();
        let (_, out) = run_leak(48_000, 8, |i| if t2.contains(&i) { near[i] } else { 0.0 });
        let loss = 20.0 * (rms(&near[talk.clone()]) / rms(&out[talk])).log10();
        assert!(loss.abs() < 3.0, "voice changed by {loss:.1} dB");
    }

    #[test]
    fn no_reference_leaves_the_mic_alone() {
        let (_prod, cons) = HeapRb::<f32>::new(ring_capacity(48_000)).split();
        let mut aec = EchoRef::new(cons, 48_000).expect("rate");
        let voice = far_end(48_000 * 3, 48_000, 3);
        let mut out = voice.clone();
        for c in out.as_chunks_mut::<FRAME>().0 {
            aec.process(c);
        }
        let r = 48_000..3 * 48_000;
        let change = 20.0 * (rms(&voice[r.clone()]) / rms(&out[r])).log10();
        assert!(change.abs() < 1.0, "voice changed by {change:.1} dB");
    }

    #[test]
    fn backlog_is_trimmed_and_gaps_reprime() {
        let (mut prod, cons) = HeapRb::<f32>::new(ring_capacity(48_000)).split();
        let mut aec = EchoRef::new(cons, 48_000).expect("rate");
        let mut x = [0.0f32; FRAME];
        // 50 frames queued (500 ms): one call drops back to START_FRAMES, uses one.
        for _ in 0..50 * FRAME {
            let _ = prod.try_push(0.1);
        }
        aec.process(&mut x);
        assert_eq!(aec.cons.occupied_len(), (START_FRAMES - 1) * FRAME);
        // Uses the last queued frame, then the ring runs dry → zeros and priming again
        // until START_FRAMES are queued.
        aec.process(&mut x);
        assert!(!aec.priming);
        aec.process(&mut x);
        assert!(aec.priming);
        assert_eq!(aec.ref_buf.iter().sum::<f32>(), 0.0);
    }

    #[test]
    fn odd_rates_are_refused() {
        let (_p, c) = HeapRb::<f32>::new(64).split();
        assert!(EchoRef::new(c, 22_050).is_err());
    }
}
