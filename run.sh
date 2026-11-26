#!/usr/bin/env bash
# Build and run overlayd (M0 spike).
#
# Usage: ./run.sh [--debug] [cargo args...]
#
# Requires a Wayland compositor (KWin validated; zwlr_layer_shell_v1 v5).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

BUILD_PROFILE="release"
CARGO_ARGS=()

while [[ $# -gt 0 ]]; do
    case "$1" in
        --debug)
            BUILD_PROFILE="debug"
            shift
            ;;
        --*)
            CARGO_ARGS+=("$1")
            shift
            ;;
        *)
            echo "Unknown argument: $1" >&2
            exit 2
            ;;
    esac
done

echo "[overlayd] building profile=$BUILD_PROFILE"
# Cargo has no `--debug` flag: the debug profile is the default. Only
# `--release` needs to be passed explicitly.
if [[ "$BUILD_PROFILE" == "release" ]]; then
    cargo build --release "${CARGO_ARGS[@]}"
else
    cargo build "${CARGO_ARGS[@]}"
fi

BIN="target/$BUILD_PROFILE/overlayd"
if [[ ! -x "$BIN" ]]; then
    echo "[overlayd] binary not found: $BIN" >&2
    exit 1
fi

echo "[overlayd] running $BIN (WAYLAND_DISPLAY=${WAYLAND_DISPLAY:-<unset>})"
exec "$BIN"
