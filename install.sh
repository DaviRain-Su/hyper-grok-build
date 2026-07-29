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

is_semver() {
    printf '%s\n' "$1" \
        | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$'
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
if [ -n "$VERSION" ] && ! is_semver "$VERSION"; then
    err "invalid version '$VERSION' (expected X.Y.Z or vX.Y.Z)"
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
# Optional: set GITHUB_TOKEN to authenticate the fixed GitHub API endpoint and
# avoid the unauthenticated rate limit (60 req/hr per IP). Never forward the
# token to release-asset hosts or a custom test endpoint.
AUTH_HDR=""
if [ -n "${GITHUB_TOKEN:-}" ]; then
    AUTH_HDR="Authorization: Bearer $GITHUB_TOKEN"
fi

is_fixed_github_api_url() {
    case "$1" in
        "https://api.github.com/repos/${REPO}/releases/"*) return 0 ;;
        *) return 1 ;;
    esac
}

if command -v curl >/dev/null 2>&1; then
    fetch() { curl -fsSL -o "$2" "$1"; }
    fetch_stdout() {
        if [ -n "$AUTH_HDR" ] && is_fixed_github_api_url "$1"; then
            curl -fsSL -H "$AUTH_HDR" "$1"
        else
            curl -fsSL "$1"
        fi
    }
elif command -v wget >/dev/null 2>&1; then
    fetch() { wget -q -O "$2" "$1"; }
    fetch_stdout() {
        if [ -n "$AUTH_HDR" ] && is_fixed_github_api_url "$1"; then
            wget -q --header="$AUTH_HDR" -O - "$1"
        else
            wget -q -O - "$1"
        fi
    }
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
STAGED=""
TMP_LINK=""
STATE_TMP=""
cleanup() {
    rm -rf "$TMP_DIR"
    [ -z "$STAGED" ] || rm -f "$STAGED"
    [ -z "$TMP_LINK" ] || rm -f "$TMP_LINK"
    [ -z "$STATE_TMP" ] || rm -f "$STATE_TMP"
}
trap cleanup EXIT HUP INT TERM

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
    | sed 's/"tag_name"/\
"tag_name"/g' \
    | sed -n 's/^[[:space:]]*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
    | head -n 1)"
[ -n "$TAG" ] || err "release metadata has no tag_name (endpoint: $RELEASE_URL)"
case "$TAG" in
    v*) ;;
    *) err "release tag '$TAG' is invalid (expected vX.Y.Z)" ;;
esac
RESOLVED_VERSION="${TAG#v}"
is_semver "$RESOLVED_VERSION" \
    || err "release tag '$TAG' is invalid (expected semantic version vX.Y.Z)"
if [ -n "$VERSION" ] && [ "$RESOLVED_VERSION" != "$VERSION" ]; then
    err "requested version $VERSION but release tag is $TAG"
fi

# Pull every browser_download_url out of the JSON. Asset selection below uses
# an exact URL suffix and rejects missing or duplicate names.
URLS="$(printf '%s' "$RELEASE_JSON" \
    | sed 's/"browser_download_url"/\
"browser_download_url"/g' \
    | sed -n 's/^[[:space:]]*"browser_download_url"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p')"
find_asset_url() {
    suffix="/$1"
    printf '%s\n' "$URLS" | awk -v suffix="$suffix" '
        length($0) >= length(suffix) &&
        substr($0, length($0) - length(suffix) + 1) == suffix {
            count++
            found = $0
        }
        END {
            if (count == 1) print found
            else exit 1
        }
    '
}
if ! SUMS_URL="$(find_asset_url "SHA256SUMS")"; then
    err "release $TAG must contain exactly one SHA256SUMS asset"
fi

# Resolve archive: preferred triple, then Linux gnu fallback when present.
ASSET=""
ARCHIVE_URL=""
for cand in "$TRIPLE" ${TRIPLE_FALLBACK:-}; do
    [ -n "$cand" ] || continue
    trial="hyper-${RESOLVED_VERSION}-${cand}.tar.gz"
    if found="$(find_asset_url "$trial")"; then
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

MANIFEST_SIZE="$(wc -c < "$TMP_DIR/SHA256SUMS" | tr -d '[:space:]')"
ARCHIVE_SIZE="$(wc -c < "$TMP_DIR/$ASSET" | tr -d '[:space:]')"
[ "$MANIFEST_SIZE" -le 1048576 ] || err "SHA256SUMS is unexpectedly large"
[ "$ARCHIVE_SIZE" -le 1073741824 ] || err "$ASSET exceeds the 1 GiB safety limit"

