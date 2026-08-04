#!/usr/bin/env bash
# macOS packaging: build the release binary for the host arch and produce
#   target/package/comet-<version>-macos-<arch>.dmg          (user download)
#   target/package/comet-<version>-macos-<arch>-app.tar.gz   (auto-updater)
# containing Comet.app (unsigned unless CODESIGN_IDENTITY is set).
#
# Usage: scripts/package-macos.sh
# Env:   CODESIGN_IDENTITY="Developer ID Application: …" to sign the bundle.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
command -v cargo >/dev/null 2>&1 || PATH="$HOME/.cargo/bin:$PATH"
VERSION="$(grep -m1 '^version' "$ROOT/Cargo.toml" | sed 's/.*"\(.*\)".*/\1/')"
ARCH="$(uname -m)" # arm64 on Apple silicon runners
OUT_DIR="$ROOT/target/package"
# Prefer Hyper branding; keep Comet.app symlink name only if tools expect it.
APP="$OUT_DIR/Hyper.app"
DMG="$OUT_DIR/hyper-$VERSION-macos-$ARCH.dmg"
APP_TARBALL="$OUT_DIR/hyper-$VERSION-macos-$ARCH-app.tar.gz"
ICON_SRC="$ROOT/dist/hyper.png"
if [[ ! -f "$ICON_SRC" ]]; then
  ICON_SRC="$ROOT/dist/comet.png"
fi

cd "$ROOT"
cargo build --release -p comet

rm -rf "$APP" "$DMG" "$APP_TARBALL"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
install -m 755 "$ROOT/target/release/comet" "$APP/Contents/MacOS/comet"
sed "s/__VERSION__/$VERSION/" "$ROOT/dist/macos/Info.plist" >"$APP/Contents/Info.plist"

# Dock / Finder icon: multi-size iconset → hyper.icns (CFBundleIconFile = hyper).
ICONSET="$OUT_DIR/hyper.iconset"
rm -rf "$ICONSET" && mkdir -p "$ICONSET"
for size in 16 32 128 256 512; do
  sips -z "$size" "$size" "$ICON_SRC" --out "$ICONSET/icon_${size}x${size}.png" >/dev/null
  retina=$((size * 2))
  sips -z "$retina" "$retina" "$ICON_SRC" --out "$ICONSET/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/hyper.icns"
rm -rf "$ICONSET"

if [[ -n "${CODESIGN_IDENTITY:-}" ]]; then
  codesign --deep --force --options runtime --sign "$CODESIGN_IDENTITY" "$APP"
else
  # Ad-hoc signature so the app launches on Apple silicon (Gatekeeper still
  # requires right-click → Open on first launch without notarization).
  codesign --deep --force --sign - "$APP"
fi

# Auto-updater / portable artifact: signed bundle as a tarball.
tar -czf "$APP_TARBALL" -C "$OUT_DIR" Hyper.app
echo "packaged: $APP_TARBALL"

hdiutil create -volname Hyper -srcfolder "$APP" -ov -format UDZO "$DMG"
echo "packaged: $DMG"
