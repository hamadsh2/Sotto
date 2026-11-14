// SPDX-License-Identifier: GPL-3.0-only
//! Exclusive-mode output to VB-Cable's "CABLE Input": "Hide my voice from screen shares".
//!
//! Why: Discord's whole-screen share records every app's sound with Windows' *process
//! loopback* capture. That capture taps the shared-mode audio engine, so Sotto's
//! shared-mode output to CABLE Input was recorded too, and viewers heard your voice
//! twice (once from the mic, once from the share). An **exclusive-mode** stream skips the
//! audio engine and goes straight to the driver, so loopback can't see it, while the
//! cable still delivers it to "CABLE Output" (your mic in Discord) as before. Measured
//! with a test tone: the loopback capture saw 0.000 in exclusive mode and 0.200 in shared
//! mode; CABLE Output got 0.200 both times.
//!
//! The rules (CLAUDE.md, user-approved 2026-11-14): exclusive mode **only** on the
//! selected CABLE Input, opened by its exact endpoint ID; never on the mic or any other
//! device; nothing about the device is changed (no format or setting writes). If it
//! fails (another app holds the cable, no usable format, Windows doesn't allow exclusive
//! control), the caller falls back to shared mode at once.
//!
//! cpal only does shared mode, so this talks to WASAPI directly through the `windows`
//! crate (the same version cpal uses). It runs on its own time-critical thread, woken by
//! the driver every period (~10 ms), and fills each buffer with `Renderer::fill`, the
//! same logic as the shared path: no allocation, locking or I/O inside the loop.
//!
//! Python analogy: COM objects are like handles to objects that live in Windows; each
//! method call returns a `Result`, and dropping the Rust value releases the handle
//! (like `close()` in a `with` block).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use ringbuf::HeapCons;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED, AUDCLNT_SHAREMODE_EXCLUSIVE,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, IAudioClient, IAudioRenderClient, IMMDeviceEnumerator,
    MMDeviceEnumerator, WAVEFORMATEX, WAVEFORMATEXTENSIBLE, WAVEFORMATEXTENSIBLE_0,
};
use windows::Win32::Media::KernelStreaming::{KSDATAFORMAT_SUBTYPE_PCM, SPEAKER_FRONT_CENTER};
use windows::Win32::Media::Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
};
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, CreateEventW, GetCurrentThread,
    SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL, WaitForSingleObject,
};
use windows::core::{HSTRING, w};

use crate::engine::{Renderer, SAMPLE_RATE};
use crate::stats::Stats;

/// Stereo front-left | front-right.
const STEREO_MASK: u32 = 0x3;
/// `WAVE_FORMAT_EXTENSIBLE` (mmreg.h).
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
/// No wake-up from the driver for this many waits in a row (100 ms each) means the
/// device is gone or stuck: report it like a disconnect.
const MAX_SILENT_WAITS: u32 = 20;

/// How samples are stored in the device buffer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Sample {
    F32,
    /// 32-bit container; `valid` bits are used (24 or 32).
    I32,
    I24,
    I16,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Format {
    pub sample: Sample,
    pub bits: u16,
    pub valid: u16,
    pub channels: u16,
}

impl Format {
    fn bytes(&self) -> usize {
        self.bits as usize / 8
    }

    /// e.g. "24-bit" or "32-bit float".
    pub fn describe(&self) -> String {
        match self.sample {
            Sample::F32 => "32-bit float".into(),
            _ => format!("{}-bit", self.valid),
        }
    }

    fn wave(&self) -> WAVEFORMATEXTENSIBLE {
        let block = self.channels * self.bits / 8;
        WAVEFORMATEXTENSIBLE {
            Format: WAVEFORMATEX {
                wFormatTag: WAVE_FORMAT_EXTENSIBLE,
                nChannels: self.channels,
                nSamplesPerSec: SAMPLE_RATE,
                nAvgBytesPerSec: SAMPLE_RATE * block as u32,
                nBlockAlign: block,
                wBitsPerSample: self.bits,
                cbSize: 22,
            },
            Samples: WAVEFORMATEXTENSIBLE_0 {
                wValidBitsPerSample: self.valid,
            },
            dwChannelMask: if self.channels == 2 {
                STEREO_MASK
            } else {
                SPEAKER_FRONT_CENTER
            },
            SubFormat: if self.sample == Sample::F32 {
                KSDATAFORMAT_SUBTYPE_IEEE_FLOAT
            } else {
                KSDATAFORMAT_SUBTYPE_PCM
            },
        }
    }
}

