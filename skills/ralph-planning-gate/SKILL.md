---
name: ralph-planning-gate
description: Plan and approve loop work before starting claw-loopd. Use when a user asks to prepare tasks first (goal confirmation, approach discussion, task decomposition), or when loop work should not start until the tasklist is explicitly approved.
---

# Ralph Planning Gate

## Rule
- Do not start `claw-loopd` before explicit tasklist approval.
- Keep asking/confirming until the user says the tasklist is approved.

## Planning flow
1. Confirm objective and success condition.
2. Confirm constraints and guardrails.
3. Propose approach options (short, practical).
4. Decompose into task IDs with completion criteria.
5. Show draft tasklist in markdown checkbox format.
6. Ask explicit approval.
7. On approval, write/update tasklist and only then start loop.

## Required interview points
Ask and confirm:
- goal / done_when
- scope in/out
- risk tolerance and safety limits
- quality gates (`fmt`, `clippy -D warnings`, `test`, `build`, `e2e`)
- delivery style (small PRs, merge policy)
- completion judgment mode:
  - default: runner success with PR+merge confirmation (`--auto-check-on-success true` + `scripts/rl-task-agent.sh`)
  - strict/manual: completion-gated (`--auto-check-on-success false`)

## Tasklist format
Use this exact format per item:
- `- [ ] <ID>: <task summary>`

Prefer stable IDs like `P1-1`, `P1-2`, `A2-3`.
Each task should have one clear completion signal.

## Approval checkpoint
Before loop start, send:
- summarized goal
- chosen approach
- proposed tasklist
- loop runtime settings

Then ask: "このタスクリストで開始していい?"

Start only after a clear yes.

## Loop start template
After approval, start with explicit runner mode.
Resolve `<discord_user_id>` from inbound `sender_id` of the request message (never hardcode in repo files).
Resolve `<agent_id>` as project-specific loop agent id (avoid `main` when running loops in parallel).
Resolve `<feedback_thread_id>` as the main control thread for completion aggregation.

```bash
claw-loopd start \
  --repo <repo> \
  --session-key <session_key> \
  --channel discord \
  --thread-id <thread_id> \
  --requester-user-id <discord_user_id> \
  --task-agent-id <agent_id> \
  --feedback-thread-id <feedback_thread_id> \
  --feedback-channel discord \
  --tick-sec 60 \
  --deliver-openclaw \
  --max-task-loops 10 \
  --task-file <tasklist_path> \
  --task-runner-cmd './scripts/rl-task-agent.sh' \
  --auto-check-on-success true
```

If strict manual completion is requested, use `--auto-check-on-success false`.
Use `--max-runtime-sec` only when you explicitly want time-bounded execution.

## Report policy after start
- Report task start, completion, failure, and limit reached.
- Keep reports in the same work thread.
- If blocked, report immediately with concrete reason.
