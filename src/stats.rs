// SPDX-License-Identifier: GPL-3.0-only
//! Lock-free statistics shared between the audio threads and the main thread,
//! plus process CPU / RAM readings from Windows.
//!
//! Python analogy: `Arc<Stats>` is like passing the same object to two threads.
//! Python would protect it with the GIL or a `threading.Lock`; here every field is an
//! *atomic* integer, which the CPU updates indivisibly — no lock, so the audio thread
//! can never be blocked waiting for the main thread.

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering::Relaxed};
use std::time::Duration;

/// Histogram bucket width for per-frame processing time.
const BUCKET_NS: u64 = 2_000; // 2 µs
/// 5000 buckets × 2 µs = 10 ms (one full frame period). Slower frames go in the last bucket.
const BUCKETS: usize = 5_000;

pub struct Stats {
    /// Per-frame processing-time histogram (audio thread writes, main thread reads+resets).
    hist: Box<[AtomicU32]>,
    max_ns: AtomicU64,
    pub frames: AtomicU64,
    /// Frames dropped because the ring buffer was full (output not draining).
    pub overflows: AtomicU64,
    /// Output callbacks that found too little audio (glitch / silence inserted).
    pub underruns: AtomicU64,
    /// Times the output skipped ahead to cut accumulated latency.
    pub resyncs: AtomicU64,
    /// Samples currently waiting in the ring buffer (≈ added latency).
    pub buffered: AtomicU32,
    /// Sum and count of `buffered` over every render callback, for a true average.
    buf_sum: AtomicU64,
    buf_n: AtomicU64,
    /// Total sample frames the mic has delivered / the cable has asked for. Each runs on
    /// its own device clock, so the difference between their rates is the clock drift.
    pub captured: AtomicU64,
    pub rendered: AtomicU64,
    pub input_failed: AtomicBool,
    pub output_failed: AtomicBool,
    /// Set if the denoise worker thread died (e.g. the engine panicked).
    pub worker_failed: AtomicBool,
    /// Frames in which the noise gate was open (main thread reads and resets).
    pub gate_open_frames: AtomicU64,
    /// Current auto-gain, dB, as f32 bits.
    pub agc_gain_db: AtomicU32,
    /// Largest limiter gain reduction since the last read, in hundredths of a dB.
    pub limit_cdb: AtomicU32,
    /// Frames the current engine failed to process (passed through instead).
    pub engine_errors: AtomicU64,
    /// Engine currently heard (`engine::engine_code`), published by the worker.
    pub active_engine: AtomicU8,
    /// Sequence number of the last engine switch the worker has completed.
    pub switch_done: AtomicU64,
}

impl Stats {
    pub fn new() -> Self {
        Self {
            hist: (0..BUCKETS).map(|_| AtomicU32::new(0)).collect(),
            max_ns: AtomicU64::new(0),
            frames: AtomicU64::new(0),
            overflows: AtomicU64::new(0),
            underruns: AtomicU64::new(0),
            resyncs: AtomicU64::new(0),
            buffered: AtomicU32::new(0),
            buf_sum: AtomicU64::new(0),
            buf_n: AtomicU64::new(0),
            captured: AtomicU64::new(0),
            rendered: AtomicU64::new(0),
            input_failed: AtomicBool::new(false),
            output_failed: AtomicBool::new(false),
            worker_failed: AtomicBool::new(false),
            gate_open_frames: AtomicU64::new(0),
            agc_gain_db: AtomicU32::new(0),
            limit_cdb: AtomicU32::new(0),
            engine_errors: AtomicU64::new(0),
            active_engine: AtomicU8::new(0),
            switch_done: AtomicU64::new(0),
        }
    }

    /// Called from the audio thread once per frame. Real-time safe (atomics only).
    pub fn record_frame(&self, elapsed: Duration) {
        let ns = elapsed.as_nanos().min(u64::MAX as u128) as u64;
        let idx = ((ns / BUCKET_NS) as usize).min(BUCKETS - 1);
        self.hist[idx].fetch_add(1, Relaxed);
        self.max_ns.fetch_max(ns, Relaxed);
        self.frames.fetch_add(1, Relaxed);
    }

