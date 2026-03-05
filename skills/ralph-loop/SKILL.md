---
name: ralph-loop
description: Start a thread-bound monitored loop daemon for "ralph loopでやって" requests. Use when autonomous iterative development must continue without manual nudges, with explicit waiting/blocked visibility and per-thread notification binding.
---

# Ralph Loop (thread-bound daemon)

## Start contract
Before implementation loop starts:
1. Confirm strategy/goal/done_when/scope constraints.
2. Start run daemon:
   - `claw-loopd start --repo <repo> --session-key <session_key> --channel discord --thread-id <thread_id>`
3. Post run id to the active thread.

## Runtime contract
- Daemon is the single state writer.
- Loop workers append events; daemon updates state.
- Report at least once per loop.
- Waiting/blocked must always include explicit reason.
- Use `claw-loopd status` to inspect bound run status and queued/dispatched notifications.
- Use `claw-loopd notify` for explicit loop progress events during implementation tests.
- Use `claw-loopd track-pr` to bind waiting state to a specific PR and let daemon poll with backoff.
- Run `claw-loopd sweep --repo <repo>` every minute (cron/systemd timer) to block orphaned runs with expired lease.

## Stop contract
On `done|failed|stopped`:
- daemon stops itself
- final summary posted to same thread

## Non-goals (for now)
- No global shared state across unrelated threads.
- No per-minute LLM monitoring loop.
