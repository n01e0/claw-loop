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
  printf '%s\n' "$1" | awk '
    {
      sub(/\r$/, "")
      if (first_nonempty == "" && $0 ~ /[^[:space:]]/) {
        first_nonempty = $0
      }
      if ($0 ~ /^TASK_[A-Z_]+([[:space:]]|$)/) {
        print
        exit
      }
    }
    END {
      if (first_nonempty != "") {
        print first_nonempty
      }
    }
  '
}

parse_pr_url() {
  local line="$1"
  printf '%s\n' "$line" | sed -n 's/.*PR_URL=\([^ ]\+\).*/\1/p' | head -n1
}

task_plan_hash() {
  local task_file="$1"
  python3 - "$task_file" <<'PY'
import hashlib
import pathlib
import re
import sys
path = pathlib.Path(sys.argv[1])
content = path.read_text()
entries = []
for raw in content.splitlines():
    m = re.match(r'^- \[( |x)\] ([^:]+):\s*(.+)$', raw)
    if not m:
        continue
    entries.append((m.group(2), m.group(3)))
sha = hashlib.sha256()
for idx, (task_id, text) in enumerate(entries):
    sha.update(str(idx).encode())
    sha.update(b'\t')
    sha.update(task_id.encode())
    sha.update(b'\t')
    sha.update(text.encode())
    sha.update(b'\n')
print(sha.hexdigest())
PY
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
AUTO_MERGE_MODE="auto"
MANUAL_MERGE_LAST_ERROR=""

is_auto_merge_unavailable_error() {
  local lowered
  lowered="$(printf '%s' "$1" | tr '[:upper:]' '[:lower:]')"
  [[ "$lowered" == *"auto merge is not allowed"*     || "$lowered" == *"auto-merge is not allowed"*     || "$lowered" == *"pull request auto merge is not allowed for this repository"*     || "$lowered" == *"auto merge is disabled"*     || "$lowered" == *"auto-merge is disabled"*     || "$lowered" == *"repository does not allow auto-merge"*     || "$lowered" == *"repository has disabled auto-merge"* ]]
}

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
  AUTO_MERGE_MODE="auto"

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
  merge_out="$(gh pr merge "$pr_url" --repo "$gh_repo" --auto --squash --delete-branch 2>&1)"
  merge_rc=$?
  set -e
  if [[ "$merge_rc" -ne 0 ]]; then
    AUTO_MERGE_LAST_ERROR="$(clip_one_line "$merge_out")"
    if is_auto_merge_unavailable_error "$AUTO_MERGE_LAST_ERROR"; then
      AUTO_MERGE_MODE="manual"
      AUTO_MERGE_LAST_ERROR=""
      return 0
    fi
    return 1
  fi

  if is_pr_merged "$pr_url"; then
    return 0
  fi

  if ! is_auto_merge_armed_or_merged "$pr_url"; then
    AUTO_MERGE_MODE="manual"
    return 0
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

pr_pending_checks_csv() {
  local pr_url="$1"
  if [[ -z "$pr_url" ]]; then
    return 0
  fi
  local gh_repo
  gh_repo="$(resolve_gh_repo || true)"
  if [[ -z "$gh_repo" ]]; then
    return 0
  fi
  gh pr view "$pr_url" --repo "$gh_repo" --json statusCheckRollup --jq '[.statusCheckRollup[]? | select((.status // "") != "COMPLETED" and (.status // "") != "") | (.name // "unknown")] | join(", ")' 2>/dev/null || true
}

pr_successful_checks_count() {
  local pr_url="$1"
  if [[ -z "$pr_url" ]]; then
    echo 0
    return 0
  fi
  local gh_repo
  gh_repo="$(resolve_gh_repo || true)"
  if [[ -z "$gh_repo" ]]; then
    echo 0
    return 0
  fi
  gh pr view "$pr_url" --repo "$gh_repo" --json statusCheckRollup --jq '[.statusCheckRollup[]? | select((.status // "") == "COMPLETED" and ((.conclusion // "") == "SUCCESS" or (.conclusion // "") == "NEUTRAL" or (.conclusion // "") == "SKIPPED"))] | length' 2>/dev/null || echo 0
}

manual_merge_pr() {
  local pr_url="$1"
  MANUAL_MERGE_LAST_ERROR=""

  if [[ -z "$pr_url" ]]; then
    MANUAL_MERGE_LAST_ERROR="missing PR_URL"
    return 1
  fi

  local gh_repo
  gh_repo="$(resolve_gh_repo || true)"
  if [[ -z "$gh_repo" ]]; then
    MANUAL_MERGE_LAST_ERROR="failed to resolve GitHub repo from remote.origin.url"
    return 1
  fi

  local merge_out merge_rc
  set +e
  merge_out="$(gh pr merge "$pr_url" --repo "$gh_repo" --squash --delete-branch 2>&1)"
  merge_rc=$?
  set -e
  if [[ "$merge_rc" -ne 0 ]]; then
    MANUAL_MERGE_LAST_ERROR="$(clip_one_line "$merge_out")"
    return 1
  fi

  return 0
}

handle_waiting_pr() {
  local pr_url="$1"

  if ! enable_pr_auto_merge "$pr_url"; then
    local err
    err="${AUTO_MERGE_LAST_ERROR:-unknown auto-merge error}"
    echo "TASK_BLOCKED: auto-merge enable failed for PR_URL=${pr_url} error=${err}" >&2
    return 2
  fi

  if is_pr_merged "$pr_url"; then
    echo "TASK_DONE PR_URL=${pr_url}"
    return 0
  fi

  local failed_checks
  failed_checks="$(pr_failed_checks_csv "$pr_url")"
  if [[ -n "$failed_checks" ]]; then
    echo "TASK_BLOCKED: CI failed for PR_URL=${pr_url} checks=${failed_checks}" >&2
    return 2
  fi

  if [[ "$AUTO_MERGE_MODE" == "manual" ]]; then
    local pending_checks successful_checks_count
    pending_checks="$(pr_pending_checks_csv "$pr_url")"
    successful_checks_count="$(pr_successful_checks_count "$pr_url")"
    if [[ -z "$pending_checks" && "$successful_checks_count" =~ ^[0-9]+$ ]] && (( successful_checks_count > 0 )); then
      if ! manual_merge_pr "$pr_url"; then
        local err
        err="${MANUAL_MERGE_LAST_ERROR:-unknown manual merge error}"
        echo "TASK_BLOCKED: manual merge failed for PR_URL=${pr_url} error=${err}" >&2
        return 2
      fi
      if is_pr_merged "$pr_url"; then
        echo "TASK_DONE PR_URL=${pr_url}"
        return 0
      fi
    fi
  fi

  local checks_warning
  checks_warning="$(required_checks_missing_warning "$pr_url")"
  if [[ -n "$checks_warning" ]]; then
    echo "TASK_WAITING_MERGE PR_URL=${pr_url} WARN_REQUIRED_CHECKS_MISSING=1"
    echo "$checks_warning"
  else
    echo "TASK_WAITING_MERGE PR_URL=${pr_url}"
  fi
  return 10
}

fetch_detailed_rulesets_json() {
  local gh_repo="$1"
  local ruleset_ids details_json

  ruleset_ids="$(gh api "repos/${gh_repo}/rulesets" --jq '.[]?.id' 2>/dev/null || true)"
  if [[ -z "$ruleset_ids" ]]; then
    echo '[]'
    return 0
  fi

  details_json="$({
    while IFS= read -r ruleset_id; do
      [[ -n "$ruleset_id" ]] || continue
      gh api "repos/${gh_repo}/rulesets/${ruleset_id}" 2>/dev/null || true
      printf '\n'
    done <<<"$ruleset_ids"
  } | python3 -c 'import json,sys; rows=[]
for raw in sys.stdin:
    raw=raw.strip()
    if not raw:
        continue
    try:
        rows.append(json.loads(raw))
    except Exception:
        pass
print(json.dumps(rows, ensure_ascii=False))')"

  if [[ -z "$details_json" ]]; then
    echo '[]'
  else
    printf '%s\n' "$details_json"
  fi
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

  rulesets_json="$(fetch_detailed_rulesets_json "$gh_repo")"

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
    handle_waiting_pr "$PR_URL"
    exit $?
  fi
fi

read -r -d '' PROMPT <<'EOF' || true
You are executing one approved dogfood task for claw-loop.

Hard requirements:
- Work only in the specified repository path.
- Complete exactly the specified task.
- Do not edit the task file; treat it as daemon-owned planning state. Only claw-loopd may mutate task checkboxes / recovery entries.
- Commit/push your changes.
- Create PR for this task. Prefer enabling auto-merge when the repository supports it.
- If CI checks fail on that PR, fix the failure and push follow-up commits until checks pass.
- If the repository does not support auto-merge, return waiting with PR URL after pushing; claw-loopd will watch CI and merge when it turns green.
- If merge is not complete yet, return waiting with PR URL.
- If task is fully complete and PR merged, first line MUST be:
  TASK_DONE PR_URL=<url>
- If PR exists but merge is pending, first line MUST be:
  TASK_WAITING_MERGE PR_URL=<url>
- If work is waiting on an upstream task or PR dependency, first line MUST be:
  TASK_WAITING_DEPENDENCY [TASK_ID=<id>] DEPENDS_ON_TASK=<id>
  or
  TASK_WAITING_DEPENDENCY [TASK_ID=<id>] DEPENDS_ON_PR_URL=<absolute-url>
  (at least one of DEPENDS_ON_TASK / DEPENDS_ON_PR_URL is required; include TASK_ID when available)
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

task_file_hash_before=""
if [[ -n "${CLAW_TASK_FILE:-}" && -f "${CLAW_TASK_FILE}" ]]; then
  task_file_hash_before="$(task_plan_hash "${CLAW_TASK_FILE}")"
fi

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
text="$(printf '%s' "$json_out" | jq -r 'if (.payloads | type) == "array" then [(.payloads[]? | .text? // empty)] | join("\n") elif (.text? // null) != null then .text else "" end')"
printf '%s\n' "$text"

if [[ -n "$task_file_hash_before" && -f "${CLAW_TASK_FILE:-}" ]]; then
  task_file_hash_after="$(task_plan_hash "${CLAW_TASK_FILE}")"
  if [[ "$task_file_hash_after" != "$task_file_hash_before" ]]; then
    echo "TASK_BLOCKED: task file was modified during runner execution (before=${task_file_hash_before} after=${task_file_hash_after}); tasklist edits are not allowed" >&2
    exit 2
  fi
fi

first_line="$(get_first_line "$text")"
pr_url="$(parse_pr_url "$first_line")"

if [[ "$first_line" == TASK_DONE* ]]; then
  if [[ -z "$pr_url" ]]; then
    echo "TASK_BLOCKED: TASK_DONE without PR_URL" >&2
    exit 2
  fi
  {
    echo "PR_URL='${pr_url}'"
  } >"$state_file"
  handle_waiting_pr "$pr_url"
  exit $?
fi

if [[ "$first_line" == TASK_WAITING_MERGE* ]]; then
  if [[ -n "$pr_url" ]]; then
    {
      echo "PR_URL='${pr_url}'"
    } >"$state_file"
    handle_waiting_pr "$pr_url"
    exit $?
  fi
  exit 10
fi

if [[ "$first_line" == TASK_WAITING_DEPENDENCY* ]]; then
  : >"$state_file"
  exit 10
fi

exit 2
