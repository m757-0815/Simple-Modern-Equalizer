#!/bin/sh
set -eu
cd "$(dirname "$0")/.."

#######################################################
# build and stage the Linux binaries
#######################################################

sh scripts/build-linux.sh

#######################################################
# assemble the AppImage directory
#######################################################

work=$(mktemp -d "$PWD/dist/.appimage.XXXXXX")
trap 'rm -rf "$work"' EXIT
appdir="$work/Simple-Modern-Equalizer.AppDir"
mkdir -p "$appdir/usr/bin" "$appdir/usr/share/licenses/simple-modern-equalizer"
cp dist/linux/simple-modern-equalizer dist/linux/resonanced "$appdir/usr/bin/"
cp -R dist/linux/share/. "$appdir/usr/share/"
cp dist/linux/LICENSE dist/linux/Inter-OFL.txt dist/linux/THIRD_PARTY_NOTICES.md "$appdir/usr/share/licenses/simple-modern-equalizer/"
cp packaging/simple-modern-equalizer.desktop "$appdir/"
cp ui/icons/app-icon.png "$appdir/simple-modern-equalizer.png"
ln -s simple-modern-equalizer.png "$appdir/.DirIcon"
cat > "$appdir/AppRun" <<'EOF'
#!/bin/sh
appdir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
exec "$appdir/usr/bin/simple-modern-equalizer" "$@"
EOF
chmod +x "$appdir/AppRun"

#######################################################
# download and verify the pinned packaging tools
#######################################################

curl -fsSL --retry 3 https://github.com/AppImage/appimagetool/releases/download/1.9.1/appimagetool-x86_64.AppImage -o "$work/appimagetool"
curl -fsSL --retry 3 https://github.com/AppImage/type2-runtime/releases/download/20251108/runtime-x86_64 -o "$work/runtime"
printf '%s  %s\n' 'ed4ce84f0d9caff66f50bcca6ff6f35aae54ce8135408b3fa33abfc3cb384eb0' "$work/appimagetool" \
    '2fca8b443c92510f1483a883f60061ad09b46b978b2631c807cd873a47ec260d' "$work/runtime" | sha256sum --check
chmod +x "$work/appimagetool"
# Extract the packager so it can run in CI containers without a FUSE mount.
(cd "$work" && ./appimagetool --appimage-extract >/dev/null)

#######################################################
# package the launcher and move the finished AppImage
#######################################################

output=Simple-Modern-Equalizer-x86_64.AppImage
ARCH=x86_64 "$work/squashfs-root/AppRun" --runtime-file "$work/runtime" "$appdir" "$work/$output"
# Move the AppImage into place only after packaging succeeds.
mv -f "$work/$output" "$PWD/dist/$output"
