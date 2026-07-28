use super::state::{ShellClientRegistryInner, ShellJobRecord};
use super::{now_ts, CLIENT_ONLINE_WINDOW_SECS, MAX_OUTPUT_BYTES, MAX_QUEUED_REQUESTS_PER_CLIENT};
use crate::shell_protocol::{ShellAgentJobResult, ShellAgentShellJobResult, ShellJobInfo};
use std::collections::VecDeque;
use std::fmt;

pub(crate) const COMMAND_PREVIEW_MAX_CHARS: usize = 120;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PendingRequestEnqueueError {
    UnknownClient { client_id: String },
    ClientOffline { client_id: String },
    QueueFull { client_id: String, limit: usize },
}

impl fmt::Display for PendingRequestEnqueueError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownClient { client_id } => {
                write!(formatter, "unknown shell client: {client_id}")
            }
            Self::ClientOffline { client_id } => write!(
                formatter,
                "shell client {client_id} is offline (no keepalive within \
                 {CLIENT_ONLINE_WINDOW_SECS}s); reconnect the agent before retrying"
            ),
            Self::QueueFull { client_id, limit } => write!(
                formatter,
                "too many pending requests for shell client {client_id} (limit {limit})"
            ),
        }
    }
}

impl From<PendingRequestEnqueueError> for String {
    fn from(error: PendingRequestEnqueueError) -> Self {
        error.to_string()
    }
}

pub(crate) fn command_preview(command: &str) -> String {
    let first_line = command.lines().next().unwrap_or_default().trim();
    if crate::action_audit_sessions::secret_like_value(first_line) {
        "[redacted]".to_string()
    } else if first_line.chars().count() <= COMMAND_PREVIEW_MAX_CHARS {
        first_line.to_string()
    } else {
        let preview = first_line
            .chars()
            .take(COMMAND_PREVIEW_MAX_CHARS)
            .collect::<String>();
        format!("{}…", preview)
    }
}

#[cfg(test)]
mod command_preview_tests {
    use super::*;

    #[test]
    fn command_preview_redacts_secret_like_first_lines() {
        assert_eq!(
            command_preview("curl -H 'Authorization: Bearer example' https://example.invalid"),
            "[redacted]"
        );
        assert_eq!(command_preview("echo token=example"), "[redacted]");
        assert_eq!(command_preview("cargo test focused"), "cargo test focused");
    }
}

#[cfg(test)]
mod select_lines_tests {
    use super::select_lines;

    fn lines(text: &str) -> Vec<String> {
        text.lines().map(str::to_string).collect()
    }

