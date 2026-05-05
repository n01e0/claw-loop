#![recursion_limit = "256"]

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Local, Utc};
use clap::{Parser, Subcommand};
#[cfg(test)]
use notify_policy::delivery_retry_backoff_sec;
use notify_policy::{
    AckRetryPolicy, NotificationDeliveryMode, ack_retry_policy, ack_retry_policy_snapshot,
    classify_ack_failure_category, normalize_error_reason, notification_delivery_mode,
    parse_openclaw_message_id,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{self, Command, Stdio};
use std::time::{Duration as StdDuration, Instant};
#[cfg(test)]
use tasklist::parse_task_checklist_entry;
use tasklist::{
    TaskApprovalMetadata, TaskChecklistEntry, append_recovery_task_for_blocked,
    load_task_checklist, task_approval_status, task_checklist_done_count, task_plan_hash,
    update_task_check, write_task_approval,
};
use uuid::Uuid;

mod notify_policy;
mod pr_body;
mod tasklist;

#[derive(Parser, Debug)]
#[command(name = "claw-loopd")]
#[command(about = "Thread-bound Ralph loop daemon controller")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
#[allow(clippy::large_enum_variant)]
enum Commands {
    Start {
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        session_key: String,
        #[arg(long)]
        channel: String,
        #[arg(long)]
        thread_id: String,
        #[arg(long)]
        owner_message_id: Option<String>,
        #[arg(long)]
        requester_user_id: Option<String>,
        #[arg(long)]
        task_agent_id: Option<String>,
        #[arg(long)]
        feedback_thread_id: Option<String>,
        #[arg(long)]
        feedback_channel: Option<String>,
        #[arg(long, default_value_t = 60)]
        tick_sec: u64,
        #[arg(long, default_value_t = false)]
        deliver_openclaw: bool,
        #[arg(long)]
        max_ticks: Option<u64>,
        #[arg(long)]
        max_runtime_sec: Option<u64>,
        #[arg(long, default_value_t = 10)]
        max_task_loops: u64,
        #[arg(long, default_value = "docs/roadmaps/ack-integration-tasklist.md")]
        task_file: PathBuf,
        #[arg(long)]
        task_runner_cmd: Option<String>,
        #[arg(long)]
        approved_tasklist_hash: Option<String>,
        #[arg(long, default_value_t = false)]
        require_task_approval: bool,
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        auto_check_on_success: bool,
        #[arg(long, default_value_t = false)]
        auto_recover_blocked: bool,
        #[arg(long, default_value_t = 3)]
        auto_recover_blocked_max_attempts: u64,
        #[arg(long)]
        backlog_detector_file: Option<PathBuf>,
        #[arg(long, default_value_t = default_backlog_detector_max_age_sec())]
        backlog_detector_max_age_sec: u64,
        #[arg(long, default_value = ".ralph/worktrees")]
        task_worktree_root: PathBuf,
    },
    Daemon {
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        run_id: Uuid,
        #[arg(long, default_value_t = 60)]
        tick_sec: u64,
    },
    Stop {
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        run_id: Uuid,
        #[arg(long, default_value_t = false)]
        immediate: bool,
    },
    Status {
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        run_id: Uuid,
    },
    DeliveryReport {
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        run_id: Uuid,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[arg(long, default_value = "all")]
        status: String,
        #[arg(long, default_value_t = 0)]
        failed_window: usize,
    },
    RequeueDeadLetter {
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        run_id: Uuid,
        #[arg(long)]
        event_id: Option<Uuid>,
        #[arg(long, default_value_t = 10)]
        limit: usize,
        #[arg(long, default_value_t = true)]
        reset_attempts: bool,
        #[arg(long, default_value_t = false)]
        dry_run: bool,
    },
    Notify {
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        run_id: Uuid,
        #[arg(long)]
        kind: String,
        #[arg(long)]
        message: String,
    },
    TrackPr {
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        run_id: Uuid,
        #[arg(long)]
        gh_repo: String,
        #[arg(long)]
        pr: u64,
        #[arg(long, default_value = "merge")]
        merge_method: String,
    },
    Sweep {
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        run_id: Option<Uuid>,
    },
    TaskNext {
        #[arg(long, default_value = "docs/roadmaps/ack-integration-tasklist.md")]
        file: PathBuf,
    },
    TaskCheck {
        #[arg(long, default_value = "docs/roadmaps/ack-integration-tasklist.md")]
        file: PathBuf,
        #[arg(long)]
        id: String,
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        done: bool,
    },
    TaskRunOnce {
        #[arg(long, default_value = "docs/roadmaps/ack-integration-tasklist.md")]
        file: PathBuf,
        #[arg(long)]
        cmd: String,
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        auto_check_on_success: bool,
        #[arg(long, default_value_t = false)]
        dry_run: bool,
    },
    TaskApprove {
        #[arg(long, default_value = "docs/roadmaps/ack-integration-tasklist.md")]
        file: PathBuf,
        #[arg(long)]
        approved_by: String,
    },
}

#[derive(Debug, Serialize, Deserialize)]
struct Manifest {
    run_id: Uuid,
    repo_path: PathBuf,
    session_key: String,
    channel: String,
    thread_id: String,
    owner_message_id: Option<String>,
    #[serde(default)]
    requester_user_id: Option<String>,
    #[serde(default)]
    task_agent_id: Option<String>,
    #[serde(default)]
    feedback_thread_id: Option<String>,
    #[serde(default)]
    feedback_channel: Option<String>,
    started_at: DateTime<Utc>,
    daemon_pid: u32,
    #[serde(default)]
    deliver_openclaw: bool,
    #[serde(default)]
    max_ticks: Option<u64>,
    #[serde(default)]
    max_runtime_sec: Option<u64>,
    #[serde(default = "default_max_task_loops")]
    max_task_loops: u64,
    #[serde(default = "default_task_file")]
    task_file: PathBuf,
    #[serde(default)]
    task_done_baseline: u64,
    #[serde(default)]
    task_runner_cmd: Option<String>,
    approved_tasklist_hash: String,
    approved_by: String,
    approved_at: DateTime<Utc>,
    #[serde(default)]
    require_task_approval: bool,
    #[serde(default = "default_auto_check_on_success")]
    auto_check_on_success: bool,
    #[serde(default)]
    auto_recover_blocked: bool,
    #[serde(default = "default_auto_recover_blocked_max_attempts")]
    auto_recover_blocked_max_attempts: u64,
    #[serde(default)]
    backlog_detector_file: Option<PathBuf>,
    #[serde(default = "default_backlog_detector_max_age_sec")]
    backlog_detector_max_age_sec: u64,
    #[serde(default = "default_task_worktree_root")]
    task_worktree_root: PathBuf,
    #[serde(default)]
    task_worktrees: HashMap<String, TaskWorktreeRecord>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum LoopStatus {
    Idle,
    Running,
    Waiting,
    Blocked,
    Done,
    Failed,
    Stopped,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct State {
    version: u64,
    status: LoopStatus,
    summary: String,
    waiting_reason: String,
    lease_expires_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    #[serde(default)]
    ticks: u64,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum RunnerTaskState {
    Queued,
    Running,
    WaitingMerge,
    WaitingDependency,
    Blocked,
    Done,
}

impl RunnerTaskState {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::WaitingMerge => "waiting_merge",
            Self::WaitingDependency => "waiting_dependency",
            Self::Blocked => "blocked",
            Self::Done => "done",
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
struct WaitingDependencyContext {
    task_id: String,
    #[serde(default)]
    depends_on_task: Option<String>,
    #[serde(default)]
    depends_on_pr_url: Option<String>,
    contract_line: String,
}

#[allow(dead_code)]
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, Default)]
struct AcpxTaskPrMetadata {
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    number: Option<u64>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    merge_state: Option<String>,
    #[serde(default)]
    auto_merge: Option<bool>,
}

#[allow(dead_code)]
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, Default)]
struct AcpxTaskResult {
    summary: String,
    #[serde(default)]
    verification: Vec<String>,
    #[serde(default)]
    notes: Vec<String>,
    pushed_branch: String,
    #[serde(default)]
    pr: Option<AcpxTaskPrMetadata>,
}

#[allow(dead_code)]
fn parse_acpx_task_result(stdout: &str) -> Result<Option<AcpxTaskResult>> {
    let marker = "ACPX_TASK_RESULT_JSON";
    let mut in_fence = false;
    let mut json_lines: Vec<&str> = Vec::new();

    for line in stdout.lines().map(str::trim) {
        if let Some(raw) = line.strip_prefix(&format!("{marker}:")) {
            let result: AcpxTaskResult = serde_json::from_str(raw.trim())
                .with_context(|| format!("parse {marker} inline payload"))?;
            return Ok(Some(result));
        }

        if line == format!("```{marker}") || line == format!("```json {marker}") {
            in_fence = true;
            json_lines.clear();
            continue;
        }

        if in_fence && line == "```" {
            let payload = json_lines.join("\n");
            let result: AcpxTaskResult = serde_json::from_str(&payload)
                .with_context(|| format!("parse {marker} fenced payload"))?;
            return Ok(Some(result));
        }

        if in_fence {
            json_lines.push(line);
        }
    }

    Ok(None)
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum TaskExecutionKind {
    Repair,
    Feature,
    Unknown,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum TaskWorktreeCleanupPolicy {
    RemoveAfterMergeIfClean,
    RetainUntilManualCleanup,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum TaskWorktreeState {
    Planned,
    Created,
    Retained,
    Removed,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
struct TaskWorktreeRecord {
    task_id: String,
    path: PathBuf,
    branch: String,
    base_branch: String,
    cleanup_policy: TaskWorktreeCleanupPolicy,
    state: TaskWorktreeState,
    created_at: DateTime<Utc>,
    #[serde(default)]
    cleanup_reason: Option<String>,
}

impl TaskExecutionKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Repair => "repair",
            Self::Feature => "feature",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
struct BacklogSnapshot {
    detector_file: PathBuf,
    repo_path: PathBuf,
    status: String,
    backlog_count: u64,
    summary: String,
    updated_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
struct RawBacklogSnapshot {
    #[serde(default)]
    repo_path: Option<PathBuf>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default, alias = "count")]
    backlog_count: Option<u64>,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    updated_at: Option<DateTime<Utc>>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Clone)]
enum TaskSelectionOutcome {
    Next(TaskChecklistEntry),
    None,
    Waiting {
        summary: String,
        reason: String,
        backlog_snapshot: BacklogSnapshot,
    },
    Blocked {
        summary: String,
        reason: String,
    },
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum BlockedContextSource {
    RunnerExit,
    WaitingMerge,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum AutoRecoverDecisionState {
    Queued,
    Halted,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
struct RecoveryTaskSnapshot {
    id: String,
    line: usize,
    text: String,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
struct AutoRecoverDecisionSnapshot {
    state: AutoRecoverDecisionState,
    decided_at: DateTime<Utc>,
    reason_key: String,
    attempts: u64,
    same_reason_count: u64,
    max_attempts: u64,
    #[serde(default)]
    guard_reason: Option<String>,
    #[serde(default)]
    recovery_task: Option<RecoveryTaskSnapshot>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
struct BlockedContext {
    task_id: String,
    #[serde(default)]
    task_text: Option<String>,
    #[serde(default)]
    task_line: Option<usize>,
    #[serde(default)]
    pr_url: Option<String>,
    source: BlockedContextSource,
    #[serde(default)]
    exit_code: Option<i32>,
    blocked_at: DateTime<Utc>,
    reason_summary: String,
    #[serde(default)]
    reason_detail: Option<String>,
    #[serde(default)]
    runner_stdout_excerpt: Option<String>,
    #[serde(default)]
    runner_stderr_excerpt: Option<String>,
    #[serde(default)]
    recovery_hint: Option<String>,
    #[serde(default)]
    auto_recover: Option<AutoRecoverDecisionSnapshot>,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
struct RunnerState {
    #[serde(default, alias = "active_task_id")]
    current_task_id: Option<String>,
    #[serde(default, alias = "active_task_text")]
    current_task_text: Option<String>,
    #[serde(default, alias = "active_task_line")]
    current_task_line: Option<usize>,
    #[serde(default, alias = "active_task_started_at")]
    current_task_started_at: Option<DateTime<Utc>>,
    #[serde(default)]
    current_task_state: Option<RunnerTaskState>,
    #[serde(default)]
    current_waiting_dependency: Option<WaitingDependencyContext>,
    #[serde(default)]
    current_worktree: Option<TaskWorktreeRecord>,
    #[serde(default)]
    current_task_blocked_reason: Option<String>,
    #[serde(default)]
    current_blocked_context: Option<BlockedContext>,
    #[serde(default)]
    current_task_pr_url: Option<String>,
    #[serde(default)]
    last_task_id: Option<String>,
    #[serde(default)]
    last_task_state: Option<RunnerTaskState>,
    #[serde(default)]
    last_task_at: Option<DateTime<Utc>>,
    #[serde(default)]
    last_task_reason: Option<String>,
    #[serde(default)]
    last_blocked_context: Option<BlockedContext>,
    #[serde(default)]
    last_task_pr_url: Option<String>,
    #[serde(default)]
    last_waiting_dependency: Option<WaitingDependencyContext>,
    #[serde(default)]
    last_worktree: Option<TaskWorktreeRecord>,
    #[serde(default)]
    preferred_next_task_id: Option<String>,
    #[serde(default)]
    tracked_task_pr_urls: HashMap<String, String>,
    #[serde(default)]
    task_loops_started: u64,
    #[serde(default)]
    waiting_last_summary: Option<String>,
    #[serde(default)]
    waiting_last_reason: Option<String>,
    #[serde(default)]
    waiting_unchanged_ticks: u64,
    #[serde(default)]
    waiting_last_notified_ticks: u64,
    #[serde(default)]
    paused: bool,
    #[serde(default)]
    pause_reason: Option<String>,
    #[serde(default)]
    status_message_id: Option<String>,
    #[serde(default)]
    status_updated_at: Option<DateTime<Utc>>,
    #[serde(default)]
    auto_recover_attempts: u64,
    #[serde(default)]
    auto_recover_last_reason: Option<String>,
    #[serde(default)]
    auto_recover_same_reason_count: u64,
    #[serde(default)]
    auto_recover_last_task_id: Option<String>,
    #[serde(default)]
    auto_recover_last_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Default)]
struct OpenclawDeliveryOutcome {
    message_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct Notification {
    event_id: Uuid,
    run_id: Uuid,
    ts: DateTime<Utc>,
    channel: String,
    thread_id: String,
    kind: String,
    message: String,
    #[serde(default)]
    attempts: u32,
    #[serde(default)]
    next_retry_at: Option<DateTime<Utc>>,
    #[serde(default)]
    last_error: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct DispatchedNotification {
    event_id: Uuid,
    run_id: Uuid,
    dispatched_at: DateTime<Utc>,
    channel: String,
    thread_id: String,
    kind: String,
    message: String,
    #[serde(default)]
    attempts: u32,
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct DeliveryMetrics {
    delivered_total: u64,
    failed_total: u64,
    retried_total: u64,
    dead_letter_total: u64,
    requeued_total: u64,
    last_delivered_at: Option<DateTime<Utc>>,
    last_failed_at: Option<DateTime<Utc>>,
    last_dead_letter_at: Option<DateTime<Utc>>,
    last_requeued_at: Option<DateTime<Utc>>,
    #[serde(default)]
    last_error: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct DeliveryAttempt {
    event_id: Uuid,
    run_id: Uuid,
    attempted_at: DateTime<Utc>,
    success: bool,
    attempts: u32,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct DeliveryAck {
    event_id: Uuid,
    run_id: Uuid,
    acked_at: DateTime<Utc>,
    ok: bool,
    category: String,
    attempts: u32,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct DeadLetterEntry {
    event_id: Uuid,
    run_id: Uuid,
    moved_at: DateTime<Utc>,
    attempts: u32,
    kind: String,
    message: String,
    #[serde(default)]
    last_error: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct PrTracking {
    gh_repo: String,
    pr: u64,
    merge_method: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    last_polled_at: Option<DateTime<Utc>>,
    next_poll_at: DateTime<Utc>,
    unchanged_polls: u32,
    consecutive_errors: u32,
    last_observed_state: Option<String>,
    last_merge_state_status: Option<String>,
    last_error: Option<String>,
    auto_merge_armed: bool,
}

#[derive(Debug, Deserialize, Clone)]
struct GhStatusCheck {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    conclusion: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GhPrView {
    state: String,
    url: String,
    #[serde(rename = "mergeStateStatus")]
    merge_state_status: Option<String>,
    #[serde(rename = "autoMergeRequest")]
    auto_merge_request: Option<serde_json::Value>,
    #[serde(rename = "statusCheckRollup", default)]
    status_check_rollup: Vec<GhStatusCheck>,
}

struct StartOptions {
    repo: PathBuf,
    session_key: String,
    channel: String,
    thread_id: String,
    owner_message_id: Option<String>,
    requester_user_id: Option<String>,
    task_agent_id: Option<String>,
    feedback_thread_id: Option<String>,
    feedback_channel: Option<String>,
    tick_sec: u64,
    deliver_openclaw: bool,
    max_ticks: Option<u64>,
    max_runtime_sec: Option<u64>,
    max_task_loops: u64,
    task_file: PathBuf,
    task_runner_cmd: Option<String>,
    approved_tasklist_hash: Option<String>,
    require_task_approval: bool,
    auto_check_on_success: bool,
    auto_recover_blocked: bool,
    auto_recover_blocked_max_attempts: u64,
    backlog_detector_file: Option<PathBuf>,
    backlog_detector_max_age_sec: u64,
    task_worktree_root: PathBuf,
}

fn default_max_task_loops() -> u64 {
    10
}

fn default_auto_check_on_success() -> bool {
    true
}

fn default_auto_recover_blocked_max_attempts() -> u64 {
    3
}

fn default_backlog_detector_max_age_sec() -> u64 {
    900
}

fn stuck_wait_ticks_threshold() -> u64 {
    std::env::var("CLAW_LOOPD_STUCK_WAIT_TICKS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(30)
}

fn default_task_worktree_root() -> PathBuf {
    PathBuf::from(".ralph/worktrees")
}

fn default_task_file() -> PathBuf {
    PathBuf::from("docs/roadmaps/ack-integration-tasklist.md")
}

fn resolve_task_file_path(repo: &Path, task_file: &Path) -> PathBuf {
    if task_file.is_absolute() {
        task_file.to_path_buf()
    } else {
        repo.join(task_file)
    }
}

fn resolve_task_worktree_root(repo: &Path, root: &Path) -> PathBuf {
    if root.is_absolute() {
        root.to_path_buf()
    } else {
        repo.join(root)
    }
}

fn sanitize_git_ref_part(input: &str) -> String {
    let mut out = String::new();
    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches(|ch| ch == '-' || ch == '.' || ch == '_');
    if trimmed.is_empty() {
        "task".to_string()
    } else {
        trimmed.to_string()
    }
}

fn git_output(repo: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .with_context(|| format!("git {}", args.join(" ")))?;
    if !output.status.success() {
        bail!(
            "git {} failed: status={:?} stderr={}",
            args.join(" "),
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn ensure_task_worktree(
    repo: &Path,
    run_id: Uuid,
    root: &Path,
    task: &TaskChecklistEntry,
    now: DateTime<Utc>,
) -> Result<TaskWorktreeRecord> {
    let abs_root = resolve_task_worktree_root(repo, root);
    fs::create_dir_all(&abs_root)?;
    let task_slug = sanitize_git_ref_part(&task.id);
    let run_slug = run_id.to_string();
    let path = abs_root.join(&run_slug).join(&task_slug);
    let branch = format!("ralph/{}/{}", &run_slug[..8], task_slug);
    let base_branch =
        git_output(repo, &["branch", "--show-current"]).unwrap_or_else(|_| "HEAD".to_string());

    if !path.exists() {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        git_output(
            repo,
            &[
                "worktree",
                "add",
                "-b",
                branch.as_str(),
                path.to_string_lossy().as_ref(),
                "HEAD",
            ],
        )?;
    }

    Ok(TaskWorktreeRecord {
        task_id: task.id.clone(),
        path,
        branch,
        base_branch,
        cleanup_policy: TaskWorktreeCleanupPolicy::RemoveAfterMergeIfClean,
        state: TaskWorktreeState::Created,
        created_at: now,
        cleanup_reason: Some("retain until PR is confirmed merged; remove only if clean".into()),
    })
}

fn require_tasklist_approval(
    task_file: &Path,
    approved_tasklist_hash: Option<&str>,
) -> Result<(TaskApprovalMetadata, String)> {
    let expected_hash = approved_tasklist_hash
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "start requires --approved-tasklist-hash; run `claw-loopd task-approve --file <task_file> --approved-by <name>` first"
            )
        })?;

    let status = task_approval_status(task_file)?;
    let approval = TaskApprovalMetadata {
        approved_by: status
            .approved_by
            .clone()
            .ok_or_else(|| anyhow::anyhow!("tasklist is missing Approved-By marker"))?,
        approved_at: status
            .approved_at
            .ok_or_else(|| anyhow::anyhow!("tasklist is missing Approved-At marker"))?,
    };

    if status.approved_tasklist_hash != expected_hash {
        bail!(
            "approved tasklist hash mismatch: expected={} actual={}",
            expected_hash,
            status.approved_tasklist_hash
        );
    }

    Ok((approval, status.approved_tasklist_hash))
}

fn sync_manifest_tasklist_hash(
    manifest_path: &Path,
    manifest: &mut Manifest,
    task_file: &Path,
) -> Result<()> {
    manifest.approved_tasklist_hash = task_plan_hash(task_file)?;
    write_json(manifest_path, manifest)?;
    Ok(())
}

fn tasklist_approval_violation_reason(task_file: &Path, manifest: &Manifest) -> String {
    if !manifest.require_task_approval {
        return String::new();
    }
    match task_plan_hash(task_file) {
        Ok(actual_hash) => {
            if actual_hash != manifest.approved_tasklist_hash {
                return format!(
                    "tasklist approval invalidated: approved task hash changed (expected={} actual={})",
                    manifest.approved_tasklist_hash, actual_hash
                );
            }
            String::new()
        }
        Err(err) => format!("tasklist approval invalidated: {}", err),
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct DaemonLockMeta {
    pid: u32,
    run_id: Uuid,
    acquired_at: DateTime<Utc>,
}

struct DaemonLockGuard {
    path: PathBuf,
    _file: fs::File,
}

impl Drop for DaemonLockGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn daemon_lock_path(run_dir: &Path) -> PathBuf {
    run_dir.join("daemon.lock")
}

fn acquire_daemon_lock(run_dir: &Path, run_id: Uuid) -> Result<DaemonLockGuard> {
    let path = daemon_lock_path(run_dir);

    for _ in 0..2 {
        match fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
        {
            Ok(mut file) => {
                let meta = DaemonLockMeta {
                    pid: process::id(),
                    run_id,
                    acquired_at: Utc::now(),
                };
                writeln!(file, "{}", serde_json::to_string(&meta)?)?;
                return Ok(DaemonLockGuard { path, _file: file });
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                let stale = match read_json::<DaemonLockMeta>(&path) {
                    Ok(meta) => !process_matches_run(meta.pid, run_id),
                    Err(_) => true,
                };

                if stale {
                    fs::remove_file(&path)
                        .with_context(|| format!("remove stale {}", path.display()))?;
                    continue;
                }

                bail!("daemon lock already held for run {}", run_id);
            }
            Err(err) => {
                return Err(err).with_context(|| format!("open {}", path.display()));
            }
        }
    }

    bail!("failed to acquire daemon lock for run {}", run_id)
}

fn runs_root(repo: &Path) -> PathBuf {
    repo.join(".ralph").join("runs")
}

fn run_dir(repo: &Path, run_id: Uuid) -> PathBuf {
    runs_root(repo).join(run_id.to_string())
}

fn pr_tracking_path(run_dir: &Path) -> PathBuf {
    run_dir.join("pr-tracking.json")
}

fn delivery_metrics_path(run_dir: &Path) -> PathBuf {
    run_dir.join("notify-metrics.json")
}

fn delivery_attempts_path(run_dir: &Path) -> PathBuf {
    run_dir.join("notify-attempts.jsonl")
}

fn delivery_ack_path(run_dir: &Path) -> PathBuf {
    run_dir.join("notify-ack.jsonl")
}

fn dead_letter_path(run_dir: &Path) -> PathBuf {
    run_dir.join("notify-dead-letter.jsonl")
}

fn runner_state_path(run_dir: &Path) -> PathBuf {
    run_dir.join("runner-state.json")
}

fn read_runner_state(run_dir: &Path) -> Result<RunnerState> {
    let path = runner_state_path(run_dir);
    if !path.exists() {
        return Ok(RunnerState::default());
    }
    read_json(&path)
}

fn write_runner_state(run_dir: &Path, runner: &RunnerState) -> Result<()> {
    let path = runner_state_path(run_dir);
    let mut next = runner.clone();
    if path.exists()
        && let Ok(existing) = read_json::<RunnerState>(&path)
    {
        let existing_is_newer = match (existing.status_updated_at, next.status_updated_at) {
            (Some(existing_at), Some(next_at)) => existing_at > next_at,
            (Some(_), None) => true,
            _ => false,
        };
        if existing_is_newer {
            next.status_message_id = existing.status_message_id;
            next.status_updated_at = existing.status_updated_at;
        }
    }
    write_json(&path, &next)
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create dir {}", parent.display()))?;
    }
    let data = serde_json::to_vec_pretty(value)?;
    fs::write(path, data).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let data = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_slice(&data).with_context(|| format!("parse {}", path.display()))
}

fn append_jsonl<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    with_jsonl_file_lock(path, || append_jsonl_unlocked(path, value))
}

fn append_jsonl_unlocked<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    let mut line = serde_json::to_vec(value)?;
    line.push(b'\n');
    f.write_all(&line)
        .with_context(|| format!("append jsonl line to {}", path.display()))?;
    Ok(())
}

fn rewrite_jsonl<T: Serialize>(path: &Path, values: &[T]) -> Result<()> {
    with_jsonl_file_lock(path, || rewrite_jsonl_unlocked(path, values))
}

fn rewrite_jsonl_unlocked<T: Serialize>(path: &Path, values: &[T]) -> Result<()> {
    if values.is_empty() {
        if path.exists() {
            fs::remove_file(path).with_context(|| format!("remove {}", path.display()))?;
        }
        return Ok(());
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut f = fs::File::create(path).with_context(|| format!("create {}", path.display()))?;
    for v in values {
        let mut line = serde_json::to_vec(v)?;
        line.push(b'\n');
        f.write_all(&line)
            .with_context(|| format!("write jsonl line to {}", path.display()))?;
    }
    Ok(())
}

fn jsonl_lock_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("jsonl");
    path.with_file_name(format!(".{file_name}.lock"))
}

struct FileLock {
    path: PathBuf,
}

impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn acquire_file_lock(lock_path: &Path) -> Result<FileLock> {
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let started = Instant::now();
    loop {
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(lock_path)
        {
            Ok(mut f) => {
                writeln!(f, "pid={} ts={}", process::id(), Utc::now())
                    .with_context(|| format!("write lock {}", lock_path.display()))?;
                return Ok(FileLock {
                    path: lock_path.to_path_buf(),
                });
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                let stale = fs::metadata(lock_path)
                    .and_then(|meta| meta.modified())
                    .ok()
                    .and_then(|modified| modified.elapsed().ok())
                    .is_some_and(|elapsed| elapsed > StdDuration::from_secs(120));
                if stale {
                    let _ = fs::remove_file(lock_path);
                    continue;
                }
                if started.elapsed() > StdDuration::from_secs(30) {
                    bail!("timeout waiting for lock {}", lock_path.display());
                }
                std::thread::sleep(StdDuration::from_millis(25));
            }
            Err(err) => {
                return Err(err).with_context(|| format!("create lock {}", lock_path.display()));
            }
        }
    }
}

fn with_file_lock<T>(lock_path: &Path, f: impl FnOnce() -> Result<T>) -> Result<T> {
    let _lock = acquire_file_lock(lock_path)?;
    f()
}

fn with_jsonl_file_lock<T>(path: &Path, f: impl FnOnce() -> Result<T>) -> Result<T> {
    with_file_lock(&jsonl_lock_path(path), f)
}

fn notify_lock_path(run_dir: &Path) -> PathBuf {
    run_dir.join("notify.lock")
}

fn with_notify_lock<T>(run_dir: &Path, f: impl FnOnce() -> Result<T>) -> Result<T> {
    with_file_lock(&notify_lock_path(run_dir), f)
}

fn run_with_timeout_cmd(
    bin: &str,
    args: &[String],
    timeout_sec: u64,
) -> Result<std::process::Output> {
    let mut timeout_args: Vec<String> = vec![format!("{}s", timeout_sec), bin.to_string()];
    timeout_args.extend(args.iter().cloned());

    let timeout_try = Command::new("timeout")
        .args(&timeout_args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output();

    match timeout_try {
        Ok(output) => Ok(output),
        Err(_) => Command::new(bin)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .with_context(|| format!("spawn {}", bin)),
    }
}

fn openclaw_bin() -> String {
    std::env::var("CLAW_LOOPD_OPENCLAW_BIN").unwrap_or_else(|_| "openclaw".to_string())
}

#[derive(Debug, Deserialize)]
struct OpenclawAgentListEntry {
    id: String,
}

fn openclaw_agent_timeout_sec() -> u64 {
    15
}

fn configured_openclaw_agent_ids_with(bin: &str, timeout_sec: u64) -> Result<HashSet<String>> {
    let args: Vec<String> = vec!["agents".into(), "list".into(), "--json".into()];
    let output = run_with_timeout_cmd(bin, &args, timeout_sec)?;
    if !output.status.success() {
        bail!(
            "openclaw agents list failed: status={:?} stderr={}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let agents: Vec<OpenclawAgentListEntry> =
        serde_json::from_slice(&output.stdout).context("parse openclaw agents list json")?;
    Ok(agents.into_iter().map(|agent| agent.id).collect())
}

fn ensure_task_agent_exists(task_agent_id: &str, workspace: &Path) -> Result<bool> {
    ensure_task_agent_exists_with(
        &openclaw_bin(),
        task_agent_id,
        workspace,
        openclaw_agent_timeout_sec(),
    )
}

fn ensure_task_agent_exists_with(
    bin: &str,
    task_agent_id: &str,
    workspace: &Path,
    timeout_sec: u64,
) -> Result<bool> {
    let agent_id = task_agent_id.trim();
    if agent_id.is_empty() {
        bail!("task agent id cannot be empty");
    }

    let configured = configured_openclaw_agent_ids_with(bin, timeout_sec)?;
    if configured.contains(agent_id) {
        return Ok(false);
    }

    let args: Vec<String> = vec![
        "agents".into(),
        "add".into(),
        agent_id.into(),
        "--workspace".into(),
        workspace.display().to_string(),
        "--non-interactive".into(),
        "--json".into(),
    ];
    let output = run_with_timeout_cmd(bin, &args, timeout_sec)?;
    if !output.status.success() {
        let configured_after = configured_openclaw_agent_ids_with(bin, timeout_sec)?;
        if configured_after.contains(agent_id) {
            return Ok(false);
        }
        bail!(
            "openclaw agents add {} failed: status={:?} stderr={}",
            agent_id,
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let configured_after = configured_openclaw_agent_ids_with(bin, timeout_sec)?;
    if configured_after.contains(agent_id) {
        return Ok(true);
    }

    bail!(
        "openclaw agents add {} succeeded but agent is still missing from agents list",
        agent_id
    )
}

fn openclaw_notify_timeout_sec_from(raw: Option<&str>) -> u64 {
    const DEFAULT_TIMEOUT_SEC: u64 = 15;

    raw.and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|v| (1..=120).contains(v))
        .unwrap_or(DEFAULT_TIMEOUT_SEC)
}

fn openclaw_notify_timeout_sec() -> u64 {
    let raw = std::env::var("CLAW_LOOPD_OPENCLAW_TIMEOUT_SEC").ok();
    openclaw_notify_timeout_sec_from(raw.as_deref())
}

fn deliver_via_openclaw(
    notification: &Notification,
    edit_message_id: Option<&str>,
) -> Result<OpenclawDeliveryOutcome> {
    let mut args: Vec<String> = vec!["message".into()];

    if let Some(message_id) = edit_message_id {
        args.extend([
            "edit".into(),
            "--channel".into(),
            notification.channel.clone(),
            "--target".into(),
            notification.thread_id.clone(),
            "--message-id".into(),
            message_id.to_string(),
            "--message".into(),
            format!(
                "[ralph-loop][{}] {}",
                notification.kind, notification.message
            ),
            "--json".into(),
        ]);
    } else {
        args.extend([
            "send".into(),
            "--channel".into(),
            notification.channel.clone(),
            "--target".into(),
            notification.thread_id.clone(),
            "--message".into(),
            format!(
                "[ralph-loop][{}] {}",
                notification.kind, notification.message
            ),
            "--silent".into(),
            "--json".into(),
        ]);
    }

    if std::env::var("CLAW_LOOPD_OPENCLAW_DRY_RUN").ok().as_deref() == Some("1") {
        args.push("--dry-run".into());
    }

    let openclaw = openclaw_bin();
    let timeout_sec = openclaw_notify_timeout_sec();
    let output = run_with_timeout_cmd(&openclaw, &args, timeout_sec)?;
    if !output.status.success() {
        let action = if edit_message_id.is_some() {
            "edit"
        } else {
            "send"
        };
        bail!(
            "openclaw message {} failed: status={:?} stderr={}",
            action,
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let parsed_message_id = parse_openclaw_message_id(&output.stdout);
    Ok(OpenclawDeliveryOutcome {
        message_id: parsed_message_id.or_else(|| edit_message_id.map(ToOwned::to_owned)),
    })
}

fn append_event(run_dir: &Path, kind: &str, extra: serde_json::Value) -> Result<()> {
    let path = run_dir.join("events.jsonl");
    let line = serde_json::json!({
        "ts": Utc::now(),
        "kind": kind,
        "extra": extra
    });
    append_jsonl(&path, &line)
}

fn queue_notification_target(
    run_dir: &Path,
    run_id: Uuid,
    channel: String,
    thread_id: String,
    kind: impl Into<String>,
    message: impl Into<String>,
) -> Result<Uuid> {
    let n = Notification {
        event_id: Uuid::new_v4(),
        run_id,
        ts: Utc::now(),
        channel,
        thread_id,
        kind: kind.into(),
        message: message.into(),
        attempts: 0,
        next_retry_at: None,
        last_error: None,
    };
    with_notify_lock(run_dir, || {
        append_jsonl(&run_dir.join("notify-queue.jsonl"), &n)?;
        append_event(
            run_dir,
            "notify_enqueued",
            serde_json::json!({
                "event_id": n.event_id,
                "kind": n.kind,
                "channel": n.channel,
                "thread_id": n.thread_id,
            }),
        )?;
        Ok(())
    })?;
    Ok(n.event_id)
}

fn queue_notification(
    run_dir: &Path,
    manifest: &Manifest,
    kind: impl Into<String>,
    message: impl Into<String>,
) -> Result<Uuid> {
    let kind = kind.into();
    let mut message = message.into();
    if matches!(
        notification_delivery_mode(&kind),
        NotificationDeliveryMode::EditStatus
    ) {
        message = format!("{message} | as_of={}", format_local_as_of(Utc::now()));
    }

    let event_id = queue_notification_target(
        run_dir,
        manifest.run_id,
        manifest.channel.clone(),
        manifest.thread_id.clone(),
        kind,
        message,
    )?;

    if let Err(err) = flush_notifications(run_dir, manifest) {
        let _ = append_event(
            run_dir,
            "notify_flush_failed",
            serde_json::json!({
                "event_id": event_id,
                "error": err.to_string(),
            }),
        );
    }

    Ok(event_id)
}

fn read_jsonl<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Vec<T>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let f = fs::File::open(path)?;
    let reader = BufReader::new(f);
    let mut out = Vec::new();
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let item = serde_json::from_str::<T>(&line)
            .with_context(|| format!("parse jsonl line in {}", path.display()))?;
        out.push(item);
    }
    Ok(out)
}

#[derive(Debug)]
struct TaskRunOutcome {
    task: Option<TaskChecklistEntry>,
    command: String,
    executed: bool,
    success: bool,
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
    check_result: Option<serde_json::Value>,
}

struct TaskRunOptions<'a> {
    task_file: &'a Path,
    selected_task: Option<TaskChecklistEntry>,
    cmd: &'a str,
    auto_check_on_success: bool,
    dry_run: bool,
    cwd: Option<&'a Path>,
    run_id: Option<Uuid>,
    thread_id: Option<&'a str>,
    channel: Option<&'a str>,
    task_agent_id: Option<&'a str>,
    task_kind: Option<TaskExecutionKind>,
    backlog_snapshot: Option<&'a BacklogSnapshot>,
    worktree: Option<&'a TaskWorktreeRecord>,
}

fn clip_text(input: &str, max_chars: usize) -> String {
    if input.chars().count() <= max_chars {
        return input.to_string();
    }
    let clipped: String = input.chars().take(max_chars).collect();
    format!("{clipped}…")
}

fn clip_optional_text(input: &str, max_chars: usize) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(clip_text(trimmed, max_chars))
    }
}

fn compact_multiline(input: &str) -> Option<String> {
    let compact = input
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if compact.is_empty() {
        None
    } else {
        Some(compact)
    }
}

fn blocked_reason_detail_from_runner(stderr: &str, stdout: &str) -> Option<String> {
    if let Some(stderr_detail) = compact_multiline(stderr) {
        return Some(clip_text(&stderr_detail, 1200));
    }

    if let Some(contract_line) = task_contract_line(stdout) {
        return Some(clip_text(&contract_line, 1200));
    }

    compact_multiline(stdout).map(|detail| clip_text(&detail, 1200))
}

fn build_runner_blocked_context(
    task: Option<&TaskChecklistEntry>,
    exit_code: Option<i32>,
    stdout: &str,
    stderr: &str,
    pr_url: Option<&str>,
    blocked_at: DateTime<Utc>,
) -> BlockedContext {
    let reason_core = blocked_reason_from_runner(stderr, stdout);
    let reason_summary = format!("runner exit={exit_code:?}: {reason_core}");
    let reason_detail = blocked_reason_detail_from_runner(stderr, stdout)
        .map(|detail| format!("runner exit={exit_code:?}: {detail}"))
        .filter(|detail| detail != &reason_summary);
    let recovery_hint = blocked_recovery_hint(&reason_summary).to_string();

    BlockedContext {
        task_id: task
            .map(|entry| entry.id.clone())
            .unwrap_or_else(|| "unknown".to_string()),
        task_text: task.map(|entry| entry.text.clone()),
        task_line: task.map(|entry| entry.line_no),
        pr_url: pr_url.map(ToOwned::to_owned),
        source: BlockedContextSource::RunnerExit,
        exit_code,
        blocked_at,
        reason_summary,
        reason_detail,
        runner_stdout_excerpt: clip_optional_text(stdout, 2000),
        runner_stderr_excerpt: clip_optional_text(stderr, 2000),
        recovery_hint: Some(recovery_hint),
        auto_recover: None,
    }
}

fn build_waiting_merge_blocked_context(
    task: &TaskChecklistEntry,
    pr_url: Option<&str>,
    reason: &str,
    blocked_at: DateTime<Utc>,
) -> BlockedContext {
    let compact_reason = compact_blocked_reason(reason);
    let reason_summary = clip_text(&compact_reason, 200);
    let reason_detail = clip_optional_text(reason, 1200).filter(|detail| detail != &reason_summary);
    let recovery_hint = blocked_recovery_hint(&compact_reason).to_string();

    BlockedContext {
        task_id: task.id.clone(),
        task_text: Some(task.text.clone()),
        task_line: Some(task.line_no),
        pr_url: pr_url.map(ToOwned::to_owned),
        source: BlockedContextSource::WaitingMerge,
        exit_code: None,
        blocked_at,
        reason_summary,
        reason_detail,
        runner_stdout_excerpt: None,
        runner_stderr_excerpt: None,
        recovery_hint: Some(recovery_hint),
        auto_recover: None,
    }
}

fn apply_blocked_context(
    runner_state: &mut RunnerState,
    state: &mut State,
    context: &BlockedContext,
    now: DateTime<Utc>,
) {
    runner_state.current_task_state = Some(RunnerTaskState::Blocked);
    runner_state.current_waiting_dependency = None;
    runner_state.current_task_blocked_reason = Some(context.reason_summary.clone());
    runner_state.current_blocked_context = Some(context.clone());
    runner_state.current_task_pr_url = context.pr_url.clone();
    runner_state.last_task_id = Some(context.task_id.clone());
    runner_state.last_task_state = Some(RunnerTaskState::Blocked);
    runner_state.last_task_at = Some(now);
    runner_state.last_task_reason = Some(context.reason_summary.clone());
    runner_state.last_blocked_context = Some(context.clone());
    runner_state.last_task_pr_url = context.pr_url.clone();
    runner_state.last_worktree = runner_state.current_worktree.clone();

    state.version += 1;
    state.status = LoopStatus::Blocked;
    state.summary = format!("task blocked: {}", context.task_id);
    state.waiting_reason = context.reason_summary.clone();
    state.updated_at = now;
}

fn clear_current_blocked_context(runner_state: &mut RunnerState) {
    runner_state.current_task_blocked_reason = None;
    runner_state.current_blocked_context = None;
}

fn clear_current_waiting_dependency(runner_state: &mut RunnerState) {
    runner_state.current_waiting_dependency = None;
}

fn missing_current_task_id_guard_reason(
    current_task_state: Option<&RunnerTaskState>,
) -> Option<String> {
    let state = current_task_state?;
    let detail = match state {
        RunnerTaskState::WaitingMerge => {
            "task was waiting_merge; refusing to misclassify it as blocked or completed"
        }
        RunnerTaskState::WaitingDependency => {
            "task was waiting_dependency (external dependency); refusing to misclassify it as generic blocked or completed"
        }
        RunnerTaskState::Blocked => {
            "task was already blocked; refusing to misclassify it as waiting_merge, dependency wait, or completed"
        }
        _ => return None,
    };
    Some(format!("runner state lost current_task_id while {detail}"))
}

fn track_task_pr_url(runner_state: &mut RunnerState, task_id: Option<&str>, pr_url: Option<&str>) {
    if let (Some(task_id), Some(pr_url)) = (task_id, pr_url)
        && !task_id.trim().is_empty()
        && !pr_url.trim().is_empty()
    {
        runner_state
            .tracked_task_pr_urls
            .insert(task_id.trim().to_string(), pr_url.trim().to_string());
    }
}

fn enrich_waiting_dependency_context(
    context: &WaitingDependencyContext,
    runner_state: &RunnerState,
) -> WaitingDependencyContext {
    let mut enriched = context.clone();
    if enriched.depends_on_pr_url.is_none()
        && let Some(depends_on_task) = enriched.depends_on_task.as_deref()
        && let Some(pr_url) = runner_state.tracked_task_pr_urls.get(depends_on_task)
    {
        enriched.depends_on_pr_url = Some(pr_url.clone());
    }
    enriched
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum WaitingDependencyProgress {
    Waiting(WaitingDependencyContext),
    Resolved {
        context: WaitingDependencyContext,
        resolution: String,
    },
}

fn ensure_waiting_dependency_progress(
    context: &WaitingDependencyContext,
    entries: &[TaskChecklistEntry],
    runner_state: &RunnerState,
) -> Result<WaitingDependencyProgress> {
    ensure_waiting_dependency_progress_with(context, entries, runner_state, pr_url_is_merged)
}

fn ensure_waiting_dependency_progress_with<F>(
    context: &WaitingDependencyContext,
    entries: &[TaskChecklistEntry],
    runner_state: &RunnerState,
    mut is_merged_fn: F,
) -> Result<WaitingDependencyProgress>
where
    F: FnMut(&str) -> Result<bool>,
{
    let enriched = enrich_waiting_dependency_context(context, runner_state);

    if let Some(depends_on_task) = enriched.depends_on_task.as_deref()
        && entries
            .iter()
            .find(|entry| entry.id == depends_on_task)
            .map(|entry| entry.done)
            .unwrap_or(false)
    {
        let resolution = if let Some(depends_on_pr_url) = enriched.depends_on_pr_url.as_deref() {
            format!(
                "dependency task {} is done (PR merged: {})",
                depends_on_task, depends_on_pr_url
            )
        } else {
            format!("dependency task {} is done", depends_on_task)
        };
        return Ok(WaitingDependencyProgress::Resolved {
            context: enriched,
            resolution,
        });
    }

    if let Some(depends_on_pr_url) = enriched.depends_on_pr_url.as_deref()
        && is_merged_fn(depends_on_pr_url)?
    {
        let resolution = if let Some(depends_on_task) = enriched.depends_on_task.as_deref() {
            format!(
                "dependency PR merged for task {}: {}",
                depends_on_task, depends_on_pr_url
            )
        } else {
            format!("dependency PR merged: {}", depends_on_pr_url)
        };
        return Ok(WaitingDependencyProgress::Resolved {
            context: enriched,
            resolution,
        });
    }

    Ok(WaitingDependencyProgress::Waiting(enriched))
}

fn canonicalize_for_repo_match(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn resolve_backlog_repo_path(raw_repo_path: &Path, detector_file: &Path) -> PathBuf {
    if raw_repo_path.is_absolute() {
        raw_repo_path.to_path_buf()
    } else {
        detector_file
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(raw_repo_path)
    }
}

fn read_backlog_snapshot(
    repo_path: &Path,
    detector_file: &Path,
    max_age_sec: u64,
    now: DateTime<Utc>,
) -> Result<BacklogSnapshot> {
    let raw_text = fs::read_to_string(detector_file)
        .with_context(|| format!("read backlog detector file {}", detector_file.display()))?;
    let raw: RawBacklogSnapshot = serde_json::from_str(&raw_text)
        .with_context(|| format!("parse backlog detector file {}", detector_file.display()))?;

    let raw_repo_path = raw.repo_path.ok_or_else(|| {
        anyhow::anyhow!(
            "backlog detector file {} is missing repo_path",
            detector_file.display()
        )
    })?;
    let resolved_repo_path = resolve_backlog_repo_path(&raw_repo_path, detector_file);
    let expected_repo_path = canonicalize_for_repo_match(repo_path);
    let snapshot_repo_path = canonicalize_for_repo_match(&resolved_repo_path);
    if snapshot_repo_path != expected_repo_path {
        bail!(
            "backlog detector file {} targets repo {} (current repo: {})",
            detector_file.display(),
            snapshot_repo_path.display(),
            expected_repo_path.display()
        );
    }

    let metadata = fs::metadata(detector_file)
        .with_context(|| format!("stat backlog detector file {}", detector_file.display()))?;
    let updated_at = raw
        .updated_at
        .or_else(|| metadata.modified().ok().map(DateTime::<Utc>::from))
        .unwrap_or(now);

    if max_age_sec > 0 {
        let age_sec = now.signed_duration_since(updated_at).num_seconds();
        if age_sec > max_age_sec as i64 {
            bail!(
                "backlog detector file {} is stale: age={}s exceeds {}s",
                detector_file.display(),
                age_sec,
                max_age_sec
            );
        }
    }

    let mut status = raw
        .status
        .unwrap_or_else(|| {
            if raw.backlog_count.unwrap_or(0) > 0 {
                "backlog".to_string()
            } else {
                "clear".to_string()
            }
        })
        .trim()
        .to_ascii_lowercase();
    if status.is_empty() {
        status = "clear".to_string();
    }

    let backlog_count = match status.as_str() {
        "clear" => raw.backlog_count.unwrap_or(0),
        "backlog" => raw.backlog_count.unwrap_or(1),
        "error" => {
            bail!(
                "backlog detector error from {}: {}",
                detector_file.display(),
                raw.error
                    .or(raw.summary)
                    .unwrap_or_else(|| "unknown detector error".to_string())
            )
        }
        "stale" => {
            bail!(
                "backlog detector marked itself stale in {}: {}",
                detector_file.display(),
                raw.summary
                    .unwrap_or_else(|| "detector reported stale status".to_string())
            )
        }
        other => bail!(
            "unsupported backlog detector status '{}' in {}",
            other,
            detector_file.display()
        ),
    };

    let summary = raw.summary.unwrap_or_else(|| match status.as_str() {
        "backlog" => format!("backlog_count={backlog_count}"),
        _ => format!("status={status}"),
    });

    Ok(BacklogSnapshot {
        detector_file: detector_file.to_path_buf(),
        repo_path: snapshot_repo_path,
        status,
        backlog_count,
        summary,
        updated_at,
    })
}

fn classify_task_execution_kind(entry: &TaskChecklistEntry) -> TaskExecutionKind {
    let text = entry.text.trim();
    let lowered = text.to_ascii_lowercase();
    let lowered_id = entry.id.to_ascii_lowercase();

    let has_tag = |tag: &str| {
        lowered.starts_with(&format!("[{tag}]")) || lowered.starts_with(&format!("({tag})"))
    };
    if has_tag("repair") || has_tag("fix") || lowered_id.contains("-recover") {
        return TaskExecutionKind::Repair;
    }
    if has_tag("feature") {
        return TaskExecutionKind::Feature;
    }

    let repair_keywords = [
        "fix",
        "bug",
        "regression",
        "recover",
        "repair",
        "blocked",
        "failure-first",
        "backlog",
        "gating",
        "guardrail",
        "guard",
        "stabilize",
        "stability",
        "retry",
        "ci fail",
        "ci failure",
        "waiting_merge",
        "dependency",
    ];
    if repair_keywords.iter().any(|kw| lowered.contains(kw)) {
        return TaskExecutionKind::Repair;
    }

    let feature_keywords = [
        "feature",
        "add ",
        "add:",
        "implement",
        "introduce",
        "create",
        "build",
        "new ",
    ];
    if feature_keywords.iter().any(|kw| lowered.contains(kw)) {
        return TaskExecutionKind::Feature;
    }

    TaskExecutionKind::Unknown
}

fn select_next_task_with_backlog(
    entries: &[TaskChecklistEntry],
    preferred_recovery_task_id: Option<&str>,
    backlog_snapshot: Option<&BacklogSnapshot>,
) -> TaskSelectionOutcome {
    let next_open = select_next_task_entry(entries, preferred_recovery_task_id);
    let Some(backlog_snapshot) = backlog_snapshot else {
        return next_open
            .map(TaskSelectionOutcome::Next)
            .unwrap_or(TaskSelectionOutcome::None);
    };

    if backlog_snapshot.status != "backlog" || backlog_snapshot.backlog_count == 0 {
        return next_open
            .map(TaskSelectionOutcome::Next)
            .unwrap_or(TaskSelectionOutcome::None);
    }

    if let Some(preferred_id) = preferred_recovery_task_id
        && !preferred_id.trim().is_empty()
        && let Some(preferred_entry) = entries.iter().find(|entry| {
            !entry.done
                && entry.id == preferred_id
                && classify_task_execution_kind(entry) == TaskExecutionKind::Repair
        })
    {
        return TaskSelectionOutcome::Next(preferred_entry.clone());
    }

    if let Some(repair_entry) = entries.iter().find(|entry| {
        !entry.done && classify_task_execution_kind(entry) == TaskExecutionKind::Repair
    }) {
        return TaskSelectionOutcome::Next(repair_entry.clone());
    }

    let Some(next_open) = next_open else {
        return TaskSelectionOutcome::None;
    };
    let blocked_kind = classify_task_execution_kind(&next_open);
    TaskSelectionOutcome::Waiting {
        summary: "task selection gated by failure-first backlog policy".to_string(),
        reason: format!(
            "backlog gate active: backlog_count={}; repair tasks only; next open task {} classified as {}; detector_summary={}; detector_updated_at={}",
            backlog_snapshot.backlog_count,
            next_open.id,
            blocked_kind.as_str(),
            clip_text(&backlog_snapshot.summary, 120),
            backlog_snapshot.updated_at.to_rfc3339()
        ),
        backlog_snapshot: backlog_snapshot.clone(),
    }
}

fn format_backlog_gate_notification(summary: &str, reason: &str) -> String {
    format!(
        "task selection gated by failure-first backlog policy\n- summary: {}\n- reason: {}",
        summary, reason
    )
}

fn format_local_as_of(ts: DateTime<Utc>) -> String {
    ts.with_timezone(&Local)
        .format("%Y-%m-%d %H:%M:%S %Z")
        .to_string()
}

fn format_elapsed_compact(total_sec: u64) -> String {
    let hours = total_sec / 3600;
    let minutes = (total_sec % 3600) / 60;
    let seconds = total_sec % 60;

    if hours > 0 {
        format!("{hours}h{minutes:02}m{seconds:02}s")
    } else if minutes > 0 {
        format!("{minutes}m{seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

fn task_elapsed_suffix(started_at: Option<&DateTime<Utc>>, now: DateTime<Utc>) -> String {
    started_at
        .map(|started| (now - *started).num_seconds().max(0) as u64)
        .map(format_elapsed_compact)
        .map(|elapsed| format!(" elapsed={elapsed}"))
        .unwrap_or_default()
}

fn completion_mention_prefix(manifest: &Manifest) -> String {
    if manifest.channel == "discord"
        && let Some(user_id) = manifest.requester_user_id.as_deref()
        && !user_id.trim().is_empty()
    {
        return format!("<@{}> ", user_id.trim());
    }
    String::new()
}

fn is_phase_or_stacked_dependency_reason(reason: &str) -> bool {
    let normalized = reason.to_ascii_lowercase();
    [
        "cannot be shipped as an isolated green pr",
        "still cannot be shipped as an isolated green pr",
        "isolated green pr without also doing",
        "phase/stacked dependency",
        "stacked change",
        "stacked pr",
        "prior phase",
        "earlier phase",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
}

fn blocked_recovery_hint(reason: &str) -> &'static str {
    let normalized = reason.to_ascii_lowercase();
    if normalized.contains("session file locked") || normalized.contains("task_waiting_agent_lock")
    {
        "別の agent session がロックを保持しているので、ロック解放を待つか `--task-agent-id` を分離して再実行する"
    } else if is_phase_or_stacked_dependency_reason(&normalized) {
        "phase/stacked sequencing が必要。この task を standalone な green PR に押し込まず、前段 task / PR を特定できるなら `TASK_WAITING_DEPENDENCY` を返し、依存先が未特定ならその旨を `TASK_BLOCKED` で明示する"
    } else if normalized.contains("merge state is dirty")
        || normalized.contains("unmergeable branch")
    {
        "PR ブランチが衝突していて merge 不能。base の取り込みや競合解消コミットを入れてから再確認する"
    } else if normalized.contains("unexpected eof")
        || normalized.contains("syntax error")
        || normalized.contains("command not found")
    {
        "runner / script の構文エラーや不足コマンドを直してから、同じタスクを再実行する"
    } else if normalized.contains("timeout") || normalized.contains("timed out") {
        "一時的な timeout の可能性が高い。再試行しつつ、恒常的なら command/agent timeout を引き上げる"
    } else if normalized.contains("permission denied")
        || normalized.contains("401")
        || normalized.contains("403")
        || normalized.contains("forbidden")
    {
        "必要な認証・権限（repo / channel / token など）を修正してから再実行する"
    } else if normalized.contains("without pr_url") || normalized.contains("pr_url") {
        "runner が `TASK_DONE PR_URL=<url>` を返し、PR が merged まで到達するように修正する"
    } else {
        "runner stderr と関連ログを確認し、詰まっているコード/設定を直してから再実行する"
    }
}

fn compact_blocked_reason(reason: &str) -> String {
    let compact = reason
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if compact.is_empty() {
        "不明なブロック理由".to_string()
    } else {
        compact
    }
}

fn blocked_next_step(manifest: &Manifest, task_label: &str) -> &'static str {
    if !manifest.auto_recover_blocked {
        "次の動作: auto-recover は無効。原因を手動で直してから再実行する（または `--auto-recover-blocked` 付きで再起動する）"
    } else if task_label.contains("-RECOVER") {
        "次の動作: 生成された recovery task 自体が失敗したので、auto-recover はここで停止する。task 分割や依存関係を見直してから再開する"
    } else {
        "次の動作: auto-recover が有効なので、daemon がこの原因を解消する recovery task を自動で積んで再開する"
    }
}

fn blocked_intervention_line(
    manifest: &Manifest,
    task_label: &str,
    phase_or_stacked: bool,
) -> &'static str {
    if phase_or_stacked {
        "人手介入: 必要。依存先 task / PR の特定、task 分割の見直し、または phase / stacked 順序の調整を行う"
    } else if !manifest.auto_recover_blocked {
        "人手介入: いま必要。daemon は自動修復しないので、原因を直してから再実行する"
    } else if task_label.contains("-RECOVER") {
        "人手介入: いま必要。auto-recover は停止済みで、同じ経路では自然回復しない"
    } else {
        "人手介入: まずは不要。daemon が auto-recover を試すが、同じ理由が続く・recovery task が失敗する・依存先が不明なままなら介入する"
    }
}

fn format_task_blocked_notification(manifest: &Manifest, blocked: &BlockedContext) -> String {
    let mention = completion_mention_prefix(manifest);
    let compact_reason = compact_blocked_reason(&blocked.reason_summary);
    let reason_summary = clip_text(&compact_reason, 240);
    let detail_source = blocked
        .reason_detail
        .as_deref()
        .unwrap_or(blocked.reason_summary.as_str());
    let compact_detail = compact_blocked_reason(detail_source);
    let detail_line = if compact_detail != compact_reason {
        format!("\n- 詳細: {}", clip_text(&compact_detail, 600))
    } else if compact_reason.chars().count() > 240 {
        format!("\n- 詳細: {}", clip_text(&compact_reason, 600))
    } else {
        String::new()
    };
    let phase_or_stacked = is_phase_or_stacked_dependency_reason(&compact_reason)
        || is_phase_or_stacked_dependency_reason(&compact_detail);
    let classification_line = if phase_or_stacked {
        "\n- 分類: phase/stacked dependency が必要。1 task 1 PR ではなく前段 task / PR の完了待ちとして扱うべき状態"
    } else {
        "\n- 分類: generic blocked。依存待ちではなく、現在の task / PR / runner 側で原因修正が必要"
    };
    let waiting_line = if phase_or_stacked {
        "\n- 今待っているもの: 前段 task / PR の特定または順序調整。依存先が判明したら `TASK_WAITING_DEPENDENCY` として待機へ切り替える"
    } else {
        "\n- 今待っているもの: 自然解消待ちはない。原因修正または auto-recover の結果待ち"
    };
    let intervention_line = blocked_intervention_line(manifest, &blocked.task_id, phase_or_stacked);
    let recovery = blocked
        .recovery_hint
        .as_deref()
        .unwrap_or_else(|| blocked_recovery_hint(&compact_reason));
    let next = blocked_next_step(manifest, &blocked.task_id);
    let pr_suffix = blocked
        .pr_url
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(|v| format!(" PR_URL={v}"))
        .unwrap_or_default();

    format!(
        "{mention}タスクが block された: {}{pr_suffix}\n- 原因: {reason_summary}{detail_line}{classification_line}{waiting_line}\n- 解決方法: {recovery}\n- {intervention_line}\n- {next}",
        blocked.task_id
    )
}

fn format_waiting_merge_notification(
    task_label: &str,
    pr_url: Option<&str>,
    required_checks_missing: bool,
) -> String {
    let wait_target = pr_url
        .map(|url| format!("PR {url} の CI / merge 完了"))
        .unwrap_or_else(|| "current task PR の CI / merge 完了".to_string());
    let checks_line = if required_checks_missing {
        "\n- 注意: required status checks が branch 保護で強制されていない可能性がある。CI が green でも merge 条件を人間が確認する"
    } else {
        ""
    };
    format!(
        "task waiting merge: {task_label}\n- 分類: waiting_merge（generic blocked ではない）\n- 今待っているもの: {wait_target}\n- 次に進む条件: PR が merged したら daemon が task 完了へ進める{checks_line}\n- 人手介入: 通常は不要。CI fail / DIRTY / merge conflict / warning が出た時だけ確認する"
    )
}

fn format_auto_recover_decision_notification(blocked: &BlockedContext) -> Option<String> {
    let decision = blocked.auto_recover.as_ref()?;
    let compact_reason = compact_blocked_reason(&blocked.reason_summary);
    let reason_summary = clip_text(&compact_reason, 240);
    let detail_source = blocked
        .reason_detail
        .as_deref()
        .unwrap_or(blocked.reason_summary.as_str());
    let compact_detail = compact_blocked_reason(detail_source);
    let detail_line = if compact_detail != compact_reason {
        format!("\n- 詳細: {}", clip_text(&compact_detail, 600))
    } else if compact_reason.chars().count() > 240 {
        format!("\n- 詳細: {}", clip_text(&compact_reason, 600))
    } else {
        String::new()
    };
    let recovery = blocked
        .recovery_hint
        .as_deref()
        .unwrap_or_else(|| blocked_recovery_hint(&compact_reason));
    let task_line = decision
        .recovery_task
        .as_ref()
        .map(|task| {
            format!(
                "\n- 実際に積んだ recovery task: {}: {}",
                task.id,
                clip_text(&task.text, 240)
            )
        })
        .unwrap_or_default();
    let status_line = match decision.state {
        AutoRecoverDecisionState::Queued => format!(
            "auto-recover 継続（attempt {}/{}, same_reason={}）",
            decision.attempts, decision.max_attempts, decision.same_reason_count
        ),
        AutoRecoverDecisionState::Halted => {
            let guard = decision
                .guard_reason
                .as_deref()
                .map(|reason| clip_text(&compact_blocked_reason(reason), 240))
                .unwrap_or_else(|| "guard reason unavailable".to_string());
            format!(
                "auto-recover 停止（attempt {}/{}, same_reason={}）: {}",
                decision.attempts, decision.max_attempts, decision.same_reason_count, guard
            )
        }
    };
    let pr_suffix = blocked
        .pr_url
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(|v| format!(" PR_URL={v}"))
        .unwrap_or_default();

    Some(format!(
        "auto-recovery decision: {}{pr_suffix}\n- 原因: {reason_summary}{detail_line}\n- 解決方針: {recovery}{task_line}\n- 状態: {status_line}",
        blocked.task_id
    ))
}

fn recovery_parent_task_id(task_id: &str) -> Option<&str> {
    let (parent, _) = task_id.split_once("-RECOVER")?;
    let parent = parent.trim();
    if parent.is_empty() {
        None
    } else {
        Some(parent)
    }
}

fn format_auto_recover_halt_notification(blocked: &BlockedContext) -> Option<String> {
    let decision = blocked.auto_recover.as_ref()?;
    if decision.state != AutoRecoverDecisionState::Halted {
        return None;
    }

    let compact_reason = compact_blocked_reason(&blocked.reason_summary);
    let reason_summary = clip_text(&compact_reason, 240);
    let detail_source = blocked
        .reason_detail
        .as_deref()
        .unwrap_or(blocked.reason_summary.as_str());
    let compact_detail = compact_blocked_reason(detail_source);
    let detail_line = if compact_detail != compact_reason {
        format!("\n- 詳細: {}", clip_text(&compact_detail, 600))
    } else if compact_reason.chars().count() > 240 {
        format!("\n- 詳細: {}", clip_text(&compact_reason, 600))
    } else {
        String::new()
    };

    let guard = decision
        .guard_reason
        .as_deref()
        .map(|reason| clip_text(&compact_blocked_reason(reason), 240))
        .unwrap_or_else(|| "guard reason unavailable".to_string());
    let recovery = blocked
        .recovery_hint
        .as_deref()
        .unwrap_or_else(|| blocked_recovery_hint(&compact_reason));
    let task_text = blocked
        .task_text
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(|text| clip_text(text, 180))
        .unwrap_or_else(|| "recovery task detail unavailable".to_string());
    let parent_line = recovery_parent_task_id(&blocked.task_id)
        .map(|parent| format!("\n  - 元タスク: {parent}"))
        .unwrap_or_default();
    let stderr_line = blocked
        .runner_stderr_excerpt
        .as_deref()
        .map(|stderr| {
            format!(
                "\n  - stderr: {}",
                clip_text(&compact_blocked_reason(stderr), 240)
            )
        })
        .unwrap_or_default();
    let stdout_line = blocked
        .runner_stdout_excerpt
        .as_deref()
        .map(|stdout| {
            format!(
                "\n  - stdout: {}",
                clip_text(&compact_blocked_reason(stdout), 240)
            )
        })
        .unwrap_or_default();
    let pr_line = blocked
        .pr_url
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(|v| format!("\n  - PR: {v}"))
        .unwrap_or_default();

    Some(format!(
        "auto-recovery halted: {}\n- 停止理由: {}\n- 原因: {reason_summary}{detail_line}\n- 次に見るポイント:\n  - 失敗した recovery task: {}: {}{}{}{}{}\n  - 手動での解決方針: {}",
        blocked.task_id,
        guard,
        blocked.task_id,
        task_text,
        parent_line,
        stderr_line,
        stdout_line,
        pr_line,
        recovery,
    ))
}

fn format_orphan_blocked_notification(manifest: &Manifest, daemon_pid: u32) -> String {
    let mention = completion_mention_prefix(manifest);
    format!(
        "{mention}run が block された: daemon pid {daemon_pid} が lease expiry 後に見つからない\n- 原因: daemon process が終了したまま stale run が残っている\n- 解決方法: 最新 binary で run を再開する前に stale pid / lock 状態を確認し、必要なら古い run を archive へ退避する"
    )
}

fn should_force_status_establish_retry(
    mode: NotificationDeliveryMode,
    status_edit_target: Option<&str>,
) -> bool {
    matches!(mode, NotificationDeliveryMode::EditStatus) && status_edit_target.is_none()
}

fn apply_status_establish_retry_override(
    mut policy: AckRetryPolicy,
    force_status_establish_retry: bool,
) -> AckRetryPolicy {
    if force_status_establish_retry {
        policy.retryable = true;
        policy.max_attempts = u32::MAX;
        policy.backoff_sec = policy.backoff_sec.max(5);
    }
    policy
}

fn queue_main_feedback_summary(
    run_dir: &Path,
    manifest: &Manifest,
    summary_message: &str,
) -> Result<Option<Uuid>> {
    let Some(thread_id) = manifest.feedback_thread_id.as_deref() else {
        return Ok(None);
    };
    let thread_id = thread_id.trim();
    if thread_id.is_empty() {
        return Ok(None);
    }

    let channel = manifest
        .feedback_channel
        .clone()
        .unwrap_or_else(|| manifest.channel.clone());

    if channel == manifest.channel && thread_id == manifest.thread_id {
        return Ok(None);
    }

    let event_id = queue_notification_target(
        run_dir,
        manifest.run_id,
        channel,
        thread_id.to_string(),
        "main_feedback",
        summary_message.to_string(),
    )?;

    if let Err(err) = flush_notifications(run_dir, manifest) {
        let _ = append_event(
            run_dir,
            "notify_flush_failed",
            serde_json::json!({
                "event_id": event_id,
                "kind": "main_feedback",
                "error": err.to_string(),
            }),
        );
    }

    Ok(Some(event_id))
}

fn emit_all_tasks_completed_notifications(
    run_dir: &Path,
    manifest: &Manifest,
    runner_state: &RunnerState,
    task_done_now: u64,
    now: DateTime<Utc>,
) -> Result<()> {
    let mention = completion_mention_prefix(manifest);
    let last_task = runner_state
        .last_task_id
        .clone()
        .unwrap_or_else(|| "unknown".to_string());
    let total_elapsed =
        format_elapsed_compact((now - manifest.started_at).num_seconds().max(0) as u64);
    let summary = format!(
        "{mention}all tasks completed (run_id={}, loops_started={}, done={}, last_task={}, total_elapsed={}); waiting for instruction",
        manifest.run_id, runner_state.task_loops_started, task_done_now, last_task, total_elapsed,
    );

    queue_notification(run_dir, manifest, "all_tasks_completed", summary.clone())?;
    let _ = queue_main_feedback_summary(run_dir, manifest, &summary)?;
    let _ = flush_notifications(run_dir, manifest)?;
    Ok(())
}

fn normalize_blocked_reason_for_recovery(reason: &str) -> String {
    let compact = reason
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();
    clip_text(&compact, 160)
}

fn blocked_reason_seed_for_auto_recovery(blocked: &BlockedContext) -> &str {
    blocked
        .reason_detail
        .as_deref()
        .unwrap_or(blocked.reason_summary.as_str())
}

fn recovery_task_subject(blocked: &BlockedContext) -> String {
    let label = blocked.task_id.trim();
    match blocked.task_text.as_deref().map(str::trim) {
        Some(text) if !text.is_empty() => {
            format!("task {} ({})", label, clip_text(text, 72))
        }
        _ => format!("task {label}"),
    }
}

fn build_recovery_task_text(blocked: &BlockedContext) -> String {
    let subject = recovery_task_subject(blocked);
    let scope = match blocked.source {
        BlockedContextSource::RunnerExit => "resolve runner block for",
        BlockedContextSource::WaitingMerge => "resolve waiting_merge block for",
    };
    let action = blocked
        .recovery_hint
        .as_deref()
        .unwrap_or_else(|| blocked_recovery_hint(&blocked.reason_summary))
        .trim();
    let blocked_reason = clip_text(
        &compact_blocked_reason(blocked_reason_seed_for_auto_recovery(blocked)),
        120,
    );
    let mut text = format!("{scope} {subject}: {action}");
    if !blocked_reason.is_empty() {
        text.push_str(&format!(" (blocked: {blocked_reason})"));
    }
    clip_text(&text, 240)
}

fn auto_recover_guard_reason(
    blocked_task_id: &str,
    reason_key: &str,
    runner_state: &RunnerState,
    max_attempts: u64,
) -> Option<String> {
    if blocked_task_id.contains("-RECOVER") {
        return Some(format!(
            "auto-recover halted: generated recovery task failed ({blocked_task_id})"
        ));
    }
    if runner_state.auto_recover_attempts >= max_attempts {
        return Some(format!(
            "auto-recover halted: max attempts reached ({}/{})",
            runner_state.auto_recover_attempts, max_attempts
        ));
    }
    if runner_state.auto_recover_last_reason.as_deref() == Some(reason_key)
        && runner_state.auto_recover_same_reason_count >= 1
    {
        return Some("auto-recover halted: duplicate blocked reason detected".to_string());
    }
    None
}

#[derive(Debug, PartialEq, Eq)]
enum AutoRecoverBlockedOutcome {
    NotTriggered,
    Recovered,
    Halted,
}

#[allow(clippy::too_many_arguments)]
fn maybe_auto_recover_blocked_task(
    run_dir: &Path,
    manifest_path: &Path,
    manifest: &mut Manifest,
    task_file_abs: &Path,
    runner_state: &mut RunnerState,
    state: &mut State,
    blocked: &BlockedContext,
    now: DateTime<Utc>,
    task_done_now: &mut u64,
    task_loops_completed: &mut u64,
) -> Result<AutoRecoverBlockedOutcome> {
    if !manifest.auto_recover_blocked {
        return Ok(AutoRecoverBlockedOutcome::NotTriggered);
    }

    let blocked_task_id = blocked.task_id.as_str();
    let reason_key =
        normalize_blocked_reason_for_recovery(blocked_reason_seed_for_auto_recovery(blocked));
    let guard_reason = auto_recover_guard_reason(
        blocked_task_id,
        &reason_key,
        runner_state,
        manifest.auto_recover_blocked_max_attempts,
    );

    if let Some(reason) = guard_reason {
        runner_state.paused = true;
        runner_state.pause_reason = Some(reason.clone());
        runner_state.auto_recover_last_reason = Some(reason_key.clone());
        runner_state.auto_recover_same_reason_count = runner_state
            .auto_recover_same_reason_count
            .saturating_add(1);
        runner_state.auto_recover_last_at = Some(now);

        let mut blocked_snapshot = blocked.clone();
        blocked_snapshot.auto_recover = Some(AutoRecoverDecisionSnapshot {
            state: AutoRecoverDecisionState::Halted,
            decided_at: now,
            reason_key: reason_key.clone(),
            attempts: runner_state.auto_recover_attempts,
            same_reason_count: runner_state.auto_recover_same_reason_count,
            max_attempts: manifest.auto_recover_blocked_max_attempts,
            guard_reason: Some(reason.clone()),
            recovery_task: None,
        });
        runner_state.last_blocked_context = Some(blocked_snapshot.clone());
        if runner_state.current_blocked_context.is_some() {
            runner_state.current_blocked_context = Some(blocked_snapshot.clone());
        }
        write_runner_state(run_dir, runner_state)?;

        state.status = LoopStatus::Stopped;
        state.summary = "auto-recovery halted".to_string();
        state.waiting_reason = reason.clone();
        state.updated_at = now;
        state.version += 1;

        append_event(
            run_dir,
            "task_blocked_auto_recover_guard_hit",
            serde_json::json!({
                "blocked_task_id": blocked_task_id,
                "blocked_reason_key": reason_key,
                "guard_reason": reason,
                "attempts": runner_state.auto_recover_attempts,
                "max_attempts": manifest.auto_recover_blocked_max_attempts,
                "blocked_context": blocked_snapshot,
            }),
        )?;

        if let Some(message) = format_auto_recover_halt_notification(&blocked_snapshot) {
            queue_notification(run_dir, manifest, "task_recovery_halted", message)?;
        }

        write_json(&run_dir.join("state.json"), state)?;
        let _ = flush_notifications(run_dir, manifest)?;
        return Ok(AutoRecoverBlockedOutcome::Halted);
    }

    let recovery_task_text = build_recovery_task_text(blocked);
    let recovery_task =
        append_recovery_task_for_blocked(task_file_abs, blocked_task_id, &recovery_task_text)?;
    sync_manifest_tasklist_hash(manifest_path, manifest, task_file_abs)?;
    let _ = update_task_check(task_file_abs, blocked_task_id, true)?;
    *task_done_now = task_checklist_done_count(task_file_abs)?;
    *task_loops_completed = (*task_done_now).saturating_sub(manifest.task_done_baseline);

    runner_state.auto_recover_attempts = runner_state.auto_recover_attempts.saturating_add(1);
    if runner_state.auto_recover_last_reason.as_deref() == Some(reason_key.as_str()) {
        runner_state.auto_recover_same_reason_count = runner_state
            .auto_recover_same_reason_count
            .saturating_add(1);
    } else {
        runner_state.auto_recover_same_reason_count = 1;
    }
    runner_state.auto_recover_last_reason = Some(reason_key.clone());
    runner_state.auto_recover_last_task_id = Some(recovery_task.id.clone());
    runner_state.auto_recover_last_at = Some(now);

    let recovery_task_snapshot = RecoveryTaskSnapshot {
        id: recovery_task.id.clone(),
        line: recovery_task.line_no,
        text: recovery_task.text.clone(),
    };
    let mut blocked_snapshot = blocked.clone();
    blocked_snapshot.auto_recover = Some(AutoRecoverDecisionSnapshot {
        state: AutoRecoverDecisionState::Queued,
        decided_at: now,
        reason_key: reason_key.clone(),
        attempts: runner_state.auto_recover_attempts,
        same_reason_count: runner_state.auto_recover_same_reason_count,
        max_attempts: manifest.auto_recover_blocked_max_attempts,
        guard_reason: None,
        recovery_task: Some(recovery_task_snapshot.clone()),
    });
    runner_state.last_blocked_context = Some(blocked_snapshot.clone());

    runner_state.current_task_id = None;
    runner_state.current_task_text = None;
    runner_state.current_task_line = None;
    runner_state.current_task_started_at = None;
    runner_state.current_task_state = None;
    clear_current_blocked_context(runner_state);
    clear_current_waiting_dependency(runner_state);
    runner_state.current_task_pr_url = None;
    runner_state.paused = false;
    runner_state.pause_reason = None;

    state.version += 1;
    state.status = LoopStatus::Running;
    state.summary = format!("auto-recovery queued: {}", recovery_task.id);
    state.waiting_reason = format!(
        "auto-recovery generated from blocked task {}",
        blocked_task_id
    );
    state.updated_at = now;

    write_runner_state(run_dir, runner_state)?;
    write_json(&run_dir.join("state.json"), state)?;

    append_event(
        run_dir,
        "task_blocked_auto_recovered",
        serde_json::json!({
            "blocked_task_id": blocked_task_id,
            "blocked_reason": runner_state.last_task_reason.clone(),
            "recovery_task_id": recovery_task.id,
            "recovery_task_line": recovery_task.line_no,
            "recovery_task_text": recovery_task.text,
            "auto_recover_attempts": runner_state.auto_recover_attempts,
            "auto_recover_same_reason_count": runner_state.auto_recover_same_reason_count,
            "blocked_context": blocked_snapshot,
        }),
    )?;

    if let Some(message) = format_auto_recover_decision_notification(&blocked_snapshot) {
        queue_notification(run_dir, manifest, "task_recovery_decision", message)?;
    }

    Ok(AutoRecoverBlockedOutcome::Recovered)
}

fn should_suppress_waiting_stuck(runner_state: &RunnerState) -> bool {
    runner_state.paused
        && runner_state.pause_reason.as_deref() == Some("all tasklist items completed")
}

fn update_waiting_stuck_tracker(
    runner_state: &mut RunnerState,
    status: &LoopStatus,
    summary: &str,
    waiting_reason: &str,
    threshold: u64,
) -> Option<u64> {
    if threshold == 0 {
        return None;
    }

    if status != &LoopStatus::Waiting {
        runner_state.waiting_last_summary = None;
        runner_state.waiting_last_reason = None;
        runner_state.waiting_unchanged_ticks = 0;
        runner_state.waiting_last_notified_ticks = 0;
        return None;
    }

    let unchanged = runner_state.waiting_last_summary.as_deref() == Some(summary)
        && runner_state.waiting_last_reason.as_deref() == Some(waiting_reason);

    if unchanged {
        runner_state.waiting_unchanged_ticks =
            runner_state.waiting_unchanged_ticks.saturating_add(1);
    } else {
        runner_state.waiting_last_summary = Some(summary.to_string());
        runner_state.waiting_last_reason = Some(waiting_reason.to_string());
        runner_state.waiting_unchanged_ticks = 1;
        runner_state.waiting_last_notified_ticks = 0;
    }

    if runner_state.waiting_unchanged_ticks < threshold {
        return None;
    }

    if runner_state
        .waiting_unchanged_ticks
        .saturating_sub(runner_state.waiting_last_notified_ticks)
        < threshold
    {
        return None;
    }

    runner_state.waiting_last_notified_ticks = runner_state.waiting_unchanged_ticks;
    Some(runner_state.waiting_unchanged_ticks)
}

fn extract_contract_value(line: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}=");
    line.split_whitespace().find_map(|token| {
        let raw = token.strip_prefix(&prefix)?;
        let trimmed = raw.trim_matches(|c: char| {
            c.is_whitespace()
                || matches!(
                    c,
                    ',' | ';' | '(' | ')' | '[' | ']' | '<' | '>' | '"' | '\''
                )
        });
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    })
}

fn extract_pr_url(line: &str) -> Option<String> {
    extract_contract_value(line, "PR_URL")
}

fn is_absolute_url(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

fn parse_waiting_dependency_contract(
    line: &str,
    fallback_task_id: Option<&str>,
) -> Result<WaitingDependencyContext> {
    let contract_line = line.trim();
    if !contract_line.starts_with("TASK_WAITING_DEPENDENCY") {
        bail!("first line must start with TASK_WAITING_DEPENDENCY");
    }

    let task_id = extract_contract_value(contract_line, "TASK_ID")
        .or_else(|| {
            fallback_task_id
                .map(str::trim)
                .filter(|task_id| !task_id.is_empty())
                .map(ToOwned::to_owned)
        })
        .ok_or_else(|| anyhow::anyhow!("TASK_WAITING_DEPENDENCY requires TASK_ID=<id>"))?;
    let depends_on_task = extract_contract_value(contract_line, "DEPENDS_ON_TASK");
    let depends_on_pr_url = extract_contract_value(contract_line, "DEPENDS_ON_PR_URL");

    if depends_on_task.is_none() && depends_on_pr_url.is_none() {
        bail!(
            "TASK_WAITING_DEPENDENCY requires DEPENDS_ON_TASK=<id> or DEPENDS_ON_PR_URL=<absolute-url>"
        );
    }

    if let Some(depends_on_pr_url) = depends_on_pr_url.as_deref()
        && !is_absolute_url(depends_on_pr_url)
    {
        bail!("DEPENDS_ON_PR_URL must be absolute URL: {depends_on_pr_url}");
    }

    Ok(WaitingDependencyContext {
        task_id,
        depends_on_task,
        depends_on_pr_url,
        contract_line: contract_line.to_string(),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum WaitingContract {
    WaitingMerge {
        contract_line: Option<String>,
        pr_url: Option<String>,
    },
    WaitingDependency(WaitingDependencyContext),
    Other {
        contract_line: Option<String>,
    },
}

fn parse_waiting_contract(
    first_stdout_line: &str,
    exit_code: Option<i32>,
    fallback_task_id: Option<&str>,
) -> Result<Option<WaitingContract>> {
    let trimmed = first_stdout_line.trim();
    if trimmed.starts_with("TASK_WAITING_DEPENDENCY") {
        return Ok(Some(WaitingContract::WaitingDependency(
            parse_waiting_dependency_contract(trimmed, fallback_task_id)?,
        )));
    }
    if trimmed.starts_with("TASK_WAITING_MERGE") {
        return Ok(Some(WaitingContract::WaitingMerge {
            contract_line: Some(trimmed.to_string()),
            pr_url: extract_pr_url(trimmed),
        }));
    }
    if trimmed.starts_with("TASK_WAITING") {
        return Ok(Some(WaitingContract::Other {
            contract_line: Some(trimmed.to_string()),
        }));
    }
    if exit_code == Some(10) {
        return Ok(Some(WaitingContract::Other {
            contract_line: (!trimmed.is_empty()).then(|| trimmed.to_string()),
        }));
    }
    Ok(None)
}

fn format_waiting_dependency_notification(
    task_label: &str,
    context: &WaitingDependencyContext,
) -> String {
    let mut waits_on: Vec<String> = Vec::new();
    if let Some(depends_on_task) = context.depends_on_task.as_deref() {
        waits_on.push(format!("task {depends_on_task}"));
    }
    if let Some(depends_on_pr_url) = context.depends_on_pr_url.as_deref() {
        waits_on.push(format!("PR {depends_on_pr_url}"));
    }
    let waits_on = if waits_on.is_empty() {
        "an upstream dependency".to_string()
    } else {
        waits_on.join(" and ")
    };
    format!(
        "task waiting dependency: {task_label}\n- 分類: dependency wait（generic blocked ではない）\n- 今待っているもの: {waits_on}; standalone PR に押し込まず、前段 phase/stacked change の完了を待つ\n- 次に進む条件: 依存 task / PR が片付いたら daemon が自動で再開する\n- 人手介入: 原則不要。依存先が長時間進まない・依存先指定が誤っている・依存先を特定できない場合のみ必要\n- Auto-recover: idle（dependency が解消するまで recovery task は積まない）"
    )
}

fn parse_github_pr_url(pr_url: &str) -> Result<(String, u64)> {
    let trimmed = pr_url.trim();
    let rest = trimmed
        .strip_prefix("https://github.com/")
        .or_else(|| trimmed.strip_prefix("http://github.com/"))
        .ok_or_else(|| anyhow::anyhow!("unsupported PR URL: {trimmed}"))?;
    let rest = rest.split(['?', '#']).next().unwrap_or(rest);
    let parts: Vec<&str> = rest.split('/').collect();
    if parts.len() < 4 || parts[2] != "pull" {
        bail!("unsupported GitHub PR URL path: {trimmed}");
    }
    let gh_repo = format!("{}/{}", parts[0], parts[1]);
    let pr = parts[3]
        .parse::<u64>()
        .with_context(|| format!("parse PR number from {trimmed}"))?;
    Ok((gh_repo, pr))
}

#[derive(Debug, PartialEq, Eq)]
enum WaitingMergeProgress {
    Waiting,
    Merged,
    Blocked(String),
    Retryable(String),
}

#[derive(Debug, Default, PartialEq, Eq)]
struct PrStatusSummary {
    failed: Vec<String>,
    pending: Vec<String>,
    successful: usize,
}

fn summarize_pr_status_checks(view: &GhPrView) -> PrStatusSummary {
    let mut summary = PrStatusSummary::default();
    for check in &view.status_check_rollup {
        let status = check.status.as_deref().unwrap_or_default();
        let conclusion = check.conclusion.as_deref().unwrap_or_default();
        let name = check.name.as_deref().unwrap_or("unknown");

        if status.eq_ignore_ascii_case("COMPLETED") {
            if matches!(
                conclusion,
                "FAILURE" | "TIMED_OUT" | "CANCELLED" | "ACTION_REQUIRED" | "STARTUP_FAILURE"
            ) {
                summary.failed.push(format!("{name}:{conclusion}"));
            }
            if matches!(conclusion, "SUCCESS" | "NEUTRAL" | "SKIPPED") {
                summary.successful += 1;
            }
        } else if !status.is_empty() || !conclusion.is_empty() {
            summary.pending.push(name.to_string());
        }
    }
    summary
}

fn auto_merge_unavailable_error(err: &str) -> bool {
    let lowered = err.to_ascii_lowercase();
    [
        "auto merge is not allowed",
        "auto-merge is not allowed",
        "pull request auto merge is not allowed for this repository",
        "auto merge is disabled",
        "auto-merge is disabled",
        "repository does not allow auto-merge",
        "repository has disabled auto-merge",
    ]
    .iter()
    .any(|needle| lowered.contains(needle))
}

fn retryable_waiting_merge_error(err: &str) -> bool {
    let lowered = err.to_ascii_lowercase();
    lowered.contains("status=some(124)")
        || lowered.contains("timed out")
        || lowered.contains("timeout")
}

fn waiting_merge_retryable_reason(pr_url: &str, err: &str) -> String {
    format!(
        "waiting_merge retryable error for PR_URL={} error={}",
        pr_url,
        clip_text(err, 200)
    )
}

fn waiting_merge_nonprogress_reason(view: &GhPrView, pr_url: &str) -> Option<String> {
    let status = view.merge_state_status.as_deref().unwrap_or_default();
    if status.eq_ignore_ascii_case("DIRTY") {
        return Some(format!(
            "PR_URL={} merge state is DIRTY (merge conflict or unmergeable branch)",
            pr_url
        ));
    }
    None
}

fn ensure_waiting_merge_progress(pr_url: &str) -> Result<WaitingMergeProgress> {
    ensure_waiting_merge_progress_with(
        pr_url,
        gh_pr_view,
        gh_pr_arm_auto_merge,
        gh_pr_merge_now,
        pr_url_is_merged,
    )
}

fn ensure_waiting_merge_progress_with<FView, FArm, FMerge, FMerged>(
    pr_url: &str,
    mut view_fn: FView,
    mut arm_auto_merge_fn: FArm,
    mut merge_now_fn: FMerge,
    mut is_merged_fn: FMerged,
) -> Result<WaitingMergeProgress>
where
    FView: FnMut(&str, u64) -> Result<GhPrView>,
    FArm: FnMut(&str, u64, &str) -> Result<()>,
    FMerge: FnMut(&str, u64, &str) -> Result<()>,
    FMerged: FnMut(&str) -> Result<bool>,
{
    let (gh_repo, pr) = parse_github_pr_url(pr_url)?;
    let mut view = match view_fn(&gh_repo, pr) {
        Ok(view) => view,
        Err(err) => {
            if retryable_waiting_merge_error(&err.to_string()) {
                return Ok(WaitingMergeProgress::Retryable(
                    waiting_merge_retryable_reason(pr_url, &err.to_string()),
                ));
            }
            return Err(err);
        }
    };
    if view.state.eq_ignore_ascii_case("MERGED") {
        return Ok(WaitingMergeProgress::Merged);
    }

    let mut manual_fallback = false;
    if view.state.eq_ignore_ascii_case("OPEN") && view.auto_merge_request.is_none() {
        match arm_auto_merge_fn(&gh_repo, pr, "squash") {
            Ok(()) => {
                view = match view_fn(&gh_repo, pr) {
                    Ok(view) => view,
                    Err(err) => {
                        if retryable_waiting_merge_error(&err.to_string()) {
                            return Ok(WaitingMergeProgress::Retryable(
                                waiting_merge_retryable_reason(pr_url, &err.to_string()),
                            ));
                        }
                        return Err(err);
                    }
                };
                if view.state.eq_ignore_ascii_case("MERGED") {
                    return Ok(WaitingMergeProgress::Merged);
                }
                manual_fallback = view.auto_merge_request.is_none();
            }
            Err(err) => {
                if auto_merge_unavailable_error(&err.to_string()) {
                    manual_fallback = true;
                } else if retryable_waiting_merge_error(&err.to_string()) {
                    return Ok(WaitingMergeProgress::Retryable(
                        waiting_merge_retryable_reason(pr_url, &err.to_string()),
                    ));
                } else {
                    return Err(err);
                }
            }
        }
    }

    if let Some(reason) = waiting_merge_nonprogress_reason(&view, pr_url) {
        return Ok(WaitingMergeProgress::Blocked(reason));
    }

    if !manual_fallback {
        return Ok(WaitingMergeProgress::Waiting);
    }

    let summary = summarize_pr_status_checks(&view);
    if !summary.failed.is_empty() {
        return Ok(WaitingMergeProgress::Blocked(format!(
            "CI failed for PR_URL={} checks={}",
            pr_url,
            summary.failed.join(", ")
        )));
    }
    if !summary.pending.is_empty() || summary.successful == 0 {
        return Ok(WaitingMergeProgress::Waiting);
    }

    match merge_now_fn(&gh_repo, pr, "squash") {
        Ok(()) => match is_merged_fn(pr_url) {
            Ok(true) => Ok(WaitingMergeProgress::Merged),
            Ok(false) => Ok(WaitingMergeProgress::Waiting),
            Err(err) if retryable_waiting_merge_error(&err.to_string()) => {
                Ok(WaitingMergeProgress::Retryable(
                    waiting_merge_retryable_reason(pr_url, &err.to_string()),
                ))
            }
            Err(err) => Err(err),
        },
        Err(err) if retryable_waiting_merge_error(&err.to_string()) => {
            Ok(WaitingMergeProgress::Retryable(
                waiting_merge_retryable_reason(pr_url, &err.to_string()),
            ))
        }
        Err(err) => Ok(WaitingMergeProgress::Blocked(format!(
            "manual merge failed for PR_URL={} error={}",
            pr_url,
            clip_text(&err.to_string(), 200)
        ))),
    }
}

fn pr_url_is_merged(pr_url: &str) -> Result<bool> {
    let gh = gh_bin();
    let args: Vec<String> = vec![
        "pr".into(),
        "view".into(),
        pr_url.to_string(),
        "--json".into(),
        "state,mergedAt".into(),
    ];
    let output = run_with_timeout_cmd(&gh, &args, 5)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let is_timeout = output.status.code() == Some(124)
            || stderr.to_ascii_lowercase().contains("timed out")
            || stderr.to_ascii_lowercase().contains("timeout");

        if is_timeout {
            bail!("merge check timed out for {pr_url}: {stderr}");
        }

        bail!(
            "gh pr view failed: status={:?} stderr={}",
            output.status.code(),
            stderr
        );
    }
    let value: serde_json::Value = serde_json::from_slice(&output.stdout)
        .with_context(|| format!("parse gh pr view json for {pr_url}"))?;
    let state = value
        .get("state")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let merged_at = value.get("mergedAt").and_then(|v| v.as_str());
    Ok(state.eq_ignore_ascii_case("MERGED") || merged_at.is_some())
}

fn validate_task_done_contract_with(
    first_stdout_line: &str,
    is_merged: impl FnOnce(&str) -> Result<bool>,
) -> Result<String> {
    if !first_stdout_line.starts_with("TASK_DONE") {
        bail!("first line must start with TASK_DONE");
    }

    let pr_url = extract_pr_url(first_stdout_line)
        .ok_or_else(|| anyhow::anyhow!("TASK_DONE line must include PR_URL=<url>"))?;

    if !is_absolute_url(&pr_url) {
        bail!("PR_URL must be absolute URL: {pr_url}");
    }

    if !is_merged(&pr_url)? {
        bail!("PR is not merged yet: {pr_url}");
    }

    Ok(pr_url)
}

fn validate_task_done_contract(first_stdout_line: &str) -> Result<String> {
    validate_task_done_contract_with(first_stdout_line, pr_url_is_merged)
}

fn completion_guard_waiting_fallback_line(first_stdout_line: &str, err: &str) -> Option<String> {
    let lowered = err.to_ascii_lowercase();
    if !lowered.contains("merge check timed out") {
        return None;
    }
    let pr_url = extract_pr_url(first_stdout_line)?;
    Some(format!("TASK_WAITING_MERGE PR_URL={pr_url}"))
}

fn select_next_task_entry(
    entries: &[TaskChecklistEntry],
    preferred_recovery_task_id: Option<&str>,
) -> Option<TaskChecklistEntry> {
    if let Some(preferred_id) = preferred_recovery_task_id
        && !preferred_id.trim().is_empty()
        && let Some(recovery_entry) = entries
            .iter()
            .find(|entry| !entry.done && entry.id == preferred_id)
    {
        return Some(recovery_entry.clone());
    }

    entries.iter().find(|entry| !entry.done).cloned()
}

fn task_contract_line(stdout: &str) -> Option<String> {
    stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .rfind(|line| line.starts_with("TASK_"))
        .map(ToString::to_string)
}

fn blocked_reason_from_runner(stderr: &str, stdout: &str) -> String {
    let stderr_first = stderr
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| clip_text(line, 200));
    if let Some(stderr_first) = stderr_first {
        return stderr_first;
    }

    if let Some(contract_line) = task_contract_line(stdout) {
        return clip_text(&contract_line, 200);
    }

    let stdout_last = stdout
        .lines()
        .map(str::trim)
        .rfind(|line| !line.is_empty())
        .map(|line| clip_text(line, 200));
    if let Some(stdout_last) = stdout_last {
        return stdout_last;
    }

    "runner produced no stderr/stdout detail".to_string()
}

fn run_task_once(opts: TaskRunOptions<'_>) -> Result<TaskRunOutcome> {
    let next = if let Some(selected) = opts.selected_task.clone() {
        Some(selected)
    } else {
        let (_, _, entries) = load_task_checklist(opts.task_file)?;
        select_next_task_entry(&entries, None)
    };

    let mut outcome = TaskRunOutcome {
        task: next.clone(),
        command: opts.cmd.to_string(),
        executed: false,
        success: false,
        exit_code: None,
        stdout: String::new(),
        stderr: String::new(),
        check_result: None,
    };

    let Some(task) = next else {
        return Ok(outcome);
    };

    if opts.dry_run {
        return Ok(outcome);
    }

    let mut command = Command::new("bash");
    command
        .arg("-lc")
        .arg(opts.cmd)
        .env("CLAW_TASK_ID", &task.id)
        .env("CLAW_TASK_TEXT", &task.text)
        .env("CLAW_TASK_LINE", task.line_no.to_string())
        .env(
            "CLAW_TASK_FILE",
            opts.task_file.to_string_lossy().to_string(),
        );

    if let Some(run_id) = opts.run_id {
        command.env("CLAW_RUN_ID", run_id.to_string());
    }
    if let Some(thread_id) = opts.thread_id {
        command.env("CLAW_THREAD_ID", thread_id);
    }
    if let Some(channel) = opts.channel {
        command.env("CLAW_CHANNEL", channel);
    }
    if let Some(task_agent_id) = opts.task_agent_id {
        command.env("CLAW_AGENT_ID", task_agent_id);
    }
    if let Some(task_kind) = opts.task_kind {
        command.env("CLAW_TASK_KIND", task_kind.as_str());
    }
    if let Some(worktree) = opts.worktree {
        command
            .env(
                "CLAW_TASK_WORKTREE",
                worktree.path.to_string_lossy().to_string(),
            )
            .env("CLAW_TASK_BRANCH", worktree.branch.as_str())
            .env("CLAW_TASK_BASE_BRANCH", worktree.base_branch.as_str())
            .env(
                "CLAW_TASK_WORKTREE_CLEANUP_POLICY",
                "remove_after_merge_if_clean",
            );
    }
    if let Some(backlog_snapshot) = opts.backlog_snapshot {
        command
            .env("CLAW_BACKLOG_STATUS", backlog_snapshot.status.as_str())
            .env(
                "CLAW_BACKLOG_COUNT",
                backlog_snapshot.backlog_count.to_string(),
            )
            .env("CLAW_BACKLOG_SUMMARY", backlog_snapshot.summary.as_str())
            .env(
                "CLAW_BACKLOG_UPDATED_AT",
                backlog_snapshot.updated_at.to_rfc3339(),
            )
            .env(
                "CLAW_BACKLOG_FILE",
                backlog_snapshot.detector_file.to_string_lossy().to_string(),
            );
    }

    if let Some(cwd) = opts.cwd {
        command.current_dir(cwd);
    }

    let output = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("run task command: {}", opts.cmd))?;

    outcome.executed = true;
    outcome.success = output.status.success();
    outcome.exit_code = output.status.code();
    outcome.stdout = String::from_utf8_lossy(&output.stdout).to_string();
    outcome.stderr = String::from_utf8_lossy(&output.stderr).to_string();

    if outcome.success && opts.auto_check_on_success {
        let first_stdout_line = task_contract_line(&outcome.stdout).unwrap_or_else(|| {
            outcome
                .stdout
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty())
                .unwrap_or("")
                .to_string()
        });

        match validate_task_done_contract(&first_stdout_line) {
            Ok(_) => {
                outcome.check_result = Some(update_task_check(opts.task_file, &task.id, true)?);
            }
            Err(err) => {
                let err_text = err.to_string().replace('\n', " ");

                if let Some(waiting_line) =
                    completion_guard_waiting_fallback_line(&first_stdout_line, &err_text)
                {
                    outcome.success = false;
                    outcome.exit_code = Some(10);
                    outcome.stdout = format!("{waiting_line}\n{}", outcome.stdout);
                    if !outcome.stderr.trim().is_empty() {
                        outcome.stderr.push('\n');
                    }
                    outcome.stderr.push_str(&format!(
                        "completion guard deferred to waiting recheck: {err_text}"
                    ));
                } else {
                    outcome.success = false;
                    if outcome.exit_code == Some(0) {
                        outcome.exit_code = Some(65);
                    }
                    if !outcome.stderr.trim().is_empty() {
                        outcome.stderr.push('\n');
                    }
                    outcome
                        .stderr
                        .push_str(&format!("completion guard failed: {err_text}"));
                }
            }
        }
    }

    Ok(outcome)
}

fn append_ack_idempotent(
    path: &Path,
    ack: &DeliveryAck,
    seen: &mut HashSet<(Uuid, u32)>,
) -> Result<bool> {
    let key = (ack.event_id, ack.attempts);
    if seen.contains(&key) {
        return Ok(false);
    }
    append_jsonl(path, ack)?;
    seen.insert(key);
    Ok(true)
}

fn reconcile_delivery_state(run_dir: &Path) -> Result<serde_json::Value> {
    with_notify_lock(run_dir, || reconcile_delivery_state_locked(run_dir))
}

fn reconcile_delivery_state_locked(run_dir: &Path) -> Result<serde_json::Value> {
    let queue_path = run_dir.join("notify-queue.jsonl");
    let dispatched_path = run_dir.join("notify-dispatched.jsonl");
    let ack_path = delivery_ack_path(run_dir);

    let mut queue = read_jsonl::<Notification>(&queue_path)?;
    let dispatched = read_jsonl::<DispatchedNotification>(&dispatched_path)?;
    let acks = read_jsonl::<DeliveryAck>(&ack_path)?;

    let mut dispatched_ids: HashSet<Uuid> = HashSet::new();
    for d in &dispatched {
        dispatched_ids.insert(d.event_id);
    }

    // Remove stale queued items already dispatched in previous process lifetimes.
    let before_queue = queue.len();
    queue.retain(|n| !dispatched_ids.contains(&n.event_id));
    let removed_queued = before_queue.saturating_sub(queue.len());
    if removed_queued > 0 {
        rewrite_jsonl(&queue_path, &queue)?;
    }

    // De-duplicate ack entries by (event_id, attempts), keeping first seen.
    let mut ack_seen: HashSet<(Uuid, u32)> = HashSet::new();
    let mut ack_deduped: Vec<DeliveryAck> = Vec::new();
    let mut removed_ack_duplicates = 0usize;
    for ack in acks {
        let key = (ack.event_id, ack.attempts);
        if ack_seen.insert(key) {
            ack_deduped.push(ack);
        } else {
            removed_ack_duplicates += 1;
        }
    }
    if removed_ack_duplicates > 0 {
        rewrite_jsonl(&ack_path, &ack_deduped)?;
    }

    Ok(serde_json::json!({
        "removed_stale_queued": removed_queued,
        "removed_ack_duplicates": removed_ack_duplicates,
    }))
}

fn flush_notifications(run_dir: &Path, manifest: &Manifest) -> Result<usize> {
    with_notify_lock(run_dir, || flush_notifications_locked(run_dir, manifest))
}

fn flush_notifications_locked(run_dir: &Path, manifest: &Manifest) -> Result<usize> {
    let queue_path = run_dir.join("notify-queue.jsonl");
    let dispatched_path = run_dir.join("notify-dispatched.jsonl");
    let attempts_path = delivery_attempts_path(run_dir);
    let ack_path = delivery_ack_path(run_dir);
    let dead_letter_file = dead_letter_path(run_dir);
    let metrics_path = delivery_metrics_path(run_dir);

    let queued = read_jsonl::<Notification>(&queue_path)?;
    if queued.is_empty() {
        return Ok(0);
    }

    let already = read_jsonl::<DispatchedNotification>(&dispatched_path)?;
    let mut terminal_ids = HashSet::new();
    for d in already {
        terminal_ids.insert(d.event_id);
    }
    let existing_dead_letter = read_jsonl::<DeadLetterEntry>(&dead_letter_file)?;
    for dlq in existing_dead_letter {
        terminal_ids.insert(dlq.event_id);
    }

    let existing_acks = read_jsonl::<DeliveryAck>(&ack_path)?;
    let mut ack_seen: HashSet<(Uuid, u32)> = existing_acks
        .into_iter()
        .map(|a| (a.event_id, a.attempts))
        .collect();

    let mut metrics = if metrics_path.exists() {
        read_json::<DeliveryMetrics>(&metrics_path)?
    } else {
        DeliveryMetrics::default()
    };

    let now = Utc::now();
    let mut delivered = 0usize;
    let mut kept: Vec<Notification> = Vec::new();
    let mut processed_in_flush: HashSet<Uuid> = HashSet::new();
    let mut runner_state = read_runner_state(run_dir)?;
    let mut runner_state_dirty = false;

    for mut n in queued {
        if terminal_ids.contains(&n.event_id) {
            continue;
        }
        if !processed_in_flush.insert(n.event_id) {
            continue;
        }

        if let Some(next_retry_at) = n.next_retry_at
            && now < next_retry_at
        {
            kept.push(n);
            continue;
        }

        let previous_attempts = n.attempts;
        n.attempts = n.attempts.saturating_add(1);

        let mode = notification_delivery_mode(&n.kind);
        let status_edit_target = if matches!(mode, NotificationDeliveryMode::EditStatus) {
            runner_state.status_message_id.clone()
        } else {
            None
        };

        let mut delivery_result = if manifest.deliver_openclaw {
            deliver_via_openclaw(&n, status_edit_target.as_deref())
        } else {
            Ok(OpenclawDeliveryOutcome {
                message_id: status_edit_target.clone(),
            })
        };

        if manifest.deliver_openclaw
            && matches!(mode, NotificationDeliveryMode::EditStatus)
            && status_edit_target.is_some()
            && let Err(edit_err) = &delivery_result
        {
            let fallback_result = deliver_via_openclaw(&n, None);
            match fallback_result {
                Ok(fallback_outcome) => {
                    append_event(
                        run_dir,
                        "notify_status_edit_fallback_send",
                        serde_json::json!({
                            "event_id": n.event_id,
                            "kind": n.kind.clone(),
                            "previous_status_message_id": status_edit_target.clone(),
                            "error": edit_err.to_string(),
                            "recreated_message_id": fallback_outcome.message_id.clone(),
                        }),
                    )?;
                    delivery_result = Ok(fallback_outcome);
                }
                Err(send_err) => {
                    delivery_result = Err(anyhow::anyhow!(
                        "status message edit fallback failed: edit_error={} send_error={}",
                        edit_err,
                        send_err
                    ));
                }
            }
        }

        match delivery_result {
            Ok(delivery_outcome) => {
                let attempt = DeliveryAttempt {
                    event_id: n.event_id,
                    run_id: n.run_id,
                    attempted_at: now,
                    success: true,
                    attempts: n.attempts,
                    error: None,
                };
                append_jsonl(&attempts_path, &attempt)?;

                let ack = DeliveryAck {
                    event_id: n.event_id,
                    run_id: n.run_id,
                    acked_at: now,
                    ok: true,
                    category: "ok".to_string(),
                    attempts: n.attempts,
                    error: None,
                };
                let ack_added = append_ack_idempotent(&ack_path, &ack, &mut ack_seen)?;
                if !ack_added {
                    append_event(
                        run_dir,
                        "ack_duplicate_skipped",
                        serde_json::json!({
                            "event_id": ack.event_id,
                            "attempts": ack.attempts,
                        }),
                    )?;
                }

                if matches!(mode, NotificationDeliveryMode::EditStatus) {
                    if let Some(message_id) = delivery_outcome.message_id
                        && runner_state.status_message_id.as_deref() != Some(message_id.as_str())
                    {
                        runner_state.status_message_id = Some(message_id);
                        runner_state_dirty = true;
                    }
                    if runner_state.status_message_id.is_some() {
                        runner_state.status_updated_at = Some(now);
                        runner_state_dirty = true;
                    }
                }

                let d = DispatchedNotification {
                    event_id: n.event_id,
                    run_id: n.run_id,
                    dispatched_at: now,
                    channel: n.channel,
                    thread_id: n.thread_id,
                    kind: n.kind,
                    message: n.message,
                    attempts: n.attempts,
                };
                append_jsonl(&dispatched_path, &d)?;
                terminal_ids.insert(n.event_id);
                delivered += 1;

                metrics.delivered_total = metrics.delivered_total.saturating_add(1);
                metrics.last_delivered_at = Some(now);
                if previous_attempts > 0 {
                    metrics.retried_total = metrics.retried_total.saturating_add(1);
                }
            }
            Err(err) => {
                let err_text = err.to_string();
                let attempt = DeliveryAttempt {
                    event_id: n.event_id,
                    run_id: n.run_id,
                    attempted_at: now,
                    success: false,
                    attempts: n.attempts,
                    error: Some(err_text.clone()),
                };
                append_jsonl(&attempts_path, &attempt)?;

                let category = classify_ack_failure_category(Some(&err_text));
                let ack = DeliveryAck {
                    event_id: n.event_id,
                    run_id: n.run_id,
                    acked_at: now,
                    ok: false,
                    category: category.clone(),
                    attempts: n.attempts,
                    error: Some(err_text.clone()),
                };
                let ack_added = append_ack_idempotent(&ack_path, &ack, &mut ack_seen)?;
                if !ack_added {
                    append_event(
                        run_dir,
                        "ack_duplicate_skipped",
                        serde_json::json!({
                            "event_id": ack.event_id,
                            "attempts": ack.attempts,
                        }),
                    )?;
                }

                metrics.failed_total = metrics.failed_total.saturating_add(1);
                metrics.last_failed_at = Some(now);
                metrics.last_error = Some(err_text.clone());

                let force_status_establish_retry =
                    should_force_status_establish_retry(mode, status_edit_target.as_deref());
                let policy = apply_status_establish_retry_override(
                    ack_retry_policy(&category, n.attempts),
                    force_status_establish_retry,
                );
                if force_status_establish_retry {
                    append_event(
                        run_dir,
                        "notify_status_establish_retry",
                        serde_json::json!({
                            "event_id": n.event_id,
                            "kind": n.kind.clone(),
                            "attempts": n.attempts,
                            "category": category,
                            "max_attempts": policy.max_attempts,
                            "backoff_sec": policy.backoff_sec,
                        }),
                    )?;
                }

                if !policy.retryable || n.attempts >= policy.max_attempts {
                    let dead = DeadLetterEntry {
                        event_id: n.event_id,
                        run_id: n.run_id,
                        moved_at: now,
                        attempts: n.attempts,
                        kind: n.kind,
                        message: n.message,
                        last_error: Some(err_text.clone()),
                    };
                    append_jsonl(&dead_letter_file, &dead)?;
                    terminal_ids.insert(n.event_id);

                    metrics.dead_letter_total = metrics.dead_letter_total.saturating_add(1);
                    metrics.last_dead_letter_at = Some(now);

                    append_event(
                        run_dir,
                        "notify_dead_letter",
                        serde_json::json!({
                            "event_id": dead.event_id,
                            "attempts": dead.attempts,
                            "category": category,
                            "error": dead.last_error,
                            "max_attempts": policy.max_attempts,
                            "retryable": policy.retryable,
                        }),
                    )?;
                } else {
                    n.next_retry_at = Some(now + chrono::Duration::seconds(policy.backoff_sec));
                    n.last_error = Some(err_text.clone());
                    kept.push(n.clone());

                    append_event(
                        run_dir,
                        "notify_delivery_error",
                        serde_json::json!({
                            "event_id": n.event_id,
                            "category": category,
                            "error": err_text,
                            "attempts": n.attempts,
                            "next_retry_at": n.next_retry_at,
                            "will_retry": true,
                            "max_attempts": policy.max_attempts,
                        }),
                    )?;
                }
            }
        }
    }

    if runner_state_dirty {
        write_runner_state(run_dir, &runner_state)?;
    }

    rewrite_jsonl(&queue_path, &kept)?;
    write_json(&metrics_path, &metrics)?;

    if delivered > 0 {
        append_event(
            run_dir,
            "notify_flushed",
            serde_json::json!({"count": delivered}),
        )?;
    }

    Ok(delivered)
}

fn gh_bin() -> String {
    std::env::var("CLAW_LOOPD_GH_BIN").unwrap_or_else(|_| "gh".to_string())
}

fn gh_pr_view(gh_repo: &str, pr: u64) -> Result<GhPrView> {
    let gh = gh_bin();
    let args: Vec<String> = vec![
        "pr".into(),
        "view".into(),
        pr.to_string(),
        "--repo".into(),
        gh_repo.into(),
        "--json".into(),
        "state,url,mergeStateStatus,autoMergeRequest,statusCheckRollup".into(),
    ];
    let output = run_with_timeout_cmd(&gh, &args, 5)?;

    if !output.status.success() {
        bail!(
            "gh pr view failed: status={:?} stderr={}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let view: GhPrView = serde_json::from_slice(&output.stdout)
        .with_context(|| format!("parse gh pr view json for {gh_repo}#{pr}"))?;
    Ok(view)
}

fn gh_merge_method_flag(merge_method: &str) -> Result<&'static str> {
    match merge_method {
        "merge" => Ok("--merge"),
        "squash" => Ok("--squash"),
        "rebase" => Ok("--rebase"),
        other => bail!("invalid merge method: {other}"),
    }
}

fn gh_pr_arm_auto_merge(gh_repo: &str, pr: u64, merge_method: &str) -> Result<()> {
    let method_flag = gh_merge_method_flag(merge_method)?;

    let gh = gh_bin();
    let args: Vec<String> = vec![
        "pr".into(),
        "merge".into(),
        pr.to_string(),
        "--repo".into(),
        gh_repo.into(),
        "--auto".into(),
        method_flag.into(),
        "--delete-branch".into(),
    ];
    let output = run_with_timeout_cmd(&gh, &args, 5)?;

    if !output.status.success() {
        bail!(
            "gh pr merge --auto failed: status={:?} stderr={}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    Ok(())
}

fn gh_pr_merge_now(gh_repo: &str, pr: u64, merge_method: &str) -> Result<()> {
    let method_flag = gh_merge_method_flag(merge_method)?;

    let gh = gh_bin();
    let args: Vec<String> = vec![
        "pr".into(),
        "merge".into(),
        pr.to_string(),
        "--repo".into(),
        gh_repo.into(),
        method_flag.into(),
        "--delete-branch".into(),
    ];
    let output = run_with_timeout_cmd(&gh, &args, 5)?;

    if !output.status.success() {
        bail!(
            "gh pr merge failed: status={:?} stderr={}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    Ok(())
}

fn compute_backoff_sec(unchanged_polls: u32) -> i64 {
    match unchanged_polls {
        0 => 60,
        1 => 120,
        2 => 240,
        _ => 300,
    }
}

fn compute_auto_stop_reason(
    ticks: u64,
    max_ticks: Option<u64>,
    started_at: DateTime<Utc>,
    max_runtime_sec: Option<u64>,
    now: DateTime<Utc>,
) -> Option<String> {
    if let Some(limit) = max_ticks
        && ticks >= limit
    {
        return Some(format!("max_ticks reached ({ticks}/{limit})"));
    }

    if let Some(limit) = max_runtime_sec {
        let elapsed = (now - started_at).num_seconds().max(0) as u64;
        if elapsed >= limit {
            return Some(format!("max_runtime_sec reached ({elapsed}s/{limit}s)"));
        }
    }

    None
}

fn lease_window_sec(tick_sec: u64) -> i64 {
    // Keep detection reasonably fast while allowing scheduler jitter.
    // tick=60s -> lease=90s.
    std::cmp::max(45, tick_sec as i64 + 30)
}

fn process_matches_run(pid: u32, run_id: Uuid) -> bool {
    let cmdline_path = PathBuf::from(format!("/proc/{pid}/cmdline"));
    let data = match fs::read(cmdline_path) {
        Ok(d) => d,
        Err(_) => return false,
    };
    let cmdline = String::from_utf8_lossy(&data).replace('\0', " ");
    cmdline.contains("claw-loopd") && cmdline.contains(&run_id.to_string())
}

fn send_signal(pid: u32, signal: &str) {
    let _ = Command::new("kill")
        .arg(format!("-{signal}"))
        .arg(pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn stop_daemon_process_now(pid: u32, run_id: Uuid) {
    if !process_matches_run(pid, run_id) {
        return;
    }

    send_signal(pid, "TERM");
    std::thread::sleep(std::time::Duration::from_millis(200));
    if process_matches_run(pid, run_id) {
        send_signal(pid, "KILL");
    }
}

fn reduce_pr_tracking(run_dir: &Path, manifest: &Manifest, state: &mut State) -> Result<bool> {
    let tracking_path = pr_tracking_path(run_dir);
    if !tracking_path.exists() {
        return Ok(false);
    }

    if state.status != LoopStatus::Waiting {
        return Ok(false);
    }

    let mut tracking: PrTracking = read_json(&tracking_path)?;
    let now = Utc::now();
    if now < tracking.next_poll_at {
        return Ok(false);
    }

    let view = match gh_pr_view(&tracking.gh_repo, tracking.pr) {
        Ok(v) => v,
        Err(err) => {
            tracking.consecutive_errors += 1;
            tracking.last_error = Some(err.to_string());
            tracking.updated_at = now;
            tracking.last_polled_at = Some(now);
            let next = compute_backoff_sec(tracking.unchanged_polls.saturating_add(1));
            tracking.next_poll_at = now + chrono::Duration::seconds(next);
            write_json(&tracking_path, &tracking)?;
            append_event(
                run_dir,
                "pr_poll_error",
                serde_json::json!({
                    "repo": tracking.gh_repo,
                    "pr": tracking.pr,
                    "error": tracking.last_error,
                    "next_poll_at": tracking.next_poll_at,
                    "consecutive_errors": tracking.consecutive_errors
                }),
            )?;
            if tracking.consecutive_errors == 1 {
                queue_notification(
                    run_dir,
                    manifest,
                    "pr_poll_error",
                    format!(
                        "PR #{} poll failed once; will retry with backoff",
                        tracking.pr
                    ),
                )?;
            }
            return Ok(false);
        }
    };

    tracking.last_polled_at = Some(now);
    tracking.updated_at = now;
    tracking.consecutive_errors = 0;
    tracking.last_error = None;

    let pr_state = view.state.as_str();
    let merge_state = view.merge_state_status.clone().unwrap_or_default();
    let observed_changed = tracking.last_observed_state.as_deref() != Some(pr_state)
        || tracking.last_merge_state_status.as_deref() != Some(merge_state.as_str());

    match pr_state {
        "MERGED" => {
            state.version += 1;
            state.summary = format!("PR #{} merged", tracking.pr);
            state.waiting_reason = "ready for next loop".into();
            state.updated_at = now;

            append_event(
                run_dir,
                "pr_merged",
                serde_json::json!({
                    "repo": tracking.gh_repo,
                    "pr": tracking.pr,
                    "url": view.url,
                }),
            )?;
            queue_notification(
                run_dir,
                manifest,
                "pr_merged",
                format!("PR #{} merged: {}", tracking.pr, view.url),
            )?;
            fs::remove_file(&tracking_path)?;
            Ok(true)
        }
        "CLOSED" => {
            state.version += 1;
            state.status = LoopStatus::Blocked;
            state.summary = format!("PR #{} closed without merge", tracking.pr);
            state.waiting_reason = format!("PR #{} state=CLOSED", tracking.pr);
            state.updated_at = now;

            append_event(
                run_dir,
                "pr_closed",
                serde_json::json!({
                    "repo": tracking.gh_repo,
                    "pr": tracking.pr,
                    "url": view.url,
                }),
            )?;
            queue_notification(
                run_dir,
                manifest,
                "pr_closed",
                format!("PR #{} closed without merge: {}", tracking.pr, view.url),
            )?;
            fs::remove_file(&tracking_path)?;
            Ok(true)
        }
        "OPEN" => {
            if view.auto_merge_request.is_none()
                && merge_state == "CLEAN"
                && gh_pr_arm_auto_merge(&tracking.gh_repo, tracking.pr, &tracking.merge_method)
                    .is_ok()
            {
                tracking.auto_merge_armed = true;
                append_event(
                    run_dir,
                    "pr_auto_merge_armed",
                    serde_json::json!({
                        "repo": tracking.gh_repo,
                        "pr": tracking.pr,
                        "merge_method": tracking.merge_method,
                    }),
                )?;
            }

            if observed_changed {
                tracking.unchanged_polls = 0;
            } else {
                tracking.unchanged_polls = tracking.unchanged_polls.saturating_add(1);
            }

            tracking.last_observed_state = Some(pr_state.to_string());
            tracking.last_merge_state_status = Some(merge_state.clone());
            let next = compute_backoff_sec(tracking.unchanged_polls);
            tracking.next_poll_at = now + chrono::Duration::seconds(next);

            write_json(&tracking_path, &tracking)?;
            append_event(
                run_dir,
                "pr_open_polled",
                serde_json::json!({
                    "repo": tracking.gh_repo,
                    "pr": tracking.pr,
                    "merge_state": merge_state,
                    "next_poll_at": tracking.next_poll_at,
                    "unchanged_polls": tracking.unchanged_polls
                }),
            )?;
            Ok(false)
        }
        _ => {
            tracking.last_observed_state = Some(pr_state.to_string());
            tracking.last_merge_state_status = Some(merge_state);
            tracking.next_poll_at = now + chrono::Duration::seconds(300);
            write_json(&tracking_path, &tracking)?;

            append_event(
                run_dir,
                "pr_unknown_state",
                serde_json::json!({
                    "repo": tracking.gh_repo,
                    "pr": tracking.pr,
                    "state": pr_state,
                    "url": view.url,
                }),
            )?;
            Ok(false)
        }
    }
}

fn cmd_start(opts: StartOptions) -> Result<()> {
    if let Some(task_agent_id) = opts.task_agent_id.as_deref() {
        ensure_task_agent_exists(task_agent_id, &opts.repo)
            .with_context(|| format!("ensure task agent exists before start: {task_agent_id}"))?;
    }

    let task_file = opts.task_file.clone();
    let task_file_abs = resolve_task_file_path(&opts.repo, &task_file);
    let now = Utc::now();
    let (approval, approved_tasklist_hash) = if opts.require_task_approval {
        require_tasklist_approval(&task_file_abs, opts.approved_tasklist_hash.as_deref())?
    } else {
        (
            TaskApprovalMetadata {
                approved_by: "<disabled>".to_string(),
                approved_at: now,
            },
            task_plan_hash(&task_file_abs)?,
        )
    };
    let task_done_baseline = task_checklist_done_count(&task_file_abs)?;

    let run_id = Uuid::new_v4();
    let dir = run_dir(&opts.repo, run_id);
    fs::create_dir_all(&dir)?;

    let exe = std::env::current_exe().context("resolve current executable")?;
    let child = Command::new(exe)
        .arg("daemon")
        .arg("--repo")
        .arg(&opts.repo)
        .arg("--run-id")
        .arg(run_id.to_string())
        .arg("--tick-sec")
        .arg(opts.tick_sec.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("spawn daemon")?;

    let manifest = Manifest {
        run_id,
        repo_path: opts.repo.clone(),
        session_key: opts.session_key,
        channel: opts.channel,
        thread_id: opts.thread_id,
        owner_message_id: opts.owner_message_id,
        requester_user_id: opts.requester_user_id,
        task_agent_id: opts.task_agent_id,
        feedback_thread_id: opts.feedback_thread_id,
        feedback_channel: opts.feedback_channel,
        started_at: now,
        daemon_pid: child.id(),
        deliver_openclaw: opts.deliver_openclaw,
        max_ticks: opts.max_ticks,
        max_runtime_sec: opts.max_runtime_sec,
        max_task_loops: opts.max_task_loops,
        task_file,
        task_done_baseline,
        task_runner_cmd: opts.task_runner_cmd,
        approved_tasklist_hash,
        approved_by: approval.approved_by,
        approved_at: approval.approved_at,
        require_task_approval: opts.require_task_approval,
        auto_check_on_success: opts.auto_check_on_success,
        auto_recover_blocked: opts.auto_recover_blocked,
        auto_recover_blocked_max_attempts: opts.auto_recover_blocked_max_attempts,
        backlog_detector_file: opts.backlog_detector_file,
        backlog_detector_max_age_sec: opts.backlog_detector_max_age_sec,
        task_worktree_root: opts.task_worktree_root,
        task_worktrees: HashMap::new(),
    };
    let state = State {
        version: 1,
        status: LoopStatus::Running,
        summary: "daemon started".into(),
        waiting_reason: String::new(),
        lease_expires_at: now + chrono::Duration::seconds(lease_window_sec(opts.tick_sec)),
        updated_at: now,
        ticks: 0,
    };

    write_json(&dir.join("manifest.json"), &manifest)?;
    write_json(&dir.join("state.json"), &state)?;
    write_runner_state(&dir, &RunnerState::default())?;
    write_json(
        &dir.join("daemon.pid"),
        &serde_json::json!({"pid": child.id()}),
    )?;

    append_event(
        &dir,
        "daemon_started",
        serde_json::json!({"pid": child.id()}),
    )?;
    queue_notification(&dir, &manifest, "run_started", "loop daemon started")?;

    println!("run_id={}", run_id);
    println!("run_dir={}", dir.display());
    println!("daemon_pid={}", child.id());
    Ok(())
}

fn cmd_daemon(repo: PathBuf, run_id: Uuid, tick_sec: u64) -> Result<()> {
    let dir = run_dir(&repo, run_id);
    if !dir.exists() {
        bail!("run directory not found: {}", dir.display());
    }

    let manifest_path = dir.join("manifest.json");
    let mut manifest: Manifest = read_json(&manifest_path)?;
    let _daemon_lock = acquire_daemon_lock(&dir, run_id)?;

    if manifest.daemon_pid != process::id() {
        let old_pid = manifest.daemon_pid;
        manifest.daemon_pid = process::id();
        write_json(&manifest_path, &manifest)?;
        append_event(
            &dir,
            "daemon_pid_rebound",
            serde_json::json!({"old_pid": old_pid, "new_pid": manifest.daemon_pid}),
        )?;
    }

    append_event(
        &dir,
        "daemon_lock_acquired",
        serde_json::json!({"pid": process::id()}),
    )?;

    let reconcile_summary = reconcile_delivery_state(&dir)?;
    append_event(&dir, "delivery_reconciled", reconcile_summary)?;

    let control_stop = dir.join("control.stop");

    loop {
        if control_stop.exists() {
            let mut state: State = read_json(&dir.join("state.json"))?;
            state.version += 1;
            state.status = LoopStatus::Stopped;
            state.summary = "stopped by control file".into();
            state.updated_at = Utc::now();
            write_json(&dir.join("state.json"), &state)?;
            append_event(&dir, "daemon_stopped", serde_json::json!({}))?;
            let mention = completion_mention_prefix(&manifest);
            queue_notification(
                &dir,
                &manifest,
                "stopped",
                format!("{mention}loop daemon stopped"),
            )?;
            let _ = flush_notifications(&dir, &manifest)?;
            break;
        }

        let mut state: State = read_json(&dir.join("state.json"))?;
        let now = Utc::now();

        let task_file_abs = resolve_task_file_path(&manifest.repo_path, &manifest.task_file);
        let approval_violation = tasklist_approval_violation_reason(&task_file_abs, &manifest);
        if !approval_violation.is_empty() {
            state.version += 1;
            state.status = LoopStatus::Blocked;
            state.summary = "tasklist approval invalidated".into();
            state.waiting_reason = approval_violation.clone();
            state.updated_at = now;
            write_json(&dir.join("state.json"), &state)?;
            append_event(
                &dir,
                "tasklist_approval_invalidated",
                serde_json::json!({
                    "reason": approval_violation,
                    "task_file": task_file_abs,
                }),
            )?;
            let mention = completion_mention_prefix(&manifest);
            queue_notification(
                &dir,
                &manifest,
                "blocked",
                format!(
                    "{mention}run blocked: tasklist approval invalidated\nrecovery: rerun `claw-loopd task-approve --file {}` and restart with the new --approved-tasklist-hash",
                    manifest.task_file.display()
                ),
            )?;
            let _ = flush_notifications(&dir, &manifest)?;
            break;
        }

        let mut task_done_now = match task_checklist_done_count(&task_file_abs) {
            Ok(v) => v,
            Err(err) => {
                append_event(
                    &dir,
                    "task_count_error",
                    serde_json::json!({
                        "task_file": task_file_abs,
                        "error": err.to_string(),
                    }),
                )?;
                manifest.task_done_baseline
            }
        };
        let mut task_loops_completed = task_done_now.saturating_sub(manifest.task_done_baseline);
        let mut runner_state = read_runner_state(&dir)?;

        if let Some(cmd) = manifest.task_runner_cmd.clone() {
            if runner_state.paused {
                state.status = LoopStatus::Waiting;
                state.summary = "runner paused".into();
                state.waiting_reason = runner_state
                    .pause_reason
                    .clone()
                    .unwrap_or_else(|| "runner paused".to_string());
            } else {
                if let Some(active_id) = runner_state.current_task_id.clone() {
                    let (_, _, entries) = load_task_checklist(&task_file_abs)?;
                    match entries.iter().find(|entry| entry.id == active_id) {
                        Some(entry) if entry.done => {
                            append_event(
                                &dir,
                                "task_completed_detected",
                                serde_json::json!({
                                    "task_id": entry.id,
                                    "line": entry.line_no,
                                    "text": entry.text,
                                }),
                            )?;
                            let elapsed_suffix = task_elapsed_suffix(
                                runner_state.current_task_started_at.as_ref(),
                                now,
                            );
                            runner_state.last_task_id = Some(entry.id.clone());
                            runner_state.last_task_state = Some(RunnerTaskState::Done);
                            runner_state.last_task_at = Some(now);
                            runner_state.last_task_reason = Some("checklist marked done".into());
                            runner_state.last_task_pr_url =
                                runner_state.current_task_pr_url.clone();
                            runner_state.current_task_id = None;
                            runner_state.current_task_text = None;
                            runner_state.current_task_line = None;
                            runner_state.current_task_started_at = None;
                            runner_state.current_task_state = None;
                            clear_current_blocked_context(&mut runner_state);
                            clear_current_waiting_dependency(&mut runner_state);
                            runner_state.current_task_pr_url = None;
                            write_runner_state(&dir, &runner_state)?;
                            task_done_now = task_checklist_done_count(&task_file_abs)?;
                            task_loops_completed =
                                task_done_now.saturating_sub(manifest.task_done_baseline);

                            let pr_suffix = runner_state
                                .last_task_pr_url
                                .clone()
                                .map(|u| format!(" PR_URL={u}"))
                                .unwrap_or_default();
                            queue_notification(
                                &dir,
                                &manifest,
                                "task_done",
                                format!(
                                    "task {} marked done in checklist; done={}{}{}",
                                    entry.id, task_done_now, pr_suffix, elapsed_suffix
                                ),
                            )?;
                        }
                        Some(entry) => {
                            state.status = LoopStatus::Waiting;
                            match runner_state.current_task_state {
                                Some(RunnerTaskState::WaitingMerge) => {
                                    if let Some(pr_url) = runner_state.current_task_pr_url.clone() {
                                        match ensure_waiting_merge_progress(&pr_url) {
                                            Ok(WaitingMergeProgress::Merged) => {
                                                let check_result = update_task_check(
                                                    &task_file_abs,
                                                    &entry.id,
                                                    true,
                                                )?;
                                                task_done_now =
                                                    task_checklist_done_count(&task_file_abs)?;
                                                task_loops_completed = task_done_now
                                                    .saturating_sub(manifest.task_done_baseline);

                                                let elapsed_suffix = task_elapsed_suffix(
                                                    runner_state.current_task_started_at.as_ref(),
                                                    now,
                                                );
                                                runner_state.last_task_id = Some(entry.id.clone());
                                                runner_state.last_task_state =
                                                    Some(RunnerTaskState::Done);
                                                runner_state.last_task_at = Some(now);
                                                runner_state.last_task_reason =
                                                    Some("PR merged while waiting_merge".into());
                                                runner_state.last_task_pr_url =
                                                    Some(pr_url.clone());
                                                track_task_pr_url(
                                                    &mut runner_state,
                                                    Some(entry.id.as_str()),
                                                    Some(pr_url.as_str()),
                                                );
                                                runner_state.current_task_id = None;
                                                runner_state.current_task_text = None;
                                                runner_state.current_task_line = None;
                                                runner_state.current_task_started_at = None;
                                                runner_state.current_task_state = None;
                                                clear_current_blocked_context(&mut runner_state);
                                                clear_current_waiting_dependency(&mut runner_state);
                                                runner_state.current_task_pr_url = None;

                                                append_event(
                                                    &dir,
                                                    "task_waiting_merge_resolved",
                                                    serde_json::json!({
                                                        "task_id": entry.id,
                                                        "pr_url": pr_url,
                                                        "check_result": check_result,
                                                        "done": task_done_now,
                                                    }),
                                                )?;

                                                queue_notification(
                                                    &dir,
                                                    &manifest,
                                                    "task_done",
                                                    format!(
                                                        "task {} merged while waiting; done={} PR_URL={}{}",
                                                        entry.id,
                                                        task_done_now,
                                                        pr_url,
                                                        elapsed_suffix
                                                    ),
                                                )?;

                                                state.status = LoopStatus::Running;
                                                state.summary =
                                                    format!("task merged: {}", entry.id);
                                                state.waiting_reason = format!(
                                                    "merge confirmed for task {}",
                                                    entry.id
                                                );
                                            }
                                            Ok(WaitingMergeProgress::Waiting) => {
                                                state.summary =
                                                    format!("task waiting_merge: {}", entry.id);
                                                state.waiting_reason =
                                                    format!("TASK_WAITING_MERGE PR_URL={pr_url}");
                                            }
                                            Ok(WaitingMergeProgress::Blocked(reason)) => {
                                                let blocked_context =
                                                    build_waiting_merge_blocked_context(
                                                        entry,
                                                        Some(&pr_url),
                                                        &reason,
                                                        now,
                                                    );
                                                apply_blocked_context(
                                                    &mut runner_state,
                                                    &mut state,
                                                    &blocked_context,
                                                    now,
                                                );

                                                append_event(
                                                    &dir,
                                                    "task_waiting_merge_blocked",
                                                    serde_json::json!({
                                                        "task_id": entry.id,
                                                        "pr_url": pr_url,
                                                        "reason": blocked_context.reason_summary,
                                                        "blocked_context": blocked_context,
                                                    }),
                                                )?;

                                                let blocked_context = runner_state
                                                    .last_blocked_context
                                                    .clone()
                                                    .expect("blocked context recorded");
                                                let blocked_message =
                                                    format_task_blocked_notification(
                                                        &manifest,
                                                        &blocked_context,
                                                    );
                                                queue_notification(
                                                    &dir,
                                                    &manifest,
                                                    "task_blocked",
                                                    blocked_message,
                                                )?;

                                                match maybe_auto_recover_blocked_task(
                                                    &dir,
                                                    &manifest_path,
                                                    &mut manifest,
                                                    &task_file_abs,
                                                    &mut runner_state,
                                                    &mut state,
                                                    &blocked_context,
                                                    now,
                                                    &mut task_done_now,
                                                    &mut task_loops_completed,
                                                )? {
                                                    AutoRecoverBlockedOutcome::Recovered => {
                                                        continue;
                                                    }
                                                    AutoRecoverBlockedOutcome::Halted => break,
                                                    AutoRecoverBlockedOutcome::NotTriggered => {}
                                                }
                                            }
                                            Ok(WaitingMergeProgress::Retryable(reason)) => {
                                                let reason = clip_text(&reason, 200);
                                                runner_state.current_task_state =
                                                    Some(RunnerTaskState::WaitingMerge);
                                                clear_current_blocked_context(&mut runner_state);
                                                clear_current_waiting_dependency(&mut runner_state);
                                                state.summary =
                                                    format!("task waiting_merge: {}", entry.id);
                                                state.waiting_reason = reason.clone();
                                                state.updated_at = now;

                                                append_event(
                                                    &dir,
                                                    "task_waiting_merge_retryable_error",
                                                    serde_json::json!({
                                                        "task_id": entry.id,
                                                        "pr_url": pr_url,
                                                        "reason": reason,
                                                    }),
                                                )?;
                                            }
                                            Err(err) => {
                                                let err_text = clip_text(&err.to_string(), 200);
                                                let blocked_context =
                                                    build_waiting_merge_blocked_context(
                                                        entry,
                                                        Some(&pr_url),
                                                        &format!(
                                                            "waiting_merge transition failed: {}",
                                                            err_text
                                                        ),
                                                        now,
                                                    );
                                                apply_blocked_context(
                                                    &mut runner_state,
                                                    &mut state,
                                                    &blocked_context,
                                                    now,
                                                );

                                                append_event(
                                                    &dir,
                                                    "task_waiting_merge_transition_failed",
                                                    serde_json::json!({
                                                        "task_id": entry.id,
                                                        "pr_url": pr_url,
                                                        "error": err_text,
                                                        "blocked_context": blocked_context,
                                                    }),
                                                )?;

                                                let blocked_context = runner_state
                                                    .last_blocked_context
                                                    .clone()
                                                    .expect("blocked context recorded");
                                                let blocked_message =
                                                    format_task_blocked_notification(
                                                        &manifest,
                                                        &blocked_context,
                                                    );
                                                queue_notification(
                                                    &dir,
                                                    &manifest,
                                                    "task_blocked",
                                                    blocked_message,
                                                )?;
                                            }
                                        }
                                    } else {
                                        state.summary = format!("task waiting_merge: {}", entry.id);
                                        state.waiting_reason =
                                            format!("TASK_WAITING_MERGE ({})", entry.id);
                                    }
                                }
                                Some(RunnerTaskState::WaitingDependency) => {
                                    let waiting_dependency = runner_state
                                        .current_waiting_dependency
                                        .as_ref()
                                        .or_else(|| {
                                            runner_state
                                                .last_waiting_dependency
                                                .as_ref()
                                                .filter(|context| context.task_id == entry.id)
                                        })
                                        .cloned();

                                    if let Some(waiting_dependency) = waiting_dependency {
                                        match ensure_waiting_dependency_progress(
                                            &waiting_dependency,
                                            &entries,
                                            &runner_state,
                                        ) {
                                            Ok(WaitingDependencyProgress::Resolved {
                                                context,
                                                resolution,
                                            }) => {
                                                runner_state.last_waiting_dependency =
                                                    Some(context.clone());
                                                runner_state.current_task_id = None;
                                                runner_state.current_task_text = None;
                                                runner_state.current_task_line = None;
                                                runner_state.current_task_started_at = None;
                                                runner_state.current_task_state = None;
                                                clear_current_blocked_context(&mut runner_state);
                                                clear_current_waiting_dependency(&mut runner_state);
                                                runner_state.current_task_pr_url = None;
                                                runner_state.preferred_next_task_id =
                                                    Some(entry.id.clone());

                                                append_event(
                                                    &dir,
                                                    "task_waiting_dependency_resolved",
                                                    serde_json::json!({
                                                        "task_id": entry.id,
                                                        "depends_on_task": context.depends_on_task,
                                                        "depends_on_pr_url": context.depends_on_pr_url,
                                                        "resolution": resolution.clone(),
                                                    }),
                                                )?;

                                                queue_notification(
                                                    &dir,
                                                    &manifest,
                                                    "task_progress",
                                                    format!(
                                                        "task dependency cleared: {}; {} — rerunning now",
                                                        entry.id, resolution
                                                    ),
                                                )?;

                                                state.status = LoopStatus::Running;
                                                state.summary = format!(
                                                    "task dependency cleared: {}",
                                                    entry.id
                                                );
                                                state.waiting_reason = resolution.clone();
                                            }
                                            Ok(WaitingDependencyProgress::Waiting(context)) => {
                                                runner_state.current_waiting_dependency =
                                                    Some(context.clone());
                                                runner_state.last_waiting_dependency =
                                                    Some(context.clone());
                                                state.summary = format!(
                                                    "task waiting_dependency: {}",
                                                    context.task_id
                                                );
                                                state.waiting_reason =
                                                    clip_text(&context.contract_line, 200);
                                            }
                                            Err(err) => {
                                                let err_text = clip_text(&err.to_string(), 200);
                                                append_event(
                                                    &dir,
                                                    "task_waiting_dependency_check_failed",
                                                    serde_json::json!({
                                                        "task_id": entry.id,
                                                        "error": err_text,
                                                        "waiting_dependency": waiting_dependency,
                                                    }),
                                                )?;
                                                state.summary = format!(
                                                    "task waiting_dependency: {}",
                                                    entry.id
                                                );
                                                state.waiting_reason = clip_text(
                                                    &waiting_dependency.contract_line,
                                                    200,
                                                );
                                            }
                                        }
                                    } else {
                                        state.summary =
                                            format!("task waiting_dependency: {}", entry.id);
                                        state.waiting_reason =
                                            format!("TASK_WAITING_DEPENDENCY TASK_ID={}", entry.id);
                                    }
                                }
                                Some(RunnerTaskState::Blocked) => {
                                    state.summary = format!("task blocked: {}", entry.id);
                                    state.waiting_reason = runner_state
                                        .current_task_blocked_reason
                                        .clone()
                                        .unwrap_or_else(|| {
                                            format!("task blocked without reason: {}", entry.id)
                                        });
                                }
                                _ => {
                                    state.summary = format!("task running: {}", entry.id);
                                    state.waiting_reason =
                                        format!("waiting for task completion: {}", entry.id);
                                }
                            }
                        }
                        None => {
                            state.status = LoopStatus::Blocked;
                            state.summary =
                                format!("active task missing from tasklist: {active_id}");
                            state.waiting_reason = "runner state is inconsistent".into();
                            state.updated_at = now;
                            state.version += 1;
                            runner_state.current_task_state = Some(RunnerTaskState::Blocked);
                            clear_current_blocked_context(&mut runner_state);
                            clear_current_waiting_dependency(&mut runner_state);
                            runner_state.current_task_blocked_reason =
                                Some("task missing from checklist".into());
                            runner_state.last_task_id = Some(active_id.clone());
                            runner_state.last_task_state = Some(RunnerTaskState::Blocked);
                            runner_state.last_task_at = Some(now);
                            runner_state.last_task_reason =
                                Some("task missing from checklist".into());
                            runner_state.last_task_pr_url =
                                runner_state.current_task_pr_url.clone();
                            write_runner_state(&dir, &runner_state)?;
                            write_json(&dir.join("state.json"), &state)?;
                            let _ = flush_notifications(&dir, &manifest)?;
                            break;
                        }
                    }
                }

                if runner_state.current_task_id.is_none() {
                    if let Some(reason) = missing_current_task_id_guard_reason(
                        runner_state.current_task_state.as_ref(),
                    ) {
                        runner_state.paused = false;
                        runner_state.pause_reason = None;
                        state.status = LoopStatus::Blocked;
                        state.summary = "runner state guardrail blocked".into();
                        state.waiting_reason = reason.clone();
                        state.updated_at = now;
                        state.version += 1;
                        write_runner_state(&dir, &runner_state)?;
                        write_json(&dir.join("state.json"), &state)?;
                        append_event(
                            &dir,
                            "runner_state_guardrail_blocked",
                            serde_json::json!({
                                "reason": reason,
                                "current_task_state": runner_state
                                    .current_task_state
                                    .as_ref()
                                    .map(RunnerTaskState::as_str),
                                "current_task_text": runner_state.current_task_text.clone(),
                                "current_waiting_dependency": runner_state.current_waiting_dependency.clone(),
                                "current_blocked_context": runner_state.current_blocked_context.clone(),
                            }),
                        )?;
                        queue_notification(
                            &dir,
                            &manifest,
                            "blocked",
                            format!(
                                "run blocked: runner state guardrail triggered\n- reason: {}\n- recovery: inspect runner-state.json and rerun the task so waiting_merge / dependency_wait / blocked state keeps its task id",
                                state.waiting_reason
                            ),
                        )?;
                        let _ = flush_notifications(&dir, &manifest)?;
                        break;
                    }

                    let (_, _, entries) = load_task_checklist(&task_file_abs)?;
                    let preferred_next_task_id = runner_state
                        .preferred_next_task_id
                        .as_deref()
                        .or(runner_state.auto_recover_last_task_id.as_deref());
                    let selection = match manifest.backlog_detector_file.as_deref() {
                        Some(detector_file) => match read_backlog_snapshot(
                            &manifest.repo_path,
                            detector_file,
                            manifest.backlog_detector_max_age_sec,
                            now,
                        ) {
                            Ok(backlog_snapshot) => select_next_task_with_backlog(
                                &entries,
                                preferred_next_task_id,
                                Some(&backlog_snapshot),
                            ),
                            Err(err) => TaskSelectionOutcome::Blocked {
                                summary: "backlog detector unavailable".to_string(),
                                reason: format!(
                                    "failure-first gate blocked: {}",
                                    clip_text(&err.to_string(), 200)
                                ),
                            },
                        },
                        None => {
                            select_next_task_with_backlog(&entries, preferred_next_task_id, None)
                        }
                    };

                    if runner_state.task_loops_started >= manifest.max_task_loops {
                        runner_state.paused = true;
                        runner_state.current_task_state = None;
                        clear_current_blocked_context(&mut runner_state);
                        clear_current_waiting_dependency(&mut runner_state);
                        runner_state.current_task_pr_url = None;
                        runner_state.pause_reason = Some(format!(
                            "max_task_loops reached ({}/{})",
                            runner_state.task_loops_started, manifest.max_task_loops
                        ));
                        write_runner_state(&dir, &runner_state)?;
                        state.status = LoopStatus::Waiting;
                        state.summary = "runner paused at max_task_loops".into();
                        state.waiting_reason = runner_state
                            .pause_reason
                            .clone()
                            .unwrap_or_else(|| "max_task_loops reached".to_string());
                        queue_notification(
                            &dir,
                            &manifest,
                            "loop_limit_reached",
                            state.waiting_reason.clone(),
                        )?;
                    } else if matches!(selection, TaskSelectionOutcome::None) {
                        runner_state.paused = true;
                        runner_state.current_task_state = None;
                        clear_current_blocked_context(&mut runner_state);
                        clear_current_waiting_dependency(&mut runner_state);
                        runner_state.current_task_pr_url = None;
                        runner_state.pause_reason = Some("all tasklist items completed".into());
                        write_runner_state(&dir, &runner_state)?;
                        state.status = LoopStatus::Stopped;
                        state.summary = "all tasklist items completed".into();
                        state.waiting_reason =
                            "all tasklist items completed; daemon stopped".into();
                        state.updated_at = now;
                        state.version += 1;
                        write_json(&dir.join("state.json"), &state)?;

                        emit_all_tasks_completed_notifications(
                            &dir,
                            &manifest,
                            &runner_state,
                            task_done_now,
                            now,
                        )?;

                        append_event(
                            &dir,
                            "daemon_stopped_all_tasks_completed",
                            serde_json::json!({
                                "task_done_now": task_done_now,
                                "task_done_baseline": manifest.task_done_baseline,
                                "task_loops_started": runner_state.task_loops_started,
                            }),
                        )?;
                        break;
                    } else {
                        match selection {
                            TaskSelectionOutcome::Next(queued_task) => {
                                if runner_state.preferred_next_task_id.as_deref()
                                    == Some(queued_task.id.as_str())
                                {
                                    runner_state.preferred_next_task_id = None;
                                }
                                if runner_state.auto_recover_last_task_id.as_deref()
                                    == Some(queued_task.id.as_str())
                                {
                                    runner_state.auto_recover_last_task_id = None;
                                }
                                runner_state.preferred_next_task_id = None;
                                runner_state.current_task_id = Some(queued_task.id.clone());
                                runner_state.current_task_text = Some(queued_task.text.clone());
                                runner_state.current_task_line = Some(queued_task.line_no);
                                runner_state.current_task_started_at = Some(now);
                                runner_state.current_task_state = Some(RunnerTaskState::Queued);
                                clear_current_blocked_context(&mut runner_state);
                                clear_current_waiting_dependency(&mut runner_state);
                                runner_state.current_task_pr_url = None;
                                runner_state.paused = false;
                                runner_state.pause_reason = None;
                                write_runner_state(&dir, &runner_state)?;

                                runner_state.current_task_state = Some(RunnerTaskState::Running);
                                write_runner_state(&dir, &runner_state)?;

                                queue_notification(
                                    &dir,
                                    &manifest,
                                    "task_started",
                                    format!(
                                        "task started: {} (line {})",
                                        queued_task.id, queued_task.line_no
                                    ),
                                )?;

                                let task_kind = classify_task_execution_kind(&queued_task);
                                let worktree = ensure_task_worktree(
                                    &manifest.repo_path,
                                    run_id,
                                    &manifest.task_worktree_root,
                                    &queued_task,
                                    now,
                                )?;
                                runner_state.current_worktree = Some(worktree.clone());
                                write_runner_state(&dir, &runner_state)?;
                                manifest
                                    .task_worktrees
                                    .insert(queued_task.id.clone(), worktree.clone());
                                write_json(&manifest_path, &manifest)?;
                                append_event(
                                    &dir,
                                    "task_worktree_created",
                                    serde_json::json!({
                                        "task_id": queued_task.id,
                                        "path": worktree.path,
                                        "branch": worktree.branch,
                                        "base_branch": worktree.base_branch,
                                        "cleanup_policy": worktree.cleanup_policy,
                                        "state": worktree.state,
                                    }),
                                )?;
                                let backlog_snapshot = manifest
                                    .backlog_detector_file
                                    .as_deref()
                                    .and_then(|detector_file| {
                                        read_backlog_snapshot(
                                            &manifest.repo_path,
                                            detector_file,
                                            manifest.backlog_detector_max_age_sec,
                                            now,
                                        )
                                        .ok()
                                    });

                                let mut runner = run_task_once(TaskRunOptions {
                                    task_file: &task_file_abs,
                                    selected_task: Some(queued_task.clone()),
                                    cmd: &cmd,
                                    auto_check_on_success: manifest.auto_check_on_success,
                                    dry_run: false,
                                    cwd: Some(&worktree.path),
                                    run_id: Some(run_id),
                                    thread_id: Some(&manifest.thread_id),
                                    channel: Some(&manifest.channel),
                                    task_agent_id: manifest.task_agent_id.as_deref(),
                                    task_kind: Some(task_kind),
                                    backlog_snapshot: backlog_snapshot.as_ref(),
                                    worktree: Some(&worktree),
                                })?;

                                append_event(
                                    &dir,
                                    "task_runner_tick",
                                    serde_json::json!({
                                        "task": runner.task.as_ref().map(|t| serde_json::json!({
                                            "id": t.id,
                                            "line": t.line_no,
                                            "text": t.text,
                                        })),
                                        "command": runner.command,
                                        "executed": runner.executed,
                                        "success": runner.success,
                                        "exit_code": runner.exit_code,
                                        "auto_check_on_success": manifest.auto_check_on_success,
                                        "worktree": runner_state.current_worktree.clone(),
                                        "check_result": runner.check_result,
                                        "stdout": clip_text(&runner.stdout, 1000),
                                        "stderr": clip_text(&runner.stderr, 1000),
                                    }),
                                )?;

                                let first_stdout_line = task_contract_line(&runner.stdout)
                                    .unwrap_or_else(|| {
                                        runner
                                            .stdout
                                            .lines()
                                            .map(str::trim)
                                            .find(|line| !line.is_empty())
                                            .unwrap_or("")
                                            .to_string()
                                    });
                                let first_line_pr_url = extract_pr_url(&first_stdout_line);
                                let task_label = runner
                                    .task
                                    .as_ref()
                                    .map(|t| t.id.clone())
                                    .unwrap_or_else(|| "unknown".to_string());

                                let waiting_contract = match parse_waiting_contract(
                                    &first_stdout_line,
                                    runner.exit_code,
                                    runner.task.as_ref().map(|task| task.id.as_str()),
                                ) {
                                    Ok(contract) => contract,
                                    Err(err) => {
                                        runner.success = false;
                                        if runner.exit_code.is_none() || runner.exit_code == Some(0)
                                        {
                                            runner.exit_code = Some(65);
                                        }
                                        if !runner.stderr.trim().is_empty() {
                                            runner.stderr.push('\n');
                                        }
                                        runner.stderr.push_str(&format!(
                                            "waiting contract failed: {}",
                                            err.to_string().replace('\n', " ")
                                        ));
                                        None
                                    }
                                };

                                if let Some(waiting_contract) = waiting_contract {
                                    state.version += 1;
                                    state.status = LoopStatus::Waiting;
                                    state.updated_at = now;

                                    let mut notification_kind = "task_waiting_merge";
                                    let notification_message;

                                    match waiting_contract {
                                        WaitingContract::WaitingMerge {
                                            contract_line,
                                            pr_url,
                                        } => {
                                            state.summary =
                                                format!("task waiting_merge: {task_label}");
                                            state.waiting_reason =
                                                contract_line.unwrap_or_else(|| {
                                                    format!("task waiting_merge: {task_label}")
                                                });

                                            runner_state.current_task_state =
                                                Some(RunnerTaskState::WaitingMerge);
                                            clear_current_blocked_context(&mut runner_state);
                                            clear_current_waiting_dependency(&mut runner_state);
                                            runner_state.current_task_pr_url = pr_url.clone();
                                            track_task_pr_url(
                                                &mut runner_state,
                                                Some(task_label.as_str()),
                                                pr_url.as_deref(),
                                            );

                                            let required_checks_missing = first_stdout_line
                                                .contains("WARN_REQUIRED_CHECKS_MISSING=1");
                                            notification_message =
                                                format_waiting_merge_notification(
                                                    &task_label,
                                                    pr_url.as_deref(),
                                                    required_checks_missing,
                                                );
                                        }
                                        WaitingContract::Other { contract_line } => {
                                            state.summary =
                                                format!("task waiting_merge: {task_label}");
                                            state.waiting_reason =
                                                contract_line.unwrap_or_else(|| {
                                                    format!("task waiting_merge: {task_label}")
                                                });

                                            runner_state.current_task_state =
                                                Some(RunnerTaskState::WaitingMerge);
                                            clear_current_blocked_context(&mut runner_state);
                                            clear_current_waiting_dependency(&mut runner_state);
                                            runner_state.current_task_pr_url = None;

                                            notification_message =
                                                format_waiting_merge_notification(
                                                    &task_label,
                                                    None,
                                                    false,
                                                );
                                        }
                                        WaitingContract::WaitingDependency(context) => {
                                            let context = enrich_waiting_dependency_context(
                                                &context,
                                                &runner_state,
                                            );
                                            state.summary = format!(
                                                "task waiting_dependency: {}",
                                                context.task_id
                                            );
                                            state.waiting_reason =
                                                clip_text(&context.contract_line, 200);

                                            runner_state.current_task_state =
                                                Some(RunnerTaskState::WaitingDependency);
                                            clear_current_blocked_context(&mut runner_state);
                                            runner_state.current_task_pr_url = None;
                                            runner_state.current_waiting_dependency =
                                                Some(context.clone());
                                            runner_state.last_waiting_dependency =
                                                Some(context.clone());

                                            append_event(
                                                &dir,
                                                "task_waiting_dependency",
                                                serde_json::json!({
                                                    "task_id": context.task_id.clone(),
                                                    "depends_on_task": context.depends_on_task.clone(),
                                                    "depends_on_pr_url": context.depends_on_pr_url.clone(),
                                                    "contract_line": context.contract_line.clone(),
                                                    "auto_recover": false,
                                                }),
                                            )?;

                                            notification_kind = "task_waiting_dependency";
                                            notification_message =
                                                format_waiting_dependency_notification(
                                                    &task_label,
                                                    &context,
                                                );
                                        }
                                    }

                                    write_runner_state(&dir, &runner_state)?;
                                    write_json(&dir.join("state.json"), &state)?;

                                    queue_notification(
                                        &dir,
                                        &manifest,
                                        notification_kind,
                                        notification_message,
                                    )?;
                                } else if !runner.success {
                                    let blocked_context = build_runner_blocked_context(
                                        runner.task.as_ref(),
                                        runner.exit_code,
                                        &runner.stdout,
                                        &runner.stderr,
                                        first_line_pr_url.as_deref(),
                                        now,
                                    );
                                    apply_blocked_context(
                                        &mut runner_state,
                                        &mut state,
                                        &blocked_context,
                                        now,
                                    );
                                    state.summary = format!("task runner failed: {}", task_label);
                                    write_runner_state(&dir, &runner_state)?;
                                    append_event(
                                        &dir,
                                        "task_runner_blocked",
                                        serde_json::json!({
                                            "task_label": task_label.clone(),
                                            "blocked_context": runner_state.last_blocked_context.clone(),
                                        }),
                                    )?;

                                    let blocked_context = runner_state
                                        .last_blocked_context
                                        .clone()
                                        .expect("blocked context recorded");
                                    let blocked_message = format_task_blocked_notification(
                                        &manifest,
                                        &blocked_context,
                                    );
                                    queue_notification(
                                        &dir,
                                        &manifest,
                                        "task_blocked",
                                        blocked_message,
                                    )?;

                                    if manifest.auto_recover_blocked {
                                        if runner.task.is_some() {
                                            match maybe_auto_recover_blocked_task(
                                                &dir,
                                                &manifest_path,
                                                &mut manifest,
                                                &task_file_abs,
                                                &mut runner_state,
                                                &mut state,
                                                &blocked_context,
                                                now,
                                                &mut task_done_now,
                                                &mut task_loops_completed,
                                            )? {
                                                AutoRecoverBlockedOutcome::Recovered => continue,
                                                AutoRecoverBlockedOutcome::Halted => break,
                                                AutoRecoverBlockedOutcome::NotTriggered => {}
                                            }
                                        } else {
                                            append_event(
                                                &dir,
                                                "task_blocked_auto_recover_skipped",
                                                serde_json::json!({
                                                    "reason": "runner.task is missing",
                                                    "task_label": task_label,
                                                    "blocked_context": blocked_context,
                                                }),
                                            )?;
                                        }
                                    }

                                    write_json(&dir.join("state.json"), &state)?;
                                    let _ = flush_notifications(&dir, &manifest)?;
                                    break;
                                }

                                if runner.success
                                    && let Some(task) = runner.task
                                {
                                    runner_state.task_loops_started =
                                        runner_state.task_loops_started.saturating_add(1);

                                    if manifest.auto_check_on_success {
                                        let elapsed_suffix = task_elapsed_suffix(
                                            runner_state.current_task_started_at.as_ref(),
                                            now,
                                        );
                                        runner_state.last_task_id = Some(task.id.clone());
                                        runner_state.last_task_state = Some(RunnerTaskState::Done);
                                        runner_state.last_task_at = Some(now);
                                        runner_state.last_task_reason =
                                            Some("runner success + auto-check".into());
                                        runner_state.last_task_pr_url = first_line_pr_url.clone();
                                        runner_state.last_worktree =
                                            runner_state.current_worktree.clone();
                                        track_task_pr_url(
                                            &mut runner_state,
                                            Some(task.id.as_str()),
                                            first_line_pr_url.as_deref(),
                                        );
                                        runner_state.current_task_id = None;
                                        runner_state.current_task_text = None;
                                        runner_state.current_task_line = None;
                                        runner_state.current_task_started_at = None;
                                        runner_state.current_task_state = None;
                                        clear_current_blocked_context(&mut runner_state);
                                        clear_current_waiting_dependency(&mut runner_state);
                                        runner_state.current_task_pr_url = None;
                                        runner_state.current_worktree = None;

                                        task_done_now = task_checklist_done_count(&task_file_abs)?;
                                        task_loops_completed = task_done_now
                                            .saturating_sub(manifest.task_done_baseline);

                                        let pr_suffix = first_line_pr_url
                                            .clone()
                                            .map(|u| format!(" PR_URL={u}"))
                                            .unwrap_or_default();
                                        queue_notification(
                                            &dir,
                                            &manifest,
                                            "task_done",
                                            format!(
                                                "task done: {} (done={}){}{}",
                                                task.id, task_done_now, pr_suffix, elapsed_suffix
                                            ),
                                        )?;
                                    } else {
                                        runner_state.current_task_id = Some(task.id.clone());
                                        runner_state.current_task_text = Some(task.text.clone());
                                        runner_state.current_task_line = Some(task.line_no);
                                        runner_state.current_task_started_at = Some(now);
                                        runner_state.current_task_state =
                                            Some(RunnerTaskState::Running);
                                        clear_current_blocked_context(&mut runner_state);
                                        clear_current_waiting_dependency(&mut runner_state);
                                        runner_state.current_task_pr_url =
                                            first_line_pr_url.clone();
                                        track_task_pr_url(
                                            &mut runner_state,
                                            Some(task.id.as_str()),
                                            first_line_pr_url.as_deref(),
                                        );
                                        state.status = LoopStatus::Waiting;
                                        state.summary = format!("task running: {}", task.id);
                                        state.waiting_reason =
                                            format!("waiting for task completion: {}", task.id);

                                        queue_notification(
                                            &dir,
                                            &manifest,
                                            "task_progress",
                                            format!(
                                                "task in progress: {} (waiting for checklist completion)",
                                                task.id
                                            ),
                                        )?;
                                    }
                                    write_runner_state(&dir, &runner_state)?;
                                }
                            }
                            TaskSelectionOutcome::Waiting {
                                summary,
                                reason,
                                backlog_snapshot,
                            } => {
                                let should_notify = state.status != LoopStatus::Waiting
                                    || state.summary != summary
                                    || state.waiting_reason != reason;
                                runner_state.current_task_state = None;
                                clear_current_blocked_context(&mut runner_state);
                                clear_current_waiting_dependency(&mut runner_state);
                                runner_state.current_task_pr_url = None;
                                runner_state.paused = false;
                                runner_state.pause_reason = None;
                                state.status = LoopStatus::Waiting;
                                state.summary = summary.clone();
                                state.waiting_reason = reason.clone();
                                state.updated_at = now;
                                if should_notify {
                                    state.version += 1;
                                    append_event(
                                        &dir,
                                        "task_selection_backlog_waiting",
                                        serde_json::json!({
                                            "summary": summary,
                                            "reason": reason,
                                            "backlog_snapshot": backlog_snapshot,
                                        }),
                                    )?;
                                    queue_notification(
                                        &dir,
                                        &manifest,
                                        "task_progress",
                                        format_backlog_gate_notification(
                                            state.summary.as_str(),
                                            state.waiting_reason.as_str(),
                                        ),
                                    )?;
                                }
                            }
                            TaskSelectionOutcome::Blocked { summary, reason } => {
                                let should_notify = state.status != LoopStatus::Blocked
                                    || state.summary != summary
                                    || state.waiting_reason != reason;
                                runner_state.current_task_state = None;
                                clear_current_blocked_context(&mut runner_state);
                                clear_current_waiting_dependency(&mut runner_state);
                                runner_state.current_task_pr_url = None;
                                runner_state.paused = false;
                                runner_state.pause_reason = None;
                                state.status = LoopStatus::Blocked;
                                state.summary = summary.clone();
                                state.waiting_reason = reason.clone();
                                state.updated_at = now;
                                if should_notify {
                                    state.version += 1;
                                    append_event(
                                        &dir,
                                        "task_selection_backlog_blocked",
                                        serde_json::json!({
                                            "summary": summary,
                                            "reason": reason,
                                            "detector_file": manifest.backlog_detector_file,
                                        }),
                                    )?;
                                    queue_notification(
                                        &dir,
                                        &manifest,
                                        "blocked",
                                        format_backlog_gate_notification(
                                            state.summary.as_str(),
                                            state.waiting_reason.as_str(),
                                        ),
                                    )?;
                                }
                            }
                            TaskSelectionOutcome::None => unreachable!("handled above"),
                        }
                    }
                }
            }
        }

        if let Some(reason) = compute_auto_stop_reason(
            state.ticks,
            manifest.max_ticks,
            manifest.started_at,
            manifest.max_runtime_sec,
            now,
        ) {
            state.version += 1;
            state.status = LoopStatus::Stopped;
            state.summary = format!("auto-stopped: {reason}");
            state.waiting_reason = reason.clone();
            state.updated_at = now;
            write_json(&dir.join("state.json"), &state)?;

            append_event(
                &dir,
                "daemon_auto_stopped",
                serde_json::json!({
                    "reason": reason,
                    "task_loops_completed": task_loops_completed,
                    "max_task_loops": manifest.max_task_loops,
                    "task_done_baseline": manifest.task_done_baseline,
                    "task_done_now": task_done_now,
                    "ticks": state.ticks,
                    "max_ticks": manifest.max_ticks,
                    "max_runtime_sec": manifest.max_runtime_sec,
                }),
            )?;
            let mention = completion_mention_prefix(&manifest);
            queue_notification(
                &dir,
                &manifest,
                "auto_stopped",
                format!(
                    "{mention}loop daemon auto-stopped: {}",
                    state.waiting_reason
                ),
            )?;
            let _ = flush_notifications(&dir, &manifest)?;
            break;
        }

        if matches!(
            state.status,
            LoopStatus::Done | LoopStatus::Failed | LoopStatus::Stopped
        ) {
            append_event(
                &dir,
                "daemon_exit_terminal",
                serde_json::json!({"status": format!("{:?}", state.status)}),
            )?;
            queue_notification(
                &dir,
                &manifest,
                "terminal",
                format!("daemon exit terminal state: {:?}", state.status),
            )?;
            let _ = flush_notifications(&dir, &manifest)?;
            break;
        }

        state.version += 1;
        state.ticks = state.ticks.saturating_add(1);
        state.updated_at = now;
        state.lease_expires_at =
            state.updated_at + chrono::Duration::seconds(lease_window_sec(tick_sec));

        let pr_changed = reduce_pr_tracking(&dir, &manifest, &mut state)?;
        let mut stuck_notified_ticks = None;
        let stuck_threshold = stuck_wait_ticks_threshold();
        let suppress_waiting_stuck = should_suppress_waiting_stuck(&runner_state);

        if manifest.task_runner_cmd.is_some() && !suppress_waiting_stuck {
            stuck_notified_ticks = update_waiting_stuck_tracker(
                &mut runner_state,
                &state.status,
                &state.summary,
                &state.waiting_reason,
                stuck_threshold,
            );
        } else {
            let _ = update_waiting_stuck_tracker(
                &mut runner_state,
                &LoopStatus::Running,
                "",
                "",
                stuck_threshold,
            );
        }

        write_runner_state(&dir, &runner_state)?;
        write_json(&dir.join("state.json"), &state)?;

        if let Some(unchanged_ticks) = stuck_notified_ticks {
            append_event(
                &dir,
                "task_waiting_stuck",
                serde_json::json!({
                    "task_id": runner_state.current_task_id,
                    "summary": state.summary,
                    "waiting_reason": state.waiting_reason,
                    "unchanged_ticks": unchanged_ticks,
                    "threshold": stuck_threshold,
                }),
            )?;
            queue_notification(
                &dir,
                &manifest,
                "task_waiting_stuck",
                format!(
                    "waiting state unchanged (ticks={}): {} ({})",
                    unchanged_ticks, state.summary, state.waiting_reason
                ),
            )?;
        }

        append_event(
            &dir,
            "tick",
            serde_json::json!({
                "version": state.version,
                "ticks": state.ticks,
                "task_loops_completed": task_loops_completed,
                "max_task_loops": manifest.max_task_loops,
                "pr_changed": pr_changed,
                "waiting_unchanged_ticks": runner_state.waiting_unchanged_ticks,
                "waiting_stuck_threshold": stuck_threshold,
                "waiting_stuck_notified": stuck_notified_ticks,
                "waiting_stuck_suppressed": suppress_waiting_stuck,
            }),
        )?;

        let _ = flush_notifications(&dir, &manifest)?;
        std::thread::sleep(std::time::Duration::from_secs(tick_sec));
    }

    Ok(())
}

fn cmd_stop(repo: PathBuf, run_id: Uuid, immediate: bool) -> Result<()> {
    let dir = run_dir(&repo, run_id);
    if !dir.exists() {
        bail!("run directory not found: {}", dir.display());
    }

    fs::write(dir.join("control.stop"), b"stop\n")?;

    if !immediate {
        println!("stop requested: {}", run_id);
        return Ok(());
    }

    let manifest: Manifest = read_json(&dir.join("manifest.json"))?;
    let mut state: State = read_json(&dir.join("state.json"))?;

    if !matches!(
        state.status,
        LoopStatus::Done | LoopStatus::Failed | LoopStatus::Stopped
    ) {
        state.version += 1;
        state.status = LoopStatus::Stopped;
        state.summary = "stopped immediately by kill switch".into();
        state.waiting_reason = "kill switch".into();
        state.updated_at = Utc::now();
        state.lease_expires_at = state.updated_at;
        write_json(&dir.join("state.json"), &state)?;

        append_event(
            &dir,
            "stop_immediate",
            serde_json::json!({
                "run_id": run_id,
                "daemon_pid": manifest.daemon_pid,
                "state_version": state.version,
            }),
        )?;
        let mention = completion_mention_prefix(&manifest);
        queue_notification(
            &dir,
            &manifest,
            "stopped",
            format!("{mention}loop daemon stopped immediately by kill switch"),
        )?;
    }

    stop_daemon_process_now(manifest.daemon_pid, run_id);
    let _ = flush_notifications(&dir, &manifest)?;

    println!("stopped immediately: {}", run_id);
    Ok(())
}

fn cmd_status(repo: PathBuf, run_id: Uuid) -> Result<()> {
    let dir = run_dir(&repo, run_id);
    if !dir.exists() {
        bail!("run directory not found: {}", dir.display());
    }
    let manifest: Manifest = read_json(&dir.join("manifest.json"))?;
    let state: State = read_json(&dir.join("state.json"))?;
    let queued_items = read_jsonl::<Notification>(&dir.join("notify-queue.jsonl"))?;
    let dispatched_items =
        read_jsonl::<DispatchedNotification>(&dir.join("notify-dispatched.jsonl"))?;
    let attempt_items = read_jsonl::<DeliveryAttempt>(&delivery_attempts_path(&dir))?;
    let ack_items = read_jsonl::<DeliveryAck>(&delivery_ack_path(&dir))?;
    let dead_letter_items = read_jsonl::<DeadLetterEntry>(&dead_letter_path(&dir))?;

    let mut seen = HashSet::new();
    for d in &dispatched_items {
        seen.insert(d.event_id);
    }
    let pending = queued_items
        .iter()
        .filter(|n| !seen.contains(&n.event_id))
        .count();

    let mut latest_ack: HashMap<Uuid, DeliveryAck> = HashMap::new();
    for ack in ack_items {
        match latest_ack.get(&ack.event_id) {
            Some(prev) if prev.acked_at >= ack.acked_at => {}
            _ => {
                latest_ack.insert(ack.event_id, ack);
            }
        }
    }

    let queued_total = queued_items.len();
    let dispatched = dispatched_items.len();
    let attempts_total = attempt_items.len();
    let dead_letter_total = dead_letter_items.len();
    let ack_entries_total = latest_ack.len();
    let acked_total = latest_ack.values().filter(|a| a.ok).count();
    let unacked_total = latest_ack.values().filter(|a| !a.ok).count();
    let last_acked_at = latest_ack
        .values()
        .filter(|a| a.ok)
        .map(|a| a.acked_at)
        .max();
    let last_attempt_at = attempt_items.iter().map(|a| a.attempted_at).max();
    let next_retry_at = queued_items.iter().filter_map(|n| n.next_retry_at).min();

    let pr_tracking = if pr_tracking_path(&dir).exists() {
        Some(read_json::<PrTracking>(&pr_tracking_path(&dir))?)
    } else {
        None
    };

    let delivery_metrics = if delivery_metrics_path(&dir).exists() {
        Some(read_json::<DeliveryMetrics>(&delivery_metrics_path(&dir))?)
    } else {
        None
    };
    let runtime_sec = (Utc::now() - manifest.started_at).num_seconds().max(0);
    let task_file_abs = resolve_task_file_path(&manifest.repo_path, &manifest.task_file);
    let task_done_current = task_checklist_done_count(&task_file_abs).unwrap_or(0);
    let task_loops_completed = task_done_current.saturating_sub(manifest.task_done_baseline);
    let runner_state = read_runner_state(&dir).unwrap_or_default();
    let runner_mode = if manifest.task_runner_cmd.is_some() {
        "dogfood"
    } else {
        "monitor_only"
    };
    let last_task_pr_url = runner_state.last_task_pr_url.clone();
    let visible_last_pr_url = runner_state
        .current_task_pr_url
        .clone()
        .or_else(|| last_task_pr_url.clone());
    let runner_current_view = serde_json::json!({
        "id": runner_state.current_task_id.clone(),
        "text": runner_state.current_task_text.clone(),
        "line": runner_state.current_task_line,
        "started_at": runner_state.current_task_started_at,
        "state": runner_state.current_task_state.clone(),
        "waiting_dependency": runner_state.current_waiting_dependency.clone(),
        "worktree": runner_state.current_worktree.clone(),
        "blocked_reason": runner_state.current_task_blocked_reason.clone(),
        "blocked_context": runner_state.current_blocked_context.clone(),
        "pr_url": runner_state.current_task_pr_url.clone(),
    });
    let runner_last_view = serde_json::json!({
        "id": runner_state.last_task_id.clone(),
        "state": runner_state.last_task_state.clone(),
        "at": runner_state.last_task_at,
        "reason": runner_state.last_task_reason.clone(),
        "pr_url": runner_state.last_task_pr_url.clone(),
        "worktree": runner_state.last_worktree.clone(),
    });
    let runner_view = serde_json::json!({
        "mode": runner_mode,
        "task_runner_cmd": manifest.task_runner_cmd,
        "task_agent_id": manifest.task_agent_id,
        "auto_check_on_success": manifest.auto_check_on_success,
        "auto_recover_blocked": manifest.auto_recover_blocked,
        "auto_recover_blocked_max_attempts": manifest.auto_recover_blocked_max_attempts,
        "task_worktree_root": manifest.task_worktree_root.clone(),
        "task_worktrees": manifest.task_worktrees.clone(),
        "auto_recover_attempts": runner_state.auto_recover_attempts,
        "auto_recover_last_reason": runner_state.auto_recover_last_reason.clone(),
        "auto_recover_same_reason_count": runner_state.auto_recover_same_reason_count,
        "auto_recover_last_task_id": runner_state.auto_recover_last_task_id.clone(),
        "auto_recover_last_at": runner_state.auto_recover_last_at,
        "preferred_next_task_id": runner_state.preferred_next_task_id.clone(),
        "tracked_task_pr_urls": runner_state.tracked_task_pr_urls.clone(),
        "task_loops_started": runner_state.task_loops_started,
        "waiting_stuck_threshold": stuck_wait_ticks_threshold(),
        "waiting_unchanged_ticks": runner_state.waiting_unchanged_ticks,
        "waiting_last_notified_ticks": runner_state.waiting_last_notified_ticks,
        "waiting_last_summary": runner_state.waiting_last_summary.clone(),
        "waiting_last_reason": runner_state.waiting_last_reason.clone(),
        "current_task_id": runner_state.current_task_id.clone(),
        "current_task_text": runner_state.current_task_text.clone(),
        "current_task_line": runner_state.current_task_line,
        "current_task_started_at": runner_state.current_task_started_at,
        "current_task_state": runner_state.current_task_state.clone(),
        "current_waiting_dependency": runner_state.current_waiting_dependency.clone(),
        "current_worktree": runner_state.current_worktree.clone(),
        "current_task_blocked_reason": runner_state.current_task_blocked_reason.clone(),
        "current_blocked_context": runner_state.current_blocked_context.clone(),
        "current_task_pr_url": runner_state.current_task_pr_url.clone(),
        "last_task_id": runner_state.last_task_id.clone(),
        "last_task_state": runner_state.last_task_state.clone(),
        "last_task_at": runner_state.last_task_at,
        "last_task_reason": runner_state.last_task_reason.clone(),
        "last_blocked_context": runner_state.last_blocked_context.clone(),
        "last_task_pr_url": last_task_pr_url.clone(),
        "last_waiting_dependency": runner_state.last_waiting_dependency.clone(),
        "last_worktree": runner_state.last_worktree.clone(),
        "current": runner_current_view,
        "last": runner_last_view,
        "paused": runner_state.paused,
        "pause_reason": runner_state.pause_reason.clone(),
        "status_message_id": runner_state.status_message_id.clone(),
        "status_updated_at": runner_state.status_updated_at,
    });

    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "run_id": manifest.run_id,
            "thread_id": manifest.thread_id,
            "session_key": manifest.session_key,
            "requester_user_id": manifest.requester_user_id,
            "feedback_thread_id": manifest.feedback_thread_id,
            "feedback_channel": manifest.feedback_channel,
            "status": state.status,
            "summary": state.summary,
            "waiting_reason": state.waiting_reason,
            "updated_at": state.updated_at,
            "lease_expires_at": state.lease_expires_at,
            "ticks": state.ticks,
            "max_ticks": manifest.max_ticks,
            "max_runtime_sec": manifest.max_runtime_sec,
            "runtime_sec": runtime_sec,
            "task_file": manifest.task_file,
            "max_task_loops": manifest.max_task_loops,
            "task_done_baseline": manifest.task_done_baseline,
            "task_done_current": task_done_current,
            "task_loops_completed": task_loops_completed,
            "runner": runner_view,
            "last_pr_url": visible_last_pr_url,
            "queued_notifications_total": queued_total,
            "pending_notifications": pending,
            "dispatched_notifications": dispatched,
            "delivery_attempts_total": attempts_total,
            "dead_letter_total": dead_letter_total,
            "ack_entries_total": ack_entries_total,
            "acked_total": acked_total,
            "unacked_total": unacked_total,
            "last_acked_at": last_acked_at,
            "last_attempt_at": last_attempt_at,
            "next_retry_at": next_retry_at,
            "daemon_pid": manifest.daemon_pid,
            "deliver_openclaw": manifest.deliver_openclaw,
            "pr_tracking": pr_tracking,
            "delivery_metrics": delivery_metrics,
            "ack_retry_policy": ack_retry_policy_snapshot()
        }))?
    );
    Ok(())
}

fn cmd_delivery_report(
    repo: PathBuf,
    run_id: Uuid,
    limit: usize,
    status: String,
    failed_window: usize,
) -> Result<()> {
    let dir = run_dir(&repo, run_id);
    if !dir.exists() {
        bail!("run directory not found: {}", dir.display());
    }

    let status_filter = status.to_ascii_lowercase();
    if !matches!(
        status_filter.as_str(),
        "all" | "pending" | "delivered" | "failed"
    ) {
        bail!("invalid status filter: {status_filter} (use all|pending|delivered|failed)");
    }

    let queued_items = read_jsonl::<Notification>(&dir.join("notify-queue.jsonl"))?;
    let dispatched_items =
        read_jsonl::<DispatchedNotification>(&dir.join("notify-dispatched.jsonl"))?;
    let attempt_items = read_jsonl::<DeliveryAttempt>(&delivery_attempts_path(&dir))?;
    let ack_items = read_jsonl::<DeliveryAck>(&delivery_ack_path(&dir))?;
    let dead_letter_items = read_jsonl::<DeadLetterEntry>(&dead_letter_path(&dir))?;

    let mut latest_attempt: HashMap<Uuid, DeliveryAttempt> = HashMap::new();
    for a in attempt_items {
        match latest_attempt.get(&a.event_id) {
            Some(prev) if prev.attempted_at >= a.attempted_at => {}
            _ => {
                latest_attempt.insert(a.event_id, a);
            }
        }
    }

    let mut latest_ack: HashMap<Uuid, DeliveryAck> = HashMap::new();
    for ack in ack_items {
        match latest_ack.get(&ack.event_id) {
            Some(prev) if prev.acked_at >= ack.acked_at => {}
            _ => {
                latest_ack.insert(ack.event_id, ack);
            }
        }
    }

    let mut rows: Vec<(DateTime<Utc>, serde_json::Value)> = Vec::new();
    let mut seen: HashSet<Uuid> = HashSet::new();

    for d in &dispatched_items {
        seen.insert(d.event_id);
        let last_attempt = latest_attempt.get(&d.event_id);
        let ack = latest_ack.get(&d.event_id);
        let last_activity = last_attempt
            .map(|a| a.attempted_at)
            .unwrap_or(d.dispatched_at);

        rows.push((
            last_activity,
            serde_json::json!({
                "event_id": d.event_id,
                "status": "delivered",
                "kind": d.kind,
                "message": d.message,
                "attempts": d.attempts,
                "last_activity": last_activity,
                "dispatched_at": d.dispatched_at,
                "last_error": last_attempt.and_then(|a| a.error.clone()),
                "acked": ack.map(|a| a.ok).unwrap_or(false),
                "ack_at": ack.map(|a| a.acked_at),
                "ack_category": ack.map(|a| a.category.clone()),
                "ack_error": ack.and_then(|a| a.error.clone()),
            }),
        ));
    }

    for q in &queued_items {
        if seen.contains(&q.event_id) {
            continue;
        }

        let last_attempt = latest_attempt.get(&q.event_id);
        let ack = latest_ack.get(&q.event_id);
        let last_activity = last_attempt.map(|a| a.attempted_at).unwrap_or(q.ts);

        rows.push((
            last_activity,
            serde_json::json!({
                "event_id": q.event_id,
                "status": "pending",
                "kind": q.kind,
                "message": q.message,
                "attempts": q.attempts,
                "last_activity": last_activity,
                "next_retry_at": q.next_retry_at,
                "last_error": q.last_error,
                "acked": ack.map(|a| a.ok).unwrap_or(false),
                "ack_at": ack.map(|a| a.acked_at),
                "ack_category": ack.map(|a| a.category.clone()),
                "ack_error": ack.and_then(|a| a.error.clone()),
            }),
        ));
    }

    for dlq in &dead_letter_items {
        let normalized_reason = normalize_error_reason(dlq.last_error.as_deref());
        let ack = latest_ack.get(&dlq.event_id);

        rows.push((
            dlq.moved_at,
            serde_json::json!({
                "event_id": dlq.event_id,
                "status": "failed",
                "kind": dlq.kind,
                "message": dlq.message,
                "attempts": dlq.attempts,
                "last_activity": dlq.moved_at,
                "dead_letter_at": dlq.moved_at,
                "last_error": dlq.last_error,
                "normalized_reason": normalized_reason,
                "acked": ack.map(|a| a.ok).unwrap_or(false),
                "ack_at": ack.map(|a| a.acked_at),
                "ack_category": ack.map(|a| a.category.clone()),
                "ack_error": ack.and_then(|a| a.error.clone()),
            }),
        ));
    }

    rows.sort_by_key(|row| std::cmp::Reverse(row.0));
    let items: Vec<serde_json::Value> = rows
        .into_iter()
        .map(|(_, v)| v)
        .filter(|v| {
            status_filter == "all"
                || v.get("status").and_then(|s| s.as_str()) == Some(status_filter.as_str())
        })
        .take(limit)
        .collect();

    let pending_count = queued_items
        .iter()
        .filter(|q| !dispatched_items.iter().any(|d| d.event_id == q.event_id))
        .count();

    let acked_total = latest_ack.values().filter(|a| a.ok).count();
    let unacked_total = latest_ack.values().filter(|a| !a.ok).count();
    let last_acked_at = latest_ack
        .values()
        .filter(|a| a.ok)
        .map(|a| a.acked_at)
        .max();

    let mut failed_for_hist: Vec<&DeadLetterEntry> = dead_letter_items.iter().collect();
    failed_for_hist.sort_by_key(|dlq| std::cmp::Reverse(dlq.moved_at));
    if failed_window > 0 && failed_for_hist.len() > failed_window {
        failed_for_hist.truncate(failed_window);
    }

    let mut failed_reason_histogram_map: HashMap<String, usize> = HashMap::new();
    let mut failed_reason_by_kind_map: HashMap<String, HashMap<String, usize>> = HashMap::new();
    for dlq in &failed_for_hist {
        let reason = normalize_error_reason(dlq.last_error.as_deref());
        *failed_reason_histogram_map
            .entry(reason.clone())
            .or_insert(0) += 1;
        *failed_reason_by_kind_map
            .entry(dlq.kind.clone())
            .or_default()
            .entry(reason)
            .or_insert(0) += 1;
    }

    let mut reason_pairs: Vec<(String, usize)> = failed_reason_histogram_map.into_iter().collect();
    reason_pairs.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let failed_reason_histogram = reason_pairs
        .into_iter()
        .map(|(reason, count)| serde_json::json!({"reason": reason, "count": count}))
        .collect::<Vec<_>>();

    let mut by_kind: Vec<serde_json::Value> = failed_reason_by_kind_map
        .into_iter()
        .map(|(kind, reasons)| {
            let mut pairs: Vec<(String, usize)> = reasons.into_iter().collect();
            pairs.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            serde_json::json!({
                "kind": kind,
                "reasons": pairs
                    .into_iter()
                    .map(|(reason, count)| serde_json::json!({"reason": reason, "count": count}))
                    .collect::<Vec<_>>(),
            })
        })
        .collect();
    by_kind.sort_by(|a, b| {
        let ak = a.get("kind").and_then(|v| v.as_str()).unwrap_or("");
        let bk = b.get("kind").and_then(|v| v.as_str()).unwrap_or("");
        ak.cmp(bk)
    });

    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "run_id": run_id,
            "filter": status_filter,
            "pending_count": pending_count,
            "delivered_count": dispatched_items.len(),
            "failed_count": dead_letter_items.len(),
            "attempt_count": latest_attempt.len(),
            "ack_summary": {
                "events_with_ack": latest_ack.len(),
                "acked_total": acked_total,
                "unacked_total": unacked_total,
                "last_acked_at": last_acked_at,
            },
            "failed_histogram_window": {
                "mode": if failed_window == 0 { "all" } else { "recent" },
                "recent_n": if failed_window == 0 { serde_json::Value::Null } else { serde_json::json!(failed_window) },
                "considered_failed_count": failed_for_hist.len(),
            },
            "failed_reason_histogram": failed_reason_histogram,
            "failed_reason_histogram_by_kind": by_kind,
            "ack_retry_policy": ack_retry_policy_snapshot(),
            "items": items,
        }))?
    );

    Ok(())
}

fn cmd_requeue_dead_letter(
    repo: PathBuf,
    run_id: Uuid,
    event_id: Option<Uuid>,
    limit: usize,
    reset_attempts: bool,
    dry_run: bool,
) -> Result<()> {
    let dir = run_dir(&repo, run_id);
    if !dir.exists() {
        bail!("run directory not found: {}", dir.display());
    }

    let manifest: Manifest = read_json(&dir.join("manifest.json"))?;
    let queue_path = dir.join("notify-queue.jsonl");
    let dead_letter_file = dead_letter_path(&dir);
    let metrics_path = delivery_metrics_path(&dir);

    let queue_items = read_jsonl::<Notification>(&queue_path)?;
    let dispatched_items =
        read_jsonl::<DispatchedNotification>(&dir.join("notify-dispatched.jsonl"))?;
    let dead_letter_items = read_jsonl::<DeadLetterEntry>(&dead_letter_file)?;

    let mut occupied: HashSet<Uuid> = queue_items.iter().map(|n| n.event_id).collect();
    for d in &dispatched_items {
        occupied.insert(d.event_id);
    }

    let mut selected = 0usize;
    let mut would_requeue = 0usize;
    let mut requeued = 0usize;
    let mut skipped_occupied = 0usize;
    let mut target_seen = false;
    let now = Utc::now();
    let mut keep_dead: Vec<DeadLetterEntry> = Vec::new();

    for entry in dead_letter_items {
        if let Some(target) = event_id {
            if entry.event_id != target {
                keep_dead.push(entry);
                continue;
            }
            if target_seen {
                keep_dead.push(entry);
                continue;
            }
            target_seen = true;
        }

        if selected >= limit {
            keep_dead.push(entry);
            continue;
        }

        selected += 1;

        if occupied.contains(&entry.event_id) {
            skipped_occupied += 1;
            keep_dead.push(entry);
            continue;
        }

        would_requeue += 1;

        let notification = Notification {
            event_id: entry.event_id,
            run_id,
            ts: now,
            channel: manifest.channel.clone(),
            thread_id: manifest.thread_id.clone(),
            kind: entry.kind,
            message: entry.message,
            attempts: if reset_attempts { 0 } else { entry.attempts },
            next_retry_at: None,
            last_error: entry.last_error,
        };

        if dry_run {
            keep_dead.push(DeadLetterEntry {
                event_id: notification.event_id,
                run_id,
                moved_at: now,
                attempts: if reset_attempts {
                    0
                } else {
                    notification.attempts
                },
                kind: notification.kind,
                message: notification.message,
                last_error: notification.last_error,
            });
            continue;
        }

        append_jsonl(&queue_path, &notification)?;
        occupied.insert(notification.event_id);
        append_event(
            &dir,
            "notify_requeued",
            serde_json::json!({
                "event_id": notification.event_id,
                "reset_attempts": reset_attempts,
                "attempts": notification.attempts,
            }),
        )?;
        requeued += 1;
    }

    if !dry_run {
        rewrite_jsonl(&dead_letter_file, &keep_dead)?;

        let mut metrics = if metrics_path.exists() {
            read_json::<DeliveryMetrics>(&metrics_path)?
        } else {
            DeliveryMetrics::default()
        };
        if requeued > 0 {
            metrics.requeued_total = metrics.requeued_total.saturating_add(requeued as u64);
            metrics.last_requeued_at = Some(now);
            write_json(&metrics_path, &metrics)?;
        }
    }

    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "run_id": run_id,
            "selected": selected,
            "would_requeue": would_requeue,
            "requeued": requeued,
            "skipped_occupied": skipped_occupied,
            "target_found": event_id.map(|_| target_seen).unwrap_or(true),
            "dry_run": dry_run,
            "remaining_dead_letter": keep_dead.len(),
        }))?
    );

    Ok(())
}

fn cmd_notify(repo: PathBuf, run_id: Uuid, kind: String, message: String) -> Result<()> {
    let dir = run_dir(&repo, run_id);
    if !dir.exists() {
        bail!("run directory not found: {}", dir.display());
    }
    let manifest: Manifest = read_json(&dir.join("manifest.json"))?;
    let event_id = queue_notification(&dir, &manifest, kind, message)?;
    println!("queued event_id={}", event_id);
    Ok(())
}

fn cmd_track_pr(
    repo: PathBuf,
    run_id: Uuid,
    gh_repo: String,
    pr: u64,
    merge_method: String,
) -> Result<()> {
    if !matches!(merge_method.as_str(), "merge" | "squash" | "rebase") {
        bail!("invalid merge method: {merge_method}");
    }

    let dir = run_dir(&repo, run_id);
    if !dir.exists() {
        bail!("run directory not found: {}", dir.display());
    }

    let manifest: Manifest = read_json(&dir.join("manifest.json"))?;
    let now = Utc::now();
    let tracking = PrTracking {
        gh_repo,
        pr,
        merge_method,
        created_at: now,
        updated_at: now,
        last_polled_at: None,
        next_poll_at: now,
        unchanged_polls: 0,
        consecutive_errors: 0,
        last_observed_state: None,
        last_merge_state_status: None,
        last_error: None,
        auto_merge_armed: false,
    };

    write_json(&pr_tracking_path(&dir), &tracking)?;

    let mut state: State = read_json(&dir.join("state.json"))?;
    state.version += 1;
    state.status = LoopStatus::Waiting;
    state.summary = format!("tracking PR #{} for merge", tracking.pr);
    state.waiting_reason = format!("PR #{} CI/merge pending", tracking.pr);
    state.updated_at = now;
    write_json(&dir.join("state.json"), &state)?;

    append_event(
        &dir,
        "pr_tracking_started",
        serde_json::json!({
            "repo": tracking.gh_repo,
            "pr": tracking.pr,
            "merge_method": tracking.merge_method
        }),
    )?;

    queue_notification(
        &dir,
        &manifest,
        "pr_tracking_started",
        format!("tracking PR #{} ({})", tracking.pr, tracking.gh_repo),
    )?;

    println!("pr tracking set: {}#{}", tracking.gh_repo, tracking.pr);
    Ok(())
}

fn reconcile_orphan_if_needed(repo: &Path, run_id: Uuid) -> Result<Option<&'static str>> {
    let dir = run_dir(repo, run_id);
    if !dir.exists() {
        return Ok(None);
    }

    let manifest: Manifest = read_json(&dir.join("manifest.json"))?;
    let mut state: State = read_json(&dir.join("state.json"))?;

    if matches!(
        state.status,
        LoopStatus::Done | LoopStatus::Failed | LoopStatus::Stopped
    ) {
        let _ = flush_notifications(&dir, &manifest)?;
        return Ok(Some("terminal"));
    }

    let now = Utc::now();
    let observed_version = state.version;
    let observed_status = state.status.clone();
    let lease_expired = now > state.lease_expires_at;
    let daemon_alive = process_matches_run(manifest.daemon_pid, run_id);

    if lease_expired && !daemon_alive {
        // CAS-style recheck: if state moved since observation, do not force blocked.
        let latest: State = read_json(&dir.join("state.json"))?;
        let latest_daemon_alive = process_matches_run(manifest.daemon_pid, run_id);
        let latest_expired = Utc::now() > latest.lease_expires_at;

        if latest.version != observed_version
            || latest.status != observed_status
            || latest_daemon_alive
            || !latest_expired
        {
            append_event(
                &dir,
                "orphan_recheck_skipped",
                serde_json::json!({
                    "observed_version": observed_version,
                    "latest_version": latest.version,
                    "observed_status": observed_status,
                    "latest_status": latest.status,
                    "latest_daemon_alive": latest_daemon_alive,
                    "latest_expired": latest_expired,
                }),
            )?;
            let _ = flush_notifications(&dir, &manifest)?;
            return Ok(Some("race_skipped"));
        }

        state = latest;
        state.version += 1;
        state.status = LoopStatus::Blocked;
        state.summary = format!(
            "orphan detected: daemon pid {} missing after lease expiry",
            manifest.daemon_pid
        );
        state.waiting_reason = "daemon orphan detected".into();
        state.updated_at = Utc::now();
        write_json(&dir.join("state.json"), &state)?;

        append_event(
            &dir,
            "orphan_blocked",
            serde_json::json!({
                "daemon_pid": manifest.daemon_pid,
                "lease_expires_at": state.lease_expires_at,
                "now": state.updated_at,
            }),
        )?;

        queue_notification(
            &dir,
            &manifest,
            "orphan_blocked",
            format_orphan_blocked_notification(&manifest, manifest.daemon_pid),
        )?;

        let _ = flush_notifications(&dir, &manifest)?;
        return Ok(Some("blocked_orphan"));
    }

    let _ = flush_notifications(&dir, &manifest)?;
    Ok(Some("ok"))
}

fn cmd_sweep(repo: PathBuf, run_id: Option<Uuid>) -> Result<()> {
    let mut scanned = 0usize;
    let mut blocked_orphan = 0usize;
    let mut errors: Vec<String> = Vec::new();

    let mut run_ids: Vec<Uuid> = Vec::new();
    if let Some(id) = run_id {
        run_ids.push(id);
    } else {
        let root = runs_root(&repo);
        if root.exists() {
            for entry in fs::read_dir(root)? {
                let entry = entry?;
                if !entry.file_type()?.is_dir() {
                    continue;
                }
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if let Ok(id) = Uuid::parse_str(&name) {
                    run_ids.push(id);
                }
            }
        }
    }

    for id in run_ids {
        scanned += 1;
        match reconcile_orphan_if_needed(&repo, id) {
            Ok(Some("blocked_orphan")) => blocked_orphan += 1,
            Ok(_) => {}
            Err(err) => errors.push(format!("{}: {}", id, err)),
        }
    }

    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "scanned": scanned,
            "blocked_orphan": blocked_orphan,
            "errors": errors,
        }))?
    );

    Ok(())
}

fn cmd_task_next(file: PathBuf) -> Result<()> {
    let (_, _, entries) = load_task_checklist(&file)?;

    let total = entries.len();
    let done_count = entries.iter().filter(|e| e.done).count();
    let open_count = total.saturating_sub(done_count);
    let next = entries.iter().find(|e| !e.done);

    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "file": file,
            "total": total,
            "done": done_count,
            "open": open_count,
            "next": next.map(|e| serde_json::json!({
                "line": e.line_no,
                "id": e.id,
                "text": e.text,
            })),
        }))?
    );

    Ok(())
}

fn cmd_task_check(file: PathBuf, id: String, done: bool) -> Result<()> {
    let result = update_task_check(&file, &id, done)?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

fn cmd_task_run_once(
    file: PathBuf,
    cmd: String,
    auto_check_on_success: bool,
    dry_run: bool,
) -> Result<()> {
    let outcome = run_task_once(TaskRunOptions {
        task_file: &file,
        selected_task: None,
        cmd: &cmd,
        auto_check_on_success,
        dry_run,
        cwd: Some(Path::new(".")),
        run_id: None,
        thread_id: None,
        channel: None,
        task_agent_id: None,
        task_kind: None,
        backlog_snapshot: None,
        worktree: None,
    })?;

    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "file": file,
            "task": outcome.task.as_ref().map(|t| serde_json::json!({
                "line": t.line_no,
                "id": t.id,
                "text": t.text,
            })),
            "command": outcome.command,
            "dry_run": dry_run,
            "executed": outcome.executed,
            "success": outcome.success,
            "exit_code": outcome.exit_code,
            "auto_check_on_success": auto_check_on_success,
            "check_result": outcome.check_result,
            "stdout": outcome.stdout,
            "stderr": outcome.stderr,
        }))?
    );

    Ok(())
}

fn cmd_task_approve(file: PathBuf, approved_by: String) -> Result<()> {
    let status = write_task_approval(&file, &approved_by)?;
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "file": file,
            "approved_by": status.approved_by,
            "approved_at": status.approved_at,
            "approved_tasklist_hash": status.approved_tasklist_hash,
        }))?
    );
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Start {
            repo,
            session_key,
            channel,
            thread_id,
            owner_message_id,
            requester_user_id,
            task_agent_id,
            feedback_thread_id,
            feedback_channel,
            tick_sec,
            deliver_openclaw,
            max_ticks,
            max_runtime_sec,
            max_task_loops,
            task_file,
            task_runner_cmd,
            approved_tasklist_hash,
            require_task_approval,
            auto_check_on_success,
            auto_recover_blocked,
            auto_recover_blocked_max_attempts,
            backlog_detector_file,
            backlog_detector_max_age_sec,
            task_worktree_root,
        } => cmd_start(StartOptions {
            repo,
            session_key,
            channel,
            thread_id,
            owner_message_id,
            requester_user_id,
            task_agent_id,
            feedback_thread_id,
            feedback_channel,
            tick_sec,
            deliver_openclaw,
            max_ticks,
            max_runtime_sec,
            max_task_loops,
            task_file,
            task_runner_cmd,
            approved_tasklist_hash,
            require_task_approval,
            auto_check_on_success,
            auto_recover_blocked,
            auto_recover_blocked_max_attempts,
            backlog_detector_file,
            backlog_detector_max_age_sec,
            task_worktree_root,
        }),
        Commands::Daemon {
            repo,
            run_id,
            tick_sec,
        } => cmd_daemon(repo, run_id, tick_sec),
        Commands::Stop {
            repo,
            run_id,
            immediate,
        } => cmd_stop(repo, run_id, immediate),
        Commands::Status { repo, run_id } => cmd_status(repo, run_id),
        Commands::DeliveryReport {
            repo,
            run_id,
            limit,
            status,
            failed_window,
        } => cmd_delivery_report(repo, run_id, limit, status, failed_window),
        Commands::RequeueDeadLetter {
            repo,
            run_id,
            event_id,
            limit,
            reset_attempts,
            dry_run,
        } => cmd_requeue_dead_letter(repo, run_id, event_id, limit, reset_attempts, dry_run),
        Commands::Notify {
            repo,
            run_id,
            kind,
            message,
        } => cmd_notify(repo, run_id, kind, message),
        Commands::TrackPr {
            repo,
            run_id,
            gh_repo,
            pr,
            merge_method,
        } => cmd_track_pr(repo, run_id, gh_repo, pr, merge_method),
        Commands::Sweep { repo, run_id } => cmd_sweep(repo, run_id),
        Commands::TaskNext { file } => cmd_task_next(file),
        Commands::TaskCheck { file, id, done } => cmd_task_check(file, id, done),
        Commands::TaskRunOnce {
            file,
            cmd,
            auto_check_on_success,
            dry_run,
        } => cmd_task_run_once(file, cmd, auto_check_on_success, dry_run),
        Commands::TaskApprove { file, approved_by } => cmd_task_approve(file, approved_by),
    }
    .map_err(|e| {
        eprintln!("error: {e:?}");
        process::exit(1);
    })
}

