//! Server-side tests for structured SSH workspace reads.

use super::super::*;
use super::support::*;
use crate::shell_protocol::{
    RemoteWorkspaceReadOutcome, RemoteWorkspaceReadResponse, ShellAgentShellRequest,
    ShellClientCapabilities, REMOTE_WORKSPACE_READ_RESULT_FORMAT,
};

/// A Runner that declares the SSH workspace-read capability.
async fn register_ssh_workspace_runner(runtime: &ToolRuntime, client_id: &str, project_id: &str) {
    let mut caps = ShellClientCapabilities::default();
    caps.ssh_shell = true;
    caps.ssh_workspace_read = true;
    caps.file_read = true;
    caps.git = true;
    register_agent_projects_for_auth(
        runtime,
        client_id,
        &open_auth_context(),
        caps,
        vec![registered_project(project_id, "/runner-local-project")],
    )
    .await;
}

/// A legacy Runner that predates `ssh_workspace_read` but declares
/// `ssh_shell` + `file_read` + `git` locally.
async fn register_legacy_ssh_runner(runtime: &ToolRuntime, client_id: &str, project_id: &str) {
    let mut caps = ShellClientCapabilities::default();
    caps.ssh_shell = true;
    caps.file_read = true;
    caps.git = true;
    register_agent_projects_for_auth(
        runtime,
        client_id,
        &open_auth_context(),
        caps,
        vec![registered_project(project_id, "/runner-local-project")],
    )
    .await;
}

fn ssh_session(runtime: &ToolRuntime, project: &str, resource: &str) -> sessions::SessionSummary {
    runtime
        .sessions
        .start_session_with_options(
            sessions::SessionCreateOptions::new(
                Some(project.to_string()),
                Some("ssh workspace read".to_string()),
                SessionMode::Normal,
                sessions::SessionGuards::default(),
            )
            .with_execution_context(sessions::SessionExecutionContext {
                default_cwd: Some("/remote/root".to_string()),
                default_shell: None,
                resource: Some(resource.to_string()),
            }),
        )
        .unwrap()
}

#[tokio::test]
async fn legacy_runner_without_ssh_workspace_read_fails_closed_before_enqueue() {
    let runtime = test_runtime();
    register_legacy_ssh_runner(&runtime, "legacy", "demo").await;
    let project = "agent:legacy:demo".to_string();
    let session = ssh_session(&runtime, &project, "tmp");
    let auth = open_auth_context();

    let result = runtime
        .dispatch_with_auth(
            ToolCall::ReadFile {
                project: project.clone(),
                path: "README.md".to_string(),
                session_id: Some(session.session_id.clone()),
                start_line: None,
                limit: None,
                with_line_numbers: None,
            },
            Some(&auth),
        )
        .await;
    assert!(!result.success, "{:?}", result.error);
    assert!(
        result
            .error
            .as_deref()
            .is_some_and(|error| error.contains("ssh_workspace_read")),
        "{:?}",
        result.error
    );
    assert!(
        next_agent_request_for_client(&runtime, "legacy")
            .await
            .is_none(),
        "a legacy Runner must never receive an SSH workspace read request"
    );
}

#[tokio::test]
async fn unsupported_resource_bound_write_fails_closed_without_agent_request() {
    let runtime = test_runtime();
    register_ssh_workspace_runner(&runtime, "ws", "demo").await;
    let project = "agent:ws:demo".to_string();
    let session = ssh_session(&runtime, &project, "tmp");
    let auth = open_auth_context();

    let result = runtime
        .dispatch_with_auth(
            ToolCall::WriteProjectFile {
                project: project.clone(),
                path: "new.txt".to_string(),
                content: "x".to_string(),
                session_id: Some(session.session_id.clone()),
                overwrite: Some(true),
                expected_sha256: None,
                expected_content_prefix: None,
            },
            Some(&auth),
        )
        .await;
    assert!(!result.success, "{:?}", result.error);
    assert!(
        result
            .error
            .as_deref()
            .is_some_and(|error| error.contains("ssh_resource_unsupported_for_request")),
        "{:?}",
        result.error
    );
    assert_eq!(
        result.output["command_started"],
        serde_json::json!(false),
        "{:?}",
        result.output
    );
    assert!(
        next_agent_request_for_client(&runtime, "ws")
            .await
            .is_none(),
        "unsupported resource-bound writes must not enqueue an Agent request"
    );
}

