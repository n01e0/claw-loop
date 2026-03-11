use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
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
#[cfg(test)]
use tasklist::parse_task_checklist_entry;
use tasklist::{
    TaskChecklistEntry, append_recovery_task_for_blocked, load_task_checklist,
    task_checklist_done_count, update_task_check,
};
use uuid::Uuid;

mod notify_policy;
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
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        auto_check_on_success: bool,
        #[arg(long, default_value_t = false)]
        auto_recover_blocked: bool,
        #[arg(long, default_value_t = 3)]
        auto_recover_blocked_max_attempts: u64,
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
    #[serde(default = "default_auto_check_on_success")]
    auto_check_on_success: bool,
    #[serde(default)]
    auto_recover_blocked: bool,
    #[serde(default = "default_auto_recover_blocked_max_attempts")]
    auto_recover_blocked_max_attempts: u64,
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
    Blocked,
    Done,
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
    current_task_blocked_reason: Option<String>,
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
    last_task_pr_url: Option<String>,
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

#[derive(Debug, Deserialize)]
struct GhPrView {
    state: String,
    url: String,
    #[serde(rename = "mergeStateStatus")]
    merge_state_status: Option<String>,
    #[serde(rename = "autoMergeRequest")]
    auto_merge_request: Option<serde_json::Value>,
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
    auto_check_on_success: bool,
    auto_recover_blocked: bool,
    auto_recover_blocked_max_attempts: u64,
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

fn stuck_wait_ticks_threshold() -> u64 {
    std::env::var("CLAW_LOOPD_STUCK_WAIT_TICKS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(30)
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
    write_json(&runner_state_path(run_dir), runner)
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
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    writeln!(f, "{}", serde_json::to_string(value)?)?;
    Ok(())
}

fn rewrite_jsonl<T: Serialize>(path: &Path, values: &[T]) -> Result<()> {
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
        writeln!(f, "{}", serde_json::to_string(v)?)?;
    }
    Ok(())
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
    Ok(n.event_id)
}

fn queue_notification(
    run_dir: &Path,
    manifest: &Manifest,
    kind: impl Into<String>,
    message: impl Into<String>,
) -> Result<Uuid> {
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
    cmd: &'a str,
    auto_check_on_success: bool,
    dry_run: bool,
    cwd: Option<&'a Path>,
    run_id: Option<Uuid>,
    thread_id: Option<&'a str>,
    channel: Option<&'a str>,
    task_agent_id: Option<&'a str>,
}

fn clip_text(input: &str, max_chars: usize) -> String {
    if input.chars().count() <= max_chars {
        return input.to_string();
    }
    let clipped: String = input.chars().take(max_chars).collect();
    format!("{clipped}…")
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

fn blocked_recovery_hint(reason: &str) -> &'static str {
    let normalized = reason.to_ascii_lowercase();
    if normalized.contains("session file locked") || normalized.contains("task_waiting_agent_lock")
    {
        "wait for the active agent session lock to clear, then retry with a dedicated --task-agent-id to avoid contention"
    } else if normalized.contains("unexpected eof")
        || normalized.contains("syntax error")
        || normalized.contains("command not found")
    {
        "fix the runner/script syntax or missing command in the repository, then rerun the blocked task"
    } else if normalized.contains("timeout") || normalized.contains("timed out") {
        "retry after the transient timeout and consider increasing command/agent timeout if it is consistently slow"
    } else if normalized.contains("permission denied")
        || normalized.contains("401")
        || normalized.contains("403")
        || normalized.contains("forbidden")
    {
        "fix the required auth/permission for this action (repo/channel/token), then rerun the task"
    } else if normalized.contains("without pr_url") || normalized.contains("pr_url") {
        "ensure the runner returns TASK_DONE PR_URL=<url> and that the PR reaches merged state"
    } else {
        "inspect runner stderr, fix the blocker in code/config, then rerun the task"
    }
}

fn format_task_blocked_notification(
    manifest: &Manifest,
    task_label: &str,
    blocked_reason: &str,
    pr_url: Option<&str>,
) -> String {
    let mention = completion_mention_prefix(manifest);
    let reason = if blocked_reason.trim().is_empty() {
        "unknown blocker".to_string()
    } else {
        clip_text(blocked_reason.trim(), 240)
    };
    let recovery = blocked_recovery_hint(&reason);
    let next = if manifest.auto_recover_blocked {
        "next: auto-recovery is enabled; daemon will queue a recovery task automatically (guard limits apply)"
    } else {
        "next: auto-recovery is disabled; fix manually and rerun (or start with --auto-recover-blocked)"
    };
    let pr_suffix = pr_url
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(|v| format!(" PR_URL={v}"))
        .unwrap_or_default();

    format!(
        "{mention}task blocked: {task_label}{pr_suffix}\nreason: {reason}\nrecovery: {recovery}\n{next}"
    )
}

fn format_orphan_blocked_notification(manifest: &Manifest, daemon_pid: u32) -> String {
    let mention = completion_mention_prefix(manifest);
    format!(
        "{mention}run blocked: daemon pid {daemon_pid} missing after lease expiry\nreason: daemon process is gone while lease expired\nrecovery: restart the run with the latest binary and verify stale pid/lock state before resuming"
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
) -> Result<()> {
    let mention = completion_mention_prefix(manifest);
    let last_task = runner_state
        .last_task_id
        .clone()
        .unwrap_or_else(|| "unknown".to_string());
    let summary = format!(
        "{mention}all tasks completed (run_id={}, loops_started={}, done={}, last_task={}); waiting for instruction",
        manifest.run_id, runner_state.task_loops_started, task_done_now, last_task,
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

fn extract_pr_url(line: &str) -> Option<String> {
    line.split_whitespace().find_map(|token| {
        let raw = token.strip_prefix("PR_URL=")?;
        let trimmed = raw.trim_matches(|c: char| {
            c.is_whitespace()
                || matches!(
                    c,
                    ',' | ';' | '(' | ')' | '[' | ']' | '<' | '>' | '"' | '\''
                )
        });
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
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

    if !pr_url.starts_with("http://") && !pr_url.starts_with("https://") {
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

fn run_task_once(opts: TaskRunOptions<'_>) -> Result<TaskRunOutcome> {
    let (_, _, entries) = load_task_checklist(opts.task_file)?;
    let next = entries.iter().find(|entry| !entry.done).cloned();

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
        let first_stdout_line = outcome
            .stdout
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .to_string();

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
        "state,url,mergeStateStatus,autoMergeRequest".into(),
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

fn gh_pr_arm_auto_merge(gh_repo: &str, pr: u64, merge_method: &str) -> Result<()> {
    let method_flag = match merge_method {
        "merge" => "--merge",
        "squash" => "--squash",
        "rebase" => "--rebase",
        other => bail!("invalid merge method: {other}"),
    };

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

    let task_file = opts.task_file;
    let task_file_abs = resolve_task_file_path(&opts.repo, &task_file);
    let task_done_baseline = task_checklist_done_count(&task_file_abs)?;

    let now = Utc::now();
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
        auto_check_on_success: opts.auto_check_on_success,
        auto_recover_blocked: opts.auto_recover_blocked,
        auto_recover_blocked_max_attempts: opts.auto_recover_blocked_max_attempts,
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

    let mut manifest: Manifest = read_json(&dir.join("manifest.json"))?;
    let _daemon_lock = acquire_daemon_lock(&dir, run_id)?;

    if manifest.daemon_pid != process::id() {
        let old_pid = manifest.daemon_pid;
        manifest.daemon_pid = process::id();
        write_json(&dir.join("manifest.json"), &manifest)?;
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

        if let Some(cmd) = manifest.task_runner_cmd.as_deref() {
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
                            runner_state.current_task_blocked_reason = None;
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
                                    "task {} marked done in checklist; done={}{}",
                                    entry.id, task_done_now, pr_suffix
                                ),
                            )?;
                        }
                        Some(entry) => {
                            state.status = LoopStatus::Waiting;
                            match runner_state.current_task_state {
                                Some(RunnerTaskState::WaitingMerge) => {
                                    state.summary = format!("task waiting_merge: {}", entry.id);
                                    if state.waiting_reason.is_empty() {
                                        state.waiting_reason =
                                            format!("TASK_WAITING_MERGE ({})", entry.id);
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
                    let (_, _, entries) = load_task_checklist(&task_file_abs)?;
                    let next = entries.iter().find(|entry| !entry.done).cloned();

                    if runner_state.task_loops_started >= manifest.max_task_loops {
                        runner_state.paused = true;
                        runner_state.current_task_state = None;
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
                    } else if next.is_none() {
                        runner_state.paused = true;
                        runner_state.current_task_state = None;
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
                        let queued_task = next.clone().expect("checked next.is_some");
                        runner_state.current_task_id = Some(queued_task.id.clone());
                        runner_state.current_task_text = Some(queued_task.text.clone());
                        runner_state.current_task_line = Some(queued_task.line_no);
                        runner_state.current_task_started_at = Some(now);
                        runner_state.current_task_state = Some(RunnerTaskState::Queued);
                        runner_state.current_task_blocked_reason = None;
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

                        let runner = run_task_once(TaskRunOptions {
                            task_file: &task_file_abs,
                            cmd,
                            auto_check_on_success: manifest.auto_check_on_success,
                            dry_run: false,
                            cwd: Some(&manifest.repo_path),
                            run_id: Some(run_id),
                            thread_id: Some(&manifest.thread_id),
                            channel: Some(&manifest.channel),
                            task_agent_id: manifest.task_agent_id.as_deref(),
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
                                "check_result": runner.check_result,
                                "stdout": clip_text(&runner.stdout, 1000),
                                "stderr": clip_text(&runner.stderr, 1000),
                            }),
                        )?;

                        let first_stdout_line = runner
                            .stdout
                            .lines()
                            .next()
                            .unwrap_or("")
                            .trim()
                            .to_string();
                        let first_line_pr_url = extract_pr_url(&first_stdout_line);
                        let task_label = runner
                            .task
                            .as_ref()
                            .map(|t| t.id.clone())
                            .unwrap_or_else(|| "unknown".to_string());

                        if runner.exit_code == Some(10)
                            || first_stdout_line.starts_with("TASK_WAITING")
                        {
                            state.version += 1;
                            state.status = LoopStatus::Waiting;
                            state.summary = format!("task waiting_merge: {task_label}");
                            state.waiting_reason = if first_stdout_line.is_empty() {
                                format!("task waiting_merge: {task_label}")
                            } else {
                                clip_text(&first_stdout_line, 200)
                            };
                            state.updated_at = now;

                            runner_state.current_task_state = Some(RunnerTaskState::WaitingMerge);
                            runner_state.current_task_blocked_reason = None;
                            runner_state.current_task_pr_url = first_line_pr_url.clone();
                            write_runner_state(&dir, &runner_state)?;
                            write_json(&dir.join("state.json"), &state)?;

                            let pr_suffix = first_line_pr_url
                                .clone()
                                .map(|u| format!(" PR_URL={u}"))
                                .unwrap_or_default();
                            queue_notification(
                                &dir,
                                &manifest,
                                "task_waiting_merge",
                                format!("task waiting merge: {}{}", task_label, pr_suffix),
                            )?;
                        } else if !runner.success {
                            state.version += 1;
                            state.status = LoopStatus::Blocked;
                            state.summary = format!("task runner failed: {}", task_label);
                            state.waiting_reason = format!(
                                "runner exit={:?}: {}",
                                runner.exit_code,
                                clip_text(&runner.stderr, 200)
                            );
                            state.updated_at = now;

                            runner_state.current_task_state = Some(RunnerTaskState::Blocked);
                            runner_state.current_task_blocked_reason =
                                Some(state.waiting_reason.clone());
                            runner_state.current_task_pr_url = first_line_pr_url.clone();
                            runner_state.last_task_id = Some(task_label.clone());
                            runner_state.last_task_state = Some(RunnerTaskState::Blocked);
                            runner_state.last_task_at = Some(now);
                            runner_state.last_task_reason = Some(state.waiting_reason.clone());
                            runner_state.last_task_pr_url = first_line_pr_url.clone();
                            write_runner_state(&dir, &runner_state)?;

                            let blocked_message = format_task_blocked_notification(
                                &manifest,
                                &task_label,
                                &state.waiting_reason,
                                first_line_pr_url.as_deref(),
                            );
                            queue_notification(&dir, &manifest, "task_blocked", blocked_message)?;

                            if manifest.auto_recover_blocked {
                                if let Some(blocked_task) = runner.task.as_ref() {
                                    let reason_key = normalize_blocked_reason_for_recovery(
                                        &state.waiting_reason,
                                    );
                                    let guard_reason = auto_recover_guard_reason(
                                        &blocked_task.id,
                                        &reason_key,
                                        &runner_state,
                                        manifest.auto_recover_blocked_max_attempts,
                                    );

                                    if let Some(reason) = guard_reason {
                                        runner_state.paused = true;
                                        runner_state.pause_reason = Some(reason.clone());
                                        runner_state.auto_recover_last_reason =
                                            Some(reason_key.clone());
                                        runner_state.auto_recover_same_reason_count = runner_state
                                            .auto_recover_same_reason_count
                                            .saturating_add(1);
                                        runner_state.auto_recover_last_at = Some(now);
                                        write_runner_state(&dir, &runner_state)?;

                                        state.status = LoopStatus::Stopped;
                                        state.summary = "auto-recovery halted".to_string();
                                        state.waiting_reason = reason.clone();
                                        state.updated_at = now;
                                        state.version += 1;

                                        append_event(
                                            &dir,
                                            "task_blocked_auto_recover_guard_hit",
                                            serde_json::json!({
                                                "blocked_task_id": blocked_task.id,
                                                "blocked_reason_key": reason_key,
                                                "guard_reason": reason,
                                                "attempts": runner_state.auto_recover_attempts,
                                                "max_attempts": manifest.auto_recover_blocked_max_attempts,
                                            }),
                                        )?;

                                        write_json(&dir.join("state.json"), &state)?;
                                        let _ = flush_notifications(&dir, &manifest)?;
                                        break;
                                    }

                                    let recovery_task = append_recovery_task_for_blocked(
                                        &task_file_abs,
                                        &blocked_task.id,
                                        &state.waiting_reason,
                                    )?;
                                    let _ =
                                        update_task_check(&task_file_abs, &blocked_task.id, true)?;
                                    task_done_now = task_checklist_done_count(&task_file_abs)?;
                                    task_loops_completed =
                                        task_done_now.saturating_sub(manifest.task_done_baseline);

                                    runner_state.auto_recover_attempts =
                                        runner_state.auto_recover_attempts.saturating_add(1);
                                    if runner_state.auto_recover_last_reason.as_deref()
                                        == Some(reason_key.as_str())
                                    {
                                        runner_state.auto_recover_same_reason_count = runner_state
                                            .auto_recover_same_reason_count
                                            .saturating_add(1);
                                    } else {
                                        runner_state.auto_recover_same_reason_count = 1;
                                    }
                                    runner_state.auto_recover_last_reason = Some(reason_key);
                                    runner_state.auto_recover_last_task_id =
                                        Some(recovery_task.id.clone());
                                    runner_state.auto_recover_last_at = Some(now);

                                    runner_state.current_task_id = None;
                                    runner_state.current_task_text = None;
                                    runner_state.current_task_line = None;
                                    runner_state.current_task_started_at = None;
                                    runner_state.current_task_state = None;
                                    runner_state.current_task_blocked_reason = None;
                                    runner_state.current_task_pr_url = None;
                                    runner_state.paused = false;
                                    runner_state.pause_reason = None;

                                    state.version += 1;
                                    state.status = LoopStatus::Running;
                                    state.summary =
                                        format!("auto-recovery queued: {}", recovery_task.id);
                                    state.waiting_reason = format!(
                                        "auto-recovery generated from blocked task {}",
                                        blocked_task.id
                                    );
                                    state.updated_at = now;

                                    write_runner_state(&dir, &runner_state)?;
                                    write_json(&dir.join("state.json"), &state)?;

                                    append_event(
                                        &dir,
                                        "task_blocked_auto_recovered",
                                        serde_json::json!({
                                            "blocked_task_id": blocked_task.id,
                                            "blocked_reason": runner_state.last_task_reason.clone(),
                                            "recovery_task_id": recovery_task.id,
                                            "recovery_task_line": recovery_task.line_no,
                                            "auto_recover_attempts": runner_state.auto_recover_attempts,
                                            "auto_recover_same_reason_count": runner_state.auto_recover_same_reason_count,
                                        }),
                                    )?;

                                    queue_notification(
                                        &dir,
                                        &manifest,
                                        "task_progress",
                                        format!(
                                            "auto-recovery task queued: {} (from blocked task {})",
                                            recovery_task.id, blocked_task.id
                                        ),
                                    )?;

                                    continue;
                                }

                                append_event(
                                    &dir,
                                    "task_blocked_auto_recover_skipped",
                                    serde_json::json!({
                                        "reason": "runner.task is missing",
                                        "task_label": task_label,
                                    }),
                                )?;
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
                                runner_state.last_task_id = Some(task.id.clone());
                                runner_state.last_task_state = Some(RunnerTaskState::Done);
                                runner_state.last_task_at = Some(now);
                                runner_state.last_task_reason =
                                    Some("runner success + auto-check".into());
                                runner_state.last_task_pr_url = first_line_pr_url.clone();
                                runner_state.current_task_id = None;
                                runner_state.current_task_text = None;
                                runner_state.current_task_line = None;
                                runner_state.current_task_started_at = None;
                                runner_state.current_task_state = None;
                                runner_state.current_task_blocked_reason = None;
                                runner_state.current_task_pr_url = None;

                                task_done_now = task_checklist_done_count(&task_file_abs)?;
                                task_loops_completed =
                                    task_done_now.saturating_sub(manifest.task_done_baseline);

                                let pr_suffix = first_line_pr_url
                                    .clone()
                                    .map(|u| format!(" PR_URL={u}"))
                                    .unwrap_or_default();
                                queue_notification(
                                    &dir,
                                    &manifest,
                                    "task_done",
                                    format!(
                                        "task done: {} (done={}){}",
                                        task.id, task_done_now, pr_suffix
                                    ),
                                )?;
                            } else {
                                runner_state.current_task_id = Some(task.id.clone());
                                runner_state.current_task_text = Some(task.text.clone());
                                runner_state.current_task_line = Some(task.line_no);
                                runner_state.current_task_started_at = Some(now);
                                runner_state.current_task_state = Some(RunnerTaskState::Running);
                                runner_state.current_task_blocked_reason = None;
                                runner_state.current_task_pr_url = first_line_pr_url.clone();
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
        "blocked_reason": runner_state.current_task_blocked_reason.clone(),
        "pr_url": runner_state.current_task_pr_url.clone(),
    });
    let runner_last_view = serde_json::json!({
        "id": runner_state.last_task_id.clone(),
        "state": runner_state.last_task_state.clone(),
        "at": runner_state.last_task_at,
        "reason": runner_state.last_task_reason.clone(),
        "pr_url": runner_state.last_task_pr_url.clone(),
    });
    let runner_view = serde_json::json!({
        "mode": runner_mode,
        "task_runner_cmd": manifest.task_runner_cmd,
        "task_agent_id": manifest.task_agent_id,
        "auto_check_on_success": manifest.auto_check_on_success,
        "auto_recover_blocked": manifest.auto_recover_blocked,
        "auto_recover_blocked_max_attempts": manifest.auto_recover_blocked_max_attempts,
        "auto_recover_attempts": runner_state.auto_recover_attempts,
        "auto_recover_last_reason": runner_state.auto_recover_last_reason.clone(),
        "auto_recover_same_reason_count": runner_state.auto_recover_same_reason_count,
        "auto_recover_last_task_id": runner_state.auto_recover_last_task_id.clone(),
        "auto_recover_last_at": runner_state.auto_recover_last_at,
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
        "current_task_blocked_reason": runner_state.current_task_blocked_reason.clone(),
        "current_task_pr_url": runner_state.current_task_pr_url.clone(),
        "last_task_id": runner_state.last_task_id.clone(),
        "last_task_state": runner_state.last_task_state.clone(),
        "last_task_at": runner_state.last_task_at,
        "last_task_reason": runner_state.last_task_reason.clone(),
        "last_task_pr_url": last_task_pr_url.clone(),
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

    rows.sort_by(|a, b| b.0.cmp(&a.0));
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
    failed_for_hist.sort_by(|a, b| b.moved_at.cmp(&a.moved_at));
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
        cmd: &cmd,
        auto_check_on_success,
        dry_run,
        cwd: Some(Path::new(".")),
        run_id: None,
        thread_id: None,
        channel: None,
        task_agent_id: None,
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
            auto_check_on_success,
            auto_recover_blocked,
            auto_recover_blocked_max_attempts,
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
            auto_check_on_success,
            auto_recover_blocked,
            auto_recover_blocked_max_attempts,
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
    }
    .map_err(|e| {
        eprintln!("error: {e:?}");
        process::exit(1);
    })
}

#[cfg(test)]
mod tests {
    use super::{
        DeadLetterEntry, DeliveryAck, DeliveryAttempt, DispatchedNotification, LoopStatus,
        Manifest, Notification, NotificationDeliveryMode, RunnerState, ack_retry_policy,
        append_jsonl, apply_status_establish_retry_override, auto_recover_guard_reason,
        blocked_recovery_hint, classify_ack_failure_category,
        completion_guard_waiting_fallback_line, compute_auto_stop_reason, compute_backoff_sec,
        dead_letter_path, delivery_ack_path, delivery_attempts_path, delivery_retry_backoff_sec,
        emit_all_tasks_completed_notifications, extract_pr_url, flush_notifications,
        format_orphan_blocked_notification, format_task_blocked_notification, lease_window_sec,
        normalize_blocked_reason_for_recovery, normalize_error_reason, notification_delivery_mode,
        openclaw_notify_timeout_sec_from, parse_openclaw_message_id, parse_task_checklist_entry,
        queue_main_feedback_summary, queue_notification, read_jsonl,
        should_force_status_establish_retry, should_suppress_waiting_stuck,
        update_waiting_stuck_tracker, validate_task_done_contract_with,
    };
    use chrono::{Duration, Utc};
    use std::{
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
    }

    impl Drop for TestRunDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
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
            auto_check_on_success: true,
            auto_recover_blocked: false,
            auto_recover_blocked_max_attempts: 3,
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

        emit_all_tasks_completed_notifications(
            &run.path,
            &test_manifest(&run.path, run_id, false),
            &runner,
            3,
        )
        .expect("emit all tasks completed notifications");

        let dispatched =
            read_jsonl::<DispatchedNotification>(&run.path.join("notify-dispatched.jsonl"))
                .expect("read dispatched");
        assert_eq!(dispatched.len(), 1);
        assert_eq!(dispatched[0].kind, "all_tasks_completed");
        assert!(dispatched[0].message.contains("all tasks completed"));
        assert!(dispatched[0].message.contains("last_task=S4-2"));

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

        emit_all_tasks_completed_notifications(&run.path, &manifest, &runner, 4)
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
        assert!(hint.contains("dedicated --task-agent-id"));
    }

    #[test]
    fn format_task_blocked_notification_includes_mention_reason_and_next_step() {
        let run = TestRunDir::new("blocked-notify");
        let run_id = Uuid::new_v4();
        let mut manifest = test_manifest(&run.path, run_id, false);
        manifest.requester_user_id = Some("512899958027059201".to_string());
        manifest.auto_recover_blocked = true;

        let msg = format_task_blocked_notification(
            &manifest,
            "S5-2",
            r#"runner exit=2: unexpected EOF while looking for matching '"'"#,
            Some("https://github.com/n01e0/claw-loop/pull/999"),
        );

        assert!(msg.starts_with("<@512899958027059201> task blocked: S5-2"));
        assert!(msg.contains("reason:"));
        assert!(msg.contains("recovery:"));
        assert!(msg.contains("next: auto-recovery is enabled"));
        assert!(msg.contains("PR_URL=https://github.com/n01e0/claw-loop/pull/999"));
    }

    #[test]
    fn format_orphan_blocked_notification_mentions_requester() {
        let run = TestRunDir::new("orphan-notify");
        let run_id = Uuid::new_v4();
        let mut manifest = test_manifest(&run.path, run_id, false);
        manifest.requester_user_id = Some("512899958027059201".to_string());

        let msg = format_orphan_blocked_notification(&manifest, 12345);
        assert!(msg.starts_with("<@512899958027059201> run blocked: daemon pid 12345"));
        assert!(msg.contains("recovery:"));
    }

    #[test]
    fn normalize_blocked_reason_for_recovery_compacts_and_lowercases() {
        let normalized = normalize_blocked_reason_for_recovery("Runner EXIT=2:\n  Timeout ERROR");
        assert!(normalized.contains("runner exit=2:"));
        assert!(normalized.contains("timeout error"));
        assert!(!normalized.contains('\n'));
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
}
