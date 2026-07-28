#!/bin/sh
#
# Hyper installer (macOS / Linux).
#
# Downloads the matching platform artifact from this repo's GitHub Releases,
# verifies its SHA-256 against the release's SHA256SUMS manifest, and installs
# the binary as ~/.hyper/bin/hyper (versioned binary in ~/.hyper/downloads/,
# atomic symlink in bin/).
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/DaviRain-Su/hyper-grok-build/dev/install.sh | sh
#   sh install.sh --version v0.2.109      # pin a specific release
#
# Environment:
#   HYPER_SHARE_DIR        install root (default: ~/.hyper)
#   HYPER_UPDATE_BASE_URL  GitHub-Releases-shaped API base (default:
#                          https://api.github.com/repos/DaviRain-Su/hyper-grok-build/releases)
#
# Fails fast on any error; never leaves a partial binary as the active hyper.

set -eu

REPO="DaviRain-Su/hyper-grok-build"
API_BASE="${HYPER_UPDATE_BASE_URL:-https://api.github.com/repos/${REPO}/releases}"
HYPER_HOME="${HYPER_SHARE_DIR:-$HOME/.hyper}"

err() {
    printf 'install.sh: error: %s\n' "$*" >&2
    exit 1
}

usage() {
    sed -n '2,20p' "$0" 2>/dev/null | sed 's/^# \{0,1\}//'
}

# ── Arguments ────────────────────────────────────────────────────────────────
VERSION=""
while [ $# -gt 0 ]; do
    case "$1" in
        --version)
            [ $# -ge 2 ] || err "--version requires an argument (e.g. --version v0.2.109)"
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
            err "unknown argument: $1 (supported: --version vX.Y.Z)"
            ;;
    esac
    shift
done
VERSION="${VERSION#v}"
if [ -n "$VERSION" ]; then
    case "$VERSION" in
        [0-9]*.[0-9]*.[0-9]*) ;;
        *) err "invalid version '$VERSION' (expected X.Y.Z or vX.Y.Z)" ;;
    esac
fi

# ── Platform detection ───────────────────────────────────────────────────────
OS="$(uname -s)"
ARCH="$(uname -m)"
TRIPLE_FALLBACK=""
case "$OS" in
    Darwin)
        PLATFORM_OS="macos"
        case "$ARCH" in
            arm64|aarch64) TRIPLE="aarch64-apple-darwin"; PLATFORM_ARCH="aarch64" ;;
            x86_64)        TRIPLE="x86_64-apple-darwin";  PLATFORM_ARCH="x86_64" ;;
            *) err "unsupported macOS architecture: $ARCH" ;;
        esac
        ;;
    Linux)
        PLATFORM_OS="linux"
        # v0.1.x publishes glibc (linux-gnu) assets — correct for Omarchy and
        # other glibc distros. Prefer gnu; fall back to musl if a later release
        # only ships static musl builds (or both are present and gnu is absent).
        case "$ARCH" in
            arm64|aarch64)
                TRIPLE="aarch64-unknown-linux-gnu"
                TRIPLE_FALLBACK="aarch64-unknown-linux-musl"
                PLATFORM_ARCH="aarch64"
                ;;
            x86_64|amd64)
                TRIPLE="x86_64-unknown-linux-gnu"
                TRIPLE_FALLBACK="x86_64-unknown-linux-musl"
                PLATFORM_ARCH="x86_64"
                ;;
            *) err "unsupported Linux architecture: $ARCH" ;;
        esac
        ;;
    *)
        err "unsupported OS: $OS (Windows: use install.ps1)"
        ;;
esac

# ── Downloader ───────────────────────────────────────────────────────────────
# Optional: set GITHUB_TOKEN to authenticate GitHub API + asset requests and
# avoid the unauthenticated rate limit (60 req/hr per IP). A fine-grained PAT
# with public-repo read access raises it to 5000 req/hr.
AUTH_HDR=""
if [ -n "${GITHUB_TOKEN:-}" ]; then
    AUTH_HDR="Authorization: Bearer $GITHUB_TOKEN"
fi

