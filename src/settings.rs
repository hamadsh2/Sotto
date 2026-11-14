// SPDX-License-Identifier: GPL-3.0-only
//! The user's saved choices, as a small TOML file in `%APPDATA%\Sotto\settings.toml`.
//!
//! - Loading never fails: a missing file means defaults; a broken one is kept aside as
//!   `settings.toml.bad` and defaults are used, with a note for the window.
//! - Every value read from the file goes through the same clamping setters the window
//!   uses, so a hand-edited file can never push the audio out of its safe ranges.
//! - Saving writes a temporary file and then swaps it in, so a crash or power cut mid-
//!   write leaves the previous file intact.
//!
//! Python analogy: a `@dataclass` turned into TOML and back by `serde` (like `tomllib`
//! plus a writer), with defaults for any missing field.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering::Relaxed;

use serde::{Deserialize, Serialize};

use crate::control::EngineKind;
use crate::params::{EqPreset, Params};

/// The mute shortcut until the user picks another.
pub const MUTE_HOTKEY_DEFAULT: &str = "Ctrl+Shift+F9";

/// A device the user chose: its Windows endpoint ID and its full name. The name is used
/// only if the ID changed (e.g. the mic moved to another USB port) and must match
/// exactly — never a partial match, never another device.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
pub struct SavedDevice {
    pub id: String,
    pub name: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Settings {
    pub input: Option<SavedDevice>,
    pub output: Option<SavedDevice>,
    /// "rnnoise", "deepfilternet" or "off".
    pub engine: String,
    /// 0..=1.
    pub strength: f32,
    pub hpf: bool,
    pub eq: bool,
    /// "natural", "warm", "clear", "broadcast" or "custom".
    pub eq_preset: String,
    /// Body, mud, presence in dB, used when the preset is "custom".
    pub eq_custom: [f32; 3],
    pub gate: bool,
    pub gate_db: f32,
    pub agc: bool,
    pub agc_max_db: f32,
    pub limiter: bool,
    pub dfn_postfilter: bool,
    /// "Hide my voice from screen shares" (exclusive CABLE Input). On by default.
    pub hide_from_share: bool,
    pub start_with_windows: bool,
    /// Global mute shortcut as text, e.g. "Ctrl+Alt+M"; empty = none.
    pub mute_hotkey: String,
    /// Look for a new Sotto at startup and once a day (installing always asks first).
    pub check_updates: bool,
}

impl Default for Settings {
    fn default() -> Self {
        let p = Params::new();
        let mut s = Self {
            input: None,
            output: None,
            engine: EngineKind::Rnnoise.name().into(),
            strength: 0.0,
            hpf: false,
            eq: false,
            eq_preset: String::new(),
            eq_custom: [0.0; 3],
            gate: false,
            gate_db: 0.0,
            agc: false,
            agc_max_db: 0.0,
            limiter: false,
            dfn_postfilter: false,
            hide_from_share: true,
            start_with_windows: false,
            mute_hotkey: MUTE_HOTKEY_DEFAULT.into(),
            check_updates: true,
        };
        // One source of truth for the defaults: a fresh `Params`.
        s.read_params(&p);
        s
    }
}

impl Settings {
    pub fn engine(&self) -> EngineKind {
        EngineKind::parse(&self.engine).unwrap_or(EngineKind::Rnnoise)
    }

    pub fn eq_preset(&self) -> EqPreset {
        match self.eq_preset.as_str() {
            "custom" => EqPreset::Custom,
            s => EqPreset::parse(s).unwrap_or(EqPreset::Natural),
        }
    }

