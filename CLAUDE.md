# Sotto — "Just your voice."

Lightweight Windows desktop app that cleans the user's microphone in real time and
writes the result to VB-Cable ("CABLE Input"), which the user selects as their mic in
Discord, games, and calls. It must stay extremely light while gaming: CPU-only
processing, zero GPU usage while the window is hidden.

Repo: github.com/hamadsh2/Sotto (public). License: GPL-3.0-only.

## Working rules (always follow)
- Work in phases. **Stop after each phase** so the user can test. Never start the next
  phase unprompted.
- Before writing code in a phase, show a short plan and wait for a go-ahead.
- Explain each module as you write it. The user comes from Python, not Rust: relate
  ownership, traits, `Arc`/`Mutex`, threads, and `Result` to Python concepts when useful.
- **Ask before adding any crate not listed under "Tech stack".** Include why, its
  license, and its size/dependency weight.
- Never claim something works without testing it. Report real measured numbers, never
  estimates presented as measurements.
- After **every** change touching audio code (`cpal`, devices, streams, the engine,
  DSP, denoisers), run the `audio-safety-reviewer` subagent before reporting done.
- At the end of **every phase**, run the `perf-auditor` subagent and report its numbers
  against the performance targets.
- Load the `sotto-design` skill for any UI, icon, logo, or README-visual work. When the
  user pastes window screenshots, review them against that skill and propose concrete
  fixes (token, spacing, or component names — not vague advice).
- Load the `sotto-release` skill for anything about versions, tags, CI, or releases.
- Every source file starts with `// SPDX-License-Identifier: GPL-3.0-only`.
- If GPUI proves unstable on Windows, **stop and tell the user** before switching to Slint.

## Phases
1. **Core audio engine (CLI, no UI):** capture → RNNoise → VB-Cable, obeying the audio
   isolation rules. Print per-frame processing time and process CPU usage. The user
   tests it while gaming before we continue.
2. **Processing chain:** DeepFilterNet (lazy-loaded), high-pass filter, noise gate,
   auto gain + limiter, glitch-free engine switching.
3. **UI:** GPUI window, tray, settings persistence, mute hotkey, branding.
4. **Distribution:** updater, GitHub Actions, licenses page, README.

## Audio isolation rules (STRICT — never violate)
- The app touches exactly **two devices**: the selected input mic and the selected
  output (VB-Cable "CABLE Input"). Nothing else.
- Never open any stream on the user's headphones, speakers, or DAC. No monitor /
  playback / "listen to yourself" feature of any kind.
- Never change Windows default playback or recording devices; never modify any system
  audio setting (volume, format, enhancements, exclusive-mode flags, etc.).
- **WASAPI shared mode only**, never exclusive mode, so other apps can still use the mic.
- The app creates no virtual audio devices of its own.
- If CABLE Input is the Windows default playback device, show a calm warning explaining
  how to fix it manually — never change it automatically.
- Implementation consequences:
  - Open streams only on devices the user explicitly selected (matched by name/ID).
    Never call `default_output_device()` to obtain a device to open a stream on.
    Reading the default device *only to compare* (for the warning above) is allowed.
  - Enumerating devices is fine; opening a stream is not, except on the two chosen ones.
  - If the selected output is missing, stop output and report — never fall back to
    another device.

## Tech stack
Only these crates without asking:
- Audio: `cpal` (WASAPI backend), `ringbuf` (lock-free SPSC between input/output threads)
- Denoise: `nnnoiseless` (RNNoise port), DeepFilterNet Rust implementation (`deep_filter`
  / libDF — license and model terms must be verified before bundling)
- UI: `gpui`, `gpui-component`, `global-hotkey`, `tray-icon`
- Settings: `directories`, `serde`, `toml`
- Updates: `self_update`
- Tooling (not linked): `cargo-about` for third-party licenses

Release profile (`Cargo.toml`):
```toml
[profile.release]
opt-level = "z"
lto = true
strip = true
codegen-units = 1
```