if command -v curl >/dev/null 2>&1; then
    if [ -n "$AUTH_HDR" ]; then
        fetch()        { curl -fsSL -H "$AUTH_HDR" -o "$2" "$1"; }
        fetch_stdout() { curl -fsSL -H "$AUTH_HDR" "$1"; }
    else
        fetch()        { curl -fsSL -o "$2" "$1"; }
        fetch_stdout() { curl -fsSL "$1"; }
    fi
elif command -v wget >/dev/null 2>&1; then
    if [ -n "$AUTH_HDR" ]; then
        fetch()        { wget -q --header="$AUTH_HDR" -O "$2" "$1"; }
        fetch_stdout() { wget -q --header="$AUTH_HDR" -O - "$1"; }
    else
        fetch()        { wget -q -O "$2" "$1"; }
        fetch_stdout() { wget -q -O - "$1"; }
    fi
else
    err "either curl or wget is required"
fi

# ── SHA-256 tool ─────────────────────────────────────────────────────────────
if command -v sha256sum >/dev/null 2>&1; then
    sha256_of() { sha256sum "$1" | cut -d' ' -f1; }
elif command -v shasum >/dev/null 2>&1; then
    sha256_of() { shasum -a 256 "$1" | cut -d' ' -f1; }
else
    err "either sha256sum or shasum is required to verify the download"
fi

TMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/hyper-install.XXXXXX")"
trap 'rm -rf "$TMP_DIR"' EXIT INT TERM

# ── Resolve the release ──────────────────────────────────────────────────────
if [ -n "$VERSION" ]; then
    RELEASE_URL="$API_BASE/tags/v$VERSION"
else
    RELEASE_URL="$API_BASE/latest"
fi
printf 'Resolving release from %s\n' "$RELEASE_URL"
RELEASE_JSON="$(fetch_stdout "$RELEASE_URL")" \
    || err "could not fetch release metadata from $RELEASE_URL
         (GitHub may be rate-limiting this IP; set GITHUB_TOKEN to authenticate)"

TAG="$(printf '%s' "$RELEASE_JSON" \
    | tr ',' '\n' \
    | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
    | head -n 1)"
[ -n "$TAG" ] || err "release metadata has no tag_name (endpoint: $RELEASE_URL)"
RESOLVED_VERSION="${TAG#v}"
if [ -n "$VERSION" ] && [ "$RESOLVED_VERSION" != "$VERSION" ]; then
    err "requested version $VERSION but release tag is $TAG"
fi

# Pull every browser_download_url out of the JSON, then select by asset name.
URLS="$(printf '%s' "$RELEASE_JSON" \
    | tr ',' '\n' \
    | sed -n 's/.*"browser_download_url"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p')"
SUMS_URL="$(printf '%s\n' "$URLS" | grep -F "/SHA256SUMS" | head -n 1 || true)"
[ -n "$SUMS_URL" ] || err "release $TAG has no SHA256SUMS asset; refusing to install unverified binaries"

# Resolve archive: preferred triple, then Linux gnu fallback when present.
ASSET=""
ARCHIVE_URL=""
for cand in "$TRIPLE" ${TRIPLE_FALLBACK:-}; do
    [ -n "$cand" ] || continue
    trial="hyper-${RESOLVED_VERSION}-${cand}.tar.gz"
    found="$(printf '%s\n' "$URLS" | grep -F "/$trial" | head -n 1 || true)"
    if [ -n "$found" ]; then
        ASSET="$trial"
        ARCHIVE_URL="$found"
        TRIPLE="$cand"
        break
    fi
done
if [ -z "$ARCHIVE_URL" ]; then
    available="$(printf '%s\n' "$URLS" \
        | sed -n 's|.*/\(hyper-[^/"]*\)|\1|p' \
        | grep -v '^$' \
        | sort -u \
        | tr '\n' ' ')"
    err "release $TAG has no asset for this platform (tried gnu${TRIPLE_FALLBACK:+ and musl}). Available: ${available:-none}"
fi

