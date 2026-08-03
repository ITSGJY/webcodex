//! Focused tests for the `work_on_project` thin coding-task entry point.
//!
//! `work_on_project` is a model-facing wrapper over `start_coding_task`: it
//! validates one of two project sources plus the task inputs, maps them onto
//! normal coding-task defaults, delegates the business implementation, and
//! projects a compact startup result. It never binds a current window, never
//! guesses a recent Session, and never falls back to a credential-wide Session.

use super::reconnect::dispatch_start_coding_task_in_window;
use super::support::*;
use crate::shell_protocol::ShellClientCapabilities;
use crate::tool_runtime::permissions::{AuthorityMode, PermissionEvaluator};
use crate::tool_runtime::sessions::{SessionEvent, SessionGuards};
use crate::tool_runtime::{registered_tool_specs, SessionMode, ToolCall, ToolResult, ToolRuntime};
use serde_json::{json, Value};

fn work_on_project_call(project: &str, instruction: &str, session_id: Option<&str>) -> ToolCall {
    ToolCall::WorkOnProject {
        project: project.to_string(),
        client_id: None,
        path: None,
        instruction: instruction.to_string(),
        session_id: session_id.map(str::to_string),
    }
}

fn path_work_on_project_call(
    client_id: &str,
    path: &str,
    instruction: &str,
    session_id: Option<&str>,
) -> ToolCall {
    ToolCall::WorkOnProject {
        project: String::new(),
        client_id: Some(client_id.to_string()),
        path: Some(path.to_string()),
        instruction: instruction.to_string(),
        session_id: session_id.map(str::to_string),
    }
}

