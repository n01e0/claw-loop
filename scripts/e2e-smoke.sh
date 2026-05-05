#!/usr/bin/env bash
set -euo pipefail

BIN="${1:-./target/debug/claw-loopd}"
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if [[ ! -x "$BIN" ]]; then
  echo "[e2e-smoke] binary not found or not executable: $BIN" >&2
  exit 1
fi

WORKDIR="$(mktemp -d)"
git -C "$WORKDIR" init -q
git -C "$WORKDIR" config user.email e2e-smoke@example.invalid
git -C "$WORKDIR" config user.name e2e-smoke
printf 'smoke\n' > "$WORKDIR/.gitkeep"
git -C "$WORKDIR" add .gitkeep
git -C "$WORKDIR" commit -q -m 'init smoke repo'
cleanup() {
  pkill -f "claw-loopd.*--repo $WORKDIR" >/dev/null 2>&1 || true
  rm -rf "$WORKDIR" >/dev/null 2>&1 || true
}
trap cleanup EXIT

approve_task_file() {
  local task_file="$1"
  mkdir -p "$(dirname "$task_file")"
  if [[ ! -f "$task_file" ]]; then
    cat > "$task_file" <<'EOF'
- [ ] T1: smoke
EOF
  fi
  $BIN task-approve --file "$task_file" --approved-by e2e-smoke | python3 -c 'import json,sys; print(json.load(sys.stdin)["approved_tasklist_hash"])'
}

run_start() {
  local tick="$1"
  local task_file="$WORKDIR/docs/roadmaps/default-tasklist.md"
  local approved_hash
  approved_hash="$(approve_task_file "$task_file")"
  local out
  out="$($BIN start --repo "$WORKDIR" --session-key test-session --channel discord --thread-id test-thread --tick-sec "$tick" --task-file "$task_file" --require-task-approval --approved-tasklist-hash "$approved_hash")"
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
TASKFILE1B="$WORKDIR/docs/roadmaps/auto-stop-tasklist.md"
cat > "$TASKFILE1B" <<'EOF'
- [ ] S1: auto-stop
EOF
APPROVED1B="$(approve_task_file "$TASKFILE1B")"
OUT1B="$($BIN start --repo "$WORKDIR" --session-key test-session --channel discord --thread-id test-thread --tick-sec 1 --max-ticks 1 --task-file "$TASKFILE1B" --require-task-approval --approved-tasklist-hash "$APPROVED1B")"
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
APPROVED1C="$(approve_task_file "$TASKFILE")"
OUT1C="$($BIN start --repo "$WORKDIR" --session-key test-session --channel discord --thread-id test-thread --tick-sec 1 --task-file "$TASKFILE" --task-runner-cmd 'echo start:$CLAW_TASK_ID' --auto-check-on-success false --max-task-loops 10 --require-task-approval --approved-tasklist-hash "$APPROVED1C")"
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
if runner.get("current_task_id") != "R1":
    raise SystemExit(f"expected current_task_id=R1, got {runner}")
PY
$BIN task-check --file "$TASKFILE" --id R1 --done true >/dev/null
STATUS1E=""
for _ in {1..10}; do
  STATUS1E="$($BIN status --repo "$WORKDIR" --run-id "$RUN1C")"
  if python3 - <<'PY' "$STATUS1E"
import json, sys
obj = json.loads(sys.argv[1])
runner = obj.get("runner") or {}
ok = int(runner.get("task_loops_started", 0)) >= 2 and runner.get("current_task_id") == "R2"
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
if runner.get("current_task_id") != "R2":
    raise SystemExit(f"expected current_task_id=R2, got {runner}")
PY
$BIN stop --repo "$WORKDIR" --run-id "$RUN1C" --immediate >/dev/null || true
sleep 1


echo "[e2e-smoke] case1d runner waiting state (no block)"
WAIT_MOCKDIR="$WORKDIR/mockbin-waiting"
mkdir -p "$WAIT_MOCKDIR"
cat > "$WAIT_MOCKDIR/gh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
if [[ "${1:-}" == "pr" && "${2:-}" == "view" ]]; then
  cat <<'JSON'
{"state":"OPEN","url":"https://github.com/demo/repo/pull/1","mergeStateStatus":"BLOCKED","autoMergeRequest":{"enabledAt":"2026-03-14T00:00:00Z"},"statusCheckRollup":[]}
JSON
  exit 0
fi
echo "unsupported mock gh args: $*" >&2
exit 1
EOF
chmod +x "$WAIT_MOCKDIR/gh"
TASKFILE_W="$WORKDIR/docs/roadmaps/waiting-tasklist.md"
cat > "$TASKFILE_W" <<'EOF'
- [ ] W1: wait merge
EOF
APPROVED1D="$(approve_task_file "$TASKFILE_W")"
OUT1D="$(CLAW_LOOPD_GH_BIN="$WAIT_MOCKDIR/gh" CLAW_LOOPD_STUCK_WAIT_TICKS=2 $BIN start --repo "$WORKDIR" --session-key test-session --channel discord --thread-id test-thread --tick-sec 1 --task-file "$TASKFILE_W" --task-runner-cmd 'echo "TASK_WAITING_MERGE PR_URL=https://github.com/demo/repo/pull/1"; exit 10' --auto-check-on-success true --max-task-loops 10 --require-task-approval --approved-tasklist-hash "$APPROVED1D")"
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
if int(runner.get("waiting_unchanged_ticks", 0)) < 2:
    raise SystemExit(f"expected waiting_unchanged_ticks>=2, got {runner}")
if int(runner.get("waiting_last_notified_ticks", 0)) < 2:
    raise SystemExit(f"expected waiting_last_notified_ticks>=2, got {runner}")
PY
python3 - <<'PY' "$WORKDIR/.ralph/runs/$RUN1D/events.jsonl"
import json, sys
path = sys.argv[1]
events = [json.loads(line) for line in open(path) if line.strip()]
if not any(e.get("kind") == "task_waiting_stuck" for e in events):
    raise SystemExit(f"expected task_waiting_stuck event, got kinds={[e.get('kind') for e in events][-10:]}")
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

TASKFILE4="$WORKDIR/docs/roadmaps/pr-track-tasklist.md"
cat > "$TASKFILE4" <<'EOF'
- [ ] P1: track pr
EOF
APPROVED4="$(approve_task_file "$TASKFILE4")"
OUT4="$(CLAW_LOOPD_GH_BIN="$MOCKDIR/gh" $BIN start --repo "$WORKDIR" --session-key test-session --channel discord --thread-id test-thread --tick-sec 1 --task-file "$TASKFILE4" --require-task-approval --approved-tasklist-hash "$APPROVED4")"
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

TASKFILE5="$WORKDIR/docs/roadmaps/delivery-retry-tasklist.md"
cat > "$TASKFILE5" <<'EOF'
- [ ] D1: delivery retry
EOF
APPROVED5="$(approve_task_file "$TASKFILE5")"
OUT5="$(CLAW_LOOPD_OPENCLAW_BIN="$MOCKDIR/openclaw" CLAW_LOOPD_MOCK_OPENCLAW_STATE="$MOCK_STATE" $BIN start --repo "$WORKDIR" --session-key test-session --channel discord --thread-id test-thread --tick-sec 1 --deliver-openclaw --task-file "$TASKFILE5" --require-task-approval --approved-tasklist-hash "$APPROVED5")"
RUN5="$(echo "$OUT5" | awk -F= '/^run_id=/{print $2}')"
if [[ -z "$RUN5" ]]; then
  echo "[e2e-smoke] failed to parse run5 id"
  echo "$OUT5"
  exit 1
fi
CLAW_LOOPD_OPENCLAW_BIN="$MOCKDIR/openclaw" CLAW_LOOPD_MOCK_OPENCLAW_STATE="$MOCK_STATE" $BIN notify --repo "$WORKDIR" --run-id "$RUN5" --kind progress --message "delivery retry" >/dev/null
sleep 7
STATUS5="$($BIN status --repo "$WORKDIR" --run-id "$RUN5")"
python3 - <<'PY' "$STATUS5"
import json, sys
obj = json.loads(sys.argv[1])
metrics = obj.get("delivery_metrics") or {}
if int(metrics.get("delivered_total", 0)) < 1:
    raise SystemExit(f"expected delivered_total>=1, got {metrics}")
if int(obj.get("pending_notifications", 0)) < 0:
    raise SystemExit(f"pending_notifications should be non-negative, got {obj.get('pending_notifications')}")
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
if len(rows) < 1:
    raise SystemExit(f"expected >=1 ack rows, got {len(rows)}")
oks = [r for r in rows if r.get("ok") is True]
if not oks:
    raise SystemExit(f"expected success ack rows, got {rows}")
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

TASKFILE6="$WORKDIR/docs/roadmaps/dead-letter-tasklist.md"
cat > "$TASKFILE6" <<'EOF'
- [ ] D2: dead letter
EOF
APPROVED6="$(approve_task_file "$TASKFILE6")"
OUT6="$(CLAW_LOOPD_OPENCLAW_BIN="$MOCKDIR/openclaw-fail" CLAW_LOOPD_DELIVERY_MAX_ATTEMPTS=1 $BIN start --repo "$WORKDIR" --session-key test-session --channel discord --thread-id test-thread --tick-sec 1 --deliver-openclaw --task-file "$TASKFILE6" --require-task-approval --approved-tasklist-hash "$APPROVED6")"
RUN6="$(echo "$OUT6" | awk -F= '/^run_id=/{print $2}')"
if [[ -z "$RUN6" ]]; then
  echo "[e2e-smoke] failed to parse run6 id"
  echo "$OUT6"
  exit 1
fi
CLAW_LOOPD_OPENCLAW_BIN="$MOCKDIR/openclaw-fail" CLAW_LOOPD_DELIVERY_MAX_ATTEMPTS=1 $BIN notify --repo "$WORKDIR" --run-id "$RUN6" --kind blocked --message "should dead-letter A" >/dev/null
CLAW_LOOPD_OPENCLAW_BIN="$MOCKDIR/openclaw-fail" CLAW_LOOPD_DELIVERY_MAX_ATTEMPTS=1 $BIN notify --repo "$WORKDIR" --run-id "$RUN6" --kind blocked --message "should dead-letter B" >/dev/null
sleep 3
STATUS6="$($BIN status --repo "$WORKDIR" --run-id "$RUN6")"
python3 - <<'PY' "$STATUS6"
import json, sys
obj = json.loads(sys.argv[1])
metrics = obj.get("delivery_metrics") or {}
if int(obj.get("dead_letter_total", 0)) < 1:
    raise SystemExit(f"expected dead_letter_total>=1, got {obj.get('dead_letter_total')}")
if int(metrics.get("dead_letter_total", 0)) < 1:
    raise SystemExit(f"expected metrics.dead_letter_total>=1, got {metrics}")
# status-message establishment retries may legitimately leave pending bootstrap events.
if int(obj.get("pending_notifications", 0)) < 0:
    raise SystemExit(f"pending_notifications should be non-negative, got {obj.get('pending_notifications')}")
PY
ACK6_PATH="$WORKDIR/.ralph/runs/$RUN6/notify-ack.jsonl"
python3 - <<'PY' "$ACK6_PATH"
import json, sys
path = sys.argv[1]
rows = [json.loads(line) for line in open(path) if line.strip()]
if len(rows) < 1:
    raise SystemExit(f"expected >=1 ack rows for run6, got {len(rows)}")
if not all(r.get("ok") is False for r in rows):
    raise SystemExit(f"expected all run6 ack rows to be failures, got {rows}")
PY
REPORT6="$($BIN delivery-report --repo "$WORKDIR" --run-id "$RUN6" --limit 10 --status failed)"
TARGET_EVENT_ID="$(python3 - <<'PY' "$REPORT6"
import json, sys
obj = json.loads(sys.argv[1])
items = obj.get("items") or []
if len(items) < 1:
    raise SystemExit(f"expected >=1 failed items, got {items}")
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
if by_kind[0].get("kind") != "blocked":
    raise SystemExit(f"expected kind 'blocked', got: {by_kind}")
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

TASKFILE8="$WORKDIR/docs/roadmaps/resend-ack-tasklist.md"
cat > "$TASKFILE8" <<'EOF'
- [ ] D3: resend ack
EOF
APPROVED8="$(approve_task_file "$TASKFILE8")"
OUT8="$(CLAW_LOOPD_OPENCLAW_BIN="$MOCKDIR/openclaw-fail" CLAW_LOOPD_DELIVERY_MAX_ATTEMPTS=1 $BIN start --repo "$WORKDIR" --session-key test-session --channel discord --thread-id test-thread --tick-sec 1 --deliver-openclaw --task-file "$TASKFILE8" --require-task-approval --approved-tasklist-hash "$APPROVED8")"
RUN8="$(echo "$OUT8" | awk -F= '/^run_id=/{print $2}')"
if [[ -z "$RUN8" ]]; then
  echo "[e2e-smoke] failed to parse run8 id"
  echo "$OUT8"
  exit 1
fi
RUN8_DIR="$WORKDIR/.ralph/runs/$RUN8"
$BIN stop --repo "$WORKDIR" --run-id "$RUN8" >/dev/null || true
sleep 1
EVENT8_ID="$(python3 - <<'PY' "$RUN8_DIR" "$RUN8"
import datetime, json, pathlib, sys, uuid
run_dir = pathlib.Path(sys.argv[1])
run_id = sys.argv[2]

dead_path = run_dir / "notify-dead-letter.jsonl"
ack_path = run_dir / "notify-ack.jsonl"
for path in (dead_path, ack_path):
    if not path.exists():
        path.write_text("")

event_id = str(uuid.uuid4())
now = datetime.datetime.utcnow().isoformat() + "Z"

with dead_path.open("a") as df:
    df.write(json.dumps({
        "event_id": event_id,
        "run_id": run_id,
        "moved_at": now,
        "attempts": 1,
        "kind": "blocked",
        "message": "run8 fail then resend",
        "last_error": "synthetic seed failure",
    }) + "\n")

with ack_path.open("a") as af:
    af.write(json.dumps({
        "event_id": event_id,
        "run_id": run_id,
        "acked_at": now,
        "ok": False,
        "category": "transport",
        "attempts": 1,
        "error": "synthetic seed failure",
    }) + "\n")

print(event_id)
PY
)"

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
if int(obj.get("pending_notifications", 0)) < 0:
    raise SystemExit(f"pending_notifications should be non-negative for run8, got {obj.get('pending_notifications')}")
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
TASKFILE9="$WORKDIR/docs/roadmaps/recovery-reconcile-tasklist.md"
cat > "$TASKFILE9" <<'EOF'
- [ ] R9: reconcile
EOF
APPROVED9="$(approve_task_file "$TASKFILE9")"
OUT9="$($BIN start --repo "$WORKDIR" --session-key test-session --channel discord --thread-id test-thread --tick-sec 1 --task-file "$TASKFILE9" --require-task-approval --approved-tasklist-hash "$APPROVED9")"
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

