// SPDX-License-Identifier: GPL-3.0-only
//! Live settings shared between the control side (today the CLI, later the UI) and the
//! denoise worker.
//!
//! Python analogy: picture a settings object that one thread writes and another reads,
//! with no `threading.Lock`. Each field is an atomic, so a read never waits and never
//! sees a half-written value. Rust has no `AtomicF32`, so floats travel as their raw
//! bit pattern in an `AtomicU32`; `f32::to_bits` and `f32::from_bits` convert exactly.

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering::Relaxed};

pub struct Params {
    pub hpf: AtomicBool,
    pub gate: AtomicBool,
    pub agc: AtomicBool,
    pub limiter: AtomicBool,
    /// Mute: the output fades to silence (10 ms) while the streams keep running, so
    /// unmuting is instant. Not saved: Sotto always starts unmuted.
    pub muted: AtomicBool,
    /// Wet/dry mix of the denoise engine, 0.0..=1.0.
    strength: AtomicU32,
    /// The gate opens when the level rises above this (dBFS).
    gate_threshold_db: AtomicU32,
    /// Most the auto gain may boost a quiet voice (dB).
    agc_max_db: AtomicU32,
    /// DeepFilterNet's post-filter (extra suppression of leftover noise). Read only
    /// when a DeepFilterNet engine is built, on the main thread; changing it rebuilds
    /// the engine through the normal crossfade.
    pub dfn_postfilter: AtomicBool,
    pub eq: AtomicBool,
    /// EQ band gains in dB: body, mud, presence (f32 bits).
    eq_gains: [AtomicU32; 3],
    /// Which preset those gains came from (`EqPreset as u8`), for display.
    eq_preset: AtomicU8,
}

/// Named EQ settings. Mild on purpose (±3 dB max): they were not tuned per mic, and
/// boosting presence too far would lift the hiss the denoiser left behind.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum EqPreset {
    Natural = 0,
    Warm = 1,
    Clear = 2,
    Broadcast = 3,
    Custom = 4,
}

impl EqPreset {
    pub const NAMED: [EqPreset; 4] = [Self::Natural, Self::Warm, Self::Clear, Self::Broadcast];

    pub fn name(self) -> &'static str {
        match self {
            Self::Natural => "natural",
            Self::Warm => "warm",
            Self::Clear => "clear",
            Self::Broadcast => "broadcast",
            Self::Custom => "custom",
        }
    }

    /// How the window shows it (sentence case).
    pub fn label(self) -> &'static str {
        match self {
            Self::Natural => "Natural",
            Self::Warm => "Warm",
            Self::Clear => "Clear",
            Self::Broadcast => "Broadcast",
            Self::Custom => "Custom",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::NAMED.into_iter().find(|p| p.name() == s)
    }

    /// Body (low shelf ~150 Hz), mud (peak ~350 Hz), presence (peak ~4 kHz), in dB.
    pub fn gains(self) -> [f32; 3] {
        match self {
            Self::Natural | Self::Custom => [0.0, 0.0, 0.0],
            Self::Warm => [2.5, -1.5, 0.0],
            Self::Clear => [0.0, -3.0, 3.0],
            Self::Broadcast => [2.0, -2.5, 2.5],
        }
    }

    fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Warm,
            2 => Self::Clear,
            3 => Self::Broadcast,
            4 => Self::Custom,
            _ => Self::Natural,
        }
    }
}

/// Custom EQ band range (dB).
pub const EQ_DB_LIMIT: f32 = 6.0;

pub const GATE_DB_MIN: f32 = -80.0;
pub const GATE_DB_MAX: f32 = -10.0;
/// Chosen from the user's listening test (fifine mic): -50 let keyboard clicks
/// through, -40 cut them hardest; -45 is the middle ground. Adjustable at runtime.
pub const GATE_DB_DEFAULT: f32 = -45.0;

pub const AGC_MAX_DB_MIN: f32 = 0.0;
pub const AGC_MAX_DB_MAX: f32 = 24.0;
/// The user compared 12 / 18 / 24 on the fifine mic (Phase 2): all fine, +12 kept as
/// the default. Mics differ, so this is a user setting (UI slider in Phase 3).
pub const AGC_MAX_DB_DEFAULT: f32 = 12.0;

impl Default for Params {
    fn default() -> Self {
        Self::new()
    }
}

impl Params {
    pub fn new() -> Self {
        Self {
            hpf: AtomicBool::new(true),
            gate: AtomicBool::new(true),
            agc: AtomicBool::new(true),
            limiter: AtomicBool::new(true),
            muted: AtomicBool::new(false),
            strength: AtomicU32::new(1.0f32.to_bits()),
            gate_threshold_db: AtomicU32::new(GATE_DB_DEFAULT.to_bits()),
            agc_max_db: AtomicU32::new(AGC_MAX_DB_DEFAULT.to_bits()),
            dfn_postfilter: AtomicBool::new(false),
            eq: AtomicBool::new(true),
            eq_gains: [const { AtomicU32::new(0) }; 3], // 0u32 is 0.0f32: Natural
            eq_preset: AtomicU8::new(EqPreset::Natural as u8),
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

    pub fn agc_max_db(&self) -> f32 {
        f32::from_bits(self.agc_max_db.load(Relaxed))
    }

    pub fn set_agc_max_db(&self, db: f32) {
        let db = if db.is_finite() {
            db.clamp(AGC_MAX_DB_MIN, AGC_MAX_DB_MAX)
        } else {
            AGC_MAX_DB_DEFAULT
        };
        self.agc_max_db.store(db.to_bits(), Relaxed);
    }

    pub fn eq_gains(&self) -> [f32; 3] {
        self.eq_gains
            .each_ref()
            .map(|g| f32::from_bits(g.load(Relaxed)))
    }

    pub fn eq_preset(&self) -> EqPreset {
        EqPreset::from_u8(self.eq_preset.load(Relaxed))
    }

    pub fn set_eq_preset(&self, preset: EqPreset) {
        self.store_eq(preset.gains());
        self.eq_preset.store(preset as u8, Relaxed);
    }

    /// User-chosen band gains, each clamped to ±6 dB (NaN or infinity becomes 0).
    pub fn set_eq_custom(&self, gains: [f32; 3]) {
        self.store_eq(gains.map(|g| {
            if g.is_finite() {
                g.clamp(-EQ_DB_LIMIT, EQ_DB_LIMIT)
            } else {
                0.0
            }
        }));
        self.eq_preset.store(EqPreset::Custom as u8, Relaxed);
    }

    // The three gains are separate atomics, so the worker may see a mix of old and new
    // for one frame; it then simply fades on to the final values a frame later.
    fn store_eq(&self, gains: [f32; 3]) {
        for (a, g) in self.eq_gains.iter().zip(gains) {
            a.store(g.to_bits(), Relaxed);
        }
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
        p.set_agc_max_db(40.0);
        assert_eq!(p.agc_max_db(), AGC_MAX_DB_MAX);
        p.set_agc_max_db(f32::NAN);
        assert_eq!(p.agc_max_db(), AGC_MAX_DB_DEFAULT);
        assert_eq!(p.eq_gains(), [0.0; 3]);
        p.set_eq_preset(EqPreset::Clear);
        assert_eq!(p.eq_gains(), [0.0, -3.0, 3.0]);
        assert_eq!(p.eq_preset(), EqPreset::Clear);
        p.set_eq_custom([9.0, f32::NAN, -2.0]);
        assert_eq!(p.eq_gains(), [6.0, 0.0, -2.0]);
        assert_eq!(p.eq_preset(), EqPreset::Custom);
    }
}
