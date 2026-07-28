use super::support::*;
use crate::auth::AuthContext;
use crate::client_window::ClientWindow;
use crate::shell_protocol::{AgentPolicySummary, ShellClientCapabilities};
use crate::tool_runtime::metadata::lookup_tool_metadata;
use crate::tool_runtime::sessions::SessionTransport;
use crate::tool_runtime::validation_parser::VALIDATION_OUTPUT_METADATA_ABSENT_REASON;
use crate::tool_runtime::{
    is_known_tool_name, registered_tool_specs, SessionMode, ToolCall, ToolResult, ToolRuntime,
};
use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;

#[test]
fn coding_task_tools_are_registered_in_metadata_and_openapi() {
    let specs = registered_tool_specs();
    let names: Vec<&str> = specs.iter().map(|spec| spec.name.as_str()).collect();

    for name in ["start_coding_task", "finish_coding_task"] {
        assert!(is_known_tool_name(name), "{name} missing from known names");
        assert!(names.contains(&name), "{name} missing from tool specs");

        let metadata = lookup_tool_metadata(name).expect("metadata");
        assert!(metadata.read_only);
        assert!(!metadata.destructive);
        assert!(!metadata.shell_like);
        assert_eq!(metadata.oauth_scope, Some("runtime:read"));

        let spec = specs
            .iter()
            .find(|spec| spec.name == name)
            .expect("tool spec");
        assert_eq!(spec.annotations["readOnlyHint"], true);
        assert_eq!(spec.annotations["destructiveHint"], false);
        assert_eq!(spec.annotations["openWorldHint"], false);
    }

    let start = spec_named(&specs, "start_coding_task");
    assert_eq!(required_fields(start), vec!["project"]);
    let start_props = start.input_schema["properties"].as_object().unwrap();
    assert!(start_props.contains_key("detail"));
    for removed in [
        "include_runtime_status",
        "compact_startup",
        "include_git",
        "include_recent_commits",
        "include_rules",
        "include_tool_manifest",
        "tool_manifest_intent",
        "tool_manifest_categories",
        "tool_manifest_limit",
    ] {
        assert!(
            !start_props.contains_key(removed),
            "start_coding_task must expose detail instead of legacy {removed}"
        );
    }
    let start_output = crate::tool_runtime::registry::output_schema_for_tool("start_coding_task");
    assert!(
        start_output["properties"]["output"]["properties"]
            .as_object()
            .unwrap()
            .contains_key("authority"),
        "start_coding_task output schema should include authority"
    );
    assert!(
        start_output["properties"]["output"]["properties"]
            .as_object()
            .unwrap()
            .contains_key("startup_verdict"),
        "start_coding_task output schema should include startup_verdict"
    );
    let finish = spec_named(&specs, "finish_coding_task");
    assert_eq!(required_fields(finish), vec!["project", "session_id"]);
    let finish_props = finish.input_schema["properties"].as_object().unwrap();
    assert!(
        finish_props.contains_key("include_workspace"),
        "finish_coding_task input schema should accept include_workspace"
    );
    assert!(
        !required_fields(finish)
            .iter()
            .any(|field| field == "include_workspace"),
        "include_workspace must remain optional"
    );
    let finish_output = crate::tool_runtime::registry::output_schema_for_tool("finish_coding_task");
    let finish_output_props = finish_output["properties"]["output"]["properties"]
        .as_object()
        .unwrap();
    assert!(!finish_output_props.contains_key("verdict"));
    assert!(!finish_output_props.contains_key("finish_verdict"));
    for field in [
        "facts",
        "hard_blockers",
        "advisories",
        "task_outcome",
        "evidence_history",
        "evidence_integrity",
    ] {
        assert!(
            finish_output_props.contains_key(field),
            "finish_coding_task output schema should include {field}"
        );
    }

    let openapi = crate::openapi::build_openapi_spec();
    let tool_call = &openapi["components"]["schemas"]["ToolCallRequest"];
    let tool_desc = tool_call["properties"]["tool"]["description"]
        .as_str()
        .unwrap();
    assert!(tool_desc.contains("start_coding_task"));
    assert!(tool_desc.contains("finish_coding_task"));
    let properties = tool_call["properties"].as_object().unwrap();
    for field in [
        "detail",
        "bind_current",
        "include_hygiene",
        "include_handoff",
        "include_workspace",
        "include_validation_summary",
        "include_validation",
        "summary_only",
        "expected_failure",
        "expected_failure_kind",
        "assertion_name",
    ] {
        assert!(
            properties.contains_key(field),
            "ToolCallRequest missing flattened field {field}"
        );
    }
    let operation_count: usize = openapi["paths"]
        .as_object()
        .unwrap()
        .values()
        .map(|methods| methods.as_object().unwrap().len())
        .sum();
    assert_eq!(operation_count, 25, "no dedicated OpenAPI operations added");

    let call = ToolCall::from_tool_name(
        "start_coding_task",
        json!({"project": "agent:test:demo", "detail": "full"}),
    )
    .expect("detail should deserialize through ToolCall");
    assert_eq!(
        call.session_log_arguments()["detail"],
        "full",
        "ToolCall audit serialization must preserve detail"
    );
}

#[tokio::test]
async fn start_coding_task_returns_session_and_does_not_bind_current_by_default() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    commit_file(
        tmp.path(),
        "AGENTS.md",
        &json!({
            "format": "webcodex.file_read_range.v1",
            "content": "# Rules\n\nUse focused tests.\n",
            "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "total_lines": 3,
            "start_line": 1,
            "limit": 2000
        })
        .to_string(),
        "add instructions",
    );
    commit_file(tmp.path(), "README.md", "hello\n", "add readme");
    let runtime = test_runtime();
    let project =
        register_agent_project_at_path(&runtime, "coding-start", "demo", tmp.path()).await;
    let auth = auth_context(None, true);

    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        let auth = auth.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::StartCodingTask {
                        project,
                        title: Some("implement deterministic aggregate".to_string()),
                        mode: SessionMode::Normal,
                        detail: crate::tool_runtime::StartupDetail::Full,
                        deny_write_tools: false,
                        deny_shell_tools: false,
                        bind_current: false,
                    },
                    Some(&auth),
                )
                .await
        }
    });

    let rules_req = next_patch_agent_request(&runtime, "coding-start")
        .await
        .expect("start_coding_task should load rules through the agent");
    assert_eq!(rules_req.kind, "file_read");
    complete_patch_agent_request(
        &runtime,
        "coding-start",
        &rules_req.request_id,
        0,
        &canonical_agent_file_read_output("# Rules\n\nUse focused tests.\n", 3),
        "",
    )
    .await;

    let status_req = next_patch_agent_request(&runtime, "coding-start")
        .await
        .expect("start_coding_task should inspect git status through the agent");
    assert!(status_req.command.contains("git status --porcelain=v1 -b"));
    let show_changes_stdout =
        "## main\n@@WEBCODEX_SHOW_CHANGES_SEP@@\nabc123\0abc123\0add readme\n@@WEBCODEX_SHOW_CHANGES_SEP@@\n";
    complete_patch_agent_request(
        &runtime,
        "coding-start",
        &status_req.request_id,
        0,
        show_changes_stdout,
        "",
    )
    .await;

    let log_req = next_patch_agent_request(&runtime, "coding-start")
        .await
        .expect("start_coding_task should inspect recent commits through the agent");
    assert!(log_req.command.contains("git log"));
    let commit_stdout = "0123456789012345678901234567890123456789\u{1f}0123456\u{1f}HEAD -> main\u{1f}WebCodex Test\u{1f}test@example.com\u{1f}2026-01-01T00:00:00+00:00\u{1f}add readme\u{1e}";
    complete_patch_agent_request(
        &runtime,
        "coding-start",
        &log_req.request_id,
        0,
        commit_stdout,
        "",
    )
    .await;

    let result = task.await.unwrap();

    assert!(result.success, "{:?}", result.error);
    let session_id = result.output["session"]["session_id"].as_str().unwrap();
    assert!(session_id.starts_with("wc_sess_"));
    assert_eq!(
        result.output["session"]["explicit_session_id_recommended"],
        true
    );
    assert_eq!(
        result.output["session"]["current_binding"]["bound"], false,
        "start_coding_task must not bind current by default"
    );
    assert_eq!(
        result.output["session"]["current_binding"]["process_local_in_memory"],
        true
    );
    for field in [
        "session",
        "runtime_status",
        "authority",
        "rules",
        "git",
        "semantic_navigation",
        "recommended_flow",
        "warnings",
        "tool_manifest",
    ] {
        assert!(
            result.output.get(field).is_some(),
            "start_coding_task output should include {field}"
        );
    }
    assert_eq!(result.output["authority"]["mode"], "trusted_agent");
    assert_eq!(result.output["authority"]["human_approval_required"], false);

    let window = ClientWindow::for_test("coding-task-window");
    let current = runtime
        .dispatch_with_auth_transport_options_and_metadata_with_sandbox(
            ToolCall::CurrentSession {
                project: project.clone(),
            },
            None,
            SessionTransport::Api,
            true,
            false,
            Default::default(),
            None,
            Some(&window),
        )
        .await;
    assert!(current.success, "{:?}", current.error);
    assert_eq!(current.output["found"], false);

    let inspect = result.output["recommended_flow"]["inspect"]
        .as_array()
        .unwrap();
    assert!(contains_string(inspect, "read_file"));
    assert!(contains_string(inspect, "search_project_text"));
    assert!(contains_string(inspect, "show_changes"));
    let edit = result.output["recommended_flow"]["edit"]
        .as_array()
        .unwrap();
    assert!(contains_string(edit, "apply_text_edits"));
    assert!(contains_string(edit, "apply_patch_checked"));
    assert!(contains_string(edit, "write_project_file"));
    assert!(!contains_string(edit, "replace_line_range"));
    assert!(!contains_string(edit, "insert_at_line"));
    assert!(!contains_string(edit, "delete_line_range"));

    assert_eq!(result.output["rules"]["present"], true);
    assert_eq!(result.output["rules"]["sources"][0]["path"], "AGENTS.md");
    let manifest = &result.output["tool_manifest"];
    assert_eq!(manifest["schema_version"], 1);
    assert_eq!(manifest["intent"], Value::Null);
    assert_eq!(manifest["filtered"], false);
    assert_eq!(manifest["categories_requested"], Value::Null);
    assert_eq!(manifest["limit"], Value::Null);
    assert_eq!(manifest["truncated"], false);
    assert!(manifest["count"].as_u64().unwrap() > 0);
    let start_tool = manifest["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "start_coding_task")
        .expect("start_coding_task manifest entry");
    assert!(start_tool["accepted_flattened_args"]
        .as_array()
        .unwrap()
        .iter()
        .any(|field| field == "detail"));
    for removed in [
        "include_tool_manifest",
        "tool_manifest_intent",
        "compact_startup",
        "tool_manifest_categories",
        "tool_manifest_limit",
    ] {
        assert!(!start_tool["accepted_flattened_args"]
            .as_array()
            .unwrap()
            .iter()
            .any(|field| field == removed));
    }
    assert!(start_tool.get("inputSchema").is_none());
    assert!(start_tool.get("outputSchema").is_none());
    assert_eq!(result.output["git"]["clean"], true);
    assert!(
        result.output["git"]["recent_commits"]
            .as_array()
            .unwrap()
            .len()
            >= 1
    );
}