#[cfg(test)]
mod tests {
    use super::{
        AutoRecoverBlockedOutcome, AutoRecoverDecisionSnapshot, AutoRecoverDecisionState,
        BacklogSnapshot, BlockedContext, BlockedContextSource, DeadLetterEntry, DeliveryAck,
        DeliveryAttempt, DispatchedNotification, GhPrView, GhStatusCheck, LoopStatus, Manifest,
        Notification, NotificationDeliveryMode, RecoveryTaskSnapshot, RunnerState, RunnerTaskState,
        State, TaskChecklistEntry, TaskExecutionKind, TaskSelectionOutcome, WaitingContract,
        WaitingDependencyContext, WaitingDependencyProgress, WaitingMergeProgress,
        ack_retry_policy, append_jsonl, apply_status_establish_retry_override,
        auto_merge_unavailable_error, auto_recover_guard_reason, blocked_reason_from_runner,
        blocked_recovery_hint, build_recovery_task_text, classify_ack_failure_category,
        classify_task_execution_kind, completion_guard_waiting_fallback_line,
        compute_auto_stop_reason, compute_backoff_sec, dead_letter_path, delivery_ack_path,
        delivery_attempts_path, delivery_retry_backoff_sec, emit_all_tasks_completed_notifications,
        ensure_task_agent_exists_with, ensure_waiting_dependency_progress_with,
        ensure_waiting_merge_progress_with, extract_pr_url, flush_notifications,
        format_auto_recover_decision_notification, format_auto_recover_halt_notification,
        format_orphan_blocked_notification, format_task_blocked_notification,
        format_waiting_dependency_notification, format_waiting_merge_notification,
        is_phase_or_stacked_dependency_reason, lease_window_sec, maybe_auto_recover_blocked_task,
        missing_current_task_id_guard_reason, normalize_blocked_reason_for_recovery,
        normalize_error_reason, notification_delivery_mode, openclaw_notify_timeout_sec_from,
        parse_acpx_task_result, parse_openclaw_message_id, parse_task_checklist_entry,
        parse_waiting_contract, parse_waiting_dependency_contract, queue_main_feedback_summary,
        queue_notification, read_backlog_snapshot, read_json, read_jsonl,
        retryable_waiting_merge_error, select_next_task_entry, select_next_task_with_backlog,
        should_force_status_establish_retry, should_suppress_waiting_stuck, task_contract_line,
        tasklist_approval_violation_reason, update_waiting_stuck_tracker,
        validate_task_done_contract_with, waiting_merge_nonprogress_reason, write_json,
        write_runner_state,
    };
    use crate::tasklist::write_task_approval;
    use chrono::{Duration, Utc};
    use std::{
        collections::HashMap,
        fs,
        path::{Path, PathBuf},
    };
    use uuid::Uuid;

