use crate::action_audit::{ActionAudit, ActionAuditRecord};
#[cfg(test)]
use crate::shell_protocol::{
    AgentPolicySummary, ClaudeCodeProviderStatus, ShellAgentJobUpdateRequest,
    ShellAgentPollRequest, ShellAgentProjectSummary, ShellAgentResultRequest,
    ShellClientCapabilities, ShellClientRegisterRequest, ShellClientView, ShellJobCodexMetadata,
    ToolProvidersStatus, SHELL_CLIENT_CAPABILITY_ASYNC_SHELL_JOBS,
    SHELL_CLIENT_CAPABILITY_FILE_READ, SHELL_CLIENT_CAPABILITY_GIT, SHELL_CLIENT_CAPABILITY_NAMES,
    SHELL_CLIENT_CAPABILITY_SHELL,
};
use crate::shell_protocol::{
    ShellClientJobLogRequest, ShellClientJobLogResponse, ShellClientJobStatusRequest,
    ShellClientJobStatusResponse, ShellClientJobStopRequest, ShellClientJobStopResponse,
    ShellClientJobsListRequest, ShellClientJobsListResponse, ShellFileOpRequest,
    ShellFileOpResponse, ShellJobInfo, ShellJobOpRequest, ShellJobOpResponse, ShellRunRequest,
    ShellRunResponse,
};
use salvo::prelude::*;
use serde_json::json;
use std::sync::Arc;
use tokio::sync::Mutex;
#[cfg(test)]
use tokio::sync::Notify;

mod agents;
mod auth;
mod handlers;
mod job_updates;
mod jobs;
mod polling;
mod projects;
mod reconciliation;
#[cfg(test)]
mod reconciliation_tests;
mod requests;
mod state;
mod validation;

#[cfg(test)]
pub(crate) use auth::assert_shell_client_owner;
#[cfg(test)]
pub(crate) use auth::ShellClientAuthGroup;
pub(crate) use auth::{
    effective_register_owner, enforce_agent_transport, enforce_register_owner,
    requested_by_from_auth, require_agent_transport_scope,
};
pub use handlers::{
    shell_agent_job_update, shell_agent_poll, shell_agent_register, shell_agent_result,
};
pub(crate) use job_updates::ShellJobStartMetadata;
pub(crate) use jobs::{command_preview, COMMAND_PREVIEW_MAX_CHARS};
#[cfg(test)]
pub(crate) use projects::ShellClientLookupError;
pub(crate) use requests::EnqueueLspError;
use state::ShellClientRegistryInner;
use validation::sha256_hex;
#[cfg(test)]
use validation::{
    validate_file_request, validate_run_request, MAX_COMMAND_LEN, MAX_RUN_STDIN_BYTES,
};

const MAX_OUTPUT_BYTES: usize = 256 * 1024;
pub(crate) const CLIENT_ONLINE_WINDOW_SECS: i64 = 60;
/// Same-process runners have this long to re-register and submit their
/// complete active inventory before a recovering job becomes terminal lost.
pub(crate) const JOB_RECOVERY_GRACE_SECS: i64 = 120;
const MAX_RETIRED_INSTANCES_PER_CLIENT: usize = 16;
/// Maximum number of pending requests queued for a single agent client.
/// Bounds memory when an agent is slow or disconnected: once a client's
/// queue reaches this depth, new enqueues are rejected with a structured
/// error instead of growing unboundedly. The WebSocket outbound channel
/// (`OUTGOING_CHANNEL_CAPACITY` in `agent_ws.rs`) is smaller than this, so a
/// slow WebSocket agent fills its outbound channel first and the request
/// pump applies natural backpressure; this cap is the hard ceiling that
/// protects the registry when even that backpressure cannot drain (e.g. a
/// dead socket the OS has not yet reported as closed).
const MAX_QUEUED_REQUESTS_PER_CLIENT: usize = 256;

/// Transport label for polling agents (HTTP `/api/shell/agent/poll`).
pub const TRANSPORT_POLLING: &str = "polling";
/// Transport label for agents connected over the WebSocket endpoint.
pub const TRANSPORT_WEBSOCKET: &str = "websocket";
/// Transport label for agents connected over the custom QUIC stream transport.
/// Reported in `ShellClientView.transport` and surfaced by `runtime_status` /
/// `listAgents`. New deployments should generally use `transport = "auto"`
/// with `[quic]` configured so QUIC is attempted before fallback transports.
pub const TRANSPORT_QUIC: &str = "quic";

#[derive(Debug, Default)]
pub struct ShellClientRegistry {
    inner: Mutex<ShellClientRegistryInner>,
}

fn now_ts() -> i64 {
    chrono::Utc::now().timestamp()
}

fn get_registry(depot: &Depot) -> Option<Arc<ShellClientRegistry>> {
    depot.obtain::<Arc<ShellClientRegistry>>().ok().cloned()
}

async fn assert_registry_client_owner(
    registry: &ShellClientRegistry,
    auth: Option<&crate::auth::AuthContext>,
    client_id: &str,
) -> Result<(), (StatusCode, String)> {
    if registry.get_client_view(client_id).await.is_none() {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("unknown shell client: {}", client_id),
        ));
    }
    registry
        .assert_client_access(auth, client_id)
        .await
        .map_err(|e| {
            let status = if e.contains("unknown shell client") {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::FORBIDDEN
            };
            (status, e)
        })
}

fn record_shell_run_action(
    audit: &ActionAudit,
    response: &ShellRunResponse,
    http_status: StatusCode,
) {
    audit.record(
        ActionAuditRecord::new("run", response.success, http_status)
            .error(response.error.clone())
            .ids(json!({"request_id": response.request_id}))
            .summary(json!({
                "client_id": response.client_id,
                "cwd": response.cwd,
                "command_preview": response.command_preview,
                "exit_code": response.exit_code,
                "duration_ms": response.duration_ms,
            })),
    );
}

fn record_shell_file_action(
    audit: &ActionAudit,
    response: &ShellFileOpResponse,
    http_status: StatusCode,
) {
    audit.record(
        ActionAuditRecord::new(response.op.clone(), response.success, http_status)
            .error(response.error.clone())
            .ids(json!({"request_id": response.request_id}))
            .summary(json!({
                "client_id": response.client_id,
                "path": response.path,
                "cwd": response.cwd,
                "bytes": response.bytes,
                "sha256": response.sha256,
                "entries_count": response.entries.len(),
            })),
    );
}

fn record_shell_job_action(
    audit: &ActionAudit,
    response: &ShellJobOpResponse,
    http_status: StatusCode,
) {
    let job_id = response.job.as_ref().map(|job| job.job_id.clone());
    let job_ids = if response.jobs.is_empty() {
        Vec::<String>::new()
    } else {
        response.jobs.iter().map(|job| job.job_id.clone()).collect()
    };
    audit.record(
        ActionAuditRecord::new(response.op.clone(), response.success, http_status)
            .error(response.error.clone())
            .ids(json!({"job_id": job_id, "job_ids": job_ids}))
            .summary(json!({
                "job_status": response.job.as_ref().map(|job| job.status.clone()),
                "client_id": response.job.as_ref().map(|job| job.client_id.clone()),
                "jobs_count": response.jobs.len(),
                "stdout_included": response.stdout.is_some(),
                "stderr_included": response.stderr.is_some(),
            })),
    );
}

fn record_shell_job_status_action(
    audit: &ActionAudit,
    response: &ShellClientJobStatusResponse,
    http_status: StatusCode,
) {
    audit.record(
        ActionAuditRecord::new("shell_job_status", response.success, http_status)
            .error(response.error.clone())
            .ids(json!({
                "job_id": response.job_id,
                "client_id": response.client_id,
            }))
            .summary(json!({
                "kind": response.kind,
                "status": response.status,
                "exit_code": response.exit_code,
                "elapsed_secs": response.elapsed_secs,
            })),
    );
}

fn record_shell_job_log_action(
    audit: &ActionAudit,
    response: &ShellClientJobLogResponse,
    http_status: StatusCode,
) {
    audit.record(
        ActionAuditRecord::new("shell_job_log", response.success, http_status)
            .error(response.error.clone())
            .ids(json!({
                "job_id": response.job_id,
                "client_id": response.client_id,
            }))
            .summary(json!({
                "stdout_included": response.stdout_tail.is_some(),
                "stderr_included": response.stderr_tail.is_some(),
                "next_stdout_line": response.next_stdout_line,
                "next_stderr_line": response.next_stderr_line,
            })),
    );
}

fn record_shell_job_stop_action(
    audit: &ActionAudit,
    response: &ShellClientJobStopResponse,
    http_status: StatusCode,
) {
    audit.record(
        ActionAuditRecord::new("shell_job_stop", response.success, http_status)
            .error(response.error.clone())
            .ids(json!({"job_id": response.job_id}))
            .summary(json!({"status": response.status})),
    );
}

fn record_shell_jobs_list_action(
    audit: &ActionAudit,
    response: &ShellClientJobsListResponse,
    http_status: StatusCode,
) {
    audit.record(
        ActionAuditRecord::new("shell_job_list", response.success, http_status)
            .error(response.error.clone())
            .ids(json!({"client_id": response.client_id}))
            .summary(json!({"jobs_count": response.jobs.len()})),
    );
}

fn render_shell_run(
    res: &mut Response,
    audit: &ActionAudit,
    status: StatusCode,
    response: ShellRunResponse,
) {
    res.status_code(status);
    record_shell_run_action(audit, &response, status);
    res.render(Json(response));
}

fn render_shell_job_status(
    res: &mut Response,
    audit: &ActionAudit,
    status: StatusCode,
    response: ShellClientJobStatusResponse,
) {
    res.status_code(status);
    record_shell_job_status_action(audit, &response, status);
    res.render(Json(response));
}

fn render_shell_job_log(
    res: &mut Response,
    audit: &ActionAudit,
    status: StatusCode,
    response: ShellClientJobLogResponse,
) {
    res.status_code(status);
    record_shell_job_log_action(audit, &response, status);
    res.render(Json(response));
}

fn render_shell_job_stop_response(
    res: &mut Response,
    audit: &ActionAudit,
    status: StatusCode,
    response: ShellClientJobStopResponse,
) {
    res.status_code(status);
    record_shell_job_stop_action(audit, &response, status);
    res.render(Json(response));
}

fn render_shell_jobs_list(
    res: &mut Response,
    audit: &ActionAudit,
    status: StatusCode,
    response: ShellClientJobsListResponse,
) {
    res.status_code(status);
    record_shell_jobs_list_action(audit, &response, status);
    res.render(Json(response));
}

fn render_shell_file(
    res: &mut Response,
    audit: &ActionAudit,
    status: StatusCode,
    response: ShellFileOpResponse,
) {
    res.status_code(status);
    record_shell_file_action(audit, &response, status);
    res.render(Json(response));
}

fn render_shell_job(
    res: &mut Response,
    audit: &ActionAudit,
    status: StatusCode,
    response: ShellJobOpResponse,
) {
    res.status_code(status);
    record_shell_job_action(audit, &response, status);
    res.render(Json(response));
}

#[handler]
pub async fn shell_run(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let audit = ActionAudit::start(req, depot, "/api/shell/run", "runShell");
    let auth = depot.obtain::<crate::auth::AuthContext>().ok().cloned();
    let Some(registry) = get_registry(depot) else {
        render_shell_run(
            res,
            &audit,
            StatusCode::INTERNAL_SERVER_ERROR,
            ShellRunResponse {
                success: false,
                request_id: String::new(),
                client_id: String::new(),
                cwd: None,
                command_preview: String::new(),
                exit_code: None,
                stdout: None,
                stderr: None,
                duration_ms: None,
                error: Some("Shell client registry not configured".to_string()),
            },
        );
        return;
    };
    let body: ShellRunRequest = match req.parse_json().await {
        Ok(body) => body,
        Err(e) => {
            render_shell_run(
                res,
                &audit,
                StatusCode::BAD_REQUEST,
                ShellRunResponse {
                    success: false,
                    request_id: String::new(),
                    client_id: String::new(),
                    cwd: None,
                    command_preview: String::new(),
                    exit_code: None,
                    stdout: None,
                    stderr: None,
                    duration_ms: None,
                    error: Some(format!("Invalid JSON: {}", e)),
                },
            );
            return;
        }
    };
    let wait_timeout_secs = body.wait_timeout_secs;
    let client_id = body.client_id.clone();
    let cwd = body.cwd.clone();
    let preview = command_preview(&body.command);
    if let Err((status, e)) =
        assert_registry_client_owner(&registry, auth.as_ref(), &client_id).await
    {
        render_shell_run(
            res,
            &audit,
            status,
            ShellRunResponse {
                success: false,
                request_id: String::new(),
                client_id,
                cwd,
                command_preview: preview,
                exit_code: None,
                stdout: None,
                stderr: None,
                duration_ms: None,
                error: Some(e),
            },
        );
        return;
    }
    let requested_by = requested_by_from_auth(auth.as_ref());
    let (request_id, rx) = match registry.enqueue_run(body, requested_by).await {
        Ok(result) => result,
        Err(e) => {
            render_shell_run(
                res,
                &audit,
                StatusCode::BAD_REQUEST,
                ShellRunResponse {
                    success: false,
                    request_id: String::new(),
                    client_id,
                    cwd,
                    command_preview: preview,
                    exit_code: None,
                    stdout: None,
                    stderr: None,
                    duration_ms: None,
                    error: Some(e),
                },
            );
            return;
        }
    };
    match tokio::time::timeout(std::time::Duration::from_secs(wait_timeout_secs), rx).await {
        Ok(Ok(response)) => render_shell_run(res, &audit, StatusCode::OK, response),
        Ok(Err(_closed)) => render_shell_run(
            res,
            &audit,
            StatusCode::INTERNAL_SERVER_ERROR,
            ShellRunResponse {
                success: false,
                request_id,
                client_id,
                cwd,
                command_preview: preview,
                exit_code: None,
                stdout: None,
                stderr: None,
                duration_ms: None,
                error: Some("shell request waiter was dropped".to_string()),
            },
        ),
        Err(_elapsed) => {
            registry.cancel_request(&request_id).await;
            render_shell_run(
                res,
                &audit,
                StatusCode::REQUEST_TIMEOUT,
                ShellRunResponse {
                    success: false,
                    request_id,
                    client_id,
                    cwd,
                    command_preview: preview,
                    exit_code: None,
                    stdout: None,
                    stderr: None,
                    duration_ms: None,
                    error: Some(format!(
                        "timed out waiting {} seconds for shell client result",
                        wait_timeout_secs
                    )),
                },
            );
        }
    }
}

fn shell_file_response_from_run(
    op: String,
    path: String,
    cwd: Option<String>,
    request_content: Option<String>,
    response: ShellRunResponse,
) -> ShellFileOpResponse {
    let success = response.error.is_none() && response.exit_code == Some(0);
    let stdout = response.stdout.unwrap_or_default();
    let entries = if op == "list" && success {
        stdout.lines().map(|line| line.to_string()).collect()
    } else {
        Vec::new()
    };
    let content = if op == "read" && success {
        Some(stdout.clone())
    } else {
        None
    };
    let bytes = match op.as_str() {
        "read" => content.as_ref().map(|s| s.len()),
        "write" if success => Some(stdout.trim().parse::<usize>().unwrap_or(0)),
        _ => None,
    };
    let sha256 = match op.as_str() {
        "read" if success => content.as_ref().map(|s| sha256_hex(s)),
        "write" if success => request_content.as_ref().map(|s| sha256_hex(s)),
        _ => None,
    };
    ShellFileOpResponse {
        success,
        op,
        request_id: response.request_id,
        client_id: response.client_id,
        path,
        cwd,
        content,
        entries,
        bytes,
        sha256,
        stderr: response.stderr,
        error: response.error,
    }
}

