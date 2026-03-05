#!/usr/bin/env bash
set -euo pipefail

BIN="${1:-./target/debug/claw-loopd}"
DURATION_SEC="${SOAK_DURATION_SEC:-86400}"
TICK_SEC="${SOAK_TICK_SEC:-1}"
NOTIFY_EVERY_SEC="${SOAK_NOTIFY_EVERY_SEC:-30}"
RECOVER_EVERY_LOOPS="${SOAK_RECOVER_EVERY_LOOPS:-20}"
REQUEUE_EVERY_LOOPS="${SOAK_REQUEUE_EVERY_LOOPS:-15}"
MAX_ATTEMPTS="${SOAK_MAX_ATTEMPTS:-3}"

if [[ ! -x "$BIN" ]]; then
  echo "[soak-24h] binary not found or not executable: $BIN" >&2
  exit 1
fi

for n in "$DURATION_SEC" "$TICK_SEC" "$NOTIFY_EVERY_SEC" "$RECOVER_EVERY_LOOPS" "$REQUEUE_EVERY_LOOPS" "$MAX_ATTEMPTS"; do
  if ! [[ "$n" =~ ^[0-9]+$ ]]; then
    echo "[soak-24h] numeric env required, got: $n" >&2
    exit 1
  fi
done

WORKDIR="$(mktemp -d)"
MOCKDIR="$WORKDIR/mockbin"
RUN_ID=""
mkdir -p "$MOCKDIR"

cleanup() {
  if [[ -n "$RUN_ID" ]]; then
    "$BIN" stop --repo "$WORKDIR" --run-id "$RUN_ID" --immediate >/dev/null 2>&1 || true
  fi
  rm -rf "$WORKDIR" >/dev/null 2>&1 || true
}
trap cleanup EXIT

cat > "$MOCKDIR/openclaw" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
STATE_FILE="${CLAW_LOOPD_SOAK_OPENCLAW_STATE:?missing CLAW_LOOPD_SOAK_OPENCLAW_STATE}"
count=0
if [[ -f "$STATE_FILE" ]]; then
  count="$(cat "$STATE_FILE")"
fi
count=$((count + 1))
echo "$count" > "$STATE_FILE"
mode=$((count % 10))

# 70% success, 30% controlled failure classes.
case "$mode" in
  1)
    echo "request timed out" >&2
    exit 1
    ;;
  2)
    echo "HTTP 429 rate limited" >&2
    exit 1
    ;;
  3)
    echo "thread/channel not found" >&2
    exit 1
    ;;
  *)
    exit 0
    ;;
esac
EOF
chmod +x "$MOCKDIR/openclaw"

OPENCLAW_STATE="$WORKDIR/mock-openclaw-count.txt"

echo "[soak-24h] start duration=${DURATION_SEC}s notify_every=${NOTIFY_EVERY_SEC}s max_attempts=${MAX_ATTEMPTS}"
OUT="$({
  CLAW_LOOPD_OPENCLAW_BIN="$MOCKDIR/openclaw" \
  CLAW_LOOPD_SOAK_OPENCLAW_STATE="$OPENCLAW_STATE" \
  CLAW_LOOPD_DELIVERY_MAX_ATTEMPTS="$MAX_ATTEMPTS" \
  "$BIN" start --repo "$WORKDIR" --session-key soak-test --channel discord --thread-id soak-thread --tick-sec "$TICK_SEC" --deliver-openclaw
})"
RUN_ID="$(echo "$OUT" | awk -F= '/^run_id=/{print $2}')"
if [[ -z "$RUN_ID" ]]; then
  echo "[soak-24h] failed to parse run_id"
  echo "$OUT"
  exit 1
fi

echo "[soak-24h] run_id=$RUN_ID"
RUN_DIR="$WORKDIR/.ralph/runs/$RUN_ID"
START_TS="$(date +%s)"
END_TS=$((START_TS + DURATION_SEC))
loop=0

run_json_cmd() {
  local out
  for _ in {1..10}; do
    if out="$($@ 2>/dev/null)"; then
      echo "$out"
      return 0
    fi
    sleep 0.2
  done
  "$@"
}

while (( $(date +%s) < END_TS )); do
  loop=$((loop + 1))

  "$BIN" notify --repo "$WORKDIR" --run-id "$RUN_ID" --kind progress --message "soak tick #$loop" >/dev/null
  sleep "$NOTIFY_EVERY_SEC"

  STATUS_JSON="$(run_json_cmd "$BIN" status --repo "$WORKDIR" --run-id "$RUN_ID")"
  python3 - <<'PY' "$STATUS_JSON"
