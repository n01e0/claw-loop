use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
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
    },
    Status {
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        run_id: Uuid,
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
}

#[derive(Debug, Serialize, Deserialize)]
struct Notification {
    event_id: Uuid,
    run_id: Uuid,
    ts: DateTime<Utc>,
    channel: String,
    thread_id: String,
    kind: String,
    message: String,
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

fn runs_root(repo: &Path) -> PathBuf {
    repo.join(".ralph").join("runs")
}

fn run_dir(repo: &Path, run_id: Uuid) -> PathBuf {
    runs_root(repo).join(run_id.to_string())
}

fn pr_tracking_path(run_dir: &Path) -> PathBuf {
    run_dir.join("pr-tracking.json")
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
    Ok(serde_json::from_slice(&data).with_context(|| format!("parse {}", path.display()))?)
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

fn flush_notifications(run_dir: &Path) -> Result<usize> {
    let queue_path = run_dir.join("notify-queue.jsonl");
    let dispatched_path = run_dir.join("notify-dispatched.jsonl");

    let queued = read_jsonl::<Notification>(&queue_path)?;
    if queued.is_empty() {
        return Ok(0);
    }

    let already = read_jsonl::<DispatchedNotification>(&dispatched_path)?;
    let mut seen = HashSet::new();
    for d in already {
        seen.insert(d.event_id);
    }

    let mut delivered = 0usize;
    for n in queued {
        if seen.contains(&n.event_id) {
            continue;
        }
        let d = DispatchedNotification {
            event_id: n.event_id,
            run_id: n.run_id,
            dispatched_at: Utc::now(),
            channel: n.channel,
            thread_id: n.thread_id,
            kind: n.kind,
            message: n.message,
        };
        append_jsonl(&dispatched_path, &d)?;
        delivered += 1;
    }

    // queue is fully consumed by local dispatcher
    if queue_path.exists() {
        fs::remove_file(&queue_path)?;
    }

    if delivered > 0 {
        append_event(
            run_dir,
            "notify_flushed",
            serde_json::json!({"count": delivered}),
        )?;
    }

    Ok(delivered)
}

fn with_timeout_or_fallback(args: &[&str]) -> Result<std::process::Output> {
    let try_timeout = Command::new("timeout")
        .arg("5s")
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output();

    match try_timeout {
        Ok(output) => Ok(output),
        Err(_) => Command::new(args[0])
            .args(&args[1..])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .with_context(|| format!("spawn {}", args[0])),
    }
}

fn gh_pr_view(gh_repo: &str, pr: u64) -> Result<GhPrView> {
    let pr_str = pr.to_string();
    let output = with_timeout_or_fallback(&[
        "gh",
        "pr",
        "view",
        &pr_str,
        "--repo",
        gh_repo,
        "--json",
        "state,url,mergeStateStatus,autoMergeRequest",
    ])?;

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

    let pr_str = pr.to_string();
    let output = with_timeout_or_fallback(&[
        "gh",
        "pr",
        "merge",
        &pr_str,
        "--repo",
        gh_repo,
        "--auto",
        method_flag,
        "--delete-branch",
    ])?;

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
            return Ok(true);
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
            return Ok(true);
        }
        "OPEN" => {
            if view.auto_merge_request.is_none() && merge_state == "CLEAN" {
                if gh_pr_arm_auto_merge(&tracking.gh_repo, tracking.pr, &tracking.merge_method)
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
            return Ok(false);
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
            return Ok(false);
        }
    }
}

fn cmd_start(
    repo: PathBuf,
    session_key: String,
    channel: String,
    thread_id: String,
    owner_message_id: Option<String>,
    tick_sec: u64,
) -> Result<()> {
    let run_id = Uuid::new_v4();
    let dir = run_dir(&repo, run_id);
    fs::create_dir_all(&dir)?;

    let exe = std::env::current_exe().context("resolve current executable")?;
    let child = Command::new(exe)
        .arg("daemon")
        .arg("--repo")
        .arg(&repo)
        .arg("--run-id")
        .arg(run_id.to_string())
        .arg("--tick-sec")
        .arg(tick_sec.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("spawn daemon")?;

    let now = Utc::now();
    let manifest = Manifest {
        run_id,
        repo_path: repo.clone(),
        session_key,
        channel,
        thread_id,
        owner_message_id,
        started_at: now,
        daemon_pid: child.id(),
    };
    let state = State {
        version: 1,
        status: LoopStatus::Running,
        summary: "daemon started".into(),
        waiting_reason: String::new(),
        lease_expires_at: now + chrono::Duration::seconds(lease_window_sec(tick_sec)),
        updated_at: now,
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

    let manifest: Manifest = read_json(&dir.join("manifest.json"))?;
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
            let _ = flush_notifications(&dir)?;
            break;
        }

        let mut state: State = read_json(&dir.join("state.json"))?;
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
            let _ = flush_notifications(&dir)?;
            break;
        }

        state.version += 1;
        state.updated_at = Utc::now();
        state.lease_expires_at =
            state.updated_at + chrono::Duration::seconds(lease_window_sec(tick_sec));

        let pr_changed = reduce_pr_tracking(&dir, &manifest, &mut state)?;
        write_json(&dir.join("state.json"), &state)?;

        append_event(
            &dir,
            "tick",
            serde_json::json!({"version": state.version, "pr_changed": pr_changed}),
        )?;

        let _ = flush_notifications(&dir)?;
        std::thread::sleep(std::time::Duration::from_secs(tick_sec));
    }

