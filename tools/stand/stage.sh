#!/bin/sh
# Assembles what `turnout deploy` uploads to a stand: the installer, the
# service script, the version the stand is to run, and the one command that
# puts them to work. On the stand, as root:
#
#   sh stand.sh
#
# Taken from HEAD, not the working tree: what reaches a stand is a committed
# state, and the version is the one the manifest names - a released one, since
# the stand installs it from the release, not from this checkout. The command
# is written here rather than kept in the deploy's configuration, so the names
# the installer reads live in this repository alone.
set -eu

out=${1:-deploy}
rm -rf "$out"
mkdir -p "$out"
out=$(cd "$out" && pwd)
# git is asked from inside the repository rather than handed its path: on
# Windows a path given to git.exe from a POSIX shell is not always translated.
cd "$(dirname "$0")/../.."

git show HEAD:tools/install.sh >"$out/install.sh"
git show HEAD:tools/service.sh >"$out/service.sh"
version=$(git show HEAD:Cargo.toml | sed -n 's/^version = "\(.*\)"$/\1/p' | head -n 1)
case "$version" in
    [0-9]*) ;;
    *)
        echo "Cannot read the version from HEAD:Cargo.toml" >&2
        exit 1
        ;;
esac
echo "v$version" >"$out/VERSION"
cat >"$out/stand.sh" <<'STAND'
#!/bin/sh
# Installs the release VERSION names where the service looks for it, then sets
# the relay up as a service - or upgrades it, with its data copied aside.
set -eu
cd "$(dirname "$0")"
EFEMA_SERVER_VERSION=$(cat VERSION) EFEMA_SERVER_INSTALL_DIR=/usr/local/bin sh install.sh
sh service.sh
STAND
echo "staged efema-server v$version into $out/"
