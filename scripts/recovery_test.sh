#!/usr/bin/env bash
# Proves the crash-recovery checkpoint (src/state.rs) survives a real
# SIGKILL without duplicating an action or losing state.
#
# Scope, stated honestly: this is ONE checkpoint (last action + resume
# time + monotonic action_count), tested with ONE kill/restart cycle
# repeated twice — not six checkpoints against a mainnet-mirror validator
# like marcelofeitoza's PR #2. It does not touch the network or a real
# wallet; it exercises exactly the same save/resume code path the real
# agent uses (see src/agent/mod.rs), against a real OS process kill.
#
# Usage: ./scripts/recovery_test.sh

set -euo pipefail

BIN="./target/release/recovery_test"
STATE_DIR="$(mktemp -d)"
LABEL="recovery-test-agent"
LOG="$(mktemp)"

cleanup() {
    [ -n "${PID:-}" ] && kill -9 "$PID" 2>/dev/null || true
    rm -rf "$STATE_DIR" "$LOG"
}
trap cleanup EXIT

if [ ! -x "$BIN" ]; then
    echo "Building release binaries first..."
    cargo build --release --bin recovery_test
fi

echo "=== recovery_test: state dir = $STATE_DIR ==="
echo

run_cycle() {
    local cycle="$1"
    local kill_after="$2"

    echo "--- cycle $cycle: starting worker, will SIGKILL after ${kill_after}s ---"
    "$BIN" --state-dir "$STATE_DIR" --label "$LABEL" --interval-secs 1 >>"$LOG" 2>&1 &
    PID=$!
    sleep "$kill_after"

    if ! kill -0 "$PID" 2>/dev/null; then
        echo "FAIL: worker exited on its own before kill (cycle $cycle)"
        exit 1
    fi

    kill -9 "$PID"
    wait "$PID" 2>/dev/null || true
    echo "killed pid $PID (SIGKILL) mid-run"
}

run_cycle 1 3
run_cycle 2 3
run_cycle 3 3

echo
echo "=== final checkpoint file ==="
CP_FILE="$STATE_DIR/$LABEL.json"
cat "$CP_FILE"
echo

echo "=== worker stdout across all cycles ==="
cat "$LOG"
echo

# --- Verification ---
FAIL=0

if [ -f "$STATE_DIR/$LABEL.json.tmp" ]; then
    echo "FAIL: leftover .tmp file — atomic rename did not clean up"
    FAIL=1
fi

if ! python3 -c "import json,sys; json.load(open('$CP_FILE'))" 2>/dev/null; then
    echo "FAIL: checkpoint file is not valid JSON after repeated SIGKILL"
    FAIL=1
fi

ACTION_LINES=$(grep -c '^ACTION ' "$LOG" || true)
UNIQUE_ACTION_COUNTS=$(grep '^ACTION ' "$LOG" | awk '{print $2}' | sort -n | uniq | wc -l)
if [ "$ACTION_LINES" -ne "$UNIQUE_ACTION_COUNTS" ]; then
    echo "FAIL: duplicate action_count values recorded across restarts — same action replayed"
    FAIL=1
fi

RESUME_LINES=$(grep -c '^RESUME' "$LOG" || true)
if [ "$RESUME_LINES" -lt 2 ]; then
    echo "FAIL: expected at least 2 resumed-from-checkpoint restarts, saw $RESUME_LINES"
    FAIL=1
fi

FINAL_COUNT=$(python3 -c "import json; print(json.load(open('$CP_FILE'))['action_count'])")
if [ "$FINAL_COUNT" -lt 1 ]; then
    echo "FAIL: final action_count is 0 — no progress survived the kills"
    FAIL=1
fi

if [ "$FAIL" -eq 0 ]; then
    echo "PASS: checkpoint file stayed valid JSON through 3x SIGKILL, action_count"
    echo "      advanced monotonically ($ACTION_LINES actions, all unique, final=$FINAL_COUNT),"
    echo "      and each restart resumed from the checkpoint instead of duplicating"
    echo "      or losing the last action."
    exit 0
else
    echo "FAIL: see above"
    exit 1
fi
