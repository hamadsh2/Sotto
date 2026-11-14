// SPDX-License-Identifier: GPL-3.0-only
//! The audio engine's control thread for the window app.
//!
//! The `Controller` (device lookup, reconnects, engine switching) lives on its own
//! thread, so the window never waits on audio work and the audio never depends on the
//! window: closing or hiding the window changes nothing here.
//!
//! Three ways the window talks to the audio:
//! - **Settings** (strength, stage switches, …) are written straight into the shared
//!   `Params` atomics; the audio thread reads them every frame. No lock, no message.
//! - **Levels** for the meters are read straight from the shared `Stats` atomics.
//! - **Engine and device changes** need real work (loading a model, opening streams),
//!   so they go as a `Command` over a channel to this thread.
//!
//! The UI reads a small `Status` snapshot (a `Mutex` touched by this thread and on each
//! redraw by the UI — neither is the audio path, which stays lock-free).
//!
//! Python analogy: a `threading.Thread` running a loop, a `queue.Queue` for commands,
//! and a status object guarded by a `threading.Lock`.

use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use cpal::DeviceId;
use sotto::control::{Change, Chosen, Controller, EngineKind, Hear};
use sotto::devices::{self, Dir};
use sotto::params::Params;
use sotto::settings::SavedDevice;
use sotto::stats::Stats;

#[derive(Clone, Default, PartialEq)]
pub enum State {
    #[default]
    Starting,
    Running,
    /// No microphone (or no VB-Cable output) chosen yet.
    NeedsDevices,
    /// A chosen device is missing; we wait for it (never another device).
    Waiting(String),
    /// Not running, with the reason.
    Stopped(String),
}

/// A device as the window shows it: the endpoint ID as text (cpal's `DeviceId`
/// prints and parses), and its name.
#[derive(Clone, Default, PartialEq)]
pub struct Device {
    pub id: String,
    pub name: String,
}

impl From<&Chosen> for Device {
    fn from(c: &Chosen) -> Self {
        Self {
            id: c.id.to_string(),
            name: c.name.clone(),
        }
    }
}

#[derive(Clone, Default)]
pub struct Status {
    pub input: Option<Device>,
    pub output: Option<Device>,
    /// Microphones, and VB-Cable outputs, available to choose.
    pub inputs: Vec<Device>,
    pub outputs: Vec<Device>,
    /// The engine asked for.
    pub engine: Option<EngineKind>,
    /// An engine switch in progress (DeepFilterNet takes ~0.5 s to load).
    pub loading: Option<EngineKind>,
    pub state: State,
    pub cable_is_default: bool,
    /// No VB-Cable output exists at all.
    pub cable_missing: bool,
    /// Processing latency: one frame + the engine's delay + the ring buffer, ms.
    pub latency_ms: Option<f64>,
    /// The last command that failed, in words.
    pub message: Option<String>,
    /// For the meters (read without any lock).
    pub stats: Option<Arc<Stats>>,
    /// "Hear yourself": where it stands, how far a recording is, and the last problem.
    pub hear: Hear,
    pub hear_progress: f32,
    pub hear_error: Option<String>,
}

pub enum Command {
    SetEngine(EngineKind),
    /// DeepFilterNet's post-filter; reloads the model if it is running (~0.5 s).
    SetPostFilter(bool),
    /// Choose the device with this endpoint ID (as text).
    Select(Dir, String),
    /// Re-read the device lists now (e.g. the window was just shown).
    Refresh,
    /// "Hear yourself": record 5 s of the cleaned voice into memory.
    Record,
    /// Play that clip once on the Windows default playback device.
    Play,
    /// Stop recording or playing.
    StopHear,
    /// Stop and drop the clip (the window was hidden).
    ForgetClip,
    Quit,
}

pub struct Audio {
    tx: mpsc::Sender<Command>,
    thread: Option<JoinHandle<()>>,
    pub status: Arc<Mutex<Status>>,
    /// Settings the audio thread reads every frame; the window writes them directly.
    pub params: Arc<Params>,
}

/// What to open at start: devices saved last time, unless the command line names others.
pub struct Choice {
    /// `--input` / `--output`: part of a device name (for testing and measurements).
    pub input: Option<String>,
    pub output: Option<String>,
    pub saved_input: Option<SavedDevice>,
    pub saved_output: Option<SavedDevice>,
    pub engine: EngineKind,
    /// A problem with the settings file, to show once.
    pub note: Option<String>,
}

/// A slow change being retried while a previous switch finishes.
#[derive(Clone, Copy)]
enum Want {
    Engine(EngineKind),
    PostFilter(bool),
}

/// While the window is visible, how often device lists and the "CABLE is default"
/// check are refreshed (catches plugged/unplugged devices). Never while hidden.
const LIST_EVERY: Duration = Duration::from_secs(3);
const TICK_EVERY: Duration = Duration::from_secs(1);
/// Retry an engine switch this often while another one is still finishing.
const RETRY_EVERY: Duration = Duration::from_millis(100);
/// Check a recording / playback this often while one runs.
const HEAR_EVERY: Duration = Duration::from_millis(50);

