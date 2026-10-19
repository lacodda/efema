#!/bin/sh
# Assembles what `turnout deploy` uploads to a stand: the installer, the
# service script, and the version the stand is to run.
#
# Taken from HEAD, not the working tree: what reaches a stand is a committed
# state, and the version is the one the manifest names - a released one, since
# the stand installs it from the release, not from this checkout. On the
# stand, as root:
#
#   EFEMA_VERSION=$(cat VERSION) EFEMA_INSTALL_DIR=/usr/local/bin sh install.sh && sh service.sh
set -eu

out=${1:-deploy}
here="$(cd "$(dirname "$0")/../.." && pwd)"

rm -rf "$out"
mkdir -p "$out"
git -C "$here" show HEAD:tools/install.sh >"$out/install.sh"
git -C "$here" show HEAD:tools/service.sh >"$out/service.sh"
version=$(git -C "$here" show HEAD:Cargo.toml | sed -n 's/^version = "\(.*\)"$/\1/p' | head -n 1)
case "$version" in
    [0-9]*) ;;
    *)
        echo "Cannot read the version from HEAD:Cargo.toml" >&2
        exit 1
        ;;
esac
echo "v$version" >"$out/VERSION"
echo "staged efema-server v$version into $out/"