#[tokio::test]
async fn start_coding_task_can_omit_compact_tool_manifest() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    let runtime = test_runtime();
    let project =
        register_agent_project_at_path(&runtime, "coding-no-manifest", "demo", tmp.path()).await;
    let auth = auth_context(None, true);

    let result = runtime
        .dispatch_with_auth(
            ToolCall::StartCodingTask {
                project,
                title: Some("small startup payload".to_string()),
                mode: SessionMode::Normal,
                detail: Default::default(),
                deny_write_tools: false,
                deny_shell_tools: false,
                bind_current: false,
            },
            Some(&auth),
        )
        .await;

    assert!(result.success, "{:?}", result.error);
    assert!(
        result.output.get("tool_manifest").is_none(),
        "include_tool_manifest=false should omit compact manifest"
    );
}

#[tokio::test]
async fn start_coding_task_minimal_runtime_status_is_compact_and_path_safe() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    let runtime = test_runtime();
    let policy = AgentPolicySummary {
        allowed_roots: vec![PathBuf::from("/tmp/startup-full-allowed-root")],
        ..Default::default()
    };
    register_agent_with_shell_profiles(
        &runtime,
        "coding-full-status",
        Some(policy),
        vec![registered_project("demo", &tmp.path().to_string_lossy())],
    )
    .await;
    let auth = auth_context(None, true);
    let project = "agent:coding-full-status:demo".to_string();

    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        let auth = auth.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::from_tool_name(
                        "start_coding_task",
                        json!({
                            "project": project,
                            "detail": "minimal"
                        }),
                    )
                    .unwrap(),
                    Some(&auth),
                )
                .await
        }
    });
    let request = next_patch_agent_request(&runtime, "coding-full-status")
        .await
        .expect("minimal startup should inspect workspace state");
    complete_agent_request_by_running_locally(&runtime, "coding-full-status", request).await;
    let result = task.await.unwrap();

    assert!(result.success, "{:?}", result.error);
    let runtime_status = &result.output["runtime_status"];
    assert_eq!(runtime_status["compact"], true);
    assert!(runtime_status["tools"]["names"].is_null());
    assert!(!serde_json::to_string(runtime_status)
        .unwrap()
        .contains("/tmp/startup-full-allowed-root"));
    assert!(result.output["connection_state"]["runner_process"]["status"].is_string());
    for omitted in [
        "tool_manifest",
        "rules",
        "recent_commits",
        "authority",
        "recommended_flow",
    ] {
        assert!(
            result.output.get(omitted).is_none(),
            "minimal startup must omit {omitted}"
        );
    }
    assert!(result.output["git"].get("recent_commits").is_none());
}

#[tokio::test]
async fn start_coding_task_compact_startup_returns_sanitized_runtime_summary() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    let runtime = test_runtime();
    let policy = AgentPolicySummary {
        allowed_roots: vec![PathBuf::from("/tmp/compact-allowed-root-never-emit")],
        ..Default::default()
    };
    register_agent_with_shell_profiles(
        &runtime,
        "coding-compact-status",
        Some(policy),
        vec![registered_project("demo", &tmp.path().to_string_lossy())],
    )
    .await;
    let auth = auth_context(None, true);
    let project = "agent:coding-compact-status:demo".to_string();

    let result = start_coding_task_serviced(
        &runtime,
        "coding-compact-status",
        json!({ "project": project }),
        &auth,
    )
    .await;

    assert!(result.success, "{:?}", result.error);
    let summary = &result.output["runtime_status"];
    assert_eq!(summary["compact"], true);
    for pointer in [
        "/build/version",
        "/build/git_commit",
        "/build/git_dirty",
        "/tools/count",
        "/jobs/active_count",
        "/agents/summary/online",
        "/projects/effective/status",
        "/projects/agent_registered/online_count",
    ] {
        assert!(
            summary.pointer(pointer).is_some(),
            "compact startup runtime_status should include {pointer}"
        );
    }
    assert_eq!(summary["build"]["version"], env!("CARGO_PKG_VERSION"));
    assert!(summary["build"].get("git_commit").is_some());
    assert!(summary["build"].get("git_dirty").is_some());
    assert!(summary["tools"]["count"].as_u64().unwrap() > 0);
    assert!(summary["tools"].get("names").is_none());
    assert_eq!(summary["jobs"]["active_count"], 0);
    assert!(summary["agents"]["summary"].is_object());
    assert_eq!(summary["agents"]["summary"]["count"], 1);
    assert_eq!(summary["agents"]["summary"]["online"], 1);
    assert_eq!(
        summary["agents"]["summary"]["clients"][0]["client_id"],
        "coding-compact-status"
    );
    assert_eq!(
        summary["agents"]["summary"]["clients"][0]["status"],
        "online"
    );
    assert_eq!(
        summary["agents"]["summary"]["clients"][0]["transport"],
        "polling"
    );
    assert_eq!(
        summary["agents"]["summary"]["clients"][0]["projects_count"],
        1
    );
    assert_eq!(summary["projects"]["effective"]["status"], "ok");
    assert_eq!(summary["projects"]["effective"]["count"], 1);
    assert_eq!(summary["projects"]["agent_registered"]["count"], 1);
    assert_eq!(summary["projects"]["agent_registered"]["online_count"], 1);
    assert!(summary["projects"].get("server_static").is_none());
    let verdict = &result.output["startup_verdict"];
    assert_startup_verdict_shape(verdict);
    assert_ne!(verdict["status"], "fail");
    assert_eq!(verdict["blocking"], false);
    assert_check_reason(verdict, "tool_manifest", "tool_manifest_not_requested");
    assert_compact_verdict_safe(verdict, "startup verdict");

    let serialized = serde_json::to_string(summary).unwrap();
    for forbidden in [
        "tools.names",
        "policy",
        "allowed_roots",
        "compact-allowed-root-never-emit",
        "stdout",
        "stderr",
        "command",
        "env",
        "token",
        "secret",
    ] {
        assert!(
            !serialized.contains(forbidden),
            "compact startup leaked {forbidden}: {serialized}"
        );
    }
}

#[tokio::test]
async fn start_coding_task_compact_startup_verdict_accepts_clean_workspace() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    commit_file(tmp.path(), "README.md", "hello\n", "add readme");
    let runtime = test_runtime();
    let project =
        register_agent_project_at_path(&runtime, "coding-start-verdict", "demo", tmp.path()).await;
    let auth = auth_context(None, true);

    let result = start_coding_task_serviced(
        &runtime,
        "coding-start-verdict",
        json!({ "project": project, "detail": "full" }),
        &auth,
    )
    .await;

    assert!(result.success, "{:?}", result.error);
    let verdict = &result.output["startup_verdict"];
    assert_startup_verdict_shape(verdict);
    assert_eq!(verdict["status"], "pass");
    assert_eq!(verdict["blocking"], false);
    assert_check_status(verdict, "runtime_status", "pass");
    assert_check_status(verdict, "workspace", "pass");
    assert_check_status(verdict, "jobs", "pass");
    assert_check_status(verdict, "agent", "pass");
    assert_check_status(verdict, "tool_manifest", "pass");
    assert_compact_verdict_safe(verdict, "startup clean verdict");
}

/// Shared helper: start_coding_task with git inspection against a real temp repo.
async fn start_coding_task_with_git_inspection(
    runtime: &ToolRuntime,
    client_id: &str,
    project: &str,
    auth: &AuthContext,
) -> ToolResult {
    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.to_string();
        let auth = auth.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::from_tool_name(
                        "start_coding_task",
                        json!({
                            "project": project,
                        }),
                    )
                    .unwrap(),
                    Some(&auth),
                )
                .await
        }
    });
    let req = next_patch_agent_request(runtime, client_id)
        .await
        .expect("start_coding_task should inspect workspace git status");
    complete_agent_request_by_running_locally(runtime, client_id, req).await;
    task.await.unwrap()
}

/// Shared helper: dispatch start_coding_task and service every startup agent
/// request (rules read, git status, git log) locally until the call finishes.
async fn start_coding_task_serviced(
    runtime: &ToolRuntime,
    client_id: &str,
    params: Value,
    auth: &AuthContext,
) -> ToolResult {
    let task = tokio::spawn({
        let runtime = runtime.clone();
        let auth = auth.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::from_tool_name("start_coding_task", params).unwrap(),
                    Some(&auth),
                )
                .await
        }
    });
    for _ in 0..200 {
        if task.is_finished() {
            break;
        }
        if let Some(req) = next_patch_agent_request(runtime, client_id).await {
            complete_agent_request_by_running_locally(runtime, client_id, req).await;
        } else {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }
    assert!(
        task.is_finished(),
        "start_coding_task did not finish after servicing startup agent requests"
    );
    task.await.unwrap()
}

fn assert_startup_nonblocking_dirty(result: &ToolResult, workspace_reason: &str) {
    assert!(result.success, "{:?}", result.error);
    let session_id = result.output["session"]["session_id"]
        .as_str()
        .expect("session_id");
    assert!(session_id.starts_with("wc_sess_"), "{session_id}");
    let verdict = &result.output["startup_verdict"];
    assert_startup_verdict_shape(verdict);
    assert_eq!(
        verdict["blocking"], false,
        "dirty workspace must not block: {verdict}"
    );
    assert_ne!(
        verdict["status"], "fail",
        "dirty workspace must not fail startup: {verdict}"
    );
    assert_eq!(verdict["status"], "warn");
    assert_check_status(verdict, "workspace", "warn");
    assert_check_reason(verdict, "workspace", workspace_reason);
    assert_eq!(result.output["git"]["clean"], false);
    assert!(
        result.output["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w["kind"] == "dirty_worktree"),
        "top-level dirty_worktree warning expected: {}",
        result.output["warnings"]
    );
}

#[tokio::test]
async fn start_coding_task_untracked_only_is_nonblocking_warning() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    commit_file(tmp.path(), "README.md", "hello\n", "add readme");
    fs::write(tmp.path().join("report.md"), "audit report\n").unwrap();
    let report_before = fs::read_to_string(tmp.path().join("report.md")).unwrap();

    let runtime = test_runtime();
    let client_id = "coding-start-untracked";
    let project = register_agent_project_at_path(&runtime, client_id, "demo", tmp.path()).await;
    let auth = auth_context(None, true);

    let result = start_coding_task_with_git_inspection(&runtime, client_id, &project, &auth).await;

    assert_startup_nonblocking_dirty(&result, "workspace_dirty");
    assert_eq!(result.output["git"]["counts"]["untracked"], 1);
    assert_eq!(result.output["git"]["counts"]["modified"], 0);
    assert_eq!(
        fs::read_to_string(tmp.path().join("report.md")).unwrap(),
        report_before,
        "start_coding_task must not modify untracked report.md"
    );
}

