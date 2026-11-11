// SPDX-License-Identifier: GPL-3.0-only
//! Updates from this repository's GitHub Releases — the only network use in Sotto.
//!
//! Two separate steps, so nothing is ever installed without the user's click:
//! 1. `check()` asks GitHub for a release newer than this one (a small JSON listing).
//! 2. `install(version)` downloads that release's zip, **verifies its ed25519 signature**
//!    against the public key built into Sotto, and only then swaps the new `sotto.exe`
//!    in place of the running one (Windows allows renaming a running exe; the new one
//!    starts on the next launch). A bad or missing signature stops before anything is
//!    unpacked.
//!
//! Both block on the network, so the window runs them on a background thread; they never
//! touch the audio.
//!
//! Release files: `sotto-<version>-windows-x64.zip` (signed with `zipsign`, holding
//! `sotto.exe`), plus the installer and `SHA256SUMS.txt`.

use std::time::Duration;

use self_update::backends::github::Update;

const OWNER: &str = "hamadsh2";
const REPO: &str = "Sotto";
/// The part of the zip's name that marks it as the updater's file.
const TARGET: &str = "windows-x64";
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Sotto's update-signing public key (ed25519, 32 bytes). The private half lives only in
/// GitHub Secrets and offline with the maintainer.
const KEY: self_update::VerifyingKey = *include_bytes!("../../assets/update/sotto-update.pub");

const TIMEOUT: Duration = Duration::from_secs(30);

pub fn current_version() -> &'static str {
    VERSION
}

fn configure() -> Result<Update, String> {
    Update::configure()
        .repo_owner(OWNER)
        .repo_name(REPO)
        .bin_name("sotto")
        .target(TARGET)
        .current_version(VERSION)
        .timeout(TIMEOUT)
        .show_output(false)
        .show_download_progress(false)
        .no_confirm(true)
        // An empty key list would silently skip the check; ours is always there.
        .verifying_keys(vec![KEY])
        .build()
        .map_err(|e| friendly(&e))
}

/// The newest release that is newer than this Sotto and has a Windows zip, if any.
pub fn check() -> Result<Option<String>, String> {
    let releases = match configure()?.get_newer_releases() {
        Ok(r) => r,
        // No releases published yet: nothing newer.
        Err(
            self_update::errors::Error::NoReleaseFound { .. }
            | self_update::errors::Error::NotFound { .. },
        ) => return Ok(None),
        Err(e) => return Err(friendly(&e)),
    };
    Ok(releases
        .into_vec()
        .into_iter()
        .find(|r| r.has_target_asset(TARGET))
        .map(|r| r.version().to_string()))
}

/// Download, verify and put `version` in place. Takes effect after a restart.
pub fn install(version: &str) -> Result<(), String> {
    let mut u = Update::configure();
    u.repo_owner(OWNER)
        .repo_name(REPO)
        .bin_name("sotto")
        .target(TARGET)
        .current_version(VERSION)
        .release_tag(format!("v{version}"))
        .timeout(TIMEOUT)
        .show_output(false)
        .show_download_progress(false)
        .no_confirm(true)
        .verifying_keys(vec![KEY]);
    u.build()
        .map_err(|e| friendly(&e))?
        .update()
        .map(|_| ())
        .map_err(|e| friendly(&e))
}

/// One calm sentence instead of a library error chain.
fn friendly(e: &self_update::errors::Error) -> String {
    use self_update::errors::Error;
    match e {
        Error::Signature(_) | Error::SignatureNonUTF8 | Error::NoSignatures(_) => {
            "The download failed its signature check, so nothing was installed.".into()
        }
        Error::Transport(_) | Error::HttpStatus { .. } => {
            "Couldn't reach GitHub. Check your connection and try again.".into()
        }
        Error::RateLimited { .. } => "GitHub asked to wait a while. Try again later.".into(),
        Error::InstallPathNotWritable { .. } | Error::Io(_) => {
            format!("Couldn't put the new version in place: {e}")
        }
        _ => format!("Update failed: {e}"),
    }
}

/// Start the new `sotto.exe` after this one has quit (used by "Restart" after an update).
pub fn relaunch() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::process::Command::new(exe).spawn();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    fn fixture(kind: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/update")
            .join(format!("sotto-0.0.0-windows-x64-{kind}.zip"))
    }

    fn verify(path: &Path) -> bool {
        self_update::verify_signature(path, &[KEY]).is_ok()
    }

    #[test]
    fn the_built_in_key_is_a_real_key() {
        assert_eq!(KEY.len(), 32);
        assert!(KEY.iter().any(|&b| b != 0));
    }

    #[test]
    fn a_zip_signed_with_sottos_key_passes() {
        assert!(verify(&fixture("good")));
    }

    #[test]
    fn unsigned_or_wrongly_signed_zips_are_refused() {
        assert!(!verify(&fixture("unsigned")));
        assert!(!verify(&fixture("wrongkey")));
    }

    #[test]
    fn a_renamed_signed_zip_is_refused() {
        // The signature covers the file name too, so a release can't be passed off as
        // another one.
        let dir = std::env::temp_dir().join(format!("sotto-update-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let renamed = dir.join("sotto-9.9.9-windows-x64.zip");
        std::fs::copy(fixture("good"), &renamed).unwrap();
        let ok = verify(&renamed);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(!ok);
    }
}
