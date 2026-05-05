# ACPX Codex backend design memo

This memo covers ACB-1 for moving claw-loop task execution from an
OpenClaw-agent runner path to an ACPX + Codex runner path.

The goal is to fix the interface contract before implementation. This document
does not change daemon behavior, runner behavior, tasklists, or OpenClaw
configuration.

## Scope

In scope:

- current `scripts/rl-task-agent.sh` boundary inventory
- current daemon runner/state responsibility inventory
- installed ACPX/Codex CLI assumptions verified for this design pass
- proposed minimal backend split between `openclaw-agent` and `acpx-codex`
- explicit TODOs where ACPX behavior or repo-local packaging is not yet pinned

Out of scope:

- implementing the backend
- editing `.ralph/tasklist-acpx-codex-backend.md`
- changing the runner output contract
- creating PRs, pushing branches, or sending external messages

## Inspected sources

- `.ralph/tasklist-acpx-codex-backend.md`
- `scripts/rl-task-agent.sh`
- `src/main.rs`
- `README.md`
- existing specs under `docs/specs/`
- installed `acpx` CLI help
- installed `acpx` package docs and generated CLI code under
  `/home/shioriko/.npm-global/lib/node_modules/acpx`

Installed ACPX observed for this memo:

```text
acpx path: /home/shioriko/.npm-global/bin/acpx
acpx version: 0.6.1
```

TODO: ACB-2 must resolve and record the plugin-local pinned `acpx` command
before falling back to any global `acpx`. This memo used the installed global
binary only to pin the current interface shape.

## ACPX assumptions

These assumptions are pinned to installed `acpx 0.6.1` help/docs/source.

### Global command shape

Use Codex through ACPX, not through OpenClaw agent sessions:

```bash
acpx --cwd <repo> --format quiet --timeout <seconds> codex -s <session-name> --file <prompt-file>
```

Important global options:

- `--cwd <dir>` sets the working directory used for session scope and client
  filesystem/terminal handling.
- `--format <text|json|quiet>` controls output mode.
- `--timeout <seconds>` bounds wait time for the agent response.
- `--ttl <seconds>` controls queue-owner idle lifetime after the queue drains;
  default is 300 seconds, `0` keeps the owner alive indefinitely.

Design requirement: claw-loop must always pass `--cwd <repo>` explicitly and
must not rely on ACPX's default current directory.

### Sessions ensure/new

Persistent prompt mode requires an existing saved session record. Create or
ensure that record before the task prompt:

```bash
acpx --cwd <repo> --format json codex sessions ensure --name <session-name>
```

Observed semantics:

- Session scope is `(agentCommand, absolute cwd, optional name)`.
- `sessions ensure --name <name>` is idempotent: it returns an existing scoped
  session or creates one when missing.
- `sessions new --name <name>` creates a fresh scoped session and soft-closes
  the prior open one for that scope.
- `sessions close [name]` soft-closes the session and keeps the record on disk.
- Session metadata lives under `~/.acpx/sessions/`.
- Prompt lookup walks up from `cwd` or `--cwd` to the nearest git root,
  inclusive, and selects the nearest active session matching the agent command
  and optional name. If no git root is found, it matches exact cwd only.
- `-s/--session <name>` selects a named session in that scope.
- Session-control JSON payloads include `acpxRecordId` and `acpxSessionId`.
  `agentSessionId` is present only when the adapter exposes a provider-native
  session id. The local ACPX record id must not be treated as a provider-native
  Codex session id.

Design requirement: `acpx-codex` must run `sessions ensure --name <name>` before
persistent prompt and must fail explicitly if ensure fails. Missing sessions
must not be treated as "maybe prompt will create one".

TODO: Capture an example `sessions ensure --format json` payload from the
plugin-local pinned ACPX before implementation tests assert exact fields.

### Persistent prompt

Persistent prompt uses the saved session:

```bash
acpx --cwd <repo> --format quiet --timeout <seconds> codex \
  -s <session-name> --file <prompt-file>
```

