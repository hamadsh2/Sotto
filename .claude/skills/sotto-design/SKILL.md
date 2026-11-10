---
name: sotto-design
description: Sotto's branding and design system — color tokens (dark/light), typography, spacing, radius, elevation, motion, meter behavior, logo/app/tray icon specs, and a screenshot review checklist. Load whenever working on UI (GPUI/gpui-component), icons, the logo, or README visuals, and whenever the user pastes a screenshot of the window.
---

# Sotto design system

**Mood:** calm, minimal, classy, easy on the eyes. Warm neutrals, one muted accent
(sage), no pure black (`#000`), no pure white (`#FFF`), no gradients, no heavy borders.

**Rule zero:** every color, size, radius, and duration comes from a token defined here.
Define tokens first (one `theme` module), then apply them everywhere. Never leave a
gpui-component on its default styling, and never hard-code a hex value in a component.

## Color tokens

| Token | Dark (default) | Light | Use |
|---|---|---|---|
| `bg` | `#1C1B1A` | `#F4F1EA` | Window background |
| `surface` | `#252422` | `#FBF9F5` | Panels, cards |
| `elevated` | `#2E2C29` | *not specified — ask the user* | Popovers, dropdowns, hover fills |
| `border` | `#3A3733` | `#E2DDD3` | Hairline dividers only (1px) |
| `text_primary` | `#E8E4DC` | `#2B2925` | Body text, values |
| `text_secondary` | `#A39E94` | `#6E695F` | Labels, captions, hints |
| `accent` | `#8FA89A` | `#6F8C7E` | Active toggles, slider fill, focus ring, selected item |
| `accent_hover` | `#A3BBAE` | *not specified — ask the user* | Hover on accent elements |
| `meter_peak` | `#C9B79C` | `#B09A78` | Meter peak-hold marker / hot zone |
| `warning` | `#C48A72` | `#B0705A` | Calm warnings (VB-Cable default device, missing device) |

Meter body uses `accent`; the peak-hold tick and the top ~10% of the scale use
`meter_peak`. Muted state uses `text_secondary` (desaturated), never red.

### Measured contrast (WCAG 2.x)

| Pair | Dark | Light | Verdict |
|---|---|---|---|
| text_primary on bg / surface | 13.6 / 12.2 | 12.9 / 13.8 | AA ✅ |
| text_secondary on bg / surface | 6.5 / 5.8 | 4.8 / 5.2 | AA ✅ |
| accent on bg / surface | 6.7 / 6.1 | **3.3 / 3.5** | Dark ✅ · Light: UI-only (≥3:1), **not for body text** |
| warning on bg / surface | 5.9 / 5.3 | **3.5 / 3.8** | Dark ✅ · Light: icon/border only, pair with text_primary text |
| meter_peak on bg / surface | 8.8 / 7.9 | **2.4 / 2.6** | Light fails 3:1 for graphics — open issue, ask user |

Rules from this: in light theme, never render text in `accent` or `warning`; use
`text_primary` for the message and the token color for an icon, dot, or left rule.
Text on an accent fill: dark theme use `bg` on `accent` (6.7:1); light theme use
`text_primary` on `accent` (4.0:1 — large/bold text only).

## Typography
- **Font:** Inter, embedded in the binary (SIL OFL 1.1 — include its license in the
  third-party licenses page).
- **Tabular numerals** (`tnum`) for every numeric readout: CPU %, latency ms, dB, slider
  values. Numbers must not jitter in width as they change.
- Scale (px): `caption 11` · `label 12` · `body 13` · `title 15` · `heading 18`.
- Weights: 400 body, 500 labels/values, 600 headings. No bold-everywhere.
- Sentence case for all labels ("Noise gate", not "NOISE GATE" or "Noise Gate").

## Spacing, radius, elevation
- Spacing scale only: `4 / 8 / 12 / 16 / 24 / 32`. Nothing in between.
  - Inside controls: 8–12. Between related controls: 12. Between sections: 24.
    Window padding: 16–24.