#[tokio::test]
async fn start_coding_task_tracked_modified_is_nonblocking_and_allows_continued_edit() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    fs::create_dir_all(tmp.path().join("src")).unwrap();
    commit_file(
        tmp.path(),
        "src/example.rs",
        "fn main() {\n    println!(\"head\");\n}\n",
        "add example",
    );
    // Pre-existing worktree change (M) that must be preserved as the edit baseline.
    let dirty_content = "fn main() {\n    println!(\"user-wip\");\n}\n";
    fs::write(tmp.path().join("src/example.rs"), dirty_content).unwrap();

    let runtime = test_runtime();
    let client_id = "coding-start-modified";
    let project = register_agent_project_at_path(&runtime, client_id, "demo", tmp.path()).await;
    let auth = auth_context(None, true);

    let result = start_coding_task_with_git_inspection(&runtime, client_id, &project, &auth).await;

    assert_startup_nonblocking_dirty(&result, "workspace_dirty");
    assert_eq!(result.output["git"]["counts"]["modified"], 1);
    assert_eq!(result.output["git"]["counts"]["unstaged"], 1);

    // Worktree content is the real baseline: edit must match current disk, not HEAD.
    let worktree = fs::read_to_string(tmp.path().join("src/example.rs")).unwrap();
    assert_eq!(worktree, dirty_content);
    assert!(worktree.contains("user-wip"));
    assert!(!worktree.contains("head"));

    let (updated, out) = crate::tool_runtime::files::apply_line_edit_content(
        &worktree,
        "src/example.rs",
        crate::tool_runtime::files::LineEditOperation::Replace,
        Some(2),
        Some(2),
        None,
        "    println!(\"user-wip-plus-agent\");",
        None,
        Some("    println!(\"user-wip\");"),
    )
    .expect("continued edit on already-modified worktree content must succeed");
    assert_eq!(out["changed"], true);
    assert!(updated.contains("user-wip-plus-agent"));
    assert!(
        updated.contains("fn main()"),
        "must preserve surrounding worktree content: {updated}"
    );
    assert!(
        !updated.contains("head"),
        "must not revert to HEAD content: {updated}"
    );

    // Applying against HEAD-only content with the same worktree expected_prefix fails,
    // proving the tool is not using HEAD as the silent baseline.
    let head_content = "fn main() {\n    println!(\"head\");\n}\n";
    let head_err = crate::tool_runtime::files::apply_line_edit_content(
        head_content,
        "src/example.rs",
        crate::tool_runtime::files::LineEditOperation::Replace,
        Some(2),
        Some(2),
        None,
        "    println!(\"user-wip-plus-agent\");",
        None,
        Some("    println!(\"user-wip\");"),
    )
    .unwrap_err();
    assert!(
        head_err.contains("expected_old_prefix mismatch")
            || head_err.contains("Rejected before write"),
        "HEAD baseline should not satisfy worktree prefix: {head_err}"
    );

    // Persist continued edit and confirm disk still differs from a clean checkout.
    fs::write(tmp.path().join("src/example.rs"), &updated).unwrap();
    assert_eq!(
        fs::read_to_string(tmp.path().join("src/example.rs")).unwrap(),
        updated
    );
}

#[tokio::test]
async fn start_coding_task_staged_changes_are_nonblocking() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    fs::create_dir_all(tmp.path().join("src")).unwrap();
    commit_file(tmp.path(), "src/example.rs", "fn a() {}\n", "add example");
    fs::write(
        tmp.path().join("src/example.rs"),
        "fn a() { /* staged */ }\n",
    )
    .unwrap();
    let (exit_code, stdout, stderr, _) =
        crate::tool_runtime::helpers::run_command_sync("git add -- src/example.rs", tmp.path(), 30);
    assert_eq!(exit_code, 0, "git add failed\n{stdout}\n{stderr}");

    let runtime = test_runtime();
    let client_id = "coding-start-staged";
    let project = register_agent_project_at_path(&runtime, client_id, "demo", tmp.path()).await;
    let auth = auth_context(None, true);

    let result = start_coding_task_with_git_inspection(&runtime, client_id, &project, &auth).await;

    assert_startup_nonblocking_dirty(&result, "workspace_dirty");
    assert_eq!(result.output["git"]["counts"]["staged"], 1);
    // Staging area must remain intact (no auto unstage).
    let (exit_code, status_stdout, stderr, _) =
        crate::tool_runtime::helpers::run_command_sync("git status --porcelain", tmp.path(), 30);
    assert_eq!(exit_code, 0, "{stderr}");
    assert!(
        status_stdout.lines().any(|line| line.starts_with("M ")),
        "staged entry should remain staged: {status_stdout}"
    );
}

#[tokio::test]
async fn start_coding_task_mixed_dirty_workspace_summarizes_counts_without_blocking() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    commit_file(tmp.path(), "tracked.rs", "tracked\n", "add tracked");
    commit_file(tmp.path(), "staged.rs", "staged-base\n", "add staged");
    fs::write(tmp.path().join("tracked.rs"), "tracked-mod\n").unwrap();
    fs::write(tmp.path().join("staged.rs"), "staged-mod\n").unwrap();
    let (exit_code, _, stderr, _) =
        crate::tool_runtime::helpers::run_command_sync("git add -- staged.rs", tmp.path(), 30);
    assert_eq!(exit_code, 0, "{stderr}");
    fs::write(tmp.path().join("notes.md"), "notes\n").unwrap();

    let runtime = test_runtime();
    let client_id = "coding-start-mixed";
    let project = register_agent_project_at_path(&runtime, client_id, "demo", tmp.path()).await;
    let auth = auth_context(None, true);

    let result = start_coding_task_with_git_inspection(&runtime, client_id, &project, &auth).await;

    assert_startup_nonblocking_dirty(&result, "workspace_dirty");
    assert_eq!(result.output["git"]["counts"]["modified"], 2);
    assert_eq!(result.output["git"]["counts"]["staged"], 1);
    assert_eq!(result.output["git"]["counts"]["unstaged"], 1);
    assert_eq!(result.output["git"]["counts"]["untracked"], 1);
    assert!(
        result.output["git"]["changed_files_count"]
            .as_u64()
            .unwrap()
            >= 3,
        "changed_files_count: {}",
        result.output["git"]["changed_files_count"]
    );
}

#[tokio::test]
async fn start_coding_task_conflict_state_is_a_hard_blocker_but_remains_inspectable() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    commit_file(tmp.path(), "conflicted.rs", "base\n", "base");
    let (exit_code, _, stderr, _) =
        crate::tool_runtime::helpers::run_command_sync("git checkout -b other", tmp.path(), 30);
    assert_eq!(exit_code, 0, "{stderr}");
    commit_file(tmp.path(), "conflicted.rs", "other-side\n", "other");
    let (exit_code, _, stderr, _) =
        crate::tool_runtime::helpers::run_command_sync("git checkout -", tmp.path(), 30);
    assert_eq!(exit_code, 0, "{stderr}");
    commit_file(tmp.path(), "conflicted.rs", "main-side\n", "main");
    let (exit_code, _, stderr, _) =
        crate::tool_runtime::helpers::run_command_sync("git merge other || true", tmp.path(), 30);
    assert_eq!(exit_code, 0, "merge helper failed: {stderr}");
    // Ensure conflict markers exist on disk for inspection.
    let conflict_body = fs::read_to_string(tmp.path().join("conflicted.rs")).unwrap();
    assert!(
        conflict_body.contains("<<<<<<<") || conflict_body.contains("other-side"),
        "expected conflict content: {conflict_body}"
    );

    let runtime = test_runtime();
    let client_id = "coding-start-conflict";
    let project = register_agent_project_at_path(&runtime, client_id, "demo", tmp.path()).await;
    let auth = auth_context(None, true);

    let result = start_coding_task_with_git_inspection(&runtime, client_id, &project, &auth).await;

    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["startup_verdict"]["status"], "fail");
    assert_eq!(result.output["startup_verdict"]["blocking"], true);
    assert_check_status(&result.output["startup_verdict"], "workspace", "fail");
    assert_check_reason(
        &result.output["startup_verdict"],
        "workspace",
        "workspace_conflicts",
    );
    assert!(
        result.output["git"]["counts"]["conflicted"]
            .as_u64()
            .unwrap()
            >= 1,
        "conflicted count: {}",
        result.output["git"]["counts"]
    );
    // Session still usable for read/inspect of conflicted path content.
    assert!(
        fs::read_to_string(tmp.path().join("conflicted.rs"))
            .unwrap()
            .contains("main-side")
            || fs::read_to_string(tmp.path().join("conflicted.rs"))
                .unwrap()
                .contains("<<<<<<<"),
        "conflict file must remain readable"
    );
}

#[tokio::test]
async fn start_coding_task_unknown_project_still_fails() {
    let runtime = test_runtime();
    let auth = auth_context(None, true);
    let result = runtime
        .dispatch_with_auth(
            ToolCall::from_tool_name(
                "start_coding_task",
                json!({
                    "project": "agent:missing:does-not-exist"
                }),
            )
            .unwrap(),
            Some(&auth),
        )
        .await;
    assert!(
        !result.success,
        "unresolvable project must fail: {:?}",
        result.output
    );
    assert!(
        result
            .error
            .as_deref()
            .unwrap_or("")
            .to_ascii_lowercase()
            .contains("project")
            || result
                .error
                .as_deref()
                .unwrap_or("")
                .to_ascii_lowercase()
                .contains("not found")
            || result
                .error
                .as_deref()
                .unwrap_or("")
                .to_ascii_lowercase()
                .contains("unknown"),
        "expected project resolution error: {:?}",
        result.error
    );
}

