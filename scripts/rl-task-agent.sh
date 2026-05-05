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


def extract_text(obj):
    if not isinstance(obj, dict):
        return ""

    payloads = obj.get("payloads")
    if isinstance(payloads, list):
        texts = [
            item.get("text", "")
            for item in payloads
            if isinstance(item, dict) and isinstance(item.get("text"), str) and item.get("text")
        ]
        if texts:
            return "\n".join(texts)

    text = obj.get("text")
    if isinstance(text, str) and text:
        return text

    for key in ("message",):
        container = obj.get(key)
        if isinstance(container, dict):
            content = container.get("content")
            if isinstance(content, list):
                texts = [
                    item.get("text", "")
                    for item in content
                    if isinstance(item, dict) and isinstance(item.get("text"), str) and item.get("text")
                ]
                if texts:
                    return "\n".join(texts)

    content = obj.get("content")
    if isinstance(content, list):
        texts = [
            item.get("text", "")
            for item in content
            if isinstance(item, dict) and isinstance(item.get("text"), str) and item.get("text")
        ]
        if texts:
            return "\n".join(texts)

    return ""


s = os.environ.get("RAW_OUT", "")
decoder = json.JSONDecoder()
selected = None
for i, ch in enumerate(s):
    if ch != '{':
        continue
    try:
        obj, _ = decoder.raw_decode(s[i:])
    except Exception:
        continue
    if selected is None:
        selected = obj
    if extract_text(obj).strip():
        selected = obj

if selected is not None:
    print(json.dumps(selected, ensure_ascii=False))
    sys.exit(0)

print("parse-error: no json object found in openclaw output", file=sys.stderr)
sys.exit(1)
PY
}

extract_agent_text() {
  JSON_OUT="$1" python3 - <<'PY'
import json, os, sys

obj = json.loads(os.environ["JSON_OUT"])

payloads = obj.get("payloads")
if isinstance(payloads, list):
    texts = [
        item.get("text", "")
        for item in payloads
        if isinstance(item, dict) and isinstance(item.get("text"), str)
    ]
    if texts:
        print("\n".join(texts))
        sys.exit(0)

text = obj.get("text")
if isinstance(text, str):
    print(text)
    sys.exit(0)

message = obj.get("message")
if isinstance(message, dict):
    content = message.get("content")
    if isinstance(content, list):
        texts = [
            item.get("text", "")
            for item in content
            if isinstance(item, dict) and isinstance(item.get("text"), str)
        ]
        if texts:
            print("\n".join(texts))
            sys.exit(0)

content = obj.get("content")
if isinstance(content, list):
    texts = [
        item.get("text", "")
        for item in content
        if isinstance(item, dict) and isinstance(item.get("text"), str)
    ]
    if texts:
        print("\n".join(texts))
        sys.exit(0)

print("")
PY
}

get_first_line() {
  printf '%s
' "$1" | awk '
    {
      sub(/\r$/, "")
      if (first_nonempty == "" && $0 ~ /[^[:space:]]/) {
        first_nonempty = $0
      }
    }
    END {
      print first_nonempty
    }
  '
}

resolve_session_jsonl() {
  local session_id="$1"
  local agent_id="$2"
  local base="${OPENCLAW_HOME:-$HOME/.openclaw}/agents/${agent_id}/sessions"
  if [[ -f "$base/${session_id}.jsonl" ]]; then
    printf '%s\n' "$base/${session_id}.jsonl"
    return 0
  fi
  local candidate
  candidate="$(find "$base" -maxdepth 1 -type f -name "${session_id}*.jsonl" 2>/dev/null | head -n1 || true)"
  if [[ -n "$candidate" ]]; then
    printf '%s\n' "$candidate"
    return 0
  fi
  return 1
}