async fn dispatch_with_path_runner(
    runtime: &ToolRuntime,
    client_id: &str,
    call: ToolCall,
    agent_project_id: &str,
    project_path: &str,
    outcome: &str,
    registered: bool,
) -> ToolResult {
    let task = tokio::spawn({
        let runtime = runtime.clone();
        let auth = auth_context(None, true);
        async move { runtime.dispatch_with_auth(call, Some(&auth)).await }
    });
    for _ in 0..400 {
        if task.is_finished() {
            break;
        }
        if let Some(request) = next_patch_agent_request(runtime, client_id).await {
            if request.kind == "resolve_or_register_project" {
                let payload: Value =
                    serde_json::from_str(request.stdin.as_deref().unwrap()).unwrap();
                assert_eq!(payload["path"], project_path);
                let response = json!({
                    "id": format!("agent:{client_id}:{agent_project_id}"),
                    "agent_project_id": agent_project_id,
                    "client_id": client_id,
                    "name": agent_project_id,
                    "path": project_path,
                    "kind": "auto_registered",
                    "description": null,
                    "allow_patch": true,
                    "disabled": false,
                    "revision": "sha256:test",
                    "source": "path",
                    "outcome": outcome,
                    "registered": registered,
                    "created_config": registered,
                    "changed": registered,
                    "recovered": !registered,
                });
                complete_patch_agent_request(
                    runtime,
                    client_id,
                    &request.request_id,
                    0,
                    &response.to_string(),
                    "",
                )
                .await;
            } else {
                complete_agent_request_by_running_locally(runtime, client_id, request).await;
            }
        } else {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }
    assert!(
        task.is_finished(),
        "path-based coding call did not finish after servicing Runner requests"
    );
    task.await.unwrap()
}

fn instruction_events(runtime: &ToolRuntime, session_id: &str) -> Vec<SessionEvent> {
    runtime
        .sessions
        .summary(session_id, Some(200))
        .unwrap()
        .events
        .into_iter()
        .filter(|event| event.kind == "task_instruction")
        .collect()
}

fn valid_work_on_project_projection_input() -> serde_json::Value {
    json!({
        "detail": "standard",
        "session": {
            "session_id": "wc_sess_projection",
            "continuation": "created",
            "execution_context": {},
        },
        "project": {
            "resolved_id": "agent:wop:demo",
        },
        "project_resolution": {
            "source": "project",
            "outcome": "resolved_existing_project",
            "resolved_project": "agent:wop:demo",
            "registered": false,
        },
        "workspace": {
            "status": "clean",
            "git_available": true,
            "branch": "main",
            "head": "0123456789abcdef0123456789abcdef01234567",
            "clean": true,
            "conflicts": 0,
        },
        "instructions": {
            "status": "loaded",
            "sources": [],
            "content_included": true,
            "truncated": false,
            "total_chars": 0,
        },
        "semantic_navigation": {
            "supported": false,
            "available": false,
            "status": "not_applicable",
            "capability": null,
            "reason_code": "project_not_agent_backed",
        },
        "repository": {
            "status": "unavailable",
            "reason_code": "unsupported_or_unavailable",
        },
        "continuation": {
            "suggested_next_actions": {
                "items": [],
            },
            "jobs": {
                "active_count": 0,
                "blocking_active_count": 0,
                "nonblocking_active_count": 0,
                "recovering_count": 0,
                "terminal_pending_count": 0,
                "latest_status": "not_observed",
            },
        },
        "blockers": [],
        "warnings": [],
        "startup_verdict": {
            "status": "pass",
            "blocking": false,
            "suggested_next_actions": [],
        },
    })
}

#[test]
fn work_on_project_schema_and_registration() {
    let specs = registered_tool_specs();
    let names: Vec<&str> = specs.iter().map(|spec| spec.name.as_str()).collect();
    assert!(names.contains(&"work_on_project"), "missing from specs");

    // Model-visible ToolDefinition with workflow category and read-only risk.
    let metadata = crate::tool_runtime::metadata::lookup_tool_metadata("work_on_project").unwrap();
    assert!(metadata.read_only);
    assert!(!metadata.destructive);
    assert!(!metadata.shell_like);
    assert!(metadata.requires_project);
    assert_eq!(metadata.oauth_scope, Some("runtime:read"));
    assert_eq!(
        crate::tool_runtime::tool_definition::runtime_tool_category("work_on_project"),
        "workflow"
    );
    let definition =
        crate::tool_runtime::tool_definition::lookup_tool_definition("work_on_project").unwrap();
    assert_eq!(
        definition.agent_capability,
        Some(crate::tool_runtime::AgentCapability::GitOrShell)
    );
    assert!(definition.creates_or_binds_session());
    assert!(!definition.requires_explicit_business_session());

    // Schema requires instruction plus exactly one project source; session_id
    // is optional with the existing wc_sess_* format constraint.
    let spec = spec_named(&specs, "work_on_project");
    assert_eq!(required_fields(spec), vec!["instruction"]);
    let props = spec.input_schema["properties"].as_object().unwrap();
    assert_eq!(props["project"]["minLength"], 1);
    assert_eq!(props["path"]["pattern"], "^/");
    assert_eq!(spec.input_schema["oneOf"].as_array().unwrap().len(), 2);
    assert_eq!(props["instruction"]["minLength"], 1);
    assert_eq!(
        props["instruction"]["maxLength"],
        crate::tool_runtime::sessions::MAX_CODING_INSTRUCTION_CHARS
    );
    assert_eq!(props["session_id"]["type"], "string");
    assert_eq!(props["session_id"]["pattern"], "^wc_sess_[A-Za-z0-9_]+$");
    let schema_accepts = |value: Value| {
        crate::tool_runtime::startup_brief::validate_schema_instance_for_test(
            &value,
            &spec.input_schema,
        )
        .is_ok()
    };
    assert!(schema_accepts(
        json!({"project": SAMPLE_PROJECT, "instruction": "do it"})
    ));
    assert!(schema_accepts(json!({
        "client_id": "special",
        "path": "/root/git/example",
        "instruction": "do it"
    })));
    for invalid in [
        json!({"path": "/root/git/example", "instruction": "do it"}),
        json!({"client_id": "special", "instruction": "do it"}),
        json!({"project": SAMPLE_PROJECT, "client_id": "special", "path": "/root/git/example", "instruction": "do it"}),
    ] {
        assert!(
            !schema_accepts(invalid.clone()),
            "work_on_project schema accepted conflicting path source: {invalid}"
        );
    }

    // The wrapper must not expose advanced start_coding_task controls.
    for hidden in [
        "bind_current",
        "new_session",
        "resume_session_id",
        "mode",
        "deny_write_tools",
        "deny_shell_tools",
        "execution_context",
        "detail",
        "temporary_project_name",
    ] {
        assert!(
            !props.contains_key(hidden),
            "work_on_project schema must not expose {hidden}"
        );
    }

    // Output schema describes the compact projection fields.
    let output = crate::tool_runtime::registry::output_schema_for_tool("work_on_project");
    let output_props = output["properties"]["output"]["properties"]
        .as_object()
        .unwrap();
    for field in [
        "session_id",
        "project",
        "resolved_project",
        "project_resolution",
        "continuation",
        "execution_context",
        "readiness",
        "workspace",
        "repository",
        "instructions",
        "semantic_navigation",
        "jobs",
        "blockers",
        "warnings",
        "suggested_next_actions",
        "deterministic",
        "llm_summary",
    ] {
        assert!(
            output_props.contains_key(field),
            "work_on_project output schema should include {field}"
        );
    }
    for hidden in [
        "runtime_status",
        "connection_state",
        "authority",
        "tool_manifest",
        "recommended_flow",
        "startup_verdict",
        "git",
        "continuation_feedback",
        "current_binding",
    ] {
        assert!(
            !output_props.contains_key(hidden),
            "work_on_project output schema must not include {hidden}"
        );
    }

    // ToolCall parsing maps the wrapper's session_id to the business accessor.
    let call = ToolCall::from_tool_name(
        "work_on_project",
        json!({
            "project": SAMPLE_PROJECT,
            "instruction": "do the thing",
            "session_id": "wc_sess_target"
        }),
    )
    .unwrap();
    match &call {
        ToolCall::WorkOnProject { session_id, .. } => {
            assert_eq!(session_id.as_deref(), Some("wc_sess_target"))
        }
        _ => panic!("expected WorkOnProject"),
    }
    assert_eq!(call.project(), Some(SAMPLE_PROJECT));
    assert_eq!(call.session_id(), Some("wc_sess_target"));
}

#[test]
fn work_on_project_tool_call_requires_project_and_instruction() {
    assert!(
        ToolCall::from_tool_name("work_on_project", json!({})).is_err(),
        "project and instruction are required"
    );
    assert!(
        ToolCall::from_tool_name("work_on_project", json!({"project": SAMPLE_PROJECT})).is_err(),
        "instruction is required"
    );
    let path_call = ToolCall::from_tool_name(
        "work_on_project",
        json!({
            "client_id": "special",
            "path": "/root/git/example",
            "instruction": "do it"
        }),
    )
    .unwrap();
    assert!(path_call.project().is_none());
    // The schema declares additionalProperties: false so advanced
    // start_coding_task controls are not part of the wrapper surface.
    let spec = registered_tool_specs()
        .into_iter()
        .find(|spec| spec.name == "work_on_project")
        .unwrap();
    assert_eq!(spec.input_schema["additionalProperties"], false);
    let props = spec.input_schema["properties"].as_object().unwrap();
    for hidden in [
        "bind_current",
        "new_session",
        "resume_session_id",
        "mode",
        "deny_write_tools",
        "deny_shell_tools",
        "execution_context",
        "detail",
        "temporary_project_name",
    ] {
        assert!(
            !props.contains_key(hidden),
            "work_on_project schema must not expose {hidden}"
        );
    }
}

#[test]
fn work_on_project_projection_fails_closed_when_required_field_is_missing() {
    let mut output = valid_work_on_project_projection_input();
    output["session"]
        .as_object_mut()
        .unwrap()
        .remove("session_id");

    let result = crate::tool_runtime::coding_task::project_work_on_project_output(
        SAMPLE_PROJECT.to_string(),
        output,
    );
    assert!(!result.success);
    assert_eq!(
        result.output["error_kind"],
        "work_on_project_projection_failed"
    );
    assert_eq!(result.output["state_changed"], true);
    assert!(result.output["detail"]
        .as_str()
        .is_some_and(|detail| detail.contains("session_id")));
}

#[test]
fn work_on_project_projection_fails_closed_for_wrong_field_type() {
    let mut output = valid_work_on_project_projection_input();
    output["workspace"]["conflicts"] = json!("0");

    let result = crate::tool_runtime::coding_task::project_work_on_project_output(
        SAMPLE_PROJECT.to_string(),
        output,
    );
    assert!(!result.success);
    assert_eq!(
        result.output["error_kind"],
        "work_on_project_projection_failed"
    );
    assert_eq!(result.output["state_changed"], true);
}

#[test]
fn work_on_project_projection_does_not_default_missing_instruction_sources() {
    let mut output = valid_work_on_project_projection_input();
    output["instructions"]
        .as_object_mut()
        .unwrap()
        .remove("sources");

    let result = crate::tool_runtime::coding_task::project_work_on_project_output(
        SAMPLE_PROJECT.to_string(),
        output,
    );
    assert!(!result.success);
    assert_eq!(
        result.output["error_kind"],
        "work_on_project_projection_failed"
    );
    assert_eq!(result.output["state_changed"], true);
    assert!(result.output["detail"]
        .as_str()
        .is_some_and(|detail| detail.contains("sources")));
}

#[tokio::test]
async fn work_on_project_creates_a_new_normal_session_without_binding() {
    let root = tempfile::tempdir().unwrap();
    init_git_repo(root.path());
    let runtime = ToolRuntime::new_for_tests();
    let project = register_agent_project_at_path(&runtime, "wop-create", "demo", root.path()).await;
    let auth = auth_context(None, true);

    let result = dispatch_start_coding_task_in_window(
        &runtime,
        "wop-create",
        work_on_project_call("demo", "first root instruction", None),
        Some(&auth),
        "wop-create-window",
    )
    .await;
    assert!(result.success, "{:?}", result.error);

    // Compact projection fields are present and no full diagnostics leak.
    assert_eq!(result.output["deterministic"], true);
    assert_eq!(result.output["llm_summary"], false);
    let session_id = result.output["session_id"].as_str().unwrap().to_string();
    assert!(session_id.starts_with("wc_sess_"));
    assert_eq!(result.output["project"], "demo");
    assert_eq!(result.output["resolved_project"], project);
    assert_eq!(
        result.output["project_resolution"]["resolved_project"],
        project
    );
    assert_eq!(result.output["continuation"], "created");
    for hidden in [
        "runtime_status",
        "connection_state",
        "authority",
        "tool_manifest",
        "recommended_flow",
        "startup_verdict",
        "git",
        "continuation_feedback",
    ] {
        assert!(
            !result.output.as_object().unwrap().contains_key(hidden),
            "compact output must not include {hidden}"
        );
    }

    // A new active normal session was created with the instruction as root.
    let summary = runtime.sessions.summary(&session_id, Some(50)).unwrap();
    assert_eq!(summary.project.as_deref(), Some(project.as_str()));
    assert_eq!(summary.mode, SessionMode::Normal);
    assert!(!summary.guards.deny_write_tools);
    assert!(!summary.guards.deny_shell_tools);
    let instructions = instruction_events(&runtime, &session_id);
    assert_eq!(instructions.len(), 1);
    assert_eq!(
        instructions[0].instruction.as_deref(),
        Some("first root instruction")
    );

    // No current-window binding and no credential-wide fallback.
    assert_eq!(runtime.sessions.process_local_binding_count_for_test(), 0);
    assert_eq!(runtime.sessions.status().durable_binding_count, 0);
    assert_eq!(
        runtime
            .sessions
            .active_session_count_for_test(Some(&project)),
        1
    );

    // Compact workspace/instruction projection reflects the underlying brief.
    assert!(result.output["workspace"]["branch"].is_string());
    assert!(result.output["instructions"]["status"].is_string());

    // The actual compact projection validates against its output schema.
    let schema = crate::tool_runtime::registry::output_schema_for_tool("work_on_project");
    let instance = json!({ "success": true, "output": result.output });
    crate::tool_runtime::startup_brief::validate_schema_instance_for_test(&instance, &schema)
        .unwrap_or_else(|error| panic!("compact output must match its schema: {error}"));
}

#[tokio::test]
async fn path_source_auto_registers_reuses_and_supports_both_coding_entries() {
    let root = tempfile::tempdir().unwrap();
    init_git_repo(root.path());
    std::fs::write(root.path().join("hello.txt"), "hello\n").unwrap();
    let project_path = root.path().canonicalize().unwrap();
    let project_path = project_path.to_string_lossy().to_string();
    let runtime = ToolRuntime::new_for_tests();
    let client_id = "wop-path";
    register_agent_with_projects(
        &runtime,
        client_id,
        None,
        ShellClientCapabilities {
            shell: true,
            git: true,
            file_read: true,
            file_write: true,
            ..Default::default()
        },
        Vec::new(),
    )
    .await;

    let first = dispatch_with_path_runner(
        &runtime,
        client_id,
        path_work_on_project_call(client_id, &project_path, "first path instruction", None),
        "repo-a1b2c3d4",
        &project_path,
        "auto_registered",
        true,
    )
    .await;
    assert!(first.success, "{:?}", first.error);
    assert_eq!(first.output["project_resolution"]["source"], "path");
    assert_eq!(
        first.output["project_resolution"]["outcome"],
        "auto_registered"
    );
    assert_eq!(first.output["project_resolution"]["registered"], true);
    assert_eq!(first.output["permission"]["status"], "auto_approved");
    assert_eq!(first.output["permission"]["tool_name"], "register_project");
    assert_eq!(
        first.output["resolved_project"],
        "agent:wop-path:repo-a1b2c3d4"
    );
    assert!(
        !first.output.to_string().contains(&project_path),
        "compact work_on_project output leaked the absolute input path"
    );
    let session_id = first.output["session_id"].as_str().unwrap().to_string();

    let second = dispatch_with_path_runner(
        &runtime,
        client_id,
        path_work_on_project_call(
            client_id,
            &project_path,
            "second path instruction",
            Some(&session_id),
        ),
        "repo-a1b2c3d4",
        &project_path,
        "reused_existing_registration",
        false,
    )
    .await;
    assert!(second.success, "{:?}", second.error);
    assert_eq!(second.output["session_id"], session_id);
    assert_eq!(second.output["continuation"], "resumed_explicitly");
    assert_eq!(second.output["permission"]["status"], "auto_approved");
    assert_eq!(second.output["permission"]["tool_name"], "register_project");
    assert_eq!(
        second.output["project_resolution"]["outcome"],
        "reused_existing_registration"
    );
    assert_eq!(instruction_events(&runtime, &session_id).len(), 2);

    let advanced = ToolCall::from_tool_name(
        "start_coding_task",
        json!({
            "client_id": client_id,
            "path": project_path,
            "title": "advanced path entry",
            "detail": "standard",
            "bind_current": false,
            "new_session": true
        }),
    )
    .unwrap();
    let advanced = dispatch_with_path_runner(
        &runtime,
        client_id,
        advanced,
        "repo-a1b2c3d4",
        &project_path,
        "reused_existing_registration",
        false,
    )
    .await;
    assert!(advanced.success, "{:?}", advanced.error);
    assert_eq!(advanced.output["permission"]["status"], "auto_approved");
    assert_eq!(
        advanced.output["permission"]["tool_name"],
        "register_project"
    );
    assert_eq!(advanced.output["project_resolution"]["source"], "path");
    assert_eq!(
        advanced.output["project_resolution"]["resolved_project"],
        "agent:wop-path:repo-a1b2c3d4"
    );

    let listed = runtime.list_projects(Some(&auth_context(None, true))).await;
    assert!(listed.success);
    assert!(listed.output["projects"]
        .as_array()
        .unwrap()
        .iter()
        .any(|project| project["id"] == "agent:wop-path:repo-a1b2c3d4"
            && project["source"] == "auto_registered"));

    let read = ToolCall::from_tool_name(
        "read_file",
        json!({
            "project": "agent:wop-path:repo-a1b2c3d4",
            "session_id": session_id,
            "path": "hello.txt"
        }),
    )
    .unwrap();
    let read = dispatch_with_path_runner(
        &runtime,
        client_id,
        read,
        "repo-a1b2c3d4",
        &project_path,
        "reused_existing_registration",
        false,
    )
    .await;
    assert!(read.success, "{:?}", read.error);
    assert!(read.output["text"]
        .as_str()
        .is_some_and(|content| content.contains("hello")));
}

#[tokio::test]
async fn path_source_registers_before_exact_session_mismatch_and_never_falls_back() {
    let first_root = tempfile::tempdir().unwrap();
    let second_root = tempfile::tempdir().unwrap();
    init_git_repo(first_root.path());
    init_git_repo(second_root.path());
    let first_path = first_root.path().canonicalize().unwrap();
    let second_path = second_root.path().canonicalize().unwrap();
    let first_path = first_path.to_string_lossy().to_string();
    let second_path = second_path.to_string_lossy().to_string();
    let runtime = ToolRuntime::new_for_tests();
    let client_id = "wop-path-mismatch";
    register_agent_with_projects(
        &runtime,
        client_id,
        None,
        ShellClientCapabilities {
            shell: true,
            git: true,
            file_read: true,
            file_write: true,
            ..Default::default()
        },
        Vec::new(),
    )
    .await;

    let first = dispatch_with_path_runner(
        &runtime,
        client_id,
        path_work_on_project_call(client_id, &first_path, "first project", None),
        "first-a1b2c3d4",
        &first_path,
        "auto_registered",
        true,
    )
    .await;
    assert!(first.success);
    let session_id = first.output["session_id"].as_str().unwrap();
    let mismatch = dispatch_with_path_runner(
        &runtime,
        client_id,
        path_work_on_project_call(
            client_id,
            &second_path,
            "must not fall back",
            Some(session_id),
        ),
        "second-a1b2c3d4",
        &second_path,
        "auto_registered",
        true,
    )
    .await;
    assert!(!mismatch.success);
    assert_eq!(mismatch.output["error_kind"], "session_project_mismatch");
    assert_eq!(mismatch.output["state_changed"], true);
    assert_eq!(mismatch.output["permission"]["status"], "auto_approved");
    assert_eq!(
        mismatch.output["permission"]["tool_name"],
        "register_project"
    );
    assert_eq!(
        mismatch.output["project_resolution"]["resolved_project"],
        "agent:wop-path-mismatch:second-a1b2c3d4"
    );
    assert_eq!(instruction_events(&runtime, session_id).len(), 1);

    let advanced_mismatch = ToolCall::from_tool_name(
        "start_coding_task",
        json!({
            "client_id": client_id,
            "path": second_path,
            "resume_session_id": session_id,
            "detail": "standard",
            "bind_current": false
        }),
    )
    .unwrap();
    let advanced_mismatch = dispatch_with_path_runner(
        &runtime,
        client_id,
        advanced_mismatch,
        "second-a1b2c3d4",
        &second_path,
        "reused_existing_registration",
        false,
    )
    .await;
    assert!(!advanced_mismatch.success);
    assert_eq!(
        advanced_mismatch.output["error_kind"],
        "session_project_mismatch"
    );
    assert_eq!(advanced_mismatch.output["state_changed"], false);
    assert_eq!(
        advanced_mismatch.output["project_resolution"]["outcome"],
        "reused_existing_registration"
    );
    assert_eq!(instruction_events(&runtime, session_id).len(), 1);

    let listed = runtime.list_projects(Some(&auth_context(None, true))).await;
    assert_eq!(listed.output["count"], 2);
}

#[tokio::test]
async fn path_source_registers_before_unknown_session_rejection() {
    let root = tempfile::tempdir().unwrap();
    init_git_repo(root.path());
    let project_path = root.path().canonicalize().unwrap();
    let project_path = project_path.to_string_lossy().to_string();
    let runtime = ToolRuntime::new_for_tests();
    let client_id = "wop-path-unknown";
    register_agent_with_projects(
        &runtime,
        client_id,
        None,
        ShellClientCapabilities {
            shell: true,
            git: true,
            file_read: true,
            file_write: true,
            ..Default::default()
        },
        Vec::new(),
    )
    .await;

    let result = dispatch_with_path_runner(
        &runtime,
        client_id,
        path_work_on_project_call(
            client_id,
            &project_path,
            "unknown must not fall back",
            Some("wc_sess_unknown"),
        ),
        "unknown-a1b2c3d4",
        &project_path,
        "auto_registered",
        true,
    )
    .await;
    assert!(!result.success);
    assert_eq!(result.output["error_kind"], "unknown_session_id");
    assert_eq!(result.output["state_changed"], true);
    assert_eq!(result.output["permission"]["status"], "auto_approved");
    assert_eq!(result.output["permission"]["tool_name"], "register_project");
    assert_eq!(
        result.output["project_resolution"]["resolved_project"],
        "agent:wop-path-unknown:unknown-a1b2c3d4"
    );
    let listed = runtime.list_projects(Some(&auth_context(None, true))).await;
    assert_eq!(listed.output["count"], 1);
}

#[tokio::test]
async fn path_source_requires_project_write_scope_before_runner_enqueue() {
    let root = tempfile::tempdir().unwrap();
    let project_path = root.path().canonicalize().unwrap();
    let project_path = project_path.to_string_lossy().to_string();
    let runtime = ToolRuntime::new_for_tests();
    let auth = managed_oauth_auth_context("path-read-only", Some("path-read-only-hash"));
    register_agent_projects_for_auth(
        &runtime,
        "oauth-client",
        &auth,
        ShellClientCapabilities {
            shell: true,
            git: true,
            ..Default::default()
        },
        Vec::new(),
    )
    .await;

    let result = runtime
        .dispatch_with_auth(
            path_work_on_project_call("oauth-client", &project_path, "must not register", None),
            Some(&auth),
        )
        .await;
    assert!(!result.success);
    assert_eq!(result.output["error_kind"], "insufficient_scope");
    assert_eq!(
        result.output["required_scope"],
        crate::auth::SCOPE_PROJECT_WRITE
    );
    assert_eq!(result.output["state_changed"], false);
}

#[tokio::test]
async fn path_source_respects_restricted_authority_before_runner_enqueue() {
    let root = tempfile::tempdir().unwrap();
    let project_path = root.path().canonicalize().unwrap();
    let project_path = project_path.to_string_lossy().to_string();
    let runtime = ToolRuntime::new_for_tests()
        .with_permission_evaluator(PermissionEvaluator::with_mode(AuthorityMode::Restricted));
    let client_id = "wop-path-restricted";
    register_agent_with_projects(
        &runtime,
        client_id,
        None,
        ShellClientCapabilities {
            shell: true,
            git: true,
            ..Default::default()
        },
        Vec::new(),
    )
    .await;

    let result = runtime
        .dispatch_with_auth(
            path_work_on_project_call(client_id, &project_path, "must not register", None),
            Some(&auth_context(None, true)),
        )
        .await;
    assert!(!result.success);
    assert_eq!(result.output["error_kind"], "permission_denied");
    assert_eq!(result.output["permission"]["status"], "denied");
    assert_eq!(result.output["permission"]["tool_name"], "register_project");
    assert!(next_patch_agent_request(&runtime, client_id)
        .await
        .is_none());
}

#[tokio::test]
async fn work_on_project_continues_exact_session_and_appends_instruction() {
    let root = tempfile::tempdir().unwrap();
    init_git_repo(root.path());
    let runtime = ToolRuntime::new_for_tests();
    let project =
        register_agent_project_at_path(&runtime, "wop-continue", "demo", root.path()).await;
    let auth = auth_context(None, true);

    let first = dispatch_start_coding_task_in_window(
        &runtime,
        "wop-continue",
        work_on_project_call(&project, "root objective", None),
        Some(&auth),
        "wop-continue-window",
    )
    .await;
    assert!(first.success, "{:?}", first.error);
    let session_id = first.output["session_id"].as_str().unwrap().to_string();
    let before = instruction_events(&runtime, &session_id);
    assert_eq!(before.len(), 1);

    let continued = dispatch_start_coding_task_in_window(
        &runtime,
        "wop-continue",
        work_on_project_call(&project, "follow-up instruction", Some(&session_id)),
        Some(&auth),
        "wop-continue-window",
    )
    .await;
    assert!(continued.success, "{:?}", continued.error);
    assert_eq!(continued.output["session_id"], session_id);
    assert_eq!(continued.output["continuation"], "resumed_explicitly");

    // Same single session reused: no second session, no current binding.
    assert_eq!(
        runtime
            .sessions
            .active_session_count_for_test(Some(&project)),
        1
    );
    assert_eq!(runtime.sessions.process_local_binding_count_for_test(), 0);
    assert_eq!(runtime.sessions.status().durable_binding_count, 0);

    // Follow-up instruction appended; root title preserved.
    let events = instruction_events(&runtime, &session_id);
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].instruction.as_deref(), Some("root objective"));
    assert_eq!(
        events[1].instruction.as_deref(),
        Some("follow-up instruction")
    );
    let summary = runtime.sessions.summary(&session_id, Some(50)).unwrap();
    assert_eq!(summary.title.as_deref(), Some("root objective"));
}

