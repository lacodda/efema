#!/bin/sh
# efema-server installer for Linux and macOS:
#   curl -fsSL https://raw.githubusercontent.com/lacodda/efema/main/tools/install.sh | sh
#
# EFEMA_VERSION pins a release (v0.6.0); EFEMA_INSTALL_DIR is where the binary
# goes (~/.local/bin). On a server, put it where the service looks for it:
#   curl -fsSL https://raw.githubusercontent.com/lacodda/efema/main/tools/install.sh | sudo EFEMA_INSTALL_DIR=/usr/local/bin sh
set -eu

REPO="lacodda/efema"

case "$(uname -s)-$(uname -m)" in
    Linux-x86_64) TARGET="x86_64-unknown-linux-gnu" ;;
    # A Raspberry Pi on a 64-bit system says aarch64; macOS says arm64 for
    # the same architecture.
    Linux-aarch64 | Linux-arm64) TARGET="aarch64-unknown-linux-gnu" ;;
    Darwin-arm64) TARGET="aarch64-apple-darwin" ;;
    # Git Bash, MSYS2 and Cygwin run this script on Windows, where the
    # Windows build is installed by install.ps1 - saying so beats the generic
    # dead end below, since a Windows release does exist.
    MINGW* | MSYS* | CYGWIN*)
        echo "This is the Linux/macOS installer, but you are on Windows ($(uname -s))." >&2
        echo "Install with PowerShell instead:" >&2
        echo "  irm https://raw.githubusercontent.com/$REPO/main/tools/install.ps1 | iex" >&2
        exit 1
        ;;
    *)
        echo "No prebuilt efema-server for $(uname -s)/$(uname -m); install with: cargo install efema-server" >&2
        exit 1
        ;;
esac

# The tag comes from the /releases/latest redirect rather than the REST API:
# unauthenticated API calls are capped at 60 an hour per address, and an
# installer that fails because someone else behind the same address ran it is
# no installer.
TAG="${EFEMA_VERSION:-}"
if [ -z "$TAG" ]; then
    LOCATION=$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/$REPO/releases/latest" || true)
    TAG="${LOCATION##*/}"
fi
case "$TAG" in
    v[0-9]*) ;;
    *)
        echo "Cannot tell the latest release of $REPO - set EFEMA_VERSION to a tag like v0.6.0" >&2
        exit 1
        ;;
esac

NAME="efema-server-$TAG-$TARGET"
URL="https://github.com/$REPO/releases/download/$TAG/$NAME.tar.gz"
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

echo "Downloading $URL"
curl -fsSL "$URL" | tar xz -C "$TMP"
# Found in what was unpacked, not at a guessed path.
BIN=$(find "$TMP" -type f -name efema-server | head -n 1)
if [ -z "$BIN" ]; then
    echo "The archive $NAME.tar.gz holds no efema-server" >&2
    exit 1
fi

DIR="${EFEMA_INSTALL_DIR:-$HOME/.local/bin}"
mkdir -p "$DIR"
install -m 755 "$BIN" "$DIR/efema-server"
echo "Installed $("$DIR/efema-server" --version) to $DIR/efema-server"

case ":$PATH:" in
    *":$DIR:"*) ;;
    *) echo "Note: add $DIR to your PATH." ;;
esac
echo "Next: https://lacodda.github.io/efema/getting-started/ - or, as a service on a Linux server, https://lacodda.github.io/efema/guides/on-a-server/"
