#!/bin/sh
#
# Hyper desktop installer (macOS / Linux).
#
# Downloads hyper-desktop-<version>-<triple>.tar.gz from GitHub Releases,
# verifies SHA-256 against SHA256SUMS, and installs:
#   ~/.hyper/desktop/bin/{comet,hyper}
#   symlinks into ~/.local/bin when writable
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/DaviRain-Su/hyper-grok-build/dev/install-desktop.sh | sh
#   sh install-desktop.sh --version v0.2.119-r1
#
# Environment:
#   HYPER_SHARE_DIR   install root (default: ~/.hyper)
#   HYPER_UPDATE_BASE_URL  releases API base
#   GITHUB_TOKEN      optional API auth

set -eu

REPO="DaviRain-Su/hyper-grok-build"
API_BASE="${HYPER_UPDATE_BASE_URL:-https://api.github.com/repos/${REPO}/releases}"
HYPER_HOME="${HYPER_SHARE_DIR:-$HOME/.hyper}"
DESKTOP_HOME="${HYPER_HOME}/desktop"
BIN_DIR="${DESKTOP_HOME}/bin"

err() {
    printf 'install-desktop.sh: error: %s\n' "$*" >&2
    exit 1
}

usage() {
    sed -n '2,18p' "$0" 2>/dev/null | sed 's/^# \{0,1\}//'
}

is_semver() {
    printf '%s\n' "$1" \
        | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$'
}

VERSION=""
while [ $# -gt 0 ]; do
    case "$1" in
        --version)
            [ $# -ge 2 ] || err "--version requires an argument"
            VERSION="$2"
            shift
            ;;
        --version=*)
            VERSION="${1#--version=}"
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            err "unknown argument: $1"
            ;;
    esac
    shift
done
VERSION="${VERSION#v}"
if [ -n "$VERSION" ] && ! is_semver "$VERSION"; then
    err "invalid version '$VERSION'"
fi

OS="$(uname -s)"
ARCH="$(uname -m)"
case "$OS" in
    Darwin)
        case "$ARCH" in
            arm64|aarch64) TRIPLE="aarch64-apple-darwin" ;;
            x86_64)
                err "no desktop package for Intel macOS yet (only aarch64-apple-darwin). Use the CLI installer: install.sh"
                ;;
            *) err "unsupported macOS architecture: $ARCH" ;;
        esac
        ;;
    Linux)
        case "$ARCH" in
            arm64|aarch64) TRIPLE="aarch64-unknown-linux-gnu" ;;
            x86_64|amd64)  TRIPLE="x86_64-unknown-linux-gnu" ;;
            *) err "unsupported Linux architecture: $ARCH" ;;
        esac
        ;;
    *)
        err "unsupported OS: $OS (desktop packages are macOS arm64 / Linux only)"
        ;;
esac

AUTH_HDR=""
if [ -n "${GITHUB_TOKEN:-}" ]; then
    AUTH_HDR="Authorization: Bearer $GITHUB_TOKEN"
fi

if command -v curl >/dev/null 2>&1; then
    fetch() { curl -fsSL -o "$2" "$1"; }
    fetch_stdout() {
        if [ -n "$AUTH_HDR" ]; then
            curl -fsSL -H "$AUTH_HDR" "$1"
        else
            curl -fsSL "$1"
        fi
    }
elif command -v wget >/dev/null 2>&1; then
    fetch() { wget -q -O "$2" "$1"; }
    fetch_stdout() {
        if [ -n "$AUTH_HDR" ]; then
            wget -q --header="$AUTH_HDR" -O - "$1"
        else
            wget -q -O - "$1"
        fi
    }
else
    err "curl or wget is required"
fi

if command -v sha256sum >/dev/null 2>&1; then
    sha256_of() { sha256sum "$1" | cut -d' ' -f1; }
elif command -v shasum >/dev/null 2>&1; then
    sha256_of() { shasum -a 256 "$1" | cut -d' ' -f1; }
else
    err "sha256sum or shasum is required"
fi

TMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/hyper-desktop-install.XXXXXX")"
cleanup() { rm -rf "$TMP_DIR"; }
trap cleanup EXIT HUP INT TERM

if [ -n "$VERSION" ]; then
    RELEASE_URL="$API_BASE/tags/v$VERSION"
else
    RELEASE_URL="$API_BASE/latest"
fi
printf 'Resolving release from %s\n' "$RELEASE_URL"
RELEASE_JSON="$(fetch_stdout "$RELEASE_URL")" \
    || err "could not fetch release metadata from $RELEASE_URL"

