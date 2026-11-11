// SPDX-License-Identifier: GPL-3.0-only
//! The real-time audio path:
//!
//! ```text
//!  mic ─(cpal capture thread)─► downmix to mono ─► raw ring ─┐
//!                                                            ▼
//!                               Sotto's denoise worker thread: 480-sample frame → denoise
//!                                                            │
//!  CABLE Input ◄─(cpal render thread)─ pull samples ◄─ clean ring
//! ```
//!
//! cpal runs each stream's callback on its own high-priority thread (shared-mode WASAPI —
//! cpal 0.18 hard-codes AUDCLNT_SHAREMODE_SHARED). These callbacks must never allocate,
//! lock, print, or do I/O: anything slow there is an audible glitch.
//!
//! Why a separate worker thread for denoising? nnnoiseless sets up its FFT tables the
//! first time it runs *on each thread* (allocating and leaking a little memory). cpal
//! creates new threads every time a stream is (re)opened, so denoising inside the cpal
//! callback would allocate on every reconnect. The worker lives for the whole program,
//! is warmed up once at startup, and then never allocates.
//!
//! Python analogy: a callback is like a function you hand to a library that calls it
//! from its own thread ~100×/second. The `move |...| { ... }` closure "captures" the
//! variables it uses and takes ownership of them — like a lambda that also owns its
//! state, so nothing else can touch that state concurrently.

use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, StreamTrait};
use cpal::{Device, ErrorKind, SampleFormat, Stream, StreamConfig};
use ringbuf::traits::{Consumer, Observer, Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};

use crate::denoise::{Denoiser, FRAME};
use crate::stats::Stats;

pub const SAMPLE_RATE: u32 = 48_000;

/// Capacity of each ring buffer: 200 ms. A hard ceiling; normal operation stays near TARGET.
const RING_CAPACITY: usize = SAMPLE_RATE as usize / 5;
/// Starting cushion: audio kept queued (on top of one callback) before the render side
/// plays. Grows by one frame after each underrun, up to `MAX_TARGET_SAMPLES`.
const TARGET_SAMPLES: usize = FRAME; // 10 ms
/// Largest cushion the render side will grow to.
const MAX_TARGET_SAMPLES: usize = FRAME * 3; // 30 ms
/// After this long without an underrun, shrink the cushion one step back toward 10 ms.
const SHRINK_AFTER_SAMPLES: usize = SAMPLE_RATE as usize * 60; // 60 s
/// If more than this is queued, skip ahead so latency never accumulates.
const MAX_SAMPLES: usize = FRAME * 6; // 60 ms

// ---------------------------------------------------------------------------
// Denoise worker (one per program run)
// ---------------------------------------------------------------------------

enum WorkerMsg {
    /// New ring halves for a freshly opened pair of streams.
    Attach(HeapCons<f32>, HeapProd<f32>),
    Quit,
}

/// Long-lived thread that turns raw mono samples into denoised frames.
pub struct Worker {
    tx: mpsc::Sender<WorkerMsg>,
    thread: JoinHandle<()>,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetCurrentThread() -> *mut core::ffi::c_void;
    fn SetThreadPriority(thread: *mut core::ffi::c_void, priority: i32) -> i32;
}
/// Same priority cpal gives its own audio threads, so a game can't starve denoising.
/// This changes only Sotto's own thread — no system setting.
const THREAD_PRIORITY_TIME_CRITICAL: i32 = 15;