extract_session_signal() {
  local session_jsonl="$1"
  python3 - "$session_jsonl" <<'PY2' 2>/dev/null
import json, sys
path = sys.argv[1]
with open(path, 'r') as fh:
    rows = [json.loads(line) for line in fh if line.strip()]

def assistant_text(row):
    msg = row.get('message') or {}
    if msg.get('role') != 'assistant':
        return None
    content = msg.get('content') or []
    parts = []
    if isinstance(content, list):
        for item in content:
            if isinstance(item, dict) and isinstance(item.get('text'), str):
                parts.append(item['text'])
            elif isinstance(item, str):
                parts.append(item)
    elif isinstance(content, str):
        parts.append(content)
    text = '\n'.join(p for p in parts if p).strip()
    return text or None

for row in reversed(rows):
    text = assistant_text(row)
    if not text:
        continue
    first = text.splitlines()[0].strip()
    if first.startswith(('TASK_DONE', 'TASK_WAITING_MERGE', 'TASK_WAITING_DEPENDENCY', 'TASK_WAITING_AGENT_LOCK', 'TASK_BLOCKED')):
        print(first)
        raise SystemExit(0)

for row in reversed(rows):
    if row.get('customType') == 'openclaw:prompt-error':
        err = ((row.get('data') or {}).get('error') or '').strip()
        if err:
            print(f'PROMPT_ERROR: {err}')
            raise SystemExit(0)

for row in reversed(rows):
    msg = row.get('message') or {}
    err = (msg.get('errorMessage') or '').strip()
    if err:
        print(f'ABORTED: {err}')
        raise SystemExit(0)
PY2
}

session_signal_for_failure() {
  local session_id="$1"
  local agent_id="$2"
  local jsonl
  jsonl="$(resolve_session_jsonl "$session_id" "$agent_id" || true)"
  if [[ -z "$jsonl" ]]; then
    return 0
  fi
  extract_session_signal "$jsonl"
}