TAG="$(printf '%s' "$RELEASE_JSON" \
    | sed 's/"tag_name"/\
"tag_name"/g' \
    | sed -n 's/^[[:space:]]*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
    | head -n 1)"
[ -n "$TAG" ] || err "release metadata has no tag_name"
RESOLVED_VERSION="${TAG#v}"
if [ -n "$VERSION" ] && [ "$RESOLVED_VERSION" != "$VERSION" ]; then
    err "requested version $VERSION but release tag is $TAG"
fi

ASSET="hyper-desktop-${RESOLVED_VERSION}-${TRIPLE}.tar.gz"
printf 'Looking for asset %s\n' "$ASSET"

# Prefer browser_download_url from API JSON.
ASSET_URL="$(printf '%s' "$RELEASE_JSON" \
    | tr ',' '\n' \
    | sed -n 's/.*"browser_download_url"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
    | grep -F "/${ASSET}" \
    | head -n 1)"
SUMS_URL="$(printf '%s' "$RELEASE_JSON" \
    | tr ',' '\n' \
    | sed -n 's/.*"browser_download_url"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
    | grep -E '/SHA256SUMS$' \
    | head -n 1)"

if [ -z "$ASSET_URL" ]; then
    # Fallback: GitHub releases/download path
    ASSET_URL="https://github.com/${REPO}/releases/download/${TAG}/${ASSET}"
fi
if [ -z "$SUMS_URL" ]; then
    SUMS_URL="https://github.com/${REPO}/releases/download/${TAG}/SHA256SUMS"
fi

ARCHIVE="$TMP_DIR/$ASSET"
SUMS="$TMP_DIR/SHA256SUMS"
printf 'Downloading %s\n' "$ASSET_URL"
fetch "$ASSET_URL" "$ARCHIVE" || err "download failed: $ASSET_URL"
printf 'Downloading SHA256SUMS\n'
fetch "$SUMS_URL" "$SUMS" || err "download failed: $SUMS_URL"

EXPECTED="$(grep -E "[[:space:]]${ASSET}\$" "$SUMS" | awk '{print $1}' | head -n 1)"
[ -n "$EXPECTED" ] || err "SHA256SUMS has no entry for $ASSET"
ACTUAL="$(sha256_of "$ARCHIVE")"
[ "$EXPECTED" = "$ACTUAL" ] || err "checksum mismatch for $ASSET
  expected $EXPECTED
  got      $ACTUAL"

EXTRACT="$TMP_DIR/extract"
mkdir -p "$EXTRACT"
tar -xzf "$ARCHIVE" -C "$EXTRACT"
[ -f "$EXTRACT/bin/comet" ] || err "archive missing bin/comet"
[ -f "$EXTRACT/bin/hyper" ] || err "archive missing bin/hyper"

mkdir -p "$BIN_DIR"
# Atomic-ish replace: stage then mv
cp -f "$EXTRACT/bin/comet" "$BIN_DIR/comet.new"
cp -f "$EXTRACT/bin/hyper" "$BIN_DIR/hyper.new"
chmod 0755 "$BIN_DIR/comet.new" "$BIN_DIR/hyper.new"
mv -f "$BIN_DIR/comet.new" "$BIN_DIR/comet"
mv -f "$BIN_DIR/hyper.new" "$BIN_DIR/hyper"

LOCAL_BIN="${HOME}/.local/bin"
if mkdir -p "$LOCAL_BIN" 2>/dev/null; then
    ln -sfn "$BIN_DIR/comet" "$LOCAL_BIN/comet"
    ln -sfn "$BIN_DIR/hyper" "$LOCAL_BIN/hyper"
    printf 'Linked into %s\n' "$LOCAL_BIN"
fi

printf '\nHyper desktop %s installed.\n' "$RESOLVED_VERSION"
printf '  comet: %s\n' "$BIN_DIR/comet"
printf '  hyper: %s\n' "$BIN_DIR/hyper"
printf '\nRun:\n'
printf '  export HYPER_AGENT_BIN=%s\n' "$BIN_DIR/hyper"
printf '  export PATH="%s:%s:\$PATH"\n' "$BIN_DIR" "$LOCAL_BIN"
printf '  comet\n'
printf '\nAgent credentials: use `hyper login` (or comet agent-login).\n'
printf 'Desktop data dir:  %s\n' "$DESKTOP_HOME"
