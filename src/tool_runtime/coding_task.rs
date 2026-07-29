//! Deterministic coding-task workflow aggregates.
//!
//! These tools reduce repetitive startup/finish calls for model-facing coding
//! loops. They only aggregate existing runtime state and never call an LLM,
//! generate prose summaries, parse validation output, or hide underlying tool
//! payloads.

use serde_json::{json, Value};

use super::handoff::{
    apply_compact_workflow_outcomes, closeout_work_projection, compact_jobs, compact_permissions,
    compact_review_evidence, compact_tool_failures, compact_validation,
    resolved_unexpected_validation_failure_count, review_evidence_summary_for_session,
    unresolved_unexpected_failure_count, validation_has_cargo_test_zero_tests,
};
use super::permissions::{authority_profile_payload, permission_summary_from_events};
use super::project_instructions::{ProjectInstructionFile, ProjectInstructionsSnapshot};
use super::project_resolution::ResolvedProject;
use super::runtime_info::compact_runtime_status;
use super::session_context::{
    session_project_mismatch_warning, SessionProjectMismatch, SESSION_PROJECT_MISMATCH_KIND,
};
use super::sessions::tool_failure_summary_from_events;
use super::sessions::{self, SessionTransport, TOOL_CALL_RECORDING_SESSION_ID_FIELD};
use super::tool_catalog::TOOL_RECOMMENDED_FLOWS;
use super::tool_inputs::{SessionMode, StartupDetail};
use super::tool_result::ToolResult;
use super::validation_events::skipped_validation_summary;
use super::{current_session_key, unknown_session_result};
use super::{ToolCall, ToolRuntime};
use crate::auth::AuthContext;
use std::collections::HashSet;

const RULES_MAX_HEADINGS: usize = 8;
const RULES_MAX_FIRST_LINES: usize = 5;
const RULES_MAX_LINE_CHARS: usize = 180;
const FINISH_SESSION_EVENT_LIMIT: usize = 200;