    /// Worker: record the chain's readings for one frame. Atomics only.
    pub fn record_chain(&self, info: &crate::dsp::FrameInfo) {
        if info.gate_open {
            self.gate_open_frames.fetch_add(1, Relaxed);
        }
        self.agc_gain_db.store(info.agc_gain_db.to_bits(), Relaxed);
        let cdb = (info.limit_db.clamp(0.0, 100.0) * 100.0) as u32;
        self.limit_cdb.fetch_max(cdb, Relaxed);
        self.engine_errors.store(info.engine_errors, Relaxed);
    }

    /// Render callback: record how many samples are queued right now. Atomics only.
    pub fn set_buffered(&self, samples: usize) {
        self.buffered.store(samples as u32, Relaxed);
        self.buf_sum.fetch_add(samples as u64, Relaxed);
        self.buf_n.fetch_add(1, Relaxed);
    }

    /// Main thread: average queued samples since the last call (None if no callbacks).
    pub fn take_buffer_avg(&self) -> Option<f64> {
        let sum = self.buf_sum.swap(0, Relaxed);
        let n = self.buf_n.swap(0, Relaxed);
        (n > 0).then(|| sum as f64 / n as f64)
    }

    /// Main thread: read and reset the timing histogram for the last interval.
    pub fn take_timing(&self) -> Timing {
        let mut counts = Vec::with_capacity(BUCKETS);
        let mut total: u64 = 0;
        for b in self.hist.iter() {
            let c = b.swap(0, Relaxed) as u64;
            total += c;
            counts.push(c);
        }
        let pct = |p: f64| -> f64 {
            if total == 0 {
                return 0.0;
            }
            let target = ((total as f64) * p).ceil().max(1.0) as u64;
            let mut seen = 0;
            for (i, c) in counts.iter().enumerate() {
                seen += c;
                if seen >= target {
                    // upper edge of the bucket, in ms
                    return ((i as u64 + 1) * BUCKET_NS) as f64 / 1e6;
                }
            }
            (BUCKETS as u64 * BUCKET_NS) as f64 / 1e6
        };
        let max_ms = self.max_ns.swap(0, Relaxed) as f64 / 1e6;
        Timing {
            frames: total,
            // Bucket upper edges can exceed the exact max; cap so p99 <= max always.
            p50_ms: pct(0.50).min(max_ms),
            p99_ms: pct(0.99).min(max_ms),
            max_ms,
        }
    }
}

/// Estimates clock drift between mic and cable with a least-squares line through
/// (seconds, captured − rendered) points taken once per second. Each point is off by up
/// to one callback (~480 samples) of jitter, but the slope averages that out: after a
/// few minutes it's accurate to a few ppm. Python analogy: `numpy.polyfit(t, d, 1)[0]`,
/// kept as running sums so it needs no list.
#[derive(Default)]
pub struct DriftEstimator {
    n: f64,
    st: f64,
    sd: f64,
    stt: f64,
    std: f64,
    sdd: f64,
}

impl DriftEstimator {
    pub fn add(&mut self, t_secs: f64, diff_samples: f64) {
        self.n += 1.0;
        self.st += t_secs;
        self.sd += diff_samples;
        self.stt += t_secs * t_secs;
        self.std += t_secs * diff_samples;
        self.sdd += diff_samples * diff_samples;
    }

    pub fn points(&self) -> usize {
        self.n as usize
    }

    /// Drift in parts per million of the sample rate, with its standard error
    /// (1 sigma). Positive = the mic delivers audio faster than the cable consumes it
    /// (queue would grow); negative = it drains. None until there are enough points.
    pub fn ppm(&self, sample_rate: f64) -> Option<(f64, f64)> {
        let n = self.n;
        if n < 3.0 {
            return None;
        }
        let sxx = self.stt - self.st * self.st / n;
        let sxy = self.std - self.st * self.sd / n;
        let syy = self.sdd - self.sd * self.sd / n;
        if sxx <= 0.0 {
            return None;
        }
        let slope = sxy / sxx; // samples per second
        let resid = (syy - slope * sxy).max(0.0) / (n - 2.0);
        let se = (resid / sxx).sqrt();
        let k = 1e6 / sample_rate;
        Some((slope * k, se * k))
    }
}

pub struct Timing {
    pub frames: u64,
    pub p50_ms: f64,
    pub p99_ms: f64,
    pub max_ms: f64,
}

