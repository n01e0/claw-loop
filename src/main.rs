use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
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

#[derive(Debug, Serialize, Deserialize)]
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

#[derive(Debug, Serialize, Deserialize)]
struct State {
    version: u64,
    status: LoopStatus,
    summary: String,
    waiting_reason: String,
    lease_expires_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

fn runs_root(repo: &Path) -> PathBuf {
    repo.join(".ralph").join("runs")
}

fn run_dir(repo: &Path, run_id: Uuid) -> PathBuf {
    runs_root(repo).join(run_id.to_string())
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

fn append_event(run_dir: &Path, line: &str) -> Result<()> {
    let events_path = run_dir.join("events.jsonl");
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&events_path)
        .with_context(|| format!("open {}", events_path.display()))?;
    writeln!(f, "{}", line)?;
    Ok(())
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
        lease_expires_at: now + chrono::Duration::seconds((tick_sec as i64) * 3),
        updated_at: now,
    };

    write_json(&dir.join("manifest.json"), &manifest)?;
    write_json(&dir.join("state.json"), &state)?;
    append_event(
        &dir,
        &serde_json::json!({"ts": now, "kind": "daemon_started", "pid": child.id()}).to_string(),
    )?;

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

    let control_stop = dir.join("control.stop");
    loop {
        if control_stop.exists() {
            let mut state: State = read_json(&dir.join("state.json"))?;
            state.version += 1;
            state.status = LoopStatus::Stopped;
            state.summary = "stopped by control file".into();
            state.updated_at = Utc::now();
            write_json(&dir.join("state.json"), &state)?;
            append_event(
                &dir,
                &serde_json::json!({"ts": Utc::now(), "kind": "daemon_stopped"}).to_string(),
            )?;
            break;
        }

        let mut state: State = read_json(&dir.join("state.json"))?;
        if matches!(
            state.status,
            LoopStatus::Done | LoopStatus::Failed | LoopStatus::Stopped
        ) {
            append_event(
                &dir,
                &serde_json::json!({"ts": Utc::now(), "kind": "daemon_exit_terminal", "status": format!("{:?}", state.status)}).to_string(),
            )?;
            break;
        }

        state.version += 1;
        state.updated_at = Utc::now();
        state.lease_expires_at =
            state.updated_at + chrono::Duration::seconds((tick_sec as i64) * 3);
        write_json(&dir.join("state.json"), &state)?;

        append_event(
            &dir,
            &serde_json::json!({"ts": Utc::now(), "kind": "tick", "version": state.version})
                .to_string(),
        )?;

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
    }
    .map_err(|e| {
        eprintln!("error: {e:?}");
        process::exit(1);
    })
}
