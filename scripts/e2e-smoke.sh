#!/usr/bin/env bash
set -euo pipefail

BIN="${1:-./target/debug/claw-loopd}"

if [[ ! -x "$BIN" ]]; then
  echo "[e2e-smoke] binary not found or not executable: $BIN" >&2
  exit 1
fi

WORKDIR="$(mktemp -d)"
cleanup() {
  pkill -f "claw-loopd.*--repo $WORKDIR" >/dev/null 2>&1 || true
  rm -rf "$WORKDIR" >/dev/null 2>&1 || true
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

echo "[e2e-smoke] case3 single-writer lock start"
IFS='|' read -r RUN3 PID3 <<<"$(run_start 10)"
set +e
$BIN daemon --repo "$WORKDIR" --run-id "$RUN3" --tick-sec 10 >/tmp/claw-loopd-e2e-lock.log 2>&1
RC=$?
set -e
if [[ "$RC" -eq 0 ]]; then
  echo "[e2e-smoke] expected second daemon to fail, but rc=0"
  cat /tmp/claw-loopd-e2e-lock.log
  exit 1
fi
$BIN stop --repo "$WORKDIR" --run-id "$RUN3" >/dev/null || true
sleep 1

echo "[e2e-smoke] case4 pr reducer with mock gh"
MOCKDIR="$WORKDIR/mockbin"
mkdir -p "$MOCKDIR"
cat > "$MOCKDIR/gh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
if [[ "${1:-}" == "pr" && "${2:-}" == "view" ]]; then
  cat <<'JSON'
{"state":"MERGED","url":"https://example.invalid/pr/123","mergeStateStatus":"CLEAN","autoMergeRequest":null}
JSON
  exit 0
fi
if [[ "${1:-}" == "pr" && "${2:-}" == "merge" ]]; then
  exit 0
fi
echo "unsupported mock gh args: $*" >&2
exit 1
EOF
chmod +x "$MOCKDIR/gh"

OUT4="$(CLAW_LOOPD_GH_BIN="$MOCKDIR/gh" $BIN start --repo "$WORKDIR" --session-key test-session --channel discord --thread-id test-thread --tick-sec 1)"
RUN4="$(echo "$OUT4" | awk -F= '/^run_id=/{print $2}')"
if [[ -z "$RUN4" ]]; then
  echo "[e2e-smoke] failed to parse run4 id"
  echo "$OUT4"
  exit 1
fi
$BIN track-pr --repo "$WORKDIR" --run-id "$RUN4" --gh-repo demo/repo --pr 123 --merge-method merge >/dev/null
sleep 2
STATUS4="$($BIN status --repo "$WORKDIR" --run-id "$RUN4")"
assert_json_field "$STATUS4" status waiting
python3 - <<'PY' "$STATUS4"
import json, sys
obj = json.loads(sys.argv[1])
if "merged" not in (obj.get("summary") or ""):
    raise SystemExit(f"expected merged summary, got: {obj.get('summary')!r}")
if obj.get("pr_tracking") is not None:
    raise SystemExit("expected pr_tracking to be removed after merge")
PY
$BIN stop --repo "$WORKDIR" --run-id "$RUN4" >/dev/null || true
sleep 1

echo "[e2e-smoke] case5 delivery retry metrics"
cat > "$MOCKDIR/openclaw" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
STATE_FILE="${CLAW_LOOPD_MOCK_OPENCLAW_STATE:?missing CLAW_LOOPD_MOCK_OPENCLAW_STATE}"
count=0
if [[ -f "$STATE_FILE" ]]; then
  count="$(cat "$STATE_FILE")"
fi
count=$((count + 1))
echo "$count" > "$STATE_FILE"
if [[ "$count" -eq 1 ]]; then
  echo "mock openclaw transient failure" >&2
  exit 1
fi
exit 0
EOF
chmod +x "$MOCKDIR/openclaw"
MOCK_STATE="$WORKDIR/mock-openclaw-count.txt"

OUT5="$(CLAW_LOOPD_OPENCLAW_BIN="$MOCKDIR/openclaw" CLAW_LOOPD_MOCK_OPENCLAW_STATE="$MOCK_STATE" $BIN start --repo "$WORKDIR" --session-key test-session --channel discord --thread-id test-thread --tick-sec 1 --deliver-openclaw)"
RUN5="$(echo "$OUT5" | awk -F= '/^run_id=/{print $2}')"
if [[ -z "$RUN5" ]]; then
  echo "[e2e-smoke] failed to parse run5 id"
  echo "$OUT5"
  exit 1
fi
$BIN notify --repo "$WORKDIR" --run-id "$RUN5" --kind progress --message "delivery retry" >/dev/null
sleep 7
STATUS5="$($BIN status --repo "$WORKDIR" --run-id "$RUN5")"
python3 - <<'PY' "$STATUS5"
import json, sys
obj = json.loads(sys.argv[1])
metrics = obj.get("delivery_metrics") or {}
if int(metrics.get("failed_total", 0)) < 1:
    raise SystemExit(f"expected failed_total>=1, got {metrics}")
if int(metrics.get("retried_total", 0)) < 1:
    raise SystemExit(f"expected retried_total>=1, got {metrics}")
if int(obj.get("pending_notifications", 0)) != 0:
    raise SystemExit(f"expected pending_notifications=0, got {obj.get('pending_notifications')}")
PY
REPORT5="$($BIN delivery-report --repo "$WORKDIR" --run-id "$RUN5" --limit 5 --status delivered)"
python3 - <<'PY' "$REPORT5"
import json, sys
obj = json.loads(sys.argv[1])
items = obj.get("items") or []
if not items:
    raise SystemExit("expected non-empty delivery report items")
if not all(it.get("status") == "delivered" for it in items):
    raise SystemExit(f"expected only delivered items in report: {items}")
PY
$BIN stop --repo "$WORKDIR" --run-id "$RUN5" >/dev/null || true
sleep 1

echo "[e2e-smoke] case6 dead-letter + report filter"
cat > "$MOCKDIR/openclaw-fail" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
echo "mock openclaw permanent failure" >&2
exit 1
EOF
chmod +x "$MOCKDIR/openclaw-fail"

OUT6="$(CLAW_LOOPD_OPENCLAW_BIN="$MOCKDIR/openclaw-fail" CLAW_LOOPD_DELIVERY_MAX_ATTEMPTS=1 $BIN start --repo "$WORKDIR" --session-key test-session --channel discord --thread-id test-thread --tick-sec 1 --deliver-openclaw)"
RUN6="$(echo "$OUT6" | awk -F= '/^run_id=/{print $2}')"
if [[ -z "$RUN6" ]]; then
  echo "[e2e-smoke] failed to parse run6 id"
  echo "$OUT6"
  exit 1
fi
$BIN notify --repo "$WORKDIR" --run-id "$RUN6" --kind progress --message "should dead-letter A" >/dev/null
$BIN notify --repo "$WORKDIR" --run-id "$RUN6" --kind progress --message "should dead-letter B" >/dev/null
sleep 3
STATUS6="$($BIN status --repo "$WORKDIR" --run-id "$RUN6")"
python3 - <<'PY' "$STATUS6"
import json, sys
obj = json.loads(sys.argv[1])
metrics = obj.get("delivery_metrics") or {}
if int(obj.get("dead_letter_total", 0)) < 2:
    raise SystemExit(f"expected dead_letter_total>=2, got {obj.get('dead_letter_total')}")
if int(metrics.get("dead_letter_total", 0)) < 2:
    raise SystemExit(f"expected metrics.dead_letter_total>=2, got {metrics}")
if int(obj.get("pending_notifications", 0)) != 0:
    raise SystemExit(f"expected pending_notifications=0, got {obj.get('pending_notifications')}")
PY
REPORT6="$($BIN delivery-report --repo "$WORKDIR" --run-id "$RUN6" --limit 10 --status failed)"
TARGET_EVENT_ID="$(python3 - <<'PY' "$REPORT6"
import json, sys
obj = json.loads(sys.argv[1])
items = obj.get("items") or []
if len(items) < 2:
    raise SystemExit(f"expected >=2 failed items, got {items}")
if not all(it.get("status") == "failed" for it in items):
    raise SystemExit(f"expected only failed items: {items}")
hist = obj.get("failed_reason_histogram") or []
if not hist:
    raise SystemExit(f"expected failed_reason_histogram entries, got: {hist}")
if hist[0].get("reason") != "openclaw_send_failed":
    raise SystemExit(f"expected normalized reason openclaw_send_failed, got: {hist}")
by_kind = obj.get("failed_reason_histogram_by_kind") or []
if not by_kind:
    raise SystemExit(f"expected failed_reason_histogram_by_kind entries, got: {by_kind}")
if by_kind[0].get("kind") != "progress":
    raise SystemExit(f"expected kind 'progress', got: {by_kind}")
print(items[0]["event_id"])
PY
)"

