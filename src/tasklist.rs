use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{fs, path::Path};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct TaskChecklistEntry {
    pub(crate) line_no: usize,
    pub(crate) done: bool,
    pub(crate) id: String,
    pub(crate) text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct TaskApprovalMetadata {
    pub(crate) approved_by: String,
    pub(crate) approved_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct TaskApprovalStatus {
    pub(crate) approved_by: Option<String>,
    pub(crate) approved_at: Option<DateTime<Utc>>,
    pub(crate) approved_tasklist_hash: String,
}

pub(crate) fn parse_task_checklist_entry(line_no: usize, line: &str) -> Option<TaskChecklistEntry> {
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

pub(crate) fn task_checklist_done_count(file: &Path) -> Result<u64> {
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

pub(crate) fn load_task_checklist(
    file: &Path,
) -> Result<(String, Vec<String>, Vec<TaskChecklistEntry>)> {
    let content = fs::read_to_string(file).with_context(|| format!("read {}", file.display()))?;
    let lines: Vec<String> = content.lines().map(|l| l.to_string()).collect();
    let entries: Vec<TaskChecklistEntry> = lines
        .iter()
        .enumerate()
        .filter_map(|(idx, line)| parse_task_checklist_entry(idx + 1, line))
        .collect();
    Ok((content, lines, entries))
}

fn parse_task_approval_metadata_from_content(
    content: &str,
) -> Result<Option<TaskApprovalMetadata>> {
    let mut approved_by: Option<String> = None;
    let mut approved_at: Option<String> = None;

    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(value) = trimmed.strip_prefix("Approved-By:") {
            let value = value.trim();
            if !value.is_empty() {
                approved_by = Some(value.to_string());
            }
        } else if let Some(value) = trimmed.strip_prefix("Approved-At:") {
            let value = value.trim();
            if !value.is_empty() {
                approved_at = Some(value.to_string());
            }
        }
    }

    match (approved_by, approved_at) {
        (None, None) => Ok(None),
        (Some(_), None) | (None, Some(_)) => {
            bail!("tasklist approval markers must include both Approved-By and Approved-At")
        }
        (Some(approved_by), Some(approved_at)) => {
            let approved_at = DateTime::parse_from_rfc3339(&approved_at)
                .with_context(|| format!("parse Approved-At timestamp: {approved_at}"))?
                .with_timezone(&Utc);
            Ok(Some(TaskApprovalMetadata {
                approved_by,
                approved_at,
            }))
        }
    }
}

fn task_plan_hash_from_entries(entries: &[TaskChecklistEntry]) -> String {
    let mut hasher = Sha256::new();
    for (idx, entry) in entries.iter().enumerate() {
        hasher.update(idx.to_string().as_bytes());
        hasher.update(b"\t");
        hasher.update(entry.id.as_bytes());
        hasher.update(b"\t");
        hasher.update(entry.text.as_bytes());
        hasher.update(b"\n");
    }
    format!("{:x}", hasher.finalize())
}

pub(crate) fn task_approval_status(file: &Path) -> Result<TaskApprovalStatus> {
    let (content, _, entries) = load_task_checklist(file)?;
    let approval = parse_task_approval_metadata_from_content(&content)?;
    Ok(TaskApprovalStatus {
        approved_by: approval.as_ref().map(|m| m.approved_by.clone()),
        approved_at: approval.as_ref().map(|m| m.approved_at),
        approved_tasklist_hash: task_plan_hash_from_entries(&entries),
    })
}

pub(crate) fn task_plan_hash(file: &Path) -> Result<String> {
    Ok(task_approval_status(file)?.approved_tasklist_hash)
}

pub(crate) fn write_task_approval(file: &Path, approved_by: &str) -> Result<TaskApprovalStatus> {
    let content = fs::read_to_string(file).with_context(|| format!("read {}", file.display()))?;
    let had_trailing_newline = content.ends_with('\n');
    let mut lines: Vec<String> = content.lines().map(|l| l.to_string()).collect();
    lines.retain(|line| {
        let trimmed = line.trim_start();
        !trimmed.starts_with("Approved-By:") && !trimmed.starts_with("Approved-At:")
    });

    let approved_by = approved_by.trim();
    if approved_by.is_empty() {
        bail!("approved_by must not be empty")
    }

    let approved_at = Utc::now();
    let mut insert_at = 0usize;
    if lines
        .first()
        .is_some_and(|line| line.trim_start().starts_with('#'))
    {
        insert_at = 1;
        if lines
            .get(insert_at)
            .is_some_and(|line| line.trim().is_empty())
        {
            insert_at += 1;
        }
    }

    let marker_lines = vec![
        format!("Approved-By: {}", approved_by),
        format!("Approved-At: {}", approved_at.to_rfc3339()),
        String::new(),
    ];
    for (idx, marker) in marker_lines.into_iter().enumerate() {
        lines.insert(insert_at + idx, marker);
    }

    let mut rebuilt = lines.join("\n");
    if had_trailing_newline || !rebuilt.ends_with('\n') {
        rebuilt.push('\n');
    }
    fs::write(file, rebuilt).with_context(|| format!("write {}", file.display()))?;

    task_approval_status(file)
}

fn clip_recovery_task_text(text: &str, blocked_id: &str, max_chars: usize) -> String {
    let flat = text.lines().map(str::trim).collect::<Vec<_>>().join(" ");
    let clipped: String = flat.chars().take(max_chars).collect();
    if clipped.trim().is_empty() {
        format!("resolve blocked task {blocked_id}")
    } else {
        clipped
    }
}

pub(crate) fn append_recovery_task_for_blocked(
    file: &Path,
    blocked_id: &str,
    recovery_task_text: &str,
) -> Result<TaskChecklistEntry> {
    let (content, mut lines, entries) = load_task_checklist(file)?;
    let had_trailing_newline = content.ends_with('\n');

    let base_id = format!("{blocked_id}-RECOVER");
    let existing_ids = entries
        .iter()
        .map(|e| e.id.as_str())
        .collect::<std::collections::HashSet<_>>();

    let mut candidate = base_id.clone();
    let mut suffix = 2usize;
    while existing_ids.contains(candidate.as_str()) {
        candidate = format!("{base_id}-{suffix}");
        suffix = suffix.saturating_add(1);
    }

    let text = clip_recovery_task_text(recovery_task_text, blocked_id, 240);
    lines.push(format!("- [ ] {}: {}", candidate, text));

    let mut rebuilt = lines.join("\n");
    if had_trailing_newline || !rebuilt.ends_with('\n') {
        rebuilt.push('\n');
    }
    fs::write(file, rebuilt).with_context(|| format!("write {}", file.display()))?;

    Ok(TaskChecklistEntry {
        line_no: lines.len(),
        done: false,
        id: candidate,
        text,
    })
}

pub(crate) fn update_task_check(file: &Path, id: &str, done: bool) -> Result<serde_json::Value> {
    let (content, mut lines, entries) = load_task_checklist(file)?;
    let had_trailing_newline = content.ends_with('\n');

    let target = entries
        .iter()
        .find(|entry| entry.id == id)
        .cloned()
        .ok_or_else(|| anyhow!("task id not found: {id}"))?;

    let idx = target.line_no.saturating_sub(1);
    let mut changed = false;
    if target.done != done {
        if done {
            lines[idx] = lines[idx].replacen("[ ]", "[x]", 1);
        } else {
            lines[idx] = lines[idx].replacen("[x]", "[ ]", 1);
        }
        changed = true;
    }

    if changed {
        let mut rebuilt = lines.join("\n");
        if had_trailing_newline {
            rebuilt.push('\n');
        }
        fs::write(file, rebuilt).with_context(|| format!("write {}", file.display()))?;
    }

    let (_, _, updated_entries) = load_task_checklist(file)?;
    let total = updated_entries.len();
    let done_count = updated_entries.iter().filter(|entry| entry.done).count();

    Ok(serde_json::json!({
        "file": file,
        "id": id,
        "line": target.line_no,
        "done": done,
        "changed": changed,
        "summary": {
            "total": total,
            "done": done_count,
            "open": total.saturating_sub(done_count),
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::{
        append_recovery_task_for_blocked, load_task_checklist, parse_task_checklist_entry,
        task_approval_status, task_checklist_done_count, task_plan_hash, update_task_check,
        write_task_approval,
    };
    use std::{fs, path::PathBuf};
    use uuid::Uuid;

    fn temp_file(name: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "claw-loopd-tasklist-test-{name}-{}",
            Uuid::new_v4()
        ));
        fs::create_dir_all(&base).expect("create temp dir");
        base.join("tasklist.md")
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
    fn update_task_check_preserves_state_when_no_change() {
        let file = temp_file("no-change");
        fs::write(&file, "- [ ] A1: alpha\n").expect("write file");

        let out = update_task_check(&file, "A1", false).expect("update task check");
        assert_eq!(out["changed"], false);

        let content = fs::read_to_string(&file).expect("read file");
        assert_eq!(content, "- [ ] A1: alpha\n");
    }

    #[test]
    fn update_task_check_marks_done_and_updates_summary_counts() {
        let file = temp_file("flip-done");
        fs::write(&file, "- [ ] A1: alpha\n- [x] A2: bravo\n").expect("write file");

        let out = update_task_check(&file, "A1", true).expect("update task check");
        assert_eq!(out["changed"], true);
        assert_eq!(out["summary"]["total"], 2);
        assert_eq!(out["summary"]["done"], 2);
        assert_eq!(out["summary"]["open"], 0);

        let content = fs::read_to_string(&file).expect("read file");
        assert_eq!(content, "- [x] A1: alpha\n- [x] A2: bravo\n");

        let (_, _, entries) = load_task_checklist(&file).expect("load checklist");
        assert!(entries.iter().all(|e| e.done));
    }

    #[test]
    fn update_task_check_errors_for_missing_task_id() {
        let file = temp_file("missing-id");
        fs::write(&file, "- [ ] A1: alpha\n").expect("write file");

        let err = update_task_check(&file, "A9", true).expect_err("missing id should fail");
        assert!(err.to_string().contains("task id not found"));
    }

    #[test]
    fn task_checklist_done_count_returns_zero_for_missing_file() {
        let file = temp_file("missing-file");
        // do not create tasklist file
        let count = task_checklist_done_count(&file).expect("done count on missing file");
        assert_eq!(count, 0);
    }

    #[test]
    fn append_recovery_task_for_blocked_appends_unique_unchecked_entry() {
        let file = temp_file("append-recovery");
        fs::write(
            &file,
            "- [ ] S5-6: blocked base\n- [ ] S5-6-RECOVER: existing\n",
        )
        .expect("write file");

        let entry = append_recovery_task_for_blocked(
            &file,
            "S5-6",
            "resolve runner block for task S5-6: restore missing command path",
        )
        .expect("append recovery task");

        assert_eq!(entry.id, "S5-6-RECOVER-2");
        assert!(!entry.done);
        assert!(entry.text.contains("resolve runner block for task S5-6"));

        let content = fs::read_to_string(&file).expect("read file");
        assert!(content.contains(
            "- [ ] S5-6-RECOVER-2: resolve runner block for task S5-6: restore missing command path"
        ));
    }

    #[test]
    fn append_recovery_task_for_blocked_clips_multiline_reason() {
        let file = temp_file("append-recovery-clip");
        fs::write(&file, "- [ ] S5-6: blocked base\n").expect("write file");

        let long_text = "resolve runner block for task S5-6: ".to_string() + &"x".repeat(400);
        let entry = append_recovery_task_for_blocked(&file, "S5-6", &long_text)
            .expect("append recovery task");

        assert!(entry.text.len() <= 240);
        assert!(!entry.text.contains('\n'));
        assert!(
            entry
                .text
                .starts_with("resolve runner block for task S5-6:")
        );
    }

    #[test]
    fn task_plan_hash_ignores_checkbox_state() {
        let file = temp_file("plan-hash-checkbox");
        fs::write(&file, "- [ ] A1: alpha\n- [x] A2: bravo\n").expect("write file");
        let hash1 = task_plan_hash(&file).expect("hash1");

        fs::write(&file, "- [x] A1: alpha\n- [ ] A2: bravo\n").expect("rewrite file");
        let hash2 = task_plan_hash(&file).expect("hash2");

        assert_eq!(hash1, hash2);
    }

    #[test]
    fn write_task_approval_writes_markers_and_hash() {
        let file = temp_file("task-approve");
        fs::write(&file, "# Title\n\n- [ ] A1: alpha\n").expect("write file");

        let status = write_task_approval(&file, "n01e0").expect("write approval");
        let content = fs::read_to_string(&file).expect("read file");

        assert!(content.contains("Approved-By: n01e0"));
        assert!(content.contains("Approved-At: "));
        assert_eq!(status.approved_by.as_deref(), Some("n01e0"));
        assert!(status.approved_at.is_some());
        assert!(!status.approved_tasklist_hash.is_empty());
    }

    #[test]
    fn task_approval_status_requires_both_markers_when_partial() {
        let file = temp_file("task-approval-partial");
        fs::write(&file, "Approved-By: n01e0\n- [ ] A1: alpha\n").expect("write file");

        let err = task_approval_status(&file).expect_err("partial approval should fail");
        assert!(err.to_string().contains("Approved-By and Approved-At"));
    }
}
