// SPDX-License-Identifier: GPL-3.0-only
//! The few files the window draws (icons, the Sotto mark), built into the binary so
//! nothing is read from disk at runtime.
//!
//! GPUI asks an `AssetSource` for files by path (e.g. gpui-component's dropdown asks for
//! "icons/chevron-down.svg"). Python analogy: a dict `{path: bytes}` filled at build
//! time by `include_bytes!` and looked up on demand.
//!
//! Icons are Lucide (ISC license; notice in `assets/icons/LICENSE-lucide.txt`).

use std::borrow::Cow;

use gpui::{AssetSource, Result, SharedString};

/// The Sotto mark (a wave calming into a line).
pub const MARK: &str = "icon/sotto-mark.svg";
pub const MIC: &str = "icons/mic.svg";
pub const PLUG: &str = "icons/plug.svg";

const FILES: &[(&str, &[u8])] = &[
    (MARK, include_bytes!("../../assets/icon/sotto-mark.svg")),
    (MIC, include_bytes!("../../assets/icons/mic.svg")),
    (PLUG, include_bytes!("../../assets/icons/plug.svg")),
    // Used by gpui-component itself (dropdown arrow, selected item).
    (
        "icons/chevron-down.svg",
        include_bytes!("../../assets/icons/chevron-down.svg"),
    ),
    (
        "icons/check.svg",
        include_bytes!("../../assets/icons/check.svg"),
    ),
];

pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        // An unknown path just draws nothing, the same as before icons existed.
        Ok(FILES
            .iter()
            .find(|(p, _)| *p == path)
            .map(|(_, bytes)| Cow::Borrowed(*bytes)))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        Ok(FILES
            .iter()
            .filter(|(p, _)| p.starts_with(path))
            .map(|(p, _)| SharedString::from(*p))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_icon_is_an_svg() {
        for (path, bytes) in FILES {
            let text = std::str::from_utf8(bytes).unwrap();
            assert!(text.trim_start().starts_with("<svg"), "{path}");
            assert!(Assets.load(path).unwrap().is_some());
        }
        assert!(Assets.load("icons/nope.svg").unwrap().is_none());
    }
}