Equivalent explicit form:

```bash
acpx --cwd <repo> --format quiet --timeout <seconds> codex prompt \
  -s <session-name> --file <prompt-file>
```

Observed semantics:

- `prompt` is the default Codex verb.
- Prompt requires an existing session record from `sessions new` or
  `sessions ensure`.
- If no session exists, ACPX reports `NO_SESSION` and exits non-zero.
- Prompt input may be read from `--file <path>`, where `-` means stdin.
- On interrupt, ACPX attempts cooperative `session/cancel` before force-kill
  fallback.

Design requirement: use a prompt file rather than embedding a large prompt in a
shell command string. This keeps quoting deterministic and avoids prompt text
leaking into process listings.

### One-shot exec

One-shot mode exists:

```bash
acpx --cwd <repo> --format quiet --timeout <seconds> codex exec \
  --file <prompt-file>
```

Observed semantics:

- `exec` runs a single prompt in a temporary ACP session.
- It does not reuse or save persistent session state.
- It accepts `--file <path>`.

Design requirement: ACB-3 should use persistent prompt, not `exec`, because the
tasklist explicitly requires deterministic saved session names. `exec` remains
useful for mocks, smoke probes, or future stateless helper commands only.

### Cancel

Cancel command:

```bash
acpx --cwd <repo> codex cancel -s <session-name>
```

Observed semantics:

- `cancel` cooperatively sends ACP `session/cancel` through queue-owner IPC.
- It returns success when no prompt is running and prints `nothing to cancel`.
- It does not delete the saved session record.

Design requirement: daemon stop/kill integration can map graceful stop to
`cancel`, but ACB-3 can initially keep the existing process-level stop behavior
unless the backend owns a long-running ACPX child.

TODO: Verify whether ACPX `cancel` returns stable JSON under `--format json` for
both "cancel requested" and "nothing to cancel".

### Status

Status command:

```bash
acpx --cwd <repo> --format json codex status -s <session-name>
```

Observed semantics:

- `status` reports local status for the current session agent process.
- Documented statuses include `running`, `idle`, `dead`, and `no-session`.
- Status payloads include `acpxRecordId` and `acpxSessionId`; process details
  may include pid, uptime, last prompt, and exit code.

Design requirement: status is diagnostic state, not the primary runner contract.
The daemon should continue to use runner output contract lines for task state.

TODO: Capture actual JSON examples for `running`, `idle`, `dead`, and
`no-session` before adding state assertions.

### Cwd scoping

ACPX persistent sessions are cwd scoped and also do git-root directory walking
for prompt lookup.

Design requirement:

- Always pass the canonical repo path via `--cwd <repo>`.
- Create/ensure the session with the same `--cwd <repo>` used for prompt,
  status, and cancel.
- Avoid relying on ACPX's directory walk for correctness. The walk can be a
  compatibility fallback, but claw-loop's selected session name and cwd must be
  explicit.

### Named sessions

Use deterministic named sessions:

```text
rl-<run-id>-<task-id>
```

Properties:

- Include run id to prevent separate loop runs from sharing session history.
- Include task id for readability and per-task isolation.
- Keep the name stable for retries of the same task in the same run.
- Use only a conservative character set after normalization:
  `[A-Za-z0-9._-]`.

TODO: Decide whether recovery tasks should use their own task id in the session
name, or whether they should intentionally resume the blocked task's session.
The safer initial default is "own task id".

### Output modes

ACPX output formats:

- `text`: human-readable stream with assistant text and tool updates.
- `json`: raw ACP NDJSON event stream for automation.
- `quiet`: final assistant text only.

Session-control JSON payloads are documented separately from prompt output.

Design requirement:

- Initial `acpx-codex` runner should prefer `--format quiet` for prompt output.
  It gives final assistant text only, which preserves the existing
  `TASK_*` contract extraction shape.
- Use `--format json` for `sessions ensure`, `status`, and future structured
  diagnostic capture.