# ── Download + verify ────────────────────────────────────────────────────────
printf 'Downloading hyper v%s (%s)...\n' "$RESOLVED_VERSION" "$TRIPLE"
fetch "$ARCHIVE_URL" "$TMP_DIR/$ASSET" || err "download failed: $ARCHIVE_URL"
fetch "$SUMS_URL" "$TMP_DIR/SHA256SUMS" || err "download failed: $SUMS_URL"

EXPECTED=""
while IFS=' ' read -r hash name; do
    name="${name#\*}"
    if [ "$name" = "$ASSET" ]; then
        EXPECTED="$hash"
    fi
done < "$TMP_DIR/SHA256SUMS"
[ -n "$EXPECTED" ] || err "SHA256SUMS has no entry for $ASSET"

ACTUAL="$(sha256_of "$TMP_DIR/$ASSET")"
if [ "$ACTUAL" != "$EXPECTED" ]; then
    err "SHA256 mismatch for $ASSET: expected $EXPECTED, got $ACTUAL"
fi
printf 'Checksum verified.\n'

# ── Extract + install ────────────────────────────────────────────────────────
tar -xzf "$TMP_DIR/$ASSET" -C "$TMP_DIR" || err "failed to extract $ASSET"
[ -f "$TMP_DIR/hyper" ] || err "archive $ASSET does not contain a 'hyper' binary"
chmod 0755 "$TMP_DIR/hyper"

DOWNLOADS_DIR="$HYPER_HOME/downloads"
BIN_DIR="$HYPER_HOME/bin"
mkdir -p "$DOWNLOADS_DIR" "$BIN_DIR"

# Smoke-test the extracted binary before touching either live component.
# A bad binary must not leave a new bundle paired with the old executable.
"$TMP_DIR/hyper" --version >/dev/null 2>&1 \
    || err "downloaded binary failed smoke test; existing install left untouched"

VERSIONED="hyper-${RESOLVED_VERSION}-${PLATFORM_OS}-${PLATFORM_ARCH}"
VERSIONED_PATH="$DOWNLOADS_DIR/$VERSIONED"
VERSIONED_STAGE="$DOWNLOADS_DIR/$VERSIONED.install.$$"
VERSIONED_ASIDE="$DOWNLOADS_DIR/$VERSIONED.old.$$"
rm -f "$VERSIONED_STAGE" "$VERSIONED_ASIDE"
mv "$TMP_DIR/hyper" "$VERSIONED_STAGE"

# Stage the installer-owned bundle as a complete immutable tree. Whole-tree
# replacement removes stale managed files; user skills remain in GROK_HOME/skills.
GROK_HOME="${GROK_HOME:-$HOME/.grok}"
BUNDLE_STAGE=""
BUNDLE_ASIDE="$GROK_HOME/bundled.old.$$"
if [ -d "$TMP_DIR/bundled" ]; then
    mkdir -p "$GROK_HOME"
    BUNDLE_STAGE="$GROK_HOME/bundled.install.$$"
    rm -rf "$BUNDLE_STAGE" "$BUNDLE_ASIDE"
    cp -R "$TMP_DIR/bundled" "$BUNDLE_STAGE" \
        || { rm -f "$VERSIONED_STAGE"; err "failed to stage bundled runtime assets; existing install left untouched"; }
fi

TMP_LINK="$BIN_DIR/hyper.install.$$"
ln -s "../downloads/$VERSIONED" "$TMP_LINK" \
    || { rm -rf "$BUNDLE_STAGE"; rm -f "$VERSIONED_STAGE"; err "failed to stage active hyper link"; }

OLD_TARGET=""
if [ -L "$BIN_DIR/hyper" ]; then
    OLD_TARGET="$(readlink "$BIN_DIR/hyper" || true)"
elif [ -e "$BIN_DIR/hyper" ]; then
    rm -f "$TMP_LINK" "$VERSIONED_STAGE"; rm -rf "$BUNDLE_STAGE"
    err "$BIN_DIR/hyper is not a managed symlink; refusing to overwrite it"
fi

if [ -e "$VERSIONED_PATH" ]; then
    mv "$VERSIONED_PATH" "$VERSIONED_ASIDE" \
        || { rm -f "$TMP_LINK" "$VERSIONED_STAGE"; rm -rf "$BUNDLE_STAGE"; err "failed to preserve existing versioned binary"; }