/// Formats to try, best first. VB-Cable accepted 48 kHz 24-bit stereo in testing.
fn candidates() -> impl Iterator<Item = Format> {
    let kinds = [
        (Sample::F32, 32, 32),
        (Sample::I24, 24, 24),
        (Sample::I32, 32, 24),
        (Sample::I32, 32, 32),
        (Sample::I16, 16, 16),
    ];
    [2u16, 1].into_iter().flat_map(move |channels| {
        kinds.into_iter().map(move |(sample, bits, valid)| Format {
            sample,
            bits,
            valid,
            channels,
        })
    })
}

/// Write interleaved f32 samples into the device buffer in its format. Real-time safe.
pub fn write(src: &[f32], dst: &mut [u8], f: Format) {
    let n = f.bytes();
    for (s, out) in src.iter().zip(dst.chunks_exact_mut(n)) {
        let v = if s.is_finite() {
            s.clamp(-1.0, 1.0)
        } else {
            0.0
        };
        match f.sample {
            Sample::F32 => out.copy_from_slice(&v.to_le_bytes()),
            Sample::I16 => out.copy_from_slice(&((v * 32767.0) as i16).to_le_bytes()),
            Sample::I24 => {
                let x = (v * 8_388_607.0) as i32;
                out.copy_from_slice(&x.to_le_bytes()[..3]);
            }
            Sample::I32 => {
                // Scale in f64: an f32 can't hold 2^31 - 1 exactly. Valid-bits 24 in a
                // 32-bit container is still full-scale left-justified, so same math.
                let x = (v as f64 * 2_147_483_647.0) as i32;
                out.copy_from_slice(&x.to_le_bytes());
            }
        }
    }
}

/// The running exclusive stream. Dropping it stops the thread and releases the device.
pub struct Exclusive {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    pub format: Format,
}

impl Drop for Exclusive {
    fn drop(&mut self) {
        self.stop.store(true, Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join(); // the loop wakes at least every 100 ms
        }
    }
}

/// Why exclusive mode couldn't start, with the ring handed back for the shared fallback
/// (`None` only if the thread itself couldn't run; then the caller retries later).
pub struct Refused {
    pub why: String,
    pub ring: Option<HeapCons<f32>>,
}

/// Open CABLE Input (`endpoint_id`, from the user's selection) in exclusive mode and play
/// from `ring`. On failure the ring comes back so the caller can open shared mode instead.
pub fn start(
    endpoint_id: &str,
    ring: HeapCons<f32>,
    stats: Arc<Stats>,
) -> Result<Exclusive, Box<Refused>> {
    let (tx, rx) = mpsc::sync_channel::<Result<Format, Refused>>(1);
    let stop = Arc::new(AtomicBool::new(false));
    let stop_flag = Arc::clone(&stop);
    let id = endpoint_id.to_string();
    let spawned = thread::Builder::new()
        .name("sotto-cable-exclusive".into())
        .spawn(move || run(&id, ring, stats, stop_flag, tx));
    let handle = match spawned {
        Ok(h) => h,
        // The ring moved into the closure that never ran; nothing to hand back.
        Err(e) => {
            return Err(Box::new(Refused {
                why: format!("could not start the output thread: {e}"),
                ring: None,
            }));
        }
    };
    match rx.recv_timeout(Duration::from_secs(3)) {
        Ok(Ok(format)) => Ok(Exclusive {
            stop,
            thread: Some(handle),
            format,
        }),
        Ok(Err(refused)) => {
            let _ = handle.join();
            Err(Box::new(refused))
        }
        Err(_) => {
            // Stuck in a driver call: don't wait for it (that could freeze the control
            // thread). Let it go; it exits when the call returns and it sees `stop`.
            stop.store(true, Relaxed);
            drop(handle);
            Err(Box::new(Refused {
                why: "the output thread didn't answer".into(),
                ring: None,
            }))
        }
    }
}

/// Marks the output as failed if the thread dies from a panic.
struct FailGuard(Arc<Stats>);

impl Drop for FailGuard {
    fn drop(&mut self) {
        if thread::panicking() {
            self.0.output_failed.store(true, Relaxed);
        }
    }
}

