//! Structured read-only workspace tools executed on a Workflow Session's SSH
//! resource.
//!
//! When a Workflow Session's execution context pins an exact SSH resource, the
//! supported read tools (`read_file`, `list_project_files`,
//! `list_project_tracked_files`, `project_overview`, `search_project_text`,
//! `git_status`, `git_diff_summary`, `git_diff`, `git_diff_hunks`,
//! `git_log`) must execute against the remote workspace, never the Runner's
//! local project checkout.
//!
//! The Server validates the request, builds a typed [`RemoteWorkspaceReadRequest`],
//! and enqueues it to the owning Runner over the `ssh_workspace_read` request
//! kind. The Runner resolves the remote root, validates the path, and executes
//! a fixed read-only command. The Server then parses the bounded raw output
//! with the same shapes as the existing local tools.
//!
//! Unsupported resource-bound operations (writes, edits, patches, artifact
//! writes, checkpoints, structured validation, LSP, lifecycle) fail closed with
//! `ssh_resource_unsupported_for_request`, `command_started=false`, and no
//! Agent request enqueued.

use super::{ToolCall, ToolResult, ToolRuntime};
use crate::shell_protocol::{
    RemoteWorkspaceReadOutcome, RemoteWorkspaceReadRequest, ShellRunResponse,
    REMOTE_WORKSPACE_READ_RESULT_FORMAT,
};
use serde_json::{json, Value};
use std::time::Duration;

/// Timeout budget for one structured remote read.
const SSH_WORKSPACE_READ_TIMEOUT_SECS: u64 = 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SshResourceRouting {
    SupportedWorkspaceRead,
    ExistingResourceAware,
    StoredIdentityOperation,
    SessionMetadataOnly,
    UnsupportedProjectOperation,
    UnrelatedGlobalOperation,
}

/// Exhaustive routing policy for a ToolCall executed in an exact Session/project
/// binding that has an SSH resource. New ToolCall variants must be classified
/// here before they can compile, so project operations cannot silently fall
/// through to the Runner-local checkout.
pub(crate) fn ssh_resource_routing(call: &ToolCall) -> SshResourceRouting {
    use SshResourceRouting::*;
    match call {
        ToolCall::ReadFile { .. }
        | ToolCall::ListProjectFiles { .. }
        | ToolCall::ListProjectTrackedFiles { .. }
        | ToolCall::ProjectOverview { .. }
        | ToolCall::SearchProjectText { .. }
        | ToolCall::GitStatus { .. }
        | ToolCall::GitDiff { .. }
        | ToolCall::GitDiffHunks { .. }
        | ToolCall::GitLog { .. }
        | ToolCall::GitDiffSummary { .. } => SupportedWorkspaceRead,

        ToolCall::RunShell { .. } | ToolCall::RunJob { .. } | ToolCall::OpenSessionShell { .. } => {
            ExistingResourceAware
        }

        ToolCall::StopJob { .. }
        | ToolCall::JobStatus { .. }
        | ToolCall::JobLog { .. }
        | ToolCall::ListJobs { .. }
        | ToolCall::JobTail { .. }
        | ToolCall::SessionShellExec { .. }
        | ToolCall::SessionShellStatus { .. }
        | ToolCall::CloseSessionShell { .. } => StoredIdentityOperation,

        ToolCall::StartSession { .. }
        | ToolCall::SessionSummary { .. }
        | ToolCall::UpdateSessionContext { .. }
        | ToolCall::CloseSession { .. }
        | ToolCall::ValidationSummary { .. }
        | ToolCall::PostSessionMessage { .. }
        | ToolCall::ListSessionMessages { .. }
        | ToolCall::ResolveSessionMessage { .. }
        | ToolCall::SessionDiscussionSummary { .. }
        | ToolCall::BindCurrentSession { .. }
        | ToolCall::CurrentSession { .. }
        | ToolCall::UnbindCurrentSession { .. } => SessionMetadataOnly,

        ToolCall::ListTools { .. }
        | ToolCall::ListProjects
        | ToolCall::ListAgents
        | ToolCall::RuntimeStatus { .. }
        | ToolCall::ToolManifest { .. } => UnrelatedGlobalOperation,

        ToolCall::StartCodingTask { .. }
        | ToolCall::FinishCodingTask { .. }
        | ToolCall::SessionHandoffSummary { .. }
        | ToolCall::WorkspaceCheckpointCreate { .. }
        | ToolCall::WorkspaceCheckpointList { .. }
        | ToolCall::WorkspaceCheckpointShow { .. }
        | ToolCall::WorkspaceCheckpointRestore { .. }
        | ToolCall::WorkspaceCheckpointDelete { .. }
        | ToolCall::ApplyPatch { .. }
        | ToolCall::ApplyPatchChecked { .. }
        | ToolCall::DeleteProjectFiles { .. }
        | ToolCall::GitRestorePaths { .. }
        | ToolCall::DiscardUntracked { .. }
        | ToolCall::ValidatePatch { .. }
        | ToolCall::CargoFmt { .. }
        | ToolCall::CargoCheck { .. }
        | ToolCall::CargoTest { .. }
        | ToolCall::ShowChanges { .. }
        | ToolCall::ReplaceInFile { .. }
        | ToolCall::ReplaceExactBlock { .. }
        | ToolCall::InsertBeforePattern { .. }
        | ToolCall::InsertAfterPattern { .. }
        | ToolCall::WriteProjectFile { .. }
        | ToolCall::SaveProjectArtifact { .. }
        | ToolCall::ReadProjectArtifactMetadata { .. }
        | ToolCall::ReadProjectArtifact { .. }
        | ToolCall::ArtifactUploadBegin { .. }
        | ToolCall::ArtifactUploadChunk { .. }
        | ToolCall::ArtifactUploadFinish { .. }
        | ToolCall::ArtifactUploadAbort { .. }
        | ToolCall::ReplaceLineRange { .. }
        | ToolCall::InsertAtLine { .. }
        | ToolCall::DeleteLineRange { .. }
        | ToolCall::ApplyTextEdits { .. }
        | ToolCall::WorkspaceHygieneCheck { .. }
        | ToolCall::LspStatus { .. }
        | ToolCall::DocumentSymbols { .. }
        | ToolCall::DocumentDiagnostics { .. }
        | ToolCall::Hover { .. }
        | ToolCall::WorkspaceSymbols { .. }
        | ToolCall::GotoDefinition { .. }
        | ToolCall::FindReferences { .. }
        | ToolCall::RegisterProject { .. }
        | ToolCall::CreateProject { .. } => UnsupportedProjectOperation,
    }
}