is_retryable_session_signal() {
  local signal="$1"
  [[ "$signal" == PROMPT_ERROR:*request\ timed\ out* ||      "$signal" == PROMPT_ERROR:*timed\ out* ||      "$signal" == ABORTED:*operation\ was\ aborted* ]]
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

validate_pr_body_readback() {
  local pr_url="$1"
  local body_file="$2"
  local gh_repo="$3"
  local actual expected hazard

  expected="$(cat "$body_file")"
  actual="$(gh pr view "$pr_url" --repo "$gh_repo" --json body --jq '.body // ""' 2>&1)" || {
    echo "TASK_BLOCKED: gh pr view --json body failed error=$(clip_one_line "$actual")" >&2
    return 2
  }

  detect_pr_body_hazard() {
    python3 - "$1" <<'PY'
import pathlib, re, sys
body = pathlib.Path(sys.argv[1]).read_text()
terms = [
    "TASK_DONE", "TASK_WAITING_MERGE", "TASK_WAITING_DEPENDENCY", "TASK_BLOCKED",
    "auto-merge", "auto merge", "waiting for CI", "waiting on CI", "worktree",
    "runner", "daemon", "agent session", "session id", "prompt file", "body file",
    "temp file", "cleanup", "branch deletion", "I pushed", "I created a branch", "I opened the PR",
]
checks = []
if "$##" in body:
    checks.append("contains $##")
if "\\n" in body:
    checks.append("contains literal \\n")
for line in body.splitlines():
    if re.match(r"^\s*#+\s*[-*]\s", line) or re.match(r"^\s*[-*]\s*#+\s", line):
        checks.append("contains broken heading")
        break
if sum(1 for line in body.splitlines() if line.strip().startswith("```")) % 2:
    checks.append("contains unclosed code fence")
lower = body.lower()
for term in terms:
    if term.lower() in lower:
        checks.append(f"contains execution-report vocabulary: {term}")
        break
if checks:
    print("; ".join(checks))
    sys.exit(1)
PY
  }

  local tmp_actual
  if [[ "$actual" != "$expected" ]]; then
    local edit_out
    edit_out="$(gh pr edit "$pr_url" --repo "$gh_repo" --body-file "$body_file" 2>&1)" || {
      echo "TASK_BLOCKED: PR body read-back mismatch and auto-fix failed for PR_URL=${pr_url} error=$(clip_one_line "$edit_out")" >&2
      return 2
    }
    actual="$(gh pr view "$pr_url" --repo "$gh_repo" --json body --jq '.body // ""' 2>&1)" || {
      echo "TASK_BLOCKED: gh pr view --json body after auto-fix failed error=$(clip_one_line "$actual")" >&2
      return 2
    }
    if [[ "$actual" != "$expected" ]]; then
      echo "TASK_BLOCKED: PR body read-back mismatch persisted after auto-fix for PR_URL=${pr_url}" >&2
      return 2
    fi
  fi

  tmp_actual="$(mktemp)"
  printf '%s' "$actual" > "$tmp_actual"
  if ! hazard="$(detect_pr_body_hazard "$tmp_actual" 2>&1)"; then
    rm -f -- "$tmp_actual"
    echo "TASK_BLOCKED: PR body read-back validation failed for PR_URL=${pr_url}: $(clip_one_line "$hazard")" >&2
    return 2
  fi
  rm -f -- "$tmp_actual"
}

create_runner_owned_pr() {
  local text="$1"
  local first_line="$2"
  local body_dir body_file title branch gh_repo pr_url

  gh_repo="$(resolve_gh_repo || true)"
  if [[ -z "$gh_repo" ]]; then
    echo "TASK_BLOCKED: failed to resolve GitHub repo from remote.origin.url" >&2
    return 2
  fi

  body_dir="${CLAW_PR_BODY_DIR:-${state_root}/pr-bodies}"
  mkdir -p "$body_dir"
  body_file="$(TEXT="$text" BODY_DIR="$body_dir" python3 - <<'PY'
import json, os, pathlib, re, sys, uuid

text = os.environ.get("TEXT", "")
marker = "ACPX_TASK_RESULT_JSON"
result = None
inline = re.search(rf"^{marker}:\s*(\{{.*\}})\s*$", text, re.M)
if inline:
    result = json.loads(inline.group(1))
else:
    fenced = re.search(rf"^```{marker}\s*\n(.*?)\n```\s*$", text, re.M | re.S)
    if fenced:
        result = json.loads(fenced.group(1))
if not isinstance(result, dict):
    print("missing ACPX_TASK_RESULT_JSON for runner-owned PR creation", file=sys.stderr)
    sys.exit(3)
summary = str(result.get("summary") or "").strip()
verification = [str(v).strip() for v in result.get("verification") or [] if str(v).strip()]
notes = [str(v).strip() for v in result.get("notes") or [] if str(v).strip()]
if not summary or not verification:
    print("ACPX_TASK_RESULT_JSON summary and verification are required", file=sys.stderr)
    sys.exit(3)
for value in [summary, *verification, *notes]:
    lowered = value.lower()
    for term in ("task_done", "task_waiting_merge", "task_blocked", "runner", "daemon", "worktree", "body file", "cleanup"):
        if term in lowered:
            print(f"PR body content contains execution-report vocabulary: {term}", file=sys.stderr)
            sys.exit(3)
body = "## Summary\n- " + summary.replace("\n", "\n- ") + "\n\n## Verification\n"
body += "".join(f"- {v}\n" for v in verification)
if notes:
    body += "\n## Notes\n" + "".join(f"- {n}\n" for n in notes)
path = pathlib.Path(os.environ["BODY_DIR"]) / f"runner-pr-body-{uuid.uuid4()}.md"
path.write_text(body)
print(path)
PY
)" || {
    echo "TASK_BLOCKED: failed to create runner-owned PR body file" >&2
    return 2
  }

  cleanup_pr_body_file() {
    [[ -z "${body_file:-}" ]] || rm -f -- "$body_file"
  }
  trap cleanup_pr_body_file RETURN

  branch="$(TEXT="$text" python3 - <<'PY'
import json, os, re
text = os.environ.get("TEXT", "")
marker = "ACPX_TASK_RESULT_JSON"
raw = None
m = re.search(rf"^{marker}:\s*(\{{.*\}})\s*$", text, re.M)
if m:
    raw = m.group(1)
else:
    m = re.search(rf"^```{marker}\s*\n(.*?)\n```\s*$", text, re.M | re.S)
    if m:
        raw = m.group(1)
if raw:
    obj = json.loads(raw)
    print((obj.get("pushed_branch") or "").strip())