#[tokio::test]
async fn start_coding_task_agent_offline_is_still_blocking() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    commit_file(tmp.path(), "README.md", "hello\n", "add readme");
    let runtime = test_runtime();
    let project =
        register_agent_project_at_path(&runtime, "coding-start-offline", "demo", tmp.path()).await;
    // Transport disconnect leaves the agent offline while project id may still resolve.
    runtime
        .shell_clients
        .reconcile_disconnect("coding-start-offline", "inst")
        .await;

    let auth = auth_context(None, true);
    let result = runtime
        .dispatch_with_auth(
            ToolCall::from_tool_name(
                "start_coding_task",
                json!({
                    "project": project
                }),
            )
            .unwrap(),
            Some(&auth),
        )
        .await;

    // Project resolution or agent health may fail closed — either is blocking.
    if result.success {
        let verdict = &result.output["startup_verdict"];
        assert_startup_verdict_shape(verdict);
        assert_eq!(
            verdict["blocking"], true,
            "agent offline / unreachable must remain blocking: {verdict}"
        );
        assert_eq!(verdict["status"], "fail");
    } else {
        assert!(
            result.error.is_some(),
            "infrastructure failure must surface an error"
        );
    }
}

#[tokio::test]
async fn start_coding_task_rejects_removed_startup_and_manifest_params() {
    for (field, value) in [
        ("include_runtime_status", json!(false)),
        ("compact_startup", json!(true)),
        ("include_git", json!(false)),
        ("include_recent_commits", json!(false)),
        ("include_rules", json!(false)),
        ("include_tool_manifest", json!(true)),
        ("tool_manifest_intent", json!("coding")),
        ("tool_manifest_categories", json!(["workflow", "session"])),
        ("tool_manifest_limit", json!(2)),
    ] {
        let mut params = json!({ "project": "agent:demo:demo", "detail": "standard" });
        params[field] = value;
        let error = ToolCall::from_tool_name("start_coding_task", params)
            .expect_err("removed startup param must be rejected");
        assert!(
            error.starts_with("invalid arguments for tool 'start_coding_task': unknown field(s)"),
            "unexpected rejection error for removed field {field}: {error}"
        );
        assert!(
            error.contains(field),
            "rejection must name the unknown field {field}: {error}"
        );
    }
}

#[tokio::test]
async fn finish_coding_task_requires_explicit_session_and_returns_structured_fields() {
    let missing_session =
        ToolCall::from_tool_name("finish_coding_task", json!({"project": "demo"})).unwrap_err();
    assert!(missing_session.contains("session_id"));

    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    commit_file(tmp.path(), "README.md", "hello\n", "add readme");
    let runtime = test_runtime();
    let project =
        register_agent_project_at_path(&runtime, "coding-finish", "demo", tmp.path()).await;
    let auth = auth_context(None, true);
    let start = runtime
        .dispatch_with_auth(
            ToolCall::StartCodingTask {
                project: project.clone(),
                title: Some("finish contract".to_string()),
                mode: SessionMode::Normal,
                detail: Default::default(),
                deny_write_tools: false,
                deny_shell_tools: false,
                bind_current: false,
            },
            Some(&auth),
        )
        .await;
    assert!(start.success, "{:?}", start.error);
    let session_id = start.output["session"]["session_id"]
        .as_str()
        .unwrap()
        .to_string();

    fs::write(tmp.path().join("README.md"), "hello\nchanged\n").unwrap();
    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        let session_id = session_id.clone();
        let auth = auth.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::FinishCodingTask {
                        project,
                        session_id,
                        summary_only: false,
                        include_diff: Some(false),
                        include_workspace: None,
                        include_hygiene: Some(false),
                        include_handoff: Some(false),
                        include_validation_summary: Some(true),
                    },
                    Some(&auth),
                )
                .await
        }
    });
    let req = next_patch_agent_request(&runtime, "coding-finish")
        .await
        .expect("finish_coding_task should inspect changes through the agent");
    assert!(req.command.contains("git status --porcelain=v1 -b"));
    let show_changes_stdout = "## main\n M README.md\n@@WEBCODEX_SHOW_CHANGES_SEP@@\nabc123\0abc123\0add readme\n@@WEBCODEX_SHOW_CHANGES_SEP@@\n README.md | 1 +\n 1 file changed, 1 insertion(+)\n";
    complete_patch_agent_request(
        &runtime,
        "coding-finish",
        &req.request_id,
        0,
        show_changes_stdout,
        "",
    )
    .await;
    let result = task.await.unwrap();

    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["session_id"], session_id);
    assert_eq!(result.output["deterministic"], true);
    assert_eq!(result.output["llm_summary"], false);
    assert_eq!(result.output["workspace"]["clean"], false);
    assert_eq!(result.output["changes"]["hunks_truncated"], false);
    assert!(result.output["changes"]["show_changes"].is_object());
    let validation = &result.output["validation"];
    assert_eq!(validation["available"], false);
    assert_eq!(validation["status"], "not_run");
    assert_eq!(validation["reason"], "no_validation_tool_invoked");
    assert_eq!(validation["source"], "session_ledger");
    assert_eq!(validation["events_total"], 0);
    assert!(validation["events"].as_array().unwrap().is_empty());
    assert_eq!(result.output["permissions"]["policy"], "trusted_agent");
    assert_eq!(result.output["permissions"]["required_count"], 0);
    assert_eq!(result.output["permissions"]["auto_approved_count"], 0);
    assert_eq!(result.output["permissions"]["manual_approved_count"], 0);
    assert_eq!(result.output["permissions"]["approved_count"], 0);
    assert_eq!(result.output["permissions"]["total_approved_count"], 0);
    assert!(result.output["permissions"]["recent"]
        .as_array()
        .unwrap()
        .is_empty());
    assert_eq!(validation["parser"]["available"], false);
    assert_eq!(
        validation["parser"]["reason"],
        VALIDATION_OUTPUT_METADATA_ABSENT_REASON
    );
    assert_no_raw_validation_output_fields(validation, "finish validation summary");
    assert!(validation.get("observed_commands").is_none());
    assert_eq!(result.output["review_evidence"]["available"], true);
    assert_eq!(result.output["review_evidence"]["source"], "session_ledger");
    assert_eq!(result.output["review_evidence"]["total"], 1);
    assert_eq!(
        result.output["review_evidence"]["workspace_review_count"],
        1
    );
    assert_eq!(
        result.output["review_evidence"]["tools"],
        json!(["show_changes"])
    );
    assert_review_evidence_tools_safe(&result.output["review_evidence"]);
    assert!(result.output["hygiene"].is_null());
    assert!(result.output["handoff"].is_null());
    assert!(result.output["final_warnings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|warning| warning["kind"] == "dirty_worktree"));
    assert_eq!(result.output["task_outcome"]["status"], "warn");
    assert_eq!(result.output["evidence_history"]["status"], "clean");
    assert_eq!(result.output["evidence_integrity"]["status"], "clean");
    assert!(result.output["informational_notes"].is_array());
    assert_eq!(result.output["task_outcome"]["blocking"], false);
    assert!(result.output["advisories"]
        .as_array()
        .unwrap()
        .iter()
        .any(|reason| reason == "workspace_dirty"));
    assert_finish_uses_canonical_outcomes(&result.output);
}

#[tokio::test]
async fn finish_coding_task_summary_only_is_compact_for_clean_project() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    commit_file(tmp.path(), "README.md", "hello\n", "add readme");
    let runtime = test_runtime();
    let project =
        register_agent_project_at_path(&runtime, "coding-finish-compact", "demo", tmp.path()).await;
    let auth = auth_context(None, true);
    let session = runtime
        .sessions
        .start_session(Some(project.clone()), Some("compact finish".to_string()));
    let session_id = session.session_id.clone();

    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        let session_id = session_id.clone();
        let auth = auth.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::FinishCodingTask {
                        project,
                        session_id,
                        summary_only: true,
                        include_diff: Some(false),
                        include_workspace: None,
                        include_hygiene: Some(true),
                        include_handoff: Some(false),
                        include_validation_summary: Some(true),
                    },
                    Some(&auth),
                )
                .await
        }
    });

    for _ in 0..200 {
        if task.is_finished() {
            break;
        }
        if let Some(req) = next_patch_agent_request(&runtime, "coding-finish-compact").await {
            complete_agent_request_by_running_locally(&runtime, "coding-finish-compact", req).await;
        } else {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }
    assert!(
        task.is_finished(),
        "finish_coding_task summary_only did not finish after read-only agent requests"
    );
    let result = task.await.unwrap();

    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["summary_only"], true);
    assert_eq!(result.output["project"], project);
    assert_eq!(result.output["session_id"], session_id);
    assert_eq!(result.output["workspace_clean"], true);
    assert_eq!(result.output["hygiene_clean"], true);
    assert_eq!(result.output["jobs"]["active_count"], 0);
    assert_eq!(result.output["jobs"]["blocking_active_count"], 0);
    assert_eq!(result.output["permissions"]["total_approved_count"], 0);
    assert_eq!(result.output["tool_failures"]["expected_count"], 0);
    assert_eq!(result.output["tool_failures"]["unexpected_count"], 0);
    assert!(result.output["tool_failures"]
        .get("expectation_mismatch_count")
        .is_some());
    assert!(result.output["tool_failures"]
        .get("unexpected_success_count")
        .is_some());
    assert_eq!(result.output["validation"]["status"], "not_run");
    assert_eq!(
        result.output["validation"]["reason"],
        "no_validation_tool_invoked"
    );
    assert_eq!(result.output["review_evidence"]["available"], true);
    assert!(
        result.output["review_evidence"]["total"].as_u64().unwrap() > 0,
        "finish summary_only should count closeout review evidence: {}",
        result.output["review_evidence"]
    );
    assert!(
        result.output["review_evidence"]["workspace_review_count"]
            .as_u64()
            .unwrap()
            > 0
            || result.output["review_evidence"]["hygiene_review_count"]
                .as_u64()
                .unwrap()
                > 0,
        "finish summary_only should count workspace or hygiene review evidence: {}",
        result.output["review_evidence"]
    );
    assert_eq!(
        result.output["review_evidence"]["tools"]
            .as_array()
            .expect("review evidence tools array")
            .first()
            .and_then(Value::as_str),
        Some("show_changes")
    );
    assert_review_evidence_tools_safe(&result.output["review_evidence"]);
    assert!(result.output["warnings"].as_array().unwrap().is_empty());
    assert_finish_uses_canonical_outcomes(&result.output);
    assert!(result.output["suggested_next_actions"].is_array());
    let task_outcome = &result.output["task_outcome"];
    assert_task_outcome_shape(task_outcome);
    assert_eq!(task_outcome["status"], "warn");
    assert_eq!(task_outcome["blocking"], false);
    assert_reason_list_contains(
        task_outcome,
        "warning_reasons",
        "validation_not_run_with_review_evidence",
    );
    assert_reason_list_not_contains(task_outcome, "warning_reasons", "validation_not_run");
    assert!(result.output["suggested_next_actions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|action| action.as_str()
            == Some("decide whether task-appropriate validation is needed before closeout")));
    assert!(!result.output["suggested_next_actions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|action| action.as_str() == Some("run validation before closeout when available")));

    let serialized = serde_json::to_string(&result.output).unwrap();
    for forbidden in ["recent_events", "recent_failed_tools", "command"] {
        assert!(
            !serialized.contains(forbidden),
            "summary_only finish leaked {forbidden}: {serialized}"
        );
    }
    assert_no_raw_validation_output_fields(&result.output, "summary_only finish structured output");
    assert!(
        !serialized.contains("\"show_changes\":"),
        "summary_only finish leaked raw show_changes payload: {serialized}"
    );
}

#[tokio::test]
async fn finish_coding_task_summary_only_includes_review_evidence_for_docs_only_session() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    commit_file(tmp.path(), "docs.md", "hello\n", "add docs");
    let runtime = test_runtime();
    let project =
        register_agent_project_at_path(&runtime, "coding-finish-docs", "demo", tmp.path()).await;
    let auth = auth_context(None, true);
    let session = runtime
        .sessions
        .start_session(Some(project.clone()), Some("docs-only finish".to_string()));
    let session_id = session.session_id.clone();

    record_coding_task_tool_event(
        &runtime,
        &session_id,
        "replace_line_range",
        json!({
            "project": project,
            "path": "docs.md",
            "start_line": 1,
            "end_line": 1,
            "replacement": "updated docs"
        }),
        true,
        json!({}),
    );
    record_coding_task_tool_event(
        &runtime,
        &session_id,
        "search_project_text",
        json!({"project": project, "query": "docs"}),
        true,
        json!({}),
    );
    record_coding_task_tool_event(
        &runtime,
        &session_id,
        "show_changes",
        json!({"project": project, "include_diff": false}),
        true,
        json!({}),
    );

    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        let session_id = session_id.clone();
        let auth = auth.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::FinishCodingTask {
                        project,
                        session_id,
                        summary_only: true,
                        include_diff: Some(false),
                        include_workspace: None,
                        include_hygiene: Some(true),
                        include_handoff: Some(false),
                        include_validation_summary: Some(true),
                    },
                    Some(&auth),
                )
                .await
        }
    });
    for _ in 0..200 {
        if task.is_finished() {
            break;
        }
        if let Some(req) = next_patch_agent_request(&runtime, "coding-finish-docs").await {
            complete_agent_request_by_running_locally(&runtime, "coding-finish-docs", req).await;
        } else {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }
    assert!(
        task.is_finished(),
        "finish_coding_task summary_only did not finish after read-only agent requests"
    );
    let result = task.await.unwrap();

    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["summary_only"], true);
    assert_eq!(result.output["validation"]["status"], "not_run");
    assert_eq!(
        result.output["validation"]["reason"],
        "no_validation_tool_invoked"
    );
    assert_eq!(result.output["review_evidence"]["available"], true);
    assert!(
        result.output["review_evidence"]["total"].as_u64().unwrap() >= 2,
        "finish summary_only should preserve existing and closeout review evidence: {}",
        result.output["review_evidence"]
    );
    assert_eq!(result.output["review_evidence"]["search_count"], 1);
    assert!(
        result.output["review_evidence"]["workspace_review_count"]
            .as_u64()
            .unwrap()
            >= 2,
        "finish summary_only should include manual and closeout workspace review evidence: {}",
        result.output["review_evidence"]
    );
    let tools = result.output["review_evidence"]["tools"]
        .as_array()
        .expect("review evidence tools array");
    assert!(tools
        .iter()
        .any(|tool| tool.as_str() == Some("search_project_text")));
    assert!(tools
        .iter()
        .any(|tool| tool.as_str() == Some("show_changes")));
    assert_review_evidence_tools_safe(&result.output["review_evidence"]);
    let task_outcome = &result.output["task_outcome"];
    assert_task_outcome_shape(task_outcome);
    assert_eq!(task_outcome["status"], "warn");
    assert_reason_list_contains(
        task_outcome,
        "warning_reasons",
        "validation_not_run_with_review_evidence",
    );
    assert_reason_list_not_contains(task_outcome, "warning_reasons", "validation_not_run");
    assert_finish_uses_canonical_outcomes(&result.output);
}

