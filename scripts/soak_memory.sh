#!/usr/bin/env bash
# Memory-boundedness soak: XTest against an isolated X server at a high rate,
# sampling engine RSS. Verifies that no per-event state accumulates (e.g.
# x11rb request bookkeeping on a write-only connection).
# Usage: scripts/soak_memory.sh [--display :99] [--minutes 5] [--interval-ms 2]
set -euo pipefail
DISP=":99"; MINUTES=5; INTERVAL=2
while [[ $# -gt 0 ]]; do
    case "$1" in
        --display) DISP="$2"; shift 2 ;;
        --minutes) MINUTES="$2"; shift 2 ;;
        --interval-ms) INTERVAL="$2"; shift 2 ;;
        *) echo "unknown argument $1" >&2; exit 64 ;;
    esac
done
[[ "$DISP" == "${DISPLAY:-}" ]] && { echo "refusing to inject into the live display" >&2; exit 1; }
ROOT="$(cd "$(dirname "$0")/.." && pwd)"; BIN="$ROOT/target/release"
cargo build --release -q -p pulse-engine -p pulse-cli --manifest-path "$ROOT/Cargo.toml"
RUN="${XDG_RUNTIME_DIR:?}/pulse-soak-$$"; CFG="$(mktemp -d)"
trap 'kill "${EP:-0}" 2>/dev/null || true; rm -rf "$RUN" "$CFG"' EXIT
cat > "$CFG/config.toml" <<CONF
schema_version = 2
[engine]
allow_advanced_rates = true
[clicker]
interval_ms = $INTERVAL
hold_ms = $(( INTERVAL / 2 > 0 ? INTERVAL / 2 : 1 ))
[target]
require_focus = false
CONF
"$BIN/pulse-engine" --display "$DISP" --config-dir "$CFG" --runtime-dir "$RUN" --log-level error & EP=$!
for _ in $(seq 50); do [[ -S "$RUN/engine.sock" ]] && break; sleep 0.1; done
cli() { "$BIN/pulse-cli" --socket "$RUN/engine.sock" "$@"; }
cli start >/dev/null
echo "elapsed_s rss_kib submitted_events"
for ((i = 0; i <= MINUTES * 2; i++)); do
    rss=$(awk '/VmRSS/ {print $2}' "/proc/$EP/status")
    ev=$(cli --json status | python3 -c "import json,sys; print(json.load(sys.stdin)['data']['value']['counters']['submitted_events'])")
    echo "$((i * 30)) $rss $ev"
    [[ $i -lt $((MINUTES * 2)) ]] && sleep 30
done
cli stop
cli diagnostics | sed -n '1,8p'
cli shutdown >/dev/null; wait "$EP"
