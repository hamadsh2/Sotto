// SPDX-License-Identifier: GPL-3.0-only
//! The Sotto mark drawn in code: a soft wave that calms into a straight line.
//!
//! It is the same shape as `assets/icon/sotto-mark.svg` (24×24 units, stroke 2, round
//! caps). Drawing it here, with no dependencies, lets one file serve two places:
//! - `build.rs` draws the app icon at every size and embeds it in `sotto.exe`;
//! - the tray draws its glyph at run time.
//!
//! Python analogy: a tiny rasterizer. For every pixel we measure the distance to the
//! wave and colour the pixel by how close it is (closer than half the stroke = inside,
//! with a one-pixel soft edge for smoothness).

/// What to draw.
#[derive(Clone, Copy)]
pub enum Style {
    /// The app icon: sage mark on a dark rounded square. Drawn by `build.rs`; the
    /// app itself only uses it in tests.
    #[allow(dead_code)]
    App,
    /// The tray glyph: the mark alone on transparent, in the given colour.
    Tray([u8; 3]),
    /// The tray glyph while muted: the wave fully flat, crossed by a short slash.
    TrayMuted([u8; 3]),
}

const BG: [u8; 3] = [0x1C, 0x1B, 0x1A];
const SAGE: [u8; 3] = [0x8F, 0xA8, 0x9A];

type P = (f32, f32);

/// The wave as cubic Béziers in the SVG's 24-unit space, then the flat line.
/// Small sizes get a single oscillation so the shape stays readable.
fn curves(simple: bool) -> (Vec<[P; 4]>, P) {
    if simple {
        let c = vec![[(2.0, 12.0), (4.2, 3.5), (6.2, 20.5), (9.0, 12.0)]];
        (c, (22.0, 12.0))
    } else {
        // M2 12 c1.2-5 2.4-5 3.6 0 s2.4 5 3.6 0 1.8-2.5 2.6 0 1.4 1.2 2.2 0 H22
        let c = vec![
            [(2.0, 12.0), (3.2, 7.0), (4.4, 7.0), (5.6, 12.0)],
            [(5.6, 12.0), (6.8, 17.0), (8.0, 17.0), (9.2, 12.0)],
            [(9.2, 12.0), (10.4, 7.0), (11.0, 9.5), (11.8, 12.0)],
            [(11.8, 12.0), (12.6, 14.5), (13.2, 13.2), (14.0, 12.0)],
        ];
        (c, (22.0, 12.0))
    }
}

/// The wave as a polyline (each curve cut into short straight pieces).
fn polyline(simple: bool) -> Vec<P> {
    let (curves, end) = curves(simple);
    let mut pts = vec![curves[0][0]];
    for [a, b, c, d] in curves {
        for i in 1..=24 {
            let t = i as f32 / 24.0;
            let u = 1.0 - t;
            let w = [u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t];
            pts.push((
                w[0] * a.0 + w[1] * b.0 + w[2] * c.0 + w[3] * d.0,
                w[0] * a.1 + w[1] * b.1 + w[2] * c.1 + w[3] * d.1,
            ));
        }
    }
    pts.push(end);
    pts
}

fn dist_to_segment(p: P, a: P, b: P) -> f32 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len2 = dx * dx + dy * dy;
    let t = if len2 > 0.0 {
        (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / len2).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let (x, y) = (a.0 + t * dx - p.0, a.1 + t * dy - p.1);
    (x * x + y * y).sqrt()
}

/// Signed distance from `p` to a rounded square covering the canvas (negative inside).
fn rounded_square(p: P, size: f32, radius: f32) -> f32 {
    let h = size / 2.0;
    let q = (
        (p.0 - h).abs() - (h - radius),
        (p.1 - h).abs() - (h - radius),
    );
    let outside = (q.0.max(0.0).powi(2) + q.1.max(0.0).powi(2)).sqrt();
    outside + q.0.max(q.1).min(0.0) - radius
}