#[tokio::test]
async fn finish_coding_task_summary_only_treats_dirty_workspace_as_advisory() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    commit_file(tmp.path(), "README.md", "hello\n", "add readme");
    fs::write(tmp.path().join("README.md"), "changed\n").unwrap();
    let runtime = test_runtime();
    let project =
        register_agent_project_at_path(&runtime, "coding-finish-dirty", "demo", tmp.path()).await;
    let auth = auth_context(None, true);
    let session = runtime
        .sessions
        .start_session(Some(project.clone()), Some("dirty finish".to_string()));
    let session_id = session.session_id.clone();

    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        let session_id = session_id.clone();
        let auth = auth.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::FinishCodingTask {
                        project,
                        session_id,
                        summary_only: true,
                        include_diff: Some(false),
                        include_workspace: None,
                        include_hygiene: Some(false),
                        include_handoff: Some(false),
                        include_validation_summary: Some(true),
                    },
                    Some(&auth),
                )
                .await
        }
    });
    let req = next_patch_agent_request(&runtime, "coding-finish-dirty")
        .await
        .expect("finish_coding_task should inspect changes");
    complete_agent_request_by_running_locally(&runtime, "coding-finish-dirty", req).await;
    let result = task.await.unwrap();

    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["workspace_clean"], false);
    assert_eq!(result.output["task_outcome"]["status"], "warn");
    assert_eq!(result.output["task_outcome"]["blocking"], false);
    assert_task_outcome_shape(&result.output["task_outcome"]);
    assert_reason_list_contains(
        &result.output["task_outcome"],
        "warning_reasons",
        "workspace_dirty",
    );
    assert_finish_uses_canonical_outcomes(&result.output);
}

#[tokio::test]
async fn finish_coding_task_does_not_resolve_a_different_validation_identity() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    commit_file(tmp.path(), "README.md", "hello\n", "add readme");
    let runtime = test_runtime();
    let project =
        register_agent_project_at_path(&runtime, "coding-finish-resolved", "demo", tmp.path())
            .await;
    let auth = auth_context(None, true);
    let session = runtime.sessions.start_session(
        Some(project.clone()),
        Some("resolved validation finish".to_string()),
    );
    let session_id = session.session_id.clone();

    record_coding_task_tool_event(
        &runtime,
        &session_id,
        "cargo_test",
        json!({
            "project": project,
            "expected_failure": true,
            "expected_failure_kind": "validation_failed",
            "assertion_name": "pre-fix validation should fail"
        }),
        false,
        json!({
            "exit_code": 101,
            "failure_kind": "validation_failed"
        }),
    );
    record_coding_task_tool_event(
        &runtime,
        &session_id,
        "cargo_check",
        json!({"project": project}),
        true,
        json!({"exit_code": 0}),
    );

    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        let session_id = session_id.clone();
        let auth = auth.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::FinishCodingTask {
                        project,
                        session_id,
                        summary_only: true,
                        include_diff: Some(false),
                        include_workspace: None,
                        include_hygiene: Some(true),
                        include_handoff: Some(false),
                        include_validation_summary: Some(true),
                    },
                    Some(&auth),
                )
                .await
        }
    });
    for _ in 0..200 {
        if task.is_finished() {
            break;
        }
        if let Some(req) = next_patch_agent_request(&runtime, "coding-finish-resolved").await {
            complete_agent_request_by_running_locally(&runtime, "coding-finish-resolved", req)
                .await;
        } else {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }
    assert!(
        task.is_finished(),
        "finish_coding_task summary_only did not finish after read-only agent requests"
    );
    let result = task.await.unwrap();

    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["workspace_clean"], true);
    assert_eq!(result.output["hygiene_clean"], true);
    assert_eq!(result.output["tool_failures"]["unexpected_count"], 0);
    assert_eq!(result.output["validation"]["status"], "mixed");
    assert_eq!(result.output["validation"]["latest_status"], "passed");
    assert_eq!(
        result.output["validation"]["historical_failures"]["count"],
        1
    );
    assert_eq!(
        result.output["validation"]["historical_failures"]["resolved"],
        false
    );
    assert_eq!(
        result.output["validation"]["historical_failures"]["unresolved"],
        true
    );
    assert_eq!(result.output["task_outcome"]["status"], "fail");
    assert_eq!(result.output["task_outcome"]["blocking"], true);
    assert_task_outcome_shape(&result.output["task_outcome"]);
    assert_eq!(
        result.output["evidence_history"]["status"],
        "mixed_unresolved"
    );
    assert_eq!(result.output["evidence_integrity"]["status"], "clean");
    assert_reason_list_contains(
        &result.output["task_outcome"],
        "blocking_reasons",
        "validation_mixed",
    );
    assert_finish_uses_canonical_outcomes(&result.output);
}

