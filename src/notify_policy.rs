use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NotificationDeliveryMode {
    Send,
    EditStatus,
}

pub(crate) fn notification_delivery_mode(kind: &str) -> NotificationDeliveryMode {
    let normalized = kind.trim().to_ascii_lowercase();

    let important_new_post = matches!(
        normalized.as_str(),
        "blocked"
            | "done"
            | "stopped"
            | "auto_stopped"
            | "orphan_blocked"
            | "all_tasks_completed"
            | "pr_closed"
    );

    if important_new_post {
        NotificationDeliveryMode::Send
    } else {
        NotificationDeliveryMode::EditStatus
    }
}

pub(crate) fn parse_openclaw_message_id(stdout: &[u8]) -> Option<String> {
    let value: Value = serde_json::from_slice(stdout).ok()?;
    let payload = value.get("payload")?;

    let from_payload = payload
        .get("messageId")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned);
    if from_payload.is_some() {
        return from_payload;
    }

    payload
        .get("result")
        .and_then(|v| v.get("messageId"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
}

pub(crate) fn delivery_retry_backoff_sec(attempts: u32) -> i64 {
    match attempts {
        0 | 1 => 5,
        2 => 15,
        3 => 30,
        _ => 60,
    }
}

pub(crate) fn delivery_max_attempts() -> u32 {
    std::env::var("CLAW_LOOPD_DELIVERY_MAX_ATTEMPTS")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(5)
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct AckRetryPolicy {
    pub(crate) retryable: bool,
    pub(crate) max_attempts: u32,
    pub(crate) backoff_sec: i64,
}

pub(crate) fn ack_retry_policy(category: &str, attempts: u32) -> AckRetryPolicy {
    let global_max = delivery_max_attempts();
    let default_backoff = delivery_retry_backoff_sec(attempts);

    match category {
        "auth" | "permission" | "not_found" => AckRetryPolicy {
            retryable: false,
            max_attempts: 1,
            backoff_sec: 0,
        },
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
        "timeout" | "transport" | "upstream_5xx" | "unknown" => AckRetryPolicy {
            retryable: true,
            max_attempts: global_max,
            backoff_sec: default_backoff,
        },
        _ => AckRetryPolicy {
            retryable: true,
            max_attempts: global_max,
            backoff_sec: default_backoff,
        },
    }
}

pub(crate) fn ack_retry_policy_snapshot() -> Value {
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

pub(crate) fn normalize_error_reason(raw: Option<&str>) -> String {
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

    if line.contains("openclaw message") && line.contains("failed") {
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

pub(crate) fn classify_ack_failure_category(raw: Option<&str>) -> String {
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

#[cfg(test)]
mod tests {
    use super::{
        NotificationDeliveryMode, classify_ack_failure_category, delivery_retry_backoff_sec,
        normalize_error_reason, notification_delivery_mode, parse_openclaw_message_id,
    };

    #[test]
    fn notification_delivery_mode_contract() {
        assert_eq!(
            notification_delivery_mode("run_started"),
            NotificationDeliveryMode::EditStatus
        );
        assert_eq!(
            notification_delivery_mode("task_waiting_stuck"),
            NotificationDeliveryMode::EditStatus
        );
        assert_eq!(
            notification_delivery_mode("blocked"),
            NotificationDeliveryMode::Send
        );
        assert_eq!(
            notification_delivery_mode("auto_stopped"),
            NotificationDeliveryMode::Send
        );
    }

    #[test]
    fn parse_openclaw_message_id_reads_cli_payload() {
        let sample = serde_json::json!({
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
    fn normalize_error_reason_classifies_edit_failures() {
        assert_eq!(
            normalize_error_reason(Some("openclaw message edit failed: status=1 stderr=mock")),
            "openclaw_send_failed"
        );
        assert_eq!(
            classify_ack_failure_category(Some("openclaw message edit failed: status=1")),
            "transport"
        );
    }

    #[test]
    fn delivery_retry_backoff_schedule() {
        assert_eq!(delivery_retry_backoff_sec(0), 5);
        assert_eq!(delivery_retry_backoff_sec(1), 5);
        assert_eq!(delivery_retry_backoff_sec(2), 15);
        assert_eq!(delivery_retry_backoff_sec(3), 30);
        assert_eq!(delivery_retry_backoff_sec(9), 60);
    }
}
