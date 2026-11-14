---
name: sotto-release
description: Sotto release checklist — version bump, changelog, git tag, GitHub Actions Windows build, release publication, SHA-256 hashes, signature verification for self_update, cargo-about third-party licenses, and README checks. Load whenever preparing, tagging, or verifying a release, or editing the release workflow.
---

# Sotto release checklist

Releases are published only from github.com/hamadsh2/Sotto via GitHub Actions on a
version tag. Never build a release locally and upload it by hand. Never push a tag or
publish without the user's explicit go-ahead.

## 0. Preconditions
- [ ] On `main`, working tree clean, up to date with `origin/main`.
- [ ] `cargo fmt --all -- --check` passes.
- [ ] `cargo clippy --all-targets -- -D warnings` passes.
- [ ] `cargo test` passes.
- [ ] `audio-safety-reviewer` has reviewed all audio changes since the last release
      with no open findings.
- [ ] `perf-auditor` numbers for this build are recorded and meet targets (or the user
      has accepted the deviation).

## 1. Version bump
- [ ] Choose SemVer: patch (fixes), minor (features), major (breaking settings/behavior).
- [ ] Update `version` in `Cargo.toml`; run `cargo build` so `Cargo.lock` updates.
- [ ] Update installer metadata version if it's stored separately.
- [ ] Add a `CHANGELOG.md` entry: date, Added / Changed / Fixed, user-facing wording.
- [ ] Commit: `Release vX.Y.Z`.

## 2. Third-party licenses
- [ ] `cargo about generate about.hbs > THIRD_PARTY_LICENSES.html` (template + `about.toml`
      in repo). Confirm the About screen embeds/loads the regenerated file.
- [ ] `about.toml` `accepted` list contains only GPL-3.0-compatible licenses
      (MIT, Apache-2.0, BSD-2/3-Clause, ISC, Zlib, Unicode-DFS/3.0, MPL-2.0, OFL-1.1 for Inter…).
      Any new license → stop and ask the user.
- [ ] Inter font license (SIL OFL 1.1) is included.
- [ ] If DeepFilterNet models are bundled: their license and distribution terms were
      verified and are included.
- [ ] VB-Cable is **not** bundled.

## 3. Release profile sanity
```toml
[profile.release]
opt-level = "z"
lto = true
strip = true
codegen-units = 1
```
- [ ] Unchanged in `Cargo.toml`.

## 4. Tag & CI build
- [ ] User approved tagging.
- [ ] `git tag -a vX.Y.Z -m "Sotto vX.Y.Z"` then `git push origin main vX.Y.Z`.
- [ ] GitHub Actions workflow (triggered on `v*` tags, `windows-latest`):
  - builds `--release` for `x86_64-pc-windows-msvc`,
  - builds the installer,
  - signs release archives for `self_update` verification (key from GitHub Secrets),
  - computes SHA-256 for every asset,
  - creates the GitHub Release with assets + `SHA256SUMS.txt` + signatures.
- [ ] Workflow uses secrets only via `${{ secrets.* }}`; never `echo`es them; no key
      files written outside the runner temp dir; actions pinned to commit SHAs.
- [ ] Watch the run to completion; if it fails, report the real error — do not retag
      without telling the user.

## 5. Verify the published release
- [ ] Download each asset and recompute: `certutil -hashfile <file> SHA256` (or
      `Get-FileHash -Algorithm SHA256`). Must match `SHA256SUMS.txt` and the release notes.
- [ ] Verify the signature with the public key embedded in the app.
- [ ] Install on a clean-ish Windows profile: app starts, finds VB-Cable (or shows the
      install message), no stream opened on any other device.
- [ ] Previous version's updater detects the new release, asks before installing,
      verifies the signature, and installs successfully.

## 6. Release notes
- [ ] Summary of changes from `CHANGELOG.md`.
- [ ] SHA-256 for each asset.
- [ ] Line: "Official builds are published only on this repository's Releases page."
- [ ] Line: "Sotto never streams live to your headphones or speakers (Hear yourself plays a short clip once, only when you click Play), and never changes Windows audio settings."

## 7. README checks
- [ ] What Sotto does (one paragraph + screenshot).
- [ ] VB-Cable setup steps with a link to the official VB-Audio download page.
- [ ] Clear statement: Sotto never opens your headphones/speakers and never changes
      default devices or system audio settings; WASAPI shared mode only.
- [ ] "Official builds only from this repo's Releases" + how to verify SHA-256.
- [ ] Privacy: no telemetry; only network call is the update check.
- [ ] License: GPL-3.0-only; third-party licenses available in the About screen.
- [ ] Screenshot and version references are current.

## Never
- Commit `.env`, `*.key`, `*.pem`, `*.pfx`, `*.p12`, minisign/zipsign keys, or any secret.
- Publish a release without hashes.
- Ship a build whose updater auto-installs without user approval.