impl ToolRuntime {
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn start_coding_task(
        &self,
        project: String,
        title: Option<String>,
        mode: SessionMode,
        deny_write_tools: bool,
        deny_shell_tools: bool,
        detail: StartupDetail,
        bind_current: bool,
        new_session: bool,
        auth: Option<&AuthContext>,
        transport: SessionTransport,
        window: Option<&crate::client_window::ClientWindow>,
    ) -> ToolResult {
        let title = match title {
            Some(title) => {
                let title = title.trim().to_string();
                if title.is_empty()
                    || title.chars().count() > sessions::MAX_CODING_INSTRUCTION_CHARS
                {
                    return ToolResult::err_with_output(
                        format!(
                            "title must contain 1..={} characters",
                            sessions::MAX_CODING_INSTRUCTION_CHARS
                        ),
                        json!({
                            "error_kind": "invalid_coding_instruction",
                            "field": "title",
                            "max_chars": sessions::MAX_CODING_INSTRUCTION_CHARS,
                        }),
                    );
                }
                Some(title)
            }
            None => None,
        };
        // `detail` is the single startup projection control: full keeps the
        // complete runtime status, recent commits, rules, and tool manifest;
        // standard/minimal use the compact projections.
        let compact_startup = detail != StartupDetail::Full;
        let include_recent_commits = detail == StartupDetail::Full;
        let include_rules = detail == StartupDetail::Full;
        let include_tool_manifest = detail == StartupDetail::Full;
        let tool_manifest = if include_tool_manifest {
            match self.compact_tool_manifest_payload_bounded(None, None, None) {
                Ok(payload) => Some(payload),
                Err(result) => return result,
            }
        } else {
            None
        };

        let resolved = match self.resolve_project_input_for_auth(&project, auth).await {
            Ok(resolved) => resolved,
            Err(err) => return err.into_tool_result(),
        };
        let semantic_navigation = self.probe_semantic_navigation_for_startup(&resolved).await;
        let project_instructions = if include_rules {
            Some(self.load_project_instructions(&resolved.config).await)
        } else {
            None
        };
        let mut warnings = Vec::new();
        let continuity_key = if bind_current {
            match current_session_key(
                auth,
                transport,
                &resolved.resolved_id,
                &resolved.config.path,
                window,
            ) {
                Ok(key) => Some(key),
                Err(message) => {
                    warnings.push(json!({
                        "kind": "current_binding_unavailable",
                        "message": message,
                    }));
                    None
                }
            }
        } else {
            None
        };

        let mut runtime_status_call_failed = false;
        let runtime_status = {
            let result = self.runtime_status(auth).await;
            if !result.success {
                runtime_status_call_failed = true;
                warnings.push(json!({
                    "kind": "runtime_status_unavailable",
                    "message": result.error,
                }));
            }
            if compact_startup {
                compact_runtime_status(&result.output)
            } else {
                result.output
            }
        };
        let git = self
            .start_coding_task_git_summary(
                &resolved.resolved_id,
                include_recent_commits,
                &mut warnings,
            )
            .await;
        // Surface dirty/conflict worktree state at top-level so compact Action
        // responses that omit full git payloads still keep the warning reason.
        if !git.is_null() {
            append_workspace_warnings(&workspace_payload_from_git_summary(&git), &mut warnings);
        }
        let binding_available = bind_current && continuity_key.is_some();
        let write_scope_verified = auth.is_none_or(|auth| {
            !auth.is_oauth_token() || auth.has_scope(crate::auth::SCOPE_PROJECT_WRITE)
        });
        let session_outcome =
            match self
                .sessions
                .ensure_coding_session(sessions::CodingSessionRequest {
                    key: continuity_key.clone(),
                    project: resolved.resolved_id.clone(),
                    instruction: title.clone(),
                    mode,
                    guards: sessions::SessionGuards {
                        deny_write_tools,
                        deny_shell_tools,
                    },
                    project_instructions: project_instructions.clone(),
                    transport,
                    bind_current: binding_available,
                    new_session,
                    // Startup always re-reads bounded current Git state and, for
                    // full detail, the fixed project-instruction candidates.
                    context_refreshed: true,
                    write_scope_verified,
                }) {
                Ok(outcome) => outcome,
                Err(sessions::CodingSessionError::WriteScopeRequired) => {
                    return ToolResult::err_with_output(
                        "session capability upgrade requires project:write",
                        json!({
                            "error_kind": "session_capability_upgrade_denied",
                            "required_scope": crate::auth::SCOPE_PROJECT_WRITE,
                            "mode": mode.as_str(),
                            "state_changed": false,
                        }),
                    );
                }
                Err(sessions::CodingSessionError::CommitFailed) => {
                    return ToolResult::err_with_output(
                        "coding continuity state could not be committed",
                        json!({
                            "error_kind": "coding_continuity_commit_failed",
                            "state_changed": false,
                        }),
                    );
                }
            };
        let session_summary = &session_outcome.summary;
        let current_binding = if binding_available {
            json!({
                "bound": true,
                "session_id": session_summary.session_id,
                "process_local_cache": true,
                "durable_exact_binding": true,
                "restored_after_restart": true,
                "transport": transport.as_str(),
                "resolved_project": resolved.resolved_id.clone(),
            })
        } else {
            json!({
                "bound": false,
                "process_local_cache": true,
                "durable_exact_binding": true,
                "restored_after_restart": true,
                "transport": transport.as_str(),
                "reason_code": if bind_current {
                    "window_identity_unavailable"
                } else {
                    "binding_disabled"
                },
            })
        };
        let mut connection_state = runtime_status
            .get("connection_layers")
            .cloned()
            .unwrap_or_else(|| {
                json!({
                    "runner_process": {"status": "not_observed"},
                    "server_transport": {"status": "not_observed"},
                    "server_registration": {"status": "not_observed"},
                    "project_registry": {"status": "resolved", "resolved_project": resolved.resolved_id},
                    "connector_endpoint": {"status": "not_observed"},
                    "session_binding": {"status": "not_observed"},
                    "last_successful_tool_call": {"status": "not_observed"},
                })
            });
        connection_state["project_registry"]["resolved_project"] = json!(resolved.resolved_id);
        connection_state["session_binding"] = json!({
            "status": if binding_available { "bound" } else { "not_bound" },
            "observed_at": chrono::Utc::now().timestamp(),
            "source": "session_store",
            "age_secs": 0,
            "stale_after_secs": Value::Null,
            "reason_code": if binding_available {
                Value::Null
            } else if bind_current {
                json!("window_identity_unavailable")
            } else {
                json!("binding_disabled")
            },
            "process_local_cache": true,
            "durable_exact_binding": true,
            "restored_after_restart": true,
            "requires_stable_window_identity": true,
            "transport": transport.as_str(),
            "durable_resume": "the same exact principal, transport, stable window, project, and canonical repository root resumes the durable wc_sess_* session",
        });
        let recommended_flow = match &tool_manifest {
            Some(manifest) => recommended_flow_payload_for_manifest_tools(manifest),
            None => recommended_flow_payload(),
        };
        let mut output = json!({
            "detail": detail.as_str(),
            "project": project,
            "resolved_project": resolved_project_payload(&resolved),
            "session": {
                "session_id": session_summary.session_id,
                "mode": session_summary.mode,
                "guards": session_summary.guards,
                "lifecycle": session_summary.lifecycle,
                "continuation": if session_outcome.reused { "continued" } else { "created" },
                "reused": session_outcome.reused,
                "new_session_requested": new_session,
                "instruction_appended": title.is_some(),
                "root_title": session_summary.title,
                "capability": {
                    "changed": session_outcome.capability_changed,
                    "previous_mode": session_outcome.previous_mode,
                    "previous_guards": session_outcome.previous_guards,
                    "requested_mode": mode,
                    "mode": session_summary.mode,
                    "guards": session_summary.guards,
                    "write_scope_verified": write_scope_verified,
                },
                "context": {
                    "refreshed": true,
                    "git_state_recaptured": true,
                    "rules_recaptured": include_rules,
                },
                "explicit_session_id_required_for_continuity": false,
                "explicit_session_id_recommended": !binding_available,
                "explicit_session_id_fields": {
                    "tool_business_input": "session_id",
                    "generic_wrapper_recorder": TOOL_CALL_RECORDING_SESSION_ID_FIELD
                },
                "current_binding": current_binding,
            },
            "runtime_status": runtime_status,
            "connection_state": connection_state,
            "authority": authority_profile_payload(),
            "rules": rules_summary(project_instructions.as_ref()),
            "git": git,
            "semantic_navigation": semantic_navigation,
            "recommended_flow": recommended_flow,
            "deterministic": true,
            "llm_summary": false,
            "warnings": warnings,
        });
        if let Some(tool_manifest) = tool_manifest {
            output["tool_manifest"] = tool_manifest;
        }
        if !include_rules {
            output.as_object_mut().map(|object| object.remove("rules"));
        }
        if detail != StartupDetail::Full {
            if let Some(object) = output.as_object_mut() {
                object.remove("recommended_flow");
            }
        }
        if detail == StartupDetail::Minimal {
            if let Some(object) = output.as_object_mut() {
                object.remove("authority");
            }
        }
        output["startup_verdict"] =
            startup_verdict(&output, runtime_status_call_failed, include_tool_manifest);
        ToolResult::ok(output)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn finish_coding_task(
        &self,
        project: String,
        session_id: String,
        summary_only: bool,
        include_diff: Option<bool>,
        include_workspace: Option<bool>,
        include_hygiene: Option<bool>,
        include_handoff: Option<bool>,
        include_validation_summary: Option<bool>,
        auth: Option<&AuthContext>,
    ) -> ToolResult {
        let include_diff = include_diff.unwrap_or(true);
        let include_workspace = include_workspace.unwrap_or(true);
        let include_hygiene = include_hygiene.unwrap_or(true);
        let include_handoff = include_handoff.unwrap_or(true);
        let include_validation_summary = include_validation_summary.unwrap_or(true);

        let resolved = match self.resolve_project_input_for_auth(&project, auth).await {
            Ok(resolved) => resolved,
            Err(err) => return err.into_tool_result(),
        };
        let session_summary = match self
            .sessions
            .summary(&session_id, Some(FINISH_SESSION_EVENT_LIMIT))
        {
            Some(summary) => summary,
            None => return unknown_session_result(&session_id),
        };
        let mut final_warnings = Vec::new();
        let session_project_mismatch =
            session_summary
                .project
                .as_ref()
                .and_then(|session_project| {
                    (session_project != &resolved.resolved_id).then(|| SessionProjectMismatch {
                        session_project: session_project.clone(),
                        request_project: resolved.resolved_id.clone(),
                    })
                });
        if let Some(mismatch) = session_project_mismatch.as_ref() {
            final_warnings.push(session_project_mismatch_warning(mismatch, false));
        }
        if session_summary.project.is_none() {
            final_warnings.push(json!({
                "kind": "session_has_no_project",
                "message": "session was not created with a project association",
            }));
        }

        let show_changes_call = ToolCall::ShowChanges {
            project: resolved.resolved_id.clone(),
            session_id: Some(session_id.clone()),
            include_diff: Some(include_diff),
            max_hunks: None,
            max_hunk_lines: None,
            session_event_limit: Some(50),
        };
        let show_changes_start = self.sessions.record_tool_call_started_with_options(
            Some(&session_id),
            SessionTransport::Api,
            show_changes_call.tool_name(),
            &show_changes_call.session_log_arguments(),
            Some(resolved.resolved_id.clone()),
        );
        let changes_result = self
            .show_changes(
                resolved.resolved_id.clone(),
                Some(session_id.clone()),
                Some(include_diff),
                None,
                None,
                Some(50),
            )
            .await;
        self.sessions.record_tool_call_finished(
            show_changes_start,
            changes_result.success,
            &changes_result.output,
            changes_result.error.as_deref(),
            None,
        );
        if !changes_result.success {
            final_warnings.push(json!({
                "kind": "show_changes_failed",
                "message": changes_result.error,
            }));
        }
        let workspace = workspace_payload_from_show_changes(&changes_result.output);
        append_workspace_warnings(&workspace, &mut final_warnings);

        let validation = if include_validation_summary {
            self.validation_summary_for_session_with_jobs(&session_summary, 10, auth)
                .await
        } else {
            skipped_validation_summary()
        };
        let permissions = permission_summary_from_events(
            &session_summary.events,
            super::permissions::DEFAULT_PERMISSION_RECENT_LIMIT,
        );

        let hygiene = if include_hygiene {
            let hygiene_call = ToolCall::WorkspaceHygieneCheck {
                project: resolved.resolved_id.clone(),
                max_findings: None,
                include_tracked: None,
                session_id: Some(session_id.clone()),
            };
            let hygiene_start = self.sessions.record_tool_call_started_with_options(
                Some(&session_id),
                SessionTransport::Api,
                hygiene_call.tool_name(),
                &hygiene_call.session_log_arguments(),
                Some(resolved.resolved_id.clone()),
            );
            let result = self
                .workspace_hygiene_check(
                    resolved.resolved_id.clone(),
                    None,
                    None,
                    Some(session_id.clone()),
                )
                .await;
            self.sessions.record_tool_call_finished(
                hygiene_start,
                result.success,
                &result.output,
                result.error.as_deref(),
                None,
            );
            if !result.success {
                final_warnings.push(json!({
                    "kind": "workspace_hygiene_failed",
                    "message": result.error,
                }));
            }
            result.output
        } else {
            Value::Null
        };
        append_hygiene_warnings(&hygiene, &mut final_warnings);

        let jobs = self
            .active_jobs_summary(Some(&resolved.resolved_id), auth, 10)
            .await;
        if let Some(warnings) = jobs.get("warnings").and_then(Value::as_array) {
            final_warnings.extend(warnings.iter().cloned());
        }

        let handoff = if include_handoff {
            let result = self
                .session_handoff_summary(
                    session_id.clone(),
                    Some(resolved.resolved_id.clone()),
                    Some(include_workspace),
                    Some(true),
                    Some(include_validation_summary),
                    summary_only,
                    Some(20),
                    auth,
                )
                .await;
            if !result.success {
                final_warnings.push(json!({
                    "kind": "session_handoff_failed",
                    "message": result.error,
                }));
            }
            result.output
        } else {
            Value::Null
        };
        let closeout_session_summary = self
            .sessions
            .summary(&session_id, Some(FINISH_SESSION_EVENT_LIMIT))
            .unwrap_or_else(|| session_summary.clone());
        let review_evidence = review_evidence_summary_for_session(&closeout_session_summary);
        let (work_performed, changed_paths) =
            closeout_work_projection(&closeout_session_summary.events);

        let mut output = json!({
            "project": project,
            "resolved_project": resolved_project_payload(&resolved),
            "session_id": session_id,
            "workspace": workspace,
            "changes": {
                "show_changes": changes_result.output,
                "hunks_truncated": changes_result.output
                    .get("hunks_truncated")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            },
            "validation": validation,
            "permissions": permissions,
            "tool_failures": tool_failure_summary_from_events(&session_summary.events, 10),
            "review_evidence": review_evidence,
            "work_performed": work_performed,
            "changed_paths": changed_paths,
            "hygiene": hygiene,
            "handoff": handoff,
            "jobs": jobs,
            "deterministic": true,
            "llm_summary": false,
            "final_warnings": final_warnings,
        });
        if let Some(mismatch) = session_project_mismatch.as_ref() {
            output["warning_kind"] = json!(SESSION_PROJECT_MISMATCH_KIND);
            output["session_project"] = json!(mismatch.session_project);
            output["request_project"] = json!(mismatch.request_project);
            output["allow_cross_project_session_required"] = json!(true);
            output["allow_cross_project_session"] = json!(false);
        }
        let resolved_unexpected_validation_failures = resolved_unexpected_validation_failure_count(
            &session_summary.events,
            output.get("validation").unwrap_or(&Value::Null),
            true,
            output
                .get("workspace")
                .and_then(|workspace| workspace.get("clean"))
                .and_then(Value::as_bool)
                .unwrap_or(false),
            output
                .get("hygiene")
                .and_then(|hygiene| hygiene.get("clean"))
                .and_then(Value::as_bool)
                .unwrap_or(true),
            output
                .get("jobs")
                .and_then(|jobs| jobs.get("blocking_active_count"))
                .and_then(Value::as_u64)
                .unwrap_or(0),
        );
        output["suggested_next_actions"] = json!(finish_suggested_next_actions(
            &output,
            resolved_unexpected_validation_failures,
        ));
        let compact = compact_finish_output(&output, resolved_unexpected_validation_failures);
        if summary_only {
            return ToolResult::ok(compact);
        }
        for field in [
            "facts",
            "hard_blockers",
            "advisories",
            "task_outcome",
            "evidence_history",
            "evidence_integrity",
            "informational_notes",
        ] {
            output[field] = compact.get(field).cloned().unwrap_or(Value::Null);
        }
        output["suggested_next_actions"] = compact["suggested_next_actions"].clone();
        ToolResult::ok(output)
    }

    async fn start_coding_task_git_summary(
        &self,
        project: &str,
        include_recent_commits: bool,
        warnings: &mut Vec<Value>,
    ) -> Value {
        let mut output = json!({
            "available": false,
            "branch": Value::Null,
            "head": Value::Null,
            "clean": Value::Null,
            "changed_files_count": 0,
            "counts": {},
            "recent_commits": [],
            "warnings": [],
        });

        {
            let result = self
                .show_changes(project.to_string(), None, Some(false), None, None, None)
                .await;
            if !result.success {
                warnings.push(json!({
                    "kind": "git_status_unavailable",
                    "message": result.error,
                }));
            }
            output["available"] = json!(result
                .output
                .get("git_available")
                .and_then(Value::as_bool)
                .unwrap_or(result.success));
            output["branch"] = result.output.get("branch").cloned().unwrap_or(Value::Null);
            output["head"] = result.output.get("head").cloned().unwrap_or(Value::Null);
            output["clean"] = result.output.get("clean").cloned().unwrap_or(Value::Null);
            output["counts"] = result
                .output
                .get("counts")
                .cloned()
                .unwrap_or_else(|| json!({}));
            output["changed_files_count"] =
                json!(changed_files_count_from_counts(&output["counts"]));
            output["warnings"] = result
                .output
                .get("warnings")
                .cloned()
                .unwrap_or_else(|| json!([]));
            output["show_changes"] = result.output;
        }

        if include_recent_commits {
            let result = self.git_log(project.to_string(), Some(5), None).await;
            if result.success {
                output["recent_commits"] = result
                    .output
                    .get("commits")
                    .cloned()
                    .unwrap_or_else(|| json!([]));
                output["recent_commits_truncated"] = result
                    .output
                    .get("truncated")
                    .cloned()
                    .unwrap_or(json!(false));
            } else {
                warnings.push(json!({
                    "kind": "recent_commits_unavailable",
                    "message": result.error,
                }));
                output["recent_commits"] = json!([]);
                output["recent_commits_truncated"] = json!(false);
            }
        } else if let Some(object) = output.as_object_mut() {
            object.remove("recent_commits");
        }

        output
    }
}

fn resolved_project_payload(resolved: &ResolvedProject) -> Value {
    json!({
        "input": resolved.input.clone(),
        "id": resolved.resolved_id.clone(),
        "path": resolved.config.path.clone(),
        "executor": if resolved.config.is_agent() { "agent" } else { "local" },
        "client_id": resolved.config.client_id.clone(),
        "allow_patch": resolved.config.allow_patch,
    })
}

fn rules_summary(snapshot: Option<&ProjectInstructionsSnapshot>) -> Value {
    let Some(snapshot) = snapshot else {
        return Value::Null;
    };
    let sources: Vec<Value> = snapshot.files.iter().map(rule_source_summary).collect();
    json!({
        "present": snapshot.loaded,
        "loaded": snapshot.loaded,
        "sources": sources,
        "candidate_paths": snapshot.candidate_paths.clone(),
        "total_chars": snapshot.total_chars,
        "max_total_chars": snapshot.max_total_chars,
        "truncated": snapshot.truncated,
        "summary": if snapshot.loaded {
            "deterministic instruction source summary; read listed sources for full content"
        } else {
            "no project instruction source loaded from the fixed candidate list"
        },
        "note": snapshot.note.clone(),
    })
}

fn rule_source_summary(file: &ProjectInstructionFile) -> Value {
    json!({
        "path": file.path.clone(),
        "chars": file.chars,
        "total_lines": file.total_lines,
        "start_line": file.start_line,
        "limit": file.limit,
        "truncated": file.truncated,
        "read_more": file.read_more.clone(),
        "headings": extract_headings(&file.content),
        "first_lines": extract_first_lines(&file.content),
    })
}

fn extract_headings(content: &str) -> Vec<String> {
    content
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with('#'))
        .take(RULES_MAX_HEADINGS)
        .map(bound_line)
        .collect()
}