#[tokio::test]
async fn finish_coding_task_summary_only_passes_with_resolved_unexpected_cargo_fmt_failure() {
    let fixture = finish_summary_fixture("coding-finish-resolved-fmt").await;

    record_coding_task_tool_event(
        &fixture.runtime,
        &fixture.session_id,
        "cargo_fmt",
        json!({"project": fixture.project.clone(), "check": true}),
        false,
        json!({
            "exit_code": 1,
            "failure_kind": "validation_failed"
        }),
    );
    record_coding_task_tool_event(
        &fixture.runtime,
        &fixture.session_id,
        "cargo_fmt",
        json!({"project": fixture.project.clone(), "check": true}),
        true,
        json!({"exit_code": 0}),
    );

    let result = finish_coding_task_summary_only_with_agent(
        &fixture.runtime,
        fixture.client_id,
        fixture.project.clone(),
        fixture.session_id.clone(),
        fixture.auth.clone(),
    )
    .await;
    let full = finish_coding_task_with_agent(
        &fixture.runtime,
        fixture.client_id,
        fixture.project,
        fixture.session_id,
        fixture.auth,
        false,
    )
    .await;

    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["workspace_clean"], true);
    assert_eq!(result.output["hygiene_clean"], true);
    assert_eq!(result.output["tool_failures"]["unexpected_count"], 1);
    assert_eq!(result.output["validation"]["status"], "mixed");
    assert_eq!(result.output["validation"]["latest_status"], "passed");
    assert_eq!(
        result.output["validation"]["historical_failures"]["resolved"],
        true
    );
    assert_eq!(
        result.output["validation"]["historical_failures"]["unresolved"],
        false
    );
    assert_eq!(result.output["task_outcome"]["status"], "warn");
    assert_eq!(result.output["task_outcome"]["blocking"], false);
    assert!(result.output["advisories"]
        .as_array()
        .unwrap()
        .iter()
        .any(|reason| reason == "historical_validation_failures_resolved"));
    assert_eq!(
        result.output["evidence_history"]["status"],
        "mixed_resolved"
    );
    assert_eq!(result.output["evidence_integrity"]["status"], "clean");
    assert_reason_list_not_contains(
        &result.output["task_outcome"],
        "blocking_reasons",
        "unexpected_tool_failures",
    );
    assert_action_list_not_contains(
        &result.output["suggested_next_actions"],
        "review unexpected failed tool calls before proceeding",
    );
    assert_eq!(full.output["task_outcome"], result.output["task_outcome"]);
    assert_eq!(
        full.output["evidence_history"],
        result.output["evidence_history"]
    );
    assert_eq!(
        full.output["evidence_integrity"],
        result.output["evidence_integrity"]
    );
    assert_eq!(
        full.output["suggested_next_actions"],
        result.output["suggested_next_actions"]
    );
    assert_finish_uses_canonical_outcomes(&result.output);
    assert_finish_uses_canonical_outcomes(&full.output);
}

#[tokio::test]
async fn finish_coding_task_summary_only_passes_with_resolved_unexpected_cargo_check_failure() {
    let fixture = finish_summary_fixture("coding-finish-resolved-check").await;

    record_coding_task_tool_event(
        &fixture.runtime,
        &fixture.session_id,
        "cargo_check",
        json!({"project": fixture.project.clone()}),
        false,
        json!({
            "exit_code": 101,
            "failure_kind": "validation_failed"
        }),
    );
    record_coding_task_tool_event(
        &fixture.runtime,
        &fixture.session_id,
        "cargo_check",
        json!({"project": fixture.project.clone()}),
        true,
        json!({"exit_code": 0}),
    );

    let result = finish_coding_task_summary_only_with_agent(
        &fixture.runtime,
        fixture.client_id,
        fixture.project,
        fixture.session_id,
        fixture.auth,
    )
    .await;

    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["tool_failures"]["unexpected_count"], 1);
    assert_eq!(result.output["validation"]["latest_status"], "passed");
    assert_eq!(
        result.output["validation"]["historical_failures"]["resolved"],
        true
    );
    assert_eq!(result.output["task_outcome"]["status"], "warn");
    assert_eq!(
        result.output["evidence_history"]["status"],
        "mixed_resolved"
    );
    assert_eq!(result.output["evidence_integrity"]["status"], "clean");
    assert_eq!(result.output["task_outcome"]["blocking"], false);
    assert_reason_list_not_contains(
        &result.output["task_outcome"],
        "blocking_reasons",
        "unexpected_tool_failures",
    );
}

#[tokio::test]
async fn finish_coding_task_summary_only_keeps_cargo_fmt_failure_blocking_when_only_cargo_test_passes(
) {
    let fixture = finish_summary_fixture("coding-finish-cross-tool-validation").await;

    record_coding_task_tool_event(
        &fixture.runtime,
        &fixture.session_id,
        "cargo_fmt",
        json!({"project": fixture.project.clone(), "check": true}),
        false,
        json!({
            "exit_code": 1,
            "failure_kind": "validation_failed"
        }),
    );
    record_coding_task_tool_event(
        &fixture.runtime,
        &fixture.session_id,
        "cargo_test",
        json!({"project": fixture.project.clone()}),
        true,
        json!({"exit_code": 0}),
    );

    let result = finish_coding_task_summary_only_with_agent(
        &fixture.runtime,
        fixture.client_id,
        fixture.project,
        fixture.session_id,
        fixture.auth,
    )
    .await;

    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["tool_failures"]["unexpected_count"], 1);
    assert_eq!(result.output["validation"]["latest_status"], "passed");
    assert_eq!(
        result.output["validation"]["historical_failures"]["resolved"],
        false
    );
    assert_eq!(result.output["task_outcome"]["status"], "fail");
    assert_eq!(result.output["task_outcome"]["blocking"], true);
    assert_reason_list_contains(
        &result.output["task_outcome"],
        "blocking_reasons",
        "unexpected_tool_failures",
    );
}

#[tokio::test]
async fn finish_coding_task_summary_only_warns_for_cargo_test_zero_tests_success() {
    let fixture = finish_summary_fixture("coding-finish-zero-tests").await;

    record_coding_task_tool_event(
        &fixture.runtime,
        &fixture.session_id,
        "cargo_test",
        json!({
            "project": fixture.project.clone(),
            "expected_failure": true,
            "expected_failure_kind": "validation_failed",
            "assertion_name": "negative assertion accidentally ran zero tests"
        }),
        true,
        json!({
            "exit_code": 0,
            "stdout_tail": "running 0 tests\n\n\
                test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n",
            "stderr_tail": "",
            "stdout_truncated": false,
            "stderr_truncated": false,
            "tests_detected": true,
            "tests_run_count": 0,
            "zero_tests_run": true
        }),
    );

    let result = finish_coding_task_summary_only_with_agent(
        &fixture.runtime,
        fixture.client_id,
        fixture.project,
        fixture.session_id,
        fixture.auth,
    )
    .await;

    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["workspace_clean"], true);
    assert_eq!(result.output["hygiene_clean"], true);
    assert_eq!(
        result.output["tool_failures"]["unexpected_success_count"],
        1
    );
    assert_eq!(
        result.output["tool_failures"]["expectation_mismatch_count"],
        0
    );
    assert_eq!(result.output["validation"]["status"], "passed");
    assert_eq!(
        result.output["validation"]["cargo_test_zero_tests_run"],
        true
    );
    assert_eq!(result.output["task_outcome"]["status"], "pass");
    assert_eq!(result.output["task_outcome"]["blocking"], false);
    assert_eq!(result.output["evidence_history"]["status"], "clean");
    assert_eq!(result.output["evidence_integrity"]["status"], "warning");
    assert_reason_list_contains(
        &result.output["evidence_integrity"],
        "warning_reasons",
        "unexpected_successes",
    );
    assert_reason_list_contains(
        &result.output["evidence_integrity"],
        "warning_reasons",
        "cargo_test_zero_tests",
    );
    assert!(result.output["suggested_next_actions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|action| action.as_str()
            == Some("cargo_test ran zero tests; verify the test filter or command")));
    assert_finish_uses_canonical_outcomes(&result.output);
}

#[tokio::test]
async fn finish_coding_task_summary_only_keeps_cargo_test_failure_blocking_after_zero_tests_success(
) {
    let fixture = finish_summary_fixture("coding-finish-zero-tests-does-not-resolve").await;

    record_coding_task_tool_event(
        &fixture.runtime,
        &fixture.session_id,
        "cargo_test",
        json!({"project": fixture.project.clone()}),
        false,
        json!({
            "exit_code": 101,
            "failure_kind": "validation_failed"
        }),
    );
    record_coding_task_tool_event(
        &fixture.runtime,
        &fixture.session_id,
        "cargo_test",
        json!({"project": fixture.project.clone()}),
        true,
        json!({
            "exit_code": 0,
            "stdout_tail": "running 0 tests\n\n\
                test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n",
            "stderr_tail": "",
            "stdout_truncated": false,
            "stderr_truncated": false,
            "tests_detected": true,
            "tests_run_count": 0,
            "zero_tests_run": true
        }),
    );

    let result = finish_coding_task_summary_only_with_agent(
        &fixture.runtime,
        fixture.client_id,
        fixture.project,
        fixture.session_id,
        fixture.auth,
    )
    .await;

    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["tool_failures"]["unexpected_count"], 1);
    assert_eq!(result.output["validation"]["status"], "mixed");
    assert_eq!(result.output["validation"]["latest_status"], "passed");
    assert_eq!(
        result.output["validation"]["cargo_test_zero_tests_run"],
        true
    );
    assert_eq!(
        result.output["validation"]["historical_failures"]["resolved"],
        false
    );
    assert_eq!(
        result.output["validation"]["historical_failures"]["unresolved"],
        true
    );
    assert_eq!(result.output["task_outcome"]["status"], "fail");
    assert_eq!(
        result.output["evidence_history"]["status"],
        "mixed_unresolved"
    );
    assert_eq!(result.output["evidence_integrity"]["status"], "warning");
    assert_eq!(result.output["task_outcome"]["blocking"], true);
    assert_reason_list_contains(
        &result.output["task_outcome"],
        "blocking_reasons",
        "unexpected_tool_failures",
    );
    assert_action_list_contains(
        &result.output["suggested_next_actions"],
        "review unexpected failed tool calls before proceeding",
    );
    assert_action_list_contains(
        &result.output["suggested_next_actions"],
        "cargo_test ran zero tests; verify the test filter or command",
    );
    assert_reason_list_contains(
        &result.output["evidence_integrity"],
        "warning_reasons",
        "cargo_test_zero_tests",
    );
    assert!(!result.output["informational_notes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|note| note.as_str()
            == Some(
                "historical validation failures were resolved by later successful validation"
            )));
}

#[tokio::test]
async fn finish_coding_task_summary_only_blocks_unresolved_cargo_fmt_failure() {
    let fixture = finish_summary_fixture("coding-finish-unresolved-fmt").await;

    record_coding_task_tool_event(
        &fixture.runtime,
        &fixture.session_id,
        "cargo_fmt",
        json!({"project": fixture.project.clone(), "check": true}),
        false,
        json!({
            "exit_code": 1,
            "failure_kind": "validation_failed"
        }),
    );

    let result = finish_coding_task_summary_only_with_agent(
        &fixture.runtime,
        fixture.client_id,
        fixture.project,
        fixture.session_id,
        fixture.auth,
    )
    .await;

    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["workspace_clean"], true);
    assert_eq!(result.output["hygiene_clean"], true);
    assert_eq!(result.output["tool_failures"]["unexpected_count"], 1);
    assert_eq!(result.output["validation"]["status"], "failed");
    assert_eq!(result.output["validation"]["latest_status"], "failed");
    assert_eq!(
        result.output["validation"]["historical_failures"]["unresolved"],
        true
    );
    assert_eq!(result.output["task_outcome"]["status"], "fail");
    assert_eq!(result.output["task_outcome"]["blocking"], true);
    assert_reason_list_contains(
        &result.output["task_outcome"],
        "blocking_reasons",
        "unexpected_tool_failures",
    );
    assert_action_list_contains(
        &result.output["suggested_next_actions"],
        "review unexpected failed tool calls before proceeding",
    );
}

