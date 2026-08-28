#!/bin/sh
#
# Hyper installer (macOS / Linux).
#
# Downloads the matching platform artifact from this repo's GitHub Releases,
# verifies its SHA-256 against the release's SHA256SUMS manifest, and installs
# the binary as ~/.hyper/bin/hyper (versioned binary in ~/.hyper/downloads/,
# atomic symlink in bin/).
#
# Usage — pipe the GitHub *Release asset*, never a git branch:
#   curl -fsSL https://github.com/DaviRain-Su/hyper-grok-build/releases/latest/download/install.sh | sh
#   sh install.sh --version v1.0.10-r1      # pin a specific release
#
# Do not pipe a git branch. The default-branch copy is source, not the
# install path (issue #46).
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
BUNDLE_STAGE=""
BUNDLE_ASIDE=""
BINARY_ASIDE=""
STATE_ASIDE=""
# Live-path renames vs successful activations are tracked separately so a
# mid-transaction failure can restore only what was actually moved aside.
ACTIVATED_BINARY=0
ACTIVATED_STATE=0
ACTIVATED_BUNDLE=0
MOVED_BINARY_ASIDE=0
MOVED_STATE_ASIDE=0
MOVED_BUNDLE_ASIDE=0
PREV_LINK_TARGET=""
PREV_STATE_BYTES=""
HAD_PREV_BINARY=0
HAD_PREV_STATE=0
HAD_PREV_BUNDLE=0
PREV_BINARY_KIND="" # symlink | regular | missing
cleanup() {
    rm -rf "$TMP_DIR"
    [ -z "$STAGED" ] || rm -f "$STAGED"
    [ -z "$TMP_LINK" ] || rm -f "$TMP_LINK"
    [ -z "$STATE_TMP" ] || rm -f "$STATE_TMP"
    # Incomplete stages only — never remove an activated live path here.
    if [ "$ACTIVATED_BUNDLE" -eq 0 ]; then
        [ -z "$BUNDLE_STAGE" ] || rm -rf "$BUNDLE_STAGE"
    fi
}
trap cleanup EXIT HUP INT TERM

# Append a secondary rollback failure without aborting the rest of rollback.
report_rollback_error() {
    ROLLBACK_ERRORS="${ROLLBACK_ERRORS}
rollback error: $*"
}

# Fail closed after a commit error, reporting any incomplete rollback steps.
fail_with_rollback() {
    commit_msg="$*"
    if [ -n "${ROLLBACK_ERRORS:-}" ]; then
        err "install failed and rollback was incomplete; installation may be inconsistent.

commit error: ${commit_msg}${ROLLBACK_ERRORS}"
    fi
    err "$commit_msg"
}

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

# Production downloads must come from this repo's GitHub Releases. Tests set
# HYPER_UPDATE_BASE_URL to a local fixture and may use that origin only.
assert_allowed_download_url() {
    case "$1" in
        "https://github.com/${REPO}/releases/download/"*) return 0 ;;
    esac
    if [ -n "${HYPER_UPDATE_BASE_URL:-}" ]; then
        case "$1" in
            "${HYPER_UPDATE_BASE_URL}"*) return 0 ;;
        esac
    fi
    err "refusing download from unexpected host: $1"
}
assert_allowed_download_url "$SUMS_URL"
assert_allowed_download_url "$ARCHIVE_URL"

# ── Download + verify ────────────────────────────────────────────────────────
printf 'Downloading hyper v%s (%s)...\n' "$RESOLVED_VERSION" "$TRIPLE"
fetch "$ARCHIVE_URL" "$TMP_DIR/$ASSET" || err "download failed: $ARCHIVE_URL"
fetch "$SUMS_URL" "$TMP_DIR/SHA256SUMS" || err "download failed: $SUMS_URL"

MANIFEST_SIZE="$(wc -c < "$TMP_DIR/SHA256SUMS" | tr -d '[:space:]')"
ARCHIVE_SIZE="$(wc -c < "$TMP_DIR/$ASSET" | tr -d '[:space:]')"
[ "$MANIFEST_SIZE" -le 1048576 ] || err "SHA256SUMS is unexpectedly large"
[ "$ARCHIVE_SIZE" -le 1073741824 ] || err "$ASSET exceeds the 1 GiB safety limit"

