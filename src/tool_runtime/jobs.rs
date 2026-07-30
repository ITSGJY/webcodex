use serde_json::{json, Value};
use std::path::Path;

use super::helpers::{
    command_rejected_message, is_safe_job_id, normalize_local_status, project_relative_agent_cwd,
    project_relative_cwd, resolve_agent_cwd, resolve_local_cwd, shell_escape_simple,
};
use super::local_jobs::{
    retain_inspect_job_until_terminal, LocalJobKiller, LocalJobRecord, TerminateOutcome,
    ACTIVE_JOB_STATUSES, ACTIVE_LOCAL_STATUSES,
};
use super::tool_result::ToolResult;
use super::{ExecutionPurpose, ExecutionShell, ToolRuntime};
use crate::auth::AuthContext;
use crate::shell_client::{command_preview, ShellJobStartMetadata, COMMAND_PREVIEW_MAX_CHARS};
use crate::shell_protocol::{ShellJobInfo, ShellJobOpRequest, ShellJobValidationStep};

pub(crate) fn is_blocking_active_job_status(status: &str) -> bool {
    matches!(
        status,
        "queued" | "running" | "started" | "agent_queued" | "recovering"
    )
}

pub(crate) fn is_stop_pending_job_status(status: &str) -> bool {
    status == "stop_requested"
}

pub(crate) fn is_terminal_job_status(status: &str) -> bool {
    matches!(
        status,
        "completed" | "failed" | "stopped" | "lost" | "timeout" | "timed_out" | "cancelled"
    )
}

fn detected_job_summary(
    command_summary: Option<&str>,
    purpose: Option<&str>,
    status: &str,
    exit_code: Option<i64>,
    stdout: &str,
    stderr: &str,
) -> Value {
    let normalized = command_summary
        .unwrap_or_default()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let kind = if normalized.starts_with("cargo test") {
        "test"
    } else if normalized.starts_with("cargo check") {
        "check"
    } else if normalized.starts_with("cargo fmt") {
        "format"
    } else if normalized.starts_with("cargo build") {
        "build"
    } else {
        match purpose {
            Some("other") | None => "operation",
            Some(purpose) => purpose,
        }
    };
    let outcome = if !is_terminal_job_status(status) {
        "in_progress"
    } else if status == "completed" && exit_code == Some(0) {
        "passed"
    } else if matches!(status, "timeout" | "timed_out") {
        "timed_out"
    } else if matches!(status, "stopped" | "cancelled") {
        "cancelled"
    } else {
        "failed"
    };
    let mut detected = json!({
        "kind": kind,
        "outcome": outcome,
    });
    if kind == "test" {
        let combined = format!("{stdout}\n{stderr}");
        let metadata = super::cargo::parse_cargo_test_run_metadata(&combined);
        let (passed, failed) = super::cargo::parse_cargo_test_counts(&combined);
        detected["tests_detected"] = json!(metadata.tests_detected);
        detected["tests_run_count"] = json!(metadata.tests_run_count);
        detected["zero_tests_run"] = json!(metadata.zero_tests_run);
        detected["tests_passed"] = json!(passed);
        detected["tests_failed"] = json!(failed);
    }
    detected
}

fn is_lifecycle_active_status(status: &str) -> bool {
    is_blocking_active_job_status(status) || is_stop_pending_job_status(status)
}

fn add_job_lifecycle_fields(
    output: &mut Value,
    status: &str,
    recovery_state: Option<&str>,
    recovery_reason_code: Option<&str>,
) {
    let blocking_active = is_blocking_active_job_status(status);
    let terminal_pending = is_stop_pending_job_status(status);
    output["active"] = json!(blocking_active || terminal_pending);
    output["blocking_active"] = json!(blocking_active);
    output["terminal"] = json!(is_terminal_job_status(status));
    output["terminal_pending"] = json!(terminal_pending);
    if let Some(text) = recovery_reason_text(recovery_state, recovery_reason_code) {
        output["recovery_reason"] = json!(text);
    }
}

/// Map the bounded `recovery_state` / `recovery_reason_code` pair to a stable,
/// human-readable `recovery_reason` string for the Console/API projection.
///
/// The text is derived only from the bounded reason codes and the recovery
/// state — never from raw backend error strings, tokens, command payloads,
/// environment, filesystem paths, transport connection ids, raw inventory, or
/// internal notifier/request-channel state. Unknown reason codes fall back to
/// a generic form that echoes only the code (safe to surface, not sensitive).
pub(crate) fn recovery_reason_text(
    recovery_state: Option<&str>,
    recovery_reason_code: Option<&str>,
) -> Option<String> {
    match (recovery_state, recovery_reason_code) {
        (Some("recovering"), _) => {
            Some("server is waiting for the same runner instance to reconnect".to_string())
        }
        (Some("reconciled"), _) => Some("reconciled after runner reconnect".to_string()),
        (Some("lost_after_reconcile"), Some(code)) => Some(match code {
            "runner_recovery_deadline_exceeded" => {
                "lost: runner did not reconnect before the recovery deadline".to_string()
            }
            "runner_inventory_missing" => {
                "lost: runner reconnect did not report this job in its inventory".to_string()
            }
            "runner_instance_replaced" => {
                "lost: runner instance was replaced by a newer process".to_string()
            }
            _ => format!("lost after reconciliation ({code})"),
        }),
        (Some("lost_after_reconcile"), None) => Some("lost after reconciliation".to_string()),
        // Jobs lost without entering the reconciliation path keep their original
        // reason code (e.g. legacy disconnect) and have recovery_state == None.
        (_, Some("legacy_runner_disconnected")) => {
            Some("lost: legacy runner disconnected without reconciliation support".to_string())
        }
        (_, Some("runner_transport_disconnected")) => {
            Some("lost: runner transport disconnected".to_string())
        }
        (_, Some("runner_transport_stale")) => {
            Some("lost: runner transport went stale while the job was running".to_string())
        }
        (_, Some("runner_request_not_dispatched")) => {
            Some("lost: runner did not dispatch the job request".to_string())
        }
        (_, Some(code)) => Some(format!("recovery ({code})")),
        (Some(state), None) => Some(format!("recovery ({state})")),
        (None, None) => None,
    }
}

fn command_preview_truncated(preview: &str) -> bool {
    preview.chars().count() > COMMAND_PREVIEW_MAX_CHARS
}

fn add_command_preview_metadata(output: &mut Value, preview: String) {
    output["command_preview_truncated"] = json!(command_preview_truncated(&preview));
    output["command_preview_max_chars"] = json!(COMMAND_PREVIEW_MAX_CHARS);
    output["command_preview_bounded"] = json!(true);
    output["command_preview"] = Value::String(preview);
}

fn job_id_for_log(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("<unknown>")
        .to_string()
}

