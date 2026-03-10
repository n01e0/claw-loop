#!/usr/bin/env bash
set -euo pipefail

if [[ -z "${CLAW_TASK_ID:-}" || -z "${CLAW_TASK_TEXT:-}" ]]; then
  echo "TASK_BLOCKED: missing CLAW_TASK_ID/CLAW_TASK_TEXT" >&2
  exit 2
fi

repo_path="$(pwd)"
run_id="${CLAW_RUN_ID:-local}"
agent_id="${CLAW_AGENT_ID:-main}"
agent_session_id="rl-${run_id}-${CLAW_TASK_ID}"
agent_timeout_sec="${CLAW_AGENT_TIMEOUT_SEC:-1800}"

state_root="${repo_path}/.ralph/runner-agent-state/${run_id}"
mkdir -p "$state_root"
state_file="${state_root}/${CLAW_TASK_ID}.env"

extract_json_object() {
  RAW_OUT="$1" python3 - <<'PY'
import json, os, sys
s = os.environ.get("RAW_OUT", "")
decoder = json.JSONDecoder()
for i, ch in enumerate(s):
    if ch != '{':
        continue
    try:
        obj, _ = decoder.raw_decode(s[i:])
    except Exception:
        continue
    print(json.dumps(obj, ensure_ascii=False))
    sys.exit(0)
print("parse-error: no json object found in openclaw output", file=sys.stderr)
sys.exit(1)
PY
}

get_first_line() {
  printf '%s' "$1" | head -n1 | tr -d '\r'
}

parse_pr_url() {
  local line="$1"
  printf '%s\n' "$line" | sed -n 's/.*PR_URL=\([^ ]\+\).*/\1/p' | head -n1
}

is_pr_merged() {
  local pr_url="$1"
  if [[ -z "$pr_url" ]]; then
    return 1
  fi
  local out
  if ! out="$(gh pr view "$pr_url" --repo n01e0/claw-loop --json state,mergedAt --jq '.state + "|" + (.mergedAt // "")' 2>/dev/null)"; then
    return 1
  fi
  [[ "$out" == MERGED* ]]
}

if [[ -f "$state_file" ]]; then
  # shellcheck disable=SC1090
  source "$state_file"
  if [[ -n "${PR_URL:-}" ]]; then
    if is_pr_merged "$PR_URL"; then
      echo "TASK_DONE PR_URL=${PR_URL}"
      exit 0
    fi

    echo "TASK_WAITING_MERGE PR_URL=${PR_URL}"
    exit 10
  fi
fi

read -r -d '' PROMPT <<'EOF' || true
You are executing one approved dogfood task for claw-loop.

Hard requirements:
- Work only in the specified repository path.
- Complete exactly the specified task.
- Commit/push your changes.
- Create PR for this task and enable auto-merge.
- If auto-merge is not complete yet, return waiting with PR URL.
- If task is fully complete and PR merged, first line MUST be:
  TASK_DONE PR_URL=<url>
- If PR exists but merge is pending, first line MUST be:
  TASK_WAITING_MERGE PR_URL=<url>
- If not complete / blocked, first line MUST be:
  TASK_BLOCKED: <reason>
- After the first line, include a short summary.
EOF

PROMPT+=$'\n\n'
PROMPT+="Repo: ${repo_path}"$'\n'
PROMPT+="Task ID: ${CLAW_TASK_ID}"$'\n'
PROMPT+="Task: ${CLAW_TASK_TEXT}"$'\n'
PROMPT+="Task file: ${CLAW_TASK_FILE:-}"$'\n'
PROMPT+="Run ID: ${run_id}"$'\n'

set +e
raw_out="$(openclaw agent --local --agent "$agent_id" --session-id "$agent_session_id" --timeout "$agent_timeout_sec" --message "$PROMPT" --json 2>&1)"
rc=$?
set -e

if [[ "$rc" -ne 0 ]]; then
  if printf '%s' "$raw_out" | grep -qi "session file locked"; then
    echo "TASK_WAITING_AGENT_LOCK"
    exit 10
  fi
  echo "TASK_BLOCKED: openclaw agent command failed (rc=$rc)" >&2
  printf '%s\n' "$raw_out" >&2
  exit 2
fi

json_out="$(extract_json_object "$raw_out")"
text="$(printf '%s' "$json_out" | jq -r '[.payloads[].text // ""] | join("\n")')"
printf '%s\n' "$text"

first_line="$(get_first_line "$text")"
pr_url="$(parse_pr_url "$first_line")"

if [[ "$first_line" == TASK_DONE* ]]; then
  if [[ -z "$pr_url" ]]; then
    echo "TASK_BLOCKED: TASK_DONE without PR_URL" >&2
    exit 2
  fi
  if is_pr_merged "$pr_url"; then
    exit 0
  fi

  {
    echo "PR_URL='${pr_url}'"
  } >"$state_file"
  echo "TASK_WAITING_MERGE PR_URL=${pr_url}"
  exit 10
fi

if [[ "$first_line" == TASK_WAITING_MERGE* ]]; then
  if [[ -n "$pr_url" ]]; then
    {
      echo "PR_URL='${pr_url}'"
    } >"$state_file"
  fi
  exit 10
fi

exit 2
