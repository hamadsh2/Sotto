// SPDX-License-Identifier: GPL-3.0-only
//! EQ: three gentle bands that shape the voice's tone, after the denoiser.
//!
//! - **Body**: low shelf around 150 Hz (warmth, "fullness").
//! - **Mud**: bell around 350 Hz (cutting it removes a boxy, muffled sound).
//! - **Presence**: bell around 4 kHz (clarity, how well words cut through).
//!
//! Each band is a biquad, the same kind of tiny filter as the high-pass: five
//! multiplications per sample. Presets are just named gains for the three bands
//! (see `params::EqPreset`).
//!
//! Python analogy for changing settings: we don't overwrite the filter in place (that
//! could click). We copy it, give the copy the new settings, run both side by side for
//! 20 ms and fade from the old output to the new one — like crossfading two tracks.

use crate::denoise::FRAME;

const SR: f64 = 48_000.0;

pub const BODY_HZ: f64 = 150.0;
pub const MUD_HZ: f64 = 350.0;
const MUD_Q: f64 = 1.0;
pub const PRESENCE_HZ: f64 = 4000.0;
const PRESENCE_Q: f64 = 0.9;

/// Length of the crossfade when the settings change (20 ms).
const FADE: usize = 960;

/// A biquad filter in f64, transposed direct form II, with Robert Bristow-Johnson's
/// cookbook designs for the two shapes the EQ needs.
#[derive(Clone, Copy)]
struct Biquad {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
    z1: f64,
    z2: f64,
}

impl Biquad {
    /// Passes the signal through unchanged.
    const IDENTITY: Biquad = Biquad {
        b0: 1.0,
        b1: 0.0,
        b2: 0.0,
        a1: 0.0,
        a2: 0.0,
        z1: 0.0,
        z2: 0.0,
    };

    /// New coefficients (normalised by a0); the filter state is kept.
    fn set(&mut self, b: [f64; 3], a: [f64; 3]) {
        self.b0 = b[0] / a[0];
        self.b1 = b[1] / a[0];
        self.b2 = b[2] / a[0];
        self.a1 = a[1] / a[0];
        self.a2 = a[2] / a[0];
    }

    /// 0 dB is exactly a pass-through (no rounding at all).
    fn identity(&mut self) {
        self.set([1.0, 0.0, 0.0], [1.0, 0.0, 0.0]);
    }

    /// Bell around `hz`: `db` at the centre, back to 0 dB away from it.
    fn peak(&mut self, hz: f64, q: f64, db: f32) {
        if db == 0.0 {
            return self.identity();
        }
        let a = 10f64.powf(db as f64 / 40.0);
        let (sin, cos) = (2.0 * std::f64::consts::PI * hz / SR).sin_cos();
        let alpha = sin / (2.0 * q);
        self.set(
            [1.0 + alpha * a, -2.0 * cos, 1.0 - alpha * a],
            [1.0 + alpha / a, -2.0 * cos, 1.0 - alpha / a],
        );
    }

    /// Low shelf: `db` well below `hz`, 0 dB well above it (shelf slope S = 1).
    fn low_shelf(&mut self, hz: f64, db: f32) {
        if db == 0.0 {
            return self.identity();
        }
        let a = 10f64.powf(db as f64 / 40.0);
        let (sin, cos) = (2.0 * std::f64::consts::PI * hz / SR).sin_cos();
        let k = 2.0 * a.sqrt() * sin / std::f64::consts::SQRT_2; // 2·√A·alpha
        self.set(
            [
                a * ((a + 1.0) - (a - 1.0) * cos + k),
                2.0 * a * ((a - 1.0) - (a + 1.0) * cos),
                a * ((a + 1.0) - (a - 1.0) * cos - k),
            ],
            [
                (a + 1.0) + (a - 1.0) * cos + k,
                -2.0 * ((a - 1.0) + (a + 1.0) * cos),
                (a + 1.0) + (a - 1.0) * cos - k,
            ],
        );
    }

    fn process(&mut self, x: f64) -> f64 {
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        if !y.is_finite() {
            // Can't happen with sanitized input; recover instead of ringing NaN forever.
            self.z1 = 0.0;
            self.z2 = 0.0;
            return 0.0;
        }
        // Avoid slow "denormal" numbers as the state decays through silence.
        if self.z1.abs() < 1e-30 {
            self.z1 = 0.0;
        }
        if self.z2.abs() < 1e-30 {
            self.z2 = 0.0;
        }
        y
    }