    Ok(())
}

fn cmd_stop(repo: PathBuf, run_id: Uuid) -> Result<()> {
    let dir = run_dir(&repo, run_id);
    if !dir.exists() {
        bail!("run directory not found: {}", dir.display());
    }
    fs::write(dir.join("control.stop"), b"stop\n")?;
    println!("stop requested: {}", run_id);
    Ok(())
}

fn cmd_status(repo: PathBuf, run_id: Uuid) -> Result<()> {
    let dir = run_dir(&repo, run_id);
    if !dir.exists() {
        bail!("run directory not found: {}", dir.display());
    }
    let manifest: Manifest = read_json(&dir.join("manifest.json"))?;
    let state: State = read_json(&dir.join("state.json"))?;
    let queued = read_jsonl::<Notification>(&dir.join("notify-queue.jsonl"))?.len();
    let dispatched =
        read_jsonl::<DispatchedNotification>(&dir.join("notify-dispatched.jsonl"))?.len();
    let pr_tracking = if pr_tracking_path(&dir).exists() {
        Some(read_json::<PrTracking>(&pr_tracking_path(&dir))?)
    } else {
        None
    };

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
            "queued_notifications": queued,
            "dispatched_notifications": dispatched,
            "daemon_pid": manifest.daemon_pid,
            "pr_tracking": pr_tracking
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
        let _ = flush_notifications(&dir)?;
        return Ok(Some("terminal"));
    }

    let now = Utc::now();
    let lease_expired = now > state.lease_expires_at;
    let daemon_alive = process_matches_run(manifest.daemon_pid, run_id);

    if lease_expired && !daemon_alive {
        state.version += 1;
        state.status = LoopStatus::Blocked;
        state.summary = format!(
            "orphan detected: daemon pid {} missing after lease expiry",
            manifest.daemon_pid
        );
        state.waiting_reason = "daemon orphan detected".into();
        state.updated_at = now;
        write_json(&dir.join("state.json"), &state)?;

        append_event(
            &dir,
            "orphan_blocked",
            serde_json::json!({
                "daemon_pid": manifest.daemon_pid,
                "lease_expires_at": state.lease_expires_at,
                "now": now,
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

        let _ = flush_notifications(&dir)?;
        return Ok(Some("blocked_orphan"));
    }

    let _ = flush_notifications(&dir)?;
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

#[cfg(test)]
mod tests {
    use super::{compute_backoff_sec, lease_window_sec};

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
        } => cmd_start(
            repo,
            session_key,
            channel,
            thread_id,
            owner_message_id,
            tick_sec,
        ),
        Commands::Daemon {
            repo,
            run_id,
            tick_sec,
        } => cmd_daemon(repo, run_id, tick_sec),
        Commands::Stop { repo, run_id } => cmd_stop(repo, run_id),
        Commands::Status { repo, run_id } => cmd_status(repo, run_id),
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
    }
    .map_err(|e| {
        eprintln!("error: {e:?}");
        process::exit(1);
    })
}
