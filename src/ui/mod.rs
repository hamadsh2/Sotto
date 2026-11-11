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
//!
//! Settings: loaded from `%APPDATA%\Sotto\settings.toml` before the audio starts, and
//! saved half a second after the last change (and once more on quit).

mod assets;
mod audio;
mod hotkey;
mod licenses;
mod main_window;
mod mark;
mod meter;
mod theme;
mod tray;
mod win;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::time::Duration;

use gpui::{
    App, AppContext, Application, Bounds, Global, Task, TitlebarOptions, WindowBounds,
    WindowHandle, WindowOptions, px, size,
};
use gpui_component::Root;
use sotto::control::EngineKind;
use sotto::params::Params;
use sotto::settings::{self, SavedDevice, Settings};

use audio::{Audio, Choice};
use main_window::MainWindow;

/// Wait this long after the last change before writing the settings file, so dragging
/// a slider writes once, not sixty times a second.
const SAVE_AFTER: Duration = Duration::from_millis(500);
/// How often to check whether the taskbar switched between light and dark.
const THEME_EVERY: Duration = Duration::from_secs(5);

struct AppState {
    audio: Audio,
    main: Option<WindowHandle<Root>>,
    /// The main window's Windows handle, for hiding/showing it.
    main_id: Option<win::WindowId>,
    /// Shared with the window: false while hidden, so it skips redraws.
    visible: Arc<AtomicBool>,
    tray: Option<tray_icon::TrayIcon>,
    settings: Settings,
    /// What is on disk now, to skip writing an unchanged file.
    saved: Settings,
    settings_path: Option<PathBuf>,
    /// The pending delayed save; replacing it cancels the previous one.
    save_task: Option<Task<()>>,
    /// Whether Sotto's "Start with Windows" entry exists (the registry is the truth).
    autostart: bool,
    /// The global mute shortcut (`None` if Windows wouldn't give us one at all).
    hotkeys: Option<hotkey::Hotkeys>,
    /// Why the saved/chosen shortcut couldn't be used, for the window.
    hotkey_error: Option<String>,
}

impl Global for AppState {}

struct Args {
    input: Option<String>,
    output: Option<String>,
    engine: Option<EngineKind>,
    /// Start in the tray without showing the window (used by "Start with Windows").
    hidden: bool,
}

/// `sotto.exe [--input NAME] [--output NAME] [--engine rnnoise|dfn|off] [--hidden]`
fn parse_args() -> Args {
    let mut a = Args {
        input: None,
        output: None,
        engine: None,
        hidden: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--input" => a.input = it.next(),
            "--output" => a.output = it.next(),
            "--engine" => a.engine = it.next().as_deref().and_then(EngineKind::parse),
            "--hidden" => a.hidden = true,
            _ => {}
        }
    }
    a
}