#[tokio::test]
async fn work_on_project_failures_never_create_or_fall_back() {
    let dir = tempfile::tempdir().unwrap();
    let root_a = dir.path().join("a");
    let root_b = dir.path().join("b");
    std::fs::create_dir_all(&root_a).unwrap();
    std::fs::create_dir_all(&root_b).unwrap();
    init_git_repo(&root_a);
    init_git_repo(&root_b);
    let runtime = ToolRuntime::new_for_tests();
    register_agent_with_projects(
        &runtime,
        "wop-fail",
        None,
        ShellClientCapabilities {
            shell: true,
            git: true,
            file_read: true,
            file_write: true,
            ..Default::default()
        },
        vec![
            registered_project("a", &root_a.to_string_lossy()),
            registered_project("b", &root_b.to_string_lossy()),
        ],
    )
    .await;
    let project_a = crate::tool_runtime::agent_project_runtime_id("wop-fail", "a");
    let project_b = crate::tool_runtime::agent_project_runtime_id("wop-fail", "b");
    let auth = auth_context(None, true);

    // Create a stable active session on project A, plus a closed one.
    let first = dispatch_start_coding_task_in_window(
        &runtime,
        "wop-fail",
        work_on_project_call(&project_a, "stable session", None),
        Some(&auth),
        "wop-fail-window",
    )
    .await;
    assert!(first.success);
    let active_id = first.output["session_id"].as_str().unwrap().to_string();
    let closed_id = runtime
        .sessions
        .start_session_with_guards(
            Some(project_a.clone()),
            Some("closed project A".to_string()),
            SessionMode::Normal,
            SessionGuards::default(),
        )
        .session_id;
    runtime.sessions.close_session(&closed_id).unwrap();

    // Unknown Session: no creation, structured unknown_session_id failure.
    let unknown = dispatch_start_coding_task_in_window(
        &runtime,
        "wop-fail",
        work_on_project_call(&project_a, "must not create", Some("wc_sess_missing")),
        Some(&auth),
        "wop-fail-window",
    )
    .await;
    assert!(!unknown.success);
    assert_eq!(unknown.output["error_kind"], "unknown_session_id");

    // Closed Session: no creation, structured session_closed failure.
    let closed = dispatch_start_coding_task_in_window(
        &runtime,
        "wop-fail",
        work_on_project_call(&project_a, "must not reopen", Some(&closed_id)),
        Some(&auth),
        "wop-fail-window",
    )
    .await;
    assert!(!closed.success);
    assert_eq!(closed.output["error_kind"], "session_closed");
    assert_eq!(closed.output["lifecycle"], "closed");

    // Project mismatch: no fallback to any other session.
    let mismatch = dispatch_start_coding_task_in_window(
        &runtime,
        "wop-fail",
        work_on_project_call(&project_b, "must not cross", Some(&active_id)),
        Some(&auth),
        "wop-fail-window",
    )
    .await;
    assert!(!mismatch.success);
    assert_eq!(mismatch.output["error_kind"], "session_project_mismatch");
    assert_eq!(mismatch.output["session_project"], project_a);
    assert_eq!(mismatch.output["request_project"], project_b);

    // Invalid Session id fails before execution (no session created).
    let invalid = dispatch_start_coding_task_in_window(
        &runtime,
        "wop-fail",
        work_on_project_call(&project_a, "must not run", Some("not-a-session")),
        Some(&auth),
        "wop-fail-window",
    )
    .await;
    assert!(!invalid.success);
    assert_eq!(invalid.output["error_kind"], "invalid_session_id");

    // Nothing new was created and the active session is unchanged.
    assert_eq!(
        runtime
            .sessions
            .active_session_count_for_test(Some(&project_a)),
        1
    );
    assert_eq!(runtime.sessions.process_local_binding_count_for_test(), 0);
    assert_eq!(runtime.sessions.status().durable_binding_count, 0);
    let events = instruction_events(&runtime, &active_id);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].instruction.as_deref(), Some("stable session"));
}

