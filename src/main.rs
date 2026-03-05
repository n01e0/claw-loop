use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{self, Command, Stdio};
use uuid::Uuid;

#[derive(Parser, Debug)]
#[command(name = "claw-loopd")]
#[command(about = "Thread-bound Ralph loop daemon controller")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
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
}

#[derive(Debug, Serialize, Deserialize)]
struct Manifest {
    run_id: Uuid,
    repo_path: PathBuf,
    session_key: String,
    channel: String,
    thread_id: String,
    owner_message_id: Option<String>,
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

#[derive(Debug, Clone)]
struct TaskChecklistEntry {
    line_no: usize,
    done: bool,
    id: String,
    text: String,
}

struct StartOptions {
    repo: PathBuf,
    session_key: String,
    channel: String,
    thread_id: String,
    owner_message_id: Option<String>,
    tick_sec: u64,
    deliver_openclaw: bool,
    max_ticks: Option<u64>,
    max_runtime_sec: Option<u64>,
    max_task_loops: u64,
    task_file: PathBuf,
}

fn default_max_task_loops() -> u64 {
    10
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

fn deliver_via_openclaw(notification: &Notification) -> Result<()> {
    let mut args: Vec<String> = vec![
        "message".into(),
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
    ];

    if std::env::var("CLAW_LOOPD_OPENCLAW_DRY_RUN").ok().as_deref() == Some("1") {
        args.push("--dry-run".into());
    }

    let openclaw = openclaw_bin();
    let output = run_with_timeout_cmd(&openclaw, &args, 5)?;
    if !output.status.success() {
        bail!(
            "openclaw message send failed: status={:?} stderr={}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    Ok(())
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

fn queue_notification(
    run_dir: &Path,
    manifest: &Manifest,
    kind: impl Into<String>,
    message: impl Into<String>,
) -> Result<Uuid> {
    let n = Notification {
        event_id: Uuid::new_v4(),
        run_id: manifest.run_id,
        ts: Utc::now(),
        channel: manifest.channel.clone(),
        thread_id: manifest.thread_id.clone(),
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
        serde_json::json!({"event_id": n.event_id, "kind": n.kind}),
    )?;
    Ok(n.event_id)
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

fn parse_task_checklist_entry(line_no: usize, line: &str) -> Option<TaskChecklistEntry> {
    let trimmed = line.trim_start();
    let (done, rest) = if let Some(rest) = trimmed.strip_prefix("- [ ] ") {
        (false, rest)
    } else if let Some(rest) = trimmed.strip_prefix("- [x] ") {
        (true, rest)
    } else {
        return None;
    };

    let (id_raw, text_raw) = rest.split_once(':')?;
    let id = id_raw.trim().to_string();
    if id.is_empty() {
        return None;
    }

    let text = text_raw.trim().to_string();
    Some(TaskChecklistEntry {
        line_no,
        done,
        id,
        text,
    })
}

fn task_checklist_done_count(file: &Path) -> Result<u64> {
    if !file.exists() {
        return Ok(0);
    }

    let content = fs::read_to_string(file).with_context(|| format!("read {}", file.display()))?;
    let done = content
        .lines()
        .enumerate()
        .filter_map(|(idx, line)| parse_task_checklist_entry(idx + 1, line))
        .filter(|entry| entry.done)
        .count() as u64;
    Ok(done)
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
    let mut seen = HashSet::new();
    for d in already {
        seen.insert(d.event_id);
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

    for mut n in queued {
        if seen.contains(&n.event_id) {
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

        let delivery_result = if manifest.deliver_openclaw {
            deliver_via_openclaw(&n)
        } else {
            Ok(())
        };

        match delivery_result {
            Ok(()) => {
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

                let policy = ack_retry_policy(&category, n.attempts);
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

fn delivery_retry_backoff_sec(attempts: u32) -> i64 {
    match attempts {
        0 | 1 => 5,
        2 => 15,
        3 => 30,
        _ => 60,
    }
}

fn delivery_max_attempts() -> u32 {
    std::env::var("CLAW_LOOPD_DELIVERY_MAX_ATTEMPTS")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(5)
}

#[derive(Debug, Clone, Copy)]
struct AckRetryPolicy {
    retryable: bool,
    max_attempts: u32,
    backoff_sec: i64,
}

fn ack_retry_policy(category: &str, attempts: u32) -> AckRetryPolicy {
    let global_max = delivery_max_attempts();
    let default_backoff = delivery_retry_backoff_sec(attempts);

    match category {
        // Non-retryable categories.
        "auth" | "permission" | "not_found" => AckRetryPolicy {
            retryable: false,
            max_attempts: 1,
            backoff_sec: 0,
        },
        // Retryable, but with longer backoff than transport jitter.
        "rate_limited" => AckRetryPolicy {
            retryable: true,
            max_attempts: global_max,
            backoff_sec: match attempts {
                0 | 1 => 30,
                2 => 60,
                3 => 120,
                _ => 300,
            },
        },
        // Retryable defaults.
        "timeout" | "transport" | "upstream_5xx" | "unknown" => AckRetryPolicy {
            retryable: true,
            max_attempts: global_max,
            backoff_sec: default_backoff,
        },
        // Future-safe fallback.
        _ => AckRetryPolicy {
            retryable: true,
            max_attempts: global_max,
            backoff_sec: default_backoff,
        },
    }
}

fn ack_retry_policy_snapshot() -> serde_json::Value {
    let max_attempts = delivery_max_attempts();
    serde_json::json!({
        "retryable_categories": ["timeout", "transport", "rate_limited", "upstream_5xx", "unknown"],
        "non_retryable_categories": ["auth", "permission", "not_found"],
        "max_attempts": {
            "default_retryable": max_attempts,
            "non_retryable": 1
        },
        "backoff_seconds": {
            "default": [5, 5, 15, 30, 60],
            "rate_limited": [30, 30, 60, 120, 300]
        }
    })
}

fn sanitize_reason_fallback(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut prev_sep = false;
    for ch in raw.chars() {
        let mapped = if ch.is_ascii_digit() {
            '#'
        } else if ch.is_ascii_whitespace() {
            ' '
        } else {
            ch.to_ascii_lowercase()
        };

        let sep = mapped == ' ' || mapped == ':' || mapped == ';' || mapped == ',';
        if sep {
            if !prev_sep {
                out.push(' ');
            }
        } else {
            out.push(mapped);
        }
        prev_sep = sep;
    }

    let trimmed = out.trim();
    if trimmed.is_empty() {
        "unknown".to_string()
    } else {
        trimmed.chars().take(96).collect()
    }
}

fn normalize_error_reason(raw: Option<&str>) -> String {
    let line = raw
        .unwrap_or("unknown")
        .lines()
        .next()
        .unwrap_or("unknown")
        .trim()
        .to_ascii_lowercase();

    if line.is_empty() {
        return "unknown".to_string();
    }

    if line.contains("openclaw message send failed") {
        return "openclaw_send_failed".to_string();
    }
    if line.contains("timeout") || line.contains("timed out") {
        return "timeout".to_string();
    }
    if line.contains("rate limit") || line.contains("429") {
        return "rate_limited".to_string();
    }
    if line.contains("permission denied") {
        return "permission_denied".to_string();
    }
    if line.contains("unauthorized") || line.contains(" 401") || line.ends_with("401") {
        return "unauthorized".to_string();
    }
    if line.contains("forbidden") || line.contains(" 403") || line.ends_with("403") {
        return "forbidden".to_string();
    }
    if line.contains("not found") || line.contains("no such file") || line.contains(" 404") {
        return "not_found".to_string();
    }
    if line.contains("connection refused") {
        return "connection_refused".to_string();
    }
    if line.contains("network is unreachable")
        || line.contains("temporary failure in name resolution")
        || line.contains("name or service not known")
        || line.contains("dns")
    {
        return "dns_or_network".to_string();
    }
    if line.contains("broken pipe") {
        return "broken_pipe".to_string();
    }
    if line.contains("500") || line.contains("502") || line.contains("503") || line.contains("504")
    {
        return "upstream_5xx".to_string();
    }

    sanitize_reason_fallback(&line)
}

fn classify_ack_failure_category(raw: Option<&str>) -> String {
    match normalize_error_reason(raw).as_str() {
        "timeout" => "timeout".to_string(),
        "rate_limited" => "rate_limited".to_string(),
        "unauthorized" => "auth".to_string(),
        "forbidden" | "permission_denied" => "permission".to_string(),
        "not_found" => "not_found".to_string(),
        "upstream_5xx" => "upstream_5xx".to_string(),
        "connection_refused" | "dns_or_network" | "broken_pipe" | "openclaw_send_failed" => {
            "transport".to_string()
        }
        _ => "unknown".to_string(),
    }
}

fn compute_auto_stop_reason(
    task_loops_completed: u64,
    max_task_loops: u64,
    ticks: u64,
    max_ticks: Option<u64>,
    started_at: DateTime<Utc>,
    max_runtime_sec: Option<u64>,
    now: DateTime<Utc>,
) -> Option<String> {
    if task_loops_completed >= max_task_loops {
        return Some(format!(
            "max_task_loops reached ({task_loops_completed}/{max_task_loops})"
        ));
    }

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
        started_at: now,
        daemon_pid: child.id(),
        deliver_openclaw: opts.deliver_openclaw,
        max_ticks: opts.max_ticks,
        max_runtime_sec: opts.max_runtime_sec,
        max_task_loops: opts.max_task_loops,
        task_file,
        task_done_baseline,
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
            queue_notification(&dir, &manifest, "stopped", "loop daemon stopped")?;
            let _ = flush_notifications(&dir, &manifest)?;
            break;
        }

        let mut state: State = read_json(&dir.join("state.json"))?;
        let now = Utc::now();

        let task_file_abs = resolve_task_file_path(&manifest.repo_path, &manifest.task_file);
        let task_done_now = match task_checklist_done_count(&task_file_abs) {
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
        let task_loops_completed = task_done_now.saturating_sub(manifest.task_done_baseline);

        if let Some(reason) = compute_auto_stop_reason(
            task_loops_completed,
            manifest.max_task_loops,
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
            queue_notification(
                &dir,
                &manifest,
                "auto_stopped",
                format!("loop daemon auto-stopped: {}", state.waiting_reason),
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
        write_json(&dir.join("state.json"), &state)?;

        append_event(
            &dir,
            "tick",
            serde_json::json!({
                "version": state.version,
                "ticks": state.ticks,
                "task_loops_completed": task_loops_completed,
                "max_task_loops": manifest.max_task_loops,
                "pr_changed": pr_changed,
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
        queue_notification(
            &dir,
            &manifest,
            "stopped",
            "loop daemon stopped immediately by kill switch".to_string(),
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

    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "run_id": manifest.run_id,
            "thread_id": manifest.thread_id,
            "session_key": manifest.session_key,
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
            format!(
                "run blocked: daemon pid {} missing after lease expiry",
                manifest.daemon_pid
            ),
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
    let content = fs::read_to_string(&file).with_context(|| format!("read {}", file.display()))?;
    let lines: Vec<&str> = content.lines().collect();

    let entries: Vec<TaskChecklistEntry> = lines
        .iter()
        .enumerate()
        .filter_map(|(idx, line)| parse_task_checklist_entry(idx + 1, line))
        .collect();

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
    let content = fs::read_to_string(&file).with_context(|| format!("read {}", file.display()))?;
    let had_trailing_newline = content.ends_with('\n');
    let mut lines: Vec<String> = content.lines().map(|l| l.to_string()).collect();

    let mut found_line = None;
    let mut changed = false;

    for (idx, line) in lines.iter_mut().enumerate() {
        if let Some(entry) = parse_task_checklist_entry(idx + 1, line)
            && entry.id == id
        {
            found_line = Some(idx + 1);
            if entry.done != done {
                if done {
                    *line = line.replacen("[ ]", "[x]", 1);
                } else {
                    *line = line.replacen("[x]", "[ ]", 1);
                }
                changed = true;
            }
            break;
        }
    }

    let line_no = match found_line {
        Some(v) => v,
        None => bail!("task id not found: {id}"),
    };

    if changed {
        let mut rebuilt = lines.join("\n");
        if had_trailing_newline {
            rebuilt.push('\n');
        }
        fs::write(&file, rebuilt).with_context(|| format!("write {}", file.display()))?;
    }

    let entries: Vec<TaskChecklistEntry> = lines
        .iter()
        .enumerate()
        .filter_map(|(idx, line)| parse_task_checklist_entry(idx + 1, line))
        .collect();
    let total = entries.len();
    let done_count = entries.iter().filter(|e| e.done).count();

    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "file": file,
            "id": id,
            "line": line_no,
            "done": done,
            "changed": changed,
            "summary": {
                "total": total,
                "done": done_count,
                "open": total.saturating_sub(done_count),
            }
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
            tick_sec,
            deliver_openclaw,
            max_ticks,
            max_runtime_sec,
            max_task_loops,
            task_file,
        } => cmd_start(StartOptions {
            repo,
            session_key,
            channel,
            thread_id,
            owner_message_id,
            tick_sec,
            deliver_openclaw,
            max_ticks,
            max_runtime_sec,
            max_task_loops,
            task_file,
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
    }
    .map_err(|e| {
        eprintln!("error: {e:?}");
        process::exit(1);
    })
}

#[cfg(test)]
mod tests {
    use super::{
        ack_retry_policy, classify_ack_failure_category, compute_auto_stop_reason,
        compute_backoff_sec, delivery_retry_backoff_sec, lease_window_sec, normalize_error_reason,
        parse_task_checklist_entry,
    };

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
    fn normalize_error_reason_classifies_common_patterns() {
        assert_eq!(
            normalize_error_reason(Some("openclaw message send failed: status=1 stderr=mock")),
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
    fn auto_stop_reason_prefers_max_task_loops() {
        let started_at = chrono::Utc::now() - chrono::Duration::seconds(1);
        let now = chrono::Utc::now();
        let reason = compute_auto_stop_reason(3, 3, 0, Some(100), started_at, Some(3600), now)
            .expect("expected auto stop");
        assert!(reason.contains("max_task_loops"));
    }

    #[test]
    fn auto_stop_reason_hits_runtime_limit() {
        let started_at = chrono::Utc::now() - chrono::Duration::seconds(11);
        let now = chrono::Utc::now();
        let reason = compute_auto_stop_reason(0, 10, 2, None, started_at, Some(10), now)
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
}