REPORT6_RECENT="$($BIN delivery-report --repo "$WORKDIR" --run-id "$RUN6" --limit 10 --status failed --failed-window 1)"
python3 - <<'PY' "$REPORT6_RECENT"
import json, sys
obj = json.loads(sys.argv[1])
window = obj.get("failed_histogram_window") or {}
if window.get("mode") != "recent":
    raise SystemExit(f"expected recent window mode, got {window}")
if int(window.get("considered_failed_count", 0)) != 1:
    raise SystemExit(f"expected considered_failed_count=1, got {window}")
PY

echo "[e2e-smoke] case7 dead-letter requeue"
$BIN stop --repo "$WORKDIR" --run-id "$RUN6" >/dev/null || true
sleep 1

REQUEUE6_DRY="$($BIN requeue-dead-letter --repo "$WORKDIR" --run-id "$RUN6" --event-id "$TARGET_EVENT_ID" --limit 1 --reset-attempts --dry-run)"
python3 - <<'PY' "$REQUEUE6_DRY"
import json, sys
obj = json.loads(sys.argv[1])
if obj.get("dry_run") is not True:
    raise SystemExit(f"expected dry_run=true, got {obj}")
if int(obj.get("would_requeue", 0)) != 1:
    raise SystemExit(f"expected would_requeue==1, got {obj}")
