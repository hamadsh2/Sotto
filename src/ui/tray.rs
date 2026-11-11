// SPDX-License-Identifier: GPL-3.0-only
//! Tray icon with "Open Sotto", "Mute" and "Quit Sotto" (plus an "Update available"
//! item when there is one).
//!
//! tray-icon reports clicks through a callback that Windows calls on the main thread
//! (the same thread as the UI). The callback doesn't touch the UI directly: it schedules
//! a small task on the UI's own queue, which runs right after. So it's event-driven —
//! nothing polls while the window is closed. The mute shortcut (`hotkey.rs`) uses the
//! same path.

use std::cell::{Cell, RefCell};

use gpui::{App, AsyncApp};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

use super::mark::{self, Style};
use super::win;

#[derive(Clone, Copy)]
pub enum Action {
    Open,
    ToggleMute,
    Quit,
}

const OPEN_ID: &str = "open";
const MUTE_ID: &str = "mute";
const UPDATE_ID: &str = "update";
const QUIT_ID: &str = "quit";

/// The UI app and the action handler.
type Handler = (AsyncApp, fn(Action, &mut App));

thread_local! {
    /// Handle to the UI app and the action handler, so callbacks (main thread) can
    /// schedule work on it.
    static APP: RefCell<Option<Handler>> = const { RefCell::new(None) };
    /// The "Mute" / "Unmute" menu item, to relabel it.
    static MUTE_ITEM: RefCell<Option<MenuItem>> = const { RefCell::new(None) };
    /// The menu, to add or remove the "Update available" item.
    static MENU: RefCell<Option<Menu>> = const { RefCell::new(None) };
    /// The "Update available" item while it's in the menu.
    static UPDATE_ITEM: RefCell<Option<MenuItem>> = const { RefCell::new(None) };
    /// Shown in the tooltip when not muted: a newer Sotto is available or installed.
    static UPDATE_TIP: Cell<bool> = const { Cell::new(false) };
    /// What the icon currently shows: (light taskbar, muted).
    static SHOWN: Cell<(bool, bool)> = const { Cell::new((false, false)) };
}

/// Run `action` on the UI thread soon (called from Windows callbacks on the main thread).
pub fn dispatch(action: Action) {
    APP.with(|a| {
        if let Some((app, on_action)) = a.borrow().as_ref() {
            let on_action = *on_action;
            app.spawn(async move |cx| {
                let _ = cx.update(|cx| on_action(action, cx));
            })
            .detach();
        }
    });
}

/// Create the tray icon. `on_action` runs on the UI thread with full app access.
pub fn build(cx: &mut App, on_action: fn(Action, &mut App)) -> Result<TrayIcon, String> {
    APP.with(|a| *a.borrow_mut() = Some((cx.to_async(), on_action)));

    MenuEvent::set_event_handler(Some(|e: MenuEvent| match e.id.0.as_str() {
        OPEN_ID | UPDATE_ID => dispatch(Action::Open),
        MUTE_ID => dispatch(Action::ToggleMute),
        QUIT_ID => dispatch(Action::Quit),
        _ => {}
    }));
    TrayIconEvent::set_event_handler(Some(|e: TrayIconEvent| {
        if let TrayIconEvent::Click {
            button: MouseButton::Left,
            button_state: MouseButtonState::Up,
            ..
        } = e
        {
            dispatch(Action::Open);
        }
    }));

    let mute = MenuItem::with_id(MUTE_ID, "Mute", true, None);
    let menu = Menu::new();
    menu.append_items(&[
        &MenuItem::with_id(OPEN_ID, "Open Sotto", true, None),
        &mute,
        &PredefinedMenuItem::separator(),
        &MenuItem::with_id(QUIT_ID, "Quit Sotto", true, None),
    ])
    .map_err(|e| e.to_string())?;
    MUTE_ITEM.with(|m| *m.borrow_mut() = Some(mute));
    MENU.with(|m| *m.borrow_mut() = Some(menu.clone()));

    let light = win::taskbar_is_light();
    SHOWN.with(|s| s.set((light, false)));
    TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .with_tooltip("Sotto")
        .with_icon(icon(light, false)?)
        .build()
        .map_err(|e| e.to_string())
}

/// Show the muted / live state: glyph, tooltip and menu label.
pub fn set_muted(tray: &TrayIcon, muted: bool) {
    MUTE_ITEM.with(|m| {
        if let Some(item) = m.borrow().as_ref() {
            item.set_text(if muted { "Unmute" } else { "Mute" });
        }
    });
    set_tooltip(tray, muted);
    let light = SHOWN.with(|s| s.get().0);
    redraw(tray, light, muted);
}

fn set_tooltip(tray: &TrayIcon, muted: bool) {
    let tip = if muted {
        "Sotto — muted"
    } else if UPDATE_TIP.with(|u| u.get()) {
        "Sotto — update available"
    } else {
        "Sotto"
    };
    let _ = tray.set_tooltip(Some(tip));
}

/// Show (Some(version)) or remove the "Update available" menu item. Clicking it opens
/// the window, where the user decides.
pub fn set_update(tray: &TrayIcon, version: Option<&str>) {
    UPDATE_TIP.with(|u| u.set(version.is_some()));
    MENU.with(|m| {
        let menu = m.borrow();
        let Some(menu) = menu.as_ref() else { return };
        UPDATE_ITEM.with(|i| {
            let mut item = i.borrow_mut();
            match (version, item.as_ref()) {
                (Some(v), Some(it)) => it.set_text(format!("Sotto {v} is available")),
                (Some(v), None) => {
                    let it =
                        MenuItem::with_id(UPDATE_ID, format!("Sotto {v} is available"), true, None);
                    if menu.insert(&it, 0).is_ok() {
                        *item = Some(it);
                    }
                }
                (None, Some(it)) => {
                    let _ = menu.remove(it);
                    *item = None;
                }
                (None, None) => {}
            }
        });
    });
    let muted = SHOWN.with(|s| s.get().1);
    set_tooltip(tray, muted);
}

/// Follow Windows switching between a light and a dark taskbar. Cheap (one registry
/// read); redraws only when it changed.
pub fn follow_theme(tray: &TrayIcon) {
    let light = win::taskbar_is_light();
    let (was, muted) = SHOWN.with(|s| s.get());
    if light != was {
        redraw(tray, light, muted);
    }
}

fn redraw(tray: &TrayIcon, light: bool, muted: bool) {
    if let Ok(i) = icon(light, muted) {
        let _ = tray.set_icon(Some(i));
        SHOWN.with(|s| s.set((light, muted)));
    }
}

/// The Sotto mark as a monochrome glyph: light on a dark taskbar, dark on a light one
/// (the spec's `text_primary` colours for each theme). Muted: the wave flat, slashed.
fn icon(light: bool, muted: bool) -> Result<Icon, String> {
    const N: u32 = 32;
    let ink = if light {
        [0x2B, 0x29, 0x25]
    } else {
        [0xE8, 0xE4, 0xDC]
    };
    let style = if muted {
        Style::TrayMuted(ink)
    } else {
        Style::Tray(ink)
    };
    Icon::from_rgba(mark::render(N, style), N, N).map_err(|e| e.to_string())
}