impl Worker {
    pub fn spawn(mut denoiser: Box<dyn Denoiser>, stats: Arc<Stats>) -> Result<Self, String> {
        let (tx, rx) = mpsc::channel::<WorkerMsg>();
        let thread = thread::Builder::new()
            .name("sotto-denoise".into())
            .spawn(move || {
                // SAFETY: GetCurrentThread returns a pseudo-handle for this thread.
                unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL) };
                // Runs when this thread ends for any reason, including a panic.
                let _guard = FailFlag(Arc::clone(&stats));

                let mut frame = [0.0f32; FRAME];
                let mut clean = [0.0f32; FRAME];
                // Warm-up: lets the engine build its per-thread tables now, not mid-stream.
                denoiser.process(&frame, &mut clean);

                let mut io: Option<(HeapCons<f32>, HeapProd<f32>)> = None;
                loop {
                    match rx.try_recv() {
                        Ok(WorkerMsg::Attach(raw, out)) => io = Some((raw, out)),
                        Ok(WorkerMsg::Quit) | Err(mpsc::TryRecvError::Disconnected) => return,
                        Err(mpsc::TryRecvError::Empty) => {}
                    }
                    if let Some((raw, out)) = io.as_mut() {
                        while raw.occupied_len() >= FRAME {
                            raw.pop_slice(&mut frame);
                            let t0 = Instant::now();
                            denoiser.process(&frame, &mut clean);
                            stats.record_frame(t0.elapsed());
                            // Drop a whole frame rather than splice half of one.
                            if out.vacant_len() >= FRAME {
                                out.push_slice(&clean);
                            } else {
                                stats.overflows.fetch_add(1, Relaxed);
                            }
                        }
                    }
                    // Sleep until the capture callback wakes us (or 20 ms, to notice
                    // messages when no audio is flowing).
                    thread::park_timeout(Duration::from_millis(20));
                }
            })
            .map_err(|e| format!("could not start denoise thread: {e}"))?;
        Ok(Self { tx, thread })
    }

    pub fn stop(self) {
        let _ = self.tx.send(WorkerMsg::Quit);
        self.thread.thread().unpark();
        let _ = self.thread.join();
    }
}

// ---------------------------------------------------------------------------
// Streams (re-created on every (re)connect)
// ---------------------------------------------------------------------------

/// A running pair of streams. Dropping it stops both (Rust calls `drop` automatically
/// when the value goes out of scope — like `__exit__` of a `with` block).
pub struct Engine {
    _input: Stream,
    _output: Stream,
    pub input_channels: u16,
    pub output_channels: u16,
}

/// Find a 48 kHz f32 config the device supports natively, preferring fewer channels.
fn pick_config(
    ranges: impl Iterator<Item = cpal::SupportedStreamConfigRange>,
    what: &str,
) -> Result<StreamConfig, String> {
    let mut seen = Vec::new();
    let mut best: Option<cpal::SupportedStreamConfigRange> = None;
    for r in ranges {
        seen.push(format!(
            "{}ch {:?} {}-{} Hz",
            r.channels(),
            r.sample_format(),
            r.min_sample_rate(),
            r.max_sample_rate()
        ));
        let ok = r.channels() > 0
            && r.sample_format() == SampleFormat::F32
            && (r.min_sample_rate()..=r.max_sample_rate()).contains(&SAMPLE_RATE);
        if ok && best.as_ref().is_none_or(|b| r.channels() < b.channels()) {
            best = Some(r);
        }
    }
    match best {
        Some(r) => Ok(StreamConfig {
            channels: r.channels(),
            sample_rate: SAMPLE_RATE,
            buffer_size: cpal::BufferSize::Default,
        }),
        None => Err(format!(
            "{what} does not support 48 kHz float audio in shared mode \
             (resampling comes in a later phase). Supported: {}",
            seen.join("; ")
        )),
    }
}

/// Errors cpal reports that don't mean the stream is dead. `DeviceChanged` is *not*
/// here: it would mean the stream was rerouted to another device, so we stop instead.
fn is_recoverable(kind: ErrorKind) -> bool {
    matches!(kind, ErrorKind::Xrun | ErrorKind::RealtimeDenied)
}

