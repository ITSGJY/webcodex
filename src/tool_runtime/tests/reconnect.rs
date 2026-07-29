//! Cross-process continuity tests: runner disconnect/reconnect, server
//! restart (durable session ledger + process-local binding loss), stale
//! registration semantics, meaningful-activity scoping, and mixed-version
//! diagnostics.

use super::support::*;
use crate::auth::AuthContext;
use crate::client_window::ClientWindow;
use crate::shell_protocol::{
    AgentBuildInfo, ShellClientCapabilities, ShellClientRegisterRequest, ShellJobOpRequest,
};
use crate::tool_runtime::tool_inputs::{SessionMode, StartupDetail};
use crate::tool_runtime::{ToolCall, ToolRuntime};
use serde_json::{json, Value};

fn register_request(
    client_id: &str,
    instance: &str,
    process_started_at: Option<i64>,
    build: Option<AgentBuildInfo>,
    protocol: &str,
) -> ShellClientRegisterRequest {
    ShellClientRegisterRequest {
        client_id: client_id.to_string(),
        agent_instance_id: instance.to_string(),
        display_name: None,
        owner: None,
        hostname: None,
        capabilities: Some(ShellClientCapabilities {
            shell: true,
            git: true,
            file_read: true,
            file_write: true,
            jobs: true,
            async_jobs: true,
            async_shell_jobs: true,
            ..Default::default()
        }),
        projects: Some(vec![registered_project("proj", "/tmp/reconnect-proj")]),
        agent_protocol_version: Some(protocol.to_string()),
        policy: None,
        process_started_at,
        build,
    }
}

async fn layers(runtime: &ToolRuntime) -> Value {
    let status = runtime.runtime_status(None).await;
    assert!(status.success);
    status.output["connection_layers"].clone()
}

fn assert_layer_contract(layer: &Value, context: &str) {
    for field in [
        "status",
        "observed_at",
        "source",
        "age_secs",
        "stale_after_secs",
        "reason_code",
    ] {
        assert!(
            layer.get(field).is_some(),
            "{context} layer missing contract field {field}: {layer}"
        );
    }
}

#[tokio::test]
async fn runner_disconnect_and_reconnect_change_layers_independently() {
    let runtime = test_runtime();
    runtime
        .shell_clients
        .register(register_request(
            "rc-agent",
            "inst-a",
            Some(1_000),
            None,
            "polling-v1",
        ))
        .await
        .unwrap();

    // Connected: every runner-derived layer is a real observation.
    let connected = layers(&runtime).await;
    for name in [
        "runner_process",
        "server_transport",
        "server_registration",
        "project_registry",
        "connector_endpoint",
        "session_binding",
        "last_successful_tool_call",
    ] {
        assert_layer_contract(&connected[name], name);
    }
    assert_eq!(connected["runner_process"]["status"], "ready");
    assert_eq!(
        connected["runner_process"]["source"],
        "runner_process_report"
    );
    assert_eq!(connected["runner_process"]["process_started_at"], 1_000);
    assert_eq!(connected["server_transport"]["status"], "connected");
    assert_eq!(
        connected["server_transport"]["connection_instance"],
        "inst-a"
    );
    assert_eq!(connected["server_registration"]["status"], "registered");
    assert_eq!(connected["project_registry"]["status"], "registered");
    // Connector runtime is not configured in this process.
    assert_eq!(connected["connector_endpoint"]["status"], "not_configured");
    assert_eq!(
        connected["connector_endpoint"]["reason_code"],
        "connector_runtime_disabled"
    );

    // Disconnect: layers change independently; stale registration is not ready.
    runtime
        .shell_clients
        .reconcile_disconnect("rc-agent", "inst-a")
        .await;
    let disconnected = layers(&runtime).await;
    assert_eq!(disconnected["runner_process"]["status"], "stale");
    assert_eq!(
        disconnected["runner_process"]["reason_code"],
        "heartbeat_expired"
    );
    assert_eq!(disconnected["server_transport"]["status"], "disconnected");
    assert!(disconnected["server_transport"]["disconnected_at"].is_i64());
    assert_eq!(disconnected["server_registration"]["status"], "stale");
    assert_eq!(
        disconnected["server_registration"]["reason_code"],
        "registration_instance_disconnected"
    );
    assert_eq!(disconnected["project_registry"]["status"], "stale");
    assert_eq!(
        disconnected["project_registry"]["reason_code"],
        "providing_runner_disconnected"
    );

    // Reconnect with a NEW process instance: new connection replaces the old
    // state, the project re-registers, and no server restart was needed.
    runtime
        .shell_clients
        .register(register_request(
            "rc-agent",
            "inst-b",
            Some(2_000),
            None,
            "polling-v1",
        ))
        .await
        .unwrap();
    let reconnected = layers(&runtime).await;
    assert_eq!(reconnected["runner_process"]["status"], "ready");
    assert_eq!(reconnected["runner_process"]["process_started_at"], 2_000);
    assert_eq!(
        reconnected["server_transport"]["connection_instance"],
        "inst-b"
    );
    assert_eq!(reconnected["server_transport"]["status"], "connected");
    assert_eq!(reconnected["server_registration"]["status"], "registered");
    assert_eq!(
        reconnected["server_registration"]["runner_instance"],
        "inst-b"
    );
    assert_eq!(reconnected["project_registry"]["status"], "registered");

    // Calls recover: a dispatched shell tool reaches the new instance.
    let project = crate::tool_runtime::agent_project_runtime_id("rc-agent", "proj");
    let run = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            let bootstrap = auth_context(None, true);
            runtime
                .dispatch_with_auth(
                    ToolCall::RunShell {
                        project,
                        command: "echo back".to_string(),
                        session_id: None,
                        timeout_secs: Some(5),
                        cwd: None,
                        purpose: None,
                        shell: None,
                    },
                    Some(&bootstrap),
                )
                .await
        }
    });
    let req = next_agent_request_for_instance(&runtime, "rc-agent", "inst-b")
        .await
        .expect("new instance receives work after reconnect");
    complete_patch_agent_request_for_instance(
        &runtime,
        "rc-agent",
        "inst-b",
        &req.request_id,
        0,
        "back\n",
        "",
    )
    .await;
    let response = run.await.unwrap();
    assert!(response.success, "{:?}", response.error);
}

