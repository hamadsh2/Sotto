// SPDX-License-Identifier: GPL-3.0-only
//! Sotto's audio engine as a library, shared by the window app (`src/main.rs`) and the
//! command-line tool (`src/bin/sotto-cli.rs`).

pub mod control;
pub mod denoise;
pub mod devices;
pub mod dfn;
pub mod dsp;
pub mod engine;
pub mod eq;
pub mod instance;
pub mod params;
pub mod settings;
pub mod stats;