echo "[e2e-smoke] case10 single-status post reduction + duplicate suppression"
cat > "$MOCKDIR/openclaw-status-mode" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
STATE_DIR="${CLAW_LOOPD_STATUS_MOCK_DIR:?missing CLAW_LOOPD_STATUS_MOCK_DIR}"
mkdir -p "$STATE_DIR"
LOG_FILE="$STATE_DIR/calls.jsonl"
SEND_COUNT_FILE="$STATE_DIR/send_count"
EDIT_COUNT_FILE="$STATE_DIR/edit_count"
LAST_STATUS_ID_FILE="$STATE_DIR/last_status_id"

send_count=0
edit_count=0
if [[ -f "$SEND_COUNT_FILE" ]]; then
  send_count="$(cat "$SEND_COUNT_FILE")"
fi
if [[ -f "$EDIT_COUNT_FILE" ]]; then
  edit_count="$(cat "$EDIT_COUNT_FILE")"
fi

action="${2:-}"
message_id=""
for ((i=1; i<=$#; i++)); do
  arg="${!i}"
  if [[ "$arg" == "--message-id" ]]; then
    j=$((i + 1))
    message_id="${!j:-}"
  fi
done

if [[ "$action" == "send" ]]; then
  send_count=$((send_count + 1))
  echo "$send_count" > "$SEND_COUNT_FILE"
  new_id="msg-${send_count}"
  if [[ ! -f "$LAST_STATUS_ID_FILE" ]]; then
    echo "$new_id" > "$LAST_STATUS_ID_FILE"
  fi
  printf '{"action":"send","message_id":null,"returned_id":"%s"}\n' "$new_id" >> "$LOG_FILE"
  printf '{"payload":{"result":{"messageId":"%s"}}}\n' "$new_id"
  exit 0
fi

if [[ "$action" == "edit" ]]; then
  edit_count=$((edit_count + 1))
  echo "$edit_count" > "$EDIT_COUNT_FILE"
  printf '{"action":"edit","message_id":"%s"}\n' "$message_id" >> "$LOG_FILE"
  printf '{"payload":{"result":{"messageId":"%s"}}}\n' "$message_id"
  exit 0
fi

echo "unsupported mock openclaw action: $*" >&2
exit 1
EOF
chmod +x "$MOCKDIR/openclaw-status-mode"

STATUS_MOCK_DIR="$WORKDIR/status-mock"
TASKFILE10="$WORKDIR/docs/roadmaps/status-mode-tasklist.md"
cat > "$TASKFILE10" <<'EOF'
- [ ] S10: status mode
EOF
APPROVED10="$(approve_task_file "$TASKFILE10")"
OUT10="$(CLAW_LOOPD_OPENCLAW_BIN="$MOCKDIR/openclaw-status-mode" CLAW_LOOPD_STATUS_MOCK_DIR="$STATUS_MOCK_DIR" $BIN start --repo "$WORKDIR" --session-key test-session --channel discord --thread-id test-thread --tick-sec 1 --deliver-openclaw --task-file "$TASKFILE10" --require-task-approval --approved-tasklist-hash "$APPROVED10")"
RUN10="$(echo "$OUT10" | awk -F= '/^run_id=/{print $2}')"
if [[ -z "$RUN10" ]]; then
  echo "[e2e-smoke] failed to parse run10 id"
  echo "$OUT10"
  exit 1
fi

CLAW_LOOPD_OPENCLAW_BIN="$MOCKDIR/openclaw-status-mode" CLAW_LOOPD_STATUS_MOCK_DIR="$STATUS_MOCK_DIR" $BIN notify --repo "$WORKDIR" --run-id "$RUN10" --kind progress --message "run10 progress #1" >/dev/null
CLAW_LOOPD_OPENCLAW_BIN="$MOCKDIR/openclaw-status-mode" CLAW_LOOPD_STATUS_MOCK_DIR="$STATUS_MOCK_DIR" $BIN notify --repo "$WORKDIR" --run-id "$RUN10" --kind progress --message "run10 progress #2" >/dev/null
CLAW_LOOPD_OPENCLAW_BIN="$MOCKDIR/openclaw-status-mode" CLAW_LOOPD_STATUS_MOCK_DIR="$STATUS_MOCK_DIR" $BIN notify --repo "$WORKDIR" --run-id "$RUN10" --kind progress --message "run10 progress #3" >/dev/null
sleep 4

STATUS10="$($BIN status --repo "$WORKDIR" --run-id "$RUN10")"
python3 - <<'PY' "$STATUS10" "$STATUS_MOCK_DIR"
import json, pathlib, sys
status = json.loads(sys.argv[1])
mock_dir = pathlib.Path(sys.argv[2])
calls_path = mock_dir / "calls.jsonl"
if not calls_path.exists():
    raise SystemExit("expected calls.jsonl for run10")
rows = [json.loads(line) for line in calls_path.read_text().splitlines() if line.strip()]
sends = [r for r in rows if r.get("action") == "send"]
edits = [r for r in rows if r.get("action") == "edit"]
if len(sends) < 1:
    raise SystemExit(f"expected >=1 send for status bootstrap, got {len(sends)} rows={rows}")
if len(edits) < 1:
    raise SystemExit(f"expected >=1 edits for progress updates, got {len(edits)} rows={rows}")
runner = status.get("runner") or {}
status_id = runner.get("status_message_id")
if not status_id:
    raise SystemExit(f"expected non-empty runner.status_message_id, got {runner}")
if not any(s.get("returned_id") == status_id for s in sends):
    raise SystemExit(f"expected status_message_id={status_id} to be one of send ids, sends={sends}")
if not all(e.get("message_id") == status_id for e in edits):
    raise SystemExit(f"expected all edits to target current status message id={status_id}, got edits={edits}")
if not runner.get("status_updated_at"):
    raise SystemExit(f"expected runner.status_updated_at to be set, got {runner}")
if int(status.get("pending_notifications", 0)) != 0:
    raise SystemExit(f"expected pending_notifications=0, got {status.get('pending_notifications')}")
PY

$BIN stop --repo "$WORKDIR" --run-id "$RUN10" >/dev/null || true
sleep 1

echo "[e2e-smoke] case11 task single-status + edit fallback + no missing/duplicate final notify"
cat > "$MOCKDIR/openclaw-task-fallback" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
STATE_DIR="${CLAW_LOOPD_STATUS_MOCK_DIR:?missing CLAW_LOOPD_STATUS_MOCK_DIR}"
mkdir -p "$STATE_DIR"
LOG_FILE="$STATE_DIR/calls-case11.jsonl"
SEND_COUNT_FILE="$STATE_DIR/send_count_case11"
EDIT_COUNT_FILE="$STATE_DIR/edit_count_case11"
FAILED_ONCE_FILE="$STATE_DIR/edit_failed_once_case11"

send_count=0
edit_count=0
if [[ -f "$SEND_COUNT_FILE" ]]; then
  send_count="$(cat "$SEND_COUNT_FILE")"
fi
if [[ -f "$EDIT_COUNT_FILE" ]]; then
  edit_count="$(cat "$EDIT_COUNT_FILE")"
fi

action="${2:-}"
message_id=""
for ((i=1; i<=$#; i++)); do
  arg="${!i}"
  if [[ "$arg" == "--message-id" ]]; then
    j=$((i + 1))
    message_id="${!j:-}"
  fi
done

if [[ "$action" == "send" ]]; then
  send_count=$((send_count + 1))
  echo "$send_count" > "$SEND_COUNT_FILE"
  new_id="msg-${send_count}"
  printf '{"action":"send","message_id":null,"returned_id":"%s"}\n' "$new_id" >> "$LOG_FILE"
  printf '{"payload":{"result":{"messageId":"%s"}}}\n' "$new_id"
  exit 0
fi

if [[ "$action" == "edit" ]]; then
  edit_count=$((edit_count + 1))
  echo "$edit_count" > "$EDIT_COUNT_FILE"
  if [[ ! -f "$FAILED_ONCE_FILE" ]]; then
    touch "$FAILED_ONCE_FILE"
    printf '{"action":"edit_fail","message_id":"%s"}\n' "$message_id" >> "$LOG_FILE"
    echo "mock edit failure (case11)" >&2
    exit 1
  fi
  printf '{"action":"edit","message_id":"%s"}\n' "$message_id" >> "$LOG_FILE"
  printf '{"payload":{"result":{"messageId":"%s"}}}\n' "$message_id"
  exit 0
fi

echo "unsupported mock openclaw action: $*" >&2
exit 1
EOF
chmod +x "$MOCKDIR/openclaw-task-fallback"

TASKFILE11="$WORKDIR/docs/roadmaps/s4-case11-tasklist.md"
mkdir -p "$(dirname "$TASKFILE11")"
cat > "$TASKFILE11" <<'EOF'
- [ ] S4X-1: single task completion
EOF

STATUS_MOCK_DIR11="$WORKDIR/status-mock-case11"
APPROVED11="$(approve_task_file "$TASKFILE11")"
OUT11="$(CLAW_LOOPD_OPENCLAW_BIN="$MOCKDIR/openclaw-task-fallback" CLAW_LOOPD_STATUS_MOCK_DIR="$STATUS_MOCK_DIR11" CLAW_LOOPD_GH_BIN="$MOCKDIR/gh" $BIN start --repo "$WORKDIR" --session-key test-session --channel discord --thread-id test-thread --tick-sec 1 --deliver-openclaw --task-file "$TASKFILE11" --task-runner-cmd 'echo "TASK_DONE PR_URL=https://example.invalid/pr/200"' --require-task-approval --approved-tasklist-hash "$APPROVED11")"
RUN11="$(echo "$OUT11" | awk -F= '/^run_id=/{print $2}')"
if [[ -z "$RUN11" ]]; then
  echo "[e2e-smoke] failed to parse run11 id"
  echo "$OUT11"
  exit 1
fi

sleep 2
CLAW_LOOPD_OPENCLAW_BIN="$MOCKDIR/openclaw-task-fallback" CLAW_LOOPD_STATUS_MOCK_DIR="$STATUS_MOCK_DIR11" $BIN notify --repo "$WORKDIR" --run-id "$RUN11" --kind progress --message "case11 forced edit for fallback" >/dev/null

STATUS11=""
for _ in {1..20}; do
  STATUS11="$($BIN status --repo "$WORKDIR" --run-id "$RUN11")"
  if python3 - <<'PY' "$STATUS11"
import json, sys
obj = json.loads(sys.argv[1])
runner = obj.get("runner") or {}
ok = (
    obj.get("status") == "stopped"
    and runner.get("pause_reason") == "all tasklist items completed"
)
raise SystemExit(0 if ok else 1)
PY
  then
    break
  fi
  sleep 1
done

python3 - <<'PY' "$STATUS11" "$STATUS_MOCK_DIR11" "$WORKDIR/.ralph/runs/$RUN11"
import json, pathlib, sys
status = json.loads(sys.argv[1])
mock_dir = pathlib.Path(sys.argv[2])
run_dir = pathlib.Path(sys.argv[3])

calls_path = mock_dir / "calls-case11.jsonl"
if not calls_path.exists():
    raise SystemExit("expected calls-case11.jsonl")
rows = [json.loads(line) for line in calls_path.read_text().splitlines() if line.strip()]
if not any(r.get("action") == "edit_fail" for r in rows):
    raise SystemExit(f"expected at least one edit_fail row, got {rows}")
sends = [r for r in rows if r.get("action") == "send"]
if len(sends) < 2:
    raise SystemExit(f"expected >=2 sends (bootstrap + fallback), got {len(sends)} rows={rows}")

runner = status.get("runner") or {}
status_id = runner.get("status_message_id")
if not status_id:
    raise SystemExit(f"expected non-empty runner.status_message_id, got {runner}")
bootstrap_id = sends[0].get("returned_id") if sends else None
if status_id == bootstrap_id:
    raise SystemExit(f"expected status_message_id to move away from bootstrap id after fallback, got {status_id}")
all_send_ids = {r.get("returned_id") for r in sends if r.get("returned_id")}
if status_id not in all_send_ids:
    raise SystemExit(f"expected status_message_id to match one of recreated send ids, got {status_id}, send_ids={all_send_ids}")

events_path = run_dir / "events.jsonl"
events = [json.loads(line) for line in events_path.read_text().splitlines() if line.strip()]
if not any(e.get("kind") == "notify_status_edit_fallback_send" for e in events):
    raise SystemExit("expected notify_status_edit_fallback_send event")

dispatched_path = run_dir / "notify-dispatched.jsonl"
dispatched = [json.loads(line) for line in dispatched_path.read_text().splitlines() if line.strip()]
kind_counts = {}
for row in dispatched:
    kind = row.get("kind")
    kind_counts[kind] = kind_counts.get(kind, 0) + 1

if kind_counts.get("all_tasks_completed", 0) != 1:
    raise SystemExit(f"expected exactly 1 all_tasks_completed dispatch, got {kind_counts}")
if int(status.get("pending_notifications", 0)) < 0:
    raise SystemExit(f"pending_notifications should be non-negative, got {status.get('pending_notifications')}")
PY

$BIN stop --repo "$WORKDIR" --run-id "$RUN11" >/dev/null || true
sleep 1

echo "[e2e-smoke] case12 timeout-delay behavior (configurable OpenClaw timeout)"
cat > "$MOCKDIR/openclaw-delay" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
STATE_DIR="${CLAW_LOOPD_STATUS_MOCK_DIR:?missing CLAW_LOOPD_STATUS_MOCK_DIR}"
mkdir -p "$STATE_DIR"
LOG_FILE="$STATE_DIR/calls-case12.jsonl"
COUNT_FILE="$STATE_DIR/send_count_case12"

count=0
if [[ -f "$COUNT_FILE" ]]; then
  count="$(cat "$COUNT_FILE")"
fi

action="${2:-}"
if [[ "$action" != "send" && "$action" != "edit" ]]; then
  echo "unsupported mock openclaw action: $*" >&2
  exit 1
fi

count=$((count + 1))
echo "$count" > "$COUNT_FILE"

DELAY_SEC="${CLAW_LOOPD_TIMEOUT_MOCK_DELAY_SEC:-1}"
printf '{"action":"%s","timeout_sec":"%s","delay_sec":"%s"}\n' "$action" "${CLAW_LOOPD_OPENCLAW_TIMEOUT_SEC:-}" "$DELAY_SEC" >> "$LOG_FILE"

sleep "$DELAY_SEC"
printf '{"payload":{"result":{"messageId":"delay-msg-%s"}}}\n' "$count"
EOF
chmod +x "$MOCKDIR/openclaw-delay"

STATUS_MOCK_DIR12A="$WORKDIR/status-mock-case12a"
TASKFILE12A="$WORKDIR/docs/roadmaps/timeout-delay-a-tasklist.md"
cat > "$TASKFILE12A" <<'EOF'
- [ ] T12A: timeout delay a
EOF
APPROVED12A="$(approve_task_file "$TASKFILE12A")"
OUT12A="$(CLAW_LOOPD_OPENCLAW_BIN="$MOCKDIR/openclaw-delay" CLAW_LOOPD_STATUS_MOCK_DIR="$STATUS_MOCK_DIR12A" CLAW_LOOPD_TIMEOUT_MOCK_DELAY_SEC=2 CLAW_LOOPD_OPENCLAW_TIMEOUT_SEC=1 $BIN start --repo "$WORKDIR" --session-key test-session --channel discord --thread-id test-thread --tick-sec 1 --deliver-openclaw --task-file "$TASKFILE12A" --require-task-approval --approved-tasklist-hash "$APPROVED12A")"
RUN12A="$(echo "$OUT12A" | awk -F= '/^run_id=/{print $2}')"
if [[ -z "$RUN12A" ]]; then
  echo "[e2e-smoke] failed to parse run12a id"
  echo "$OUT12A"
  exit 1
fi
sleep 4
python3 - <<'PY' "$STATUS_MOCK_DIR12A"
import json, pathlib, sys
mock_dir = pathlib.Path(sys.argv[1])
calls = mock_dir / "calls-case12.jsonl"
if not calls.exists():
    raise SystemExit("expected calls-case12.jsonl for run12a")
rows = [json.loads(line) for line in calls.read_text().splitlines() if line.strip()]
if not rows:
    raise SystemExit("expected at least one run12a openclaw call")
if not any(r.get("timeout_sec") == "1" for r in rows):
    raise SystemExit(f"expected timeout_sec=1 in run12a calls, got {rows}")
PY
$BIN stop --repo "$WORKDIR" --run-id "$RUN12A" --immediate >/dev/null || true
sleep 1

STATUS_MOCK_DIR12B="$WORKDIR/status-mock-case12b"
TASKFILE12B="$WORKDIR/docs/roadmaps/timeout-delay-b-tasklist.md"
cat > "$TASKFILE12B" <<'EOF'
- [ ] T12B: timeout delay b
EOF
APPROVED12B="$(approve_task_file "$TASKFILE12B")"
OUT12B="$(CLAW_LOOPD_OPENCLAW_BIN="$MOCKDIR/openclaw-delay" CLAW_LOOPD_STATUS_MOCK_DIR="$STATUS_MOCK_DIR12B" CLAW_LOOPD_TIMEOUT_MOCK_DELAY_SEC=1 CLAW_LOOPD_OPENCLAW_TIMEOUT_SEC=3 $BIN start --repo "$WORKDIR" --session-key test-session --channel discord --thread-id test-thread --tick-sec 1 --deliver-openclaw --task-file "$TASKFILE12B" --require-task-approval --approved-tasklist-hash "$APPROVED12B")"
RUN12B="$(echo "$OUT12B" | awk -F= '/^run_id=/{print $2}')"
if [[ -z "$RUN12B" ]]; then
  echo "[e2e-smoke] failed to parse run12b id"
  echo "$OUT12B"
  exit 1
fi
sleep 4
STATUS12B="$($BIN status --repo "$WORKDIR" --run-id "$RUN12B")"
python3 - <<'PY' "$STATUS12B" "$STATUS_MOCK_DIR12B"
import json, pathlib, sys
obj = json.loads(sys.argv[1])
mock_dir = pathlib.Path(sys.argv[2])
metrics = obj.get("delivery_metrics") or {}
if int(metrics.get("delivered_total", 0)) < 1:
    raise SystemExit(f"expected delivered_total>=1 under longer timeout, got {metrics}")
if int(obj.get("dispatched_notifications", 0)) < 1:
    raise SystemExit(f"expected dispatched_notifications>=1 under longer timeout, got {obj.get('dispatched_notifications')}")
rows = [json.loads(line) for line in (mock_dir / "calls-case12.jsonl").read_text().splitlines() if line.strip()]
if not any(r.get("timeout_sec") == "3" for r in rows):
    raise SystemExit(f"expected timeout_sec=3 in run12b calls, got {rows}")
PY
$BIN stop --repo "$WORKDIR" --run-id "$RUN12B" --immediate >/dev/null || true
sleep 1

echo "[e2e-smoke] case13 auto-recover blocked task into generated next task"
TASKFILE13="$WORKDIR/docs/roadmaps/s5-case13-tasklist.md"
mkdir -p "$(dirname "$TASKFILE13")"
cat > "$TASKFILE13" <<'EOF'
- [ ] S5X-1: blocked sample task
EOF

APPROVED13="$(approve_task_file "$TASKFILE13")"
OUT13="$(CLAW_LOOPD_OPENCLAW_BIN="$MOCKDIR/openclaw-ok" CLAW_LOOPD_GH_BIN="$MOCKDIR/gh" $BIN start --repo "$WORKDIR" --session-key test-session --channel discord --thread-id test-thread --tick-sec 1 --deliver-openclaw --task-file "$TASKFILE13" --task-runner-cmd 'if [[ "$CLAW_TASK_ID" == *"-RECOVER"* ]]; then echo "TASK_DONE PR_URL=https://example.invalid/pr/313"; exit 0; fi; printf "simulated blocked\nmissing fixture in generated workspace\n" >&2; exit 2' --auto-recover-blocked --require-task-approval --approved-tasklist-hash "$APPROVED13")"
RUN13="$(echo "$OUT13" | awk -F= '/^run_id=/{print $2}')"
if [[ -z "$RUN13" ]]; then
  echo "[e2e-smoke] failed to parse run13 id"
  echo "$OUT13"
  exit 1
fi

STATUS13=""
for _ in {1..25}; do
  STATUS13="$($BIN status --repo "$WORKDIR" --run-id "$RUN13")"
  if python3 - <<'PY' "$STATUS13"
import json, sys
obj = json.loads(sys.argv[1])
runner = obj.get("runner") or {}
ok = (
    obj.get("status") == "stopped"
    and runner.get("pause_reason") == "all tasklist items completed"
)
raise SystemExit(0 if ok else 1)
PY
  then
    break
  fi
  sleep 1
done

python3 - <<'PY' "$STATUS13" "$TASKFILE13" "$WORKDIR/.ralph/runs/$RUN13/events.jsonl" "$WORKDIR/.ralph/runs/$RUN13/notify-dispatched.jsonl"
import json, pathlib, sys
status = json.loads(sys.argv[1])
taskfile = pathlib.Path(sys.argv[2])
events_path = pathlib.Path(sys.argv[3])
dispatched_path = pathlib.Path(sys.argv[4])

content = taskfile.read_text()
if "- [x] S5X-1: blocked sample task" not in content:
    raise SystemExit(f"expected original blocked task to be marked done, got:\n{content}")
if "S5X-1-RECOVER" not in content:
    raise SystemExit(f"expected generated recovery task id in tasklist, got:\n{content}")
if "- [x] S5X-1-RECOVER" not in content:
    raise SystemExit(f"expected generated recovery task to be completed, got:\n{content}")
if "resolve runner block for task S5X-1 (blocked sample task):" not in content:
    raise SystemExit(f"expected generated recovery task text to describe the fix, got:\n{content}")

events = [json.loads(line) for line in events_path.read_text().splitlines() if line.strip()]
if not any(e.get("kind") == "task_blocked_auto_recovered" for e in events):
    raise SystemExit("expected task_blocked_auto_recovered event")

dispatched = [json.loads(line) for line in dispatched_path.read_text().splitlines() if line.strip()]
blocked_notes = [d for d in dispatched if d.get("kind") == "task_blocked"]
if not blocked_notes:
    raise SystemExit(f"expected task_blocked notification, got {dispatched}")
blocked_msg = blocked_notes[-1].get("message") or ""
for needle in ["- 原因: runner exit=Some(2): simulated blocked", "- 詳細: runner exit=Some(2): simulated blocked missing fixture in generated workspace", "- 解決方法:"]:
    if needle not in blocked_msg:
        raise SystemExit(f"expected {needle!r} in blocked notification, got: {blocked_msg!r}")

recovery_notes = [d for d in dispatched if d.get("kind") == "task_recovery_decision"]
if not recovery_notes:
    raise SystemExit(f"expected task_recovery_decision notification, got {dispatched}")
msg = recovery_notes[-1].get("message") or ""
for needle in ["- 原因:", "- 解決方針:", "- 実際に積んだ recovery task:", "- 状態: auto-recover 継続"]:
    if needle not in msg:
        raise SystemExit(f"expected {needle!r} in recovery decision notification, got: {msg!r}")

kind_order = [d.get("kind") for d in dispatched]
try:
    blocked_idx = kind_order.index("task_blocked")
    decision_idx = kind_order.index("task_recovery_decision")
except ValueError as exc:
    raise SystemExit(f"expected blocked and decision notifications, got {kind_order}") from exc
if blocked_idx >= decision_idx:
    raise SystemExit(f"expected task_blocked before task_recovery_decision, got {kind_order}")

runner = status.get("runner") or {}
last_id = runner.get("last_task_id")
if not isinstance(last_id, str) or "RECOVER" not in last_id:
    raise SystemExit(f"expected last_task_id to be recovery task, got {last_id}")
PY

$BIN stop --repo "$WORKDIR" --run-id "$RUN13" --immediate >/dev/null || true
sleep 1

echo "[e2e-smoke] case13b auto-recover halt notification for failed recovery task"
TASKFILE13B="$WORKDIR/docs/roadmaps/s5-case13b-tasklist.md"
mkdir -p "$(dirname "$TASKFILE13B")"
cat > "$TASKFILE13B" <<'EOF'
- [ ] S5X-13B: blocked sample task for halt
EOF

APPROVED13B="$(approve_task_file "$TASKFILE13B")"
OUT13B="$(CLAW_LOOPD_OPENCLAW_BIN="$MOCKDIR/openclaw-ok" CLAW_LOOPD_GH_BIN="$MOCKDIR/gh" $BIN start --repo "$WORKDIR" --session-key test-session --channel discord --thread-id test-thread --tick-sec 1 --deliver-openclaw --task-file "$TASKFILE13B" --task-runner-cmd 'if [[ "$CLAW_TASK_ID" == *"-RECOVER"* ]]; then echo "recovery still blocked" >&2; exit 2; fi; echo "initial blocked" >&2; exit 2' --auto-recover-blocked --require-task-approval --approved-tasklist-hash "$APPROVED13B")"
RUN13B="$(echo "$OUT13B" | awk -F= '/^run_id=/{print $2}')"
if [[ -z "$RUN13B" ]]; then
  echo "[e2e-smoke] failed to parse run13b id"
  echo "$OUT13B"
  exit 1
fi

STATUS13B=""
for _ in {1..25}; do
  STATUS13B="$($BIN status --repo "$WORKDIR" --run-id "$RUN13B")"
  if python3 - <<'PY' "$STATUS13B"
import json, sys
obj = json.loads(sys.argv[1])
runner = obj.get("runner") or {}
ok = (
    obj.get("status") == "stopped"
    and isinstance(runner.get("pause_reason"), str)
    and "generated recovery task failed" in runner.get("pause_reason")
)
raise SystemExit(0 if ok else 1)
PY
  then
    break
  fi
  sleep 1
done

python3 - <<'PY' "$STATUS13B" "$WORKDIR/.ralph/runs/$RUN13B/notify-dispatched.jsonl"
import json, pathlib, sys
status = json.loads(sys.argv[1])
dispatched_path = pathlib.Path(sys.argv[2])

runner = status.get("runner") or {}
pause_reason = runner.get("pause_reason") or ""
if "generated recovery task failed" not in pause_reason:
    raise SystemExit(f"expected generated recovery task failure pause_reason, got {pause_reason!r}")

dispatched = [json.loads(line) for line in dispatched_path.read_text().splitlines() if line.strip()]
blocked_notes = [d for d in dispatched if d.get("kind") == "task_blocked"]
if not blocked_notes:
    raise SystemExit(f"expected task_blocked notification for halt path, got {dispatched}")

halt_notes = [d for d in dispatched if d.get("kind") == "task_recovery_halted"]
if not halt_notes:
    raise SystemExit(f"expected task_recovery_halted notification, got {dispatched}")
msg = halt_notes[-1].get("message") or ""
for needle in ["- 停止理由:", "- 原因:", "- 次に見るポイント:", "- 失敗した recovery task:", "- 元タスク: S5X-13B", "- stderr: recovery still blocked", "- 手動での解決方針:"]:
    if needle not in msg:
        raise SystemExit(f"expected {needle!r} in recovery halt notification, got: {msg!r}")

kind_order = [d.get("kind") for d in dispatched]
try:
    blocked_idx = kind_order.index("task_blocked")
    halt_idx = kind_order.index("task_recovery_halted")
except ValueError as exc:
    raise SystemExit(f"expected blocked and halted notifications, got {kind_order}") from exc
if blocked_idx >= halt_idx:
    raise SystemExit(f"expected task_blocked before task_recovery_halted, got {kind_order}")
PY

$BIN stop --repo "$WORKDIR" --run-id "$RUN13B" --immediate >/dev/null || true
sleep 1

echo "[e2e-smoke] case13c dependency wait stays out of auto-recover and resumes after merge"
DEP_MOCKDIR="$WORKDIR/mockbin-dependency"
mkdir -p "$DEP_MOCKDIR"
DEP_READY_FILE="$WORKDIR/dependency-merged.flag"
cat > "$DEP_MOCKDIR/gh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
READY_FILE="${CLAW_LOOPD_DEP_READY_FILE:?missing CLAW_LOOPD_DEP_READY_FILE}"
if [[ "${1:-}" == "pr" && "${2:-}" == "view" ]]; then
  if [[ -f "$READY_FILE" ]]; then
    cat <<'JSON'
{"state":"MERGED","mergedAt":"2026-03-16T00:00:00Z","url":"https://github.com/demo/repo/pull/777"}
JSON
  else
    cat <<'JSON'
{"state":"OPEN","mergedAt":null,"url":"https://github.com/demo/repo/pull/777"}
JSON
  fi
  exit 0
fi
echo "unsupported mock gh args: $*" >&2
exit 1
EOF
chmod +x "$DEP_MOCKDIR/gh"

TASKFILE13C="$WORKDIR/docs/roadmaps/s5-case13c-tasklist.md"
mkdir -p "$(dirname "$TASKFILE13C")"
cat > "$TASKFILE13C" <<'EOF'
- [ ] S5X-13C: dependency wait sample task
EOF

APPROVED13C="$(approve_task_file "$TASKFILE13C")"
OUT13C="$(CLAW_LOOPD_OPENCLAW_BIN="$MOCKDIR/openclaw-ok" CLAW_LOOPD_GH_BIN="$DEP_MOCKDIR/gh" CLAW_LOOPD_DEP_READY_FILE="$DEP_READY_FILE" $BIN start --repo "$WORKDIR" --session-key test-session --channel discord --thread-id test-thread --tick-sec 1 --deliver-openclaw --task-file "$TASKFILE13C" --task-runner-cmd 'if [[ -f "$CLAW_LOOPD_DEP_READY_FILE" ]]; then echo "TASK_DONE PR_URL=https://github.com/demo/repo/pull/777"; exit 0; fi; echo "TASK_WAITING_DEPENDENCY TASK_ID=$CLAW_TASK_ID DEPENDS_ON_PR_URL=https://github.com/demo/repo/pull/777"; exit 10' --auto-recover-blocked --require-task-approval --approved-tasklist-hash "$APPROVED13C")"
RUN13C="$(echo "$OUT13C" | awk -F= '/^run_id=/{print $2}')"
if [[ -z "$RUN13C" ]]; then
  echo "[e2e-smoke] failed to parse run13c id"
  echo "$OUT13C"
  exit 1
fi

STATUS13C=""
for _ in {1..20}; do
  STATUS13C="$($BIN status --repo "$WORKDIR" --run-id "$RUN13C")"
  if python3 - <<'PY' "$STATUS13C"
import json, sys
obj = json.loads(sys.argv[1])
runner = obj.get("runner") or {}
ok = (
    obj.get("status") == "waiting"
    and runner.get("current_task_state") == "waiting_dependency"
    and "TASK_WAITING_DEPENDENCY" in (obj.get("waiting_reason") or "")
)
raise SystemExit(0 if ok else 1)
PY
  then
    break
  fi
  sleep 1
done

python3 - <<'PY' "$STATUS13C" "$TASKFILE13C" "$WORKDIR/.ralph/runs/$RUN13C/events.jsonl" "$WORKDIR/.ralph/runs/$RUN13C/notify-dispatched.jsonl"
import json, pathlib, sys
status = json.loads(sys.argv[1])
taskfile = pathlib.Path(sys.argv[2])
events_path = pathlib.Path(sys.argv[3])
dispatched_path = pathlib.Path(sys.argv[4])
runner = status.get("runner") or {}
if status.get("status") != "waiting":
    raise SystemExit(f"expected waiting, got {status.get('status')!r}")
if runner.get("current_task_state") != "waiting_dependency":
    raise SystemExit(f"expected waiting_dependency state, got {runner}")
if "TASK_WAITING_DEPENDENCY" not in (status.get("waiting_reason") or ""):
    raise SystemExit(f"expected dependency waiting reason, got {status.get('waiting_reason')!r}")
content = taskfile.read_text()
if "RECOVER" in content:
    raise SystemExit(f"dependency wait should not create recovery tasks, got:\n{content}")
events = [json.loads(line) for line in events_path.read_text().splitlines() if line.strip()]
if not any(e.get("kind") == "task_waiting_dependency" for e in events):
    raise SystemExit(f"expected task_waiting_dependency event, got {[e.get('kind') for e in events][-10:]}")
if any(e.get("kind") == "task_blocked_auto_recovered" for e in events):
    raise SystemExit("dependency wait should not emit task_blocked_auto_recovered")
dispatched = [json.loads(line) for line in dispatched_path.read_text().splitlines() if line.strip()]
if any(d.get("kind") == "task_recovery_decision" for d in dispatched):
    raise SystemExit(f"dependency wait should not emit task_recovery_decision, got {dispatched}")
waiting_notes = [d for d in dispatched if d.get("kind") == "task_waiting_dependency"]
if not waiting_notes:
    raise SystemExit(f"expected task_waiting_dependency notification, got {dispatched}")
msg = waiting_notes[-1].get("message") or ""
for needle in ["- 分類: dependency wait", "- 次に進む条件:", "- Auto-recover: idle"]:
    if needle not in msg:
        raise SystemExit(f"expected {needle!r} in waiting notification, got: {msg!r}")
PY

touch "$DEP_READY_FILE"
STATUS13C_DONE=""
for _ in {1..25}; do
  STATUS13C_DONE="$($BIN status --repo "$WORKDIR" --run-id "$RUN13C")"
  if python3 - <<'PY' "$STATUS13C_DONE"
import json, sys
obj = json.loads(sys.argv[1])
runner = obj.get("runner") or {}
ok = (
    obj.get("status") == "stopped"
    and runner.get("pause_reason") == "all tasklist items completed"
)
raise SystemExit(0 if ok else 1)
PY
  then
    break
  fi
  sleep 1
done

python3 - <<'PY' "$STATUS13C_DONE" "$TASKFILE13C" "$WORKDIR/.ralph/runs/$RUN13C/events.jsonl"
import json, pathlib, sys
status = json.loads(sys.argv[1])
taskfile = pathlib.Path(sys.argv[2])
events_path = pathlib.Path(sys.argv[3])
runner = status.get("runner") or {}
if status.get("status") != "stopped":
    raise SystemExit(f"expected stopped after dependency merge, got {status.get('status')!r}")
if runner.get("pause_reason") != "all tasklist items completed":
    raise SystemExit(f"expected all tasklist items completed, got {runner}")
content = taskfile.read_text()
if "- [x] S5X-13C: dependency wait sample task" not in content:
    raise SystemExit(f"expected dependency wait task to complete after merge, got:\n{content}")
events = [json.loads(line) for line in events_path.read_text().splitlines() if line.strip()]
if not any(e.get("kind") == "task_waiting_dependency_resolved" for e in events):
    raise SystemExit(f"expected task_waiting_dependency_resolved event, got {[e.get('kind') for e in events][-10:]}")
if any(e.get("kind") == "task_blocked_auto_recovered" for e in events):
    raise SystemExit("dependency wait resolve path should not emit task_blocked_auto_recovered")
PY

$BIN stop --repo "$WORKDIR" --run-id "$RUN13C" --immediate >/dev/null || true
sleep 1


echo "[e2e-smoke] case13d failure-first backlog gate waits when only feature tasks are open"
TASKFILE13D="$WORKDIR/docs/roadmaps/s5-case13d-tasklist.md"
mkdir -p "$(dirname "$TASKFILE13D")"
cat > "$TASKFILE13D" <<'EOF'
- [ ] S5X-13D: add shiny feature
EOF

BACKLOG13D="$WORKDIR/.ralph/backlog-case13d.json"
python3 - <<'PY' "$BACKLOG13D" "$WORKDIR"
import json, pathlib, sys
path = pathlib.Path(sys.argv[1])
repo = pathlib.Path(sys.argv[2])
path.write_text(json.dumps({
    "repo_path": str(repo),
    "status": "backlog",
    "backlog_count": 2,
    "summary": "2 repairs pending",
    "updated_at": "2026-03-16T00:00:00Z"
}))
PY

APPROVED13D="$(approve_task_file "$TASKFILE13D")"
SHOULD_NOT_RUN13D="$WORKDIR/.ralph/should-not-run-case13d"
rm -f "$SHOULD_NOT_RUN13D"
OUT13D="$($BIN start --repo "$WORKDIR" --session-key test-session --channel discord --thread-id test-thread --tick-sec 1 --task-file "$TASKFILE13D" --task-runner-cmd 'echo ran > "$WORKDIR/.ralph/should-not-run-case13d"; exit 0' --auto-check-on-success false --backlog-detector-file "$BACKLOG13D" --backlog-detector-max-age-sec 31536000 --require-task-approval --approved-tasklist-hash "$APPROVED13D")"
RUN13D="$(echo "$OUT13D" | awk -F= '/^run_id=/{print $2}')"
if [[ -z "$RUN13D" ]]; then
  echo "[e2e-smoke] failed to parse run13d id"
  echo "$OUT13D"
  exit 1
fi

STATUS13D=""
for _ in {1..20}; do
  STATUS13D="$($BIN status --repo "$WORKDIR" --run-id "$RUN13D")"
  if python3 - <<'PY' "$STATUS13D"
import json, sys
obj = json.loads(sys.argv[1])
ok = obj.get("status") == "waiting" and "backlog gate active" in (obj.get("waiting_reason") or "")
raise SystemExit(0 if ok else 1)
PY
  then
    break
  fi
  sleep 1
done

python3 - <<'PY' "$STATUS13D" "$WORKDIR/.ralph/runs/$RUN13D/events.jsonl" "$SHOULD_NOT_RUN13D"
import json, pathlib, sys
status = json.loads(sys.argv[1])
events_path = pathlib.Path(sys.argv[2])
should_not_run = pathlib.Path(sys.argv[3])
if status.get("status") != "waiting":
    raise SystemExit(f"expected waiting, got {status.get('status')!r}")
reason = status.get("waiting_reason") or ""
if "backlog gate active" not in reason:
    raise SystemExit(f"expected backlog gate waiting reason, got {reason!r}")
if should_not_run.exists():
    raise SystemExit("feature task should not have executed while backlog gate is active")
events = [json.loads(line) for line in events_path.read_text().splitlines() if line.strip()]
if not any(e.get("kind") == "task_selection_backlog_waiting" for e in events):
    raise SystemExit(f"expected task_selection_backlog_waiting event, got {[e.get('kind') for e in events][-10:]}")
if any(e.get("kind") == "task_runner_tick" for e in events):
    raise SystemExit("feature task should not emit task_runner_tick while backlog gate is active")
PY

$BIN stop --repo "$WORKDIR" --run-id "$RUN13D" --immediate >/dev/null || true
sleep 1


echo "[e2e-smoke] case13e guardrail blocks inconsistent waiting_dependency state instead of stopping"
TASKFILE13E="$WORKDIR/docs/roadmaps/s5-case13e-tasklist.md"
mkdir -p "$(dirname "$TASKFILE13E")"
cat > "$TASKFILE13E" <<'EOF'
- [ ] S5X-13E: repair external dependency sequencing
EOF

APPROVED13E="$(approve_task_file "$TASKFILE13E")"
OUT13E="$($BIN start --repo "$WORKDIR" --session-key test-session --channel discord --thread-id test-thread --tick-sec 1 --task-file "$TASKFILE13E" --task-runner-cmd 'echo should-not-run > "$WORKDIR/.ralph/should-not-run-case13e"; exit 0' --auto-check-on-success false --require-task-approval --approved-tasklist-hash "$APPROVED13E")"
RUN13E="$(echo "$OUT13E" | awk -F= '/^run_id=/{print $2}')"
if [[ -z "$RUN13E" ]]; then
  echo "[e2e-smoke] failed to parse run13e id"
  echo "$OUT13E"
  exit 1
fi

$BIN stop --repo "$WORKDIR" --run-id "$RUN13E" --immediate >/dev/null || true
sleep 1
rm -f "$WORKDIR/.ralph/runs/$RUN13E/control.stop"

SHOULD_NOT_RUN13E="$WORKDIR/.ralph/should-not-run-case13e"
rm -f "$SHOULD_NOT_RUN13E"
python3 - <<'PY' "$WORKDIR" "$RUN13E"
import json, pathlib, sys
workdir = pathlib.Path(sys.argv[1])
run_id = sys.argv[2]
run_dir = workdir / '.ralph' / 'runs' / run_id
runner_state_path = run_dir / 'runner-state.json'
state_path = run_dir / 'state.json'
runner = json.loads(runner_state_path.read_text())
runner.update({
    'current_task_id': None,
    'current_task_text': 'repair external dependency sequencing',
    'current_task_line': 1,
    'current_task_started_at': None,
    'current_task_state': 'waiting_dependency',
    'current_task_pr_url': None,
    'current_task_blocked_reason': None,
    'current_blocked_context': None,
    'paused': False,
    'pause_reason': None,
    'task_loops_started': 0,
    'current_waiting_dependency': {
        'task_id': 'S5X-13E',
        'depends_on_task': None,
        'depends_on_pr_url': 'https://github.com/demo/repo/pull/888',
        'contract_line': 'TASK_WAITING_DEPENDENCY TASK_ID=S5X-13E DEPENDS_ON_PR_URL=https://github.com/demo/repo/pull/888'
    }
})
runner_state_path.write_text(json.dumps(runner))
state = json.loads(state_path.read_text())
state.update({'status': 'running', 'summary': 'daemon started', 'waiting_reason': ''})
state_path.write_text(json.dumps(state))
PY

"$BIN" daemon --repo "$WORKDIR" --run-id "$RUN13E" --tick-sec 1 >/tmp/case13e-daemon.log 2>&1 &
DAEMON13E=$!
STATUS13E=""
for _ in {1..20}; do
  STATUS13E="$($BIN status --repo "$WORKDIR" --run-id "$RUN13E")"
  if python3 - <<'PY' "$STATUS13E"
import json, sys
obj = json.loads(sys.argv[1])
ok = obj.get('status') == 'blocked' and 'waiting_dependency' in (obj.get('waiting_reason') or '') and 'lost current_task_id' in (obj.get('waiting_reason') or '')
raise SystemExit(0 if ok else 1)
PY
  then
    break
  fi
  sleep 1
done
kill "$DAEMON13E" >/dev/null 2>&1 || true
wait "$DAEMON13E" >/dev/null 2>&1 || true

python3 - <<'PY' "$STATUS13E" "$WORKDIR/.ralph/runs/$RUN13E/events.jsonl" "$SHOULD_NOT_RUN13E"
import json, pathlib, sys
status = json.loads(sys.argv[1])
events_path = pathlib.Path(sys.argv[2])
should_not_run = pathlib.Path(sys.argv[3])
if status.get('status') != 'blocked':
    raise SystemExit(f"expected blocked, got {status.get('status')!r}")
reason = status.get('waiting_reason') or ''
if 'waiting_dependency' not in reason or 'lost current_task_id' not in reason:
    raise SystemExit(f"expected waiting_dependency guardrail reason, got {reason!r}")
if should_not_run.exists():
    raise SystemExit('guardrail should block before task runner executes')
events = [json.loads(line) for line in events_path.read_text().splitlines() if line.strip()]
if not any(e.get('kind') == 'runner_state_guardrail_blocked' for e in events):
    raise SystemExit(f"expected runner_state_guardrail_blocked event, got {[e.get('kind') for e in events][-10:]}")
if any(e.get('kind') == 'daemon_stopped_all_tasks_completed' for e in events):
    raise SystemExit('guardrail regression: daemon should not stop as all tasks completed')
PY


echo "[e2e-smoke] case14 rl-task-agent ignores leading empty payload before TASK_DONE"
RUNNER_MOCKDIR="$WORKDIR/mockbin-runner"
mkdir -p "$RUNNER_MOCKDIR"
cat > "$RUNNER_MOCKDIR/openclaw" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
if [[ "${1:-}" == "agent" ]]; then
  cat <<'JSON'
{"payloads":[{"text":""},{"text":"TASK_DONE PR_URL=https://github.com/demo/repo/pull/314\nrecovery summary"}]}
JSON
  exit 0
fi

echo "unsupported mock openclaw args: $*" >&2
exit 1
EOF
chmod +x "$RUNNER_MOCKDIR/openclaw"

cat > "$RUNNER_MOCKDIR/gh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
if [[ "${1:-}" == "pr" && "${2:-}" == "view" ]]; then
  echo 'MERGED|2026-03-16T00:00:00Z'
  exit 0
fi

echo "unsupported mock gh args: $*" >&2
exit 1
EOF
chmod +x "$RUNNER_MOCKDIR/gh"

TASKFILE14="$WORKDIR/docs/roadmaps/s5-case14-tasklist.md"
mkdir -p "$(dirname "$TASKFILE14")"
cat > "$TASKFILE14" <<'EOF'
- [ ] S5X-14: leading empty payload
EOF

OUT14="$(PATH="$RUNNER_MOCKDIR:$PATH" CLAW_TASK_ID="S5X-14" CLAW_TASK_TEXT="leading empty payload" CLAW_TASK_FILE="$TASKFILE14" CLAW_RUN_ID="run14" bash ./scripts/rl-task-agent.sh)"
FIRST14="$(printf '%s\n' "$OUT14" | awk 'NF { print; exit }')"
if [[ "$FIRST14" != "TASK_DONE PR_URL=https://github.com/demo/repo/pull/314" ]]; then
  echo "[e2e-smoke] unexpected case14 output"
  printf '%s\n' "$OUT14"
  exit 1
fi

echo "[e2e-smoke] case15 rl-task-agent prefers TASK contract line over earlier chatter"
cat > "$RUNNER_MOCKDIR/openclaw" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
if [[ "${1:-}" == "agent" ]]; then
  cat <<'JSON'
{"payloads":[{"text":"working on it"},{"text":"TASK_DONE PR_URL=https://github.com/demo/repo/pull/315\nextra summary"}]}
JSON
  exit 0
fi

echo "unsupported mock openclaw args: $*" >&2
exit 1
EOF
chmod +x "$RUNNER_MOCKDIR/openclaw"

TASKFILE15="$WORKDIR/docs/roadmaps/s5-case15-tasklist.md"
mkdir -p "$(dirname "$TASKFILE15")"
cat > "$TASKFILE15" <<'EOF'
- [ ] S5X-15: chatter before contract line
EOF

OUT15="$(PATH="$RUNNER_MOCKDIR:$PATH" CLAW_TASK_ID="S5X-15" CLAW_TASK_TEXT="chatter before contract line" CLAW_TASK_FILE="$TASKFILE15" CLAW_RUN_ID="run15" bash ./scripts/rl-task-agent.sh)"
if [[ "$OUT15" != *"TASK_DONE PR_URL=https://github.com/demo/repo/pull/315"* ]]; then
  echo "[e2e-smoke] expected case15 TASK_DONE line in transcript"
  printf '%s\n' "$OUT15"
  exit 1
fi

echo "[e2e-smoke] case15b rl-task-agent falls back to top-level text when payloads is null"
cat > "$RUNNER_MOCKDIR/openclaw" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
if [[ "${1:-}" == "agent" ]]; then
  cat <<'JSON'
{"payloads":null,"text":"TASK_DONE PR_URL=https://github.com/demo/repo/pull/315b\nsummary from top-level text"}
JSON
  exit 0
fi

echo "unsupported mock openclaw args: $*" >&2
exit 1
EOF
chmod +x "$RUNNER_MOCKDIR/openclaw"

TASKFILE15B="$WORKDIR/docs/roadmaps/s5-case15b-tasklist.md"
mkdir -p "$(dirname "$TASKFILE15B")"
cat > "$TASKFILE15B" <<'EOF'
- [ ] S5X-15B: top-level text fallback
EOF

OUT15B="$(PATH="$RUNNER_MOCKDIR:$PATH" CLAW_TASK_ID="S5X-15B" CLAW_TASK_TEXT="top-level text fallback" CLAW_TASK_FILE="$TASKFILE15B" CLAW_RUN_ID="run15b" bash ./scripts/rl-task-agent.sh)"
FIRST15B="$(printf '%s\n' "$OUT15B" | awk 'NF { print; exit }')"
if [[ "$FIRST15B" != "TASK_DONE PR_URL=https://github.com/demo/repo/pull/315b" ]]; then
  echo "[e2e-smoke] expected case15b TASK_DONE first line"
  printf '%s\n' "$OUT15B"
  exit 1
fi

echo "[e2e-smoke] case15bb rl-task-agent blocks feature tasks while backlog gate is active"
BACKLOG_GUARD_MARKER="$WORKDIR/.ralph/case15bb-openclaw-called"
rm -f "$BACKLOG_GUARD_MARKER"
cat > "$RUNNER_MOCKDIR/openclaw" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
if [[ "${1:-}" == "agent" ]]; then
  : > "${BACKLOG_GUARD_MARKER:?}"
  cat <<'JSON'
{"payloads":null,"text":"TASK_DONE PR_URL=https://github.com/demo/repo/pull/315bb\nthis should never be reached"}
JSON
  exit 0
fi

echo "unsupported mock openclaw args: $*" >&2
exit 1
EOF
chmod +x "$RUNNER_MOCKDIR/openclaw"

TASKFILE15BB="$WORKDIR/docs/roadmaps/s5-case15bb-tasklist.md"
mkdir -p "$(dirname "$TASKFILE15BB")"
cat > "$TASKFILE15BB" <<'EOF'
- [ ] S5X-15BB: feature task blocked by backlog gate
EOF

set +e
OUT15BB="$(PATH="$RUNNER_MOCKDIR:$PATH" BACKLOG_GUARD_MARKER="$BACKLOG_GUARD_MARKER" CLAW_TASK_ID="S5X-15BB" CLAW_TASK_TEXT="feature task blocked by backlog gate" CLAW_TASK_FILE="$TASKFILE15BB" CLAW_RUN_ID="run15bb" CLAW_TASK_KIND="feature" CLAW_BACKLOG_STATUS="backlog" CLAW_BACKLOG_COUNT="2" CLAW_BACKLOG_SUMMARY="2 repairs pending" bash ./scripts/rl-task-agent.sh 2>&1)"
RC15BB=$?
set -e
if [[ "$RC15BB" -eq 0 ]]; then
  echo "[e2e-smoke] expected case15bb to block feature task under backlog gate"
  printf '%s\n' "$OUT15BB"
  exit 1
fi
if [[ "$OUT15BB" != *"TASK_BLOCKED: failure-first backlog gate active"* ]]; then
  echo "[e2e-smoke] expected case15bb backlog gate block message"
  printf '%s\n' "$OUT15BB"
  exit 1
fi
if [[ -f "$BACKLOG_GUARD_MARKER" ]]; then
  echo "[e2e-smoke] expected case15bb to fail before openclaw agent was called"
  exit 1
fi

echo "[e2e-smoke] case15c rl-task-agent prefers the last text-bearing JSON object"
cat > "$RUNNER_MOCKDIR/openclaw" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
if [[ "${1:-}" == "agent" ]]; then
  cat <<'JSON'
{"message":{"role":"assistant","content":[{"type":"toolCall","name":"read"}]}}
{"message":{"role":"assistant","content":[{"type":"text","text":"TASK_DONE PR_URL=https://github.com/demo/repo/pull/315c\nsummary from final response"}]}}
JSON
  exit 0
fi

echo "unsupported mock openclaw args: $*" >&2
exit 1
EOF
chmod +x "$RUNNER_MOCKDIR/openclaw"

TASKFILE15C="$WORKDIR/docs/roadmaps/s5-case15c-tasklist.md"
mkdir -p "$(dirname "$TASKFILE15C")"
cat > "$TASKFILE15C" <<'EOF'
- [ ] S5X-15C: final response in later JSON object
EOF

OUT15C="$(PATH="$RUNNER_MOCKDIR:$PATH" CLAW_TASK_ID="S5X-15C" CLAW_TASK_TEXT="final response in later JSON object" CLAW_TASK_FILE="$TASKFILE15C" CLAW_RUN_ID="run15c" bash ./scripts/rl-task-agent.sh)"
FIRST15C="$(printf '%s\n' "$OUT15C" | awk 'NF { print; exit }')"
if [[ "$FIRST15C" != "TASK_DONE PR_URL=https://github.com/demo/repo/pull/315c" ]]; then
  echo "[e2e-smoke] expected case15c TASK_DONE first line from last JSON object"
  printf '%s\n' "$OUT15C"
  exit 1
fi

echo "[e2e-smoke] case15cc rl-task-agent blocks empty assistant text with detail"
cat > "$RUNNER_MOCKDIR/openclaw" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
if [[ "${1:-}" == "agent" ]]; then
  cat <<'JSON'
{"payloads":null,"text":""}
JSON
  exit 0
fi

echo "unsupported mock openclaw args: $*" >&2
exit 1
EOF
chmod +x "$RUNNER_MOCKDIR/openclaw"

TASKFILE15CC="$WORKDIR/docs/roadmaps/s5-case15cc-tasklist.md"
mkdir -p "$(dirname "$TASKFILE15CC")"
cat > "$TASKFILE15CC" <<'EOF'
- [ ] S5X-15CC: empty assistant text
EOF

set +e
OUT15CC="$(PATH="$RUNNER_MOCKDIR:$PATH" CLAW_TASK_ID="S5X-15CC" CLAW_TASK_TEXT="empty assistant text" CLAW_TASK_FILE="$TASKFILE15CC" CLAW_RUN_ID="run15cc" bash ./scripts/rl-task-agent.sh 2>&1)"
RC15CC=$?
set -e
if [[ "$RC15CC" -eq 0 ]]; then
  echo "[e2e-smoke] expected case15cc to block empty assistant text"
  printf '%s\n' "$OUT15CC"
  exit 1
fi
if [[ "$OUT15CC" != *"TASK_BLOCKED: openclaw agent returned no assistant text"* ]]; then
  echo "[e2e-smoke] expected case15cc no-text block detail"
  printf '%s\n' "$OUT15CC"
  exit 1
fi

echo "[e2e-smoke] case15d rl-task-agent blocks task file mutations during runner execution"
cat > "$RUNNER_MOCKDIR/openclaw" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
if [[ "${1:-}" == "agent" ]]; then
  printf '\n- [ ] S5X-15D-EXTRA: illegal tasklist mutation\n' >> "$CLAW_TASK_FILE"
  cat <<'JSON'
{"payloads":null,"text":"TASK_DONE PR_URL=https://github.com/demo/repo/pull/315c\nthis should be rejected because the task file was modified"}
JSON
  exit 0
fi

echo "unsupported mock openclaw args: $*" >&2
exit 1
EOF
chmod +x "$RUNNER_MOCKDIR/openclaw"

TASKFILE15D="$WORKDIR/docs/roadmaps/s5-case15d-tasklist.md"
mkdir -p "$(dirname "$TASKFILE15D")"
cat > "$TASKFILE15D" <<'EOF'
- [ ] S5X-15D: forbid tasklist mutation
EOF

set +e
OUT15D="$(PATH="$RUNNER_MOCKDIR:$PATH" CLAW_TASK_ID="S5X-15D" CLAW_TASK_TEXT="forbid tasklist mutation" CLAW_TASK_FILE="$TASKFILE15D" CLAW_RUN_ID="run15d" bash ./scripts/rl-task-agent.sh 2>&1)"
RC15D=$?
set -e
if [[ "$RC15D" -eq 0 ]]; then
  echo "[e2e-smoke] expected case15d to fail when task file mutates"
  printf '%s\n' "$OUT15D"
  exit 1
fi
if [[ "$OUT15D" != *"TASK_BLOCKED: task file was modified during runner execution"* ]]; then
  echo "[e2e-smoke] expected case15d mutation block message"
  printf '%s\n' "$OUT15D"
  exit 1
fi

echo "[e2e-smoke] case15e rl-task-agent creates PR with runner-owned --body-file and cleans it up"
RUNNER_PR_MOCKDIR="$WORKDIR/mockbin-runner-pr"
RUNNER_PR_LOG="$WORKDIR/mockbin-runner-pr.log"
RUNNER_PR_BODY_DIR="$WORKDIR/runner-pr-bodies"
rm -f "$RUNNER_PR_LOG"
rm -rf "$RUNNER_PR_BODY_DIR" "$REPO/.ralph/runner-agent-state/run15e"
mkdir -p "$RUNNER_PR_MOCKDIR" "$RUNNER_PR_BODY_DIR"
cat > "$RUNNER_PR_MOCKDIR/openclaw" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
if [[ "${1:-}" == "agent" ]]; then
  cat <<'JSON'
{"payloads":null,"text":"TASK_DONE\n\nACPX_TASK_RESULT_JSON: {\"summary\":\"Added body-file PR creation.\",\"verification\":[\"bash -n scripts/rl-task-agent.sh\"],\"notes\":[\"Reviewer-facing behavior is covered.\"],\"pushed_branch\":\"apb-5-body-file\"}"}
JSON
  exit 0
fi

echo "unsupported mock openclaw args: $*" >&2
exit 1
EOF
chmod +x "$RUNNER_PR_MOCKDIR/openclaw"
cat > "$RUNNER_PR_MOCKDIR/gh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
log="${RUNNER_PR_LOG:?}"
if [[ "${1:-}" == "pr" && "${2:-}" == "create" ]]; then
  body_file=""
  prev=""
  for arg in "$@"; do
    if [[ "$prev" == "--body-file" ]]; then
      body_file="$arg"
    fi
    if [[ "$arg" == "--body" ]]; then
      echo "inline --body is forbidden" >&2
      exit 1
    fi
    prev="$arg"
  done
  [[ -n "$body_file" && -f "$body_file" ]] || { echo "missing body file" >&2; exit 1; }
  grep -q '^## Summary' "$body_file"
  grep -q 'Added body-file PR creation.' "$body_file"
  printf 'create body_file=%s args=%s\n' "$body_file" "$*" >> "$log"
  echo 'https://github.com/demo/repo/pull/3155'
  exit 0
fi
if [[ "${1:-}" == "pr" && "${2:-}" == "view" ]]; then
  joined="$*"
  if [[ "$joined" == *"state,mergedAt"* ]]; then
    echo 'OPEN|'
    exit 0
  fi
  if [[ "$joined" == *"state,autoMergeRequest"* ]]; then
    echo '1'
    exit 0
  fi
  if [[ "$joined" == *"statusCheckRollup"* ]]; then
    echo ''
    exit 0
  fi
  if [[ "$joined" == *"baseRefName"* ]]; then
    echo 'main'
    exit 0
  fi
fi
if [[ "${1:-}" == "pr" && "${2:-}" == "merge" ]]; then
  exit 0
fi
if [[ "${1:-}" == "api" && "${2:-}" == "repos/demo/repo" ]]; then
  echo 'main'
  exit 0
fi
if [[ "${1:-}" == "api" && "${2:-}" == repos/demo/repo/branches/*/protection ]]; then
  exit 1
fi
if [[ "${1:-}" == "api" ]]; then
  echo '[]'
  exit 0
fi

echo "unsupported mock gh args: $*" >&2
exit 1
EOF
chmod +x "$RUNNER_PR_MOCKDIR/gh"

TASKFILE15E="$WORKDIR/docs/roadmaps/s5-case15e-tasklist.md"
mkdir -p "$(dirname "$TASKFILE15E")"
cat > "$TASKFILE15E" <<'EOF'
- [ ] S5X-15E: runner owned pr create
EOF
set +e
OUT15E="$(PATH="$RUNNER_PR_MOCKDIR:$PATH" RUNNER_PR_LOG="$RUNNER_PR_LOG" CLAW_GH_REPO="demo/repo" CLAW_PR_BODY_DIR="$RUNNER_PR_BODY_DIR" CLAW_TASK_ID="S5X-15E" CLAW_TASK_TEXT="runner owned pr create" CLAW_TASK_FILE="$TASKFILE15E" CLAW_RUN_ID="run15e" bash ./scripts/rl-task-agent.sh 2>&1)"
RC15E=$?
set -e
if [[ "$RC15E" -ne 10 ]]; then
  echo "[e2e-smoke] expected case15e rc=10, got $RC15E"
  printf '%s\n' "$OUT15E"
  exit 1
fi
FIRST15E="$(printf '%s\n' "$OUT15E" | awk 'NF { print; exit }')"
if [[ "$FIRST15E" != "TASK_WAITING_MERGE PR_URL=https://github.com/demo/repo/pull/3155" ]]; then
  echo "[e2e-smoke] expected case15e waiting merge first line"
  printf '%s\n' "$OUT15E"
  exit 1
fi
if ! grep -q -- '--body-file' "$RUNNER_PR_LOG"; then
  echo "[e2e-smoke] expected gh pr create --body-file in case15e"
  cat "$RUNNER_PR_LOG" || true
  exit 1
fi
if find "$RUNNER_PR_BODY_DIR" -type f | grep -q .; then
  echo "[e2e-smoke] expected runner PR body file cleanup in case15e"
  find "$RUNNER_PR_BODY_DIR" -type f -print
  exit 1
fi


echo "[e2e-smoke] case16 rl-task-agent sees required checks from ruleset detail endpoint"
RULESET_MOCKDIR="$WORKDIR/mockbin-ruleset"
mkdir -p "$RULESET_MOCKDIR"
cat > "$RULESET_MOCKDIR/gh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
if [[ "${1:-}" == "api" && "${2:-}" == "repos/demo/repo" ]]; then
  echo 'main'
  exit 0
fi
if [[ "${1:-}" == "api" && "${2:-}" == "repos/demo/repo/branches/feature/user/protection" ]]; then
  echo 'not protected' >&2
  exit 1
fi
if [[ "${1:-}" == "api" && "${2:-}" == "repos/demo/repo/rulesets" ]]; then
  if [[ "${3:-}" == "--jq" ]]; then
    echo '12451868'
  else
    cat <<'JSON'
[{"id":12451868,"name":"main","target":"branch","enforcement":"active","conditions":{"ref_name":{"include":["~DEFAULT_BRANCH","refs/heads/feature/user"],"exclude":[]}},"rules":null}]
JSON
  fi
  exit 0
fi
if [[ "${1:-}" == "api" && "${2:-}" == "repos/demo/repo/rulesets/12451868" ]]; then
  cat <<'JSON'
{"id":12451868,"name":"main","target":"branch","enforcement":"active","conditions":{"ref_name":{"include":["~DEFAULT_BRANCH","refs/heads/feature/user"],"exclude":[]}},"rules":[{"type":"required_status_checks","parameters":{"required_status_checks":[{"context":"rust","integration_id":15368}]}}]}
JSON
  exit 0
fi
if [[ "${1:-}" == "pr" && "${2:-}" == "view" ]]; then
  joined="$*"
  if [[ "$joined" == *"baseRefName"* ]]; then
    echo 'feature/user'
    exit 0
  fi
  if [[ "$joined" == *"state,mergedAt"* ]]; then
    echo 'OPEN|'
    exit 0
  fi
  if [[ "$joined" == *"state,autoMergeRequest"* ]]; then
    echo '1'
    exit 0
  fi
  if [[ "$joined" == *"statusCheckRollup"* ]]; then
    echo ''
    exit 0
  fi
fi
if [[ "${1:-}" == "pr" && "${2:-}" == "merge" ]]; then
  exit 0
fi

echo "unsupported mock gh args: $*" >&2
exit 1
EOF
chmod +x "$RULESET_MOCKDIR/gh"
mkdir -p "$WORKDIR/.ralph/runner-agent-state/run16"
cat > "$WORKDIR/.ralph/runner-agent-state/run16/S5X-16.env" <<'EOF'
PR_URL='https://github.com/demo/repo/pull/316'
EOF
set +e
OUT16="$(cd "$WORKDIR" && PATH="$RULESET_MOCKDIR:$PATH" CLAW_GH_REPO="demo/repo" CLAW_TASK_ID="S5X-16" CLAW_TASK_TEXT="ruleset detail required checks" CLAW_RUN_ID="run16" bash "$REPO/scripts/rl-task-agent.sh")"
RC16=$?
set -e
if [[ "$RC16" -ne 10 ]]; then
  echo "[e2e-smoke] expected case16 rc=10, got $RC16"
  printf '%s\n' "$OUT16"
  exit 1
fi
FIRST16="$(printf '%s\n' "$OUT16" | awk 'NF { print; exit }')"
if [[ "$FIRST16" != "TASK_WAITING_MERGE PR_URL=https://github.com/demo/repo/pull/316" ]]; then
  echo "[e2e-smoke] unexpected case16 first line"
  printf '%s\n' "$OUT16"
  exit 1
fi
if [[ "$OUT16" == *"WARN_REQUIRED_CHECKS_MISSING=1"* ]]; then
  echo "[e2e-smoke] case16 should not warn when ruleset detail enforces checks"
  printf '%s\n' "$OUT16"
  exit 1
fi

echo "[e2e-smoke] case17 rl-task-agent falls back to manual merge when --auto fails but PR still exists"
AUTOMERGE_FALLBACK_MOCKDIR="$WORKDIR/mockbin-automerge-fallback"
mkdir -p "$AUTOMERGE_FALLBACK_MOCKDIR"
cat > "$AUTOMERGE_FALLBACK_MOCKDIR/gh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
state_file="${MOCK_GH_STATE_FILE:?}"
if [[ "${1:-}" == "pr" && "${2:-}" == "view" ]]; then
  joined="$*"
  if [[ "$joined" == *"state,mergedAt"* ]]; then
    if [[ -f "$state_file" ]]; then
      echo 'MERGED|2026-04-06T01:00:00Z'
    else
      echo 'OPEN|'
    fi
    exit 0
  fi
  if [[ "$joined" == *"state,autoMergeRequest"* ]]; then
    echo '0'
    exit 0
  fi
  if [[ "$joined" == *"statusCheckRollup"* && "$joined" == *"| length"* ]]; then
    echo '1'
    exit 0
  fi
  if [[ "$joined" == *"statusCheckRollup"* ]]; then
    echo ''
    exit 0
  fi
fi
if [[ "${1:-}" == "pr" && "${2:-}" == "merge" ]]; then
  if [[ "$*" == *"--auto"* ]]; then
    echo 'GraphQL: Could not resolve to a PullRequest with the number of 68. (repository.pullRequest)' >&2
    exit 1
  fi
  : > "$state_file"
  exit 0
fi

echo "unsupported mock gh args: $*" >&2
exit 1
EOF
chmod +x "$AUTOMERGE_FALLBACK_MOCKDIR/gh"
MOCK_GH_STATE_FILE="$WORKDIR/mock-gh-merge-state-17"
rm -f "$MOCK_GH_STATE_FILE"
mkdir -p "$WORKDIR/.ralph/runner-agent-state/run17"
cat > "$WORKDIR/.ralph/runner-agent-state/run17/S5X-17.env" <<'EOF'
PR_URL='https://github.com/demo/repo/pull/68'
EOF
set +e
OUT17="$(cd "$WORKDIR" && PATH="$AUTOMERGE_FALLBACK_MOCKDIR:$PATH" MOCK_GH_STATE_FILE="$MOCK_GH_STATE_FILE" CLAW_GH_REPO="demo/repo" CLAW_TASK_ID="S5X-17" CLAW_TASK_TEXT="automerge fallback" CLAW_RUN_ID="run17" bash "$REPO/scripts/rl-task-agent.sh")"
RC17=$?
set -e
if [[ "$RC17" -ne 0 ]]; then
  echo "[e2e-smoke] expected case17 rc=0, got $RC17"
  printf '%s\n' "$OUT17"
  exit 1
fi
FIRST17="$(printf '%s\n' "$OUT17" | awk 'NF { print; exit }')"
if [[ "$FIRST17" != "TASK_DONE PR_URL=https://github.com/demo/repo/pull/68" ]]; then
  echo "[e2e-smoke] unexpected case17 first line"
  printf '%s\n' "$OUT17"
  exit 1
fi

echo "[e2e-smoke] case18 rl-task-agent blocks when --auto fails and PR no longer exists"
AUTOMERGE_MISSING_MOCKDIR="$WORKDIR/mockbin-automerge-missing"
mkdir -p "$AUTOMERGE_MISSING_MOCKDIR"
cat > "$AUTOMERGE_MISSING_MOCKDIR/gh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
if [[ "${1:-}" == "pr" && "${2:-}" == "view" ]]; then
  echo 'GraphQL: Could not resolve to a PullRequest with the number of 69. (repository.pullRequest)' >&2
  exit 1
fi
if [[ "${1:-}" == "pr" && "${2:-}" == "merge" ]]; then
  echo 'GraphQL: Could not resolve to a PullRequest with the number of 69. (repository.pullRequest)' >&2
  exit 1
fi

echo "unsupported mock gh args: $*" >&2
exit 1
EOF
chmod +x "$AUTOMERGE_MISSING_MOCKDIR/gh"
mkdir -p "$WORKDIR/.ralph/runner-agent-state/run18"
cat > "$WORKDIR/.ralph/runner-agent-state/run18/S5X-18.env" <<'EOF'
PR_URL='https://github.com/demo/repo/pull/69'
EOF
set +e
OUT18="$(cd "$WORKDIR" && PATH="$AUTOMERGE_MISSING_MOCKDIR:$PATH" CLAW_GH_REPO="demo/repo" CLAW_TASK_ID="S5X-18" CLAW_TASK_TEXT="automerge missing pr" CLAW_RUN_ID="run18" bash "$REPO/scripts/rl-task-agent.sh" 2>&1)"
RC18=$?
set -e
if [[ "$RC18" -ne 2 ]]; then
  echo "[e2e-smoke] expected case18 rc=2, got $RC18"
  printf '%s\n' "$OUT18"
  exit 1
fi
if [[ "$OUT18" != *"TASK_BLOCKED: auto-merge enable failed for PR_URL=https://github.com/demo/repo/pull/69"* ]]; then
  echo "[e2e-smoke] unexpected case18 blocked message"
  printf '%s\n' "$OUT18"
  exit 1
fi

echo "[e2e-smoke] ok"