PY
)"
  branch="${branch:-$(git -C "$repo_path" branch --show-current)}"
  title="${CLAW_PR_TITLE:-${CLAW_TASK_ID}: ${CLAW_TASK_TEXT}}"

  pr_url="$(gh pr create --repo "$gh_repo" --head "$branch" --title "$title" --body-file "$body_file" 2>&1)" || {
    local err
    err="$(clip_one_line "$pr_url")"
    echo "TASK_BLOCKED: gh pr create --body-file failed error=${err}" >&2
    return 2
  }
  pr_url="$(printf '%s\n' "$pr_url" | grep -Eo 'https://github.com/[^[:space:]]+/pull/[0-9]+' | head -n1)"
  if [[ -z "$pr_url" ]]; then
    echo "TASK_BLOCKED: gh pr create did not return a PR URL" >&2
    return 2
  fi
  validate_pr_body_readback "$pr_url" "$body_file" "$gh_repo" || return 2
  printf '%s\n' "$pr_url"
}

pr_state_summary() {
  local pr_url="$1"
  if [[ -z "$pr_url" ]]; then
    return 1
  fi
  local gh_repo
  gh_repo="$(resolve_gh_repo || true)"
  if [[ -z "$gh_repo" ]]; then
    return 1
  fi
  gh pr view "$pr_url" --repo "$gh_repo" --json state,mergedAt --jq '.state + "|" + (.mergedAt // "")' 2>/dev/null
}

is_pr_merged() {
  local pr_url="$1"
  local out
  if ! out="$(pr_state_summary "$pr_url")"; then
    return 1
  fi
  [[ "$out" == MERGED* ]]
}

