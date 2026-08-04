#!/usr/bin/env bash
# Build Hyper agent + Comet desktop and run the local-link UI.
#
# Usage (from monorepo root):
#   ./scripts/run-desktop.sh              # release hyper + debug/release comet
#   ./scripts/run-desktop.sh --release    # both release
#   ./scripts/run-desktop.sh --status     # print status only
#   ./scripts/run-desktop.sh -- headless  # pass args to comet after --
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
COMET_DIR="$ROOT/desktop/comet"
HYPER_TARGET="${CARGO_TARGET_DIR:-$ROOT/target}"
COMET_TARGET="${COMET_CARGO_TARGET_DIR:-${CARGO_TARGET_DIR:-$ROOT/target/desktop-comet}}"

PROFILE=release
COMET_PROFILE=debug
STATUS_ONLY=0
COMET_ARGS=()

while [[ $# -gt 0 ]]; do
  case "$1" in
    --release)
      COMET_PROFILE=release
      shift
      ;;
    --debug-hyper)
      PROFILE=debug
      shift
      ;;
    --status)
      STATUS_ONLY=1
      shift
      ;;
    --)
      shift
      COMET_ARGS+=("$@")
      break
      ;;
    -h|--help)
      sed -n '2,12p' "$0"
      exit 0
      ;;
    *)
      COMET_ARGS+=("$1")
      shift
      ;;
  esac
done

echo "==> Building Hyper agent ($PROFILE) → $HYPER_TARGET"
(
  cd "$ROOT"
  if [[ "$PROFILE" == "release" ]]; then
    CARGO_TARGET_DIR="$HYPER_TARGET" cargo build -p xai-grok-pager-bin \
      --features community-build --release
  else
    CARGO_TARGET_DIR="$HYPER_TARGET" cargo build -p xai-grok-pager-bin \
      --features community-build
  fi
)

if [[ "$PROFILE" == "release" ]]; then
  HYPER_BIN="$HYPER_TARGET/release/hyper"
else
  HYPER_BIN="$HYPER_TARGET/debug/hyper"
fi
if [[ ! -x "$HYPER_BIN" ]]; then
  echo "error: hyper binary not found at $HYPER_BIN" >&2
  exit 1
fi
export HYPER_AGENT_BIN="$HYPER_BIN"
echo "    HYPER_AGENT_BIN=$HYPER_AGENT_BIN"

echo "==> Building Comet desktop ($COMET_PROFILE) → $COMET_TARGET"
(
  cd "$COMET_DIR"
  if [[ "$COMET_PROFILE" == "release" ]]; then
    CARGO_TARGET_DIR="$COMET_TARGET" cargo build -p comet --release
  else
    CARGO_TARGET_DIR="$COMET_TARGET" cargo build -p comet
  fi
)

if [[ "$COMET_PROFILE" == "release" ]]; then
  COMET_BIN="$COMET_TARGET/release/comet"
else
  COMET_BIN="$COMET_TARGET/debug/comet"
fi
if [[ ! -x "$COMET_BIN" ]]; then
  echo "error: comet binary not found at $COMET_BIN" >&2
  exit 1
fi

export COMET_DATA_DIR="${COMET_DATA_DIR:-${HOME}/.hyper/desktop}"
export COMET_HARNESS="${COMET_HARNESS:-hyper}"
# Share agent auth/memory/skills/workflows/extensions with the Hyper CLI.
export GROK_HOME="${GROK_HOME:-${HOME}/.grok}"
mkdir -p "$COMET_DATA_DIR" "$GROK_HOME"

if [[ "$STATUS_ONLY" -eq 1 ]]; then
  exec "$COMET_BIN" status
fi

echo "==> Running $COMET_BIN ${COMET_ARGS[*]:-}"
echo "    data: $COMET_DATA_DIR  harness: $COMET_HARNESS"
exec "$COMET_BIN" "${COMET_ARGS[@]}"