/// Fail-closed result for a resource-bound operation this round does not
/// support. Never touches the local project, never falls back to `run_shell`,
/// never uses a persistent shell, and enqueues no Agent request.
pub(crate) fn ssh_resource_unsupported_result(call: &ToolCall) -> ToolResult {
    let tool = call.tool_name();
    ToolResult::err_with_output(
        format!(
            "ssh_resource_unsupported_for_request: SSH resources do not support {tool} this round; the Runner-local project is not accessed"
        ),
        json!({
            "error_kind": "ssh_resource_unsupported_for_request",
            "tool": tool,
            "command_started": false,
            "command_completed": false,
            "command_ok": false,
            "exit_code": null,
            "tool_failure": true,
        }),
    )
}

/// Build the typed remote read request for one supported tool call.
fn build_remote_read_request(
    call: &ToolCall,
    ssh_session_id: &str,
) -> Result<(String, RemoteWorkspaceReadRequest), ToolResult> {
    let (operation, path, extra) = match call {
        ToolCall::ReadFile {
            path,
            start_line,
            limit,
            ..
        } => {
            let (eff_start, _eff_limit, eff_end) =
                super::files::effective_read_file_range(*start_line, *limit);
            (
                "read_file".to_string(),
                path.clone(),
                json!({
                    "start_line": eff_start,
                    "end_line": eff_end,
                    "max_bytes": 512 * 1024,
                }),
            )
        }
        ToolCall::ListProjectFiles { path, limit, .. } => (
            "list_project_files".to_string(),
            path.clone().unwrap_or_else(|| ".".to_string()),
            json!({ "limit": limit.unwrap_or(200).clamp(1, 500) }),
        ),
        ToolCall::ListProjectTrackedFiles {
            path,
            globs,
            depth,
            limit,
            offset,
            ..
        } => (
            "list_project_tracked_files".to_string(),
            path.clone().unwrap_or_else(|| ".".to_string()),
            json!({
                "globs": globs,
                "depth": depth,
                "limit": limit,
                "offset": offset,
            }),
        ),
        ToolCall::ProjectOverview {
            path,
            max_depth,
            limit,
            ..
        } => (
            "project_overview".to_string(),
            path.clone().unwrap_or_else(|| ".".to_string()),
            json!({
                "depth": max_depth,
                "limit": limit,
            }),
        ),
        ToolCall::SearchProjectText {
            pattern,
            path,
            limit,
            context_before,
            context_after,
            include_globs,
            exclude_globs,
            result_mode,
            timeout_secs,
            ..
        } => (
            "search_project_text".to_string(),
            path.clone().unwrap_or_else(|| ".".to_string()),
            json!({
                "pattern": pattern,
                "limit": limit,
                "context_before": context_before,
                "context_after": context_after,
                "include_globs": include_globs,
                "exclude_globs": exclude_globs,
                "result_mode": result_mode.map(|mode| mode.as_str()),
                "timeout_secs": timeout_secs,
            }),
        ),
        ToolCall::GitStatus { .. } => ("git_status".to_string(), ".".to_string(), json!({})),
        ToolCall::GitDiff { args, .. } => (
            "git_diff".to_string(),
            ".".to_string(),
            json!({ "paths": args }),
        ),
        ToolCall::GitDiffHunks {
            paths,
            max_hunks,
            max_hunk_lines,
            cached,
            ..
        } => (
            "git_diff_hunks".to_string(),
            ".".to_string(),
            json!({
                "paths": paths,
                "limit": max_hunks,
                "context_after": max_hunk_lines,
                "cached": cached,
            }),
        ),
        ToolCall::GitLog { limit, skip, .. } => (
            "git_log".to_string(),
            ".".to_string(),
            json!({ "limit": limit, "skip": skip }),
        ),
        ToolCall::GitDiffSummary { .. } => {
            ("git_diff_summary".to_string(), ".".to_string(), json!({}))
        }
        _ => {
            return Err(ssh_resource_unsupported_result(call));
        }
    };
    let timeout_secs = extra
        .get("timeout_secs")
        .and_then(Value::as_i64)
        .unwrap_or(SSH_WORKSPACE_READ_TIMEOUT_SECS as i64)
        .clamp(1, 120) as u64;
    let read = RemoteWorkspaceReadRequest {
        operation,
        path,
        pattern: extra
            .get("pattern")
            .and_then(Value::as_str)
            .map(str::to_string),
        include_globs: extra
            .get("include_globs")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            }),
        exclude_globs: extra
            .get("exclude_globs")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            }),
        result_mode: extra
            .get("result_mode")
            .and_then(Value::as_str)
            .map(str::to_string),
        context_before: extra
            .get("context_before")
            .and_then(Value::as_u64)
            .map(|v| v as usize),
        context_after: extra
            .get("context_after")
            .and_then(Value::as_u64)
            .map(|v| v as usize),
        limit: extra
            .get("limit")
            .and_then(Value::as_u64)
            .map(|v| v as usize),
        offset: extra
            .get("offset")
            .and_then(Value::as_u64)
            .map(|v| v as usize),
        depth: extra
            .get("depth")
            .and_then(Value::as_u64)
            .map(|v| v as usize),
        start_line: extra
            .get("start_line")
            .and_then(Value::as_u64)
            .map(|v| v as usize),
        end_line: extra
            .get("end_line")
            .and_then(Value::as_u64)
            .map(|v| v as usize),
        with_line_numbers: extra.get("with_line_numbers").and_then(Value::as_bool),
        max_bytes: extra
            .get("max_bytes")
            .and_then(Value::as_u64)
            .map(|v| v as usize),
        cached: extra.get("cached").and_then(Value::as_bool),
        paths: extra.get("paths").and_then(Value::as_array).map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        }),
        skip: extra
            .get("skip")
            .and_then(Value::as_u64)
            .map(|v| v as usize),
        timeout_secs,
    };
    Ok((ssh_session_id.to_string(), read))
}

