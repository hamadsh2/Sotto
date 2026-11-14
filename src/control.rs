// SPDX-License-Identifier: GPL-3.0-only
//! Starts, watches and steers the audio engine: finds the two chosen devices, opens and
//! re-opens their streams, switches denoise engines, and frees retired models.
//!
//! Shared by the command-line tool and the window app, so both behave identically. It
//! runs on an ordinary thread, never on the audio path, so it may allocate and block
//! briefly (e.g. while DeepFilterNet loads).
//!
//! Python analogy: `Controller` is a class whose methods you call from a loop
//! (`tick()` once a second, `set_engine()` on a command). It collects human-readable
//! messages in `notes` instead of printing, so the CLI can print them and the window can
//! show them.

use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::{Duration, Instant};

use cpal::{DeviceId, Host};
use ringbuf::traits::{Consumer, Observer, Split};
use ringbuf::{HeapCons, HeapRb};

use crate::denoise::{Denoiser, Off, Rnnoise};
use crate::devices::{self, Dir};
use crate::dfn;
use crate::dsp::EngineSlot;
use crate::engine;
use crate::params::Params;
use crate::playback::{self, CLIP_SAMPLES};
use crate::stats::{self, Stats};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EngineKind {
    Rnnoise,
    Dfn,
    Off,
}

impl EngineKind {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "rnnoise" | "light" => Some(Self::Rnnoise),
            "dfn" | "deepfilternet" | "hq" => Some(Self::Dfn),
            "off" | "none" => Some(Self::Off),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Rnnoise => "rnnoise",
            Self::Dfn => "deepfilternet",
            Self::Off => "off",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Rnnoise => "Light — for gaming",
            Self::Dfn => "High quality — for recording & calls",
            Self::Off => "Off",
        }
    }
}

/// Outcome of `set_engine` / `set_postfilter`, for the caller to report.
#[derive(Debug, PartialEq, Eq)]
pub enum Change {
    Started,
    AlreadySo,
    /// A switch is still finishing (they take ~0.1–0.2 s): try again shortly.
    Busy,
    /// Stored; it takes effect when DeepFilterNet is selected.
    Stored,
}

/// A device the user chose: its exact Windows endpoint ID plus its name for messages.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Chosen {
    pub id: DeviceId,
    pub name: String,
}

pub struct Controller {
    host: Host,
    /// `None` until the user picks one. Streams open only when both are chosen.
    input: Option<Chosen>,
    output: Option<Chosen>,
    pub params: Arc<Params>,
    pub stats: Arc<Stats>,
    worker: Option<engine::Worker>,
    streams: Option<engine::Engine>,
    engine_kind: EngineKind,
    /// Delay of the engine last asked for, in samples.
    engine_latency: usize,
    switch_seq: u64,
    lost_reported: bool,
    /// Counts stream (re)openings, so callers can reset clock-drift tracking.
    generation: u64,
    notes: Vec<String>,
    /// "Hear yourself": a recording in progress (the worker fills the other half).
    recording: Option<(HeapCons<f32>, Instant)>,
    /// The last finished recording (48 kHz mono, in memory only).
    clip: Option<Vec<f32>>,
    /// The clip playing right now.
    player: Option<playback::Player>,
}

/// Where "Hear yourself" stands.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Hear {
    #[default]
    Idle,
    Recording,
    /// A clip is ready to play.
    Ready,
    Playing,
}

/// A recording that hasn't filled up this long after starting is given up (e.g. the
/// mic was unplugged mid-way).
const RECORD_TIMEOUT: Duration = Duration::from_secs(playback::CLIP_SECONDS as u64 + 3);

