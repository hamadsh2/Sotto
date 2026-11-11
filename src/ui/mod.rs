// SPDX-License-Identifier: GPL-3.0-only
//! The window app: a tray icon, a main window, and the audio engine on its own thread.
//!
//! Lifetime rules (CLAUDE.md):
//! - Closing the main window *hides* it; audio keeps running. "Open Sotto" in the tray
//!   shows it again. (GPUI ends the program when its last window is destroyed, and a
//!   hidden keep-alive window measured +40 MB, so we hide instead.)
//! - While hidden: Windows never asks a hidden window to paint, and our once-a-second
//!   refresh skips its redraw, so nothing is drawn and the GPU stays idle.
//! - Only "Quit Sotto" in the tray ends the program.

mod assets;
mod audio;
mod main_window;
mod meter;
mod theme;
mod tray;
mod win;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

use gpui::{
    App, AppContext, Application, Bounds, Global, TitlebarOptions, WindowBounds, WindowHandle,
    WindowOptions, px, size,
};
use gpui_component::Root;
use sotto::control::EngineKind;

use audio::{Audio, Choice};
use main_window::MainWindow;

struct AppState {
    audio: Audio,
    main: Option<WindowHandle<Root>>,
    /// The main window's Windows handle, for hiding/showing it.
    main_id: Option<win::WindowId>,
    /// Shared with the window: false while hidden, so it skips redraws.
    visible: Arc<AtomicBool>,
    tray: Option<tray_icon::TrayIcon>,
}

impl Global for AppState {}

/// `sotto.exe [--input NAME] [--output NAME] [--engine rnnoise|dfn|off]`
fn parse_args() -> Choice {
    let mut choice = Choice {
        input: None,
        output: None,
        engine: EngineKind::Rnnoise,
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--input" => choice.input = it.next(),
            "--output" => choice.output = it.next(),
            "--engine" => {
                if let Some(k) = it.next().as_deref().and_then(EngineKind::parse) {
                    choice.engine = k;
                }
            }
            _ => {}
        }
    }
    choice
}

pub fn run() {
    let choice = parse_args();
    Application::new()
        .with_assets(assets::Assets)
        .run(move |cx: &mut App| {
            gpui_component::init(cx);
            theme::apply(cx);

            let visible = Arc::new(AtomicBool::new(true));
            let audio = Audio::spawn(choice, Arc::clone(&visible));
            let tray = tray::build(cx, on_tray).ok();
            cx.set_global(AppState {
                audio,
                main: None,
                main_id: None,
                visible,
                tray,
            });
            open_main(cx);
        });
}

fn on_tray(action: tray::Action, cx: &mut App) {
    match action {
        tray::Action::Open => open_main(cx),
        tray::Action::Quit => {
            // Stop streams and the engine cleanly before the program ends.
            cx.global_mut::<AppState>().audio.stop();
            cx.quit();
        }
    }
}

/// Show the main window (creating it the first time) and bring it to the front.
fn open_main(cx: &mut App) {
    let st = cx.global::<AppState>();
    if let (Some(handle), Some(id)) = (st.main, st.main_id)
        && win::exists(id)
    {
        st.visible.store(true, Relaxed);
        // Devices may have changed while hidden (lists aren't refreshed then).
        st.audio.send(audio::Command::Refresh);
        win::show(id);
        // Nothing was drawn while hidden: redraw now with fresh values.
        let _ = handle.update(cx, |_, window, _| window.refresh());
        return;
    }

    let status = st.audio.status.clone();
    let params = st.audio.params.clone();
    let visible = st.visible.clone();
    visible.store(true, Relaxed);
    // Without a tray there would be no way back, so then closing really closes (and,
    // being the last window, ends the program).
    let hide_on_close = st.tray.is_some();
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
            None,
            size(px(400.0), px(800.0)),
            cx,
        ))),
        titlebar: Some(TitlebarOptions {
            title: Some("Sotto".into()),
            ..Default::default()
        }),
        is_resizable: false,
        app_id: Some("Sotto".into()),
        ..Default::default()
    };
    let handle = cx.open_window(options, |window, cx| {
        if hide_on_close {
            window.on_window_should_close(cx, |_, cx| hide_main(cx));
        }
        let view = cx.new(|cx| MainWindow::new(status, params, visible, window, cx));
        cx.new(|cx| Root::new(view, window, cx))
    });
    let st = cx.global_mut::<AppState>();
    st.main = handle.ok();
    // Sotto has exactly one GPUI window. If a second one is ever added (e.g. About),
    // pick the main one explicitly here instead of the first.
    st.main_id = win::gpui_windows().first().copied();
}

/// Close button / Alt+F4: hide instead of closing. Returns whether to really close.
fn hide_main(cx: &mut App) -> bool {
    let st = cx.global_mut::<AppState>();
    if st.main_id.is_none_or(|id| !win::exists(id)) {
        st.main_id = win::gpui_windows().first().copied();
    }
    match st.main_id {
        Some(id) => {
            st.visible.store(false, Relaxed);
            win::hide(id);
            false
        }
        // Can't find our window to hide it: let it close rather than ignore the user.
        None => true,
    }
}
