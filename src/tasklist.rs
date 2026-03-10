use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use std::{fs, path::Path};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct TaskChecklistEntry {
    pub(crate) line_no: usize,
    pub(crate) done: bool,
    pub(crate) id: String,
    pub(crate) text: String,
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

fn clip_recovery_reason(reason: &str, max_chars: usize) -> String {
    let flat = reason.lines().map(str::trim).collect::<Vec<_>>().join(" ");
    let clipped: String = flat.chars().take(max_chars).collect();
    if clipped.trim().is_empty() {
        "blocked without reason".to_string()
    } else {
        clipped
    }
}

pub(crate) fn append_recovery_task_for_blocked(
    file: &Path,
    blocked_id: &str,
    blocked_reason: &str,
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

    let clipped_reason = clip_recovery_reason(blocked_reason, 160);
    let text = format!("auto-recover from {blocked_id}: {clipped_reason}");
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
        task_checklist_done_count, update_task_check,
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

        let entry =
            append_recovery_task_for_blocked(&file, "S5-6", "runner exit=2: command failed")
                .expect("append recovery task");

        assert_eq!(entry.id, "S5-6-RECOVER-2");
        assert!(!entry.done);
        assert!(entry.text.contains("auto-recover from S5-6"));

        let content = fs::read_to_string(&file).expect("read file");
        assert!(content.contains("- [ ] S5-6-RECOVER-2: auto-recover from S5-6"));
    }

    #[test]
    fn append_recovery_task_for_blocked_clips_multiline_reason() {
        let file = temp_file("append-recovery-clip");
        fs::write(&file, "- [ ] S5-6: blocked base\n").expect("write file");

        let long_reason = "line1\nline2\n".to_string() + &"x".repeat(300);
        let entry = append_recovery_task_for_blocked(&file, "S5-6", &long_reason)
            .expect("append recovery task");

        assert!(entry.text.len() < 260);
        assert!(!entry.text.contains('\n'));
    }
}