    /// Magnitude response at `hz`, in dB (tests only).
    #[cfg(test)]
    fn mag_db(&self, hz: f64) -> f64 {
        let w = 2.0 * std::f64::consts::PI * hz / SR;
        let (c1, s1, c2, s2) = (w.cos(), w.sin(), (2.0 * w).cos(), (2.0 * w).sin());
        let num = ((self.b0 + self.b1 * c1 + self.b2 * c2).powi(2)
            + (self.b1 * s1 + self.b2 * s2).powi(2))
        .sqrt();
        let den = ((1.0 + self.a1 * c1 + self.a2 * c2).powi(2)
            + (self.a1 * s1 + self.a2 * s2).powi(2))
        .sqrt();
        20.0 * (num / den).log10()
    }
}

/// The three bands in series.
#[derive(Clone, Copy)]
struct Bands([Biquad; 3]);

impl Bands {
    fn design(&mut self, g: [f32; 3]) {
        self.0[0].low_shelf(BODY_HZ, g[0]);
        self.0[1].peak(MUD_HZ, MUD_Q, g[1]);
        self.0[2].peak(PRESENCE_HZ, PRESENCE_Q, g[2]);
    }

    fn process(&mut self, x: f32) -> f32 {
        let mut y = x as f64;
        for b in &mut self.0 {
            y = b.process(y);
        }
        y as f32
    }

    fn reset(&mut self) {
        for b in &mut self.0 {
            b.z1 = 0.0;
            b.z2 = 0.0;
        }
    }
}

pub struct Eq {
    cur: Bands,
    next: Bands,
    cur_gains: [f32; 3],
    next_gains: [f32; 3],
    /// Samples into a settings crossfade, or `None`.
    fade: Option<usize>,
}

impl Eq {
    pub fn new(gains: [f32; 3]) -> Self {
        let mut cur = Bands([Biquad::IDENTITY; 3]);
        cur.design(gains);
        Self {
            cur,
            next: cur,
            cur_gains: gains,
            next_gains: gains,
            fade: None,
        }
    }

    /// Filter one frame in place, moving toward `gains` (body, mud, presence in dB).
    /// Allocation-free; a change costs one coefficient design (a few `sin`/`powf`).
    pub fn process(&mut self, buf: &mut [f32; FRAME], gains: [f32; 3]) {
        if self.fade.is_none() && gains != self.cur_gains {
            // Start the copy from the same state, so both outputs begin close together.
            self.next = self.cur;
            self.next.design(gains);
            self.next_gains = gains;
            self.fade = Some(0);
        }
        match self.fade {
            None => {
                for x in buf.iter_mut() {
                    *x = self.cur.process(*x);
                }
            }
            Some(mut pos) => {
                // Both see the same input, so their outputs are strongly correlated and
                // a linear crossfade keeps the level steady.
                for x in buf.iter_mut() {
                    let a = self.cur.process(*x);
                    let b = self.next.process(*x);
                    pos += 1;
                    let t = (pos as f32 / FADE as f32).min(1.0);
                    *x = a + (b - a) * t;
                }
                if pos >= FADE {
                    self.cur = self.next;
                    self.cur_gains = self.next_gains;
                    self.fade = None;
                } else {
                    self.fade = Some(pos);
                }
            }
        }
    }

