// SPDX-License-Identifier: GPL-3.0-only
//! DeepFilterNet 3 — "High quality — for recording & calls".
//!
//! Uses the `deep_filter` crate (imported as `df`): tract ONNX inference with the
//! DeepFilterNet3 model embedded in the binary. Code and weights are MIT/Apache-2.0.
//!
//! **Why a thread of its own.** The model object holds `Rc` pointers internally. `Rc` is
//! a reference-counted pointer that is *not* thread-safe (Python's refcount is protected
//! by the GIL; Rust has no GIL, so it simply refuses to let an `Rc` cross threads). So
//! the model can't be built on one thread and handed to another. Instead, a dedicated
//! "sotto-dfn" thread loads the model and keeps it for its whole life. `DeepFilter`,
//! the object the processing chain holds, is a thin proxy that passes each frame to
//! that thread through two lock-free ring buffers and waits for the answer:
//!
//! ```text
//! denoise worker ──frame──► [ring] ──► sotto-dfn thread (model lives here)
//!        ▲                                        │
//!        └──────── cleaned frame ◄── [ring] ◄─────┘   (+ unpark the worker)
//! ```
//!
//! Bonus: loading (hundreds of ms) happens on that thread too, and if the library ever
//! panics only that thread dies — the proxy notices and passes audio through.
//!
//! The waiting is bounded: if no answer arrives within `DEADLINE`, the original audio
//! is used for that frame, so a slow moment can never stall the output.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::mpsc;
use std::thread::{self, JoinHandle, Thread};
use std::time::{Duration, Instant};

use ringbuf::traits::{Consumer, Observer, Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};

use crate::denoise::{Denoiser, FRAME};

/// Longest the worker waits for a frame (the whole frame period is 10 ms).
const DEADLINE: Duration = Duration::from_millis(7);
/// How long `start` waits for the model to load before giving up.
const LOAD_TIMEOUT: Duration = Duration::from_secs(60);
/// Most frames allowed in flight. Beyond this the model is badly behind (CPU
/// starvation, sleep/resume): stop queueing and pass audio through until it catches up.
const MAX_IN_FLIGHT: usize = 3;

/// Model settings. DeepFilterNet estimates each frame's local SNR and, by these
/// thresholds, chooses: zero mask (noise only) / full processing / first stage only /
/// no processing (already clean). The library's `RuntimeParams::default()` uses
/// −10 / 30 / 20 dB; with a close mic a clean voice sits around 20–30 dB, so the mode
/// flipped on ~24% of frames — heard as an unstable, "layered" voice (user report,
/// Phase 2b). DeepFilterNet's own tools (`enhance_wav`, the C API) use −15 / 35 / 35,
/// which keeps a voice on full processing with no flips (`dfn_stage_flips` test).
///
/// `post_filter` turns on DeepFilterNet's post-filter, which pushes leftover noise
/// further down (stronger isolation, voice can sound slightly thinner). β = 0.02 is
/// the value DeepFilterNet's own tools use when it is enabled.
pub(crate) fn runtime_params(post_filter: bool) -> df::tract::RuntimeParams {
    let p = df::tract::RuntimeParams::default_with_ch(1).with_thresholds(-15.0, 35.0, 35.0);
    if post_filter {
        p.with_post_filter(POST_FILTER_BETA)
    } else {
        p
    }
}

const POST_FILTER_BETA: f32 = 0.02;

/// State shared between the proxy and the model thread. Atomics only.
struct Shared {
    quit: AtomicBool,
    alive: AtomicBool,
    errors: AtomicU64,
    latency: AtomicUsize,
    /// Test hook: the model thread sleeps this many ms once, to simulate a stall.
    #[cfg(test)]
    stall_ms: AtomicU64,
}

/// The proxy the processing chain owns. `Send` (it holds only ring halves, atomics,
/// and thread handles), so it can be created on the main thread and moved to the worker.
pub struct DeepFilter {
    to_model: HeapProd<f32>,
    from_model: HeapCons<f32>,
    shared: Arc<Shared>,
    model_thread: Thread,
    join: Option<JoinHandle<()>>,
    /// Sends the worker's thread handle once, so the model thread can wake it.
    waker_tx: Option<mpsc::Sender<Thread>>,
    /// Frames sent whose answer hasn't arrived yet. Answers that arrive after their
    /// deadline are discarded at the next call.
    outstanding: usize,
    timeouts: u64,
    latency: usize,
    /// The input delayed by `latency` samples. When an answer is late (or the model is
    /// gone), this is what we output: the original audio at the *same* delay as the
    /// model's output, so the timeline never jumps (no click, and no echo at partial
    /// strength). Allocated once in `start`.
    history: Vec<f32>,
    history_pos: usize,
    delayed: [f32; FRAME],
}

