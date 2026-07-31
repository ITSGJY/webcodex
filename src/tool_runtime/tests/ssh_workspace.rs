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
    complete_patch_agent_request_for_instance(
        &runtime,
        "ws",
        "inst-ws",
        &request.request_id,
        0,
        &serde_json::to_string(&remote).unwrap(),
        "",
    )
    .await;
    let result = task.await.unwrap();
    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["path"], "src/main.rs");
    assert_eq!(result.output["executor"], "ssh");
    assert_eq!(result.output["resource"], "tmp");
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