#[tokio::test]
async fn finish_coding_task_summary_only_keeps_non_validation_tool_failures_blocking() {
    let fixture = finish_summary_fixture("coding-finish-read-failure").await;

    record_coding_task_tool_event(
        &fixture.runtime,
        &fixture.session_id,
        "read_file",
        json!({"project": fixture.project.clone(), "path": "README.md"}),
        false,
        json!({
            "error_kind": "permission_denied"
        }),
    );
    record_coding_task_tool_event(
        &fixture.runtime,
        &fixture.session_id,
        "cargo_test",
        json!({"project": fixture.project.clone()}),
        true,
        json!({"exit_code": 0}),
    );

    let result = finish_coding_task_summary_only_with_agent(
        &fixture.runtime,
        fixture.client_id,
        fixture.project,
        fixture.session_id,
        fixture.auth,
    )
    .await;

    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["workspace_clean"], true);
    assert_eq!(result.output["hygiene_clean"], true);
    assert_eq!(result.output["tool_failures"]["unexpected_count"], 1);
    assert_eq!(result.output["validation"]["status"], "passed");
    assert_eq!(result.output["validation"]["latest_status"], "passed");
    assert_eq!(result.output["task_outcome"]["status"], "fail");
    assert_eq!(result.output["task_outcome"]["blocking"], true);
    assert_reason_list_contains(
        &result.output["task_outcome"],
        "blocking_reasons",
        "unexpected_tool_failures",
    );
}

#[tokio::test]
async fn finish_coding_task_includes_active_jobs_warning_without_logs() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    commit_file(tmp.path(), "README.md", "hello\n", "add readme");
    let runtime = test_runtime();
    let mut caps = ShellClientCapabilities::default();
    caps.shell = true;
    caps.git = true;
    caps.async_shell_jobs = true;
    let project_path = tmp.path().to_string_lossy().to_string();
    let auth = open_auth_context();
    register_agent_projects_for_auth(
        &runtime,
        "coding-finish-jobs",
        &auth,
        caps,
        vec![registered_project("demo", &project_path)],
    )
    .await;
    let project = "agent:coding-finish-jobs:demo".to_string();
    let start = runtime
        .dispatch_with_auth(
            ToolCall::StartCodingTask {
                project: project.clone(),
                title: Some("finish active jobs".to_string()),
                mode: SessionMode::Normal,
                detail: Default::default(),
                deny_write_tools: false,
                deny_shell_tools: false,
                bind_current: false,
            },
            Some(&auth),
        )
        .await;
    assert!(start.success, "{:?}", start.error);
    let session_id = start.output["session"]["session_id"]
        .as_str()
        .unwrap()
        .to_string();

    let run = runtime
        .dispatch_with_auth(
            ToolCall::RunJob {
                project: project.clone(),
                command: "printf secret-job-output".to_string(),
                session_id: Some(session_id.clone()),
                timeout_secs: None,
                cwd: None,
                purpose: None,
                shell: None,
            },
            Some(&auth),
        )
        .await;
    assert!(run.success, "{:?}", run.error);
    let queued_job = next_agent_request_for_client(&runtime, "coding-finish-jobs")
        .await
        .expect("run_job should enqueue a job request");
    assert_eq!(queued_job.kind, "start_job");

    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        let session_id = session_id.clone();
        let auth = auth.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::FinishCodingTask {
                        project,
                        session_id,
                        summary_only: false,
                        include_diff: Some(false),
                        include_workspace: None,
                        include_hygiene: Some(false),
                        include_handoff: Some(false),
                        include_validation_summary: Some(false),
                    },
                    Some(&auth),
                )
                .await
        }
    });
    let req = next_agent_request_for_client(&runtime, "coding-finish-jobs")
        .await
        .expect("finish_coding_task should inspect changes through the agent");
    assert!(req.command.contains("git status --porcelain=v1 -b"));
    let show_changes_stdout = "## main\n@@WEBCODEX_SHOW_CHANGES_SEP@@\nabc123\0abc123\0add readme\n@@WEBCODEX_SHOW_CHANGES_SEP@@\n";
    complete_patch_agent_request_for_instance(
        &runtime,
        "coding-finish-jobs",
        "inst-coding-finish-jobs",
        &req.request_id,
        0,
        show_changes_stdout,
        "",
    )
    .await;
    let result = task.await.unwrap();

    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["jobs"]["active_count"], 1);
    assert_eq!(result.output["jobs"]["running_count"], 1);
    assert_eq!(result.output["jobs"]["stop_requested_count"], 0);
    assert_eq!(result.output["jobs"]["terminal_pending_count"], 0);
    assert_eq!(result.output["jobs"]["blocking_active_count"], 1);
    assert_eq!(result.output["jobs"]["nonblocking_active_count"], 0);
    assert_eq!(result.output["task_outcome"]["status"], "fail");
    assert_eq!(result.output["task_outcome"]["blocking"], true);
    assert_eq!(
        result.output["jobs"]["recent"][0]["job_id"],
        run.output["job_id"]
    );
    assert!(result.output["final_warnings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|warning| warning["kind"] == "active_jobs_present" && warning["blocking"] == true));
    assert_no_raw_validation_output_fields(&result.output["jobs"], "finish jobs summary");
    let serialized = serde_json::to_string(&result.output["jobs"]).unwrap();
    assert!(!serialized.contains("secret-job-output"));
}

#[tokio::test]
async fn finish_coding_task_treats_stop_requested_jobs_as_nonblocking() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    commit_file(tmp.path(), "README.md", "hello\n", "add readme");
    let runtime = test_runtime();
    let mut caps = ShellClientCapabilities::default();
    caps.shell = true;
    caps.git = true;
    caps.async_shell_jobs = true;
    let project_path = tmp.path().to_string_lossy().to_string();
    let auth = open_auth_context();
    register_agent_projects_for_auth(
        &runtime,
        "coding-finish-stop-pending",
        &auth,
        caps,
        vec![registered_project("demo", &project_path)],
    )
    .await;
    let project = "agent:coding-finish-stop-pending:demo".to_string();
    let start = runtime
        .dispatch_with_auth(
            ToolCall::StartCodingTask {
                project: project.clone(),
                title: Some("finish stop pending".to_string()),
                mode: SessionMode::Normal,
                detail: Default::default(),
                deny_write_tools: false,
                deny_shell_tools: false,
                bind_current: false,
            },
            Some(&auth),
        )
        .await;
    assert!(start.success, "{:?}", start.error);
    let session_id = start.output["session"]["session_id"]
        .as_str()
        .unwrap()
        .to_string();

    let run = runtime
        .dispatch_with_auth(
            ToolCall::RunJob {
                project: project.clone(),
                command: "printf stop-pending-secret-output".to_string(),
                session_id: Some(session_id.clone()),
                timeout_secs: None,
                cwd: None,
                purpose: None,
                shell: None,
            },
            Some(&auth),
        )
        .await;
    assert!(run.success, "{:?}", run.error);
    let job_id = run.output["job_id"].as_str().unwrap().to_string();
    let start_job = next_agent_request_for_client(&runtime, "coding-finish-stop-pending")
        .await
        .expect("run_job should enqueue a job request");
    assert_eq!(start_job.kind, "start_job");

    let stop = runtime
        .dispatch_with_auth(
            ToolCall::StopJob {
                project: project.clone(),
                job_id: job_id.clone(),
                session_id: Some(session_id.clone()),
                confirm: true,
            },
            Some(&auth),
        )
        .await;
    assert!(stop.success, "{:?}", stop.error);
    assert_eq!(stop.output["status_after"], "stop_requested");
    let stop_req = next_agent_request_for_client(&runtime, "coding-finish-stop-pending")
        .await
        .expect("stop_job should enqueue a stop request");
    assert_eq!(stop_req.kind, "stop_job");

    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        let session_id = session_id.clone();
        let auth = auth.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::FinishCodingTask {
                        project,
                        session_id,
                        summary_only: false,
                        include_diff: Some(false),
                        include_workspace: None,
                        include_hygiene: Some(false),
                        include_handoff: Some(false),
                        include_validation_summary: Some(false),
                    },
                    Some(&auth),
                )
                .await
        }
    });
    let req = next_agent_request_for_client(&runtime, "coding-finish-stop-pending")
        .await
        .expect("finish_coding_task should inspect changes through the agent");
    assert!(req.command.contains("git status --porcelain=v1 -b"));
    let show_changes_stdout = "## main\n@@WEBCODEX_SHOW_CHANGES_SEP@@\nabc123\0abc123\0add readme\n@@WEBCODEX_SHOW_CHANGES_SEP@@\n";
    complete_patch_agent_request_for_instance(
        &runtime,
        "coding-finish-stop-pending",
        "inst-coding-finish-stop-pending",
        &req.request_id,
        0,
        show_changes_stdout,
        "",
    )
    .await;
    let result = task.await.unwrap();

    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["jobs"]["active_count"], 1);
    assert_eq!(result.output["jobs"]["running_count"], 0);
    assert_eq!(result.output["jobs"]["stop_requested_count"], 1);
    assert_eq!(result.output["jobs"]["terminal_pending_count"], 1);
    assert_eq!(result.output["jobs"]["blocking_active_count"], 0);
    assert_eq!(result.output["jobs"]["nonblocking_active_count"], 1);
    assert_eq!(result.output["jobs"]["recent"][0]["job_id"], job_id);
    let final_warnings = result.output["final_warnings"].as_array().unwrap();
    assert!(final_warnings
        .iter()
        .all(|warning| warning["kind"] != "active_jobs_present"));
    assert!(final_warnings.iter().any(|warning| {
        warning["kind"] == "jobs_terminal_pending" && warning["blocking"] == false
    }));
    assert_no_raw_validation_output_fields(&result.output["jobs"], "finish jobs summary");
    let serialized = serde_json::to_string(&result.output["jobs"]).unwrap();
    assert!(!serialized.contains("stop-pending-secret-output"));
}

fn contains_string(values: &[Value], needle: &str) -> bool {
    values.iter().any(|value| value.as_str() == Some(needle))
}