EXPECTED=""
EXPECTED_COUNT=0
while IFS=' ' read -r hash name; do
    name="${name#\*}"
    if [ "$name" = "$ASSET" ]; then
        EXPECTED="$hash"
        EXPECTED_COUNT=$((EXPECTED_COUNT + 1))
    fi
done < "$TMP_DIR/SHA256SUMS"
[ "$EXPECTED_COUNT" -eq 1 ] \
    || err "SHA256SUMS must contain exactly one entry for $ASSET"
case "$EXPECTED" in
    *[!0-9A-Fa-f]*|'') err "SHA256SUMS contains an invalid digest for $ASSET" ;;
esac
[ "${#EXPECTED}" -eq 64 ] || err "SHA256SUMS contains an invalid digest for $ASSET"
EXPECTED="$(printf '%s' "$EXPECTED" | tr 'A-F' 'a-f')"
ACTUAL="$(sha256_of "$TMP_DIR/$ASSET" | tr 'A-F' 'a-f')"
if [ "$ACTUAL" != "$EXPECTED" ]; then
    err "SHA256 mismatch for $ASSET: expected $EXPECTED, got $ACTUAL"
fi
printf 'Checksum verified.\n'

# ── Extract + install ────────────────────────────────────────────────────────
# Extract only trusted members: the root-level binary, plus optional
# installer-owned `bundled/` assets (no `..` path segments).
tar -tzf "$TMP_DIR/$ASSET" > "$TMP_DIR/archive.list" \
    || err "failed to inspect $ASSET"
if ! BINARY_MEMBER="$(awk '
    $0 == "hyper" || $0 == "./hyper" { count++; member = $0 }
    END { if (count == 1) print member; else exit 1 }
' "$TMP_DIR/archive.list")"; then
    err "archive $ASSET must contain exactly one root-level hyper binary"
fi
tar -xOzf "$TMP_DIR/$ASSET" "$BINARY_MEMBER" > "$TMP_DIR/hyper" \
    || err "failed to extract hyper from $ASSET"
[ -s "$TMP_DIR/hyper" ] || err "archive $ASSET contains an empty hyper binary"
BINARY_SIZE="$(wc -c < "$TMP_DIR/hyper" | tr -d '[:space:]')"
[ "$BINARY_SIZE" -le 1073741824 ] || err "extracted hyper exceeds the 1 GiB safety limit"
chmod 0755 "$TMP_DIR/hyper"

# Optionally extract the installer-owned `bundled/` tree for resume-session
# skills and related runtime assets packaged in the release archive.
#
# Only list *file* members for `tar -T`. Archives created with
# `tar -C staging -czf archive .` store directories as names ending in `/`.
# GNU tar (1.35) strips that trailing slash when matching `-T` names, fails
# to find the directory entry, and then reports subsequent file members as
# missing too — even though `tar -tz` listed them. Parent directories are
# created automatically when the files are extracted.
: > "$TMP_DIR/bundled.members"
while IFS= read -r member; do
    case "$member" in
        bundled|bundled/*|./bundled|./bundled/*) ;;
        *) continue ;;
    esac
    # Directory members end with `/` in these archives; skip them.
    case "$member" in
        */) continue ;;
    esac
    case "$member" in
        *..*) err "archive $ASSET contains an unsafe bundled path: $member" ;;
    esac
    printf '%s\n' "$member" >> "$TMP_DIR/bundled.members"
done < "$TMP_DIR/archive.list"
if [ -s "$TMP_DIR/bundled.members" ]; then
    tar -xzf "$TMP_DIR/$ASSET" -C "$TMP_DIR" -T "$TMP_DIR/bundled.members" \
        || err "failed to extract bundled runtime assets from $ASSET"
fi

ensure_directory() {
    path="$1"
    label="$2"
    [ ! -L "$path" ] || err "refusing to use symlinked $label: $path"
    if [ -e "$path" ]; then
        [ -d "$path" ] || err "$label is not a directory: $path"
    else
        mkdir -p "$path" || err "could not create $label: $path"
    fi
}
DOWNLOADS_DIR="$HYPER_HOME/downloads"
BIN_DIR="$HYPER_HOME/bin"
ensure_directory "$HYPER_HOME" "Hyper install root"
ensure_directory "$DOWNLOADS_DIR" "Hyper downloads directory"
ensure_directory "$BIN_DIR" "Hyper bin directory"

