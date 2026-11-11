// SPDX-License-Identifier: GPL-3.0-only
//! Live settings shared between the control side (today the CLI, later the UI) and the
//! denoise worker.
//!
//! Python analogy: picture a settings object that one thread writes and another reads,
//! with no `threading.Lock`. Each field is an atomic, so a read never waits and never
//! sees a half-written value. Rust has no `AtomicF32`, so floats travel as their raw
//! bit pattern in an `AtomicU32`; `f32::to_bits` and `f32::from_bits` convert exactly.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::Relaxed};

pub struct Params {
    pub hpf: AtomicBool,
    pub gate: AtomicBool,
    pub agc: AtomicBool,
    pub limiter: AtomicBool,
    /// Wet/dry mix of the denoise engine, 0.0..=1.0.
    strength: AtomicU32,
    /// The gate opens when the level rises above this (dBFS).
    gate_threshold_db: AtomicU32,
}

pub const GATE_DB_MIN: f32 = -80.0;
pub const GATE_DB_MAX: f32 = -10.0;
/// Chosen from the user's listening test (fifine mic): -50 let keyboard clicks
/// through, -40 cut them hardest; -45 is the middle ground. Adjustable at runtime.
pub const GATE_DB_DEFAULT: f32 = -45.0;

impl Params {
    pub fn new() -> Self {
        Self {
            hpf: AtomicBool::new(true),
            gate: AtomicBool::new(true),
            agc: AtomicBool::new(true),
            limiter: AtomicBool::new(true),
            strength: AtomicU32::new(1.0f32.to_bits()),
            gate_threshold_db: AtomicU32::new(GATE_DB_DEFAULT.to_bits()),
        }
    }

    pub fn strength(&self) -> f32 {
        f32::from_bits(self.strength.load(Relaxed))
    }

    pub fn set_strength(&self, v: f32) {
        let v = if v.is_finite() {
            v.clamp(0.0, 1.0)
        } else {
            1.0
        };
        self.strength.store(v.to_bits(), Relaxed);
    }

    pub fn gate_threshold_db(&self) -> f32 {
        f32::from_bits(self.gate_threshold_db.load(Relaxed))
    }

    pub fn set_gate_threshold_db(&self, db: f32) {
        let db = if db.is_finite() {
            db.clamp(GATE_DB_MIN, GATE_DB_MAX)
        } else {
            GATE_DB_DEFAULT
        };
        self.gate_threshold_db.store(db.to_bits(), Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setters_clamp_and_reject_nan() {
        let p = Params::new();
        p.set_strength(1.7);
        assert_eq!(p.strength(), 1.0);
        p.set_strength(f32::NAN);
        assert_eq!(p.strength(), 1.0);
        p.set_strength(0.25);
        assert_eq!(p.strength(), 0.25);
        p.set_gate_threshold_db(-200.0);
        assert_eq!(p.gate_threshold_db(), GATE_DB_MIN);
        p.set_gate_threshold_db(-42.5);
        assert_eq!(p.gate_threshold_db(), -42.5);
    }
}