/// Everything opened: device client, render client, wake-up event, buffer size.
struct Opened {
    client: IAudioClient,
    render: IAudioRenderClient,
    event: HANDLE,
    frames: u32,
    format: Format,
}

impl Drop for Opened {
    fn drop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
            let _ = CloseHandle(self.event);
        }
    }
}

fn open(endpoint_id: &str) -> Result<Opened, String> {
    let e = |what: &str| {
        let what = what.to_string();
        move |err: windows::core::Error| format!("{what}: {}", err.message())
    };
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(e("device list"))?;
        let device = enumerator
            .GetDevice(&HSTRING::from(endpoint_id))
            .map_err(e("CABLE Input not found"))?;
        let mut client: IAudioClient = device
            .Activate(CLSCTX_ALL, None)
            .map_err(e("opening CABLE Input"))?;

        let format = candidates()
            .find(|f| {
                let w = f.wave();
                // Pointer to the whole EXTENSIBLE struct (WASAPI reads all 40 bytes).
                client
                    .IsFormatSupported(
                        AUDCLNT_SHAREMODE_EXCLUSIVE,
                        std::ptr::from_ref(&w).cast::<WAVEFORMATEX>(),
                        None,
                    )
                    .is_ok()
            })
            .ok_or("CABLE Input has no 48 kHz format Sotto can use in exclusive mode")?;
        let wave = format.wave();

        let mut period = 0i64;
        client
            .GetDevicePeriod(Some(&mut period), None)
            .map_err(e("reading the device period"))?;
        let init = |c: &IAudioClient, p: i64| {
            c.Initialize(
                AUDCLNT_SHAREMODE_EXCLUSIVE,
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                p,
                p,
                std::ptr::from_ref(&wave).cast::<WAVEFORMATEX>(),
                None,
            )
        };
        if let Err(err) = init(&client, period) {
            if err.code() != AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED {
                return Err(match err.code().0 as u32 {
                    0x8889_000A => "another app is using CABLE Input".to_string(),
                    0x8889_000E => "Windows doesn't allow exclusive control of CABLE Input \
                                    (Sound settings → CABLE Input → Properties → Advanced)"
                        .to_string(),
                    _ => format!("exclusive mode: {}", err.message()),
                });
            }
            // The driver wants a buffer size it can align: use the size it suggests.
            let frames = client.GetBufferSize().map_err(e("buffer size"))?;
            period = (10_000_000.0 * frames as f64 / SAMPLE_RATE as f64).round() as i64;
            client = device
                .Activate(CLSCTX_ALL, None)
                .map_err(e("reopening CABLE Input"))?;
            init(&client, period).map_err(e("exclusive mode"))?;
        }
        let frames = client.GetBufferSize().map_err(e("buffer size"))?;
        let event = CreateEventW(None, false, false, None).map_err(e("wake-up event"))?;
        let opened = Opened {
            render: match client.GetService::<IAudioRenderClient>() {
                Ok(r) => r,
                Err(err) => {
                    let _ = CloseHandle(event);
                    return Err(e("render client")(err));
                }
            },
            client,
            event,
            frames,
            format,
        };
        opened
            .client
            .SetEventHandle(opened.event)
            .map_err(e("wake-up event"))?;
        Ok(opened)
    }
}