#[test]
fn finish_coding_task_remains_optional_and_advisory() {
    let specs = registered_tool_specs();
    let names: Vec<&str> = specs.iter().map(|spec| spec.name.as_str()).collect();
    assert!(names.contains(&"finish_coding_task"), "still public");

    let finish = spec_named(&specs, "finish_coding_task");
    let description = finish.description.to_lowercase();
    for phrase in [
        "optional",
        "advisory",
        "does not decide task completion",
        "generate the user-facing final report",
    ] {
        assert!(
            description.contains(phrase),
            "finish_coding_task description must include {phrase}: {description}"
        );
    }
    assert!(
        finish.description.contains("does not"),
        "finish_coding_task description must be explicit about non-authority"
    );

    // The default coding manifest intent does not mark finish as the required
    // final step: it is the last optional evidence snapshot in the list.
    let coding = crate::tool_runtime::tool_definition::TOOL_MANIFEST_INTENTS
        .iter()
        .find(|intent| intent.name == "coding")
        .expect("coding intent");
    assert!(coding.tools.contains(&"work_on_project"));
    assert!(coding.tools.contains(&"finish_coding_task"));
    assert!(
        coding
            .tools
            .iter()
            .position(|t| *t == "finish_coding_task")
            .unwrap()
            > coding
                .tools
                .iter()
                .position(|t| *t == "work_on_project")
                .unwrap()
    );
}