impl DeepFilter {
    /// Spawn the model thread, load the model there, and wait until it's ready.
    /// Call from the main thread (it blocks while loading), never from the audio path.
    pub fn start(post_filter: bool) -> Result<Self, String> {
        let (to_model, model_in) = HeapRb::<f32>::new(FRAME * 4).split();
        // The answer ring is larger than the request ring, so the model thread can never
        // find it full and drop an answer (at most MAX_IN_FLIGHT + 1 are ever pending).
        let (model_out, from_model) = HeapRb::<f32>::new(FRAME * 8).split();
        let shared = Arc::new(Shared {
            quit: AtomicBool::new(false),
            alive: AtomicBool::new(true),
            errors: AtomicU64::new(0),
            latency: AtomicUsize::new(0),
            #[cfg(test)]
            stall_ms: AtomicU64::new(0),
        });
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
        let (waker_tx, waker_rx) = mpsc::channel::<Thread>();
        let sh = Arc::clone(&shared);
        let join = thread::Builder::new()
            .name("sotto-dfn".into())
            .spawn(move || model_thread(model_in, model_out, sh, ready_tx, waker_rx, post_filter))
            .map_err(|e| format!("could not start DeepFilterNet thread: {e}"))?;
        let model_thread = join.thread().clone();

        let mut me = Self {
            to_model,
            from_model,
            shared,
            model_thread,
            join: Some(join),
            waker_tx: Some(waker_tx),
            outstanding: 0,
            timeouts: 0,
            latency: 0,
            history: Vec::new(),
            history_pos: 0,
            delayed: [0.0; FRAME],
        };
        // On error, `me` is dropped here, which stops and joins the (finished) thread.
        match ready_rx.recv_timeout(LOAD_TIMEOUT) {
            Ok(Ok(())) => {
                me.latency = me.shared.latency.load(Relaxed);
                me.history = vec![0.0; me.latency.max(1)];
                Ok(me)
            }
            Ok(Err(e)) => Err(e),
            Err(_) => {
                // Still stuck inside the library's loader, which can't be interrupted.
                // Don't wait for it (that would hang Sotto): detach the thread. Drop sets
                // `quit`, so it exits on its own once loading ends.
                me.join.take();
                Err("DeepFilterNet did not finish loading".into())
            }
        }
    }

    /// Push this frame into the delay line and fill `self.delayed` with the audio from
    /// `latency` samples ago.
    fn update_delayed(&mut self, input: &[f32; FRAME]) {
        if self.latency == 0 {
            self.delayed = *input;
            return;
        }
        let len = self.history.len();
        for (d, x) in self.delayed.iter_mut().zip(input) {
            *d = self.history[self.history_pos];
            self.history[self.history_pos] = *x;
            self.history_pos += 1;
            if self.history_pos == len {
                self.history_pos = 0;
            }
        }
    }

    fn fallback(&mut self, output: &mut [f32; FRAME]) {
        output.copy_from_slice(&self.delayed);
        self.timeouts += 1;
    }
}

impl Denoiser for DeepFilter {
    fn name(&self) -> &'static str {
        "deepfilternet"
    }

    fn latency(&self) -> usize {
        self.latency
    }

    fn errors(&self) -> u64 {
        self.timeouts + self.shared.errors.load(Relaxed)
    }

    fn process(&mut self, input: &[f32; FRAME], output: &mut [f32; FRAME]) {
        self.update_delayed(input);
        if !self.shared.alive.load(Relaxed) {
            return self.fallback(output);
        }
        // First call happens on the worker thread: tell the model thread who to wake.
        // (One small allocation, once, on the worker — never in a sound-card callback.)
        if let Some(tx) = self.waker_tx.take() {
            let _ = tx.send(thread::current());
        }
        // Answers already here belong to earlier frames that missed their deadline:
        // discard them. Answers arrive in order, so what's left in flight is exact.
        let late = (self.from_model.occupied_len() / FRAME).min(self.outstanding);
        self.from_model.skip(late * FRAME);
        self.outstanding -= late;
        if self.outstanding >= MAX_IN_FLIGHT || self.to_model.vacant_len() < FRAME {
            // The model is badly behind; don't pile on. It recovers by itself once
            // those frames are answered.
            return self.fallback(output);
        }
        self.to_model.push_slice(input);
        self.outstanding += 1;
        self.model_thread.unpark();

        // Wait for this frame's answer: with `outstanding` frames in flight, ours is the
        // last one.
        let deadline = Instant::now() + DEADLINE;
        loop {
            if self.from_model.occupied_len() >= self.outstanding * FRAME {
                self.from_model.skip((self.outstanding - 1) * FRAME);
                self.from_model.pop_slice(output);
                self.outstanding = 0;
                return;
            }
            let now = Instant::now();
            if now >= deadline || !self.shared.alive.load(Relaxed) {
                return self.fallback(output);
            }
            // Woken by the model thread when it finishes (or by the mic — harmless).
            thread::park_timeout(deadline - now);
        }
    }
}

