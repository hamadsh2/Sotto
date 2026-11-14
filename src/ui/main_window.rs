// SPDX-License-Identifier: GPL-3.0-only
//! The main window: engine selector, strength, level meters, device pickers, the
//! voice-chain switches (with fine settings that open under their row), warnings, and
//! a CPU · latency readout. "About Sotto" near the bottom swaps in the About page (version,
//! license, third-party licenses) in the same window.
//!
//! How the controls reach the audio (see `audio.rs`):
//! - strength, the stage switches and their fine settings write `Params` atomics
//!   directly — instant, no lock;
//! - engine, post-filter and device choices are sent as commands to the control thread.
//!
//! Every change also asks for a delayed save of the settings file (`super::save_soon`).
//!
//! Redraws: about 30 a second while visible, and only while a meter is moving or once
//! a second for the readouts. While hidden: no redraw at all — the loop just wakes once
//! a second to keep the CPU/memory numbers current.
//!
//! Python analogy: `MainWindow` is a widget class; `render` is like a `paint()` that
//! rebuilds the whole layout from the current values each time (GPUI diffs it).

use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, App, AppContext, ClickEvent, Context, Div, Entity, FocusHandle, FontWeight,
    InteractiveElement, IntoElement, KeyDownEvent, ParentElement, Render, SharedString,
    StatefulInteractiveElement, Styled, Subscription, Task, Window, div, px, relative, svg,
};
use gpui_component::input::{Input, InputEvent, InputState, SelectAll};
use gpui_component::select::{Select, SelectEvent, SelectItem, SelectState};
use gpui_component::slider::{Slider, SliderEvent, SliderState};
use gpui_component::switch::Switch;
use gpui_component::{Disableable, IndexPath, Sizable};
use sotto::control::{EngineKind, Hear};
use sotto::devices::{Dir, VB_CABLE_URL};
use sotto::params::{
    AGC_MAX_DB_MAX, AGC_MAX_DB_MIN, EQ_DB_LIMIT, EqPreset, GATE_DB_MAX, GATE_DB_MIN, Params,
};
use sotto::stats;
use sotto::tune::{self, Failure, Phase, Suggestion};

use super::assets;
use super::audio::{Command, Device, State, Status};
use super::health::{Health, Hints};
use super::licenses::{self, License};
use super::meter::{self, Meter};
use super::theme::*;
use super::{
    AppState, UpdateState, check_updates, hotkey, install_update, restart, save_soon, toggle_mute,
};

/// Meter frame interval while visible (~30 Hz).
const METER_EVERY: Duration = Duration::from_millis(33);
/// Wake-up interval while hidden: only the CPU/memory sampling runs.
const HIDDEN_EVERY: Duration = Duration::from_secs(1);
/// Content column width cap, so a wide (e.g. tiled) window keeps short lines.
const MAX_CONTENT_W: f32 = 520.0;
const EQ_BANDS: [&str; 3] = ["Body", "Mud", "Presence"];

impl SelectItem for Device {
    type Value = String;

    fn title(&self) -> SharedString {
        self.name.clone().into()
    }

    fn value(&self) -> &String {
        &self.id
    }

    /// A row in the open list. gpui-component pads each row (8 px sides, 4 px top and
    /// bottom) and sets no cursor; this stretches over that padding so the whole row
    /// shows the hand.
    fn render(&self, _: &mut Window, _: &mut App) -> impl IntoElement {
        div()
            .mx(-SP_8)
            .my(-SP_4)
            .px(SP_8)
            .py(SP_4)
            .cursor_pointer()
            .child(self.name.clone())
    }
}

type DeviceSelect = Entity<SelectState<Vec<Device>>>;

/// The voice-chain stages, in chain order.
#[derive(Clone, Copy, PartialEq)]
enum Stage {
    Hpf,
    Eq,
    Gate,
    Agc,
    Limiter,
}

impl Stage {
    const ALL: [Stage; 5] = [Self::Hpf, Self::Eq, Self::Gate, Self::Agc, Self::Limiter];

    fn id(self) -> &'static str {
        match self {
            Self::Hpf => "hpf",
            Self::Eq => "eq",
            Self::Gate => "gate",
            Self::Agc => "agc",
            Self::Limiter => "limiter",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Hpf => "Low-cut filter",
            Self::Eq => "Voice EQ",
            Self::Gate => "Noise gate",
            Self::Agc => "Auto gain",
            Self::Limiter => "Limiter",
        }
    }

    /// The `Params` switch for this stage.
    fn flag(self, p: &Params) -> &AtomicBool {
        match self {
            Self::Hpf => &p.hpf,
            Self::Eq => &p.eq,
            Self::Gate => &p.gate,
            Self::Agc => &p.agc,
            Self::Limiter => &p.limiter,
        }
    }

    /// Has fine settings that open under its row.
    fn tunable(self) -> bool {
        matches!(self, Self::Eq | Self::Gate | Self::Agc)
    }

    /// The small line under the label: what it does, or its current value.
    fn hint(self, p: &Params) -> String {
        match self {
            Self::Hpf => "Removes rumble below 80 Hz".into(),
            Self::Eq => p.eq_preset().label().into(),
            Self::Gate => format!("Opens at {}", db(p.gate_threshold_db())),
            Self::Agc => format!("Up to {}", db_signed(p.agc_max_db())),
            Self::Limiter => "Stops sudden peaks".into(),
        }
    }
}

/// "−45 dB" with a real minus sign (U+2212), not a hyphen.
fn db(v: f32) -> String {
    format!("{v:.0} dB").replace('-', "\u{2212}")
}

/// "+12 dB" / "−1.5 dB" / "0 dB".
fn db_signed(v: f32) -> String {
    let s = if v.fract().abs() > 0.01 {
        format!("{v:+.1} dB")
    } else {
        format!("{v:+.0} dB")
    };
    match s.as_str() {
        "+0 dB" | "-0 dB" => "0 dB".into(),
        _ => s.replace('-', "\u{2212}"),
    }
}

/// A value beside a slider that can also be typed in.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    Strength,
    Gate,
    Agc,
    Eq(usize),
}

impl Field {
    fn id(self) -> SharedString {
        match self {
            Field::Strength => "val-strength".into(),
            Field::Gate => "val-gate".into(),
            Field::Agc => "val-agc".into(),
            Field::Eq(i) => format!("val-eq-{i}").into(),
        }
    }

    /// The slider's step: typed values are rounded to it, so slider and text agree.
    fn step(self) -> f32 {
        match self {
            Field::Eq(_) => 0.5,
            _ => 1.0,
        }
    }
}

/// Read a typed number: "-45", "−45 dB", "+12", "80%", " 3.5 " all work. `None` if it
/// isn't a number (then the old value stays).
fn parse_number(text: &str) -> Option<f32> {
    let t = text
        .trim()
        .replace('\u{2212}', "-")
        .to_lowercase()
        .replace("db", "")
        .replace('%', "");
    let v: f32 = t.trim().trim_start_matches('+').trim().parse().ok()?;
    v.is_finite().then_some(v)
}

pub struct MainWindow {
    status: Arc<Mutex<Status>>,
    params: Arc<Params>,
    strength: Entity<SliderState>,
    gate: Entity<SliderState>,
    agc: Entity<SliderState>,
    eq: [Entity<SliderState>; 3],
    input_select: DeviceSelect,
    output_select: DeviceSelect,
    /// What the pickers currently show, to update them only when something changed.
    shown: [(Vec<Device>, Option<String>); 2],
    /// The stage whose fine settings are open (one at a time).
    expanded: Option<Stage>,
    /// "Tune for my room" is open under its row.
    tune_open: bool,
    /// A measurement in progress, with the engine it measures with.
    tuning: Option<(tune::Session, Option<EngineKind>)>,
    /// The last finished measurement, until it's applied or started again.
    tune_done: Option<(Result<Suggestion, Failure>, Option<EngineKind>)>,
    /// The last result was applied (shows a short confirmation).
    tune_applied: bool,
    /// Mic too loud / too quiet, checked while the window is visible.
    health: Health,
    hints: Hints,
    /// "Hear yourself" was used since the window was shown (its clip is dropped from
    /// memory when the window hides).
    hear_used: bool,
    /// Set if Windows refused to add/remove the "Start with Windows" entry.
    autostart_error: Option<String>,
    /// The value being typed in (one at a time) and its text box.
    editing: Option<Field>,
    edit: Entity<InputState>,
    /// Recording a new mute shortcut: the box has keyboard focus and the old shortcut
    /// is paused.
    recording: bool,
    rec_focus: FocusHandle,
    /// Why the last pressed shortcut wasn't accepted.
    rec_error: Option<String>,
    /// The About page is shown instead of the controls.
    about: bool,
    /// The license opened on the About page (0 = Sotto's own, n = third-party n-1) and
    /// its text reflowed for the window's width.
    open_license: Option<(usize, SharedString)>,
    meter_in: Meter,
    meter_out: Meter,
    cpu_pct: f64,
    private_mb: f64,
    last_cpu: Duration,
    last_wall: Instant,
    cores: f64,
    _subs: Vec<Subscription>,
    /// Dropping the window drops this task, which stops the loop.
    _refresh: Task<()>,
}