impl Controller {
    /// Resolve the two devices by name, remember their exact Windows endpoint IDs, build
    /// the first engine and start the denoise worker. Streams open on the first `tick`.
    pub fn new(
        input_query: &str,
        output_query: Option<&str>,
        kind: EngineKind,
        params: Arc<Params>,
    ) -> Result<Self, String> {
        let mut ctl = Self::idle(kind, params)?;
        let mic = devices::find_input(&ctl.host, input_query)?;
        let cable = devices::find_output(&ctl.host, output_query)?;
        ctl.input = Some(Chosen {
            id: mic.id,
            name: mic.name,
        });
        ctl.output = Some(Chosen {
            id: cable.id,
            name: cable.name,
        });
        Ok(ctl)
    }

    /// Start the denoise worker with no devices chosen yet: nothing is opened until
    /// `select` has been called for both an input and an output.
    pub fn idle(kind: EngineKind, params: Arc<Params>) -> Result<Self, String> {
        let host = cpal::default_host(); // WASAPI on Windows
        let stats = Arc::new(Stats::new());
        let mut notes = Vec::new();
        let first = make_engine(kind, &params, &mut notes)?;
        let engine_latency = first.latency();
        let worker = engine::Worker::spawn(first, Arc::clone(&params), Arc::clone(&stats))?;
        Ok(Self {
            host,
            input: None,
            output: None,
            params,
            stats,
            worker: Some(worker),
            streams: None,
            engine_kind: kind,
            engine_latency,
            switch_seq: 0,
            lost_reported: false,
            generation: 0,
            notes,
            recording: None,
            clip: None,
            player: None,
        })
    }

    pub fn input(&self) -> Option<&Chosen> {
        self.input.as_ref()
    }

    pub fn output(&self) -> Option<&Chosen> {
        self.output.as_ref()
    }

    pub fn input_name(&self) -> &str {
        self.input.as_ref().map_or("", |c| c.name.as_str())
    }

    pub fn output_name(&self) -> &str {
        self.output.as_ref().map_or("", |c| c.name.as_str())
    }

    /// The devices the user may choose from right now (mics; VB-Cable outputs only).
    /// Only lists them — opens nothing.
    pub fn choices(&self, dir: Dir) -> Result<Vec<Chosen>, String> {
        Ok(devices::choices(&self.host, dir)?
            .into_iter()
            .map(|d| Chosen {
                id: d.id,
                name: d.name,
            })
            .collect())
    }

    /// Use the device with exactly this endpoint ID for `dir`. It must be in
    /// `choices(dir)` now (so an output is always a VB-Cable endpoint). On a change the
    /// streams stop and reopen on the new pair at the next `tick`; the old device is
    /// released, and there is never a fallback to some other device.
    pub fn select(&mut self, dir: Dir, id: &DeviceId) -> Result<(), String> {
        let found = devices::choices(&self.host, dir)?
            .into_iter()
            .find(|d| &d.id == id)
            .ok_or_else(|| match dir {
                Dir::Input => "That microphone is not connected.".to_string(),
                Dir::Output => "That VB-Cable output is not available.".to_string(),
            })?;
        let chosen = Some(Chosen {
            id: found.id,
            name: found.name,
        });
        let slot = match dir {
            Dir::Input => &mut self.input,
            Dir::Output => &mut self.output,
        };
        if *slot == chosen {
            return Ok(());
        }
        *slot = chosen;
        self.streams = None; // stops both streams; tick() opens the new pair
        self.lost_reported = false;
        Ok(())
    }

    /// Bring back a device chosen in an earlier session (from the settings file). It
    /// may be unplugged right now: then the streams wait for exactly that device (by ID,
    /// or exact full name). An output is accepted only if its name is a VB-Cable one,
    /// and `find_chosen` again only ever finds VB-Cable endpoints for it.
    pub fn restore(&mut self, dir: Dir, id: &str, name: &str) -> Result<(), String> {
        let id: DeviceId = id
            .parse()
            .map_err(|_| "Saved device ID is invalid.".to_string())?;
        if dir == Dir::Output && !devices::is_vb_cable(name) {
            return Err("The saved output isn't a VB-Cable device; choose it again.".into());
        }
        if dir == Dir::Input && devices::is_vb_cable(name) {
            return Err("The saved microphone is a VB-Cable device; choose it again.".into());
        }
        let chosen = Some(Chosen {
            id,
            name: name.to_string(),
        });
        match dir {
            Dir::Input => self.input = chosen,
            Dir::Output => self.output = chosen,
        }
        self.streams = None;
        self.lost_reported = false;
        Ok(())
    }