/// Draw the mark as `size`×`size` RGBA pixels (straight alpha, row by row from the top).
pub fn render(size: u32, style: Style) -> Vec<u8> {
    let n = size as f32;
    let simple = size <= 24;
    // How much of the width the mark spans (the spec asks ~60% on the app icon; small
    // sizes get a little more so the wave stays visible).
    let span = match style {
        Style::App if simple => 0.74,
        Style::App => 0.60,
        Style::Tray(_) | Style::TrayMuted(_) => 0.92,
    };
    // 20 units (x 2..22) plus the round caps map onto `span` of the canvas.
    let scale = n * span / 22.0;
    let stroke = (2.0 * scale).max(if simple { 1.7 } else { 0.0 });
    let half = stroke / 2.0;
    let to_px = |(x, y): P| (n / 2.0 + (x - 12.0) * scale, n / 2.0 + (y - 12.0) * scale);
    let lines: Vec<Vec<P>> = match style {
        Style::TrayMuted(_) => vec![
            vec![(2.0, 12.0), (22.0, 12.0)],
            vec![(7.5, 19.0), (16.5, 5.0)],
        ],
        _ => vec![polyline(simple)],
    }
    .into_iter()
    .map(|l| l.into_iter().map(to_px).collect())
    .collect();
    let (ink, bg) = match style {
        Style::App => (SAGE, Some(BG)),
        Style::Tray(c) | Style::TrayMuted(c) => (c, None),
    };

    let mut out = vec![0u8; (size * size * 4) as usize];
    for y in 0..size {
        for x in 0..size {
            let p = (x as f32 + 0.5, y as f32 + 0.5);
            // Background coverage (soft one-pixel edge).
            let bg_a = match bg {
                Some(_) => (0.5 - rounded_square(p, n, n * 0.22)).clamp(0.0, 1.0),
                None => 0.0,
            };
            // Stroke coverage: distance to the nearest piece of the wave.
            let d = lines
                .iter()
                .flat_map(|l| l.windows(2))
                .map(|s| dist_to_segment(p, s[0], s[1]))
                .fold(f32::MAX, f32::min);
            let ink_a = (half + 0.5 - d).clamp(0.0, 1.0);
            // Stroke over background ("over" compositing, straight alpha).
            let a = ink_a + bg_a * (1.0 - ink_a);
            let i = ((y * size + x) * 4) as usize;
            if a > 0.0 {
                let bgc = bg.unwrap_or([0; 3]);
                for k in 0..3 {
                    let v = (ink[k] as f32 * ink_a + bgc[k] as f32 * bg_a * (1.0 - ink_a)) / a;
                    out[i + k] = v.round() as u8;
                }
            }
            out[i + 3] = (a * 255.0).round() as u8;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alpha(img: &[u8], size: u32, x: u32, y: u32) -> u8 {
        img[((y * size + x) * 4 + 3) as usize]
    }

    #[test]
    fn app_icon_is_a_filled_square_with_the_mark() {
        let img = render(64, Style::App);
        assert_eq!(img.len(), 64 * 64 * 4);
        // Corners are rounded off (transparent); the middle of an edge is filled.
        assert_eq!(alpha(&img, 64, 0, 0), 0);
        assert_eq!(alpha(&img, 64, 32, 0), 255);
        // The flat line on the right is sage, on the centre row.
        let i = ((32 * 64 + 44) * 4) as usize;
        assert_eq!(&img[i..i + 3], &SAGE);
    }

    #[test]
    fn muted_glyph_is_flat_with_a_slash() {
        let img = render(32, Style::TrayMuted([0xE8, 0xE4, 0xDC]));
        // The flat line crosses the left edge area, where the normal wave would be
        // above or below the centre.
        assert!((4..8).all(|x| alpha(&img, 32, x, 16) == 255));
        // The slash reaches well above the line.
        assert!((0..32).any(|x| alpha(&img, 32, x, 8) > 200));
    }

    #[test]
    fn tray_glyph_is_transparent_around_the_mark() {
        let img = render(32, Style::Tray([0xE8, 0xE4, 0xDC]));
        assert_eq!(alpha(&img, 32, 0, 0), 0);
        assert_eq!(alpha(&img, 32, 16, 31), 0);
        // Somewhere on the flat line there is solid ink.
        assert!((20..30).any(|x| alpha(&img, 32, x, 16) == 255));
    }
}
