# claw-loop

Thread-bound monitored Ralph loop daemon (Rust).

## Goal
- Bind each autonomous loop run to a specific chat thread/session.
- Keep monitoring lightweight (1-minute tick without waking LLM each tick).
- Guarantee explicit stop/blocked/done visibility.

## Current status
- CLI skeleton implemented (`start`, `daemon`, `stop`).
- Per-run isolated state under `.ralph/runs/<run_id>/`.
- Event log and lease heartbeat in place.
- PR-sync/notify integration is TODO.

## Build

```bash
cargo build
```