#[handler]
pub async fn shell_file_op(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let audit = ActionAudit::start(req, depot, "/api/shell/file", "shellFileOp");
    let auth = depot.obtain::<crate::auth::AuthContext>().ok().cloned();
    let Some(registry) = get_registry(depot) else {
        let response = shell_file_error_response(
            "unknown".to_string(),
            String::new(),
            String::new(),
            None,
            "Shell client registry not configured".to_string(),
        );
        render_shell_file(res, &audit, StatusCode::INTERNAL_SERVER_ERROR, response);
        return;
    };
    let body: ShellFileOpRequest = match req.parse_json().await {
        Ok(body) => body,
        Err(e) => {
            let response = shell_file_error_response(
                "unknown".to_string(),
                String::new(),
                String::new(),
                None,
                format!("Invalid JSON: {}", e),
            );
            render_shell_file(res, &audit, StatusCode::BAD_REQUEST, response);
            return;
        }
    };
    let op = body.op.clone();
    let client_id = body.client_id.clone();
    let path = body.path.clone();
    let cwd = body.cwd.clone();
    let request_content = body.content.clone();
    let wait_timeout_secs = body.wait_timeout_secs;
    if let Err((status, e)) =
        assert_registry_client_owner(&registry, auth.as_ref(), &client_id).await
    {
        let response = shell_file_error_response(op, client_id, path, cwd, e);
        render_shell_file(res, &audit, status, response);
        return;
    }
    let requested_by = requested_by_from_auth(auth.as_ref());
    let (request_id, rx) = match registry.enqueue_file_op(body, requested_by).await {
        Ok(result) => result,
        Err(e) => {
            let response = shell_file_error_response(op, client_id, path, cwd, e);
            render_shell_file(res, &audit, StatusCode::BAD_REQUEST, response);
            return;
        }
    };
    match tokio::time::timeout(std::time::Duration::from_secs(wait_timeout_secs), rx).await {
        Ok(Ok(response)) => render_shell_file(
            res,
            &audit,
            StatusCode::OK,
            shell_file_response_from_run(op, path, cwd, request_content, response),
        ),
        Ok(Err(_closed)) => {
            let response = shell_file_error_response(
                op,
                client_id,
                path,
                cwd,
                "shell file request waiter was dropped".to_string(),
            );
            render_shell_file(res, &audit, StatusCode::INTERNAL_SERVER_ERROR, response);
        }
        Err(_elapsed) => {
            registry.cancel_request(&request_id).await;
            let response = shell_file_error_response(
                op,
                client_id,
                path,
                cwd,
                format!(
                    "timed out waiting {} seconds for shell file result",
                    wait_timeout_secs
                ),
            );
            render_shell_file(res, &audit, StatusCode::REQUEST_TIMEOUT, response);
        }
    }
}

fn shell_file_error_response(
    op: String,
    client_id: String,
    path: String,
    cwd: Option<String>,
    error: String,
) -> ShellFileOpResponse {
    ShellFileOpResponse {
        success: false,
        op,
        request_id: String::new(),
        client_id,
        path,
        cwd,
        content: None,
        entries: Vec::new(),
        bytes: None,
        sha256: None,
        stderr: None,
        error: Some(error),
    }
}

fn shell_job_error_response(op: String, error: String) -> ShellJobOpResponse {
    ShellJobOpResponse {
        success: false,
        op,
        job: None,
        jobs: Vec::new(),
        stdout: None,
        stderr: None,
        next_stdout_line: None,
        next_stderr_line: None,
        error: Some(error),
    }
}

fn shell_job_status_response_from_job(job: ShellJobInfo) -> ShellClientJobStatusResponse {
    ShellClientJobStatusResponse {
        success: true,
        job_id: Some(job.job_id.clone()),
        client_id: Some(job.client_id.clone()),
        kind: Some(job.kind.clone()),
        status: Some(job.status.clone()),
        elapsed_secs: job.elapsed_secs,
        exit_code: job.exit_code,
        result: job.result.clone(),
        job: Some(job),
        error: None,
    }
}

fn shell_job_status_error_response(error: String) -> ShellClientJobStatusResponse {
    ShellClientJobStatusResponse {
        success: false,
        job_id: None,
        client_id: None,
        kind: None,
        status: None,
        elapsed_secs: None,
        exit_code: None,
        result: None,
        job: None,
        error: Some(error),
    }
}

fn shell_job_log_error_response(error: String) -> ShellClientJobLogResponse {
    ShellClientJobLogResponse {
        success: false,
        job_id: None,
        client_id: None,
        stdout_tail: None,
        stderr_tail: None,
        next_stdout_line: None,
        next_stderr_line: None,
        job: None,
        error: Some(error),
    }
}

fn shell_job_stop_error_response(error: String) -> ShellClientJobStopResponse {
    ShellClientJobStopResponse {
        success: false,
        job_id: None,
        status: None,
        job: None,
        error: Some(error),
    }
}

fn shell_jobs_list_error_response(client_id: String, error: String) -> ShellClientJobsListResponse {
    ShellClientJobsListResponse {
        success: false,
        client_id,
        jobs: Vec::new(),
        error: Some(error),
    }
}

async fn authorize_job_access(
    registry: &ShellClientRegistry,
    auth: Option<&crate::auth::AuthContext>,
    job_id: &str,
    requested_client_id: Option<&str>,
) -> Result<ShellJobInfo, (StatusCode, String)> {
    let job = registry
        .get_job(job_id)
        .await
        .map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    if let Some(requested_client_id) = requested_client_id {
        if requested_client_id != job.client_id {
            return Err((
                StatusCode::FORBIDDEN,
                format!(
                    "job_id {} belongs to client {}, not {}",
                    job_id, job.client_id, requested_client_id
                ),
            ));
        }
    }
    assert_registry_client_owner(registry, auth, &job.client_id).await?;
    Ok(job)
}

#[handler]
pub async fn shell_job(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let audit = ActionAudit::start(req, depot, "/api/shell/job", "runShellJob");
    let auth = depot.obtain::<crate::auth::AuthContext>().ok().cloned();
    let Some(registry) = get_registry(depot) else {
        render_shell_job(
            res,
            &audit,
            StatusCode::INTERNAL_SERVER_ERROR,
            shell_job_error_response(
                "unknown".to_string(),
                "Shell client registry not configured".to_string(),
            ),
        );
        return;
    };
    let body: ShellJobOpRequest = match req.parse_json().await {
        Ok(body) => body,
        Err(e) => {
            render_shell_job(
                res,
                &audit,
                StatusCode::BAD_REQUEST,
                shell_job_error_response("unknown".to_string(), format!("Invalid JSON: {}", e)),
            );
            return;
        }
    };
    let op = body.op.clone();
    match op.as_str() {
        "start" => {
            let Some(client_id) = body.client_id.as_deref() else {
                render_shell_job(
                    res,
                    &audit,
                    StatusCode::BAD_REQUEST,
                    shell_job_error_response(op, "client_id is required for op=start".to_string()),
                );
                return;
            };
            if let Err((status, e)) =
                assert_registry_client_owner(&registry, auth.as_ref(), client_id).await
            {
                render_shell_job(res, &audit, status, shell_job_error_response(op, e));
                return;
            }
            let requested_by = requested_by_from_auth(auth.as_ref());
            match registry.start_job(body, requested_by).await {
                Ok(job) => render_shell_job(
                    res,
                    &audit,
                    StatusCode::OK,
                    ShellJobOpResponse {
                        success: true,
                        op,
                        job: Some(job),
                        jobs: Vec::new(),
                        stdout: None,
                        stderr: None,
                        next_stdout_line: None,
                        next_stderr_line: None,
                        error: None,
                    },
                ),
                Err(e) => render_shell_job(
                    res,
                    &audit,
                    StatusCode::BAD_REQUEST,
                    shell_job_error_response(op, e),
                ),
            }
        }
        "status" => {
            let Some(job_id) = body.job_id.as_deref() else {
                render_shell_job(
                    res,
                    &audit,
                    StatusCode::BAD_REQUEST,
                    shell_job_error_response(op, "job_id is required for op=status".to_string()),
                );
                return;
            };
            match registry.get_job(job_id).await {
                Ok(job) => {
                    if let Err((status, e)) =
                        assert_registry_client_owner(&registry, auth.as_ref(), &job.client_id).await
                    {
                        render_shell_job(res, &audit, status, shell_job_error_response(op, e));
                        return;
                    }
                    render_shell_job(
                        res,
                        &audit,
                        StatusCode::OK,
                        ShellJobOpResponse {
                            success: true,
                            op,
                            job: Some(job),
                            jobs: Vec::new(),
                            stdout: None,
                            stderr: None,
                            next_stdout_line: None,
                            next_stderr_line: None,
                            error: None,
                        },
                    )
                }
                Err(e) => render_shell_job(
                    res,
                    &audit,
                    StatusCode::BAD_REQUEST,
                    shell_job_error_response(op, e),
                ),
            }
        }
        "list" => {
            let limit = body.limit.unwrap_or(20).clamp(1, 100);
            let mut jobs = Vec::new();
            for job in registry.list_jobs(Some(100)).await {
                if auth.as_ref().map(|auth| auth.is_admin()).unwrap_or(false) {
                    jobs.push(job);
                    continue;
                }
                if registry
                    .assert_client_access(auth.as_ref(), &job.client_id)
                    .await
                    .is_ok()
                {
                    jobs.push(job);
                }
            }
            jobs.truncate(limit);
            render_shell_job(
                res,
                &audit,
                StatusCode::OK,
                ShellJobOpResponse {
                    success: true,
                    op,
                    job: None,
                    jobs,
                    stdout: None,
                    stderr: None,
                    next_stdout_line: None,
                    next_stderr_line: None,
                    error: None,
                },
            );
        }
        "log" => {
            let Some(job_id) = body.job_id.as_deref() else {
                render_shell_job(
                    res,
                    &audit,
                    StatusCode::BAD_REQUEST,
                    shell_job_error_response(op, "job_id is required for op=log".to_string()),
                );
                return;
            };
            let job = match registry.get_job(job_id).await {
                Ok(job) => job,
                Err(e) => {
                    render_shell_job(
                        res,
                        &audit,
                        StatusCode::BAD_REQUEST,
                        shell_job_error_response(op, e),
                    );
                    return;
                }
            };
            if let Err((status, e)) =
                assert_registry_client_owner(&registry, auth.as_ref(), &job.client_id).await
            {
                render_shell_job(res, &audit, status, shell_job_error_response(op, e));
                return;
            }
            match registry
                .job_log(
                    job_id,
                    body.since_stdout_line,
                    body.since_stderr_line,
                    body.tail_lines,
                )
                .await
            {
                Ok((job, stdout, stderr, next_stdout_line, next_stderr_line)) => render_shell_job(
                    res,
                    &audit,
                    StatusCode::OK,
                    ShellJobOpResponse {
                        success: true,
                        op,
                        job: Some(job),
                        jobs: Vec::new(),
                        stdout,
                        stderr,
                        next_stdout_line: Some(next_stdout_line),
                        next_stderr_line: Some(next_stderr_line),
                        error: None,
                    },
                ),
                Err(e) => render_shell_job(
                    res,
                    &audit,
                    StatusCode::BAD_REQUEST,
                    shell_job_error_response(op, e),
                ),
            }
        }
        "stop" => {
            let Some(job_id) = body.job_id.as_deref() else {
                render_shell_job(
                    res,
                    &audit,
                    StatusCode::BAD_REQUEST,
                    shell_job_error_response(op, "job_id is required for op=stop".to_string()),
                );
                return;
            };
            let job = match registry.get_job(job_id).await {
                Ok(job) => job,
                Err(e) => {
                    render_shell_job(
                        res,
                        &audit,
                        StatusCode::BAD_REQUEST,
                        shell_job_error_response(op, e),
                    );
                    return;
                }
            };
            if let Err((status, e)) =
                assert_registry_client_owner(&registry, auth.as_ref(), &job.client_id).await
            {
                render_shell_job(res, &audit, status, shell_job_error_response(op, e));
                return;
            }
            let requested_by = requested_by_from_auth(auth.as_ref());
            match registry.stop_job(job_id, requested_by).await {
                Ok(job) => render_shell_job(
                    res,
                    &audit,
                    StatusCode::OK,
                    ShellJobOpResponse {
                        success: true,
                        op,
                        job: Some(job),
                        jobs: Vec::new(),
                        stdout: None,
                        stderr: None,
                        next_stdout_line: None,
                        next_stderr_line: None,
                        error: None,
                    },
                ),
                Err(e) => render_shell_job(
                    res,
                    &audit,
                    StatusCode::BAD_REQUEST,
                    shell_job_error_response(op, e),
                ),
            }
        }
        _ => render_shell_job(
            res,
            &audit,
            StatusCode::BAD_REQUEST,
            shell_job_error_response(
                op,
                "op must be one of start, status, log, stop, list".to_string(),
            ),
        ),
    }
}

#[handler]
pub async fn shell_job_status(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let audit = ActionAudit::start(
        req,
        depot,
        "/api/shell/jobs/status",
        "getShellClientJobStatus",
    );
    let auth = depot.obtain::<crate::auth::AuthContext>().ok().cloned();
    let Some(registry) = get_registry(depot) else {
        render_shell_job_status(
            res,
            &audit,
            StatusCode::INTERNAL_SERVER_ERROR,
            shell_job_status_error_response("Shell client registry not configured".to_string()),
        );
        return;
    };
    let body: ShellClientJobStatusRequest = match req.parse_json().await {
        Ok(body) => body,
        Err(e) => {
            render_shell_job_status(
                res,
                &audit,
                StatusCode::BAD_REQUEST,
                shell_job_status_error_response(format!("Invalid JSON: {}", e)),
            );
            return;
        }
    };
    match authorize_job_access(
        &registry,
        auth.as_ref(),
        &body.job_id,
        body.client_id.as_deref(),
    )
    .await
    {
        Ok(job) => render_shell_job_status(
            res,
            &audit,
            StatusCode::OK,
            shell_job_status_response_from_job(job),
        ),
        Err((status, e)) => {
            render_shell_job_status(res, &audit, status, shell_job_status_error_response(e))
        }
    }
}

#[handler]
pub async fn shell_job_log(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let audit = ActionAudit::start(req, depot, "/api/shell/jobs/log", "getShellClientJobLog");
    let auth = depot.obtain::<crate::auth::AuthContext>().ok().cloned();
    let Some(registry) = get_registry(depot) else {
        render_shell_job_log(
            res,
            &audit,
            StatusCode::INTERNAL_SERVER_ERROR,
            shell_job_log_error_response("Shell client registry not configured".to_string()),
        );
        return;
    };
    let body: ShellClientJobLogRequest = match req.parse_json().await {
        Ok(body) => body,
        Err(e) => {
            render_shell_job_log(
                res,
                &audit,
                StatusCode::BAD_REQUEST,
                shell_job_log_error_response(format!("Invalid JSON: {}", e)),
            );
            return;
        }
    };
    let job = match authorize_job_access(
        &registry,
        auth.as_ref(),
        &body.job_id,
        body.client_id.as_deref(),
    )
    .await
    {
        Ok(job) => job,
        Err((status, e)) => {
            render_shell_job_log(res, &audit, status, shell_job_log_error_response(e));
            return;
        }
    };
    match registry
        .job_log(
            &body.job_id,
            body.since_stdout_line,
            body.since_stderr_line,
            body.tail_lines,
        )
        .await
    {
        Ok((job, stdout_tail, stderr_tail, next_stdout_line, next_stderr_line)) => {
            render_shell_job_log(
                res,
                &audit,
                StatusCode::OK,
                ShellClientJobLogResponse {
                    success: true,
                    job_id: Some(job.job_id.clone()),
                    client_id: Some(job.client_id.clone()),
                    stdout_tail,
                    stderr_tail,
                    next_stdout_line: Some(next_stdout_line),
                    next_stderr_line: Some(next_stderr_line),
                    job: Some(job),
                    error: None,
                },
            );
        }
        Err(e) => render_shell_job_log(
            res,
            &audit,
            StatusCode::BAD_REQUEST,
            shell_job_log_error_response(e),
        ),
    }
    let _ = job;
}

#[handler]
pub async fn shell_job_stop(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let audit = ActionAudit::start(req, depot, "/api/shell/jobs/stop", "stopShellClientJob");
    let auth = depot.obtain::<crate::auth::AuthContext>().ok().cloned();
    let Some(registry) = get_registry(depot) else {
        render_shell_job_stop_response(
            res,
            &audit,
            StatusCode::INTERNAL_SERVER_ERROR,
            shell_job_stop_error_response("Shell client registry not configured".to_string()),
        );
        return;
    };
    let body: ShellClientJobStopRequest = match req.parse_json().await {
        Ok(body) => body,
        Err(e) => {
            render_shell_job_stop_response(
                res,
                &audit,
                StatusCode::BAD_REQUEST,
                shell_job_stop_error_response(format!("Invalid JSON: {}", e)),
            );
            return;
        }
    };
    if let Err((status, e)) = authorize_job_access(
        &registry,
        auth.as_ref(),
        &body.job_id,
        body.client_id.as_deref(),
    )
    .await
    {
        render_shell_job_stop_response(res, &audit, status, shell_job_stop_error_response(e));
        return;
    }
    let requested_by = requested_by_from_auth(auth.as_ref());
    match registry.stop_job(&body.job_id, requested_by).await {
        Ok(job) => render_shell_job_stop_response(
            res,
            &audit,
            StatusCode::OK,
            ShellClientJobStopResponse {
                success: true,
                job_id: Some(job.job_id.clone()),
                status: Some(job.status.clone()),
                job: Some(job),
                error: None,
            },
        ),
        Err(e) => render_shell_job_stop_response(
            res,
            &audit,
            StatusCode::BAD_REQUEST,
            shell_job_stop_error_response(e),
        ),
    }
}

