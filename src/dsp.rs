// SPDX-License-Identifier: GPL-3.0-only
//! The processing chain around the denoise engine, in this fixed order:
//!
//! ```text
//! high-pass 80 Hz → denoise (wet/dry "strength") → EQ → noise gate → auto gain → limiter
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
use crate::eq::Eq;
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
const AGC_MIN_GAIN_DB: f32 = -12.0;
/// Gain changes slowly so it never "pumps": +3 dB/s up, −12 dB/s down (per 10 ms frame).
const AGC_UP_DB_PER_FRAME: f32 = 0.03;
const AGC_DOWN_DB_PER_FRAME: f32 = 0.12;
/// A word much louder than the gain expects (e.g. a shout after quiet speech at
/// +18 dB) would otherwise lean on the limiter for ~1.5 s. When this frame alone is
/// more than this far above target, the gain drops fast instead.
const AGC_LOUD_MARGIN_DB: f32 = 6.0;
const AGC_FAST_DOWN_DB_PER_FRAME: f32 = 1.0;

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
    /// up background noise. `max_db` caps the boost (user setting). `gains` receives a per-sample gain that glides from the
    /// previous frame's value to the new one, so changes are click-free.
    pub fn process(
        &mut self,
        buf: &[f32; FRAME],
        voiced: &[bool; FRAME],
        max_db: f32,
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
            let want = (AGC_TARGET_DB - level_db).clamp(AGC_MIN_GAIN_DB, max_db.max(0.0));
            let d = want - self.gain_db;
            self.gain_db += d.clamp(-AGC_DOWN_DB_PER_FRAME, AGC_UP_DB_PER_FRAME);
            // Only the part above target + margin is pulled down fast, so ordinary
            // stressed syllables barely move it (no pumping).
            let frame_db = 10.0 * frame_ms.max(1e-12).log10();
            let excess = frame_db + self.gain_db - (AGC_TARGET_DB + AGC_LOUD_MARGIN_DB);
            if excess > 0.0 {
                self.gain_db -= excess.min(AGC_FAST_DOWN_DB_PER_FRAME);
            }
            self.gain_db = self.gain_db.max(AGC_MIN_GAIN_DB);
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
    /// Frames the current engine has failed to process so far (passed through).
    pub engine_errors: u64,
}

/// A denoise engine plus the dry-path delay matched to its latency, so the strength
/// (wet/dry) mix lines up. Built on the main thread (the delay buffer is allocated
/// there) and then moved to the worker.
pub struct EngineSlot {
    den: Box<dyn Denoiser>,
    dry: Delay,
}

impl EngineSlot {
    pub fn new(den: Box<dyn Denoiser>) -> Self {
        debug_assert!(den.latency() <= MAX_LATENCY);
        let mut dry = Delay::new();
        dry.set_len(den.latency());
        Self { den, dry }
    }

    pub fn name(&self) -> &'static str {
        self.den.name()
    }

    /// Denoise `a` and mix with the delayed dry signal, per-sample `strength`.
    fn process(&mut self, a: &[f32; FRAME], strength: &[f32; FRAME], out: &mut [f32; FRAME]) {
        self.den.process(a, out);
        for ((wet, x), s) in out.iter_mut().zip(a).zip(strength) {
            let dry = self.dry.process(*x);
            let w = if wet.is_finite() { *wet } else { dry };
            *wet = dry + (w - dry) * s;
        }
    }

    /// Forget old audio: run enough silent frames to push everything the engine holds
    /// (its latency) out the other end.
    fn flush(&mut self, scratch: &mut [f32; FRAME]) {
        for _ in 0..=self.den.latency() / FRAME {
            self.den.process(&[0.0; FRAME], scratch);
        }
        self.dry.buf.fill(0.0);
        self.dry.pos = 0;
    }
}

