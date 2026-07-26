use super::*;
use crate::shell_protocol::ShellAgentShellRequest;
use std::env;
use std::fs;
use std::process::Command;
use std::sync::{Arc, OnceLock, Weak};
use tempfile::TempDir;

static FAKE_SERVER: OnceLock<Mutex<Weak<FakeBinary>>> = OnceLock::new();

struct FakeBinary {
    _temp: TempDir,
    path: PathBuf,
}

fn fake_binary() -> Arc<FakeBinary> {
    let cache = FAKE_SERVER.get_or_init(|| Mutex::new(Weak::new()));
    let mut cached = cache.lock().unwrap();
    if let Some(binary) = cached.upgrade() {
        return binary;
    }
    let temp = tempfile::tempdir().unwrap();
    let output = temp.path().join(format!(
        "webcodex-claude-mcp-fake{}",
        env::consts::EXE_SUFFIX
    ));
    let source =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/bin/webcodex_agent/fake_claude_mcp.rs");
    let rustc = env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let result = Command::new(rustc)
        .arg("--edition=2021")
        .arg("--crate-name=webcodex_claude_mcp_fake")
        .arg(source)
        .arg("-o")
        .arg(&output)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let binary = Arc::new(FakeBinary {
        _temp: temp,
        path: output,
    });
    *cached = Arc::downgrade(&binary);
    binary
}

struct Fixture {
    provider: ClaudeCodeMcpProvider,
    config: ClaudeCodeMcpConfig,
    _fake: Arc<FakeBinary>,
    _temp: TempDir,
    root: PathBuf,
    marker: PathBuf,
}

impl Fixture {
    fn new(scenario: &str) -> Self {
        Self::with_timeout(scenario, 1)
    }

    fn with_timeout(scenario: &str, timeout_secs: u64) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("project");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("edit.txt"), "before\n").unwrap();
        fs::write(root.join("src/lib.rs"), "zero\nneedle\n").unwrap();
        let marker = temp.path().join("marker.log");
        let fake = fake_binary();
        let config = ClaudeCodeMcpConfig {
            enabled: true,
            command: fake.path.to_string_lossy().to_string(),
            args: vec![scenario.to_string(), marker.to_string_lossy().to_string()],
            mapping: HashMap::from([
                ("search_project_text".to_string(), "fake_search".to_string()),
                ("edit_file".to_string(), "fake_edit".to_string()),
            ]),
            timeout_secs,
        };
        let provider = ClaudeCodeMcpProvider::new(config.clone());
        Self {
            provider,
            config,
            _fake: fake,
            _temp: temp,
            root,
            marker,
        }
    }

    fn context<'a>(&'a self, path: &'a str) -> ToolExecutionContext<'a> {
        ToolExecutionContext {
            project_root: &self.root,
            target: self.root.join(path),
            relative_path: path,
            max_output_bytes: MAX_MCP_OUTPUT_BYTES,
            timeout_secs: self.config.timeout_secs,
        }
    }

    fn starts(&self) -> usize {
        fs::read_to_string(&self.marker)
            .unwrap_or_default()
            .lines()
            .filter(|line| *line == "start")
            .count()
    }
}

fn search_request() -> Value {
    json!({
        "pattern": "needle",
        "path": ".",
        "limit": 20,
        "context_before": 0,
        "context_after": 0,
        "include_globs": [],
        "exclude_globs": [],
        "result_mode": "matches",
    })
}

fn edit_request() -> Value {
    json!({
        "old": "before",
        "new": "after",
        "expected_replacements": 1,
        "allow_multiple": false,
    })
}

fn call_search(fixture: &Fixture) -> Result<Value, ProviderError> {
    fixture.provider.call(
        ProviderCapability::SearchProjectText,
        search_request(),
        fixture.context("."),
    )
}

fn pending_count(provider: &ClaudeCodeMcpProvider) -> usize {
    provider
        .projects
        .lock()
        .unwrap()
        .values()
        .map(|client| client.connection.pending.lock().unwrap().len())
        .sum()
}

fn process_ids(provider: &ClaudeCodeMcpProvider) -> Vec<u32> {
    provider
        .projects
        .lock()
        .unwrap()
        .values()
        .map(|client| client.connection.child.lock().unwrap().id())
        .collect()
}