import json, sys
obj = json.loads(sys.argv[1])
print("[soak-24h] status", json.dumps({
    "ticks": obj.get("ticks"),
    "pending": obj.get("pending_notifications"),
    "dispatched": obj.get("dispatched_notifications"),
    "dead_letter": obj.get("dead_letter_total"),
    "acked": obj.get("acked_total"),
    "unacked": obj.get("unacked_total"),
    "next_retry_at": obj.get("next_retry_at"),
}, ensure_ascii=False))
PY

  python3 - <<'PY' "$RUN_DIR"
import json, pathlib, sys
run_dir = pathlib.Path(sys.argv[1])

def read_jsonl(path):
    if not path.exists():
        return []
    return [json.loads(line) for line in path.read_text().splitlines() if line.strip()]

queue = read_jsonl(run_dir / "notify-queue.jsonl")
dispatched = read_jsonl(run_dir / "notify-dispatched.jsonl")
dead = read_jsonl(run_dir / "notify-dead-letter.jsonl")
ack = read_jsonl(run_dir / "notify-ack.jsonl")

ack_keys = [(row.get("event_id"), row.get("attempts")) for row in ack]
if len(ack_keys) != len(set(ack_keys)):
    raise SystemExit(f"duplicate ack keys detected: total={len(ack_keys)} unique={len(set(ack_keys))}")

terminal_ids = {row.get("event_id") for row in dispatched} | {row.get("event_id") for row in dead}
stale = [row.get("event_id") for row in queue if row.get("event_id") in terminal_ids]
if stale:
    raise SystemExit(f"queued contains terminal event_ids: {stale[:5]}")
PY

  if (( RECOVER_EVERY_LOOPS > 0 )) && (( loop % RECOVER_EVERY_LOOPS == 0 )); then
    DAEMON_PID="$(python3 - <<'PY' "$STATUS_JSON"
import json, sys
obj = json.loads(sys.argv[1])
print(obj.get("daemon_pid") or "")
PY
)"
    if [[ -n "$DAEMON_PID" ]]; then
      echo "[soak-24h] recovery: kill daemon pid=$DAEMON_PID and restart"
      kill -9 "$DAEMON_PID" >/dev/null 2>&1 || true
      sleep 1
      CLAW_LOOPD_OPENCLAW_BIN="$MOCKDIR/openclaw" \
      CLAW_LOOPD_SOAK_OPENCLAW_STATE="$OPENCLAW_STATE" \
      CLAW_LOOPD_DELIVERY_MAX_ATTEMPTS="$MAX_ATTEMPTS" \
      "$BIN" daemon --repo "$WORKDIR" --run-id "$RUN_ID" --tick-sec "$TICK_SEC" >/tmp/claw-loopd-soak-daemon.log 2>&1 &
      sleep 2
    fi
  fi

  if (( REQUEUE_EVERY_LOOPS > 0 )) && (( loop % REQUEUE_EVERY_LOOPS == 0 )); then
    FAILED_JSON="$(run_json_cmd "$BIN" delivery-report --repo "$WORKDIR" --run-id "$RUN_ID" --status failed --limit 1)"
    EVENT_ID="$(python3 - <<'PY' "$FAILED_JSON"
import json, sys
obj = json.loads(sys.argv[1])
items = obj.get("items") or []
print(items[0]["event_id"] if items else "")
PY
)"
    if [[ -n "$EVENT_ID" ]]; then
      echo "[soak-24h] requeue dead-letter event_id=$EVENT_ID"
      "$BIN" requeue-dead-letter --repo "$WORKDIR" --run-id "$RUN_ID" --event-id "$EVENT_ID" --limit 1 --reset-attempts >/dev/null || true
    fi
  fi
done

FINAL_STATUS="$(run_json_cmd "$BIN" status --repo "$WORKDIR" --run-id "$RUN_ID")"
python3 - <<'PY' "$FINAL_STATUS"
import json, sys
obj = json.loads(sys.argv[1])
print("[soak-24h] final", json.dumps({
    "ticks": obj.get("ticks"),
    "pending": obj.get("pending_notifications"),
    "dispatched": obj.get("dispatched_notifications"),
    "dead_letter": obj.get("dead_letter_total"),
    "acked": obj.get("acked_total"),
    "unacked": obj.get("unacked_total"),
}, ensure_ascii=False))
PY

echo "[soak-24h] done"
