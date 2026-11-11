// SPDX-License-Identifier: GPL-3.0-only
//! The main window: engine selector, strength, level meters, device pickers, the
//! voice-chain switches, warnings, and a CPU · latency readout.
//!
//! How the controls reach the audio (see `audio.rs`):
//! - strength and the stage switches write `Params` atomics directly — instant, no lock;
//! - engine and device choices are sent as commands to the control thread.
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
    App, AppContext, ClickEvent, Context, Entity, FontWeight, InteractiveElement, IntoElement,
    ParentElement, Render, SharedString, StatefulInteractiveElement, Styled, Subscription, Task,
    Window, div, px, relative, svg,
};
use gpui_component::IndexPath;
use gpui_component::select::{Select, SelectEvent, SelectItem, SelectState};
use gpui_component::slider::{Slider, SliderEvent, SliderState};
use gpui_component::switch::Switch;
use sotto::control::EngineKind;
use sotto::devices::{Dir, VB_CABLE_URL};
use sotto::params::Params;
use sotto::stats;

use super::AppState;
use super::assets;
use super::audio::{Command, Device, State, Status};
use super::meter::Meter;
use super::theme::*;

/// Meter frame interval while visible (~30 Hz).
const METER_EVERY: Duration = Duration::from_millis(33);
/// Wake-up interval while hidden: only the CPU/memory sampling runs.
const HIDDEN_EVERY: Duration = Duration::from_secs(1);
/// Content column width cap, so a wide (e.g. tiled) window keeps short lines.
const MAX_CONTENT_W: f32 = 520.0;

impl SelectItem for Device {
    type Value = String;

    fn title(&self) -> SharedString {
        self.name.clone().into()
    }

    fn value(&self) -> &String {
        &self.id
    }
}

type DeviceSelect = Entity<SelectState<Vec<Device>>>;