    /// Forget past audio (after a device reconnect).
    pub fn reset(&mut self) {
        self.cur.reset();
        self.next.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::params::EqPreset;

    fn designed(g: [f32; 3]) -> Bands {
        let mut b = Bands([Biquad::IDENTITY; 3]);
        b.design(g);
        b
    }

    fn total_db(b: &Bands, hz: f64) -> f64 {
        b.0.iter().map(|q| q.mag_db(hz)).sum()
    }

    /// Gain of a steady sine through the EQ, measured on the output (not the formula).
    fn measured_db(gains: [f32; 3], hz: f64) -> f64 {
        let mut eq = Eq::new(gains);
        let mut buf = [0.0f32; FRAME];
        let (mut si, mut so) = (0.0f64, 0.0f64);
        for k in 0..100 {
            for (i, x) in buf.iter_mut().enumerate() {
                let n = (k * FRAME + i) as f64;
                *x = (0.25 * (2.0 * std::f64::consts::PI * hz * n / SR).sin()) as f32;
            }
            let input = buf;
            eq.process(&mut buf, gains);
            if k >= 50 {
                si += input.iter().map(|v| (*v as f64).powi(2)).sum::<f64>();
                so += buf.iter().map(|v| (*v as f64).powi(2)).sum::<f64>();
            }
        }
        10.0 * (so / si).log10()
    }

    #[test]
    fn natural_is_an_exact_passthrough() {
        let mut eq = Eq::new(EqPreset::Natural.gains());
        let mut buf = [0.0f32; FRAME];
        let mut seed: u32 = 3;
        for _ in 0..50 {
            for x in &mut buf {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                *x = (seed >> 8) as f32 / (1 << 24) as f32 - 0.5;
            }
            let input = buf;
            eq.process(&mut buf, [0.0; 3]);
            assert_eq!(buf, input);
        }
    }

    #[test]
    fn bands_hit_their_design_gain() {
        // Each band alone, at its own frequency, gives its gain; far away, ~0 dB.
        let shelf = designed([3.0, 0.0, 0.0]);
        assert!((total_db(&shelf, 20.0) - 3.0).abs() < 0.2);
        assert!(total_db(&shelf, 3000.0).abs() < 0.1);
        let mud = designed([0.0, -3.0, 0.0]);
        assert!((total_db(&mud, MUD_HZ) + 3.0).abs() < 1e-6);
        assert!(total_db(&mud, 5000.0).abs() < 0.1);
        let pres = designed([0.0, 0.0, 3.0]);
        assert!((total_db(&pres, PRESENCE_HZ) - 3.0).abs() < 1e-6);
        assert!(total_db(&pres, 100.0).abs() < 0.1);
    }

    #[test]
    fn presets_are_mild_and_audio_matches_the_design() {
        for preset in EqPreset::NAMED {
            let g = preset.gains();
            let b = designed(g);
            // Never more than ±3.5 dB anywhere from 20 Hz to 20 kHz.
            let mut hz = 20.0;
            while hz < 20_000.0 {
                let d = total_db(&b, hz);
                assert!(d.abs() <= 3.5, "{} at {hz:.0} Hz: {d:.2} dB", preset.name());
                hz *= 1.05;
            }
            // The real filtered audio matches the formula (within 0.1 dB).
            for hz in [100.0, MUD_HZ, 1000.0, PRESENCE_HZ] {
                let (m, d) = (measured_db(g, hz), total_db(&b, hz));
                assert!(
                    (m - d).abs() < 0.1,
                    "{} {hz} Hz: {m:.2} vs {d:.2}",
                    preset.name()
                );
            }
        }
    }

    #[test]
    fn changing_preset_never_jumps() {
        let hz = 1000.0;
        let max_slope = 0.5 * (2.0 * std::f64::consts::PI * hz / SR).sin() as f32;
        let mut eq = Eq::new([0.0; 3]);
        let mut buf = [0.0f32; FRAME];
        let mut prev = 0.0f32;
        let mut worst = 0.0f32;
        let presets = [
            EqPreset::Clear,
            EqPreset::Warm,
            EqPreset::Broadcast,
            EqPreset::Natural,
        ];
        for k in 0..200 {
            for (i, x) in buf.iter_mut().enumerate() {
                let n = (k * FRAME + i) as f64;
                *x = (0.5 * (2.0 * std::f64::consts::PI * hz * n / SR).sin()) as f32;
            }
            // A new preset every 100 ms, sometimes mid-fade.
            let g = presets[(k / 10) % presets.len()].gains();
            eq.process(&mut buf, g);
            for s in buf {
                if k > 0 {
                    worst = worst.max((s - prev).abs());
                }
                prev = s;
            }
        }
        // ±3 dB at most on a 1 kHz tone: allow 1.5× the plain tone's steepest step.
        assert!(
            worst < max_slope * 1.5,
            "worst step {worst}, tone {max_slope}"
        );
    }

    #[test]
    fn nan_never_sticks() {
        let mut eq = Eq::new(EqPreset::Broadcast.gains());
        let mut buf = [f32::NAN; FRAME];
        eq.process(&mut buf, EqPreset::Broadcast.gains());
        let mut buf = [0.1f32; FRAME];
        eq.process(&mut buf, EqPreset::Broadcast.gains());
        eq.process(&mut buf, EqPreset::Broadcast.gains());
        assert!(buf.iter().all(|x| x.is_finite()));
    }
}