#[tokio::test]
async fn stale_heartbeat_without_disconnect_is_not_ready() {
    let runtime = test_runtime();
    runtime
        .shell_clients
        .register(register_request(
            "stale-agent",
            "inst-a",
            None,
            None,
            "polling-v1",
        ))
        .await
        .unwrap();
    runtime
        .shell_clients
        .set_last_seen_for_test("stale-agent", chrono::Utc::now().timestamp() - 3600)
        .await;
    let stale = layers(&runtime).await;
    assert_eq!(stale["runner_process"]["status"], "stale");
    assert_eq!(stale["server_registration"]["status"], "stale");
    assert_eq!(stale["project_registry"]["status"], "stale");
    // Stale must never be projected as ready/connected.
    assert_ne!(stale["server_transport"]["status"], "connected");
}

#[tokio::test]
async fn no_runner_layers_are_not_observed_with_reason_codes() {
    let runtime = test_runtime();
    let empty = layers(&runtime).await;
    assert_eq!(empty["runner_process"]["status"], "not_observed");
    assert_eq!(
        empty["runner_process"]["reason_code"],
        "no_runner_registered"
    );
    assert_eq!(empty["server_transport"]["status"], "not_observed");
    assert_eq!(empty["server_registration"]["status"], "not_observed");
    assert_eq!(empty["project_registry"]["status"], "not_configured");
    assert_eq!(
        empty["session_binding"]["reason_code"],
        "binding_is_process_local_and_principal_scoped"
    );
    assert_eq!(empty["last_successful_tool_call"]["status"], "not_observed");
    assert_eq!(
        empty["last_successful_tool_call"]["reason_code"],
        "no_meaningful_tool_calls_recorded"
    );
}

