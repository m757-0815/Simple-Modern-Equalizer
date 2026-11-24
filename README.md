# Simple Modern Equalizer

System-wide EQ for Linux and Windows with tone controls, editable curves, headphone presets, and live audio monitoring.

| Standard · Before / After EQ | Simple · Oscilloscope |
| --- | --- |
| [![Standard mode with three EQ bands and Before / After EQ](docs/screenshots/standard.png)](docs/screenshots/standard.png) | [![Simple mode with Bass, Mid, Treble, and Oscilloscope](docs/screenshots/simple.png)](docs/screenshots/simple.png) |

## Install

Download a package from a **Build installers** run in GitHub Actions. Local builds are in `dist/`. [Build workflow](.github/workflows/package.yml)

**Linux.** The x86-64 AppImage needs a running PipeWire session. Launch it from the folder containing the AppImage.

```sh
chmod +x Simple-Modern-Equalizer-x86_64.AppImage
./Simple-Modern-Equalizer-x86_64.AppImage
```

AppImages built on Fedora 44 require glibc 2.43 or later. On Fedora 44, you can install the RPM with `sudo dnf install ./FILE.rpm` using its filename, then launch `simple-modern-equalizer`.

**Windows.** Run `SimpleModernEqualizer-Setup-x64.msi` and open the app from Start. Uninstall Resonance first. The development MSI registers an unsigned audio component, sets `DisableProtectedAudioDG`, and restarts Windows Audio.

## How to use it

- **Simple** gives you Bass, Mid, Treble, and Loudness. Double-click a lever to reset it. Both modes share **Preamp & Balance**. Enabling **Auto Headroom** replaces the manual preamp gain with a calculated value.
- **Standard** lets you drag EQ handles, hold **Shift** for fine adjustment, and double-click to add a handle. Right-click a band or interior spline point to remove it. **Use Spline** and **Use Parametric** fit the same reference curve. **Optimizing EQ curve…** appears while a fit runs. **Undo** keeps the last 15 EQ actions.
- **Select Preset** saves tuning, links presets to an audio output, and imports Equalizer APO `.txt` files. Loading or importing asks before replacing unsaved EQ changes.
- **Live Monitor** offers six views, including Before / After EQ on PipeWire. Use **Settings** to choose an output or enable **Start on login** on Linux. [Settings screenshot](docs/screenshots/settings.png)

## Developer

Use the pinned Rust toolchain and install the platform dependencies listed in the build workflow. The app uses a Slint UI and the vendored Resonance audio engine. [Toolchain](rust-toolchain.toml) · [Build workflow](.github/workflows/package.yml) · [UI](ui/main.slint) · [Engine](vendor/resonance)

```sh
cargo test --locked -j 2
sh scripts/build-linux.sh
./dist/linux/simple-modern-equalizer
```

| Package | Build command |
| --- | --- |
| Linux AppImage | `sh scripts/build-appimage.sh` |
| Fedora RPM | `sh scripts/build-fedora.sh` |
| Windows MSI | `./scripts/build-windows.ps1` |

Windows builds need MSVC, a Windows SDK, and WiX 4 with its Util extension. Enable EQ and pause playback before running the Windows audio check. [Audio check script](scripts/test-windows-audio.ps1)

To contribute a headphone preset, add your tuning with the model, impedance, and source, then run `cargo test --locked bundled_catalog`. [Preset catalog](presets/catalog.json)

Source is GPL-3.0-or-later. Builds using Slint’s GPL option are distributed under GPLv3. [License](LICENSE) · [Third-party notices](THIRD_PARTY_NOTICES.md)
