#!/bin/sh
set -eu
cd "$(dirname "$0")/.."

#######################################################
# compile the app and the PipeWire audio daemon
#######################################################

cargo build --locked --release
cargo build --locked --manifest-path vendor/resonance/Cargo.toml --release -p resonance-daemon
mkdir -p dist/linux

#######################################################
# stage the binaries, desktop launcher, and licenses
#######################################################

# Keep both binaries together so the app can find the daemon at startup.
cp target/release/simple-modern-equalizer vendor/resonance/target/release/resonanced dist/linux/
mkdir -p dist/linux/share/applications dist/linux/share/icons/hicolor/256x256/apps
cp packaging/simple-modern-equalizer.desktop dist/linux/share/applications/
cp ui/icons/app-icon.png dist/linux/share/icons/hicolor/256x256/apps/simple-modern-equalizer.png
cp LICENSE THIRD_PARTY_NOTICES.md dist/linux/
cp ui/fonts/OFL.txt dist/linux/Inter-OFL.txt
printf '%s\n' 'Staged dist/linux. Requires the desktop system PipeWire service.'
