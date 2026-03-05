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

echo "[e2e-smoke] case1b auto-stop guard start"
OUT1B="$($BIN start --repo "$WORKDIR" --session-key test-session --channel discord --thread-id test-thread --tick-sec 1 --max-ticks 1)"
RUN1B="$(echo "$OUT1B" | awk -F= '/^run_id=/{print $2}')"
if [[ -z "$RUN1B" ]]; then
  echo "[e2e-smoke] failed to parse run1b id"
  echo "$OUT1B"
  exit 1
fi
sleep 3
STATUS1C="$($BIN status --repo "$WORKDIR" --run-id "$RUN1B")"
assert_json_field "$STATUS1C" status stopped
python3 - <<'PY' "$STATUS1C"
import json, sys
obj = json.loads(sys.argv[1])
summary = obj.get("summary") or ""
if "auto-stopped" not in summary:
    raise SystemExit(f"expected auto-stopped summary, got: {summary!r}")
if int(obj.get("ticks", 0)) < 1:
    raise SystemExit(f"expected ticks>=1, got: {obj.get('ticks')}")
PY


echo "[e2e-smoke] case1c dogfood runner sequential gate"
TASKFILE="$WORKDIR/docs/roadmaps/ack-integration-tasklist.md"
mkdir -p "$(dirname "$TASKFILE")"
cat > "$TASKFILE" <<'EOF'
- [ ] R1: first
- [ ] R2: second
EOF
OUT1C="$($BIN start --repo "$WORKDIR" --session-key test-session --channel discord --thread-id test-thread --tick-sec 1 --task-runner-cmd 'echo start:$CLAW_TASK_ID' --auto-check-on-success false --max-task-loops 10)"
RUN1C="$(echo "$OUT1C" | awk -F= '/^run_id=/{print $2}')"
if [[ -z "$RUN1C" ]]; then
  echo "[e2e-smoke] failed to parse run1c id"
  echo "$OUT1C"
  exit 1
fi
sleep 3
STATUS1D="$($BIN status --repo "$WORKDIR" --run-id "$RUN1C")"
python3 - <<'PY' "$STATUS1D"
import json, sys
obj = json.loads(sys.argv[1])
runner = obj.get("runner") or {}
if obj.get("status") != "waiting":
    raise SystemExit(f"expected waiting, got {obj.get('status')!r}")
if int(runner.get("task_loops_started", 0)) != 1:
    raise SystemExit(f"expected loops_started=1, got {runner}")
if runner.get("active_task_id") != "R1":
    raise SystemExit(f"expected active_task_id=R1, got {runner}")
PY
$BIN task-check --file "$TASKFILE" --id R1 --done true >/dev/null
STATUS1E=""
for _ in {1..10}; do
  STATUS1E="$($BIN status --repo "$WORKDIR" --run-id "$RUN1C")"
  if python3 - <<'PY' "$STATUS1E"
import json, sys
obj = json.loads(sys.argv[1])
runner = obj.get("runner") or {}
ok = int(runner.get("task_loops_started", 0)) >= 2 and runner.get("active_task_id") == "R2"
raise SystemExit(0 if ok else 1)
PY
  then
    break
  fi
  sleep 1
done
python3 - <<'PY' "$STATUS1E"
import json, sys
obj = json.loads(sys.argv[1])
runner = obj.get("runner") or {}
if int(runner.get("task_loops_started", 0)) < 2:
    raise SystemExit(f"expected loops_started>=2, got {runner}")
if runner.get("active_task_id") != "R2":
    raise SystemExit(f"expected active_task_id=R2, got {runner}")
PY
$BIN stop --repo "$WORKDIR" --run-id "$RUN1C" --immediate >/dev/null || true
sleep 1