- Do not use `text` for contract parsing. It is useful for humans but includes
  tool/update stream content.
- Structured task result metadata is defined in
  `docs/specs/acpx-task-result-contract.md`; the first-line `TASK_*` signal
  remains authoritative, and `ACPX_TASK_RESULT_JSON` provides runner-readable
  summary, verification, notes, pushed branch, and optional PR metadata.
- If `json` is later used for prompt output, parse NDJSON events and extract the
  final assistant text. Do not grep raw ANSI/PTY output.

### Queue and busy behavior

Observed ACPX queue behavior:

- Queueing is per persistent session.
- The active ACPX process for a running prompt becomes the queue owner.
- Other invocations submit prompts over local IPC.
- Unix-like IPC uses `~/.acpx/queues/<hash>.sock`.
- Ownership uses `~/.acpx/queues/<hash>.lock`.
- Default submission behavior is enqueue-and-wait: if the session is busy, the
  new prompt queues and the CLI waits for queued prompt completion.
- `--no-wait` submits to the queue and returns after queue acknowledgement.
- Queue max depth defaults to 16 in installed ACPX config code.

Design requirement:

- The task runner should not pass `--no-wait`; the daemon expects one runner
  invocation to finish with a concrete `TASK_*` line.
- A busy session should normally be treated as ACPX queue wait, bounded by
  `--timeout`.
- A queue-depth or queue-IPC failure should become explicit `TASK_BLOCKED:
  acpx queue unavailable ...` unless ACPX exposes a stable retryable code later.
- `TASK_WAITING_AGENT_LOCK` remains part of the common runner contract for
  legacy OpenClaw compatibility. `acpx-codex` should only emit it if a verified
  ACPX busy/lock condition is safely retryable and semantically equivalent.

TODO: Verify installed ACPX's exact stderr/JSON shape when queue max depth is
exceeded or queue IPC is corrupt.

### Timeout and exit classification

Installed ACPX exit-code constants:

```text
0   SUCCESS
1   ERROR / runtime failure
2   USAGE
3   TIMEOUT
4   NO_SESSION
5   PERMISSION_DENIED or PERMISSION_PROMPT_UNAVAILABLE
130 INTERRUPTED
```

Observed defaults:

- Config default timeout is `null`/unset unless CLI/config sets one.
- `--timeout <seconds>` is accepted as a positive number.
- Timeout errors include output code `TIMEOUT` and hint to increase timeout or
  inspect provider stall.

Proposed runner mapping:

| ACPX result | Runner output | Daemon state |
|---|---|---|
| rc=0 and final assistant text has valid `TASK_DONE PR_URL=<url>` with merged PR | `TASK_DONE PR_URL=<url>` | `RunnerTaskState::Done` after existing merge guard/checklist update |
| rc=0 and valid `TASK_WAITING_MERGE PR_URL=<url>` | same line | `RunnerTaskState::WaitingMerge`, top-level `waiting` |
| rc=0 and valid `TASK_WAITING_DEPENDENCY ...` | same line | `RunnerTaskState::WaitingDependency`, top-level `waiting` |
| rc=0 and valid `TASK_BLOCKED: <reason>` | same line, non-zero runner exit | `RunnerTaskState::Blocked`, top-level `blocked` |
| rc=0 but missing/malformed `TASK_*` contract | `TASK_BLOCKED: malformed acpx-codex output ...` | `blocked` |
| rc=3 / `TIMEOUT` | `TASK_BLOCKED: acpx codex prompt timed out ...` | `blocked` initially |
| rc=4 / `NO_SESSION` after explicit ensure | `TASK_BLOCKED: acpx codex session missing after ensure ...` | `blocked` |
| rc=5 / permission denied or prompt unavailable | `TASK_BLOCKED: acpx permission denied ...` | `blocked` |
| rc=2 / usage error | `TASK_BLOCKED: acpx invocation usage error ...` | `blocked` |
| rc=130 / interrupted by daemon stop | stop path should record cancellation/stopped, not task completion | `stopped` if operator stop; otherwise `blocked` |
| queue IPC/depth failure | `TASK_BLOCKED: acpx queue unavailable ...` | `blocked` |

