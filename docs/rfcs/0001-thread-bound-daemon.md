# RFC-0001: Thread-bound Ralph Loop Daemon

## Problem
Current Ralph-loop behavior can look stalled because state updates and runtime process ownership are not strictly coupled. A run may appear `running` while no watcher/worker is alive, and notifications may not be routed to the right thread deterministically.

## Decision
Introduce a **thread-bound run daemon** in Rust:

- one daemon per `run_id`
- run metadata explicitly binds to `session_key` + `thread_id`
- daemon is single writer for run state
- lightweight 60s tick with lease heartbeat
- terminal state (`done|failed|stopped`) auto-stops daemon

## Data model
Per-run directory:

```text
.ralph/runs/<run_id>/
  manifest.json
  state.json
  events.jsonl
  notify-queue.jsonl   # TODO (next step)
  daemon.pid           # optional (can be in manifest)
```

### manifest.json
- `run_id`
- `repo_path`
- `session_key`
- `channel`
- `thread_id`
- `owner_message_id`
- `started_at`
- `daemon_pid`

### state.json
- `version` (CAS support)
- `status`: `idle|running|waiting|blocked|done|failed|stopped`
- `summary`
- `waiting_reason`
- `lease_expires_at`
- `updated_at`

## State ownership rule
- daemon: **may write state**
- workers/watchers: append events only
- reducers: daemon consumes events and updates state

## Tick policy (low-load)
Every 60s (configurable):
1. acquire run lock
2. read state
3. refresh lease
4. check stop signal / terminal status
5. run minimal external checks only when required

No LLM calls in tick loop.

## External checks budget
- Max 1 GitHub API call per tick when `waiting` + PR-tracking active.
- Timeout per external call: 3-5s.
- Backoff for unchanged PR state: 1m -> 2m -> 4m (cap 5m).

## OpenClaw integration
Event-driven only:
- enqueue notify event (`notify-queue.jsonl`)
- flusher sends message/wake on change
- dedupe by `event_id`

No periodic OpenClaw agent turn for monitoring.

## Acceptance criteria
1. `running` with dead daemon is detected and transitions to `blocked` within 90s.
2. Each run notifies only its bound thread.
3. No duplicate notification for same state transition.
4. Daemon exits on terminal state and cleans resources.
5. Tick loop avg CPU and I/O remain low under idle conditions.

## Next steps
1. implement PR tracking reducer
2. implement stale daemon/orphan detection
3. add OpenClaw delivery bridge (thread-targeted send/wake)
4. add integration tests (start -> wait PR -> merged -> next -> done)

## Progress note
- notify queue + local dispatcher has been implemented in the first Rust iteration.