echo "[e2e-smoke] case1d runner waiting state (no block)"
TASKFILE_W="$WORKDIR/docs/roadmaps/waiting-tasklist.md"
cat > "$TASKFILE_W" <<'EOF'
- [ ] W1: wait merge
EOF
OUT1D="$($BIN start --repo "$WORKDIR" --session-key test-session --channel discord --thread-id test-thread --tick-sec 1 --task-file "$TASKFILE_W" --task-runner-cmd 'echo "TASK_WAITING_MERGE PR_URL=https://example.invalid/pr/1"; exit 10' --auto-check-on-success true --max-task-loops 10)"
RUN1D="$(echo "$OUT1D" | awk -F= '/^run_id=/{print $2}')"
if [[ -z "$RUN1D" ]]; then
  echo "[e2e-smoke] failed to parse run1d id"
  echo "$OUT1D"
  exit 1
fi
sleep 3
STATUS1F="$($BIN status --repo "$WORKDIR" --run-id "$RUN1D")"
python3 - <<'PY' "$STATUS1F"
import json, sys
obj = json.loads(sys.argv[1])
runner = obj.get("runner") or {}
if obj.get("status") != "waiting":
    raise SystemExit(f"expected waiting, got {obj.get('status')!r}")
if "TASK_WAITING_MERGE" not in (obj.get("waiting_reason") or ""):
    raise SystemExit(f"expected waiting reason to include TASK_WAITING_MERGE, got {obj.get('waiting_reason')!r}")
if int(runner.get("task_loops_started", 0)) != 0:
    raise SystemExit(f"expected loops_started=0, got {runner}")
if int(obj.get("task_done_current", 0)) != 0:
    raise SystemExit(f"expected task_done_current=0, got {obj.get('task_done_current')}")
PY
$BIN stop --repo "$WORKDIR" --run-id "$RUN1D" --immediate >/dev/null || true
sleep 1


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
if int(obj.get("acked_total", 0)) < 1:
    raise SystemExit(f"expected acked_total>=1, got {obj.get('acked_total')}")
if int(obj.get("ack_entries_total", 0)) < 1:
    raise SystemExit(f"expected ack_entries_total>=1, got {obj.get('ack_entries_total')}")
PY
ACK5_PATH="$WORKDIR/.ralph/runs/$RUN5/notify-ack.jsonl"
python3 - <<'PY' "$ACK5_PATH"
import json, sys
path = sys.argv[1]
rows = [json.loads(line) for line in open(path) if line.strip()]
if len(rows) < 2:
    raise SystemExit(f"expected >=2 ack rows, got {len(rows)}")
oks = [r for r in rows if r.get("ok") is True]
fails = [r for r in rows if r.get("ok") is False]
if not oks:
    raise SystemExit(f"expected success ack rows, got {rows}")
if not fails:
    raise SystemExit(f"expected failure ack rows, got {rows}")
if not any(r.get("category") == "transport" for r in fails):
    raise SystemExit(f"expected transport category in failed ack rows, got {fails}")
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
if not all(it.get("acked") is True for it in items):
    raise SystemExit(f"expected delivered items to be acked=true: {items}")
if not all(it.get("ack_at") is not None for it in items):
    raise SystemExit(f"expected delivered items to include ack_at: {items}")
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
ACK6_PATH="$WORKDIR/.ralph/runs/$RUN6/notify-ack.jsonl"
python3 - <<'PY' "$ACK6_PATH"
import json, sys
path = sys.argv[1]
rows = [json.loads(line) for line in open(path) if line.strip()]
if len(rows) < 2:
    raise SystemExit(f"expected >=2 ack rows for run6, got {len(rows)}")
if not all(r.get("ok") is False for r in rows):
    raise SystemExit(f"expected all run6 ack rows to be failures, got {rows}")
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
if not all(it.get("acked") is False for it in items):
    raise SystemExit(f"expected failed items to be acked=false: {items}")
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

echo "[e2e-smoke] case8 resend + ack consistency"
cat > "$MOCKDIR/openclaw-ok" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
exit 0
EOF
chmod +x "$MOCKDIR/openclaw-ok"

