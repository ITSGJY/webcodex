use super::*;
use crate::mcp_bridge::{McpBridgeContent, McpBridgeResponsePayload};
use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use tempfile::TempDir;

static FAKE_SERVER: OnceLock<Mutex<Weak<FakeBinary>>> = OnceLock::new();

struct FakeBinary {
    _temp: TempDir,
    path: PathBuf,
}

fn fake_binary() -> Arc<FakeBinary> {
    let cache = FAKE_SERVER.get_or_init(|| Mutex::new(Weak::new()));
    let mut cached = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(binary) = cached.upgrade() {
        return binary;
    }
    let temp = tempfile::tempdir().unwrap();
    let output = temp.path().join(format!(
        "webcodex-mcp-bridge-fake{}",
        env::consts::EXE_SUFFIX
    ));
    let source =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/webcodex_runner/fake_mcp_bridge.rs");
    let rustc = env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let result = Command::new(rustc)
        .arg("--edition=2021")
        .arg("--crate-name=webcodex_mcp_bridge_fake")
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
    manager: McpBridgeManager,
    marker: PathBuf,
    _fake: Arc<FakeBinary>,
    _temp: TempDir,
}

impl Fixture {
    fn new(scenario: &str, timeout_secs: u64) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let marker = temp.path().join("marker.log");
        let fake = fake_binary();
        let manager = McpBridgeManager::new(&McpBridgeConfig {
            request_timeout_secs: timeout_secs,
            providers: vec![McpBridgeProviderConfig {
                id: "fake".to_string(),
                name: "Fake provider".to_string(),
                executable: fake.path.to_string_lossy().to_string(),
                args: vec![scenario.to_string(), marker.to_string_lossy().to_string()],
            }],
        });
        Self {
            manager,
            marker,
            _fake: fake,
            _temp: temp,
        }
    }

    fn provider(&self) -> McpBridgeProvider {
        let response = self.manager.handle(McpBridgeRequest::Discover);
        let Some(McpBridgeResponsePayload::Providers { providers }) = response.payload else {
            panic!("discover payload missing");
        };
        providers.into_iter().next().unwrap()
    }

    fn list(&self, provider: &McpBridgeProvider) -> McpBridgeResponse {
        self.manager.handle(McpBridgeRequest::ToolsList {
            provider_id: provider.provider_id.clone(),
            provider_instance_id: provider.provider_instance_id.clone(),
        })
    }

    fn call(&self, provider: &McpBridgeProvider) -> McpBridgeResponse {
        self.manager.handle(McpBridgeRequest::ToolsCall {
            provider_id: provider.provider_id.clone(),
            provider_instance_id: provider.provider_instance_id.clone(),
            name: "echo".to_string(),
            arguments: json!({"value": "hello"}),
        })
    }

    fn marker_count(&self, value: &str) -> usize {
        fs::read_to_string(&self.marker)
            .unwrap_or_default()
            .lines()
            .filter(|line| *line == value)
            .count()
    }
}

#[test]
fn persistent_provider_initializes_once_and_serves_repeated_calls() {
    let fixture = Fixture::new("normal", 5);
    let provider = fixture.provider();
    let listed = fixture.list(&provider);
    let Some(McpBridgeResponsePayload::Tools { tools }) = listed.payload else {
        panic!("tools payload missing: {:?}", listed.error);
    };
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "echo");

    for expected in ["call-1", "call-2"] {
        let response = fixture.call(&provider);
        let Some(McpBridgeResponsePayload::ToolResult { result }) = response.payload else {
            panic!("call payload missing: {:?}", response.error);
        };
        assert_eq!(
            result.content,
            vec![McpBridgeContent::Text {
                text: expected.to_string()
            }]
        );
    }
    assert_eq!(fixture.marker_count("start"), 1);
    assert_eq!(fixture.marker_count("initialize"), 1);
    assert_eq!(fixture.marker_count("initialized"), 1);
    assert_eq!(fixture.marker_count("call"), 2);
}