TODO: Decide after ACB-4 whether prompt timeout should sometimes map to
`TASK_WAITING_DEPENDENCY`/retry wait. For now it is blocked because an unbounded
retry can duplicate work or hide provider stalls.

## Current runner boundary inventory

The current default runner is `scripts/rl-task-agent.sh`. It is a shell wrapper
around OpenClaw agent execution plus PR/CI post-processing.

Current responsibilities:

- Validate required daemon environment:
  - `CLAW_TASK_ID`
  - `CLAW_TASK_TEXT`
- Derive run/session inputs:
  - `repo_path="$(pwd)"`
  - `CLAW_RUN_ID` defaulting to `local`
  - `CLAW_AGENT_ID` defaulting to `main`
  - OpenClaw session id `rl-${run_id}-${CLAW_TASK_ID}`
  - `CLAW_AGENT_TIMEOUT_SEC` defaulting to 1800
- Maintain per-task runner state under:
  - `.ralph/runner-agent-state/<run_id>/<task-id>.env`
  - currently stores `PR_URL` while waiting on merge
- Parse OpenClaw `--json` output by extracting a JSON object and then assistant
  text from multiple possible payload shapes.
- Fall back to OpenClaw session JSONL logs under:
  - `${OPENCLAW_HOME:-$HOME/.openclaw}/agents/<agent>/sessions/*.jsonl`
  - used to recover a final `TASK_*` signal after command failure or empty
    output.
- Detect OpenClaw session lock text and emit `TASK_WAITING_AGENT_LOCK`.
- Classify retryable OpenClaw prompt errors such as request timeout/abort into
  a waiting-style retry response.
- Build the dogfood task prompt, including:
  - repo path
  - task id/text/file
  - run id
  - task kind
  - backlog detector status/count/summary/updated-at
  - strict first-line `TASK_*` contract instructions
- Guard tasklist ownership:
  - hash task plan before agent run
  - hash it after agent run
  - block if task file changed during runner execution
- Enforce failure-first backlog gate as a second guard:
  - if backlog is active and task is not repair scoped, emit `TASK_BLOCKED`.
- Call the OpenClaw backend:
  - `openclaw agent --local --agent "$agent_id" --session-id "$agent_session_id" --timeout "$agent_timeout_sec" --message "$PROMPT" --json`
- Extract first non-empty/contract line and PR URL.
- For `TASK_DONE`, require `PR_URL`, persist it, and re-enter PR merge handling.
- For `TASK_WAITING_MERGE`, persist `PR_URL` when present and re-enter PR merge
  handling.
- For `TASK_WAITING_DEPENDENCY`, clear per-task state and exit with wait code.
- Exit non-zero for blocked/malformed output.
- Own PR/CI merge post-processing:
  - infer GitHub repo from `CLAW_GH_REPO` or `remote.origin.url`
  - inspect PR state with `gh pr view`
  - arm auto-merge with `gh pr merge --auto --squash --delete-branch`
  - fall back to manual squash merge when auto-merge is unavailable
  - detect failed/pending/successful checks
  - detect missing required checks policy via branch protection/rulesets
  - emit `TASK_WAITING_MERGE`, `TASK_DONE`, or `TASK_BLOCKED` accordingly

Backend-coupled pieces that must move behind a backend boundary:

- OpenClaw agent id/session id construction
- OpenClaw agent creation expectation
- `openclaw agent ... --json` invocation
- OpenClaw output payload parsing
- OpenClaw session JSONL failure recovery
- OpenClaw session lock and prompt-error classification

Backend-neutral pieces that should remain shared:

- prompt construction
- tasklist mutation guard
- first-line runner contract parsing
- PR URL extraction
- PR/CI merge post-processing
- backlog second guard
- per-task `PR_URL` waiting state