# Strict whole-manifest parse: every non-empty line must be
#   <64 hex> <space or " *"><basename>
# Basename is a single path segment (no /, \, control, spaces). Duplicate and
# case-colliding names fail closed. Exactly one entry must name $ASSET.
EXPECTED=""
EXPECTED_COUNT=0
: > "$TMP_DIR/sums.seen"
LINE_NO=0
while IFS= read -r line || [ -n "$line" ]; do
    LINE_NO=$((LINE_NO + 1))
    # Skip blank lines only.
    case "$line" in
        ''|*[![:space:]]*) ;;
    esac
    trimmed="$(printf '%s' "$line" | sed 's/^[[:space:]]*//;s/[[:space:]]*$//')"
    [ -n "$trimmed" ] || continue
    # Reject control characters.
    case "$trimmed" in
        *[![:print:]]*) err "SHA256SUMS line $LINE_NO contains control characters" ;;
    esac
    # Exactly two whitespace-separated fields after collapse:
    #   <64 hex>  [*]basename
    # (GNU sha256sum uses two spaces; awk collapses runs of whitespace.)
    nf="$(printf '%s\n' "$trimmed" | awk '{print NF}')"
    [ "$nf" = "2" ] || err "SHA256SUMS line $LINE_NO has trailing fields or is malformed"
    hash="$(printf '%s\n' "$trimmed" | awk '{print $1}')"
    rest="$(printf '%s\n' "$trimmed" | awk '{print $2}')"
    [ -n "$hash" ] && [ -n "$rest" ] \
        || err "SHA256SUMS line $LINE_NO is malformed"
    name="${rest#\*}"
    case "$hash" in
        *[!0-9A-Fa-f]*|'') err "SHA256SUMS line $LINE_NO has an invalid digest" ;;
    esac
    [ "${#hash}" -eq 64 ] || err "SHA256SUMS line $LINE_NO has an invalid digest"
    # Basename only: [A-Za-z0-9._+-]+ (release asset charset).
    case "$name" in
        ''|*/*|*\\*|*..*) err "SHA256SUMS line $LINE_NO has an illegal asset name: $name" ;;
        *[!A-Za-z0-9._+-]*) err "SHA256SUMS line $LINE_NO has an illegal asset name: $name" ;;
    esac
    fold="$(printf '%s' "$name" | tr 'A-Z' 'a-z')"
    if grep -Fqx "$fold" "$TMP_DIR/sums.seen" 2>/dev/null; then
        err "SHA256SUMS contains duplicate or case-colliding entry for $name"
    fi
    printf '%s\n' "$fold" >> "$TMP_DIR/sums.seen"
    if [ "$name" = "$ASSET" ]; then
        EXPECTED="$(printf '%s' "$hash" | tr 'A-F' 'a-f')"
        EXPECTED_COUNT=$((EXPECTED_COUNT + 1))
    fi
done < "$TMP_DIR/SHA256SUMS"
[ "$EXPECTED_COUNT" -eq 1 ] \
    || err "SHA256SUMS must contain exactly one entry for $ASSET"
ACTUAL="$(sha256_of "$TMP_DIR/$ASSET" | tr 'A-F' 'a-f')"
if [ "$ACTUAL" != "$EXPECTED" ]; then
    err "SHA256 mismatch for $ASSET: expected $EXPECTED, got $ACTUAL"
fi
printf 'Checksum verified.\n'

# ── Extract + install ────────────────────────────────────────────────────────
# Strict pre-scan of tar members (type + path) before any extract. Rejects
# traversal, absolute/backslash paths, symlink/hardlink/special types,
# duplicate/case collisions, size/entry budgets, and unexpected root entries.
# CI also gates archives with the Rust community verifier; this is defense in
# depth for installers run outside CI.
MAX_ARCHIVE_ENTRIES=4096
MAX_BINARY_BYTES=1073741824
MAX_BUNDLE_FILE_BYTES=33554432
MAX_BUNDLE_TOTAL_BYTES=536870912
MAX_BUNDLE_FILES=4096

# Detect tar family and list with LC_ALL=C for stable field layouts.
# GNU tar  -tv: mode owner/group size date time name
# bsdtar   -tv: mode links owner group size mon day time name
export LC_ALL=C
TAR_VERSION="$(tar --version 2>&1 | head -n 1 || true)"
case "$TAR_VERSION" in
    *GNU*|*tar\ \(GNU*) TAR_FLAVOR=gnu ;;
    *bsdtar*|*libarchive*|*bsdtar\ *) TAR_FLAVOR=bsd ;;
    *)
        # macOS ships bsdtar as `tar` and often prints "bsdtar …" or
        # "tar (bsdtar)". Fall back via feature probe.
        if tar --help 2>&1 | grep -qi 'bsdtar\|libarchive'; then
            TAR_FLAVOR=bsd
        elif tar --version 2>&1 | grep -qi gnu; then
            TAR_FLAVOR=gnu
        else
            err "unsupported tar implementation (need GNU tar or bsdtar): $TAR_VERSION"
        fi
        ;;
esac

tar -tvzf "$TMP_DIR/$ASSET" > "$TMP_DIR/archive.tv" \
    || err "failed to inspect $ASSET"
ENTRY_COUNT="$(wc -l < "$TMP_DIR/archive.tv" | tr -d '[:space:]')"
[ "$ENTRY_COUNT" -le "$MAX_ARCHIVE_ENTRIES" ] \
    || err "archive $ASSET contains too many entries ($ENTRY_COUNT > $MAX_ARCHIVE_ENTRIES)"

# Normalize a member name: strip one leading ./ and at most one trailing /
# for directory markers. Explicitly reject mid-path '//' (do not collapse).
# Prints normalized relative path on stdout; exits 1 if unsafe.
normalize_member() {
    raw="$1"
    case "$raw" in
        *[![:print:]]*) return 1 ;;
        *\\*) return 1 ;;
        /*) return 1 ;;
        [A-Za-z]:*) return 1 ;;
    esac
    # Mid-path empty components (//) are never accepted.
    case "$raw" in
        *//*) return 1 ;;
    esac
    # Strip a single leading ./
    case "$raw" in
        ./) raw="." ;;
        ./*) raw="${raw#./}" ;;
    esac
    # Directory markers may end with exactly one trailing /; strip it for
    # classification while still rejecting // (handled above).
    is_dir_marker=0
    case "$raw" in
        */)
            is_dir_marker=1
            raw="${raw%/}"
            ;;
    esac
    if [ -z "$raw" ] || [ "$raw" = "." ]; then
        printf '%s\n' ""
        return 0
    fi
    # Component walk without IFS-split collapse: use parameter expansion.
    rest="$raw"
    while [ -n "$rest" ]; do
        case "$rest" in
            */*)
                part="${rest%%/*}"
                rest="${rest#*/}"
                ;;
            *)
                part="$rest"
                rest=""
                ;;
        esac
        case "$part" in
            ''|.|..) return 1 ;;
            *:*|*[' ']*) return 1 ;;
            *.) return 1 ;;
        esac
    done
    # Re-emit without trailing slash (classification uses type char for dirs).
    printf '%s\n' "$raw"
    # Silence unused in pure-POSIX shells that keep is_dir_marker.
    : "$is_dir_marker"
}

