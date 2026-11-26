#!/bin/sh
set -eu
cd "$(dirname "$0")/.."

#######################################################
# prepare locked Rust dependencies for offline builds
#######################################################

root="$PWD"
work="$root/dist/flatpak-build"
app_id=io.github.m757_0815.simple-modern-equalizer
remote_url=https://dl.flathub.org/repo/flathub.flatpakrepo
mkdir -p "$work"
exec 9>"$work/build.lock"
flock 9
(
    cd "$work"
    cargo vendor --locked --versioned-dirs --manifest-path "$root/Cargo.toml" \
        --sync "$root/vendor/resonance/Cargo.toml" cargo-vendor > cargo-config.toml
)

#######################################################
# build and export the installable Flatpak bundle
#######################################################

flatpak remote-add --user --if-not-exists flathub "$remote_url"
flatpak-builder --user --install-deps-from=flathub --jobs=4 --disable-rofiles-fuse \
    --force-clean --default-branch=stable --state-dir="$work/state" --repo="$work/repo" \
    "$work/app" "packaging/flatpak/$app_id.yml"

bundle="Simple-Modern-Equalizer-$(flatpak --default-arch).flatpak"
flatpak build-bundle --runtime-repo="$remote_url" "$work/repo" "$work/$bundle" "$app_id" stable
mv -f "$work/$bundle" "$root/dist/$bundle"
printf 'Flatpak bundle: dist/%s\n' "$bundle"
