// SPDX-License-Identifier: GPL-3.0-only
//! The main window (3a: status only; controls arrive in 3b).
//!
//! It redraws once a second while visible. While hidden (closed to the tray) it only
//! keeps its CPU/memory numbers current, which costs two cheap system calls a second,
//! and never asks for a redraw.

use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gpui::{
    Context, FontWeight, IntoElement, ParentElement, Render, SharedString, Styled, Task, Window,
    div, px,
};
use sotto::stats;

use super::audio::{State, Status};
use super::theme::*;

pub struct MainWindow {
    status: Arc<Mutex<Status>>,
    cpu_pct: f64,
    private_mb: f64,
    last_cpu: Duration,
    last_wall: Instant,
    cores: f64,
    /// Dropping the window drops this task, which stops the refresh timer.
    _refresh: Task<()>,
}

impl MainWindow {
    pub fn new(
        status: Arc<Mutex<Status>>,
        visible: Arc<AtomicBool>,
        cx: &mut Context<Self>,
    ) -> Self {
        let refresh = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let alive = this.update(cx, |this, cx| {
                    this.sample();
                    if visible.load(Relaxed) {
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
            cpu_pct: 0.0,
            private_mb: stats::process_memory().map_or(0.0, |(_, p)| p as f64 / 1_048_576.0),
            last_cpu: stats::process_cpu_time().unwrap_or_default(),
            last_wall: Instant::now(),
            cores: std::thread::available_parallelism().map_or(1, |n| n.get()) as f64,
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
}

fn row(label: &'static str, value: impl Into<SharedString>) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap(SP_4)
        .child(
            div()
                .text_size(px(LABEL))
                .text_color(c(TEXT_SECONDARY))
                .child(label),
        )
        .child(
            div()
                .text_size(px(BODY))
                .font_weight(FontWeight::MEDIUM)
                .text_color(c(TEXT_PRIMARY))
                .child(value.into()),
        )
}

/// Calm warning card: 2px `warning` rule on the left, text in `text_primary`.
fn warning_card(what: &'static str, fix: &'static str) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap(SP_4)
        .p(SP_12)
        .bg(c(SURFACE))
        .rounded(RADIUS_CONTROL)
        .border_l_2()
        .border_color(c(WARNING))
        .text_size(px(LABEL))
        .child(div().text_color(c(TEXT_PRIMARY)).child(what))
        .child(div().text_color(c(TEXT_SECONDARY)).child(fix))
}

impl Render for MainWindow {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let st = self.status.lock().map(|s| s.clone()).unwrap_or_default();
        let (state_text, state_hint): (String, Option<String>) = match &st.state {
            State::Starting => ("Starting…".into(), None),
            State::Running => ("Running".into(), None),
            State::Waiting(why) => ("Waiting for devices".into(), Some(why.clone())),
            State::Stopped(why) => ("Stopped".into(), Some(why.clone())),
        };
        let dash = |s: &str| {
            if s.is_empty() {
                "—".to_string()
            } else {
                s.to_string()
            }
        };

        div()
            .size_full()
            .flex()
            .flex_col()
            .gap(SP_24)
            .p(SP_24)
            .bg(c(BG))
            .font_family(FONT)
            .text_color(c(TEXT_PRIMARY))
            .text_size(px(BODY))
            // Header: wordmark + tagline.
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(SP_4)
                    .child(
                        div()
                            .text_size(px(HEADING))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Sotto"),
                    )
                    .child(
                        div()
                            .text_size(px(LABEL))
                            .text_color(c(TEXT_SECONDARY))
                            .child("Just your voice."),
                    ),
            )
            // Status panel.
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(SP_16)
                    .p(SP_16)
                    .bg(c(SURFACE))
                    .rounded(RADIUS_PANEL)
                    .child(row("Status", state_text))
                    .child(row(
                        "Engine",
                        st.engine.map_or("—".to_string(), |e| e.label().to_string()),
                    ))
                    .child(row("Microphone", dash(&st.input)))
                    .child(row("Output", dash(&st.output))),
            )
            .children(state_hint.map(|h| {
                div()
                    .text_size(px(LABEL))
                    .text_color(c(TEXT_SECONDARY))
                    .child(h)
            }))
            .children(st.cable_is_default.then(|| {
                warning_card(
                    "CABLE Input is your Windows default playback device, so games and music go into the cable.",
                    "Fix it in Settings → System → Sound → Output: pick your headphones or speakers. Sotto won't change it for you.",
                )
            }))
            // Footer readout.
            .child(
                div()
                    .mt_auto()
                    .flex()
                    .gap(SP_8)
                    .text_size(px(CAPTION))
                    .text_color(c(TEXT_SECONDARY))
                    .child("CPU")
                    .child(div().text_color(c(TEXT_PRIMARY)).child(format!("{:.2}%", self.cpu_pct)))
                    .child("·")
                    .child("Memory")
                    .child(
                        div()
                            .text_color(c(TEXT_PRIMARY))
                            .child(format!("{:.1} MB", self.private_mb)),
                    ),
            )
    }
}
