#!/usr/bin/env bash
set -euo pipefail

BIN="${1:-./target/debug/claw-loopd}"

if [[ ! -x "$BIN" ]]; then
  echo "[e2e-smoke] binary not found or not executable: $BIN" >&2
  exit 1
fi

WORKDIR="$(mktemp -d)"
cleanup() {
  rm -rf "$WORKDIR"
}
trap cleanup EXIT

run_start() {
  local tick="$1"
  local out
  out="$($BIN start --repo "$WORKDIR" --session-key test-session --channel discord --thread-id test-thread --tick-sec "$tick")"
  local run_id pid
  run_id="$(echo "$out" | awk -F= '/^run_id=/{print $2}')"
  pid="$(echo "$out" | awk -F= '/^daemon_pid=/{print $2}')"
  if [[ -z "$run_id" || -z "$pid" ]]; then
    echo "[e2e-smoke] failed to parse start output"
    echo "$out"
    exit 1
  fi
  echo "$run_id|$pid"
}

assert_json_field() {
  local json="$1" key="$2" expected="$3"
  python3 - <<'PY' "$json" "$key" "$expected"
import json, sys
obj = json.loads(sys.argv[1])
key = sys.argv[2]
expected = sys.argv[3]
val = obj.get(key)
if str(val) != expected:
    raise SystemExit(f"assert failed: {key}={val!r} expected {expected!r}")
PY
}

assert_json_int_ge() {
  local json="$1" key="$2" min="$3"
  python3 - <<'PY' "$json" "$key" "$min"
import json, sys
obj = json.loads(sys.argv[1])
key = sys.argv[2]
minv = int(sys.argv[3])
val = int(obj.get(key, 0))
if val < minv:
    raise SystemExit(f"assert failed: {key}={val} < {minv}")
PY
}

echo "[e2e-smoke] case1 lifecycle start"
IFS='|' read -r RUN1 PID1 <<<"$(run_start 1)"
$BIN notify --repo "$WORKDIR" --run-id "$RUN1" --kind progress --message "loop step done" >/dev/null
sleep 2
STATUS1="$($BIN status --repo "$WORKDIR" --run-id "$RUN1")"
assert_json_field "$STATUS1" status running
assert_json_int_ge "$STATUS1" dispatched_notifications 2
$BIN stop --repo "$WORKDIR" --run-id "$RUN1" >/dev/null
sleep 2
STATUS1B="$($BIN status --repo "$WORKDIR" --run-id "$RUN1")"
assert_json_field "$STATUS1B" status stopped

echo "[e2e-smoke] case2 orphan sweep start"
IFS='|' read -r RUN2 PID2 <<<"$(run_start 60)"
kill -9 "$PID2" >/dev/null 2>&1 || true

python3 - <<'PY' "$WORKDIR/.ralph/runs/$RUN2/state.json"
import json, sys
p = sys.argv[1]
obj = json.load(open(p))
obj["lease_expires_at"] = "2000-01-01T00:00:00Z"
json.dump(obj, open(p, "w"), indent=2)
PY

$BIN sweep --repo "$WORKDIR" --run-id "$RUN2" >/dev/null
STATUS2="$($BIN status --repo "$WORKDIR" --run-id "$RUN2")"
assert_json_field "$STATUS2" status blocked

echo "[e2e-smoke] ok"
