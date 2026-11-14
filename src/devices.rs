// SPDX-License-Identifier: GPL-3.0-only
//! Finding audio devices — and refusing anything that isn't explicitly chosen.
//!
//! Audio isolation rules enforced here:
//! - Devices come only from *enumeration*. We never open a stream on
//!   `default_input_device()` / `default_output_device()`: in cpal those handles follow
//!   whatever Windows' default is, which could be the user's headphones.
//! - The output must be a VB-Cable endpoint ("CABLE … (VB-Audio Virtual Cable)" or the
//!   A/B/C/D variants). Every other output — speakers, headphones, Voicemeeter strips
//!   that could route to headphones — is refused.
//! - After startup, devices are found again only by their exact Windows endpoint ID.
//!   If one is missing we return an error. There is no fallback device.
//!
//! Python analogy: `Result<T, String>` is like a function that either returns a value or
//! raises an exception — but the caller is *forced* to handle the error case (with `?`,
//! which works like "re-raise to my caller").

use std::panic::{AssertUnwindSafe, catch_unwind};

use cpal::traits::{DeviceTrait, HostTrait};
use cpal::{Device, DeviceId, Host};

pub const VB_CABLE_URL: &str = "https://vb-audio.com/Cable/";
/// The render endpoint VB-Cable creates; this is where Sotto writes clean audio.
const CABLE_INPUT: &str = "CABLE Input";

/// A device together with the name and ID we read from it once.
pub struct Named {
    pub name: String,
    pub id: DeviceId,
    pub device: Device,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Input,
    Output,
}

/// True only for VB-Cable endpoints: "CABLE Input (VB-Audio Virtual Cable)",
/// "CABLE In 16ch (VB-Audio Virtual Cable)", "CABLE-A Input (VB-Audio Cable A)", …
pub fn is_vb_cable(name: &str) -> bool {
    name.starts_with("CABLE")
        && (name.contains("(VB-Audio Virtual Cable)") || name.contains("(VB-Audio Cable "))
}

/// Run `f`, turning a panic into an error. cpal's WASAPI enumeration uses `.expect()`
/// internally and can panic if a device vanishes mid-enumeration; we must not crash.
pub fn no_panic<T>(what: &str, f: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    catch_unwind(AssertUnwindSafe(f))
        .unwrap_or_else(|_| Err(format!("{what}: device list changed while reading it")))
}

/// List devices in one direction. Devices whose name or ID can't be read (e.g. being
/// unplugged right now) are skipped instead of failing.
pub fn list(host: &Host, dir: Dir) -> Result<Vec<Named>, String> {
    no_panic("listing devices", || {
        let devices: Vec<Device> = match dir {
            Dir::Input => host.input_devices().map_err(|e| e.to_string())?.collect(),
            Dir::Output => host.output_devices().map_err(|e| e.to_string())?.collect(),
        };
        Ok(devices
            .into_iter()
            .filter_map(|device| {
                let name = device.description().ok()?.name().to_owned();
                let id = device.id().ok()?;
                Some(Named { name, id, device })
            })
            .collect())
    })
}

/// Pick exactly one device: an exact (case-insensitive) name match, or else the single
/// device whose name contains `query`. Ambiguous or missing → error, never a guess.
fn pick(mut devices: Vec<Named>, query: &str, kind: &str) -> Result<Named, String> {
    let q = query.to_lowercase();
    let quoted = |idx: &[usize], devs: &[Named]| {
        idx.iter()
            .map(|&i| format!("\"{}\"", devs[i].name))
            .collect::<Vec<_>>()
            .join(", ")
    };

    let exact: Vec<usize> = (0..devices.len())
        .filter(|&i| devices[i].name.to_lowercase() == q)
        .collect();
    let candidates = if exact.is_empty() {
        (0..devices.len())
            .filter(|&i| devices[i].name.to_lowercase().contains(&q))
            .collect()
    } else {
        exact
    };
    match candidates.as_slice() {
        &[i] => Ok(devices.swap_remove(i)),
        [] => Err(format!(
            "no {kind} device matches \"{query}\". Run `sotto --list` to see device names."
        )),
        many => Err(format!(
            "\"{query}\" matches {} {kind} devices: {}. Use the full name.",
            many.len(),
            quoted(many, &devices)
        )),
    }
}

/// What the user may pick: microphones (never VB-Cable's own "CABLE Output"), or
/// VB-Cable render endpoints only. Listing opens nothing.
pub fn choices(host: &Host, dir: Dir) -> Result<Vec<Named>, String> {
    Ok(list(host, dir)?
        .into_iter()
        .filter(|d| is_vb_cable(&d.name) == (dir == Dir::Output))
        .collect())
}

pub fn find_input(host: &Host, query: &str) -> Result<Named, String> {
    pick(choices(host, Dir::Input)?, query, "microphone")
}

/// Find the output. Only VB-Cable endpoints are ever considered.
/// With no `query`, picks the standard "CABLE Input" endpoint.
pub fn find_output(host: &Host, query: Option<&str>) -> Result<Named, String> {
    let cables: Vec<Named> = list(host, Dir::Output)?
        .into_iter()
        .filter(|d| is_vb_cable(&d.name))
        .collect();

    if cables.is_empty() {
        return Err(format!(
            "VB-Cable is not installed (no \"CABLE Input\" device found).\n\
             1. On {VB_CABLE_URL} download the \"New Package\" zip \
             (VBCABLE_Driver_Pack46 or newer), not the old one.\n\
             2. Extract all the files.\n\
             3. Right-click VBCABLE_Setup_x64.exe, Run as administrator, Install Driver.\n\
             4. Restart Windows and run Sotto again."
        ));
    }
    pick(cables, query.unwrap_or(CABLE_INPUT), "VB-Cable output").map_err(|e| {
        format!("{e}\n(Sotto only outputs to VB-Cable — never to speakers or headphones.)")
    })
}