impl Drop for DeepFilter {
    /// Stop the model thread and wait for it, which frees the model's memory.
    /// Runs on the main thread (the worker hands retired engines back for dropping).
    fn drop(&mut self) {
        self.shared.quit.store(true, Relaxed);
        self.model_thread.unpark();
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetCurrentThread() -> *mut core::ffi::c_void;
    fn SetThreadPriority(thread: *mut core::ffi::c_void, priority: i32) -> i32;
}
const THREAD_PRIORITY_TIME_CRITICAL: i32 = 15;

/// Clears `alive` when the model thread ends for any reason, including a panic.
struct AliveGuard(Arc<Shared>);
impl Drop for AliveGuard {
    fn drop(&mut self) {
        self.0.alive.store(false, Relaxed);
    }
}

fn model_thread(
    mut input: HeapCons<f32>,
    mut output: HeapProd<f32>,
    shared: Arc<Shared>,
    ready: mpsc::Sender<Result<(), String>>,
    waker_rx: mpsc::Receiver<Thread>,
    post_filter: bool,
) {
    let _guard = AliveGuard(Arc::clone(&shared));
    // Same priority as the denoise worker: this thread is part of the audio path.
    // Changes only this thread — no system setting.
    // SAFETY: GetCurrentThread returns a pseudo-handle for the calling thread.
    unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL) };

    use df::tract::{DfParams, DfTract};
    // The crate panics (unwrap/expect) on a malformed model; catch that so it becomes
    // an error message. Python analogy: try/except around the load.
    let loaded = std::panic::catch_unwind(|| {
        DfTract::new(DfParams::default(), &runtime_params(post_filter))
    });
    let mut model = match loaded {
        Ok(Ok(m)) => m,
        Ok(Err(e)) => {
            let _ = ready.send(Err(format!("DeepFilterNet model failed to load: {e}")));
            return;
        }
        Err(_) => {
            let _ = ready.send(Err("DeepFilterNet model failed to load".into()));
            return;
        }
    };
    if model.sr != 48_000 || model.hop_size != FRAME {
        let _ = ready.send(Err(format!(
            "DeepFilterNet model expects {} Hz / {}-sample frames; Sotto uses 48000 / {FRAME}",
            model.sr, model.hop_size
        )));
        return;
    }
    // STFT analysis/synthesis costs one hop; the model looks `lookahead` hops ahead.
    // Checked by the test `deepfilter_latency_matches_measurement`.
    shared
        .latency
        .store(model.hop_size * (model.lookahead + 1), Relaxed);

    // ndarray 2-D arrays (shape [channels, samples]) — like numpy arrays. Allocated
    // once and reused.
    let mut noisy = ndarray::Array2::<f32>::zeros((1, FRAME));
    let mut enh = ndarray::Array2::<f32>::zeros((1, FRAME));
    let mut frame = [0.0f32; FRAME];
    let mut clean = [0.0f32; FRAME];
    // Warm-up: the first inference sets up internal buffers; do it now, not mid-stream.
    let _ = model.process(noisy.view(), enh.view_mut());
    let _ = ready.send(Ok(()));

    let mut waker: Option<Thread> = None;
    while !shared.quit.load(Relaxed) {
        if waker.is_none() {
            waker = waker_rx.try_recv().ok();
        }
        if input.occupied_len() < FRAME {
            thread::park_timeout(Duration::from_millis(50));
            continue;
        }
        #[cfg(test)]
        {
            let ms = shared.stall_ms.swap(0, Relaxed);
            if ms > 0 {
                thread::sleep(Duration::from_millis(ms));
            }
        }
        input.pop_slice(&mut frame);
        for (d, s) in noisy.iter_mut().zip(&frame) {
            *d = *s;
        }
        let ok = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            model.process(noisy.view(), enh.view_mut()).is_ok()
        }));
        if !matches!(ok, Ok(true)) {
            // An error or panic leaves the model in an unknown state: stop using it.
            // Ending this thread clears `alive` (AliveGuard), and the proxy then plays
            // the delay-matched original audio by itself, with no round trip.
            shared.errors.fetch_add(1, Relaxed);
            shared.alive.store(false, Relaxed);
            if let Some(w) = &waker {
                w.unpark();
            }
            return;
        }
        for (c, e) in clean.iter_mut().zip(enh.iter()) {
            *c = if e.is_finite() {
                e.clamp(-1.0, 1.0)
            } else {
                0.0
            };
        }
        if output.vacant_len() >= FRAME {
            output.push_slice(&clean);
        }
        // The worker's handle may have arrived while we were processing.
        if waker.is_none() {
            waker = waker_rx.try_recv().ok();
        }
        if let Some(w) = &waker {
            w.unpark();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovers_after_a_long_stall() {
        // Freeze the model thread for 120 ms (12 frames: CPU starvation, sleep and
        // resume). Frames during the stall fall back to the delayed original audio;
        // afterwards the model must answer again by itself.
        let mut df = DeepFilter::start(false).unwrap();
        let x = [0.01f32; FRAME];
        let mut y = [0.0f32; FRAME];
        // Real audio arrives every 10 ms; pace the frames the same way.
        let mut frame = |df: &mut DeepFilter| {
            let t = Instant::now();
            df.process(&x, &mut y);
            thread::sleep(Duration::from_millis(10).saturating_sub(t.elapsed()));
        };
        for _ in 0..20 {
            frame(&mut df);
        }
        let before = df.errors();
        df.shared.stall_ms.store(120, Relaxed);
        for _ in 0..30 {
            frame(&mut df); // 300 ms: the 120 ms stall happens in here
        }
        assert!(
            df.errors() > before,
            "the stall should have caused fallbacks"
        );
        let settled = df.errors();
        for _ in 0..50 {
            frame(&mut df);
        }
        assert_eq!(
            df.errors(),
            settled,
            "DeepFilterNet did not recover after the stall"
        );
        assert!(df.shared.alive.load(Relaxed));
    }

    #[test]
    fn fallback_is_delay_matched() {
        // With the model gone, the output is the input delayed by exactly `latency`.
        let mut df = DeepFilter::start(false).unwrap();
        df.shared.alive.store(false, Relaxed); // pretend the model thread died
        let lat = df.latency();
        let n = FRAME * 6;
        let x: Vec<f32> = (0..n).map(|i| i as f32 / n as f32).collect();
        let mut y = vec![0.0f32; n];
        for (xi, yo) in x
            .as_chunks::<FRAME>()
            .0
            .iter()
            .zip(y.as_chunks_mut::<FRAME>().0)
        {
            df.process(xi, yo);
        }
        for i in lat..n {
            assert_eq!(y[i], x[i - lat]);
        }
    }

    /// The post-filter must remove more of a steady noise than the plain model.
    #[test]
    fn post_filter_removes_more_noise() {
        let residual = |pf: bool| {
            let mut df = DeepFilter::start(pf).unwrap();
            let mut seed: u32 = 11;
            let mut out = [0.0f32; FRAME];
            let (mut sum, mut n) = (0.0f64, 0usize);
            for k in 0..300 {
                let mut x = [0.0f32; FRAME];
                for s in &mut x {
                    seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    *s = ((seed >> 8) as f32 / (1 << 24) as f32 - 0.5) * 0.02;
                }
                df.process(&x, &mut out);
                if k >= 100 {
                    sum += out.iter().map(|v| (*v as f64).powi(2)).sum::<f64>();
                    n += FRAME;
                }
                thread::sleep(Duration::from_millis(2));
            }
            10.0 * (sum / n as f64).max(1e-20).log10()
        };
        let (off, on) = (residual(false), residual(true));
        eprintln!("residual noise: post-filter off {off:.1} dBFS, on {on:.1} dBFS");
        assert!(on <= off, "post-filter on {on:.1} > off {off:.1}");
    }
}
