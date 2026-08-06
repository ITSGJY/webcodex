use super::super::experimental::{
    schema_hash_hex, validate_against_schema, EXPERIMENTAL_KIND_CALL, EXPERIMENTAL_KIND_DESCRIBE,
    EXPERIMENTAL_KIND_LIST,
};
use super::*;

fn experimental_router(fixture: &Fixture) -> ExternalToolRouter {
    // Experimental kinds ignore production strategy; native keeps default path safe.
    ExternalToolRouter::new(&ToolProvidersConfig {
        strategy: ToolProviderStrategy::Native,
        claude_code: fixture.config.clone(),
    })
}

fn experimental_request(kind: &str, root: &Path, payload: Option<Value>) -> ShellAgentShellRequest {
    agent_request(kind, root, ".", payload)
}

fn experimental_stdout(router: &ExternalToolRouter, request: ShellAgentShellRequest) -> Value {
    let ExternalRoute::Handled(result) = router.route(&permissive_test_policy(), &request) else {
        panic!("experimental request left the experimental path");
    };
    serde_json::from_str(result.stdout.as_deref().unwrap()).unwrap()
}

fn call_payload(tool_name: &str, arguments: Value) -> Option<Value> {
    Some(json!({"tool_name": tool_name, "arguments": arguments}))
}

#[test]
fn experimental_invalid_payload_is_rejected_before_claude_start() {
    let _serial = serialize_fake_mcp_test();
    let fixture = Fixture::new("normal");
    let router = experimental_router(&fixture);

    let mut malformed = experimental_request(EXPERIMENTAL_KIND_CALL, &fixture.root, None);
    malformed.content = Some("{".to_string());
    let malformed = experimental_stdout(&router, malformed);
    assert_eq!(malformed["code"], "provider_invalid_request");
    assert_eq!(malformed["write_state"], "not_submitted");
    assert_eq!(fixture.starts(), 0);
    let last_call = router.status().claude_code.last_call.clone().unwrap();
    assert_eq!(last_call.capability, EXPERIMENTAL_KIND_CALL);
    assert_eq!(
        last_call.error_code.as_deref(),
        Some("provider_invalid_request")
    );

    let missing = experimental_stdout(
        &router,
        experimental_request(EXPERIMENTAL_KIND_DESCRIBE, &fixture.root, Some(json!({}))),
    );
    assert_eq!(missing["code"], "provider_invalid_request");
    assert_eq!(missing["write_state"], "not_submitted");
    assert_eq!(fixture.starts(), 0);
    let last_call = router.status().claude_code.last_call.unwrap();
    assert_eq!(last_call.capability, EXPERIMENTAL_KIND_DESCRIBE);
    assert_eq!(
        last_call.error_code.as_deref(),
        Some("provider_invalid_request")
    );
}

