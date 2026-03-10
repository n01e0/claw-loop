# claw-loop

Thread-bound monitored Ralph loop daemon (Rust).

## Goal
- Bind each autonomous loop run to a specific chat thread/session.
- Keep monitoring lightweight (1-minute tick without waking LLM each tick).
- Guarantee explicit stop/blocked/done visibility.

## Current status
- Thread-bound CLI implemented (`start`, `daemon`, `stop`, `status`, `delivery-report`, `requeue-dead-letter`, `notify`, `track-pr`, `sweep`, `task-next`, `task-check`, `task-run-once`).
- Safety guard for runaway loops:
  - `start --max-task-loops <N>`: auto-stop after N completed task checks (default: 10)
  - `start --max-ticks <N>`: optional tick-based cap
  - `start --max-runtime-sec <SEC>`: optional wall-clock cap
- Dogfood runner mode:
  - `start --task-runner-cmd '<shell command>'` to execute/monitor one task loop at a time
  - recommended: `scripts/rl-task-agent.sh` (task agent must produce PR and wait/confirm merge)
  - `start --task-agent-id <agent_id>` specifies a dedicated loop agent (recommended to isolate per loop in parallel runs)
  - `start --requester-user-id <id>` adds a mention summary for that user when all tasks are completed
  - `start --feedback-thread-id <thread_id> [--feedback-channel <channel>]` sends completion summaries to a separate thread (main aggregation destination)
  - default (`--auto-check-on-success=true`): automatically marks runner success as task completion
  - optional (`--auto-check-on-success=false`): runner starts one task, then waits until the checklist is marked complete before starting the next task
  - runner can return `TASK_WAITING_MERGE` (exit 10) to keep task in waiting instead of failing
  - `status.runner.mode` reports `dogfood` or `monitor_only`
- Per-run isolated state under `.ralph/runs/<run_id>/`.
- Event log and lease heartbeat in place.
- Notification queue + dispatcher log (`notify-queue.jsonl` -> `notify-dispatched.jsonl`) in place.
- Delivery traces in run dir:
  - `notify-attempts.jsonl` (every attempt)
  - `notify-ack.jsonl` (ack success/failure history)
  - `notify-dead-letter.jsonl` (max-attempt exceeded)
  - `delivery-report --status failed` includes normalized `failed_reason_histogram`
  - `delivery-report` rows include ack fields: `acked`, `ack_at`, `ack_category`, `ack_error`
  - `status` includes ack aggregates: `acked_total`, `unacked_total`, `last_acked_at`
- Optional OpenClaw delivery bridge (`--deliver-openclaw`):
  - sends notifications via `openclaw message send`
  - keeps unsent events in queue for retry
  - tracks attempts/backoff/last_error and delivery metrics
  - moves permanently failing events to dead-letter after max attempts (`CLAW_LOOPD_DELIVERY_MAX_ATTEMPTS`, default 5)
- Optional binary overrides for deterministic tests:
  - `CLAW_LOOPD_GH_BIN=/path/to/mock-gh`
  - `CLAW_LOOPD_OPENCLAW_BIN=/path/to/mock-openclaw`
- PR tracking reducer in daemon tick:
  - polls only while `waiting`
  - backoff (60s -> 120s -> 240s -> 300s)
  - auto-arm merge when possible
  - transitions on merged/closed and emits notifications
- Orphan/stale guard via `sweep` command:
  - checks lease expiry against daemon process ownership
  - marks run `blocked` when lease expired and daemon process is gone
- Remaining TODO: expand autonomous dogfood entry points + continuously improve long-run soak operations.
- Roadmap / tasklist: `docs/roadmaps/ack-integration-tasklist.md`
- Skills:
  - `skills/ralph-loop/SKILL.md` (loop operations)
  - `skills/ralph-planning-gate/SKILL.md` (pre-loop planning + approval gate)
- Ack contract: `docs/specs/ack-contract.md`
- Ack retry policy: `docs/specs/ack-retry-policy.md`
- Ack state transitions: `docs/specs/ack-state-transitions.md`
- 24h soak scenario: `docs/specs/ack-soak-24h.md` (`scripts/soak-24h.sh`)
- Dogfood runbook: `docs/runbooks/dogfood-runbook.md`