    /// Copy the live audio settings out of `p` (devices, engine and autostart are
    /// kept as they are). Custom EQ gains are remembered even while a named preset is
    /// selected, so switching back to Custom restores them.
    pub fn read_params(&mut self, p: &Params) {
        self.strength = p.strength();
        self.hpf = p.hpf.load(Relaxed);
        self.eq = p.eq.load(Relaxed);
        let preset = p.eq_preset();
        self.eq_preset = preset.name().into();
        if preset == EqPreset::Custom {
            self.eq_custom = p.eq_gains();
        }
        self.gate = p.gate.load(Relaxed);
        self.gate_db = p.gate_threshold_db();
        self.agc = p.agc.load(Relaxed);
        self.agc_max_db = p.agc_max_db();
        self.limiter = p.limiter.load(Relaxed);
        self.dfn_postfilter = p.dfn_postfilter.load(Relaxed);
        self.hide_from_share = p.hide_from_share.load(Relaxed);
    }

    /// Put the saved values into `p`, through the clamping setters.
    pub fn apply(&self, p: &Params) {
        p.set_strength(self.strength);
        p.hpf.store(self.hpf, Relaxed);
        p.eq.store(self.eq, Relaxed);
        match self.eq_preset() {
            EqPreset::Custom => p.set_eq_custom(self.eq_custom),
            named => p.set_eq_preset(named),
        }
        p.gate.store(self.gate, Relaxed);
        p.set_gate_threshold_db(self.gate_db);
        p.agc.store(self.agc, Relaxed);
        p.set_agc_max_db(self.agc_max_db);
        p.limiter.store(self.limiter, Relaxed);
        p.dfn_postfilter.store(self.dfn_postfilter, Relaxed);
        p.hide_from_share.store(self.hide_from_share, Relaxed);
    }

    /// Parse TOML and clamp every value into its valid range.
    pub fn parse(text: &str) -> Result<Self, String> {
        let raw: Settings = toml::from_str(text).map_err(|e| e.to_string())?;
        // Round-trip through `Params` = the same clamping the live controls use.
        let p = Params::new();
        raw.apply(&p);
        let mut s = raw;
        s.read_params(&p);
        if s.eq_preset() != EqPreset::Custom {
            // Not applied right now, but clamp the remembered custom gains too.
            let c = Params::new();
            c.set_eq_custom(s.eq_custom);
            s.eq_custom = c.eq_gains();
        }
        s.engine = s.engine().name().into();
        Ok(s)
    }

    pub fn to_toml(&self) -> String {
        // Plain data with no maps keyed by non-strings: serializing cannot fail.
        toml::to_string_pretty(self).unwrap_or_default()
    }
}

/// `%APPDATA%\Sotto\settings.toml`, or `None` if Windows doesn't tell us where
/// `%APPDATA%` is (then nothing is saved).
pub fn default_path() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|d| d.config_dir().join("Sotto").join("settings.toml"))
}

pub struct Loaded {
    pub settings: Settings,
    /// Set if the file was unreadable and has been set aside.
    pub note: Option<String>,
}

pub fn load(path: &Path) -> Loaded {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Loaded {
                settings: Settings::default(),
                note: None,
            };
        }
        Err(e) => {
            return Loaded {
                settings: Settings::default(),
                note: Some(format!(
                    "Couldn't read your settings ({e}); using defaults."
                )),
            };
        }
    };
    match Settings::parse(&text) {
        Ok(settings) => Loaded {
            settings,
            note: None,
        },
        Err(_) => {
            let bad = path.with_extension("toml.bad");
            let kept = fs::rename(path, &bad).is_ok();
            Loaded {
                settings: Settings::default(),
                note: Some(if kept {
                    format!(
                        "Your settings file was damaged, so Sotto started with defaults. The old file is kept as {}.",
                        bad.display()
                    )
                } else {
                    "Your settings file was damaged, so Sotto started with defaults.".into()
                }),
            }
        }
    }
}

