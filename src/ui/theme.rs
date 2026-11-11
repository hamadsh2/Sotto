// SPDX-License-Identifier: GPL-3.0-only
//! Design tokens from the `sotto-design` skill (dark theme). Every color, size and
//! radius in the UI comes from here; components never hard-code a value.
//!
//! Python analogy: a constants module (`theme.py`) that every widget imports, plus an
//! `apply()` that pushes the same values into the component library's global theme so
//! its widgets don't keep their default (blue/grey) look.

use gpui::{App, Hsla, Pixels, px, rgb};
use gpui_component::{Theme, ThemeMode};

/// Hex `0xRRGGBB` → gpui color.
pub fn c(hex: u32) -> Hsla {
    rgb(hex).into()
}

// Colors (dark).
pub const BG: u32 = 0x1C1B1A;
pub const SURFACE: u32 = 0x252422;
pub const ELEVATED: u32 = 0x2E2C29;
pub const BORDER: u32 = 0x3A3733;
pub const TEXT_PRIMARY: u32 = 0xE8E4DC;
pub const TEXT_SECONDARY: u32 = 0xA39E94;
pub const ACCENT: u32 = 0x8FA89A;
pub const ACCENT_HOVER: u32 = 0xA3BBAE;
pub const WARNING: u32 = 0xC48A72;
/// Meter peak-hold tick and the top ~10% of the scale.
pub const METER_PEAK: u32 = 0xC9B79C;

// Type scale (px).
pub const CAPTION: f32 = 11.0;
pub const LABEL: f32 = 12.0;
pub const BODY: f32 = 13.0;
pub const HEADING: f32 = 18.0;

// Spacing scale (px): 4 / 8 / 12 / 16 / 24 / 32 only.
pub const SP_4: Pixels = px(4.0);
pub const SP_8: Pixels = px(8.0);
pub const SP_12: Pixels = px(12.0);
pub const SP_16: Pixels = px(16.0);
pub const SP_24: Pixels = px(24.0);

// Radius.
pub const RADIUS_CONTROL: Pixels = px(8.0);
pub const RADIUS_PANEL: Pixels = px(12.0);

/// Font. Inter (embedded) arrives with the branding step; Segoe UI is Windows' own UI
/// font and needs nothing bundled.
pub const FONT: &str = "Segoe UI";

/// Point gpui-component's theme at our tokens.
pub fn apply(cx: &mut App) {
    Theme::change(ThemeMode::Dark, None, cx);
    let t = Theme::global_mut(cx);
    t.font_family = FONT.into();
    t.font_size = px(BODY);
    t.radius = RADIUS_CONTROL;
    t.radius_lg = RADIUS_PANEL;
    t.shadow = false;

    let k = &mut t.colors;
    k.background = c(BG);
    k.foreground = c(TEXT_PRIMARY);
    k.border = c(BORDER);
    k.input = c(BORDER);
    k.ring = c(ACCENT);
    k.primary = c(ACCENT);
    k.primary_hover = c(ACCENT_HOVER);
    k.primary_active = c(ACCENT);
    k.primary_foreground = c(BG);
    k.secondary = c(SURFACE);
    k.secondary_hover = c(ELEVATED);
    k.secondary_active = c(ELEVATED);
    k.secondary_foreground = c(TEXT_PRIMARY);
    k.muted = c(SURFACE);
    k.muted_foreground = c(TEXT_SECONDARY);
    // gpui-component's "accent" is its hover fill, i.e. our `elevated`.
    k.accent = c(ELEVATED);
    k.accent_foreground = c(TEXT_PRIMARY);
    k.popover = c(ELEVATED);
    k.popover_foreground = c(TEXT_PRIMARY);
    k.slider_bar = c(ACCENT);
    k.slider_thumb = c(TEXT_PRIMARY);
    k.switch = c(BORDER);
    k.warning = c(WARNING);
    k.window_border = c(BORDER);
    k.title_bar = c(BG);
    k.title_bar_border = c(BORDER);
}
