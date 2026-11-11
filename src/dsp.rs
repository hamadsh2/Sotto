// SPDX-License-Identifier: GPL-3.0-only
//! The processing chain around the denoise engine, in this fixed order:
//!
//! ```text
//! high-pass 80 Hz → denoise (wet/dry "strength") → noise gate → auto gain → limiter
//! ```
//!
//! Every stage is a few multiplications per sample: a biquad filter or envelope
//! followers, with no FFTs, so together they cost almost nothing next to the denoiser.
//!
//! **Glitch-free toggles.** Each stage always runs, so its internal state stays current.
//! A smoothed 0..1 "amount" then fades between the stage's input and its output over
//! 10 ms. Turning a stage on or off is a short crossfade, never a jump that would click.
//! Python analogy: rather than `if enabled: x = stage(x)`, it is
//! `y = stage(x); x = x + (y - x) * amount`, with `amount` gliding toward 0 or 1.

use crate::denoise::{Denoiser, FRAME};
use crate::params::Params;
use std::sync::atomic::Ordering::Relaxed;

const SR: f64 = 48_000.0;

fn db_to_lin(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

fn lin_to_db(x: f32) -> f32 {
    20.0 * x.max(1e-9).log10()
}

/// One-pole smoothing coefficient for a time constant in milliseconds:
/// `y = c·y + (1−c)·x` reaches about 63% of a step after `ms`.
fn coef(ms: f64) -> f32 {
    (-1.0 / (ms * 0.001 * SR)).exp() as f32
}

/// Values this small turn into "denormal" floats, which x86 CPUs process up to 100×
/// slower. Envelopes that decay through silence would reach them, so snap them to zero.
fn flush(x: f32) -> f32 {
    if x.abs() < 1e-15 { 0.0 } else { x }
}

/// Moves linearly toward a target, by at most `step` per sample.
struct Ramp {
    value: f32,
    step: f32,
}

impl Ramp {
    fn new(value: f32, ms: f32) -> Self {
        Self {
            value,
            step: 1.0 / (ms * 0.001 * SR as f32),
        }
    }

    fn next(&mut self, target: f32) -> f32 {
        let d = target - self.value;
        if d.abs() <= self.step {
            self.value = target;
        } else {
            self.value += self.step.copysign(d);
        }
        self.value
    }
}

// ---------------------------------------------------------------------------
// High-pass: removes rumble, desk bumps and mains hum below ~80 Hz.
// ---------------------------------------------------------------------------

/// 2nd-order Butterworth high-pass biquad (Robert Bristow-Johnson's cookbook formulas),
/// computed in f64: at 80 Hz / 48 kHz the poles sit very close to 1, where f32
/// rounding would add noise.
pub struct HighPass {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
    z1: f64,
    z2: f64,
}

impl HighPass {
    pub fn new(cutoff_hz: f64) -> Self {
        let w0 = 2.0 * std::f64::consts::PI * cutoff_hz / SR;
        let (sin, cos) = w0.sin_cos();
        let q = std::f64::consts::FRAC_1_SQRT_2; // Butterworth: maximally flat
        let alpha = sin / (2.0 * q);
        let a0 = 1.0 + alpha;
        Self {
            b0: (1.0 + cos) / 2.0 / a0,
            b1: -(1.0 + cos) / a0,
            b2: (1.0 + cos) / 2.0 / a0,
            a1: -2.0 * cos / a0,
            a2: (1.0 - alpha) / a0,
            z1: 0.0,
            z2: 0.0,
        }
    }

    /// Transposed direct form II, one sample.
    pub fn process(&mut self, x: f32) -> f32 {
        let x = x as f64;
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        if !y.is_finite() {
            // Can't happen with sanitized input; recover instead of ringing NaN forever.
            self.z1 = 0.0;
            self.z2 = 0.0;
            return 0.0;
        }
        if self.z1.abs() < 1e-30 {
            self.z1 = 0.0;
        }
        if self.z2.abs() < 1e-30 {
            self.z2 = 0.0;
        }
        y as f32
    }
}

// ---------------------------------------------------------------------------
// Noise gate: silences what's left between words.
// ---------------------------------------------------------------------------

/// Gain while closed: −80 dB, effectively silent, but not exactly 0.
const GATE_FLOOR: f32 = 1e-4;
/// Opens at the threshold, closes 6 dB below it, so a level hovering right at the
/// threshold doesn't make it flutter.
const GATE_HYSTERESIS_DB: f32 = 6.0;
/// Stays open at least this long after the level falls below the close threshold, so
/// word endings aren't cut.
const GATE_HOLD_SAMPLES: u32 = (SR * 0.150) as u32;

pub struct Gate {
    env: f32,
    env_att: f32,
    env_rel: f32,
    open: bool,
    hold_left: u32,
    gain: f32,
    gain_open: f32,
    gain_close: f32,
    thr_db: f32,
    open_lin: f32,
    close_lin: f32,
}

impl Gate {
    pub fn new(threshold_db: f32) -> Self {
        let mut g = Self {
            env: 0.0,
            env_att: coef(1.0),
            env_rel: coef(30.0),
            open: false,
            hold_left: 0,
            gain: GATE_FLOOR,
            gain_open: coef(2.0), // opens in ~2 ms: the first syllable isn't eaten
            gain_close: coef(60.0), // fades out over ~60 ms: no chopped tails
            thr_db: f32::NAN,
            open_lin: 0.0,
            close_lin: 0.0,
        };
        g.set_threshold(threshold_db);
        g
    }

    /// Cheap to call every frame: recomputes only when the value changed.
    pub fn set_threshold(&mut self, db: f32) {
        if db != self.thr_db {
            self.thr_db = db;
            self.open_lin = db_to_lin(db);
            self.close_lin = db_to_lin(db - GATE_HYSTERESIS_DB);
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    /// True while the level is at or above the threshold right now. Unlike `is_open`,
    /// this excludes the hold time and word tails, so the AGC measures only real speech.
    pub fn is_speaking(&self) -> bool {
        self.env >= self.open_lin
    }

    fn reset(&mut self) {
        self.env = 0.0;
        self.open = false;
        self.hold_left = 0;
        self.gain = GATE_FLOOR;
    }

    /// Feed one sample; returns the gain to apply to it.
    pub fn process(&mut self, x: f32) -> f32 {
        let a = x.abs();
        let c = if a > self.env {
            self.env_att
        } else {
            self.env_rel
        };
        self.env = flush(c * self.env + (1.0 - c) * a);
        if !self.env.is_finite() {
            self.env = 0.0; // never let one bad value freeze the gate
        }

        if self.env >= self.open_lin {
            self.open = true;
            self.hold_left = GATE_HOLD_SAMPLES;
        } else if self.env < self.close_lin {
            if self.hold_left > 0 {
                self.hold_left -= 1;
            } else {
                self.open = false;
            }
        }

        let (target, c) = if self.open {
            (1.0, self.gain_open)
        } else {
            (GATE_FLOOR, self.gain_close)
        };
        self.gain = c * self.gain + (1.0 - c) * target;
        self.gain
    }
}

// ---------------------------------------------------------------------------
// Auto gain: brings a quiet or loud voice toward a steady level.
// ---------------------------------------------------------------------------

/// Speech level the AGC aims for (RMS).
const AGC_TARGET_DB: f32 = -20.0;
const AGC_MAX_GAIN_DB: f32 = 12.0;
const AGC_MIN_GAIN_DB: f32 = -12.0;
/// Gain changes slowly so it never "pumps": +3 dB/s up, −12 dB/s down (per 10 ms frame).
const AGC_UP_DB_PER_FRAME: f32 = 0.03;
const AGC_DOWN_DB_PER_FRAME: f32 = 0.12;

pub struct Agc {
    /// Smoothed mean square of the voice (only while speaking), ~400 ms window.
    ms: f32,
    ms_coef: f32,
    gain_db: f32,
    gain_lin: f32,
}

impl Agc {
    pub fn new() -> Self {
        let t = db_to_lin(AGC_TARGET_DB);
        Self {
            ms: t * t, // start "on target", i.e. at 0 dB gain
            ms_coef: (-(FRAME as f64) / (0.400 * SR)).exp() as f32,
            gain_db: 0.0,
            gain_lin: 1.0,
        }
    }

    pub fn gain_db(&self) -> f32 {
        self.gain_db
    }

    /// Update the gain from one frame. `voiced[i]` says whether sample i is speech
    /// (the gate's decision); silence never moves the gain, so the AGC doesn't crank
    /// up background noise. `gains` receives a per-sample gain that glides from the
    /// previous frame's value to the new one, so changes are click-free.
    pub fn process(
        &mut self,
        buf: &[f32; FRAME],
        voiced: &[bool; FRAME],
        gains: &mut [f32; FRAME],
    ) {
        let (mut sum, mut n) = (0.0f32, 0u32);
        for (x, v) in buf.iter().zip(voiced) {
            if *v {
                sum += x * x;
                n += 1;
            }
        }
        if n as usize >= FRAME / 2 {
            let frame_ms = sum / n as f32;
            self.ms = self.ms_coef * self.ms + (1.0 - self.ms_coef) * frame_ms;
            if !self.ms.is_finite() {
                let t = db_to_lin(AGC_TARGET_DB);
                self.ms = t * t;
            }
            let level_db = 10.0 * self.ms.max(1e-12).log10();
            let want = (AGC_TARGET_DB - level_db).clamp(AGC_MIN_GAIN_DB, AGC_MAX_GAIN_DB);
            let d = want - self.gain_db;
            self.gain_db += d.clamp(-AGC_DOWN_DB_PER_FRAME, AGC_UP_DB_PER_FRAME);
        }
        let start = self.gain_lin;
        let end = db_to_lin(self.gain_db);
        let step = (end - start) / FRAME as f32;
        for (i, g) in gains.iter_mut().enumerate() {
            *g = start + step * (i + 1) as f32;
        }
        self.gain_lin = end;
    }
}

// ---------------------------------------------------------------------------
// Limiter: last stage, guarantees the output never exceeds −1 dBFS.
// ---------------------------------------------------------------------------

pub const LIMIT_CEILING: f32 = 0.891_250_9; // −1 dBFS

/// Zero-latency peak limiter. Its envelope jumps up instantly to any peak and decays
/// over ~80 ms, and is always ≥ the current sample, so `x · ceiling / env` can never
/// exceed the ceiling: no clipping and no added delay.
pub struct Limiter {
    env: f32,
    rel: f32,
}

impl Limiter {
    pub fn new() -> Self {
        Self {
            env: 0.0,
            rel: coef(80.0),
        }
    }

    /// Returns the gain for this sample (1.0 when below the ceiling).
    pub fn process(&mut self, x: f32) -> f32 {
        let a = x.abs();
        self.env = if a > self.env {
            a
        } else {
            flush(self.rel * self.env + (1.0 - self.rel) * a)
        };
        if !self.env.is_finite() {
            self.env = 0.0; // a NaN envelope would silently disable limiting
        }
        if self.env > LIMIT_CEILING {
            LIMIT_CEILING / self.env
        } else {
            1.0
        }
    }
}

// ---------------------------------------------------------------------------
// Delay line: lines the dry signal up with the denoiser's own delay before mixing.
// ---------------------------------------------------------------------------

/// Largest denoiser latency the dry path can match (80 ms).
pub const MAX_LATENCY: usize = FRAME * 8;

struct Delay {
    buf: Box<[f32; MAX_LATENCY]>,
    pos: usize,
    len: usize,
}

impl Delay {
    fn new() -> Self {
        Self {
            buf: Box::new([0.0; MAX_LATENCY]),
            pos: 0,
            len: 0,
        }
    }

    /// Change the delay. Clears the history (allocation-free).
    fn set_len(&mut self, len: usize) {
        let len = len.min(MAX_LATENCY);
        if len != self.len {
            self.len = len;
            self.pos = 0;
            self.buf.fill(0.0);
        }
    }

    fn process(&mut self, x: f32) -> f32 {
        if self.len == 0 {
            return x;
        }
        let y = self.buf[self.pos];
        self.buf[self.pos] = x;
        self.pos += 1;
        if self.pos == self.len {
            self.pos = 0;
        }
        y
    }
}

// ---------------------------------------------------------------------------
// The whole chain.
// ---------------------------------------------------------------------------

/// Per-frame readings for the once-a-second report.
#[derive(Clone, Copy, Default)]
pub struct FrameInfo {
    pub gate_open: bool,
    pub agc_gain_db: f32,
    /// Largest limiter gain reduction in this frame, dB (0 = not limiting).
    pub limit_db: f32,
}

pub struct Chain {
    hpf: HighPass,
    gate: Gate,
    agc: Agc,
    lim: Limiter,
    dry: Delay,
    hpf_amt: Ramp,
    gate_amt: Ramp,
    agc_amt: Ramp,
    lim_amt: Ramp,
    strength: Ramp,
    a: [f32; FRAME],
    b: [f32; FRAME],
    voiced: [bool; FRAME],
    gains: [f32; FRAME],
}

fn on(b: bool) -> f32 {
    if b { 1.0 } else { 0.0 }
}

impl Chain {
    /// Starts with every stage at its current setting (no fade-in on startup).
    pub fn new(p: &Params) -> Self {
        Self {
            hpf: HighPass::new(80.0),
            gate: Gate::new(p.gate_threshold_db()),
            agc: Agc::new(),
            lim: Limiter::new(),
            dry: Delay::new(),
            hpf_amt: Ramp::new(on(p.hpf.load(Relaxed)), 10.0),
            gate_amt: Ramp::new(on(p.gate.load(Relaxed)), 10.0),
            // Slower: the AGC may be up to ±12 dB, so switching it fades over 300 ms
            // rather than jumping in loudness.
            agc_amt: Ramp::new(on(p.agc.load(Relaxed)), 300.0),
            lim_amt: Ramp::new(on(p.limiter.load(Relaxed)), 10.0),
            strength: Ramp::new(p.strength(), 50.0),
            a: [0.0; FRAME],
            b: [0.0; FRAME],
            voiced: [false; FRAME],
            gains: [1.0; FRAME],
        }
    }

    /// After a device reconnect: forget the audio from before the gap, so the first
    /// new frame isn't spliced onto old filter/envelope state. Keeps the AGC's learned
    /// level. Allocation-free.
    pub fn reset(&mut self) {
        self.hpf.z1 = 0.0;
        self.hpf.z2 = 0.0;
        self.dry.buf.fill(0.0);
        self.dry.pos = 0;
        self.gate.reset();
        self.lim.env = 0.0;
    }

    /// Process one frame. Real-time safe: no allocation, no locks, no I/O.
    pub fn process(
        &mut self,
        input: &[f32; FRAME],
        den: &mut dyn Denoiser,
        p: &Params,
        out: &mut [f32; FRAME],
    ) -> FrameInfo {
        // Read every setting once per frame.
        let hpf_on = on(p.hpf.load(Relaxed));
        let gate_on = on(p.gate.load(Relaxed));
        let agc_on = on(p.agc.load(Relaxed));
        let lim_on = on(p.limiter.load(Relaxed));
        let strength = p.strength();
        self.gate.set_threshold(p.gate_threshold_db());
        debug_assert!(den.latency() <= MAX_LATENCY);
        self.dry.set_len(den.latency());

        // 0+1. Sanitize, then high-pass → self.a. A driver can hand us NaN, infinity or
        // wildly out-of-range values; one such sample would otherwise poison every
        // filter state downstream for good. Bad samples become silence, the rest is
        // bounded to the valid range.
        for (y, x) in self.a.iter_mut().zip(input) {
            let x = if x.is_finite() {
                x.clamp(-1.0, 1.0)
            } else {
                0.0
            };
            let f = self.hpf.process(x);
            *y = x + (f - x) * self.hpf_amt.next(hpf_on);
        }

        // 2. Denoise → self.b, mixed with the (delay-matched) dry signal by strength.
        den.process(&self.a, &mut self.b);
        for (wet, x) in self.b.iter_mut().zip(&self.a) {
            let dry = self.dry.process(*x);
            let w = if wet.is_finite() { *wet } else { dry };
            *wet = dry + (w - dry) * self.strength.next(strength);
        }

        // 3. Gate (the detector always runs; the AGC uses it to tell speech from
        // silence).
        let mut any_open = false;
        for (i, x) in self.b.iter_mut().enumerate() {
            let g = self.gate.process(*x);
            self.voiced[i] = self.gate.is_speaking();
            any_open |= self.gate.is_open();
            *x *= 1.0 + (g - 1.0) * self.gate_amt.next(gate_on);
        }

        // 4. Auto gain.
        self.agc.process(&self.b, &self.voiced, &mut self.gains);
        for (x, g) in self.b.iter_mut().zip(&self.gains) {
            *x *= 1.0 + (g - 1.0) * self.agc_amt.next(agc_on);
        }

        // 5. Limiter (last), then a hard safety clamp to the valid range.
        let mut min_gain = 1.0f32;
        for (y, x) in out.iter_mut().zip(&self.b) {
            let g = self.lim.process(*x);
            let g = 1.0 + (g - 1.0) * self.lim_amt.next(lim_on);
            min_gain = min_gain.min(g);
            let v = x * g;
            *y = if v.is_nan() { 0.0 } else { v.clamp(-1.0, 1.0) };
        }

        FrameInfo {
            gate_open: any_open,
            // Report the gain actually applied (0 dB while the AGC is off).
            agc_gain_db: if agc_on > 0.0 {
                self.agc.gain_db()
            } else {
                0.0
            },
            limit_db: -lin_to_db(min_gain),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::denoise::Off;

    fn sine(freq: f32, amp: f32, n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| amp * (2.0 * std::f32::consts::PI * freq * i as f32 / SR as f32).sin())
            .collect()
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|s| s * s).sum::<f32>() / x.len() as f32).sqrt()
    }

    #[test]
    fn highpass_cuts_rumble_keeps_voice() {
        for (freq, lo, hi) in [(20.0, -60.0, -20.0), (1000.0, -0.2, 0.2)] {
            let mut f = HighPass::new(80.0);
            let x = sine(freq, 0.5, 48_000);
            let y: Vec<f32> = x.iter().map(|s| f.process(*s)).collect();
            let db = lin_to_db(rms(&y[24_000..]) / rms(&x[24_000..]));
            assert!(db > lo && db < hi, "{freq} Hz: {db:.1} dB");
        }
    }

    #[test]
    fn gate_closes_on_hiss_and_opens_fast_on_voice() {
        let mut g = Gate::new(-50.0);
        // −70 dBFS hiss for 1 s: closed, gain at the floor.
        for s in sine(3000.0, db_to_lin(-70.0), 48_000) {
            g.process(s);
        }
        assert!(!g.is_open());
        assert!(g.gain < 1e-3);
        // −20 dBFS voice: open, and near full gain within 10 ms.
        let mut gain = 0.0;
        for s in sine(300.0, db_to_lin(-20.0), 480) {
            gain = g.process(s);
        }
        assert!(g.is_open());
        assert!(gain > 0.9, "gain after 10 ms: {gain}");
    }

    #[test]
    fn agc_raises_quiet_voice_but_caps_at_12_db() {
        let mut agc = Agc::new();
        let voiced = [true; FRAME];
        let mut gains = [0.0; FRAME];
        let quiet: Vec<f32> = sine(300.0, db_to_lin(-50.0), FRAME);
        let frame: [f32; FRAME] = quiet.try_into().unwrap();
        for _ in 0..1000 {
            agc.process(&frame, &voiced, &mut gains); // 10 s
        }
        assert!(
            (agc.gain_db() - AGC_MAX_GAIN_DB).abs() < 1e-3,
            "{}",
            agc.gain_db()
        );
        // Silence (gate closed) doesn't move it.
        let before = agc.gain_db();
        for _ in 0..100 {
            agc.process(&[0.0; FRAME], &[false; FRAME], &mut gains);
        }
        assert_eq!(agc.gain_db(), before);
    }

    #[test]
    fn limiter_never_exceeds_ceiling() {
        let mut l = Limiter::new();
        let mut peak = 0.0f32;
        for s in sine(440.0, 2.0, 48_000) {
            peak = peak.max((s * l.process(s)).abs());
        }
        assert!(peak <= LIMIT_CEILING + 1e-6, "peak {peak}");
        // Quiet audio passes untouched.
        let mut l = Limiter::new();
        for s in sine(440.0, 0.5, 4800) {
            assert_eq!(l.process(s), 1.0);
        }
    }

    #[test]
    fn all_stages_off_is_passthrough() {
        let p = Params::new();
        for b in [&p.hpf, &p.gate, &p.agc, &p.limiter] {
            b.store(false, Relaxed);
        }
        let mut c = Chain::new(&p);
        let x: [f32; FRAME] = sine(440.0, 0.5, FRAME).try_into().unwrap();
        let mut y = [0.0; FRAME];
        c.process(&x, &mut Off, &p, &mut y);
        for (a, b) in x.iter().zip(&y) {
            assert!((a - b).abs() < 1e-6);
        }
    }

    #[test]
    fn toggling_a_stage_never_jumps() {
        // A steady tone; toggle every stage on and off. Sample-to-sample change must
        // stay close to the tone's own slope (no step = no click).
        let p = Params::new();
        p.set_gate_threshold_db(-80.0); // keep the gate open on the tone
        let mut c = Chain::new(&p);
        let tone = sine(200.0, 0.3, FRAME * 400);
        let max_slope = 0.3 * 2.0 * std::f32::consts::PI * 200.0 / SR as f32;
        let mut prev = 0.0f32;
        let mut worst = 0.0f32;
        for (k, chunk) in tone.as_chunks::<FRAME>().0.iter().enumerate() {
            if k % 20 == 10 {
                for b in [&p.hpf, &p.gate, &p.agc, &p.limiter] {
                    b.store(!b.load(Relaxed), Relaxed);
                }
            }
            let x = *chunk;
            let mut y = [0.0; FRAME];
            c.process(&x, &mut Off, &p, &mut y);
            if k > 50 {
                for s in y {
                    worst = worst.max((s - prev).abs());
                    prev = s;
                }
            } else {
                prev = y[FRAME - 1];
            }
        }
        // AGC gain up to +12 dB (×4) allowed on top of the tone's slope.
        assert!(
            worst < max_slope * 4.5,
            "worst step {worst}, tone slope {max_slope}"
        );
    }

    #[test]
    fn nan_and_inf_input_never_poison_the_chain() {
        let p = Params::new();
        p.set_gate_threshold_db(-80.0);
        let mut c = Chain::new(&p);
        let mut rn = crate::denoise::Rnnoise::new();
        let mut y = [0.0; FRAME];
        let mut bad = [0.0f32; FRAME];
        bad[0] = f32::NAN;
        bad[1] = f32::INFINITY;
        bad[2] = f32::NEG_INFINITY;
        bad[3] = 1e30;
        c.process(&bad, &mut rn, &p, &mut y);
        assert!(y.iter().all(|s| s.is_finite()));
        // A tone afterwards comes through, finite and audible.
        let tone = sine(440.0, 0.3, FRAME * 100);
        let mut last = 0.0;
        for chunk in tone.as_chunks::<FRAME>().0 {
            c.process(chunk, &mut rn, &p, &mut y);
            assert!(y.iter().all(|s| s.is_finite() && s.abs() <= 1.0));
            last = rms(&y);
        }
        assert!(last > 0.01, "tone didn't recover: rms {last}");
    }

    #[test]
    fn delay_line_delays_exactly() {
        let mut d = Delay::new();
        d.set_len(3);
        let y: Vec<f32> = (1..=6).map(|i| d.process(i as f32)).collect();
        assert_eq!(y, [0.0, 0.0, 0.0, 1.0, 2.0, 3.0]);
    }
}