impl ToolRuntime {
    /// Dispatch a supported SSH-workspace read tool. The Session's pinned
    /// resource routes execution to the Runner; a capability absence or enqueue
    /// rejection fails closed before any local project access.
    pub(crate) async fn dispatch_ssh_workspace_read(
        &self,
        call: ToolCall,
        ssh_resource: &str,
        ssh_session_id: &str,
        ssh_session_cwd: Option<&str>,
        auth: Option<&crate::auth::AuthContext>,
    ) -> ToolResult {
        if let ToolCall::SearchProjectText {
            pattern,
            path,
            limit,
            context_before,
            context_after,
            include_globs,
            exclude_globs,
            result_mode,
            timeout_secs,
            ..
        } = &call
        {
            let options =
                match super::files::SearchOptions::normalize(super::files::SearchRequest {
                    pattern: pattern.clone(),
                    path: path.clone(),
                    limit: *limit,
                    context_before: *context_before,
                    context_after: *context_after,
                    include_globs: include_globs.clone(),
                    exclude_globs: exclude_globs.clone(),
                    result_mode: *result_mode,
                    timeout_secs: *timeout_secs,
                }) {
                    Ok(options) => options,
                    Err(error) => return error.into_tool_result(),
                };
            if super::files::is_search_project_text_excluded_path(&options.path) {
                let mut result = super::files::empty_search_project_text_output(
                    call.project().unwrap_or(""),
                    &options,
                );
                result.output["executor"] = json!("ssh");
                result.output["resource"] = json!(ssh_resource);
                return result;
            }
        }
        let project = match call.project() {
            Some(project) => project.to_string(),
            None => return ssh_resource_unsupported_result(&call),
        };
        let proj = match self.resolve_project_for_auth(&project, auth).await {
            Ok(proj) => proj,
            Err(e) => return ToolResult::err(e),
        };
        let client_id = match proj.agent_client_id() {
            Ok(client_id) => client_id.to_string(),
            Err(e) => return ToolResult::err(e),
        };
        let (_, read) = match build_remote_read_request(&call, ssh_session_id) {
            Ok(pair) => pair,
            Err(result) => return result,
        };
        let (request_id, rx) = match self
            .shell_clients
            .enqueue_remote_workspace_read(
                client_id,
                read,
                ssh_resource.to_string(),
                ssh_session_id.to_string(),
                ssh_session_cwd.map(str::to_string),
                "tool_runtime".to_string(),
            )
            .await
        {
            Ok(pair) => pair,
            Err(e) => {
                return ToolResult::err_with_output(
                    format!("ssh_resource_unsupported_for_request: {e}"),
                    json!({
                        "error_kind": "ssh_resource_unsupported_for_request",
                        "command_started": false,
                        "command_completed": false,
                        "command_ok": false,
                        "exit_code": null,
                        "tool_failure": true,
                    }),
                )
            }
        };
        let wait_timeout = SSH_WORKSPACE_READ_TIMEOUT_SECS + 4;
        match tokio::time::timeout(Duration::from_secs(wait_timeout), rx).await {
            Ok(Ok(resp)) => self.parse_remote_read_response(&call, ssh_resource, resp),
            Ok(Err(_)) => {
                self.shell_clients.cancel_request(&request_id).await;
                ToolResult::err("ssh workspace read request was dropped")
            }
            Err(_) => {
                self.shell_clients.cancel_request(&request_id).await;
                ToolResult::err("timed out waiting for ssh workspace read")
            }
        }
    }