fn send(cmd: Command, cx: &mut App) {
    cx.global::<AppState>().audio.send(cmd);
}

fn slider(cx: &mut App, min: f32, max: f32, step: f32, value: f32) -> Entity<SliderState> {
    cx.new(|_| {
        SliderState::new()
            .min(min)
            .max(max)
            .step(step)
            .default_value(value)
    })
}

impl MainWindow {
    pub fn new(
        status: Arc<Mutex<Status>>,
        params: Arc<Params>,
        visible: Arc<AtomicBool>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let strength = slider(cx, 0.0, 100.0, 1.0, params.strength() * 100.0);
        let gate = slider(
            cx,
            GATE_DB_MIN,
            GATE_DB_MAX,
            1.0,
            params.gate_threshold_db(),
        );
        let agc = slider(cx, AGC_MAX_DB_MIN, AGC_MAX_DB_MAX, 1.0, params.agc_max_db());
        let custom = cx.global::<AppState>().settings.eq_custom;
        let eq = [0, 1, 2].map(|i| slider(cx, -EQ_DB_LIMIT, EQ_DB_LIMIT, 0.5, custom[i]));

        // Each slider writes its `Params` value (clamped there) and asks for a save.
        let mut subs = Vec::new();
        let on_slide = |cx: &mut Context<Self>, s: &Entity<SliderState>, set: fn(&Params, f32)| {
            let p = Arc::clone(&params);
            cx.subscribe(s, move |_, _, ev: &SliderEvent, cx| {
                let SliderEvent::Change(v) = ev;
                set(&p, v.start());
                save_soon(cx);
                cx.notify();
            })
        };
        subs.push(on_slide(cx, &strength, |p, v| p.set_strength(v / 100.0)));
        subs.push(on_slide(cx, &gate, |p, v| p.set_gate_threshold_db(v)));
        subs.push(on_slide(cx, &agc, |p, v| p.set_agc_max_db(v)));
        for (i, band) in eq.iter().enumerate() {
            let p = Arc::clone(&params);
            subs.push(cx.subscribe(band, move |_, _, ev: &SliderEvent, cx| {
                let SliderEvent::Change(v) = ev;
                let mut gains = cx.global::<AppState>().settings.eq_custom;
                gains[i] = v.start();
                p.set_eq_custom(gains);
                // Remember them now, so picking a named preset and coming back to
                // Custom brings them back even before the next save.
                cx.global_mut::<AppState>().settings.eq_custom = p.eq_gains();
                save_soon(cx);
                cx.notify();
            }));
        }

        // One text box, shown in place of whichever value is being typed.
        let edit = cx.new(|cx| InputState::new(window, cx));
        subs.push(
            cx.subscribe_in(&edit, window, |this, _, ev: &InputEvent, window, cx| {
                if matches!(ev, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                    this.commit_edit(window, cx);
                }
            }),
        );

        let input_select = cx.new(|cx| SelectState::new(Vec::<Device>::new(), None, window, cx));
        let output_select = cx.new(|cx| SelectState::new(Vec::<Device>::new(), None, window, cx));
        for (sel, dir) in [(&input_select, Dir::Input), (&output_select, Dir::Output)] {
            subs.push(
                cx.subscribe(sel, move |_, _, ev: &SelectEvent<Vec<Device>>, cx| {
                    if let SelectEvent::Confirm(Some(id)) = ev {
                        send(Command::Select(dir, id.clone()), cx);
                        save_soon(cx);
                    }
                }),
            );
        }

        let stats_src = Arc::clone(&status);
        let refresh = cx.spawn(async move |this, cx| {
            let mut last = Instant::now();
            let mut last_sample = Instant::now();
            loop {
                let shown = visible.load(Relaxed);
                let wait = if shown { METER_EVERY } else { HIDDEN_EVERY };
                cx.background_executor().timer(wait).await;
                let (st, hear_busy) = stats_src
                    .lock()
                    .map(|s| {
                        (
                            s.stats.clone(),
                            matches!(s.hear, Hear::Recording | Hear::Playing),
                        )
                    })
                    .unwrap_or((None, false));
                let alive = this.update(cx, |this, cx| {
                    let now = Instant::now();
                    let dt = now.duration_since(last).as_secs_f32();
                    last = now;
                    let (lin, lout) = st.as_ref().map_or((0.0, 0.0), |s| s.take_levels());
                    let second = now.duration_since(last_sample) >= Duration::from_secs(1);
                    if second {
                        last_sample = now;
                        this.sample();
                    }
                    if visible.load(Relaxed) {
                        this.check_tuning_and_health(st.as_deref(), now, cx);
                    } else {
                        // Measuring needs the user watching the steps: hiding cancels.
                        this.tuning = None;
                        this.health.reset();
                        this.hints = Hints::default();
                        // Hiding stops any recording or playback and frees the clip.
                        if this.hear_used {
                            this.hear_used = false;
                            send(Command::ForgetClip, cx);
                        }
                    }
                    if !visible.load(Relaxed) || this.about {
                        // Hidden (or the About page, which has no meters or readouts):
                        // forget the levels so the meters start from rest, and don't draw.
                        this.meter_in.reset();
                        this.meter_out.reset();
                        return;
                    }
                    let was_moving = this.meter_in.is_moving() || this.meter_out.is_moving();
                    this.meter_in.update(lin, dt);
                    this.meter_out.update(lout, dt);
                    let moving = this.meter_in.is_moving() || this.meter_out.is_moving();
                    // Draw while moving, one last frame when they come to rest, and
                    // once a second for the readouts.
                    if was_moving || moving || second || hear_busy {
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        });

        Self {
            status,
            params,
            strength,
            gate,
            agc,
            eq,
            input_select,
            output_select,
            shown: Default::default(),
            expanded: None,
            tune_open: false,
            tuning: None,
            tune_done: None,
            tune_applied: false,
            health: Health::default(),
            hints: Hints::default(),
            hear_used: false,
            autostart_error: None,
            editing: None,
            edit,
            recording: false,
            rec_focus: cx.focus_handle(),
            rec_error: None,
            about: false,
            open_license: None,
            meter_in: Meter::default(),
            meter_out: Meter::default(),
            cpu_pct: 0.0,
            private_mb: stats::process_memory().map_or(0.0, |(_, p)| p as f64 / 1_048_576.0),
            last_cpu: stats::process_cpu_time().unwrap_or_default(),
            last_wall: Instant::now(),
            cores: std::thread::available_parallelism().map_or(1, |n| n.get()) as f64,
            _subs: subs,
            _refresh: refresh,
        }
    }

    /// Whole-process CPU and private memory, like the CLI's report.
    fn sample(&mut self) {
        let now = Instant::now();
        let cpu = stats::process_cpu_time().unwrap_or_default();
        let wall = now.duration_since(self.last_wall).as_secs_f64();
        if wall > 0.0 {
            self.cpu_pct =
                cpu.saturating_sub(self.last_cpu).as_secs_f64() / (wall * self.cores) * 100.0;
        }
        (self.last_cpu, self.last_wall) = (cpu, now);
        if let Some((_, private)) = stats::process_memory() {
            self.private_mb = private as f64 / 1_048_576.0;
        }
    }

    /// On each refresh while visible: move a measurement on, and update the mic hints.
    fn check_tuning_and_health(
        &mut self,
        st: Option<&stats::Stats>,
        now: Instant,
        cx: &mut Context<Self>,
    ) {
        if let Some((t, engine)) = self.tuning.as_mut() {
            let done = match t.tick() {
                Phase::Done(r) => Some((r.clone(), *engine)),
                _ => None,
            };
            if let Some(d) = done {
                self.tuning = None;
                self.tune_done = Some(d);
            }
            cx.notify(); // the progress bar moves
        }
        if let Some(s) = st {
            let p = &self.params;
            let boost = if p.agc.load(Relaxed) {
                p.agc_max_db()
            } else {
                0.0
            };
            let hints = self.health.update(
                now,
                s.clip_frames.load(Relaxed),
                s.speech_frames.load(Relaxed),
                f32::from_bits(s.voice_db.load(Relaxed)),
                boost,
            );
            if hints != self.hints {
                self.hints = hints;
                cx.notify();
            }
        }
    }

    fn start_tune(&mut self, cx: &mut Context<Self>) {
        let (stats, engine) = self
            .status
            .lock()
            .map(|s| (s.stats.clone(), s.engine))
            .unwrap_or((None, None));
        if let Some(stats) = stats {
            self.tuning = Some((tune::Session::start(stats), engine));
            self.tune_done = None;
            self.tune_applied = false;
        }
        cx.notify();
    }

    /// Use the suggestion: set both values, move their sliders, save.
    fn apply_tune(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((Ok(s), _)) = self.tune_done.take() {
            let p = Arc::clone(&self.params);
            p.set_gate_threshold_db(s.gate_db);
            p.set_agc_max_db(s.agc_max_db);
            let (g, a) = (p.gate_threshold_db(), p.agc_max_db());
            self.gate.update(cx, |sl, cx| sl.set_value(g, window, cx));
            self.agc.update(cx, |sl, cx| sl.set_value(a, window, cx));
            self.tune_applied = true;
            save_soon(cx);
        }
        cx.notify();
    }

    /// "Tune for my room": a row like the chain stages, opening its steps below.
    fn tune_row(&self, st: &Status, cx: &mut Context<Self>) -> AnyElement {
        let open = self.tune_open;
        let hint = if self.tuning.is_some() {
            "Measuring…"
        } else {
            "Sets the noise gate and auto gain for your mic"
        };
        let head = div()
            .id("tune")
            .flex()
            .items_center()
            .gap(SP_8)
            .cursor_pointer()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .child("Tune for my room")
                    .child(caption(hint)),
            )
            .child(
                svg()
                    .path(if open {
                        assets::CHEVRON_DOWN
                    } else {
                        assets::CHEVRON_RIGHT
                    })
                    .size(px(14.0))
                    .text_color(c(TEXT_SECONDARY)),
            )
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.tune_open = !open;
                if open {
                    // Closing stops a measurement and forgets an unapplied result.
                    this.tuning = None;
                    this.tune_done = None;
                    this.tune_applied = false;
                }
                cx.notify();
            }));
        div()
            .flex()
            .flex_col()
            .py(SP_12)
            .child(head)
            .when(open, |d| d.child(self.tune_body(st, cx)))
            .into_any_element()
    }