#[test]
fn work_on_project_is_not_current_session_control_and_never_falls_back() {
    use crate::tool_runtime::tool_definition::{
        runtime_tool_allows_current_session_fallback, runtime_tool_is_current_session_control,
    };
    assert!(!runtime_tool_is_current_session_control("work_on_project"));
    assert!(
        !runtime_tool_allows_current_session_fallback("work_on_project"),
        "work_on_project must never implicitly use a current-session binding"
    );
}

/// Seed a representative Rust-style repository for the startup overview. The
/// files are committed so the tracked git index (the overview's project
/// boundary) includes every fixture entry; sensitive/build paths stay
/// excluded by the overview's own path policy.
fn seed_coding_repository(root: &std::path::Path, agents_body: &str) {
    init_git_repo(root);
    std::fs::write(
        root.join("AGENTS.md"),
        format!("# Repository rules\n\n{agents_body}\n"),
    )
    .unwrap();
    for path in [
        "README.md",
        "Cargo.toml",
        "src/lib.rs",
        "tests/basic.rs",
        "docs/index.md",
        "scripts/check.sh",
        ".github/workflows/ci.yml",
        "src/generated/deep/path.rs",
    ] {
        let path = root.join(path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, b"fixture contents must never be read").unwrap();
    }
    // Untracked/build/sensitive paths must never appear in the overview.
    std::fs::write(root.join(".env"), b"SECRET=do-not-leak").unwrap();
    std::fs::create_dir_all(root.join("target/debug")).unwrap();
    std::fs::write(root.join("target/debug/output"), b"binary").unwrap();
    for cmd in [
        "git add -A",
        "git commit -m 'seed fixture'",
        "git config status.showUntrackedFiles all",
    ] {
        let (exit_code, stdout, stderr, _) =
            crate::tool_runtime::helpers::run_command_sync(cmd, root, 30);
        assert_eq!(exit_code, 0, "{cmd}\n{stdout}{stderr}");
    }
}

/// Overwrite `AGENTS.md` in place (still tracked) so a follow-up resume sees a
/// changed fingerprint without a commit.
fn overwrite_agents_rule(root: &std::path::Path, body: &str) {
    std::fs::write(
        root.join("AGENTS.md"),
        format!("# Repository rules\n\n{body}\n"),
    )
    .unwrap();
}