# The archive digest is part of the deployment identity. A deliberately
# republished tag therefore gets a new path and cannot overwrite the active
# same-semver binary before its smoke test succeeds.
VERSIONED="hyper-${RESOLVED_VERSION}-${PLATFORM_OS}-${PLATFORM_ARCH}-sha256-${EXPECTED}"
DEST="$DOWNLOADS_DIR/$VERSIONED"
STAGED="$(mktemp "$DOWNLOADS_DIR/.hyper-stage.XXXXXX")" \
    || err "could not create a staged binary under $DOWNLOADS_DIR"
cp "$TMP_DIR/hyper" "$STAGED" || err "could not stage downloaded hyper"
chmod 0755 "$STAGED"
"$STAGED" --version >/dev/null 2>&1 \
    || err "downloaded binary failed smoke test; existing install left untouched"
mv -f "$STAGED" "$DEST"
STAGED=""

# Stage the installer-owned bundle as a complete immutable tree. Whole-tree
# replacement removes stale managed files; user skills remain in GROK_HOME/skills.
GROK_HOME="${GROK_HOME:-$HOME/.grok}"
BUNDLE_STAGE=""
BUNDLE_ASIDE="$GROK_HOME/bundled.old.$$"
if [ -d "$TMP_DIR/bundled" ]; then
    ensure_directory "$GROK_HOME" "Grok home"
    BUNDLE_STAGE="$GROK_HOME/bundled.install.$$"
    rm -rf "$BUNDLE_STAGE" "$BUNDLE_ASIDE"
    cp -R "$TMP_DIR/bundled" "$BUNDLE_STAGE" \
        || err "failed to stage bundled runtime assets; existing install left untouched"
fi

TMP_LINK="$BIN_DIR/hyper.install.$$"
[ ! -e "$TMP_LINK" ] && [ ! -L "$TMP_LINK" ] \
    || { rm -rf "$BUNDLE_STAGE"; err "temporary activation path already exists: $TMP_LINK"; }
ln -s "../downloads/$VERSIONED" "$TMP_LINK" \
    || { rm -rf "$BUNDLE_STAGE"; err "failed to stage active hyper link"; }
mv -f "$TMP_LINK" "$BIN_DIR/hyper" \
    || { rm -f "$TMP_LINK"; rm -rf "$BUNDLE_STAGE"; err "failed to activate hyper binary"; }
TMP_LINK=""

# Record the exact release-archive identity used by the in-app community
# updater. This lets Hyper detect a deliberately republished tag by checksum
# without ever consulting the official Grok updater state under ~/.grok.
STATE_FILE="$HYPER_HOME/update-state.json"
STATE_TMP="$(mktemp "$HYPER_HOME/.update-state.XXXXXX")" \
    || { rm -rf "$BUNDLE_STAGE"; err "could not create temporary update state under $HYPER_HOME"; }
CHECKED_AT="$(date -u +%s)"
case "$CHECKED_AT" in
    *[!0-9]*|'') rm -rf "$BUNDLE_STAGE"; err "could not determine the current Unix timestamp" ;;
esac
# These fields are safe to serialize directly: version/tag and asset names are
# constrained above, the digest is exactly 64 hex characters, and the managed
# filename is composed only from those validated values.
printf '{\n  "installed_version": "%s",\n  "installed_asset": "%s",\n  "installed_sha256": "%s",\n  "installed_binary": "%s",\n  "checked_at_unix": %s\n}\n' \
    "$RESOLVED_VERSION" "$ASSET" "$EXPECTED" "$VERSIONED" "$CHECKED_AT" > "$STATE_TMP"
mv -f "$STATE_TMP" "$STATE_FILE" \
    || { rm -rf "$BUNDLE_STAGE"; err "could not record Hyper update state"; }
STATE_TMP=""

# Activate the complete bundle after the binary is known-good.
if [ -n "$BUNDLE_STAGE" ]; then
    if [ -e "$GROK_HOME/bundled" ]; then
        mv "$GROK_HOME/bundled" "$BUNDLE_ASIDE" \
            || { rm -rf "$BUNDLE_STAGE"; err "failed to preserve existing bundled runtime"; }
    fi
    if ! mv "$BUNDLE_STAGE" "$GROK_HOME/bundled"; then
        [ ! -e "$BUNDLE_ASIDE" ] || mv "$BUNDLE_ASIDE" "$GROK_HOME/bundled" || true
        err "failed to activate bundled runtime; previous bundle restored if available"
    fi
fi
rm -rf "$BUNDLE_ASIDE"

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