## Current daemon boundary inventory

The daemon code in `src/main.rs` owns task selection, process invocation, runner
contract classification, durable state, and notifications.

Current daemon runner inputs:

- `--task-runner-cmd <cmd>` enables task execution mode.
- `--task-agent-id <id>` is optional but, when present, `start` currently calls
  `openclaw agents list` and `openclaw agents add --workspace <repo>` before
  daemon spawn.
- Manifest records:
  - `task_runner_cmd`
  - `task_agent_id`
  - task file and approval metadata
  - notification/delivery settings
  - auto-recover and backlog detector settings

Current `run_task_once` behavior:

- Selects the next task or uses a selected task.
- Executes `bash -lc <task_runner_cmd>` with cwd set to the repo.
- Passes environment:
  - `CLAW_TASK_ID`
  - `CLAW_TASK_TEXT`
  - `CLAW_TASK_LINE`
  - `CLAW_TASK_FILE`
  - `CLAW_RUN_ID`
  - `CLAW_THREAD_ID`
  - `CLAW_CHANNEL`
  - `CLAW_AGENT_ID`
  - `CLAW_TASK_KIND`
  - backlog detector env vars when present
- Captures stdout, stderr, exit code, and success.
- If `auto_check_on_success` is enabled, validates `TASK_DONE PR_URL=<url>` and
  confirms the PR is merged before marking the checklist item done.
- If merge confirmation times out, converts completion to
  `TASK_WAITING_MERGE PR_URL=<url>`.

Current daemon contract parser:

- Finds the last non-empty stdout line starting with `TASK_`.
- Parses:
  - `TASK_DONE PR_URL=<absolute-url>`
  - `TASK_WAITING_MERGE [PR_URL=<url>]`
  - `TASK_WAITING_DEPENDENCY TASK_ID=<id> DEPENDS_ON_TASK=<id>` and/or
    `DEPENDS_ON_PR_URL=<absolute-url>`
  - other `TASK_WAITING*` as waiting merge compatibility
  - `TASK_BLOCKED` through non-success runner failure path
- Requires `TASK_WAITING_DEPENDENCY` to have `TASK_ID` either explicitly or via
  selected task fallback, and at least one dependency target.
- Requires dependency PR URLs to be absolute.

Current daemon state mapping:

| Runner signal | `RunnerTaskState` | Top-level state |
|---|---|---|
| selected next task before process | `Queued` then `Running` | usually `running` |
| valid `TASK_DONE PR_URL=<url>` and merged | `Done` | continue running or stop when all done |
| valid `TASK_WAITING_MERGE` | `WaitingMerge` | `waiting` |
| valid `TASK_WAITING_DEPENDENCY` | `WaitingDependency` | `waiting` |
| non-success/malformed/blocked runner output | `Blocked` | `blocked` |
| legacy `TASK_WAITING_AGENT_LOCK` | parsed as generic `TASK_WAITING*` | currently `WaitingMerge`/`waiting` compatibility |

Current daemon responsibilities that must stay backend-neutral:

- task selection and failure-first backlog selection gate
- state files under `.ralph/runs/<run-id>/`
- runner-state durability
- notification queueing/flushing
- PR waiting rechecks
- dependency wait rechecks
- blocked context and auto-recovery
- tasklist approval/hash checks
- loop caps and stop controls

Current daemon responsibilities that are OpenClaw-agent-specific:

- pre-start task agent creation via `openclaw agents list/add`
- `CLAW_AGENT_ID` as required backend input for the default runner

## Proposed minimal backend split

Add an explicit backend dimension without changing the runner contract:

```text
runner backend = openclaw-agent | acpx-codex
```

No implicit fallback is allowed. If `acpx-codex` is selected and ACPX is
unavailable, the task blocks with an explicit ACPX error. If `openclaw-agent` is
selected, the legacy OpenClaw path remains available as compatibility.

