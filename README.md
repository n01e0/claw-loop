# claw-loop

Thread-bound autonomous loop daemon for OpenClaw workflows.

`claw-loopd` runs one checklist task at a time, drives implementation through a task runner, and keeps progress/blocked/waiting state visible in a chat thread.

## Why this exists

When you run long autonomous work in a chat thread, you need four things:

- **Clear state** (`running`, `waiting`, `blocked`, `stopped`)
- **Strict completion contract** (task done means PR merged, not just “looks done”)
- **Operational safety** (stop/kill, loop caps, recovery guards)
- **Traceability** (events, delivery attempts, acks, dead-letter)

`claw-loopd` provides those as a small Rust daemon with OpenClaw-first integration.

---

## Quickstart (5 minutes)

### 1) Build

```bash
cargo build --release
```

### 2) Approve the tasklist

```bash
cargo run -- task-approve \
  --file docs/roadmaps/ack-integration-tasklist.md \
  --approved-by <approver_name>
```

This writes approval markers into the tasklist and prints an `approved_tasklist_hash`.

### 3) Start a run (Discord/OpenClaw delivery enabled)

```bash
cargo run -- start \
  --repo . \
  --session-key discord:<thread_id> \
  --channel discord \
  --thread-id <thread_id> \
  --requester-user-id <discord_user_id> \
  --task-agent-id <loop_agent_id> \
  --feedback-thread-id <control_thread_id> \
  --feedback-channel discord \
  --tick-sec 60 \
  --deliver-openclaw \
  --max-task-loops 10 \
  --task-runner-cmd './scripts/rl-task-agent.sh' \
  --require-task-approval \
  --approved-tasklist-hash <approved_tasklist_hash> \
  --auto-recover-blocked \
  --auto-recover-blocked-max-attempts 3
```

By default, `start` no longer requires task approval markers.

If you pass `--require-task-approval`, start will fail if:

- approval markers are missing
- `--approved-tasklist-hash` is missing
- current task plan hash does not match the approved hash
- `--task-agent-id` creation/check fails before daemon start

If `--task-agent-id` does not exist yet, `claw-loopd start` now auto-creates it with `openclaw agents add --workspace <repo>` before spawning the daemon.

### 4) Inspect status

```bash
cargo run -- status --repo . --run-id <run_id>
```

### 4) Stop

```bash
cargo run -- stop --repo . --run-id <run_id>
# immediate kill path
cargo run -- stop --repo . --run-id <run_id> --immediate
```

---

## Core behavior

### Task execution model

- One task at a time from a markdown checklist (`--task-file`)
- Runner command (`--task-runner-cmd`) receives task context env vars
- Daemon ticks periodically (`--tick-sec`) and records every transition
- Start is not gated by tasklist approval unless you explicitly pass `--require-task-approval` together with `--approved-tasklist-hash`

### Planning gate / approval contract

Task approval is now **optional / opt-in**.

- `task-approve` still stamps the tasklist with:
  - `Approved-By: <name>`
  - `Approved-At: <RFC3339 timestamp>`
- It also prints a canonical `approved_tasklist_hash`
- `start` only enforces approval when you pass `--require-task-approval` together with `--approved-tasklist-hash`
- While approval is enabled for a run, daemon re-checks the approved plan hash each tick
- If the plan changes outside daemon-owned mutations, run is blocked with `tasklist approval invalidated`

The approved hash is based on task IDs/text in checklist order, so checkbox flips (`[ ]` → `[x]`) do not invalidate the run. Approval metadata drift alone no longer invalidates an active run.

### Strict done contract

A task is considered complete only when runner output contract is satisfied:

- `TASK_DONE PR_URL=<url>` and PR is **actually merged**
- If merge is pending: `TASK_WAITING_MERGE PR_URL=<url>`
- If work is waiting on an upstream dependency: `TASK_WAITING_DEPENDENCY [TASK_ID=<id>] [DEPENDS_ON_TASK=<id>] [DEPENDS_ON_PR_URL=<absolute-url>]` (`DEPENDS_ON_TASK` or `DEPENDS_ON_PR_URL` required)
  - use this when the task should not be forced into an isolated green PR because it depends on a prior phase / stacked task or PR
- If blocked: `TASK_BLOCKED: <reason>`
  - if phase / stacked sequencing is required but the dependency target is still unknown, block explicitly and say that the phase/stacked dependency is not yet identifiable

The `TASK_*` contract line must be the first emitted line. Task agents must not send progress narration, delegation chatter, `NO_REPLY`, or `HEARTBEAT_OK` instead of the contract.

### Auto-merge + CI failure handling

The default runner (`scripts/rl-task-agent.sh`) does all of the following:

- requests/arms auto-merge for task PRs when the repository supports it
- re-applies `gh pr merge --auto --squash` while waiting (to avoid “enabled once but not armed” drift)
- if the repository does not allow auto-merge, keeps watching CI and performs a normal squash merge itself once checks are green
- checks PR CI rollup; if checks fail during initial run or while rechecking `waiting_merge`, daemon converts that block into an auto-recovery task when `--auto-recover-blocked` is enabled

This allows daemon auto-recovery flow to turn CI failures into explicit recovery work instead of silently waiting forever.

### Required-checks warning (repo policy visibility)

While in waiting-merge state, runner inspects branch policy:

- branch protection required checks
- active rulesets required checks

If required checks are not enforced on target branch, waiting output is tagged with warning marker and daemon surfaces a warning in `task_waiting_merge` notification.