    /// The steps: start → stay quiet → talk → result (Apply / Try again).
    fn tune_body(&self, st: &Status, cx: &mut Context<Self>) -> AnyElement {
        let body = div().flex().flex_col().gap(SP_12).pt(SP_12);
        let start_label = if self.tune_done.is_some() || self.tune_applied {
            "Try again"
        } else {
            "Start"
        };
        let start = button("tune-start", start_label, self.tune_done.is_none())
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.start_tune(cx)));
        let running = st.state == State::Running;
        if let Some((t, _)) = &self.tuning {
            let (title, hint) = match t.phase() {
                Phase::Quiet => (
                    "Stay quiet…",
                    "Leave your usual sounds on: fan, game, typing.",
                ),
                _ => (
                    "Now talk normally…",
                    "Say a few sentences in your usual voice.",
                ),
            };
            return body
                .child(div().flex().flex_col().child(title).child(caption(hint)))
                .child(progress_bar(t.progress()))
                .child(
                    div()
                        .flex()
                        .child(button("tune-cancel", "Cancel", false).on_click(cx.listener(
                            |this, _: &ClickEvent, _, cx| {
                                this.tuning = None;
                                cx.notify();
                            },
                        ))),
                )
                .into_any_element();
        }
        match &self.tune_done {
            Some((Ok(s), engine)) => {
                let p = &self.params;
                let mut notes: Vec<&'static str> = Vec::new();
                if s.clipping {
                    notes.push(
                        "Your mic clipped while you talked. Lower its input volume in \
                         Windows sound settings, then tune again.",
                    );
                }
                if s.too_quiet {
                    notes.push(
                        "Your voice is very quiet, even with the most auto gain. Move closer \
                         or raise the mic's input volume in Windows sound settings.",
                    );
                }
                if s.noisy {
                    notes.push(if *engine == Some(EngineKind::Dfn) {
                        "Your room is loud next to your voice, so the gate can't fully \
                         separate them."
                    } else {
                        "Your room is loud next to your voice, so the gate can't fully \
                         separate them. High quality removes more noise; tune again with it."
                    });
                }
                body.child(result_line(
                    "Noise gate",
                    db(p.gate_threshold_db()),
                    db(s.gate_db),
                ))
                .child(result_line(
                    "Auto gain",
                    format!("Up to {}", db_signed(p.agc_max_db())),
                    format!("Up to {}", db_signed(s.agc_max_db)),
                ))
                .child(caption(format!(
                    "Measured with {}: room {}, voice {}.",
                    engine.map_or("Off", engine_short),
                    db(s.noise_db),
                    db(s.voice_db)
                )))
                .children(notes.into_iter().map(note))
                .child(
                    div()
                        .flex()
                        .gap(SP_8)
                        .child(button("tune-apply", "Apply", true).on_click(cx.listener(
                            |this, _: &ClickEvent, window, cx| this.apply_tune(window, cx),
                        )))
                        .child(start),
                )
                .into_any_element()
            }
            Some((Err(f), _)) => body
                .child(note(f.message()))
                .child(div().flex().child(start))
                .into_any_element(),
            None if self.tune_applied => body
                .child(caption(
                    "Applied. You can still fine-tune the noise gate and auto gain below.",
                ))
                .child(div().flex().child(start))
                .into_any_element(),
            None => body
                .child(caption(if running {
                    "Takes 10 seconds: 5 quiet, then 5 talking. Nothing changes until you \
                     apply it."
                } else {
                    "Start Sotto with your microphone first."
                }))
                .when(running, |d| d.child(div().flex().child(start)))
                .into_any_element(),
        }
    }

    /// "Hear yourself": record 5 s of the cleaned voice, then play it back once. Never
    /// live, so your voice is never doubled in a screen share or recording.
    fn hear_row(&self, st: &Status, cx: &mut Context<Self>) -> AnyElement {
        let running = st.state == State::Running;
        let muted = self.params.muted.load(Relaxed);
        let click = |cmd: fn() -> Command| {
            cx.listener(move |this: &mut Self, _: &ClickEvent, _, cx| {
                this.hear_used = true;
                send(cmd(), cx);
                cx.notify();
            })
        };
        let (hint, controls): (SharedString, Div) = match st.hear {
            Hear::Recording => (
                "Recording… talk normally.".into(),
                div()
                    .flex()
                    .items_center()
                    .gap(SP_12)
                    .child(div().flex_1().child(progress_bar(st.hear_progress)))
                    .child(button("hear-stop", "Cancel", false).on_click(click(|| Command::StopHear))),
            ),
            Hear::Playing => (
                "Playing in your headphones…".into(),
                div()
                    .flex()
                    .child(button("hear-stop", "Stop", false).on_click(click(|| Command::StopHear))),
            ),
            Hear::Ready => (
                "Plays once in your headphones. If you're sharing your screen, viewers hear it too."
                    .into(),
                div()
                    .flex()
                    .gap(SP_8)
                    .child(button("hear-play", "Play", true).on_click(click(|| Command::Play)))
                    .when(running && !muted, |d| {
                        d.child(
                            button("hear-record", "Record again", false)
                                .on_click(click(|| Command::Record)),
                        )
                    }),
            ),
            Hear::Idle => (
                if !running {
                    "Start Sotto with your microphone first.".into()
                } else if muted {
                    "Unmute to record.".into()
                } else {
                    "Record 5 seconds of what others hear, then play it back once.".into()
                },
                div().flex().when(running && !muted, |d| {
                    d.child(button("hear-record", "Record", true).on_click(click(|| Command::Record)))
                }),
            ),
        };
        div()
            .flex()
            .flex_col()
            .gap(SP_8)
            .pt(SP_12)
            .border_t_1()
            .border_color(c(BORDER))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .child("Hear yourself")
                    .child(caption(hint)),
            )
            .when_some(st.hear_error.clone(), |d, e| d.child(note(e)))
            .child(controls)
            .into_any_element()
    }

    /// Bring the pickers in line with the control thread's lists and choices.
    fn sync_pickers(&mut self, st: &Status, window: &mut Window, cx: &mut Context<Self>) {
        let pairs = [
            (&self.input_select, &st.inputs, &st.input),
            (&self.output_select, &st.outputs, &st.output),
        ];
        for (i, (sel, list, chosen)) in pairs.into_iter().enumerate() {
            let chosen_id = chosen.as_ref().map(|d| d.id.clone());
            let shown = &mut self.shown[i];
            if shown.0 == *list && shown.1 == chosen_id {
                continue;
            }
            *shown = (list.clone(), chosen_id.clone());
            let items = list.clone();
            sel.update(cx, |s, cx| {
                s.set_items(items, window, cx);
                match &chosen_id {
                    Some(id) => s.set_selected_value(id, window, cx),
                    None => s.set_selected_index(None::<IndexPath>, window, cx),
                }
            });
        }
    }

    /// Noise removal card: engine segments, what the engine is for, strength, and the
    /// post-filter switch while High quality is selected.
    fn engine_card(&self, st: &Status, cx: &mut Context<Self>) -> impl IntoElement {
        let current = st.loading.or(st.engine);
        let segments = [
            (EngineKind::Off, "Off"),
            (EngineKind::Rnnoise, "Light"),
            (EngineKind::Dfn, "High quality"),
        ];
        let hint = match (st.loading, current) {
            (Some(EngineKind::Dfn), _) => "Loading DeepFilterNet…",
            (Some(_), _) => "Switching…",
            (None, Some(EngineKind::Off)) => "Off — passthrough, to hear the difference",
            (None, Some(k)) => k.label(),
            (None, None) => "",
        };
        let pf_on = self.params.dfn_postfilter.load(Relaxed);
        let loading = st.loading.is_some();
        card("Noise removal")
            .child(segmented(
                segments.into_iter().map(|(kind, title)| {
                    (
                        SharedString::from(kind.name()),
                        title,
                        current == Some(kind),
                        Box::new(move |cx: &mut App| {
                            send(Command::SetEngine(kind), cx);
                            save_soon(cx);
                        }) as Box<dyn Fn(&mut App)>,
                    )
                }),
                LABEL,
                32.0,
            ))
            .child(div().mb(SP_8).child(caption(hint)))
            .child(self.strength_row(st, cx))
            .when(st.engine == Some(EngineKind::Dfn), |d| {
                d.child(
                    div()
                        .mt(SP_8)
                        .pt(SP_12)
                        .border_t_1()
                        .border_color(c(BORDER))
                        .flex()
                        .items_center()
                        .gap(SP_12)
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .child("Post-filter")
                                .child(caption("Removes more noise. Can sound a little thinner.")),
                        )
                        .child(
                            div()
                                .flex_none()
                                .when(!loading, |d| d.cursor_pointer())
                                .child(
                                    Switch::new("postfilter")
                                        .checked(pf_on)
                                        .disabled(loading)
                                        .on_click(|on: &bool, _, cx| {
                                            send(Command::SetPostFilter(*on), cx);
                                            save_soon(cx);
                                        }),
                                ),
                        ),
                )
            })
    }

    /// The value as plain text for typing (a normal minus, no unit).
    fn edit_text(&self, f: Field) -> String {
        let p = &self.params;
        let v = match f {
            Field::Strength => p.strength() * 100.0,
            Field::Gate => p.gate_threshold_db(),
            Field::Agc => p.agc_max_db(),
            Field::Eq(i) => p.eq_gains()[i],
        };
        if v.fract().abs() > 0.01 {
            format!("{v:.1}")
        } else {
            format!("{v:.0}")
        }
    }

    fn start_edit(&mut self, f: Field, window: &mut Window, cx: &mut Context<Self>) {
        // Clicking another value while typing applies the first one.
        self.commit_edit(window, cx);
        self.editing = Some(f);
        let text = self.edit_text(f);
        self.edit.update(cx, |s, cx| s.set_value(text, window, cx));
        // Once the box has been drawn: focus it again (if this click also activated the
        // window, GPUI applies that activation afterwards and the first focus is lost),
        // then select the text so typing replaces it.
        let edit = self.edit.clone();
        window.on_next_frame(move |window, _| {
            window.on_next_frame(move |window, cx| {
                edit.update(cx, |s, cx| s.focus(window, cx));
                window.dispatch_action(Box::new(SelectAll), cx);
            });
        });
        cx.notify();
    }

    fn cancel_edit(&mut self, cx: &mut Context<Self>) {
        self.editing = None;
        cx.notify();
    }

    /// Apply the typed value (rounded to the slider's step, clamped by the same setters
    /// the sliders use), move the slider to match, and save.
    fn commit_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(f) = self.editing.take() else {
            return;
        };
        let text = self.edit.read(cx).value();
        if let Some(v) = parse_number(&text) {
            let v = (v / f.step()).round() * f.step();
            let p = Arc::clone(&self.params);
            let (slider, shown) = match f {
                Field::Strength => {
                    p.set_strength(v / 100.0);
                    (&self.strength, p.strength() * 100.0)
                }
                Field::Gate => {
                    p.set_gate_threshold_db(v);
                    (&self.gate, p.gate_threshold_db())
                }
                Field::Agc => {
                    p.set_agc_max_db(v);
                    (&self.agc, p.agc_max_db())
                }
                Field::Eq(i) => {
                    let mut gains = cx.global::<AppState>().settings.eq_custom;
                    gains[i] = v;
                    p.set_eq_custom(gains);
                    cx.global_mut::<AppState>().settings.eq_custom = p.eq_gains();
                    (&self.eq[i], p.eq_gains()[i])
                }
            };
            slider.update(cx, |s, cx| s.set_value(shown, window, cx));
            save_soon(cx);
        }
        cx.notify();
    }

    /// The value beside a slider. Click it to type a number instead of dragging.
    fn value_cell(
        &self,
        f: Field,
        text: String,
        enabled: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if self.editing == Some(f) {
            return div()
                .w(px(VALUE_W))
                .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _, cx| {
                    if ev.keystroke.key == "escape" {
                        this.cancel_edit(cx);
                    }
                }))
                .on_mouse_down_out(cx.listener(|this, _, window, cx| this.commit_edit(window, cx)))
                .child(Input::new(&self.edit).small())
                .into_any_element();
        }
        div()
            .id(f.id())
            .w(px(VALUE_W))
            .flex()
            .justify_end()
            .text_color(c(if enabled {
                TEXT_PRIMARY
            } else {
                TEXT_SECONDARY
            }))
            .child(
                numeric(div())
                    .px(SP_4)
                    .rounded(RADIUS_CONTROL)
                    .when(enabled, |d| d.hover(|d| d.bg(c(ELEVATED))))
                    .child(text),
            )
            .when(enabled, |d| {
                d.cursor_pointer()
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.start_edit(f, window, cx)
                    }))
            })
            .into_any_element()
    }

    fn strength_row(&self, st: &Status, cx: &mut Context<Self>) -> impl IntoElement {
        let off = st.engine == Some(EngineKind::Off);
        let value = (self.params.strength() * 100.0).round();
        div()
            .flex()
            .items_center()
            .gap(SP_12)
            .child(div().w(px(64.0)).child("Strength"))
            .child(slider_box(&self.strength, !off))
            .child(self.value_cell(Field::Strength, format!("{value:.0}%"), !off, cx))
    }

    /// One device row: icon, small label, and a borderless dropdown (its own chevron).
    fn device_row(
        &self,
        icon: &'static str,
        label: &'static str,
        sel: &DeviceSelect,
        placeholder: &'static str,
        empty: bool,
    ) -> Div {
        div()
            .flex()
            .items_center()
            .gap(SP_12)
            .py(SP_8)
            .child(
                svg()
                    .path(icon)
                    .size(px(16.0))
                    .flex_none()
                    .text_color(c(TEXT_SECONDARY)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(caption(label))
                    .child(
                        // gpui-component's controls don't ask for a hand cursor, so
                        // each sits in a box that does (only while it can be used).
                        div().when(!empty, |d| d.cursor_pointer()).child(
                            Select::new(sel)
                                .placeholder(placeholder)
                                .disabled(empty)
                                .appearance(false)
                                // Line the name up with its label above.
                                .px_0(),
                        ),
                    ),
            )
    }

    /// One voice-chain row: label + current value (click to open fine settings, for
    /// stages that have them), and its on/off switch.
    fn stage_row(&self, stage: Stage, cx: &mut Context<Self>) -> AnyElement {
        let p = &self.params;
        let on = stage.flag(p).load(Relaxed);
        let open = self.expanded == Some(stage);
        let params = Arc::clone(p);
        let head = div()
            .id(stage.id())
            .flex_1()
            .min_w_0()
            .flex()
            .items_center()
            .gap(SP_8)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .child(stage.label())
                    .child(caption(stage.hint(p))),
            )
            .when(stage.tunable(), |d| {
                d.cursor_pointer()
                    .child(
                        svg()
                            .path(if open {
                                assets::CHEVRON_DOWN
                            } else {
                                assets::CHEVRON_RIGHT
                            })
                            .size(px(14.0))
                            .text_color(c(TEXT_SECONDARY)),
                    )
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.expanded = if open { None } else { Some(stage) };
                        cx.notify();
                    }))
            });
        div()
            .flex()
            .flex_col()
            .py(SP_12)
            .child(div().flex().items_center().gap(SP_12).child(head).child(
                div().flex_none().cursor_pointer().child(
                    Switch::new(stage.id()).checked(on).on_click(
                        move |checked: &bool, window, cx| {
                            // Read by the audio thread on its next frame; no lock.
                            stage.flag(&params).store(*checked, Relaxed);
                            save_soon(cx);
                            window.refresh();
                        },
                    ),
                ),
            ))
            .when(open, |d| d.child(self.stage_settings(stage, cx)))
            .into_any_element()
    }

    /// Mute switch. Same action as the tray item and the shortcut.
    fn mute_row(&self) -> AnyElement {
        let muted = self.params.muted.load(Relaxed);
        div()
            .flex()
            .items_center()
            .gap(SP_12)
            .py(SP_12)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child("Mute")
                    .child(caption(if muted {
                        "Nothing is sent to CABLE Input"
                    } else {
                        "Silences your voice everywhere at once"
                    })),
            )
            .child(
                div().flex_none().cursor_pointer().child(
                    Switch::new("mute")
                        .checked(muted)
                        .on_click(|checked: &bool, _, cx| {
                            let now = cx.global::<AppState>().audio.params.muted.load(Relaxed);
                            if *checked != now {
                                toggle_mute(cx);
                            }
                        }),
                ),
            )
            .into_any_element()
    }

    /// The global mute shortcut. Click the box, then press the new combination.
    fn shortcut_row(&self, cx: &mut Context<Self>) -> AnyElement {
        let st = cx.global::<AppState>();
        let current = st
            .hotkeys
            .as_ref()
            .and_then(|h| h.current())
            .map(hotkey::label);
        let hint: SharedString = if let Some(e) = self.rec_error.clone().or(st.hotkey_error.clone())
        {
            e.into()
        } else if self.recording {
            "Esc cancels · Backspace removes the shortcut".into()
        } else {
            "Works in games too".into()
        };
        let text: SharedString = if self.recording {
            "Press keys…".into()
        } else {
            current.unwrap_or_else(|| "None".into()).into()
        };
        let rec = self.recording;
        div()
            .flex()
            .items_center()
            .gap(SP_12)
            .py(SP_12)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child("Mute shortcut")
                    .child(caption(hint)),
            )
            .child(
                div()
                    .id("shortcut")
                    .track_focus(&self.rec_focus)
                    .flex_none()
                    .min_w(px(96.0))
                    .flex()
                    .justify_center()
                    .px(SP_12)
                    .py(SP_4)
                    .rounded(RADIUS_CONTROL)
                    .border_1()
                    .border_color(c(if rec { ACCENT } else { BORDER }))
                    .bg(c(if rec { ELEVATED } else { BG }))
                    .text_size(px(LABEL))
                    .text_color(c(if rec { TEXT_SECONDARY } else { TEXT_PRIMARY }))
                    .cursor_pointer()
                    .hover(|d| d.bg(c(ELEVATED)))
                    .child(text)
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        if !this.recording {
                            this.recording = true;
                            this.rec_error = None;
                            if let Some(h) = cx.global_mut::<AppState>().hotkeys.as_mut() {
                                h.pause();
                            }
                            this.rec_focus.focus(window);
                            cx.notify();
                        }
                    }))
                    .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _, cx| {
                        if this.recording {
                            cx.stop_propagation();
                            this.record_key(ev, cx);
                        }
                    }))
                    .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                        if this.recording {
                            this.stop_recording(cx);
                        }
                    })),
            )
            .into_any_element()
    }

    fn stop_recording(&mut self, cx: &mut Context<Self>) {
        self.recording = false;
        if let Some(h) = cx.global_mut::<AppState>().hotkeys.as_mut() {
            h.resume();
        }
        cx.notify();
    }

    /// One key press while recording a shortcut.
    fn record_key(&mut self, ev: &KeyDownEvent, cx: &mut Context<Self>) {
        let k = &ev.keystroke;
        let m = k.modifiers;
        let any_mod = m.control || m.alt || m.shift || m.platform;
        // Modifiers alone: wait for the actual key.
        if matches!(
            k.key.as_str(),
            "control" | "ctrl" | "alt" | "shift" | "platform" | "win"
        ) {
            return;
        }
        let chosen: Option<String> = match k.key.as_str() {
            "escape" if !any_mod => return self.stop_recording(cx),
            "backspace" | "delete" if !any_mod => None,
            key => match hotkey::from_press(m.control, m.alt, m.shift, m.platform, key) {
                Ok(t) => Some(t),
                Err(why) => {
                    self.rec_error = Some(why.into());
                    cx.notify();
                    return;
                }
            },
        };
        let st = cx.global_mut::<AppState>();
        let Some(h) = st.hotkeys.as_mut() else {
            return self.stop_recording(cx);
        };
        match h.set(chosen.as_deref()) {
            Ok(()) => {
                st.hotkey_error = None;
                st.settings.mute_hotkey = chosen.unwrap_or_default();
                self.rec_error = None;
                self.recording = false;
                save_soon(cx);
                cx.notify();
            }
            Err(e) => {
                self.rec_error = Some(e);
                cx.notify();
            }
        }
    }

    /// The About page: what Sotto is, its version and license, and every third-party
    /// license. Licenses open one at a time under their row.
    fn about_page(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let open = self.open_license.as_ref().map(|(i, _)| *i);
        let gpl = License {
            name: "GNU General Public License v3.0 only",
            used_by: "Sotto",
            text: licenses::GPL,
        };
        let mut own: Vec<AnyElement> = std::iter::once(&gpl)
            .chain(licenses::ALL.iter())
            .enumerate()
            .map(|(i, l)| self.license_row(i, l, open == Some(i), cx))
            .collect();
        let third = own.split_off(1);
        column()
            .child(
                div()
                    .id("back")
                    .flex()
                    .items_center()
                    .gap(SP_4)
                    .text_color(c(TEXT_SECONDARY))
                    .cursor_pointer()
                    .hover(|d| d.text_color(c(TEXT_PRIMARY)))
                    .child(
                        svg()
                            .path(assets::CHEVRON_LEFT)
                            .size(px(16.0))
                            .text_color(c(TEXT_SECONDARY)),
                    )
                    .child("Back")
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.about = false;
                        this.open_license = None;
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(SP_12)
                    .child(
                        svg()
                            .path(assets::MARK)
                            .size(px(28.0))
                            .flex_none()
                            .text_color(c(ACCENT)),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .text_size(px(HEADING))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("Sotto"),
                            )
                            .child(caption(concat!("Version ", env!("CARGO_PKG_VERSION")))),
                    ),
            )
            .child(
                card("About")
                    .child(
                        "Sotto cleans your microphone in real time and sends it to VB-Cable. \
                         It never touches your headphones, speakers or Windows sound settings.",
                    )
                    .child(caption(
                        "Free software: you can use, study, share and change it under the \
                         GNU General Public License v3.0. No telemetry; the only network use \
                         is checking for updates.",
                    ))
                    .child(
                        div()
                            .id("repo")
                            .text_color(c(ACCENT))
                            .cursor_pointer()
                            .hover(|d| d.text_color(c(ACCENT_HOVER)))
                            .child("github.com/hamadsh2/Sotto")
                            .on_click(|_: &ClickEvent, _, cx| cx.open_url(REPO_URL)),
                    ),
            )
            .child(list_card(own))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(SP_4)
                    .child(
                        div()
                            .text_size(px(LABEL))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(c(TEXT_SECONDARY))
                            .child("Third-party licenses"),
                    )
                    .child(caption(
                        "Sotto is built with these open-source projects. Thank you.",
                    )),
            )
            .child(list_card(third))
    }

    /// One license: its name and who uses it; click to read it below.
    fn license_row(&self, i: usize, l: &License, open: bool, cx: &mut Context<Self>) -> AnyElement {
        let text = self
            .open_license
            .as_ref()
            .filter(|_| open)
            .map(|(_, t)| t.clone());
        div()
            .flex()
            .flex_col()
            .child(
                div()
                    .id(("license", i))
                    .flex()
                    .items_center()
                    .gap(SP_12)
                    .py(SP_12)
                    .cursor_pointer()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(l.name)
                            .child(caption(l.used_by)),
                    )
                    .child(
                        svg()
                            .path(if open {
                                assets::CHEVRON_DOWN
                            } else {
                                assets::CHEVRON_RIGHT
                            })
                            .size(px(16.0))
                            .flex_none()
                            .text_color(c(TEXT_SECONDARY)),
                    )
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        let was_open = this.open_license.as_ref().is_some_and(|(o, _)| *o == i);
                        this.open_license = if was_open {
                            None
                        } else {
                            let text = if i == 0 {
                                licenses::GPL
                            } else {
                                licenses::ALL.get(i - 1).map_or("", |l| l.text)
                            };
                            Some((i, licenses::reflow(text).into()))
                        };
                        cx.notify();
                    })),
            )
            .when_some(text, |d, text| {
                d.child(
                    div()
                        .pb(SP_12)
                        .text_size(px(CAPTION))
                        .text_color(c(TEXT_SECONDARY))
                        .child(text),
                )
            })
            .into_any_element()
    }

    /// "Start with Windows": one value in the current user's Run key (see `win.rs`).
    fn startup_row(&self, cx: &mut Context<Self>) -> AnyElement {
        let on = cx.global::<AppState>().autostart;
        let hint = self
            .autostart_error
            .clone()
            .unwrap_or_else(|| "Opens quietly in the tray when you sign in".into());
        div()
            .flex()
            .items_center()
            .gap(SP_12)
            .py(SP_12)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child("Start with Windows")
                    .child(caption(hint)),
            )
            .child(div().flex_none().cursor_pointer().child(
                Switch::new("autostart").checked(on).on_click(cx.listener(
                    |this, checked: &bool, _, cx| {
                        match super::win::set_autostart(*checked) {
                            Ok(()) => {
                                this.autostart_error = None;
                                let st = cx.global_mut::<AppState>();
                                st.autostart = *checked;
                                st.settings.start_with_windows = *checked;
                                save_soon(cx);
                            }
                            Err(e) => this.autostart_error = Some(e),
                        }
                        cx.notify();
                    },
                )),
            ))
            .into_any_element()
    }

    /// "Check for updates automatically" (at start and once a day).
    fn auto_update_row(&self, cx: &mut Context<Self>) -> AnyElement {
        let on = cx.global::<AppState>().settings.check_updates;
        div()
            .flex()
            .items_center()
            .gap(SP_12)
            .py(SP_12)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child("Check for updates")
                    .child(caption("Daily. Installing always asks you first.")),
            )
            .child(
                div().flex_none().cursor_pointer().child(
                    Switch::new("check-updates")
                        .checked(on)
                        .on_click(|checked: &bool, _, cx| {
                            cx.global_mut::<AppState>().settings.check_updates = *checked;
                            save_soon(cx);
                            cx.refresh_windows();
                        }),
                ),
            )
            .into_any_element()
    }

    /// Where the update stands, with the one button that fits: Check, Install, Restart.
    fn update_row(&self, cx: &mut Context<Self>) -> AnyElement {
        let state = cx.global::<AppState>().update.clone();
        let current = super::update::current_version();
        let (title, hint, button): (SharedString, SharedString, UpdateButton) = match &state {
            UpdateState::Idle => (
                format!("Sotto {current}").into(),
                "Not checked yet".into(),
                Some(("Check now", check_updates)),
            ),
            UpdateState::Checking => (
                format!("Sotto {current}").into(),
                "Checking for updates…".into(),
                None,
            ),
            UpdateState::UpToDate => (
                format!("Sotto {current}").into(),
                "You have the latest version".into(),
                Some(("Check now", check_updates)),
            ),
            UpdateState::Available(v) => (
                format!("Sotto {v} is available").into(),
                format!("You have {current} · signature checked first").into(),
                Some(("Install", install_update)),
            ),
            UpdateState::Installing(v) => (
                format!("Installing Sotto {v}…").into(),
                "Downloading and checking the signature".into(),
                None,
            ),
            UpdateState::Ready(v) => (
                format!("Sotto {v} is installed").into(),
                "Restart Sotto to start using it".into(),
                Some(("Restart", restart)),
            ),
            UpdateState::Failed(why) => (
                format!("Sotto {current}").into(),
                why.clone().into(),
                Some(("Try again", check_updates)),
            ),
        };
        div()
            .flex()
            .items_center()
            .gap(SP_12)
            .py(SP_12)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(title)
                    .child(caption(hint)),
            )
            .when_some(button, |d, (label, action)| {
                d.child(
                    div()
                        .id("update-action")
                        .flex_none()
                        .px(SP_12)
                        .py(SP_4)
                        .rounded(RADIUS_CONTROL)
                        .border_1()
                        .border_color(c(BORDER))
                        .text_size(px(LABEL))
                        .cursor_pointer()
                        .hover(|d| d.bg(c(ELEVATED)))
                        .child(label)
                        .on_click(move |_: &ClickEvent, _, cx| action(cx)),
                )
            })
            .into_any_element()
    }

    /// "About Sotto": opens the About page.
    fn about_row(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .id("open-about")
            .flex()
            .items_center()
            .gap(SP_12)
            .py(SP_12)
            .cursor_pointer()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child("About Sotto")
                    .child(caption(concat!(
                        "Version ",
                        env!("CARGO_PKG_VERSION"),
                        " \u{00B7} licenses"
                    ))),
            )
            .child(
                svg()
                    .path(assets::CHEVRON_RIGHT)
                    .size(px(16.0))
                    .flex_none()
                    .text_color(c(TEXT_SECONDARY)),
            )
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                this.about = true;
                cx.notify();
            }))
            .into_any_element()
    }

    /// The fine settings under an open row.
    fn stage_settings(&self, stage: Stage, cx: &mut Context<Self>) -> AnyElement {
        let p = &self.params;
        let body = div().flex().flex_col().gap(SP_12).pt(SP_12);
        match stage {
            Stage::Gate => body
                .child(value_slider(
                    &self.gate,
                    self.value_cell(Field::Gate, db(p.gate_threshold_db()), true, cx),
                ))
                .child(caption(
                    "Higher cuts more between words; lower lets quiet speech through. \
                     The mark on the Mic meter shows where it opens.",
                )),
            Stage::Agc => body
                .child(value_slider(
                    &self.agc,
                    self.value_cell(Field::Agc, db_signed(p.agc_max_db()), true, cx),
                ))
                .child(caption("The most a quiet voice may be raised.")),
            Stage::Eq => {
                let preset = p.eq_preset();
                let mut presets: Vec<EqPreset> = EqPreset::NAMED.to_vec();
                presets.push(EqPreset::Custom);
                body.child(segmented(
                    presets.into_iter().map(|pr| {
                        let params = Arc::clone(p);
                        (
                            SharedString::from(format!("eq-{}", pr.name())),
                            pr.label(),
                            pr == preset,
                            Box::new(move |cx: &mut App| {
                                if pr == EqPreset::Custom {
                                    let gains = cx.global::<AppState>().settings.eq_custom;
                                    params.set_eq_custom(gains);
                                } else {
                                    params.set_eq_preset(pr);
                                }
                                save_soon(cx);
                                cx.refresh_windows();
                            }) as Box<dyn Fn(&mut App)>,
                        )
                    }),
                    CAPTION,
                    28.0,
                ))
                .when(preset == EqPreset::Custom, |d| {
                    let gains = p.eq_gains();
                    d.children((0..3).map(|i| {
                        div()
                            .flex()
                            .items_center()
                            .gap(SP_12)
                            .child(
                                div()
                                    .w(px(64.0))
                                    .text_size(px(LABEL))
                                    .text_color(c(TEXT_SECONDARY))
                                    .child(EQ_BANDS[i]),
                            )
                            .child(slider_box(&self.eq[i], true))
                            .child(self.value_cell(Field::Eq(i), db_signed(gains[i]), true, cx))
                    }))
                })
                .child(caption(
                    "Natural leaves your voice as it is. Custom: ±6 dB per band.",
                ))
            }
            Stage::Hpf | Stage::Limiter => body,
        }
        .into_any_element()
    }
}