    struct TestRunDir {
        path: PathBuf,
    }

    impl TestRunDir {
        fn new(tag: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("claw-loopd-test-{tag}-{}", Uuid::new_v4()));
            fs::create_dir_all(&path).expect("create temp run dir");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    #[test]
    fn parse_acpx_task_result_reads_fenced_payload() {
        let stdout = r#"TASK_WAITING_MERGE PR_URL=https://github.com/n01e0/claw-loop/pull/123
Summary for humans.
```ACPX_TASK_RESULT_JSON
{
  "summary": "Defined the ACPX task result contract.",
  "verification": ["cargo test"],
  "notes": ["CI pending under auto-merge."],
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
"#;

        let result = parse_acpx_task_result(stdout)
            .expect("parse result")
            .expect("result payload");

        assert_eq!(result.summary, "Defined the ACPX task result contract.");
        assert_eq!(result.verification, vec!["cargo test"]);
        assert_eq!(result.notes, vec!["CI pending under auto-merge."]);
        assert_eq!(result.pushed_branch, "apb-3-acpx-result-contract");
        let pr = result.pr.expect("pr metadata");
        assert_eq!(pr.number, Some(123));
        assert_eq!(pr.merge_state.as_deref(), Some("pending"));
        assert_eq!(pr.auto_merge, Some(true));
    }

    #[test]
    fn parse_acpx_task_result_reads_inline_payload() {
        let stdout = concat!(
            "TASK_DONE PR_URL=https://github.com/n01e0/claw-loop/pull/124\n",
            "ACPX_TASK_RESULT_JSON: {\"summary\":\"done\",\"verification\":[],\"notes\":[],\"pushed_branch\":\"apb-3\"}\n"
        );

        let result = parse_acpx_task_result(stdout)
            .expect("parse result")
            .expect("result payload");

        assert_eq!(result.summary, "done");
        assert_eq!(result.pushed_branch, "apb-3");
        assert!(result.pr.is_none());
    }

    impl Drop for TestRunDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn write_mock_openclaw_script(dir: &TestRunDir) -> (PathBuf, PathBuf) {
        let script_path = dir.path.join("mock-openclaw.sh");
        let state_path = dir.path.join("agents.json");
        fs::write(&state_path, r#"[{"id":"main"}]"#).expect("write mock agents state");
        fs::write(
            &script_path,
            format!(
                "#!/usr/bin/env bash\nset -euo pipefail\nSTATE_FILE=\"{}\"\nFAIL_ADD_MARKER=\"$(dirname \"$STATE_FILE\")/fail-add\"\nif [[ \"${{1:-}}\" == \"agents\" && \"${{2:-}}\" == \"list\" ]]; then\n  cat \"$STATE_FILE\"\n  exit 0\nfi\nif [[ \"${{1:-}}\" == \"agents\" && \"${{2:-}}\" == \"add\" ]]; then\n  if [[ -f \"$FAIL_ADD_MARKER\" ]]; then\n    echo \"mock add failed\" >&2\n    exit 1\n  fi\n  name=\"${{3:?missing agent id}}\"\n  python3 - <<'PY' \"$STATE_FILE\" \"$name\"\nimport json, pathlib, sys\npath = pathlib.Path(sys.argv[1])\nname = sys.argv[2]\ndata = json.loads(path.read_text())\nif not any(entry.get('id') == name for entry in data):\n    data.append({{'id': name}})\npath.write_text(json.dumps(data))\nPY\n  printf '{{\"id\":\"%s\"}}\\n' \"$name\"\n  exit 0\nfi\necho \"unsupported mock openclaw args: $*\" >&2\nexit 1\n",
                state_path.display(),
            ),
        )
        .expect("write mock openclaw script");
        let mut perms = fs::metadata(&script_path)
            .expect("stat mock openclaw script")
            .permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            perms.set_mode(0o755);
        }
        fs::set_permissions(&script_path, perms).expect("chmod mock openclaw script");
        (script_path, state_path)
    }

    fn test_manifest(run_dir: &Path, run_id: Uuid, deliver_openclaw: bool) -> Manifest {
        Manifest {
            run_id,
            repo_path: run_dir.to_path_buf(),
            session_key: "test-session".to_string(),
            channel: "discord".to_string(),
            thread_id: "test-thread".to_string(),
            owner_message_id: None,
            requester_user_id: None,
            task_agent_id: None,
            feedback_thread_id: None,
            feedback_channel: None,
            started_at: Utc::now(),
            daemon_pid: std::process::id(),
            deliver_openclaw,
            max_ticks: None,
            max_runtime_sec: None,
            max_task_loops: 10,
            task_file: PathBuf::from("docs/roadmaps/ack-integration-tasklist.md"),
            task_done_baseline: 0,
            task_runner_cmd: None,
            approved_tasklist_hash: "test-approved-hash".to_string(),
            approved_by: "test-approver".to_string(),
            approved_at: Utc::now(),
            require_task_approval: false,
            auto_check_on_success: true,
            auto_recover_blocked: false,
            auto_recover_blocked_max_attempts: 3,
            backlog_detector_file: None,
            backlog_detector_max_age_sec: 900,
            task_worktree_root: PathBuf::from(".ralph/worktrees"),
            task_worktrees: HashMap::new(),
        }
    }

    fn test_blocked_context(
        task_id: &str,
        task_text: &str,
        task_line: usize,
        pr_url: Option<&str>,
        reason_summary: &str,
        reason_detail: Option<&str>,
        blocked_at: chrono::DateTime<Utc>,
    ) -> BlockedContext {
        BlockedContext {
            task_id: task_id.to_string(),
            task_text: Some(task_text.to_string()),
            task_line: Some(task_line),
            pr_url: pr_url.map(ToOwned::to_owned),
            source: BlockedContextSource::WaitingMerge,
            exit_code: None,
            blocked_at,
            reason_summary: reason_summary.to_string(),
            reason_detail: reason_detail.map(ToOwned::to_owned),
            runner_stdout_excerpt: None,
            runner_stderr_excerpt: None,
            recovery_hint: Some(blocked_recovery_hint(reason_summary).to_string()),
            auto_recover: None,
        }
    }

    fn base_notification(event_id: Uuid, run_id: Uuid, message: &str) -> Notification {
        Notification {
            event_id,
            run_id,
            ts: Utc::now(),
            channel: "discord".to_string(),
            thread_id: "test-thread".to_string(),
            kind: "progress".to_string(),
            message: message.to_string(),
            attempts: 0,
            next_retry_at: None,
            last_error: None,
        }
    }

    #[test]
    fn flush_notifications_drops_queued_terminal_event_ids() {
        let run = TestRunDir::new("terminal");
        let run_id = Uuid::new_v4();
        let queue_path = run.path.join("notify-queue.jsonl");
        let dispatched_path = run.path.join("notify-dispatched.jsonl");

        let dispatched_id = Uuid::new_v4();
        let dead_letter_id = Uuid::new_v4();

        append_jsonl(
            &queue_path,
            &base_notification(dispatched_id, run_id, "stale dispatched"),
        )
        .expect("append stale dispatched queued");
        append_jsonl(
            &queue_path,
            &base_notification(dead_letter_id, run_id, "stale dead-letter"),
        )
        .expect("append stale dead-letter queued");

        append_jsonl(
            &dispatched_path,
            &DispatchedNotification {
                event_id: dispatched_id,
                run_id,
                dispatched_at: Utc::now(),
                channel: "discord".to_string(),
                thread_id: "test-thread".to_string(),
                kind: "progress".to_string(),
                message: "already delivered".to_string(),
                attempts: 1,
            },
        )
        .expect("append dispatched terminal");

        append_jsonl(
            &dead_letter_path(&run.path),
            &DeadLetterEntry {
                event_id: dead_letter_id,
                run_id,
                moved_at: Utc::now(),
                attempts: 1,
                kind: "progress".to_string(),
                message: "already failed".to_string(),
                last_error: Some("permanent failure".to_string()),
            },
        )
        .expect("append dead-letter terminal");

        let delivered = flush_notifications(&run.path, &test_manifest(&run.path, run_id, false))
            .expect("flush");
        assert_eq!(delivered, 0);

        let remaining_queue = read_jsonl::<Notification>(&queue_path).expect("read queue");
        assert!(remaining_queue.is_empty());
        assert!(
            read_jsonl::<DeliveryAttempt>(&delivery_attempts_path(&run.path))
                .expect("read attempts")
                .is_empty()
        );
        assert!(
            read_jsonl::<DeliveryAck>(&delivery_ack_path(&run.path))
                .expect("read ack")
                .is_empty()
        );
    }

    #[test]
    fn write_runner_state_preserves_newer_status_delivery_fields() {
        let run = TestRunDir::new("runner-status-preserve");
        let newer = Utc::now();
        let older = newer - Duration::seconds(10);

        let existing = RunnerState {
            status_message_id: Some("msg-new".to_string()),
            status_updated_at: Some(newer),
            ..RunnerState::default()
        };
        write_json(&run.path.join("runner-state.json"), &existing).expect("write existing state");

        let stale_writer = RunnerState {
            last_task_id: Some("T1".to_string()),
            status_message_id: Some("msg-old".to_string()),
            status_updated_at: Some(older),
            ..RunnerState::default()
        };
        write_runner_state(&run.path, &stale_writer).expect("write stale runner state");

        let actual = read_json::<RunnerState>(&run.path.join("runner-state.json"))
            .expect("read merged runner state");
        assert_eq!(actual.last_task_id.as_deref(), Some("T1"));
        assert_eq!(actual.status_message_id.as_deref(), Some("msg-new"));
        assert_eq!(actual.status_updated_at, Some(newer));
    }

    #[test]
    fn concurrent_queue_notification_writes_valid_dispatched_jsonl() {
        let run = TestRunDir::new("concurrent-notify");
        let run_id = Uuid::new_v4();
        let handles: Vec<_> = (0..16)
            .map(|idx| {
                let run_path = run.path.clone();
                std::thread::spawn(move || {
                    let manifest = test_manifest(&run_path, run_id, false);
                    queue_notification(&run_path, &manifest, "progress", format!("message {idx}"))
                        .expect("queue notification");
                })
            })
            .collect();

        for handle in handles {
            handle.join().expect("join notification worker");
        }

        let dispatched_path = run.path.join("notify-dispatched.jsonl");
        let raw = fs::read_to_string(&dispatched_path).expect("read dispatched raw");
        assert_eq!(raw.lines().count(), 16);
        for line in raw.lines() {
            serde_json::from_str::<DispatchedNotification>(line).expect("valid dispatched line");
        }

        let dispatched = read_jsonl::<DispatchedNotification>(&dispatched_path)
            .expect("read dispatched notifications");
        assert_eq!(dispatched.len(), 16);
        let event_ids: std::collections::HashSet<_> =
            dispatched.iter().map(|d| d.event_id).collect();
        assert_eq!(event_ids.len(), 16);
    }

    #[test]
    fn flush_notifications_processes_duplicate_event_id_once_per_flush() {
        let run = TestRunDir::new("duplicate");
        let run_id = Uuid::new_v4();
        let queue_path = run.path.join("notify-queue.jsonl");
        let event_id = Uuid::new_v4();

        append_jsonl(&queue_path, &base_notification(event_id, run_id, "first"))
            .expect("append first duplicate");
        append_jsonl(&queue_path, &base_notification(event_id, run_id, "second"))
            .expect("append second duplicate");

        let delivered = flush_notifications(&run.path, &test_manifest(&run.path, run_id, false))
            .expect("flush");
        assert_eq!(delivered, 1);

        assert!(
            read_jsonl::<Notification>(&queue_path)
                .expect("read queue")
                .is_empty()
        );

        let dispatched =
            read_jsonl::<DispatchedNotification>(&run.path.join("notify-dispatched.jsonl"))
                .expect("read dispatched");
        assert_eq!(dispatched.len(), 1);
        assert_eq!(dispatched[0].event_id, event_id);

        let attempts = read_jsonl::<DeliveryAttempt>(&delivery_attempts_path(&run.path))
            .expect("read attempts");
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].event_id, event_id);

        let ack = read_jsonl::<DeliveryAck>(&delivery_ack_path(&run.path)).expect("read ack");
        assert_eq!(ack.len(), 1);
        assert!(ack[0].ok);
        assert_eq!(ack[0].event_id, event_id);
        assert_eq!(ack[0].attempts, 1);
    }

    #[test]
    fn flush_notifications_keeps_retry_wait_when_not_due() {
        let run = TestRunDir::new("retry-wait");
        let run_id = Uuid::new_v4();
        let queue_path = run.path.join("notify-queue.jsonl");
        let event_id = Uuid::new_v4();

        let mut notification = base_notification(event_id, run_id, "waiting for backoff");
        notification.attempts = 2;
        notification.next_retry_at = Some(Utc::now() + Duration::seconds(120));
        append_jsonl(&queue_path, &notification).expect("append queued retry_wait");

        let delivered = flush_notifications(&run.path, &test_manifest(&run.path, run_id, false))
            .expect("flush");
        assert_eq!(delivered, 0);

        let queue = read_jsonl::<Notification>(&queue_path).expect("read queue");
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].event_id, event_id);
        assert_eq!(queue[0].attempts, 2);
        assert!(queue[0].next_retry_at.is_some());

        assert!(
            read_jsonl::<DeliveryAttempt>(&delivery_attempts_path(&run.path))
                .expect("read attempts")
                .is_empty()
        );
        assert!(
            read_jsonl::<DeliveryAck>(&delivery_ack_path(&run.path))
                .expect("read ack")
                .is_empty()
        );
    }

    #[test]
    fn flush_notifications_retries_when_next_retry_at_is_due() {
        let run = TestRunDir::new("retry-due");
        let run_id = Uuid::new_v4();
        let queue_path = run.path.join("notify-queue.jsonl");
        let event_id = Uuid::new_v4();

        let mut notification = base_notification(event_id, run_id, "retry due now");
        notification.attempts = 2;
        notification.next_retry_at = Some(Utc::now() - Duration::seconds(1));
        append_jsonl(&queue_path, &notification).expect("append queued due retry");

        let delivered = flush_notifications(&run.path, &test_manifest(&run.path, run_id, false))
            .expect("flush");
        assert_eq!(delivered, 1);
        assert!(
            read_jsonl::<Notification>(&queue_path)
                .expect("read queue")
                .is_empty()
        );

        let attempts = read_jsonl::<DeliveryAttempt>(&delivery_attempts_path(&run.path))
            .expect("read attempts");
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].event_id, event_id);
        assert_eq!(attempts[0].attempts, 3);
        assert!(attempts[0].success);

        let ack = read_jsonl::<DeliveryAck>(&delivery_ack_path(&run.path)).expect("read ack");
        assert_eq!(ack.len(), 1);
        assert_eq!(ack[0].event_id, event_id);
        assert!(ack[0].ok);
        assert_eq!(ack[0].attempts, 3);
    }

    #[test]
    fn queue_notification_flushes_immediately_when_delivery_succeeds() {
        let run = TestRunDir::new("queue-immediate-flush");
        let run_id = Uuid::new_v4();
        let manifest = test_manifest(&run.path, run_id, false);

        let event_id = queue_notification(&run.path, &manifest, "progress", "immediate")
            .expect("queue notification");

        let queued =
            read_jsonl::<Notification>(&run.path.join("notify-queue.jsonl")).expect("read queue");
        assert!(queued.is_empty());

        let dispatched =
            read_jsonl::<DispatchedNotification>(&run.path.join("notify-dispatched.jsonl"))
                .expect("read dispatched");
        assert_eq!(dispatched.len(), 1);
        assert_eq!(dispatched[0].event_id, event_id);
        assert_eq!(dispatched[0].kind, "progress");
    }

    #[test]
    fn queue_main_feedback_summary_flushes_immediately() {
        let run = TestRunDir::new("main-feedback-immediate-flush");
        let run_id = Uuid::new_v4();
        let mut manifest = test_manifest(&run.path, run_id, false);
        manifest.feedback_channel = Some("discord".to_string());
        manifest.feedback_thread_id = Some("main-thread".to_string());

        let event_id = queue_main_feedback_summary(&run.path, &manifest, "summary")
            .expect("queue main feedback")
            .expect("event id");

        let queued =
            read_jsonl::<Notification>(&run.path.join("notify-queue.jsonl")).expect("read queue");
        assert!(queued.is_empty());

        let dispatched =
            read_jsonl::<DispatchedNotification>(&run.path.join("notify-dispatched.jsonl"))
                .expect("read dispatched");
        assert_eq!(dispatched.len(), 1);
        assert_eq!(dispatched[0].event_id, event_id);
        assert_eq!(dispatched[0].kind, "main_feedback");
    }

    #[test]
    fn emit_all_tasks_completed_notifications_dispatches_final_notification() {
        let run = TestRunDir::new("all-tasks-completed");
        let run_id = Uuid::new_v4();
        let runner = RunnerState {
            last_task_id: Some("S4-2".to_string()),
            task_loops_started: 3,
            ..RunnerState::default()
        };

        let manifest = test_manifest(&run.path, run_id, false);
        let now = manifest.started_at + Duration::seconds(3723);

        emit_all_tasks_completed_notifications(&run.path, &manifest, &runner, 3, now)
            .expect("emit all tasks completed notifications");

        let dispatched =
            read_jsonl::<DispatchedNotification>(&run.path.join("notify-dispatched.jsonl"))
                .expect("read dispatched");
        assert_eq!(dispatched.len(), 1);
        assert_eq!(dispatched[0].kind, "all_tasks_completed");
        assert!(dispatched[0].message.contains("all tasks completed"));
        assert!(dispatched[0].message.contains("last_task=S4-2"));
        assert!(dispatched[0].message.contains("total_elapsed=1h02m03s"));

        assert!(
            read_jsonl::<Notification>(&run.path.join("notify-queue.jsonl"))
                .expect("read queue")
                .is_empty()
        );
    }

    #[test]
    fn emit_all_tasks_completed_notifications_also_dispatches_main_feedback_when_configured() {
        let run = TestRunDir::new("all-tasks-feedback");
        let run_id = Uuid::new_v4();
        let runner = RunnerState {
            last_task_id: Some("S4-2B".to_string()),
            task_loops_started: 4,
            ..RunnerState::default()
        };

        let mut manifest = test_manifest(&run.path, run_id, false);
        manifest.feedback_channel = Some("discord".to_string());
        manifest.feedback_thread_id = Some("main-thread".to_string());

        let now = manifest.started_at + Duration::seconds(65);
        emit_all_tasks_completed_notifications(&run.path, &manifest, &runner, 4, now)
            .expect("emit all tasks completed notifications");

        let dispatched =
            read_jsonl::<DispatchedNotification>(&run.path.join("notify-dispatched.jsonl"))
                .expect("read dispatched");
        assert_eq!(dispatched.len(), 2);

        let kinds: Vec<_> = dispatched.iter().map(|d| d.kind.as_str()).collect();
        assert!(kinds.contains(&"all_tasks_completed"));
        assert!(kinds.contains(&"main_feedback"));
    }

    #[test]
    fn backoff_schedule_is_expected() {
        assert_eq!(compute_backoff_sec(0), 60);
        assert_eq!(compute_backoff_sec(1), 120);
        assert_eq!(compute_backoff_sec(2), 240);
        assert_eq!(compute_backoff_sec(3), 300);
        assert_eq!(compute_backoff_sec(99), 300);
    }

    #[test]
    fn lease_window_defaults() {
        assert_eq!(lease_window_sec(60), 90);
        assert_eq!(lease_window_sec(30), 60);
        assert_eq!(lease_window_sec(1), 45);
    }

    #[test]
    fn delivery_retry_backoff_schedule() {
        assert_eq!(delivery_retry_backoff_sec(0), 5);
        assert_eq!(delivery_retry_backoff_sec(1), 5);
        assert_eq!(delivery_retry_backoff_sec(2), 15);
        assert_eq!(delivery_retry_backoff_sec(3), 30);
        assert_eq!(delivery_retry_backoff_sec(9), 60);
    }

    #[test]
    fn openclaw_notify_timeout_sec_uses_safe_default_and_accepts_config() {
        assert_eq!(openclaw_notify_timeout_sec_from(None), 15);
        assert_eq!(openclaw_notify_timeout_sec_from(Some("")), 15);
        assert_eq!(openclaw_notify_timeout_sec_from(Some("30")), 30);
        assert_eq!(openclaw_notify_timeout_sec_from(Some("1")), 1);
    }

    #[test]
    fn openclaw_notify_timeout_sec_rejects_invalid_or_unsafe_values() {
        assert_eq!(openclaw_notify_timeout_sec_from(Some("0")), 15);
        assert_eq!(openclaw_notify_timeout_sec_from(Some("-5")), 15);
        assert_eq!(openclaw_notify_timeout_sec_from(Some("nan")), 15);
        assert_eq!(openclaw_notify_timeout_sec_from(Some("999")), 15);
    }

    #[test]
    fn normalize_error_reason_classifies_common_patterns() {
        assert_eq!(
            normalize_error_reason(Some("openclaw message send failed: status=1 stderr=mock")),
            "openclaw_send_failed"
        );
        assert_eq!(
            normalize_error_reason(Some("openclaw message edit failed: status=1 stderr=mock")),
            "openclaw_send_failed"
        );
        assert_eq!(normalize_error_reason(Some("request timed out")), "timeout");
        assert_eq!(
            normalize_error_reason(Some("HTTP 429 rate limit")),
            "rate_limited"
        );
        assert_eq!(
            normalize_error_reason(Some("permission denied")),
            "permission_denied"
        );
        assert_eq!(
            normalize_error_reason(Some("connection refused by peer")),
            "connection_refused"
        );
    }

    #[test]
    fn normalize_error_reason_fallback_sanitizes_digits() {
        let out = normalize_error_reason(Some("Custom Error CODE 12345 at shard 7"));
        assert!(out.contains('#'));
        assert!(!out.contains("12345"));
    }

    #[test]
    fn classify_ack_failure_category_maps_expected_buckets() {
        assert_eq!(
            classify_ack_failure_category(Some("openclaw message send failed: status=1")),
            "transport"
        );
        assert_eq!(
            classify_ack_failure_category(Some("openclaw message edit failed: status=1")),
            "transport"
        );
        assert_eq!(
            classify_ack_failure_category(Some("request timed out")),
            "timeout"
        );
        assert_eq!(
            classify_ack_failure_category(Some("permission denied")),
            "permission"
        );
        assert_eq!(classify_ack_failure_category(Some("HTTP 401")), "auth");
        assert_eq!(
            classify_ack_failure_category(Some("random opaque error")),
            "unknown"
        );
    }

    #[test]
    fn ack_retry_policy_respects_category() {
        let auth = ack_retry_policy("auth", 1);
        assert!(!auth.retryable);
        assert_eq!(auth.max_attempts, 1);

        let transport = ack_retry_policy("transport", 2);
        assert!(transport.retryable);
        assert_eq!(transport.backoff_sec, 15);

        let rate = ack_retry_policy("rate_limited", 3);
        assert!(rate.retryable);
        assert_eq!(rate.backoff_sec, 120);
    }

    #[test]
    fn status_establish_retry_override_forces_retry_when_status_unset() {
        let base = ack_retry_policy("permission", 1);
        assert!(!base.retryable);
        assert_eq!(base.max_attempts, 1);

        let force = should_force_status_establish_retry(NotificationDeliveryMode::EditStatus, None);
        assert!(force);

        let overridden = apply_status_establish_retry_override(base, force);
        assert!(overridden.retryable);
        assert_eq!(overridden.max_attempts, u32::MAX);
        assert!(overridden.backoff_sec >= 5);
    }

    #[test]
    fn status_establish_retry_override_does_not_apply_when_status_exists() {
        let base = ack_retry_policy("permission", 1);
        let force = should_force_status_establish_retry(
            NotificationDeliveryMode::EditStatus,
            Some("status-msg-1"),
        );
        assert!(!force);

        let overridden = apply_status_establish_retry_override(base, force);
        assert!(!overridden.retryable);
        assert_eq!(overridden.max_attempts, 1);
    }

    #[test]
    fn status_establish_retry_override_does_not_apply_for_send_mode() {
        let base = ack_retry_policy("permission", 1);
        let force = should_force_status_establish_retry(NotificationDeliveryMode::Send, None);
        assert!(!force);

        let overridden = apply_status_establish_retry_override(base, force);
        assert!(!overridden.retryable);
        assert_eq!(overridden.max_attempts, 1);
    }

    #[test]
    fn blocked_recovery_hint_maps_common_lock_error() {
        let hint = blocked_recovery_hint("Error: session file locked (timeout 10000ms)");
        assert!(hint.contains("--task-agent-id"));
        assert!(hint.contains("ロック"));
    }

    #[test]
    fn is_phase_or_stacked_dependency_reason_matches_common_phrases() {
        assert!(is_phase_or_stacked_dependency_reason(
            "still cannot be shipped as an isolated green PR without also doing D4"
        ));
        assert!(is_phase_or_stacked_dependency_reason(
            "phase/stacked dependency is required before this lands"
        ));
        assert!(is_phase_or_stacked_dependency_reason(
            "this needs a prior phase before it can merge"
        ));
        assert!(!is_phase_or_stacked_dependency_reason(
            "merge state is dirty for PR_URL=https://example.test/pull/1"
        ));
    }

    #[test]
    fn missing_current_task_id_guard_reason_distinguishes_waiting_merge_blocked_and_dependency() {
        let waiting_merge =
            missing_current_task_id_guard_reason(Some(&RunnerTaskState::WaitingMerge))
                .expect("waiting_merge guard reason");
        assert!(waiting_merge.contains("waiting_merge"));
        assert!(waiting_merge.contains("blocked or completed"));

        let waiting_dependency =
            missing_current_task_id_guard_reason(Some(&RunnerTaskState::WaitingDependency))
                .expect("waiting_dependency guard reason");
        assert!(waiting_dependency.contains("waiting_dependency"));
        assert!(waiting_dependency.contains("external dependency"));
        assert!(waiting_dependency.contains("generic blocked or completed"));

        let blocked = missing_current_task_id_guard_reason(Some(&RunnerTaskState::Blocked))
            .expect("blocked guard reason");
        assert!(blocked.contains("already blocked"));
        assert!(blocked.contains("waiting_merge, dependency wait, or completed"));

        assert!(missing_current_task_id_guard_reason(Some(&RunnerTaskState::Running)).is_none());
        assert!(missing_current_task_id_guard_reason(None).is_none());
    }

    #[test]
    fn format_task_blocked_notification_includes_mention_reason_and_next_step() {
        let run = TestRunDir::new("blocked-notify");
        let run_id = Uuid::new_v4();
        let mut manifest = test_manifest(&run.path, run_id, false);
        manifest.requester_user_id = Some("test-user-id".to_string());
        manifest.auto_recover_blocked = true;

        let blocked = test_blocked_context(
            "S5-2",
            "blocked task",
            2,
            Some("https://github.com/n01e0/claw-loop/pull/999"),
            r#"runner exit=2: unexpected EOF while looking for matching '"'"#,
            None,
            Utc::now(),
        );
        let msg = format_task_blocked_notification(&manifest, &blocked);

        assert!(msg.starts_with("<@test-user-id> タスクが block された: S5-2"));
        assert!(msg.contains("- 原因:"));
        assert!(msg.contains("- 分類: generic blocked。依存待ちではなく、現在の task / PR / runner 側で原因修正が必要"));
        assert!(msg.contains(
            "- 今待っているもの: 自然解消待ちはない。原因修正または auto-recover の結果待ち"
        ));
        assert!(msg.contains("- 人手介入: まずは不要。daemon が auto-recover を試す"));
        assert!(msg.contains("- 解決方法:"));
        assert!(msg.contains("次の動作: auto-recover が有効"));
        assert!(msg.contains("PR_URL=https://github.com/n01e0/claw-loop/pull/999"));
    }

    #[test]
    fn format_waiting_merge_notification_names_wait_target_and_intervention() {
        let msg = format_waiting_merge_notification(
            "D6",
            Some("https://github.com/n01e0/claw-loop/pull/126"),
            true,
        );

        assert!(msg.contains("task waiting merge: D6"));
        assert!(msg.contains("- 分類: waiting_merge（generic blocked ではない）"));
        assert!(msg.contains(
            "- 今待っているもの: PR https://github.com/n01e0/claw-loop/pull/126 の CI / merge 完了"
        ));
        assert!(msg.contains("- 次に進む条件: PR が merged したら daemon が task 完了へ進める"));
        assert!(msg.contains("required status checks"));
        assert!(msg.contains("- 人手介入: 通常は不要。CI fail / DIRTY / merge conflict / warning が出た時だけ確認する"));
    }

    #[test]
    fn format_task_blocked_notification_shows_detail_for_blocked_regression() {
        let run = TestRunDir::new("blocked-notify-detail");
        let run_id = Uuid::new_v4();
        let mut manifest = test_manifest(&run.path, run_id, false);
        manifest.auto_recover_blocked = true;

        let blocked = test_blocked_context(
            "A1",
            "blocked task",
            1,
            Some("https://example.test/pull/1"),
            "runner exit=Some(2): initial blocked",
            Some(
                "runner exit=Some(2): initial blocked\nstderr: missing fixture in generated workspace",
            ),
            Utc::now(),
        );
        let msg = format_task_blocked_notification(&manifest, &blocked);

        assert!(msg.contains("- 原因: runner exit=Some(2): initial blocked"));
        assert!(msg.contains("- 詳細: runner exit=Some(2): initial blocked stderr: missing fixture in generated workspace"));
        assert!(msg.contains("- 解決方法:"));
        assert!(msg.contains("次の動作: auto-recover が有効"));
    }

    #[test]
    fn format_task_blocked_notification_shows_detail_and_recovery_halt_for_recover_task() {
        let run = TestRunDir::new("blocked-notify-recover");
        let run_id = Uuid::new_v4();
        let mut manifest = test_manifest(&run.path, run_id, false);
        manifest.auto_recover_blocked = true;

        let blocked = test_blocked_context(
            "B2-RECOVER",
            "recovery task",
            4,
            None,
            "runner exit=Some(2): TASK_BLOCKED: B2 still cannot be shipped as an isolated green PR without also doing B3, because the current runtime response types/handlers still expose the very staff-visible fields the requested regression needs to change before this can pass in isolation",
            Some(
                "runner exit=Some(2): TASK_BLOCKED: B2 still cannot be shipped as an isolated green PR without also doing B3, because the current runtime response types/handlers still expose the very staff-visible fields the requested regression needs to change before this can pass in isolation\nextra detail",
            ),
            Utc::now(),
        );
        let msg = format_task_blocked_notification(&manifest, &blocked);

        assert!(msg.contains("- 詳細:"));
        assert!(msg.contains("green PR"));
        assert!(msg.contains("- 分類: phase/stacked dependency が必要。1 task 1 PR ではなく前段 task / PR の完了待ちとして扱うべき状態"));
        assert!(msg.contains("- 今待っているもの: 前段 task / PR の特定または順序調整。依存先が判明したら `TASK_WAITING_DEPENDENCY` として待機へ切り替える"));
        assert!(msg.contains("phase/stacked sequencing が必要"));
        assert!(msg.contains("`TASK_WAITING_DEPENDENCY`"));
        assert!(msg.contains("- 人手介入: 必要。依存先 task / PR の特定、task 分割の見直し、または phase / stacked 順序の調整を行う"));
        assert!(msg.contains(
            "生成された recovery task 自体が失敗したので、auto-recover はここで停止する"
        ));
    }

    #[test]
    fn format_auto_recover_decision_notification_shows_cause_plan_task_and_continue_state() {
        let mut blocked = test_blocked_context(
            "A2",
            "stabilize waiting-merge retry path",
            3,
            Some("https://example.test/pull/1"),
            "merge state is dirty for PR_URL=https://example.test/pull/1",
            Some(
                "merge state is dirty for PR_URL=https://example.test/pull/1\nconflicts detected in generated branch",
            ),
            Utc::now(),
        );
        blocked.auto_recover = Some(AutoRecoverDecisionSnapshot {
            state: AutoRecoverDecisionState::Queued,
            decided_at: Utc::now(),
            reason_key: "merge state is dirty".to_string(),
            attempts: 1,
            same_reason_count: 1,
            max_attempts: 3,
            guard_reason: None,
            recovery_task: Some(RecoveryTaskSnapshot {
                id: "A2-RECOVER".to_string(),
                line: 4,
                text: "resolve waiting_merge block for task A2 (stabilize waiting-merge retry path): PR branch を clean にして merge 不能 を解消する".to_string(),
            }),
        });

        let msg = format_auto_recover_decision_notification(&blocked)
            .expect("recovery decision notification");
        assert!(msg.starts_with("auto-recovery decision: A2 PR_URL=https://example.test/pull/1"));
        assert!(msg.contains("- 原因:"));
        assert!(msg.contains("conflicts detected in generated branch"));
        assert!(msg.contains("- 解決方針:"));
        assert!(msg.contains("- 実際に積んだ recovery task: A2-RECOVER:"));
        assert!(msg.contains("- 状態: auto-recover 継続"));
    }

    #[test]
    fn format_auto_recover_halt_notification_points_humans_to_failed_recovery_task() {
        let mut blocked = test_blocked_context(
            "A2-RECOVER",
            "repair merge blockers on generated branch",
            4,
            Some("https://example.test/pull/1"),
            "runner exit=Some(2): generated recovery task failed again",
            Some(
                "runner exit=Some(2): generated recovery task failed again\nstderr mentions missing fixture",
            ),
            Utc::now(),
        );
        blocked.runner_stderr_excerpt = Some("missing fixture in recovery workspace".to_string());
        blocked.runner_stdout_excerpt = Some("partial recovery output".to_string());
        blocked.auto_recover = Some(AutoRecoverDecisionSnapshot {
            state: AutoRecoverDecisionState::Halted,
            decided_at: Utc::now(),
            reason_key: "generated recovery task failed".to_string(),
            attempts: 1,
            same_reason_count: 1,
            max_attempts: 3,
            guard_reason: Some(
                "auto-recover halted: generated recovery task failed (A2-RECOVER)".to_string(),
            ),
            recovery_task: None,
        });

        let msg =
            format_auto_recover_halt_notification(&blocked).expect("recovery halt notification");
        assert!(msg.starts_with("auto-recovery halted: A2-RECOVER"));
        assert!(msg.contains("- 停止理由:"));
        assert!(msg.contains("- 原因:"));
        assert!(msg.contains("- 次に見るポイント:"));
        assert!(msg.contains(
            "- 失敗した recovery task: A2-RECOVER: repair merge blockers on generated branch"
        ));
        assert!(msg.contains("- 元タスク: A2"));
        assert!(msg.contains("- stderr: missing fixture in recovery workspace"));
        assert!(msg.contains("- stdout: partial recovery output"));
        assert!(msg.contains("- PR: https://example.test/pull/1"));
        assert!(msg.contains("- 手動での解決方針:"));
    }

    #[test]
    fn format_orphan_blocked_notification_mentions_requester() {
        let run = TestRunDir::new("orphan-notify");
        let run_id = Uuid::new_v4();
        let mut manifest = test_manifest(&run.path, run_id, false);
        manifest.requester_user_id = Some("test-user-id".to_string());

        let msg = format_orphan_blocked_notification(&manifest, 12345);
        assert!(msg.starts_with("<@test-user-id> run が block された: daemon pid 12345"));
        assert!(msg.contains("- 原因:"));
        assert!(msg.contains("- 解決方法:"));
    }

    #[test]
    fn normalize_blocked_reason_for_recovery_compacts_and_lowercases() {
        let normalized = normalize_blocked_reason_for_recovery("Runner EXIT=2:\n  Timeout ERROR");
        assert!(normalized.contains("runner exit=2:"));
        assert!(normalized.contains("timeout error"));
        assert!(!normalized.contains('\n'));
    }

    #[test]
    fn build_recovery_task_text_uses_hint_and_task_context() {
        let blocked = test_blocked_context(
            "A2",
            "stabilize waiting-merge retry path",
            3,
            Some("https://example.test/pull/1"),
            "merge state is dirty for PR_URL=https://example.test/pull/1",
            Some("merge state is dirty for PR_URL=https://example.test/pull/1\nconflicts detected"),
            Utc::now(),
        );

        let text = build_recovery_task_text(&blocked);
        assert!(text.starts_with(
            "resolve waiting_merge block for task A2 (stabilize waiting-merge retry path):"
        ));
        assert!(text.contains("merge 不能"));
        assert!(text.contains("blocked: merge state is dirty"));
        assert!(text.chars().count() <= 241);
    }

    #[test]
    fn build_runner_blocked_context_keeps_detail_and_output_excerpts() {
        let task = TaskChecklistEntry {
            line_no: 7,
            done: false,
            id: "N2".to_string(),
            text: "blocked task".to_string(),
        };
        let now = Utc::now();
        let blocked = super::build_runner_blocked_context(
            Some(&task),
            Some(2),
            "progress line\nTASK_DONE PR_URL=https://example.test/pull/9",
            "first line\nsecond line with more detail",
            Some("https://example.test/pull/9"),
            now,
        );

        assert_eq!(blocked.task_id, "N2");
        assert_eq!(blocked.source, BlockedContextSource::RunnerExit);
        assert_eq!(blocked.exit_code, Some(2));
        assert_eq!(
            blocked.pr_url.as_deref(),
            Some("https://example.test/pull/9")
        );
        assert!(
            blocked
                .reason_summary
                .contains("runner exit=Some(2): first line")
        );
        assert!(
            blocked
                .reason_detail
                .as_deref()
                .expect("reason detail")
                .contains("second line with more detail")
        );
        assert!(
            blocked
                .runner_stdout_excerpt
                .as_deref()
                .expect("stdout excerpt")
                .contains("TASK_DONE PR_URL=https://example.test/pull/9")
        );
        assert!(
            blocked
                .runner_stderr_excerpt
                .as_deref()
                .expect("stderr excerpt")
                .contains("second line with more detail")
        );
    }

    #[test]
    fn maybe_auto_recover_blocked_task_queues_recovery_for_generic_runner_block() {
        let run = TestRunDir::new("generic-block-auto-recover");
        let run_id = Uuid::new_v4();
        let task_file = run.path.join("tasklist.md");
        fs::write(&task_file, "- [ ] G1: generic blocked task\n").expect("write task file");

        let manifest_path = run.path.join("manifest.json");
        let mut manifest = test_manifest(&run.path, run_id, false);
        manifest.auto_recover_blocked = true;
        manifest.task_file = task_file.clone();
        write_json(&manifest_path, &manifest).expect("write manifest");

        let now = Utc::now();
        let mut blocked = test_blocked_context(
            "G1",
            "generic blocked task",
            1,
            None,
            "runner exit=Some(2): missing fixture in generated workspace",
            Some(
                "runner exit=Some(2): missing fixture in generated workspace\nextra stderr detail",
            ),
            now,
        );
        blocked.source = BlockedContextSource::RunnerExit;
        blocked.exit_code = Some(2);

        let mut runner_state = RunnerState {
            current_task_id: Some("G1".into()),
            current_task_text: Some("generic blocked task".into()),
            current_task_line: Some(1),
            current_task_started_at: Some(now),
            current_task_state: Some(RunnerTaskState::Blocked),
            current_task_blocked_reason: Some(blocked.reason_summary.clone()),
            current_blocked_context: Some(blocked.clone()),
            last_task_id: Some("G1".into()),
            last_task_state: Some(RunnerTaskState::Blocked),
            last_task_at: Some(now),
            last_task_reason: Some(blocked.reason_summary.clone()),
            last_blocked_context: Some(blocked.clone()),
            ..RunnerState::default()
        };
        let mut state = State {
            version: 1,
            status: LoopStatus::Blocked,
            summary: "task blocked: G1".into(),
            waiting_reason: blocked.reason_summary.clone(),
            lease_expires_at: now,
            updated_at: now,
            ticks: 0,
        };
        let mut task_done_now = 0;
        let mut task_loops_completed = 0;

        let outcome = maybe_auto_recover_blocked_task(
            &run.path,
            &manifest_path,
            &mut manifest,
            &task_file,
            &mut runner_state,
            &mut state,
            &blocked,
            now,
            &mut task_done_now,
            &mut task_loops_completed,
        )
        .expect("auto recover generic blocked task");

        assert_eq!(outcome, AutoRecoverBlockedOutcome::Recovered);
        assert_eq!(state.status, LoopStatus::Running);
        let content = fs::read_to_string(&task_file).expect("read task file");
        assert!(content.contains("- [x] G1: generic blocked task"));
        assert!(content.contains("- [ ] G1-RECOVER"));
        let dispatched =
            read_jsonl::<DispatchedNotification>(&run.path.join("notify-dispatched.jsonl"))
                .expect("read dispatched notifications");
        let decision_note = dispatched
            .iter()
            .find(|item| item.kind == "task_recovery_decision")
            .expect("recovery decision notification");
        assert!(decision_note.message.contains("- 状態: auto-recover 継続"));
        let recovery_text = runner_state
            .last_blocked_context
            .as_ref()
            .and_then(|ctx| ctx.auto_recover.as_ref())
            .and_then(|decision| decision.recovery_task.as_ref())
            .map(|task| task.text.clone())
            .expect("recovery task text");
        assert!(
            recovery_text.starts_with("resolve runner block for task G1 (generic blocked task):")
        );
    }

    #[test]
    fn maybe_auto_recover_blocked_task_queues_recovery_for_waiting_merge_ci_failures() {
        let run = TestRunDir::new("waiting-merge-auto-recover");
        let run_id = Uuid::new_v4();
        let task_file = run.path.join("tasklist.md");
        fs::write(&task_file, "- [ ] A2: original task\n").expect("write task file");

        let manifest_path = run.path.join("manifest.json");
        let mut manifest = test_manifest(&run.path, run_id, false);
        manifest.auto_recover_blocked = true;
        manifest.task_file = task_file.clone();
        write_json(&manifest_path, &manifest).expect("write manifest");

        let now = Utc::now();
        let blocked = test_blocked_context(
            "A2",
            "original task",
            1,
            Some("https://example.test/pull/1"),
            "CI failed for PR_URL=https://example.test/pull/1 checks=rust:FAILURE",
            Some(
                "CI failed for PR_URL=https://example.test/pull/1 checks=rust:FAILURE\njob rust failed in merge recheck",
            ),
            now,
        );
        let mut runner_state = RunnerState {
            current_task_id: Some("A2".into()),
            current_task_text: Some("original task".into()),
            current_task_line: Some(1),
            current_task_started_at: Some(now),
            current_task_state: Some(RunnerTaskState::Blocked),
            current_task_blocked_reason: Some(blocked.reason_summary.clone()),
            current_blocked_context: Some(blocked.clone()),
            current_task_pr_url: Some("https://example.test/pull/1".into()),
            last_task_id: Some("A2".into()),
            last_task_state: Some(RunnerTaskState::Blocked),
            last_task_at: Some(now),
            last_task_reason: Some(blocked.reason_summary.clone()),
            last_blocked_context: Some(blocked.clone()),
            last_task_pr_url: Some("https://example.test/pull/1".into()),
            ..RunnerState::default()
        };
        let mut state = State {
            version: 1,
            status: LoopStatus::Blocked,
            summary: "task blocked: A2".into(),
            waiting_reason: blocked.reason_summary.clone(),
            lease_expires_at: now,
            updated_at: now,
            ticks: 0,
        };
        let mut task_done_now = 0;
        let mut task_loops_completed = 0;

        let outcome = maybe_auto_recover_blocked_task(
            &run.path,
            &manifest_path,
            &mut manifest,
            &task_file,
            &mut runner_state,
            &mut state,
            &blocked,
            now,
            &mut task_done_now,
            &mut task_loops_completed,
        )
        .expect("auto recover waiting-merge blocked task");

        assert_eq!(outcome, AutoRecoverBlockedOutcome::Recovered);
        assert_eq!(state.status, LoopStatus::Running);
        assert!(
            state
                .summary
                .starts_with("auto-recovery queued: A2-RECOVER")
        );
        assert_eq!(task_done_now, 1);
        assert_eq!(task_loops_completed, 1);
        assert_eq!(runner_state.current_task_id, None);
        assert!(runner_state.current_blocked_context.is_none());
        assert!(
            runner_state
                .auto_recover_last_task_id
                .as_deref()
                .expect("recovery task id")
                .starts_with("A2-RECOVER")
        );
        let last_blocked = runner_state
            .last_blocked_context
            .as_ref()
            .expect("last blocked context");
        let decision = last_blocked
            .auto_recover
            .as_ref()
            .expect("auto-recover decision");
        assert_eq!(decision.state, AutoRecoverDecisionState::Queued);
        assert_eq!(
            decision.reason_key,
            "ci failed for pr_url=https://example.test/pull/1 checks=rust:failure job rust failed in merge recheck"
        );
        let recovery_text = &decision.recovery_task.as_ref().expect("recovery task").text;
        assert!(
            recovery_text.starts_with("resolve waiting_merge block for task A2 (original task):")
        );
        assert!(recovery_text.contains("job rust failed in merge recheck"));

        let dispatched =
            read_jsonl::<DispatchedNotification>(&run.path.join("notify-dispatched.jsonl"))
                .expect("read dispatched notifications");
        let decision_note = dispatched
            .iter()
            .find(|item| item.kind == "task_recovery_decision")
            .expect("recovery decision notification");
        assert!(decision_note.message.contains("- 原因:"));
        assert!(decision_note.message.contains("- 解決方針:"));
        assert!(
            decision_note
                .message
                .contains("- 実際に積んだ recovery task:")
        );
        assert!(decision_note.message.contains("- 状態: auto-recover 継続"));

        let content = fs::read_to_string(&task_file).expect("read task file");
        assert!(content.contains("- [x] A2: original task"));
        assert!(content.contains("- [ ] A2-RECOVER"));
    }

    #[test]
    fn maybe_auto_recover_blocked_task_dispatches_halt_notification_for_failed_recovery_task() {
        let run = TestRunDir::new("recovery-halt-notify");
        let run_id = Uuid::new_v4();
        let task_file = run.path.join("tasklist.md");
        fs::write(
            &task_file,
            "- [ ] A2: original task\n- [ ] A2-RECOVER: generated follow-up\n",
        )
        .expect("write task file");

        let manifest_path = run.path.join("manifest.json");
        let mut manifest = test_manifest(&run.path, run_id, false);
        manifest.auto_recover_blocked = true;
        manifest.task_file = task_file.clone();
        write_json(&manifest_path, &manifest).expect("write manifest");

        let now = Utc::now();
        let mut blocked = test_blocked_context(
            "A2-RECOVER",
            "generated follow-up",
            2,
            Some("https://example.test/pull/1"),
            "runner exit=Some(2): generated recovery task failed again",
            Some(
                "runner exit=Some(2): generated recovery task failed again\nmissing fixture in recovery workspace",
            ),
            now,
        );
        blocked.runner_stderr_excerpt = Some("missing fixture in recovery workspace".to_string());
        blocked.runner_stdout_excerpt = Some("partial recovery output".to_string());

        let mut runner_state = RunnerState {
            current_task_id: Some("A2-RECOVER".into()),
            current_task_text: Some("generated follow-up".into()),
            current_task_line: Some(2),
            current_task_started_at: Some(now),
            current_task_state: Some(RunnerTaskState::Blocked),
            current_task_blocked_reason: Some(blocked.reason_summary.clone()),
            current_blocked_context: Some(blocked.clone()),
            last_task_id: Some("A2-RECOVER".into()),
            last_task_state: Some(RunnerTaskState::Blocked),
            last_task_at: Some(now),
            last_task_reason: Some(blocked.reason_summary.clone()),
            last_blocked_context: Some(blocked.clone()),
            ..RunnerState::default()
        };
        let mut state = State {
            version: 1,
            status: LoopStatus::Blocked,
            summary: "task blocked: A2-RECOVER".into(),
            waiting_reason: blocked.reason_summary.clone(),
            lease_expires_at: now,
            updated_at: now,
            ticks: 0,
        };
        let mut task_done_now = 1;
        let mut task_loops_completed = 1;

        let outcome = maybe_auto_recover_blocked_task(
            &run.path,
            &manifest_path,
            &mut manifest,
            &task_file,
            &mut runner_state,
            &mut state,
            &blocked,
            now,
            &mut task_done_now,
            &mut task_loops_completed,
        )
        .expect("halt auto recover for failed recovery task");

        assert_eq!(outcome, AutoRecoverBlockedOutcome::Halted);
        assert_eq!(state.status, LoopStatus::Stopped);
        assert_eq!(state.summary, "auto-recovery halted");
        assert!(
            state
                .waiting_reason
                .contains("generated recovery task failed")
        );

        let dispatched =
            read_jsonl::<DispatchedNotification>(&run.path.join("notify-dispatched.jsonl"))
                .expect("read dispatched notifications");
        let halt_note = dispatched
            .iter()
            .find(|item| item.kind == "task_recovery_halted")
            .expect("halt notification");
        assert!(halt_note.message.contains("- 次に見るポイント:"));
        assert!(halt_note.message.contains("- 元タスク: A2"));
        assert!(
            halt_note
                .message
                .contains("missing fixture in recovery workspace")
        );
    }

    #[test]
    fn auto_recover_guard_reason_stops_on_recovery_task_failure() {
        let runner = RunnerState::default();
        let reason = auto_recover_guard_reason("S5-6-RECOVER", "same-reason", &runner, 3);
        assert!(
            reason
                .expect("guard reason")
                .contains("generated recovery task failed")
        );
    }

    #[test]
    fn auto_recover_guard_reason_stops_on_duplicate_reason() {
        let runner = RunnerState {
            auto_recover_last_reason: Some("same-reason".to_string()),
            auto_recover_same_reason_count: 1,
            ..RunnerState::default()
        };
        let reason = auto_recover_guard_reason("S5-6", "same-reason", &runner, 3);
        assert_eq!(
            reason,
            Some("auto-recover halted: duplicate blocked reason detected".to_string())
        );
    }

    #[test]
    fn auto_recover_guard_reason_stops_on_max_attempts() {
        let runner = RunnerState {
            auto_recover_attempts: 3,
            ..RunnerState::default()
        };
        let reason = auto_recover_guard_reason("S5-6", "new-reason", &runner, 3);
        assert!(
            reason
                .expect("guard reason")
                .contains("max attempts reached")
        );
    }

    #[test]
    fn select_next_task_entry_prioritizes_recovery_task_when_present() {
        let entries = vec![
            TaskChecklistEntry {
                line_no: 3,
                done: false,
                id: "H2".to_string(),
                text: "normal task".to_string(),
            },
            TaskChecklistEntry {
                line_no: 7,
                done: false,
                id: "H1-RECOVER".to_string(),
                text: "recovery task".to_string(),
            },
        ];

        let next = select_next_task_entry(&entries, Some("H1-RECOVER")).expect("next task");
        assert_eq!(next.id, "H1-RECOVER");
    }

    #[test]
    fn select_next_task_entry_falls_back_to_first_open_when_recovery_missing() {
        let entries = vec![
            TaskChecklistEntry {
                line_no: 3,
                done: false,
                id: "H2".to_string(),
                text: "normal task".to_string(),
            },
            TaskChecklistEntry {
                line_no: 7,
                done: true,
                id: "H1-RECOVER".to_string(),
                text: "recovery task".to_string(),
            },
        ];

        let next = select_next_task_entry(&entries, Some("H1-RECOVER")).expect("next task");
        assert_eq!(next.id, "H2");
    }

    #[test]
    fn classify_task_execution_kind_treats_failure_first_gating_work_as_repair() {
        let entry = TaskChecklistEntry {
            line_no: 1,
            done: false,
            id: "C14-2".to_string(),
            text: "backlog>0 時の failure-first gating を daemon/task-agent に実装し、state / waiting_reason を明示する".to_string(),
        };

        assert_eq!(
            classify_task_execution_kind(&entry),
            TaskExecutionKind::Repair
        );
    }

    #[test]
    fn select_next_task_with_backlog_prioritizes_repair_task_over_feature_task() {
        let entries = vec![
            TaskChecklistEntry {
                line_no: 3,
                done: false,
                id: "F1".to_string(),
                text: "add shiny feature".to_string(),
            },
            TaskChecklistEntry {
                line_no: 5,
                done: false,
                id: "R1".to_string(),
                text: "fix flaky backlog gate".to_string(),
            },
        ];
        let backlog_snapshot = BacklogSnapshot {
            detector_file: PathBuf::from("/tmp/backlog.json"),
            repo_path: PathBuf::from("/tmp/repo"),
            status: "backlog".to_string(),
            backlog_count: 2,
            summary: "2 repairs pending".to_string(),
            updated_at: Utc::now(),
        };

        let next = select_next_task_with_backlog(&entries, None, Some(&backlog_snapshot));
        match next {
            TaskSelectionOutcome::Next(entry) => assert_eq!(entry.id, "R1"),
            other => panic!("expected repair task selection, got {other:?}"),
        }
    }

    #[test]
    fn select_next_task_with_backlog_waits_when_only_feature_tasks_are_open() {
        let entries = vec![TaskChecklistEntry {
            line_no: 3,
            done: false,
            id: "F1".to_string(),
            text: "add shiny feature".to_string(),
        }];
        let backlog_snapshot = BacklogSnapshot {
            detector_file: PathBuf::from("/tmp/backlog.json"),
            repo_path: PathBuf::from("/tmp/repo"),
            status: "backlog".to_string(),
            backlog_count: 3,
            summary: "feature work blocked behind backlog".to_string(),
            updated_at: Utc::now(),
        };

        let next = select_next_task_with_backlog(&entries, None, Some(&backlog_snapshot));
        match next {
            TaskSelectionOutcome::Waiting { reason, .. } => {
                assert!(reason.contains("backlog gate active"));
                assert!(reason.contains("F1"));
                assert!(reason.contains("feature"));
            }
            other => panic!("expected waiting outcome, got {other:?}"),
        }
    }

    #[test]
    fn read_backlog_snapshot_accepts_repo_bound_file() {
        let dir = TestRunDir::new("backlog-detector-file");
        let detector_file = dir.path().join("backlog.json");
        fs::write(
            &detector_file,
            serde_json::json!({
                "repo_path": dir.path(),
                "status": "backlog",
                "backlog_count": 2,
                "summary": "2 repairs pending",
                "updated_at": Utc::now().to_rfc3339(),
            })
            .to_string(),
        )
        .expect("write detector file");

        let snapshot = read_backlog_snapshot(dir.path(), &detector_file, 900, Utc::now())
            .expect("backlog snapshot");
        assert_eq!(snapshot.status, "backlog");
        assert_eq!(snapshot.backlog_count, 2);
    }

    #[test]
    fn read_backlog_snapshot_rejects_repo_mismatch() {
        let dir = TestRunDir::new("backlog-detector-mismatch");
        let other_repo = TestRunDir::new("backlog-detector-other");
        let detector_file = dir.path().join("backlog.json");
        fs::write(
            &detector_file,
            serde_json::json!({
                "repo_path": other_repo.path(),
                "status": "backlog",
                "backlog_count": 1,
                "summary": "wrong repo",
                "updated_at": Utc::now().to_rfc3339(),
            })
            .to_string(),
        )
        .expect("write detector file");

        let err = read_backlog_snapshot(dir.path(), &detector_file, 900, Utc::now())
            .expect_err("expected repo mismatch");
        assert!(err.to_string().contains("targets repo"));
    }

    #[test]
    fn task_contract_line_uses_last_protocol_line() {
        let stdout = "progress one\nTASK_WAITING_MERGE PR_URL=https://example/pull/1\n";
        assert_eq!(
            task_contract_line(stdout).as_deref(),
            Some("TASK_WAITING_MERGE PR_URL=https://example/pull/1")
        );
    }

    #[test]
    fn blocked_reason_from_runner_uses_protocol_line_when_stderr_empty() {
        let stdout = "chatty preface\nTASK_BLOCKED: auto-merge enable failed\n";
        let reason = blocked_reason_from_runner("", stdout);
        assert_eq!(reason, "TASK_BLOCKED: auto-merge enable failed");
    }

    #[test]
    fn auto_stop_reason_hits_max_ticks() {
        let started_at = chrono::Utc::now() - chrono::Duration::seconds(1);
        let now = chrono::Utc::now();
        let reason = compute_auto_stop_reason(3, Some(3), started_at, Some(3600), now)
            .expect("expected auto stop");
        assert!(reason.contains("max_ticks"));
    }

    #[test]
    fn auto_stop_reason_hits_runtime_limit() {
        let started_at = chrono::Utc::now() - chrono::Duration::seconds(11);
        let now = chrono::Utc::now();
        let reason = compute_auto_stop_reason(2, None, started_at, Some(10), now)
            .expect("expected runtime auto stop");
        assert!(reason.contains("max_runtime_sec"));
    }

    #[test]
    fn parse_task_checklist_entry_handles_checkbox_lines() {
        let open = parse_task_checklist_entry(10, "- [ ] A1-5: define retry policy").unwrap();
        assert!(!open.done);
        assert_eq!(open.id, "A1-5");

        let done = parse_task_checklist_entry(11, "  - [x] A2-1: idempotent ack").unwrap();
        assert!(done.done);
        assert_eq!(done.id, "A2-1");

        assert!(parse_task_checklist_entry(12, "- [ ] malformed without colon").is_none());
    }

    #[test]
    fn extract_pr_url_parses_task_contract_line() {
        assert_eq!(
            extract_pr_url("TASK_WAITING_MERGE PR_URL=https://github.com/n01e0/claw-loop/pull/42"),
            Some("https://github.com/n01e0/claw-loop/pull/42".to_string())
        );
        assert_eq!(
            extract_pr_url("TASK_DONE PR_URL=<https://github.com/n01e0/claw-loop/pull/99>"),
            Some("https://github.com/n01e0/claw-loop/pull/99".to_string())
        );
        assert_eq!(extract_pr_url("TASK_BLOCKED: no PR"), None);
    }

    #[test]
    fn parse_waiting_dependency_contract_accepts_task_or_pr_dependencies() {
        let context = parse_waiting_dependency_contract(
            "TASK_WAITING_DEPENDENCY TASK_ID=D2 DEPENDS_ON_TASK=D1 DEPENDS_ON_PR_URL=https://github.com/n01e0/claw-loop/pull/42",
            None,
        )
        .expect("dependency wait should parse");

        assert_eq!(context.task_id, "D2");
        assert_eq!(context.depends_on_task.as_deref(), Some("D1"));
        assert_eq!(
            context.depends_on_pr_url.as_deref(),
            Some("https://github.com/n01e0/claw-loop/pull/42")
        );
    }

    #[test]
    fn parse_waiting_dependency_contract_accepts_fallback_task_id() {
        let context = parse_waiting_dependency_contract(
            "TASK_WAITING_DEPENDENCY DEPENDS_ON_TASK=D1",
            Some("D2"),
        )
        .expect("fallback task id should be accepted");

        assert_eq!(context.task_id, "D2");
        assert_eq!(context.depends_on_task.as_deref(), Some("D1"));
    }

    #[test]
    fn parse_waiting_dependency_contract_requires_dependency_target() {
        assert!(
            parse_waiting_dependency_contract("TASK_WAITING_DEPENDENCY TASK_ID=D2", None).is_err()
        );
    }

    #[test]
    fn parse_waiting_dependency_contract_requires_absolute_pr_url() {
        assert!(
            parse_waiting_dependency_contract(
                "TASK_WAITING_DEPENDENCY TASK_ID=D2 DEPENDS_ON_PR_URL=/pull/42",
                None
            )
            .is_err()
        );
    }

    #[test]
    fn format_waiting_dependency_notification_names_dependency_and_no_auto_recover() {
        let context = WaitingDependencyContext {
            task_id: "D3".to_string(),
            depends_on_task: Some("D2".to_string()),
            depends_on_pr_url: Some("https://github.com/n01e0/claw-loop/pull/121".to_string()),
            contract_line: "TASK_WAITING_DEPENDENCY TASK_ID=D3 DEPENDS_ON_TASK=D2 DEPENDS_ON_PR_URL=https://github.com/n01e0/claw-loop/pull/121".to_string(),
        };

        let message = format_waiting_dependency_notification("D3", &context);
        assert!(message.contains("task waiting dependency: D3"));
        assert!(message.contains("- 分類: dependency wait（generic blocked ではない）"));
        assert!(
            message
                .contains("- 今待っているもの: task D2 and PR https://github.com/n01e0/claw-loop/pull/121; standalone PR に押し込まず、前段 phase/stacked change の完了を待つ")
        );
        assert!(
            message.contains("- 次に進む条件: 依存 task / PR が片付いたら daemon が自動で再開する")
        );
        assert!(message.contains("- 人手介入: 原則不要。依存先が長時間進まない・依存先指定が誤っている・依存先を特定できない場合のみ必要"));
        assert!(message.contains(
            "- Auto-recover: idle（dependency が解消するまで recovery task は積まない）"
        ));
    }

    #[test]
    fn ensure_waiting_dependency_progress_resolves_when_dependency_task_is_done() {
        let context = WaitingDependencyContext {
            task_id: "D4".to_string(),
            depends_on_task: Some("D3".to_string()),
            depends_on_pr_url: None,
            contract_line: "TASK_WAITING_DEPENDENCY TASK_ID=D4 DEPENDS_ON_TASK=D3".to_string(),
        };
        let entries = vec![TaskChecklistEntry {
            line_no: 1,
            done: true,
            id: "D3".to_string(),
            text: "upstream task".to_string(),
        }];
        let runner_state = RunnerState::default();

        let progress =
            ensure_waiting_dependency_progress_with(&context, &entries, &runner_state, |_| {
                Ok(false)
            })
            .expect("dependency progress should resolve from completed upstream task");

        match progress {
            WaitingDependencyProgress::Resolved {
                context,
                resolution,
            } => {
                assert_eq!(context.depends_on_task.as_deref(), Some("D3"));
                assert!(resolution.contains("dependency task D3 is done"));
            }
            other => panic!("expected resolved dependency progress, got {other:?}"),
        }
    }

    #[test]
    fn ensure_waiting_dependency_progress_resolves_when_same_run_task_pr_merges() {
        let context = WaitingDependencyContext {
            task_id: "D4".to_string(),
            depends_on_task: Some("D3".to_string()),
            depends_on_pr_url: None,
            contract_line: "TASK_WAITING_DEPENDENCY TASK_ID=D4 DEPENDS_ON_TASK=D3".to_string(),
        };
        let entries = vec![TaskChecklistEntry {
            line_no: 1,
            done: false,
            id: "D3".to_string(),
            text: "upstream task".to_string(),
        }];
        let mut runner_state = RunnerState::default();
        runner_state.tracked_task_pr_urls.insert(
            "D3".to_string(),
            "https://github.com/n01e0/claw-loop/pull/123".to_string(),
        );

        let progress =
            ensure_waiting_dependency_progress_with(&context, &entries, &runner_state, |pr_url| {
                Ok(pr_url.ends_with("/123"))
            })
            .expect("dependency progress should resolve");

        match progress {
            WaitingDependencyProgress::Resolved {
                context,
                resolution,
            } => {
                assert_eq!(context.depends_on_task.as_deref(), Some("D3"));
                assert_eq!(
                    context.depends_on_pr_url.as_deref(),
                    Some("https://github.com/n01e0/claw-loop/pull/123")
                );
                assert!(resolution.contains("dependency PR merged for task D3"));
            }
            other => panic!("expected resolved dependency progress, got {other:?}"),
        }
    }

    #[test]
    fn ensure_waiting_dependency_progress_waits_when_dependency_still_open() {
        let context = WaitingDependencyContext {
            task_id: "D4".to_string(),
            depends_on_task: Some("D3".to_string()),
            depends_on_pr_url: None,
            contract_line: "TASK_WAITING_DEPENDENCY TASK_ID=D4 DEPENDS_ON_TASK=D3".to_string(),
        };
        let entries = vec![TaskChecklistEntry {
            line_no: 1,
            done: false,
            id: "D3".to_string(),
            text: "upstream task".to_string(),
        }];
        let runner_state = RunnerState::default();

        let progress =
            ensure_waiting_dependency_progress_with(&context, &entries, &runner_state, |_| {
                Ok(false)
            })
            .expect("dependency progress should stay waiting");

        match progress {
            WaitingDependencyProgress::Waiting(context) => {
                assert_eq!(context.depends_on_task.as_deref(), Some("D3"));
                assert_eq!(context.depends_on_pr_url, None);
            }
            other => panic!("expected waiting dependency progress, got {other:?}"),
        }
    }

    #[test]
    fn parse_waiting_contract_recognizes_dependency_wait_as_waiting() {
        let waiting = parse_waiting_contract(
            "TASK_WAITING_DEPENDENCY TASK_ID=D2 DEPENDS_ON_TASK=D1",
            Some(10),
            None,
        )
        .expect("waiting contract should parse");

        match waiting {
            Some(WaitingContract::WaitingDependency(context)) => {
                assert_eq!(context.task_id, "D2");
                assert_eq!(context.depends_on_task.as_deref(), Some("D1"));
                assert_eq!(context.depends_on_pr_url, None);
            }
            other => panic!("expected dependency waiting contract, got {other:?}"),
        }
    }

    #[test]
    fn validate_task_done_contract_requires_task_done_prefix_and_pr_url() {
        assert!(validate_task_done_contract_with("TASK_BLOCKED: reason", |_| Ok(true)).is_err());
        assert!(validate_task_done_contract_with("TASK_DONE", |_| Ok(true)).is_err());
    }

    #[test]
    fn validate_task_done_contract_requires_merged_pr() {
        let line = "TASK_DONE PR_URL=https://github.com/n01e0/claw-loop/pull/123";
        assert!(validate_task_done_contract_with(line, |_| Ok(false)).is_err());

        let ok = validate_task_done_contract_with(line, |_| Ok(true))
            .expect("merged PR should satisfy completion guard");
        assert_eq!(ok, "https://github.com/n01e0/claw-loop/pull/123");
    }

    #[test]
    fn completion_guard_waiting_fallback_line_emits_waiting_merge_on_timeout() {
        let line = "TASK_DONE PR_URL=https://github.com/n01e0/claw-loop/pull/777";
        let err = "merge check timed out for https://github.com/n01e0/claw-loop/pull/777: timeout";
        let waiting = completion_guard_waiting_fallback_line(line, err);
        assert_eq!(
            waiting,
            Some(
                "TASK_WAITING_MERGE PR_URL=https://github.com/n01e0/claw-loop/pull/777".to_string()
            )
        );
    }

    #[test]
    fn completion_guard_waiting_fallback_line_ignores_non_timeout_errors() {
        let line = "TASK_DONE PR_URL=https://github.com/n01e0/claw-loop/pull/778";
        let err = "PR is not merged yet: https://github.com/n01e0/claw-loop/pull/778";
        assert_eq!(completion_guard_waiting_fallback_line(line, err), None);
    }

    #[test]
    fn notification_delivery_mode_sends_only_important_events_as_new_posts() {
        assert_eq!(
            notification_delivery_mode("run_started"),
            NotificationDeliveryMode::EditStatus
        );
        assert_eq!(
            notification_delivery_mode("task_waiting_stuck"),
            NotificationDeliveryMode::EditStatus
        );
        assert_eq!(
            notification_delivery_mode("pr_tracking_started"),
            NotificationDeliveryMode::EditStatus
        );
        assert_eq!(
            notification_delivery_mode("pr_merged"),
            NotificationDeliveryMode::EditStatus
        );
        assert_eq!(
            notification_delivery_mode("task_started"),
            NotificationDeliveryMode::EditStatus
        );
        assert_eq!(
            notification_delivery_mode("task_waiting_merge"),
            NotificationDeliveryMode::EditStatus
        );
        assert_eq!(
            notification_delivery_mode("task_waiting_dependency"),
            NotificationDeliveryMode::EditStatus
        );
        assert_eq!(
            notification_delivery_mode("task_progress"),
            NotificationDeliveryMode::EditStatus
        );

        assert_eq!(
            notification_delivery_mode("blocked"),
            NotificationDeliveryMode::Send
        );
        assert_eq!(
            notification_delivery_mode("done"),
            NotificationDeliveryMode::Send
        );
        assert_eq!(
            notification_delivery_mode("stopped"),
            NotificationDeliveryMode::Send
        );
        assert_eq!(
            notification_delivery_mode("auto_stopped"),
            NotificationDeliveryMode::Send
        );
        assert_eq!(
            notification_delivery_mode("task_blocked"),
            NotificationDeliveryMode::Send
        );
        assert_eq!(
            notification_delivery_mode("task_done"),
            NotificationDeliveryMode::Send
        );
        assert_eq!(
            notification_delivery_mode("orphan_blocked"),
            NotificationDeliveryMode::Send
        );
        assert_eq!(
            notification_delivery_mode("all_tasks_completed"),
            NotificationDeliveryMode::Send
        );
        assert_eq!(
            notification_delivery_mode("pr_closed"),
            NotificationDeliveryMode::Send
        );
    }

    #[test]
    fn parse_openclaw_message_id_reads_cli_json_payload() {
        let sample = serde_json::json!({
            "action": "send",
            "channel": "discord",
            "dryRun": false,
            "handledBy": "plugin",
            "payload": {
                "result": {
                    "messageId": "1234567890"
                }
            }
        });
        let encoded = serde_json::to_vec(&sample).expect("encode sample json");
        assert_eq!(
            parse_openclaw_message_id(&encoded),
            Some("1234567890".to_string())
        );
    }

    #[test]
    fn waiting_stuck_tracker_notifies_on_threshold_and_interval() {
        let mut runner = RunnerState::default();
        let threshold = 3;

        assert_eq!(
            update_waiting_stuck_tracker(
                &mut runner,
                &LoopStatus::Waiting,
                "task waiting_merge: S1",
                "TASK_WAITING_MERGE PR_URL=https://example.invalid/pr/1",
                threshold,
            ),
            None
        );
        assert_eq!(runner.waiting_unchanged_ticks, 1);

        assert_eq!(
            update_waiting_stuck_tracker(
                &mut runner,
                &LoopStatus::Waiting,
                "task waiting_merge: S1",
                "TASK_WAITING_MERGE PR_URL=https://example.invalid/pr/1",
                threshold,
            ),
            None
        );
        assert_eq!(runner.waiting_unchanged_ticks, 2);

        assert_eq!(
            update_waiting_stuck_tracker(
                &mut runner,
                &LoopStatus::Waiting,
                "task waiting_merge: S1",
                "TASK_WAITING_MERGE PR_URL=https://example.invalid/pr/1",
                threshold,
            ),
            Some(3)
        );
        assert_eq!(runner.waiting_last_notified_ticks, 3);

        assert_eq!(
            update_waiting_stuck_tracker(
                &mut runner,
                &LoopStatus::Waiting,
                "task waiting_merge: S1",
                "TASK_WAITING_MERGE PR_URL=https://example.invalid/pr/1",
                threshold,
            ),
            None
        );
        assert_eq!(
            update_waiting_stuck_tracker(
                &mut runner,
                &LoopStatus::Waiting,
                "task waiting_merge: S1",
                "TASK_WAITING_MERGE PR_URL=https://example.invalid/pr/1",
                threshold,
            ),
            None
        );
        assert_eq!(
            update_waiting_stuck_tracker(
                &mut runner,
                &LoopStatus::Waiting,
                "task waiting_merge: S1",
                "TASK_WAITING_MERGE PR_URL=https://example.invalid/pr/1",
                threshold,
            ),
            Some(6)
        );
    }

    #[test]
    fn waiting_stuck_tracker_resets_on_non_waiting_status() {
        let mut runner = RunnerState::default();
        let _ = update_waiting_stuck_tracker(
            &mut runner,
            &LoopStatus::Waiting,
            "task waiting_merge: S2",
            "TASK_WAITING_MERGE PR_URL=https://example.invalid/pr/2",
            3,
        );
        let _ = update_waiting_stuck_tracker(
            &mut runner,
            &LoopStatus::Waiting,
            "task waiting_merge: S2",
            "TASK_WAITING_MERGE PR_URL=https://example.invalid/pr/2",
            3,
        );
        assert_eq!(runner.waiting_unchanged_ticks, 2);

        assert_eq!(
            update_waiting_stuck_tracker(&mut runner, &LoopStatus::Running, "", "", 3),
            None
        );
        assert_eq!(runner.waiting_unchanged_ticks, 0);
        assert!(runner.waiting_last_summary.is_none());
        assert!(runner.waiting_last_reason.is_none());
    }

    #[test]
    fn waiting_stuck_tracker_resets_when_waiting_reason_changes() {
        let mut runner = RunnerState::default();

        assert_eq!(
            update_waiting_stuck_tracker(
                &mut runner,
                &LoopStatus::Waiting,
                "task waiting_merge: S3",
                "TASK_WAITING_MERGE PR_URL=https://example.invalid/pr/3",
                3,
            ),
            None
        );
        assert_eq!(runner.waiting_unchanged_ticks, 1);

        assert_eq!(
            update_waiting_stuck_tracker(
                &mut runner,
                &LoopStatus::Waiting,
                "task waiting_merge: S3",
                "TASK_WAITING_MERGE PR_URL=https://example.invalid/pr/4",
                3,
            ),
            None
        );
        assert_eq!(runner.waiting_unchanged_ticks, 1);
        assert_eq!(
            runner.waiting_last_reason.as_deref(),
            Some("TASK_WAITING_MERGE PR_URL=https://example.invalid/pr/4")
        );
    }

    #[test]
    fn waiting_stuck_is_not_suppressed_without_pause_reason() {
        let runner = RunnerState {
            paused: true,
            pause_reason: None,
            ..RunnerState::default()
        };
        assert!(!should_suppress_waiting_stuck(&runner));
    }

    #[test]
    fn waiting_stuck_is_suppressed_when_paused_all_tasks_completed() {
        let runner = RunnerState {
            paused: true,
            pause_reason: Some("all tasklist items completed".into()),
            ..RunnerState::default()
        };
        assert!(should_suppress_waiting_stuck(&runner));
    }

    #[test]
    fn waiting_stuck_is_not_suppressed_for_other_pause_reason() {
        let runner = RunnerState {
            paused: true,
            pause_reason: Some("max_task_loops reached (10/10)".into()),
            ..RunnerState::default()
        };
        assert!(!should_suppress_waiting_stuck(&runner));
    }

    #[test]
    fn ensure_task_agent_exists_auto_creates_missing_agent() {
        let dir = TestRunDir::new("ensure-task-agent-create");
        let (mock_openclaw, state_path) = write_mock_openclaw_script(&dir);

        let created = ensure_task_agent_exists_with(
            mock_openclaw.to_str().expect("mock openclaw path"),
            "loop-rta-rbac",
            Path::new("/tmp/workspace"),
            5,
        )
        .expect("create missing task agent");
        assert!(created);

        let agents: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&state_path).expect("read mock agent state"))
                .expect("parse mock agent state");
        assert!(agents.as_array().expect("agent state array").iter().any(
            |entry| entry.get("id") == Some(&serde_json::Value::String("loop-rta-rbac".into()))
        ));

        let created_again = ensure_task_agent_exists_with(
            mock_openclaw.to_str().expect("mock openclaw path"),
            "loop-rta-rbac",
            Path::new("/tmp/workspace"),
            5,
        )
        .expect("skip existing task agent");
        assert!(!created_again);
    }

    #[test]
    fn ensure_task_agent_exists_surfaces_add_failure_before_start() {
        let dir = TestRunDir::new("ensure-task-agent-fail");
        let (mock_openclaw, _state_path) = write_mock_openclaw_script(&dir);
        fs::write(dir.path.join("fail-add"), "1").expect("write fail-add marker");

        let result = ensure_task_agent_exists_with(
            mock_openclaw.to_str().expect("mock openclaw path"),
            "loop-rta-rbac",
            Path::new("/tmp/workspace"),
            5,
        );

        let err = result.expect_err("missing task agent should fail when add fails");
        assert!(
            err.to_string()
                .contains("openclaw agents add loop-rta-rbac failed")
        );
    }

    #[test]
    fn retryable_waiting_merge_error_matches_timeout_failures() {
        assert!(retryable_waiting_merge_error(
            "gh pr view failed: status=Some(124) stderr="
        ));
        assert!(retryable_waiting_merge_error("request timed out"));
        assert!(!retryable_waiting_merge_error("permission denied"));
    }

    #[test]
    fn ensure_waiting_merge_progress_returns_retryable_on_timeout() {
        let result = ensure_waiting_merge_progress_with(
            "https://github.com/demo/repo/pull/789",
            |_, _| {
                Err(anyhow::anyhow!(
                    "gh pr view failed: status=Some(124) stderr="
                ))
            },
            |_, _, _| Ok(()),
            |_, _, _| Ok(()),
            |_| Ok(false),
        )
        .expect("waiting merge progress");

        assert_eq!(
            result,
            WaitingMergeProgress::Retryable(
                "waiting_merge retryable error for PR_URL=https://github.com/demo/repo/pull/789 error=gh pr view failed: status=Some(124) stderr=".into()
            )
        );
    }

    #[test]
    fn waiting_merge_nonprogress_reason_detects_dirty_prs() {
        let view = GhPrView {
            state: "OPEN".into(),
            url: "https://github.com/demo/repo/pull/123".into(),
            merge_state_status: Some("DIRTY".into()),
            auto_merge_request: None,
            status_check_rollup: vec![],
        };
        assert_eq!(
            waiting_merge_nonprogress_reason(&view, "https://github.com/demo/repo/pull/123"),
            Some(
                "PR_URL=https://github.com/demo/repo/pull/123 merge state is DIRTY (merge conflict or unmergeable branch)".into()
            )
        );
    }

    #[test]
    fn ensure_waiting_merge_progress_blocks_dirty_prs() {
        let result = ensure_waiting_merge_progress_with(
            "https://github.com/demo/repo/pull/253",
            |_, _| {
                Ok(GhPrView {
                    state: "OPEN".into(),
                    url: "https://github.com/demo/repo/pull/253".into(),
                    merge_state_status: Some("DIRTY".into()),
                    auto_merge_request: Some(
                        serde_json::json!({"enabledAt": "2026-03-14T00:00:00Z"}),
                    ),
                    status_check_rollup: vec![],
                })
            },
            |_, _, _| Ok(()),
            |_, _, _| Ok(()),
            |_| Ok(false),
        )
        .expect("waiting merge progress");

        assert_eq!(
            result,
            WaitingMergeProgress::Blocked(
                "PR_URL=https://github.com/demo/repo/pull/253 merge state is DIRTY (merge conflict or unmergeable branch)".into()
            )
        );
    }

    #[test]
    fn tasklist_approval_violation_reason_ignores_approval_marker_drift() {
        let run = TestRunDir::new("approval-marker-drift");
        let task_file = run.path.join("tasklist.md");
        fs::write(&task_file, "# Title\n\n- [ ] A1: alpha\n").expect("write task file");

        let status = write_task_approval(&task_file, "n01e0").expect("approve task file");
        let mut manifest = test_manifest(&run.path, Uuid::new_v4(), false);
        manifest.task_file = task_file.clone();
        manifest.require_task_approval = true;
        manifest.approved_tasklist_hash = status.approved_tasklist_hash.clone();
        manifest.approved_by = status.approved_by.clone().expect("approved by");
        manifest.approved_at = status.approved_at.expect("approved at");

        let content = fs::read_to_string(&task_file).expect("read task file");
        let updated = content.replacen(
            &format!("Approved-At: {}", manifest.approved_at.to_rfc3339()),
            "Approved-At: 2026-03-18T11:38:57.199213524+00:00",
            1,
        );
        fs::write(&task_file, updated).expect("rewrite approval marker only");

        let reason = tasklist_approval_violation_reason(&task_file, &manifest);
        assert!(reason.is_empty(), "unexpected reason: {reason}");
    }

    #[test]
    fn tasklist_approval_violation_reason_skips_checks_when_approval_is_disabled() {
        let run = TestRunDir::new("approval-disabled");
        let task_file = run.path.join("tasklist.md");
        fs::write(
            &task_file,
            "# Title

- [ ] A1: alpha
",
        )
        .expect("write task file");

        let mut manifest = test_manifest(&run.path, Uuid::new_v4(), false);
        manifest.task_file = task_file.clone();
        manifest.require_task_approval = false;
        manifest.approved_tasklist_hash = "mismatch".into();

        let reason = tasklist_approval_violation_reason(&task_file, &manifest);
        assert!(reason.is_empty(), "unexpected reason: {reason}");
    }

    #[test]
    fn tasklist_approval_violation_reason_still_blocks_plan_hash_drift() {
        let run = TestRunDir::new("approval-plan-drift");
        let task_file = run.path.join("tasklist.md");
        fs::write(&task_file, "# Title\n\n- [ ] A1: alpha\n").expect("write task file");

        let status = write_task_approval(&task_file, "n01e0").expect("approve task file");
        let mut manifest = test_manifest(&run.path, Uuid::new_v4(), false);
        manifest.task_file = task_file.clone();
        manifest.require_task_approval = true;
        manifest.approved_tasklist_hash = status.approved_tasklist_hash.clone();
        manifest.approved_by = status.approved_by.clone().expect("approved by");
        manifest.approved_at = status.approved_at.expect("approved at");

        let content = fs::read_to_string(&task_file).expect("read task file");
        let updated = content.replace("- [ ] A1: alpha", "- [ ] A1: beta");
        fs::write(&task_file, updated).expect("rewrite task entry");

        let reason = tasklist_approval_violation_reason(&task_file, &manifest);
        assert!(
            reason.contains("approved task hash changed"),
            "unexpected reason: {reason}"
        );
    }

    #[test]
    fn auto_merge_unavailable_error_matches_repo_setting_failures() {
        assert!(auto_merge_unavailable_error(
            "GraphQL: Pull request Auto merge is not allowed for this repository"
        ));
        assert!(auto_merge_unavailable_error(
            "repository has disabled auto-merge for this branch"
        ));
        assert!(!auto_merge_unavailable_error("permission denied"));
    }

    #[test]
    fn ensure_waiting_merge_progress_falls_back_to_manual_merge_after_green_ci() {
        let merged = std::cell::Cell::new(false);
        let result = ensure_waiting_merge_progress_with(
            "https://github.com/demo/repo/pull/123",
            |_, _| {
                Ok(GhPrView {
                    state: if merged.get() { "MERGED" } else { "OPEN" }.into(),
                    url: "https://github.com/demo/repo/pull/123".into(),
                    merge_state_status: Some("CLEAN".into()),
                    auto_merge_request: None,
                    status_check_rollup: vec![GhStatusCheck {
                        name: Some("ci".into()),
                        status: Some("COMPLETED".into()),
                        conclusion: Some("SUCCESS".into()),
                    }],
                })
            },
            |_, _, _| {
                Err(anyhow::anyhow!(
                    "gh pr merge --auto failed: GraphQL: Pull request Auto merge is not allowed for this repository"
                ))
            },
            |_, _, _| {
                merged.set(true);
                Ok(())
            },
            |_| Ok(merged.get()),
        )
        .expect("waiting merge progress");

        assert_eq!(result, WaitingMergeProgress::Merged);
    }

    #[test]
    fn ensure_waiting_merge_progress_waits_for_ci_before_manual_merge() {
        let merge_attempted = std::cell::Cell::new(false);
        let result = ensure_waiting_merge_progress_with(
            "https://github.com/demo/repo/pull/456",
            |_, _| {
                Ok(GhPrView {
                    state: "OPEN".into(),
                    url: "https://github.com/demo/repo/pull/456".into(),
                    merge_state_status: Some("BLOCKED".into()),
                    auto_merge_request: None,
                    status_check_rollup: vec![GhStatusCheck {
                        name: Some("ci".into()),
                        status: Some("IN_PROGRESS".into()),
                        conclusion: None,
                    }],
                })
            },
            |_, _, _| {
                Err(anyhow::anyhow!(
                    "gh pr merge --auto failed: GraphQL: Pull request Auto merge is not allowed for this repository"
                ))
            },
            |_, _, _| {
                merge_attempted.set(true);
                Ok(())
            },
            |_| Ok(false),
        )
        .expect("waiting merge progress");

        assert_eq!(result, WaitingMergeProgress::Waiting);
        assert!(!merge_attempted.get());
    }
}
