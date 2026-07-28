use serde_json::json;
use std::time::Duration;

use super::helpers::{
    bounded_tail, command_failed_message, command_rejected_message, command_timeout_message,
    looks_like_command_timeout, project_relative_agent_cwd, project_relative_cwd,
    resolve_agent_cwd, resolve_local_cwd, resolve_sync_timeout_secs,
    run_command_sync_bounded_with_shell_and_sandbox, shell_escape_simple,
    sync_timeout_out_of_range_result, LocalRunFailure, COMMAND_STDIO_TAIL_CHARS,
    DEFAULT_RUN_SHELL_TIMEOUT_SECS, MAX_SYNC_TIMEOUT_SECS, MIN_SYNC_TIMEOUT_SECS,
};
use super::tool_result::ToolResult;
use super::{ExecutionPurpose, ExecutionShell, ToolRuntime};
use crate::shell_client::command_preview;
use crate::shell_protocol::ShellRunRequest;

pub(crate) struct ProjectCommandOutput {
    pub(crate) exit_code: Option<i32>,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
    pub(crate) duration_ms: u64,
    pub(crate) error: Option<String>,
    pub(crate) command_started: bool,
    pub(crate) command_completed: bool,
}

impl ToolRuntime {
    fn run_shell_success_output(
        exit_code: i32,
        stdout: String,
        stderr: String,
        duration_ms: Option<u64>,
    ) -> serde_json::Value {
        let (stdout_tail, stdout_truncated) = bounded_tail(&stdout, COMMAND_STDIO_TAIL_CHARS);
        let (stderr_tail, stderr_truncated) = bounded_tail(&stderr, COMMAND_STDIO_TAIL_CHARS);
        json!({
            "exit_code": exit_code,
            "stdout_tail": stdout_tail,
            "stderr_tail": stderr_tail,
            "stdout_lines": stdout.lines().count(),
            "stderr_lines": stderr.lines().count(),
            "stdout_truncated": stdout_truncated,
            "stderr_truncated": stderr_truncated,
            "duration_ms": duration_ms,
            "command_started": true,
            "command_completed": true,
            "command_ok": true,
            "failure_kind": null,
            "tool_failure": false,
        })
    }

    fn run_shell_command_failure_result(
        exit_code: Option<i32>,
        stdout: String,
        stderr: String,
        duration_ms: Option<u64>,
        timeout_secs: u64,
    ) -> ToolResult {
        let (stdout_tail, stdout_truncated) = bounded_tail(&stdout, COMMAND_STDIO_TAIL_CHARS);
        let (stderr_tail, stderr_truncated) = bounded_tail(&stderr, COMMAND_STDIO_TAIL_CHARS);
        let timed_out = looks_like_command_timeout(exit_code, &stderr, timeout_secs);
        let output = json!({
            "exit_code": exit_code,
            "duration_ms": duration_ms,
            "stdout_tail": stdout_tail,
            "stderr_tail": stderr_tail,
            "stdout_lines": stdout.lines().count(),
            "stderr_lines": stderr.lines().count(),
            "stdout_truncated": stdout_truncated,
            "stderr_truncated": stderr_truncated,
            "command_started": true,
            "command_completed": !timed_out,
            "command_ok": false,
            "failure_kind": if timed_out { "timeout" } else { "command_exit_nonzero" },
            "tool_failure": false,
        });
        let error = if timed_out {
            command_timeout_message(timeout_secs, &stdout_tail, &stderr_tail)
        } else {
            command_failed_message(exit_code, &stdout_tail, &stderr_tail)
        };
        ToolResult {
            success: false,
            output,
            error: Some(error),
        }
    }

    fn run_shell_tool_failure_result(
        message: String,
        failure_kind: &'static str,
        command_started: bool,
        command_completed: bool,
    ) -> ToolResult {
        ToolResult::err_with_output(
            message,
            json!({
                "command_started": command_started,
                "command_completed": command_completed,
                "command_ok": false,
                "exit_code": null,
                "failure_kind": failure_kind,
                "tool_failure": true,
            }),
        )
    }