/// A slider in a box that shows the hand cursor while it can be used.
fn slider_box(s: &Entity<SliderState>, enabled: bool) -> impl IntoElement {
    div()
        .flex_1()
        .when(enabled, |d| d.cursor_pointer())
        .child(Slider::new(s).disabled(!enabled))
}

/// Width of the value column beside sliders (fits "−80 dB" and the text box).
const VALUE_W: f32 = 64.0;

fn value_slider(s: &Entity<SliderState>, value: AnyElement) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap(SP_12)
        .child(slider_box(s, true))
        .child(value)
}

/// A segmented control: equal segments on a `bg` track, the selected one `accent`.
fn segmented(
    items: impl Iterator<Item = (SharedString, &'static str, bool, Box<dyn Fn(&mut App)>)>,
    text: f32,
    height: f32,
) -> impl IntoElement {
    div()
        .flex()
        .gap(SP_4)
        .p(SP_4)
        .bg(c(BG))
        .rounded(RADIUS_CONTROL)
        .children(items.map(move |(id, title, selected, on_pick)| {
            div()
                .id(id)
                .flex_1()
                .h(px(height))
                .flex()
                .items_center()
                .justify_center()
                .rounded(RADIUS_CONTROL)
                .text_size(px(text))
                .font_weight(FontWeight::MEDIUM)
                .cursor_pointer()
                .map(|d| {
                    if selected {
                        d.bg(c(ACCENT)).text_color(c(BG))
                    } else {
                        d.text_color(c(TEXT_SECONDARY))
                            .hover(|s| s.bg(c(ELEVATED)).text_color(c(TEXT_PRIMARY)))
                    }
                })
                .on_click(move |_: &ClickEvent, _, cx| on_pick(cx))
                .child(title)
        }))
}

/// A panel on `surface` with a small label on top.
/// The update row's button: its label and what it does.
type UpdateButton = Option<(&'static str, fn(&mut App))>;

const REPO_URL: &str = "https://github.com/hamadsh2/Sotto";

/// The scrolling window background with the app font and text defaults. Each page has
/// its own `id`, so each keeps its own scroll position.
fn page(id: &'static str) -> gpui::Stateful<Div> {
    div()
        .id(id)
        .size_full()
        .overflow_y_scroll()
        .bg(c(BG))
        .font_family(FONT)
        .text_color(c(TEXT_PRIMARY))
        .text_size(px(BODY))
}

/// Tabular numerals for a value that changes while you watch (slider values, CPU,
/// latency), so it doesn't jitter in width. Only there: Inter's tabular set also widens
/// the hyphen and period, which looks odd in ordinary text.
fn numeric<E: Styled>(mut e: E) -> E {
    e.text_style()
        .get_or_insert_with(Default::default)
        .font_features = Some(tabular_numbers());
    e
}

/// The content column. It sits inside the scroll area with its own height, so cards
/// never get squeezed together when the window is short, and it stays a readable width
/// when the window is wide.
fn column() -> Div {
    div()
        .w_full()
        .max_w(px(MAX_CONTENT_W))
        .mx_auto()
        .flex()
        .flex_col()
        .gap(SP_16)
        .p(SP_24)
}

fn card(title: &'static str) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(SP_8)
        .p(SP_16)
        .bg(c(SURFACE))
        .rounded(RADIUS_PANEL)
        .child(
            div()
                .text_size(px(LABEL))
                .font_weight(FontWeight::MEDIUM)
                .text_color(c(TEXT_SECONDARY))
                .child(title),
        )
}

/// A panel of rows separated by hairlines (no label).
fn list_card(rows: Vec<AnyElement>) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .px(SP_16)
        .py(SP_4)
        .bg(c(SURFACE))
        .rounded(RADIUS_PANEL)
        .children(rows.into_iter().enumerate().map(|(i, row)| {
            div()
                .when(i > 0, |d| d.border_t_1().border_color(c(BORDER)))
                .child(row)
        }))
}

fn caption(text: impl Into<SharedString>) -> impl IntoElement {
    div()
        .text_size(px(CAPTION))
        .text_color(c(TEXT_SECONDARY))
        .child(text.into())
}

/// One level meter: `accent` body, `meter_peak` only in the top 10% and for the
/// peak-hold tick, on an `elevated` track. `mark` is an optional thin `text_secondary`
/// line (the gate threshold on the Mic meter).
/// `dim`: drawn in `text_secondary` (the Mic meter while muted: you can see you're
/// talking, but nothing is sent).
fn level_meter(
    label: &'static str,
    m: Meter,
    live: bool,
    mark: Option<f32>,
    dim: bool,
) -> impl IntoElement {
    const HOT: f32 = 0.9;
    let (body, hot) = if dim {
        (TEXT_SECONDARY, TEXT_SECONDARY)
    } else {
        (ACCENT, METER_PEAK)
    };
    let (level, peak) = if live { (m.level, m.peak) } else { (0.0, 0.0) };
    div()
        .flex()
        .items_center()
        .gap(SP_12)
        .child(
            div()
                .w(px(48.0))
                .text_size(px(LABEL))
                .text_color(c(TEXT_SECONDARY))
                .child(label),
        )
        .child(
            div()
                .flex_1()
                .relative()
                .h(px(8.0))
                .rounded(RADIUS_CONTROL)
                .bg(c(ELEVATED))
                .overflow_hidden()
                .child(
                    div()
                        .size_full()
                        .flex()
                        .child(div().h_full().w(relative(level.min(HOT))).bg(c(body)))
                        .child(
                            div()
                                .h_full()
                                .w(relative((level - HOT).max(0.0)))
                                .bg(c(hot)),
                        ),
                )
                .when_some(mark, |d, at| {
                    d.child(
                        div()
                            .absolute()
                            .top_0()
                            .h_full()
                            .w(px(1.0))
                            .left(relative(at))
                            .bg(c(TEXT_SECONDARY)),
                    )
                })
                .when(peak > 0.01, |d| {
                    d.child(
                        div()
                            .absolute()
                            .top_0()
                            .h_full()
                            .w(px(2.0))
                            .left(relative((peak - 0.01).max(0.0)))
                            .bg(c(hot)),
                    )
                }),
        )
}

/// A small button: `primary` is `accent`-filled (one per group), the others outlined.
fn button(id: &'static str, label: &'static str, primary: bool) -> gpui::Stateful<Div> {
    div()
        .id(id)
        .flex_none()
        .px(SP_12)
        .py(SP_4)
        .rounded(RADIUS_CONTROL)
        .border_1()
        .text_size(px(LABEL))
        .cursor_pointer()
        .map(|d| {
            if primary {
                d.border_color(c(ACCENT))
                    .bg(c(ACCENT))
                    .text_color(c(BG))
                    .font_weight(FontWeight::MEDIUM)
                    .hover(|d| d.bg(c(ACCENT_HOVER)).border_color(c(ACCENT_HOVER)))
            } else {
                d.border_color(c(BORDER)).hover(|d| d.bg(c(ELEVATED)))
            }
        })
        .child(label)
}

/// A thin progress bar: `accent` on an `elevated` track.
fn progress_bar(done: f32) -> impl IntoElement {
    div()
        .h(px(4.0))
        .rounded(RADIUS_CONTROL)
        .bg(c(ELEVATED))
        .overflow_hidden()
        .child(
            div()
                .h_full()
                .w(relative(done.clamp(0.0, 1.0)))
                .bg(c(ACCENT)),
        )
}

/// "Noise gate   −45 dB → −52 dB": the current value quiet, the suggestion bright.
fn result_line(label: &'static str, now: String, new: String) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap(SP_8)
        .child(div().flex_1().child(label))
        .child(numeric(div()).text_color(c(TEXT_SECONDARY)).child(now))
        .child(div().text_color(c(TEXT_SECONDARY)).child("\u{2192}"))
        .child(numeric(div()).font_weight(FontWeight::MEDIUM).child(new))
}

/// A one-line advisory: caption text with a 2px `warning` rule.
fn note(text: impl Into<SharedString>) -> impl IntoElement {
    div()
        .pl(SP_8)
        .border_l_2()
        .border_color(c(WARNING))
        .text_size(px(CAPTION))
        .text_color(c(TEXT_PRIMARY))
        .child(text.into())
}

/// The engine as the segmented control names it.
fn engine_short(k: EngineKind) -> &'static str {
    match k {
        EngineKind::Off => "Off",
        EngineKind::Rnnoise => "Light",
        EngineKind::Dfn => "High quality",
    }
}