fn agent_request(
    kind: &str,
    root: &Path,
    path: &str,
    content: Option<Value>,
) -> ShellAgentShellRequest {
    ShellAgentShellRequest {
        request_id: "request".to_string(),
        client_id: "client".to_string(),
        kind: kind.to_string(),
        job_id: None,
        cwd: Some(root.to_string_lossy().to_string()),
        path: Some(path.to_string()),
        content: content.map(|value| value.to_string()),
        max_bytes: Some(MAX_MCP_OUTPUT_BYTES),
        old_text: None,
        pattern: None,
        expected_sha256: None,
        expected_prefix: None,
        start_line: Some(1),
        end_line: Some(20),
        line: None,
        create_dirs: false,
        command: String::new(),
        stdin: None,
        timeout_secs: 1,
        requested_by: "test".to_string(),
        created_at: 0,
        validation: None,
        lsp: None,
    }
}

fn wait_until(timeout: Duration, condition: impl Fn() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    condition()
}

fn schema_fields(tool: &DiscoveredTool) -> Vec<String> {
    let mut fields = tool
        .fields
        .iter()
        .map(|name| sanitize_name(name))
        .collect::<Vec<_>>();
    fields.sort();
    fields.truncate(32);
    fields
}

fn discovery_inventory(client: &ProjectMcpClient) -> Value {
    let mut tools = client
        .tools
        .iter()
        .map(|(name, tool)| {
            json!({"name": sanitize_name(name), "schema_fields": schema_fields(tool)})
        })
        .collect::<Vec<_>>();
    tools.sort_by(|left, right| left["name"].as_str().cmp(&right["name"].as_str()));
    tools.truncate(32);
    Value::Array(tools)
}

fn real_tool_name(
    client: &ProjectMcpClient,
    capability: ProviderCapability,
    env_key: &str,
) -> Result<String, String> {
    if let Ok(name) = env::var(env_key) {
        let Some(tool) = client.tools.get(&name) else {
            return Err(format!(
                "{env_key} selected {:?}, but discovered tools were {}",
                sanitize_name(&name),
                discovery_inventory(client)
            ));
        };
        let fields = schema_fields(tool);
        let missing = required_fields(capability)
            .iter()
            .filter(|field| !fields.iter().any(|actual| actual == **field))
            .collect::<Vec<_>>();
        return if missing.is_empty() {
            Ok(name)
        } else {
            Err(format!(
                "{env_key} selected {:?}; missing schema fields {:?}; actual fields {:?}",
                sanitize_name(&name),
                missing,
                fields
            ))
        };
    }
    let needle = match capability {
        ProviderCapability::SearchProjectText => "grep",
        ProviderCapability::EditFile => "edit",
    };
    let candidates = client
        .tools
        .iter()
        .filter(|(name, tool)| {
            name.to_ascii_lowercase().contains(needle)
                && required_fields(capability)
                    .iter()
                    .all(|field| tool.fields.contains(*field))
        })
        .map(|(name, _)| name.clone())
        .collect::<Vec<_>>();
    if candidates.len() == 1 {
        Ok(candidates[0].clone())
    } else {
        Err(format!(
            "expected one schema-compatible {needle} tool with fields {:?}; candidates {:?}; discovery {}",
            required_fields(capability),
            candidates,
            discovery_inventory(client)
        ))
    }
}

#[test]
fn provider_is_disabled_by_default_and_missing_command_is_nonfatal() {
    let parsed: ToolProvidersConfig = toml::from_str(
        r#"
strategy = "claude_code_then_native"
[claude_code]
enabled = true
[claude_code.mapping]
search_project_text = "project_search"
edit_file = "project_edit"
"#,
    )
    .unwrap();
    assert_eq!(parsed.strategy, ToolProviderStrategy::ClaudeCodeThenNative);
    assert_eq!(parsed.claude_code.mapping["edit_file"], "project_edit");

    let disabled = ClaudeCodeMcpProvider::new(ClaudeCodeMcpConfig::default());
    assert!(!disabled.status().available);

    let mut missing = ClaudeCodeMcpConfig::default();
    missing.enabled = true;
    missing.command = "/definitely/missing/claude".to_string();
    let provider = ClaudeCodeMcpProvider::new(missing);
    assert!(!provider.status().available);

    let root = tempfile::tempdir().unwrap();
    let router = ExternalToolRouter::new(&ToolProvidersConfig {
        strategy: ToolProviderStrategy::ClaudeCode,
        claude_code: ClaudeCodeMcpConfig::default(),
    });
    let mut request = agent_request("run_shell", root.path(), ".", None);
    request.command = EXTERNAL_SEARCH_REQUEST_PREFIX.to_string();
    request.stdin = Some(search_request().to_string());
    let ExternalRoute::Handled(result) = router.route(&AgentPolicy::default(), &request) else {
        panic!("disabled provider routed to native");
    };
    assert!(result.stdout.unwrap().contains("claude_code_unavailable"));
}