/// Write to a temporary file next to the real one, then swap it in.
pub fn save(path: &Path, s: &Settings) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("toml.tmp");
    fs::write(&tmp, s.to_toml())?;
    // On Windows this replaces an existing file in one step (MoveFileEx).
    fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sotto-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn defaults_match_a_fresh_engine() {
        let s = Settings::default();
        assert_eq!(s.engine(), EngineKind::Rnnoise);
        assert_eq!(s.strength, 1.0);
        assert_eq!(s.gate_db, crate::params::GATE_DB_DEFAULT);
        assert_eq!(s.agc_max_db, crate::params::AGC_MAX_DB_DEFAULT);
        assert_eq!(s.eq_preset(), EqPreset::Natural);
        assert!(s.hpf && s.eq && s.gate && s.agc && s.limiter);
        assert!(!s.dfn_postfilter && !s.start_with_windows);
        assert_eq!(s.mute_hotkey, MUTE_HOTKEY_DEFAULT);
        assert!(s.check_updates);
        assert!(s.input.is_none() && s.output.is_none());
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = temp_dir("roundtrip");
        let path = dir.join("settings.toml");
        let s = Settings {
            input: Some(SavedDevice {
                id: "wasapi:{0.0.1.00000000}.{abc}".into(),
                name: "Microphone (fifine Microphone)".into(),
            }),
            engine: "deepfilternet".into(),
            strength: 0.75,
            eq_preset: "custom".into(),
            eq_custom: [1.5, -2.0, 3.0],
            gate_db: -52.0,
            agc_max_db: 18.0,
            limiter: false,
            dfn_postfilter: true,
            hide_from_share: false,
            start_with_windows: true,
            mute_hotkey: "Shift+F9".into(),
            check_updates: false,
            ..Settings::default()
        };
        save(&path, &s).unwrap();
        let back = load(&path);
        assert!(back.note.is_none());
        assert_eq!(back.settings, s);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_gives_defaults_quietly() {
        let dir = temp_dir("missing");
        let l = load(&dir.join("settings.toml"));
        assert!(l.note.is_none());
        assert_eq!(l.settings, Settings::default());
    }

    #[test]
    fn damaged_file_is_kept_aside_and_defaults_used() {
        let dir = temp_dir("damaged");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.toml");
        fs::write(&path, "strength = [this is not toml").unwrap();
        let l = load(&path);
        assert_eq!(l.settings, Settings::default());
        assert!(l.note.is_some());
        assert!(dir.join("settings.toml.bad").exists());
        assert!(!path.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn wild_values_are_clamped() {
        let s = Settings::parse(
            "strength = 9.0\ngate_db = -500.0\nagc_max_db = 99.0\n\
             eq_preset = \"custom\"\neq_custom = [40.0, -40.0, 2.0]\n\
             engine = \"turbo\"\n",
        )
        .unwrap();
        assert_eq!(s.strength, 1.0);
        assert_eq!(s.gate_db, crate::params::GATE_DB_MIN);
        assert_eq!(s.agc_max_db, crate::params::AGC_MAX_DB_MAX);
        assert_eq!(s.eq_custom, [6.0, -6.0, 2.0]);
        assert_eq!(s.engine(), EngineKind::Rnnoise);
        // Unknown preset names fall back to Natural.
        let s = Settings::parse("eq_preset = \"loud\"\n").unwrap();
        assert_eq!(s.eq_preset(), EqPreset::Natural);
    }

    #[test]
    fn missing_and_unknown_fields_are_fine() {
        let s = Settings::parse("future_option = 3\nlimiter = false\n").unwrap();
        assert!(!s.limiter);
        assert_eq!(s.strength, 1.0);
    }

    #[test]
    fn apply_sets_the_live_params() {
        let s = Settings {
            strength: 0.4,
            gate: false,
            eq_preset: "clear".into(),
            ..Settings::default()
        };
        let p = Params::new();
        s.apply(&p);
        assert_eq!(p.strength(), 0.4);
        assert!(!p.gate.load(Relaxed));
        assert_eq!(p.eq_preset(), EqPreset::Clear);
        assert_eq!(p.eq_gains(), EqPreset::Clear.gains());
    }
}
