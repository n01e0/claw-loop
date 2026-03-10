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
    use super::{parse_task_checklist_entry, update_task_check};
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
}