// ---------------------------------------------------------------------------
// Process CPU and memory, read straight from the Windows API.
//
// Instead of adding a crate (sysinfo, windows-sys), we declare the three functions we
// need ourselves. `unsafe extern "system"` tells Rust "these exist in kernel32.dll, use
// the Windows calling convention; I promise the signatures are right".
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct FileTime {
    low: u32,
    high: u32,
}

impl FileTime {
    /// FILETIME is in 100-nanosecond units.
    fn as_100ns(self) -> u64 {
        ((self.high as u64) << 32) | self.low as u64
    }
}

#[repr(C)]
#[derive(Default)]
struct ProcessMemoryCountersEx {
    cb: u32,
    page_fault_count: u32,
    peak_working_set_size: usize,
    working_set_size: usize,
    quota_peak_paged_pool_usage: usize,
    quota_paged_pool_usage: usize,
    quota_peak_non_paged_pool_usage: usize,
    quota_non_paged_pool_usage: usize,
    pagefile_usage: usize,
    peak_pagefile_usage: usize,
    private_usage: usize,
}

type Handle = *mut core::ffi::c_void;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetCurrentProcess() -> Handle;
    fn GetProcessTimes(
        process: Handle,
        creation: *mut FileTime,
        exit: *mut FileTime,
        kernel: *mut FileTime,
        user: *mut FileTime,
    ) -> i32;
    fn K32GetProcessMemoryInfo(
        process: Handle,
        counters: *mut ProcessMemoryCountersEx,
        cb: u32,
    ) -> i32;
}

/// Total CPU time (user + kernel) this process has used so far.
pub fn process_cpu_time() -> Option<Duration> {
    let (mut c, mut e, mut k, mut u) = Default::default();
    // SAFETY: GetCurrentProcess returns a pseudo-handle that needs no closing; all four
    // out-pointers point to valid, writable FileTime values on our stack.
    let ok = unsafe { GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u) };
    let k: FileTime = k;
    let u: FileTime = u;
    (ok != 0).then(|| Duration::from_nanos((k.as_100ns() + u.as_100ns()) * 100))
}

/// (working set, private bytes) in bytes.
pub fn process_memory() -> Option<(usize, usize)> {
    let mut m = ProcessMemoryCountersEx {
        cb: size_of::<ProcessMemoryCountersEx>() as u32,
        ..Default::default()
    };
    // SAFETY: `m` is a correctly sized, writable PROCESS_MEMORY_COUNTERS_EX and `cb`
    // tells Windows its size.
    let ok = unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut m, m.cb) };
    (ok != 0).then_some((m.working_set_size, m.private_usage))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_from_histogram() {
        let s = Stats::new();
        for _ in 0..98 {
            s.record_frame(Duration::from_micros(100));
        }
        s.record_frame(Duration::from_micros(900));
        s.record_frame(Duration::from_millis(20)); // beyond the last bucket
        let t = s.take_timing();
        assert_eq!(t.frames, 100);
        assert!((t.p50_ms - 0.102).abs() < 1e-9);
        assert!((t.p99_ms - 0.902).abs() < 1e-9);
        assert!((t.max_ms - 20.0).abs() < 1e-9);
        // reset after take
        assert_eq!(s.take_timing().frames, 0);
    }

    #[test]
    fn drift_estimator_finds_slope_through_jitter() {
        // Mic 50 ppm slower than cable: diff falls 2.4 samples/s, plus ±240 of jitter.
        let mut d = DriftEstimator::default();
        let mut seed: u32 = 1;
        for s in 0..600 {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let jitter = (seed >> 16) as f64 / 65_536.0 * 480.0 - 240.0;
            d.add(s as f64, 1000.0 - 2.4 * s as f64 + jitter);
        }
        let (ppm, se) = d.ppm(48_000.0).unwrap();
        assert!((ppm + 50.0).abs() < 10.0, "got {ppm}");
        // Uniform ±240 jitter over 600 s: SE ≈ 139 / (√600 · 173) samples/s ≈ 0.7 ppm.
        assert!(se > 0.2 && se < 2.0, "se {se}");
        assert!(DriftEstimator::default().ppm(48_000.0).is_none());
    }

    #[test]
    fn windows_counters_work() {
        assert!(process_cpu_time().is_some());
        let (ws, private) = process_memory().unwrap();
        assert!(ws > 0 && private > 0);
    }
}