fi
if ! mv "$VERSIONED_STAGE" "$VERSIONED_PATH"; then
    [ ! -e "$VERSIONED_ASIDE" ] || mv "$VERSIONED_ASIDE" "$VERSIONED_PATH" || true
    rm -f "$TMP_LINK"; rm -rf "$BUNDLE_STAGE"
    err "failed to activate versioned binary; previous install restored"
fi

if [ -n "$BUNDLE_STAGE" ]; then
    if [ -e "$GROK_HOME/bundled" ]; then
        mv "$GROK_HOME/bundled" "$BUNDLE_ASIDE" \
            || { rm -f "$TMP_LINK" "$VERSIONED_PATH"; [ ! -e "$VERSIONED_ASIDE" ] || mv "$VERSIONED_ASIDE" "$VERSIONED_PATH" || true; rm -rf "$BUNDLE_STAGE"; err "failed to preserve existing bundled runtime"; }
    fi
    if ! mv "$BUNDLE_STAGE" "$GROK_HOME/bundled"; then
        [ ! -e "$BUNDLE_ASIDE" ] || mv "$BUNDLE_ASIDE" "$GROK_HOME/bundled" || true
        rm -f "$TMP_LINK" "$VERSIONED_PATH"
        [ ! -e "$VERSIONED_ASIDE" ] || mv "$VERSIONED_ASIDE" "$VERSIONED_PATH" || true
        err "failed to activate bundled runtime; previous install restored"
    fi
fi

if ! mv -f "$TMP_LINK" "$BIN_DIR/hyper"; then
    if [ -n "$BUNDLE_STAGE" ]; then
        rm -rf "$GROK_HOME/bundled"
        [ ! -e "$BUNDLE_ASIDE" ] || mv "$BUNDLE_ASIDE" "$GROK_HOME/bundled" || true
    fi
    rm -f "$VERSIONED_PATH"
    [ ! -e "$VERSIONED_ASIDE" ] || mv "$VERSIONED_ASIDE" "$VERSIONED_PATH" || true
    err "failed to activate hyper binary; previous install restored"
fi
rm -rf "$BUNDLE_ASIDE"
rm -f "$VERSIONED_ASIDE"

printf '\nhyper v%s installed to %s\n' "$RESOLVED_VERSION" "$BIN_DIR/hyper"

case ":$PATH:" in
    *":$BIN_DIR:"*)
        printf 'Run `hyper` to get started.\n'
        ;;
    *)
        # Persist BIN_DIR on PATH in the login shell's rc file.
        persist_line() {
            rc="$1"
            line="$2"
            if [ -f "$rc" ] && grep -qF "$BIN_DIR" "$rc"; then
                printf '\n%s is already configured in %s.\n' "$BIN_DIR" "$rc"
                return 0
            fi
            printf '\n# Added by the hyper installer\n%s\n' "$line" >> "$rc" \
                || err "could not write $rc — add hyper to your PATH manually: $line"
            printf '\nAdded %s to your PATH in %s.\n' "$BIN_DIR" "$rc"
        }
        EXPORT_LINE="export PATH=\"$BIN_DIR:\$PATH\""
        case "${SHELL:-}" in
            */zsh)
                persist_line "${ZDOTDIR:-$HOME}/.zshrc" "$EXPORT_LINE"
                ;;
            */bash)
                if [ "$PLATFORM_OS" = "macos" ]; then
                    persist_line "$HOME/.bash_profile" "$EXPORT_LINE"
                else
                    persist_line "$HOME/.bashrc" "$EXPORT_LINE"
                fi
                ;;
            */fish)
                FISH_CONF_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/fish"
                mkdir -p "$FISH_CONF_DIR"
                persist_line "$FISH_CONF_DIR/config.fish" "fish_add_path $BIN_DIR"
                ;;
            *)
                persist_line "$HOME/.profile" "$EXPORT_LINE"
                ;;
        esac
        printf 'Open a new terminal, then run `hyper` to get started.\n'
        ;;
esac