fn assert_check_status(verdict: &Value, name: &str, status: &str) {
    let check = verdict["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["name"] == name)
        .unwrap_or_else(|| panic!("missing startup check {name}: {verdict}"));
    assert_eq!(check["status"], status);
}

fn assert_check_reason(verdict: &Value, name: &str, reason: &str) {
    let check = verdict["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["name"] == name)
        .unwrap_or_else(|| panic!("missing startup check {name}: {verdict}"));
    assert_eq!(check["reason"], reason);
}

fn assert_reason_list_contains(verdict: &Value, key: &str, reason: &str) {
    let reasons = verdict[key].as_array().expect("reason list");
    assert!(
        reasons.iter().any(|value| value.as_str() == Some(reason)),
        "{key} should contain {reason}: {verdict}"
    );
}

fn assert_reason_list_not_contains(verdict: &Value, key: &str, reason: &str) {
    let reasons = verdict[key].as_array().expect("reason list");
    assert!(
        !reasons.iter().any(|value| value.as_str() == Some(reason)),
        "{key} should not contain {reason}: {verdict}"
    );
}

fn assert_startup_verdict_shape(verdict: &Value) {
    assert_status_string(verdict);
    assert!(verdict["blocking"].is_boolean(), "blocking bool: {verdict}");
    let checks = verdict["checks"].as_array().expect("startup checks array");
    assert!(!checks.is_empty(), "startup checks should not be empty");
    for check in checks {
        assert!(
            check["name"].is_string(),
            "startup check name should be present: {check}"
        );
        assert_status_string(check);
        if let Some(reason) = check.get("reason") {
            assert!(reason.is_string(), "reason must be a string: {check}");
        }
    }
    assert!(
        verdict["suggested_next_actions"].is_array(),
        "suggested_next_actions array: {verdict}"
    );
}

fn assert_task_outcome_shape(task_outcome: &Value) {
    assert_status_string(task_outcome);
    assert!(
        task_outcome["blocking"].is_boolean(),
        "blocking bool: {task_outcome}"
    );
    for key in ["blocking_reasons", "warning_reasons"] {
        assert!(task_outcome[key].is_array(), "{key} array: {task_outcome}");
    }
}

fn assert_status_string(value: &Value) {
    let status = value["status"].as_str().expect("status string");
    assert!(
        matches!(status, "pass" | "warn" | "fail"),
        "unexpected verdict status {status}: {value}"
    );
}

fn assert_finish_uses_canonical_outcomes(output: &Value) {
    assert!(output["task_outcome"].is_object(), "{output}");
    assert!(output["evidence_history"].is_object(), "{output}");
    assert!(output["evidence_integrity"].is_object(), "{output}");
    assert!(output.get("verdict").is_none(), "{output}");
    assert!(output.get("finish_verdict").is_none(), "{output}");
}

fn assert_action_list_contains(actions: &Value, action: &str) {
    assert!(
        actions
            .as_array()
            .expect("suggested_next_actions array")
            .iter()
            .any(|candidate| candidate.as_str() == Some(action)),
        "suggested_next_actions should contain {action}: {actions}"
    );
}

fn assert_action_list_not_contains(actions: &Value, action: &str) {
    assert!(
        !actions
            .as_array()
            .expect("suggested_next_actions array")
            .iter()
            .any(|candidate| candidate.as_str() == Some(action)),
        "suggested_next_actions should not contain {action}: {actions}"
    );
}

fn assert_compact_verdict_safe(value: &Value, context: &str) {
    let serialized = serde_json::to_string(value).unwrap();
    for forbidden in [
        "stdout", "stderr", "tail", "excerpt", "command", "token", "secret", "env",
    ] {
        assert!(
            !serialized.contains(forbidden),
            "{context} leaked {forbidden}: {serialized}"
        );
    }
}

fn assert_review_evidence_tools_safe(review_evidence: &Value) {
    let tools = review_evidence["tools"]
        .as_array()
        .expect("review_evidence.tools array");
    assert!(
        !tools.is_empty(),
        "review_evidence.tools should not be empty"
    );
    assert!(tools.len() <= 20, "review_evidence.tools should be bounded");
    for tool in tools {
        let tool = tool.as_str().expect("review evidence tool name");
        assert!(
            matches!(
                tool,
                "read_file"
                    | "list_project_files"
                    | "search_project_text"
                    | "git_diff"
                    | "git_diff_summary"
                    | "git_diff_hunks"
                    | "show_changes"
                    | "git_status"
                    | "workspace_hygiene_check"
            ),
            "unexpected review evidence tool name {tool}"
        );
        for forbidden in [
            "stdout", "stderr", "tail", "excerpt", "command", "token", "secret", "env",
        ] {
            assert!(
                !tool.contains(forbidden),
                "review_evidence.tools leaked {forbidden}: {review_evidence}"
            );
        }
    }
}

fn json_contains_key(value: &Value, key: &str) -> bool {
    match value {
        Value::Object(map) => {
            map.contains_key(key) || map.values().any(|value| json_contains_key(value, key))
        }
        Value::Array(values) => values.iter().any(|value| json_contains_key(value, key)),
        _ => false,
    }
}

fn assert_no_raw_validation_output_fields(value: &Value, context: &str) {
    for key in [
        "stdout",
        "stderr",
        "stdout_tail",
        "stderr_tail",
        "stdout_tail_excerpt",
        "stderr_tail_excerpt",
        "validation_output_summary",
    ] {
        assert!(
            !json_contains_key(value, key),
            "{context} must not include {key}: {value}"
        );
    }
}

fn record_coding_task_tool_event(
    runtime: &ToolRuntime,
    session_id: &str,
    tool_name: &str,
    arguments: Value,
    success: bool,
    output: Value,
) {
    let start = runtime.sessions.record_tool_call_started(
        Some(session_id),
        SessionTransport::Api,
        tool_name,
        &arguments,
    );
    let error = (!success).then_some("tool failed");
    runtime
        .sessions
        .record_tool_call_finished(start, success, &output, error, None);
}

struct FinishSummaryFixture {
    _tmp: tempfile::TempDir,
    runtime: ToolRuntime,
    project: String,
    session_id: String,
    auth: AuthContext,
    client_id: &'static str,
}

async fn finish_summary_fixture(client_id: &'static str) -> FinishSummaryFixture {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    commit_file(tmp.path(), "README.md", "hello\n", "add readme");
    let runtime = test_runtime();
    let project = register_agent_project_at_path(&runtime, client_id, "demo", tmp.path()).await;
    let session = runtime
        .sessions
        .start_session(Some(project.clone()), Some(client_id.to_string()));
    FinishSummaryFixture {
        _tmp: tmp,
        runtime,
        project,
        session_id: session.session_id,
        auth: auth_context(None, true),
        client_id,
    }
}

async fn finish_coding_task_summary_only_with_agent(
    runtime: &ToolRuntime,
    client_id: &str,
    project: String,
    session_id: String,
    auth: AuthContext,
) -> ToolResult {
    finish_coding_task_with_agent(runtime, client_id, project, session_id, auth, true).await
}

async fn finish_coding_task_with_agent(
    runtime: &ToolRuntime,
    client_id: &str,
    project: String,
    session_id: String,
    auth: AuthContext,
    summary_only: bool,
) -> ToolResult {
    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        let session_id = session_id.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::FinishCodingTask {
                        project,
                        session_id,
                        summary_only,
                        include_diff: Some(false),
                        include_workspace: None,
                        include_hygiene: Some(true),
                        include_handoff: Some(false),
                        include_validation_summary: Some(true),
                    },
                    Some(&auth),
                )
                .await
        }
    });
    for _ in 0..200 {
        if task.is_finished() {
            break;
        }
        if let Some(req) = next_patch_agent_request(runtime, client_id).await {
            complete_agent_request_by_running_locally(runtime, client_id, req).await;
        } else {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }
    assert!(
        task.is_finished(),
        "finish_coding_task summary_only did not finish after read-only agent requests"
    );
    task.await.unwrap()
}

#[tokio::test]
async fn start_coding_task_top_level_recommended_flow_projects_to_visible_manifest_tools() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    let runtime = test_runtime();
    let project =
        register_agent_project_at_path(&runtime, "coding-flow-proj", "demo", tmp.path()).await;
    let auth = auth_context(None, true);

    let result = start_coding_task_serviced(
        &runtime,
        "coding-flow-proj",
        json!({ "project": project, "detail": "full" }),
        &auth,
    )
    .await;
    assert!(result.success, "{:?}", result.error);

    let manifest_tools: std::collections::BTreeSet<&str> = result.output["tool_manifest"]["tools"]
        .as_array()
        .expect("tool_manifest.tools")
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    assert!(
        manifest_tools.contains("finish_coding_task"),
        "coding startup should keep finish_coding_task visible"
    );

    let flow = &result.output["recommended_flow"];
    for group in ["inspect", "edit", "validate", "review", "handoff"] {
        assert!(
            flow.get(group).and_then(Value::as_array).is_some(),
            "recommended_flow must keep group key {group}"
        );
    }

    for group in ["inspect", "edit", "validate", "review", "handoff"] {
        for tool in flow[group].as_array().unwrap() {
            let tool = tool.as_str().unwrap();
            assert!(
                manifest_tools.contains(tool),
                "recommended_flow.{group} references invisible tool {tool}; visible={manifest_tools:?}"
            );
        }
    }

    let handoff = flow["handoff"].as_array().unwrap();
    assert!(
        handoff.iter().any(|tool| tool == "finish_coding_task"),
        "handoff should retain finish_coding_task when visible: {handoff:?}"
    );
}

#[tokio::test]
async fn start_coding_task_standard_omits_repeated_manifest_and_recommended_flow() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    let runtime = test_runtime();
    let project =
        register_agent_project_at_path(&runtime, "coding-flow-full", "demo", tmp.path()).await;
    let auth = auth_context(None, true);

    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        let auth = auth.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::from_tool_name(
                        "start_coding_task",
                        json!({
                            "project": project,
                            "detail": "standard"
                        }),
                    )
                    .unwrap(),
                    Some(&auth),
                )
                .await
        }
    });
    let request = next_patch_agent_request(&runtime, "coding-flow-full")
        .await
        .expect("standard startup should inspect workspace state");
    complete_agent_request_by_running_locally(&runtime, "coding-flow-full", request).await;
    let result = task.await.unwrap();
    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["detail"], "standard");
    assert!(result.output.get("tool_manifest").is_none());
    assert!(result.output.get("recommended_flow").is_none());
    assert!(result.output.get("rules").is_none());
}