# Parse one listing line into TYPE|SIZE|NAME. Flavor-specific, no heuristics.
# Prints "typec size member" or exits 1.
parse_tar_tv_line() {
    line="$1"
    mode="${line%% *}"
    [ -n "$mode" ] || return 1
    typec="$(printf '%s' "$mode" | cut -c1)"
    case "$TAR_FLAVOR" in
        gnu)
            # mode owner/group size YYYY-MM-DD HH:MM name...
            # $2 must contain '/' (owner/group). Name starts at field 6.
            printf '%s\n' "$line" | awk -v typec="$typec" '
                BEGIN { OFS = "\t" }
                {
                    if (NF < 6) exit 1
                    if ($2 !~ /\//) exit 1
                    size = $3
                    if (size !~ /^[0-9]+$/) exit 1
                    name = $6
                    for (i = 7; i <= NF; i++) {
                        if ($i == "->") break
                        name = name " " $i
                    }
                    if (name == "" || name ~ /[[:cntrl:]]/) exit 1
                    print typec, size, name
                    exit 0
                }
            '
            ;;
        bsd)
            # mode links owner group size Mon DD HH:MM name...
            # Name starts at field 9.
            printf '%s\n' "$line" | awk -v typec="$typec" '
                BEGIN { OFS = "\t" }
                {
                    if (NF < 9) exit 1
                    size = $5
                    if (size !~ /^[0-9]+$/) exit 1
                    name = $9
                    for (i = 10; i <= NF; i++) {
                        if ($i == "->" || $i == "link") break
                        name = name " " $i
                    }
                    if (name == "" || name ~ /[[:cntrl:]]/) exit 1
                    print typec, size, name
                    exit 0
                }
            '
            ;;
        *) return 1 ;;
    esac
}

: > "$TMP_DIR/archive.seen"
: > "$TMP_DIR/bundled.members"
BINARY_MEMBER=""
BINARY_COUNT=0
BUNDLE_FILE_COUNT=0
BUNDLE_TOTAL=0
FOUND_BUNDLE=0

