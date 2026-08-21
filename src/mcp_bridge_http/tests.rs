use super::*;
use crate::mcp_bridge::{McpBridgeContent, McpBridgeTool, McpBridgeToolResult};
use crate::shell_protocol::{
    ShellAgentPollRequest, ShellAgentResultPayload, ShellAgentResultRequest,
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

async fn register_runner(registry: &ShellClientRegistry) {
    registry
        .register(ShellClientRegisterRequest {
            client_id: "bridge-http-runner".to_string(),
            agent_instance_id: "bridge-http-instance".to_string(),
            display_name: None,
            owner: None,
            hostname: None,
            capabilities: Some(ShellClientCapabilities {
                mcp_bridge: true,
                ..Default::default()
            }),
            host_context: None,
            projects: None,
            agent_protocol_version: Some("polling-v1".to_string()),
            policy: None,
            process_started_at: None,
            build: None,
            job_concurrency_limit: None,
            job_inventory: None,
        })
        .await
        .unwrap();
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
                McpBridgeRequest::Discover => {
                    McpBridgeResponse::success(McpBridgeResponsePayload::Providers {
                        providers: vec![McpBridgeProvider {
                            provider_id: "local-test".to_string(),
                            provider_instance_id: "provider-instance".to_string(),
                            name: "Local test provider".to_string(),
                            available: true,
                        }],
                    })
                }
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
    let mut response = TestClient::post(format!("http://localhost{endpoint}"))
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

#[tokio::test]
async fn hosted_bridge_runs_initialize_list_and_repeated_calls_without_changing_mcp() {
    let config = test_config(None);
    let (_temp, db) = test_db();
    let registry = Arc::new(ShellClientRegistry::default());
    register_runner(&registry).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let runner = spawn_fake_runner(Arc::clone(&registry), Arc::clone(&calls));
    let service = Service::new(test_router(config, db, registry));

    let mut discovery = TestClient::get("http://localhost/mcp/bridge")
        .send(&service)
        .await;
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

    let initialized = rpc(
        &service,
        &endpoint,
        1,
        "initialize",
        json!({"protocolVersion": MCP_PROTOCOL_VERSION}),
    )
    .await;
    assert_eq!(
        initialized["result"]["protocolVersion"],
        MCP_PROTOCOL_VERSION
    );
    assert!(initialized["result"]["capabilities"]["tools"].is_object());

    let listed = rpc(&service, &endpoint, 2, "tools/list", json!({})).await;
    assert_eq!(listed["result"]["tools"][0]["name"], "echo");

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

fn seed_oauth_token(
    db: &crate::Database,
    client: &crate::models::OAuthClientRecord,
    user: &crate::models::UserRecord,
    scopes: &str,
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
        resource: None,
        shared_key_hash: None,
        created_at: now,
        expires_at: now + 3600,
        revoked_at: None,
        last_used_at: None,
    })
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
    let runtime_only = seed_oauth_token(&db, &client, &user, crate::auth::SCOPE_RUNTIME_READ);
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