impl Audio {
    /// `params` already holds the saved settings.
    pub fn spawn(choice: Choice, params: Arc<Params>, visible: Arc<AtomicBool>) -> Self {
        let status = Arc::new(Mutex::new(Status::default()));
        let (tx, rx) = mpsc::channel();
        let (st, p) = (Arc::clone(&status), Arc::clone(&params));
        let thread = thread::Builder::new()
            .name("sotto-control".into())
            .spawn(move || control_loop(choice, p, &st, &rx, &visible))
            .ok();
        if thread.is_none() {
            set(&status, |s| {
                s.state = State::Stopped("could not start the audio thread".into())
            });
        }
        Self {
            tx,
            thread,
            status,
            params,
        }
    }

    pub fn send(&self, cmd: Command) {
        let _ = self.tx.send(cmd);
    }

    /// Stop streams and the engine, and wait for the thread to end.
    pub fn stop(&mut self) {
        let _ = self.tx.send(Command::Quit);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Audio {
    fn drop(&mut self) {
        self.stop();
    }
}

fn set(status: &Mutex<Status>, f: impl FnOnce(&mut Status)) {
    if let Ok(mut s) = status.lock() {
        f(&mut s);
    }
}

/// Pick the starting devices. The mic is never guessed: it is the one named on the
/// command line, or the one saved last time (waited for if it's unplugged), or none
/// until the user picks it. The output is the one named, or the saved one, or else the
/// standard "CABLE Input" endpoint (VB-Cable only — `find_output` never returns anything
/// else).
fn initial_devices(ctl: &mut Controller, choice: &Choice) -> Option<String> {
    let mut err = None;
    let host = cpal::default_host();
    if let Some(q) = choice.input.as_deref() {
        match devices::find_input(&host, q) {
            Ok(m) => err = ctl.select(Dir::Input, &m.id).err(),
            Err(e) => err = Some(e),
        }
    } else if let Some(d) = &choice.saved_input
        && let Err(e) = ctl.restore(Dir::Input, &d.id, &d.name)
    {
        err = Some(e);
    }
    if choice.output.is_none()
        && let Some(d) = &choice.saved_output
    {
        if let Err(e) = ctl.restore(Dir::Output, &d.id, &d.name) {
            err = Some(e);
        } else {
            return err;
        }
    }
    if let Ok(c) = devices::find_output(&host, choice.output.as_deref())
        && let Err(e) = ctl.select(Dir::Output, &c.id)
    {
        err = Some(e);
    }
    err
}

fn select(ctl: &mut Controller, dir: Dir, id: &str) -> Result<(), String> {
    let id = DeviceId::from_str(id).map_err(|_| "Unknown device.".to_string())?;
    ctl.select(dir, &id)
}

fn control_loop(
    choice: Choice,
    params: Arc<Params>,
    status: &Mutex<Status>,
    rx: &mpsc::Receiver<Command>,
    visible: &AtomicBool,
) {
    let mut ctl = match Controller::idle(choice.engine, params) {
        Ok(c) => c,
        Err(e) => {
            set(status, |s| s.state = State::Stopped(e));
            // Nothing runs; just wait to be told to quit.
            let _ = rx.recv();
            return;
        }
    };
    let first_err = initial_devices(&mut ctl, &choice);
    set(status, |s| {
        s.stats = Some(Arc::clone(&ctl.stats));
        s.engine = Some(ctl.engine_kind());
        s.message = first_err.or(choice.note.clone());
    });

    let mut waiting_note = String::new();
    let mut pending: Option<Want> = None;
    let mut next_tick = Instant::now();
    let mut next_list = Instant::now();
    loop {
        let now = Instant::now();
        if now >= next_tick {
            next_tick = now + TICK_EVERY;
            if let Err(e) = ctl.tick() {
                set(status, |s| s.state = State::Stopped(e));
                break;
            }
            for n in ctl.take_notes() {
                if n.starts_with("Waiting for devices") || n.contains("disconnected") {
                    waiting_note = n;
                }
            }
            publish(&ctl, status, &mut waiting_note);
        }
        if visible.load(Relaxed) && now >= next_list {
            next_list = now + LIST_EVERY;
            refresh_lists(&ctl, status);
        }
        if let Some(want) = pending {
            pending = change(&mut ctl, want, status);
        }

        let busy_hear = matches!(ctl.hear(), Hear::Recording | Hear::Playing);
        if busy_hear {
            ctl.poll_hear();
            publish_hear(&mut ctl, status, None);
        }
        let wait = if busy_hear {
            HEAR_EVERY
        } else if pending.is_some() {
            RETRY_EVERY
        } else {
            next_tick.saturating_duration_since(Instant::now())
        };
        match rx.recv_timeout(wait) {
            Ok(Command::SetEngine(kind)) => pending = change(&mut ctl, Want::Engine(kind), status),
            Ok(Command::SetPostFilter(on)) => {
                pending = change(&mut ctl, Want::PostFilter(on), status)
            }
            Ok(Command::Select(dir, id)) => {
                let r = select(&mut ctl, dir, &id);
                set(status, |s| s.message = r.err());
                // Show the new choice at once; the streams open on the next tick.
                next_tick = Instant::now();
                refresh_lists(&ctl, status);
            }
            Ok(Command::Refresh) => {
                next_list = Instant::now() + LIST_EVERY;
                refresh_lists(&ctl, status);
            }
            Ok(Command::Record) => {
                let r = ctl.record();
                publish_hear(&mut ctl, status, Some(r));
            }
            Ok(Command::Play) => {
                let r = ctl.play();
                publish_hear(&mut ctl, status, Some(r));
            }
            Ok(Command::StopHear) => {
                ctl.stop_hear();
                publish_hear(&mut ctl, status, None);
            }
            Ok(Command::ForgetClip) => {
                ctl.forget_clip();
                publish_hear(&mut ctl, status, Some(Ok(())));
            }
            Ok(Command::Quit) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {}
        }
    }
    ctl.stop();
}

/// Make a slow change (engine switch, or post-filter = DeepFilterNet reload). Returns
/// `Some` to retry shortly if a previous switch is still finishing.
fn change(ctl: &mut Controller, want: Want, status: &Mutex<Status>) -> Option<Want> {
    let loading = match want {
        Want::Engine(kind) => Some(kind),
        // Only a running DeepFilterNet needs reloading; otherwise it's just stored.
        Want::PostFilter(_) => (ctl.engine_kind() == EngineKind::Dfn).then_some(EngineKind::Dfn),
    };
    set(status, |s| {
        if let Some(kind) = loading {
            s.loading = Some(kind);
            s.engine = Some(kind);
        }
    });
    // DeepFilterNet loads inside these calls (~0.5 s); the window keeps drawing meanwhile.
    let r = match want {
        Want::Engine(kind) => ctl.set_engine(kind),
        Want::PostFilter(on) => ctl.set_postfilter(on),
    };
    let _ = ctl.take_notes();
    let retry = matches!(r, Ok(Change::Busy));
    set(status, |s| {
        if !retry {
            s.loading = None;
            s.engine = Some(ctl.engine_kind());
        }
        s.message = r.err().map(|e| match want {
            Want::Engine(_) => format!("Could not switch engine: {e}"),
            Want::PostFilter(_) => format!("Could not change the post-filter: {e}"),
        });
    });
    retry.then_some(want)
}

/// Share "Hear yourself" state with the window. `result`: the outcome of a click, which
/// sets or clears the shown problem; `None` keeps it (but a note from the controller,
/// e.g. a recording that timed out, replaces it).
fn publish_hear(ctl: &mut Controller, status: &Mutex<Status>, result: Option<Result<(), String>>) {
    let note = ctl
        .take_notes()
        .into_iter()
        .rfind(|n| n.starts_with("Recording stopped"));
    let (hear, progress) = (ctl.hear(), ctl.record_progress());
    set(status, |s| {
        s.hear = hear;
        s.hear_progress = progress;
        match (result, note) {
            (_, Some(n)) => s.hear_error = Some(n),
            (Some(r), None) => s.hear_error = r.err(),
            (None, None) => {}
        }
    });
}

fn publish(ctl: &Controller, status: &Mutex<Status>, waiting_note: &mut String) {
    let running = ctl.is_running();
    if running {
        waiting_note.clear(); // never show an old reason after a reconnect
    }
    let buffer = ctl.stats.take_buffer_avg();
    set(status, |s| {
        s.state = if running {
            State::Running
        } else if !ctl.has_devices() {
            State::NeedsDevices
        } else {
            State::Waiting(waiting_note.clone())
        };
        s.input = ctl.input().map(Device::from);
        s.output = ctl.output().map(Device::from);
        s.latency_ms = running.then(|| ctl.engine_latency_ms() + buffer.unwrap_or(0.0) / 48.0);
        if s.loading.is_none() {
            s.engine = Some(ctl.engine_kind());
        }
    });
}

/// Re-read what can be chosen. Only lists devices and reads the default device's name
/// to compare; no stream is opened and nothing is changed.
fn refresh_lists(ctl: &Controller, status: &Mutex<Status>) {
    let list = |dir| {
        ctl.choices(dir)
            .map(|v| v.iter().map(Device::from).collect::<Vec<_>>())
    };
    let (inputs, outputs) = (list(Dir::Input), list(Dir::Output));
    let cable_is_default = ctl.cable_is_default();
    set(status, |s| {
        // Keep the old lists if Windows was busy changing them mid-read.
        if let Ok(v) = inputs {
            s.inputs = v;
        }
        if let Ok(v) = outputs {
            s.cable_missing = v.is_empty();
            s.outputs = v;
        }
        s.cable_is_default = cable_is_default;
        s.input = ctl.input().map(Device::from);
        s.output = ctl.output().map(Device::from);
    });
}
