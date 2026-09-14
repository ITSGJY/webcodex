use crate::tool_runtime::{AgentWaitEventSelectorCall, ToolRuntime};
use std::sync::Arc;

fn runtime_with_db() -> (tempfile::TempDir, Arc<crate::db::Database>, ToolRuntime) {
    let temp = tempfile::tempdir().unwrap();
    let db = Arc::new(crate::db::Database::open(&temp.path().join("agent-waits.db")).unwrap());
    let runtime = ToolRuntime::new_for_tests().with_communication_database(db.clone());
    (temp, db, runtime)
}

fn create_agent(runtime: &ToolRuntime, handle: &str) -> String {
    let result = runtime.create_agent_identity(
        None,
        handle.to_string(),
        format!("{handle} display"),
        None,
        Vec::new(),
        format!("create-{handle}"),
    );
    assert!(result.success, "{:?}", result.output);
    result.output["agent"]["agent_id"]
        .as_str()
        .unwrap()
        .to_string()
}

#[test]
fn wait_runtime_surface_returns_exact_wait_and_existing_continuation_projection() {
    let (_temp, _db, runtime) = runtime_with_db();
    let watcher = create_agent(&runtime, "wait-runtime-watcher");
    let worker = create_agent(&runtime, "wait-runtime-worker");
    let endpoint = runtime.attach_agent_endpoint(
        None,
        watcher.clone(),
        "ChatGPT".to_string(),
        Some("wait-runtime-view".to_string()),
        "wait-runtime-endpoint".to_string(),
    );
    assert!(endpoint.success, "{:?}", endpoint.output);
    let endpoint_id = endpoint.output["endpoint"]["endpoint_id"]
        .as_str()
        .unwrap()
        .to_string();
    let generation = endpoint.output["endpoint"]["controller_generation"]
        .as_i64()
        .unwrap();

    let created_task = runtime.create_agent_task(
        None,
        "Wait runtime source".to_string(),
        "PRIVATE source instruction".to_string(),
        Some(worker.clone()),
        None,
        None,
        Some("agent:special:private-source-project".to_string()),
        "wait-runtime-task".to_string(),
    );
    assert!(created_task.success, "{:?}", created_task.output);
    let task_id = created_task.output["task"]["summary"]["task_id"]
        .as_str()
        .unwrap()
        .to_string();
    let started = runtime.start_agent_task_attempt(
        None,
        task_id.clone(),
        worker.clone(),
        "wait-runtime-attempt".to_string(),
    );
    assert!(started.success, "{:?}", started.output);
    let completed = runtime.complete_agent_task_attempt(
        None,
        task_id.clone(),
        started.output["attempt"]["attempt_id"]
            .as_str()
            .unwrap()
            .to_string(),
        worker,
        started.output["attempt_fence"]
            .as_str()
            .unwrap()
            .to_string(),
        started.output["attempt"]["attempt_controller_generation"]
            .as_i64()
            .unwrap(),
        "succeeded".to_string(),
        Some("PRIVATE terminal result".to_string()),
        Some("PRIVATE terminal reason".to_string()),
        "wait-runtime-complete".to_string(),
    );
    assert!(completed.success, "{:?}", completed.output);

    let waited = runtime.wait_for_agent_events(
        None,
        watcher.clone(),
        endpoint_id,
        generation,
        vec![AgentWaitEventSelectorCall {
            kind: "agent_task_terminal".to_string(),
            task_id: task_id.clone(),
        }],
        "wait-runtime-create".to_string(),
    );
    assert!(waited.success, "{:?}", waited.output);
    assert_eq!(waited.output["agent_wait"]["state"], "triggered");
    assert_eq!(waited.output["agent_wait"]["match_count"], 1);
    assert_eq!(
        waited.output["agent_wait"]["matches"][0]["task_id"],
        task_id
    );
    let wait_id = waited.output["agent_wait"]["wait_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        waited.output["agent_continuation"]["wake"]["wait_id"],
        wait_id
    );
    assert_eq!(
        waited.output["agent_continuation"]["wake"]["wait_match_count"],
        1
    );
    let serialized = waited.output.to_string();
    for private in [
        "PRIVATE source instruction",
        "PRIVATE terminal result",
        "PRIVATE terminal reason",
        "private-source-project",
        "attempt_fence",
        "consume_token",
    ] {
        assert!(
            !serialized.contains(private),
            "Wait output leaked {private}"
        );
    }

    let read = runtime.read_agent_wait(None, wait_id.clone());
    assert!(read.success, "{:?}", read.output);
    assert_eq!(read.output["agent_wait"]["state"], "triggered");
    let app_read = runtime.agent_wait_state(None, wait_id.clone());
    assert!(app_read.success, "{:?}", app_read.output);
    assert_eq!(app_read.output, read.output);

    let cancelled =
        runtime.cancel_agent_wait(None, wait_id.clone(), "wait-runtime-cancel".to_string());
    assert!(cancelled.success, "{:?}", cancelled.output);
    assert_eq!(cancelled.output["agent_wait"]["state"], "cancelled");
    let replay = runtime.cancel_agent_wait(None, wait_id, "wait-runtime-cancel".to_string());
    assert!(replay.success, "{:?}", replay.output);
    assert_eq!(replay.output["replayed"], true);
    assert_eq!(replay.output["state_changed"], false);
}

#[test]
fn wait_tool_contracts_are_definition_owned_and_hidden_state_stays_app_only() {
    let specs = crate::tool_runtime::registered_tool_specs();
    for name in [
        "wait_for_agent_events",
        "read_agent_wait",
        "cancel_agent_wait",
    ] {
        let spec = specs.iter().find(|spec| spec.name == name).unwrap();
        assert!(spec.description.contains("AgentWait") || spec.description.contains("Agent Wait"));
    }
    assert!(
        specs.iter().all(|spec| spec.name != "agent_wait_state"),
        "App-only Wait polling must stay hidden from the ordinary model-visible registry"
    );
}