/// Crossfade length when switching engines (spec: 20–50 ms).
const SWITCH_FADE: usize = FRAME * 3; // 30 ms
/// Before the crossfade the new engine runs silently on the live audio this long, on
/// top of its own latency, so it has settled (noise estimate, lookahead filled) by the
/// time it's heard. Otherwise its first frames would be silence or half-adapted.
const SWITCH_WARMUP_FRAMES: u32 = 10; // 100 ms

struct Switch {
    next: EngineSlot,
    warm_left: u32,
    /// Samples into the crossfade.
    pos: usize,
}

pub struct Chain {
    hpf: HighPass,
    eq: Eq,
    gate: Gate,
    agc: Agc,
    lim: Limiter,
    engine: EngineSlot,
    switch: Option<Switch>,
    /// The engine replaced by the last finished switch, waiting to be handed back to
    /// the main thread (dropping a model frees memory, which shouldn't happen here).
    retired: Option<EngineSlot>,
    hpf_amt: Ramp,
    eq_amt: Ramp,
    eq_dry: [f32; FRAME],
    gate_amt: Ramp,
    agc_amt: Ramp,
    lim_amt: Ramp,
    strength: Ramp,
    a: [f32; FRAME],
    b: [f32; FRAME],
    next_out: [f32; FRAME],
    strength_buf: [f32; FRAME],
    voiced: [bool; FRAME],
    gains: [f32; FRAME],
}

fn on(b: bool) -> f32 {
    if b { 1.0 } else { 0.0 }
}

impl Chain {
    /// Starts with every stage at its current setting (no fade-in on startup).
    pub fn new(engine: EngineSlot, p: &Params) -> Self {
        Self {
            hpf: HighPass::new(80.0),
            eq: Eq::new(p.eq_gains()),
            gate: Gate::new(p.gate_threshold_db()),
            agc: Agc::new(),
            lim: Limiter::new(),
            engine,
            switch: None,
            retired: None,
            hpf_amt: Ramp::new(on(p.hpf.load(Relaxed)), 10.0),
            eq_amt: Ramp::new(on(p.eq.load(Relaxed)), 10.0),
            eq_dry: [0.0; FRAME],
            gate_amt: Ramp::new(on(p.gate.load(Relaxed)), 10.0),
            // Slower: the AGC may be up to +24 dB, so switching it fades over 300 ms
            // rather than jumping in loudness.
            agc_amt: Ramp::new(on(p.agc.load(Relaxed)), 300.0),
            lim_amt: Ramp::new(on(p.limiter.load(Relaxed)), 10.0),
            strength: Ramp::new(p.strength(), 50.0),
            a: [0.0; FRAME],
            b: [0.0; FRAME],
            next_out: [0.0; FRAME],
            strength_buf: [0.0; FRAME],
            voiced: [false; FRAME],
            gains: [1.0; FRAME],
        }
    }

    /// Start a glitch-free switch to `next`: it warms up silently, then crossfades in
    /// over 30 ms. If another switch was still in progress, its target is abandoned and
    /// returned (for the caller to hand back to the main thread).
    pub fn switch_to(&mut self, next: EngineSlot) -> Option<EngineSlot> {
        let warm_left = (next.den.latency() / FRAME) as u32 + SWITCH_WARMUP_FRAMES;
        self.switch
            .replace(Switch {
                next,
                warm_left,
                pos: 0,
            })
            .map(|s| s.next)
    }

    /// Switch immediately while no audio is flowing (nothing to crossfade). Returns the
    /// replaced engine and any abandoned switch target.
    pub fn replace_now(&mut self, next: EngineSlot) -> (EngineSlot, Option<EngineSlot>) {
        let pending = self.switch.take().map(|s| s.next);
        (std::mem::replace(&mut self.engine, next), pending)
    }