/// Find the device the user chose, among what may be chosen for `dir` (so an output is
/// always a VB-Cable endpoint): by its exact endpoint ID, or — if Windows gave it a new
/// ID, e.g. after moving the mic to another USB port — by its exact full name. Never a
/// partial name, never some other device.
pub fn find_chosen(host: &Host, dir: Dir, id: &DeviceId, name: &str) -> Result<Named, String> {
    let mut found = choices(host, dir)?;
    let keys: Vec<(String, &str)> = found
        .iter()
        .map(|d| (d.id.to_string(), d.name.as_str()))
        .collect();
    match pick_saved(&keys, &id.to_string(), name) {
        Some(i) => Ok(found.swap_remove(i)),
        None => Err(format!("\"{name}\" is not connected")),
    }
}

/// The matching rule of `find_chosen`, on plain `(id, name)` pairs so it can be tested
/// without audio hardware. A name match counts only if exactly one device has that
/// full name (two identical names would be a guess).
pub fn pick_saved(devices: &[(String, &str)], id: &str, name: &str) -> Option<usize> {
    if let Some(i) = devices.iter().position(|(d, _)| d == id) {
        return Some(i);
    }
    let mut same_name = devices
        .iter()
        .enumerate()
        .filter(|(_, (_, n))| !name.is_empty() && *n == name);
    match (same_name.next(), same_name.next()) {
        (Some((i, _)), None) => Some(i),
        _ => None,
    }
}

/// True if Windows' *default playback* device is a VB-Cable endpoint.
///
/// This only reads the default device's name to compare it. No stream is opened on it
/// and nothing is changed.
pub fn default_playback_is_cable(host: &Host) -> bool {
    no_panic("reading default device", || {
        Ok(host
            .default_output_device()
            .and_then(|d| d.description().ok())
            .is_some_and(|desc| is_vb_cable(desc.name())))
    })
    .unwrap_or(false)
}

pub fn print_list(host: &Host) -> Result<(), String> {
    println!("Microphones:");
    for d in list(host, Dir::Input)? {
        if !is_vb_cable(&d.name) {
            println!("  {}", d.name);
        }
    }
    println!("\nVB-Cable outputs (where Sotto can send clean audio):");
    let cables: Vec<Named> = list(host, Dir::Output)?
        .into_iter()
        .filter(|d| is_vb_cable(&d.name))
        .collect();
    if cables.is_empty() {
        println!("  none — install VB-Cable: {VB_CABLE_URL}");
    }
    for d in cables {
        println!("  {}", d.name);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vb_cable_detection() {
        assert!(is_vb_cable("CABLE Input (VB-Audio Virtual Cable)"));
        assert!(is_vb_cable("CABLE In 16ch (VB-Audio Virtual Cable)"));
        assert!(is_vb_cable("CABLE-A Input (VB-Audio Cable A)"));
        assert!(!is_vb_cable("Speakers (TOPPING USB DAC)"));
        assert!(!is_vb_cable("Microphone (fifine Microphone)"));
        // Voicemeeter strips can route to hardware outputs (headphones) — refused.
        assert!(!is_vb_cable(
            "VoiceMeeter Input (VB-Audio VoiceMeeter VAIO)"
        ));
        assert!(!is_vb_cable("Hi-Fi Cable Input (VB-Audio Hi-Fi Cable)"));
    }

    fn devs(list: &[(&str, &'static str)]) -> Vec<(String, &'static str)> {
        list.iter().map(|(i, n)| (i.to_string(), *n)).collect()
    }

    #[test]
    fn saved_device_matches_by_id_first() {
        let d = devs(&[
            ("id-a", "Microphone (fifine Microphone)"),
            ("id-b", "Line (DAC)"),
        ]);
        assert_eq!(
            pick_saved(&d, "id-b", "Microphone (fifine Microphone)"),
            Some(1)
        );
    }

    #[test]
    fn saved_device_falls_back_only_to_the_exact_full_name() {
        let d = devs(&[
            ("id-new", "Microphone (fifine Microphone)"),
            ("id-x", "Line (DAC)"),
        ]);
        // New ID after a USB port change: same full name → found.
        assert_eq!(
            pick_saved(&d, "id-old", "Microphone (fifine Microphone)"),
            Some(0)
        );
        // Partial or different names never match.
        assert_eq!(pick_saved(&d, "id-old", "fifine"), None);
        assert_eq!(
            pick_saved(&d, "id-old", "Microphone (fifine Microphone) "),
            None
        );
        assert_eq!(pick_saved(&d, "id-old", ""), None);
    }

    #[test]
    fn missing_device_is_never_replaced() {
        let d = devs(&[("id-x", "Line (DAC)"), ("id-y", "Microphone (Webcam)")]);
        assert_eq!(
            pick_saved(&d, "id-gone", "Microphone (fifine Microphone)"),
            None
        );
        assert_eq!(
            pick_saved(&[], "id-gone", "Microphone (fifine Microphone)"),
            None
        );
    }

    #[test]
    fn two_devices_with_the_same_name_are_not_guessed() {
        let d = devs(&[
            ("id-1", "Microphone (USB Audio)"),
            ("id-2", "Microphone (USB Audio)"),
        ]);
        assert_eq!(pick_saved(&d, "id-gone", "Microphone (USB Audio)"), None);
        // ...but the exact ID still picks the right one.
        assert_eq!(pick_saved(&d, "id-2", "Microphone (USB Audio)"), Some(1));
    }
}
