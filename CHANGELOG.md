# Changelog

All notable changes to Sotto. Versions follow [Semantic Versioning](https://semver.org).

## [Unreleased]

### Added
- **Remove headphone echo** (off by default): for open-back headphones, which leak what you
  hear (game, friends, music) into your mic. Pick your headphones and Sotto subtracts that
  leak with WebRTC's AEC3 echo canceller, before noise removal. Sotto only *listens* to what
  Windows plays to those headphones (a read-only loopback); it never plays anything to them,
  never changes their settings, and keeps nothing. Your headphones stay on your own DAC or
  sound card, untouched. Tuned for headphone leak rather than a speakerphone, so it doesn't
  cut your own voice while you talk over the game or your friends (measured: 0.2–0.6 dB of
  your voice touched, where WebRTC's default setting ate up to ~5 dB and whole syllables).
- `sotto-cli` shows, while you talk, how much each stage (echo removal, noise removal,
  EQ/gate/auto gain/limiter) changes your voice, and whether the mic clipped.

### Changed
- **Simpler window.** The main screen now fits without scrolling: noise removal, levels,
  Hear yourself, headphone echo and mute (with its shortcut shown). Devices, the voice
  chain, the mute shortcut and app options moved to a new **Settings** page (bottom right).

## [0.2.0] - 2026-11-14

### Added
- **Tune for my room:** a 10-second check (5 s quiet, 5 s talking) that suggests a noise-gate
  threshold and auto-gain limit for your mic and room. Nothing changes until you click Apply.
- Calm hints when your microphone is set too loud (it clips) or your voice stays very quiet.
- **Hear yourself:** record 5 seconds of your cleaned voice, then play it back once in your
  headphones. It's never live, so your voice is never doubled in a screen share or recording
  while you talk. The clip stays in memory only and is never played to a virtual cable.
- **Hide my voice from screen shares** (on by default): viewers of a Discord whole-screen
  share no longer hear your voice twice. Sotto now writes to CABLE Input in WASAPI
  exclusive mode, which the screen share's audio capture can't see (measured: none of it
  reaches that capture, all of it reaches CABLE Output; confirmed in a real Discord share).
  Turn it off to go back to shared mode, for example if you play a soundboard into CABLE Input.
- `sotto-cli`: `tune`, `tune apply`, `record`, `play` and `hide on|off` commands.

### Changed
- Auto gain aims for a slightly quieter level (−23 dB instead of −20 dB).
- The "VB-Cable isn't installed" message, the CLI and the README now give step-by-step
  install help (download the "New Package" zip, run the setup as administrator, restart).

## [0.1.0] - 2026-11-11

The first release.

### Added
- Real-time noise removal from your microphone to VB-Cable's **CABLE Input**, for
  Discord, games and calls.
- Two engines: **Light** (RNNoise, for gaming) and **High quality** (DeepFilterNet, for
  recording and calls, with an optional post-filter), plus **Off** to compare. Switching
  is glitch-free. A strength slider mixes in your original voice.
- Voice chain, each part optional: low-cut filter, 3-band voice EQ (Natural, Warm,
  Clear, Broadcast or your own), noise gate, auto gain and a limiter. Values can be
  typed or set with sliders.
- **Mute** from anywhere: a global shortcut (default Ctrl+Shift+F9, changeable), the
  window or the tray. The tray icon shows when you're muted.
- Compact window with level meters and a live CPU, latency and memory readout; closing
  it keeps Sotto running in the tray. Optional **Start with Windows**.
- Calm warnings when VB-Cable is missing, a chosen device is unplugged, or CABLE Input
  is the Windows default playback device.
- Signed updates from this repository's GitHub Releases: checked at start and daily
  (can be turned off), installed only when you click Install.
- About page with the version, license and all third-party licenses.
- Per-user installer (no admin rights).

### Privacy and safety
- Sotto opens only the microphone you choose and CABLE Input, in WASAPI shared mode. It
  never touches your headphones, speakers, default devices or Windows sound settings.
- No telemetry. The only network use is the update check.