/// Open and start streams on exactly these two devices, feeding `worker`.
///
/// `input` / `output` must come from the `devices` module (enumerated, never default).
pub fn start(
    input: &Device,
    output: &Device,
    worker: &Worker,
    stats: Arc<Stats>,
) -> Result<Engine, String> {
    let in_cfg = pick_config(
        input
            .supported_input_configs()
            .map_err(|e| format!("input device: {e}"))?,
        "The input device",
    )?;
    let out_cfg = pick_config(
        output
            .supported_output_configs()
            .map_err(|e| format!("output device: {e}"))?,
        "The output device",
    )?;
    let in_ch = in_cfg.channels as usize;
    let out_ch = out_cfg.channels as usize;

    // `split` turns a ring buffer into a producer half (only it can push) and a consumer
    // half (only it can pop). Each half lives in exactly one thread — single producer,
    // single consumer — which is what makes it safe without a lock.
    let (mut raw_prod, raw_cons) = HeapRb::<f32>::new(RING_CAPACITY).split();
    let (clean_prod, mut clean_cons) = HeapRb::<f32>::new(RING_CAPACITY).split();
    worker
        .tx
        .send(WorkerMsg::Attach(raw_cons, clean_prod))
        .map_err(|_| "denoise thread is not running".to_string())?;
    let worker_thread = worker.thread.thread().clone();

    // ---------------- capture side ----------------
    let st = Arc::clone(&stats);
    let on_input = move |data: &[f32], _: &cpal::InputCallbackInfo| {
        st.captured.fetch_add((data.len() / in_ch) as u64, Relaxed); // mic clock, for drift
        for chunk in data.chunks_exact(in_ch) {
            // Downmix to mono (average of channels).
            let mono = chunk.iter().sum::<f32>() / in_ch as f32;
            if raw_prod.try_push(mono).is_err() {
                st.overflows.fetch_add(1, Relaxed);
                break;
            }
        }
        if raw_prod.occupied_len() >= FRAME {
            worker_thread.unpark(); // a full frame is ready
        }
    };
    let st = Arc::clone(&stats);
    let on_input_err = move |e: cpal::Error| {
        if !is_recoverable(e.kind()) {
            st.input_failed.store(true, Relaxed);
        }
    };

    // ---------------- render side ----------------
    let mut renderer = Renderer::new();
    let st = Arc::clone(&stats);
    let on_output = move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
        renderer.fill(&mut clean_cons, data, out_ch, &st);
    };
    let st = Arc::clone(&stats);
    let on_output_err = move |e: cpal::Error| {
        if !is_recoverable(e.kind()) {
            st.output_failed.store(true, Relaxed);
        }
    };

    let timeout = Some(Duration::from_secs(3));
    let out_stream = output
        .build_output_stream::<f32, _, _>(out_cfg, on_output, on_output_err, timeout)
        .map_err(|e| format!("could not open output stream: {e}"))?;
    let in_stream = input
        .build_input_stream::<f32, _, _>(in_cfg, on_input, on_input_err, timeout)
        .map_err(|e| format!("could not open input stream: {e}"))?;

    out_stream
        .play()
        .map_err(|e| format!("could not start output: {e}"))?;
    in_stream
        .play()
        .map_err(|e| format!("could not start input: {e}"))?;

    Ok(Engine {
        _input: in_stream,
        _output: out_stream,
        input_channels: in_cfg.channels,
        output_channels: out_cfg.channels,
    })
}

/// Render-side state: how much audio to keep queued, and whether we are playing yet.
///
/// The cushion ("target") starts at 10 ms. If the output ever runs dry (an underrun —
/// an audible click), the cushion grows by 10 ms, up to 30 ms, so a mic that delivers
/// audio unevenly stops clicking. After 60 s with no underrun it shrinks back by 10 ms
/// (dropping 10 ms of the oldest audio once), so one stray hiccup doesn't cost latency
/// for the rest of the session. Whenever playback (re)starts, anything queued beyond
/// the target is dropped, so the delay is always exactly the chosen cushion and never
/// creeps upward.
struct Renderer {
    primed: bool,
    target: usize,
    /// Samples played since the last underrun (or the last shrink).
    calm: usize,
}

impl Renderer {
    fn new() -> Self {
        Self {
            primed: false,
            target: TARGET_SAMPLES,
            calm: 0,
        }
    }

