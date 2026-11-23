# Simple Modern Equalizer

A system-wide equalizer for Linux and Windows with tone controls, editable EQ curves, headphone presets, and live audio meters.

## Install and start

On Linux, use an x86-64 build and a running PipeWire session. From the repository root, launch the local AppImage:

```sh
chmod +x dist/Simple-Modern-Equalizer-x86_64.AppImage
./dist/Simple-Modern-Equalizer-x86_64.AppImage
```

If you downloaded the AppImage, use its download path. The AppImage built on Fedora 44 requires glibc 2.43 or newer.

On Fedora 44, you can install the RPM instead and launch the app from the menu:

```sh
sudo dnf install ./dist/simple-modern-equalizer-*.rpm
simple-modern-equalizer
```

On Windows, run the MSI and open **Simple Modern Equalizer** from the Start menu. The development installer registers an unsigned audio component, sets `DisableProtectedAudioDG`, and restarts Windows Audio. It shares a component ID with Resonance and cannot be installed alongside it.

![Standard mode with a live spectrum and editable EQ curve](docs/screenshots/standard.png)

## Use the equalizer

- **Simple** has Bass, Mid, Treble, Loudness, and its own saved preamp. Both views use the same **Preamp & Balance** panel and balance setting. Double-click a tone lever to reset it.
- **Standard** has parametric bands, cubic spline points, preamp, and balance. **Use Spline** and **Use Parametric** fit the same reference curve with up to 32 bands or points. A curve edit sets a new reference. Drag a point to edit it or double-click to add one. Spline edits and fits keep a minimum gap so handles stay selectable.
- **Undo** restores the last 15 EQ actions. Each drag counts once.
- **Select Preset** lets you load, save, rename, or overwrite presets and link them to an audio output. **Import Equalizer APO** saves a supported `.txt` file as a custom preset and opens it in Standard.
- **Settings** controls output, accent color, and panel visibility. **Live Monitor** targets the display’s refresh rate up to 240 Hz and includes Before / After EQ (PipeWire), 30 seconds of output RMS history, and a Stereo Field with phase correlation.
- **Settings → Start on login** opens the app when you sign in on Linux. If you move the AppImage, toggle the setting off and back on from its new location.

The **Equalizer On Off** box below the toolbar toggles EQ and balance and stays visible while scrolling in Simple. **Auto Headroom** replaces the manual preamp with a calculated gain. Turning it off keeps that gain within the manual range.

## Build and test

Use the pinned Rust toolchain, then run `cargo test --locked`. For Linux packaging, install the PipeWire development headers, Clang, and the desktop dependencies listed in the [build workflow](.github/workflows/package.yml).

| Package | Build command |
| --- | --- |
| Linux AppImage | `sh scripts/build-appimage.sh` |
| Fedora RPM | `sh scripts/build-fedora.sh` |
| Windows MSI | `./scripts/build-windows.ps1` in PowerShell with MSVC, a Windows SDK, and WiX 4 with its Util extension |

The packages go in `dist/`. The GitHub **Build installers** workflow can also build them manually or when a `v*` tag is pushed.

To test the Windows audio driver, enable the EQ and pause other playback before running:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\test-windows-audio.ps1
```

## Contribute a preset

Save your tuning in Standard. Preset JSON files are in `~/.config/simpledarkeq/presets` or `$XDG_CONFIG_HOME/simpledarkeq/presets` on Linux and `%LOCALAPPDATA%\SimpleDarkEQ\presets` on Windows. Add your preset to [presets/catalog.json](presets/catalog.json) with the headphone model, impedance, and tuning or measurement method. Include source and license details for any third-party data. Validate the catalog with `cargo test --locked bundled_catalog`.

Copyright © 2026 Simple Modern Equalizer contributors. Source is [GPL-3.0-or-later](LICENSE). Builds using Slint’s GPL option are distributed under GPLv3. See [third-party notices](THIRD_PARTY_NOTICES.md).