/// Calm warning card: 2px `warning` rule on the left, text in `text_primary`.
fn warning_card(what: impl Into<SharedString>, fix: impl Into<SharedString>) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(SP_4)
        .p(SP_12)
        .bg(c(SURFACE))
        .border_l_2()
        .border_color(c(WARNING))
        .text_size(px(LABEL))
        .child(div().text_color(c(TEXT_PRIMARY)).child(what.into()))
        .child(div().text_color(c(TEXT_SECONDARY)).child(fix.into()))
}

/// One numbered step inside a warning card.
fn step(n: u8, text: &'static str) -> impl IntoElement {
    div()
        .flex()
        .w_full()
        .gap(SP_8)
        .text_color(c(TEXT_SECONDARY))
        .child(div().flex_none().child(format!("{n}.")))
        .child(div().flex_1().min_w_0().child(text))
}

/// Header: the Sotto mark, wordmark and tagline, and a small status pill.
fn header(state: &State, muted: bool) -> impl IntoElement {
    let (dot, text) = match state {
        State::Running if muted => (TEXT_SECONDARY, "Muted"),
        State::Running => (ACCENT, "Running"),
        State::Starting => (TEXT_SECONDARY, "Starting…"),
        State::NeedsDevices => (TEXT_SECONDARY, "Choose a microphone"),
        State::Waiting(_) => (WARNING, "Waiting for device"),
        State::Stopped(_) => (WARNING, "Stopped"),
    };
    div()
        .flex()
        .items_center()
        .gap(SP_12)
        .child(
            svg()
                .path(assets::MARK)
                .size(px(28.0))
                .flex_none()
                .text_color(c(ACCENT)),
        )
        .child(
            div()
                .flex_1()
                .flex()
                .flex_col()
                .child(
                    div()
                        .text_size(px(HEADING))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child("Sotto"),
                )
                .child(caption("Just your voice.")),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap(SP_8)
                .px(SP_12)
                .py(SP_4)
                .bg(c(SURFACE))
                .rounded(RADIUS_CONTROL)
                .text_size(px(CAPTION))
                .text_color(c(TEXT_SECONDARY))
                .child(div().size(px(6.0)).rounded_full().bg(c(dot)))
                .child(text),
        )
}

