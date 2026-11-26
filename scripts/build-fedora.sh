#!/bin/sh
set -eu
cd "$(dirname "$0")/.."

#######################################################
# build and stage the Linux binaries
#######################################################

sh scripts/build-linux.sh

#######################################################
# build the RPM in a temporary directory
#######################################################

version=$(cargo metadata --no-deps --locked --format-version 1 | python3 -c 'import json, sys; print(json.load(sys.stdin)["packages"][0]["version"])')
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
rpmbuild -bb packaging/simple-modern-equalizer.spec \
    --define "_topdir $work" --define "app_version $version" --define "staged_app $PWD/dist/linux"
cp "$work"/RPMS/*/*.rpm dist/