OUT8="$(CLAW_LOOPD_OPENCLAW_BIN="$MOCKDIR/openclaw-fail" CLAW_LOOPD_DELIVERY_MAX_ATTEMPTS=1 $BIN start --repo "$WORKDIR" --session-key test-session --channel discord --thread-id test-thread --tick-sec 1 --deliver-openclaw)"
RUN8="$(echo "$OUT8" | awk -F= '/^run_id=/{print $2}')"
if [[ -z "$RUN8" ]]; then
  echo "[e2e-smoke] failed to parse run8 id"
  echo "$OUT8"
  exit 1
fi
$BIN notify --repo "$WORKDIR" --run-id "$RUN8" --kind progress --message "run8 fail then resend" >/dev/null
sleep 3
REPORT8_FAIL="$($BIN delivery-report --repo "$WORKDIR" --run-id "$RUN8" --limit 10 --status failed)"
EVENT8_ID="$(python3 - <<'PY' "$REPORT8_FAIL"
import json, sys
obj = json.loads(sys.argv[1])
items = obj.get("items") or []
if not items:
    raise SystemExit(f"expected failed items for run8, got {items}")
print(items[0]["event_id"])
PY
)"
$BIN stop --repo "$WORKDIR" --run-id "$RUN8" >/dev/null || true
sleep 1

RUN8_DIR="$WORKDIR/.ralph/runs/$RUN8"
python3 - <<'PY' "$RUN8_DIR" "$EVENT8_ID" "$RUN8"
import json, pathlib, sys
run_dir = pathlib.Path(sys.argv[1])
event_id = sys.argv[2]
run_id = sys.argv[3]
dead_path = run_dir / "notify-dead-letter.jsonl"
queue_path = run_dir / "notify-queue.jsonl"
rows = [json.loads(line) for line in dead_path.read_text().splitlines() if line.strip()]
keep = []
target = None
for row in rows:
    if row.get("event_id") == event_id and target is None:
        target = row
    else:
        keep.append(row)
if target is None:
    raise SystemExit(f"target dead-letter event not found: {event_id}")
dead_path.write_text("" if not keep else "\n".join(json.dumps(r) for r in keep) + "\n")
with queue_path.open("a") as qf:
    qf.write(json.dumps({
        "event_id": target["event_id"],
        "run_id": run_id,
        "ts": "2020-01-01T00:00:00Z",
        "channel": "discord",
        "thread_id": "test-thread",
        "kind": target["kind"],
        "message": target["message"],
        "attempts": target["attempts"],
        "next_retry_at": None,
        "last_error": target.get("last_error"),
    }) + "\n")
PY

CLAW_LOOPD_OPENCLAW_BIN="$MOCKDIR/openclaw-ok" $BIN daemon --repo "$WORKDIR" --run-id "$RUN8" --tick-sec 1 >/tmp/claw-loopd-e2e-run8.log 2>&1 || true
sleep 1

STATUS8="$($BIN status --repo "$WORKDIR" --run-id "$RUN8")"
python3 - <<'PY' "$STATUS8"
import json, sys
obj = json.loads(sys.argv[1])
if int(obj.get("pending_notifications", 0)) != 0:
    raise SystemExit(f"expected pending_notifications=0 for run8, got {obj.get('pending_notifications')}")
if int(obj.get("dispatched_notifications", 0)) < 1:
    raise SystemExit(f"expected dispatched_notifications>=1 for run8, got {obj.get('dispatched_notifications')}")
if int(obj.get("acked_total", 0)) < 1:
    raise SystemExit(f"expected acked_total>=1 for run8, got {obj.get('acked_total')}")
PY

ACK8_PATH="$WORKDIR/.ralph/runs/$RUN8/notify-ack.jsonl"
python3 - <<'PY' "$ACK8_PATH" "$EVENT8_ID"
import json, sys
path = sys.argv[1]
event_id = sys.argv[2]
rows = []
for line in open(path):
    line = line.strip()
    if not line:
        continue
    row = json.loads(line)
    if row.get("event_id") == event_id:
        rows.append(row)
if len(rows) < 2:
    raise SystemExit(f"expected >=2 ack rows for event {event_id}, got {rows}")
if not any(r.get("ok") is False for r in rows):
    raise SystemExit(f"expected at least one failed ack row, got {rows}")