- Radius: **8** for controls (buttons, inputs, toggles, dropdowns), **12** for panels.
- Elevation: very soft only — e.g. shadow `0 2 8` at ≤20% opacity of `bg`'s darker
  neighbor in dark, ≤8% black-ish warm in light. Prefer a surface color step over a shadow.
- Borders: 1px `border` hairlines only where a surface step isn't enough.
- Generous whitespace: when in doubt, remove a divider and add 8px instead.

## Motion
- Durations 150–200 ms, ease-out. No bounces, springs, or looping animations.
- Hover/press: color transitions only (150 ms). No scale effects.
- **Meters:** fast attack (~10 ms), slow release (~300 ms), peak-hold ~1 s then fall.
  Meters animate only while the window is visible; stop all animation frames when
  hidden (zero GPU while hidden is a hard requirement).

## Components (apply to gpui-component)
- **Engine selector:** segmented control — "Off" · "Light — for gaming" ·
  "High quality — for recording & calls". Selected segment: `accent` fill or underline.
- **Strength slider:** track `elevated`, fill `accent`, thumb `text_primary`, value in
  tabular numerals to the right.
- **Toggles (chain stages):** off track `border`, on track `accent`. Label left in
  `text_primary`, optional hint below in `text_secondary`.
- **Device pickers:** dropdown on `surface`, menu on `elevated`, radius 8.
- **Readouts (CPU / latency):** `caption`, `text_secondary` label + `text_primary`
  tabular value, e.g. `CPU 0.6%  ·  Latency 21 ms`.
- **Mute state:** subtle — a small pill or the meter dimming to `text_secondary`, plus
  "Muted" label. Never a red flashing banner.
- **Warnings:** calm card on `surface` with a 2px `warning` left rule, one sentence of
  what's wrong + one sentence of how to fix it.
- Focus ring: 2px `accent`, offset 2px, for keyboard accessibility.

## Logo & icons
- **Mark:** a single soft sound wave that gradually flattens into a straight line —
  noise becoming calm. Left side: 2–3 gentle oscillations of decreasing amplitude;
  right side: a flat line. Single stroke, round caps and joins, uniform stroke width.
- Master as SVG (`assets/icon/sotto-mark.svg`), 24×24 viewBox, stroke-based.
- **App icon:** sage (`#8FA89A`) mark centered on a `#1C1B1A` rounded square
  (corner radius ≈ 22% of size), mark occupying ~60% of width.
  Export `.ico` with sizes 16, 20, 24, 32, 40, 48, 64, 128, 256.
  At 16/20/24 px, thicken the stroke and reduce to one oscillation so it stays legible.
- **Tray icon:** monochrome mark (follows Windows light/dark taskbar: light glyph on dark
  taskbar, dark glyph on light). **Muted state:** the same mark with the wave fully flat
  plus a small diagonal slash, or reduced opacity — distinct at 16 px.
- Wordmark: "Sotto" in Inter 600, `text_primary`, slight positive tracking.
- Tagline: "Just your voice." — `text_secondary`.

## README visuals
- Use the dark-theme app icon and a real screenshot (no mockups) at 1× and 2×.
- No gradients, badges wall, or stock imagery. Keep it as calm as the app.

## Screenshot review checklist
When the user pastes a screenshot, check each and give concrete fixes (token / px):
1. Any default gpui-component styling left (blue accents, default greys, default radii)?
2. Any color not in the token table? Any pure black/white?
3. Spacing on the 4/8/12/16/24/32 scale? Consistent section gaps (24)?
4. Radii: 8 on controls, 12 on panels?
5. Numbers in tabular numerals; no width jitter?
6. Text contrast meets the table above (no accent/warning text in light theme)?
7. Hierarchy: one clear primary control (engine selector), secondary info quieter?
8. Heavy borders, dividers, or shadows that could be removed?
9. Meter: fast attack / slow release, peak color only at the top?
10. Mute and warning states calm and legible?