    fn classify_run_shell_enqueue_failure(message: &str) -> &'static str {
        let lower = message.to_ascii_lowercase();
        if lower.contains("offline")
            || lower.contains("not connected")
            || lower.contains("no connected")
            || lower.contains("unknown agent")
            || lower.contains("unknown_project")
        {
            "agent_offline"
        } else if lower.contains("permission")
            || lower.contains("denied")
            || lower.contains("outside")
            || lower.contains("not allowed")
        {
            "permission_denied"
        } else if lower.contains("timeout") || lower.contains("timed out") {
            "timeout"
        } else {
            "runtime_error"
        }
    }

    pub(crate) async fn run_project_command_capture(
        &self,
        project: &str,
        command: String,
        timeout_secs: u64,
        cwd: Option<String>,
    ) -> Result<ProjectCommandOutput, String> {
        self.run_project_command_capture_with_sandbox(project, command, timeout_secs, cwd, None)
            .await
    }

    pub(crate) async fn run_project_command_capture_with_sandbox(
        &self,
        project: &str,
        command: String,
        timeout_secs: u64,
        cwd: Option<String>,
        sandbox: Option<&str>,
    ) -> Result<ProjectCommandOutput, String> {
        let proj = self.resolve_project(project).await?;
        // Shared root of the sync agent-wait contract: wait_timeout_secs and
        // command timeout must both stay within 1..=120 before enqueue so
        // shell_client validation never rejects with implementation-detail
        // errors about runShell.
        if !(MIN_SYNC_TIMEOUT_SECS..=MAX_SYNC_TIMEOUT_SECS).contains(&timeout_secs) {
            return Err(format!(
                "timeout_secs must be between {MIN_SYNC_TIMEOUT_SECS} and {MAX_SYNC_TIMEOUT_SECS}"
            ));
        }
        let timeout = timeout_secs;
        if proj.is_agent() {
            let client_id = proj.agent_client_id()?.to_string();
            let effective_cwd = Some(resolve_agent_cwd(&proj, cwd.as_deref())?);
            let wait_timeout = timeout;
            let (request_id, rx) = self
                .shell_clients
                .enqueue_run_with_sandbox(
                    ShellRunRequest {
                        client_id,
                        cwd: effective_cwd,
                        command,
                        stdin: None,
                        timeout_secs: timeout,
                        wait_timeout_secs: wait_timeout,
                    },
                    "tool_runtime".to_string(),
                    sandbox.map(str::to_string),
                )
                .await?;
            match tokio::time::timeout(Duration::from_secs(wait_timeout + 2), rx).await {
                Ok(Ok(response)) => {
                    let exit_code = response.exit_code;
                    let stderr = response.stderr.unwrap_or_default();
                    let timed_out = looks_like_command_timeout(exit_code, &stderr, timeout);
                    Ok(ProjectCommandOutput {
                        exit_code,
                        stdout: response.stdout.unwrap_or_default(),
                        stderr,
                        duration_ms: response.duration_ms.unwrap_or_default(),
                        command_started: exit_code.is_some(),
                        command_completed: !timed_out,
                        error: response.error,
                    })
                }
                Ok(Err(_)) => {
                    self.shell_clients.cancel_request(&request_id).await;
                    Err("shell request waiter was dropped".to_string())
                }
                Err(_) => {
                    let command_started = self.shell_clients.cancel_request(&request_id).await;
                    Ok(ProjectCommandOutput {
                        exit_code: None,
                        stdout: String::new(),
                        stderr: String::new(),
                        duration_ms: wait_timeout.saturating_mul(1_000),
                        error: Some(format!(
                            "timed out waiting {wait_timeout} seconds for agent shell result"
                        )),
                        command_started,
                        command_completed: false,
                    })
                }
            }
        } else {
            let cwd_path = resolve_local_cwd(&proj, cwd.as_deref())?;
            let result = run_command_sync_bounded_with_shell_and_sandbox(
                command,
                cwd_path,
                timeout,
                "sh".to_string(),
                sandbox.map(str::to_string),
            )
                .await
                .map_err(|failure| match failure {
                    LocalRunFailure::HardTimeout { bound_secs } => format!(
                        "local command did not return within {} seconds (hard bound); an orphaned background process may still be holding its output pipes",
                        bound_secs
                    ),
                    LocalRunFailure::Join(e) => format!("task join error: {}", e),
                })?;
            let timed_out = looks_like_command_timeout(Some(result.0), &result.2, timeout);
            Ok(ProjectCommandOutput {
                exit_code: Some(result.0),
                stdout: result.1,
                stderr: result.2,
                duration_ms: result.3,
                error: None,
                command_started: true,
                command_completed: !timed_out,
            })
        }
    }

    pub(crate) async fn run_shell(
        &self,
        project: String,
        command: String,
        timeout_secs: Option<u64>,
        cwd: Option<String>,
    ) -> ToolResult {
        self.run_shell_with_contract(project, command, timeout_secs, cwd, None, None)
            .await
    }

    pub(crate) async fn run_shell_with_contract(
        &self,
        project: String,
        command: String,
        timeout_secs: Option<u64>,
        cwd: Option<String>,
        purpose: Option<ExecutionPurpose>,
        shell: Option<ExecutionShell>,
    ) -> ToolResult {
        self.run_shell_with_contract_in_sandbox(
            project,
            command,
            timeout_secs,
            cwd,
            purpose,
            shell,
            None,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn run_shell_with_contract_in_sandbox(
        &self,
        project: String,
        command: String,
        timeout_secs: Option<u64>,
        cwd: Option<String>,
        purpose: Option<ExecutionPurpose>,
        shell: Option<ExecutionShell>,
        sandbox: Option<&str>,
    ) -> ToolResult {
        let timeout = match resolve_sync_timeout_secs(timeout_secs, DEFAULT_RUN_SHELL_TIMEOUT_SECS)
        {
            Ok(timeout) => timeout,
            Err(_) => {
                return sync_timeout_out_of_range_result(
                    "run_shell",
                    DEFAULT_RUN_SHELL_TIMEOUT_SECS,
                )
            }
        };
        let proj = match self.resolve_project(&project).await {
            Ok(p) => p,
            Err(e) => {
                return Self::run_shell_tool_failure_result(
                    command_rejected_message(
                        e.to_message(),
                        "verify the project id with list_projects, then retry with a registered project.",
                    ),
                    "agent_offline",
                    false,
                    false,
                )
            }
        };
        let declared_purpose = purpose.unwrap_or_default();
        let command_summary = command_preview(&command);
        if proj.is_agent() {
            let client_id =
                match proj.agent_client_id() {
                    Ok(id) => id.to_string(),
                    Err(e) => return Self::run_shell_tool_failure_result(
                        command_rejected_message(
                            e,
                            "refresh the agent project registry with list_projects, then retry.",
                        ),
                        "agent_offline",
                        false,
                        false,
                    ),
                };
            let effective_cwd = match resolve_agent_cwd(&proj, cwd.as_deref()) {
                Ok(cwd) => cwd,
                Err(e) => {
                    return Self::run_shell_tool_failure_result(
                        command_rejected_message(
                            e,
                            "choose '.', an existing project-relative cwd, or an absolute path inside the registered project root.",
                        ),
                        "permission_denied",
                        false,
                        false,
                    )
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
            let wait_timeout = timeout;
            let (request_id, rx) = match self
                .shell_clients
                .enqueue_run_with_sandbox(
                    ShellRunRequest {
                        client_id,
                        cwd: Some(effective_cwd),
                        command: dispatched_command,
                        stdin: None,
                        timeout_secs: timeout,
                        wait_timeout_secs: wait_timeout,
                    },
                    "tool_runtime".to_string(),
                    sandbox.map(str::to_string),
                )
                .await
            {
                Ok(result) => result,
                Err(e) => {
                    let failure_kind = Self::classify_run_shell_enqueue_failure(&e);
                    return Self::run_shell_tool_failure_result(
                        command_rejected_message(
                            e,
                            "confirm the agent is connected and the command request is allowed, then retry or use run_job for long-running work.",
                        ),
                        failure_kind,
                        false,
                        false,
                    );
                }
            };
            match tokio::time::timeout(Duration::from_secs(wait_timeout + 2), rx).await {
                Ok(Ok(response)) => {
                    let success = response.error.is_none() && response.exit_code == Some(0);
                    let mut result = if success {
                        ToolResult::ok(Self::run_shell_success_output(
                            0,
                            response.stdout.unwrap_or_default(),
                            response.stderr.unwrap_or_default(),
                            response.duration_ms,
                        ))
                    } else if let Some(error) = response.error {
                        Self::run_shell_tool_failure_result(
                            command_rejected_message(
                                &error,
                                "inspect the rejection reason, adjust the cwd/command/project, then retry.",
                            ),
                            Self::classify_run_shell_enqueue_failure(&error),
                            false,
                            false,
                        )
                    } else {
                        Self::run_shell_command_failure_result(
                            response.exit_code,
                            response.stdout.unwrap_or_default(),
                            response.stderr.unwrap_or_default(),
                            response.duration_ms,
                            timeout,
                        )
                    };
                    decorate_execution_output(
                        &mut result.output,
                        declared_purpose,
                        &command_summary,
                        &resolved_cwd,
                        actual_shell,
                        "agent",
                    );
                    result
                }
                Ok(Err(_)) => {
                    self.shell_clients.cancel_request(&request_id).await;
                    Self::run_shell_tool_failure_result(
                        command_rejected_message(
                            "shell request waiter was dropped before a result was returned",
                            "check agent connectivity, then retry or use run_job for recoverable long-running work.",
                        ),
                        "runtime_error",
                        false,
                        false,
                    )
                }
                Err(_) => {
                    let command_started = self.shell_clients.cancel_request(&request_id).await;
                    Self::run_shell_tool_failure_result(
                        command_timeout_message(wait_timeout, "", ""),
                        "timeout",
                        command_started,
                        false,
                    )
                }
            }
        } else {
            let cwd_path = match resolve_local_cwd(&proj, cwd.as_deref()) {
                Ok(path) => path,
                Err(e) => {
                    return Self::run_shell_tool_failure_result(
                        command_rejected_message(
                            e,
                            "read the project root and choose an existing project-relative cwd, then retry.",
                        ),
                        "permission_denied",
                        false,
                        false,
                    )
                }
            };
            let resolved_cwd =
                project_relative_cwd(&proj, &cwd_path).unwrap_or_else(|_| ".".to_string());
            let actual_shell = shell.map(ExecutionShell::as_str).unwrap_or("sh");
            let result = match run_command_sync_bounded_with_shell_and_sandbox(
                command,
                cwd_path,
                timeout,
                actual_shell.to_string(),
                sandbox.map(str::to_string),
            )
            .await
            {
                Ok((exit_code, stdout, stderr, duration_ms)) => {
                    if exit_code == 0 {
                        ToolResult::ok(Self::run_shell_success_output(
                            exit_code,
                            stdout,
                            stderr,
                            Some(duration_ms),
                        ))
                    } else {
                        Self::run_shell_command_failure_result(
                            Some(exit_code),
                            stdout,
                            stderr,
                            Some(duration_ms),
                            timeout,
                        )
                    }
                }
                // The command's own timeout is reported through the Ok tuple;
                // this arm means the post-exit output drain wedged (a
                // descendant escaped the process group while holding the
                // pipes) and the outer backstop fired instead of parking the
                // MCP request indefinitely.
                Err(LocalRunFailure::HardTimeout { bound_secs }) => {
                    Self::run_shell_tool_failure_result(
                        command_timeout_message(bound_secs, "", ""),
                        "timeout",
                        true,
                        false,
                    )
                }
                Err(LocalRunFailure::Join(e)) => Self::run_shell_tool_failure_result(
                    command_rejected_message(
                        format!("task join error: {}", e),
                        "retry the command; if the worker keeps failing, inspect server logs.",
                    ),
                    "runtime_error",
                    false,
                    false,
                ),
            };
            let mut result = result;
            decorate_execution_output(
                &mut result.output,
                declared_purpose,
                &command_summary,
                &resolved_cwd,
                actual_shell,
                "local",
            );
            result
        }
    }
}

fn decorate_execution_output(
    output: &mut serde_json::Value,
    purpose: ExecutionPurpose,
    command_summary: &str,
    cwd: &str,
    shell: &str,
    executor: &str,
) {
    output["execution_source"] = json!("run_shell");
    output["purpose"] = json!(purpose.as_str());
    output["command_summary"] = json!(command_summary);
    output["cwd"] = json!(cwd);
    output["shell"] = json!(shell);
    output["executor"] = json!(executor);
    output["execution_state"] = json!(if output
        .get("failure_kind")
        .and_then(serde_json::Value::as_str)
        == Some("timeout")
    {
        "timed_out"
    } else {
        "completed"
    });
}
