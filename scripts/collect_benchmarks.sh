#!/usr/bin/env bash
# Reproducible engine benchmarks (see docs/benchmarks.md). Writes raw JSON to
# benchmarks/raw/<date>/ and never injects into the live desktop:
# XTest runs against an isolated X server (default Xvfb :99) and uinput
# benchmarks grab their own virtual devices exclusively.
#
# Usage: scripts/collect_benchmarks.sh [--display :99] [--seconds 30] [--skip-uinput]
set -euo pipefail

DISPLAY_ARG=":99"
SECONDS_PER_RUN=30
SKIP_UINPUT=0
while [[ $# -gt 0 ]]; do
    case "$1" in
        --display) DISPLAY_ARG="$2"; shift 2 ;;
        --seconds) SECONDS_PER_RUN="$2"; shift 2 ;;
        --skip-uinput) SKIP_UINPUT=1; shift ;;
        *) echo "unknown argument $1" >&2; exit 64 ;;
    esac
done

if [[ "$DISPLAY_ARG" == "${DISPLAY:-}" ]]; then
    echo "refusing to benchmark against the live display $DISPLAY_ARG; start Xvfb (scripts/run_xvfb_integration.sh does)" >&2
    exit 1
fi

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
cargo build --release -p pulse-engine -p pulse-cli -p pulse-bench >/dev/null
BIN="$ROOT/target/release"
OUT="$ROOT/benchmarks/raw/$(date +%F)"
mkdir -p "$OUT"
RUN="${XDG_RUNTIME_DIR:?XDG_RUNTIME_DIR must be set}/pulse-bench-$$"
CFG="$(mktemp -d)"
cleanup() {
    [[ -n "${ENGINE_PID:-}" ]] && kill "$ENGINE_PID" 2>/dev/null || true
    rm -rf "$RUN" "$CFG"
}
trap cleanup EXIT

cli() { "$BIN/pulse-cli" --socket "$RUN/engine.sock" "$@"; }

bench() { # name, then pulse-bench args
    local name="$1"; shift
    echo "== $name"
    "$BIN/pulse-bench" "$@" > "$OUT/$name.json"
}

bench timer_10ms timer --interval-us 10000 --count 1000
bench timer_1ms timer --interval-us 1000 --count 3000
bench alloc alloc --display "$DISPLAY_ARG"
bench dispatch dispatch
bench emit_xtest emit --backend xtest --display "$DISPLAY_ARG" --count 5000
bench e2e_xtest e2e --display "$DISPLAY_ARG" --count 2000
if [[ $SKIP_UINPUT -eq 0 ]]; then
    bench emit_uinput emit --backend uinput --count 2000
fi

engine_profile() { # backend, interval_ms
    local backend="$1" interval="$2"
    cat > "$CFG/config.toml" <<EOF
schema_version = 2
[engine]
backend = "$backend"
allow_advanced_rates = true
[clicker]
interval_ms = $interval
hold_ms = $(( interval > 40 ? 20 : interval / 3 + 1 ))
[target]
require_focus = false
EOF
}

start_engine() {
    "$BIN/pulse-engine" --display "$DISPLAY_ARG" --config-dir "$CFG" --runtime-dir "$RUN" --log-level warn &
    ENGINE_PID=$!
    for _ in $(seq 50); do [[ -S "$RUN/engine.sock" ]] && break; sleep 0.1; done
}

stop_engine() {
    cli shutdown >/dev/null || true
    wait "$ENGINE_PID" 2>/dev/null || true
    ENGINE_PID=""
}

# Idle engine, no UI.
engine_profile xtest 100
start_engine
sleep 2
echo "== engine_idle (${SECONDS_PER_RUN}s)"
"$BIN/pulse-bench" process --pid "$ENGINE_PID" --seconds "$SECONDS_PER_RUN" > "$OUT/engine_idle.json"
stop_engine

# Active runs. uinput runs inject into the host input stack, so they only
# run when explicitly requested with an isolated session; XTest targets Xvfb.
for interval in 200 100 50 20; do
    engine_profile xtest "$interval"
    start_engine
    cli start >/dev/null
    sleep 1
    echo "== engine_active_xtest_${interval}ms (${SECONDS_PER_RUN}s)"
    "$BIN/pulse-bench" process --pid "$ENGINE_PID" --seconds "$SECONDS_PER_RUN" > "$OUT/engine_active_xtest_${interval}ms.json"
    cli stop >/dev/null
    cli --json diagnostics > "$OUT/engine_active_xtest_${interval}ms_diagnostics.json"
    stop_engine
done

echo "raw results in $OUT"