#[handler]
pub async fn shell_jobs_list(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let audit = ActionAudit::start(req, depot, "/api/shell/jobs/list", "listShellClientJobs");
    let auth = depot.obtain::<crate::auth::AuthContext>().ok().cloned();
    let Some(registry) = get_registry(depot) else {
        render_shell_jobs_list(
            res,
            &audit,
            StatusCode::INTERNAL_SERVER_ERROR,
            shell_jobs_list_error_response(
                String::new(),
                "Shell client registry not configured".to_string(),
            ),
        );
        return;
    };
    let body: ShellClientJobsListRequest = match req.parse_json().await {
        Ok(body) => body,
        Err(e) => {
            render_shell_jobs_list(
                res,
                &audit,
                StatusCode::BAD_REQUEST,
                shell_jobs_list_error_response(String::new(), format!("Invalid JSON: {}", e)),
            );
            return;
        }
    };
    let client_id = body.client_id.clone();
    if let Err((status, e)) =
        assert_registry_client_owner(&registry, auth.as_ref(), &client_id).await
    {
        render_shell_jobs_list(
            res,
            &audit,
            status,
            shell_jobs_list_error_response(client_id, e),
        );
        return;
    }
    match registry
        .list_jobs_for_client(
            &client_id,
            body.status.as_deref(),
            Some(body.limit.unwrap_or(20).clamp(1, 100)),
        )
        .await
    {
        Ok(jobs) => render_shell_jobs_list(
            res,
            &audit,
            StatusCode::OK,
            ShellClientJobsListResponse {
                success: true,
                client_id,
                jobs,
                error: None,
            },
        ),
        Err(e) => render_shell_jobs_list(
            res,
            &audit,
            StatusCode::BAD_REQUEST,
            shell_jobs_list_error_response(client_id, e),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp_bridge::{AgentLspPayload, AgentLspRequest};
    use crate::shell_protocol::AGENT_PROTOCOL_VERSION_QUIC_V1;

    fn auth_context(username: Option<&str>, is_bootstrap: bool) -> crate::auth::AuthContext {
        let (role, scopes) = if is_bootstrap {
            ("admin".to_string(), vec!["admin".to_string()])
        } else {
            ("user".to_string(), Vec::new())
        };
        crate::auth::AuthContext {
            kind: if is_bootstrap {
                crate::auth::AuthKind::Bootstrap
            } else {
                crate::auth::AuthKind::ApiToken
            },
            user_id: username.map(|username| format!("user-{}", username)),
            username: username.map(str::to_string),
            api_key_id: username.map(|username| format!("key-{}", username)),
            api_key_name: username.map(|username| format!("{} key", username)),
            role: Some(role),
            scopes,
            is_bootstrap,
            token_kind: if is_bootstrap {
                None
            } else {
                Some("user".to_string())
            },
            allowed_client_id: None,
            shared_key_hash: None,
            project_grant_id: None,
        }
    }

    /// Phase 3 test helper: build an agent-token AuthContext bound to
    /// `username` and `allowed_client_id`, carrying the given agent scopes.
    fn agent_auth_context(
        username: &str,
        allowed_client_id: &str,
        scopes: Vec<&str>,
    ) -> crate::auth::AuthContext {
        crate::auth::AuthContext {
            kind: crate::auth::AuthKind::AgentToken,
            user_id: Some(format!("user-{}", username)),
            username: Some(username.to_string()),
            api_key_id: Some("key-agent".to_string()),
            api_key_name: Some("agent key".to_string()),
            role: Some("user".to_string()),
            scopes: scopes.into_iter().map(str::to_string).collect(),
            is_bootstrap: false,
            token_kind: Some("agent".to_string()),
            allowed_client_id: Some(allowed_client_id.to_string()),
            shared_key_hash: None,
            project_grant_id: None,
        }
    }

    fn open_auth_context() -> crate::auth::AuthContext {
        crate::auth::shared_key::open_anonymous_context()
    }

    fn oauth_bridge_auth_context(hash: &str, scopes: Vec<&str>) -> crate::auth::AuthContext {
        crate::auth::AuthContext {
            kind: crate::auth::AuthKind::OAuth2Token,
            user_id: None,
            username: None,
            api_key_id: Some("oauth-access-token".to_string()),
            api_key_name: None,
            role: Some("shared-key".to_string()),
            scopes: scopes.into_iter().map(str::to_string).collect(),
            is_bootstrap: false,
            token_kind: Some("oauth2_shared_key".to_string()),
            allowed_client_id: Some("oauth-client".to_string()),
            shared_key_hash: Some(hash.to_string()),
            project_grant_id: None,
        }
    }

    fn managed_oauth_auth_context(
        username: &str,
        shared_key_hash: Option<&str>,
    ) -> crate::auth::AuthContext {
        crate::auth::AuthContext {
            kind: crate::auth::AuthKind::OAuth2Token,
            user_id: Some(format!("user-{}", username)),
            username: Some(username.to_string()),
            api_key_id: Some("oauth-access-token".to_string()),
            api_key_name: None,
            role: Some("user".to_string()),
            scopes: Vec::new(),
            is_bootstrap: false,
            token_kind: Some("oauth2".to_string()),
            allowed_client_id: Some("oauth-client".to_string()),
            shared_key_hash: shared_key_hash.map(str::to_string),
            project_grant_id: None,
        }
    }

    fn project_summary(id: &str, path: &str) -> ShellAgentProjectSummary {
        ShellAgentProjectSummary {
            id: id.to_string(),
            name: Some(id.to_string()),
            path: path.to_string(),
            allow_patch: true,
            kind: Some("rust".to_string()),
            description: Some("test project".to_string()),
            hooks: vec!["doctor".to_string(), "precommit".to_string()],
            disabled: false,
            revision: None,
            git_branch: Some("codex".to_string()),
            git_head: Some("9a7d3ce".to_string()),
            git_dirty: Some(false),
            updated_at: 123456,
            shell_profile: None,
        }
    }

    fn async_job_capabilities() -> ShellClientCapabilities {
        let mut capabilities = ShellClientCapabilities::default();
        capabilities.async_jobs = true;
        capabilities.async_shell_jobs = true;
        capabilities.jobs = true;
        capabilities
    }

    fn file_request(op: &str) -> ShellFileOpRequest {
        ShellFileOpRequest {
            op: op.to_string(),
            client_id: "oe".to_string(),
            path: "src/auth/scopes.rs".to_string(),
            cwd: Some("/root/git/webcodex".to_string()),
            content: None,
            max_bytes: None,
            old_text: None,
            pattern: None,
            expected_sha256: None,
            expected_prefix: None,
            start_line: None,
            end_line: None,
            line: None,
            create_dirs: false,
            wait_timeout_secs: 0,
        }
    }

    #[test]
    fn validate_file_request_allows_read_with_start_and_end_line() {
        let mut req = file_request("read");
        req.start_line = Some(10);
        req.end_line = Some(20);

        validate_file_request(&req).unwrap();
    }

    #[test]
    fn validate_file_request_rejects_invalid_read_requests() {
        let cases: Vec<(&str, fn(&mut ShellFileOpRequest), &str)> = vec![
            (
                "only start_line",
                |req| req.start_line = Some(10),
                "end_line is required when start_line is set for op=read",
            ),
            (
                "only end_line",
                |req| req.end_line = Some(20),
                "start_line is required when end_line is set for op=read",
            ),
            (
                "inverted line range",
                |req| {
                    req.start_line = Some(20);
                    req.end_line = Some(10);
                },
                "invalid line range",
            ),
            (
                "zero start_line",
                |req| {
                    req.start_line = Some(0);
                    req.end_line = Some(10);
                },
                "invalid line range",
            ),
            (
                "line field on read",
                |req| req.line = Some(10),
                "line is only allowed for op=insert_at_line",
            ),
            (
                "expected_prefix on read",
                |req| req.expected_prefix = Some("pub fn".to_string()),
                "expected_prefix is only allowed for line edit ops",
            ),
        ];

        for (case, mutate, expected) in cases {
            let mut req = file_request("read");
            mutate(&mut req);
            let err = validate_file_request(&req).unwrap_err();
            assert_eq!(err, expected, "case: {case}");
        }
    }

    #[test]
    fn validate_file_request_allows_structured_edit_payload_ops() {
        for op in ["replace_in_file", "write_project_file"] {
            let mut req = file_request(op);
            req.content = Some(r#"{"path":"src/lib.rs"}"#.to_string());

            validate_file_request(&req).unwrap();
        }
    }

    #[test]
    fn validate_file_request_rejects_structured_edit_extra_fields() {
        let mut req = file_request("write_project_file");
        req.content = Some(r#"{"path":"src/lib.rs"}"#.to_string());
        req.expected_sha256 =
            Some("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".to_string());

        let err = validate_file_request(&req).unwrap_err();
        assert!(err.contains("expected_sha256 is only allowed"), "{err}");
    }

    #[tokio::test]
    async fn registry_filters_lightweight_clients_by_auth_group() {
        let registry = ShellClientRegistry::default();
        let shared_a = crate::auth::shared_key::shared_key_context("token-a");
        let shared_b = crate::auth::shared_key::shared_key_context("token-b");
        let shared_hash = crate::auth::shared_key::shared_key_hash_of("token-a");
        let bridge_a = oauth_bridge_auth_context(&shared_hash, vec![]);
        let managed_oauth = managed_oauth_auth_context("alice", Some("hash-a"));
        let open = open_auth_context();
        let bootstrap = auth_context(None, true);

        for (client_id, auth) in [
            ("shared-a", &shared_a),
            ("shared-b", &shared_b),
            ("open", &open),
        ] {
            registry
                .register_with_auth(
                    ShellClientRegisterRequest {
                        process_started_at: None,
                        build: None,
                        job_inventory: None,
                        client_id: client_id.to_string(),
                        agent_instance_id: format!("inst-{}", client_id),
                        display_name: None,
                        owner: None,
                        hostname: None,
                        capabilities: Some(async_job_capabilities()),
                        projects: Some(vec![project_summary(client_id, "/tmp/project")]),
                        agent_protocol_version: None,
                        policy: None,
                    },
                    Some(auth),
                )
                .await
                .unwrap();
        }
        registry
            .register(ShellClientRegisterRequest {
                process_started_at: None,
                build: None,
                job_inventory: None,
                client_id: "managed".to_string(),
                agent_instance_id: "inst-managed".to_string(),
                display_name: None,
                owner: Some("alice".to_string()),
                hostname: None,
                capabilities: Some(async_job_capabilities()),
                projects: Some(vec![project_summary("managed", "/tmp/managed")]),
                agent_protocol_version: None,
                policy: None,
            })
            .await
            .unwrap();

        let visible_to_a: Vec<String> = registry
            .list_clients_for_auth(Some(&shared_a))
            .await
            .into_iter()
            .map(|c| c.client_id)
            .collect();
        assert_eq!(visible_to_a, vec!["shared-a"]);
        let visible_to_bridge_a: Vec<String> = registry
            .list_clients_for_auth(Some(&bridge_a))
            .await
            .into_iter()
            .map(|c| c.client_id)
            .collect();
        assert_eq!(visible_to_bridge_a, vec!["shared-a"]);
        assert!(registry
            .assert_client_access(Some(&shared_a), "shared-a")
            .await
            .is_ok());
        assert!(registry
            .assert_client_access(Some(&bridge_a), "shared-a")
            .await
            .is_ok());
        assert!(registry
            .assert_client_access(Some(&shared_a), "shared-b")
            .await
            .unwrap_err()
            .contains("unknown shell client"));
        assert!(registry
            .assert_client_access(Some(&shared_a), "open")
            .await
            .unwrap_err()
            .contains("unknown shell client"));
        assert!(registry
            .assert_client_access(Some(&bridge_a), "shared-b")
            .await
            .unwrap_err()
            .contains("unknown shell client"));
        assert!(registry
            .assert_client_access(Some(&bridge_a), "open")
            .await
            .unwrap_err()
            .contains("unknown shell client"));

        let visible_to_open: Vec<String> = registry
            .list_clients_for_auth(Some(&open))
            .await
            .into_iter()
            .map(|c| c.client_id)
            .collect();
        assert_eq!(visible_to_open, vec!["open"]);
        assert_eq!(
            ShellClientAuthGroup::from_auth(&open),
            Some(ShellClientAuthGroup::OpenAnonymous)
        );
        assert_eq!(
            ShellClientAuthGroup::from_auth(&bridge_a),
            Some(ShellClientAuthGroup::SharedKey(shared_hash))
        );
        assert!(bridge_a.is_oauth_shared_key_subject());
        assert_eq!(ShellClientAuthGroup::from_auth(&managed_oauth), None);
        assert!(!managed_oauth.is_oauth_shared_key_subject());
        let visible_to_managed_oauth: Vec<String> = registry
            .list_clients_for_auth(Some(&managed_oauth))
            .await
            .into_iter()
            .map(|c| c.client_id)
            .collect();
        assert_eq!(visible_to_managed_oauth, vec!["managed"]);
        assert!(registry
            .assert_client_access(Some(&managed_oauth), "managed")
            .await
            .is_ok());
        assert!(registry
            .assert_client_access(Some(&managed_oauth), "shared-a")
            .await
            .unwrap_err()
            .contains("unknown shell client"));

        let visible_to_bootstrap: Vec<String> = registry
            .list_clients_for_auth(Some(&bootstrap))
            .await
            .into_iter()
            .map(|c| c.client_id)
            .collect();
        assert_eq!(
            visible_to_bootstrap,
            vec!["managed", "open", "shared-a", "shared-b"]
        );
    }

    #[tokio::test]
    async fn same_client_id_in_different_project_grants_is_isolated() {
        // Expected pre-fix failure: reusing the same instance id currently
        // lets a second auth group replace the first group's global lease.
        let registry = ShellClientRegistry::default();
        let grant_a =
            crate::auth::shared_key::project_credential_context("wc_pgrant_aaaaaaaaaaaaaaaa");
        let grant_b =
            crate::auth::shared_key::project_credential_context("wc_pgrant_bbbbbbbbbbbbbbbb");
        let registration = |hostname: &str, project: &str| ShellClientRegisterRequest {
            process_started_at: None,
            build: None,
            job_inventory: None,
            client_id: "same-project-agent".to_string(),
            agent_instance_id: "same-instance-id".to_string(),
            display_name: None,
            owner: None,
            hostname: Some(hostname.to_string()),
            capabilities: Some(async_job_capabilities()),
            projects: Some(vec![project_summary(project, "/tmp/project")]),
            agent_protocol_version: None,
            policy: None,
        };
        registry
            .register_with_auth(
                registration("grant-a-host", "grant-a-project"),
                Some(&grant_a),
            )
            .await
            .unwrap();

        let error = registry
            .register_with_auth(
                registration("grant-b-host", "grant-b-project"),
                Some(&grant_b),
            )
            .await
            .unwrap_err();
        assert!(!error.contains("grant-a-host"));
        assert!(!error.contains("grant-a-project"));
        let original = registry
            .get_client_view_for_auth("same-project-agent", Some(&grant_a))
            .await
            .expect("the original grant must retain its lease");
        assert_eq!(original.hostname.as_deref(), Some("grant-a-host"));
        assert!(registry
            .get_client_view_for_auth("same-project-agent", Some(&grant_b))
            .await
            .is_none());
    }

    #[test]
    fn requested_by_from_auth_uses_bootstrap_username_or_anonymous() {
        let bootstrap = auth_context(None, true);
        assert_eq!(requested_by_from_auth(Some(&bootstrap)), "bootstrap");

        let alice = auth_context(Some("alice"), false);
        assert_eq!(requested_by_from_auth(Some(&alice)), "alice");

        assert_eq!(requested_by_from_auth(None), "anonymous");
    }

    #[test]
    fn assert_shell_client_owner_enforces_owner_boundary() {
        let bootstrap = auth_context(None, true);
        assert!(assert_shell_client_owner(Some(&bootstrap), "client-1", None).is_ok());

        let alice = auth_context(Some("alice"), false);
        assert!(assert_shell_client_owner(Some(&alice), "client-1", Some("alice")).is_ok());

        let mismatch =
            assert_shell_client_owner(Some(&alice), "client-1", Some("bob")).unwrap_err();
        assert!(mismatch.contains("owned by bob"));
        assert!(mismatch.contains("belongs to alice"));

        let missing = assert_shell_client_owner(Some(&alice), "client-1", None).unwrap_err();
        assert_eq!(missing, "agent client client-1 has no owner");

        let anonymous = assert_shell_client_owner(None, "client-1", Some("anonymous")).unwrap_err();
        assert!(anonymous.contains("belongs to anonymous"));
    }

    #[tokio::test]
    async fn registry_registers_and_lists_client() {
        let registry = ShellClientRegistry::default();
        registry
            .register(ShellClientRegisterRequest {
                process_started_at: None,
                build: None,
                job_inventory: None,
                client_id: "xrh".to_string(),
                agent_instance_id: "inst".to_string(),
                display_name: Some("XRH".to_string()),
                owner: Some("yyjeqhc".to_string()),
                hostname: Some("fineserver".to_string()),
                capabilities: None,
                projects: None,
                agent_protocol_version: None,
                policy: None,
            })
            .await
            .unwrap();
        let clients = registry.list_clients().await;
        assert_eq!(clients.len(), 1);
        assert_eq!(clients[0].client_id, "xrh");
        assert!(clients[0].connected);
        assert_eq!(clients[0].pending_requests, 0);
    }

    #[tokio::test]
    async fn registry_register_saves_projects() {
        let registry = ShellClientRegistry::default();
        registry
            .register(ShellClientRegisterRequest {
                process_started_at: None,
                build: None,
                job_inventory: None,
                client_id: "oe".to_string(),
                agent_instance_id: "inst".to_string(),
                display_name: None,
                owner: Some("alice".to_string()),
                hostname: None,
                capabilities: None,
                projects: Some(vec![project_summary("webcodex", "/root/git/webcodex")]),
                agent_protocol_version: None,
                policy: None,
            })
            .await
            .unwrap();
        let clients = registry.list_clients().await;
        assert_eq!(clients[0].projects.len(), 1);
        assert_eq!(clients[0].projects[0].id, "webcodex");

        let projects = registry.list_client_projects("oe").await.unwrap();
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].path, "/root/git/webcodex");
    }

    #[tokio::test]
    async fn registry_poll_updates_projects() {
        let registry = ShellClientRegistry::default();
        registry
            .register(ShellClientRegisterRequest {
                process_started_at: None,
                build: None,
                job_inventory: None,
                client_id: "oe".to_string(),
                agent_instance_id: "inst".to_string(),
                display_name: None,
                owner: Some("alice".to_string()),
                hostname: None,
                capabilities: None,
                projects: Some(vec![project_summary("one", "/tmp/one")]),
                agent_protocol_version: None,
                policy: None,
            })
            .await
            .unwrap();
        let polled = registry
            .poll(ShellAgentPollRequest {
                client_id: "oe".to_string(),
                agent_instance_id: "inst".to_string(),
                projects: Some(vec![
                    project_summary("one", "/tmp/one"),
                    project_summary("two", "/tmp/two"),
                ]),
            })
            .await
            .unwrap();
        assert!(polled.is_none());

        let projects = registry.list_client_projects("oe").await.unwrap();
        assert_eq!(projects.len(), 2);
        assert_eq!(projects[0].id, "one");
        assert_eq!(projects[1].id, "two");
    }

    #[tokio::test]
    async fn registry_project_owner_check_enforces_boundary() {
        let registry = ShellClientRegistry::default();
        registry
            .register(ShellClientRegisterRequest {
                process_started_at: None,
                build: None,
                job_inventory: None,
                client_id: "alice-client".to_string(),
                agent_instance_id: "inst".to_string(),
                display_name: None,
                owner: Some("alice".to_string()),
                hostname: None,
                capabilities: None,
                projects: Some(vec![project_summary("webcodex", "/root/git/webcodex")]),
                agent_protocol_version: None,
                policy: None,
            })
            .await
            .unwrap();
        registry
            .register(ShellClientRegisterRequest {
                process_started_at: None,
                build: None,
                job_inventory: None,
                client_id: "bob-client".to_string(),
                agent_instance_id: "inst".to_string(),
                display_name: None,
                owner: Some("bob".to_string()),
                hostname: None,
                capabilities: None,
                projects: Some(vec![project_summary("secret", "/tmp/secret")]),
                agent_protocol_version: None,
                policy: None,
            })
            .await
            .unwrap();

        let alice = auth_context(Some("alice"), false);
        assert!(
            assert_registry_client_owner(&registry, Some(&alice), "alice-client")
                .await
                .is_ok()
        );
        let projects = registry.list_client_projects("alice-client").await.unwrap();
        assert_eq!(projects.len(), 1);

        let mismatch = assert_registry_client_owner(&registry, Some(&alice), "bob-client")
            .await
            .unwrap_err();
        assert_eq!(mismatch.0, StatusCode::FORBIDDEN);
        assert!(mismatch.1.contains("owned by bob"));
    }

    #[test]
    fn protocol_async_capability_defaults_false() {
        let capabilities = ShellClientCapabilities::default();
        assert!(!capabilities.async_jobs);
        assert!(!capabilities.async_shell_jobs);
        assert!(!capabilities.structured_validation_argv);

        let request: ShellClientRegisterRequest = serde_json::from_str(
            r#"{
                "client_id": "oe",
                "agent_instance_id": "inst-1",
                "capabilities": {"shell": true}
            }"#,
        )
        .unwrap();
        let capabilities = request.capabilities.unwrap();
        assert!(!capabilities.async_jobs);
        assert!(!capabilities.async_shell_jobs);
        assert!(!capabilities.structured_validation_argv);
    }

    #[test]
    fn protocol_serde_keeps_old_register_compatible() {
        let request: ShellClientRegisterRequest = serde_json::from_str(
            r#"{
                "client_id": "oe",
                "agent_instance_id": "inst-1",
                "capabilities": {"shell": true, "file_read": true}
            }"#,
        )
        .unwrap();
        assert_eq!(request.client_id, "oe");
        assert!(request.projects.is_none());
        // Old agents omit agent_protocol_version; the field deserializes as None.
        assert!(request.agent_protocol_version.is_none());
    }

    #[test]
    fn protocol_serde_parses_agent_protocol_version() {
        let request: ShellClientRegisterRequest = serde_json::from_str(
            r#"{
                "client_id": "oe",
                "agent_instance_id": "inst-1",
                "agent_protocol_version": "polling-v1"
            }"#,
        )
        .unwrap();
        assert_eq!(
            request.agent_protocol_version.as_deref(),
            Some("polling-v1")
        );
    }

    #[tokio::test]
    async fn register_without_protocol_version_defaults_to_unknown() {
        let registry = ShellClientRegistry::default();
        registry
            .register(ShellClientRegisterRequest {
                process_started_at: None,
                build: None,
                job_inventory: None,
                client_id: "oe".to_string(),
                agent_instance_id: "inst".to_string(),
                display_name: None,
                owner: None,
                hostname: None,
                capabilities: None,
                projects: None,
                agent_protocol_version: None,
                policy: None,
            })
            .await
            .unwrap();
        let clients = registry.list_clients().await;
        assert_eq!(clients[0].agent_protocol_version, "unknown");
    }

    #[tokio::test]
    async fn register_with_protocol_version_is_exposed_in_view() {
        let registry = ShellClientRegistry::default();
        registry
            .register(ShellClientRegisterRequest {
                process_started_at: None,
                build: None,
                job_inventory: None,
                client_id: "xrh".to_string(),
                agent_instance_id: "inst".to_string(),
                display_name: None,
                owner: Some("alice".to_string()),
                hostname: None,
                capabilities: None,
                projects: None,
                agent_protocol_version: Some("polling-v1".to_string()),
                policy: None,
            })
            .await
            .unwrap();
        let clients = registry.list_clients().await;
        assert_eq!(clients.len(), 1);
        assert_eq!(clients[0].client_id, "xrh");
        assert_eq!(clients[0].agent_protocol_version, "polling-v1");
        let view = registry.get_client_view("xrh").await.unwrap();
        assert_eq!(view.agent_protocol_version, "polling-v1");
    }

    #[tokio::test]
    async fn register_blank_protocol_version_falls_back_to_unknown() {
        let registry = ShellClientRegistry::default();
        registry
            .register(ShellClientRegisterRequest {
                process_started_at: None,
                build: None,
                job_inventory: None,
                client_id: "oe".to_string(),
                agent_instance_id: "inst".to_string(),
                display_name: None,
                owner: None,
                hostname: None,
                capabilities: None,
                projects: None,
                agent_protocol_version: Some("   ".to_string()),
                policy: None,
            })
            .await
            .unwrap();
        let clients = registry.list_clients().await;
        assert_eq!(clients[0].agent_protocol_version, "unknown");
    }

    #[tokio::test]
    async fn client_supports_reflects_registered_capabilities() {
        let registry = ShellClientRegistry::default();
        let mut caps = ShellClientCapabilities::default();
        caps.shell = true;
        caps.file_read = true;
        caps.async_shell_jobs = true;
        registry
            .register(ShellClientRegisterRequest {
                process_started_at: None,
                build: None,
                job_inventory: None,
                client_id: "oe".to_string(),
                agent_instance_id: "inst".to_string(),
                display_name: None,
                owner: None,
                hostname: None,
                capabilities: Some(caps),
                projects: None,
                agent_protocol_version: None,
                policy: None,
            })
            .await
            .unwrap();
        assert!(registry
            .client_supports("oe", SHELL_CLIENT_CAPABILITY_SHELL)
            .await
            .unwrap());
        assert!(registry
            .client_supports("oe", SHELL_CLIENT_CAPABILITY_FILE_READ)
            .await
            .unwrap());
        assert!(registry
            .client_supports("oe", SHELL_CLIENT_CAPABILITY_ASYNC_SHELL_JOBS)
            .await
            .unwrap());
        assert!(!registry
            .client_supports("oe", SHELL_CLIENT_CAPABILITY_GIT)
            .await
            .unwrap());
        // Unknown capability name is false, not an error.
        assert!(!registry.client_supports("oe", "teleport").await.unwrap());
        // Unknown client is a structured error.
        let err = registry
            .client_supports("ghost", SHELL_CLIENT_CAPABILITY_SHELL)
            .await
            .unwrap_err();
        assert_eq!(
            err,
            ShellClientLookupError::UnknownClient {
                client_id: "ghost".to_string()
            }
        );
        let err = registry.get_client_capabilities("ghost").await.unwrap_err();
        assert_eq!(
            err,
            ShellClientLookupError::UnknownClient {
                client_id: "ghost".to_string()
            }
        );
    }

    #[tokio::test]
    async fn client_supports_recognizes_all_protocol_capability_names() {
        let registry = ShellClientRegistry::default();
        registry
            .register(ShellClientRegisterRequest {
                process_started_at: None,
                build: None,
                job_inventory: Some(crate::shell_protocol::ShellJobInventory {
                    active_complete: true,
                    jobs: Vec::new(),
                }),
                client_id: "all".to_string(),
                agent_instance_id: "inst".to_string(),
                display_name: None,
                owner: None,
                hostname: None,
                capabilities: Some(ShellClientCapabilities {
                    shell: true,
                    file_read: true,
                    file_write: true,
                    git: true,
                    jobs: true,
                    async_jobs: true,
                    async_shell_jobs: true,
                    structured_validation_argv: true,
                    lsp_read_only_navigation: true,
                    sandbox_inspect_commands: true,
                    project_lifecycle: true,
                    job_state_reconciliation: true,
                }),
                projects: None,
                agent_protocol_version: None,
                policy: None,
            })
            .await
            .unwrap();

        for capability in SHELL_CLIENT_CAPABILITY_NAMES {
            assert!(
                registry.client_supports("all", capability).await.unwrap(),
                "shell client matcher must recognize protocol capability {capability}"
            );
        }
    }

    #[tokio::test]
    async fn registry_enqueues_polls_and_completes_shell_request() {
        let registry = ShellClientRegistry::default();
        registry
            .register(ShellClientRegisterRequest {
                process_started_at: None,
                build: None,
                job_inventory: None,
                client_id: "xrh".to_string(),
                agent_instance_id: "inst".to_string(),
                display_name: None,
                owner: None,
                hostname: None,
                capabilities: None,
                projects: None,
                agent_protocol_version: None,
                policy: None,
            })
            .await
            .unwrap();
        let (request_id, rx) = registry
            .enqueue_run(
                ShellRunRequest {
                    client_id: "xrh".to_string(),
                    cwd: Some("/tmp".to_string()),
                    command: "echo hello".to_string(),
                    stdin: Some("hello stdin".to_string()),
                    timeout_secs: 10,
                    wait_timeout_secs: 1,
                },
                "test".to_string(),
            )
            .await
            .unwrap();
        let polled = registry
            .poll(ShellAgentPollRequest {
                client_id: "xrh".to_string(),
                agent_instance_id: "inst".to_string(),
                projects: None,
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(polled.request_id, request_id);
        assert_eq!(polled.command, "echo hello");
        assert_eq!(polled.stdin.as_deref(), Some("hello stdin"));
        registry
            .complete(ShellAgentResultRequest {
                client_id: "xrh".to_string(),
                agent_instance_id: "inst".to_string(),
                request_id,
                exit_code: Some(0),
                stdout: Some("hello\n".to_string()),
                stderr: Some(String::new()),
                duration_ms: Some(12),
                error: None,
            })
            .await
            .unwrap();
        let response = rx.await.unwrap();
        assert!(response.success);
        assert_eq!(response.stdout.as_deref(), Some("hello\n"));
    }

    #[tokio::test]
    async fn registry_rejects_unknown_client_run() {
        let registry = ShellClientRegistry::default();
        let err = registry
            .enqueue_run(
                ShellRunRequest {
                    client_id: "missing".to_string(),
                    cwd: None,
                    command: "pwd".to_string(),
                    stdin: None,
                    timeout_secs: 10,
                    wait_timeout_secs: 1,
                },
                "test".to_string(),
            )
            .await
            .unwrap_err();
        assert!(err.contains("unknown shell client"));
    }

    fn lsp_status_payload() -> AgentLspPayload {
        AgentLspPayload {
            project_id: "demo".to_string(),
            request: AgentLspRequest::Status,
        }
    }

    async fn register_lsp_test_client(
        registry: &ShellClientRegistry,
        client_id: &str,
        lsp_capable: bool,
    ) {
        registry
            .register(ShellClientRegisterRequest {
                process_started_at: None,
                build: None,
                job_inventory: None,
                client_id: client_id.to_string(),
                agent_instance_id: "inst".to_string(),
                display_name: None,
                owner: None,
                hostname: None,
                capabilities: Some(ShellClientCapabilities {
                    lsp_read_only_navigation: lsp_capable,
                    ..Default::default()
                }),
                projects: None,
                agent_protocol_version: None,
                policy: None,
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn enqueue_lsp_returns_structured_unknown_client_error() {
        let registry = ShellClientRegistry::default();
        let error = registry
            .enqueue_lsp(
                "missing".to_string(),
                lsp_status_payload(),
                "test".to_string(),
                5,
            )
            .await
            .unwrap_err();

        assert_eq!(
            error,
            EnqueueLspError::UnknownClient {
                client_id: "missing".to_string()
            }
        );
        assert_eq!(error.to_string(), "unknown shell client: missing");
    }

    #[tokio::test]
    async fn enqueue_lsp_returns_structured_unsupported_capability_error() {
        let registry = ShellClientRegistry::default();
        register_lsp_test_client(&registry, "legacy", false).await;
        let error = registry
            .enqueue_lsp(
                "legacy".to_string(),
                lsp_status_payload(),
                "test".to_string(),
                5,
            )
            .await
            .unwrap_err();

        assert_eq!(
            error,
            EnqueueLspError::UnsupportedCapability {
                client_id: "legacy".to_string()
            }
        );
        assert_eq!(
            error.to_string(),
            "agent client legacy does not support lsp_read_only_navigation"
        );
    }

    #[tokio::test]
    async fn enqueue_lsp_returns_structured_offline_client_error() {
        let registry = ShellClientRegistry::default();
        register_lsp_test_client(&registry, "stale-lsp", true).await;
        registry
            .set_last_seen_for_test("stale-lsp", now_ts() - CLIENT_ONLINE_WINDOW_SECS - 1)
            .await;
        let error = registry
            .enqueue_lsp(
                "stale-lsp".to_string(),
                lsp_status_payload(),
                "test".to_string(),
                5,
            )
            .await
            .unwrap_err();

        assert_eq!(
            error,
            EnqueueLspError::ClientOffline {
                client_id: "stale-lsp".to_string()
            }
        );
    }

    #[tokio::test]
    async fn enqueue_lsp_returns_structured_queue_full_error() {
        let registry = ShellClientRegistry::default();
        register_lsp_test_client(&registry, "full-lsp", true).await;
        {
            let mut inner = registry.inner.lock().await;
            inner.queues_by_client.insert(
                "full-lsp".to_string(),
                (0..MAX_QUEUED_REQUESTS_PER_CLIENT)
                    .map(|index| format!("queued-{index}"))
                    .collect(),
            );
        }
        let error = registry
            .enqueue_lsp(
                "full-lsp".to_string(),
                lsp_status_payload(),
                "test".to_string(),
                5,
            )
            .await
            .unwrap_err();

        assert_eq!(
            error,
            EnqueueLspError::QueueFull {
                client_id: "full-lsp".to_string(),
                limit: MAX_QUEUED_REQUESTS_PER_CLIENT,
            }
        );
    }

    async fn register_quic_v1_client(registry: &ShellClientRegistry, client_id: &str) {
        registry
            .register(ShellClientRegisterRequest {
                process_started_at: None,
                build: None,
                job_inventory: None,
                client_id: client_id.to_string(),
                agent_instance_id: "inst".to_string(),
                display_name: None,
                owner: Some("alice".to_string()),
                hostname: None,
                capabilities: Some(async_job_capabilities()),
                projects: Some(vec![project_summary("webcodex", "/tmp/webcodex")]),
                agent_protocol_version: Some(AGENT_PROTOCOL_VERSION_QUIC_V1.to_string()),
                policy: None,
            })
            .await
            .unwrap();
        registry
            .set_transport(client_id, TRANSPORT_QUIC)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn registry_allows_quic_v1_run_queueing() {
        let registry = ShellClientRegistry::default();
        register_quic_v1_client(&registry, "quic-run").await;

        let (_request_id, _rx) = registry
            .enqueue_run(
                ShellRunRequest {
                    client_id: "quic-run".to_string(),
                    cwd: None,
                    command: "echo hi".to_string(),
                    stdin: None,
                    timeout_secs: 5,
                    wait_timeout_secs: 0,
                },
                "tester".to_string(),
            )
            .await
            .unwrap();
        let view = registry.get_client_view("quic-run").await.unwrap();
        assert_eq!(view.transport, TRANSPORT_QUIC);
        assert_eq!(view.agent_protocol_version, AGENT_PROTOCOL_VERSION_QUIC_V1);
        assert_eq!(view.pending_requests, 1);
        assert!(view.capabilities.shell);
        assert!(view.capabilities.async_shell_jobs);
    }

    #[tokio::test]
    async fn enqueue_file_op_allows_read_with_line_range() {
        let registry = ShellClientRegistry::default();
        register_quic_v1_client(&registry, "oe").await;

        let mut req = file_request("read");
        req.start_line = Some(7);
        req.end_line = Some(12);
        let (request_id, _rx) = registry
            .enqueue_file_op(req, "tester".to_string())
            .await
            .unwrap();

        let polled = registry
            .poll(ShellAgentPollRequest {
                client_id: "oe".to_string(),
                agent_instance_id: "inst".to_string(),
                projects: None,
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(polled.request_id, request_id);
        assert_eq!(polled.kind, "file_read");
        assert_eq!(polled.path.as_deref(), Some("src/auth/scopes.rs"));
        assert_eq!(polled.start_line, Some(7));
        assert_eq!(polled.end_line, Some(12));
        assert_eq!(polled.line, None);
    }

    #[tokio::test]
    async fn registry_allows_quic_v1_file_and_project_ops_queueing() {
        let registry = ShellClientRegistry::default();
        register_quic_v1_client(&registry, "quic-ops").await;

        let (_file_request_id, _file_rx) = registry
            .enqueue_file_op(
                ShellFileOpRequest {
                    op: "read".to_string(),
                    client_id: "quic-ops".to_string(),
                    path: "README.md".to_string(),
                    cwd: None,
                    content: None,
                    max_bytes: None,
                    old_text: None,
                    pattern: None,
                    expected_sha256: None,
                    expected_prefix: None,
                    start_line: None,
                    end_line: None,
                    line: None,
                    create_dirs: false,
                    wait_timeout_secs: 0,
                },
                "tester".to_string(),
            )
            .await
            .unwrap();

        let (_project_request_id, _project_rx) = registry
            .enqueue_project_op(
                "quic-ops".to_string(),
                "register_project",
                "{}".to_string(),
                "tester".to_string(),
            )
            .await
            .unwrap();

        let view = registry.get_client_view("quic-ops").await.unwrap();
        assert_eq!(view.pending_requests, 2);
    }

    #[tokio::test]
    async fn registry_allows_quic_v1_start_job_queueing() {
        let registry = ShellClientRegistry::default();
        register_quic_v1_client(&registry, "quic-job").await;

        let job = registry
            .start_job(
                ShellJobOpRequest {
                    op: "start".to_string(),
                    client_id: Some("quic-job".to_string()),
                    cwd: None,
                    command: Some("sleep 1".to_string()),
                    timeout_secs: Some(5),
                    job_id: None,
                    since_stdout_line: None,
                    since_stderr_line: None,
                    tail_lines: None,
                    limit: None,
                    codex: None,
                },
                "tester".to_string(),
            )
            .await
            .unwrap();

        let view = registry.get_client_view("quic-job").await.unwrap();
        assert_eq!(view.pending_requests, 1);
        assert_eq!(job.status, "queued");
        assert_eq!(registry.list_jobs(Some(10)).await.len(), 1);
    }

    #[tokio::test]
    async fn registry_allows_quic_v1_stop_job_delivery_queueing() {
        let registry = ShellClientRegistry::default();
        registry
            .register(ShellClientRegisterRequest {
                process_started_at: None,
                build: None,
                job_inventory: None,
                client_id: "quic-stop".to_string(),
                agent_instance_id: "inst".to_string(),
                display_name: None,
                owner: Some("alice".to_string()),
                hostname: None,
                capabilities: Some(async_job_capabilities()),
                projects: None,
                agent_protocol_version: Some(AGENT_PROTOCOL_VERSION_QUIC_V1.to_string()),
                policy: None,
            })
            .await
            .unwrap();
        let job = registry
            .start_job(
                ShellJobOpRequest {
                    op: "start".to_string(),
                    client_id: Some("quic-stop".to_string()),
                    cwd: None,
                    command: Some("sleep 10".to_string()),
                    timeout_secs: Some(10),
                    job_id: None,
                    since_stdout_line: None,
                    since_stderr_line: None,
                    tail_lines: None,
                    limit: None,
                    codex: None,
                },
                "tester".to_string(),
            )
            .await
            .unwrap();
        let _ = registry
            .poll(ShellAgentPollRequest {
                client_id: "quic-stop".to_string(),
                agent_instance_id: "inst".to_string(),
                projects: None,
            })
            .await
            .unwrap()
            .unwrap();
        registry
            .set_transport("quic-stop", TRANSPORT_QUIC)
            .await
            .unwrap();

        let stopped = registry
            .stop_job(&job.job_id, "tester".to_string())
            .await
            .unwrap();
        let view = registry.get_client_view("quic-stop").await.unwrap();
        assert_eq!(view.pending_requests, 1);
        assert_eq!(stopped.status, "stop_requested");
    }

    #[test]
    fn validate_run_request_allows_bounded_stdin_beyond_command_limit() {
        let body = ShellRunRequest {
            client_id: "client-1".to_string(),
            cwd: None,
            command: "cat >/dev/null".to_string(),
            stdin: Some("x".repeat(MAX_COMMAND_LEN + 1024)),
            timeout_secs: 10,
            wait_timeout_secs: 1,
        };
        validate_run_request(&body).expect("stdin has its own larger bound");
    }

    #[test]
    fn validate_run_request_rejects_oversized_stdin() {
        let body = ShellRunRequest {
            client_id: "client-1".to_string(),
            cwd: None,
            command: "cat >/dev/null".to_string(),
            stdin: Some("x".repeat(MAX_RUN_STDIN_BYTES + 1)),
            timeout_secs: 10,
            wait_timeout_secs: 1,
        };
        let err = validate_run_request(&body).unwrap_err();
        assert!(err.contains("stdin is too large"), "got: {}", err);
    }

    #[tokio::test]
    async fn registry_shell_job_start_poll_complete_and_log() {
        let registry = ShellClientRegistry::default();
        registry
            .register(ShellClientRegisterRequest {
                process_started_at: None,
                build: None,
                job_inventory: None,
                client_id: "oe".to_string(),
                agent_instance_id: "inst".to_string(),
                display_name: None,
                owner: None,
                hostname: None,
                capabilities: Some(async_job_capabilities()),
                projects: None,
                agent_protocol_version: None,
                policy: None,
            })
            .await
            .unwrap();
        let job = registry
            .start_job(
                ShellJobOpRequest {
                    op: "start".to_string(),
                    client_id: Some("oe".to_string()),
                    cwd: Some("/tmp".to_string()),
                    command: Some("printf hello".to_string()),
                    timeout_secs: Some(10),
                    job_id: None,
                    since_stdout_line: None,
                    since_stderr_line: None,
                    tail_lines: None,
                    limit: None,
                    codex: Some(ShellJobCodexMetadata {
                        project: Some("demo".to_string()),
                        goal_id: Some("goal-1".to_string()),
                        client_request_id: Some("crid-1".to_string()),
                        command: Some("printf hello".to_string()),
                        kind: Some("command".to_string()),
                        suite: None,
                        script_path: None,
                        reason: Some("test job".to_string()),
                        max_runtime_secs: Some(10),
                    }),
                },
                "test".to_string(),
            )
            .await
            .unwrap();
        assert_eq!(job.status, "queued");
        assert_eq!(
            job.codex
                .as_ref()
                .and_then(|codex| codex.client_request_id.as_deref()),
            Some("crid-1")
        );
        let polled = registry
            .poll(ShellAgentPollRequest {
                client_id: "oe".to_string(),
                agent_instance_id: "inst".to_string(),
                projects: None,
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(polled.command, "printf hello");
        let running = registry.get_job(&job.job_id).await.unwrap();
        assert_eq!(running.status, "agent_queued");
        registry
            .complete(ShellAgentResultRequest {
                client_id: "oe".to_string(),
                agent_instance_id: "inst".to_string(),
                request_id: polled.request_id,
                exit_code: Some(0),
                stdout: Some("hello\n".to_string()),
                stderr: Some(String::new()),
                duration_ms: Some(20),
                error: None,
            })
            .await
            .unwrap();
        let done = registry.get_job(&job.job_id).await.unwrap();
        assert_eq!(done.status, "completed");
        assert_eq!(done.exit_code, Some(0));
        assert_eq!(
            done.codex
                .as_ref()
                .and_then(|codex| codex.project.as_deref()),
            Some("demo")
        );
        let listed = registry.list_jobs(Some(10)).await;
        assert_eq!(
            listed
                .iter()
                .find(|listed| listed.job_id == job.job_id)
                .and_then(|listed| listed.codex.as_ref())
                .and_then(|codex| codex.goal_id.as_deref()),
            Some("goal-1")
        );
        let (_info, stdout, stderr, next_stdout, next_stderr) = registry
            .job_log(&job.job_id, Some(1), Some(1), None)
            .await
            .unwrap();
        assert_eq!(stdout.as_deref(), Some("hello\n"));
        assert_eq!(stderr.as_deref(), Some(""));
        assert_eq!(next_stdout, 2);
        assert_eq!(next_stderr, 1);
    }

    #[tokio::test]
    async fn registry_shell_job_stop_cancels_queued_job() {
        let registry = ShellClientRegistry::default();
        registry
            .register(ShellClientRegisterRequest {
                process_started_at: None,
                build: None,
                job_inventory: None,
                client_id: "oe".to_string(),
                agent_instance_id: "inst".to_string(),
                display_name: None,
                owner: None,
                hostname: None,
                capabilities: Some(async_job_capabilities()),
                projects: None,
                agent_protocol_version: None,
                policy: None,
            })
            .await
            .unwrap();
        let job = registry
            .start_job(
                ShellJobOpRequest {
                    op: "start".to_string(),
                    client_id: Some("oe".to_string()),
                    cwd: None,
                    command: Some("sleep 10".to_string()),
                    timeout_secs: Some(10),
                    job_id: None,
                    since_stdout_line: None,
                    since_stderr_line: None,
                    tail_lines: None,
                    limit: None,
                    codex: None,
                },
                "test".to_string(),
            )
            .await
            .unwrap();
        let stopped = registry
            .stop_job(&job.job_id, "test".to_string())
            .await
            .unwrap();
        assert_eq!(stopped.status, "stopped");
        let polled = registry
            .poll(ShellAgentPollRequest {
                client_id: "oe".to_string(),
                agent_instance_id: "inst".to_string(),
                projects: None,
            })
            .await
            .unwrap();
        assert!(polled.is_none());
    }

    #[tokio::test]
    async fn registry_shell_job_stop_running_delivers_stop_to_client() {
        let registry = ShellClientRegistry::default();
        registry
            .register(ShellClientRegisterRequest {
                process_started_at: None,
                build: None,
                job_inventory: None,
                client_id: "oe".to_string(),
                agent_instance_id: "inst".to_string(),
                display_name: None,
                owner: None,
                hostname: None,
                capabilities: Some(async_job_capabilities()),
                projects: None,
                agent_protocol_version: None,
                policy: None,
            })
            .await
            .unwrap();
        let job = registry
            .start_job(
                ShellJobOpRequest {
                    op: "start".to_string(),
                    client_id: Some("oe".to_string()),
                    cwd: None,
                    command: Some("sleep 10".to_string()),
                    timeout_secs: Some(10),
                    job_id: None,
                    since_stdout_line: None,
                    since_stderr_line: None,
                    tail_lines: None,
                    limit: None,
                    codex: None,
                },
                "test".to_string(),
            )
            .await
            .unwrap();
        let started = registry
            .poll(ShellAgentPollRequest {
                client_id: "oe".to_string(),
                agent_instance_id: "inst".to_string(),
                projects: None,
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(started.kind, "start_job");

        let stop_requested = registry
            .stop_job(&job.job_id, "test".to_string())
            .await
            .unwrap();
        assert_eq!(stop_requested.status, "stop_requested");
        let stop = registry
            .poll(ShellAgentPollRequest {
                client_id: "oe".to_string(),
                agent_instance_id: "inst".to_string(),
                projects: None,
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stop.kind, "stop_job");
        assert_eq!(stop.job_id.as_deref(), Some(job.job_id.as_str()));
    }

    #[tokio::test]
    async fn registry_marks_running_job_lost_when_client_stale() {
        let registry = ShellClientRegistry::default();
        registry
            .register(ShellClientRegisterRequest {
                process_started_at: None,
                build: None,
                job_inventory: None,
                client_id: "oe".to_string(),
                agent_instance_id: "inst".to_string(),
                display_name: None,
                owner: None,
                hostname: None,
                capabilities: Some(async_job_capabilities()),
                projects: None,
                agent_protocol_version: None,
                policy: None,
            })
            .await
            .unwrap();
        let job = registry
            .start_job(
                ShellJobOpRequest {
                    op: "start".to_string(),
                    client_id: Some("oe".to_string()),
                    cwd: None,
                    command: Some("sleep 10".to_string()),
                    timeout_secs: Some(10),
                    job_id: None,
                    since_stdout_line: None,
                    since_stderr_line: None,
                    tail_lines: None,
                    limit: None,
                    codex: None,
                },
                "test".to_string(),
            )
            .await
            .unwrap();
        let _ = registry
            .poll(ShellAgentPollRequest {
                client_id: "oe".to_string(),
                agent_instance_id: "inst".to_string(),
                projects: None,
            })
            .await
            .unwrap()
            .unwrap();
        {
            let mut inner = registry.inner.lock().await;
            let client = inner.clients.get_mut("oe").unwrap();
            client.last_seen = now_ts() - CLIENT_ONLINE_WINDOW_SECS - 1;
        }
        let lost = registry.get_job(&job.job_id).await.unwrap();
        assert_eq!(lost.status, "lost");
        assert!(lost.error.unwrap().contains("stale"));
    }

    #[tokio::test]
    async fn touch_client_refreshes_stale_client_back_to_online() {
        let registry = ShellClientRegistry::default();
        registry
            .register(ShellClientRegisterRequest {
                process_started_at: None,
                build: None,
                job_inventory: None,
                client_id: "oe".to_string(),
                agent_instance_id: "inst".to_string(),
                display_name: None,
                owner: None,
                hostname: None,
                capabilities: Some(async_job_capabilities()),
                projects: None,
                agent_protocol_version: None,
                policy: None,
            })
            .await
            .unwrap();

        // Age the client past the online window so it reads as stale.
        registry
            .set_last_seen_for_test("oe", now_ts() - CLIENT_ONLINE_WINDOW_SECS - 1)
            .await;
        let stale = registry.get_client_view("oe").await.unwrap();
        assert!(!stale.connected);
        assert_eq!(stale.status, "stale");

        // A keepalive touch must bring it back online.
        registry.touch_client("oe", "inst").await.unwrap();
        let fresh = registry.get_client_view("oe").await.unwrap();
        assert!(fresh.connected);
        assert_eq!(fresh.status, "online");

        // Unknown client_id is a clear error and does not mutate state.
        assert!(registry.touch_client("nope", "inst").await.is_err());
    }

    #[tokio::test]
    async fn touch_client_refreshes_websocket_transport_client() {
        let registry = ShellClientRegistry::default();
        registry
            .register(ShellClientRegisterRequest {
                process_started_at: None,
                build: None,
                job_inventory: None,
                client_id: "ws-1".to_string(),
                agent_instance_id: "inst".to_string(),
                display_name: None,
                owner: None,
                hostname: None,
                capabilities: Some(async_job_capabilities()),
                projects: None,
                agent_protocol_version: None,
                policy: None,
            })
            .await
            .unwrap();
        registry
            .set_transport("ws-1", TRANSPORT_WEBSOCKET)
            .await
            .unwrap();

        registry
            .set_last_seen_for_test("ws-1", now_ts() - CLIENT_ONLINE_WINDOW_SECS - 1)
            .await;
        let stale = registry.get_client_view("ws-1").await.unwrap();
        assert_eq!(stale.transport, "websocket");
        assert!(!stale.connected);

        registry.touch_client("ws-1", "inst").await.unwrap();
        let fresh = registry.get_client_view("ws-1").await.unwrap();
        assert_eq!(fresh.transport, "websocket");
        assert!(fresh.connected);
        assert_eq!(fresh.status, "online");
    }

    #[tokio::test]
    async fn touch_client_rejects_stale_instance_and_accepts_active() {
        // Regression: a stale/replaced instance must not refresh the active
        // lease's `last_seen` via Ping/Pong keepalive.
        let registry = ShellClientRegistry::default();
        // Instance A registers and is online.
        let view_a = register_with_instance(&registry, "oe", "inst-a").await;
        assert!(view_a.connected);

        // Age A out so a newer instance may take over the lease.
        registry
            .set_last_seen_for_test("oe", now_ts() - CLIENT_ONLINE_WINDOW_SECS - 1)
            .await;
        // Instance B replaces A.
        let view_b = register_with_instance(&registry, "oe", "inst-b").await;
        assert_eq!(view_b.agent_instance_id, "inst-b");
        assert!(view_b.connected);

        // Capture B's last_seen right after registration.
        let before = registry.get_client_view("oe").await.unwrap().last_seen;
        // Sleep a moment so a successful touch would observably advance
        // last_seen.
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;

        // Stale instance A's keepalive must be rejected and must NOT advance
        // last_seen for B.
        let err = registry.touch_client("oe", "inst-a").await.unwrap_err();
        assert!(
            err.contains("no longer the active instance"),
            "error was: {err}"
        );
        let after_a = registry.get_client_view("oe").await.unwrap().last_seen;
        assert_eq!(
            after_a, before,
            "stale instance touch must not refresh active last_seen"
        );
        // A stale instance must not resurrect the client to online either.
        let view_after_a = registry.get_client_view("oe").await.unwrap();
        assert!(view_after_a.connected);

        // Active instance B's keepalive succeeds and refreshes last_seen.
        registry.touch_client("oe", "inst-b").await.unwrap();
        let after_b = registry.get_client_view("oe").await.unwrap().last_seen;
        assert!(
            after_b > before,
            "active instance touch must refresh last_seen"
        );
        assert!(registry.get_client_view("oe").await.unwrap().connected);

        // An empty agent_instance_id is rejected by validation.
        assert!(registry.touch_client("oe", "").await.is_err());
    }

    #[test]
    fn enforce_register_owner_cases() {
        let bootstrap = auth_context(None, true);
        let user_alice = auth_context(Some("alice"), false);
        let agent_alice = agent_auth_context(
            "alice",
            "alice-laptop",
            vec![
                "agent:register",
                "agent:poll",
                "agent:result",
                "agent:job_update",
            ],
        );
        let agent_alice_register_only =
            agent_auth_context("alice", "alice-laptop", vec!["agent:register"]);

        // (case, auth, client_id, owner, Ok or Err(required error fragments)).
        let cases = vec![
            // No AuthMiddleware (unit tests): defer to the middleware, which in
            // production rejects anonymous requests before the handler runs.
            (
                "no auth skips with owner",
                None,
                "client-1",
                Some("anyone"),
                Ok(()),
            ),
            (
                "no auth skips without owner",
                None,
                "client-1",
                None,
                Ok(()),
            ),
            // Bootstrap may register any owner.
            (
                "bootstrap allows missing owner",
                Some(&bootstrap),
                "client-1",
                None,
                Ok(()),
            ),
            (
                "bootstrap allows any owner",
                Some(&bootstrap),
                "client-1",
                Some("bob"),
                Ok(()),
            ),
            // Phase 3: user tokens (Phase 2 personal API tokens) are no longer
            // allowed on agent transport endpoints. Only bootstrap or agent
            // tokens may register.
            (
                "user token is rejected",
                Some(&user_alice),
                "client-1",
                Some("alice"),
                Err(vec!["user tokens are not allowed"]),
            ),
            // Matching client_id + matching owner -> Ok.
            (
                "agent token matching client_id and owner",
                Some(&agent_alice),
                "alice-laptop",
                Some("alice"),
                Ok(()),
            ),
            // Matching client_id + missing owner -> Ok (owner filled in by the
            // caller via effective_register_owner).
            (
                "agent token matching client_id, missing owner",
                Some(&agent_alice),
                "alice-laptop",
                None,
                Ok(()),
            ),
            (
                "agent token wrong client_id rejected",
                Some(&agent_alice_register_only),
                "other-laptop",
                None,
                Err(vec!["not bound to client_id"]),
            ),
            (
                "agent token owner mismatch rejected",
                Some(&agent_alice_register_only),
                "alice-laptop",
                Some("bob"),
                Err(vec!["agent token owner is 'alice'", "bob"]),
            ),
        ];

        for (case, auth, client_id, owner, expected) in cases {
            let result = enforce_register_owner(auth, client_id, owner);
            match expected {
                Ok(()) => assert!(result.is_ok(), "case '{case}': got: {result:?}"),
                Err(fragments) => {
                    let err = result.expect_err(&format!("case '{case}': expected an error"));
                    for fragment in fragments {
                        assert!(
                            err.contains(fragment),
                            "case '{case}': missing '{fragment}' in error: {err}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn effective_register_owner_agent_token_fills_username() {
        let alice = agent_auth_context("alice", "alice-laptop", vec!["agent:register"]);
        // Missing owner -> filled with the token's username.
        assert_eq!(
            effective_register_owner(Some(&alice), None),
            Some("alice".to_string())
        );
        // Matching owner preserved.
        assert_eq!(
            effective_register_owner(Some(&alice), Some("alice")),
            Some("alice".to_string())
        );
        // Bootstrap keeps the request owner.
        let bootstrap = auth_context(None, true);
        assert_eq!(
            effective_register_owner(Some(&bootstrap), Some("bob")),
            Some("bob".to_string())
        );
    }

    #[test]
    fn enforce_agent_transport_rejects_user_token() {
        let alice = auth_context(Some("alice"), false);
        let err = enforce_agent_transport(Some(&alice), "client-1").unwrap_err();
        assert!(err.contains("user tokens are not allowed"), "got: {}", err);
    }

    #[test]
    fn enforce_agent_transport_agent_token_matching_client_succeeds() {
        let alice = agent_auth_context("alice", "alice-laptop", vec!["agent:poll"]);
        assert!(enforce_agent_transport(Some(&alice), "alice-laptop").is_ok());
        let err = enforce_agent_transport(Some(&alice), "other").unwrap_err();
        assert!(err.contains("not bound"), "got: {}", err);
    }

    #[test]
    fn enforce_agent_transport_bootstrap_succeeds() {
        let bootstrap = auth_context(None, true);
        assert!(enforce_agent_transport(Some(&bootstrap), "any-client").is_ok());
    }

    #[test]
    fn require_agent_transport_scope_agent_token_with_scope_succeeds() {
        let alice = agent_auth_context("alice", "alice-laptop", vec!["agent:poll"]);
        assert!(require_agent_transport_scope(Some(&alice), "agent:poll").is_ok());
        assert!(require_agent_transport_scope(Some(&alice), "agent:register").is_err());
    }

    #[test]
    fn require_agent_transport_scope_bootstrap_always_succeeds() {
        let bootstrap = auth_context(None, true);
        assert!(require_agent_transport_scope(Some(&bootstrap), "agent:register").is_ok());
    }

    #[test]
    fn require_agent_transport_scope_user_token_rejected() {
        let alice = auth_context(Some("alice"), false);
        let err = require_agent_transport_scope(Some(&alice), "agent:register").unwrap_err();
        assert!(err.contains("missing required scope"), "got: {}", err);
    }

    #[test]
    fn oauth_bridge_token_remains_blocked_from_agent_transport() {
        let bridge = oauth_bridge_auth_context(
            "hash-a",
            vec![
                "agent:register",
                "agent:poll",
                "agent:result",
                "agent:job_update",
            ],
        );
        assert!(!bridge.is_lightweight());
        assert!(enforce_agent_transport(Some(&bridge), "client-a")
            .unwrap_err()
            .contains("user tokens are not allowed"));
        assert!(
            require_agent_transport_scope(Some(&bridge), "agent:register")
                .unwrap_err()
                .contains("missing required scope")
        );
    }

    #[tokio::test]
    async fn registry_rejects_enqueue_when_queue_full() {
        let registry = ShellClientRegistry::default();
        registry
            .register(ShellClientRegisterRequest {
                process_started_at: None,
                build: None,
                job_inventory: None,
                client_id: "full".to_string(),
                agent_instance_id: "inst".to_string(),
                display_name: None,
                owner: Some("alice".to_string()),
                hostname: None,
                capabilities: None,
                projects: None,
                agent_protocol_version: None,
                policy: None,
            })
            .await
            .unwrap();
        // Fill the queue to the limit without any consumer draining it.
        for _ in 0..MAX_QUEUED_REQUESTS_PER_CLIENT {
            registry
                .enqueue_run(
                    ShellRunRequest {
                        client_id: "full".to_string(),
                        cwd: None,
                        command: "echo hi".to_string(),
                        stdin: None,
                        timeout_secs: 5,
                        wait_timeout_secs: 0,
                    },
                    "tester".to_string(),
                )
                .await
                .unwrap();
        }
        // The next enqueue must be rejected with a structured error instead
        // of growing the queue unboundedly.
        let err = registry
            .enqueue_run(
                ShellRunRequest {
                    client_id: "full".to_string(),
                    cwd: None,
                    command: "echo hi".to_string(),
                    stdin: None,
                    timeout_secs: 5,
                    wait_timeout_secs: 0,
                },
                "tester".to_string(),
            )
            .await
            .unwrap_err();
        assert!(err.contains("too many pending requests"));
        assert!(err.contains("full"));
        // The queue is exactly at the cap; memory is bounded.
        let view = registry.get_client_view("full").await.unwrap();
        assert_eq!(view.pending_requests, MAX_QUEUED_REQUESTS_PER_CLIENT);
    }

    #[tokio::test]
    async fn registry_rejects_enqueue_when_client_offline() {
        // Registered-but-stale agents must fail fast at enqueue rather than
        // accepting work that can only time out (or fill the 256-deep queue).
        let registry = ShellClientRegistry::default();
        registry
            .register(ShellClientRegisterRequest {
                process_started_at: None,
                build: None,
                job_inventory: None,
                client_id: "stale".to_string(),
                agent_instance_id: "inst".to_string(),
                display_name: None,
                owner: None,
                hostname: None,
                capabilities: None,
                projects: None,
                agent_protocol_version: None,
                policy: None,
            })
            .await
            .unwrap();
        registry
            .set_last_seen_for_test("stale", now_ts() - CLIENT_ONLINE_WINDOW_SECS - 1)
            .await;

        let err = registry
            .enqueue_run(
                ShellRunRequest {
                    client_id: "stale".to_string(),
                    cwd: None,
                    command: "echo hi".to_string(),
                    stdin: None,
                    timeout_secs: 5,
                    wait_timeout_secs: 0,
                },
                "tester".to_string(),
            )
            .await
            .unwrap_err();
        assert!(
            err.contains("offline"),
            "enqueue against a stale agent must fail fast as offline: {err}"
        );
        let view = registry.get_client_view("stale").await.unwrap();
        assert_eq!(view.pending_requests, 0);
        assert!(!view.connected);
    }

    #[tokio::test]
    async fn reconcile_disconnect_marks_running_jobs_lost() {
        let registry = ShellClientRegistry::default();
        registry
            .register(ShellClientRegisterRequest {
                process_started_at: None,
                build: None,
                job_inventory: None,
                client_id: "oe".to_string(),
                agent_instance_id: "inst".to_string(),
                display_name: None,
                owner: None,
                hostname: None,
                capabilities: Some(async_job_capabilities()),
                projects: None,
                agent_protocol_version: None,
                policy: None,
            })
            .await
            .unwrap();
        let job = registry
            .start_job(
                ShellJobOpRequest {
                    op: "start".to_string(),
                    client_id: Some("oe".to_string()),
                    cwd: None,
                    command: Some("sleep 10".to_string()),
                    timeout_secs: Some(10),
                    job_id: None,
                    since_stdout_line: None,
                    since_stderr_line: None,
                    tail_lines: None,
                    limit: None,
                    codex: None,
                },
                "test".to_string(),
            )
            .await
            .unwrap();
        // Job is "queued" with its request sitting in the client's queue.
        let before = registry.get_client_view("oe").await.unwrap();
        assert_eq!(before.pending_requests, 1);
        // Transport disconnects (e.g. WebSocket dropped).
        registry.reconcile_disconnect("oe", "inst").await;
        let lost = registry.get_job(&job.job_id).await.unwrap();
        assert_eq!(lost.status, "lost");
        assert!(lost.error.unwrap().contains("disconnected"));
        // Pending request was dropped: no dangling waiter / queue entry.
        let after = registry.get_client_view("oe").await.unwrap();
        assert_eq!(after.pending_requests, 0);
    }

    #[tokio::test]
    async fn reconcile_disconnect_fails_pending_sync_requests_fast() {
        // Regression guard for the MCP "no reply" hang: a synchronous tool
        // request (run_shell/read_file/... with job_id: None) whose agent drops
        // mid-flight must be resolved immediately, not parked until the caller's
        // wait timeout.
        let registry = ShellClientRegistry::default();
        registry
            .register(ShellClientRegisterRequest {
                process_started_at: None,
                build: None,
                job_inventory: None,
                client_id: "oe".to_string(),
                agent_instance_id: "inst".to_string(),
                display_name: None,
                owner: None,
                hostname: None,
                capabilities: None,
                projects: None,
                agent_protocol_version: None,
                policy: None,
            })
            .await
            .unwrap();
        let (_request_id, rx) = registry
            .enqueue_run(
                ShellRunRequest {
                    client_id: "oe".to_string(),
                    cwd: Some("/tmp".to_string()),
                    command: "echo hi".to_string(),
                    stdin: None,
                    timeout_secs: 30,
                    wait_timeout_secs: 30,
                },
                "test".to_string(),
            )
            .await
            .unwrap();
        let before = registry.get_client_view("oe").await.unwrap();
        assert_eq!(before.pending_requests, 1);

        // Agent transport drops before returning a result.
        registry.reconcile_disconnect("oe", "inst").await;

        // Waiter resolves promptly with a disconnect error rather than parking
        // for the full 30s wait timeout. The short timeout turns a regression
        // (unbounded park) into a fast test failure instead of a hang.
        let response = tokio::time::timeout(std::time::Duration::from_secs(5), rx)
            .await
            .expect("waiter must resolve promptly, not park until the caller timeout")
            .expect("waiter must be resolved, not dropped");
        assert!(!response.success);
        let error = response.error.expect("disconnect must set an error");
        assert!(
            error.contains("offline"),
            "error should classify as agent_offline: {error}"
        );
        // No dangling waiter or queue entry remains.
        let after = registry.get_client_view("oe").await.unwrap();
        assert_eq!(after.pending_requests, 0);
    }

    #[tokio::test]
    async fn reconcile_disconnect_releases_active_lease_immediately() {
        let registry = ShellClientRegistry::default();
        register_with_instance(&registry, "oe", "inst-a").await;

        registry.reconcile_disconnect("oe", "inst-a").await;

        let offline = registry.get_client_view("oe").await.unwrap();
        assert!(
            !offline.connected,
            "active disconnect must immediately leave online window"
        );
        assert!(now_ts().saturating_sub(offline.last_seen) > CLIENT_ONLINE_WINDOW_SECS);

        let new_view = register_with_instance(&registry, "oe", "inst-b").await;
        assert_eq!(new_view.agent_instance_id, "inst-b");
        assert!(
            new_view.connected,
            "new instance should register without waiting 60 seconds"
        );
    }

    // ------------------------------------------------------------------------
    // Agent instance identity / lease model (Phase 1)
    // ------------------------------------------------------------------------

    /// Helper: register a client with an explicit `agent_instance_id`.
    async fn register_with_instance(
        registry: &ShellClientRegistry,
        client_id: &str,
        instance: &str,
    ) -> ShellClientView {
        registry
            .register(ShellClientRegisterRequest {
                process_started_at: None,
                build: None,
                job_inventory: None,
                client_id: client_id.to_string(),
                agent_instance_id: instance.to_string(),
                display_name: None,
                owner: Some("alice".to_string()),
                hostname: None,
                capabilities: Some(async_job_capabilities()),
                projects: None,
                agent_protocol_version: Some("polling-v1".to_string()),
                policy: None,
            })
            .await
            .unwrap()
    }

    /// Helper: register a long-lived-transport (WebSocket/QUIC) client bound to
    /// a server-internal `connection_id`. Mirrors what `agent_ws`/`agent_quic`
    /// do at register time. Returns the view along with the connection_id so a
    /// test can drive the connection-scoped poll/touch/result/update APIs.
    async fn register_with_connection(
        registry: &ShellClientRegistry,
        client_id: &str,
        instance: &str,
        connection_id: &str,
    ) -> ShellClientView {
        registry
            .register_with_auth_connection(
                ShellClientRegisterRequest {
                    process_started_at: None,
                    build: None,
                    job_inventory: None,
                    client_id: client_id.to_string(),
                    agent_instance_id: instance.to_string(),
                    display_name: None,
                    owner: Some("alice".to_string()),
                    hostname: None,
                    capabilities: Some(async_job_capabilities()),
                    projects: None,
                    agent_protocol_version: Some("polling-v1".to_string()),
                    policy: None,
                },
                None,
                Some(connection_id),
            )
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn lease_first_register_accepts_instance() {
        let registry = ShellClientRegistry::default();
        let view = register_with_instance(&registry, "oe", "inst-a").await;
        assert_eq!(view.agent_instance_id, "inst-a");
        assert!(view.connected);
        // The view/list path exposes the instance id.
        let clients = registry.list_clients().await;
        assert_eq!(clients[0].agent_instance_id, "inst-a");
    }

    #[tokio::test]
    async fn lease_same_instance_reregister_accepts() {
        let registry = ShellClientRegistry::default();
        register_with_instance(&registry, "oe", "inst-a").await;
        // Same client_id + same instance id is a reconnect/refresh: accepted.
        let _ = register_with_instance(&registry, "oe", "inst-a").await;
        let view = registry.get_client_view("oe").await.unwrap();
        assert_eq!(view.agent_instance_id, "inst-a");
        assert!(view.connected);
    }

    #[tokio::test]
    async fn lease_different_online_instance_rejected() {
        let registry = ShellClientRegistry::default();
        register_with_instance(&registry, "oe", "inst-a").await;
        // A second process with the same client_id but a different instance
        // must be rejected while the first is online.
        let err = registry
            .register(ShellClientRegisterRequest {
                process_started_at: None,
                build: None,
                job_inventory: None,
                client_id: "oe".to_string(),
                agent_instance_id: "inst-b".to_string(),
                display_name: None,
                owner: Some("alice".to_string()),
                hostname: None,
                capabilities: Some(async_job_capabilities()),
                projects: None,
                agent_protocol_version: Some("polling-v1".to_string()),
                policy: None,
            })
            .await
            .unwrap_err();
        assert!(err.contains("already online"), "error was: {err}");
        assert!(err.contains("different instance"), "error was: {err}");
        // The active instance is unchanged.
        let view = registry.get_client_view("oe").await.unwrap();
        assert_eq!(view.agent_instance_id, "inst-a");
    }

    #[tokio::test]
    async fn lease_stale_replaced_by_different_instance_accepts() {
        let registry = ShellClientRegistry::default();
        register_with_instance(&registry, "oe", "inst-a").await;
        // Age the first instance past the online window so it reads as stale.
        registry
            .set_last_seen_for_test("oe", chrono::Utc::now().timestamp() - 120)
            .await;
        // A different instance may now take over the lease.
        let _ = register_with_instance(&registry, "oe", "inst-b").await;
        let view = registry.get_client_view("oe").await.unwrap();
        assert_eq!(view.agent_instance_id, "inst-b");
        assert!(view.connected);
    }

    #[tokio::test]
    async fn lease_stale_instance_poll_rejected() {
        let registry = ShellClientRegistry::default();
        register_with_instance(&registry, "oe", "inst-a").await;
        // Replace with a newer instance after aging out.
        registry
            .set_last_seen_for_test("oe", chrono::Utc::now().timestamp() - 120)
            .await;
        register_with_instance(&registry, "oe", "inst-b").await;

        // The stale instance A can no longer poll.
        let err = registry
            .poll(ShellAgentPollRequest {
                client_id: "oe".to_string(),
                agent_instance_id: "inst-a".to_string(),
                projects: None,
            })
            .await
            .unwrap_err();
        assert!(
            err.contains("no longer the active instance"),
            "error was: {err}"
        );

        // The active instance B can still poll.
        registry
            .poll(ShellAgentPollRequest {
                client_id: "oe".to_string(),
                agent_instance_id: "inst-b".to_string(),
                projects: None,
            })
            .await
            .expect("active instance must poll");
    }

    #[tokio::test]
    async fn lease_stale_instance_result_rejected() {
        let registry = ShellClientRegistry::default();
        register_with_instance(&registry, "oe", "inst-a").await;
        // Enqueue a request and let instance A poll it.
        let (request_id, _rx) = registry
            .enqueue_run(
                ShellRunRequest {
                    client_id: "oe".to_string(),
                    cwd: None,
                    command: "echo hi".to_string(),
                    stdin: None,
                    timeout_secs: 5,
                    wait_timeout_secs: 0,
                },
                "tester".to_string(),
            )
            .await
            .unwrap();
        let _ = registry
            .poll(ShellAgentPollRequest {
                client_id: "oe".to_string(),
                agent_instance_id: "inst-a".to_string(),
                projects: None,
            })
            .await
            .unwrap()
            .unwrap();

        // Replace instance A with B after aging out.
        registry
            .set_last_seen_for_test("oe", chrono::Utc::now().timestamp() - 120)
            .await;
        register_with_instance(&registry, "oe", "inst-b").await;

        // The stale instance A cannot submit the result.
        let err = registry
            .complete(ShellAgentResultRequest {
                client_id: "oe".to_string(),
                agent_instance_id: "inst-a".to_string(),
                request_id: request_id.clone(),
                exit_code: Some(0),
                stdout: Some("hi".to_string()),
                stderr: None,
                duration_ms: Some(1),
                error: None,
            })
            .await
            .unwrap_err();
        assert!(
            err.contains("no longer the active instance"),
            "error was: {err}"
        );

        // The active instance B can submit the result.
        registry
            .complete(ShellAgentResultRequest {
                client_id: "oe".to_string(),
                agent_instance_id: "inst-b".to_string(),
                request_id,
                exit_code: Some(0),
                stdout: Some("hi".to_string()),
                stderr: None,
                duration_ms: Some(1),
                error: None,
            })
            .await
            .expect("active instance must submit result");
    }

    #[tokio::test]
    async fn lease_stale_instance_job_update_rejected() {
        let registry = ShellClientRegistry::default();
        register_with_instance(&registry, "oe", "inst-a").await;
        let job = registry
            .start_job(
                ShellJobOpRequest {
                    op: "start".to_string(),
                    client_id: Some("oe".to_string()),
                    cwd: None,
                    command: Some("sleep 10".to_string()),
                    timeout_secs: Some(10),
                    job_id: None,
                    since_stdout_line: None,
                    since_stderr_line: None,
                    tail_lines: None,
                    limit: None,
                    codex: None,
                },
                "tester".to_string(),
            )
            .await
            .unwrap();

        // Replace instance A with B after aging out.
        registry
            .set_last_seen_for_test("oe", chrono::Utc::now().timestamp() - 120)
            .await;
        register_with_instance(&registry, "oe", "inst-b").await;

        // The stale instance A cannot update the job.
        let err = registry
            .update_job(ShellAgentJobUpdateRequest {
                client_id: "oe".to_string(),
                agent_instance_id: "inst-a".to_string(),
                update_seq: None,
                job_id: job.job_id.clone(),
                request_id: None,
                status: "running".to_string(),
                stdout_chunk: None,
                stderr_chunk: None,
                stdout_tail: None,
                stderr_tail: None,
                log_snapshot: None,
                exit_code: None,
                duration_ms: None,
                error: None,
                validation_progress: None,
                finished: false,
            })
            .await
            .unwrap_err();
        assert!(
            err.contains("no longer the active instance"),
            "error was: {err}"
        );

        // The active instance B can update the job.
        registry
            .update_job(ShellAgentJobUpdateRequest {
                client_id: "oe".to_string(),
                agent_instance_id: "inst-b".to_string(),
                update_seq: None,
                job_id: job.job_id.clone(),
                request_id: None,
                status: "running".to_string(),
                stdout_chunk: None,
                stderr_chunk: None,
                stdout_tail: None,
                stderr_tail: None,
                log_snapshot: None,
                exit_code: None,
                duration_ms: None,
                error: None,
                validation_progress: None,
                finished: false,
            })
            .await
            .expect("active instance must update job");
    }

    #[tokio::test]
    async fn lease_list_clients_exposes_instance_id() {
        let registry = ShellClientRegistry::default();
        register_with_instance(&registry, "oe", "inst-a").await;
        let clients = registry.list_clients().await;
        assert_eq!(clients.len(), 1);
        assert_eq!(clients[0].agent_instance_id, "inst-a");
        let view = registry.get_client_view("oe").await.unwrap();
        assert_eq!(view.agent_instance_id, "inst-a");
    }

    #[tokio::test]
    async fn lease_reconcile_disconnect_stale_instance_is_noop() {
        // A stale instance disconnecting after a newer instance has taken over
        // must NOT clear the active notifier or mark the active instance's
        // jobs lost.
        let registry = ShellClientRegistry::default();
        register_with_instance(&registry, "oe", "inst-a").await;
        // Install a notifier for instance A.
        let notify_a = Arc::new(Notify::new());
        registry
            .register_notifier("oe", "inst-a", notify_a.clone())
            .await
            .unwrap();
        // Start a job under instance A.
        let job = registry
            .start_job(
                ShellJobOpRequest {
                    op: "start".to_string(),
                    client_id: Some("oe".to_string()),
                    cwd: None,
                    command: Some("sleep 10".to_string()),
                    timeout_secs: Some(10),
                    job_id: None,
                    since_stdout_line: None,
                    since_stderr_line: None,
                    tail_lines: None,
                    limit: None,
                    codex: None,
                },
                "tester".to_string(),
            )
            .await
            .unwrap();

        // Age out A and let B take over.
        registry
            .set_last_seen_for_test("oe", chrono::Utc::now().timestamp() - 120)
            .await;
        register_with_instance(&registry, "oe", "inst-b").await;
        // B installs its own notifier.
        let notify_b = Arc::new(Notify::new());
        registry
            .register_notifier("oe", "inst-b", notify_b.clone())
            .await
            .unwrap();

        // A's transport finally disconnects. This must be a no-op: B's notifier
        // stays and B's job is not marked lost.
        registry.reconcile_disconnect("oe", "inst-a").await;
        let job_view = registry.get_job(&job.job_id).await.unwrap();
        assert_ne!(
            job_view.status, "lost",
            "stale disconnect must not mark active instance job lost"
        );
        // B's disconnect, however, does reconcile.
        registry.reconcile_disconnect("oe", "inst-b").await;
        let job_view = registry.get_job(&job.job_id).await.unwrap();
        assert_eq!(job_view.status, "lost");
    }

    #[tokio::test]
    async fn lease_register_notifier_rejects_stale_instance() {
        let registry = ShellClientRegistry::default();
        register_with_instance(&registry, "oe", "inst-a").await;
        // Replace A with B.
        registry
            .set_last_seen_for_test("oe", chrono::Utc::now().timestamp() - 120)
            .await;
        register_with_instance(&registry, "oe", "inst-b").await;
        // A's late notifier registration must be rejected so it cannot
        // overwrite B's notifier.
        let err = registry
            .register_notifier("oe", "inst-a", Arc::new(Notify::new()))
            .await
            .unwrap_err();
        assert!(
            err.contains("no longer the active instance"),
            "error was: {err}"
        );
        // B can still install its notifier.
        registry
            .register_notifier("oe", "inst-b", Arc::new(Notify::new()))
            .await
            .expect("active instance must install notifier");
    }

    #[tokio::test]
    async fn lease_register_rejects_empty_instance_id() {
        let registry = ShellClientRegistry::default();
        let err = registry
            .register(ShellClientRegisterRequest {
                process_started_at: None,
                build: None,
                job_inventory: None,
                client_id: "oe".to_string(),
                agent_instance_id: "".to_string(),
                display_name: None,
                owner: None,
                hostname: None,
                capabilities: None,
                projects: None,
                agent_protocol_version: None,
                policy: None,
            })
            .await
            .unwrap_err();
        assert!(err.contains("agent_instance_id"), "error was: {err}");
    }
    #[tokio::test]
    async fn project_active_job_query_is_not_truncated_and_unregister_fences_starts() {
        let registry = ShellClientRegistry::default();
        register_with_instance(&registry, "oe", "inst-jobs").await;
        let request = |command: &str| ShellJobOpRequest {
            op: "start".to_string(),
            client_id: Some("oe".to_string()),
            cwd: None,
            command: Some(command.to_string()),
            timeout_secs: Some(60),
            job_id: None,
            since_stdout_line: None,
            since_stderr_line: None,
            tail_lines: None,
            limit: None,
            codex: None,
        };
        let target = "agent:oe:target";
        let target_job = registry
            .start_job_with_metadata(
                request("sleep 60"),
                "tester".to_string(),
                ShellJobStartMetadata {
                    project_id: Some(target.to_string()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        {
            let mut inner = registry.inner.lock().await;
            inner
                .jobs_by_id
                .get_mut(&target_job.job_id)
                .unwrap()
                .created_at = 0;
        }
        for index in 0..101 {
            registry
                .start_job_with_metadata(
                    request(&format!("echo {index}")),
                    "tester".to_string(),
                    ShellJobStartMetadata {
                        project_id: Some(format!("agent:oe:other-{index}")),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
        }
        assert_eq!(registry.list_jobs(Some(100)).await.len(), 100);
        assert_eq!(
            registry.count_active_jobs_for_project(None, target).await,
            1
        );
        assert_eq!(
            registry
                .begin_project_unregister(None, target)
                .await
                .unwrap(),
            1
        );

        {
            let mut inner = registry.inner.lock().await;
            let job = inner.jobs_by_id.get_mut(&target_job.job_id).unwrap();
            job.status = "completed".to_string();
            job.ended_at = Some(now_ts());
        }
        assert_eq!(
            registry
                .begin_project_unregister(None, target)
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            registry
                .begin_project_unregister(None, target)
                .await
                .unwrap(),
            0
        );
        registry.end_project_unregister(target).await;
        let blocked = registry
            .start_job_with_metadata(
                request("echo blocked"),
                "tester".to_string(),
                ShellJobStartMetadata {
                    project_id: Some(target.to_string()),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert_eq!(blocked, "project_unregister_in_progress");
        registry.end_project_unregister(target).await;
        registry
            .start_job_with_metadata(
                request("echo allowed"),
                "tester".to_string(),
                ShellJobStartMetadata {
                    project_id: Some(target.to_string()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
    }

    // ------------------------------------------------------------------------
    // Connection-scoped lease: same-instance transport reconnect races.
    // A replaced connection (same client_id + same agent_instance_id but a
    // newer connection_id) must not let the older socket dequeue new
    // requests, refresh liveness, or clobber the new connection's metadata.
    // ------------------------------------------------------------------------

    #[tokio::test]
    async fn stale_connection_poll_cannot_steal_new_request() {
        // Same runner instance registers over connection A, a request is
        // queued, then the instance reconnects over connection B (new lease).
        // Connection A's connection-scoped poll must be rejected with a stale
        // connection error AND leave the request in the queue / undispatched /
        // job un-transitioned (atomic: not just a stale error string). B then
        // polls and is the only one to receive the request.
        let registry = ShellClientRegistry::default();
        register_with_connection(&registry, "oe", "inst-x", "conn-a").await;

        // Start an async job (queued -> agent_queued only on dispatch).
        let job = registry
            .start_job(
                ShellJobOpRequest {
                    op: "start".to_string(),
                    client_id: Some("oe".to_string()),
                    cwd: None,
                    command: Some("sleep 1".to_string()),
                    timeout_secs: Some(1),
                    job_id: None,
                    since_stdout_line: None,
                    since_stderr_line: None,
                    tail_lines: None,
                    limit: None,
                    codex: None,
                },
                "tester".to_string(),
            )
            .await
            .unwrap();
        // The job starts queued with one pending request in the queue.
        assert_eq!(
            registry.get_job(&job.job_id).await.unwrap().status,
            "queued"
        );

        // Same instance reconnects over connection B; B takes the lease.
        register_with_connection(&registry, "oe", "inst-x", "conn-b").await;

        // A's connection-scoped poll is rejected with the stable stale error.
        let err = registry
            .poll_for_connection(
                ShellAgentPollRequest {
                    client_id: "oe".to_string(),
                    agent_instance_id: "inst-x".to_string(),
                    projects: None,
                },
                "conn-a",
            )
            .await
            .unwrap_err();
        assert!(
            err.contains("transport connection is no longer active"),
            "error was: {err}"
        );

        // Atomicity: the request must still be queued, undispatched, and the
        // job must still be queued (no queued -> agent_queued transition).
        let pending_depth = registry
            .get_client_view("oe")
            .await
            .unwrap()
            .pending_requests;
        assert_eq!(pending_depth, 1, "stale poll must not dequeue the request");
        {
            let inner = registry.inner.lock().await;
            let request_id = inner
                .jobs_by_id
                .get(&job.job_id)
                .and_then(|j| j.request_id.clone());
            let request_id = request_id.expect("job has a request_id");
            let pending = inner
                .pending_by_id
                .get(&request_id)
                .expect("request still pending");
            assert!(
                !pending.dispatched,
                "stale poll must not mark request dispatched"
            );
            assert_eq!(
                inner.jobs_by_id.get(&job.job_id).unwrap().status,
                "queued",
                "stale poll must not transition the job"
            );
        }

        // B's connection-scoped poll receives the request (exactly once).
        let polled_b = registry
            .poll_for_connection(
                ShellAgentPollRequest {
                    client_id: "oe".to_string(),
                    agent_instance_id: "inst-x".to_string(),
                    projects: None,
                },
                "conn-b",
            )
            .await
            .unwrap()
            .expect("current connection must receive the request");
        assert_eq!(polled_b.kind, "start_job");
        assert_eq!(
            registry.get_job(&job.job_id).await.unwrap().status,
            "agent_queued"
        );
        // The queue is now drained: a second poll by either connection gets None.
        let again_a = registry
            .poll_for_connection(
                ShellAgentPollRequest {
                    client_id: "oe".to_string(),
                    agent_instance_id: "inst-x".to_string(),
                    projects: None,
                },
                "conn-a",
            )
            .await;
        // A is still stale, so this is an error (not a None success).
        assert!(again_a.is_err());
    }

    #[tokio::test]
    async fn stale_connection_keepalive_does_not_refresh_new_lease() {
        // After a same-instance reconnect, a delayed Ping/Pong from the old
        // connection must not refresh the new connection's last_seen or revive
        // a disconnected client. The current connection's keepalive does
        // refresh.
        let registry = ShellClientRegistry::default();
        register_with_connection(&registry, "oe", "inst-x", "conn-a").await;
        register_with_connection(&registry, "oe", "inst-x", "conn-b").await;

        // Pin the current client's last_seen to a known stale value so a
        // successful touch would observably advance it.
        let pinned = chrono::Utc::now().timestamp() - 90;
        registry.set_last_seen_for_test("oe", pinned).await;

        // A's connection-scoped touch fails and leaves last_seen unchanged.
        let err = registry
            .touch_client_for_connection("oe", "inst-x", "conn-a")
            .await
            .unwrap_err();
        assert!(
            err.contains("transport connection is no longer active"),
            "error was: {err}"
        );
        assert_eq!(
            registry.get_client_view("oe").await.unwrap().last_seen,
            pinned,
            "stale connection touch must not refresh last_seen"
        );

        // B's connection-scoped touch succeeds and advances last_seen.
        registry
            .touch_client_for_connection("oe", "inst-x", "conn-b")
            .await
            .unwrap();
        assert!(
            registry.get_client_view("oe").await.unwrap().last_seen > pinned,
            "current connection touch must refresh last_seen"
        );

        // An even newer connection C supersedes B; B's touch now fails too.
        register_with_connection(&registry, "oe", "inst-x", "conn-c").await;
        let err = registry
            .touch_client_for_connection("oe", "inst-x", "conn-b")
            .await
            .unwrap_err();
        assert!(
            err.contains("transport connection is no longer active"),
            "superseded connection touch must be rejected, error was: {err}"
        );
    }

    #[tokio::test]
    async fn stale_connection_runtime_metadata_does_not_overwrite_current() {
        // A stale same-instance connection must not overwrite the current
        // connection's provider metadata. The current connection can.
        let registry = ShellClientRegistry::default();
        let register_with_policy = async |connection_id: &str| {
            registry
                .register_with_auth_connection(
                    ShellClientRegisterRequest {
                        process_started_at: None,
                        build: None,
                        job_inventory: None,
                        client_id: "oe".to_string(),
                        agent_instance_id: "inst-x".to_string(),
                        display_name: None,
                        owner: Some("alice".to_string()),
                        hostname: None,
                        capabilities: Some(async_job_capabilities()),
                        projects: None,
                        agent_protocol_version: Some("polling-v1".to_string()),
                        policy: Some(AgentPolicySummary::default()),
                    },
                    None,
                    Some(connection_id),
                )
                .await
                .unwrap()
        };
        register_with_policy("conn-a").await;
        register_with_policy("conn-b").await;

        let provider_status = |strategy: &str| ToolProvidersStatus {
            strategy: strategy.to_string(),
            claude_code: ClaudeCodeProviderStatus {
                enabled: true,
                version: None,
                available: true,
                process_state: "running".to_string(),
                discovered_tool_names: Vec::new(),
                capabilities: std::collections::BTreeMap::new(),
                last_error_code: None,
                last_call: None,
            },
            config_reload: Default::default(),
        };

        // Current connection B reports a provider status.
        registry
            .update_tool_providers_for_connection(
                "oe",
                "inst-x",
                "conn-b",
                Some(provider_status("claude_code")),
            )
            .await
            .unwrap();
        {
            let inner = registry.inner.lock().await;
            let client = inner.clients.get("oe").unwrap();
            assert_eq!(
                client
                    .policy
                    .as_ref()
                    .unwrap()
                    .tool_providers
                    .as_ref()
                    .unwrap()
                    .strategy,
                "claude_code"
            );
        }

        // Stale connection A tries to overwrite with a different valid
        // strategy; it must be rejected and must not change the recorded
        // strategy.
        let err = registry
            .update_tool_providers_for_connection(
                "oe",
                "inst-x",
                "conn-a",
                Some(provider_status("native")),
            )
            .await
            .unwrap_err();
        assert!(
            err.contains("transport connection is no longer active"),
            "{err}"
        );
        {
            let inner = registry.inner.lock().await;
            let client = inner.clients.get("oe").unwrap();
            assert_eq!(
                client
                    .policy
                    .as_ref()
                    .unwrap()
                    .tool_providers
                    .as_ref()
                    .unwrap()
                    .strategy,
                "claude_code",
                "stale connection must not overwrite current metadata"
            );
        }
    }

    #[tokio::test]
    async fn stale_connection_disconnect_cleanup_is_noop_for_current_lease() {
        // Same-instance reconnect: A's delayed disconnect cleanup must not
        // touch B's notifier/queue/liveness. Extends the existing same-instance
        // reconnect coverage to the connection lease.
        let registry = ShellClientRegistry::default();
        register_with_connection(&registry, "oe", "inst-x", "conn-a").await;
        let notify_a = Arc::new(Notify::new());
        registry
            .register_notifier_for_connection("oe", "inst-x", "conn-a", notify_a)
            .await
            .unwrap();
        let job = registry
            .start_job(
                ShellJobOpRequest {
                    op: "start".to_string(),
                    client_id: Some("oe".to_string()),
                    cwd: None,
                    command: Some("sleep 10".to_string()),
                    timeout_secs: Some(10),
                    job_id: None,
                    since_stdout_line: None,
                    since_stderr_line: None,
                    tail_lines: None,
                    limit: None,
                    codex: None,
                },
                "tester".to_string(),
            )
            .await
            .unwrap();

        // B reconnects (same instance) and installs its own notifier.
        register_with_connection(&registry, "oe", "inst-x", "conn-b").await;
        let notify_b = Arc::new(Notify::new());
        registry
            .register_notifier_for_connection("oe", "inst-x", "conn-b", notify_b)
            .await
            .unwrap();

        // A's delayed disconnect cleanup is a no-op: B's job is not lost.
        registry
            .reconcile_disconnect_for_connection("oe", "inst-x", "conn-a")
            .await;
        assert_ne!(
            registry.get_job(&job.job_id).await.unwrap().status,
            "lost",
            "stale connection cleanup must not mark current job lost"
        );
        // B's notifier survives A's cleanup and B's own dispatch still works.
        let polled = registry
            .poll_for_connection(
                ShellAgentPollRequest {
                    client_id: "oe".to_string(),
                    agent_instance_id: "inst-x".to_string(),
                    projects: None,
                },
                "conn-b",
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(polled.kind, "start_job");

        // B's own disconnect does reconcile the job to lost.
        registry
            .reconcile_disconnect_for_connection("oe", "inst-x", "conn-b")
            .await;
        assert_eq!(registry.get_job(&job.job_id).await.unwrap().status, "lost");
    }

    #[tokio::test]
    async fn late_result_on_stale_connection_is_accepted_without_refreshing_liveness() {
        // A request dispatched to A (same instance) before the reconnect must
        // still complete on a late result arriving over the stale connection
        // A — it belongs to the same instance — but must NOT refresh B's
        // liveness. A cannot then poll a new request that arrived after B's
        // register.
        let registry = ShellClientRegistry::default();
        register_with_connection(&registry, "oe", "inst-x", "conn-a").await;

        // Enqueue a sync request and let A poll it (still current lease).
        let (request_id, rx) = registry
            .enqueue_run(
                ShellRunRequest {
                    client_id: "oe".to_string(),
                    cwd: None,
                    command: "echo hi".to_string(),
                    stdin: None,
                    timeout_secs: 5,
                    wait_timeout_secs: 0,
                },
                "tester".to_string(),
            )
            .await
            .unwrap();
        let polled_a = registry
            .poll_for_connection(
                ShellAgentPollRequest {
                    client_id: "oe".to_string(),
                    agent_instance_id: "inst-x".to_string(),
                    projects: None,
                },
                "conn-a",
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(polled_a.request_id, request_id);

        // Same instance reconnects over B; B is now the current lease.
        register_with_connection(&registry, "oe", "inst-x", "conn-b").await;
        // Pin B's last_seen to an online-but-observable value. A refresh by a
        // successful connection-scoped operation would advance it to `now`; the
        // stale connection must leave it at the pinned value. Staying inside the
        // 60s online window keeps the later enqueue path valid.
        let pinned = chrono::Utc::now().timestamp() - 30;
        registry.set_last_seen_for_test("oe", pinned).await;

        // The late result arrives over stale connection A. It is accepted
        // (same instance) and resolves the waiter.
        registry
            .complete_for_connection(
                ShellAgentResultRequest {
                    client_id: "oe".to_string(),
                    agent_instance_id: "inst-x".to_string(),
                    request_id: request_id.clone(),
                    exit_code: Some(0),
                    stdout: Some("hi".to_string()),
                    stderr: None,
                    duration_ms: Some(1),
                    error: None,
                },
                "conn-a",
            )
            .await
            .unwrap();
        let response = rx.await.unwrap();
        assert!(response.success);
        // But it did NOT refresh B's liveness.
        assert_eq!(
            registry.get_client_view("oe").await.unwrap().last_seen,
            pinned,
            "late result on stale connection must not refresh new lease liveness"
        );

        // A cannot now poll a request enqueued after B's register. Enqueue a
        // new request under B's lease and verify A's poll is rejected.
        let (_new_request_id, _new_rx) = registry
            .enqueue_run(
                ShellRunRequest {
                    client_id: "oe".to_string(),
                    cwd: None,
                    command: "echo two".to_string(),
                    stdin: None,
                    timeout_secs: 5,
                    wait_timeout_secs: 0,
                },
                "tester".to_string(),
            )
            .await
            .unwrap();
        let err = registry
            .poll_for_connection(
                ShellAgentPollRequest {
                    client_id: "oe".to_string(),
                    agent_instance_id: "inst-x".to_string(),
                    projects: None,
                },
                "conn-a",
            )
            .await
            .unwrap_err();
        assert!(
            err.contains("transport connection is no longer active"),
            "{err}"
        );

        // B receives the new request.
        let polled_b = registry
            .poll_for_connection(
                ShellAgentPollRequest {
                    client_id: "oe".to_string(),
                    agent_instance_id: "inst-x".to_string(),
                    projects: None,
                },
                "conn-b",
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(polled_b.command, "echo two");
    }

    #[tokio::test]
    async fn late_job_update_on_stale_connection_is_accepted_without_refreshing_liveness() {
        // A job dispatched to A before the reconnect: its high-sequence job
        // update arriving over stale connection A is still applied (ownership
        // + update_seq), but does not refresh B's liveness. A replaced runner
        // instance is still rejected.
        let registry = ShellClientRegistry::default();
        register_with_connection(&registry, "oe", "inst-x", "conn-a").await;
        let job = registry
            .start_job(
                ShellJobOpRequest {
                    op: "start".to_string(),
                    client_id: Some("oe".to_string()),
                    cwd: None,
                    command: Some("sleep 10".to_string()),
                    timeout_secs: Some(10),
                    job_id: None,
                    since_stdout_line: None,
                    since_stderr_line: None,
                    tail_lines: None,
                    limit: None,
                    codex: None,
                },
                "tester".to_string(),
            )
            .await
            .unwrap();
        // A polls/dispatches the job (still current lease).
        registry
            .poll_for_connection(
                ShellAgentPollRequest {
                    client_id: "oe".to_string(),
                    agent_instance_id: "inst-x".to_string(),
                    projects: None,
                },
                "conn-a",
            )
            .await
            .unwrap()
            .unwrap();

        // Same instance reconnects over B.
        register_with_connection(&registry, "oe", "inst-x", "conn-b").await;
        // Pin to an online-but-observable value: a refresh would advance it to
        // `now`, but the stale connection must leave it pinned. Staying online
        // also prevents `get_job`'s status refresh from marking the active job
        // lost while we inspect it.
        let pinned = chrono::Utc::now().timestamp() - 30;
        registry.set_last_seen_for_test("oe", pinned).await;

        // Late job update over stale connection A is accepted and applied.
        registry
            .update_job_for_connection(
                ShellAgentJobUpdateRequest {
                    client_id: "oe".to_string(),
                    agent_instance_id: "inst-x".to_string(),
                    update_seq: None,
                    job_id: job.job_id.clone(),
                    request_id: None,
                    status: "running".to_string(),
                    stdout_chunk: None,
                    stderr_chunk: None,
                    stdout_tail: None,
                    stderr_tail: None,
                    log_snapshot: None,
                    exit_code: None,
                    duration_ms: None,
                    error: None,
                    validation_progress: None,
                    finished: false,
                },
                "conn-a",
            )
            .await
            .unwrap();
        assert_eq!(
            registry.get_job(&job.job_id).await.unwrap().status,
            "running"
        );
        // But B's liveness was not refreshed.
        assert_eq!(
            registry.get_client_view("oe").await.unwrap().last_seen,
            pinned,
            "late job update on stale connection must not refresh new lease liveness"
        );

        // A replaced runner instance is still rejected outright (a brand new
        // instance cannot submit updates for the old instance's job). Age the
        // old instance out so the replacement can take the lease.
        registry
            .set_last_seen_for_test("oe", chrono::Utc::now().timestamp() - 120)
            .await;
        register_with_instance(&registry, "oe", "inst-y").await;
        let err = registry
            .update_job(ShellAgentJobUpdateRequest {
                client_id: "oe".to_string(),
                agent_instance_id: "inst-x".to_string(),
                update_seq: None,
                job_id: job.job_id.clone(),
                request_id: None,
                status: "completed".to_string(),
                stdout_chunk: None,
                stderr_chunk: None,
                stdout_tail: None,
                stderr_tail: None,
                log_snapshot: None,
                exit_code: Some(0),
                duration_ms: Some(1),
                error: None,
                validation_progress: None,
                finished: true,
            })
            .await
            .unwrap_err();
        assert!(
            err.contains("no longer the active instance"),
            "replaced runner instance must be rejected, error was: {err}"
        );
    }
}
