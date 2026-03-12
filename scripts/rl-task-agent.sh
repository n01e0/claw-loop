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

clip_one_line() {
  local input="$1"
  printf '%s' "$input" \
    | tr '\n' ' ' \
    | tr '\r' ' ' \
    | sed -E 's/[[:space:]]+/ /g; s/^ //; s/ $//' \
    | cut -c1-240
}

infer_gh_repo() {
  local remote_url
  remote_url="$(git -C "$repo_path" config --get remote.origin.url 2>/dev/null || true)"
  if [[ "$remote_url" =~ github.com[:/]([^/]+/[^[:space:]]+) ]]; then
    local repo="${BASH_REMATCH[1]}"
    repo="${repo%.git}"
    echo "$repo"
    return 0
  fi
  return 1
}

resolve_gh_repo() {
  if [[ -n "${CLAW_GH_REPO:-}" ]]; then
    echo "$CLAW_GH_REPO"
    return 0
  fi
  if infer_gh_repo >/dev/null 2>&1; then
    infer_gh_repo
    return 0
  fi
  return 1
}

is_pr_merged() {
  local pr_url="$1"
  if [[ -z "$pr_url" ]]; then
    return 1
  fi
  local gh_repo
  gh_repo="$(resolve_gh_repo || true)"
  if [[ -z "$gh_repo" ]]; then
    return 1
  fi
  local out
  if ! out="$(gh pr view "$pr_url" --repo "$gh_repo" --json state,mergedAt --jq '.state + "|" + (.mergedAt // "")' 2>/dev/null)"; then
    return 1
  fi
  [[ "$out" == MERGED* ]]
}

AUTO_MERGE_LAST_ERROR=""

is_auto_merge_armed_or_merged() {
  local pr_url="$1"
  if [[ -z "$pr_url" ]]; then
    return 1
  fi
  local gh_repo
  gh_repo="$(resolve_gh_repo || true)"
  if [[ -z "$gh_repo" ]]; then
    return 1
  fi
  local armed
  armed="$(gh pr view "$pr_url" --repo "$gh_repo" --json state,autoMergeRequest --jq 'if ((.state // "") == "MERGED") then "1" elif .autoMergeRequest != null then "1" else "0" end' 2>/dev/null || echo "0")"
  [[ "$armed" == "1" ]]
}

enable_pr_auto_merge() {
  local pr_url="$1"
  AUTO_MERGE_LAST_ERROR=""

  if [[ -z "$pr_url" ]]; then
    AUTO_MERGE_LAST_ERROR="missing PR_URL"
    return 1
  fi

  local gh_repo
  gh_repo="$(resolve_gh_repo || true)"
  if [[ -z "$gh_repo" ]]; then
    AUTO_MERGE_LAST_ERROR="failed to resolve GitHub repo from remote.origin.url"
    return 1
  fi

  if is_pr_merged "$pr_url"; then
    return 0
  fi

  local merge_out merge_rc
  set +e
  merge_out="$(gh pr merge "$pr_url" --repo "$gh_repo" --auto --squash 2>&1)"
  merge_rc=$?
  set -e
  if [[ "$merge_rc" -ne 0 ]]; then
    AUTO_MERGE_LAST_ERROR="$(clip_one_line "$merge_out")"
    return 1
  fi

  if is_pr_merged "$pr_url"; then
    return 0
  fi

  if ! is_auto_merge_armed_or_merged "$pr_url"; then
    AUTO_MERGE_LAST_ERROR="auto-merge is not armed after request (check repository settings/permissions)"
    return 1
  fi

  return 0
}

pr_failed_checks_csv() {
  local pr_url="$1"
  if [[ -z "$pr_url" ]]; then
    return 0
  fi
  local gh_repo
  gh_repo="$(resolve_gh_repo || true)"
  if [[ -z "$gh_repo" ]]; then
    return 0
  fi
  gh pr view "$pr_url" --repo "$gh_repo" --json statusCheckRollup --jq '[.statusCheckRollup[]? | select((.status // "") == "COMPLETED" and ((.conclusion // "") == "FAILURE" or (.conclusion // "") == "TIMED_OUT" or (.conclusion // "") == "CANCELLED" or (.conclusion // "") == "ACTION_REQUIRED" or (.conclusion // "") == "STARTUP_FAILURE")) | ((.name // "unknown") + ":" + (.conclusion // "UNKNOWN"))] | join(", ")' 2>/dev/null || true
}