if int(obj.get("requeued", 0)) != 0:
    raise SystemExit(f"expected requeued==0 in dry-run, got {obj}")
if obj.get("target_found") is not True:
    raise SystemExit(f"expected target_found=true in dry-run, got {obj}")
PY

REQUEUE6="$($BIN requeue-dead-letter --repo "$WORKDIR" --run-id "$RUN6" --event-id "$TARGET_EVENT_ID" --limit 1 --reset-attempts)"
python3 - <<'PY' "$REQUEUE6"
import json, sys
obj = json.loads(sys.argv[1])
if int(obj.get("requeued", 0)) != 1:
    raise SystemExit(f"expected requeued==1, got {obj}")
if obj.get("target_found") is not True:
    raise SystemExit(f"expected target_found=true, got {obj}")
if int(obj.get("remaining_dead_letter", 0)) < 1:
    raise SystemExit(f"expected remaining_dead_letter>=1, got {obj}")
PY

REQUEUE6B="$($BIN requeue-dead-letter --repo "$WORKDIR" --run-id "$RUN6" --event-id "$TARGET_EVENT_ID" --limit 1 --reset-attempts)"
python3 - <<'PY' "$REQUEUE6B"
import json, sys
obj = json.loads(sys.argv[1])
if int(obj.get("requeued", 0)) != 0:
    raise SystemExit(f"expected second requeue requeued==0, got {obj}")
if obj.get("target_found") is not False:
    raise SystemExit(f"expected target_found=false on second requeue, got {obj}")
PY

REPORT6B="$($BIN delivery-report --repo "$WORKDIR" --run-id "$RUN6" --limit 10 --status pending)"
python3 - <<'PY' "$REPORT6B" "$TARGET_EVENT_ID"
import json, sys
obj = json.loads(sys.argv[1])
target = sys.argv[2]
items = obj.get("items") or []
if not items:
    raise SystemExit("expected pending items after dead-letter requeue")
if not all(it.get("status") == "pending" for it in items):
    raise SystemExit(f"expected only pending items: {items}")
if not any(it.get("event_id") == target for it in items):
    raise SystemExit(f"expected target event in pending report: {items}")
PY

echo "[e2e-smoke] ok"
