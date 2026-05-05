# ACPX task result contract

ACPX-backed task runners keep the existing first-line `TASK_*` contract as the daemon state signal. Structured details for automation are emitted after that first line in an optional JSON payload marked `ACPX_TASK_RESULT_JSON`.

## Required shape

The first non-empty line is still one of the runner contract lines:

```text
TASK_DONE PR_URL=<absolute-url>
TASK_WAITING_MERGE PR_URL=<absolute-url>
TASK_WAITING_DEPENDENCY [TASK_ID=<id>] DEPENDS_ON_TASK=<id>
TASK_WAITING_DEPENDENCY [TASK_ID=<id>] DEPENDS_ON_PR_URL=<absolute-url>
TASK_BLOCKED: <reason>
```

After the first line, ACPX runners may include human-readable text and exactly one structured payload using either inline JSON:

```text
ACPX_TASK_RESULT_JSON: {"summary":"...","verification":["cargo test"],"notes":[],"pushed_branch":"apb-3","pr":{"url":"https://github.com/n01e0/claw-loop/pull/123"}}
```

or a fenced block:

````text
```ACPX_TASK_RESULT_JSON
{
  "summary": "Defined ACPX runner result parsing.",
  "verification": ["cargo test"],
  "notes": ["Auto-merge enabled; CI pending."],
  "pushed_branch": "apb-3-acpx-result-contract",
  "pr": {
    "url": "https://github.com/n01e0/claw-loop/pull/123",
    "number": 123,
    "title": "Define ACPX task result contract",
    "merge_state": "pending",
    "auto_merge": true
  }
}
```
````

## Fields

- `summary` (string, required): concise task result summary for runner state and PR body generation.
- `verification` (array of strings, default empty): commands or checks run by the task agent, including relevant pass/fail notes.
- `notes` (array of strings, default empty): extra operator-facing context, follow-ups, or limitations.
- `pushed_branch` (string, required): branch pushed for this task. Use an empty string only when the result is blocked before branch creation.
- `pr` (object, optional): PR metadata when a PR exists.
  - `url` (string, optional): absolute PR URL.
  - `number` (number, optional): PR number.
  - `title` (string, optional): PR title.
  - `merge_state` (string, optional): observed merge/CI state such as `pending`, `merged`, `dirty`, or provider-native state.
  - `auto_merge` (boolean, optional): whether auto-merge was enabled or already active.

## Runner behavior

- The first-line `TASK_*` line remains authoritative for daemon state transitions.
- `ACPX_TASK_RESULT_JSON` is additive metadata. Missing metadata must not make an otherwise valid legacy result malformed.
- If present but malformed, an ACPX runner may classify the result as blocked before handing it to daemon state, because downstream automation cannot safely consume it.
- Runner parsers should ignore surrounding human text and read only the marked payload.
