// SPDX-License-Identifier: GPL-3.0-only
//! Level-meter ballistics: how the bar moves, separate from how it's drawn, so it can
//! be tested without a window.
//!
//! - Scale: −60 dBFS … 0 dBFS mapped to 0 … 1 (decibels match how loud things sound).
//! - Fast attack: the bar jumps up to a new peak at once.
//! - Slow release: it falls back with a 300 ms time constant.
//! - Peak hold: a tick stays at the highest point for 1 s, then falls.

/// Bottom of the scale.
pub const FLOOR_DB: f32 = -60.0;
const RELEASE_S: f32 = 0.3;
const HOLD_S: f32 = 1.0;
/// After the hold, the peak tick falls this much of the scale per second.
const PEAK_FALL_PER_S: f32 = 0.6;

/// Sample peak (0..=1, linear) → bar position (0..=1).
pub fn position(peak: f32) -> f32 {
    if peak.is_nan() || peak <= 0.0 {
        return 0.0; // silence, or NaN
    }
    let db = 20.0 * peak.min(1.0).log10();
    ((db - FLOOR_DB) / -FLOOR_DB).clamp(0.0, 1.0)
}

#[derive(Default, Clone, Copy)]
pub struct Meter {
    /// Bar position, 0..=1.
    pub level: f32,
    /// Peak-hold tick position, 0..=1.
    pub peak: f32,
    held_for: f32,
}

impl Meter {
    /// Feed the loudest sample since the last update and the seconds elapsed.
    pub fn update(&mut self, sample_peak: f32, dt: f32) {
        let dt = dt.clamp(0.0, 1.0);
        let target = position(sample_peak);
        self.level = if target >= self.level {
            target
        } else {
            target + (self.level - target) * (-dt / RELEASE_S).exp()
        };
        if self.level >= self.peak {
            self.peak = self.level;
            self.held_for = 0.0;
        } else {
            self.held_for += dt;
            if self.held_for > HOLD_S {
                self.peak = (self.peak - PEAK_FALL_PER_S * dt).max(self.level);
            }
        }
    }

    /// Back to silence at once (e.g. the streams stopped).
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// True while anything is still visibly moving, so the window knows whether
    /// another frame is worth drawing.
    pub fn is_moving(&self) -> bool {
        self.level > 0.001 || self.peak > 0.001
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DT: f32 = 1.0 / 30.0;

    #[test]
    fn scale_is_in_decibels() {
        assert_eq!(position(1.0), 1.0);
        assert!((position(0.1) - 2.0 / 3.0).abs() < 1e-5); // −20 dB
        assert!((position(0.001) - 0.0).abs() < 1e-5); // −60 dB
        assert_eq!(position(0.0), 0.0);
        assert_eq!(position(f32::NAN), 0.0);
        assert_eq!(position(5.0), 1.0);
    }

    #[test]
    fn attack_is_instant() {
        let mut m = Meter::default();
        m.update(0.5, DT);
        assert_eq!(m.level, position(0.5));
    }

    #[test]
    fn release_takes_about_300_ms() {
        let mut m = Meter::default();
        m.update(1.0, DT);
        // After one time constant the bar has fallen ~63% of the way to silence.
        for _ in 0..9 {
            m.update(0.0, DT);
        }
        assert!(
            (m.level - (-1.0f32).exp()).abs() < 0.02,
            "level {}",
            m.level
        );
        // and it doesn't drop to zero in a single frame
        let mut m = Meter::default();
        m.update(1.0, DT);
        m.update(0.0, DT);
        assert!(m.level > 0.85);
    }

    #[test]
    fn peak_holds_one_second_then_falls() {
        let mut m = Meter::default();
        m.update(1.0, DT);
        for _ in 0..29 {
            m.update(0.0, DT); // 0.97 s
        }
        assert_eq!(m.peak, 1.0, "still held just before 1 s");
        for _ in 0..6 {
            m.update(0.0, DT); // 1.17 s
        }
        assert!(
            m.peak < 1.0 && m.peak > 0.8,
            "falling after 1 s: {}",
            m.peak
        );
        for _ in 0..90 {
            m.update(0.0, DT);
        }
        assert!(!m.is_moving(), "settles to rest: {} {}", m.level, m.peak);
    }

    #[test]
    fn peak_never_below_bar() {
        let mut m = Meter::default();
        for i in 0..200 {
            let x = ((i as f32) * 0.37).sin().abs();
            m.update(x, DT);
            assert!(m.peak >= m.level);
        }
    }
}