#[tokio::test]
async fn meaningful_activity_is_scoped_and_not_refreshed_by_status_calls() {
    let runtime = test_runtime();

    // runtime_status / discovery calls are not meaningful activity.
    for _ in 0..3 {
        let result = runtime
            .dispatch_with_auth(
                ToolCall::RuntimeStatus {
                    compact: false,
                    summary_only: false,
                },
                None,
            )
            .await;
        assert!(result.success);
    }
    let before = layers(&runtime).await;
    assert_eq!(
        before["last_successful_tool_call"]["status"],
        "not_observed"
    );

    // A session start is real work and is recorded with principal scope.
    let result = runtime
        .dispatch_with_auth(
            ToolCall::StartSession {
                project: None,
                title: Some("continuity".to_string()),
                mode: SessionMode::Normal,
                deny_write_tools: false,
                deny_shell_tools: false,
            },
            None,
        )
        .await;
    assert!(result.success);

    let after = layers(&runtime).await;
    let last = &after["last_successful_tool_call"];
    assert_eq!(last["status"], "observed");
    assert_eq!(last["tool"], "start_session");
    assert_eq!(last["scope"], "principal");
    assert_eq!(last["surface"], "api");
    assert!(last["principal_kind"].is_string());
    let observed_at = last["observed_at"].as_i64().unwrap();

    // Additional status polling must not refresh the observation.
    for _ in 0..3 {
        let result = runtime
            .dispatch_with_auth(
                ToolCall::RuntimeStatus {
                    compact: false,
                    summary_only: false,
                },
                None,
            )
            .await;
        assert!(result.success);
    }
    let still = layers(&runtime).await;
    assert_eq!(still["last_successful_tool_call"]["tool"], "start_session");
    assert_eq!(
        still["last_successful_tool_call"]["observed_at"]
            .as_i64()
            .unwrap(),
        observed_at
    );
}

#[tokio::test]
async fn server_restart_keeps_durable_session_and_reports_binding_lost() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = dir.path().join("sessions.json");
    let project_root = dir.path().join("proj");
    std::fs::create_dir_all(&project_root).unwrap();
    init_git_repo(&project_root);

    // "First server process": start a coding task with a current binding.
    let runtime1 = ToolRuntime::new_for_tests().with_session_ledger(&ledger);
    let project =
        register_agent_project_at_path(&runtime1, "restart-agent", "proj", &project_root).await;
    let start = dispatch_start_coding_task_with_local_agent(
        &runtime1,
        "restart-agent",
        ToolCall::StartCodingTask {
            project: project.clone(),
            title: Some("restart continuity".to_string()),
            mode: SessionMode::Normal,
            deny_write_tools: false,
            deny_shell_tools: false,
            detail: StartupDetail::Standard,
            bind_current: true,
            new_session: false,
        },
    )
    .await;
    assert!(start.success, "{:?}", start.error);
    let session_id = start.output["session"]["session_id"]
        .as_str()
        .expect("session id")
        .to_string();
    assert_eq!(start.output["session"]["current_binding"]["bound"], true);
    assert_eq!(
        start.output["connection_state"]["session_binding"]["status"],
        "bound"
    );
    runtime1.sessions.flush_persistence();

    // "Restarted server process": same ledger, fresh in-memory state.
    let runtime2 = ToolRuntime::new_for_tests().with_session_ledger(&ledger);
    register_agent_project_at_path(&runtime2, "restart-agent", "proj", &project_root).await;

    // The durable session is still queryable via the explicit session id.
    let summary = runtime2
        .dispatch_with_auth(
            ToolCall::SessionSummary {
                session_id: session_id.clone(),
                limit: Some(10),
            },
            None,
        )
        .await;
    assert!(summary.success, "{:?}", summary.error);
    assert_eq!(summary.output["session_id"], json!(session_id));
    assert_eq!(summary.output["title"], "restart continuity");
    let restored_instruction = summary.output["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["kind"] == "task_instruction")
        .expect("durable task instruction event");
    assert_eq!(restored_instruction["instruction"], "restart continuity");
    assert_eq!(restored_instruction["requested_mode"], "normal");
    assert_eq!(restored_instruction["context_refreshed"], true);

    // The process-local current binding is absent after restart and never
    // guesses the durable session from the matching window/project alone.
    let window = ClientWindow::for_test("reconnect-window");
    let bootstrap = auth_context(None, true);
    let current = runtime2
        .dispatch_with_auth_transport_options_and_metadata_with_sandbox(
            ToolCall::CurrentSession {
                project: project.clone(),
            },
            Some(&bootstrap),
            crate::tool_runtime::sessions::SessionTransport::Mcp,
            true,
            false,
            Default::default(),
            None,
            Some(&window),
        )
        .await;
    assert!(current.success, "{:?}", current.error);
    assert_eq!(current.output["found"], false);

    // Default startup creates a new exact binding; it does not accidentally
    // attach the durable ledger from before restart.
    let restarted = dispatch_start_coding_task_with_local_agent(
        &runtime2,
        "restart-agent",
        ToolCall::StartCodingTask {
            project: project.clone(),
            title: Some("new post-restart context".to_string()),
            mode: SessionMode::Normal,
            deny_write_tools: false,
            deny_shell_tools: false,
            detail: StartupDetail::Standard,
            bind_current: true,
            new_session: false,
        },
    )
    .await;
    assert!(restarted.success, "{:?}", restarted.error);
    assert_ne!(
        restarted.output["session"]["session_id"].as_str(),
        Some(session_id.as_str()),
        "restart guessed a durable Workflow Session from process-local identity"
    );
    let binding = &restarted.output["connection_state"]["session_binding"];
    assert_eq!(binding["status"], "bound");
    assert_eq!(binding["process_local_in_memory"], true);
    assert_eq!(binding["lost_after_restart"], true);
    assert!(binding["durable_resume"]
        .as_str()
        .unwrap()
        .contains("explicit session_id"));

    // Continuing with the explicit durable session id still works.
    let continued = runtime2
        .dispatch_with_auth(
            ToolCall::SessionSummary {
                session_id,
                limit: Some(5),
            },
            None,
        )
        .await;
    assert!(continued.success, "{:?}", continued.error);
}