    /// Convert the Runner's bounded raw response into the existing local tool
    /// output shape, plus a safe execution-target summary.
    fn parse_remote_read_response(
        &self,
        call: &ToolCall,
        resource: &str,
        resp: ShellRunResponse,
    ) -> ToolResult {
        // The resource name comes from the already-validated exact Session
        // binding. The response itself never supplies routing identity.
        let executor = json!({
            "executor": "ssh",
            "resource": resource,
        });
        if resp.error.is_some() {
            return ToolResult::err_with_output(
                "SSH workspace transport failed",
                json!({
                    "error_kind": "ssh_workspace_transport_failure",
                    "command_started": resp.exit_code.is_some(),
                    "command_completed": false,
                    "command_ok": false,
                    "exit_code": resp.exit_code,
                    "tool_failure": true,
                    "executor": executor["executor"],
                    "resource": executor["resource"],
                }),
            );
        }
        let Some(remote) = resp.remote_workspace else {
            return ToolResult::err_with_output(
                "malformed SSH workspace response",
                json!({
                    "error_kind": "ssh_workspace_protocol_failure",
                    "command_started": resp.exit_code.is_some(),
                    "command_completed": resp.exit_code.is_some(),
                    "command_ok": false,
                    "exit_code": resp.exit_code,
                    "tool_failure": true,
                    "executor": executor["executor"],
                    "resource": executor["resource"],
                }),
            );
        };
        let expected_operation = build_remote_read_request(call, "")
            .ok()
            .map(|(_, read)| read.operation)
            .unwrap_or_default();
        if remote.format != REMOTE_WORKSPACE_READ_RESULT_FORMAT
            || remote.operation != expected_operation
        {
            return ToolResult::err_with_output(
                "malformed SSH workspace response",
                json!({
                    "error_kind": "ssh_workspace_protocol_failure",
                    "command_started": true,
                    "command_completed": true,
                    "command_ok": false,
                    "exit_code": null,
                    "tool_failure": true,
                    "executor": executor["executor"],
                    "resource": executor["resource"],
                }),
            );
        }
        let (stdout, exit_code) = match remote.outcome {
            RemoteWorkspaceReadOutcome::Success {
                exit_code, stdout, ..
            } => (stdout, Some(exit_code)),
            RemoteWorkspaceReadOutcome::Failure {
                error_kind,
                message,
                command_started,
                command_completed,
                exit_code,
            } => {
                return ToolResult::err_with_output(
                    message,
                    json!({
                        "error_kind": error_kind,
                        "command_started": command_started,
                        "command_completed": command_completed,
                        "command_ok": false,
                        "exit_code": exit_code,
                        "tool_failure": true,
                        "executor": executor["executor"],
                        "resource": executor["resource"],
                    }),
                );
            }
        };
        let stderr = String::new();
        match call {
            ToolCall::ReadFile {
                path,
                start_line,
                limit,
                with_line_numbers,
                ..
            } => {
                let mut result = super::files::read_file_agent_stdout_result_with_options(
                    stdout,
                    *start_line,
                    *limit,
                    with_line_numbers.unwrap_or(false),
                );
                if result.success {
                    result.output["path"] = json!(path);
                    result.output["executor"] = executor["executor"].clone();
                    result.output["resource"] = executor["resource"].clone();
                }
                result
            }
            ToolCall::ListProjectFiles { path, limit, .. } => {
                let rel_path = path.clone().unwrap_or_else(|| ".".to_string());
                let max_entries = limit.unwrap_or(200).clamp(1, 500);
                let (entries, truncated) =
                    super::files::parse_file_list_entries(&stdout, &rel_path, max_entries);
                ToolResult::ok(json!({
                    "path": rel_path,
                    "entries": entries,
                    "truncated": truncated,
                    "executor": executor["executor"],
                    "resource": executor["resource"],
                }))
            }
            ToolCall::ListProjectTrackedFiles {
                path,
                globs,
                depth,
                limit,
                offset,
                ..
            } => {
                let (paths, list_truncated) = super::file_listing::parse_nul_separated(&stdout);
                let scope = super::file_listing::normalize_scope(path.as_deref());
                let listing = super::file_listing::build_listing(
                    &paths,
                    &scope,
                    globs.as_deref().unwrap_or(&[]),
                    *depth,
                    limit.unwrap_or(200).clamp(1, 1000),
                    offset.unwrap_or(0),
                );
                let mut payload =
                    listing.to_json(call.project().unwrap_or(""), &scope, list_truncated);
                payload["executor"] = executor["executor"].clone();
                payload["resource"] = executor["resource"].clone();
                ToolResult::ok(payload)
            }
            ToolCall::ProjectOverview {
                path,
                max_depth,
                limit,
                ..
            } => {
                let _ = (max_depth, limit);
                let rel_path = path.clone().unwrap_or_else(|| ".".to_string());
                let (entries, warnings) = parse_overview_entries(&stdout, &rel_path);
                ToolResult::ok(json!({
                    "schema_version": 1,
                    "path": rel_path,
                    "deterministic": true,
                    "entries": entries,
                    "warnings": warnings,
                    "executor": executor["executor"],
                    "resource": executor["resource"],
                }))
            }
            ToolCall::SearchProjectText {
                pattern,
                path,
                limit,
                context_before,
                context_after,
                include_globs,
                exclude_globs,
                result_mode,
                timeout_secs,
                ..
            } => {
                let options =
                    match super::files::SearchOptions::normalize(super::files::SearchRequest {
                        pattern: pattern.clone(),
                        path: path.clone(),
                        limit: *limit,
                        context_before: *context_before,
                        context_after: *context_after,
                        include_globs: include_globs.clone(),
                        exclude_globs: exclude_globs.clone(),
                        result_mode: *result_mode,
                        timeout_secs: *timeout_secs,
                    }) {
                        Ok(options) => options,
                        Err(error) => return error.into_tool_result(),
                    };
                let mut result = super::files::search_project_text_output(
                    call.project().unwrap_or(""),
                    &options,
                    &stdout,
                    exit_code,
                    &stderr,
                );
                result.output["executor"] = executor["executor"].clone();
                result.output["resource"] = executor["resource"].clone();
                result
            }
            ToolCall::GitDiffSummary { .. } => {
                let (porcelain, diff_stat) = super::git::split_diff_summary(&stdout);
                let summary = super::git::parse_porcelain_summary(&porcelain);
                ToolResult::ok(json!({
                    "porcelain": porcelain,
                    "diff_stat": diff_stat,
                    "changed_files": summary.changed_files,
                    "changed_files_count": summary.changed_files_count,
                    "tracked_changed_files": summary.tracked_changed_files,
                    "untracked_files": summary.untracked_files,
                    "ignored_files": summary.ignored_files,
                    "exit_code": exit_code,
                    "executor": executor["executor"],
                    "resource": executor["resource"],
                }))
            }
            ToolCall::GitDiffHunks {
                paths,
                max_hunks,
                max_hunk_lines,
                cached,
                ..
            } => {
                let max_hunks = max_hunks.unwrap_or(30).clamp(1, 100);
                let max_hunk_lines = max_hunk_lines.unwrap_or(160).clamp(1, 400);
                let (files, hunk_count, truncated) =
                    super::git::parse_git_diff_hunks(&stdout, max_hunks, max_hunk_lines);
                ToolResult::ok(json!({
                    "project": call.project().unwrap_or(""),
                    "paths": paths.clone().unwrap_or_default(),
                    "cached": cached.unwrap_or(false),
                    "files": files,
                    "hunk_count": hunk_count,
                    "max_hunks": max_hunks,
                    "max_hunk_lines": max_hunk_lines,
                    "truncated": truncated,
                    "exit_code": exit_code,
                    "stderr": stderr,
                    "executor": executor["executor"],
                    "resource": executor["resource"],
                }))
            }
            ToolCall::GitLog { limit, skip, .. } => {
                let limit = super::git::normalize_git_log_limit(*limit);
                let skip = super::git::normalize_git_log_skip(*skip);
                let (commits, truncated) = super::git::parse_git_log_commits(&stdout, limit);
                ToolResult::ok(json!({
                    "project": call.project().unwrap_or(""),
                    "limit": limit,
                    "skip": skip,
                    "count": commits.len(),
                    "truncated": truncated,
                    "commits": commits,
                    "executor": executor["executor"],
                    "resource": executor["resource"],
                }))
            }
            ToolCall::GitStatus { .. } | ToolCall::GitDiff { .. } => ToolResult::ok(json!({
                "stdout": stdout,
                "stderr": stderr,
                "exit_code": exit_code,
                "executor": executor["executor"],
                "resource": executor["resource"],
            })),
            _ => ssh_resource_unsupported_result(call),
        }
    }
}

/// Parse `find -printf '%y %p\n'` output into the project-overview entry
/// shape. `f ./src/main.rs` → file, `d ./src` → directory.
pub(crate) fn parse_overview_entries(stdout: &str, relative: &str) -> (Vec<Value>, Vec<Value>) {
    let mut entries = Vec::new();
    let mut warnings = Vec::new();
    for line in stdout.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        let (kind, path) = match line.split_once(' ') {
            Some((kind, path)) => (kind, path),
            None => continue,
        };
        let is_dir = match kind {
            "d" => true,
            "f" => false,
            _ => {
                warnings.push(json!("unreadable_entries_skipped"));
                continue;
            }
        };
        let path = path.trim_start_matches("./");
        if path.is_empty() {
            continue;
        }
        let scoped = if relative == "." {
            path.to_string()
        } else {
            format!("{relative}/{path}")
        };
        entries.push(json!({
            "path": scoped,
            "kind": if is_dir { "directory" } else { "file" },
            "depth": 1,
        }));
    }
    entries.sort_by(|left, right| {
        left["path"]
            .as_str()
            .unwrap_or("")
            .cmp(right["path"].as_str().unwrap_or(""))
    });
    (entries, warnings)
}