#[test]
fn experimental_list_describe_and_fixed_allowlist() {
    let _serial = serialize_fake_mcp_test();
    let fixture = Fixture::new("normal");
    let router = experimental_router(&fixture);
    assert_eq!(
        fixture.starts(),
        0,
        "constructing a generation router must not start Claude"
    );
    let listed = experimental_stdout(
        &router,
        experimental_request(EXPERIMENTAL_KIND_LIST, &fixture.root, None),
    );
    assert_eq!(listed["experimental"], true);
    assert_eq!(listed["claude_version"], "Claude Fake 1.2.3");
    assert_eq!(listed["process_reused"], false);
    assert_eq!(listed["truncated"], false);
    let names = listed["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert!(names.windows(2).all(|pair| pair[0] <= pair[1]));
    for expected in [
        "Bash",
        "Edit",
        "Read",
        "TaskCreate",
        "Write",
        "fake_edit",
        "fake_search",
    ] {
        assert!(
            names.iter().any(|name| name == expected),
            "missing {expected}"
        );
    }
    for tool in listed["tools"].as_array().unwrap() {
        let name = tool["name"].as_str().unwrap();
        assert_eq!(tool["schema_hash"].as_str().unwrap().len(), 64);
        assert_eq!(tool["schema_available"], true);
        let expected_callable = matches!(name, "Read" | "Edit" | "Write" | "Bash");
        assert_eq!(
            tool["callable"], expected_callable,
            "callable mismatch for {name}"
        );
    }
    assert!(!serde_json::to_string(&listed)
        .unwrap()
        .contains(&fixture.config.command));

    let described = experimental_stdout(
        &router,
        experimental_request(
            EXPERIMENTAL_KIND_DESCRIBE,
            &fixture.root,
            Some(json!({"tool_name": "Bash"})),
        ),
    );
    assert_eq!(described["tool_name"], "Bash");
    assert_eq!(described["callable"], true);
    assert_eq!(described["schema_hash"].as_str().unwrap().len(), 64);
    assert!(described["input_schema"]["properties"]["command"].is_object());
    assert!(described["description"].as_str().unwrap().contains("shell"));

    let missing = experimental_stdout(
        &router,
        experimental_request(
            EXPERIMENTAL_KIND_DESCRIBE,
            &fixture.root,
            Some(json!({"tool_name": "Agent"})),
        ),
    );
    assert_eq!(missing["code"], "claude_tool_not_found");
    assert_eq!(missing["write_state"], "not_submitted");

    let task = experimental_stdout(
        &router,
        experimental_request(
            EXPERIMENTAL_KIND_DESCRIBE,
            &fixture.root,
            Some(json!({"tool_name": "TaskCreate"})),
        ),
    );
    assert_eq!(task["callable"], false);
    assert!(task["input_schema"]["properties"]["subject"].is_object());

    let blocked = experimental_stdout(
        &router,
        experimental_request(
            EXPERIMENTAL_KIND_CALL,
            &fixture.root,
            call_payload("TaskCreate", json!({"subject": "blocked"})),
        ),
    );
    assert_eq!(blocked["code"], "claude_tool_not_allowed");
    assert_eq!(blocked["write_state"], "not_submitted");
    assert_eq!(blocked["changed"], false);
    let marker = fs::read_to_string(&fixture.marker).unwrap_or_default();
    assert!(
        !marker
            .lines()
            .any(|line| line.contains("tools/call") && line.contains("TaskCreate")),
        "fake MCP must not receive tools/call for TaskCreate: {marker}"
    );

    let unknown = experimental_stdout(
        &router,
        experimental_request(
            EXPERIMENTAL_KIND_CALL,
            &fixture.root,
            call_payload("Agent", json!({})),
        ),
    );
    assert_eq!(unknown["code"], "claude_tool_not_found");
}

#[test]
fn experimental_arguments_validation_and_schema_hash_are_stable() {
    let _serial = serialize_fake_mcp_test();
    let fixture = Fixture::new("normal");
    let router = experimental_router(&fixture);
    let invalid = experimental_stdout(
        &router,
        experimental_request(
            EXPERIMENTAL_KIND_CALL,
            &fixture.root,
            call_payload("Read", json!({"not_file_path": true})),
        ),
    );
    assert_eq!(invalid["code"], "claude_arguments_invalid");
    assert_eq!(invalid["write_state"], "not_submitted");
    assert_eq!(invalid["changed"], false);

    let schema = json!({
        "type": "object",
        "properties": {
            "b": {"type": "string"},
            "a": {"type": "integer"}
        },
        "required": ["a"]
    });
    let reordered = json!({
        "required": ["a"],
        "type": "object",
        "properties": {
            "a": {"type": "integer"},
            "b": {"type": "string"}
        }
    });
    assert_eq!(schema_hash_hex(&schema), schema_hash_hex(&reordered));
    assert!(validate_against_schema(&schema, &json!({"a": 1})).is_ok());
    assert!(validate_against_schema(&schema, &json!({"b": "x"})).is_err());
}

#[test]
fn experimental_read_edit_write_bash_reuse_and_tool_level_is_error() {
    let _serial = serialize_fake_mcp_test();
    let fixture = Fixture::new("normal");
    let router = experimental_router(&fixture);

    let read = experimental_stdout(
        &router,
        experimental_request(
            EXPERIMENTAL_KIND_CALL,
            &fixture.root,
            call_payload("Read", json!({"file_path": "src/lib.rs"})),
        ),
    );
    assert_eq!(read["is_error"], false);
    assert_eq!(read["tool_status"], "success");
    assert_eq!(read["process_reused"], false);
    assert!(read["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("needle"));
    assert_eq!(fixture.starts(), 1);

    let edit = experimental_stdout(
        &router,
        experimental_request(
            EXPERIMENTAL_KIND_CALL,
            &fixture.root,
            call_payload(
                "Edit",
                json!({
                    "file_path": "edit.txt",
                    "old_string": "before",
                    "new_string": "after"
                }),
            ),
        ),
    );
    assert_eq!(edit["tool_status"], "success");
    assert_eq!(edit["process_reused"], true);
    assert_eq!(
        fs::read_to_string(fixture.root.join("edit.txt")).unwrap(),
        "after\n"
    );
    fs::write(fixture.root.join("edit.txt"), "before\n").unwrap();

    let write = experimental_stdout(
        &router,
        experimental_request(
            EXPERIMENTAL_KIND_CALL,
            &fixture.root,
            call_payload(
                "Write",
                json!({
                    "file_path": "tmp-exp.txt",
                    "content": "hello-experimental"
                }),
            ),
        ),
    );
    assert_eq!(write["tool_status"], "success");
    assert_eq!(
        fs::read_to_string(fixture.root.join("tmp-exp.txt")).unwrap(),
        "hello-experimental"
    );
    let _ = fs::remove_file(fixture.root.join("tmp-exp.txt"));

    let bash_ok = experimental_stdout(
        &router,
        experimental_request(
            EXPERIMENTAL_KIND_CALL,
            &fixture.root,
            call_payload("Bash", json!({"command": "printf hi"})),
        ),
    );
    assert_eq!(bash_ok["tool_status"], "success");
    assert!(bash_ok["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("printf hi"));
    let ok_call = router.status().claude_code.last_call.clone().unwrap();
    assert_eq!(ok_call.result, "success");
    assert_eq!(ok_call.error_code, None);

    let bash_err = experimental_stdout(
        &router,
        experimental_request(
            EXPERIMENTAL_KIND_CALL,
            &fixture.root,
            call_payload("Bash", json!({"command": "nonzero"})),
        ),
    );
    // Transport succeeded; raw Claude result is preserved (not a provider envelope).
    assert!(bash_err.get("code").is_none() || bash_err["code"].is_null());
    assert_eq!(bash_err["tool_status"], "failure");
    assert_eq!(bash_err["is_error"], true);
    assert_eq!(bash_err["result"]["isError"], true);
    assert!(bash_err["result"]["content"].is_array());
    // Mutating tool + isError: tools/call completed → post-send write-state.
    assert_eq!(bash_err["write_state"], "uncertain");
    assert!(bash_err["changed"].is_null());
    let last_call = router.status().claude_code.last_call.unwrap();
    assert_eq!(last_call.result, "failure");
    assert_eq!(
        last_call.error_code.as_deref(),
        Some("claude_tool_error"),
        "tool-level isError must record runtime failure without clearing error code"
    );
    assert_eq!(last_call.write_state.as_deref(), Some("uncertain"));
    assert_eq!(fixture.starts(), 1);
}

#[test]
fn experimental_mutating_tool_exit_returns_uncertain_write_state() {
    let _serial = serialize_fake_mcp_test();
    let fixture = Fixture::new("exp_mutate_exit");
    let router = experimental_router(&fixture);
    let target = fixture.root.join("tmp-mutate-exit.txt");
    let result = experimental_stdout(
        &router,
        experimental_request(
            EXPERIMENTAL_KIND_CALL,
            &fixture.root,
            call_payload(
                "Write",
                json!({
                    "file_path": "tmp-mutate-exit.txt",
                    "content": "mutated-before-exit"
                }),
            ),
        ),
    );
    assert_eq!(result["code"], "claude_mcp_process_exited");
    assert_eq!(result["write_state"], "uncertain");
    assert!(result["changed"].is_null());
    assert_eq!(
        fs::read_to_string(&target).unwrap_or_default(),
        "mutated-before-exit",
        "mutating tool may have already changed the file"
    );
    let _ = fs::remove_file(&target);
    let marker = fs::read_to_string(&fixture.marker).unwrap_or_default();
    assert!(
        marker.contains("mutated_then_exit"),
        "expected fake to mutate then exit: {marker}"
    );
}

#[test]
fn experimental_recovers_lazily_after_process_exit() {
    let _serial = serialize_fake_mcp_test();
    let exit = Fixture::new("exit");
    let router = experimental_router(&exit);
    let first = experimental_stdout(
        &router,
        experimental_request(EXPERIMENTAL_KIND_LIST, &exit.root, None),
    );
    assert_eq!(first["experimental"], true);
    // Force a tools/call so the exit scenario ends the child without a response.
    let result = experimental_stdout(
        &router,
        experimental_request(
            EXPERIMENTAL_KIND_CALL,
            &exit.root,
            call_payload("Read", json!({"file_path": "src/lib.rs"})),
        ),
    );
    assert_eq!(result["code"], "claude_mcp_process_exited");
    assert_eq!(result["write_state"], "not_submitted");
    assert!(wait_until(Duration::from_secs(2), || exit.starts() >= 1));
    let second = experimental_stdout(
        &router,
        experimental_request(EXPERIMENTAL_KIND_LIST, &exit.root, None),
    );
    assert_eq!(second["experimental"], true);
    assert!(
        exit.starts() >= 2,
        "expected lazy restart after Claude process exit, starts={}",
        exit.starts()
    );
}

#[test]
fn experimental_discovery_bounds_over_64_tools_and_oversized_schema() {
    let _serial = serialize_fake_mcp_test();
    let many = Fixture::new("exp_many_tools");
    let router = experimental_router(&many);
    let listed = experimental_stdout(
        &router,
        experimental_request(EXPERIMENTAL_KIND_LIST, &many.root, None),
    );
    assert_eq!(
        listed["tools"].as_array().unwrap().len(),
        MAX_EXPERIMENTAL_TOOLS,
        "must keep only {MAX_EXPERIMENTAL_TOOLS} tools"
    );
    assert_eq!(
        listed["truncated"], true,
        "truncated must be true when discovery saw a 65th+ valid tool"
    );

    let fixture = Fixture::new("exp_large_schema");
    let router = experimental_router(&fixture);
    let listed = experimental_stdout(
        &router,
        experimental_request(EXPERIMENTAL_KIND_LIST, &fixture.root, None),
    );
    let large = listed["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "LargeSchemaTool")
        .expect("oversized-schema tool must still be listed by name");
    assert_eq!(large["schema_available"], false);
    assert_eq!(large["callable"], false);
    let hash = large["schema_hash"].as_str().unwrap();
    assert_eq!(hash.len(), 64);

    let described = experimental_stdout(
        &router,
        experimental_request(
            EXPERIMENTAL_KIND_DESCRIBE,
            &fixture.root,
            Some(json!({"tool_name": "LargeSchemaTool"})),
        ),
    );
    assert_eq!(described["schema_hash"], hash);
    assert_eq!(described["truncated"], true);
    assert_eq!(described["input_schema"]["truncated"], true);
    assert_eq!(described["callable"], false);
    assert!(described["input_schema"]["note"]
        .as_str()
        .unwrap()
        .contains("schema exceeded"));

    let call = experimental_stdout(
        &router,
        experimental_request(
            EXPERIMENTAL_KIND_CALL,
            &fixture.root,
            call_payload("LargeSchemaTool", json!({"payload": "x"})),
        ),
    );
    assert_eq!(call["code"], "claude_schema_unavailable");
    assert_ne!(call["code"], "claude_tool_not_found");
    assert_eq!(call["write_state"], "not_submitted");
}

#[test]
fn experimental_result_bounds_soft_truncate_and_hard_fail() {
    let _serial = serialize_fake_mcp_test();
    let soft = Fixture::new("exp_soft_oversized");
    let router = experimental_router(&soft);
    let result = experimental_stdout(
        &router,
        experimental_request(
            EXPERIMENTAL_KIND_CALL,
            &soft.root,
            call_payload("Bash", json!({"command": "big-soft"})),
        ),
    );
    assert_ne!(
        result["code"].as_str(),
        Some("claude_result_too_large"),
        "300 KiB must soft-truncate, not hard-fail: {result}"
    );
    assert_eq!(result["result_truncated"], true);
    assert_eq!(result["result"]["truncated"], true);
    assert_eq!(result["tool_status"], "success");
    assert_eq!(result["is_error"], false);

    let hard = Fixture::new("exp_oversized");
    let router = experimental_router(&hard);
    let result = experimental_stdout(
        &router,
        experimental_request(
            EXPERIMENTAL_KIND_CALL,
            &hard.root,
            call_payload("Bash", json!({"command": "big-hard"})),
        ),
    );
    assert_eq!(
        result["code"], "claude_result_too_large",
        "600 KiB must hard-fail: {result}"
    );
    // tools/call already completed; mutating tool keeps post-send write-state.
    assert_eq!(result["write_state"], "uncertain");
    assert!(result["changed"].is_null());
    let last_call = router.status().claude_code.last_call.unwrap();
    assert_eq!(last_call.result, "failure");
    assert_eq!(last_call.write_state.as_deref(), Some("uncertain"));
    assert_eq!(
        last_call.error_code.as_deref(),
        Some("claude_result_too_large")
    );

    let read = experimental_stdout(
        &router,
        experimental_request(
            EXPERIMENTAL_KIND_CALL,
            &hard.root,
            call_payload("Read", json!({"file_path": "src/lib.rs"})),
        ),
    );
    assert_eq!(
        read["code"], "claude_result_too_large",
        "Read hard oversized must still hard-fail: {read}"
    );
    // Read is read-only even after tools/call completed.
    assert_eq!(read["write_state"], "not_submitted");
    assert_eq!(read["changed"], false);
    let last_call = router.status().claude_code.last_call.unwrap();
    assert_eq!(last_call.result, "failure");
    assert_eq!(last_call.write_state.as_deref(), Some("not_submitted"));
    assert_eq!(
        last_call.error_code.as_deref(),
        Some("claude_result_too_large")
    );
}

#[test]
fn experimental_oversized_mcp_request_is_pre_send_not_submitted() {
    let _serial = serialize_fake_mcp_test();
    let fixture = Fixture::new("normal");
    let router = experimental_router(&fixture);
    // Warm process so marker has only non-tools/call traffic before the huge call.
    let listed = experimental_stdout(
        &router,
        experimental_request(EXPERIMENTAL_KIND_LIST, &fixture.root, None),
    );
    assert_eq!(listed["experimental"], true);

    // Command payload alone exceeds MAX_MCP_MESSAGE_BYTES; encode rejects before stdin write.
    let huge = "x".repeat(MAX_MCP_MESSAGE_BYTES);
    let result = experimental_stdout(
        &router,
        experimental_request(
            EXPERIMENTAL_KIND_CALL,
            &fixture.root,
            call_payload("Bash", json!({"command": huge})),
        ),
    );
    assert_eq!(result["write_state"], "not_submitted");
    assert_eq!(result["changed"], false);
    let marker = fs::read_to_string(&fixture.marker).unwrap_or_default();
    assert!(
        !marker
            .lines()
            .any(|line| line.contains("tools/call") && line.contains("Bash")),
        "oversized tools/call must not be written to Claude stdin: {marker}"
    );
    let last_call = router.status().claude_code.last_call.unwrap();
    assert_eq!(last_call.result, "failure");
    assert_eq!(last_call.write_state.as_deref(), Some("not_submitted"));
}

/// Reload semantics: the in-flight raw mutating call keeps its generation's
/// router (and Claude process) alive; the retired router shuts down after the
/// call completes; a new disabled generation rejects raw calls without
/// spawning; the old generation's completion never rewrites the new
/// generation's runtime state.
#[cfg(unix)]
#[test]
fn experimental_raw_edit_survives_generation_router_retirement() {
    let _serial = serialize_fake_mcp_test();
    let fixture = Fixture::with_timeout("delayed", 2);
    let old = Arc::new(experimental_router(&fixture));
    let weak = Arc::downgrade(&old);
    let request = experimental_request(
        EXPERIMENTAL_KIND_CALL,
        &fixture.root,
        call_payload(
            "Edit",
            json!({
                "file_path": "edit.txt",
                "old_string": "before",
                "new_string": "after"
            }),
        ),
    );
    let worker_router = Arc::clone(&old);
    let worker = std::thread::spawn(move || {
        let ExternalRoute::Handled(result) =
            worker_router.route(&permissive_test_policy(), &request)
        else {
            panic!("raw edit left the experimental path");
        };
        serde_json::from_str::<Value>(result.stdout.as_deref().unwrap()).unwrap()
    });
    assert!(wait_until(Duration::from_secs(1), || {
        fs::read_to_string(&fixture.marker)
            .unwrap_or_default()
            .lines()
            .any(|line| line.contains("tools/call") && line.contains("\"Edit\""))
    }));
    let pid = process_ids(&old.claude)[0];

    // Simulated reload: the new generation's provider is disabled.
    let replacement = ExternalToolRouter::new(&ToolProvidersConfig::default());
    let rejected = experimental_stdout(
        &replacement,
        experimental_request(
            EXPERIMENTAL_KIND_CALL,
            &fixture.root,
            call_payload("Read", json!({"file_path": "src/lib.rs"})),
        ),
    );
    assert_eq!(rejected["code"], "claude_code_unavailable");
    drop(old);
    assert!(
        weak.upgrade().is_some(),
        "in-flight raw edit lost its generation router"
    );
    assert_eq!(unsafe { libc::kill(pid as i32, 0) }, 0);

    let edited = worker.join().unwrap();
    assert_eq!(edited["is_error"], false);
    assert_eq!(
        fs::read_to_string(fixture.root.join("edit.txt")).unwrap(),
        "after\n"
    );
    assert!(wait_until(Duration::from_secs(1), || weak
        .upgrade()
        .is_none()));
    assert!(wait_until(Duration::from_secs(1), || {
        (unsafe { libc::kill(pid as i32, 0) }) == -1
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
    }));
    // The retired generation completed with a success, but the new
    // generation's state still reports its own last call.
    let last_call = replacement.status().claude_code.last_call.unwrap();
    assert_eq!(last_call.capability, EXPERIMENTAL_KIND_CALL);
    assert_eq!(
        last_call.error_code.as_deref(),
        Some("claude_code_unavailable")
    );
    assert_eq!(
        fixture.starts(),
        1,
        "disabled new generation must not spawn Claude"
    );
}

#[test]
fn opt_in_experimental_real_claude_tools_smoke() {
    let _serial = serialize_fake_mcp_test();
    if env::var_os("WEBCODEX_EXPERIMENTAL_CLAUDE_TOOLS").is_none() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/sample.txt"), "alpha\nbeta\n").unwrap();
    fs::write(root.join("edit.txt"), "before\n").unwrap();
    let config = ClaudeCodeMcpConfig {
        enabled: true,
        command: "claude".to_string(),
        args: vec!["mcp".to_string(), "serve".to_string()],
        mapping: HashMap::new(),
        timeout_secs: 45,
    };
    let router = ExternalToolRouter::new(&ToolProvidersConfig {
        strategy: ToolProviderStrategy::Native,
        claude_code: config,
    });
    let listed = experimental_stdout(
        &router,
        experimental_request(EXPERIMENTAL_KIND_LIST, &root, None),
    );
    eprintln!(
        "experimental_real_claude_list version={:?} tools={}",
        listed["claude_version"], listed["tools"]
    );
    for name in ["Read", "Edit", "Write", "Bash"] {
        assert!(
            listed["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| tool["name"] == name),
            "real Claude tools/list missing {name}: {listed}"
        );
        let described = experimental_stdout(
            &router,
            experimental_request(
                EXPERIMENTAL_KIND_DESCRIBE,
                &root,
                Some(json!({"tool_name": name})),
            ),
        );
        eprintln!(
            "experimental_real_claude_describe tool={name} hash={} required={:?}",
            described["schema_hash"], described["input_schema"]["required"]
        );
    }

    let read = experimental_stdout(
        &router,
        experimental_request(
            EXPERIMENTAL_KIND_CALL,
            &root,
            call_payload(
                "Read",
                json!({"file_path": root.join("src/sample.txt").to_string_lossy()}),
            ),
        ),
    );
    assert_eq!(read["is_error"], false, "{read}");
    assert!(format!("{read}").contains("alpha"), "{read}");

    // Claude Code Edit requires a prior Read of the same path in-session.
    let edit_path = root.join("edit.txt");
    let pre_read = experimental_stdout(
        &router,
        experimental_request(
            EXPERIMENTAL_KIND_CALL,
            &root,
            call_payload("Read", json!({"file_path": edit_path.to_string_lossy()})),
        ),
    );
    assert_eq!(pre_read["is_error"], false, "{pre_read}");
    let edit = experimental_stdout(
        &router,
        experimental_request(
            EXPERIMENTAL_KIND_CALL,
            &root,
            call_payload(
                "Edit",
                json!({
                    "file_path": edit_path.to_string_lossy(),
                    "old_string": "before",
                    "new_string": "after"
                }),
            ),
        ),
    );
    assert_eq!(edit["is_error"], false, "{edit}");
    assert_eq!(fs::read_to_string(&edit_path).unwrap(), "after\n");
    fs::write(&edit_path, "before\n").unwrap();

    let write_path = root.join("tmp-real-claude.txt");
    let write = experimental_stdout(
        &router,
        experimental_request(
            EXPERIMENTAL_KIND_CALL,
            &root,
            call_payload(
                "Write",
                json!({
                    "file_path": write_path.to_string_lossy(),
                    "content": "temporary"
                }),
            ),
        ),
    );
    assert_eq!(write["is_error"], false, "{write}");
    assert_eq!(fs::read_to_string(&write_path).unwrap(), "temporary");
    let _ = fs::remove_file(&write_path);

    let bash = experimental_stdout(
        &router,
        experimental_request(
            EXPERIMENTAL_KIND_CALL,
            &root,
            call_payload("Bash", json!({"command": "printf 'ok-from-bash'"})),
        ),
    );
    assert_eq!(bash["is_error"], false, "{bash}");
    assert!(format!("{bash}").contains("ok-from-bash"), "{bash}");

    let bash_fail = experimental_stdout(
        &router,
        experimental_request(
            EXPERIMENTAL_KIND_CALL,
            &root,
            call_payload("Bash", json!({"command": "exit 7"})),
        ),
    );
    // Claude may surface non-zero exits as isError or as structured text.
    eprintln!("experimental_real_claude_bash_nonzero={bash_fail}");

    #[cfg(unix)]
    let process_groups = process_ids(&router.claude);
    router.shutdown();
    #[cfg(unix)]
    for pid in process_groups {
        assert!(
            wait_until(Duration::from_secs(2), || !process_exists(pid)),
            "Claude process {pid} remained after experimental shutdown"
        );
    }
}
