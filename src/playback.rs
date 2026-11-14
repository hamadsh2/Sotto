// SPDX-License-Identifier: GPL-3.0-only
//! "Hear yourself": play back a short clip of the cleaned voice, once, when the user
//! clicks Play.
//!
//! This is the one exception to "Sotto never opens a stream on your headphones" (see
//! CLAUDE.md, user-approved 2026-11-13), and it is fenced in:
//! - It plays a clip **recorded earlier** into memory, never live audio, so your voice
//!   is never doubled in a screen share or recording while you talk.
//! - It plays only on a click, once, and the stream is closed as soon as the clip ends.
//! - The device is Windows' default playback device, read **once** at the click and then
//!   opened by its exact endpoint ID, so it can't follow a later change of default.
//! - It refuses VB-Audio / Voicemeeter / CABLE devices and Sotto's own output, so the
//!   clip can never reach a virtual microphone (Discord would hear it).
//! - Shared mode (cpal's WASAPI default); no setting is changed. Nothing touches disk.
//!
//! The clip is converted to the device's rate and channel count **before** playing, on
//! the calling thread, so the audio callback only copies samples.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{DeviceId, Host, SampleFormat, Stream, StreamConfig};

use crate::devices::{self, Dir, Named};
use crate::engine::SAMPLE_RATE;

/// Length of a recorded clip.
pub const CLIP_SECONDS: usize = 5;
pub const CLIP_SAMPLES: usize = SAMPLE_RATE as usize * CLIP_SECONDS;
/// Fade in/out at the clip's ends, so starting and stopping never clicks.
const FADE_SAMPLES: usize = SAMPLE_RATE as usize / 100; // 10 ms

/// Devices the clip must never go to: anything that could be a virtual microphone.
pub use crate::devices::is_virtual;

/// Windows' default playback device right now, found again in the enumerated list by
/// its exact endpoint ID (so the handle is fixed to that device). Refuses virtual
/// devices and Sotto's own output (`avoid`).
pub fn resolve(host: &Host, avoid: Option<&DeviceId>) -> Result<Named, String> {
    let id = devices::no_panic("reading the default device", || {
        host.default_output_device()
            .and_then(|d| d.id().ok())
            .ok_or_else(|| "Windows has no playback device set".to_string())
    })?;
    let dev = devices::list(host, Dir::Output)?
        .into_iter()
        .find(|d| d.id == id)
        .ok_or_else(|| "The playback device isn't available".to_string())?;
    if is_virtual(&dev.name) || avoid.is_some_and(|a| *a == dev.id) {
        return Err(format!(
            "Your Windows playback device is \"{}\", a virtual cable, so playing there \
             could send the clip into your calls. Pick your headphones or speakers in \
             Settings → System → Sound → Output.",
            dev.name
        ));
    }
    Ok(dev)
}

/// The clip at `rate` Hz with `channels` interleaved copies, faded in and out.
/// Linear interpolation is plenty for a voice check.
pub fn convert(clip: &[f32], rate: u32, channels: usize) -> Vec<f32> {
    if clip.is_empty() || rate == 0 || channels == 0 {
        return Vec::new();
    }
    let step = SAMPLE_RATE as f64 / rate as f64;
    // Integer math: a float divide can land a hair under a whole number.
    let n = (clip.len() as u64 * rate as u64 / SAMPLE_RATE as u64) as usize;
    let fade = ((FADE_SAMPLES as f64) / step) as usize;
    let mut out = Vec::with_capacity(n * channels);
    for i in 0..n {
        let t = i as f64 * step;
        let k = t as usize;
        let frac = (t - k as f64) as f32;
        let a = clip[k.min(clip.len() - 1)];
        let b = clip[(k + 1).min(clip.len() - 1)];
        let mut v = a + (b - a) * frac;
        if fade > 0 {
            let edge = i.min(n - 1 - i);
            if edge < fade {
                v *= edge as f32 / fade as f32;
            }
        }
        let v = if v.is_finite() {
            v.clamp(-1.0, 1.0)
        } else {
            0.0
        };
        out.extend(std::iter::repeat_n(v, channels));
    }
    out
}

/// A clip being played. Dropping it closes the stream at once.
pub struct Player {
    _stream: Stream,
    done: Arc<AtomicBool>,
}

