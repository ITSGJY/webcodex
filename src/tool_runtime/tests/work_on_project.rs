//! Focused tests for the `work_on_project` thin coding-task entry point.
//!
//! `work_on_project` is a model-facing wrapper over `start_coding_task`: it
//! validates three simple inputs, maps them onto normal coding-task defaults,
//! delegates the business implementation, and projects a compact startup
//! result. It never binds a current window, never guesses a recent Session,
//! and never falls back to a credential-wide Session.

use super::reconnect::dispatch_start_coding_task_in_window;
use super::support::*;
use crate::shell_protocol::ShellClientCapabilities;
use crate::tool_runtime::sessions::{SessionEvent, SessionGuards};
use crate::tool_runtime::{registered_tool_specs, SessionMode, ToolCall, ToolRuntime};
use serde_json::json;

fn work_on_project_call(project: &str, instruction: &str, session_id: Option<&str>) -> ToolCall {
    ToolCall::WorkOnProject {
        project: project.to_string(),
        instruction: instruction.to_string(),
        session_id: session_id.map(str::to_string),
    }
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
        "detail": "minimal",
        "session": {
            "session_id": "wc_sess_projection",
            "continuation": "created",
            "execution_context": {},
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
        },
        "continuation": {
            "suggested_next_actions": {
                "items": [],
            },
        },
        "blockers": [],
        "warnings": [],
        "startup_verdict": {
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

    // Schema requires project and instruction; session_id is optional with the
    // existing wc_sess_* format constraint.
    let spec = spec_named(&specs, "work_on_project");
    assert_eq!(required_fields(spec), vec!["project", "instruction"]);
    let props = spec.input_schema["properties"].as_object().unwrap();
    assert_eq!(props["project"]["minLength"], 1);
    assert_eq!(props["instruction"]["minLength"], 1);
    assert_eq!(
        props["instruction"]["maxLength"],
        crate::tool_runtime::sessions::MAX_CODING_INSTRUCTION_CHARS
    );
    assert_eq!(props["session_id"]["type"], "string");
    assert_eq!(props["session_id"]["pattern"], "^wc_sess_[A-Za-z0-9_]+$");

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
        "client_id",
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
        "continuation",
        "execution_context",
        "workspace",
        "instructions",
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
        "semantic_navigation",
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
    // The schema declares additionalProperties: false so advanced start_coding_task
    // controls are not part of the wrapper's model-visible surface.
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
        "client_id",
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
        work_on_project_call(&project, "first root instruction", None),
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
    assert_eq!(result.output["project"], project);
    assert_eq!(result.output["continuation"], "created");
    for hidden in [
        "runtime_status",
        "connection_state",
        "authority",
        "tool_manifest",
        "recommended_flow",
        "startup_verdict",
        "git",
        "semantic_navigation",
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