pr_exists() {
  local pr_url="$1"
  local out
  if ! out="$(pr_state_summary "$pr_url")"; then
    return 1
  fi
  [[ -n "$out" ]]
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
    if is_pr_merged "$pr_url"; then
      AUTO_MERGE_LAST_ERROR=""
      return 0
    fi
    if pr_exists "$pr_url"; then
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

if [[ "${CLAW_BACKLOG_STATUS:-}" == "backlog" ]]; then
  task_kind="${CLAW_TASK_KIND:-unknown}"
  if [[ "$task_kind" != "repair" ]]; then
    backlog_count="${CLAW_BACKLOG_COUNT:-unknown}"
    backlog_summary="$(clip_one_line "${CLAW_BACKLOG_SUMMARY:-backlog active}")"
    echo "TASK_BLOCKED: failure-first backlog gate active for task ${CLAW_TASK_ID}; backlog_count=${backlog_count}; task_kind=${task_kind}; repair tasks only; detector=${backlog_summary}" >&2
    exit 2
  fi
fi

read -r -d '' PROMPT <<'EOF' || true
You are executing one approved dogfood task for claw-loop.

Hard requirements:
- Work only in the specified repository path.
- Complete exactly the specified task.
- Do not edit the task file; treat it as daemon-owned planning state. Only claw-loopd may mutate task checkboxes / recovery entries.
- Commit/push your changes.
- Do not create the PR yourself unless explicitly instructed with a runner-generated body file path. By default, emit ACPX_TASK_RESULT_JSON with summary / verification / notes / pushed_branch and let the runner create the PR with gh pr create --body-file.
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
- If the task cannot be shipped as an isolated green PR because it must land after an upstream phase/stacked task or PR, emit TASK_WAITING_DEPENDENCY with DEPENDS_ON_TASK and/or DEPENDS_ON_PR_URL when known.
- If you know phase/stacked sequencing is required but do not know the dependency target, emit TASK_BLOCKED explaining that a phase/stacked dependency is required.
- If not complete / blocked, first line MUST be:
  TASK_BLOCKED: <reason>
- Do not emit any preamble before the required `TASK_*` line.
- Do not narrate progress updates, tool calls, delegation, or sub-agent handoffs (for example: "I'm handing this to a coding agent...").
- Do not return `NO_REPLY` or `HEARTBEAT_OK` for this task runner contract.
- If you need background work or a sub-agent, wait until it is finished and then respond once with the final `TASK_*` line.
- After the first line, include a short summary.
EOF

PROMPT+=$'\n\n'
PROMPT+="Repo: ${repo_path}"$'\n'
PROMPT+="Task ID: ${CLAW_TASK_ID}"$'\n'
PROMPT+="Task: ${CLAW_TASK_TEXT}"$'\n'
PROMPT+="Task file: ${CLAW_TASK_FILE:-}"$'\n'
PROMPT+="Run ID: ${run_id}"$'\n'
if [[ -n "${CLAW_TASK_KIND:-}" ]]; then
  PROMPT+="Task kind: ${CLAW_TASK_KIND}"$'\n'
fi
if [[ -n "${CLAW_BACKLOG_STATUS:-}" ]]; then
  PROMPT+="Backlog detector status: ${CLAW_BACKLOG_STATUS}"$'\n'
  PROMPT+="Backlog count: ${CLAW_BACKLOG_COUNT:-0}"$'\n'
  PROMPT+="Backlog summary: ${CLAW_BACKLOG_SUMMARY:-}"$'\n'
  PROMPT+="Backlog updated at: ${CLAW_BACKLOG_UPDATED_AT:-}"$'\n'
  if [[ "${CLAW_BACKLOG_STATUS}" == "backlog" ]]; then
    PROMPT+="Failure-first gate: backlog is active. Repair-scoped tasks may proceed; do not ship a standalone feature-style PR while backlog_count>0."$'\n'
  fi
fi

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
  session_signal="$(session_signal_for_failure "$agent_session_id" "$agent_id" || true)"
  if [[ "$session_signal" == TASK_DONE* || "$session_signal" == TASK_WAITING_MERGE* || "$session_signal" == TASK_WAITING_DEPENDENCY* || "$session_signal" == TASK_WAITING_AGENT_LOCK* || "$session_signal" == TASK_BLOCKED* ]]; then
    raw_out="$session_signal"
  elif is_retryable_session_signal "$session_signal"; then
    echo "TASK_WAITING_DEPENDENCY: subagent request timed out for ${agent_session_id}; retry task"
    exit 10
  else
    echo "TASK_BLOCKED: openclaw agent command failed (rc=$rc)" >&2
    printf '%s\n' "$raw_out" >&2
    if [[ -n "$session_signal" ]]; then
      printf '%s\n' "$session_signal" >&2
    fi
    exit 2
  fi
fi

if [[ "$raw_out" == TASK_DONE* || "$raw_out" == TASK_WAITING_MERGE* || "$raw_out" == TASK_WAITING_DEPENDENCY* || "$raw_out" == TASK_WAITING_AGENT_LOCK* || "$raw_out" == TASK_BLOCKED* ]]; then
  text="$raw_out"
else
  json_out="$(extract_json_object "$raw_out")"
  text="$(extract_agent_text "$json_out")"
fi
if [[ -z "$text" ]]; then
  session_signal="$(session_signal_for_failure "$agent_session_id" "$agent_id" || true)"
  if [[ "$session_signal" == TASK_DONE* || "$session_signal" == TASK_WAITING_MERGE* || "$session_signal" == TASK_WAITING_DEPENDENCY* || "$session_signal" == TASK_WAITING_AGENT_LOCK* || "$session_signal" == TASK_BLOCKED* ]]; then
    text="$session_signal"
  elif is_retryable_session_signal "$session_signal"; then
    echo "TASK_WAITING_DEPENDENCY: subagent request timed out for ${agent_session_id}; retry task"
    exit 10
  else
    echo "TASK_BLOCKED: openclaw agent returned no assistant text for ${agent_session_id}" >&2
    exit 2
  fi
fi

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
    pr_url="$(create_runner_owned_pr "$text" "$first_line")" || exit $?
    printf 'TASK_WAITING_MERGE PR_URL=%s\n' "$pr_url"
  else
    printf '%s\n' "$text"
  fi
  {
    echo "PR_URL='${pr_url}'"
  } >"$state_file"
  handle_waiting_pr "$pr_url"
  exit $?
fi

printf '%s\n' "$text"

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
