// SPDX-License-Identifier: GPL-3.0-only
//! The audio engine's control thread for the window app.
//!
//! The `Controller` (device lookup, reconnects, engine switching) lives on its own
//! thread, so the window never waits on audio work and the audio never depends on the
//! window: closing or hiding the window changes nothing here.
//!
//! The UI reads a small `Status` snapshot (a `Mutex` touched once a second by this
//! thread and on each redraw by the UI — neither is the audio path, which stays
//! lock-free) and sends commands over a channel.
//!
//! Python analogy: a `threading.Thread` running a loop, a `queue.Queue` for commands,
//! and a status object guarded by a `threading.Lock`.

use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use sotto::control::{Controller, EngineKind};
use sotto::params::Params;

#[derive(Clone, Default)]
pub enum State {
    #[default]
    Starting,
    Running,
    /// A chosen device is missing; we wait for it (never another device).
    Waiting(String),
    /// Not running, with the reason.
    Stopped(String),
}

#[derive(Clone, Default)]
pub struct Status {
    pub input: String,
    pub output: String,
    pub engine: Option<EngineKind>,
    pub state: State,
    pub cable_is_default: bool,
}

enum Command {
    Quit,
}

pub struct Audio {
    tx: mpsc::Sender<Command>,
    thread: Option<JoinHandle<()>>,
    pub status: Arc<Mutex<Status>>,
}

/// What to open, from the command line for now (device pickers come in 3b).
pub struct Choice {
    pub input: Option<String>,
    pub output: Option<String>,
    pub engine: EngineKind,
}

impl Audio {
    pub fn spawn(choice: Choice) -> Self {
        let status = Arc::new(Mutex::new(Status::default()));
        let (tx, rx) = mpsc::channel();
        let st = Arc::clone(&status);
        let thread = thread::Builder::new()
            .name("sotto-control".into())
            .spawn(move || control_loop(choice, &st, &rx))
            .ok();
        if thread.is_none() {
            set(&status, |s| {
                s.state = State::Stopped("could not start the audio thread".into())
            });
        }
        Self { tx, thread, status }
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

fn control_loop(choice: Choice, status: &Mutex<Status>, rx: &mpsc::Receiver<Command>) {
    let ctl = match choice.input.as_deref() {
        Some(q) => Controller::new(
            q,
            choice.output.as_deref(),
            choice.engine,
            Arc::new(Params::new()),
        ),
        None => Err("No microphone chosen yet. For now, start Sotto with \
                     --input \"<part of your mic's name>\"; device pickers come next."
            .into()),
    };
    let mut ctl = match ctl {
        Ok(c) => c,
        Err(e) => {
            set(status, |s| s.state = State::Stopped(e));
            // Nothing runs; just wait to be told to quit.
            let _ = rx.recv();
            return;
        }
    };
    set(status, |s| {
        s.input = ctl.input_name().to_string();
        s.output = ctl.output_name().to_string();
        s.engine = Some(ctl.engine_kind());
        s.cable_is_default = ctl.cable_is_default();
    });

    let mut waiting_note = String::new();
    loop {
        if let Err(e) = ctl.tick() {
            set(status, |s| s.state = State::Stopped(e));
            break;
        }
        for n in ctl.take_notes() {
            if n.starts_with("Waiting for devices") || n.contains("disconnected") {
                waiting_note = n;
            }
        }
        let running = ctl.is_running();
        if running {
            waiting_note.clear(); // never show an old reason after a reconnect
        }
        set(status, |s| {
            s.state = if running {
                State::Running
            } else {
                State::Waiting(waiting_note.clone())
            };
            s.engine = Some(ctl.engine_kind());
        });
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(Command::Quit) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {}
        }
    }
    ctl.stop();
}
