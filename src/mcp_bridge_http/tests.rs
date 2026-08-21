use super::*;
use crate::mcp_bridge::{McpBridgeContent, McpBridgeTool, McpBridgeToolResult};
use crate::shell_protocol::{
    AgentPolicySummary, ShellAgentPollRequest, ShellAgentResultPayload, ShellAgentResultRequest,
    ShellClientCapabilities, ShellClientRegisterRequest,
};
use crate::test_support::{seed_oauth_client, seed_user, test_config, test_config_oauth2, test_db};
use crate::tool_runtime::ToolRuntime;
use salvo::test::{ResponseExt, TestClient};
use salvo::Service;
use std::sync::atomic::{AtomicUsize, Ordering};

fn test_router(
    config: Arc<crate::Config>,
    db: Arc<crate::Database>,
    registry: Arc<ShellClientRegistry>,
) -> Router {
    let runtime = Arc::new(ToolRuntime::new_for_tests_with_shell_clients(Arc::clone(
        &registry,
    )));
    test_router_with_runtime(config, db, registry, runtime)
}

fn test_router_with_runtime(
    config: Arc<crate::Config>,
    db: Arc<crate::Database>,
    registry: Arc<ShellClientRegistry>,
    runtime: Arc<ToolRuntime>,
) -> Router {
    Router::new()
        .hoop(affix_state::inject(config))
        .hoop(affix_state::inject(db))
        .hoop(affix_state::inject(registry))
        .hoop(affix_state::inject(runtime))
        .hoop(affix_state::inject(
            crate::connector_runtime::ConnectorRuntimeSlot::default(),
        ))
        .push(
            Router::with_path("mcp")
                .hoop(crate::AuthMiddleware)
                .get(crate::mcp::mcp_info)
                .post(crate::mcp::mcp_post)
                .push(
                    Router::with_path("bridge").get(bridge_list).push(
                        Router::with_path("{bridge_id}")
                            .get(bridge_info)
                            .post(bridge_post),
                    ),
                ),
        )
}

fn default_bridge_provider() -> McpBridgeProvider {
    McpBridgeProvider {
        provider_id: "local-test".to_string(),
        provider_instance_id: "provider-instance".to_string(),
        name: "Local test provider".to_string(),
    }
}

async fn register_runner_with_owner_and_providers(
    registry: &ShellClientRegistry,
    owner: Option<&str>,
    providers: Vec<McpBridgeProvider>,
) {
    registry
        .register(ShellClientRegisterRequest {
            client_id: "bridge-http-runner".to_string(),
            agent_instance_id: "bridge-http-instance".to_string(),
            display_name: None,
            owner: owner.map(str::to_string),
            hostname: None,
            capabilities: Some(ShellClientCapabilities {
                mcp_bridge: true,
                ..Default::default()
            }),
            host_context: None,
            projects: None,
            agent_protocol_version: Some("polling-v1".to_string()),
            policy: Some(AgentPolicySummary {
                mcp_bridge_providers: Some(providers),
                ..Default::default()
            }),
            process_started_at: None,
            build: None,
            job_concurrency_limit: None,
            job_inventory: None,
        })
        .await
        .unwrap();
}

async fn register_runner_with_owner(registry: &ShellClientRegistry, owner: Option<&str>) {
    register_runner_with_owner_and_providers(registry, owner, vec![default_bridge_provider()])
        .await;
}

async fn register_runner(registry: &ShellClientRegistry) {
    register_runner_with_owner(registry, None).await;
}