if not any(r.get("ok") is True for r in rows):
    raise SystemExit(f"expected at least one success ack row, got {rows}")
keys = {(r.get("event_id"), r.get("attempts")) for r in rows}
if len(keys) != len(rows):
    raise SystemExit(f"expected no duplicate (event_id,attempts) keys, got {rows}")
PY

echo "[e2e-smoke] case9 recovery reconcile + ack dedupe"
OUT9="$($BIN start --repo "$WORKDIR" --session-key test-session --channel discord --thread-id test-thread --tick-sec 1)"
RUN9="$(echo "$OUT9" | awk -F= '/^run_id=/{print $2}')"
if [[ -z "$RUN9" ]]; then
  echo "[e2e-smoke] failed to parse run9 id"
  echo "$OUT9"
  exit 1
fi
$BIN notify --repo "$WORKDIR" --run-id "$RUN9" --kind progress --message "run9 reconcile seed" >/dev/null
sleep 2
$BIN stop --repo "$WORKDIR" --run-id "$RUN9" >/dev/null || true
sleep 1

RUN9_DIR="$WORKDIR/.ralph/runs/$RUN9"
python3 - <<'PY' "$RUN9_DIR"
import json, pathlib, sys
run_dir = pathlib.Path(sys.argv[1])
ack_path = run_dir / "notify-ack.jsonl"
queue_path = run_dir / "notify-queue.jsonl"
acks = [json.loads(line) for line in ack_path.read_text().splitlines() if line.strip()]
ok = next((r for r in acks if r.get("ok") is True), None)
if ok is None:
    raise SystemExit(f"expected at least one success ack row: {acks}")
with queue_path.open("a") as qf:
    qf.write(json.dumps({
        "event_id": ok["event_id"],
        "run_id": ok["run_id"],
        "ts": "2020-01-01T00:00:00Z",
        "channel": "discord",
        "thread_id": "test-thread",
        "kind": "progress",
        "message": "stale queued duplicate",
        "attempts": 0,
        "next_retry_at": None,
        "last_error": None,
    }) + "\n")
with ack_path.open("a") as af:
    af.write(json.dumps(ok) + "\n")
PY

$BIN daemon --repo "$WORKDIR" --run-id "$RUN9" --tick-sec 1 >/tmp/claw-loopd-e2e-run9.log 2>&1 || true

python3 - <<'PY' "$RUN9_DIR"
import json, pathlib, sys
run_dir = pathlib.Path(sys.argv[1])
queue_path = run_dir / "notify-queue.jsonl"
ack_path = run_dir / "notify-ack.jsonl"
dispatched_path = run_dir / "notify-dispatched.jsonl"

def read_jsonl(path):
    if not path.exists():
        return []
    return [json.loads(line) for line in path.read_text().splitlines() if line.strip()]

queue_rows = read_jsonl(queue_path)
ack_rows = read_jsonl(ack_path)
events = read_jsonl(run_dir / "events.jsonl")
reconciles = [e for e in events if e.get("kind") == "delivery_reconciled"]
if not reconciles:
    raise SystemExit("expected delivery_reconciled event")
last = reconciles[-1].get("extra") or {}
if int(last.get("removed_stale_queued", 0)) < 1:
    raise SystemExit(f"expected removed_stale_queued>=1, got {last}")
if int(last.get("removed_ack_duplicates", 0)) < 1:
    raise SystemExit(f"expected removed_ack_duplicates>=1, got {last}")
keys = [(r.get("event_id"), r.get("attempts")) for r in ack_rows]
if len(keys) != len(set(keys)):
    raise SystemExit(f"expected ack keys deduped after reconcile, got duplicates in {ack_rows}")
terminal_ids = {r.get("event_id") for r in read_jsonl(dispatched_path)}
if any(r.get("event_id") in terminal_ids for r in queue_rows):
    raise SystemExit(f"expected no stale queued terminal rows after reconcile, got queue={queue_rows}")
PY

echo "[e2e-smoke] ok"
