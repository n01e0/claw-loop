#![allow(dead_code)]

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunnerPrBodyInput {
    pub summary: String,
    #[serde(default)]
    pub verification: Vec<String>,
    #[serde(default)]
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerPrBodyFile {
    pub path: PathBuf,
    pub body: String,
}

const EXECUTION_REPORT_TERMS: &[&str] = &[
    "TASK_DONE",
    "TASK_WAITING_MERGE",
    "TASK_WAITING_DEPENDENCY",
    "TASK_BLOCKED",
    "auto-merge",
    "auto merge",
    "waiting for CI",
    "waiting on CI",
    "worktree",
    "runner",
    "daemon",
    "agent session",
    "session id",
    "prompt file",
    "body file",
    "temp file",
    "cleanup",
    "branch deletion",
    "I pushed",
    "I created a branch",
    "I opened the PR",
];

pub fn build_runner_pr_body(input: &RunnerPrBodyInput) -> Result<String> {
    let summary = normalize_required_lines("summary", &[input.summary.as_str()])?;
    let verification_values: Vec<&str> = input.verification.iter().map(String::as_str).collect();
    let verification = normalize_required_lines("verification", &verification_values)?;
    let note_values: Vec<&str> = input.notes.iter().map(String::as_str).collect();
    let notes = normalize_optional_lines(&note_values)?;

    let mut body = String::new();
    body.push_str("## Summary\n");
    push_bullets(&mut body, &summary);
    body.push_str("\n## Verification\n");
    push_bullets(&mut body, &verification);
    if !notes.is_empty() {
        body.push_str("\n## Notes\n");
        push_bullets(&mut body, &notes);
    }

    Ok(body)
}

pub fn write_runner_pr_body_file(
    dir: &Path,
    input: &RunnerPrBodyInput,
) -> Result<RunnerPrBodyFile> {
    let body = build_runner_pr_body(input)?;
    fs::create_dir_all(dir)
        .with_context(|| format!("create PR body directory {}", dir.display()))?;
    let path = dir.join(format!("runner-pr-body-{}.md", uuid::Uuid::new_v4()));
    fs::write(&path, &body).with_context(|| format!("write PR body file {}", path.display()))?;
    Ok(RunnerPrBodyFile { path, body })
}

fn normalize_required_lines(field: &str, values: &[&str]) -> Result<Vec<String>> {
    let lines = normalize_optional_lines(values)?;
    if lines.is_empty() {
        bail!("PR body {field} must not be empty");
    }
    Ok(lines)
}

fn normalize_optional_lines(values: &[&str]) -> Result<Vec<String>> {
    let mut lines = Vec::new();
    for value in values {
        for line in value.lines() {
            let normalized = normalize_line(line);
            if normalized.is_empty() {
                continue;
            }
            reject_execution_report_vocabulary(&normalized)?;
            lines.push(normalized);
        }
    }
    Ok(lines)
}

fn normalize_line(line: &str) -> String {
    line.trim().trim_start_matches('-').trim_start().to_string()
}

fn reject_execution_report_vocabulary(line: &str) -> Result<()> {
    let lowered = line.to_ascii_lowercase();
    for term in EXECUTION_REPORT_TERMS {
        if lowered.contains(&term.to_ascii_lowercase()) {
            bail!("PR body content contains execution-report vocabulary: {term}");
        }
    }
    Ok(())
}

fn push_bullets(body: &mut String, lines: &[String]) {
    for line in lines {
        body.push_str("- ");
        body.push_str(line);
        body.push('\n');
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_fixed_template_from_structured_result() {
        let body = build_runner_pr_body(&RunnerPrBodyInput {
            summary: "Added structured ACPX result parsing.".into(),
            verification: vec!["cargo test --all --all-features passed".into()],
            notes: vec!["Reviewer-facing follow-up remains documented.".into()],
        })
        .expect("body");

        assert_eq!(
            body,
            "## Summary\n- Added structured ACPX result parsing.\n\n## Verification\n- cargo test --all --all-features passed\n\n## Notes\n- Reviewer-facing follow-up remains documented.\n"
        );
    }

    #[test]
    fn omits_empty_notes_section() {
        let body = build_runner_pr_body(&RunnerPrBodyInput {
            summary: "Documented the artifact behavior.".into(),
            verification: vec!["cargo test passed".into()],
            notes: vec![],
        })
        .expect("body");

        assert!(!body.contains("## Notes"));
        assert!(body.starts_with("## Summary\n- Documented"));
    }

    #[test]
    fn rejects_execution_report_vocabulary() {
        let err = build_runner_pr_body(&RunnerPrBodyInput {
            summary: "Implemented body generation.".into(),
            verification: vec!["cargo test passed".into()],
            notes: vec!["Auto-merge is enabled and the runner is waiting for CI.".into()],
        })
        .expect_err("execution report should be rejected");

        assert!(err.to_string().contains("execution-report vocabulary"));
    }

    #[test]
    fn requires_summary_and_verification() {
        assert!(
            build_runner_pr_body(&RunnerPrBodyInput {
                summary: "".into(),
                verification: vec!["cargo test".into()],
                notes: vec![],
            })
            .is_err()
        );
        assert!(
            build_runner_pr_body(&RunnerPrBodyInput {
                summary: "Implemented feature.".into(),
                verification: vec![],
                notes: vec![],
            })
            .is_err()
        );
    }

    #[test]
    fn writes_runner_owned_body_file() {
        let dir =
            std::env::temp_dir().join(format!("claw-loop-pr-body-test-{}", uuid::Uuid::new_v4()));
        let file = write_runner_pr_body_file(
            &dir,
            &RunnerPrBodyInput {
                summary: "Added body builder.".into(),
                verification: vec!["cargo test passed".into()],
                notes: vec![],
            },
        )
        .expect("write body file");

        assert!(file.path.starts_with(&dir));
        assert_eq!(
            fs::read_to_string(&file.path).expect("read body"),
            file.body
        );
        let _ = fs::remove_file(&file.path);
        let _ = fs::remove_dir(&dir);
    }
}