fn run(
    endpoint_id: &str,
    mut ring: HeapCons<f32>,
    stats: Arc<Stats>,
    stop: Arc<AtomicBool>,
    ready: mpsc::SyncSender<Result<Format, Refused>>,
) {
    let _guard = FailGuard(Arc::clone(&stats));
    // COM on this thread (multithreaded apartment); undone when the thread ends.
    let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    if com.is_err() {
        let _ = ready.send(Err(Refused {
            why: format!("COM: {}", com.message()),
            ring: Some(ring),
        }));
        return;
    }
    // Scope so every COM object is released before CoUninitialize.
    {
        let dev = match open(endpoint_id) {
            Ok(d) => d,
            Err(why) => {
                let _ = ready.send(Err(Refused {
                    why,
                    ring: Some(ring),
                }));
                unsafe { CoUninitialize() };
                return;
            }
        };
        let ch = dev.format.channels as usize;
        let frames = dev.frames as usize;
        let block = ch * dev.format.bytes();
        // Allocated once, before the loop.
        let mut scratch = vec![0.0f32; frames * ch];
        let mut renderer = Renderer::new();
        let mmcss;

        unsafe {
            // Pro-audio scheduling (MMCSS), so a busy game can't starve this thread;
            // a plain high priority if that's unavailable.
            let mut task = 0u32;
            mmcss = AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task).ok();
            if mmcss.is_none() {
                let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL);
            }
            // Start with one buffer of silence so the first wake-up has something queued.
            if dev.render.GetBuffer(dev.frames).is_ok() {
                let _ = dev
                    .render
                    .ReleaseBuffer(dev.frames, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32);
            }
            if let Err(err) = dev.client.Start() {
                let _ = ready.send(Err(Refused {
                    why: format!("starting exclusive output: {}", err.message()),
                    ring: Some(ring),
                }));
                drop(dev);
                CoUninitialize();
                return;
            }
        }
        let _ = ready.send(Ok(dev.format));

        let mut silent_waits = 0u32;
        while !stop.load(Relaxed) {
            if unsafe { WaitForSingleObject(dev.event, 100) } != WAIT_OBJECT_0 {
                silent_waits += 1;
                if silent_waits >= MAX_SILENT_WAITS {
                    stats.output_failed.store(true, Relaxed);
                    break;
                }
                continue;
            }
            silent_waits = 0;
            // Exclusive event mode: fill the whole buffer every period.
            let ptr = match unsafe { dev.render.GetBuffer(dev.frames) } {
                Ok(p) if !p.is_null() => p,
                _ => {
                    // Device unplugged / invalidated: report like a disconnect.
                    stats.output_failed.store(true, Relaxed);
                    break;
                }
            };
            renderer.fill(&mut ring, &mut scratch, ch, &stats);
            let dst = unsafe { std::slice::from_raw_parts_mut(ptr, frames * block) };
            write(&scratch, dst, dev.format);
            if unsafe { dev.render.ReleaseBuffer(dev.frames, 0) }.is_err() {
                stats.output_failed.store(true, Relaxed);
                break;
            }
        }
        if let Some(h) = mmcss {
            let _ = unsafe { AvRevertMmThreadCharacteristics(h) };
        }
    }
    unsafe { CoUninitialize() };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(sample: Sample, bits: u16, valid: u16) -> Format {
        Format {
            sample,
            bits,
            valid,
            channels: 2,
        }
    }

    #[test]
    fn writes_each_format_little_endian_and_clamped() {
        let src = [0.5f32, -1.5, f32::NAN];
        let mut b = [0u8; 9];
        write(&src, &mut b, fmt(Sample::I24, 24, 24));
        assert_eq!(&b[0..3], &[0xFF, 0xFF, 0x3F]); // 0.5 → 4194303
        assert_eq!(&b[3..6], &[0x01, 0x00, 0x80]); // clamped to −1 → −8388607
        assert_eq!(&b[6..9], &[0, 0, 0]); // NaN → silence

        let mut b = [0u8; 4];
        write(&[1.0], &mut b, fmt(Sample::I16, 16, 16));
        assert_eq!(i16::from_le_bytes([b[0], b[1]]), 32767);

        write(&[0.25], &mut b, fmt(Sample::F32, 32, 32));
        assert_eq!(f32::from_le_bytes(b), 0.25);

        write(&[1.0], &mut b, fmt(Sample::I32, 32, 24));
        assert_eq!(i32::from_le_bytes(b), i32::MAX);
    }

    #[test]
    fn wave_format_fields_match() {
        let w = fmt(Sample::I24, 24, 24).wave();
        // Packed struct: copy each field out before comparing.
        let (block, rate, valid, sub) = (
            { w.Format.nBlockAlign },
            { w.Format.nAvgBytesPerSec },
            unsafe { w.Samples.wValidBitsPerSample },
            { w.SubFormat },
        );
        assert_eq!(block, 6);
        assert_eq!(rate, 48_000 * 6);
        assert_eq!(valid, 24);
        assert_eq!(sub, KSDATAFORMAT_SUBTYPE_PCM);
    }

    #[test]
    fn stereo_formats_come_first() {
        let first: Vec<_> = candidates().take(5).collect();
        assert!(first.iter().all(|f| f.channels == 2));
        assert_eq!(candidates().count(), 10);
    }
}