#[tokio::test]
async fn resource_bound_apply_patch_and_checkpoint_fail_closed() {
    let runtime = test_runtime();
    register_ssh_workspace_runner(&runtime, "ws", "demo").await;
    let project = "agent:ws:demo".to_string();
    let session = ssh_session(&runtime, &project, "tmp");
    let auth = open_auth_context();

    let patch = runtime
        .dispatch_with_auth(
            ToolCall::ApplyPatch {
                project: project.clone(),
                session_id: Some(session.session_id.clone()),
                patch: "--- a/x\n+++ b/x\n".to_string(),
            },
            Some(&auth),
        )
        .await;
    assert!(!patch.success, "{:?}", patch.error);
    assert!(
        patch
            .error
            .as_deref()
            .is_some_and(|error| error.contains("ssh_resource_unsupported_for_request")),
        "{:?}",
        patch.error
    );

    let checkpoint = runtime
        .dispatch_with_auth(
            ToolCall::WorkspaceCheckpointCreate {
                project,
                title: Some("t".to_string()),
                note: None,
                include_untracked: None,
                kind: None,
                labels: Vec::new(),
                validation: None,
                session_id: Some(session.session_id),
            },
            Some(&auth),
        )
        .await;
    assert!(!checkpoint.success, "{:?}", checkpoint.error);
    assert!(
        checkpoint
            .error
            .as_deref()
            .is_some_and(|error| error.contains("ssh_resource_unsupported_for_request")),
        "{:?}",
        checkpoint.error
    );
    assert!(
        next_agent_request_for_client(&runtime, "ws")
            .await
            .is_none(),
        "resource-bound checkpoint must not enqueue an Agent request"
    );
}

#[tokio::test]
async fn resource_bound_read_routes_to_ssh_workspace_read_request() {
    let runtime = test_runtime();
    register_ssh_workspace_runner(&runtime, "ws", "demo").await;
    let project = "agent:ws:demo".to_string();
    let session = ssh_session(&runtime, &project, "tmp");
    let auth = open_auth_context();

    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        let session_id = session.session_id.clone();
        let auth = auth.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::ReadFile {
                        project,
                        path: "src/main.rs".to_string(),
                        session_id: Some(session_id),
                        start_line: None,
                        limit: Some(10),
                        with_line_numbers: None,
                    },
                    Some(&auth),
                )
                .await
        }
    });

    let request = next_agent_request_for_client(&runtime, "ws")
        .await
        .expect("SSH workspace read should enqueue");
    assert_eq!(
        request.kind,
        crate::shell_protocol::SSH_WORKSPACE_READ_REQUEST_KIND
    );
    let context = request.job_context.as_ref().expect("safe job context");
    assert_eq!(
        context.workflow_session_id.as_deref(),
        Some(session.session_id.as_str())
    );
    assert_eq!(context.ssh_resource.as_deref(), Some("tmp"));
    assert_eq!(request.cwd.as_deref(), Some("/remote/root"));
    assert_eq!(context.cwd.as_deref(), Some("/remote/root"));
    assert_eq!(context.project_cwd.as_deref(), Some("/remote/root"));
    assert!(request.remote_workspace.is_some(), "typed payload present");
    let read = request.remote_workspace.as_ref().unwrap();
    assert_eq!(read.operation, "read_file");
    assert_eq!(read.path, "src/main.rs");

    // Complete the request with the exact versioned envelope produced by the Runner.
    let remote = RemoteWorkspaceReadResponse {
        format: REMOTE_WORKSPACE_READ_RESULT_FORMAT.to_string(),
        operation: "read_file".to_string(),
        outcome: RemoteWorkspaceReadOutcome::Success {
            exit_code: 0,
            stdout: "{\"format\":\"webcodex.file_read_range.v1\",\"content\":\"remote main\\n\",\"sha256\":\"abc\",\"total_lines\":1,\"start_line\":1,\"limit\":10}".to_string(),
            stdout_truncated: false,
        },
    };
    runtime
        .shell_clients
        .complete(crate::shell_protocol::ShellAgentResultRequest {
            client_id: "ws".to_string(),
            agent_instance_id: "inst-ws".to_string(),
            request_id: request.request_id.clone(),
            exit_code: Some(0),
            stdout: None,
            stderr: None,
            duration_ms: Some(1),
            error: None,
            remote_workspace: Some(remote),
        })
        .await
        .unwrap();
    let result = task.await.unwrap();
    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["path"], "src/main.rs");
    assert_eq!(result.output["executor"], "ssh");
    assert_eq!(result.output["resource"], "tmp");
}