### Backend-neutral runner pipeline

Keep one shared runner orchestration layer with these steps:

1. Validate `CLAW_TASK_ID` and `CLAW_TASK_TEXT`.
2. Build the same dogfood task prompt.
3. Apply backlog second guard.
4. Snapshot task plan hash before backend call.
5. Invoke selected backend.
6. Extract final assistant text.
7. Print assistant text.
8. Verify task plan hash did not change.
9. Parse first contract line and PR URL.
10. Run existing PR/CI merge post-processing for `TASK_DONE` and
    `TASK_WAITING_MERGE`.
11. Return existing exit semantics to daemon.

### `openclaw-agent` backend

Keep the current behavior behind a named backend:

- Requires/uses `CLAW_AGENT_ID`.
- Uses OpenClaw session id `rl-${run_id}-${task_id}`.
- Calls `openclaw agent --local --agent ... --session-id ... --timeout ...
  --message ... --json`.
- May recover signals from OpenClaw session JSONL logs.
- May emit `TASK_WAITING_AGENT_LOCK` when OpenClaw session locking is observed.
- May use existing `CLAW_AGENT_TIMEOUT_SEC`.

Daemon start may continue to ensure/create the task agent only when this backend
is selected.

### `acpx-codex` backend

New behavior:

- Does not call `openclaw agents add`.
- Does not call `openclaw agent`.
- Does not create OpenClaw agent sessions.
- Resolves ACPX command explicitly and records the resolved command/version in
  manifest/status in ACB-2.
- Constructs deterministic session name from run/task identity.
- Ensures session:

```bash
acpx --cwd <repo> --format json codex sessions ensure --name <session-name>
```

- Writes prompt to a runner-owned prompt file under `.ralph/runs/<run-id>/` or
  another run artifact directory.
- Runs persistent prompt:

```bash
acpx --cwd <repo> --format quiet --timeout <seconds> codex \
  -s <session-name> --file <prompt-file>
```

- Extracts the contract from final assistant text.
- Does not scrape PTYs or ANSI output.
- Treats ACPX command/session/permission/timeout failures as explicit
  `TASK_BLOCKED` unless a later ACB-4 fixture proves a safe retry/wait mapping.

### Minimal daemon changes implied for later ACBs

ACB-1 does not implement these, but the split implies:

- Add backend selection to CLI/manifest.
- Only call `ensure_task_agent_exists` for `openclaw-agent`.
- Record resolved backend details:
  - backend name
  - ACPX command path/version for `acpx-codex`
  - deterministic session name for current task
  - output format
  - timeout
- Keep `task_runner_cmd` compatibility until the backend model replaces or wraps
  it.
- Ensure mock ACPX tests do not require real Codex.

## Open questions and TODOs

- TODO: Locate and pin the plugin-local ACPX package/command. The inspected
  `/home/shioriko/.openclaw-slots/current/openclaw/extensions/acpx` path from
  historical logs was not present during inspection.
- TODO: Capture exact `--format json` payloads for:
  - `sessions ensure --name <name>`
  - `sessions new --name <name>`
  - `status -s <name>`
  - `cancel -s <name>`
- TODO: Verify ACPX queue max-depth and corrupt-IPC error shapes.
- TODO: Verify whether prompt `--format json` includes a stable final assistant
  text event that would be better than `quiet` for contract extraction.
- TODO: Decide whether `acpx-codex` should ever emit `TASK_WAITING_AGENT_LOCK`;
  initial design says no unless a verified ACPX retryable lock condition exists.
- TODO: Decide the durable location and retention policy for generated prompt
  files.
- TODO: Decide whether daemon `stop --immediate` should call ACPX cancel before
  killing a running backend child, and how long to wait.
- TODO: Decide whether ACPX prompt timeout can become a retryable wait in a
  future policy. Initial mapping is blocked.
- TODO: Confirm Codex adapter-specific auth failure text and whether it maps
  consistently to ACPX rc=5 or runtime rc=1.