pub fn run() {
    // Only one Sotto: a second copy brings the first one's window up and leaves.
    // `_claim` marks us as running until `run` returns (the app has quit).
    let (_claim, show_signal) = match win::claim_instance() {
        win::Instance::Second => return,
        win::Instance::First(claim, signal) => (claim, signal),
    };
    let args = parse_args();
    let settings_path = settings::default_path();
    let loaded = settings_path
        .as_deref()
        .map(settings::load)
        .unwrap_or(settings::Loaded {
            settings: Settings::default(),
            note: None,
        });
    let mut s = loaded.settings;
    win::refresh_autostart();
    let autostart = win::autostart_enabled();
    s.start_with_windows = autostart;
    let params = Arc::new(Params::new());
    s.apply(&params);
    let choice = Choice {
        input: args.input,
        output: args.output,
        saved_input: s.input.clone(),
        saved_output: s.output.clone(),
        engine: args.engine.unwrap_or(s.engine()),
        note: loaded.note,
    };
    let hidden = args.hidden;

    Application::new()
        .with_assets(assets::Assets)
        .run(move |cx: &mut App| {
            gpui_component::init(cx);
            theme::load_fonts(cx);
            theme::apply(cx);

            let visible = Arc::new(AtomicBool::new(!hidden));
            let audio = Audio::spawn(choice, params, Arc::clone(&visible));
            let tray = tray::build(cx, on_tray).ok();
            let mut hotkeys = hotkey::Hotkeys::new();
            let hotkey_error = match &mut hotkeys {
                Some(h) => h.set(Some(s.mute_hotkey.as_str())).err(),
                None => Some("Windows didn't allow a global shortcut.".into()),
            };
            // Without a tray there would be no way to open a hidden window.
            let show = !hidden || tray.is_none();
            cx.set_global(AppState {
                audio,
                main: None,
                main_id: None,
                visible,
                tray,
                saved: s.clone(),
                settings: s,
                settings_path,
                save_task: None,
                autostart,
                hotkeys,
                hotkey_error,
            });
            if show {
                open_main(cx);
            }
            // Follow Windows switching between a light and a dark taskbar (one small
            // registry read every 5 s; redraws the tray glyph only if it changed).
            cx.spawn(async move |cx| {
                loop {
                    cx.background_executor().timer(THEME_EVERY).await;
                    let alive = cx.update(|cx| {
                        if let Some(t) = &cx.global::<AppState>().tray {
                            tray::follow_theme(t);
                        }
                    });
                    if alive.is_err() {
                        break;
                    }
                }
            })
            .detach();
            // Wait (off the UI thread, in the kernel: no CPU) for a second copy to ask
            // us to show the window.
            if let Some(signal) = show_signal {
                cx.spawn(async move |cx| {
                    loop {
                        let asked = cx
                            .background_executor()
                            .spawn(async move { signal.wait() })
                            .await;
                        if !asked || cx.update(open_main).is_err() {
                            break;
                        }
                    }
                })
                .detach();
            }
        });
}

fn on_tray(action: tray::Action, cx: &mut App) {
    match action {
        tray::Action::Open => open_main(cx),
        tray::Action::ToggleMute => toggle_mute(cx),
        tray::Action::Quit => {
            save_now(cx);
            // Stop streams and the engine cleanly before the program ends.
            cx.global_mut::<AppState>().audio.stop();
            cx.quit();
        }
    }
}

/// Mute or unmute (tray, shortcut or window). The audio fades in 10 ms; the tray glyph
/// and the window follow.
pub fn toggle_mute(cx: &mut App) {
    let st = cx.global::<AppState>();
    let muted = !st.audio.params.muted.load(Relaxed);
    st.audio.params.muted.store(muted, Relaxed);
    if let Some(t) = &st.tray {
        tray::set_muted(t, muted);
    }
    cx.refresh_windows();
}

/// Something the user can change was changed: save shortly.
fn save_soon(cx: &mut App) {
    let task = cx.spawn(async move |cx| {
        cx.background_executor().timer(SAVE_AFTER).await;
        let _ = cx.update(save_now);
    });
    cx.global_mut::<AppState>().save_task = Some(task);
}

/// Gather the current choices and write them if anything changed. The file is a few
/// hundred bytes, so this is quick enough for the UI thread.
fn save_now(cx: &mut App) {
    let st = cx.global_mut::<AppState>();
    st.save_task = None;
    st.settings.read_params(&st.audio.params);
    if let Ok(s) = st.audio.status.lock() {
        let saved = |d: &audio::Device| SavedDevice {
            id: d.id.clone(),
            name: d.name.clone(),
        };
        // Keep a remembered device even while it's unplugged (the status still holds
        // it then); only a real choice replaces it.
        if let Some(d) = &s.input {
            st.settings.input = Some(saved(d));
        }
        if let Some(d) = &s.output {
            st.settings.output = Some(saved(d));
        }
        if let Some(e) = s.engine {
            st.settings.engine = e.name().into();
        }
    }
    if st.settings == st.saved {
        return;
    }
    if let Some(path) = &st.settings_path
        && settings::save(path, &st.settings).is_ok()
    {
        st.saved = st.settings.clone();
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
    // Lists are only refreshed while visible; this window may be the first one.
    st.audio.send(audio::Command::Refresh);
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
