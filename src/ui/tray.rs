// SPDX-License-Identifier: GPL-3.0-only
//! Tray icon with "Open Sotto" and "Quit Sotto".
//!
//! tray-icon reports clicks through a callback that Windows calls on the main thread
//! (the same thread as the UI). The callback doesn't touch the UI directly: it schedules
//! a small task on the UI's own queue, which runs right after. So it's event-driven —
//! nothing polls while the window is closed.

use std::cell::RefCell;

use gpui::{App, AsyncApp};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

#[derive(Clone, Copy)]
pub enum Action {
    Open,
    Quit,
}

const OPEN_ID: &str = "open";
const QUIT_ID: &str = "quit";

thread_local! {
    /// Handle to the UI app, so tray callbacks (main thread) can schedule work on it.
    static APP: RefCell<Option<AsyncApp>> = const { RefCell::new(None) };
}

/// Create the tray icon. `on_action` runs on the UI thread with full app access.
pub fn build(cx: &mut App, on_action: fn(Action, &mut App)) -> Result<TrayIcon, String> {
    APP.with(|a| *a.borrow_mut() = Some(cx.to_async()));

    let dispatch = move |action: Action| {
        APP.with(|a| {
            if let Some(app) = a.borrow().as_ref() {
                app.spawn(async move |cx| {
                    let _ = cx.update(|cx| on_action(action, cx));
                })
                .detach();
            }
        });
    };
    MenuEvent::set_event_handler(Some(move |e: MenuEvent| match e.id.0.as_str() {
        OPEN_ID => dispatch(Action::Open),
        QUIT_ID => dispatch(Action::Quit),
        _ => {}
    }));
    TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
        if let TrayIconEvent::Click {
            button: MouseButton::Left,
            button_state: MouseButtonState::Up,
            ..
        } = e
        {
            dispatch(Action::Open);
        }
    }));

    let menu = Menu::new();
    menu.append_items(&[
        &MenuItem::with_id(OPEN_ID, "Open Sotto", true, None),
        &PredefinedMenuItem::separator(),
        &MenuItem::with_id(QUIT_ID, "Quit Sotto", true, None),
    ])
    .map_err(|e| e.to_string())?;

    TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .with_tooltip("Sotto")
        .with_icon(icon()?)
        .build()
        .map_err(|e| e.to_string())
}

/// The Sotto mark as a monochrome glyph: light on a dark taskbar, dark on a light one
/// (read once at start; the spec's `text_primary` colours for each theme).
fn icon() -> Result<Icon, String> {
    const N: u32 = 32;
    let ink = if super::win::taskbar_is_light() {
        [0x2B, 0x29, 0x25]
    } else {
        [0xE8, 0xE4, 0xDC]
    };
    let rgba = super::mark::render(N, super::mark::Style::Tray(ink));
    Icon::from_rgba(rgba, N, N).map_err(|e| e.to_string())
}
