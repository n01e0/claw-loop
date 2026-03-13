---
name: ralph-loop
description: Operate thread-bound claw-loopd runs for "ralph loop" requests. Use when you must start/monitor/stop autonomous iteration in a Discord thread, keep waiting/blocked reasons explicit, and route notifications to the same thread.
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
- safety guard (`--max-task-loops` / `--max-ticks`), with `max_task_loops` defaulting to `10`
- `--max-runtime-sec` only when needed (omit for long pause-oriented operation)
- dogfood runner command (`--task-runner-cmd`), monitor-only when omitted
- recommended runner: `scripts/rl-task-agent.sh` (PR creation → auto-merge preferred → merge confirmation; if repo auto-merge is unavailable, daemon keeps watching CI and squash-merges after green; auto-merge/CI failures are fail-closed as blocked for auto-recovery; missing required checks policy is surfaced as waiting warning)
- `--task-agent-id <agent_id>` (dedicated loop agent; split per loop for parallel operation)
- approved tasklist gate:
  - `claw-loopd task-approve --file <task_file> --approved-by <name>`
  - `--approved-tasklist-hash <hash>` is required on `start`
  - start fails if approval markers/hash are missing or mismatched
- `--auto-check-on-success` defaults to `true` (auto-check on runner success)
- `--auto-recover-blocked` (enable blocked→recovery-task auto-resume)
- `--auto-recover-blocked-max-attempts <n>` (default `3`)
- `--requester-user-id <discord_user_id>` (completion mention target; never hardcode)
  - in Discord operation, pass the triggering message `sender_id` directly
- `--feedback-thread-id <thread_id> [--feedback-channel <channel>]` (main aggregation target for completion summaries)

Never start without `thread_id` + `session_key`.

## Start flow
1. Confirm preflight in-thread:
   - strategy
   - goal
   - done_when
   - scope/constraints
   - if tasklist is not approved yet, run `ralph-planning-gate` first
2. Resolve requester id from inbound metadata:
   - use current message `sender_id` as `<discord_user_id>`
3. Resolve loop agent id:
   - use project-specific agent id (e.g., `loop-worker-<project>`)
4. Resolve aggregation target:
   - use main control thread id as `<feedback_thread_id>`
5. Stamp and hash the approved tasklist:
   - `claw-loopd task-approve --file <task_file> --approved-by <name>`
   - capture `approved_tasklist_hash` from JSON output
6. Start daemon:
   - `claw-loopd start --repo <repo> --session-key <session_key> --channel discord --thread-id <thread_id> --requester-user-id <discord_user_id> --task-agent-id <agent_id> --feedback-thread-id <feedback_thread_id> --feedback-channel discord --tick-sec 60 --deliver-openclaw --max-task-loops 10 --task-runner-cmd './scripts/rl-task-agent.sh' --approved-tasklist-hash <hash> --auto-recover-blocked --auto-recover-blocked-max-attempts 3`
   - default: auto-check and continue on runner success (`--auto-check-on-success=true`)
   - with `--auto-check-on-success=false`, run in completion-gated mode (do not start the next task until the active task is checked complete)
   - daemon blocks the run if the approved plan markers/hash drift after start
7. Post `run_id` in-thread immediately.
8. Record first planned loop item.

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
Task-level notifications are single-channel from `scripts/rl-task-agent.sh`:
- `🚀 <task> started`
- `⏳ <task> waiting for merge`
- `✅ <task> merged`
- `❌ <task> blocked`

Daemon notifications are lifecycle-level:
- `run_started`: right after `start`
- `pr_tracking_started`: right after `track-pr`
- `pr_poll_error`: PR poll error first occurrence
- `pr_merged`: tracked PR merged
- `pr_closed`: tracked PR closed without merge
- `all_tasks_completed`: tasklist has no open item
- `orphan_blocked`: sweep detected expired lease + missing daemon
- `auto_stopped`: max-task-loops / max-ticks / max-runtime reached
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
- Normal stop: `claw-loopd stop ...` -> daemon stops on next tick after seeing `control.stop`.
- Immediate stop: `claw-loopd stop ... --immediate` -> set state to `stopped` immediately and stop daemon PID with TERM/KILL.
- Stop latency target:
  - normal: `<= tick_sec + flush time`
  - immediate: near-immediate (process signal + local flush)

`blocked` does not stop immediately by itself (except when max-task-loops / max-ticks / max-runtime is reached and `auto_stopped` is triggered).

## Per-loop reporting rules
- Send one progress update per loop minimum.
- If no progress, still send `waiting` or `blocked` with concrete reason.
- Keep updates in the same work thread.
- On completion/failure, send final summary without waiting for prompt.

## Recovery rules
- If daemon pid changed, allow daemon PID rebind (built-in).
- If queue/ack state drifted after restart, rely on startup reconciliation (built-in).
- If process dies and lease expires, run `sweep` to mark orphaned run as `blocked`.
