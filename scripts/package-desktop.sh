#!/usr/bin/env bash
# Build a portable Hyper desktop (comet) archive next to monorepo release assets.
#
# Output: dist/desktop/hyper-desktop-<version>-<triple>.{tar.gz|zip}
# Does not publish — wire into CI or run manually after ./scripts/run-desktop.sh --release.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
COMET_DIR="$ROOT/desktop/comet"
VERSION="$(tr -d '[:space:]' < "$ROOT/VERSION")"
OUT_DIR="${OUT_DIR:-$ROOT/dist/desktop}"
HYPER_TARGET="${CARGO_TARGET_DIR:-$ROOT/target}"
COMET_TARGET="${COMET_CARGO_TARGET_DIR:-$ROOT/target/desktop-comet}"

triple="$(rustc -vV | awk '/^host:/{print $2}')"
echo "==> version=$VERSION host=$triple"

echo "==> Build hyper (release)"
(
  cd "$ROOT"
  CARGO_TARGET_DIR="$HYPER_TARGET" cargo build -p xai-grok-pager-bin \
    --features community-build --release
)

echo "==> Build comet (release)"
(
  cd "$COMET_DIR"
  CARGO_TARGET_DIR="$COMET_TARGET" cargo build -p comet --release
)

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
mkdir -p "$STAGE/bin" "$OUT_DIR"

cp -f "$HYPER_TARGET/release/hyper" "$STAGE/bin/hyper"
cp -f "$COMET_TARGET/release/comet" "$STAGE/bin/comet"
chmod 0755 "$STAGE/bin/hyper" "$STAGE/bin/comet"

cat > "$STAGE/README.txt" <<EOF
Hyper desktop local-link bundle $VERSION ($triple)

bin/comet  — desktop UI + local engine (offline)
bin/hyper  — agent (ACP stdio); comet spawns this by default

Data:
  Desktop engine store: ~/.hyper/desktop  (COMET_DATA_DIR)
  Hyper agent home:     ~/.grok            (GROK_HOME)

Run:
  export PATH="\$PWD/bin:\$PATH"
  export HYPER_AGENT_BIN="\$PWD/bin/hyper"
  comet

Cloud multi-device sync is disabled in this fork.
EOF

name="hyper-desktop-${VERSION}-${triple}"
if [[ "$triple" == *windows* ]]; then
  # Cross-packaging on Unix still uses tar; native Windows CI can zip later.
  archive="$OUT_DIR/${name}.tar.gz"
else
  archive="$OUT_DIR/${name}.tar.gz"
fi
tar -C "$STAGE" -czf "$archive" .
echo "==> wrote $archive"
ls -lh "$archive"
