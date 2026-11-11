// SPDX-License-Identifier: GPL-3.0-only
//! The global mute shortcut: works while a game or any other app has focus.
//!
//! `global-hotkey` asks Windows to report one key combination to us (RegisterHotKey).
//! Windows calls back on the main thread, like the tray; the callback only schedules
//! "toggle mute" on the UI's queue. Nothing polls.
//!
//! Shortcuts are stored as readable text, e.g. "Ctrl+Alt+M", which is also what the
//! window shows. Every shortcut needs at least one modifier (Ctrl, Alt, Shift or Win),
//! so Sotto never takes over a plain key you use in a game.

use std::str::FromStr;

use global_hotkey::hotkey::HotKey;
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};

use super::tray::{self, Action};

pub struct Hotkeys {
    manager: GlobalHotKeyManager,
    /// The registered shortcut and its text.
    current: Option<(HotKey, String)>,
    /// While the user records a new shortcut, the old one is set aside so pressing it
    /// is seen by the window instead of toggling mute.
    paused: bool,
}

impl Hotkeys {
    /// Start listening. `None` if Windows refuses (then there's simply no shortcut).
    pub fn new() -> Option<Self> {
        let manager = GlobalHotKeyManager::new().ok()?;
        GlobalHotKeyEvent::set_event_handler(Some(|e: GlobalHotKeyEvent| {
            // Only one shortcut is ever registered, so any press is "toggle mute".
            if e.state == HotKeyState::Pressed {
                tray::dispatch(Action::ToggleMute);
            }
        }));
        Some(Self {
            manager,
            current: None,
            paused: false,
        })
    }

    /// The shortcut's text, if one is set.
    pub fn current(&self) -> Option<&str> {
        self.current.as_ref().map(|(_, t)| t.as_str())
    }

    /// Use `text` as the shortcut (`None` = no shortcut). On failure the previous one
    /// stays active and the reason is returned.
    pub fn set(&mut self, text: Option<&str>) -> Result<(), String> {
        let new = match text {
            Some(t) if !t.is_empty() => Some((
                HotKey::from_str(t)
                    .map_err(|_| format!("\u{201C}{t}\u{201D} isn't a shortcut Sotto can use."))?,
                t.to_string(),
            )),
            _ => None,
        };
        let old = self.current.take();
        if let Some((k, _)) = &old
            && !self.paused
        {
            let _ = self.manager.unregister(*k);
        }
        if let Some((k, t)) = &new
            && let Err(_) = self.manager.register(*k)
        {
            // Put the previous one back.
            if let Some((ok, _)) = &old
                && !self.paused
            {
                let _ = self.manager.register(*ok);
            }
            self.current = old;
            return Err(format!(
                "{} is already used by another app. Try another shortcut.",
                label(t)
            ));
        }
        self.current = new;
        self.paused = false;
        Ok(())
    }

    /// Stop reacting to the shortcut for a moment (while recording a new one).
    pub fn pause(&mut self) {
        if !self.paused
            && let Some((k, _)) = &self.current
        {
            let _ = self.manager.unregister(*k);
        }
        self.paused = true;
    }

    pub fn resume(&mut self) {
        if self.paused
            && let Some((k, _)) = &self.current
        {
            let _ = self.manager.register(*k);
        }
        self.paused = false;
    }
}

/// How a shortcut is shown: "Ctrl+Alt+M" ("Super" is shown as "Win").
pub fn label(text: &str) -> String {
    text.replace("Super", "Win")
}

/// Turn a key press in the window into shortcut text, or say why it can't be one.
/// `key` is GPUI's key name ("m", "f5", "1", "space", …).
pub fn from_press(
    ctrl: bool,
    alt: bool,
    shift: bool,
    win: bool,
    key: &str,
) -> Result<String, &'static str> {
    if !(ctrl || alt || shift || win) {
        return Err("Hold Ctrl, Alt, Shift or Win with the key, so games keep their keys.");
    }
    let key = match key {
        k if k.len() == 1 && k.chars().all(|c| c.is_ascii_alphanumeric()) => k.to_ascii_uppercase(),
        k if k.starts_with('f') && k[1..].parse::<u8>().is_ok_and(|n| (1..=24).contains(&n)) => {
            k.to_ascii_uppercase()
        }
        "space" => "Space".into(),
        "home" => "Home".into(),
        "end" => "End".into(),
        "insert" => "Insert".into(),
        "pageup" => "PageUp".into(),
        "pagedown" => "PageDown".into(),
        _ => return Err("Use a letter, a number or F1–F24 for the key."),
    };
    let mut parts: Vec<&str> = Vec::new();
    if ctrl {
        parts.push("Ctrl");
    }
    if alt {
        parts.push("Alt");
    }
    if shift {
        parts.push("Shift");
    }
    if win {
        parts.push("Super");
    }
    let text = format!("{}+{key}", parts.join("+"));
    // Make sure the shortcut library reads it the same way.
    HotKey::from_str(&text).map_err(|_| "That key can't be used for a shortcut.")?;
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_shortcut_parses() {
        assert!(HotKey::from_str(sotto::settings::MUTE_HOTKEY_DEFAULT).is_ok());
    }

    #[test]
    fn presses_become_readable_shortcuts() {
        assert_eq!(
            from_press(true, true, false, false, "m"),
            Ok("Ctrl+Alt+M".into())
        );
        assert_eq!(
            from_press(false, false, true, false, "f9"),
            Ok("Shift+F9".into())
        );
        assert_eq!(
            from_press(true, false, false, true, "1"),
            Ok("Ctrl+Super+1".into())
        );
        assert_eq!(label("Ctrl+Super+1"), "Ctrl+Win+1");
        // Every produced shortcut parses.
        for k in ["a", "z", "0", "9", "f1", "f24", "space", "home", "pagedown"] {
            let t = from_press(true, false, false, false, k).unwrap();
            assert!(HotKey::from_str(&t).is_ok(), "{t}");
        }
    }

    #[test]
    fn plain_keys_and_odd_keys_are_refused() {
        assert!(from_press(false, false, false, false, "m").is_err());
        assert!(from_press(true, false, false, false, "f25").is_err());
        assert!(from_press(true, false, false, false, "escape").is_err());
    }
}
