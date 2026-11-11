// SPDX-License-Identifier: GPL-3.0-only
//! Build step: give `sotto.exe` its icon.
//!
//! Draws the app icon at every size Windows asks for (with the same code the tray uses,
//! `src/ui/mark.rs`), packs the images into a Windows resource file (`.res`) and hands
//! it to the MSVC linker. GPUI loads icon #1 from the exe for its window, so the window,
//! the taskbar, Alt+Tab and Explorer all show it. No resource compiler or extra crate
//! is needed: the `.res` format is a simple list of headers + data, written below.
//!
//! Python analogy: like a `setup.py` step that generates a file before packaging.

use std::path::PathBuf;

#[allow(dead_code)]
#[path = "src/ui/mark.rs"]
mod mark;

/// Sizes from the design spec.
const SIZES: [u32; 9] = [16, 20, 24, 32, 40, 48, 64, 128, 256];
const RT_ICON: u16 = 3;
const RT_GROUP_ICON: u16 = 14;
/// GPUI loads the icon group with this ID.
const GROUP_ID: u16 = 1;
const LANG_EN_US: u16 = 0x0409;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/ui/mark.rs");
    let windows = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows");
    let msvc = std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc");
    if !(windows && msvc) {
        return;
    }
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("cargo sets OUT_DIR"));
    let res = out.join("sotto-icon.res");
    std::fs::write(&res, icon_res()).expect("write icon resource");
    // link.exe accepts .res files as inputs. Only the window app gets the icon.
    println!("cargo:rustc-link-arg-bin=sotto={}", res.display());
}

/// One icon image in the classic DIB format: a BITMAPINFOHEADER (height doubled, as
/// icons require), 32-bit BGRA pixels bottom-up, then a 1-bit AND mask (all zero; the
/// alpha channel does the work).
fn dib(size: u32) -> Vec<u8> {
    let rgba = mark::render(size, mark::Style::App);
    let mask_row = size.div_ceil(32) * 4;
    let mut v = Vec::new();
    for x in [40u32, size, size * 2] {
        v.extend_from_slice(&x.to_le_bytes());
    }
    v.extend_from_slice(&1u16.to_le_bytes()); // planes
    v.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
    for _ in 0..6 {
        v.extend_from_slice(&0u32.to_le_bytes()); // no compression, sizes, colours
    }
    for y in (0..size).rev() {
        for x in 0..size {
            let i = ((y * size + x) * 4) as usize;
            v.extend_from_slice(&[rgba[i + 2], rgba[i + 1], rgba[i], rgba[i + 3]]);
        }
    }
    v.resize(v.len() + (mask_row * size) as usize, 0);
    v
}

/// One resource: a 32-byte header (numeric type and ID) followed by the data, padded
/// to 4 bytes.
fn entry(res: &mut Vec<u8>, kind: u16, id: u16, flags: u16, data: &[u8]) {
    res.extend_from_slice(&(data.len() as u32).to_le_bytes());
    res.extend_from_slice(&32u32.to_le_bytes());
    for w in [0xFFFF, kind, 0xFFFF, id] {
        res.extend_from_slice(&u16::to_le_bytes(w));
    }
    res.extend_from_slice(&0u32.to_le_bytes()); // data version
    res.extend_from_slice(&flags.to_le_bytes());
    let lang = if kind == 0 { 0 } else { LANG_EN_US };
    res.extend_from_slice(&lang.to_le_bytes());
    res.extend_from_slice(&0u32.to_le_bytes()); // version
    res.extend_from_slice(&0u32.to_le_bytes()); // characteristics
    res.extend_from_slice(data);
    while !res.len().is_multiple_of(4) {
        res.push(0);
    }
}

fn icon_res() -> Vec<u8> {
    let mut res = Vec::new();
    // Every .res file starts with an empty entry.
    entry(&mut res, 0, 0, 0, &[]);
    // The group directory: which images exist and their IDs.
    let mut group = Vec::new();
    for w in [0u16, 1, SIZES.len() as u16] {
        group.extend_from_slice(&w.to_le_bytes());
    }
    for (i, &size) in SIZES.iter().enumerate() {
        let img = dib(size);
        let id = i as u16 + 1;
        entry(&mut res, RT_ICON, id, 0x1010, &img);
        let b = if size >= 256 { 0 } else { size as u8 }; // 0 means 256
        group.extend_from_slice(&[b, b, 0, 0]);
        group.extend_from_slice(&1u16.to_le_bytes()); // planes
        group.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
        group.extend_from_slice(&(img.len() as u32).to_le_bytes());
        group.extend_from_slice(&id.to_le_bytes());
    }
    entry(&mut res, RT_GROUP_ICON, GROUP_ID, 0x1030, &group);
    res
}