fn extract_first_lines(content: &str) -> Vec<String> {
    content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .take(RULES_MAX_FIRST_LINES)
        .map(bound_line)
        .collect()
}

fn bound_line(line: &str) -> String {
    let mut out = String::new();
    for ch in line.chars().take(RULES_MAX_LINE_CHARS) {
        out.push(ch);
    }
    out
}

/// Full default startup recommended flow. Reuses the shared
/// `TOOL_RECOMMENDED_FLOWS` group definitions so top-level startup guidance
/// does not drift from `tool_manifest.recommended_flows`.
fn recommended_flow_payload() -> Value {
    recommended_flow_groups(None)
}

/// Project top-level `recommended_flow` onto tools present in the embedded
/// `tool_manifest`. Group keys stay fixed; empty groups are allowed.
fn recommended_flow_payload_for_manifest_tools(manifest: &Value) -> Value {
    let visible: HashSet<&str> = manifest
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|tool| tool.get("name").and_then(Value::as_str))
        .collect();
    recommended_flow_groups(Some(&visible))
}

fn recommended_flow_groups(visible: Option<&HashSet<&str>>) -> Value {
    const GROUPS: &[&str] = &["inspect", "edit", "validate", "review", "handoff"];
    let mut map = serde_json::Map::new();
    for group in GROUPS {
        let tools = TOOL_RECOMMENDED_FLOWS
            .iter()
            .find(|flow| flow.name == *group)
            .map(|flow| {
                let mut seen = HashSet::new();
                flow.tools
                    .iter()
                    .copied()
                    .filter(|tool| {
                        let allowed = match visible {
                            Some(set) => set.contains(*tool),
                            None => true,
                        };
                        allowed && seen.insert(*tool)
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        map.insert((*group).to_string(), json!(tools));
    }
    Value::Object(map)
}

fn workspace_payload_from_show_changes(show_changes: &Value) -> Value {
    let counts = show_changes
        .get("counts")
        .cloned()
        .unwrap_or_else(|| json!({}));
    json!({
        "clean": show_changes
            .get("clean")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        "git_available": show_changes
            .get("git_available")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        "non_git_project": show_changes
            .get("non_git_project")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        "branch": show_changes.get("branch").cloned().unwrap_or(Value::Null),
        "head": show_changes.get("head").cloned().unwrap_or(Value::Null),
        "changed_files_count": changed_files_count_from_counts(&counts),
        "counts": counts,
        "warnings": show_changes
            .get("warnings")
            .cloned()
            .unwrap_or_else(|| json!([])),
    })
}

/// Map startup `git` summary fields into the workspace warning shape.
fn workspace_payload_from_git_summary(git: &Value) -> Value {
    let counts = git.get("counts").cloned().unwrap_or_else(|| json!({}));
    json!({
        "clean": git.get("clean").and_then(Value::as_bool).unwrap_or(false),
        "git_available": git
            .get("available")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        "changed_files_count": git
            .get("changed_files_count")
            .and_then(Value::as_u64)
            .unwrap_or_else(|| changed_files_count_from_counts(&counts)),
        "counts": counts,
    })
}

fn compact_finish_output(output: &Value, resolved_unexpected_validation_failures: usize) -> Value {
    let hygiene_checked = output
        .get("hygiene")
        .is_some_and(|hygiene| !hygiene.is_null());
    let workspace_clean = output
        .get("workspace")
        .and_then(|workspace| workspace.get("clean"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let workspace_conflicts = output
        .pointer("/workspace/counts/conflicted")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let hygiene_clean = output
        .get("hygiene")
        .and_then(|hygiene| hygiene.get("clean"))
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let hygiene_secret_like_paths = output
        .pointer("/hygiene/counts/secret_like_paths")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let hygiene_truncated = output
        .pointer("/hygiene/truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut compact = json!({
        "summary_only": true,
        "project": output.get("project").cloned().unwrap_or(Value::Null),
        "session_id": output.get("session_id").cloned().unwrap_or(Value::Null),
        "workspace_clean": workspace_clean,
        "workspace_conflicts": workspace_conflicts,
        "hygiene_clean": hygiene_clean,
        "hygiene_secret_like_paths": hygiene_secret_like_paths,
        "hygiene_truncated": hygiene_truncated,
        "jobs": compact_jobs(output.get("jobs").unwrap_or(&Value::Null)),
        "permissions": compact_permissions(output.get("permissions").unwrap_or(&Value::Null)),
        "tool_failures": compact_tool_failures(output.get("tool_failures").unwrap_or(&Value::Null)),
        "validation": compact_validation(output.get("validation").unwrap_or(&Value::Null)),
        "review_evidence": compact_review_evidence(output.get("review_evidence").unwrap_or(&Value::Null)),
        "work_performed": output.get("work_performed").cloned().unwrap_or_else(|| json!([])),
        "changed_paths": output.get("changed_paths").cloned().unwrap_or_else(|| json!([])),
        "warnings": output.get("final_warnings").cloned().unwrap_or_else(|| json!([])),
        "suggested_next_actions": output.get("suggested_next_actions").cloned().unwrap_or_else(|| json!([])),
    });
    apply_compact_workflow_outcomes(
        &mut compact,
        true,
        Some(hygiene_checked),
        resolved_unexpected_validation_failures,
    );
    let verdict = compact.get("verdict").cloned().unwrap_or_else(|| json!({}));
    compact["suggested_next_actions"] = json!(merged_suggested_next_actions(&compact, &verdict));
    compact
        .as_object_mut()
        .expect("compact finish output is an object")
        .remove("verdict");
    compact
}

fn startup_verdict(
    output: &Value,
    runtime_status_call_failed: bool,
    tool_manifest_requested: bool,
) -> Value {
    let mut checks = Vec::new();
    let mut actions: Vec<String> = Vec::new();

    push_startup_check(
        &mut checks,
        "runtime_status",
        runtime_status_check(output, runtime_status_call_failed),
    );
    push_startup_check(&mut checks, "workspace", workspace_check(output));
    push_startup_check(&mut checks, "jobs", startup_jobs_check(output));
    push_startup_check(&mut checks, "agent", startup_agent_check(output));
    push_startup_check(
        &mut checks,
        "tool_manifest",
        startup_tool_manifest_check(output, tool_manifest_requested),
    );

    for check in &checks {
        match check.get("reason").and_then(Value::as_str) {
            Some("runtime_status_call_failed") => {
                push_unique_action(&mut actions, "inspect runtime_status directly")
            }
            Some("workspace_dirty") => push_unique_action(
                &mut actions,
                "inspect existing worktree changes with show_changes and preserve them while editing",
            ),
            Some("workspace_conflicts") => push_unique_action(
                &mut actions,
                "review merge/rebase conflicts carefully; do not reset or overwrite conflict markers unless resolving them",
            ),
            Some("active_jobs_present") | Some("blocking_active_jobs") => {
                push_unique_action(&mut actions, "inspect active jobs before proceeding")
            }
            Some("agent_offline") => {
                push_unique_action(&mut actions, "check agent connectivity with list_agents")
            }
            Some("tool_manifest_not_requested") => push_unique_action(
                &mut actions,
                "request tool_manifest if workflow discovery is needed",
            ),
            Some("truncated_by_limit") => push_unique_action(
                &mut actions,
                "continue with the bounded tool_manifest or request a focused category",
            ),
            Some("tool_manifest_unavailable") => {
                push_unique_action(&mut actions, "inspect tool_manifest directly")
            }
            _ => {}
        }
    }

    if actions.is_empty() {
        actions.push("proceed with the coding task using the explicit session_id".to_string());
    }
    let status = aggregate_startup_status(&checks);
    json!({
        "status": status,
        "blocking": status == "fail",
        "checks": checks,
        "suggested_next_actions": actions,
    })
}

fn runtime_status_check(
    output: &Value,
    runtime_status_call_failed: bool,
) -> (&'static str, Option<&'static str>) {
    if runtime_status_call_failed {
        return ("fail", Some("runtime_status_call_failed"));
    }
    let runtime_status = output.get("runtime_status").unwrap_or(&Value::Null);
    if !runtime_status.is_object() {
        return ("fail", Some("runtime_status_unavailable"));
    }
    match runtime_status
        .pointer("/tools/count")
        .and_then(Value::as_u64)
    {
        Some(count) if count > 0 => ("pass", None),
        Some(_) => ("fail", Some("tool_count_zero")),
        None => ("warn", Some("tool_count_unknown")),
    }
}

fn workspace_check(output: &Value) -> (&'static str, Option<&'static str>) {
    let git = output.get("git").unwrap_or(&Value::Null);
    if git.get("available").and_then(Value::as_bool) == Some(false) {
        return ("warn", Some("git_unavailable"));
    }
    // Ordinary tracked/staged/untracked edits are expected development state.
    // An unresolved merge/rebase conflict is a deterministic blocker until it
    // is resolved; the session itself remains usable for inspection and repair.
    let conflicted = git
        .pointer("/counts/conflicted")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if conflicted > 0 {
        return ("fail", Some("workspace_conflicts"));
    }
    match git.get("clean").and_then(Value::as_bool) {
        Some(true) => ("pass", None),
        Some(false) => ("warn", Some("workspace_dirty")),
        None => ("warn", Some("workspace_unknown")),
    }
}

fn startup_jobs_check(output: &Value) -> (&'static str, Option<&'static str>) {
    let jobs = output
        .pointer("/runtime_status/jobs")
        .unwrap_or(&Value::Null);
    if jobs
        .get("blocking_active_count")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        > 0
    {
        return ("fail", Some("blocking_active_jobs"));
    }
    match jobs.get("active_count").and_then(Value::as_u64) {
        Some(0) => ("pass", None),
        Some(_) => ("warn", Some("active_jobs_present")),
        None => ("warn", Some("jobs_unknown")),
    }
}

fn startup_agent_check(output: &Value) -> (&'static str, Option<&'static str>) {
    let executor = output
        .pointer("/resolved_project/executor")
        .and_then(Value::as_str);
    let online = output
        .pointer("/runtime_status/agents/summary/online")
        .or_else(|| output.pointer("/runtime_status/agents/online_count"))
        .and_then(Value::as_u64);
    match (executor, online) {
        (Some("agent"), Some(0)) => ("fail", Some("agent_offline")),
        (Some("agent"), Some(_)) => ("pass", None),
        (Some("local"), _) => ("pass", None),
        (_, Some(_)) => ("pass", None),
        _ => ("warn", Some("agent_health_unknown")),
    }
}

fn startup_tool_manifest_check(
    output: &Value,
    tool_manifest_requested: bool,
) -> (&'static str, Option<&'static str>) {
    if !tool_manifest_requested {
        return ("warn", Some("tool_manifest_not_requested"));
    }
    let Some(manifest) = output.get("tool_manifest") else {
        return ("fail", Some("tool_manifest_unavailable"));
    };
    if !manifest.is_object() {
        return ("fail", Some("tool_manifest_unavailable"));
    }
    if manifest
        .get("truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        if manifest.get("truncation_reason").and_then(Value::as_str) == Some("limit") {
            return ("warn", Some("truncated_by_limit"));
        }
        return ("warn", Some("tool_manifest_truncated"));
    }
    ("pass", None)
}

fn push_startup_check(
    checks: &mut Vec<Value>,
    name: &'static str,
    (status, reason): (&'static str, Option<&'static str>),
) {
    let mut check = json!({
        "name": name,
        "status": status,
    });
    if let Some(reason) = reason {
        check["reason"] = json!(reason);
    }
    checks.push(check);
}

fn aggregate_startup_status(checks: &[Value]) -> &'static str {
    if checks
        .iter()
        .any(|check| check.get("status").and_then(Value::as_str) == Some("fail"))
    {
        "fail"
    } else if checks
        .iter()
        .any(|check| check.get("status").and_then(Value::as_str) == Some("warn"))
    {
        "warn"
    } else {
        "pass"
    }
}

fn push_unique_action(actions: &mut Vec<String>, action: &str) {
    if !actions.iter().any(|existing| existing == action) {
        actions.push(action.to_string());
    }
}

fn merged_suggested_next_actions(output: &Value, verdict: &Value) -> Vec<String> {
    let mut actions = string_array(output.get("suggested_next_actions"));
    for action in string_array(verdict.get("suggested_next_actions")) {
        push_unique_action(&mut actions, &action);
    }
    actions
}

fn string_array(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn finish_suggested_next_actions(
    output: &Value,
    resolved_unexpected_validation_failures: usize,
) -> Vec<String> {
    let mut actions = Vec::new();
    let push = |actions: &mut Vec<String>, action: &str| {
        if !actions.iter().any(|existing| existing == action) {
            actions.push(action.to_string());
        }
    };
    let tool_failures = output.get("tool_failures").unwrap_or(&Value::Null);
    let expectation_mismatch_count = tool_failures
        .get("expectation_mismatch_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let unexpected_success_count = tool_failures
        .get("unexpected_success_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);

    if unresolved_unexpected_failure_count(tool_failures, resolved_unexpected_validation_failures)
        > 0
    {
        push(
            &mut actions,
            "review unexpected failed tool calls before proceeding",
        );
    }
    if expectation_mismatch_count > 0 {
        push(
            &mut actions,
            "review expected failure mismatches before proceeding",
        );
    }
    if unexpected_success_count > 0 {
        push(
            &mut actions,
            "review expected-failure assertions that unexpectedly succeeded",
        );
    }
    if output
        .get("workspace")
        .and_then(|workspace| workspace.get("clean"))
        .and_then(Value::as_bool)
        == Some(false)
    {
        push(&mut actions, "review workspace changes with show_changes");
    }
    if output
        .get("jobs")
        .and_then(|jobs| jobs.get("blocking_active_count"))
        .and_then(Value::as_u64)
        .unwrap_or(0)
        > 0
    {
        push(&mut actions, "stop or await blocking active jobs");
    }
    if validation_has_cargo_test_zero_tests(output.get("validation").unwrap_or(&Value::Null)) {
        push(
            &mut actions,
            "cargo_test ran zero tests; verify the test filter or command",
        );
    }
    actions
}

fn changed_files_count_from_counts(counts: &Value) -> u64 {
    [
        "modified",
        "added",
        "deleted",
        "renamed",
        "copied",
        "untracked",
        "conflicted",
    ]
    .iter()
    .map(|key| counts.get(*key).and_then(Value::as_u64).unwrap_or(0))
    .sum()
}

fn append_workspace_warnings(workspace: &Value, warnings: &mut Vec<Value>) {
    if !workspace
        .get("clean")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        let conflicted = workspace
            .pointer("/counts/conflicted")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let message = if conflicted > 0 {
            "workspace has merge/rebase conflicts; inspect and preserve existing worktree state"
        } else {
            "workspace has existing tracked or untracked changes; inspect and preserve them while editing"
        };
        warnings.push(json!({
            "kind": "dirty_worktree",
            "changed_files_count": workspace
                .get("changed_files_count")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            "conflicted": conflicted,
            "message": message,
        }));
    }
    if !workspace
        .get("git_available")
        .and_then(Value::as_bool)
        .unwrap_or(true)
    {
        warnings.push(json!({
            "kind": "git_unavailable",
            "message": "git-backed workspace inspection unavailable",
        }));
    }
}

fn append_hygiene_warnings(hygiene: &Value, warnings: &mut Vec<Value>) {
    let finding_count = hygiene
        .get("counts")
        .and_then(|counts| counts.get("findings"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if finding_count > 0 {
        warnings.push(json!({
            "kind": "workspace_hygiene_findings",
            "findings": finding_count,
            "message": "workspace hygiene findings should be reviewed",
        }));
    }
}
