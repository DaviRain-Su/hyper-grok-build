#!/usr/bin/env bash
# Linux packaging: build the release binary and produce
#   target/package/comet-<version>-linux-<arch>.tar.gz
# containing the binary, the .desktop entry, and the icon, plus an install.sh
# that drops them into ~/.local (XDG) paths.
#
# Usage: scripts/package-linux.sh
# Env:   PROFILE=debug for a fast unoptimized package (CI smoke); default release.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
command -v cargo >/dev/null 2>&1 || PATH="$HOME/.cargo/bin:$PATH"
PROFILE="${PROFILE:-release}"
ARCH="$(uname -m)"
VERSION="$(grep -m1 '^version' "$ROOT/Cargo.toml" | sed 's/.*"\(.*\)".*/\1/')"
OUT_DIR="$ROOT/target/package"
STAGE="$OUT_DIR/comet-$VERSION-linux-$ARCH"
TARBALL="$STAGE.tar.gz"

cd "$ROOT"
if [[ "$PROFILE" == "release" ]]; then
  cargo build --release -p comet
  BIN="$ROOT/target/release/comet"
else
  cargo build -p comet
  BIN="$ROOT/target/debug/comet"
fi

rm -rf "$STAGE" "$TARBALL"
mkdir -p "$STAGE"
install -m 755 "$BIN" "$STAGE/comet"
install -m 644 "$ROOT/dist/comet.desktop" "$STAGE/comet.desktop"
install -m 644 "$ROOT/dist/hyper.desktop" "$STAGE/hyper.desktop"
install -m 644 "$ROOT/dist/hyper.png" "$STAGE/hyper.png"
# Back-compat names for older install scripts.
install -m 644 "$ROOT/dist/hyper.png" "$STAGE/comet.png"

cat >"$STAGE/install.sh" <<'INSTALL'
#!/usr/bin/env bash
# Install Hyper desktop (comet binary) into ~/.local (no root needed).
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
install -Dm755 "$HERE/comet" "$HOME/.local/bin/comet"
install -Dm644 "$HERE/hyper.desktop" "$HOME/.local/share/applications/hyper.desktop"
install -Dm644 "$HERE/comet.desktop" "$HOME/.local/share/applications/comet.desktop"
# Icon name is `hyper` (matches Icon=hyper in .desktop).
install -Dm644 "$HERE/hyper.png" "$HOME/.local/share/icons/hicolor/1024x1024/apps/hyper.png"
install -Dm644 "$HERE/hyper.png" "$HOME/.local/share/icons/hicolor/256x256/apps/hyper.png"
command -v update-desktop-database >/dev/null 2>&1 \
  && update-desktop-database "$HOME/.local/share/applications" || true
command -v gtk-update-icon-cache >/dev/null 2>&1 \
  && gtk-update-icon-cache -f -t "$HOME/.local/share/icons/hicolor" 2>/dev/null || true
echo "Installed Hyper desktop. Make sure ~/.local/bin is on your PATH, then run: comet"
INSTALL
chmod 755 "$STAGE/install.sh"

tar -czf "$TARBALL" -C "$OUT_DIR" "$(basename "$STAGE")"
rm -rf "$STAGE"
echo "packaged: $TARBALL"
tar -tzf "$TARBALL"