impl Render for MainWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.about {
            return page("about").child(self.about_page(cx));
        }
        let st = self.status.lock().map(|s| s.clone()).unwrap_or_default();
        self.sync_pickers(&st, window, cx);
        let running = st.state == State::Running;
        let muted = self.params.muted.load(Relaxed);

        let mut warnings: Vec<AnyElement> = Vec::new();
        if st.cable_missing {
            warnings.push(
                warning_card(
                    "VB-Cable isn't installed, so Sotto has nowhere to send your voice.",
                    "It's free and takes a minute:",
                )
                .child(step(1, "On VB-Audio's page, download the \"New Package\" zip (VBCABLE_Driver_Pack46 or newer), not the old one."))
                .child(step(2, "Extract all the files from the zip."))
                .child(step(3, "Right-click VBCABLE_Setup_x64.exe, choose \"Run as administrator\", then click Install Driver."))
                .child(step(4, "Restart Windows, then open Sotto."))
                .child(
                    div()
                        .id("get-vb-cable")
                        .text_color(c(ACCENT))
                        .cursor_pointer()
                        .hover(|s| s.text_color(c(ACCENT_HOVER)))
                        .on_click(|_: &ClickEvent, _, cx| cx.open_url(VB_CABLE_URL))
                        .child("Open vb-audio.com/Cable"),
                )
                .into_any_element(),
            );
        }
        if st.cable_is_default {
            warnings.push(
                warning_card(
                    "CABLE Input is your Windows default playback device, so games and music go into the cable.",
                    "Fix it in Settings → System → Sound → Output: pick your headphones or speakers. Sotto won't change it for you.",
                )
                .into_any_element(),
            );
        }
        match &st.state {
            State::Waiting(why) => warnings.push(
                warning_card(
                    if why.is_empty() {
                        "A chosen device isn't available.".to_string()
                    } else {
                        why.clone()
                    },
                    "Sotto resumes on its own when it's back. It never switches to another device.",
                )
                .into_any_element(),
            ),
            State::Stopped(why) => {
                warnings.push(warning_card("Sotto stopped.", why.clone()).into_any_element())
            }
            _ => {}
        }
        if let Some(m) = &st.message {
            warnings.push(warning_card(m.clone(), "Nothing else was changed.").into_any_element());
        }
        if running && self.hints.too_loud {
            warnings.push(
                warning_card(
                    "Your microphone is too loud, so your voice distorts.",
                    "Lower its input volume in Settings → System → Sound → your microphone.",
                )
                .into_any_element(),
            );
        }
        if running && self.hints.too_quiet {
            warnings.push(
                warning_card(
                    "Your voice is very quiet, even with auto gain.",
                    "Move closer, or raise the input volume in Settings → System → Sound → \
                     your microphone. Tune for my room can help too.",
                )
                .into_any_element(),
            );
        }

        let latency = st
            .latency_ms
            .map_or("—".to_string(), |ms| format!("{ms:.0} ms"));
        // Where the gate opens, on the Mic meter (only if the gate is on and the
        // threshold is inside the meter's −60…0 dB scale).
        let gate_mark = (self.params.gate.load(Relaxed) && running)
            .then(|| meter::position(10f32.powf(self.params.gate_threshold_db() / 20.0)))
            .filter(|&x| x > 0.0);
        let chain: Vec<AnyElement> = std::iter::once(self.tune_row(&st, cx))
            .chain(Stage::ALL.into_iter().map(|s| self.stage_row(s, cx)))
            .collect();
        let devices = vec![
            self.device_row(
                assets::MIC,
                "Microphone",
                &self.input_select,
                "Choose your microphone",
                st.inputs.is_empty(),
            )
            .into_any_element(),
            self.device_row(
                assets::PLUG,
                "Output (VB-Cable)",
                &self.output_select,
                "Choose CABLE Input",
                st.outputs.is_empty(),
            )
            .into_any_element(),
        ];
        let readout = |label: &'static str, value: String| {
            div()
                .flex()
                .gap(SP_4)
                .child(label)
                .child(numeric(div()).text_color(c(TEXT_PRIMARY)).child(value))
        };

        page("main").child(
            column()
                .child(header(&st.state, self.params.muted.load(Relaxed)))
                .children(warnings)
                .child(self.engine_card(&st, cx))
                .child(
                    card("Levels")
                        .gap(SP_12)
                        .child(level_meter("Mic", self.meter_in, running, gate_mark, muted))
                        .child(level_meter("Output", self.meter_out, running, None, false))
                        .child(self.hear_row(&st, cx)),
                )
                .child(list_card(devices))
                .child(list_card(chain))
                .child(list_card(vec![self.mute_row(), self.shortcut_row(cx)]))
                .child(list_card(vec![
                    self.startup_row(cx),
                    self.auto_update_row(cx),
                    self.update_row(cx),
                    self.about_row(cx),
                ]))
                .child(
                    div()
                        .flex()
                        .gap(SP_8)
                        .text_size(px(CAPTION))
                        .text_color(c(TEXT_SECONDARY))
                        .child(readout("CPU", format!("{:.2}%", self.cpu_pct)))
                        .child("·")
                        .child(readout("Latency", latency))
                        .child("·")
                        .child(readout("Memory", format!("{:.0} MB", self.private_mb))),
                ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decibel_text_uses_a_real_minus_sign() {
        assert_eq!(db(-45.0), "\u{2212}45 dB");
        assert_eq!(db_signed(12.0), "+12 dB");
        assert_eq!(db_signed(-1.5), "\u{2212}1.5 dB");
        assert_eq!(db_signed(0.0), "0 dB");
        assert_eq!(db_signed(-0.0), "0 dB");
    }

    #[test]
    fn typed_numbers_are_read_leniently() {
        assert_eq!(parse_number("-45"), Some(-45.0));
        assert_eq!(parse_number("\u{2212}45 dB"), Some(-45.0));
        assert_eq!(parse_number(" +12 dB "), Some(12.0));
        assert_eq!(parse_number("80%"), Some(80.0));
        assert_eq!(parse_number("3.5"), Some(3.5));
        assert_eq!(parse_number("loud"), None);
        assert_eq!(parse_number(""), None);
        assert_eq!(parse_number("inf"), None);
    }
}