    /// Fill one output buffer (`channels` interleaved). Real-time safe: no allocation,
    /// no locks, no I/O — only ring-buffer operations and atomics.
    fn fill(
        &mut self,
        ring: &mut impl Consumer<Item = f32>,
        data: &mut [f32],
        channels: usize,
        stats: &Stats,
    ) {
        if channels == 0 {
            data.fill(0.0); // impossible device config; never divide by zero here
            return;
        }
        let want = data.len() / channels;
        stats.rendered.fetch_add(want as u64, Relaxed); // cable clock, for drift
        let keep = self.target + want;
        let mut avail = ring.occupied_len();

        if !self.primed {
            if avail < keep {
                data.fill(0.0); // still filling up: output silence
                // Shown live, but kept out of the average (which measures playing time).
                stats.buffered.store(avail as u32, Relaxed);
                return;
            }
            // Audio arrives in 480-sample blocks, so we may have overshot the target
            // while waiting. Drop the oldest extra so latency is exactly the target.
            ring.skip(avail - keep); // avail >= keep checked just above
            avail = keep;
            self.primed = true;
        } else if avail > MAX_SAMPLES.max(keep + FRAME) {
            // Too much queued (e.g. render thread stalled): drop the oldest audio.
            // The `max` keeps a very large callback from counting as "too much", and
            // guarantees avail > keep, so the subtraction can't underflow.
            ring.skip(avail - keep);
            stats.resyncs.fetch_add(1, Relaxed);
            avail = keep;
        } else if self.calm >= SHRINK_AFTER_SAMPLES && self.target > TARGET_SAMPLES {
            // Long stretch without trouble: give back 10 ms of latency.
            self.target -= FRAME;
            self.calm = 0;
            if avail >= FRAME + want {
                ring.skip(FRAME);
                avail -= FRAME;
            }
        }

        let mut short = false;
        for frame in data.chunks_exact_mut(channels) {
            let s = ring.try_pop().unwrap_or_else(|| {
                short = true;
                0.0
            });
            frame.fill(s); // same mono sample on every output channel
        }
        if short {
            stats.underruns.fetch_add(1, Relaxed);
            self.primed = false; // rebuild the cushion before playing again
            self.target = (self.target + FRAME).min(MAX_TARGET_SAMPLES);
            self.calm = 0;
        } else {
            self.calm = self.calm.saturating_add(want);
        }
        stats.set_buffered(avail.saturating_sub(want));
    }
}

/// Drop guard: marks the worker as failed when its thread exits unexpectedly.
/// (Like a `try/finally` that runs even if the thread crashes.)
struct FailFlag(Arc<Stats>);