    /// Both devices are chosen (streams may still be waiting for one to appear).
    pub fn has_devices(&self) -> bool {
        self.input.is_some() && self.output.is_some()
    }

    /// The engine last asked for (the one heard is `stats.active_engine`).
    pub fn engine_kind(&self) -> EngineKind {
        self.engine_kind
    }

    /// Delay added by the chosen engine plus the one-frame (10 ms) collection, in ms.
    /// The ring buffer's share is measured separately (`stats.take_buffer_avg`).
    pub fn engine_latency_ms(&self) -> f64 {
        (self.engine_latency + crate::denoise::FRAME) as f64 / 48.0
    }

    pub fn is_running(&self) -> bool {
        self.streams.is_some()
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// True if "CABLE Input" is Windows' default playback device (we only warn).
    pub fn cable_is_default(&self) -> bool {
        devices::default_playback_is_cable(&self.host)
    }

    /// Messages since the last call, oldest first.
    pub fn take_notes(&mut self) -> Vec<String> {
        std::mem::take(&mut self.notes)
    }

    /// Call about once a second: notices a lost device, (re)opens the streams when both
    /// devices are present, and frees retired engines. `Err` means the denoise worker
    /// died, which is not recoverable.
    pub fn tick(&mut self) -> Result<(), String> {
        let Some(worker) = self.worker.as_ref() else {
            return Err("the engine is stopped".into());
        };
        if self.stats.worker_failed.load(Relaxed) {
            self.streams = None;
            return Err("the denoise engine stopped unexpectedly".into());
        }
        drain_retired(worker, &mut self.notes);

        // Only while streams run: the flags stay set until the device is back, and the
        // loss is reported once.
        let in_lost = self.stats.input_failed.load(Relaxed);
        if self.streams.is_some() && (in_lost || self.stats.output_failed.load(Relaxed)) {
            let which = if in_lost { "Microphone" } else { "VB-Cable" };
            self.notes.push(format!(
                "{which} disconnected — stopping streams and waiting for it to come back."
            ));
            self.streams = None; // drops (stops) both streams
            self.lost_reported = true;
        }

        // (Re)open. Only the two remembered endpoint IDs are ever looked up; if either
        // is missing we wait — never a fallback device.
        if let (None, Some(mic), Some(cable)) = (&self.streams, &self.input, &self.output) {
            let attempt = devices::find_chosen(&self.host, Dir::Input, &mic.id, &mic.name)
                .and_then(|i| {
                    let o = devices::find_chosen(&self.host, Dir::Output, &cable.id, &cable.name)?;
                    self.stats.input_failed.store(false, Relaxed);
                    self.stats.output_failed.store(false, Relaxed);
                    let e = devices::no_panic("opening streams", || {
                        engine::start(&i.device, &o.device, worker, Arc::clone(&self.stats))
                    })?;
                    Ok((e, i.id, o.id))
                });
            match attempt {
                Ok((e, in_id, out_id)) => {
                    // Found by its exact name under a new ID: remember the new one.
                    for (slot, id) in [(&mut self.input, in_id), (&mut self.output, out_id)] {
                        if let Some(c) = slot.as_mut() {
                            c.id = id;
                        }
                    }
                    // Drop numbers left over from before a disconnect.
                    let _ = self.stats.take_timing();
                    let _ = self.stats.take_buffer_avg();
                    self.stats.gate_open_frames.store(0, Relaxed);
                    self.stats.limit_cdb.store(0, Relaxed);
                    self.notes.push(format!(
                        "Streams open: mic {}ch → 48 kHz mono → CABLE {}ch (shared mode)",
                        e.input_channels, e.output_channels
                    ));
                    self.streams = Some(e);
                    self.generation += 1;
                    self.lost_reported = false;
                }
                Err(e) if !self.lost_reported => {
                    self.notes.push(format!("Waiting for devices: {e}"));
                    self.lost_reported = true;
                }
                Err(_) => {}
            }
        }
        Ok(())
    }

    /// Switch the denoise engine (crossfaded). DeepFilterNet loads here, which blocks
    /// for ~0.5 s.
    pub fn set_engine(&mut self, kind: EngineKind) -> Result<Change, String> {
        if kind == self.engine_kind {
            return Ok(Change::AlreadySo);
        }
        self.switch(kind, None)
    }

    /// Turn DeepFilterNet's post-filter on/off. The setting is stored only once applied,
    /// so it always matches the running model.
    pub fn set_postfilter(&mut self, on: bool) -> Result<Change, String> {
        if on == self.params.dfn_postfilter.load(Relaxed) {
            return Ok(Change::AlreadySo);
        }
        if self.engine_kind != EngineKind::Dfn {
            self.params.dfn_postfilter.store(on, Relaxed);
            return Ok(Change::Stored);
        }
        self.switch(EngineKind::Dfn, Some(on))
    }

    /// One switch at a time. So at most two models run at once: the old and the new
    /// one, during the warm-up and 30 ms crossfade of a switch.
    fn switch(&mut self, kind: EngineKind, postfilter: Option<bool>) -> Result<Change, String> {
        let Some(worker) = self.worker.as_ref() else {
            return Err("the engine is stopped".into());
        };
        if self.stats.switch_done.load(Relaxed) != self.switch_seq {
            return Ok(Change::Busy);
        }
        let old_pf = postfilter.map(|on| self.params.dfn_postfilter.swap(on, Relaxed));
        // Free a just-retired model before loading another one.
        drain_retired(worker, &mut self.notes);
        match make_engine(kind, &self.params, &mut self.notes) {
            // Count the switch only once the worker has it, so a failed send can't leave
            // every later switch stuck at `Busy`.
            Ok(slot) => {
                let latency = slot.latency();
                match worker.set_engine(slot, self.switch_seq + 1) {
                    Ok(()) => {
                        self.engine_latency = latency;
                        self.switch_seq += 1;
                        self.engine_kind = kind;
                        Ok(Change::Started)
                    }
                    Err(e) => {
                        if let Some(old) = old_pf {
                            self.params.dfn_postfilter.store(old, Relaxed);
                        }
                        Err(e)
                    }
                }
            }
            Err(e) => {
                if let Some(old) = old_pf {
                    self.params.dfn_postfilter.store(old, Relaxed);
                }
                Err(e)
            }
        }
    }

    /// Start recording the next 5 s of what CABLE Input gets (the cleaned voice), into
    /// memory. Stops any playback first: the clip is never heard while recording.
    pub fn record(&mut self) -> Result<(), String> {
        let Some(worker) = self.worker.as_ref() else {
            return Err("the engine is stopped".into());
        };
        if self.streams.is_none() {
            return Err("The microphone isn't running.".into());
        }
        self.player = None;
        if self.recording.take().is_some() {
            worker.stop_recording();
        }
        // Allocated here, on this thread; the worker only copies into it.
        let (prod, cons) = HeapRb::<f32>::new(CLIP_SAMPLES).split();
        worker.record(prod)?;
        self.recording = Some((cons, Instant::now()));
        self.clip = None;
        Ok(())
    }

    /// Play the recorded clip once on Windows' default playback device (never a
    /// virtual cable or Sotto's own output). See `playback`.
    pub fn play(&mut self) -> Result<(), String> {
        if self.recording.is_some() {
            return Err("Still recording.".into());
        }
        let Some(clip) = self.clip.as_deref() else {
            return Err("Record a clip first.".into());
        };
        self.player = None;
        let avoid = self.output.as_ref().map(|c| &c.id);
        let dev = playback::resolve(&self.host, avoid)?;
        let player = devices::no_panic("starting playback", || playback::play(&dev, clip))?;
        self.player = Some(player);
        Ok(())
    }

    /// Stop recording or playing at once.
    pub fn stop_hear(&mut self) {
        self.player = None;
        if self.recording.take().is_some()
            && let Some(w) = self.worker.as_ref()
        {
            w.stop_recording();
        }
    }

    /// Stop, and drop the clip from memory.
    pub fn forget_clip(&mut self) {
        self.stop_hear();
        self.clip = None;
    }

    /// Call every ~100 ms while `hear()` isn't Idle/Ready: picks up a finished
    /// recording, closes a finished playback, frees returned rings.
    pub fn poll_hear(&mut self) {
        let Some(worker) = self.worker.as_ref() else {
            return;
        };
        let mut returned = false;
        while let Ok(ring) = worker.recorded.try_recv() {
            drop(ring); // freed here, not on the audio path
            returned = true;
        }
        if let Some((cons, started)) = self.recording.as_mut() {
            if returned && cons.is_full() {
                let mut clip = vec![0.0f32; CLIP_SAMPLES];
                let n = cons.pop_slice(&mut clip);
                clip.truncate(n);
                self.clip = Some(clip);
                self.recording = None;
            } else if started.elapsed() > RECORD_TIMEOUT {
                self.stop_hear();
                self.notes
                    .push("Recording stopped: no audio came through.".to_string());
            }
        }
        if self.player.as_ref().is_some_and(|p| p.is_done()) {
            self.player = None; // closes the stream
        }
    }

    pub fn hear(&self) -> Hear {
        if self.recording.is_some() {
            Hear::Recording
        } else if self.player.is_some() {
            Hear::Playing
        } else if self.clip.is_some() {
            Hear::Ready
        } else {
            Hear::Idle
        }
    }

    /// 0..=1 through the recording.
    pub fn record_progress(&self) -> f32 {
        self.recording
            .as_ref()
            .map_or(0.0, |(c, _)| c.occupied_len() as f32 / CLIP_SAMPLES as f32)
    }

    /// Stop streams first, then the worker they feed (same as dropping it).
    pub fn stop(self) {}
}

impl Drop for Controller {
    fn drop(&mut self) {
        self.player = None;
        self.recording = None;
        self.streams = None;
        if let Some(w) = self.worker.take() {
            w.stop();
        }
    }
}

/// Build an engine. DeepFilterNet is loaded only here, i.e. only when selected
/// ("lazy loading").
fn make_engine(
    kind: EngineKind,
    p: &Params,
    notes: &mut Vec<String>,
) -> Result<EngineSlot, String> {
    let den: Box<dyn Denoiser> = match kind {
        EngineKind::Rnnoise => Box::new(Rnnoise::new()),
        EngineKind::Off => Box::new(Off),
        EngineKind::Dfn => {
            let t0 = Instant::now();
            let d = dfn::DeepFilter::start(p.dfn_postfilter.load(Relaxed))?;
            notes.push(format!(
                "DeepFilterNet ready in {:.0} ms (adds {:.0} ms latency)",
                t0.elapsed().as_secs_f64() * 1e3,
                d.latency() as f64 / 48.0
            ));
            Box::new(d)
        }
    };
    Ok(EngineSlot::new(den))
}

/// Drop engines the worker has finished with, here rather than on the audio path:
/// dropping DeepFilterNet stops its thread and frees its model.
fn drain_retired(worker: &engine::Worker, notes: &mut Vec<String>) {
    while let Ok(old) = worker.retired.try_recv() {
        let name = old.name();
        drop(old);
        if name == "deepfilternet" {
            let (_, private) = stats::process_memory().unwrap_or((0, 0));
            notes.push(format!(
                "DeepFilterNet unloaded (private memory now {:.1} MB)",
                private as f64 / 1_048_576.0
            ));
        }
    }
}
