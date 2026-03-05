# claw-loop

Thread-bound monitored Ralph loop daemon (Rust).

## Goal
- Bind each autonomous loop run to a specific chat thread/session.
- Keep monitoring lightweight (1-minute tick without waking LLM each tick).
- Guarantee explicit stop/blocked/done visibility.

## Current status
- Thread-bound CLI implemented (`start`, `daemon`, `stop`, `status`, `notify`, `track-pr`, `sweep`).
- Per-run isolated state under `.ralph/runs/<run_id>/`.
- Event log and lease heartbeat in place.
- Notification queue + local dispatcher log (`notify-queue.jsonl` -> `notify-dispatched.jsonl`) in place.
- PR tracking reducer in daemon tick:
  - polls only while `waiting`
  - backoff (60s -> 120s -> 240s -> 300s)
  - auto-arm merge when possible
  - transitions on merged/closed and emits notifications
- Orphan/stale guard via `sweep` command:
  - checks lease expiry against daemon process ownership
  - marks run `blocked` when lease expired and daemon process is gone
- OpenClaw delivery bridge is TODO.

## Build

```bash
cargo build
```

## Quick test

```bash
# 1) start daemon
cargo run -- start --repo . --session-key test --channel discord --thread-id thread-test --tick-sec 1

# 2) bind PR tracking (example)
cargo run -- track-pr --repo . --run-id <RUN_ID> --gh-repo n01e0/dimpact --pr 24 --merge-method merge

# 3) inspect status
cargo run -- status --repo . --run-id <RUN_ID>

# 4) reconcile stale/orphan runs (cron想定: 1分おき)
cargo run -- sweep --repo .

# 5) stop daemon
cargo run -- stop --repo . --run-id <RUN_ID>
```