    /// Name of the engine currently heard.
    pub fn engine_name(&self) -> &'static str {
        self.engine.name()
    }

    pub fn is_switching(&self) -> bool {
        self.switch.is_some()
    }

    /// The engine replaced by a finished switch, if any.
    pub fn take_retired(&mut self) -> Option<EngineSlot> {
        self.retired.take()
    }

    /// After a device reconnect: forget the audio from before the gap, so the first
    /// new frame isn't spliced onto old filter/envelope state. Keeps the AGC's learned
    /// level. Allocation-free.
    pub fn reset(&mut self) {
        self.hpf.z1 = 0.0;
        self.hpf.z2 = 0.0;
        self.eq.reset();
        // A reconnect is a break in the audio anyway, so a switch in progress simply
        // completes now (the old engine goes to `retired`).
        if let Some(sw) = self.switch.take() {
            self.retired = Some(std::mem::replace(&mut self.engine, sw.next));
        }
        self.engine.flush(&mut self.next_out);
        self.gate.reset();
        self.lim.env = 0.0;
    }

    /// Process one frame. No allocation, no locks, no I/O (the DeepFilterNet engine
    /// waits a bounded time for its own thread; see `dfn.rs`).
    pub fn process(
        &mut self,
        input: &[f32; FRAME],
        p: &Params,
        out: &mut [f32; FRAME],
    ) -> FrameInfo {
        // Read every setting once per frame.
        let hpf_on = on(p.hpf.load(Relaxed));
        let eq_on = on(p.eq.load(Relaxed));
        let gate_on = on(p.gate.load(Relaxed));
        let agc_on = on(p.agc.load(Relaxed));
        let lim_on = on(p.limiter.load(Relaxed));
        let strength = p.strength();
        self.gate.set_threshold(p.gate_threshold_db());

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
        for s in self.strength_buf.iter_mut() {
            *s = self.strength.next(strength);
        }
        self.engine
            .process(&self.a, &self.strength_buf, &mut self.b);

        // 2b. Engine switch in progress: the new engine runs on the same input; after
        // its warm-up, an equal-power crossfade (cos/sin) moves from old to new. Each
        // engine is mixed with its own matched dry signal, so strength < 100% stays
        // aligned on both sides.
        let mut finished = false;
        if let Some(sw) = self.switch.as_mut() {
            sw.next
                .process(&self.a, &self.strength_buf, &mut self.next_out);
            if sw.warm_left > 0 {
                sw.warm_left -= 1;
            } else {
                for (i, (o, n)) in self.b.iter_mut().zip(&self.next_out).enumerate() {
                    let t = ((sw.pos + i + 1) as f32 / SWITCH_FADE as f32).min(1.0);
                    let (g_new, g_old) = (t * std::f32::consts::FRAC_PI_2).sin_cos();
                    *o = *o * g_old + n * g_new;
                }
                sw.pos += FRAME;
                finished = sw.pos >= SWITCH_FADE;
            }
        }
        if finished && let Some(sw) = self.switch.take() {
            let old = std::mem::replace(&mut self.engine, sw.next);
            self.retired = Some(old);
        }

        // 2c. EQ (always runs; faded in/out by `eq_amt` like the other stages; a preset
        // change crossfades inside `Eq`).
        self.eq_dry = self.b;
        self.eq.process(&mut self.b, p.eq_gains());
        for (y, x) in self.b.iter_mut().zip(&self.eq_dry) {
            *y = x + (*y - x) * self.eq_amt.next(eq_on);
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
        self.agc
            .process(&self.b, &self.voiced, p.agc_max_db(), &mut self.gains);
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
            engine_errors: self.engine.den.errors(),
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

    /// On ordinary speech (syllables well above the average level), the fast descent
    /// must not drag the level down: after settling, speech should sit near target.
    #[test]
    fn agc_settles_near_target_on_speech() {
        let n = FRAME * 3000; // 30 s
        let mut v = crate::denoise::tests::voice_like_pub(n);
        let rms = (v.iter().map(|x| x * x).sum::<f32>() / n as f32).sqrt();
        let scale = db_to_lin(-32.0) / rms;
        v.iter_mut().for_each(|x| *x *= scale);
        let mut agc = Agc::new();
        let mut gains = [0.0; FRAME];
        let (mut sum, mut cnt) = (0.0f64, 0usize);
        let (mut lo, mut hi) = (f32::MAX, f32::MIN);
        for (k, f) in v.as_chunks::<FRAME>().0.iter().enumerate() {
            agc.process(f, &[true; FRAME], 18.0, &mut gains);
            if k >= 2000 {
                for (x, g) in f.iter().zip(&gains) {
                    sum += ((x * g) as f64).powi(2);
                }
                cnt += FRAME;
                lo = lo.min(agc.gain_db());
                hi = hi.max(agc.gain_db());
            }
        }
        let out_db = 10.0 * (sum / cnt as f64).log10();
        eprintln!("speech out {out_db:.1} dBFS (target {AGC_TARGET_DB}), gain {lo:.1}..{hi:.1} dB");
        assert!(
            (out_db - AGC_TARGET_DB as f64).abs() < 2.0,
            "out {out_db:.1} dBFS"
        );
        assert!(hi - lo < 3.0, "gain wandered {lo:.1}..{hi:.1} dB");
    }

    #[test]
    fn agc_drops_fast_when_a_loud_word_follows_quiet_speech() {
        let mut agc = Agc::new();
        let voiced = [true; FRAME];
        let mut gains = [0.0; FRAME];
        let quiet: [f32; FRAME] = sine(300.0, db_to_lin(-50.0), FRAME).try_into().unwrap();
        for _ in 0..1000 {
            agc.process(&quiet, &voiced, 18.0, &mut gains);
        }
        assert!(agc.gain_db() > 17.9);
        // A loud word (RMS ≈ −9 dBFS): within 200 ms the gain is near or below 0 dB,
        // instead of ~1.5 s at the normal −12 dB/s.
        let loud: [f32; FRAME] = sine(300.0, 0.5, FRAME).try_into().unwrap();
        for _ in 0..20 {
            agc.process(&loud, &voiced, 18.0, &mut gains);
        }
        assert!(agc.gain_db() < 0.5, "gain after 200 ms: {}", agc.gain_db());
    }

    #[test]
    fn agc_raises_quiet_voice_but_caps_at_the_user_max() {
        let mut agc = Agc::new();
        let voiced = [true; FRAME];
        let mut gains = [0.0; FRAME];
        let quiet: Vec<f32> = sine(300.0, db_to_lin(-50.0), FRAME);
        let frame: [f32; FRAME] = quiet.try_into().unwrap();
        for _ in 0..1000 {
            agc.process(&frame, &voiced, 18.0, &mut gains); // 10 s
        }
        assert!((agc.gain_db() - 18.0).abs() < 1e-3, "{}", agc.gain_db());
        // Lowering the cap brings the gain down smoothly (−12 dB/s), never a jump.
        let mut prev = gains[FRAME - 1];
        for _ in 0..100 {
            agc.process(&frame, &voiced, 12.0, &mut gains);
            for g in gains {
                assert!((g - prev).abs() < 1e-3, "gain jumped {prev} -> {g}");
                prev = g;
            }
        }
        assert!((agc.gain_db() - 12.0).abs() < 1e-3, "{}", agc.gain_db());
        // Silence (gate closed) doesn't move it.
        let before = agc.gain_db();
        for _ in 0..100 {
            agc.process(&[0.0; FRAME], &[false; FRAME], 12.0, &mut gains);
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
        let mut c = Chain::new(EngineSlot::new(Box::new(Off)), &p);
        let x: [f32; FRAME] = sine(440.0, 0.5, FRAME).try_into().unwrap();
        let mut y = [0.0; FRAME];
        c.process(&x, &p, &mut y);
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
        let mut c = Chain::new(EngineSlot::new(Box::new(Off)), &p);
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
            c.process(&x, &p, &mut y);
            if k > 50 {
                for s in y {
                    worst = worst.max((s - prev).abs());
                    prev = s;
                }
            } else {
                prev = y[FRAME - 1];
            }
        }
        // The tone is loud (0.3), so the AGC applies little gain; its changes are
        // ramped per sample. Allow ×4.5 on top of the tone's slope.
        assert!(
            worst < max_slope * 4.5,
            "worst step {worst}, tone slope {max_slope}"
        );
    }

    #[test]
    fn nan_and_inf_input_never_poison_the_chain() {
        let p = Params::new();
        p.set_gate_threshold_db(-80.0);
        let mut c = Chain::new(
            EngineSlot::new(Box::new(crate::denoise::Rnnoise::new())),
            &p,
        );
        let mut y = [0.0; FRAME];
        let mut bad = [0.0f32; FRAME];
        bad[0] = f32::NAN;
        bad[1] = f32::INFINITY;
        bad[2] = f32::NEG_INFINITY;
        bad[3] = 1e30;
        c.process(&bad, &p, &mut y);
        assert!(y.iter().all(|s| s.is_finite()));
        // A tone afterwards comes through, finite and audible.
        let tone = sine(440.0, 0.3, FRAME * 100);
        let mut last = 0.0;
        for chunk in tone.as_chunks::<FRAME>().0 {
            c.process(chunk, &p, &mut y);
            assert!(y.iter().all(|s| s.is_finite() && s.abs() <= 1.0));
            last = rms(&y);
        }
        assert!(last > 0.01, "tone didn't recover: rms {last}");
    }

    #[test]
    fn engine_switch_crossfades_without_a_jump_and_retires_old() {
        // Switch Off → RNNoise → Off on a steady tone at 50% strength. The output must
        // never step (no click), the switch must finish, and the old engine must come
        // back via take_retired.
        let p = Params::new();
        for b in [&p.gate, &p.agc, &p.limiter] {
            b.store(false, Relaxed);
        }
        p.set_strength(0.5);
        let mut c = Chain::new(EngineSlot::new(Box::new(Off)), &p);
        let tone = sine(200.0, 0.3, FRAME * 300);
        let max_slope = 0.3 * 2.0 * std::f32::consts::PI * 200.0 / SR as f32;
        let (mut prev, mut worst) = (0.0f32, 0.0f32);
        let mut retired = Vec::new();
        for (k, chunk) in tone.as_chunks::<FRAME>().0.iter().enumerate() {
            match k {
                50 => assert!(
                    c.switch_to(EngineSlot::new(Box::new(crate::denoise::Rnnoise::new())))
                        .is_none()
                ),
                150 => assert!(c.switch_to(EngineSlot::new(Box::new(Off))).is_none()),
                _ => {}
            }
            let mut y = [0.0; FRAME];
            c.process(chunk, &p, &mut y);
            if let Some(old) = c.take_retired() {
                retired.push((k, old.name()));
            }
            if k > 10 {
                for s in y {
                    worst = worst.max((s - prev).abs());
                    prev = s;
                }
            } else {
                prev = y[FRAME - 1];
            }
        }
        // Off → rnnoise: 1 latency frame + 10 warm-up + 3 fade frames.
        assert_eq!(retired, [(50 + 13, "off"), (150 + 12, "rnnoise")]);
        assert_eq!(c.engine.name(), "off");
        // RNNoise changes the tone's level/phase; allow a few times the tone's slope.
        assert!(
            worst < max_slope * 3.0,
            "worst step {worst}, tone slope {max_slope}"
        );
    }

    #[test]
    fn delay_line_delays_exactly() {
        let mut d = Delay::new();
        d.set_len(3);
        let y: Vec<f32> = (1..=6).map(|i| d.process(i as f32)).collect();
        assert_eq!(y, [0.0, 0.0, 0.0, 1.0, 2.0, 3.0]);
    }
}
