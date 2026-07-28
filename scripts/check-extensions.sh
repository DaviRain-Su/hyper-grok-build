#!/usr/bin/env bash
# Build official WASM guest examples and smoke-check them via the runtime unit tests.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

rustup target add wasm32-unknown-unknown >/dev/null 2>&1 || true

build_guest() {
  local dir="$1"
  local out_name="$2"
  echo "==> building $dir"
  (
    cd "$dir"
    cargo build --release --target wasm32-unknown-unknown
    local wasm
    wasm="$(ls target/wasm32-unknown-unknown/release/*.wasm | head -1)"
    cp "$wasm" extension.wasm
    echo "    wrote $dir/extension.wasm ($(wc -c < extension.wasm) bytes)"
  )
}

EX="$ROOT/crates/codegen/xai-grok-extension-runtime/examples"
build_guest "$EX/rust-guest-template" hyper_ext_rust_guest_template
build_guest "$EX/sdk-path-guard" sdk_path_guard
build_guest "$EX/sdk-stop-once" sdk_stop_once

echo "==> cargo test extension-runtime + extension-api + extension-sdk"
cargo test -p xai-grok-extension-runtime -p xai-grok-extension-api -p xai-grok-extension-sdk --lib

echo "==> OK (extensions smoke)"