    // A default bounded tail returns only the last `tail_lines`, flags earlier
    // content, and points the cursor one past the last known line.
    #[test]
    fn tail_is_bounded_and_reports_next_cursor() {
        let value = (1..=10)
            .map(|n| format!("l{n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let (text, next, total, has_earlier) = select_lines(Some(&value), None, Some(3));
        assert_eq!(lines(&text.unwrap()), ["l8", "l9", "l10"]);
        assert_eq!(next, 11, "cursor is one past the last line");
        assert_eq!(total, 10);
        assert!(has_earlier, "earlier lines were skipped by the tail bound");
    }

    // Offset-only follow reads never re-emit consumed lines: reading from the
    // returned cursor yields nothing new, so a follower cannot loop on a tail.
    #[test]
    fn offset_follow_does_not_duplicate_consumed_lines() {
        let value = (1..=5)
            .map(|n| format!("l{n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let (first, next, _, has_earlier) = select_lines(Some(&value), Some(1), None);
        assert_eq!(lines(&first.unwrap()), ["l1", "l2", "l3", "l4", "l5"]);
        assert_eq!(next, 6);
        assert!(!has_earlier);
        // Following from the returned cursor returns no already-seen lines.
        let (second, next_again, _, _) = select_lines(Some(&value), Some(next), None);
        assert_eq!(
            second.unwrap(),
            "",
            "cursor past the end yields nothing new"
        );
        assert_eq!(next_again, 6, "cursor stays stable when drained");
        // A mid-stream offset returns only the forward slice.
        let (mid, _, _, mid_earlier) = select_lines(Some(&value), Some(4), None);
        assert_eq!(lines(&mid.unwrap()), ["l4", "l5"]);
        assert!(mid_earlier);
    }

    // When both bounds are supplied the tail wins, but the cursor still points
    // past the end so the next follow read drains rather than repeats the tail.
    #[test]
    fn tail_takes_precedence_but_cursor_still_advances() {
        let value = (1..=8)
            .map(|n| format!("l{n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let (text, next, _, _) = select_lines(Some(&value), Some(2), Some(3));
        assert_eq!(
            lines(&text.unwrap()),
            ["l6", "l7", "l8"],
            "tail_lines bounds the segment even when an offset is passed"
        );
        assert_eq!(next, 9);
        let (drained, _, _, _) = select_lines(Some(&value), Some(next), None);
        assert_eq!(
            drained.unwrap(),
            "",
            "following the cursor does not repeat the tail"
        );
    }
}

pub(super) fn truncate_output(value: Option<String>) -> Option<String> {
    truncate_output_to(value, MAX_OUTPUT_BYTES)
}

pub(super) fn truncate_output_to(value: Option<String>, max_bytes: usize) -> Option<String> {
    value.map(|s| {
        if s.len() <= max_bytes {
            s
        } else {
            let mut start = s.len() - max_bytes;
            while start < s.len() && !s.is_char_boundary(start) {
                start += 1;
            }
            format!(
                "[output truncated to last {} bytes]\n{}",
                max_bytes,
                &s[start..]
            )
        }
    })
}

pub(super) fn job_view(job: &ShellJobRecord) -> ShellJobInfo {
    let now = now_ts();
    let elapsed_secs = if let Some(duration_ms) = job.duration_ms {
        Some(duration_ms / 1000)
    } else {
        job.started_at
            .map(|started_at| job.ended_at.unwrap_or(now).saturating_sub(started_at) as u64)
    };
    let result = if is_final_job_status(&job.status) {
        Some(ShellAgentJobResult {
            shell: Some(ShellAgentShellJobResult {
                cwd: job.cwd.clone(),
                command_preview: job.command_preview.clone(),
                exit_code: job.exit_code,
                duration_ms: job.duration_ms,
                error: job.error.clone(),
            }),
        })
    } else {
        None
    };
    ShellJobInfo {
        job_id: job.job_id.clone(),
        request_id: job.request_id.clone(),
        client_id: job.client_id.clone(),
        kind: job.kind.clone(),
        project_id: job.project_id.clone(),
        session_id: job.session_id.clone(),
        cwd: job.cwd.clone(),
        project_cwd: job.project_cwd.clone(),
        purpose: job.purpose.clone(),
        shell: job.shell.clone(),
        command_preview: job.command_preview.clone(),
        status: job.status.clone(),
        created_at: job.created_at,
        started_at: job.started_at,
        ended_at: job.ended_at,
        exit_code: job.exit_code,
        duration_ms: job.duration_ms,
        elapsed_secs,
        error: job.error.clone(),
        codex: job.codex.clone(),
        result,
        validation_progress: job.validation_progress.clone(),
    }
}

pub(super) fn select_lines(
    value: Option<&String>,
    since_line: Option<usize>,
    tail_lines: Option<usize>,
) -> (Option<String>, usize, usize, bool) {
    let Some(value) = value else {
        return (Some(String::new()), since_line.unwrap_or(1), 0, false);
    };
    let lines = value.lines().collect::<Vec<_>>();
    if let Some(tail) = tail_lines.filter(|n| *n > 0) {
        let start = lines.len().saturating_sub(tail);
        let selected = lines[start..].join("\n");
        let text = if selected.is_empty() {
            selected
        } else {
            format!("{}\n", selected)
        };
        return (Some(text), lines.len() + 1, lines.len(), start > 0);
    }
    let start_line = since_line.unwrap_or(1).max(1);
    let start_idx = start_line.saturating_sub(1).min(lines.len());
    let selected = lines[start_idx..].join("\n");
    let text = if selected.is_empty() {
        selected
    } else {
        format!("{}\n", selected)
    };
    (Some(text), lines.len() + 1, lines.len(), start_idx > 0)
}

pub(super) fn append_limited(target: &mut Option<String>, chunk: Option<String>) {
    let Some(chunk) = chunk else {
        return;
    };
    let target_value = target.get_or_insert_with(String::new);
    target_value.push_str(&chunk);
    if target_value.len() > MAX_OUTPUT_BYTES {
        let mut start = target_value.len() - MAX_OUTPUT_BYTES;
        while start < target_value.len() && !target_value.is_char_boundary(start) {
            start += 1;
        }
        *target_value = format!(
            "[output truncated to last {} bytes]\n{}",
            MAX_OUTPUT_BYTES,
            &target_value[start..]
        );
    }
}

pub(super) fn replace_limited(target: &mut Option<String>, value: Option<String>) {
    let Some(value) = value else {
        return;
    };
    *target = truncate_output(Some(value));
}

pub(super) fn is_final_job_status(status: &str) -> bool {
    matches!(
        status,
        "completed" | "failed" | "stopped" | "timeout" | "timed_out" | "lost" | "cancelled"
    )
}

fn client_is_connected_locked(inner: &ShellClientRegistryInner, client_id: &str) -> bool {
    inner
        .clients
        .get(client_id)
        .map(|client| now_ts().saturating_sub(client.last_seen) <= CLIENT_ONLINE_WINDOW_SECS)
        .unwrap_or(false)
}

pub(super) fn offline_last_seen(now: i64) -> i64 {
    now.saturating_sub(CLIENT_ONLINE_WINDOW_SECS.saturating_add(1))
}

/// Verify that `client_id` exists and that `agent_instance_id` matches the
/// instance that currently holds the lease for it. A stale/replaced instance
/// (e.g. a second process that was rejected, or the previous process after a
/// stale replacement) is rejected so it can no longer poll or submit results.
/// Callers must already hold `inner`.
pub(super) fn assert_active_instance_locked(
    inner: &ShellClientRegistryInner,
    client_id: &str,
    agent_instance_id: &str,
) -> Result<(), String> {
    let Some(client) = inner.clients.get(client_id) else {
        return Err(format!("unknown shell client: {}", client_id));
    };
    if client.agent_instance_id != agent_instance_id {
        return Err(format!(
            "agent client {} is no longer the active instance (stale or replaced)",
            client_id
        ));
    }
    Ok(())
}

/// Reject enqueue when a client's pending queue has reached
/// `MAX_QUEUED_REQUESTS_PER_CLIENT`. Callers must already hold `inner`.
pub(super) fn ensure_queue_capacity_locked(
    inner: &ShellClientRegistryInner,
    client_id: &str,
) -> Result<(), PendingRequestEnqueueError> {
    let len = inner
        .queues_by_client
        .get(client_id)
        .map(VecDeque::len)
        .unwrap_or(0);
    if len >= MAX_QUEUED_REQUESTS_PER_CLIENT {
        return Err(PendingRequestEnqueueError::QueueFull {
            client_id: client_id.to_string(),
            limit: MAX_QUEUED_REQUESTS_PER_CLIENT,
        });
    }
    Ok(())
}

/// Ensure a request target exists and is currently online before enqueueing
/// work for the agent pump. Callers must already hold `inner`.
///
/// Online is defined by `CLIENT_ONLINE_WINDOW_SECS` against `last_seen`. Without
/// this gate, a registered-but-disconnected agent still accepts enqueues that
/// can only fail after the caller's wait timeout (or pile up until
/// `MAX_QUEUED_REQUESTS_PER_CLIENT` and then permanently reject new work for
/// that client until process restart) — a major amplifier of MCP "no reply".
pub(super) fn ensure_dispatch_supported_locked(
    inner: &ShellClientRegistryInner,
    client_id: &str,
) -> Result<(), PendingRequestEnqueueError> {
    if !inner.clients.contains_key(client_id) {
        return Err(PendingRequestEnqueueError::UnknownClient {
            client_id: client_id.to_string(),
        });
    }
    if !client_is_connected_locked(inner, client_id) {
        return Err(PendingRequestEnqueueError::ClientOffline {
            client_id: client_id.to_string(),
        });
    }
    Ok(())
}

pub(super) fn refresh_job_status_locked(inner: &mut ShellClientRegistryInner, job_id: &str) {
    let Some(job) = inner.jobs_by_id.get(job_id) else {
        return;
    };
    if is_final_job_status(&job.status)
        || !matches!(
            job.status.as_str(),
            "agent_queued" | "running" | "stop_requested"
        )
    {
        return;
    }
    let client_id = job.client_id.clone();
    if client_is_connected_locked(inner, &client_id) {
        return;
    }
    if let Some(job) = inner.jobs_by_id.get_mut(job_id) {
        job.status = "lost".to_string();
        job.ended_at = Some(now_ts());
        job.error = Some("shell client went stale while job was running".to_string());
    }
}