fn local_read_trim(record: &LocalJobRecord, name: &str) -> Option<String> {
    record
        .read_text(name)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn local_read_lines(
    record: &LocalJobRecord,
    name: &str,
    offset: Option<usize>,
    tail_lines: Option<usize>,
) -> (String, usize, usize, bool) {
    record.read_log_lines(name, offset, tail_lines)
}

/// Build a bounded job summary `Value` for an agent-known job. Never includes
/// stdout/stderr bodies.
pub(crate) fn agent_job_summary_value(job: &ShellJobInfo) -> Value {
    json!({
        "job_id": job.job_id,
        "kind": job.kind,
        "status": job.status,
        "project": job.project_id,
        "executor": "agent",
        "client_id": job.client_id,
        "created_at": job.created_at,
        "started_at": job.started_at,
        "ended_at": job.ended_at,
        "duration_ms": job.duration_ms,
        "elapsed_secs": job.elapsed_secs,
        "exit_code": job.exit_code,
        "recovery_state": job.recovery_state,
        "recovered_after_server_restart": job.recovered_after_server_restart,
        "reconciled_at": job.reconciled_at,
        "recovery_reason_code": job.recovery_reason_code,
        "last_update_seq": job.last_update_seq,
        "recovery_reason": recovery_reason_text(
            job.recovery_state.as_deref(),
            job.recovery_reason_code.as_deref(),
        ),
    })
}

/// Build a bounded job summary `Value` for a local on-disk job by reading
/// lightweight metadata/status files. Returns `None` when a status filter is
/// set and the job does not match. Never includes stdout/stderr bodies.
pub(crate) fn local_job_summary_value(
    job_id: &str,
    record: &LocalJobRecord,
    status_filter: &Option<String>,
) -> Option<Value> {
    let meta = record.read_json("metadata.json");
    let raw_status = local_read_trim(record, "status").unwrap_or_default();
    let status = normalize_local_status(&raw_status);
    if let Some(filter) = status_filter {
        if &status != filter {
            return None;
        }
    }
    let exit_code = local_read_trim(record, "exit_code").and_then(|v| v.parse::<i32>().ok());
    let created_at = meta
        .get("created_at")
        .and_then(Value::as_i64)
        .unwrap_or_default();
    let started_at = meta.get("started_at").and_then(Value::as_i64);
    let ended_at = local_read_trim(record, "finished_at").and_then(|v| v.parse::<i64>().ok());
    let kind = meta
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("shell")
        .to_string();
    Some(json!({
        "job_id": job_id,
        "kind": kind,
        "status": status,
        "project": record.project,
        "executor": "local",
        "created_at": created_at,
        "started_at": started_at,
        "ended_at": ended_at,
        "exit_code": exit_code,
    }))
}

pub(crate) fn local_job_status(
    job_id: &str,
    record: &LocalJobRecord,
    killer: &dyn LocalJobKiller,
    include_command_preview: bool,
) -> ToolResult {
    // Reclaim overtime jobs before reading status: this persists a terminal
    // `lost` status (and terminates the process group) so callers see a
    // consistent terminal state and we don't leak processes.
    let timeout_note = enforce_local_job_timeout(record, killer);
    let meta = record.read_json("metadata.json");
    let raw_status = local_read_trim(record, "status").unwrap_or_default();
    let status = normalize_local_status(&raw_status);
    let exit_code = local_read_trim(record, "exit_code").and_then(|v| v.parse::<i32>().ok());
    let created_at = meta
        .get("created_at")
        .and_then(Value::as_i64)
        .unwrap_or_default();
    let started_at = meta.get("started_at").and_then(Value::as_i64);
    let finished_at = local_read_trim(record, "finished_at").and_then(|v| v.parse::<i64>().ok());
    let max_runtime_secs = meta.get("max_runtime_secs").and_then(Value::as_i64);
    let elapsed_secs = started_at.map(|started| {
        finished_at
            .unwrap_or_else(|| chrono::Utc::now().timestamp())
            .saturating_sub(started) as u64
    });
    let mut output = json!({
        "job_id": job_id,
        "project": record.project,
        "status": status,
        "exit_code": exit_code,
        "created_at": created_at,
        "started_at": started_at,
        "ended_at": finished_at,
        "elapsed_secs": elapsed_secs,
        "max_runtime_secs": max_runtime_secs,
        "executor": "local",
        "kind": meta.get("kind").cloned().unwrap_or_else(|| Value::String("shell".to_string())),
        "command_preview_included": include_command_preview,
    });
    add_job_lifecycle_fields(&mut output, &status, None, None);
    if let Some(note) = timeout_note {
        output["note"] = Value::String(note);
    }
    if include_command_preview {
        if let Some(command) = meta.get("command").and_then(Value::as_str) {
            add_command_preview_metadata(&mut output, command_preview(command));
        }
    }
    ToolResult::ok(output)
}

pub(crate) fn local_job_log(
    job_id: &str,
    record: &LocalJobRecord,
    killer: &dyn LocalJobKiller,
    offset: Option<usize>,
    tail_lines: Option<usize>,
) -> ToolResult {
    // A log query on an overtime job also reclaims it so the reported status
    // is terminal and the process group is not leaked.
    let timeout_note = enforce_local_job_timeout(record, killer);
    let stdout = local_read_lines(record, "stdout.log", offset, tail_lines);
    let stderr = local_read_lines(record, "stderr.log", offset, tail_lines);
    let raw_status = local_read_trim(record, "status").unwrap_or_default();
    let status = normalize_local_status(&raw_status);
    let exit_code = local_read_trim(record, "exit_code").and_then(|v| v.parse::<i32>().ok());
    let meta = record.read_json("metadata.json");
    let purpose = meta
        .get("purpose")
        .and_then(Value::as_str)
        .unwrap_or("other");
    let command_summary = meta
        .get("command")
        .and_then(Value::as_str)
        .map(command_preview)
        .unwrap_or_default();
    let detected_summary = detected_job_summary(
        Some(&command_summary),
        Some(purpose),
        &status,
        exit_code.map(i64::from),
        &stdout.0,
        &stderr.0,
    );
    let mut output = json!({
        "job_id": job_id,
        "status": status,
        "exit_code": exit_code,
        "stdout_tail": stdout.0,
        "stderr_tail": stderr.0,
        "stdout_lines": stdout.2,
        "stderr_lines": stderr.2,
        "stdout_truncated": stdout.3,
        "stderr_truncated": stderr.3,
        "cursor": {
            "stdout": stdout.1,
            "stderr": stderr.1,
        },
        "executor": "local",
        "cwd": meta.get("cwd").cloned().unwrap_or_else(|| json!(".")),
        "shell": meta.get("shell").cloned().unwrap_or_else(|| json!("bash")),
        "purpose": purpose,
        "command_summary": command_summary,
        "detected_summary": detected_summary,
    });
    if let Some(note) = timeout_note {
        output["note"] = Value::String(note);
    }
    ToolResult::ok(output)
}

/// Resolve the process-group id to signal for a local job. Prefers an explicit
/// `process_group_id` in metadata (written by current spawn code); falls back
/// to the `pid` file, which under `setsid` is equal to the pgid. Returns
/// `None` when neither is recorded (e.g. very old metadata predating pid
/// tracking) — in that case we never guess at a pid to kill.
pub(crate) fn resolve_job_pgid(meta: &Value, record: &LocalJobRecord) -> Option<i64> {
    meta.get("process_group_id")
        .and_then(Value::as_i64)
        .or_else(|| local_read_trim(record, "pid").and_then(|s| s.parse::<i64>().ok()))
}

/// If a local job is still `running` but has exceeded `max_runtime_secs`,
/// terminate its process group and persist a terminal `lost` status. Returns a
/// short human-readable note when a timeout was enforced, or `None` if the job
/// is not running or not over time.
///
/// Safety: the pid/pgid come only from this job's own on-disk files (written by
/// us at spawn time via `setsid`). We never kill based on caller-supplied pids.
/// If no pid/pgid is recorded, we only mark the job `lost` — never guess. Kill
/// failures never panic; a conservative `lost` status is persisted regardless.
pub(crate) fn enforce_local_job_timeout(
    record: &LocalJobRecord,
    killer: &dyn LocalJobKiller,
) -> Option<String> {
    let meta = record.read_json("metadata.json");
    let raw_status = local_read_trim(record, "status").unwrap_or_default();
    if normalize_local_status(&raw_status) != "running" {
        return None;
    }
    let started_at = meta.get("started_at").and_then(Value::as_i64)?;
    let max_runtime_secs = meta.get("max_runtime_secs").and_then(Value::as_i64)?;
    // The wrapper writes `finished_at` before `status`. If it exists, the job
    // just finished (or was already reclaimed) — do not double-reclaim.
    if local_read_trim(record, "finished_at").is_some() {
        return None;
    }
    let now = chrono::Utc::now().timestamp();
    if now.saturating_sub(started_at) <= max_runtime_secs {
        return None;
    }
    // Over time. Reclaim the process group if we recorded one.
    let pgid = resolve_job_pgid(&meta, record);
    let note = match pgid {
        Some(pgid) => {
            let pid = local_read_trim(record, "pid")
                .and_then(|s| s.parse::<i64>().ok())
                .unwrap_or(pgid);
            let outcome = killer.terminate_group(pid, pgid);
            match outcome {
                TerminateOutcome::Terminated {
                    pgid,
                    escalated_to_kill,
                } => {
                    let sig = if escalated_to_kill {
                        "SIGKILL"
                    } else {
                        "SIGTERM"
                    };
                    format!(
                        "timed out after {}s; process group {} terminated ({})",
                        max_runtime_secs, pgid, sig
                    )
                }
                TerminateOutcome::AlreadyGone => format!(
                    "timed out after {}s; process group {} already exited; marked lost",
                    max_runtime_secs, pgid
                ),
            }
        }
        None => format!(
            "timed out after {}s; no pid/process_group_id on record; marked lost",
            max_runtime_secs
        ),
    };
    // Persist terminal state so subsequent reads are consistent and we don't
    // repeatedly attempt to kill. The wrapper shell was part of the group and
    // is now gone, so it will not write its own status/finished_at.
    if let Err(e) = std::fs::write(record.dir.join("finished_at"), now.to_string()) {
        tracing::warn!(
            job_id = %job_id_for_log(&record.dir),
            error = %e,
            "failed to write timed-out local job finished_at"
        );
    }
    if let Err(e) = std::fs::write(record.dir.join("status"), "lost") {
        tracing::warn!(
            job_id = %job_id_for_log(&record.dir),
            error = %e,
            "failed to write timed-out local job status"
        );
    }
    Some(note)
}

/// Stop a local job by terminating its process group and persisting a
/// `stopped` status. Only acts on active jobs; terminal jobs are left alone.
/// Like `enforce_local_job_timeout`, the pid/pgid come only from the job's own
/// on-disk files, and missing pid/pgid yields a conservative `stopped` marker
/// without guessing. Kill failures never panic.
pub(crate) fn stop_local_job(
    job_id: &str,
    record: &LocalJobRecord,
    killer: &dyn LocalJobKiller,
) -> ToolResult {
    let meta = record.read_json("metadata.json");
    let raw_status = local_read_trim(record, "status").unwrap_or_default();
    let status = normalize_local_status(&raw_status);
    if !ACTIVE_LOCAL_STATUSES.contains(&status.as_str()) {
        return ToolResult::ok(json!({
            "job_id": job_id,
            "project": record.project,
            "status": status,
            "note": "job already terminal; not stopped again",
        }));
    }
    let now = chrono::Utc::now().timestamp();
    let note = match resolve_job_pgid(&meta, record) {
        Some(pgid) => {
            let pid = local_read_trim(record, "pid")
                .and_then(|s| s.parse::<i64>().ok())
                .unwrap_or(pgid);
            let outcome = killer.terminate_group(pid, pgid);
            match outcome {
                TerminateOutcome::Terminated {
                    pgid,
                    escalated_to_kill,
                } => {
                    let sig = if escalated_to_kill {
                        "SIGKILL"
                    } else {
                        "SIGTERM"
                    };
                    format!("stopped; process group {} terminated ({})", pgid, sig)
                }
                TerminateOutcome::AlreadyGone => {
                    format!("stopped; process group {} already exited", pgid)
                }
            }
        }
        None => "stopped; no pid/process_group_id on record; marked stopped".to_string(),
    };
    if let Err(e) = std::fs::write(record.dir.join("finished_at"), now.to_string()) {
        tracing::warn!(
            job_id,
            error = %e,
            "failed to write stopped local job finished_at"
        );
    }
    if let Err(e) = std::fs::write(record.dir.join("status"), "stopped") {
        tracing::warn!(
            job_id,
            error = %e,
            "failed to write stopped local job status"
        );
    }
    ToolResult::ok(json!({
        "job_id": job_id,
        "project": record.project,
        "status": "stopped",
        "note": note,
    }))
}

fn local_job_status_string(record: &LocalJobRecord) -> String {
    let raw_status = local_read_trim(record, "status").unwrap_or_default();
    normalize_local_status(&raw_status)
}

fn local_job_session_id(record: &LocalJobRecord) -> Option<String> {
    record
        .read_json("metadata.json")
        .get("session_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn confirmation_required_result(project: &str, job_id: &str) -> ToolResult {
    ToolResult::err_with_output(
        "confirmation_required: stop_job requires confirm=true".to_string(),
        json!({
            "error_kind": "confirmation_required",
            "failure_kind": "confirmation_required",
            "project": project,
            "job_id": job_id,
            "already_finished": false,
            "already_stop_requested": false,
            "stop_request_accepted": false,
            "target_was_active_at_request": false,
            "terminal": false,
            "terminal_pending": false,
            "final_status": Value::Null,
            "stop_effect": "confirmation_required",
            "command_started": false,
        }),
    )
}

fn job_not_found_result(project: &str, job_id: &str) -> ToolResult {
    ToolResult::err_with_output(
        format!("job_not_found: {}", job_id),
        json!({
            "error_kind": "job_not_found",
            "failure_kind": "job_not_found",
            "project": project,
            "job_id": job_id,
            "already_finished": false,
            "already_stop_requested": false,
            "stop_request_accepted": false,
            "target_was_active_at_request": false,
            "terminal": false,
            "terminal_pending": false,
            "final_status": Value::Null,
            "stop_effect": "not_found",
            "command_started": false,
        }),
    )
}

fn job_project_mismatch_result(
    request_project: &str,
    job_project: &str,
    job_id: &str,
) -> ToolResult {
    ToolResult::err_with_output(
        format!(
            "job_project_mismatch: job {} belongs to project {} but request used {}",
            job_id, job_project, request_project
        ),
        json!({
            "error_kind": "job_project_mismatch",
            "failure_kind": "job_project_mismatch",
            "project": request_project,
            "job_project": job_project,
            "job_id": job_id,
            "already_finished": false,
            "already_stop_requested": false,
            "stop_request_accepted": false,
            "target_was_active_at_request": false,
            "terminal": false,
            "terminal_pending": false,
            "final_status": Value::Null,
            "stop_effect": "forbidden",
            "command_started": false,
        }),
    )
}

fn job_stop_forbidden_result(
    request_project: &str,
    job_id: &str,
    request_session_id: Option<&str>,
    job_session_id: Option<&str>,
) -> ToolResult {
    ToolResult::err_with_output(
        format!(
            "job_stop_forbidden: job {} is bound to a different session",
            job_id
        ),
        json!({
            "error_kind": "job_stop_forbidden",
            "failure_kind": "job_stop_forbidden",
            "project": request_project,
            "job_id": job_id,
            "request_session_id": request_session_id,
            "job_session_id": job_session_id,
            "already_finished": false,
            "already_stop_requested": false,
            "stop_request_accepted": false,
            "target_was_active_at_request": false,
            "terminal": false,
            "terminal_pending": false,
            "final_status": Value::Null,
            "stop_effect": "forbidden",
            "command_started": false,
        }),
    )
}

fn job_session_unknown_warning() -> Value {
    json!({
        "kind": "job_session_unknown",
        "warning_kind": "job_session_unknown",
        "message": "job has no recorded session_id; stop authorized by project boundary only",
    })
}

fn job_recovering_stop_result(project: &str, job: &ShellJobInfo) -> ToolResult {
    ToolResult::err_with_output(
        "runner_unavailable_recovering: the runner must reconcile this job before it can be stopped"
            .to_string(),
        json!({
            "error_kind": "runner_unavailable_recovering",
            "failure_kind": "runner_unavailable_recovering",
            "project": project,
            "job_id": job.job_id,
            "status_before": "recovering",
            "status_after": "recovering",
            "recovery_state": job.recovery_state,
            "recovery_reason_code": job.recovery_reason_code,
            "recovery_reason": recovery_reason_text(
                job.recovery_state.as_deref(),
                job.recovery_reason_code.as_deref(),
            ),
            "already_finished": false,
            "already_stop_requested": false,
            "stop_request_accepted": false,
            "target_was_active_at_request": true,
            "terminal": false,
            "terminal_pending": false,
            "final_status": Value::Null,
            "stop_effect": "runner_unavailable",
            "command_started": false,
        }),
    )
}

fn ownership_basis_for_stop(
    request_project: &str,
    job_id: &str,
    request_session_id: Option<&str>,
    job_session_id: Option<&str>,
) -> Result<(&'static str, Vec<Value>), ToolResult> {
    match job_session_id {
        Some(job_session_id) if Some(job_session_id) == request_session_id => {
            Ok(("project_and_session", Vec::new()))
        }
        Some(job_session_id) => Err(job_stop_forbidden_result(
            request_project,
            job_id,
            request_session_id,
            Some(job_session_id),
        )),
        None => Ok((
            "unknown_session_project_only",
            vec![job_session_unknown_warning()],
        )),
    }
}

fn stop_job_output(
    project: &str,
    job_id: &str,
    status_before: &str,
    status_after: &str,
    stopped: bool,
    already_finished: bool,
    ownership_basis: &str,
    warnings: Vec<Value>,
) -> Value {
    let already_stop_requested = is_stop_pending_job_status(status_before) && !already_finished;
    let terminal = is_terminal_job_status(status_after);
    let terminal_pending = is_stop_pending_job_status(status_after);
    let stop_request_accepted = !already_finished && !already_stop_requested && stopped;
    let stop_effect = if already_finished {
        "already_finished"
    } else if already_stop_requested {
        "already_stop_requested"
    } else if terminal {
        "stopped"
    } else if terminal_pending || stopped {
        "requested"
    } else {
        "requested"
    };
    let mut output = json!({
        "already_finished": already_finished,
        "already_stop_requested": already_stop_requested,
        "stop_request_accepted": stop_request_accepted,
        "target_was_active_at_request": is_lifecycle_active_status(status_before),
        "terminal": terminal,
        "terminal_pending": terminal_pending,
        "final_status": if terminal { json!(status_after) } else { Value::Null },
        "stop_effect": stop_effect,
        "job_id": job_id,
        "project": project,
        "status_before": status_before,
        "status_after": status_after,
        "command_started": false,
        "ownership_basis": ownership_basis,
    });
    if !warnings.is_empty() {
        output["warning_kind"] = warnings
            .first()
            .and_then(|warning| warning.get("warning_kind"))
            .cloned()
            .unwrap_or(Value::Null);
        output["warnings"] = Value::Array(warnings);
    }
    output
}

fn active_job_brief(summary: &Value) -> Value {
    json!({
        "job_id": summary.get("job_id").cloned().unwrap_or(Value::Null),
        "kind": summary.get("kind").cloned().unwrap_or_else(|| json!("shell")),
        "status": summary.get("status").cloned().unwrap_or(Value::Null),
        "project": summary.get("project").cloned().unwrap_or(Value::Null),
        "started_at": summary.get("started_at").cloned().unwrap_or(Value::Null),
        "created_at": summary.get("created_at").cloned().unwrap_or(Value::Null),
        "executor": summary.get("executor").cloned().unwrap_or(Value::Null),
    })
}

pub(crate) fn local_jobs_visible_to_auth(auth: Option<&AuthContext>) -> bool {
    !auth
        .map(|auth| auth.is_lightweight() || auth.is_oauth_shared_key_subject())
        .unwrap_or(false)
}

impl ToolRuntime {
    pub(crate) async fn run_job_for_auth(
        &self,
        project: String,
        command: String,
        session_id: Option<String>,
        timeout_secs: Option<i64>,
        cwd: Option<String>,
        validation_steps: Vec<ShellJobValidationStep>,
        sandbox: Option<String>,
        auth: Option<&AuthContext>,
    ) -> ToolResult {
        self.run_job_for_auth_with_contract(
            project,
            command,
            session_id,
            timeout_secs,
            cwd,
            validation_steps,
            sandbox,
            auth,
            None,
            None,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn run_job_for_auth_with_contract(
        &self,
        project: String,
        command: String,
        session_id: Option<String>,
        timeout_secs: Option<i64>,
        cwd: Option<String>,
        validation_steps: Vec<ShellJobValidationStep>,
        sandbox: Option<String>,
        auth: Option<&AuthContext>,
        purpose: Option<ExecutionPurpose>,
        shell: Option<ExecutionShell>,
    ) -> ToolResult {
        let resolved = match self.resolve_project_input_for_auth(&project, auth).await {
            Ok(resolved) => resolved,
            Err(e) => return ToolResult::err(command_rejected_message(
                e.to_message(),
                "verify the project id with list_projects, then retry with a registered project.",
            )),
        };
        let project_id = resolved.resolved_id.clone();
        let proj = resolved.config;
        let max_runtime = timeout_secs.unwrap_or(3600).clamp(1, 604800);
        let declared_purpose = purpose.unwrap_or_default();
        let command_summary = command_preview(&command);
        if proj.is_agent() {
            let client_id = match proj.agent_client_id() {
                Ok(id) => id.to_string(),
                Err(e) => {
                    return ToolResult::err(command_rejected_message(
                        e,
                        "refresh the agent project registry with list_projects, then retry.",
                    ))
                }
            };
            let effective_cwd = match resolve_agent_cwd(&proj, cwd.as_deref()) {
                Ok(cwd) => cwd,
                Err(error) => {
                    return ToolResult::err(command_rejected_message(
                        error,
                        "choose '.', an existing project-relative cwd, or a path inside the registered project root.",
                    ))
                }
            };
            let resolved_cwd = project_relative_agent_cwd(&proj, &effective_cwd)
                .unwrap_or_else(|_| ".".to_string());
            let actual_shell = shell.map(ExecutionShell::as_str).unwrap_or("configured");
            let dispatched_command = shell
                .map(|shell| {
                    format!(
                        "exec {} -c {}",
                        shell.as_str(),
                        shell_escape_simple(&command)
                    )
                })
                .unwrap_or_else(|| command.clone());
            match self
                .shell_clients
                .start_job_with_metadata_for_auth(
                    ShellJobOpRequest {
                        op: "start".to_string(),
                        client_id: Some(client_id),
                        cwd: Some(effective_cwd),
                        command: Some(dispatched_command),
                        timeout_secs: Some(max_runtime as u64),
                        job_id: None,
                        since_stdout_line: None,
                        since_stderr_line: None,
                        tail_lines: None,
                        limit: None,
                        codex: None,
                    },
                    "tool_runtime".to_string(),
                    ShellJobStartMetadata {
                        project_id: Some(project_id.clone()),
                        session_id: session_id.clone(),
                        project_cwd: Some(resolved_cwd.clone()),
                        purpose: Some(declared_purpose.as_str().to_string()),
                        shell: Some(actual_shell.to_string()),
                        validation_steps,
                        sandbox,
                    },
                    auth,
                )
                .await
            {
                Ok(job) => ToolResult::ok(json!({
                    "job_id": job.job_id,
                    "kind": job.kind,
                    "status": job.status,
                    "project": project_id,
                    "execution_source": "run_job",
                    "purpose": declared_purpose.as_str(),
                    "command_summary": command_summary,
                    "cwd": resolved_cwd,
                    "shell": actual_shell,
                    "executor": "agent",
                    "execution_state": "started",
                    "created_at": job.created_at,
                    "stdout_tail": "",
                    "stderr_tail": "",
                    "stdout_lines": 0,
                    "stderr_lines": 0,
                    "stdout_truncated": false,
                    "stderr_truncated": false,
                })),
                Err(e) => ToolResult::err(command_rejected_message(
                    e,
                    "confirm the agent is connected and async jobs are allowed, then retry or use run_shell for short commands.",
                )),
            }
        } else {
            if !validation_steps.is_empty() {
                return ToolResult::err(
                    "structured validation jobs require an agent-backed project".to_string(),
                );
            }
            let root = proj.root();
            let cwd_path = match resolve_local_cwd(&proj, cwd.as_deref()) {
                Ok(path) => path,
                Err(error) => {
                    return ToolResult::err(command_rejected_message(
                        error,
                        "choose '.', an existing project-relative cwd, or a path inside the project root.",
                    ))
                }
            };
            let resolved_cwd =
                project_relative_cwd(&proj, &cwd_path).unwrap_or_else(|_| ".".to_string());
            // Preserve the existing local async-job command language (bash)
            // when omitted; explicit sh/bash selects the requested language.
            let actual_shell = shell.map(ExecutionShell::as_str).unwrap_or("bash");
            let job_id = uuid::Uuid::new_v4().to_string();
            let inspect_scratch = match sandbox.as_deref() {
                None => None,
                Some(crate::command_sandbox::INSPECT_SANDBOX_MODE) => {
                    match crate::command_sandbox::InspectScratch::create() {
                        Ok(scratch) => Some(scratch),
                        Err(error) => {
                            return ToolResult::err(format!("inspect sandbox unavailable: {error}"))
                        }
                    }
                }
                Some(other) => return ToolResult::err(format!("unknown sandbox mode '{other}'")),
            };
            let dir = inspect_scratch
                .as_ref()
                .map(|scratch| scratch.path().join("job"))
                .unwrap_or_else(|| root.join(format!(".codex/jobs/{}", job_id)));
            if let Err(e) = std::fs::create_dir_all(&dir) {
                return ToolResult::err(format!("Failed to create job dir: {}", e));
            }
            let now = chrono::Utc::now().timestamp();
            let mut meta = json!({
                "job_id": job_id,
                "project": project_id.clone(),
                "command": command,
                "status": "running",
                "created_at": now,
                "started_at": now,
                "max_runtime_secs": max_runtime,
                "executor": "local",
                "path": proj.path.clone(),
                "kind": "shell",
                "purpose": declared_purpose.as_str(),
                "cwd": resolved_cwd,
                "shell": actual_shell,
            });
            if let Some(session_id) = session_id.as_ref() {
                meta["session_id"] = json!(session_id);
            }
            if let Err(e) = std::fs::write(
                dir.join("metadata.json"),
                serde_json::to_string_pretty(&meta).unwrap_or_default(),
            ) {
                return ToolResult::err(format!("Failed to write metadata: {}", e));
            }
            let cmd_content = format!("#!/usr/bin/env {actual_shell}\n{command}\n");
            if let Err(e) = std::fs::write(dir.join("command.sh"), &cmd_content) {
                return ToolResult::err(format!("Failed to write command.sh: {}", e));
            }
            if let Err(e) = std::fs::write(dir.join("status"), "running") {
                tracing::warn!(
                    job_id = %job_id,
                    error = %e,
                    "failed to write initial local job status"
                );
            }
            let dir_s = dir.to_string_lossy().to_string();
            let wrapper = format!(
                "{1} {0}/command.sh > {0}/stdout.log 2> {0}/stderr.log; code=$?; echo $code > {0}/exit_code; finished=$(date +%s); echo $finished > {0}/finished_at; if [ $code -eq 0 ]; then echo completed > {0}/status; else echo failed > {0}/status; fi",
                shell_escape_simple(&dir_s),
                actual_shell,
            );
            let mut job_command = std::process::Command::new("setsid");
            job_command
                .arg("sh")
                .arg("-c")
                .arg(wrapper)
                .current_dir(&cwd_path)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            if let Some(scratch) = inspect_scratch.as_ref() {
                if let Err(error) =
                    crate::command_sandbox::sandbox_command_inspect(&mut job_command, scratch)
                {
                    return ToolResult::err(format!("inspect sandbox unavailable: {error}"));
                }
            }
            match job_command.spawn() {
                Ok(child) => {
                    // `setsid` makes the child a session + process-group
                    // leader, so child.id() is both the leader pid and the
                    // process-group id. Record the pgid so timeout/stop can
                    // signal the whole subtree (`kill -<pgid>`).
                    let pgid = child.id() as i64;
                    if let Err(e) = std::fs::write(dir.join("pid"), child.id().to_string()) {
                        tracing::warn!(
                            job_id = %job_id,
                            error = %e,
                            "failed to write local job pid"
                        );
                    }
                    meta["process_group_id"] = json!(pgid);
                    if let Err(e) = std::fs::write(
                        dir.join("metadata.json"),
                        serde_json::to_string_pretty(&meta).unwrap_or_default(),
                    ) {
                        tracing::warn!(
                            job_id = %job_id,
                            error = %e,
                            "failed to update local job metadata with process group"
                        );
                    }
                    let record = LocalJobRecord::new(project_id.clone(), dir.clone());
                    let terminal_snapshot = record.terminal_snapshot_handle();
                    self.local_jobs.lock().await.insert(job_id.clone(), record);
                    if let Some(scratch) = inspect_scratch {
                        retain_inspect_job_until_terminal(dir, terminal_snapshot, scratch, child);
                    }
                    ToolResult::ok(json!({
                        "job_id": job_id,
                        "kind": "shell",
                        "status": "running",
                        "project": project_id,
                        "execution_source": "run_job",
                        "purpose": declared_purpose.as_str(),
                        "command_summary": command_summary,
                        "cwd": resolved_cwd,
                        "shell": actual_shell,
                        "executor": "local",
                        "execution_state": "started",
                        "created_at": now,
                        "stdout_tail": "",
                        "stderr_tail": "",
                        "stdout_lines": 0,
                        "stderr_lines": 0,
                        "stdout_truncated": false,
                        "stderr_truncated": false,
                    }))
                }
                Err(e) => ToolResult::err(format!("Failed to spawn job: {}", e)),
            }
        }
    }

    #[allow(dead_code)]
    pub(crate) async fn job_status(&self, job_id: String) -> ToolResult {
        self.job_status_for_auth(job_id, false, None).await
    }

    pub(crate) async fn job_status_for_auth(
        &self,
        job_id: String,
        include_command_preview: bool,
        auth: Option<&AuthContext>,
    ) -> ToolResult {
        let killer = self.job_killer.as_ref();
        if let Some(record) = self.local_jobs.lock().await.get(&job_id).cloned() {
            if !local_jobs_visible_to_auth(auth) {
                return ToolResult::err(format!("unknown job: {}", job_id));
            }
            return local_job_status(&job_id, &record, killer, include_command_preview);
        }
        // Fall through to agent-backed jobs. If the agent registry does not
        // know this job either, attempt local recovery from on-disk metadata
        // so jobs started before a server restart remain queryable.
        if self
            .shell_clients
            .get_job_for_auth(auth, &job_id)
            .await
            .is_err()
        {
            if let Some(record) = self.recover_local_job(&job_id).await {
                if !local_jobs_visible_to_auth(auth) {
                    return ToolResult::err(format!("unknown job: {}", job_id));
                }
                return local_job_status(&job_id, &record, killer, include_command_preview);
            }
            return ToolResult::err(format!("unknown job: {}", job_id));
        }
        match self.shell_clients.get_job_for_auth(auth, &job_id).await {
            Ok(job) => {
                let mut output = json!({
                    "job_id": job.job_id,
                    "project": job.project_id,
                    "status": job.status,
                    "exit_code": job.exit_code,
                    "started_at": job.started_at,
                    "ended_at": job.ended_at,
                    "duration_ms": job.duration_ms,
                    "elapsed_secs": job.elapsed_secs,
                    "client_id": job.client_id,
                    "error": job.error,
                    "recovery_state": job.recovery_state,
                    "recovered_after_server_restart": job.recovered_after_server_restart,
                    "reconciled_at": job.reconciled_at,
                    "recovery_reason_code": job.recovery_reason_code,
                    "last_update_seq": job.last_update_seq,
                    "stdout_retained_from_line": job.stdout_retained_from_line,
                    "stderr_retained_from_line": job.stderr_retained_from_line,
                    "stdout_log_truncated": job.stdout_log_truncated,
                    "stderr_log_truncated": job.stderr_log_truncated,
                    "command_preview_included": include_command_preview,
                });
                let status = output["status"].as_str().unwrap_or_default().to_string();
                add_job_lifecycle_fields(
                    &mut output,
                    &status,
                    job.recovery_state.as_deref(),
                    job.recovery_reason_code.as_deref(),
                );
                if include_command_preview {
                    add_command_preview_metadata(&mut output, job.command_preview);
                }
                ToolResult::ok(output)
            }
            Err(_) => ToolResult::err(format!("unknown job: {}", job_id)),
        }
    }

    #[allow(dead_code)]
    pub(crate) async fn job_log(
        &self,
        job_id: String,
        offset: Option<usize>,
        tail_lines: Option<usize>,
    ) -> ToolResult {
        self.job_log_for_auth(job_id, offset, tail_lines, None)
            .await
    }

    pub(crate) async fn job_log_for_auth(
        &self,
        job_id: String,
        offset: Option<usize>,
        tail_lines: Option<usize>,
        auth: Option<&AuthContext>,
    ) -> ToolResult {
        let tail_lines = if offset.is_none() && tail_lines.is_none() {
            Some(super::helpers::DEFAULT_JOB_LOG_TAIL_LINES)
        } else {
            tail_lines
        };
        let killer = self.job_killer.as_ref();
        if let Some(record) = self.local_jobs.lock().await.get(&job_id).cloned() {
            if !local_jobs_visible_to_auth(auth) {
                return ToolResult::err(format!("unknown job: {}", job_id));
            }
            return local_job_log(&job_id, &record, killer, offset, tail_lines);
        }
        if self
            .shell_clients
            .get_job_for_auth(auth, &job_id)
            .await
            .is_err()
        {
            if let Some(record) = self.recover_local_job(&job_id).await {
                if !local_jobs_visible_to_auth(auth) {
                    return ToolResult::err(format!("unknown job: {}", job_id));
                }
                return local_job_log(&job_id, &record, killer, offset, tail_lines);
            }
            return ToolResult::err(format!("unknown job: {}", job_id));
        }
        match self
            .shell_clients
            .job_log_for_auth(auth, &job_id, offset, None, tail_lines)
            .await
        {
            Ok((job, stdout, stderr, next_stdout_line, next_stderr_line)) => {
                let stdout = stdout.unwrap_or_default();
                let stderr = stderr.unwrap_or_default();
                let stdout_lines = stdout.lines().count();
                let stderr_lines = stderr.lines().count();
                let command_summary = job.command_preview.clone();
                let purpose = job.purpose.clone().unwrap_or_else(|| "other".to_string());
                let detected_summary = detected_job_summary(
                    Some(&command_summary),
                    Some(&purpose),
                    &job.status,
                    job.exit_code.map(i64::from),
                    &stdout,
                    &stderr,
                );
                ToolResult::ok(json!({
                    "job_id": job.job_id,
                    "status": job.status,
                    "exit_code": job.exit_code,
                    "stdout_tail": stdout,
                    "stderr_tail": stderr,
                    "stdout_lines": next_stdout_line.saturating_sub(1),
                    "stderr_lines": next_stderr_line.saturating_sub(1),
                    "stdout_returned_lines": stdout_lines,
                    "stderr_returned_lines": stderr_lines,
                    "stdout_truncated": stdout_lines < next_stdout_line.saturating_sub(1),
                    "stderr_truncated": stderr_lines < next_stderr_line.saturating_sub(1),
                    "stdout_retained_from_line": job.stdout_retained_from_line,
                    "stderr_retained_from_line": job.stderr_retained_from_line,
                    "earlier_stdout_unavailable": job
                        .stdout_retained_from_line
                        .is_some_and(|line| line > 1)
                        || job.stdout_log_truncated,
                    "earlier_stderr_unavailable": job
                        .stderr_retained_from_line
                        .is_some_and(|line| line > 1)
                        || job.stderr_log_truncated,
                    "recovery_state": job.recovery_state,
                    "recovery_reason_code": job.recovery_reason_code,
                    "recovery_reason": recovery_reason_text(
                        job.recovery_state.as_deref(),
                        job.recovery_reason_code.as_deref(),
                    ),
                    "last_update_seq": job.last_update_seq,
                    "cursor": {
                        "stdout": next_stdout_line,
                        "stderr": next_stderr_line,
                    },
                    "executor": "agent",
                    "cwd": job.project_cwd,
                    "shell": job.shell,
                    "purpose": purpose,
                    "command_summary": command_summary,
                    "detected_summary": detected_summary,
                }))
            }
            Err(_) => ToolResult::err(format!("unknown job: {}", job_id)),
        }
    }

    /// `list_jobs`: bounded job summaries across agent and local executors.
    /// Never returns stdout/stderr bodies — only metadata.
    #[allow(dead_code)]
    pub(crate) async fn list_jobs(
        &self,
        limit: Option<usize>,
        status: Option<String>,
    ) -> ToolResult {
        self.list_jobs_for_auth(limit, status, None).await
    }

    pub(crate) async fn list_jobs_for_auth(
        &self,
        limit: Option<usize>,
        status: Option<String>,
        auth: Option<&AuthContext>,
    ) -> ToolResult {
        let max = limit.unwrap_or(20).clamp(1, 100);
        let status_filter = status
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        // Agent jobs come pre-bounded to `max` by the registry. Local jobs are
        // collected fully (the in-memory map is small) so truncation can be
        // detected accurately for the common local-only case.
        let agent_jobs = self.shell_clients.list_jobs_for_auth(auth, Some(max)).await;
        let mut summaries: Vec<Value> = agent_jobs
            .iter()
            .filter(|j| {
                status_filter
                    .as_ref()
                    .map(|s| s == &j.status)
                    .unwrap_or(true)
            })
            .map(agent_job_summary_value)
            .collect();
        let local_records: Vec<(String, LocalJobRecord)> = if local_jobs_visible_to_auth(auth) {
            let local_jobs_map = self.local_jobs.lock().await;
            local_jobs_map
                .iter()
                .map(|(job_id, record)| (job_id.clone(), record.clone()))
                .collect()
        } else {
            Vec::new()
        };
        for (job_id, record) in &local_records {
            if let Some(summary) = local_job_summary_value(job_id, record, &status_filter) {
                summaries.push(summary);
            }
        }
        summaries.sort_by(|a, b| {
            b["created_at"]
                .as_i64()
                .unwrap_or(0)
                .cmp(&a["created_at"].as_i64().unwrap_or(0))
        });
        let truncated = summaries.len() > max;
        summaries.truncate(max);
        ToolResult::ok(json!({
            "jobs": summaries,
            "count": summaries.len(),
            "truncated": truncated,
        }))
    }

    /// `job_tail`: bounded stdout/stderr tails for a job. Reuses the bounded
    /// `job_log` path with a tail-focused default so the console never reads
    /// full logs by default.
    #[allow(dead_code)]
    pub(crate) async fn job_tail(&self, job_id: String, tail_lines: Option<usize>) -> ToolResult {
        self.job_tail_for_auth(job_id, tail_lines, None).await
    }

    pub(crate) async fn job_tail_for_auth(
        &self,
        job_id: String,
        tail_lines: Option<usize>,
        auth: Option<&AuthContext>,
    ) -> ToolResult {
        let tail = tail_lines.unwrap_or(200).clamp(1, 500);
        self.job_log_for_auth(job_id, None, Some(tail), auth).await
    }

    /// Model-facing `stop_job`: requires confirm=true, verifies project/session
    /// ownership, and never exposes stdout/stderr.
    pub(crate) async fn stop_job_model_facing(
        &self,
        project: String,
        job_id: String,
        session_id: Option<String>,
        confirm: bool,
        auth: Option<&AuthContext>,
    ) -> ToolResult {
        if !confirm {
            return confirmation_required_result(&project, &job_id);
        }
        if !is_safe_job_id(&job_id) {
            return job_not_found_result(&project, &job_id);
        }

        let cached = {
            let jobs = self.local_jobs.lock().await;
            jobs.get(&job_id).cloned()
        };
        if let Some(record) = match cached {
            Some(record) => Some(record),
            None => self.recover_local_job(&job_id).await,
        } {
            if !local_jobs_visible_to_auth(auth) {
                return job_not_found_result(&project, &job_id);
            }
            let request_project = self
                .resolve_project_input_for_auth(&project, auth)
                .await
                .map(|resolved| resolved.resolved_id)
                .unwrap_or_else(|_| project.trim().to_string());
            if record.project != request_project {
                return job_project_mismatch_result(&request_project, &record.project, &job_id);
            }
            let job_session_id = local_job_session_id(&record);
            let (ownership_basis, warnings) = match ownership_basis_for_stop(
                &request_project,
                &job_id,
                session_id.as_deref(),
                job_session_id.as_deref(),
            ) {
                Ok(value) => value,
                Err(result) => return result,
            };
            let status_before = local_job_status_string(&record);
            if is_stop_pending_job_status(&status_before) {
                return ToolResult::ok(stop_job_output(
                    &request_project,
                    &job_id,
                    &status_before,
                    &status_before,
                    true,
                    false,
                    ownership_basis,
                    warnings,
                ));
            }
            if !ACTIVE_LOCAL_STATUSES.contains(&status_before.as_str()) {
                return ToolResult::ok(stop_job_output(
                    &request_project,
                    &job_id,
                    &status_before,
                    &status_before,
                    false,
                    true,
                    ownership_basis,
                    warnings,
                ));
            }
            let stop_result = stop_local_job(&job_id, &record, self.job_killer.as_ref());
            if !stop_result.success {
                return stop_result;
            }
            let status_after = stop_result
                .output
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("stopped")
                .to_string();
            return ToolResult::ok(stop_job_output(
                &request_project,
                &job_id,
                &status_before,
                &status_after,
                true,
                false,
                ownership_basis,
                warnings,
            ));
        }

        let resolved = match self.resolve_project_input_for_auth(&project, auth).await {
            Ok(resolved) => resolved,
            Err(err) => return err.into_tool_result(),
        };
        let request_project = resolved.resolved_id;
        let job = match self.shell_clients.get_job_for_auth(auth, &job_id).await {
            Ok(job) => job,
            Err(_) => return job_not_found_result(&request_project, &job_id),
        };
        let Some(job_project) = job
            .project_id
            .as_deref()
            .map(str::trim)
            .filter(|project| !project.is_empty())
        else {
            return job_stop_forbidden_result(
                &request_project,
                &job_id,
                session_id.as_deref(),
                job.session_id.as_deref(),
            );
        };
        if job_project != request_project {
            return job_project_mismatch_result(&request_project, job_project, &job_id);
        }
        let (ownership_basis, warnings) = match ownership_basis_for_stop(
            &request_project,
            &job_id,
            session_id.as_deref(),
            job.session_id.as_deref(),
        ) {
            Ok(value) => value,
            Err(result) => return result,
        };
        let status_before = job.status.clone();
        if status_before == "recovering" {
            return job_recovering_stop_result(&request_project, &job);
        }
        if is_stop_pending_job_status(&status_before) {
            return ToolResult::ok(stop_job_output(
                &request_project,
                &job_id,
                &status_before,
                &status_before,
                true,
                false,
                ownership_basis,
                warnings,
            ));
        }
        if !ACTIVE_JOB_STATUSES.contains(&status_before.as_str()) {
            return ToolResult::ok(stop_job_output(
                &request_project,
                &job_id,
                &status_before,
                &status_before,
                false,
                true,
                ownership_basis,
                warnings,
            ));
        }
        let stopped = match self
            .shell_clients
            .stop_job_for_auth(auth, &job_id, "tool_runtime".to_string())
            .await
        {
            Ok(job) => job,
            Err(error) if error.contains("runner_unavailable_recovering") => {
                let recovering = self
                    .shell_clients
                    .get_job_for_auth(auth, &job_id)
                    .await
                    .unwrap_or(job);
                return job_recovering_stop_result(&request_project, &recovering);
            }
            Err(_) => return job_not_found_result(&request_project, &job_id),
        };
        ToolResult::ok(stop_job_output(
            &request_project,
            &job_id,
            &status_before,
            &stopped.status,
            true,
            false,
            ownership_basis,
            warnings,
        ))
    }

    /// Bounded active job summary for finish/handoff. Never returns stdout,
    /// stderr, tails, command text, or command previews.
    pub(crate) async fn active_jobs_summary(
        &self,
        project: Option<&str>,
        auth: Option<&AuthContext>,
        limit: usize,
    ) -> Value {
        let max = limit.clamp(1, 20);
        let mut active = Vec::new();
        for job in self.shell_clients.list_jobs_for_auth(auth, Some(100)).await {
            if !ACTIVE_JOB_STATUSES.contains(&job.status.as_str()) {
                continue;
            }
            if let Some(project) = project {
                if job.project_id.as_deref() != Some(project) {
                    continue;
                }
            }
            active.push(agent_job_summary_value(&job));
        }

        if local_jobs_visible_to_auth(auth) {
            let local_records: Vec<(String, LocalJobRecord)> = {
                let local_jobs_map = self.local_jobs.lock().await;
                local_jobs_map
                    .iter()
                    .map(|(job_id, record)| (job_id.clone(), record.clone()))
                    .collect()
            };
            for (job_id, record) in local_records {
                if let Some(project) = project {
                    if record.project != project {
                        continue;
                    }
                }
                let status = local_job_status_string(&record);
                if !ACTIVE_JOB_STATUSES.contains(&status.as_str()) {
                    continue;
                }
                if let Some(summary) = local_job_summary_value(&job_id, &record, &None) {
                    active.push(summary);
                }
            }
        }

        active.sort_by(|a, b| {
            b["created_at"]
                .as_i64()
                .unwrap_or(0)
                .cmp(&a["created_at"].as_i64().unwrap_or(0))
        });
        let running_count = active
            .iter()
            .filter(|summary| {
                summary
                    .get("status")
                    .and_then(Value::as_str)
                    .is_some_and(is_blocking_active_job_status)
            })
            .count();
        // `recovering` jobs are a subset of running/blocking-active jobs that the
        // runner must reconcile before their output can be trusted. Counted over
        // the full active vector (not the truncated `recent` list) so the count is
        // reliable regardless of how many recent jobs are surfaced.
        let recovering_count = active
            .iter()
            .filter(|summary| summary.get("status").and_then(Value::as_str) == Some("recovering"))
            .count();
        let stop_requested_count = active
            .iter()
            .filter(|summary| {
                summary
                    .get("status")
                    .and_then(Value::as_str)
                    .is_some_and(is_stop_pending_job_status)
            })
            .count();
        let terminal_pending_count = stop_requested_count;
        let blocking_active_count = running_count;
        let nonblocking_active_count = terminal_pending_count;
        let active_count = blocking_active_count + nonblocking_active_count;
        let recent: Vec<Value> = active.iter().take(max).map(active_job_brief).collect();
        let mut warnings = Vec::new();
        if blocking_active_count > 0 {
            warnings.push(json!({
                "kind": "active_jobs_present",
                "blocking": true,
                "active_count": active_count,
                "blocking_active_count": blocking_active_count,
                "message": format!(
                    "{} blocking active job{} still running",
                    blocking_active_count,
                    if blocking_active_count == 1 { "" } else { "s" }
                ),
            }));
        }
        if terminal_pending_count > 0 {
            warnings.push(json!({
                "kind": "jobs_terminal_pending",
                "blocking": false,
                "stop_requested_count": stop_requested_count,
                "terminal_pending_count": terminal_pending_count,
                "message": format!(
                    "{} job{} stop_requested and waiting for terminal status",
                    terminal_pending_count,
                    if terminal_pending_count == 1 { " is" } else { "s are" }
                ),
            }));
        }
        json!({
            "active_count": active_count,
            "running_count": running_count,
            "recovering_count": recovering_count,
            "stop_requested_count": stop_requested_count,
            "terminal_pending_count": terminal_pending_count,
            "blocking_active_count": blocking_active_count,
            "nonblocking_active_count": nonblocking_active_count,
            "recent": recent,
            "recent_limit": max,
            "truncated": active_count > max,
            "warnings": warnings,
        })
    }

    /// Stop a local job by terminating its process group and marking it
    /// `stopped`.
    ///
    /// This is an internal lifecycle method intended as the implementation
    /// backing a future explicit stop API; it is deliberately **not** exposed
    /// as a GPT Actions / MCP write tool, to avoid surfacing an arbitrary kill
    /// surface to remote callers. Only jobs we created and recorded (in-memory
    /// or recoverable on disk) can be stopped, and the pid/pgid come
    /// exclusively from the job's own on-disk files — never from caller input.
    pub async fn stop_job(&self, job_id: String) -> ToolResult {
        if !is_safe_job_id(&job_id) {
            return ToolResult::err("invalid job id");
        }
        let cached = {
            let jobs = self.local_jobs.lock().await;
            jobs.get(&job_id).cloned()
        };
        let record = match cached {
            Some(r) => r,
            None => match self.recover_local_job(&job_id).await {
                Some(r) => r,
                None => return ToolResult::err(format!("unknown job: {}", job_id)),
            },
        };
        stop_local_job(&job_id, &record, self.job_killer.as_ref())
    }

    /// On-disk local job recovery used to scan server-configured project roots.
    /// The runtime no longer has a server-side project map, so only in-memory
    /// local jobs from the current process can be queried or stopped.
    pub(crate) async fn recover_local_job(&self, job_id: &str) -> Option<LocalJobRecord> {
        if !is_safe_job_id(job_id) {
            return None;
        }
        None
    }
}

#[cfg(test)]
mod recovery_projection_tests {
    use super::recovery_reason_text;
    use serde_json::json;

    #[test]
    fn recovery_reason_text_recovering_explains_wait() {
        let text = recovery_reason_text(Some("recovering"), Some("runner_transport_disconnected"));
        assert_eq!(
            text.as_deref(),
            Some("server is waiting for the same runner instance to reconnect")
        );
        // recovering state is described regardless of the specific reason code.
        let text2 = recovery_reason_text(Some("recovering"), None);
        assert_eq!(
            text2.as_deref(),
            Some("server is waiting for the same runner instance to reconnect")
        );
    }

    #[test]
    fn recovery_reason_text_lost_after_reconcile_codes_are_distinct() {
        let deadline = recovery_reason_text(
            Some("lost_after_reconcile"),
            Some("runner_recovery_deadline_exceeded"),
        );
        let missing = recovery_reason_text(
            Some("lost_after_reconcile"),
            Some("runner_inventory_missing"),
        );
        let replaced = recovery_reason_text(
            Some("lost_after_reconcile"),
            Some("runner_instance_replaced"),
        );
        assert_eq!(
            deadline.as_deref(),
            Some("lost: runner did not reconnect before the recovery deadline")
        );
        assert_eq!(
            missing.as_deref(),
            Some("lost: runner reconnect did not report this job in its inventory")
        );
        assert_eq!(
            replaced.as_deref(),
            Some("lost: runner instance was replaced by a newer process")
        );
        // The three reasons must produce three distinct human strings so the
        // Console can tell them apart.
        let texts = [deadline, missing, replaced]
            .into_iter()
            .filter_map(|opt| opt.as_deref().map(str::to_string))
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(texts.len(), 3, "distinct recoverable loss reasons");
    }

    #[test]
    fn recovery_reason_text_legacy_runner_disconnect() {
        let text = recovery_reason_text(None, Some("legacy_runner_disconnected"));
        assert_eq!(
            text.as_deref(),
            Some("lost: legacy runner disconnected without reconciliation support")
        );
    }

    #[test]
    fn recovery_reason_text_unknown_code_falls_back_safely() {
        let text = recovery_reason_text(Some("lost_after_reconcile"), Some("some_new_code"));
        assert!(
            text.as_deref().unwrap().contains("some_new_code"),
            "unknown code is echoed for debuggability"
        );
        assert!(
            !text.as_deref().unwrap().contains("token"),
            "no sensitive leak"
        );
        let other = recovery_reason_text(None, Some("unknown_reason"));
        assert!(other.as_deref().unwrap().contains("unknown_reason"));
    }

    #[test]
    fn recovery_reason_text_none_when_no_state_or_code() {
        assert_eq!(recovery_reason_text(None, None), None);
    }

    #[test]
    fn agent_job_summary_includes_recovery_reason() {
        use crate::shell_protocol::ShellJobInfo;
        let job = ShellJobInfo {
            job_id: "job-1".to_string(),
            request_id: Some("req-1".to_string()),
            client_id: "oe".to_string(),
            kind: "shell".to_string(),
            project_id: None,
            session_id: None,
            cwd: None,
            project_cwd: None,
            purpose: None,
            shell: None,
            command_preview: String::new(),
            status: "lost".to_string(),
            created_at: 1,
            started_at: Some(2),
            ended_at: Some(3),
            exit_code: None,
            duration_ms: None,
            elapsed_secs: Some(1),
            error: Some("runner did not reconcile".to_string()),
            codex: None,
            result: None,
            validation_progress: None,
            recovery_state: Some("lost_after_reconcile".to_string()),
            recovered_after_server_restart: true,
            reconciled_at: Some(3),
            recovery_reason_code: Some("runner_recovery_deadline_exceeded".to_string()),
            last_update_seq: Some(4),
            stdout_retained_from_line: Some(1),
            stderr_retained_from_line: Some(1),
            stdout_log_truncated: false,
            stderr_log_truncated: false,
        };
        let value = super::agent_job_summary_value(&job);
        assert_eq!(
            value["recovery_reason_code"],
            json!("runner_recovery_deadline_exceeded")
        );
        assert_eq!(
            value["recovery_reason"],
            json!("lost: runner did not reconnect before the recovery deadline")
        );
        // Never expose the raw error string or command payload via the summary.
        assert!(
            value.get("error").is_none(),
            "summary must not surface raw error"
        );
        assert!(
            value.get("command").is_none(),
            "summary must not surface command"
        );
    }
}