#[tokio::test]
async fn agent_job_lost_on_disconnect_stays_terminal_after_reconnect() {
    let runtime = test_runtime();
    runtime
        .shell_clients
        .register(register_request(
            "job-agent",
            "inst-a",
            None,
            None,
            "polling-v1",
        ))
        .await
        .unwrap();

    // Start an async agent job and let the agent pick it up.
    let job = runtime
        .shell_clients
        .start_job(
            ShellJobOpRequest {
                op: "start".to_string(),
                client_id: Some("job-agent".to_string()),
                cwd: None,
                command: Some("sleep 60".to_string()),
                timeout_secs: Some(120),
                job_id: None,
                since_stdout_line: None,
                since_stderr_line: None,
                tail_lines: None,
                limit: None,
                codex: None,
            },
            "reconnect-test".to_string(),
        )
        .await
        .unwrap();
    let _req = next_agent_request_for_instance(&runtime, "job-agent", "inst-a")
        .await
        .expect("job request dispatched");

    // Transport drops mid-job: job authority is not silently completed.
    runtime
        .shell_clients
        .reconcile_disconnect("job-agent", "inst-a")
        .await;
    let jobs = runtime.shell_clients.list_jobs(None).await;
    let lost = jobs
        .iter()
        .find(|info| info.job_id == job.job_id)
        .expect("job still queryable after disconnect");
    assert_eq!(lost.status, "lost");
    let first_ended_at = lost.ended_at.expect("lost job has terminal timestamp");

    // Reconnect with a new instance: the terminal state must not be
    // resurrected or duplicated.
    runtime
        .shell_clients
        .register(register_request(
            "job-agent",
            "inst-b",
            None,
            None,
            "polling-v1",
        ))
        .await
        .unwrap();
    let jobs = runtime.shell_clients.list_jobs(None).await;
    let still_lost = jobs
        .iter()
        .find(|info| info.job_id == job.job_id)
        .expect("job still queryable after reconnect");
    assert_eq!(still_lost.status, "lost");
    assert_eq!(still_lost.ended_at, Some(first_ended_at));
}

#[tokio::test]
async fn version_compatibility_reports_stable_mismatch_facts() {
    let runtime = test_runtime();
    let server_version = env!("CARGO_PKG_VERSION");

    // Matching build + supported protocol → compatible.
    runtime
        .shell_clients
        .register(register_request(
            "same-build",
            "inst-1",
            None,
            Some(AgentBuildInfo {
                version: Some(server_version.to_string()),
                git_commit: Some("abc123".to_string()),
            }),
            "polling-v1",
        ))
        .await
        .unwrap();
    // Different build version → version_mismatch (connected ≠ compatible).
    runtime
        .shell_clients
        .register(register_request(
            "old-build",
            "inst-2",
            None,
            Some(AgentBuildInfo {
                version: Some("0.0.1".to_string()),
                git_commit: None,
            }),
            "websocket-v1",
        ))
        .await
        .unwrap();
    // Unrecognized protocol → capability_mismatch.
    runtime
        .shell_clients
        .register(register_request(
            "legacy",
            "inst-3",
            None,
            None,
            "prehistoric-v0",
        ))
        .await
        .unwrap();

    let status = runtime.runtime_status(None).await;
    assert!(status.success);
    let compat = &status.output["version_compatibility"];
    assert_eq!(compat["status"], "capability_mismatch");
    assert_eq!(compat["server"]["version"], server_version);
    let runners = compat["runners"].as_array().unwrap();
    let by_id = |id: &str| {
        runners
            .iter()
            .find(|runner| runner["client_id"] == id)
            .unwrap_or_else(|| panic!("runner {id} missing"))
    };
    assert_eq!(by_id("same-build")["status"], "compatible");
    assert_eq!(by_id("old-build")["status"], "version_mismatch");
    assert_eq!(
        by_id("old-build")["reason_code"],
        "runner_build_differs_from_server"
    );
    assert!(by_id("old-build")["action"]
        .as_str()
        .unwrap()
        .contains("align"));
    assert_eq!(by_id("legacy")["status"], "capability_mismatch");
    assert_eq!(
        by_id("legacy")["reason_code"],
        "agent_protocol_version_unsupported"
    );
    // No secrets/paths in the diagnostics.
    let text = compat.to_string().to_lowercase();
    assert!(!text.contains("token"));
    assert!(!text.contains("/root/"));
}