#[tokio::test]
async fn ssh_workspace_contract_matrix_covers_all_ten_tools() {
    let runtime = test_runtime();
    register_ssh_workspace_runner(&runtime, "contract", "demo").await;
    let project = "agent:contract:demo".to_string();
    let session = ssh_session(&runtime, &project, "tmp");
    let sid = Some(session.session_id.clone());
    let auth = open_auth_context();
    let cases: Vec<(ToolCall, &str, Vec<&str>)> = vec![
        (ToolCall::ProjectOverview { project: project.clone(), session_id: sid.clone(), path: None, max_depth: Some(2), limit: Some(50) }, "d ./src\nf ./Cargo.toml\nf ./README.md\n", vec!["schema_version","project","path","deterministic","project_types","manifests","key_files","roots","top_level","suggested_next_reads","scan","warnings"]),
        (ToolCall::ListProjectFiles { project: project.clone(), session_id: sid.clone(), path: None, limit: Some(20) }, "d ./src\nf ./README.md\n", vec!["project","path","entries","truncated"]),
        (ToolCall::ListProjectTrackedFiles { project: project.clone(), session_id: sid.clone(), path: None, globs: None, depth: None, limit: Some(20), offset: None }, "Cargo.toml\0src/lib.rs\0", vec!["project","path","entries","returned","total_files","total_entries","depth","depth_auto","truncated","next_offset","list_truncated","source"]),
        (ToolCall::ReadFile { project: project.clone(), path: "README.md".to_string(), session_id: sid.clone(), start_line: Some(1), limit: Some(10), with_line_numbers: Some(false) }, r#"{"format":"webcodex.file_read_range.v1","content":"remote","sha256":"abc","total_lines":1,"start_line":1,"limit":10}"#, vec!["text","format","path","sha256","total_lines","start_line","limit"]),
        (ToolCall::SearchProjectText { project: project.clone(), pattern: "needle".to_string(), session_id: sid.clone(), path: None, limit: Some(10), context_before: None, context_after: None, include_globs: None, exclude_globs: None, result_mode: None, timeout_secs: None }, "{\"webcodex_search\":{\"backend\":\"grep\",\"feature_unavailable\":false}}\nREADME.md:1:needle\n", vec!["project","pattern","path","backend","result_mode","effective_timeout_secs","matches","count","truncated","truncation_reason","exit_code","context_before","context_after"]),
        (ToolCall::GitStatus { project: project.clone(), session_id: sid.clone() }, "", vec!["exit_code","stdout","stderr"]),
        (ToolCall::GitDiffSummary { project: project.clone(), session_id: sid.clone() }, "", vec!["status","diff_stat","changed_files"]),
        (ToolCall::GitDiff { project: project.clone(), session_id: sid.clone(), args: None }, "diff --git a/a b/a\n", vec!["exit_code","stdout","stderr"]),
        (ToolCall::GitDiffHunks { project: project.clone(), session_id: sid.clone(), paths: Some(vec!["src/lib.rs".to_string()]), max_hunks: Some(10), max_hunk_lines: Some(20), cached: Some(false) }, "", vec!["files","hunk_count","truncated","exit_code","stderr"]),
        (ToolCall::GitLog { project: project.clone(), limit: Some(10), skip: Some(0), session_id: sid }, "", vec!["project","commits","count","limit","skip","truncated"]),
    ];
    for (call, stdout, required) in cases {
        let task = tokio::spawn({
            let runtime = runtime.clone();
            let auth = auth.clone();
            async move { runtime.dispatch_with_auth(call, Some(&auth)).await }
        });
        let request = next_agent_request_for_client(&runtime, "contract")
            .await
            .expect("contract request");
        let operation = request.remote_workspace.as_ref().unwrap().operation.clone();
        runtime
            .shell_clients
            .complete(crate::shell_protocol::ShellAgentResultRequest {
                client_id: "contract".to_string(),
                agent_instance_id: "inst-contract".to_string(),
                request_id: request.request_id,
                exit_code: Some(0),
                stdout: None,
                stderr: None,
                duration_ms: Some(1),
                error: None,
                remote_workspace: Some(RemoteWorkspaceReadResponse {
                    format: REMOTE_WORKSPACE_READ_RESULT_FORMAT.to_string(),
                    operation,
                    outcome: RemoteWorkspaceReadOutcome::Success {
                        exit_code: 0,
                        stdout: stdout.to_string(),
                        stdout_truncated: false,
                    },
                }),
            })
            .await
            .unwrap();
        let result = task.await.unwrap();
        assert!(result.success, "{:?}", result.error);
        for field in required {
            assert!(
                result.output.get(field).is_some(),
                "missing {field}: {}",
                result.output
            );
        }
        assert_eq!(result.output["executor"], "ssh");
        assert_eq!(result.output["resource"], "tmp");
        let encoded = result.output.to_string();
        assert!(!encoded.contains("/remote/root"));
        assert!(!encoded.contains("HostName"));
        assert!(!encoded.contains("ControlPath"));
    }
}

#[tokio::test]
async fn ssh_workspace_git_paths_fail_before_enqueue() {
    let runtime = test_runtime();
    register_ssh_workspace_runner(&runtime, "paths", "demo").await;
    let project = "agent:paths:demo".to_string();
    let session = ssh_session(&runtime, &project, "tmp");
    let auth = open_auth_context();
    for bad in [
        "/etc/passwd",
        "../escape",
        "file://host/path",
        "C:\\temp",
        "\\\\server\\share",
        "a\tb",
    ] {
        let result = runtime
            .dispatch_with_auth(
                ToolCall::GitDiffHunks {
                    project: project.clone(),
                    session_id: Some(session.session_id.clone()),
                    paths: Some(vec![bad.to_string()]),
                    max_hunks: None,
                    max_hunk_lines: None,
                    cached: None,
                },
                Some(&auth),
            )
            .await;
        assert!(!result.success, "accepted {bad:?}");
        assert_eq!(result.output["command_started"], false);
        assert!(next_agent_request_for_client(&runtime, "paths")
            .await
            .is_none());
    }
    let root = runtime
        .dispatch_with_auth(
            ToolCall::GitDiffHunks {
                project,
                session_id: Some(session.session_id),
                paths: Some(vec![".".to_string()]),
                max_hunks: None,
                max_hunk_lines: None,
                cached: None,
            },
            Some(&auth),
        )
        .await;
    assert!(!root.success);
    assert_eq!(root.output["command_started"], false);
}

#[tokio::test]
async fn ssh_workspace_missing_typed_envelope_is_protocol_failure() {
    let runtime = test_runtime();
    register_ssh_workspace_runner(&runtime, "protocol", "demo").await;
    let project = "agent:protocol:demo".to_string();
    let session = ssh_session(&runtime, &project, "tmp");
    let auth = open_auth_context();
    let task = tokio::spawn({
        let runtime = runtime.clone();
        let auth = auth.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::ReadFile {
                        project,
                        path: "README.md".to_string(),
                        session_id: Some(session.session_id),
                        start_line: None,
                        limit: None,
                        with_line_numbers: None,
                    },
                    Some(&auth),
                )
                .await
        }
    });
    let request = next_agent_request_for_client(&runtime, "protocol")
        .await
        .unwrap();
    runtime
        .shell_clients
        .complete(crate::shell_protocol::ShellAgentResultRequest {
            client_id: "protocol".to_string(),
            agent_instance_id: "inst-protocol".to_string(),
            request_id: request.request_id,
            exit_code: Some(0),
            stdout: Some("{partial".to_string()),
            stderr: None,
            duration_ms: Some(1),
            error: None,
            remote_workspace: None,
        })
        .await
        .unwrap();
    let result = task.await.unwrap();
    assert!(!result.success);
    assert_eq!(
        result.output["error_kind"],
        "ssh_workspace_protocol_failure"
    );
}

