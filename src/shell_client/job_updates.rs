use super::auth::{assert_shell_client_access, shell_job_visible_to_auth};
use super::jobs::{
    append_log_limited, assert_active_instance_locked, command_preview, is_final_job_status,
    job_view, refresh_job_status_locked, replace_log_limited, select_log_lines,
};
use super::reconciliation::validate_stream_snapshot;
use super::requests::{
    enqueue_pending_request_locked, next_request_id, notify_client_locked,
    remove_pending_request_locked,
};
use super::state::ShellJobRecord;
use super::validation::{validate_agent_instance_id, validate_id, validate_run_request};
use super::{now_ts, ShellClientRegistry};
use crate::shell_protocol::{
    validation_infrastructure_failure_code, ShellAgentJobUpdateRequest, ShellAgentShellRequest,
    ShellJobContext, ShellJobInfo, ShellJobOpRequest, ShellJobValidationStep, ShellRunRequest,
};
use std::collections::HashSet;
use uuid::Uuid;

#[derive(Clone, Copy)]
struct ValidationProtocolError(&'static str);

fn invalid_progress(code: &'static str) -> Result<(), ValidationProtocolError> {
    Err(ValidationProtocolError(code))
}

fn validate_validation_progress(
    job: &ShellJobRecord,
    update: &ShellAgentJobUpdateRequest,
) -> Result<(), ValidationProtocolError> {
    if job.validation_steps.is_empty() {
        return if update.validation_progress.is_none() {
            Ok(())
        } else {
            invalid_progress("validation_progress_unexpected")
        };
    }
    if job.validation_steps.iter().collect::<HashSet<_>>().len() != job.validation_steps.len() {
        return invalid_progress("validation_plan_invalid");
    }
    let status = update.status.trim();
    if update.finished && !is_final_job_status(status) {
        return invalid_progress("validation_progress_invalid");
    }
    let cancelling = matches!(
        status,
        "stopped" | "cancelled" | "timeout" | "timed_out" | "lost"
    );
    let Some(progress) = update.validation_progress.as_ref() else {
        if matches!(status, "queued" | "agent_queued") || cancelling {
            return Ok(());
        }
        return invalid_progress("validation_progress_missing");
    };
    if matches!(status, "queued" | "agent_queued")
        || progress.completed > job.validation_steps.len()
    {
        return invalid_progress("validation_progress_invalid");
    }
    let previous = job
        .validation_progress
        .as_ref()
        .map(|progress| progress.completed)
        .unwrap_or(0);
    if progress.completed < previous {
        return invalid_progress("validation_progress_invalid");
    }
    let no_active_step = progress.current_step.is_none() && progress.failed_step.is_none();
    let infrastructure_failure = status == "failed"
        && update.finished
        && update.exit_code.is_none()
        && update
            .error
            .as_deref()
            .and_then(validation_infrastructure_failure_code)
            .is_some();
    let valid = if cancelling {
        no_active_step
    } else if infrastructure_failure {
        progress.completed < job.validation_steps.len() && no_active_step
    } else if !is_final_job_status(status) {
        let expected = job.validation_steps.get(progress.completed);
        expected.map(String::as_str) == progress.current_step.as_deref()
            && progress.failed_step.is_none()
    } else if status == "completed" && update.exit_code == Some(0) {
        progress.completed == job.validation_steps.len() && no_active_step
    } else if status == "failed" {
        let expected = job.validation_steps.get(progress.completed);
        expected.map(String::as_str) == progress.failed_step.as_deref()
            && progress.current_step.is_none()
    } else {
        false
    };
    if valid {
        Ok(())
    } else if status == "completed" && update.exit_code == Some(0) {
        invalid_progress("validation_progress_incomplete")
    } else {
        invalid_progress("validation_progress_invalid")
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ShellJobStartMetadata {
    pub(crate) project_id: Option<String>,
    pub(crate) session_id: Option<String>,
    pub(crate) project_cwd: Option<String>,
    pub(crate) purpose: Option<String>,
    pub(crate) shell: Option<String>,
    pub(crate) validation_steps: Vec<ShellJobValidationStep>,
    pub(crate) sandbox: Option<String>,
}

impl ShellClientRegistry {
    pub async fn start_job(
        &self,
        body: ShellJobOpRequest,
        requested_by: String,
    ) -> Result<ShellJobInfo, String> {
        self.start_job_with_metadata(body, requested_by, ShellJobStartMetadata::default())
            .await
    }

    pub(crate) async fn start_job_with_metadata(
        &self,
        body: ShellJobOpRequest,
        requested_by: String,
        metadata: ShellJobStartMetadata,
    ) -> Result<ShellJobInfo, String> {
        self.start_job_with_metadata_for_auth(body, requested_by, metadata, None)
            .await
    }

    pub(crate) async fn start_job_with_metadata_for_auth(
        &self,
        body: ShellJobOpRequest,
        requested_by: String,
        metadata: ShellJobStartMetadata,
        auth: Option<&crate::auth::AuthContext>,
    ) -> Result<ShellJobInfo, String> {
        let client_id = body
            .client_id
            .clone()
            .ok_or_else(|| "client_id is required for op=start".to_string())?;
        let command = body
            .command
            .clone()
            .ok_or_else(|| "command is required for op=start".to_string())?;
        let run = ShellRunRequest {
            client_id: client_id.clone(),
            cwd: body.cwd.clone(),
            command: command.clone(),
            stdin: None,
            timeout_secs: body.timeout_secs.unwrap_or(120),
            wait_timeout_secs: 0,
        };
        validate_run_request(&run)?;
        let request_id = next_request_id();
        let job_id = Uuid::new_v4().to_string();
        let created_at = now_ts();
        let sandbox = metadata.sandbox;
        let validation_steps = metadata.validation_steps;
        if validation_steps.len() > 3
            || validation_steps.iter().any(|step| !step.is_canonical())
            || validation_steps
                .iter()
                .map(|step| step.name.as_str())
                .collect::<HashSet<_>>()
                .len()
                != validation_steps.len()
        {
            return Err("invalid structured validation plan".to_string());
        }
        let request_kind = if validation_steps.is_empty() {
            "start_job"
        } else {
            "start_validation_job"
        };
        let request_command = if validation_steps.is_empty() {
            command.clone()
        } else {
            serde_json::to_string(&validation_steps)
                .map_err(|error| format!("could not serialize validation plan: {error}"))?
        };
        let validation_step_names = validation_steps
            .iter()
            .map(|step| step.name.clone())
            .collect::<Vec<_>>();
        let normalized_job_cwd = run.cwd.clone().map(|cwd| cwd.trim().to_string());
        let safe_command_preview = if validation_steps.is_empty() {
            command_preview(&run.command)
        } else {
            format!("validation: {}", validation_step_names.join(", "))
        };
        let job_context = ShellJobContext {
            runtime_project_id: metadata.project_id.clone(),
            workflow_session_id: metadata.session_id.clone(),
            project_cwd: metadata.project_cwd.clone(),
            cwd: normalized_job_cwd.clone(),
            purpose: metadata.purpose.clone(),
            shell: metadata.shell.clone(),
            command_preview: safe_command_preview.clone(),
            validation_steps: validation_step_names.clone(),
        };
        let request = ShellAgentShellRequest {
            request_id: request_id.clone(),
            client_id: client_id.clone(),
            kind: request_kind.to_string(),
            job_id: Some(job_id.clone()),
            cwd: normalized_job_cwd.clone(),
            path: None,
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
            command: request_command,
            stdin: None,
            timeout_secs: run.timeout_secs,
            requested_by,
            created_at,
            validation: None,
            lsp: None,
            sandbox,
            job_context: Some(job_context),
        };
        let mut inner = self.inner.lock().await;
        let Some(client) = inner.clients.get(&client_id) else {
            return Err(format!("unknown shell client: {}", client_id));
        };
        if auth.is_some() {
            assert_shell_client_access(auth, client)?;
        }
        if !(client.capabilities.async_jobs || client.capabilities.async_shell_jobs) {
            return Err(format!(
                "agent client {} does not support async shell jobs",
                client_id
            ));
        }
        if let Some(mode) = request.sandbox.as_deref() {
            if mode != crate::command_sandbox::INSPECT_SANDBOX_MODE {
                return Err(format!("unknown sandbox mode '{mode}'"));
            }
            if !client.capabilities.sandbox_inspect_commands {
                return Err(format!(
                    "{}: agent client {} cannot enforce the inspect sandbox",
                    crate::shell_protocol::SHELL_CLIENT_CAPABILITY_SANDBOX_INSPECT_COMMANDS,
                    client_id
                ));
            }
        }
        if !validation_steps.is_empty() && !client.capabilities.structured_validation_argv {
            return Err(format!(
                "structured_validation_unavailable: agent client {} does not support structured argv validation jobs",
                client_id
            ));
        }
        if metadata
            .project_id
            .as_deref()
            .is_some_and(|project| inner.unregistering_projects.contains_key(project))
        {
            return Err("project_unregister_in_progress".to_string());
        }
        let agent_instance_id = client.agent_instance_id.clone();
        enqueue_pending_request_locked(
            &mut inner,
            &client_id,
            request_id.clone(),
            request,
            None,
            Some(job_id.clone()),
        )?;
        let job = ShellJobRecord {
            job_id: job_id.clone(),
            request_id: Some(request_id.clone()),
            client_id: client_id.clone(),
            agent_instance_id,
            kind: "shell".to_string(),
            project_id: metadata.project_id,
            session_id: metadata.session_id,
            cwd: normalized_job_cwd,
            project_cwd: metadata.project_cwd,
            purpose: metadata.purpose,
            shell: metadata.shell,
            command_preview: safe_command_preview,
            status: "queued".to_string(),
            created_at,
            started_at: None,
            ended_at: None,
            exit_code: None,
            duration_ms: None,
            stdout: Default::default(),
            stderr: Default::default(),
            error: None,
            codex: body.codex.clone(),
            validation_steps: validation_step_names,
            validation_progress: None,
            last_update_seq: 0,
            recovery_state: None,
            recovered_after_server_restart: false,
            reconciled_at: None,
            recovery_reason_code: None,
            recovering_since: None,
            recovery_original_status: None,
        };
        inner.request_to_job.insert(request_id, job_id.clone());
        inner.jobs_by_id.insert(job_id.clone(), job);
        notify_client_locked(&inner, &client_id);
        Ok(job_view(
            inner.jobs_by_id.get(&job_id).expect("job just inserted"),
        ))
    }

    pub async fn get_job(&self, job_id: &str) -> Result<ShellJobInfo, String> {
        self.get_job_for_auth(None, job_id).await
    }

    pub(crate) async fn get_job_for_auth(
        &self,
        auth: Option<&crate::auth::AuthContext>,
        job_id: &str,
    ) -> Result<ShellJobInfo, String> {
        validate_id(job_id, "job_id")?;
        let mut inner = self.inner.lock().await;
        refresh_job_status_locked(&mut inner, job_id);
        let Some(job) = inner.jobs_by_id.get(job_id) else {
            return Err(format!("unknown shell job: {}", job_id));
        };
        if !shell_job_visible_to_auth(auth, &inner, &job.client_id) {
            return Err(format!("unknown shell job: {}", job_id));
        }
        Ok(job_view(job))
    }

    pub async fn list_jobs(&self, limit: Option<usize>) -> Vec<ShellJobInfo> {
        self.list_jobs_for_auth(None, limit).await
    }

    pub(crate) async fn list_jobs_for_auth(
        &self,
        auth: Option<&crate::auth::AuthContext>,
        limit: Option<usize>,
    ) -> Vec<ShellJobInfo> {
        let mut inner = self.inner.lock().await;
        let job_ids = inner.jobs_by_id.keys().cloned().collect::<Vec<_>>();
        for job_id in job_ids {
            refresh_job_status_locked(&mut inner, &job_id);
        }
        let mut jobs = inner
            .jobs_by_id
            .values()
            .filter(|job| shell_job_visible_to_auth(auth, &inner, &job.client_id))
            .cloned()
            .collect::<Vec<_>>();
        jobs.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        jobs.into_iter()
            .take(limit.unwrap_or(20).clamp(1, 100))
            .map(|job| job_view(&job))
            .collect()
    }

    /// Count active jobs for one exact runtime project without applying the
    /// display-list pagination limit. Local-only jobs do not carry an agent
    /// runtime project id and are intentionally excluded.
    pub(crate) async fn count_active_jobs_for_project(
        &self,
        auth: Option<&crate::auth::AuthContext>,
        runtime_project_id: &str,
    ) -> usize {
        let mut inner = self.inner.lock().await;
        let job_ids = inner.jobs_by_id.keys().cloned().collect::<Vec<_>>();
        for job_id in job_ids {
            refresh_job_status_locked(&mut inner, &job_id);
        }
        inner
            .jobs_by_id
            .values()
            .filter(|job| shell_job_visible_to_auth(auth, &inner, &job.client_id))
            .filter(|job| job.project_id.as_deref() == Some(runtime_project_id))
            .filter(|job| crate::tool_runtime::ACTIVE_JOB_STATUSES.contains(&job.status.as_str()))
            .count()
    }

    /// Atomically fence new job starts and count all currently active jobs for
    /// a runtime project. The fence remains until `end_project_unregister`.
    pub(crate) async fn begin_project_unregister(
        &self,
        auth: Option<&crate::auth::AuthContext>,
        runtime_project_id: &str,
    ) -> Result<usize, String> {
        let mut inner = self.inner.lock().await;
        let job_ids = inner.jobs_by_id.keys().cloned().collect::<Vec<_>>();
        for job_id in job_ids {
            refresh_job_status_locked(&mut inner, &job_id);
        }
        let active = inner
            .jobs_by_id
            .values()
            .filter(|job| shell_job_visible_to_auth(auth, &inner, &job.client_id))
            .filter(|job| job.project_id.as_deref() == Some(runtime_project_id))
            .filter(|job| crate::tool_runtime::ACTIVE_JOB_STATUSES.contains(&job.status.as_str()))
            .count();
        if active == 0 {
            *inner
                .unregistering_projects
                .entry(runtime_project_id.to_string())
                .or_insert(0) += 1;
        }
        Ok(active)
    }

    pub(crate) async fn end_project_unregister(&self, runtime_project_id: &str) {
        let mut inner = self.inner.lock().await;
        if let Some(count) = inner.unregistering_projects.get_mut(runtime_project_id) {
            *count -= 1;
            if *count == 0 {
                inner.unregistering_projects.remove(runtime_project_id);
            }
        }
    }

    pub async fn list_jobs_for_client(
        &self,
        client_id: &str,
        status: Option<&str>,
        limit: Option<usize>,
    ) -> Result<Vec<ShellJobInfo>, String> {
        validate_id(client_id, "client_id")?;
        let mut inner = self.inner.lock().await;
        if !inner.clients.contains_key(client_id) {
            return Err(format!("unknown shell client: {}", client_id));
        }
        let job_ids = inner.jobs_by_id.keys().cloned().collect::<Vec<_>>();
        for job_id in job_ids {
            refresh_job_status_locked(&mut inner, &job_id);
        }
        let mut jobs = inner
            .jobs_by_id
            .values()
            .filter(|job| job.client_id == client_id)
            .filter(|job| status.map(|status| status == job.status).unwrap_or(true))
            .cloned()
            .collect::<Vec<_>>();
        jobs.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        Ok(jobs
            .into_iter()
            .take(limit.unwrap_or(20).clamp(1, 100))
            .map(|job| job_view(&job))
            .collect())
    }

    pub async fn job_log(
        &self,
        job_id: &str,
        since_stdout_line: Option<usize>,
        since_stderr_line: Option<usize>,
        tail_lines: Option<usize>,
    ) -> Result<(ShellJobInfo, Option<String>, Option<String>, usize, usize), String> {
        self.job_log_for_auth(
            None,
            job_id,
            since_stdout_line,
            since_stderr_line,
            tail_lines,
        )
        .await
    }

    pub(crate) async fn job_log_for_auth(
        &self,
        auth: Option<&crate::auth::AuthContext>,
        job_id: &str,
        since_stdout_line: Option<usize>,
        since_stderr_line: Option<usize>,
        tail_lines: Option<usize>,
    ) -> Result<(ShellJobInfo, Option<String>, Option<String>, usize, usize), String> {
        validate_id(job_id, "job_id")?;
        let mut inner = self.inner.lock().await;
        refresh_job_status_locked(&mut inner, job_id);
        let Some(job) = inner.jobs_by_id.get(job_id) else {
            return Err(format!("unknown shell job: {}", job_id));
        };
        if !shell_job_visible_to_auth(auth, &inner, &job.client_id) {
            return Err(format!("unknown shell job: {}", job_id));
        }
        let (stdout, next_stdout_line, _, _) =
            select_log_lines(&job.stdout, since_stdout_line, tail_lines);
        let (stderr, next_stderr_line, _, _) =
            select_log_lines(&job.stderr, since_stderr_line, tail_lines);
        Ok((
            job_view(job),
            stdout,
            stderr,
            next_stdout_line,
            next_stderr_line,
        ))
    }

    pub async fn stop_job(
        &self,
        job_id: &str,
        requested_by: String,
    ) -> Result<ShellJobInfo, String> {
        self.stop_job_for_auth(None, job_id, requested_by).await
    }

    pub(crate) async fn stop_job_for_auth(
        &self,
        auth: Option<&crate::auth::AuthContext>,
        job_id: &str,
        requested_by: String,
    ) -> Result<ShellJobInfo, String> {
        validate_id(job_id, "job_id")?;
        let mut inner = self.inner.lock().await;
        refresh_job_status_locked(&mut inner, job_id);
        let Some(job) = inner.jobs_by_id.get(job_id).cloned() else {
            return Err(format!("unknown shell job: {}", job_id));
        };
        if !shell_job_visible_to_auth(auth, &inner, &job.client_id) {
            return Err(format!("unknown shell job: {}", job_id));
        }
        match job.status.as_str() {
            "queued" => {
                if let Some(request_id) = &job.request_id {
                    remove_pending_request_locked(&mut inner, request_id);
                    inner.request_to_job.remove(request_id);
                }
                let job = inner.jobs_by_id.get_mut(job_id).expect("job exists");
                job.status = "stopped".to_string();
                job.ended_at = Some(now_ts());
                job.error = Some("job stopped before agent picked it up".to_string());
                Ok(job_view(job))
            }
            "agent_queued" | "running" | "stop_requested" => {
                let stop_request_id = next_request_id();
                let client_id = job.client_id.clone();
                let request = ShellAgentShellRequest {
                    request_id: stop_request_id.clone(),
                    client_id: client_id.clone(),
                    kind: "stop_job".to_string(),
                    job_id: Some(job_id.to_string()),
                    cwd: None,
                    path: None,
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
                    command: String::new(),
                    stdin: None,
                    timeout_secs: 1,
                    requested_by,
                    created_at: now_ts(),
                    validation: None,
                    lsp: None,
                    sandbox: None,
                    job_context: None,
                };
                enqueue_pending_request_locked(
                    &mut inner,
                    &client_id,
                    stop_request_id,
                    request,
                    None,
                    Some(job_id.to_string()),
                )?;
                let job = inner.jobs_by_id.get_mut(job_id).expect("job exists");
                job.status = "stop_requested".to_string();
                job.error = Some("stop requested".to_string());
                let notify_client_id = job.client_id.clone();
                notify_client_locked(&inner, &notify_client_id);
                Ok(job_view(inner.jobs_by_id.get(job_id).expect("job exists")))
            }
            "recovering" => Err(
                "runner_unavailable_recovering: wait for same-instance job reconciliation before retrying stop_job"
                    .to_string(),
            ),
            _ => Ok(job_view(inner.jobs_by_id.get(job_id).expect("job exists"))),
        }
    }

    /// Polling-transport job update entry point. Job ownership and update
    /// sequence rules decide acceptance; this path refreshes `last_seen` for
    /// the active instance. Used by the HTTP `/job_update` handler.
    pub async fn update_job(
        &self,
        body: ShellAgentJobUpdateRequest,
    ) -> Result<ShellJobInfo, String> {
        self.update_job_checked(body, None).await
    }

    /// Connection-scoped job update entry point for long-lived transports.
    /// Acceptance still follows the existing `agent_instance_id`, job
    /// ownership and `update_seq` rules so a legitimately-dispatched job's
    /// late update is not dropped just because the transport connection was
    /// replaced. A late update arriving on a stale same-instance connection,
    /// however, must not refresh the new connection's `last_seen` liveness.
    pub(crate) async fn update_job_for_connection(
        &self,
        body: ShellAgentJobUpdateRequest,
        connection_id: &str,
    ) -> Result<ShellJobInfo, String> {
        self.update_job_checked(body, Some(connection_id)).await
    }

    async fn update_job_checked(
        &self,
        body: ShellAgentJobUpdateRequest,
        expected_connection_id: Option<&str>,
    ) -> Result<ShellJobInfo, String> {
        validate_id(&body.client_id, "client_id")?;
        validate_id(&body.job_id, "job_id")?;
        validate_agent_instance_id(&body.agent_instance_id)?;
        if let Some(snapshot) = body.log_snapshot.as_ref() {
            validate_stream_snapshot(&snapshot.stdout, "job update stdout snapshot")?;
            validate_stream_snapshot(&snapshot.stderr, "job update stderr snapshot")?;
            if body.stdout_chunk.is_some()
                || body.stderr_chunk.is_some()
                || body.stdout_tail.is_some()
                || body.stderr_tail.is_some()
            {
                return Err(
                    "job update log_snapshot cannot be combined with chunk or legacy tail fields"
                        .to_string(),
                );
            }
        }
        let mut inner = self.inner.lock().await;
        // Reject job updates from a stale/replaced instance before refreshing
        // liveness or mutating job state.
        assert_active_instance_locked(&inner, &body.client_id, &body.agent_instance_id)?;
        let sequenced = inner
            .clients
            .get(&body.client_id)
            .is_some_and(|client| client.capabilities.job_state_reconciliation);
        let incoming_seq = if sequenced {
            Some(
                body.update_seq
                    .filter(|sequence| *sequence > 0)
                    .ok_or_else(|| {
                        "job_state_reconciliation update requires update_seq".to_string()
                    })?,
            )
        } else {
            body.update_seq
        };
        let incoming_status = body.status.trim();
        if sequenced && body.status != incoming_status {
            return Err(
                "job_state_reconciliation update status must be canonical without surrounding whitespace"
                    .to_string(),
            );
        }
        if sequenced
            && !matches!(
                incoming_status,
                "agent_queued"
                    | "running"
                    | "stop_requested"
                    | "completed"
                    | "failed"
                    | "stopped"
                    | "timeout"
                    | "timed_out"
                    | "cancelled"
                    | "lost"
            )
        {
            return Err(format!(
                "job_state_reconciliation update status '{}' is invalid",
                incoming_status
            ));
        }
        if sequenced && body.finished != is_final_job_status(incoming_status) {
            return Err(
                "job_state_reconciliation update has inconsistent finished/status".to_string(),
            );
        }
        if sequenced
            && !is_final_job_status(incoming_status)
            && (body.exit_code.is_some() || body.duration_ms.is_some())
        {
            return Err(
                "active job_state_reconciliation update contains terminal result fields"
                    .to_string(),
            );
        }
        if sequenced && incoming_status == "completed" && body.exit_code != Some(0) {
            return Err(
                "completed job_state_reconciliation update requires exit_code=0".to_string(),
            );
        }
        // Refresh liveness only for the connection that currently holds the
        // transport lease. A late job update on a stale same-instance
        // connection is still applied below, but it must not make the new
        // connection appear online.
        if expected_connection_id.is_none()
            || inner
                .clients
                .get(&body.client_id)
                .is_some_and(|client| client.connection_id.as_deref() == expected_connection_id)
        {
            if let Some(client) = inner.clients.get_mut(&body.client_id) {
                client.last_seen = now_ts();
            }
        }
        let mut request_id_to_remove = None;
        let view = {
            let Some(job) = inner.jobs_by_id.get_mut(&body.job_id) else {
                return Err(format!("unknown shell job: {}", body.job_id));
            };
            if job.client_id != body.client_id {
                return Err("job_id does not belong to client_id".to_string());
            }
            if job.agent_instance_id != body.agent_instance_id {
                return Err("job_id belongs to a replaced runner instance".to_string());
            }
            if body
                .request_id
                .as_deref()
                .is_some_and(|request_id| job.request_id.as_deref() != Some(request_id))
            {
                return Err("job update request_id does not match job_id".to_string());
            }
            if body.log_snapshot.is_some() && !sequenced {
                return Err(
                    "job update log_snapshot requires job_state_reconciliation capability"
                        .to_string(),
                );
            }
            if job.status == "recovering" && sequenced && body.log_snapshot.is_none() {
                return Err(
                    "recovering job update requires an authoritative log_snapshot or register inventory"
                        .to_string(),
                );
            }
            if sequenced && incoming_seq.is_some_and(|sequence| sequence <= job.last_update_seq) {
                return Ok(job_view(job));
            }
            if is_final_job_status(&job.status) {
                // Terminal class is server-authoritative once accepted.
                return Ok(job_view(job));
            }
            if body.log_snapshot.as_ref().is_some_and(|snapshot| {
                snapshot.stdout.next_line < job.stdout.next_line
                    || snapshot.stderr.next_line < job.stderr.next_line
            }) {
                return Err(
                    "job update authoritative log snapshot regresses an absolute cursor"
                        .to_string(),
                );
            }
            if let Err(error) = validate_validation_progress(job, &body) {
                job.status = "failed".to_string();
                job.ended_at = Some(now_ts());
                job.exit_code = body.exit_code;
                job.duration_ms = body.duration_ms;
                job.error = Some(format!("executor protocol violation: {}", error.0));
                request_id_to_remove = job.request_id.clone();
            } else {
                let was_recovering = job.status == "recovering";
                if let Some(snapshot) = body.log_snapshot {
                    super::jobs::replace_log_from_snapshot(&mut job.stdout, &snapshot.stdout);
                    super::jobs::replace_log_from_snapshot(&mut job.stderr, &snapshot.stderr);
                } else {
                    replace_log_limited(&mut job.stdout, body.stdout_tail);
                    replace_log_limited(&mut job.stderr, body.stderr_tail);
                    append_log_limited(&mut job.stdout, body.stdout_chunk);
                    append_log_limited(&mut job.stderr, body.stderr_chunk);
                }
                if body.validation_progress.is_some() {
                    job.validation_progress = body.validation_progress.clone();
                }
                if job.started_at.is_none()
                    && matches!(
                        body.status.as_str(),
                        "running" | "completed" | "failed" | "stopped" | "timeout"
                    )
                {
                    job.started_at = Some(now_ts());
                }
                if !body.status.trim().is_empty() && !is_final_job_status(&job.status) {
                    let incoming_status = body.status.trim();
                    job.status = if incoming_status == "queued" && job.started_at.is_some() {
                        "agent_queued".to_string()
                    } else {
                        incoming_status.to_string()
                    };
                }
                if is_final_job_status(&body.status) {
                    job.status = body.status;
                    job.ended_at = Some(now_ts());
                    job.exit_code = body.exit_code;
                    job.duration_ms = body.duration_ms;
                    job.error = body.error;
                    job.recovery_state = job.reconciled_at.map(|_| "reconciled".to_string());
                    job.recovering_since = None;
                    job.recovery_original_status = None;
                    request_id_to_remove = job.request_id.clone();
                } else if body.error.is_some() {
                    job.error = body.error;
                }
                if body.finished && !is_final_job_status(&job.status) {
                    job.status = if job.error.is_none() && job.exit_code == Some(0) {
                        "completed".to_string()
                    } else {
                        "failed".to_string()
                    };
                    job.ended_at = Some(now_ts());
                    request_id_to_remove = job.request_id.clone();
                }
                if was_recovering {
                    job.recovery_state = Some("reconciled".to_string());
                    job.reconciled_at = Some(now_ts());
                    job.recovery_reason_code =
                        Some("same_instance_update_reconciliation".to_string());
                    job.recovering_since = None;
                    job.recovery_original_status = None;
                }
            }
            if let Some(sequence) = incoming_seq {
                job.last_update_seq = sequence;
            }
            job_view(job)
        };
        if let Some(request_id) = request_id_to_remove {
            inner.pending_by_id.remove(&request_id);
            inner.request_to_job.remove(&request_id);
        }
        Ok(view)
    }
}