#[test]
fn crash_is_outcome_unknown_and_never_restarted_or_replayed() {
    let fixture = Fixture::new("crash", 2);
    let provider = fixture.provider();
    assert!(fixture.list(&provider).error.is_none());
    let first = fixture.call(&provider);
    assert_eq!(first.dispatch_state, McpBridgeDispatchState::OutcomeUnknown);
    assert_eq!(first.error.as_ref().unwrap().code, "provider_eof");

    let second = fixture.call(&provider);
    assert_eq!(second.dispatch_state, McpBridgeDispatchState::NotStarted);
    assert_eq!(second.error.as_ref().unwrap().code, "stale_provider");
    assert_eq!(fixture.marker_count("start"), 1);
    assert_eq!(fixture.marker_count("call"), 1);
}

#[test]
fn initialization_failure_is_not_misreported_as_tool_dispatch() {
    for (scenario, code) in [
        ("init_crash", "provider_eof"),
        ("init_timeout", "provider_timeout"),
    ] {
        let fixture = Fixture::new(scenario, 1);
        let provider = fixture.provider();
        let response = fixture.call(&provider);
        assert_eq!(
            response.dispatch_state,
            McpBridgeDispatchState::NotStarted,
            "{scenario}"
        );
        assert_eq!(response.error.as_ref().unwrap().code, code, "{scenario}");
        assert_eq!(fixture.marker_count("call"), 0, "{scenario}");
        assert_eq!(fixture.marker_count("start"), 1, "{scenario}");
        assert_eq!(
            fixture.call(&provider).error.as_ref().unwrap().code,
            "stale_provider",
            "{scenario}"
        );
        assert_eq!(fixture.marker_count("start"), 1, "{scenario}");
    }
}

#[test]
fn malformed_unknown_and_duplicate_responses_fail_closed() {
    for (scenario, code) in [
        ("malformed", "provider_malformed_json"),
        ("unknown_id", "provider_unknown_response_id"),
    ] {
        let fixture = Fixture::new(scenario, 2);
        let response = fixture.list(&fixture.provider());
        assert_eq!(response.error.as_ref().unwrap().code, code, "{scenario}");
        assert_eq!(
            response.dispatch_state,
            McpBridgeDispatchState::OutcomeUnknown
        );
    }

    let fixture = Fixture::new("duplicate_id", 2);
    let provider = fixture.provider();
    assert!(fixture.list(&provider).error.is_none());
    let response = fixture.call(&provider);
    assert_eq!(
        response.error.as_ref().unwrap().code,
        "provider_duplicate_response_id"
    );
}

#[test]
fn timeout_and_invalid_untrusted_outputs_are_bounded() {
    let timeout = Fixture::new("timeout", 1);
    let provider = timeout.provider();
    assert!(timeout.list(&provider).error.is_none());
    let response = timeout.call(&provider);
    assert_eq!(response.error.as_ref().unwrap().code, "provider_timeout");
    assert_eq!(
        response.dispatch_state,
        McpBridgeDispatchState::OutcomeUnknown
    );

    for (scenario, operation, code) in [
        ("bad_tools", "list", "invalid_provider_tools"),
        ("oversized_message", "list", "provider_message_too_large"),
        ("bad_result", "call", "unsupported_provider_content"),
        ("oversized_result", "call", "provider_protocol_error"),
    ] {
        let fixture = Fixture::new(scenario, 2);
        let provider = fixture.provider();
        let response = if operation == "list" {
            fixture.list(&provider)
        } else {
            assert!(fixture.list(&provider).error.is_none());
            fixture.call(&provider)
        };
        assert_eq!(response.error.as_ref().unwrap().code, code, "{scenario}");
        assert_eq!(
            response.dispatch_state,
            McpBridgeDispatchState::OutcomeUnknown
        );
    }
}

#[test]
fn stale_provider_instance_fails_without_starting_child() {
    let fixture = Fixture::new("normal", 2);
    let mut provider = fixture.provider();
    provider.provider_instance_id = "stale".to_string();
    let response = fixture.list(&provider);
    assert_eq!(response.dispatch_state, McpBridgeDispatchState::NotStarted);
    assert_eq!(response.error.as_ref().unwrap().code, "stale_provider");
    assert_eq!(fixture.marker_count("start"), 0);
}
