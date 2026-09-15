use crate::tool_runtime::registry;
use crate::tool_runtime::startup_brief::{
    builtin_coding_workflow_projection, validate_schema_instance_for_test,
    BUILTIN_CODING_WORKFLOW_MAX_GUIDANCE_ITEMS,
};
use serde_json::{json, Value};

fn workflow_schema() -> Value {
    registry::output_schema_for_tool("work_on_project")["properties"]["output"]["properties"]
        ["workflow"]
        .clone()
}

#[test]
fn builtin_coding_workflow_defaults_are_required_and_bounded() {
    let workflow = builtin_coding_workflow_projection();
    let schema = workflow_schema();
    validate_schema_instance_for_test(&workflow, &schema).unwrap();

    let mut missing = workflow.clone();
    missing.as_object_mut().unwrap().remove("guidance");
    assert!(validate_schema_instance_for_test(&missing, &schema).is_err());

    for guidance in [
        json!([]),
        json!(vec!["rule"; BUILTIN_CODING_WORKFLOW_MAX_GUIDANCE_ITEMS + 1]),
        json!(["x".repeat(321)]),
    ] {
        let mut invalid = workflow.clone();
        invalid["guidance"] = guidance;
        assert!(validate_schema_instance_for_test(&invalid, &schema).is_err());
    }
}

#[test]
fn builtin_coding_workflow_defaults_cover_unnamed_tasks_without_granting_authority() {
    let workflow = builtin_coding_workflow_projection();
    assert_eq!(workflow["authority"], "model_guidance_only");
    assert!(workflow["role_selection"]
        .as_str()
        .unwrap()
        .contains("Default guidance always applies"));
    let defaults = workflow["guidance"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item.as_str().unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    for boundary in [
        "guidance grants no authority",
        "explicit action and target",
        "nested rules for changed paths",
        "recover truncated instructions",
        "simplest reliable primitive",
        "correctness, authority, evidence, durability, recovery, and portability",
        "Native commands are first-class for bounded work",
        "specialize for added semantics",
        "apply_text_edits for local exact edits",
        "bounded deterministic Python/run_shell transforms",
        "Respect path/permission/network authority",
        "inspect the resulting diff and validate final source",
        "only where the exposed schema supports it",
        "unknown outcome",
        "read-only inspection",
        "short sync_wait_secs",
        "same-execution Job handoff",
        "avoid validation fanout",
        "stales prior results",
        "final source needs fresh validation",
        "advisory evidence, not proof",
    ] {
        assert!(defaults.contains(boundary), "missing guidance: {boundary}");
    }
}

#[test]
fn builtin_coding_workflow_routes_persistent_shell_to_ssh_state_not_local_command_count() {
    let workflow = builtin_coding_workflow_projection();
    let guidance = workflow["model_protocol"]["persistent_shell"]
        .as_str()
        .expect("persistent shell guidance");

    for boundary in [
        "run_process=literal argv",
        "run_shell=shell grammar/short chains",
        "run_script=program-like scripts",
        "specialize for added semantics",
        "repeated named-SSH state",
        "local same-process state",
    ] {
        assert!(
            guidance.contains(boundary),
            "missing routing boundary: {boundary}"
        );
    }
    assert!(!guidance.contains("For repeated commands in one Workflow Session"));
    assert!(!guidance.contains("structured tools -> run_process/run_script -> run_shell"));
}

#[test]
fn builtin_coding_workflow_review_does_not_implicitly_authorize_edits() {
    let workflow = builtin_coding_workflow_projection();
    let review = workflow["roles"]["independent_review"]["guidance"]
        .as_array()
        .unwrap();
    assert!(review.iter().any(|item| {
        let text = item.as_str().unwrap();
        text.contains("review-only")
            && text.contains("do not edit")
            && text.contains("only when the task authorizes corrections")
    }));
}