impl Drop for FailFlag {
    fn drop(&mut self) {
        if thread::panicking() {
            self.0.worker_failed.store(true, Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stereo output, 480 frames per callback (10 ms) — what VB-Cable gave us live.
    const CH: usize = 2;
    const WANT: usize = FRAME;

    fn ring_with(n: usize) -> (HeapProd<f32>, HeapCons<f32>) {
        let (mut p, c) = HeapRb::<f32>::new(RING_CAPACITY).split();
        for i in 0..n {
            p.try_push(i as f32).unwrap();
        }
        (p, c)
    }

    #[test]
    fn prime_trims_overshoot_to_target() {
        let stats = Stats::new();
        let mut r = Renderer::new();
        let (_p, mut c) = ring_with(2000); // way more than 10 ms + one callback
        let mut out = [0.0f32; WANT * CH];
        r.fill(&mut c, &mut out, CH, &stats);
        // Kept exactly target (480) + one callback (480); played one callback.
        assert_eq!(c.occupied_len(), TARGET_SAMPLES);
        assert_eq!(stats.buffered.load(Relaxed) as usize, TARGET_SAMPLES);
        // Oldest extra was dropped: first sample played is #(2000 - 960).
        assert_eq!(out[0], (2000 - TARGET_SAMPLES - WANT) as f32);
        assert_eq!(out[0], out[1], "mono copied to both channels");
    }

    #[test]
    fn underrun_grows_cushion_and_caps_at_30ms() {
        let stats = Stats::new();
        let mut r = Renderer::new();
        let mut out = [0.0f32; WANT * CH];
        let (mut p, mut c) = ring_with(TARGET_SAMPLES + WANT);
        r.fill(&mut c, &mut out, CH, &stats); // primes and plays

        for expected in [2 * FRAME, 3 * FRAME, 3 * FRAME] {
            // Starve the output: only 100 samples queued for a 480-sample callback.
            while c.try_pop().is_some() {}
            for _ in 0..100 {
                p.try_push(0.5).unwrap();
            }
            r.fill(&mut c, &mut out, CH, &stats);
            assert_eq!(r.target, expected);

            // Refill generously; replay must trim to the new target exactly.
            for _ in 0..3000 {
                p.try_push(0.5).unwrap();
            }
            r.fill(&mut c, &mut out, CH, &stats);
            assert_eq!(c.occupied_len(), expected);
        }
        assert_eq!(stats.underruns.load(Relaxed), 3);
        assert_eq!(stats.resyncs.load(Relaxed), 0);
    }

    #[test]
    fn resync_drops_oldest_down_to_target() {
        let stats = Stats::new();
        let mut r = Renderer::new();
        let mut out = [0.0f32; WANT * CH];
        let (mut p, mut c) = ring_with(TARGET_SAMPLES + WANT);
        r.fill(&mut c, &mut out, CH, &stats);
        for _ in 0..MAX_SAMPLES {
            p.try_push(0.0).unwrap(); // render stalled: 60+ ms piled up
        }
        r.fill(&mut c, &mut out, CH, &stats);
        assert_eq!(stats.resyncs.load(Relaxed), 1);
        assert_eq!(c.occupied_len(), TARGET_SAMPLES);
    }

    #[test]
    fn huge_callback_never_underflows() {
        // A driver asking for 100 ms at once, with 30 ms cushion and ~61 ms queued:
        // used to compute `avail - keep` with keep > avail (panic in debug builds).
        let stats = Stats::new();
        let mut r = Renderer::new();
        r.primed = true;
        r.target = MAX_TARGET_SAMPLES;
        let (_p, mut c) = ring_with(MAX_SAMPLES + 1);
        let mut out = vec![0.0f32; 4800 * CH];
        r.fill(&mut c, &mut out, CH, &stats);
        assert_eq!(stats.resyncs.load(Relaxed), 0);
        assert_eq!(stats.underruns.load(Relaxed), 1); // not enough audio: honest underrun
    }

    #[test]
    fn cushion_shrinks_after_a_calm_minute() {
        let stats = Stats::new();
        let mut r = Renderer::new();
        r.primed = true;
        r.target = 2 * FRAME;
        r.calm = SHRINK_AFTER_SAMPLES;
        let (_p, mut c) = ring_with(2 * FRAME + WANT);
        let mut out = [0.0f32; WANT * CH];
        r.fill(&mut c, &mut out, CH, &stats);
        assert_eq!(r.target, TARGET_SAMPLES);
        assert_eq!(c.occupied_len(), TARGET_SAMPLES); // 10 ms of latency given back
        assert_eq!(stats.underruns.load(Relaxed), 0);
    }

    #[test]
    fn zero_channels_is_silence_not_panic() {
        let stats = Stats::new();
        let mut r = Renderer::new();
        let (_p, mut c) = ring_with(2000);
        let mut out = [1.0f32; 8];
        r.fill(&mut c, &mut out, 0, &stats);
        assert!(out.iter().all(|&s| s == 0.0));
    }

    #[test]
    fn steady_state_does_not_trim_normal_jitter() {
        let stats = Stats::new();
        let mut r = Renderer::new();
        let mut out = [0.0f32; WANT * CH];
        let (mut p, mut c) = ring_with(TARGET_SAMPLES + WANT);
        r.fill(&mut c, &mut out, CH, &stats);
        // Two blocks arrive before the next callback (scheduling jitter): keep them.
        for _ in 0..2 * FRAME {
            p.try_push(0.0).unwrap();
        }
        r.fill(&mut c, &mut out, CH, &stats);
        assert_eq!(c.occupied_len(), TARGET_SAMPLES + FRAME);
        assert_eq!(stats.resyncs.load(Relaxed), 0);
        assert_eq!(stats.underruns.load(Relaxed), 0);
    }
}