impl Player {
    /// The whole clip has been handed to the device.
    pub fn is_done(&self) -> bool {
        self.done.load(Relaxed)
    }
}

/// Play `clip` (48 kHz mono) once on `dev` (from `resolve`).
pub fn play(dev: &Named, clip: &[f32]) -> Result<Player, String> {
    let cfg = dev
        .device
        .default_output_config()
        .map_err(|e| format!("Couldn't read the playback device's format: {e}"))?;
    if cfg.sample_format() != SampleFormat::F32 {
        return Err(format!(
            "The playback device uses {:?} audio, which Sotto can't play yet.",
            cfg.sample_format()
        ));
    }
    let channels = cfg.channels() as usize;
    let rate = cfg.sample_rate();
    let samples = convert(clip, rate, channels);
    let done = Arc::new(AtomicBool::new(samples.is_empty()));
    let finished = Arc::clone(&done);
    let mut pos = 0usize;
    // Silent callbacks after the end, so the last samples (and the fade-out) have left
    // the device buffer before the stream is closed: closing early could click.
    let mut tail = 0u8;
    // Real-time safe: copies from the prepared buffer, fills silence after the end.
    let on_output = move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
        let n = data.len().min(samples.len() - pos);
        data[..n].copy_from_slice(&samples[pos..pos + n]);
        data[n..].fill(0.0);
        pos += n;
        if pos >= samples.len() && n == 0 {
            tail = tail.saturating_add(1);
            if tail >= 2 {
                finished.store(true, Relaxed);
            }
        }
    };
    let failed = Arc::clone(&done);
    let on_err = move |_: cpal::Error| failed.store(true, Relaxed);
    let config = StreamConfig {
        channels: cfg.channels(),
        sample_rate: rate,
        buffer_size: cpal::BufferSize::Default,
    };
    let stream = dev
        .device
        .build_output_stream::<f32, _, _>(config, on_output, on_err, Some(Duration::from_secs(3)))
        .map_err(|e| format!("Couldn't open the playback device: {e}"))?;
    stream
        .play()
        .map_err(|e| format!("Couldn't start playback: {e}"))?;
    Ok(Player {
        _stream: stream,
        done,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn virtual_devices_are_refused() {
        assert!(is_virtual("CABLE Input (VB-Audio Virtual Cable)"));
        assert!(is_virtual("VoiceMeeter Input (VB-Audio VoiceMeeter VAIO)"));
        assert!(is_virtual("Voicemeeter AUX Input"));
        assert!(is_virtual("CABLE-A Input (VB-Audio Cable A)"));
        assert!(!is_virtual("Headphones (Arctis Nova 7)"));
        assert!(!is_virtual("Speakers (Realtek(R) Audio)"));
    }

    #[test]
    fn same_rate_mono_to_stereo_copies_and_fades() {
        let clip = vec![0.5f32; 4800];
        let out = convert(&clip, 48_000, 2);
        assert_eq!(out.len(), 4800 * 2);
        assert_eq!(out[0], 0.0); // faded in
        assert_eq!(out[2400 * 2], 0.5);
        assert_eq!(out[2400 * 2 + 1], 0.5); // both channels
        assert_eq!(*out.last().unwrap(), 0.0); // faded out
    }

    #[test]
    fn resampling_keeps_the_duration() {
        let clip = vec![0.25f32; 48_000]; // 1 s
        assert_eq!(convert(&clip, 44_100, 1).len(), 44_100);
        assert_eq!(convert(&clip, 96_000, 2).len(), 96_000 * 2);
        // A ramp stays a ramp (interpolated, not stepped).
        let ramp: Vec<f32> = (0..4800).map(|i| i as f32 / 10_000.0).collect();
        let up = convert(&ramp, 96_000, 1);
        assert!((up[4801] - 0.240_05).abs() < 1e-5, "{}", up[4801]);
    }

    #[test]
    fn bad_input_gives_silence_not_a_panic() {
        assert!(convert(&[], 48_000, 2).is_empty());
        assert!(convert(&[0.1], 0, 2).is_empty());
        assert!(convert(&[0.1], 48_000, 0).is_empty());
        let out = convert(&[f32::NAN, 9.0, -9.0], 48_000, 1);
        assert!(out.iter().all(|v| v.is_finite() && v.abs() <= 1.0));
    }
}