while IFS= read -r line; do
    [ -n "$line" ] || continue
    parsed="$(parse_tar_tv_line "$line")" \
        || err "archive $ASSET has an unparseable $TAR_FLAVOR listing line: $line"
    typec="$(printf '%s' "$parsed" | cut -f1)"
    size="$(printf '%s' "$parsed" | cut -f2)"
    member="$(printf '%s' "$parsed" | cut -f3-)"
    # Strip hardlink/symlink target if a parser left it (should not).
    case "$member" in
        *" -> "*) member="${member%% -> *}" ;;
    esac

    norm="$(normalize_member "$member")" || err "archive $ASSET contains an unsafe path: $member"
    # Root placeholder (`.`): only directories allowed.
    if [ -z "$norm" ]; then
        case "$typec" in
            d) continue ;;
            *) err "archive $ASSET root entry has unsupported type ($typec)" ;;
        esac
    fi

    # Case-fold collision / duplicate detection.
    fold="$(printf '%s' "$norm" | tr 'A-Z' 'a-z')"
    if grep -Fqx "$fold" "$TMP_DIR/archive.seen" 2>/dev/null; then
        err "archive $ASSET contains duplicate or case-colliding entry: $norm"
    fi
    printf '%s\n' "$fold" >> "$TMP_DIR/archive.seen"

    # Reject non-regular / non-directory types entirely (before any extract).
    case "$typec" in
        -|d) ;;
        l) err "archive $ASSET contains a symlink: $norm" ;;
        h) err "archive $ASSET contains a hardlink: $norm" ;;
        b|c|p|s) err "archive $ASSET contains a special file type ($typec): $norm" ;;
        *) err "archive $ASSET contains unsupported entry type ($typec): $norm" ;;
    esac

    # Classify path.
    case "$norm" in
        hyper)
            [ "$typec" = "-" ] || err "archive $ASSET hyper entry must be a regular file"
            BINARY_COUNT=$((BINARY_COUNT + 1))
            BINARY_MEMBER="$member"
            ;;
        LICENSE|NOTICE|THIRD-PARTY-NOTICES|THIRD-PARTY-NOTICES.md)
            [ "$typec" = "-" ] || err "archive $ASSET notice entry must be a regular file: $norm"
            ;;
        bundled)
            [ "$typec" = "d" ] || err "archive $ASSET bundled entry must be a directory"
            FOUND_BUNDLE=1
            ;;
        bundled/*)
            FOUND_BUNDLE=1
            if [ "$typec" = "-" ]; then
                BUNDLE_FILE_COUNT=$((BUNDLE_FILE_COUNT + 1))
                [ "$BUNDLE_FILE_COUNT" -le "$MAX_BUNDLE_FILES" ] \
                    || err "archive $ASSET bundle has too many files"
                case "$size" in
                    *[!0-9]*|'') err "archive $ASSET has invalid size for $norm" ;;
                esac
                [ "$size" -le "$MAX_BUNDLE_FILE_BYTES" ] \
                    || err "archive $ASSET bundle file exceeds per-file limit: $norm"
                BUNDLE_TOTAL=$((BUNDLE_TOTAL + size))
                [ "$BUNDLE_TOTAL" -le "$MAX_BUNDLE_TOTAL_BYTES" ] \
                    || err "archive $ASSET bundle exceeds total size limit"
                printf '%s\n' "$member" >> "$TMP_DIR/bundled.members"
            fi
            ;;
        *)
            err "archive $ASSET contains unexpected entry: $norm"
            ;;
    esac
done < "$TMP_DIR/archive.tv"

[ "$BINARY_COUNT" -eq 1 ] \
    || err "archive $ASSET must contain exactly one root-level hyper binary"
[ -n "$BINARY_MEMBER" ] || err "archive $ASSET missing hyper member name"

# Extract binary to a fresh temp path (stdout, no on-disk traversal).
tar -xOzf "$TMP_DIR/$ASSET" "$BINARY_MEMBER" > "$TMP_DIR/hyper" \
    || err "failed to extract hyper from $ASSET"
[ -s "$TMP_DIR/hyper" ] || err "archive $ASSET contains an empty hyper binary"
BINARY_SIZE="$(wc -c < "$TMP_DIR/hyper" | tr -d '[:space:]')"
[ "$BINARY_SIZE" -le "$MAX_BINARY_BYTES" ] || err "extracted hyper exceeds the 1 GiB safety limit"
# No-follow: refuse if extract somehow produced a symlink.
[ ! -L "$TMP_DIR/hyper" ] || err "extracted hyper is a symlink"
chmod 0755 "$TMP_DIR/hyper"

# Extract bundled files into a fresh subdir, then no-follow scan.
if [ -s "$TMP_DIR/bundled.members" ]; then
    EXTRACT_ROOT="$TMP_DIR/extract-root"
    mkdir -p "$EXTRACT_ROOT" || err "could not create extract root"
    # shellcheck disable=SC2039
    tar -xzf "$TMP_DIR/$ASSET" -C "$EXTRACT_ROOT" -T "$TMP_DIR/bundled.members" \
        || err "failed to extract bundled runtime assets from $ASSET"
    # No-follow walk: reject any symlink that snuck in.
    if command -v find >/dev/null 2>&1; then
        if find "$EXTRACT_ROOT" \( -type l -o -type b -o -type c -o -type p -o -type s \) 2>/dev/null | grep -q .; then
            err "extracted bundle contains symlink or special file"
        fi
    fi
    if [ -d "$EXTRACT_ROOT/bundled" ]; then
        mv "$EXTRACT_ROOT/bundled" "$TMP_DIR/bundled" \
            || err "failed to materialize extracted bundled tree"
    fi
    rm -rf "$EXTRACT_ROOT"
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

# ── Layout preflight (all shape/symlink/rw checks before any live rename) ───
DOWNLOADS_DIR="$HYPER_HOME/downloads"
BIN_DIR="$HYPER_HOME/bin"
ensure_directory "$HYPER_HOME" "Hyper install root"
ensure_directory "$DOWNLOADS_DIR" "Hyper downloads directory"
ensure_directory "$BIN_DIR" "Hyper bin directory"

VERSIONED="hyper-${RESOLVED_VERSION}-${PLATFORM_OS}-${PLATFORM_ARCH}-sha256-${EXPECTED}"
DEST="$DOWNLOADS_DIR/$VERSIONED"
ACTIVE_BIN="$BIN_DIR/hyper"
STATE_FILE="$HYPER_HOME/update-state.json"
GROK_HOME="${GROK_HOME:-$HOME/.grok}"
BUNDLE_DEST="$GROK_HOME/bundled"

# Community installs live only under HYPER_HOME (default ~/.hyper) plus optional
# GROK_HOME/bundled. This script never writes ~/.grok/bin/grok.

# Preflight active binary shape — capture only, no renames yet.
if [ -L "$ACTIVE_BIN" ]; then
    HAD_PREV_BINARY=1
    PREV_BINARY_KIND="symlink"
    PREV_LINK_TARGET="$(readlink "$ACTIVE_BIN")" \
        || err "cannot read active hyper symlink at $ACTIVE_BIN"
elif [ -e "$ACTIVE_BIN" ]; then
    [ -f "$ACTIVE_BIN" ] || err "active hyper is not a regular file or symlink: $ACTIVE_BIN"
    [ ! -L "$ACTIVE_BIN" ] || err "active hyper unexpectedly a symlink after -e check"
    HAD_PREV_BINARY=1
    PREV_BINARY_KIND="regular"
else
    PREV_BINARY_KIND="missing"
fi

# Preflight update-state shape + capture bytes (no renames).
if [ -L "$STATE_FILE" ]; then
    err "refusing to replace symlinked update state: $STATE_FILE"
fi
if [ -e "$STATE_FILE" ]; then
    [ -f "$STATE_FILE" ] || err "Hyper update state is not a regular file: $STATE_FILE"
    HAD_PREV_STATE=1
    PREV_STATE_BYTES="$(cat "$STATE_FILE")" \
        || err "cannot read existing update state at $STATE_FILE"
fi

# Preflight bundle destination shape (no renames).
if [ -L "$BUNDLE_DEST" ]; then
    err "refusing to replace symlinked bundled runtime: $BUNDLE_DEST"
fi
if [ -e "$BUNDLE_DEST" ]; then
    [ -d "$BUNDLE_DEST" ] || err "bundled runtime path is not a directory: $BUNDLE_DEST"
    HAD_PREV_BUNDLE=1
fi

# Stage versioned binary under downloads/ (not the active link yet).
STAGED="$(mktemp "$DOWNLOADS_DIR/.hyper-stage.XXXXXX")" \
    || err "could not create a staged binary under $DOWNLOADS_DIR"
cp "$TMP_DIR/hyper" "$STAGED" || err "could not stage downloaded hyper"
chmod 0755 "$STAGED"
"$STAGED" --version >/dev/null 2>&1 \
    || err "downloaded binary failed smoke test; existing install left untouched"
# Versioned path must not be a symlink target we overwrite unsafely.
if [ -L "$DEST" ]; then
    rm -f "$STAGED"
    err "refusing to overwrite symlinked versioned binary path: $DEST"
fi
mv -f "$STAGED" "$DEST"
STAGED=""

# Stage bundle tree under GROK_HOME (same FS as final dest) without touching live.
if [ -d "$TMP_DIR/bundled" ]; then
    ensure_directory "$GROK_HOME" "Grok home"
    BUNDLE_STAGE="$GROK_HOME/bundled.install.$$.$EXPECTED"
    # Preflight: stage path must be free.
    if [ -e "$BUNDLE_STAGE" ] || [ -L "$BUNDLE_STAGE" ]; then
        err "bundle stage path already exists: $BUNDLE_STAGE"
    fi
    # Prefer cp -R without following symlinks when available.
    if cp -R -P "$TMP_DIR/bundled" "$BUNDLE_STAGE" 2>/dev/null; then
        :
    else
        cp -R "$TMP_DIR/bundled" "$BUNDLE_STAGE" \
            || err "failed to stage bundled runtime assets; existing install left untouched"
    fi
    if command -v find >/dev/null 2>&1; then
        if find "$BUNDLE_STAGE" -type l 2>/dev/null | grep -q .; then
            rm -rf "$BUNDLE_STAGE"
            err "staged bundle contains a symlink"
        fi
    fi
fi

# Prepare state payload + temp path (no live rename yet).
CHECKED_AT="$(date -u +%s)"
case "$CHECKED_AT" in
    *[!0-9]*|'') err "could not determine the current Unix timestamp" ;;
esac
STATE_TMP="$(mktemp "$HYPER_HOME/.update-state.XXXXXX")" \
    || err "could not create temporary update state under $HYPER_HOME"
printf '{\n  "installed_version": "%s",\n  "installed_asset": "%s",\n  "installed_sha256": "%s",\n  "installed_binary": "%s",\n  "checked_at_unix": %s\n}\n' \
    "$RESOLVED_VERSION" "$ASSET" "$EXPECTED" "$VERSIONED" "$CHECKED_AT" > "$STATE_TMP"
[ -f "$STATE_TMP" ] && [ ! -L "$STATE_TMP" ] \
    || err "state temp is not a regular file"

# Reserve unique aside / temp-link paths (existence only — still no live renames).
BINARY_ASIDE="$ACTIVE_BIN.old.$$.$EXPECTED"
STATE_ASIDE="$STATE_FILE.old.$$.$EXPECTED"
BUNDLE_ASIDE="$BUNDLE_DEST.old.$$.$EXPECTED"
TMP_LINK="$BIN_DIR/hyper.install.$$.$EXPECTED"
for p in "$BINARY_ASIDE" "$STATE_ASIDE" "$BUNDLE_ASIDE" "$TMP_LINK"; do
    if [ -e "$p" ] || [ -L "$p" ]; then
        rm -rf "$BUNDLE_STAGE"
        err "temporary path already exists: $p"
    fi
done

# Compensating transaction: binary → state → bundle.
# Success is only claimed after all steps complete. Any failure restores the
# previous deployment and reports secondary rollback failures explicitly.
# INJECT_AFTER_STATE_MARKER
ROLLBACK_ERRORS=""

rollback_binary() {
    # Restore when we either activated a new binary or moved the old one aside.
    if [ "$ACTIVATED_BINARY" -eq 0 ] && [ "$MOVED_BINARY_ASIDE" -eq 0 ]; then
        return 0
    fi
    if [ "$PREV_BINARY_KIND" = "symlink" ] && [ -n "$PREV_LINK_TARGET" ]; then
        restore_link="$BIN_DIR/hyper.restore.$$"
        if ln -s "$PREV_LINK_TARGET" "$restore_link" 2>/dev/null \
            && mv -f "$restore_link" "$ACTIVE_BIN" 2>/dev/null; then
            :
        else
            rm -f "$restore_link" 2>/dev/null || true
            report_rollback_error "binary: failed to restore previous symlink target ($PREV_LINK_TARGET)"
        fi
    elif [ "$PREV_BINARY_KIND" = "regular" ] && [ "$MOVED_BINARY_ASIDE" -eq 1 ] \
        && [ -n "$BINARY_ASIDE" ] && [ -e "$BINARY_ASIDE" ]; then
        if ! mv -f "$BINARY_ASIDE" "$ACTIVE_BIN" 2>/dev/null; then
            report_rollback_error "binary: failed to restore previous regular file from $BINARY_ASIDE"
        else
            BINARY_ASIDE=""
            MOVED_BINARY_ASIDE=0
        fi
    else
        # Fresh install or activated over missing prior: remove new link/file.
        if [ -e "$ACTIVE_BIN" ] || [ -L "$ACTIVE_BIN" ]; then
            if ! rm -f "$ACTIVE_BIN" 2>/dev/null; then
                report_rollback_error "binary: failed to remove partially activated $ACTIVE_BIN"
            fi
        fi
        # If we had moved a regular aside and activation never completed, put it back.
        if [ "$MOVED_BINARY_ASIDE" -eq 1 ] && [ -n "$BINARY_ASIDE" ] && [ -e "$BINARY_ASIDE" ]; then
            if ! mv -f "$BINARY_ASIDE" "$ACTIVE_BIN" 2>/dev/null; then
                report_rollback_error "binary: failed to restore aside $BINARY_ASIDE after failed activation"
            else
                MOVED_BINARY_ASIDE=0
            fi
        fi
    fi
    ACTIVATED_BINARY=0
}

rollback_state() {
    if [ "$ACTIVATED_STATE" -eq 0 ] && [ "$MOVED_STATE_ASIDE" -eq 0 ]; then
        return 0
    fi
    if [ "$HAD_PREV_STATE" -eq 1 ]; then
        if [ "$MOVED_STATE_ASIDE" -eq 1 ] && [ -n "$STATE_ASIDE" ] && [ -e "$STATE_ASIDE" ]; then
            if ! mv -f "$STATE_ASIDE" "$STATE_FILE" 2>/dev/null; then
                report_rollback_error "state: failed to restore previous update-state from $STATE_ASIDE"
            else
                STATE_ASIDE=""
                MOVED_STATE_ASIDE=0
            fi
        elif [ -n "$PREV_STATE_BYTES" ]; then
            if ! printf '%s' "$PREV_STATE_BYTES" > "$STATE_FILE" 2>/dev/null; then
                report_rollback_error "state: failed to rewrite previous update-state bytes"
            fi
        fi
    else
        if [ -e "$STATE_FILE" ] || [ -L "$STATE_FILE" ]; then
            if ! rm -f "$STATE_FILE" 2>/dev/null; then
                report_rollback_error "state: failed to remove partially written $STATE_FILE"
            fi
        fi
    fi
    ACTIVATED_STATE=0
}

rollback_bundle() {
    if [ "$ACTIVATED_BUNDLE" -eq 0 ] && [ "$MOVED_BUNDLE_ASIDE" -eq 0 ]; then
        [ -z "$BUNDLE_STAGE" ] || rm -rf "$BUNDLE_STAGE" 2>/dev/null || true
        BUNDLE_STAGE=""
        return 0
    fi
    doomed=""
    if [ -e "$BUNDLE_DEST" ] || [ -L "$BUNDLE_DEST" ]; then
        doomed="$BUNDLE_DEST.failed.$$"
        if ! mv "$BUNDLE_DEST" "$doomed" 2>/dev/null; then
            report_rollback_error "bundle: cannot clear active $BUNDLE_DEST before restore"
            return 0
        fi
    fi
    if [ "$HAD_PREV_BUNDLE" -eq 1 ] && [ "$MOVED_BUNDLE_ASIDE" -eq 1 ] \
        && [ -n "$BUNDLE_ASIDE" ] && [ -e "$BUNDLE_ASIDE" ]; then
        if ! mv "$BUNDLE_ASIDE" "$BUNDLE_DEST" 2>/dev/null; then
            report_rollback_error "bundle: failed to restore previous tree from $BUNDLE_ASIDE (aside preserved)"
            if [ -n "$doomed" ] && [ -e "$doomed" ]; then
                mv "$doomed" "$BUNDLE_DEST" 2>/dev/null || true
            fi
        else
            BUNDLE_ASIDE=""
            MOVED_BUNDLE_ASIDE=0
            [ -z "$doomed" ] || rm -rf "$doomed" 2>/dev/null || true
        fi
    else
        [ -z "$doomed" ] || rm -rf "$doomed" 2>/dev/null || true
    fi
    ACTIVATED_BUNDLE=0
    BUNDLE_STAGE=""
}

rollback_all() {
    # Reverse order of commit: bundle → state → binary.
    rollback_bundle
    rollback_state
    rollback_binary
}

# --- Activate binary (symlink flip). Regular-file prior is moved aside first. ---
if [ "$PREV_BINARY_KIND" = "regular" ]; then
    if ! mv "$ACTIVE_BIN" "$BINARY_ASIDE"; then
        rm -rf "$BUNDLE_STAGE"
        err "cannot preserve existing hyper at $ACTIVE_BIN; close running sessions and retry"
    fi
    MOVED_BINARY_ASIDE=1
fi
if ! ln -s "../downloads/$VERSIONED" "$TMP_LINK"; then
    rm -rf "$BUNDLE_STAGE"
    if [ "$MOVED_BINARY_ASIDE" -eq 1 ] && [ -e "$BINARY_ASIDE" ]; then
        mv -f "$BINARY_ASIDE" "$ACTIVE_BIN" 2>/dev/null || report_rollback_error "binary: failed to restore after link stage failure"
        MOVED_BINARY_ASIDE=0
    fi
    err "failed to stage active hyper link"
fi
if ! mv -f "$TMP_LINK" "$ACTIVE_BIN"; then
    rm -f "$TMP_LINK"
    rm -rf "$BUNDLE_STAGE"
    if [ "$MOVED_BINARY_ASIDE" -eq 1 ] && [ -e "$BINARY_ASIDE" ]; then
        mv -f "$BINARY_ASIDE" "$ACTIVE_BIN" 2>/dev/null || report_rollback_error "binary: failed to restore after activation failure"
        MOVED_BINARY_ASIDE=0
    fi
    err "failed to activate hyper binary"
fi
TMP_LINK=""
ACTIVATED_BINARY=1

# --- Activate update-state.json ---
if [ "$HAD_PREV_STATE" -eq 1 ]; then
    if ! mv "$STATE_FILE" "$STATE_ASIDE"; then
        rollback_all
        fail_with_rollback "failed to preserve existing update state at $STATE_FILE"
    fi
    MOVED_STATE_ASIDE=1
fi
if ! mv -f "$STATE_TMP" "$STATE_FILE"; then
    STATE_TMP=""
    rollback_all
    fail_with_rollback "could not record Hyper update state"
fi
STATE_TMP=""
ACTIVATED_STATE=1
# INJECT_FAIL_AFTER_STATE

# --- Activate bundle (optional; binary-only keeps the old tree) ---
if [ -n "$BUNDLE_STAGE" ]; then
    if [ "$HAD_PREV_BUNDLE" -eq 1 ]; then
        if ! mv "$BUNDLE_DEST" "$BUNDLE_ASIDE"; then
            rm -rf "$BUNDLE_STAGE"
            BUNDLE_STAGE=""
            rollback_all
            fail_with_rollback "failed to preserve existing bundled runtime at $BUNDLE_DEST"
        fi
        MOVED_BUNDLE_ASIDE=1
    fi
    if ! mv "$BUNDLE_STAGE" "$BUNDLE_DEST"; then
        if [ "$MOVED_BUNDLE_ASIDE" -eq 1 ] && [ -e "$BUNDLE_ASIDE" ]; then
            if ! mv "$BUNDLE_ASIDE" "$BUNDLE_DEST" 2>/dev/null; then
                report_rollback_error "bundle: failed to re-publish previous tree after activation failure (aside at $BUNDLE_ASIDE)"
            else
                BUNDLE_ASIDE=""
                MOVED_BUNDLE_ASIDE=0
            fi
        fi
        rm -rf "$BUNDLE_STAGE" 2>/dev/null || true
        BUNDLE_STAGE=""
        ACTIVATED_BUNDLE=0
        rollback_state
        rollback_binary
        fail_with_rollback "failed to activate bundled runtime; previous deployment restored if available"
    fi
    BUNDLE_STAGE=""
    ACTIVATED_BUNDLE=1
fi

# Commit succeeded — best-effort cleanup of asides.
[ -z "$BUNDLE_ASIDE" ] || rm -rf "$BUNDLE_ASIDE" 2>/dev/null || true
BUNDLE_ASIDE=""
MOVED_BUNDLE_ASIDE=0
[ -z "$BINARY_ASIDE" ] || rm -f "$BINARY_ASIDE" 2>/dev/null || true
BINARY_ASIDE=""
MOVED_BINARY_ASIDE=0
[ -z "$STATE_ASIDE" ] || rm -f "$STATE_ASIDE" 2>/dev/null || true
STATE_ASIDE=""
MOVED_STATE_ASIDE=0

printf '\nhyper v%s installed to %s\n' "$RESOLVED_VERSION" "$ACTIVE_BIN"

# ── Scheme live image (optional; fail-open) ──────────────────────────────────
# Prebuilt `gsc -exe` Gambit image for plugins that declare `runtime.scheme`.
# Runs AFTER the main install transaction commits: any failure here only warns
# and never rolls back hyper. Without the image the scheme runtime falls back
# to `gxi` / `gsi` discovered on PATH (or degrades silently).
install_scheme_image() {
    scheme_asset="hyper-scheme-image-${RESOLVED_VERSION}-${TRIPLE}.tar.gz"
    scheme_url="$(find_asset_url "$scheme_asset")" || return 0 # not shipped for this platform
    scheme_expected="$(awk -v want="$scheme_asset" '
        {
            name = $2
            sub(/^\*/, "", name)
            if (name == want) { print tolower($1); count++ }
        }
        END { if (count != 1) exit 1 }
    ' "$TMP_DIR/SHA256SUMS")" || {
        printf 'note: skipping scheme image (no unique SHA256SUMS entry for %s)\n' "$scheme_asset" >&2
        return 0
    }
    case "$scheme_expected" in
        *[!0-9a-f]*|'') printf 'note: skipping scheme image (bad digest)\n' >&2; return 0 ;;
    esac
    [ "${#scheme_expected}" -eq 64 ] || { printf 'note: skipping scheme image (bad digest)\n' >&2; return 0; }
    fetch "$scheme_url" "$TMP_DIR/$scheme_asset" || {
        printf 'note: scheme image download failed; continuing without it\n' >&2
        return 0
    }
    scheme_actual="$(sha256_of "$TMP_DIR/$scheme_asset" | tr 'A-F' 'a-f')"
    if [ "$scheme_actual" != "$scheme_expected" ]; then
        printf 'note: scheme image checksum mismatch; continuing without it\n' >&2
        return 0
    fi
    tar -xOzf "$TMP_DIR/$scheme_asset" hyper-scheme-image > "$TMP_DIR/hyper-scheme-image" 2>/dev/null || {
        printf 'note: scheme image extraction failed; continuing without it\n' >&2
        return 0
    }
    [ -s "$TMP_DIR/hyper-scheme-image" ] || {
        printf 'note: scheme image archive was empty; continuing without it\n' >&2
        return 0
    }
    scheme_bin_dir="$GROK_HOME/bin"
    if [ -L "$scheme_bin_dir" ]; then
        printf 'note: %s is a symlink; skipping scheme image\n' "$scheme_bin_dir" >&2
        return 0
    fi
    mkdir -p "$scheme_bin_dir" 2>/dev/null || {
        printf 'note: cannot create %s; skipping scheme image\n' "$scheme_bin_dir" >&2
        return 0
    }
    scheme_stage="$scheme_bin_dir/.hyper-scheme-image.install.$$"
    if cp "$TMP_DIR/hyper-scheme-image" "$scheme_stage" 2>/dev/null \
        && chmod 0755 "$scheme_stage" 2>/dev/null \
        && mv -f "$scheme_stage" "$scheme_bin_dir/hyper-scheme-image" 2>/dev/null; then
        printf 'Scheme live image installed to %s\n' "$scheme_bin_dir/hyper-scheme-image"
    else
        rm -f "$scheme_stage" 2>/dev/null || true
        printf 'note: could not install scheme image to %s; continuing without it\n' "$scheme_bin_dir" >&2
    fi
    return 0
}
install_scheme_image

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