## Code structure

- `src/main.rs`
  - CLI entrypoint, daemon loop orchestration, runtime wiring
- `src/notify_policy.rs`
  - notification delivery mode routing (`send` vs `edit`)
  - OpenClaw message-id parsing
  - retry/backoff policy and error normalization
- `src/tasklist.rs`
  - task checklist parsing/counting/updating helpers

When adding new behavior, prefer extending the module that owns the concern first, then keep `main.rs` as orchestration glue.

## Build

```bash
cargo build
```

## CI
- `cargo fmt --all -- --check`
- `cargo clippy --all-targets --all-features -- -D warnings`
- `cargo test --all --all-features`
- `find scripts -type f -name '*.sh' -print0 | xargs -0 -r -n1 bash -n`
- `./scripts/e2e-smoke.sh ./target/debug/claw-loopd`
  - lifecycle (start/notify/stop)
  - orphan sweep block
  - single-writer lock rejection
  - PR reducer merge transition using mocked gh
  - delivery retry/backoff metrics with mocked openclaw
  - dead-letter transition + failed-only report filter
  - dead-letter requeue back to pending queue
  - requeue idempotency (`--event-id`) + dry-run behavior
  - single-status post reduction + duplicate suppression

## Quick test

```bash
# 1) start daemon (add --deliver-openclaw to enable OpenClaw delivery)
# max-task-loops defaults to 10 (based on done-delta in task_file)
# adding task-runner-cmd enables dogfood runner mode (default is auto-check based on agent output)
cargo run -- start --repo . --session-key test --channel discord --thread-id thread-test --requester-user-id EXAMPLE_DISCORD_USER_ID --task-agent-id loop-worker --feedback-thread-id EXAMPLE_FEEDBACK_THREAD_ID --feedback-channel discord --tick-sec 1 --deliver-openclaw --task-runner-cmd './scripts/rl-task-agent.sh'

# 2) bind PR tracking (example)
cargo run -- track-pr --repo . --run-id <RUN_ID> --gh-repo n01e0/dimpact --pr 24 --merge-method merge

# 3) inspect status
cargo run -- status --repo . --run-id <RUN_ID>

# 3.1) event-level delivery report
cargo run -- delivery-report --repo . --run-id <RUN_ID> --limit 20 --status all
# status: all|pending|delivered|failed
# failed status output includes:
# - normalized `failed_reason_histogram`
# - `failed_reason_histogram_by_kind`
# - `--failed-window <N>` for recent-N failed histogram window

# 3.2) requeue dead-letter entries
cargo run -- requeue-dead-letter --repo . --run-id <RUN_ID> --event-id <EVENT_ID> --limit 1 --reset-attempts

# 3.3) dry-run requeue (no state changes)
cargo run -- requeue-dead-letter --repo . --run-id <RUN_ID> --event-id <EVENT_ID> --limit 1 --reset-attempts --dry-run

# 3.4) dogfood: fetch the next unchecked task
cargo run -- task-next

# 3.5) dogfood: mark a task as done
cargo run -- task-check --id A1-5 --done true

# 3.6) dogfood: run the next unchecked task once (auto-check on success)
cargo run -- task-run-once --cmd 'echo "$CLAW_TASK_ID :: $CLAW_TASK_TEXT"'

# 3.7) optional: daemon runner waits for manual completion confirmation
cargo run -- start --repo . --session-key test --channel discord --thread-id thread-test --tick-sec 1 --task-runner-cmd 'echo "$CLAW_TASK_ID"' --auto-check-on-success false

# optional: verify delivery bridge behavior without actually sending
CLAW_LOOPD_OPENCLAW_DRY_RUN=1 cargo run -- start --repo . --session-key test --channel discord --thread-id thread-test --tick-sec 1 --deliver-openclaw

# 4) reconcile stale/orphan runs (intended for cron, e.g. every minute)
cargo run -- sweep --repo .

# 5) stop daemon
cargo run -- stop --repo . --run-id <RUN_ID>

# 5.1) kill switch (immediate stop)
cargo run -- stop --repo . --run-id <RUN_ID> --immediate
```
