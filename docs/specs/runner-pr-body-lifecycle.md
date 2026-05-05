# Runner-owned PR body lifecycle

This spec inventories the current runner/daemon boundaries for PR creation,
PR body generation, merge waiting, and cleanup. It also fixes the rule that PR
bodies describe shipped artifacts only; execution reports live in runner state,
events, and notifications.

## 1. Current responsibility inventory

### 1-1. PR creation

Current state:

- The daemon selects a task and launches the task runner with `CLAW_TASK_*`,
  `CLAW_RUN_ID`, notification, and backlog context.
- The default runner, `scripts/rl-task-agent.sh`, builds a strict dogfood prompt
  and delegates implementation to an OpenClaw agent session.
- The delegated agent is currently instructed to commit, push, create the PR,
  and return one of the `TASK_*` first-line contracts.
- The runner does **not** currently own `gh pr create`.
- The runner only extracts `PR_URL=<url>` from the agent's contract line and
  stores it in `.ralph/runner-agent-state/<run-id>/<task-id>.env` while the PR
  is waiting.
- The daemon validates `TASK_DONE PR_URL=<url>` only when auto-check is enabled
  and the PR is already merged; otherwise it converts completion back to
  `TASK_WAITING_MERGE PR_URL=<url>`.

Target ownership:

- The runner must own PR creation for the standard path.
- A backend may create a PR only in an explicit compatibility mode, and only
  using a runner-generated body file path.
- Inline `gh pr create --body ...` is forbidden.
- The PR creation command must use `gh pr create --body-file <runner-temp-file>`.

### 1-2. PR body generation

Current state:

- PR body content is effectively agent-owned.
- The prompt tells the agent to create a PR and report execution status.
- Because there is no structured artifact boundary, implementation summaries,
  verification logs, runner mechanics, worktree details, auto-merge status, and
  dogfood execution commentary can be mixed into the PR body.
- The runner does not currently create a body file, validate body content, or
  read the created PR body back from GitHub.

Target ownership:

- The implementation backend returns structured task result data, not a final
  PR body. Minimum fields:
  - `summary`: shipped artifact / behavior changes
  - `verification`: commands or checks that validate the artifact
  - `notes`: user-facing caveats, follow-ups, or known limits
  - `branch`: pushed branch or ref
  - optional PR metadata only when the compatibility path created a PR
- The runner builds the PR body from a fixed template and writes it to a
  runner-owned temporary body file.
- The body file is the only source used for PR create/edit.
- The PR body must pass validation before create/edit and again after read-back
  from GitHub.

### 1-3. Merge waiting

Current state:

- The runner owns the first merge post-processing pass after receiving
  `TASK_DONE` or `TASK_WAITING_MERGE`:
  - infer GitHub repo
  - inspect PR state with `gh pr view`
  - arm auto-merge using `gh pr merge --auto --squash --delete-branch`
  - fall back to manual squash merge when auto-merge is unavailable and checks
    are not pending
  - classify failed checks as `TASK_BLOCKED`
  - emit `TASK_WAITING_MERGE` while CI or merge is still pending
- The daemon owns durable waiting-state rechecks:
  - persist current task PR URL in runner state
  - call `ensure_waiting_merge_progress`
  - re-arm auto-merge or manual-merge fallback when appropriate
  - mark the task done only after merged confirmation
  - turn CI failure, dirty merge state, or non-retryable merge errors into
    blocked context for repair.

Target ownership:

- Merge waiting remains backend-neutral runner/daemon responsibility.
- Backend implementation sessions should not decide whether waiting is complete;
  they only provide the pushed artifact and structured result data.
- `TASK_WAITING_MERGE PR_URL=<url>` is the contract for an open PR whose current
  task is waiting on CI/merge.
- `TASK_DONE PR_URL=<url>` is valid only after merged confirmation.

### 1-4. Cleanup

Current state:

- The current default runner has per-task PR URL state cleanup/overwrite, but no
  disposable worktree lifecycle.
- The PR body temp-file lifecycle does not exist yet.
- Branch deletion is delegated to `gh pr merge --delete-branch` in merge paths.
- The daemon does not remove task worktrees after merge because task worktrees do
  not exist yet.

Target ownership:

- PR body temp files are runner-owned scratch files and must be deleted via a
  `trap` after create/edit/read-back completes, whether the operation succeeds
  or fails.
- Disposable task worktrees are retained while the task is running, blocked,
  waiting on merge, or under repair.
- A task worktree is removed only after the PR is confirmed merged and the
  worktree is clean.
- Dirty, blocked, or debug worktrees are never auto-deleted; state/status/events
  must record why cleanup was skipped.

## 2. Artifact description vs execution report rule

The PR body is an artifact description. It answers: "What changed, how was it
verified, and what should a reviewer know about the shipped result?"

