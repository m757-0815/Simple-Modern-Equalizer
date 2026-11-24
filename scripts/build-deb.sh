#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
umask 022

# Build against the libraries of the distribution that will install the package.
. /etc/os-release
name=simple-modern-equalizer
app_version=$(cargo metadata --no-deps --locked --format-version 1 | python3 -c 'import json, sys; print(json.load(sys.stdin)["packages"][0]["version"])')
version="${app_version}-1+${ID}${VERSION_ID}"
arch=$(dpkg --print-architecture)
maintainer='Simple Modern Equalizer contributors <m757-0815@users.noreply.github.com>'
homepage=https://github.com/m757-0815/Simple-Modern-Equalizer

sh scripts/build-linux.sh
work=$(mktemp -d "$PWD/dist/.deb.XXXXXX")
trap 'rm -rf "$work"' EXIT
package_dir="debian/$name"
package="$work/$package_dir"
app_dir="usr/lib/$name"
doc_dir="$package/usr/share/doc/$name"
mkdir -p "$package/DEBIAN" "$package/usr/bin" "$package/$app_dir" "$doc_dir"

install -m755 dist/linux/simple-modern-equalizer dist/linux/resonanced "$package/$app_dir/"
ln -s "../lib/$name/$name" "$package/usr/bin/$name"
desktop="share/applications/$name.desktop"
icon="share/icons/hicolor/256x256/apps/$name.png"
install -Dm644 "dist/linux/$desktop" "$package/usr/$desktop"
install -Dm644 "dist/linux/$icon" "$package/usr/$icon"
install -m644 dist/linux/THIRD_PARTY_NOTICES.md "$doc_dir/"
{
    printf 'Source: %s\n\nSee THIRD_PARTY_NOTICES.md for bundled components.\n\n' "$homepage"
    cat dist/linux/LICENSE
    printf '\n'
    cat dist/linux/Inter-OFL.txt
} > "$doc_dir/copyright"

desktop-file-validate "$package/usr/$desktop"

# dpkg-shlibdeps reads its source package description from the working directory.
cat > "$work/debian/control" <<EOF
Source: $name
Section: sound
Priority: optional
Maintainer: $maintainer

Package: $name
Architecture: any
Description: System-wide graphical audio equalizer
EOF
shlib_deps=$(cd "$work" && dpkg-shlibdeps -O -e"$package_dir/$app_dir/$name" -e"$package_dir/$app_dir/resonanced")
shlib_deps=${shlib_deps#shlibs:Depends=}
test -n "$shlib_deps"

installed_size=$(du -ks "$package" | cut -f1)
# The window backend loads its display libraries dynamically.
cat > "$package/DEBIAN/control" <<EOF
Package: $name
Version: $version
Section: sound
Priority: optional
Architecture: $arch
Maintainer: $maintainer
Installed-Size: $installed_size
Depends: $shlib_deps, pipewire-audio, hicolor-icon-theme,
 libegl1, libgl1, libwayland-client0, libwayland-cursor0, libwayland-egl1,
 libx11-6, libx11-xcb1, libxcursor1, libxi6, libxkbcommon0, libxkbcommon-x11-0, libxrandr2
Homepage: $homepage
Description: System-wide graphical audio equalizer
 Adjust system audio with tone controls, editable EQ curves,
 headphone presets, and live audio monitoring.
EOF

output="${name}_${version}_${arch}.deb"
dpkg-deb --root-owner-group --build "$package" "$work/$output"
mv -f "$work/$output" "dist/$output"
printf 'Built dist/%s\n' "$output"
