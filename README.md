# Sotto

**Just your voice.** Sotto cleans your microphone in real time — keyboard clicks, fans,
background noise — and passes only your voice to Discord, games and calls. It is small,
runs on the CPU only, and stays out of the way while you play.

<p align="center">
  <img src="assets/readme/sotto-window.png" width="416" alt="The Sotto window: noise removal set to Light, level meters, microphone and VB-Cable output, and the voice-chain switches">
</p>

## How it works

```
your mic ──► Sotto (cleans it) ──► CABLE Input ══ VB-Cable ══► CABLE Output ──► Discord / game / call
```

Sotto sends the cleaned voice to [VB-Cable](https://vb-audio.com/Cable/), a free virtual
audio cable. In your other apps you pick **CABLE Output** as the microphone.

- **Noise removal:** *Light* (RNNoise — for gaming), *High quality* (DeepFilterNet — for
  recording and calls), or *Off* to compare. A strength slider mixes in the original.
- **Voice chain**, each part optional: low-cut filter, voice EQ (presets or your own),
  noise gate, auto gain and a limiter.
- **Mute** from anywhere with a shortcut (default **Ctrl+Shift+F9**, works inside
  games), the window, or the tray.
- Closing the window keeps Sotto running in the tray. It can start with Windows.

## Your audio stays yours

Sotto opens exactly **two** devices: the microphone you choose and VB-Cable's
**CABLE Input**. It:

- never opens your headphones, speakers or DAC, and has no "listen to yourself" feature;
- never changes your default playback or recording device, volume, formats,
  enhancements or any other Windows sound setting;
- uses WASAPI **shared mode** only, so other apps can still use your mic;
- creates no virtual devices of its own;
- never switches to another device: if one you chose is unplugged, it waits for it.

If CABLE Input ever becomes your Windows default playback device (so games and music
would go into the cable), Sotto shows a calm warning and tells you how to fix it — it
won't change it for you.

## Install

**Requires:** Windows 10 or 11 (64-bit) and VB-Cable.

1. **Install VB-Cable** from the official page: <https://vb-audio.com/Cable/>
   (run `VBCABLE_Setup_x64.exe` as administrator), then **restart Windows**.
2. **Download Sotto** — `Sotto-<version>-setup.exe` from this repository's
   [Releases](https://github.com/hamadsh2/Sotto/releases) page — and run it. It installs
   for your user only, in `%LOCALAPPDATA%\Programs\Sotto`, with no admin rights.
   - The installer isn't code-signed yet, so the first time Windows SmartScreen may say
     *"Windows protected your PC"*. Choose **More info → Run anyway** — after checking
     the file's SHA-256 (below) if you like.
3. **In Sotto:** pick your microphone and **CABLE Input (VB-Audio Virtual Cable)**.
4. **In Discord, your game or your call app:** set the input / microphone to
   **CABLE Output (VB-Audio Virtual Cable)**.
   Keep your headphones or speakers as the output, as before.

Uninstall from *Settings → Apps*. Your settings stay in `%APPDATA%\Sotto` in case you
come back; delete that folder to remove them too.

## Official builds and verifying them

Official builds are published **only** on this repository's
[Releases](https://github.com/hamadsh2/Sotto/releases) page, built from the tagged source
by [GitHub Actions](.github/workflows/release.yml). Each release lists the SHA-256 of
every file and includes `SHA256SUMS.txt`. To check a download in PowerShell:

```powershell
Get-FileHash .\Sotto-0.1.0-setup.exe -Algorithm SHA256
```

The hash must match the one on the release page.

**Updates** are signed. Sotto checks this repository's releases (at start and once a
day; you can turn that off) and tells you when there's a new version. It installs only
when you click **Install**, and only after verifying the download's ed25519 signature
against the public key built into the app.

## Privacy

- No telemetry, analytics or crash reporting. Nothing about you or your audio leaves
  your PC.
- The only network use is the update check against GitHub (and the update download,
  when you choose to install).
- Settings are a small TOML file in `%APPDATA%\Sotto`.

## Performance

Measured on an Intel Core i5-14400F (16 threads), Windows 11, with Sotto's window hidden
in the tray:

| | Light (RNNoise) | High quality (DeepFilterNet) |
|---|---|---|
| CPU (share of the whole machine) | about 0.1 % | about 0.8–0.9 % |
| Processing time per 10 ms of audio (median / 99th percentile) | 0.08 ms / 0.31 ms | 1.2 ms / 1.9 ms |
| Memory (private working set, whole app) | about 15 MB | about 28 MB |
| GPU | 0 % | 0 % |

Numbers vary with the PC and settings; the readout at the bottom of the window shows
Sotto's own live CPU, latency and memory.

## Build from source

Requires Rust (MSVC toolchain) on Windows.

```powershell
cargo build --release      # target\release\sotto.exe (the app) and sotto-cli.exe
cargo test --release
```

`sotto-cli` runs the same audio engine without a window, for testing and measurements
(`sotto-cli --help`).

## License

Sotto is free software under the [GNU General Public License v3.0 only](LICENSE).

It builds on open-source work including [RNNoise](https://github.com/xiph/rnnoise) (via
[nnnoiseless](https://github.com/jneem/nnnoiseless)),
[DeepFilterNet](https://github.com/Rikorose/DeepFilterNet),
[cpal](https://github.com/RustAudio/cpal), [GPUI](https://www.gpui.rs/),
the [Inter](https://rsms.me/inter/) font and [Lucide](https://lucide.dev) icons.
All third-party licenses are in the app (*About Sotto*) and in
[`assets/THIRD_PARTY_LICENSES.txt`](assets/THIRD_PARTY_LICENSES.txt).

VB-Cable is made by VB-Audio and is not part of Sotto.
