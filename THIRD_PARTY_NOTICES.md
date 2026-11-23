# Open-source notices

Simple Modern Equalizer is GPL-3.0-or-later. Source distributions retain all upstream files
and their license notices.

## Resonance

Source: https://github.com/ealtun21/resonance

Vendored commit: `be8cc6a1c142706cffaaffa1e980094a28faddcd` (workspace 0.11.2).

License: GPL-3.0-or-later. Full text: `vendor/resonance/LICENSE`.
**Local modifications, 2026-11-21.** This copy changes per-channel RMS and
channel-count telemetry, output-switch reporting, frequency response calculation,
managed daemon startup and shutdown, and Windows APO build, installation, and
removal. The modified paths are:

- `contrib/windows/{build-apo,install-apo,uninstall-apo}.ps1`
- `crates/resonance-apo/src/{ffi,state}.rs`
- `crates/resonance-daemon/src/{ipc_server,main,meters,state}.rs` and
  `crates/resonance-daemon/src/audio/{coreaudio,pipewire}.rs`
- `crates/resonance-dsp/Cargo.toml` and `crates/resonance-dsp/src/analysis.rs`
- `crates/resonance-gui/src/app.rs`
- `crates/resonance-ipc/Cargo.toml` and `crates/resonance-ipc/src/{fr,lib}.rs`

The Windows engine shares Resonance's COM and shared-state identifiers.
The development installer sets `DisableProtectedAudioDG` to load the unsigned
APO, and its uninstaller attempts to restore the previous registry value and
endpoint effects. The installation scripts are in `vendor/resonance/contrib/windows`.

## Slint

Source: https://github.com/slint-ui/slint

Slint 1.18.1 is used under its GPL-3.0 license option. See Slint's crate source
for full license text and notices. The app does not require a commercial Slint license
when distributed in compliance with that GPL option.

Other Rust dependencies retain their own licenses in Cargo registry source.

## Publishing builds

For each distributed installer or archive, offer the complete corresponding
source from the same release location. Use the exact revision that built it,
including the modified engine, lockfiles, and build and install scripts. Include
applicable third-party copyright and license notices with the binaries.

## Inter Light

The interface uses Inter Light under the SIL Open Font License 1.1.
Source: https://github.com/rsms/inter/releases/tag/v4.1
The license is in `ui/fonts/OFL.txt` and ships with packaged builds.

## Bundled headphone presets

The bundled filter settings and descriptions were created for Simple Modern
Equalizer as listening starting points and distributed under GPL-3.0-or-later.
Several impedance variants share a family tuning. The DT 770 M preset is flat.
The manufacturer references below provide model names and impedance information.

Beyerdynamic and its model names identify the headphones these presets are
intended for. Simple Modern Equalizer is an independent project.

Verified model references:

- https://support.beyerdynamic.com/hc/en-us/articles/25075623967004-Supported-headphones
- https://support.beyerdynamic.com/hc/en-us/articles/202502561-Which-DT-770-PRO-for-which-source
- https://support.beyerdynamic.com/hc/en-us/articles/12354416525980-How-does-the-DT-770-PRO-X-differ-from-the-DT-770-PRO
- https://global.beyerdynamic.com/p/dt-990-pro-x
- https://support.beyerdynamic.com/hc/en-us/articles/360001638013-What-is-the-difference-between-the-models-DT-770-PRO-and-DT-770-M

Catalog scope: the listed PRO, Black Edition, Edition, PRO X, PRO X Limited
Edition and M variants, not an exhaustive archive of every discontinued special
edition. More entries can be added as JSON without changing application code.
