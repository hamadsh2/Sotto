// SPDX-License-Identifier: GPL-3.0-only
//! Denoise engines behind one common `Denoiser` trait.
//!
//! Python analogy: a `trait` is like an abstract base class (`abc.ABC`) — it lists the
//! methods every engine must have. The engine loop holds a `Box<dyn Denoiser>`, i.e.
//! "some object that implements Denoiser", so engines can be swapped at runtime.

/// Samples per frame: 10 ms at 48 kHz. Every engine works on frames of this size.
pub const FRAME: usize = 480;

/// A noise-suppression engine.
///
/// `Send` means "safe to move to another thread" — the engine is created on the main
/// thread and then moved into the audio thread, so this is required.
pub trait Denoiser: Send {
    /// Short name for logs.
    fn name(&self) -> &'static str;

    /// Process exactly one frame. Samples are f32 in [-1.0, 1.0].
    ///
    /// Must be real-time safe: no allocation, no locks, no I/O.
    fn process(&mut self, input: &[f32; FRAME], output: &mut [f32; FRAME]);
}

/// Passthrough — for before/after comparison.
pub struct Off;

impl Denoiser for Off {
    fn name(&self) -> &'static str {
        "off"
    }

    fn process(&mut self, input: &[f32; FRAME], output: &mut [f32; FRAME]) {
        output.copy_from_slice(input);
    }
}

/// RNNoise via the pure-Rust `nnnoiseless` port. "Light — for gaming".
pub struct Rnnoise {
    state: Box<nnnoiseless::DenoiseState<'static>>,
    // Scratch buffers, allocated once here so `process` never allocates.
    scaled_in: [f32; FRAME],
    scaled_out: [f32; FRAME],
}

// nnnoiseless works on 480-sample frames too, so no frame-size adapter is needed.
const _: () = assert!(nnnoiseless::FRAME_SIZE == FRAME);

/// nnnoiseless expects 16-bit-range values (-32768..32767) stored in f32.
const I16_SCALE: f32 = 32768.0;

impl Rnnoise {
    pub fn new() -> Self {
        Self {
            state: nnnoiseless::DenoiseState::new(),
            scaled_in: [0.0; FRAME],
            scaled_out: [0.0; FRAME],
        }
    }
}

impl Denoiser for Rnnoise {
    fn name(&self) -> &'static str {
        "rnnoise"
    }

    fn process(&mut self, input: &[f32; FRAME], output: &mut [f32; FRAME]) {
        for (dst, src) in self.scaled_in.iter_mut().zip(input) {
            *dst = src * I16_SCALE;
        }
        // Returns voice-activity probability; unused in Phase 1.
        let _vad = self
            .state
            .process_frame(&mut self.scaled_out, &self.scaled_in);
        for (dst, src) in output.iter_mut().zip(&self.scaled_out) {
            *dst = (src / I16_SCALE).clamp(-1.0, 1.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_is_passthrough() {
        let mut input = [0.0; FRAME];
        for (i, s) in input.iter_mut().enumerate() {
            *s = (i as f32 / FRAME as f32) - 0.5;
        }
        let mut out = [0.0; FRAME];
        Off.process(&input, &mut out);
        assert_eq!(input, out);
    }

    #[test]
    fn rnnoise_attenuates_white_noise() {
        // Deterministic pseudo-random noise (no `rand` crate needed), about -37 dBFS RMS —
        // a steady fan-like background level. RNNoise is tuned for real-world noise, so
        // this is a smoke test that it suppresses at all, not a quality benchmark.
        let mut seed: u32 = 12345;
        let mut noise = || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            ((seed >> 8) as f32 / (1 << 24) as f32 - 0.5) * 0.05
        };
        let mut rn = Rnnoise::new();
        let (mut e_in, mut e_out) = (0.0f64, 0.0f64);
        for frame_idx in 0..300 {
            let mut input = [0.0; FRAME];
            input.iter_mut().for_each(|s| *s = noise());
            let mut out = [0.0; FRAME];
            rn.process(&input, &mut out);
            if frame_idx >= 100 {
                // let the model settle first
                e_in += input.iter().map(|s| (*s as f64).powi(2)).sum::<f64>();
                e_out += out.iter().map(|s| (*s as f64).powi(2)).sum::<f64>();
            }
        }
        let reduction_db = 10.0 * (e_in / e_out.max(1e-12)).log10();
        eprintln!("reduction {reduction_db:.1} dB");
        assert!(reduction_db > 6.0, "only {reduction_db:.1} dB reduction");
    }
}