fn spawn_fake_runner(
    registry: Arc<ShellClientRegistry>,
    calls: Arc<AtomicUsize>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let request = registry
                .poll(ShellAgentPollRequest {
                    client_id: "bridge-http-runner".to_string(),
                    agent_instance_id: "bridge-http-instance".to_string(),
                    projects: None,
                })
                .await
                .unwrap();
            let Some(request) = request else {
                tokio::time::sleep(Duration::from_millis(1)).await;
                continue;
            };
            let response = match request.mcp_bridge.unwrap() {
                McpBridgeRequest::ToolsList { .. } => {
                    McpBridgeResponse::success(McpBridgeResponsePayload::Tools {
                        tools: vec![McpBridgeTool {
                            name: "echo".to_string(),
                            description: Some("Bounded echo".to_string()),
                            input_schema: json!({
                                "type": "object",
                                "properties": {"value": {"type": "string"}}
                            }),
                        }],
                    })
                }
                McpBridgeRequest::ToolsCall {
                    name, arguments, ..
                } => {
                    assert_eq!(name, "echo");
                    let call = calls.fetch_add(1, Ordering::SeqCst) + 1;
                    McpBridgeResponse::success(McpBridgeResponsePayload::ToolResult {
                        result: McpBridgeToolResult {
                            content: vec![McpBridgeContent::Text {
                                text: format!(
                                    "call-{call}:{}",
                                    arguments["value"].as_str().unwrap_or_default()
                                ),
                            }],
                            structured_content: Some(json!({"call": call})),
                            is_error: false,
                        },
                    })
                }
            };
            registry
                .complete(ShellAgentResultPayload {
                    result: ShellAgentResultRequest {
                        client_id: "bridge-http-runner".to_string(),
                        agent_instance_id: "bridge-http-instance".to_string(),
                        request_id: request.request_id,
                        exit_code: None,
                        stdout: None,
                        stderr: None,
                        duration_ms: None,
                        error: None,
                    },
                    command_execution_state: None,
                    mcp_bridge: Some(response),
                })
                .await
                .unwrap();
        }
    })
}

async fn rpc(service: &Service, endpoint: &str, id: u64, method: &str, params: Value) -> Value {
    let mut request = TestClient::post(format!("http://localhost{endpoint}")).add_header(
        "accept",
        "application/json, text/event-stream",
        true,
    );
    if method != "initialize" {
        request = request.add_header(MCP_PROTOCOL_VERSION_HEADER, MCP_PROTOCOL_VERSION, true);
    }
    let mut response = request
        .json(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))
        .send(service)
        .await;
    assert_eq!(
        response.status_code.unwrap_or(StatusCode::OK),
        StatusCode::OK
    );
    response.take_json::<Value>().await.unwrap()
}

#[test]
fn completed_invalid_provider_result_is_non_retryable() {
    let error = bridge_rpc_error(
        Some(json!(7)),
        McpBridgeResponse::error(
            McpBridgeDispatchState::Completed,
            "invalid_provider_result",
            "correlated provider result failed bounded V1 validation",
        ),
    );
    assert_eq!(error["error"]["data"]["dispatchState"], "completed");
    assert_eq!(error["error"]["data"]["retryable"], false);
    assert_eq!(error["error"]["data"]["reconciliationRequired"], false);
}

#[test]
fn bridge_id_binds_runner_and_provider_instances_independently() {
    let provider = default_bridge_provider();
    let base = opaque_bridge_id("runner", "runner-instance-a", &provider);
    assert_ne!(
        base,
        opaque_bridge_id("runner", "runner-instance-b", &provider),
        "Runner instance changes must produce a new hosted resource"
    );
    let replacement_provider = McpBridgeProvider {
        provider_instance_id: "provider-instance-b".to_string(),
        ..provider
    };
    assert_ne!(
        base,
        opaque_bridge_id("runner", "runner-instance-a", &replacement_provider),
        "provider instance changes must produce a new hosted resource"
    );
}

