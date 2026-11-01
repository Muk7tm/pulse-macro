#!/usr/bin/env bash
# Run the X11 integration tests against an isolated Xvfb server so no input
# is ever injected into the live desktop.
#
# Usage: scripts/run_xvfb_integration.sh [--xvfb /path/to/Xvfb] [--display :99] [--uinput]
#
# --uinput additionally runs the uinput tests (needs write access to
# /dev/uinput; the tests grab their virtual devices so events never reach
# the desktop).
#
# Xvfb is packaged as `xorg-server-xvfb` on Arch/CachyOS. Without root you can
# extract the official package and point --xvfb at the binary.
set -euo pipefail

XVFB="$(command -v Xvfb || true)"
DISP=":99"
UINPUT=0
while [[ $# -gt 0 ]]; do
    case "$1" in
        --xvfb) XVFB="$2"; shift 2 ;;
        --display) DISP="$2"; shift 2 ;;
        --uinput) UINPUT=1; shift ;;
        *) echo "unknown argument $1" >&2; exit 64 ;;
    esac
done

if [[ "$DISP" == "${DISPLAY:-}" ]]; then
    echo "refusing to use the live display $DISP" >&2
    exit 1
fi

cd "$(dirname "$0")/.."

STARTED=""
if ! DISPLAY="$DISP" xdpyinfo >/dev/null 2>&1; then
    if [[ -z "$XVFB" || ! -x "$XVFB" ]]; then
        echo "Xvfb not found; install xorg-server-xvfb or pass --xvfb PATH" >&2
        exit 1
    fi
    # -noreset: without it the server regenerates whenever its last client
    # disconnects and in-flight test connections are reset.
    "$XVFB" "$DISP" -screen 0 1280x720x24 -nolisten tcp -noreset >/dev/null 2>&1 &
    STARTED=$!
    trap '[[ -n "$STARTED" ]] && kill "$STARTED" 2>/dev/null || true' EXIT
    for _ in $(seq 50); do DISPLAY="$DISP" xdpyinfo >/dev/null 2>&1 && break; sleep 0.1; done
fi

export PULSE_TEST_DISPLAY="$DISP"
if [[ $UINPUT -eq 1 ]]; then
    export PULSE_TEST_UINPUT=1
fi

cargo test -p pulse-input --test x11_integration_tests
cargo test -p pulse-engine --test x11_engine_tests
if [[ $UINPUT -eq 1 ]]; then
    cargo test -p pulse-input --test uinput_integration_tests
fi
