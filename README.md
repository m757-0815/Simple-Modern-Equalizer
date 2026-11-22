# Simple Modern Equalizer

An open-source system-wide equalizer for Windows and Linux, built with Rust, Slint, and the bundled Resonance engine.

- **Simple** gives you Bass, Mid, and Treble in 0.5 dB steps. Double-click a lever to reset it.
- **Standard** has up to 32 bands, a 0.05 dB preamp control, stereo balance, preset links per output, and volume-matched A/B.
- **Live View** targets 120 Hz and has VU meters, a 128-band spectrum, frequency bars, and an oscilloscope. Drag the bottom grip to resize either graph.

Auto Headroom lowers the effective preamp to make room for EQ boosts. The Standard view shows the applied gain and disables the manual preamp controls while Auto Headroom is on. When the output clips, the EQ graph highlights the strongest frequency regions around the event.

## Build and run

Use the pinned Rust toolchain. Run `cargo test --locked` for the application tests.

**Windows x64.** Install MSVC Build Tools, a Windows SDK, and WiX 4 with its Util extension. From a developer PowerShell run:

```powershell
.\scripts\build-windows.ps1
```

The WiX installer builds to `dist/SimpleModernEqualizer-Setup-x64.msi`. It registers an unsigned audio processing object, sets `DisableProtectedAudioDG`, and restarts Windows Audio. It shares Resonance's component ID, so the two Windows builds cannot be installed alongside each other. See the [release notes and license notices](THIRD_PARTY_NOTICES.md).

**Linux.** Install PipeWire development headers, Clang, pkg-config, and Slint’s windowing dependencies. With a running PipeWire session:

```sh
sh scripts/build-linux.sh
./dist/linux/simpledarkeq
```

The package places the engine beside the app. On Windows, portable builds use the audio component from the installer.

## Contributing

`ui/main.slint` defines the interface. `src/engine.rs` handles engine IPC and `src/spectrum.rs` contains the display FFT. Preset storage uses JSON, with a supported subset of Equalizer APO text for interchange.

Keep pull requests focused and run the tests. For audio issues, include your OS, playback device, expected result, and the app’s connection and signal status.

Project source is GPL-3.0-or-later. Builds using Slint's GPL option are distributed under GPLv3. See [LICENSE](LICENSE) and [third-party notices](THIRD_PARTY_NOTICES.md).