The PR body must not be an execution report. It must not describe:

- which agent/backend/session performed the work
- worktree paths, runner temp files, prompt files, or daemon internals
- auto-merge arm attempts, merge waiting, cleanup attempts, or branch deletion
- task runner contract lines such as `TASK_WAITING_MERGE` / `TASK_DONE`
- chronological dogfood logs like "I created a branch", "I pushed", or "I am
  waiting for CI"
- raw tool-call transcripts, shell quoting details, or notification delivery
  details

Execution reports belong in these channels instead:

- runner stdout/stderr contract lines
- `.ralph/runs/<run-id>/runner-state.json`
- `.ralph/runs/<run-id>/events.jsonl`
- Discord/current-channel notifications
- blocked context / recovery task prompts
- local debug logs or retained worktrees when blocked

## 3. Required PR body template

Runner-generated PR bodies must use this logical structure. Empty optional
sections may be omitted, but headings must not be malformed.

```markdown
## Summary
- <artifact/result change>

## Verification
- <command or check and outcome>

## Notes
- <reviewer-facing caveat or follow-up, if any>
```

Template constraints:

- `Summary` is about shipped changes, not execution mechanics.
- `Verification` lists artifact validation commands/results only.
- `Notes` may include reviewer-facing caveats; it must not include runner state
  or merge waiting status.
- If verification could not be run, say so as an artifact/review caveat, not as
  a runner diary.

## 4. Body-file and read-back validation

Before PR create/edit, the runner validates the generated body file. After PR
create/edit, it reads the body back with `gh pr view --json body` and validates
again.

Validation must reject or repair at least:

- literal shell-expansion damage such as `$##`
- literal escaped newlines (`\\n`) where markdown line breaks were intended
- broken or missing markdown headings from the fixed template
- unclosed fenced code blocks
- execution-report vocabulary in the PR body, including backend/session/worktree
  mechanics, auto-merge/merge-wait/cleanup status, and `TASK_*` contract lines
- empty `Summary` or empty `Verification` when the backend claimed a complete
  artifact

Validation failure is a runner/blocking error unless a deterministic rewrite from
structured task result data can produce a valid replacement body.

## 5. State/event separation requirements

When the runner creates or edits a PR, durable state and events should capture
execution details that are intentionally excluded from the PR body:

- body temp-file path basename or opaque id, never required for reviewers
- pre-create validation result
- PR URL/number and branch
- post-create read-back validation result
- auto-merge arm/merge fallback attempts
- waiting, blocked, repair, and cleanup transitions
- cleanup skipped reason for retained worktrees or temp artifacts

Notifications may summarize execution status for the operator, but they should
not be copied into the PR body.

## 6. Implications for follow-up tasks

- APB-2 should add disposable worktree state without putting worktree paths in
  PR bodies.
- APB-3 should define the structured backend result contract as PR body input,
  not as the PR body itself.
- APB-4/APB-5 should implement the runner-owned body builder and force
  `--body-file` use.
- APB-6 should implement body-file and read-back validation from this spec.
- APB-7/APB-9 should keep merge waiting and cleanup in runner/daemon state,
  events, and notifications only.

## APB-2 disposable task worktree lifecycle

`claw-loopd` now treats dogfood task execution as worktree-scoped runner work:

- Each selected task gets a deterministic disposable worktree at
  `<repo>/<task_worktree_root>/<run-id>/<task-id-slug>`.
- The runner creates a per-task branch named `ralph/<run-id-prefix>/<task-id-slug>`
  from the daemon repository `HEAD` and executes the task runner command with
  that worktree as its current directory.
- The task runner receives the worktree contract through environment variables:
  `CLAW_TASK_WORKTREE`, `CLAW_TASK_BRANCH`, `CLAW_TASK_BASE_BRANCH`, and
  `CLAW_TASK_WORKTREE_CLEANUP_POLICY`.
- Durable state separates runner mechanics from PR body content:
  - `manifest.json` stores `task_worktree_root` and a `task_worktrees` map keyed
    by task id, including worktree path, branch, base branch, cleanup policy, and
    lifecycle state.
  - `runner-state.json` stores `current_worktree` / `last_worktree` alongside
    current and last task status.
  - `status` exposes the same fields under `runner.current.worktree`,
    `runner.last.worktree`, `runner.current_worktree`, `runner.last_worktree`,
    and `runner.task_worktrees`.
  - `events.jsonl` records `task_worktree_created` and includes the worktree
    record on `task_runner_tick` events.
- Cleanup policy is explicitly `remove_after_merge_if_clean`: later merge/cleanup
  phases may remove only clean worktrees after the PR is confirmed merged; blocked
  or dirty worktrees must be retained for debugging.

These records are runner/daemon state only and must not be copied into the PR body.