#[tokio::test]
async fn work_on_project_new_task_returns_repository_overview_and_startup_context() {
    let root = tempfile::tempdir().unwrap();
    seed_coding_repository(root.path(), "Preserve unrelated changes.");
    let runtime = ToolRuntime::new_for_tests();
    let project = register_agent_project_at_path(&runtime, "wop-repo", "demo", root.path()).await;
    let auth = auth_context(None, true);

    let result = dispatch_start_coding_task_in_window(
        &runtime,
        "wop-repo",
        work_on_project_call(&project, "start on the repository", None),
        Some(&auth),
        "wop-repo-window",
    )
    .await;
    assert!(result.success, "{:?}", result.error);

    // resolved_project is the full runtime project id.
    assert_eq!(result.output["resolved_project"], project);
    // readiness mirrors the shared startup verdict.
    assert_eq!(result.output["readiness"]["status"].as_str(), Some("warn"));
    assert_eq!(result.output["readiness"]["blocking"], false);

    // Repository overview: Rust detected via Cargo.toml, project-relative.
    let repository = &result.output["repository"];
    assert_eq!(repository["status"], "available");
    let types = repository["project_types"]["items"].as_array().unwrap();
    assert!(types.iter().any(|kind| kind["kind"] == "rust"), "{types:?}");
    let manifests = repository["manifests"]["items"].as_array().unwrap();
    assert!(
        manifests
            .iter()
            .any(|manifest| manifest["path"] == "Cargo.toml"),
        "{manifests:?}"
    );
    let key_files = repository["key_files"]["items"].as_array().unwrap();
    assert!(key_files
        .iter()
        .any(|key| key["path"] == "AGENTS.md" || key["path"] == "README.md"));
    let roots = &repository["roots"];
    assert!(roots["source"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .any(|path| path == "src"));
    let top_level = repository["top_level"]["items"].as_array().unwrap();
    assert!(top_level.iter().any(|entry| entry["path"] == "Cargo.toml"));
    let serialized = repository.to_string();
    // No absolute path leak.
    assert!(!serialized.contains(&root.path().to_string_lossy().to_string()));
    // Overview warnings are present (symlinks are not part of the fixture but
    // the scan metadata is deterministic).
    assert!(repository["scan"].is_object());

    // suggested reads use project-relative paths with reasons.
    let suggested = repository["suggested_next_reads"]["items"]
        .as_array()
        .unwrap();
    assert!(suggested
        .iter()
        .all(|item| { item["path"].as_str().is_some() && item["reason"].as_str().is_some() }));

    // Instructions loaded with bounded body and headings.
    let instructions = &result.output["instructions"];
    assert_eq!(instructions["status"], "loaded");
    assert_eq!(instructions["content_included"], true);
    assert!(instructions["sources"]
        .as_array()
        .unwrap()
        .iter()
        .any(|source| source["path"] == "AGENTS.md"
            && source["content"]
                .as_str()
                .is_some_and(|content| content.contains("Preserve unrelated changes"))
            && source["headings"]
                .as_array()
                .is_some_and(|headings| !headings.is_empty())));

    // Semantic navigation block exists and is deterministic.
    assert!(result.output["semantic_navigation"].is_object());
    assert!(result.output["semantic_navigation"]["status"].is_string());

    // jobs block initial counts.
    let jobs = &result.output["jobs"];
    assert_eq!(jobs["active_count"], 0);
    assert_eq!(jobs["blocking_active_count"], 0);
    assert!(jobs["latest_status"].is_string());

    // No full diagnostics leak.
    for hidden in [
        "runtime_status",
        "connection_state",
        "authority",
        "tool_manifest",
        "recommended_flow",
        "startup_verdict",
        "git",
        "continuation_feedback",
    ] {
        assert!(
            !result.output.as_object().unwrap().contains_key(hidden),
            "compact output must not include {hidden}"
        );
    }

    // No current binding was established and a single session exists.
    assert_eq!(runtime.sessions.process_local_binding_count_for_test(), 0);
    assert_eq!(
        runtime
            .sessions
            .active_session_count_for_test(Some(&project)),
        1
    );

    // Schema validates.
    let schema = crate::tool_runtime::registry::output_schema_for_tool("work_on_project");
    let instance = json!({ "success": true, "output": result.output });
    crate::tool_runtime::startup_brief::validate_schema_instance_for_test(&instance, &schema)
        .unwrap_or_else(|error| panic!("compact output must match its schema: {error}"));
    let bytes = serde_json::to_vec(&result.output).unwrap().len();
    assert!(bytes <= crate::tool_runtime::startup_brief::STANDARD_STARTUP_HARD_MAX_BYTES);
}

#[tokio::test]
async fn work_on_project_exact_resume_reuses_rules_and_detects_changes() {
    let root = tempfile::tempdir().unwrap();
    seed_coding_repository(root.path(), "first rule body");
    let runtime = ToolRuntime::new_for_tests();
    let project = register_agent_project_at_path(&runtime, "wop-reuse", "demo", root.path()).await;
    let auth = auth_context(None, true);

    let first = dispatch_start_coding_task_in_window(
        &runtime,
        "wop-reuse",
        work_on_project_call(&project, "root objective", None),
        Some(&auth),
        "wop-reuse-window",
    )
    .await;
    assert!(first.success, "{:?}", first.error);
    let session_id = first.output["session_id"].as_str().unwrap().to_string();

    // Exact resume with unchanged rules: status=reused, no repeated content.
    let reused = dispatch_start_coding_task_in_window(
        &runtime,
        "wop-reuse",
        work_on_project_call(&project, "follow-up", Some(&session_id)),
        Some(&auth),
        "wop-reuse-window",
    )
    .await;
    assert!(reused.success, "{:?}", reused.error);
    assert_eq!(reused.output["session_id"], session_id);
    assert_eq!(reused.output["continuation"], "resumed_explicitly");
    let reused_instructions = &reused.output["instructions"];
    assert_eq!(reused_instructions["status"], "reused");
    assert_eq!(reused_instructions["content_included"], false);
    assert!(reused_instructions["changed_sources"]
        .as_array()
        .map(|sources| sources.is_empty())
        .unwrap_or(false));
    for source in reused_instructions["sources"].as_array().unwrap() {
        if source["path"] == "AGENTS.md" {
            assert_eq!(source["content"], serde_json::Value::Null);
            assert!(source["fingerprint"].is_string());
        }
    }

    // Change the rule then resume: status=changed, changed_sources includes it.
    overwrite_agents_rule(root.path(), "changed rule body");
    let changed = dispatch_start_coding_task_in_window(
        &runtime,
        "wop-reuse",
        work_on_project_call(&project, "after rule change", Some(&session_id)),
        Some(&auth),
        "wop-reuse-window",
    )
    .await;
    assert!(changed.success, "{:?}", changed.error);
    assert_eq!(changed.output["session_id"], session_id);
    assert_eq!(changed.output["instructions"]["status"], "changed");
    assert!(
        changed.output["instructions"]["changed_sources"]
            .as_array()
            .unwrap()
            .contains(&json!("AGENTS.md")),
        "{:?}",
        changed.output["instructions"]["changed_sources"]
    );
    assert!(changed.output["instructions"]["sources"]
        .as_array()
        .unwrap()
        .iter()
        .any(|source| source["path"] == "AGENTS.md"
            && source["content"]
                .as_str()
                .is_some_and(|content| content.contains("changed rule body"))));
}

#[tokio::test]
async fn work_on_project_repository_unavailable_keeps_session_and_warns() {
    // Agent without the file capability: the overview probe fails closed to
    // unavailable, but the session still creates/continues successfully.
    let root = tempfile::tempdir().unwrap();
    seed_coding_repository(root.path(), "rules still load");
    let runtime = ToolRuntime::new_for_tests();
    register_agent_with_projects(
        &runtime,
        "wop-nocap",
        None,
        ShellClientCapabilities {
            shell: true,
            git: true,
            file_read: false,
            file_write: false,
            ..Default::default()
        },
        vec![registered_project("demo", &root.path().to_string_lossy())],
    )
    .await;
    let project = crate::tool_runtime::agent_project_runtime_id("wop-nocap", "demo");
    let auth = auth_context(None, true);

    let result = dispatch_start_coding_task_in_window(
        &runtime,
        "wop-nocap",
        work_on_project_call(&project, "start without file capability", None),
        Some(&auth),
        "wop-nocap-window",
    )
    .await;
    assert!(result.success, "{:?}", result.error);

    // Session created; repository overview unavailable; warning present.
    let session_id = result.output["session_id"].as_str().unwrap().to_string();
    assert!(session_id.starts_with("wc_sess_"));
    assert_eq!(result.output["repository"]["status"], "unavailable");
    assert_eq!(
        result.output["repository"]["reason_code"],
        "unsupported_or_unavailable"
    );
    assert!(result.output["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|warning| warning == "repository_overview_unavailable"));

    // No raw error, command, or absolute path leaks.
    let serialized = result.output.to_string();
    assert!(!serialized.contains(&root.path().to_string_lossy().to_string()));
    assert!(!serialized.contains("file_read capability"));
    assert!(!serialized.contains("stderr"));

    // No fallback to an arbitrary shell scan: no extra agent request enqueued.
    assert!(
        next_patch_agent_request(&runtime, "wop-nocap")
            .await
            .is_none(),
        "unavailable overview must not fall back to a shell scan"
    );
}

#[tokio::test]
async fn work_on_project_repository_overview_timeout_is_nonblocking() {
    let root = tempfile::tempdir().unwrap();
    seed_coding_repository(root.path(), "rules load despite overview timeout");
    // Tight overview timeout so the probe expires quickly.
    let runtime = ToolRuntime::new_for_tests()
        .with_repository_overview_probe_timeout(std::time::Duration::from_millis(50));
    let project =
        register_agent_project_at_path(&runtime, "wop-timeout", "demo", root.path()).await;
    let auth = auth_context(None, true);

    let task = tokio::spawn({
        let runtime = runtime.clone();
        let auth = auth.clone();
        let project = project.clone();
        async move {
            let window = crate::client_window::ClientWindow::for_test("wop-timeout-window");
            runtime
                .dispatch_with_auth_transport_options_and_metadata_with_sandbox(
                    work_on_project_call(&project, "start despite overview timeout", None),
                    Some(&auth),
                    crate::tool_runtime::sessions::SessionTransport::Mcp,
                    true,
                    false,
                    Default::default(),
                    None,
                    Some(&window),
                )
                .await
        }
    });

    // Service the git/instruction probes but never the overview request.
    let mut overview_request = None;
    while !task.is_finished() {
        let Some(request) = next_patch_agent_request(&runtime, "wop-timeout").await else {
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            continue;
        };
        if request.kind == "file_project_overview" {
            overview_request = Some(request.request_id.clone());
            // Intentionally never complete it; the probe must time out.
            continue;
        }
        let (exit_code, stdout, stderr) = run_agent_shell_request_locally(&request);
        complete_patch_agent_request(
            &runtime,
            "wop-timeout",
            &request.request_id,
            exit_code,
            &stdout,
            &stderr,
        )
        .await;
    }
    let result = task.await.unwrap();
    assert!(result.success, "{:?}", result.error);

    // Overview unavailable with the deterministic reason; session still works.
    assert_eq!(result.output["repository"]["status"], "unavailable");
    assert_eq!(
        result.output["repository"]["reason_code"],
        "unsupported_or_unavailable"
    );
    assert!(result.output["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|warning| warning == "repository_overview_unavailable"));
    let session_id = result.output["session_id"].as_str().unwrap();
    assert!(session_id.starts_with("wc_sess_"));
    let summary = runtime.sessions.summary(session_id, Some(20)).unwrap();
    assert_eq!(summary.project.as_deref(), Some(project.as_str()));

    // The timed-out overview request was cancelled server-side.
    if let Some(request_id) = overview_request {
        let expired = runtime
            .shell_clients
            .complete(crate::shell_protocol::ShellAgentResultRequest {
                client_id: "wop-timeout".to_string(),
                agent_instance_id: "inst".to_string(),
                request_id,
                exit_code: Some(0),
                stdout: Some("{}".to_string()),
                stderr: None,
                duration_ms: Some(1),
                error: None,
            })
            .await
            .expect_err("timed-out overview probe must remove pending waiter");
        assert!(
            expired.contains("unknown or expired shell request"),
            "{expired}"
        );
    }
}

/// Drive a `work_on_project` dispatch to completion, completing the
/// `file_project_overview` probe with `overview_stdout` (exit code 0, no error)
/// while servicing every other agent request locally. Returns the compact
/// task result and the overview request id that was answered.
async fn dispatch_work_on_project_with_overview_stdout(
    runtime: &ToolRuntime,
    client_id: &str,
    project: &str,
    instruction: &str,
    overview_stdout: String,
    auth: Option<&crate::auth::AuthContext>,
) -> (crate::tool_runtime::ToolResult, Option<String>) {
    use crate::client_window::ClientWindow;
    use crate::tool_runtime::sessions::SessionTransport;
    use crate::tool_runtime::StartupDetail;

    let task = tokio::spawn({
        let runtime = runtime.clone();
        let auth = auth.cloned();
        let call = work_on_project_call(project, instruction, None);
        async move {
            let window = ClientWindow::for_test("overview-window");
            runtime
                .dispatch_with_auth_transport_options_and_metadata_with_sandbox(
                    call,
                    auth.as_ref(),
                    SessionTransport::Mcp,
                    true,
                    false,
                    Default::default(),
                    None,
                    Some(&window),
                )
                .await
        }
    });

    let mut overview_request_id = None;
    while !task.is_finished() {
        let Some(request) = next_patch_agent_request(runtime, client_id).await else {
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            continue;
        };
        if request.kind == "file_project_overview" {
            overview_request_id = Some(request.request_id.clone());
            complete_patch_agent_request(
                runtime,
                client_id,
                &request.request_id,
                0,
                &overview_stdout,
                "",
            )
            .await;
            continue;
        }
        let (exit_code, stdout, stderr) = run_agent_shell_request_locally(&request);
        complete_patch_agent_request(
            runtime,
            client_id,
            &request.request_id,
            exit_code,
            &stdout,
            &stderr,
        )
        .await;
    }
    let result = task.await.unwrap();
    let _ = StartupDetail::Standard;
    (result, overview_request_id)
}

/// Build a structurally-valid root overview (depth 2 / limit 120) so a test can
/// mutate one field and observe fail-closed behavior. The fixture repo must
/// already be seeded and committed.
fn valid_agent_overview_stdout(
    runtime: &ToolRuntime,
    client_id: &str,
    root: &std::path::Path,
) -> String {
    let _ = (runtime, client_id);
    let overview = crate::project_overview::build_project_overview(root, ".", Some(2), Some(120))
        .expect("valid agent overview fixture");
    overview.to_string()
}

#[tokio::test]
async fn work_on_project_repository_overview_rejects_malformed_runner_responses() {
    let root = tempfile::tempdir().unwrap();
    seed_coding_repository(root.path(), "rules load despite malformed overview");
    let runtime = ToolRuntime::new_for_tests();
    let project =
        register_agent_project_at_path(&runtime, "wop-malformed", "demo", root.path()).await;
    let auth = auth_context(None, true);

    let valid = valid_agent_overview_stdout(&runtime, "wop-malformed", root.path());
    let valid_value: serde_json::Value = serde_json::from_str(&valid).unwrap();

    let cases: Vec<(&str, serde_json::Value)> = vec![
        ("absolute path", {
            let mut v = valid_value.clone();
            v["top_level"]
                .as_array_mut()
                .unwrap()
                .push(json!({"path": "/etc/passwd", "kind": "file"}));
            v
        }),
        ("parent traversal", {
            let mut v = valid_value.clone();
            v["manifests"]
                .as_array_mut()
                .unwrap()
                .push(json!({"path": "../outside/Cargo.toml", "kind": "rust_manifest"}));
            v
        }),
        ("request boundary mismatch", {
            let mut v = valid_value.clone();
            v["scan"]["max_depth"] = json!(4);
            v["scan"]["limit"] = json!(500);
            v["path"] = json!("src");
            v
        }),
        ("unknown project type", {
            let mut v = valid_value.clone();
            v["project_types"]
                .as_array_mut()
                .unwrap()
                .push(json!({"kind": "cobol", "evidence": []}));
            v
        }),
        ("unknown key-file kind", {
            let mut v = valid_value.clone();
            v["key_files"]
                .as_array_mut()
                .unwrap()
                .push(json!({"path": "README.md", "kind": "mystery", "reason": "x"}));
            v
        }),
        ("unknown warning", {
            let mut v = valid_value.clone();
            v["warnings"]
                .as_array_mut()
                .unwrap()
                .push(json!("nuclear_launch_detected"));
            v
        }),
        ("returned_entry_count as string", {
            let mut v = valid_value.clone();
            v["scan"]["returned_entry_count"] = json!("plenty");
            v
        }),
        ("warnings as object", {
            let mut v = valid_value.clone();
            v["warnings"] = json!({"note": "not an array"});
            v
        }),
        ("duplicate top-level paths", {
            let mut v = valid_value.clone();
            let top = v["top_level"].as_array_mut().unwrap();
            top.push(top[0].clone());
            v
        }),
    ];

    for (label, payload) in cases {
        let stdout = payload.to_string();
        let (result, overview_id) = dispatch_work_on_project_with_overview_stdout(
            &runtime,
            "wop-malformed",
            &project,
            label,
            stdout,
            Some(&auth),
        )
        .await;
        assert!(
            result.success,
            "{label}: task must still succeed: {:?}",
            result.error
        );
        assert!(
            overview_id.is_some(),
            "{label}: overview probe must be issued"
        );
        let repository = &result.output["repository"];
        assert_eq!(
            repository["status"], "unavailable",
            "{label}: malformed Runner response must fail closed"
        );
        assert_eq!(
            repository["reason_code"], "unsupported_or_unavailable",
            "{label}: deterministic reason code"
        );
        // No raw stdout, stderr, error text, or absolute paths leak into the
        // model-facing compact output.
        let serialized = result.output.to_string();
        assert!(
            !serialized.contains("runner_secret"),
            "{label}: extra Runner field leaked"
        );
        assert!(
            !serialized.contains("/etc/passwd") && !serialized.contains("/absolute/leak"),
            "{label}: absolute path leaked"
        );
        assert!(
            !serialized.contains("nuclear_launch_detected") && !serialized.contains("cobol"),
            "{label}: malformed enum leaked"
        );
        assert!(
            !serialized.contains("../outside"),
            "{label}: traversal path leaked"
        );
        assert!(
            serialized.len() <= crate::tool_runtime::startup_brief::STANDARD_STARTUP_HARD_MAX_BYTES,
            "{label}: compact output exceeded 30 KiB"
        );
        // The deterministic unavailable warning is surfaced.
        assert!(
            result.output["warnings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|warning| warning == "repository_overview_unavailable"),
            "{label}: repository_overview_unavailable warning missing"
        );
        // A session is still created despite the malformed overview.
        let session_id = result.output["session_id"].as_str().unwrap();
        assert!(
            session_id.starts_with("wc_sess_"),
            "{label}: session not created"
        );
    }
}

#[tokio::test]
async fn work_on_project_repository_overview_strips_unknown_runner_fields_and_stays_bounded() {
    let root = tempfile::tempdir().unwrap();
    seed_coding_repository(root.path(), "rules load despite extra runner fields");
    let runtime = ToolRuntime::new_for_tests();
    let project = register_agent_project_at_path(&runtime, "wop-strip", "demo", root.path()).await;
    let auth = auth_context(None, true);

    let valid = valid_agent_overview_stdout(&runtime, "wop-strip", root.path());
    let mut payload: serde_json::Value = serde_json::from_str(&valid).unwrap();
    // A malicious/defensive Runner adds an oversized `scan` extra field and
    // top-level unknowns (including an absolute path). The contract must not
    // fail on mere extras — it must strip them and keep the formal fields only,
    // so the model output stays small and free of leaked content.
    payload["scan"]["padding"] = json!("X".repeat(40_000));
    payload["scan"]["nested"] = json!({"deep": json!(["Y".repeat(10_000), 1, 2])});
    payload["runner_secret"] = json!("/absolute/leak");

    let (result, overview_id) = dispatch_work_on_project_with_overview_stdout(
        &runtime,
        "wop-strip",
        &project,
        "strip extras",
        payload.to_string(),
        Some(&auth),
    )
    .await;
    assert!(result.success, "{:?}", result.error);
    assert!(overview_id.is_some(), "overview probe must be issued");
    let repository = &result.output["repository"];
    assert_eq!(
        repository["status"], "available",
        "extras must be stripped, not rejected"
    );

    // scan keeps exactly the 5 fixed formal fields; padding/nested dropped.
    let scan = &repository["scan"];
    assert!(scan.is_object());
    let mut scan_keys: Vec<&str> = scan
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    scan_keys.sort_unstable();
    assert_eq!(
        scan_keys,
        [
            "limit",
            "max_depth",
            "returned_entry_count",
            "truncated",
            "truncation_reason"
        ],
        "scan must keep only the fixed fields: {scan:?}"
    );
    assert_eq!(scan["max_depth"], 2);
    assert_eq!(scan["limit"], 120);

    let serialized = result.output.to_string();
    assert!(!serialized.contains("padding"), "scan padding leaked");
    assert!(
        !serialized.contains("runner_secret"),
        "extra runner field leaked"
    );
    assert!(
        !serialized.contains("/absolute/leak"),
        "absolute path leaked"
    );
    assert!(
        serialized.len() <= crate::tool_runtime::startup_brief::STANDARD_STARTUP_HARD_MAX_BYTES,
        "compact output exceeded 30 KiB after stripping"
    );
}

#[tokio::test]
async fn work_on_project_repository_overview_accepts_valid_runner_response() {
    let root = tempfile::tempdir().unwrap();
    seed_coding_repository(root.path(), "rules load with valid overview");
    let runtime = ToolRuntime::new_for_tests();
    let project =
        register_agent_project_at_path(&runtime, "wop-valid-overview", "demo", root.path()).await;

    let stdout = valid_agent_overview_stdout(&runtime, "wop-valid-overview", root.path());
    let auth = auth_context(None, true);
    let (result, overview_id) = dispatch_work_on_project_with_overview_stdout(
        &runtime,
        "wop-valid-overview",
        &project,
        "valid overview",
        stdout,
        Some(&auth),
    )
    .await;
    assert!(result.success, "{:?}", result.error);
    assert!(overview_id.is_some(), "overview probe must be issued");
    let repository = &result.output["repository"];
    assert_eq!(
        repository["status"], "available",
        "valid response must be accepted"
    );
    // scan projection keeps only the fixed fields, no extras.
    let scan = &repository["scan"];
    assert!(scan.is_object());
    assert!(
        scan.as_object().unwrap().len() == 5,
        "scan must keep 5 fields"
    );
    assert_eq!(scan["max_depth"], 2);
    assert_eq!(scan["limit"], 120);
    // Rust is detected via the committed Cargo.toml fixture.
    let types = repository["project_types"]["items"].as_array().unwrap();
    assert!(types.iter().any(|kind| kind["kind"] == "rust"));
    let serialized = repository.to_string();
    assert!(!serialized.contains(&root.path().to_string_lossy().to_string()));
}