pub struct MainWindow {
    status: Arc<Mutex<Status>>,
    params: Arc<Params>,
    strength: Entity<SliderState>,
    input_select: DeviceSelect,
    output_select: DeviceSelect,
    /// What the pickers currently show, to update them only when something changed.
    shown: [(Vec<Device>, Option<String>); 2],
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

impl MainWindow {
    pub fn new(
        status: Arc<Mutex<Status>>,
        params: Arc<Params>,
        visible: Arc<AtomicBool>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let strength = cx.new(|_| {
            SliderState::new()
                .min(0.0)
                .max(100.0)
                .step(1.0)
                .default_value(params.strength() * 100.0)
        });
        let p = Arc::clone(&params);
        let mut subs = vec![cx.subscribe(&strength, move |_, _, ev: &SliderEvent, cx| {
            let SliderEvent::Change(v) = ev;
            p.set_strength(v.start() / 100.0);
            cx.notify();
        })];

        let input_select = cx.new(|cx| SelectState::new(Vec::<Device>::new(), None, window, cx));
        let output_select = cx.new(|cx| SelectState::new(Vec::<Device>::new(), None, window, cx));
        for (sel, dir) in [(&input_select, Dir::Input), (&output_select, Dir::Output)] {
            subs.push(
                cx.subscribe(sel, move |_, _, ev: &SelectEvent<Vec<Device>>, cx| {
                    if let SelectEvent::Confirm(Some(id)) = ev {
                        send(Command::Select(dir, id.clone()), cx);
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
                let st = stats_src.lock().ok().and_then(|s| s.stats.clone());
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
                    if !visible.load(Relaxed) {
                        // Hidden: forget the levels so the meters start from rest.
                        this.meter_in.reset();
                        this.meter_out.reset();
                        return;
                    }
                    let moving = this.meter_in.is_moving() || this.meter_out.is_moving();
                    this.meter_in.update(lin, dt);
                    this.meter_out.update(lout, dt);
                    if moving || this.meter_in.is_moving() || second {
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
            input_select,
            output_select,
            shown: Default::default(),
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

    /// Noise removal card: engine segments, what the engine is for, and strength.
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
        card("Noise removal")
            .child(
                div()
                    .flex()
                    .gap(SP_4)
                    .p(SP_4)
                    .bg(c(BG))
                    .rounded(RADIUS_CONTROL)
                    .children(segments.into_iter().map(|(kind, title)| {
                        let selected = current == Some(kind);
                        div()
                            .id(kind.name())
                            .flex_1()
                            .h(px(32.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(RADIUS_CONTROL)
                            .text_size(px(LABEL))
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
                            .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                                send(Command::SetEngine(kind), cx);
                            }))
                            .child(title)
                    })),
            )
            .child(div().mb(SP_8).child(caption(hint)))
            .child(self.strength_row(st))
    }

    fn strength_row(&self, st: &Status) -> impl IntoElement {
        let off = st.engine == Some(EngineKind::Off);
        let value = (self.params.strength() * 100.0).round();
        div()
            .flex()
            .items_center()
            .gap(SP_12)
            .child(div().w(px(64.0)).child("Strength"))
            .child(
                // gpui-component's controls don't ask for a hand cursor, so each sits in
                // a box that does (only while it can be used).
                div()
                    .flex_1()
                    .when(!off, |d| d.cursor_pointer())
                    .child(Slider::new(&self.strength).disabled(off)),
            )
            .child(
                div()
                    .w(px(36.0))
                    .flex()
                    .justify_end()
                    .text_color(c(if off { TEXT_SECONDARY } else { TEXT_PRIMARY }))
                    .child(format!("{value:.0}%")),
            )
    }

    /// One device row: icon, small label, and a borderless dropdown (its own chevron).
    fn device_row(
        &self,
        icon: &'static str,
        label: &'static str,
        sel: &DeviceSelect,
        placeholder: &'static str,
        empty: bool,
    ) -> gpui::Div {
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
}

/// A panel on `surface` with a small label on top.
fn card(title: &'static str) -> gpui::Div {
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
fn list_card(rows: Vec<gpui::AnyElement>) -> impl IntoElement {
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
/// peak-hold tick, on an `elevated` track.
fn meter(label: &'static str, m: Meter, live: bool) -> impl IntoElement {
    const HOT: f32 = 0.9;
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
                        .child(div().h_full().w(relative(level.min(HOT))).bg(c(ACCENT)))
                        .child(
                            div()
                                .h_full()
                                .w(relative((level - HOT).max(0.0)))
                                .bg(c(METER_PEAK)),
                        ),
                )
                .when(peak > 0.01, |d| {
                    d.child(
                        div()
                            .absolute()
                            .top_0()
                            .h_full()
                            .w(px(2.0))
                            .left(relative((peak - 0.01).max(0.0)))
                            .bg(c(METER_PEAK)),
                    )
                }),
        )
}

/// Which `Params` switch a stage row drives.
type Flag = fn(&Params) -> &AtomicBool;

fn stage_row(
    id: &'static str,
    label: &'static str,
    hint: String,
    flag: Flag,
    params: &Arc<Params>,
) -> gpui::AnyElement {
    let on = flag(params).load(Relaxed);
    let p = Arc::clone(params);
    div()
        .flex()
        .items_center()
        .gap(SP_12)
        .py(SP_12)
        .child(
            div()
                .flex_1()
                .flex()
                .flex_col()
                .child(label)
                .child(caption(hint)),
        )
        .child(
            div()
                .cursor_pointer()
                .child(
                    Switch::new(id)
                        .checked(on)
                        .on_click(move |checked: &bool, window, _| {
                            // Read by the audio thread on its next frame; no lock, no message.
                            flag(&p).store(*checked, Relaxed);
                            window.refresh();
                        }),
                ),
        )
        .into_any_element()
}

/// Calm warning card: 2px `warning` rule on the left, text in `text_primary`.
fn warning_card(what: impl Into<SharedString>, fix: impl Into<SharedString>) -> gpui::Div {
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

/// Header: the Sotto mark, wordmark and tagline, and a small status pill.
fn header(state: &State) -> impl IntoElement {
    let (dot, text) = match state {
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
        let st = self.status.lock().map(|s| s.clone()).unwrap_or_default();
        self.sync_pickers(&st, window, cx);
        let running = st.state == State::Running;

        let mut warnings: Vec<gpui::AnyElement> = Vec::new();
        if st.cable_missing {
            warnings.push(
                warning_card(
                    "VB-Cable isn't installed, so Sotto has nowhere to send your voice.",
                    "Install it from VB-Audio's official page, restart Windows, then reopen Sotto.",
                )
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

        let latency = st
            .latency_ms
            .map_or("—".to_string(), |ms| format!("{ms:.0} ms"));
        let p = &self.params;
        let chain = vec![
            stage_row(
                "hpf",
                "Low-cut filter",
                "Removes rumble below 80 Hz".into(),
                |p| &p.hpf,
                p,
            ),
            stage_row(
                "eq",
                "Voice EQ",
                p.eq_preset().label().to_string(),
                |p| &p.eq,
                p,
            ),
            stage_row(
                "gate",
                "Noise gate",
                // A real minus sign (U+2212), not a hyphen.
                format!("Opens at {:.0} dB", p.gate_threshold_db()).replace('-', "\u{2212}"),
                |p| &p.gate,
                p,
            ),
            stage_row(
                "agc",
                "Auto gain",
                format!("Up to +{:.0} dB", p.agc_max_db()),
                |p| &p.agc,
                p,
            ),
            stage_row(
                "limiter",
                "Limiter",
                "Stops sudden peaks".into(),
                |p| &p.limiter,
                p,
            ),
        ];
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
                .child(div().text_color(c(TEXT_PRIMARY)).child(value))
        };

        div()
            .id("main")
            .size_full()
            .overflow_y_scroll()
            .bg(c(BG))
            .font_family(FONT)
            .text_color(c(TEXT_PRIMARY))
            .text_size(px(BODY))
            .child(
                // The content column. It sits inside the scroll area with its own height,
                // so cards never get squeezed together when the window is short, and it
                // stays a readable width when the window is wide.
                div()
                    .w_full()
                    .max_w(px(MAX_CONTENT_W))
                    .mx_auto()
                    .flex()
                    .flex_col()
                    .gap(SP_16)
                    .p(SP_24)
                    .child(header(&st.state))
                    .children(warnings)
                    .child(self.engine_card(&st, cx))
                    .child(
                        card("Levels")
                            .gap(SP_12)
                            .child(meter("Mic", self.meter_in, running))
                            .child(meter("Output", self.meter_out, running)),
                    )
                    .child(list_card(devices))
                    .child(list_card(chain))
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
