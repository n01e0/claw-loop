---
name: ralph-loop
description: Operate thread-bound claw-loopd runs for "ralph loopでやって" requests. Use when you must start/monitor/stop autonomous iteration in a Discord thread, keep waiting/blocked reasons explicit, and route notifications to the same thread.
---

# Ralph Loop (thread-bound daemon)

## Required inputs
Collect all of these before start:
- `repo` (absolute path)
- `session_key` (thread-bound session key)
- `channel` (`discord`)
- `thread_id` (current Discord thread id)
- `tick_sec` (default 60)
- delivery mode (`--deliver-openclaw` on/off)
- safety guard (`--max-task-loops` / `--max-ticks` / `--max-runtime-sec`) ※`max_task_loops` のデフォルトは 10
- dogfood runner command (`--task-runner-cmd`) ※未指定時は monitor_only
- `--auto-check-on-success` default は `true`（agent判定で自動チェック）

Never start without `thread_id` + `session_key`.

## Start flow
1. Confirm preflight in-thread:
   - strategy
   - goal
   - done_when
   - scope/constraints
2. Start daemon:
   - `claw-loopd start --repo <repo> --session-key <session_key> --channel discord --thread-id <thread_id> --tick-sec 60 --deliver-openclaw --max-task-loops 10 --max-runtime-sec 3600 --task-runner-cmd '<loop command>'`
   - default: agent判定で自動チェックして次へ進む（`--auto-check-on-success=true`）
   - `--auto-check-on-success=false` で完了確認待ちモード（進行中タスクが完了チェックされるまで次は開始しない）
3. Post `run_id` in-thread immediately.
4. Record first planned loop item.

## Command set (operator minimum)
- Status:
  - `claw-loopd status --repo <repo> --run-id <run_id>`
- Progress notification:
  - `claw-loopd notify --repo <repo> --run-id <run_id> --kind progress --message "..."`
- PR binding:
  - `claw-loopd track-pr --repo <repo> --run-id <run_id> --gh-repo <owner/repo> --pr <num> --merge-method merge`
- Tasklist helper:
  - `claw-loopd task-next`
  - `claw-loopd task-check --id <TASK_ID> --done true|false`
  - `claw-loopd task-run-once --cmd '<loop command>'`
- Orphan sweep (periodic):
  - `claw-loopd sweep --repo <repo>`
- Stop:
  - `claw-loopd stop --repo <repo> --run-id <run_id>`
  - `claw-loopd stop --repo <repo> --run-id <run_id> --immediate` (kill switch)

## Notification contract (what arrives and when)
Expect these kinds:
- `run_started`: right after `start`
- `pr_tracking_started`: right after `track-pr`
- `pr_poll_error`: PR poll error first occurrence
- `pr_merged`: tracked PR merged
- `pr_closed`: tracked PR closed without merge
- `orphan_blocked`: sweep detected expired lease + missing daemon
- `auto_stopped`: max-task-loops / max-ticks / max-runtime に到達
- `stopped`: stop request processed
- `terminal`: daemon exits because state is `done|failed|stopped`

Delivery behavior:
- With `--deliver-openclaw`: send to Discord thread.
- Without it: keep local queue/dispatched logs only.

## State contract
- `running`: daemon active and ticking
- `waiting`: next action/input/CI pending
- `blocked`: cannot proceed (must include reason)
- `done`: completed
- `failed`: unrecoverable failure
- `stopped`: explicit stop request handled

Always set explicit `waiting_reason` for `waiting` and `blocked`.

## Stop semantics (Discord operations)
- Discord users stop runs by asking the agent **or** native command kill switch.
- Normal stop: `claw-loopd stop ...` → daemon stops on next tick after seeing `control.stop`.
- Immediate stop: `claw-loopd stop ... --immediate` → stateを即 `stopped` 化し、daemon pidをTERM/KILLで停止。
- Stop latency target:
  - normal: `<= tick_sec + flush time`
  - immediate: near-immediate (process signal + local flush)

`blocked` は即停止しない（ただし max-task-loops / max-ticks / max-runtime 到達時は `auto_stopped`）。

## Per-loop reporting rules
- Send one progress update per loop minimum.
- If no progress, still send `waiting` or `blocked` with concrete reason.
- Keep updates in the same work thread.
- On completion/failure, send final summary without waiting for prompt.

## Recovery rules
- If daemon pid changed, allow daemon PID rebind (built-in).
- If queue/ack state drifted after restart, rely on startup reconciliation (built-in).
- If process dies and lease expires, run `sweep` to mark orphaned run as `blocked`.