/// Drive a `start_coding_task` dispatch while servicing the fake agent's git
/// inspection requests locally.
pub(in crate::tool_runtime::tests) async fn dispatch_start_coding_task_with_local_agent(
    runtime: &ToolRuntime,
    client_id: &str,
    call: ToolCall,
) -> crate::tool_runtime::ToolResult {
    let bootstrap = auth_context(None, true);
    dispatch_start_coding_task_in_window(
        runtime,
        client_id,
        call,
        Some(&bootstrap),
        "reconnect-window",
    )
    .await
}

pub(in crate::tool_runtime::tests) async fn dispatch_start_coding_task_in_window(
    runtime: &ToolRuntime,
    client_id: &str,
    call: ToolCall,
    auth: Option<&AuthContext>,
    window_id: &str,
) -> crate::tool_runtime::ToolResult {
    let task = tokio::spawn({
        let runtime = runtime.clone();
        let auth = auth.cloned();
        let window_id = window_id.to_string();
        async move {
            let window = ClientWindow::for_test(&window_id);
            runtime
                .dispatch_with_auth_transport_options_and_metadata_with_sandbox(
                    call,
                    auth.as_ref(),
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
    while !task.is_finished() {
        if let Some(req) = runtime
            .shell_clients
            .poll(crate::shell_protocol::ShellAgentPollRequest {
                client_id: client_id.to_string(),
                agent_instance_id: "inst".to_string(),
                projects: None,
            })
            .await
            .unwrap()
        {
            let (exit_code, stdout, stderr) = run_agent_shell_request_locally(&req);
            complete_patch_agent_request(
                runtime,
                client_id,
                &req.request_id,
                exit_code,
                &stdout,
                &stderr,
            )
            .await;
        } else {
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    }
    task.await.unwrap()
}

fn coding_start_call(
    project: &str,
    instruction: &str,
    mode: SessionMode,
    new_session: bool,
) -> ToolCall {
    ToolCall::StartCodingTask {
        project: project.to_string(),
        title: Some(instruction.to_string()),
        mode,
        deny_write_tools: false,
        deny_shell_tools: false,
        detail: StartupDetail::Standard,
        bind_current: true,
        new_session,
    }
}

#[tokio::test]
async fn start_coding_task_reuses_same_window_project_and_appends_instruction() {
    let dir = tempfile::tempdir().unwrap();
    init_git_repo(dir.path());
    let runtime = ToolRuntime::new_for_tests();
    let project =
        register_agent_project_at_path(&runtime, "continuity-agent", "demo", dir.path()).await;
    let auth = auth_context(None, true);

    let first = dispatch_start_coding_task_in_window(
        &runtime,
        "continuity-agent",
        coding_start_call(&project, "root objective", SessionMode::Normal, false),
        Some(&auth),
        "continuity-window",
    )
    .await;
    let second = dispatch_start_coding_task_in_window(
        &runtime,
        "continuity-agent",
        coding_start_call(&project, "follow-up objective", SessionMode::Normal, false),
        Some(&auth),
        "continuity-window",
    )
    .await;

    assert!(first.success, "{:?}", first.error);
    assert!(second.success, "{:?}", second.error);
    let first_id = first.output["session"]["session_id"].as_str().unwrap();
    let second_id = second.output["session"]["session_id"].as_str().unwrap();
    assert_eq!(second_id, first_id);
    assert_eq!(second.output["session"]["continuation"], "continued");
    assert_eq!(
        runtime
            .sessions
            .active_session_count_for_test(Some(&project)),
        1
    );
    let summary = runtime.sessions.summary(first_id, Some(20)).unwrap();
    assert_eq!(summary.title.as_deref(), Some("root objective"));
    let instructions: Vec<_> = summary
        .events
        .iter()
        .filter(|event| event.kind == "task_instruction")
        .collect();
    assert_eq!(instructions.len(), 2);
    assert_eq!(
        instructions[1].instruction.as_deref(),
        Some("follow-up objective")
    );
    assert_eq!(instructions[1].requested_mode.as_deref(), Some("normal"));
    assert_eq!(instructions[1].capability_changed, Some(false));
    assert_eq!(instructions[1].context_refreshed, Some(true));
}

#[tokio::test]
async fn start_coding_task_explicit_new_session_preserves_previous_session() {
    let dir = tempfile::tempdir().unwrap();
    init_git_repo(dir.path());
    let runtime = ToolRuntime::new_for_tests();
    let project =
        register_agent_project_at_path(&runtime, "isolation-agent", "demo", dir.path()).await;
    let auth = auth_context(None, true);
    let first = dispatch_start_coding_task_in_window(
        &runtime,
        "isolation-agent",
        coding_start_call(&project, "first root", SessionMode::Normal, false),
        Some(&auth),
        "isolation-window",
    )
    .await;
    let isolated = dispatch_start_coding_task_in_window(
        &runtime,
        "isolation-agent",
        coding_start_call(&project, "isolated root", SessionMode::Normal, true),
        Some(&auth),
        "isolation-window",
    )
    .await;
    assert!(first.success && isolated.success);
    let first_id = first.output["session"]["session_id"].as_str().unwrap();
    let isolated_id = isolated.output["session"]["session_id"].as_str().unwrap();
    assert_ne!(first_id, isolated_id);
    assert_eq!(
        runtime
            .sessions
            .active_session_count_for_test(Some(&project)),
        2
    );
    let old = runtime.sessions.summary(first_id, Some(10)).unwrap();
    assert_eq!(old.title.as_deref(), Some("first root"));
    assert_eq!(
        old.lifecycle,
        crate::tool_runtime::sessions::SessionLifecycle::Active
    );
}

#[tokio::test]
async fn start_coding_task_bindings_are_isolated_by_window() {
    let dir = tempfile::tempdir().unwrap();
    init_git_repo(dir.path());
    let runtime = ToolRuntime::new_for_tests();
    let project =
        register_agent_project_at_path(&runtime, "window-agent", "demo", dir.path()).await;
    let auth = auth_context(None, true);
    let first_a = dispatch_start_coding_task_in_window(
        &runtime,
        "window-agent",
        coding_start_call(&project, "window A", SessionMode::Normal, false),
        Some(&auth),
        "window-a",
    )
    .await;
    let first_b = dispatch_start_coding_task_in_window(
        &runtime,
        "window-agent",
        coding_start_call(&project, "window B", SessionMode::Normal, false),
        Some(&auth),
        "window-b",
    )
    .await;
    let again_a = dispatch_start_coding_task_in_window(
        &runtime,
        "window-agent",
        coding_start_call(&project, "window A again", SessionMode::Normal, false),
        Some(&auth),
        "window-a",
    )
    .await;
    assert!(first_a.success && first_b.success && again_a.success);
    assert_ne!(
        first_a.output["session"]["session_id"],
        first_b.output["session"]["session_id"]
    );
    assert_eq!(
        first_a.output["session"]["session_id"],
        again_a.output["session"]["session_id"]
    );
}

#[tokio::test]
async fn start_coding_task_switches_projects_and_restores_each_binding() {
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    init_git_repo(dir_a.path());
    init_git_repo(dir_b.path());
    let runtime = ToolRuntime::new_for_tests();
    let project_a =
        register_agent_project_at_path(&runtime, "project-a-agent", "a", dir_a.path()).await;
    let project_b =
        register_agent_project_at_path(&runtime, "project-b-agent", "b", dir_b.path()).await;
    let auth = auth_context(None, true);

    let first_a = dispatch_start_coding_task_in_window(
        &runtime,
        "project-a-agent",
        coding_start_call(&project_a, "work on A", SessionMode::Normal, false),
        Some(&auth),
        "project-switch-window",
    )
    .await;
    let first_b = dispatch_start_coding_task_in_window(
        &runtime,
        "project-b-agent",
        coding_start_call(&project_b, "work on B", SessionMode::Normal, false),
        Some(&auth),
        "project-switch-window",
    )
    .await;
    let again_a = dispatch_start_coding_task_in_window(
        &runtime,
        "project-a-agent",
        coding_start_call(&project_a, "return to A", SessionMode::Normal, false),
        Some(&auth),
        "project-switch-window",
    )
    .await;
    assert!(first_a.success && first_b.success && again_a.success);
    assert_ne!(
        first_a.output["session"]["session_id"],
        first_b.output["session"]["session_id"]
    );
    assert_eq!(
        first_a.output["session"]["session_id"],
        again_a.output["session"]["session_id"]
    );
}

#[tokio::test]
async fn failed_project_start_does_not_pollute_full_runtime_binding() {
    let dir = tempfile::tempdir().unwrap();
    init_git_repo(dir.path());
    let runtime = ToolRuntime::new_for_tests();
    let project = register_agent_project_at_path(&runtime, "stable-agent", "a", dir.path()).await;
    let auth = auth_context(None, true);
    let first = dispatch_start_coding_task_in_window(
        &runtime,
        "stable-agent",
        coding_start_call(&project, "stable A", SessionMode::Normal, false),
        Some(&auth),
        "failed-switch-window",
    )
    .await;
    assert!(first.success);

    let failed = dispatch_start_coding_task_in_window(
        &runtime,
        "stable-agent",
        coding_start_call(
            "agent:missing-agent:b",
            "failed B",
            SessionMode::Normal,
            false,
        ),
        Some(&auth),
        "failed-switch-window",
    )
    .await;
    assert!(!failed.success);
    assert_eq!(failed.output["error_kind"], "unknown_project");

    let again = dispatch_start_coding_task_in_window(
        &runtime,
        "stable-agent",
        coding_start_call(&project, "continue A", SessionMode::Normal, false),
        Some(&auth),
        "failed-switch-window",
    )
    .await;
    assert!(again.success);
    assert_eq!(
        first.output["session"]["session_id"],
        again.output["session"]["session_id"]
    );
}

#[tokio::test]
async fn start_coding_task_mode_upgrade_is_atomic_and_permission_checked() {
    let dir = tempfile::tempdir().unwrap();
    init_git_repo(dir.path());
    let runtime = ToolRuntime::new_for_tests();
    let read_auth = oauth_bridge_auth_context(
        "continuity-subject",
        &[
            crate::auth::SCOPE_RUNTIME_READ,
            crate::auth::SCOPE_PROJECT_READ,
        ],
    );
    let write_auth = oauth_bridge_auth_context(
        "continuity-subject",
        &[
            crate::auth::SCOPE_RUNTIME_READ,
            crate::auth::SCOPE_PROJECT_READ,
            crate::auth::SCOPE_PROJECT_WRITE,
        ],
    );
    let project = register_agent_project_at_path_with_auth(
        &runtime,
        "oauth-client",
        "demo",
        dir.path(),
        &read_auth,
    )
    .await;
    let inspected = dispatch_start_coding_task_in_window(
        &runtime,
        "oauth-client",
        coding_start_call(&project, "inspect first", SessionMode::ReadOnly, false),
        Some(&read_auth),
        "upgrade-window",
    )
    .await;
    assert!(inspected.success, "{:?}", inspected.error);
    let session_id = inspected.output["session"]["session_id"]
        .as_str()
        .unwrap()
        .to_string();

    let denied = dispatch_start_coding_task_in_window(
        &runtime,
        "oauth-client",
        coding_start_call(&project, "request writes", SessionMode::Normal, false),
        Some(&read_auth),
        "upgrade-window",
    )
    .await;
    assert!(!denied.success);
    assert_eq!(
        denied.output["error_kind"],
        "session_capability_upgrade_denied"
    );
    let unchanged = runtime.sessions.summary(&session_id, Some(20)).unwrap();
    assert_eq!(unchanged.mode, SessionMode::ReadOnly);
    assert!(unchanged.guards.deny_write_tools);
    assert!(unchanged.guards.deny_shell_tools);
    assert_eq!(
        unchanged
            .events
            .iter()
            .filter(|event| event.kind == "task_instruction")
            .count(),
        1
    );

    let upgraded = dispatch_start_coding_task_in_window(
        &runtime,
        "oauth-client",
        coding_start_call(&project, "enable writes", SessionMode::Normal, false),
        Some(&write_auth),
        "upgrade-window",
    )
    .await;
    assert!(upgraded.success, "{:?}", upgraded.error);
    assert_eq!(upgraded.output["session"]["session_id"], session_id);
    assert_eq!(upgraded.output["session"]["mode"], "normal");
    assert_eq!(
        upgraded.output["session"]["guards"]["deny_write_tools"],
        false
    );
    assert_eq!(
        upgraded.output["session"]["guards"]["deny_shell_tools"],
        false
    );
    let summary = runtime.sessions.summary(&session_id, Some(20)).unwrap();
    let transition = summary
        .events
        .iter()
        .rev()
        .find(|event| event.kind == "task_instruction")
        .unwrap();
    assert_eq!(transition.previous_mode.as_deref(), Some("read_only"));
    assert_eq!(transition.requested_mode.as_deref(), Some("normal"));
    assert_eq!(transition.capability_changed, Some(true));
}

#[tokio::test]
async fn failed_continuity_commit_leaves_binding_and_ledger_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    init_git_repo(dir.path());
    let runtime = ToolRuntime::new_for_tests();
    let project = register_agent_project_at_path(&runtime, "fault-agent", "demo", dir.path()).await;
    let auth = auth_context(None, true);
    let first = dispatch_start_coding_task_in_window(
        &runtime,
        "fault-agent",
        coding_start_call(&project, "stable root", SessionMode::ReadOnly, false),
        Some(&auth),
        "fault-window",
    )
    .await;
    assert!(first.success);
    let session_id = first.output["session"]["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    runtime
        .sessions
        .fail_next_coding_continuity_commit_for_test();
    let failed = dispatch_start_coding_task_in_window(
        &runtime,
        "fault-agent",
        coding_start_call(&project, "must roll back", SessionMode::Normal, false),
        Some(&auth),
        "fault-window",
    )
    .await;
    assert!(!failed.success);
    assert_eq!(
        failed.output["error_kind"],
        "coding_continuity_commit_failed"
    );
    let unchanged = runtime.sessions.summary(&session_id, Some(20)).unwrap();
    assert_eq!(unchanged.mode, SessionMode::ReadOnly);
    assert_eq!(
        unchanged
            .events
            .iter()
            .filter(|event| event.kind == "task_instruction")
            .count(),
        1
    );

    let retried = dispatch_start_coding_task_in_window(
        &runtime,
        "fault-agent",
        coding_start_call(&project, "must roll back", SessionMode::Normal, false),
        Some(&auth),
        "fault-window",
    )
    .await;
    assert!(retried.success, "{:?}", retried.error);
    assert_eq!(retried.output["session"]["session_id"], session_id);
    let summary = runtime.sessions.summary(&session_id, Some(20)).unwrap();
    assert_eq!(
        summary
            .events
            .iter()
            .filter(|event| event.instruction.as_deref() == Some("must roll back"))
            .count(),
        1
    );
}

#[tokio::test]
async fn changed_repository_root_does_not_reuse_project_id_binding() {
    let first_root = tempfile::tempdir().unwrap();
    let second_root = tempfile::tempdir().unwrap();
    init_git_repo(first_root.path());
    init_git_repo(second_root.path());
    let runtime = ToolRuntime::new_for_tests();
    let project =
        register_agent_project_at_path(&runtime, "moving-agent", "demo", first_root.path()).await;
    let auth = auth_context(None, true);
    let first = dispatch_start_coding_task_in_window(
        &runtime,
        "moving-agent",
        coding_start_call(&project, "first root", SessionMode::Normal, false),
        Some(&auth),
        "moving-window",
    )
    .await;
    register_agent_project_at_path(&runtime, "moving-agent", "demo", second_root.path()).await;
    let moved = dispatch_start_coding_task_in_window(
        &runtime,
        "moving-agent",
        coding_start_call(&project, "second root", SessionMode::Normal, false),
        Some(&auth),
        "moving-window",
    )
    .await;
    assert!(first.success && moved.success);
    assert_ne!(
        first.output["session"]["session_id"],
        moved.output["session"]["session_id"]
    );
    assert_eq!(moved.output["session"]["continuation"], "created");
}