#[tokio::test]
async fn no_resource_keeps_local_behavior() {
    let runtime = test_runtime();
    register_ssh_workspace_runner(&runtime, "ws", "demo").await;
    let project = "agent:ws:demo".to_string();
    let session = runtime
        .sessions
        .start_session_with_options(
            sessions::SessionCreateOptions::new(
                Some(project.clone()),
                Some("no resource".to_string()),
                SessionMode::Normal,
                sessions::SessionGuards::default(),
            )
            .with_execution_context(sessions::SessionExecutionContext {
                default_cwd: None,
                default_shell: None,
                resource: None,
            }),
        )
        .unwrap();
    let auth = open_auth_context();

    // read_file without a resource enqueues a plain file op, not SSH workspace.
    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        let session_id = session.session_id.clone();
        let auth = auth.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::ReadFile {
                        project,
                        path: "src/main.rs".to_string(),
                        session_id: Some(session_id),
                        start_line: None,
                        limit: Some(10),
                        with_line_numbers: None,
                    },
                    Some(&auth),
                )
                .await
        }
    });
    let request: Option<ShellAgentShellRequest> =
        next_agent_request_for_client(&runtime, "ws").await;
    let request = request.expect("no-resource read_file should enqueue");
    assert_ne!(
        request.kind,
        crate::shell_protocol::SSH_WORKSPACE_READ_REQUEST_KIND,
        "no-resource read must stay on the local file path"
    );
    complete_patch_agent_request_for_instance(
        &runtime,
        "ws",
        "inst-ws",
        &request.request_id,
        0,
        "{\"format\":\"webcodex.file_read_range.v1\",\"content\":\"local\\n\",\"sha256\":\"abc\",\"total_lines\":1,\"start_line\":1,\"limit\":10}",
        "",
    )
    .await;
    let result = task.await.unwrap();
    assert!(result.success, "{:?}", result.error);
}