---

## OpenClaw/Discord integration

### Delivery modes

With `--deliver-openclaw`, notifications are sent via `openclaw message`.

Delivery path includes:

- queue (`notify-queue.jsonl`)
- attempts (`notify-attempts.jsonl`)
- acks (`notify-ack.jsonl`)
- dispatched (`notify-dispatched.jsonl`)
- dead-letter (`notify-dead-letter.jsonl`)

### Notification strategy

- status-style events use edit-first delivery flow
- important lifecycle events remain explicit posts
- enqueue triggers immediate flush; failed delivery falls back to retry queue

### Blocked notification quality

Blocked notifications include:

- requester mention (Discord)
- `reason`
- `recovery`
- `next` action (auto-recovery enabled/disabled path)

---

## Auto-recovery from blocked

Enable with:

- `--auto-recover-blocked`
- `--auto-recover-blocked-max-attempts <N>` (default `3`)

Behavior:

- on task blocked, daemon can append a recovery task (`<TASK_ID>-RECOVER...`)
- marks blocked task as checked, queues recovery task, and resumes
- guard rails stop runaway loops:
  - duplicate blocked reason detection
  - max attempts cap
  - generated recovery task failure guard

---

## Safety controls

- `--max-task-loops <N>`: primary runaway cap (default `10`)
- `--max-ticks <N>`: optional tick cap
- `--max-runtime-sec <SEC>`: optional wall-clock cap
- `stop`: graceful stop via control file
- `stop --immediate`: state-first terminal update + process kill path

On all tasks completed, daemon transitions to terminal stop state instead of lingering indefinitely.

---

## CLI surface

Implemented commands:

- `start`
- `daemon`
- `stop`
- `status`
- `notify`
- `track-pr`
- `sweep`
- `delivery-report`
- `requeue-dead-letter`
- `task-next`
- `task-check`
- `task-run-once`
- `task-approve`

---

## Task runner contract (important)

Runner must emit first-line protocol responses:

- `TASK_DONE PR_URL=<url>`
- `TASK_WAITING_MERGE PR_URL=<url>`
- `TASK_WAITING_DEPENDENCY [TASK_ID=<id>] [DEPENDS_ON_TASK=<id>] [DEPENDS_ON_PR_URL=<absolute-url>]`
  - at least one of `DEPENDS_ON_TASK` / `DEPENDS_ON_PR_URL` is required
  - use this when the task cannot land as an isolated green PR and must wait for a prior phase / stacked task or PR
  - daemon preserves this as dependency waiting state (not `waiting_merge` and not `blocked`)
  - daemon tracks same-run dependency task/PR metadata, keeps dependency waits out of auto-recover, and re-runs the waiting task once the upstream task/PR is cleared
- `TASK_BLOCKED: <reason>`
  - if phase / stacked sequencing is required but no dependency target can be named yet, return `TASK_BLOCKED` and explain that the upstream phase/stacked dependency is still unidentified
- `TASK_WAITING_AGENT_LOCK` (treated as waiting, not hard failure)

No preamble is allowed before the `TASK_*` line: do not narrate progress, tool use, or sub-agent handoffs, and do not return `NO_REPLY` / `HEARTBEAT_OK` from the task runner agent.

Any non-conforming output is treated as failure and surfaced in state/logs.

---

## Project layout

- `src/main.rs`
  - daemon orchestration, state transitions, command wiring
- `src/notify_policy.rs`
  - notification routing (`send` vs `edit`), retry/normalization helpers
- `src/tasklist.rs`
  - checklist parser/update helpers
- `scripts/rl-task-agent.sh`
  - dogfood runner contract + PR/merge/CI checks orchestration
- `skills/ralph-loop/SKILL.md`
  - operator workflow in OpenClaw
- `skills/ralph-planning-gate/SKILL.md`
  - pre-loop planning/approval workflow

---

## Testing & CI

CI should run:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all --all-features
find scripts -type f -name '*.sh' -print0 | xargs -0 -r -n1 bash -n
./scripts/e2e-smoke.sh ./target/debug/claw-loopd
```

The test suite covers delivery retry behavior, dead-letter flows, status edit routing, task completion contract, and recovery guards.

---

## Troubleshooting

### “CI is green but task keeps waiting_merge”

Likely causes:

- PR not merged yet (green != merged)
- auto-merge not armed
- required checks policy missing on target branch (warning should appear)

Check:

```bash
cargo run -- status --repo <repo> --run-id <run_id>
cargo run -- delivery-report --repo <repo> --run-id <run_id> --status all --limit 50
```

### “No notifications in thread”

- verify `--deliver-openclaw`
- inspect queue/attempt files in `.ralph/runs/<run_id>/`
- check `CLAW_LOOPD_OPENCLAW_TIMEOUT_SEC` and OpenClaw availability

### “Blocked repeatedly”

- inspect `state.waiting_reason` and runner stderr in events
- if auto-recover is enabled, review recovery guard counters in status JSON

---

## Documentation map

- Roadmap/tasklist: `docs/roadmaps/ack-integration-tasklist.md`
- Ack contract: `docs/specs/ack-contract.md`
- Ack retry policy: `docs/specs/ack-retry-policy.md`
- Ack state transitions: `docs/specs/ack-state-transitions.md`
- Soak scenario: `docs/specs/ack-soak-24h.md`
- Dogfood runbook: `docs/runbooks/dogfood-runbook.md`

---

## License

MIT. See `LICENSE`.
