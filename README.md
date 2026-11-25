# Simple Modern Equalizer for Linux and Windows

Simple Modern Equalizer is a free, open-source system-wide audio equalizer for headphones and speakers. Adjust bass, midrange, and treble, edit parametric EQ bands or spline curves, and monitor your audio live. The app uses PipeWire on Linux and imports Equalizer APO presets.

[Download installers](#downloads) · [Equalizer features](#equalizer-features) · [Build from source](#build-from-source)

| Parametric EQ with before / after monitoring | Tone controls with a live oscilloscope |
| --- | --- |
| [![Parametric audio equalizer with three editable bands and a before / after spectrum](docs/screenshots/standard.png)](docs/screenshots/standard.png) | [![Bass, midrange, and treble controls with a live audio oscilloscope](docs/screenshots/simple.png)](docs/screenshots/simple.png) |

## Install

### Downloads

Sign in to GitHub, open a successful **Build installers** run, and download your package from **Artifacts**. New runs offer the files individually. [Build installers runs](https://github.com/m757-0815/Simple-Modern-Equalizer/actions/workflows/package.yml)

| Target | Package |
| --- | --- |
| Windows x86-64 MSI | `SimpleModernEqualizer-Setup-x64.msi` |
| Linux x86-64 AppImage | `Simple-Modern-Equalizer-x86_64.AppImage` |
| Fedora 44 x86-64 RPM | `simple-modern-equalizer-*.x86_64.rpm` |
| Ubuntu 24.04 x86-64 DEB | `simple-modern-equalizer_*+ubuntu24.04_amd64.deb` |
| Debian 13 x86-64 DEB | `simple-modern-equalizer_*+debian13_amd64.deb` |
| Source | `Simple-Modern-Equalizer-source.tar.gz` |

Local installer builds are in `dist/`.

### Linux AppImage

The x86-64 AppImage needs a running PipeWire session. Launch the app from the folder containing the AppImage.

```sh
chmod +x Simple-Modern-Equalizer-x86_64.AppImage
./Simple-Modern-Equalizer-x86_64.AppImage
```

AppImages built on Fedora 44 require glibc 2.43 or later.

### Ubuntu 24.04 and Debian 13 packages

Build the x86-64 `.deb` for your distribution or get it from a completed **Build installers** run. Run `sudo apt install ./FILE.deb` with its filename, then launch **Simple Modern Equalizer** from the application menu.

### Fedora 44 RPM

Run `sudo dnf install ./FILE.rpm` with the RPM filename, then launch `simple-modern-equalizer`.

### Windows x64 installer

Run `SimpleModernEqualizer-Setup-x64.msi` and open the app from Start. Uninstall Resonance first. The development MSI registers an unsigned audio component, sets `DisableProtectedAudioDG`, and restarts Windows Audio.

## Equalizer features

- **Tone controls.** The **Simple** view gives you Bass, Mid, Treble, and Loudness. Double-click a lever to reset it. Both modes share **Preamp & Balance**. Enabling **Auto Headroom** replaces the manual preamp gain with a calculated value.
- **Parametric EQ and spline editing.** In **Standard**, drag EQ handles, hold **Shift** for fine adjustment, and double-click to add a handle. Set the graph range in the upper right from **±1 dB** to **±24 dB**. At ±1 dB, moving through half the graph height adjusts gain by 1 dB. Right-click a band or interior spline point to remove it. **Use Spline** and **Use Parametric** fit the same reference curve. **Optimizing EQ curve…** appears while a fit runs. **Undo** keeps the last 15 EQ actions.
- **EQ phase mode.** In **Settings → Playback**, choose **Minimum Phase** (default), **Matched Phase**, **Linear Phase** (up to 176 ms of EQ buffering), or **Zero Phase (Beta)** (up to 250 ms). Matched Phase keeps the biquads and adds a short correction to approximate the analog magnitude and phase of the parametric bands. It adds about 11 ms at 48 kHz and retains the minimum-phase spline response. Linear and Zero Phase use a symmetric FIR for the full curve. Zero Phase uses a longer filter to follow narrow bands more closely. Very narrow bass bands are approximated at these fixed buffer lengths. The setting applies across presets.
- **Headphone presets and Equalizer APO import.** Start with Flat, DT 770 PRO (32, 80, or 250 ohms), or one of eight DT 990 presets covering PRO, PRO Black Edition, Edition, and PRO X. **Select Preset** links presets to an audio output, saves custom tunings, and imports Equalizer APO `.txt` files. Loading or importing asks before replacing unsaved EQ changes. The **Delete** button asks for confirmation before deleting a saved preset or hiding a built-in one. [Headphone preset catalog](presets/catalog.json)
- **Live audio monitoring.** **Live Monitor** offers six views, including Before / After EQ on PipeWire. Use **Settings** to choose an output or enable **Start on login** on Linux. [Audio output settings screenshot](docs/screenshots/settings.png)
- **Now Playing.** Shows the current song and artist from the system media session.

## Build from source

Use the pinned Rust toolchain and install the platform dependencies listed in the build workflow. The app uses a Slint UI and the vendored Resonance audio engine. [Toolchain](rust-toolchain.toml) · [Build workflow](.github/workflows/package.yml) · [UI](ui/main.slint) · [Engine](vendor/resonance)

```sh
cargo test --locked -j 2
sh scripts/build-linux.sh
./dist/linux/simple-modern-equalizer
```

| Package | Build command |
| --- | --- |
| [Linux AppImage](scripts/build-appimage.sh) | `sh scripts/build-appimage.sh` |
| [Ubuntu or Debian package](scripts/build-deb.sh) | `sh scripts/build-deb.sh` on the target distribution |
| [Fedora RPM](scripts/build-fedora.sh) | `sh scripts/build-fedora.sh` |
| [Windows MSI](scripts/build-windows.ps1) | `./scripts/build-windows.ps1` |

Windows builds need MSVC, a Windows SDK, and WiX 4 with its Util extension. Enable EQ and pause playback before running the Windows audio check. [Audio check script](scripts/test-windows-audio.ps1)

To contribute a headphone preset, add your tuning with the model, impedance, and source, then run `cargo test --locked bundled_catalog`.

## License

Source is GPL-3.0-or-later. Builds using Slint’s GPL option are distributed under GPLv3. [License](LICENSE) · [Third-party notices](THIRD_PARTY_NOTICES.md)