required_checks_missing_warning() {
  local pr_url="$1"
  if [[ -z "$pr_url" ]]; then
    return 0
  fi

  local gh_repo
  gh_repo="$(resolve_gh_repo || true)"
  if [[ -z "$gh_repo" ]]; then
    return 0
  fi

  local base_branch default_branch bp_required_count rulesets_json
  base_branch="$(gh pr view "$pr_url" --repo "$gh_repo" --json baseRefName --jq '.baseRefName' 2>/dev/null || true)"
  if [[ -z "$base_branch" || "$base_branch" == "null" ]]; then
    return 0
  fi

  default_branch="$(gh api "repos/${gh_repo}" --jq '.default_branch' 2>/dev/null || true)"
  bp_required_count="$(gh api "repos/${gh_repo}/branches/${base_branch}/protection" --jq '(.required_status_checks.contexts // []) | length' 2>/dev/null || echo 0)"

  if [[ "$bp_required_count" =~ ^[0-9]+$ ]] && (( bp_required_count > 0 )); then
    return 0
  fi

  rulesets_json="$(gh api "repos/${gh_repo}/rulesets" 2>/dev/null || echo '[]')"

  if REQ_CHECKS_ENFORCED="$(RULESETS_JSON="$rulesets_json" BASE_BRANCH="$base_branch" DEFAULT_BRANCH="$default_branch" python3 - <<'PY'
import json, os

rulesets_raw = os.environ.get('RULESETS_JSON', '[]')
base = os.environ.get('BASE_BRANCH', '')
default_branch = os.environ.get('DEFAULT_BRANCH', '')

try:
    rulesets = json.loads(rulesets_raw)
except Exception:
    print('0')
    raise SystemExit(0)

branch_ref = f"refs/heads/{base}"

def token_matches(token: str) -> bool:
    if token == '~ALL':
        return True
    if token == '~DEFAULT_BRANCH':
        return bool(default_branch) and base == default_branch
    return token == branch_ref or token == base

def applies_to_branch(rs: dict) -> bool:
    if rs.get('target') != 'branch':
        return False
    if rs.get('enforcement') != 'active':
        return False

    cond = rs.get('conditions') or {}
    ref_name = cond.get('ref_name') or {}
    include = ref_name.get('include') or []
    exclude = ref_name.get('exclude') or []

    include_match = True if not include else any(token_matches(t) for t in include)
    exclude_match = any(token_matches(t) for t in exclude)
    return include_match and not exclude_match

for rs in rulesets if isinstance(rulesets, list) else []:
    if not applies_to_branch(rs):
        continue
    rules = rs.get('rules') or []
    for rule in rules:
        if rule.get('type') != 'required_status_checks':
            continue
        params = rule.get('parameters') or {}
        checks = params.get('required_status_checks') or []
        if isinstance(checks, list) and len(checks) > 0:
            print('1')
            raise SystemExit(0)

print('0')
PY
)"; then
    if [[ "$REQ_CHECKS_ENFORCED" == "1" ]]; then
      return 0
    fi
  fi

  echo "required status checks are not enforced on ${gh_repo}#${base_branch}; merges may bypass CI"
}

if [[ -f "$state_file" ]]; then
  # shellcheck disable=SC1090
  source "$state_file"
  if [[ -n "${PR_URL:-}" ]]; then
    if ! enable_pr_auto_merge "$PR_URL"; then
      err="${AUTO_MERGE_LAST_ERROR:-unknown auto-merge error}"
      echo "TASK_BLOCKED: auto-merge enable failed for PR_URL=${PR_URL} error=${err}" >&2
      exit 2
    fi
    if is_pr_merged "$PR_URL"; then
      echo "TASK_DONE PR_URL=${PR_URL}"
      exit 0
    fi

    failed_checks="$(pr_failed_checks_csv "$PR_URL")"
    if [[ -n "$failed_checks" ]]; then
      echo "TASK_BLOCKED: CI failed for PR_URL=${PR_URL} checks=${failed_checks}" >&2
      exit 2
    fi

    checks_warning="$(required_checks_missing_warning "$PR_URL")"
    if [[ -n "$checks_warning" ]]; then
      echo "TASK_WAITING_MERGE PR_URL=${PR_URL} WARN_REQUIRED_CHECKS_MISSING=1"
      echo "$checks_warning"
    else
      echo "TASK_WAITING_MERGE PR_URL=${PR_URL}"
    fi
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
- If CI checks fail on that PR, fix the failure and push follow-up commits until checks pass.
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
  if ! enable_pr_auto_merge "$pr_url"; then
    err="${AUTO_MERGE_LAST_ERROR:-unknown auto-merge error}"
    echo "TASK_BLOCKED: auto-merge enable failed for PR_URL=${pr_url} error=${err}" >&2
    exit 2
  fi
  if is_pr_merged "$pr_url"; then
    exit 0
  fi

  failed_checks="$(pr_failed_checks_csv "$pr_url")"
  if [[ -n "$failed_checks" ]]; then
    echo "TASK_BLOCKED: CI failed for PR_URL=${pr_url} checks=${failed_checks}" >&2
    exit 2
  fi

  {
    echo "PR_URL='${pr_url}'"
  } >"$state_file"
  checks_warning="$(required_checks_missing_warning "$pr_url")"
  if [[ -n "$checks_warning" ]]; then
    echo "TASK_WAITING_MERGE PR_URL=${pr_url} WARN_REQUIRED_CHECKS_MISSING=1"
    echo "$checks_warning"
  else
    echo "TASK_WAITING_MERGE PR_URL=${pr_url}"
  fi
  exit 10
fi

if [[ "$first_line" == TASK_WAITING_MERGE* ]]; then
  if [[ -n "$pr_url" ]]; then
    {
      echo "PR_URL='${pr_url}'"
    } >"$state_file"
    if ! enable_pr_auto_merge "$pr_url"; then
      err="${AUTO_MERGE_LAST_ERROR:-unknown auto-merge error}"
      echo "TASK_BLOCKED: auto-merge enable failed for PR_URL=${pr_url} error=${err}" >&2
      exit 2
    fi
    failed_checks="$(pr_failed_checks_csv "$pr_url")"
    if [[ -n "$failed_checks" ]]; then
      echo "TASK_BLOCKED: CI failed for PR_URL=${pr_url} checks=${failed_checks}" >&2
      exit 2
    fi
    checks_warning="$(required_checks_missing_warning "$pr_url")"
    if [[ -n "$checks_warning" ]]; then
      echo "TASK_WAITING_MERGE PR_URL=${pr_url} WARN_REQUIRED_CHECKS_MISSING=1"
      echo "$checks_warning"
      exit 10
    fi
  fi
  exit 10
fi

exit 2
