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

fn schema_fields(schema: &BTreeSet<String>) -> Vec<String> {
    let mut fields = schema
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
        .map(|(name, schema)| {
            json!({"name": sanitize_name(name), "schema_fields": schema_fields(schema)})
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
        let Some(schema) = client.tools.get(&name) else {
            return Err(format!(
                "{env_key} selected {:?}, but discovered tools were {}",
                sanitize_name(&name),
                discovery_inventory(client)
            ));
        };
        let fields = schema_fields(schema);
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
        .filter(|(name, schema)| {
            name.to_ascii_lowercase().contains(needle)
                && required_fields(capability)
                    .iter()
                    .all(|field| schema.contains(*field))
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
    let mut fixture = Fixture::new("normal");
    assert_eq!(fixture.provider.status().process_state, "not_started");
    let output = call_search(&fixture).unwrap();
    assert!(output.as_str().unwrap().contains("src/lib.rs:2:needle"));
    let status = fixture.provider.status();
    assert_eq!(status.version.as_deref(), Some("Claude Fake 1.2.3"));
    assert_eq!(status.process_state, "running");
    assert_eq!(status.discovered_tool_names, ["fake_edit", "fake_search"]);
    assert_eq!(status.capabilities["search_project_text"], "available");
    assert_eq!(status.capabilities["edit_file"], "available");
    assert_eq!(status.last_error_code, None);
    fixture.provider.config.mapping.remove("edit_file");
    fixture
        .provider
        .config
        .mapping
        .insert("search_project_text".to_string(), "fake_edit".to_string());
    let status = fixture.provider.status();
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
    }));
    assert!(call_search(&fixture).is_ok());
    assert_eq!(fixture.starts(), 2);
}

#[test]
fn timeout_removes_pending_and_uncertain_edit_never_falls_back() {
    let fixture = Fixture::new("timeout");
    let error = call_search(&fixture).unwrap_err();
    assert_eq!(error.code, "mcp_request_timeout");
    assert_eq!(pending_count(&fixture.provider), 0);

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
        ExternalRoute::Native
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
        panic!("{error}");
    }
}

#[cfg(unix)]
fn process_group_exists(process_group: u32) -> bool {
    // SAFETY: signal 0 only probes the private process group captured above.
    (unsafe { libc::kill(-(process_group as i32), 0) == 0 })
        || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}