## Audio engine
- Dedicated audio thread(s), fully independent of the UI. UI talks to the engine via
  lock-free/atomic parameters or a small command channel — never locks on the audio path.
- 48 kHz mono internally, 480-sample frames (10 ms). Resample only if a device requires it.
- No allocation, no locking, no logging, no I/O inside audio callbacks.
- Ring buffer between capture and render; keep it bounded and reset it to avoid
  accumulated latency (drop oldest rather than grow).
- Device disconnect/reconnect must be handled gracefully — never panic, never crash.
  Use `Result` everywhere on device paths; no `unwrap()` there.

## Processing chain (exact order)
`High-pass (~80 Hz) → Denoise engine → EQ → Noise gate → Auto gain + limiter → output`
- Every stage is independently toggleable.
- EQ (added Phase 2c, user-approved): 3 bands — body (low shelf ~150 Hz), mud (peak
  ~350 Hz), presence (peak ~4 kHz); presets Natural (flat, default) / Warm / Clear /
  Broadcast at ±3 dB max, plus Custom (±6 dB per band, user-adjustable). After denoise
  (the model sees the natural mic) and before AGC (which evens out preset loudness).
- Noise gate has a threshold parameter and always comes after denoise.
- Limiter is always last.
- DSP stages (HPF, EQ, gate, AGC, limiter) must cost near-zero CPU (simple biquads /
  envelope followers, no FFTs).

## Denoise engines
- Common `Denoiser` trait so engines are swappable and extensible.
- **nnnoiseless (RNNoise)** — default, labeled "Light — for gaming".
- **DeepFilterNet** — labeled "High quality — for recording & calls". Lazy-load only
  when selected; drop it from memory when switching away.
- **Off** — passthrough for before/after comparison.
- Strength slider = wet/dry mix.
- Runtime switching must not glitch: 20–50 ms crossfade plus buffer reset.
- Use frame-size adapters where engine frame sizes differ from 480.

## UI behavior (not visual design — see `sotto-design` skill for that)
- GPUI + gpui-component, English only.
- Compact single window: engine selector, strength slider, input/output level meters,
  device pickers (input mic + output VB-Cable), chain-stage toggles, live CPU/latency.
- Global mute hotkey (`global-hotkey`), configurable, with a subtle on-screen state.
- Closing the window hides to tray (`tray-icon`) without stopping audio. Recreate the
  window only when opened from the tray.
- No redraws while hidden; meters animate only while visible.
- "Start with Windows": optional, off by default.
- Settings persisted as TOML in `%APPDATA%\Sotto`.

## VB-Cable
- Detect whether VB-Cable is installed (a "CABLE Input" render device exists). If not,
  show a clear message linking to the official VB-Audio download page.
- Never bundle VB-Cable.

## Privacy
- No network calls except the update check.
- No analytics, no telemetry, no crash reporting to remote servers.

## Distribution
- Updates via `self_update` against this repo's GitHub Releases: check only; install
  only after explicit user approval; verify release signatures.
- GitHub Actions builds the Windows installer on version tags and publishes the release.
- Signing keys live only in GitHub Secrets — never committed, never printed in logs.
- README: what Sotto does, VB-Cable setup, a clear statement that Sotto never touches
  headphones or system audio settings, official builds only from this repo's Releases,
  SHA-256 hashes per release.

## Licensing
- Project: GPL-3.0-only (`LICENSE`).
- SPDX header on every source file.
- Third-party licenses generated with `cargo-about`, shown in an About screen.
- Verify DeepFilterNet's code license and model distribution terms before bundling models.

## Performance targets
| Metric | Target |
|---|---|
| "Light" mode (RNNoise) RAM | < ~15 MB |
| "Light" mode CPU | < 1% |
| GPU while window hidden | 0% |
| Per-frame processing time | well under 10 ms (report p50/p99/max) |

Measure and report actual numbers at the end of each phase (via `perf-auditor`).

## Commands
- Build: `cargo build --release`
- Lint: `cargo clippy --all-targets -- -D warnings`
- Format: `cargo fmt --all`
- Test: `cargo test`