#[tokio::test]
async fn hosted_bridge_runs_initialize_list_and_repeated_calls_without_changing_mcp() {
    let config = test_config(None);
    let (_temp, db) = test_db();
    let registry = Arc::new(ShellClientRegistry::default());
    register_runner(&registry).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let service = Service::new(test_router(config, db, Arc::clone(&registry)));

    let mut discovery = tokio::time::timeout(
        Duration::from_millis(250),
        TestClient::get("http://localhost/mcp/bridge").send(&service),
    )
    .await
    .expect("registration-based bridge discovery must not wait for Runner RPC");
    assert_eq!(
        discovery.status_code.unwrap_or(StatusCode::OK),
        StatusCode::OK
    );
    let discovery = discovery.take_json::<Value>().await.unwrap();
    let endpoint = discovery["providers"][0]["endpoint"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(endpoint.starts_with("/mcp/bridge/wc_mcpb_"));
    assert!(!endpoint.contains("bridge-http-runner"));
    assert!(!endpoint.contains("local-test"));
    assert_eq!(registry.list_clients().await[0].pending_requests, 0);

    let exact_get = TestClient::get(format!("http://localhost{endpoint}"))
        .send(&service)
        .await;
    assert_eq!(
        exact_get.status_code.unwrap_or(StatusCode::OK),
        StatusCode::METHOD_NOT_ALLOWED,
        "exact Streamable HTTP endpoint must return 405 when SSE is unsupported"
    );

    let initialized = rpc(
        &service,
        &endpoint,
        1,
        "initialize",
        json!({
            "protocolVersion": MCP_PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {"name": "bridge-test-client", "version": "1"}
        }),
    )
    .await;
    assert_eq!(
        initialized["result"]["protocolVersion"],
        MCP_PROTOCOL_VERSION
    );
    assert!(initialized["result"]["capabilities"]["tools"].is_object());

    let mut latest_initialized = TestClient::post(format!("http://localhost{endpoint}"))
        .json(&json!({
            "jsonrpc":"2.0",
            "id":19,
            "method":"initialize",
            "params":{
                "protocolVersion": MCP_PROTOCOL_VERSION_2025_11_25,
                "capabilities": {"tasks": {}},
                "clientInfo": {"name": "chatgpt-compatible-test", "title": "ChatGPT", "version": "1"}
            }
        }))
        .send(&service)
        .await;
    assert_eq!(
        latest_initialized.status_code.unwrap_or(StatusCode::OK),
        StatusCode::OK
    );
    let latest_initialized = latest_initialized.take_json::<Value>().await.unwrap();
    assert_eq!(
        latest_initialized["result"]["protocolVersion"],
        MCP_PROTOCOL_VERSION_2025_11_25
    );

    let initialized_notification = TestClient::post(format!("http://localhost{endpoint}"))
        .json(&json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
        .send(&service)
        .await;
    assert_eq!(
        initialized_notification
            .status_code
            .unwrap_or(StatusCode::OK),
        StatusCode::ACCEPTED,
        "the exact initialized notification tolerates ChatGPT omitting the protocol header"
    );

    let mut wrong_notification_version = TestClient::post(format!("http://localhost{endpoint}"))
        .add_header(MCP_PROTOCOL_VERSION_HEADER, "2024-11-05", true)
        .json(&json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
        .send(&service)
        .await;
    assert_eq!(
        wrong_notification_version
            .status_code
            .unwrap_or(StatusCode::OK),
        StatusCode::BAD_REQUEST,
        "an explicitly unsupported notification protocol version must still fail closed"
    );
    let wrong_notification_version = wrong_notification_version
        .take_json::<Value>()
        .await
        .unwrap();
    assert_eq!(wrong_notification_version["error"]["code"], -32600);

    let mut latest_ping = TestClient::post(format!("http://localhost{endpoint}"))
        .add_header(
            MCP_PROTOCOL_VERSION_HEADER,
            MCP_PROTOCOL_VERSION_2025_11_25,
            true,
        )
        .json(&json!({"jsonrpc":"2.0","id":18,"method":"ping","params":{}}))
        .send(&service)
        .await;
    assert_eq!(
        latest_ping.status_code.unwrap_or(StatusCode::OK),
        StatusCode::OK
    );
    let latest_ping = latest_ping.take_json::<Value>().await.unwrap();
    assert!(latest_ping["result"].is_object());

    let mut unsupported_initialize = TestClient::post(format!("http://localhost{endpoint}"))
        .json(&json!({
            "jsonrpc":"2.0",
            "id":17,
            "method":"initialize",
            "params":{
                "protocolVersion":"2099-01-01",
                "capabilities":{},
                "clientInfo":{"name":"unsupported-test","version":"1"}
            }
        }))
        .send(&service)
        .await;
    assert_eq!(
        unsupported_initialize.status_code.unwrap_or(StatusCode::OK),
        StatusCode::OK
    );
    let unsupported_initialize = unsupported_initialize.take_json::<Value>().await.unwrap();
    assert_eq!(unsupported_initialize["error"]["code"], -32602);
    assert_eq!(
        unsupported_initialize["error"]["data"]["supportedProtocolVersion"],
        MCP_LATEST_PROTOCOL_VERSION
    );

    let mut missing_version = TestClient::post(format!("http://localhost{endpoint}"))
        .json(&json!({"jsonrpc":"2.0","id":20,"method":"ping","params":{}}))
        .send(&service)
        .await;
    assert_eq!(
        missing_version.status_code.unwrap_or(StatusCode::OK),
        StatusCode::BAD_REQUEST
    );
    let missing_version = missing_version.take_json::<Value>().await.unwrap();
    assert_eq!(missing_version["error"]["code"], -32600);

    let mut unknown_envelope = TestClient::post(format!("http://localhost{endpoint}"))
        .add_header(MCP_PROTOCOL_VERSION_HEADER, MCP_PROTOCOL_VERSION, true)
        .json(&json!({
            "jsonrpc":"2.0",
            "id":21,
            "method":"ping",
            "params":{},
            "raw_jsonrpc":{}
        }))
        .send(&service)
        .await;
    assert_eq!(
        unknown_envelope.status_code.unwrap_or(StatusCode::OK),
        StatusCode::BAD_REQUEST
    );
    let unknown_envelope = unknown_envelope.take_json::<Value>().await.unwrap();
    assert_eq!(unknown_envelope["error"]["code"], -32600);

    let oversized = format!(
        r#"{{"jsonrpc":"2.0","id":22,"method":"ping","params":{{}},"junk":"{}"}}"#,
        "x".repeat(MCP_BRIDGE_MAX_MESSAGE_BYTES)
    );
    let oversized = TestClient::post(format!("http://localhost{endpoint}"))
        .add_header("content-type", "application/json", true)
        .add_header(MCP_PROTOCOL_VERSION_HEADER, MCP_PROTOCOL_VERSION, true)
        .body(oversized)
        .send(&service)
        .await;
    assert_eq!(
        oversized.status_code.unwrap_or(StatusCode::OK),
        StatusCode::PAYLOAD_TOO_LARGE
    );

    let runner = spawn_fake_runner(Arc::clone(&registry), Arc::clone(&calls));
    let listed = rpc(&service, &endpoint, 2, "tools/list", json!({})).await;
    assert_eq!(listed["result"]["tools"][0]["name"], "echo");

    let mut latest_list = TestClient::post(format!("http://localhost{endpoint}"))
        .add_header(
            MCP_PROTOCOL_VERSION_HEADER,
            MCP_PROTOCOL_VERSION_2025_11_25,
            true,
        )
        .json(&json!({"jsonrpc":"2.0","id":16,"method":"tools/list","params":{}}))
        .send(&service)
        .await;
    assert_eq!(
        latest_list.status_code.unwrap_or(StatusCode::OK),
        StatusCode::OK
    );
    let latest_list = latest_list.take_json::<Value>().await.unwrap();
    assert_eq!(latest_list["result"]["tools"][0]["name"], "echo");

    for (id, expected) in [(3, "call-1:a"), (4, "call-2:b")] {
        let value = if id == 3 { "a" } else { "b" };
        let called = rpc(
            &service,
            &endpoint,
            id,
            "tools/call",
            json!({"name": "echo", "arguments": {"value": value}}),
        )
        .await;
        assert_eq!(called["result"]["content"][0]["text"], expected);
    }
    assert_eq!(calls.load(Ordering::SeqCst), 2);

    let called_with_meta = rpc(
        &service,
        &endpoint,
        15,
        "tools/call",
        json!({
            "name": "echo",
            "arguments": {"value": "with-meta"},
            "_meta": {"progressToken": "chatgpt-tool-call"}
        }),
    )
    .await;
    assert_eq!(
        called_with_meta["result"]["content"][0]["text"],
        "call-3:with-meta"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 3);

    let unsupported = rpc(&service, &endpoint, 5, "resources/list", json!({})).await;
    assert_eq!(unsupported["error"]["code"], -32601);

    let existing = TestClient::get("http://localhost/mcp").send(&service).await;
    assert_eq!(
        existing.status_code.unwrap_or(StatusCode::OK),
        StatusCode::OK,
        "the pre-existing /mcp surface must remain independently mounted"
    );
    runner.abort();
}

#[tokio::test]
async fn hosted_bridge_respects_restricted_authority_before_tool_dispatch() {
    let config = test_config(None);
    let (_temp, db) = test_db();
    let registry = Arc::new(ShellClientRegistry::default());
    register_runner(&registry).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let runner = spawn_fake_runner(Arc::clone(&registry), Arc::clone(&calls));
    let runtime = ToolRuntime::new_for_tests_with_shell_clients(Arc::clone(&registry))
        .with_permission_evaluator(
            crate::tool_runtime::permissions::PermissionEvaluator::with_mode(
                crate::tool_runtime::permissions::AuthorityMode::Restricted,
            ),
        );
    let service = Service::new(test_router_with_runtime(
        config,
        db,
        Arc::clone(&registry),
        Arc::new(runtime),
    ));

    let mut discovery = TestClient::get("http://localhost/mcp/bridge")
        .send(&service)
        .await;
    let discovery = discovery.take_json::<Value>().await.unwrap();
    let endpoint = discovery["providers"][0]["endpoint"]
        .as_str()
        .unwrap()
        .to_string();
    let _ = rpc(
        &service,
        &endpoint,
        1,
        "initialize",
        json!({
            "protocolVersion": MCP_PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {"name": "restricted-test", "version": "1"}
        }),
    )
    .await;

    let mut denied = TestClient::post(format!("http://localhost{endpoint}"))
        .add_header(MCP_PROTOCOL_VERSION_HEADER, MCP_PROTOCOL_VERSION, true)
        .json(&json!({
            "jsonrpc":"2.0",
            "id":2,
            "method":"tools/call",
            "params":{"name":"echo","arguments":{"value":"blocked"}}
        }))
        .send(&service)
        .await;
    assert_eq!(
        denied.status_code.unwrap_or(StatusCode::OK),
        StatusCode::FORBIDDEN
    );
    let denied = denied.take_json::<Value>().await.unwrap();
    assert_eq!(denied["error"]["data"]["dispatchState"], "not_started");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    runner.abort();
}

fn seed_oauth_token(
    db: &crate::Database,
    client: &crate::models::OAuthClientRecord,
    user: &crate::models::UserRecord,
    scopes: &str,
    resource: Option<&str>,
) -> String {
    let token = crate::auth::generate_oauth_access_token();
    let now = chrono::Utc::now().timestamp();
    db.insert_oauth_access_token(&crate::models::OAuthAccessTokenRecord {
        id: uuid::Uuid::new_v4().to_string(),
        token_hash: crate::auth::hash_token(&token),
        client_id: client.client_id.clone(),
        subject_kind: "managed_user".to_string(),
        subject_id: user.id.clone(),
        user_id: Some(user.id.clone()),
        scopes: scopes.to_string(),
        resource: resource.map(str::to_string),
        shared_key_hash: None,
        created_at: now,
        expires_at: now + 3600,
        revoked_at: None,
        last_used_at: None,
    })
    .unwrap();
    token
}

fn seed_pat(db: &crate::Database, user: &crate::models::UserRecord, scopes: &str) -> String {
    let token = crate::auth::generate_api_token();
    let now = chrono::Utc::now().timestamp();
    db.insert_api_key(
        &crate::models::ApiKeyRecord {
            id: uuid::Uuid::new_v4().to_string(),
            user_id: user.id.clone(),
            name: "hosted-bridge-test".to_string(),
            key_prefix: crate::auth::token_prefix(&token),
            created_at: now,
            last_used_at: None,
            revoked_at: None,
            scopes: scopes.to_string(),
            expires_at: None,
            kind: crate::models::TOKEN_KIND_USER.to_string(),
            allowed_client_id: None,
        },
        &crate::auth::hash_token(&token),
    )
    .unwrap();
    token
}

#[tokio::test]
async fn hosted_bridge_requires_auth_and_the_fixed_bridge_scope() {
    let _auth = crate::auth::AuthEnvGuard::auth_required();
    let config = test_config_oauth2(Some("bootstrap-secret"));
    let (_temp, db) = test_db();
    let registry = Arc::new(ShellClientRegistry::default());
    let user = seed_user(&db, "alice");
    let client = seed_oauth_client(&db, &user);
    let runtime_only = seed_oauth_token(&db, &client, &user, crate::auth::SCOPE_RUNTIME_READ, None);
    let service = Service::new(test_router(config, db, registry));

    let unauthenticated = TestClient::get("http://localhost/mcp/bridge")
        .send(&service)
        .await;
    assert_eq!(
        unauthenticated.status_code.unwrap_or(StatusCode::OK),
        StatusCode::UNAUTHORIZED
    );

    let insufficient = TestClient::get("http://localhost/mcp/bridge")
        .bearer_auth(runtime_only)
        .send(&service)
        .await;
    assert_eq!(
        insufficient.status_code.unwrap_or(StatusCode::OK),
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn hosted_bridge_oauth_audience_is_exact_without_affecting_first_party_credentials() {
    let _auth = crate::auth::AuthEnvGuard::auth_required();
    let mut config = test_config_oauth2(Some("bootstrap-secret"));
    Arc::get_mut(&mut config).unwrap().oauth2.issuer =
        Some("https://codex.example.com".to_string());
    let (_temp, db) = test_db();
    let registry = Arc::new(ShellClientRegistry::default());
    let providers = vec![
        McpBridgeProvider {
            provider_id: "provider-a".to_string(),
            provider_instance_id: "instance-a".to_string(),
            name: "Provider A".to_string(),
        },
        McpBridgeProvider {
            provider_id: "provider-b".to_string(),
            provider_instance_id: "instance-b".to_string(),
            name: "Provider B".to_string(),
        },
    ];
    register_runner_with_owner_and_providers(&registry, Some("alice"), providers.clone()).await;
    let bridge_a = opaque_bridge_id("bridge-http-runner", "bridge-http-instance", &providers[0]);
    let bridge_b = opaque_bridge_id("bridge-http-runner", "bridge-http-instance", &providers[1]);
    let endpoint_a = format!("/mcp/bridge/{bridge_a}");
    let endpoint_b = format!("/mcp/bridge/{bridge_b}");
    let resource_a = format!("https://codex.example.com{endpoint_a}");
    let resource_mcp = "https://codex.example.com/mcp";
    let user = seed_user(&db, "alice");
    let client = seed_oauth_client(&db, &user);
    let pat = seed_pat(&db, &user, crate::auth::SCOPE_MCP_BRIDGE);
    let token_a = seed_oauth_token(
        &db,
        &client,
        &user,
        &format!(
            "{} {}",
            crate::auth::SCOPE_RUNTIME_READ,
            crate::auth::SCOPE_MCP_BRIDGE
        ),
        Some(&resource_a),
    );
    let mcp_token = seed_oauth_token(
        &db,
        &client,
        &user,
        &format!(
            "{} {}",
            crate::auth::SCOPE_RUNTIME_READ,
            crate::auth::SCOPE_MCP_BRIDGE
        ),
        Some(resource_mcp),
    );
    let service = Service::new(test_router(config, db, registry));

    let matching = TestClient::get(format!("http://localhost{endpoint_a}"))
        .bearer_auth(&token_a)
        .send(&service)
        .await;
    assert_eq!(matching.status_code, Some(StatusCode::METHOD_NOT_ALLOWED));

    let bridge_b_rejected = TestClient::get(format!("http://localhost{endpoint_b}"))
        .bearer_auth(&token_a)
        .send(&service)
        .await;
    assert_eq!(
        bridge_b_rejected.status_code,
        Some(StatusCode::UNAUTHORIZED)
    );
    let bridge_b_challenge = bridge_b_rejected
        .headers
        .get("www-authenticate")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    assert!(bridge_b_challenge.contains("error=\"invalid_token\""));
    assert!(bridge_b_challenge.contains(&format!(
        "https://codex.example.com/.well-known/oauth-protected-resource/mcp/bridge/{bridge_b}"
    )));

    let generic_mcp_rejected = TestClient::get(format!("http://localhost{endpoint_a}"))
        .bearer_auth(&mcp_token)
        .send(&service)
        .await;
    assert_eq!(
        generic_mcp_rejected.status_code,
        Some(StatusCode::UNAUTHORIZED)
    );

    let exact_token_on_ordinary_mcp = TestClient::get("http://localhost/mcp")
        .bearer_auth(&token_a)
        .send(&service)
        .await;
    assert_eq!(
        exact_token_on_ordinary_mcp.status_code,
        Some(StatusCode::UNAUTHORIZED),
        "an exact bridge audience must not widen to ordinary /mcp even when the token also carries runtime:read"
    );

    let exact_token_on_bridge_collection = TestClient::get("http://localhost/mcp/bridge")
        .bearer_auth(&token_a)
        .send(&service)
        .await;
    assert_eq!(
        exact_token_on_bridge_collection.status_code,
        Some(StatusCode::UNAUTHORIZED),
        "an exact bridge audience must not authorize the provider collection"
    );

    let ordinary_mcp = TestClient::get("http://localhost/mcp")
        .bearer_auth(&mcp_token)
        .send(&service)
        .await;
    assert_eq!(
        ordinary_mcp.status_code,
        Some(StatusCode::OK),
        "ordinary /mcp audience behavior must remain regression-compatible"
    );

    let unauthenticated = TestClient::get(format!("http://localhost{endpoint_a}"))
        .send(&service)
        .await;
    assert_eq!(unauthenticated.status_code, Some(StatusCode::UNAUTHORIZED));
    let challenge = unauthenticated
        .headers
        .get("www-authenticate")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    assert!(challenge.contains(&format!(
        "https://codex.example.com/.well-known/oauth-protected-resource/mcp/bridge/{bridge_a}"
    )));

    let bootstrap = TestClient::get(format!("http://localhost{endpoint_a}"))
        .bearer_auth("bootstrap-secret")
        .send(&service)
        .await;
    assert_eq!(
        bootstrap.status_code,
        Some(StatusCode::METHOD_NOT_ALLOWED),
        "first-party bootstrap credentials must not be subjected to OAuth audience binding"
    );
    let pat = TestClient::get(format!("http://localhost{endpoint_a}"))
        .bearer_auth(pat)
        .send(&service)
        .await;
    assert_eq!(
        pat.status_code,
        Some(StatusCode::METHOD_NOT_ALLOWED),
        "first-party PAT credentials must not be subjected to OAuth audience binding"
    );
}
