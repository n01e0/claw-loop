# claw-loop

Thread-bound monitored Ralph loop daemon (Rust).

## Goal
- Bind each autonomous loop run to a specific chat thread/session.
- Keep monitoring lightweight (1-minute tick without waking LLM each tick).
- Guarantee explicit stop/blocked/done visibility.

## Current status
- Thread-bound CLI implemented (`start`, `daemon`, `stop`, `status`, `delivery-report`, `requeue-dead-letter`, `notify`, `track-pr`, `sweep`).
- Per-run isolated state under `.ralph/runs/<run_id>/`.
- Event log and lease heartbeat in place.
- Notification queue + dispatcher log (`notify-queue.jsonl` -> `notify-dispatched.jsonl`) in place.
- Delivery traces in run dir:
  - `notify-attempts.jsonl` (every attempt)
  - `notify-ack.jsonl` (ack success/failure history)
  - `notify-dead-letter.jsonl` (max-attempt exceeded)
  - `delivery-report --status failed` includes normalized `failed_reason_histogram`
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
- Remaining TODO: OpenClaw delivery acknowledgement integration + long-run soak tests.
- Roadmap / tasklist: `docs/roadmaps/ack-integration-tasklist.md`
- Ack contract: `docs/specs/ack-contract.md`

## Build

```bash
cargo build
```

## CI
- `cargo fmt --all -- --check`
- `cargo clippy --all-targets --all-features -- -D warnings`
- `cargo test --all --all-features`
- `./scripts/e2e-smoke.sh ./target/debug/claw-loopd`
  - lifecycle (start/notify/stop)
  - orphan sweep block
  - single-writer lock rejection
  - PR reducer merge transition using mocked gh
  - delivery retry/backoff metrics with mocked openclaw
  - dead-letter transition + failed-only report filter
  - dead-letter requeue back to pending queue
  - requeue idempotency (`--event-id`) + dry-run behavior

## Quick test

```bash
# 1) start daemon (OpenClaw delivery有効化するなら --deliver-openclaw を付ける)
cargo run -- start --repo . --session-key test --channel discord --thread-id thread-test --tick-sec 1 --deliver-openclaw

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

# 3.3) dry-run requeue (state変更なし)
cargo run -- requeue-dead-letter --repo . --run-id <RUN_ID> --event-id <EVENT_ID> --limit 1 --reset-attempts --dry-run

# (任意) delivery bridge を送信せず検証
CLAW_LOOPD_OPENCLAW_DRY_RUN=1 cargo run -- start --repo . --session-key test --channel discord --thread-id thread-test --tick-sec 1 --deliver-openclaw

# 4) reconcile stale/orphan runs (cron想定: 1分おき)
cargo run -- sweep --repo .

# 5) stop daemon
cargo run -- stop --repo . --run-id <RUN_ID>
```