#[test]
fn status_reports_discovery_mapping_process_and_bounded_error() {
    let fixture = Fixture::new("normal");
    assert_eq!(fixture.provider.status().process_state, "not_started");
    let output = call_search(&fixture).unwrap();
    assert!(output.as_str().unwrap().contains("src/lib.rs:2:needle"));
    let status = fixture.provider.status();
    assert_eq!(status.version.as_deref(), Some("Claude Fake 1.2.3"));
    assert_eq!(status.process_state, "running");
    assert_eq!(
        status.discovered_tool_names,
        ["Bash", "Edit", "Read", "Write", "fake_edit", "fake_search"]
    );
    assert_eq!(status.capabilities["search_project_text"], "available");
    assert_eq!(status.capabilities["edit_file"], "available");
    assert_eq!(status.last_error_code, None);
    assert!(status
        .discovered_tool_names
        .iter()
        .all(|name| name.chars().count() <= 120));
    let serialized = serde_json::to_string(&status).unwrap();
    let root_text = fixture.root.to_string_lossy();
    let marker_text = fixture.marker.to_string_lossy();
    for forbidden in [
        root_text.as_ref(),
        marker_text.as_ref(),
        fixture.config.command.as_str(),
        "stderr",
        "environment",
        "token",
        "cookie",
    ] {
        assert!(!serialized.contains(forbidden), "status leaked {forbidden}");
    }
    let mut mismatched = fixture.config.clone();
    mismatched.mapping.remove("edit_file");
    mismatched
        .mapping
        .insert("search_project_text".to_string(), "fake_edit".to_string());
    let mismatched = ClaudeCodeMcpProvider::new(mismatched);
    mismatched
        .project_client(&fixture.root, Instant::now() + Duration::from_secs(1))
        .unwrap();
    let status = mismatched.status();
    assert_eq!(status.capabilities["edit_file"], "unmapped");
    assert_eq!(
        status.capabilities["search_project_text"],
        "schema_mismatch"
    );
    let marker = fs::read_to_string(&fixture.marker).unwrap();
    assert!(marker.contains(r#""method":"tools/list""#));
}

#[test]
fn search_and_edit_mappings_normalize_results() {
    let fixture = Fixture::new("normal");
    let router = ExternalToolRouter::new(&ToolProvidersConfig {
        strategy: ToolProviderStrategy::ClaudeCode,
        claude_code: fixture.config.clone(),
    });
    let mut search = agent_request("run_shell", &fixture.root, ".", None);
    search.command = format!("{EXTERNAL_SEARCH_REQUEST_PREFIX}\nignored native command");
    search.stdin = Some(search_request().to_string());
    let ExternalRoute::Handled(search) = router.route(&AgentPolicy::default(), &search) else {
        panic!("search routed to native");
    };
    assert!(search.stdout.unwrap().contains("src/lib.rs:2:needle"));
    assert!(fs::read_to_string(&fixture.marker)
        .unwrap()
        .contains(r#""output_mode":"content""#));

    let edit = agent_request(
        "file_replace_in_file",
        &fixture.root,
        "edit.txt",
        Some(edit_request()),
    );
    let ExternalRoute::Handled(edit) = router.route(&AgentPolicy::default(), &edit) else {
        panic!("edit routed to native");
    };
    let edit: Value = serde_json::from_str(edit.stdout.as_deref().unwrap()).unwrap();
    assert_eq!(edit["changed"], true);
    assert_eq!(
        fs::read_to_string(fixture.root.join("edit.txt")).unwrap(),
        "after\n"
    );

    let status = router.status();
    let call = status.claude_code.last_call.unwrap();
    assert_eq!(call.capability, "edit_file");
    assert_eq!(call.selected_provider, "claude_code");
    assert!(!call.fallback_used);
    assert_eq!(call.result, "success");
    assert_eq!(call.write_state.as_deref(), Some("confirmed"));
    assert_eq!(call.error_code, None);
}

#[test]
fn fallback_and_failure_routes_record_bounded_last_call_evidence() {
    let fixture = Fixture::new("normal");
    let mut unmapped_search = fixture.config.clone();
    unmapped_search.mapping.remove("search_project_text");
    let router = ExternalToolRouter::new(&ToolProvidersConfig {
        strategy: ToolProviderStrategy::ClaudeCodeThenNative,
        claude_code: unmapped_search,
    });
    let mut search = agent_request("run_shell", &fixture.root, ".", None);
    search.command = EXTERNAL_SEARCH_REQUEST_PREFIX.to_string();
    search.stdin = Some(search_request().to_string());
    let ExternalRoute::NativeFallback(fallback) = router.route(&AgentPolicy::default(), &search)
    else {
        panic!("unmapped search did not request Native fallback");
    };
    router.complete_native_fallback(
        fallback,
        &CommandResult {
            exit_code: Some(0),
            stdout: Some("native search".to_string()),
            stderr: Some(String::new()),
            duration_ms: Some(1),
            error: None,
        },
    );
    let status = router.status();
    let call = status.claude_code.last_call.unwrap();
    assert_eq!(call.selected_provider, "native");
    assert!(call.fallback_used);
    assert_eq!(call.result, "success");
    assert_eq!(call.write_state, None);
    assert_eq!(call.error_code, None);
    assert_eq!(
        status.claude_code.last_error_code.as_deref(),
        Some("provider_capability_unavailable")
    );

    let timeout = Fixture::new("timeout");
    let router = ExternalToolRouter::new(&ToolProvidersConfig {
        strategy: ToolProviderStrategy::ClaudeCodeThenNative,
        claude_code: timeout.config.clone(),
    });
    let edit = agent_request(
        "file_replace_in_file",
        &timeout.root,
        "edit.txt",
        Some(edit_request()),
    );
    let ExternalRoute::Handled(_) = router.route(&AgentPolicy::default(), &edit) else {
        panic!("uncertain edit was allowed to fall back");
    };
    let status = router.status();
    let call = status.claude_code.last_call.unwrap();
    assert_eq!(call.selected_provider, "claude_code");
    assert!(!call.fallback_used);
    assert_eq!(call.result, "failure");
    assert_eq!(call.write_state.as_deref(), Some("uncertain"));
    assert_eq!(call.error_code.as_deref(), Some("mcp_request_timeout"));
}

#[test]
fn edit_falls_back_only_before_submission_and_confirms_native_write() {
    let fixture = Fixture::new("normal");
    let mut unmapped_edit = fixture.config.clone();
    unmapped_edit.mapping.remove("edit_file");
    let router = ExternalToolRouter::new(&ToolProvidersConfig {
        strategy: ToolProviderStrategy::ClaudeCodeThenNative,
        claude_code: unmapped_edit,
    });
    let edit = agent_request(
        "file_replace_in_file",
        &fixture.root,
        "edit.txt",
        Some(edit_request()),
    );
    let ExternalRoute::NativeFallback(fallback) = router.route(&AgentPolicy::default(), &edit)
    else {
        panic!("unsubmitted edit did not request Native fallback");
    };
    router.complete_native_fallback(
        fallback,
        &CommandResult {
            exit_code: Some(0),
            stdout: Some(json!({"changed": true}).to_string()),
            stderr: Some(String::new()),
            duration_ms: Some(1),
            error: None,
        },
    );
    let call = router.status().claude_code.last_call.unwrap();
    assert_eq!(call.selected_provider, "native");
    assert!(call.fallback_used);
    assert_eq!(call.write_state.as_deref(), Some("confirmed"));
}

#[test]
fn status_revisions_are_changed_only_and_registration_reads_latest_snapshot() {
    let fixture = Fixture::new("normal");
    let router = ExternalToolRouter::new(&ToolProvidersConfig {
        strategy: ToolProviderStrategy::ClaudeCode,
        claude_code: fixture.config.clone(),
    });
    let (_, initial_revision) = router.registration_status();
    router.mark_status_reported(initial_revision);
    assert!(router.claim_status_update().is_none());

    let mut search = agent_request("run_shell", &fixture.root, ".", None);
    search.command = EXTERNAL_SEARCH_REQUEST_PREFIX.to_string();
    search.stdin = Some(search_request().to_string());
    assert!(matches!(
        router.route(&AgentPolicy::default(), &search),
        ExternalRoute::Handled(_)
    ));
    let (update, revision) = router.claim_status_update().unwrap();
    assert_eq!(
        update.claude_code.last_call.as_ref().unwrap().capability,
        "search_project_text"
    );
    assert!(
        router.claude.state.status.try_lock().is_ok(),
        "status claim retained the Provider lock across transport send"
    );
    assert!(router.claim_status_update().is_none());
    // A newer state cannot overtake an already claimed snapshot. Once the
    // first snapshot is reported, the next claim observes the newer revision.
    router
        .claude
        .record_error(&ProviderError::new("mcp_protocol_error"));
    assert!(router.claim_status_update().is_none());
    router.mark_status_reported(revision);
    let (_, newer_revision) = router.claim_status_update().unwrap();
    assert!(newer_revision > revision);
    // A failed best-effort metadata send releases only the status claim; the
    // already computed state remains available for retry.
    router.release_status_update(newer_revision);
    assert!(router.claim_status_update().is_some());

    let (registered, latest_revision) = router.registration_status();
    assert!(latest_revision > initial_revision);
    assert_eq!(registered.claude_code.process_state, "running");
    assert!(registered.claude_code.last_call.is_some());
}

#[test]
fn router_rejects_absolute_parent_and_symlink_escape_paths() {
    let fixture = Fixture::new("normal");
    let router = ExternalToolRouter::new(&ToolProvidersConfig {
        strategy: ToolProviderStrategy::ClaudeCode,
        claude_code: fixture.config.clone(),
    });
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("outside.txt"), "before").unwrap();
    let cases = [
        "../outside.txt".to_string(),
        outside
            .path()
            .join("outside.txt")
            .to_string_lossy()
            .to_string(),
    ];
    for path in cases {
        let request = agent_request(
            "file_replace_in_file",
            &fixture.root,
            &path,
            Some(edit_request()),
        );
        let ExternalRoute::Handled(result) = router.route(&AgentPolicy::default(), &request) else {
            panic!("unsafe path routed to native");
        };
        let output: Value = serde_json::from_str(result.stdout.as_deref().unwrap()).unwrap();
        assert_eq!(output["code"], "provider_path_rejected");
    }

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(outside.path(), fixture.root.join("escape")).unwrap();
        let request = agent_request(
            "file_replace_in_file",
            &fixture.root,
            "escape/outside.txt",
            Some(edit_request()),
        );
        let ExternalRoute::Handled(result) = router.route(&AgentPolicy::default(), &request) else {
            panic!("symlink escape routed to native");
        };
        assert!(result.stdout.unwrap().contains("provider_path_rejected"));
    }
}

#[test]
fn protocol_failures_are_bounded_and_unknown_ids_are_ignored() {
    for (scenario, expected) in [
        ("invalid_json", "mcp_invalid_json"),
        ("oversized", "mcp_message_too_large"),
    ] {
        let fixture = Fixture::new(scenario);
        let error = call_search(&fixture).unwrap_err();
        assert_eq!(error.code, expected);
        assert_eq!(pending_count(&fixture.provider), 0);
        assert_eq!(
            fixture.provider.status().last_error_code.as_deref(),
            Some(expected)
        );
    }

    let fixture = Fixture::new("unknown_id");
    assert!(call_search(&fixture).is_ok());

    let fixture = Fixture::new("server_request");
    assert!(call_search(&fixture).is_ok());
    assert!(fs::read_to_string(&fixture.marker)
        .unwrap()
        .contains("server_request_error_received"));
}

#[test]
fn process_exit_clears_pending_and_next_call_restarts_lazily() {
    let fixture = Fixture::new("restart_once");
    assert!(call_search(&fixture).is_err());
    assert!(wait_until(Duration::from_secs(1), || {
        pending_count(&fixture.provider) == 0
            && fixture.provider.status().process_state == "stopped"
    }));
    assert!(call_search(&fixture).is_ok());
    assert_eq!(fixture.starts(), 2);
    assert_eq!(fixture.provider.status().process_state, "running");
    fixture.provider.shutdown();
    let status = fixture.provider.status();
    assert_eq!(status.process_state, "stopped");
    assert!(!status.available);
}

#[test]
fn timeout_removes_pending_and_uncertain_edit_never_falls_back() {
    let fixture = Fixture::new("timeout");
    let error = call_search(&fixture).unwrap_err();
    assert_eq!(error.code, "mcp_request_timeout");
    assert_eq!(pending_count(&fixture.provider), 0);
    let status = fixture.provider.status();
    assert_eq!(status.process_state, "stopped");
    assert_eq!(
        status.last_error_code.as_deref(),
        Some("mcp_request_timeout")
    );

    let router = ExternalToolRouter::new(&ToolProvidersConfig {
        strategy: ToolProviderStrategy::ClaudeCodeThenNative,
        claude_code: fixture.config.clone(),
    });
    let request = agent_request(
        "file_replace_in_file",
        &fixture.root,
        "edit.txt",
        Some(edit_request()),
    );
    let ExternalRoute::Handled(result) = router.route(&AgentPolicy::default(), &request) else {
        panic!("uncertain edit fell back to native");
    };
    let output: Value = serde_json::from_str(result.stdout.as_deref().unwrap()).unwrap();
    assert_eq!(output["write_state"], "uncertain");
    assert_eq!(output["changed"], Value::Null);
    assert_eq!(
        fs::read_to_string(fixture.root.join("edit.txt")).unwrap(),
        "before\n"
    );

    let mut unmapped = fixture.config.clone();
    unmapped.mapping.remove("edit_file");
    let router = ExternalToolRouter::new(&ToolProvidersConfig {
        strategy: ToolProviderStrategy::ClaudeCodeThenNative,
        claude_code: unmapped,
    });
    assert!(matches!(
        router.route(&AgentPolicy::default(), &request),
        ExternalRoute::NativeFallback(_)
    ));
}

#[test]
fn native_strategy_does_not_start_claude() {
    let fixture = Fixture::new("normal");
    let router = ExternalToolRouter::new(&ToolProvidersConfig {
        strategy: ToolProviderStrategy::Native,
        claude_code: fixture.config.clone(),
    });
    let mut request = agent_request("run_shell", &fixture.root, ".", None);
    request.command = EXTERNAL_SEARCH_REQUEST_PREFIX.to_string();
    request.stdin = Some(search_request().to_string());
    assert!(matches!(
        router.route(&AgentPolicy::default(), &request),
        ExternalRoute::Native
    ));
    assert_eq!(fixture.starts(), 0);
}

#[test]
fn opt_in_real_claude_mcp_probe() {
    if env::var("WEBCODEX_PROBE_CLAUDE_PROVIDER").as_deref() != Ok("1") {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let mut config = ClaudeCodeMcpConfig::default();
    config.enabled = true;
    config
        .mapping
        .insert("edit_file".to_string(), "Edit".to_string());
    let provider = ClaudeCodeMcpProvider::new(config);
    provider
        .project_client(root.path(), Instant::now() + Duration::from_secs(30))
        .unwrap_or_else(|error| panic!("Claude MCP probe failed: {}", error.code));
    let status = provider.status();
    assert!(status.available);
    assert_eq!(status.process_state, "running");
    println!(
        "{}",
        serde_json::to_string(&status).expect("provider status must serialize")
    );
    provider.shutdown();
}

#[test]
fn opt_in_real_claude_mcp_smoke() {
    if env::var("WEBCODEX_TEST_CLAUDE_MCP").as_deref() != Ok("1") {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("claude-smoke-project");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("fixture.txt"), "zero\nneedle\n").unwrap();
    fs::write(root.join("edit.txt"), "before\n").unwrap();
    let root = root.canonicalize().unwrap();

    let mut config = ClaudeCodeMcpConfig::default();
    config.enabled = true;
    let provider = ClaudeCodeMcpProvider::new(config.clone());
    let client = provider
        .project_client(&root, Instant::now() + Duration::from_secs(30))
        .unwrap_or_else(|error| panic!("real claude MCP initialization failed: {}", error.code));
    let status = provider.status();
    assert!(status.available, "Claude MCP did not become available");
    eprintln!(
        "claude_mcp_version={:?} discovery={}",
        status.version,
        discovery_inventory(&client)
    );

    let grep_tool = real_tool_name(
        &client,
        ProviderCapability::SearchProjectText,
        "WEBCODEX_TEST_CLAUDE_GREP_TOOL",
    );
    config.mapping.insert(
        "edit_file".to_string(),
        real_tool_name(
            &client,
            ProviderCapability::EditFile,
            "WEBCODEX_TEST_CLAUDE_EDIT_TOOL",
        )
        .unwrap_or_else(|error| panic!("{error}")),
    );

    if let Ok(name) = &grep_tool {
        config
            .mapping
            .insert("search_project_text".to_string(), name.clone());
        let search_context = ToolExecutionContext {
            project_root: &root,
            target: root.clone(),
            relative_path: ".",
            max_output_bytes: MAX_MCP_OUTPUT_BYTES,
            timeout_secs: 30,
        };
        let search = client
            .call(
                ProviderCapability::SearchProjectText,
                search_request(),
                &search_context,
                &config,
                Instant::now() + Duration::from_secs(30),
            )
            .unwrap_or_else(|error| panic!("real Grep call failed with {}", error.code));
        let matched = search
            .as_str()
            .is_some_and(|text| text.contains("fixture.txt") && text.contains("needle"));
        assert!(
            matched,
            "real Grep did not return the temporary fixture: {search}"
        );
        eprintln!("claude_mcp_grep_result matched={matched}");
    }

    let edit_context = ToolExecutionContext {
        project_root: &root,
        target: root.join("edit.txt"),
        relative_path: "edit.txt",
        max_output_bytes: MAX_MCP_OUTPUT_BYTES,
        timeout_secs: 30,
    };
    let edit = client
        .call(
            ProviderCapability::EditFile,
            edit_request(),
            &edit_context,
            &config,
            Instant::now() + Duration::from_secs(30),
        )
        .unwrap_or_else(|error| panic!("real Edit call failed with {}", error.code));
    let final_content = fs::read_to_string(root.join("edit.txt")).unwrap();
    assert_eq!(final_content, "after\n");
    eprintln!(
        "claude_mcp_edit_result changed={} final_content_verified=true",
        edit["changed"]
    );

    let process_groups = process_ids(&provider);
    assert!(
        !process_groups.is_empty(),
        "provider did not retain its child"
    );
    provider.shutdown();
    #[cfg(unix)]
    for process_group in process_groups {
        assert!(
            wait_until(Duration::from_secs(2), || !process_group_exists(
                process_group
            )),
            "Claude process group {process_group} remained after provider shutdown"
        );
    }
    eprintln!("claude_mcp_shutdown process_groups_reaped=true");
    if let Err(error) = grep_tool {
        eprintln!("claude_mcp_grep_unavailable={}", sanitize_name(&error));
    }
}

#[cfg(unix)]
fn process_group_exists(process_group: u32) -> bool {
    // SAFETY: signal 0 only probes the private process group captured above.
    (unsafe { libc::kill(-(process_group as i32), 0) == 0 })
        || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

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
    let ExternalRoute::Handled(result) = router.route(&AgentPolicy::default(), &request) else {
        panic!("experimental request left the experimental path");
    };
    serde_json::from_str(result.stdout.as_deref().unwrap()).unwrap()
}

#[test]
fn experimental_list_tools_discovers_sorted_bounded_names_and_hashes() {
    let fixture = Fixture::new("normal");
    let router = experimental_router(&fixture);
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
    for expected in ["Bash", "Edit", "Read", "Write", "fake_edit", "fake_search"] {
        assert!(
            names.iter().any(|name| name == expected),
            "missing {expected}"
        );
    }
    assert!(listed["tools"].as_array().unwrap().iter().all(|tool| {
        tool["schema_hash"]
            .as_str()
            .is_some_and(|hash| hash.len() == 64)
    }));
    assert!(!serde_json::to_string(&listed)
        .unwrap()
        .contains(&fixture.config.command));
}

#[test]
fn experimental_describe_tool_returns_live_schema_hash_and_rejects_unknown() {
    let fixture = Fixture::new("normal");
    let router = experimental_router(&fixture);
    let described = experimental_stdout(
        &router,
        experimental_request(
            EXPERIMENTAL_KIND_DESCRIBE,
            &fixture.root,
            Some(json!({"tool_name": "Bash"})),
        ),
    );
    assert_eq!(described["tool_name"], "Bash");
    assert_eq!(described["experimental"], true);
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
}

#[test]
fn experimental_arguments_validation_and_schema_hash_are_stable() {
    let fixture = Fixture::new("normal");
    let router = experimental_router(&fixture);
    let invalid = experimental_stdout(
        &router,
        experimental_request(
            EXPERIMENTAL_KIND_CALL,
            &fixture.root,
            Some(json!({
                "tool_name": "Read",
                "arguments": {"not_file_path": true}
            })),
        ),
    );
    assert_eq!(invalid["code"], "claude_arguments_invalid");

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
fn experimental_read_edit_write_bash_paths_and_process_reuse() {
    let fixture = Fixture::new("normal");
    let router = experimental_router(&fixture);

    let read = experimental_stdout(
        &router,
        experimental_request(
            EXPERIMENTAL_KIND_CALL,
            &fixture.root,
            Some(json!({
                "tool_name": "Read",
                "arguments": {"file_path": "src/lib.rs"}
            })),
        ),
    );
    assert_eq!(read["is_error"], false);
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
            Some(json!({
                "tool_name": "Edit",
                "arguments": {
                    "file_path": "edit.txt",
                    "old_string": "before",
                    "new_string": "after"
                }
            })),
        ),
    );
    assert_eq!(edit["is_error"], false);
    assert_eq!(edit["process_reused"], true);
    assert_eq!(
        fs::read_to_string(fixture.root.join("edit.txt")).unwrap(),
        "after\n"
    );
    // restore fixture file
    fs::write(fixture.root.join("edit.txt"), "before\n").unwrap();

    let write = experimental_stdout(
        &router,
        experimental_request(
            EXPERIMENTAL_KIND_CALL,
            &fixture.root,
            Some(json!({
                "tool_name": "Write",
                "arguments": {
                    "file_path": "tmp-exp.txt",
                    "content": "hello-experimental"
                }
            })),
        ),
    );
    assert_eq!(write["is_error"], false);
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
            Some(json!({
                "tool_name": "Bash",
                "arguments": {"command": "printf hi"}
            })),
        ),
    );
    assert_eq!(bash_ok["is_error"], false);
    assert!(bash_ok["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("printf hi"));

    let bash_err = experimental_stdout(
        &router,
        experimental_request(
            EXPERIMENTAL_KIND_CALL,
            &fixture.root,
            Some(json!({
                "tool_name": "Bash",
                "arguments": {"command": "nonzero"}
            })),
        ),
    );
    assert_eq!(bash_err["is_error"], true);
    assert_eq!(fixture.starts(), 1);
}

#[test]
fn experimental_rejects_unknown_tools_and_recovers_after_process_exit() {
    let fixture = Fixture::new("normal");
    let router = experimental_router(&fixture);
    let unknown = experimental_stdout(
        &router,
        experimental_request(
            EXPERIMENTAL_KIND_CALL,
            &fixture.root,
            Some(json!({
                "tool_name": "TaskCreate",
                "arguments": {}
            })),
        ),
    );
    assert_eq!(unknown["code"], "claude_tool_not_found");

    let exit = Fixture::new("exit");
    let router = experimental_router(&exit);
    let first = experimental_stdout(
        &router,
        experimental_request(EXPERIMENTAL_KIND_LIST, &exit.root, None),
    );
    assert_eq!(first["experimental"], true);
    // Force a tools/call so the exit scenario ends the child after first call path.
    let _ = experimental_stdout(
        &router,
        experimental_request(
            EXPERIMENTAL_KIND_CALL,
            &exit.root,
            Some(json!({
                "tool_name": "Read",
                "arguments": {"file_path": "src/lib.rs"}
            })),
        ),
    );
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
fn experimental_result_bounding_marks_truncated_output() {
    let fixture = Fixture::new("exp_oversized");
    let router = experimental_router(&fixture);
    let result = experimental_stdout(
        &router,
        experimental_request(
            EXPERIMENTAL_KIND_CALL,
            &fixture.root,
            Some(json!({
                "tool_name": "Bash",
                "arguments": {"command": "big"}
            })),
        ),
    );
    // oversized responses are either hard-failed or soft-truncated.
    let hard = result["code"].as_str() == Some("claude_result_too_large");
    let soft = result["result_truncated"].as_bool() == Some(true)
        || result["result"]["truncated"].as_bool() == Some(true);
    assert!(hard || soft, "expected bounded oversized result: {result}");
}

#[test]
fn opt_in_experimental_real_claude_tools_smoke() {
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
            Some(json!({
                "tool_name": "Read",
                "arguments": {"file_path": root.join("src/sample.txt").to_string_lossy()}
            })),
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
            Some(json!({
                "tool_name": "Read",
                "arguments": {"file_path": edit_path.to_string_lossy()}
            })),
        ),
    );
    assert_eq!(pre_read["is_error"], false, "{pre_read}");
    let edit = experimental_stdout(
        &router,
        experimental_request(
            EXPERIMENTAL_KIND_CALL,
            &root,
            Some(json!({
                "tool_name": "Edit",
                "arguments": {
                    "file_path": edit_path.to_string_lossy(),
                    "old_string": "before",
                    "new_string": "after"
                }
            })),
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
            Some(json!({
                "tool_name": "Write",
                "arguments": {
                    "file_path": write_path.to_string_lossy(),
                    "content": "temporary"
                }
            })),
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
            Some(json!({
                "tool_name": "Bash",
                "arguments": {"command": "printf 'ok-from-bash'"}
            })),
        ),
    );
    assert_eq!(bash["is_error"], false, "{bash}");
    assert!(format!("{bash}").contains("ok-from-bash"), "{bash}");

    let bash_fail = experimental_stdout(
        &router,
        experimental_request(
            EXPERIMENTAL_KIND_CALL,
            &root,
            Some(json!({
                "tool_name": "Bash",
                "arguments": {"command": "exit 7"}
            })),
        ),
    );
    // Claude may surface non-zero exits as isError or as structured text.
    eprintln!("experimental_real_claude_bash_nonzero={bash_fail}");

    let process_groups = process_ids(&router.claude);
    router.shutdown();
    #[cfg(unix)]
    for process_group in process_groups {
        assert!(
            wait_until(Duration::from_secs(2), || !process_group_exists(
                process_group
            )),
            "Claude process group {process_group} remained after experimental shutdown"
        );
    }
}
