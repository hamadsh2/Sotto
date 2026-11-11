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
    /// Runs on Sotto's denoise worker thread, never inside a sound-card callback. It
    /// must not lock or do I/O, and must return well within the 10 ms frame.
    /// DeepFilterNet hands the frame to its own thread and waits a bounded time for the
    /// answer (see `dfn.rs`).
    fn process(&mut self, input: &[f32; FRAME], output: &mut [f32; FRAME]);

    /// How many samples the output lags the input. The chain delays the dry signal by
    /// the same amount, so the strength (wet/dry) mix doesn't smear or comb-filter.
    fn latency(&self) -> usize {
        0
    }

    /// Frames this engine failed to process (it passed the input through instead).
    fn errors(&self) -> u64 {
        0
    }
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

const RNNOISE_LATENCY: usize = FRAME;

/// nnnoiseless expects 16-bit-range values (-32768..32767) stored in f32.
const I16_SCALE: f32 = 32768.0;

impl Default for Rnnoise {
    fn default() -> Self {
        Self::new()
    }
}

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

    /// RNNoise analyses overlapping 20 ms windows and outputs by overlap-add, so its
    /// output trails its input by one frame (checked by `rnnoise_latency_is_one_frame`).
    fn latency(&self) -> usize {
        RNNOISE_LATENCY
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
pub(crate) mod tests {
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

    /// A synthetic voice: a glottal-like harmonic series whose pitch glides between
    /// ~100 and ~180 Hz (so it never repeats exactly and the correlation has one clear
    /// peak), shaped by two vowel formants and chopped into 4 Hz "syllables". Steady
    /// tones won't do: DeepFilterNet rightly treats them as noise and removes them.
    pub(crate) fn voice_like_pub(n: usize) -> Vec<f32> {
        voice_like(n)
    }

    fn voice_like(n: usize) -> Vec<f32> {
        use std::f32::consts::PI;
        let mut phase = 0.0f32;
        (0..n)
            .map(|i| {
                let t = i as f32 / 48_000.0;
                let f0 =
                    140.0 + 40.0 * (2.0 * PI * 0.7 * t).sin() + 8.0 * (2.0 * PI * 5.0 * t).sin();
                phase += 2.0 * PI * f0 / 48_000.0;
                if phase > 2.0 * PI {
                    phase -= 2.0 * PI;
                }
                let mut v = 0.0;
                for h in 1..=30 {
                    let fh = f0 * h as f32;
                    if fh > 8000.0 {
                        break;
                    }
                    // two formants (~700 Hz and ~1200 Hz), like an "ah"
                    let formant = 1.0 / (1.0 + ((fh - 700.0) / 150.0).powi(2))
                        + 0.6 / (1.0 + ((fh - 1200.0) / 200.0).powi(2))
                        + 0.05;
                    v += formant * (phase * h as f32).sin() / h as f32;
                }
                let syl = (2.0 * PI * 4.0 * t).sin().max(0.0);
                0.15 * syl * v
            })
            .collect()
    }

    /// Measures an engine's delay: the lag with the highest cross-correlation between
    /// input and output.
    fn measure_latency(den: &mut dyn Denoiser) -> usize {
        let n = FRAME * 300;
        let x = voice_like(n);
        let mut y = vec![0.0f32; n];
        for (xi, yo) in x
            .as_chunks::<FRAME>()
            .0
            .iter()
            .zip(y.as_chunks_mut::<FRAME>().0)
        {
            den.process(xi, yo);
        }
        let (start, max_lag) = (FRAME * 100, FRAME * 6);
        let energy = |v: &[f32]| v[start..].iter().map(|s| (*s as f64).powi(2)).sum::<f64>();
        eprintln!(
            "{}: kept {:.1} dB of the voice",
            den.name(),
            10.0 * (energy(&y) / energy(&x)).log10()
        );
        let corr = |lag: usize| -> f64 {
            (start..n - max_lag)
                .map(|i| (x[i] * y[i + lag]) as f64)
                .sum()
        };
        (0..max_lag)
            .max_by(|&a, &b| corr(a).total_cmp(&corr(b)))
            .unwrap()
    }

    #[test]
    fn rnnoise_latency_is_one_frame() {
        let mut rn = Rnnoise::new();
        let best = measure_latency(&mut rn);
        eprintln!("measured RNNoise latency: {best} samples");
        assert_eq!(best, rn.latency());
    }

    #[test]
    fn deepfilter_latency_matches_measurement() {
        let mut df = crate::dfn::DeepFilter::start(false).unwrap();
        let best = measure_latency(&mut df);
        eprintln!("measured DeepFilterNet latency: {best} samples");
        assert_eq!(best, df.latency());
        assert_eq!(df.errors(), 0);
    }

    #[test]
    fn deepfilter_attenuates_noise_and_reports_speed() {
        let mut seed: u32 = 12345;
        let mut noise = || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            ((seed >> 8) as f32 / (1 << 24) as f32 - 0.5) * 0.05
        };
        let t0 = std::time::Instant::now();
        let mut df = crate::dfn::DeepFilter::start(false).unwrap();
        eprintln!(
            "DeepFilterNet load: {:.0} ms",
            t0.elapsed().as_secs_f64() * 1e3
        );
        let (mut e_in, mut e_out) = (0.0f64, 0.0f64);
        let mut times = Vec::new();
        for frame_idx in 0..300 {
            let mut input = [0.0; FRAME];
            input.iter_mut().for_each(|s| *s = noise());
            let mut out = [0.0; FRAME];
            let t = std::time::Instant::now();
            df.process(&input, &mut out);
            times.push(t.elapsed().as_secs_f64() * 1e3);
            if frame_idx >= 100 {
                e_in += input.iter().map(|s| (*s as f64).powi(2)).sum::<f64>();
                e_out += out.iter().map(|s| (*s as f64).powi(2)).sum::<f64>();
            }
        }
        times.sort_by(f64::total_cmp);
        eprintln!(
            "DeepFilterNet per frame (this test build): p50 {:.3} ms, p99 {:.3} ms, max {:.3} ms",
            times[150], times[296], times[299]
        );
        let reduction_db = 10.0 * (e_in / e_out.max(1e-12)).log10();
        eprintln!("DeepFilterNet reduction {reduction_db:.1} dB");
        assert!(reduction_db > 6.0, "only {reduction_db:.1} dB reduction");
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

#[cfg(test)]
mod dfn_speech_probe {
    use super::*;

    #[test]
    #[ignore] // diagnostic: cargo test --release dfn_speech -- --ignored --nocapture
    fn dfn_speech_timing() {
        let x = super::tests::voice_like_pub(FRAME * 500);
        let mut df = crate::dfn::DeepFilter::start(false).unwrap();
        let mut times = Vec::new();
        let mut y = [0.0f32; FRAME];
        for xi in x.as_chunks::<FRAME>().0 {
            let t = std::time::Instant::now();
            df.process(xi, &mut y);
            times.push(t.elapsed().as_secs_f64() * 1e3);
            std::thread::sleep(std::time::Duration::from_millis(10).saturating_sub(t.elapsed()));
        }
        times.sort_by(f64::total_cmp);
        let n = times.len();
        eprintln!(
            "DFN on voice: p50 {:.3} p90 {:.3} p99 {:.3} max {:.3} ms, errors {} of {n}",
            times[n / 2],
            times[n * 9 / 10],
            times[n * 99 / 100],
            times[n - 1],
            df.errors()
        );
    }
}

#[cfg(test)]
mod dfn_stage_probe {
    use super::*;

    /// DeepFilterNet must not flip between processing modes on a steady voice: with
    /// the library defaults it flipped on ~24% of frames (heard as a "layered",
    /// unstable voice). Sotto's settings (`dfn::runtime_params`) must keep it steady.
    #[test]
    fn dfn_stage_flips() {
        use df::tract::{DfParams, DfTract};
        let n = FRAME * 1000;
        let voice = super::tests::voice_like_pub(n);
        let mut seed: u32 = 7;
        for noise_db in [-60.0f32, -45.0] {
            let amp = 10f32.powf(noise_db / 20.0) * 3.4;
            let mut m =
                DfTract::new(DfParams::default(), &crate::dfn::runtime_params(false)).unwrap();
            let mut x = ndarray::Array2::<f32>::zeros((1, FRAME));
            let mut y = ndarray::Array2::<f32>::zeros((1, FRAME));
            let (mut flips, mut prev) = (0, (false, false, false));
            for (k, chunk) in voice.as_chunks::<FRAME>().0.iter().enumerate() {
                for (d, s) in x.iter_mut().zip(chunk) {
                    seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    *d = s + ((seed >> 8) as f32 / (1 << 24) as f32 - 0.5) * amp;
                }
                let lsnr = m.process(x.view(), y.view_mut()).unwrap();
                let st = m.apply_stages(lsnr);
                if k > 0 && st != prev {
                    flips += 1;
                }
                prev = st;
            }
            eprintln!("noise {noise_db} dBFS: {flips} mode flips in 1000 frames");
            assert!(flips <= 10, "{flips} mode flips at {noise_db} dBFS noise");
        }
    }
}
