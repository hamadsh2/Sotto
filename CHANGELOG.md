# Changelog

All notable changes to Sotto. Versions follow [Semantic Versioning](https://semver.org).

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
