use super::*;
use crate::auth::{AuthContext, AuthKind};
use crate::mcp_bridge::{
    McpBridgeDispatchState, McpBridgeProvider, McpBridgeRequest, McpBridgeResponse,
    McpBridgeResponsePayload,
};
use crate::shell_protocol::{
    AgentPolicySummary, ShellAgentPollRequest, ShellAgentResultPayload, ShellAgentResultRequest,
    ShellClientRegisterRequest,
};

fn bridge_provider(provider_instance_id: &str) -> McpBridgeProvider {
    McpBridgeProvider {
        provider_id: "provider".to_string(),
        provider_instance_id: provider_instance_id.to_string(),
        name: "Provider".to_string(),
    }
}

async fn register_bridge_runner(registry: &ShellClientRegistry) {
    registry
        .register(ShellClientRegisterRequest {
            client_id: "bridge-runner".to_string(),
            agent_instance_id: "bridge-instance".to_string(),
            display_name: None,
            owner: Some("alice".to_string()),
            hostname: None,
            capabilities: Some(ShellClientCapabilities {
                mcp_bridge: true,
                ..Default::default()
            }),
            host_context: None,
            projects: None,
            agent_protocol_version: Some("polling-v1".to_string()),
            policy: Some(AgentPolicySummary {
                mcp_bridge_providers: Some(vec![bridge_provider("provider-instance")]),
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

fn list_request(provider_instance_id: &str) -> McpBridgeRequest {
    McpBridgeRequest::ToolsList {
        provider_id: "provider".to_string(),
        provider_instance_id: provider_instance_id.to_string(),
    }
}

fn bridge_registration(
    client_id: &str,
    agent_instance_id: &str,
    providers: Option<Vec<McpBridgeProvider>>,
    capability: bool,
) -> ShellClientRegisterRequest {
    ShellClientRegisterRequest {
        client_id: client_id.to_string(),
        agent_instance_id: agent_instance_id.to_string(),
        display_name: None,
        owner: Some("alice".to_string()),
        hostname: None,
        capabilities: Some(ShellClientCapabilities {
            mcp_bridge: capability,
            ..Default::default()
        }),
        host_context: None,
        projects: None,
        agent_protocol_version: Some("polling-v1".to_string()),
        policy: providers.map(|providers| AgentPolicySummary {
            mcp_bridge_providers: Some(providers),
            ..Default::default()
        }),
        process_started_at: None,
        build: None,
        job_concurrency_limit: None,
        job_inventory: None,
    }
}

#[tokio::test]
async fn bridge_registration_inventory_is_bounded_and_exact() {
    let registry = ShellClientRegistry::default();
    registry
        .register(bridge_registration(
            "valid-bridge-runner",
            "valid-instance",
            Some(vec![bridge_provider("provider-instance")]),
            true,
        ))
        .await
        .unwrap();
    let view = registry
        .get_client_view("valid-bridge-runner")
        .await
        .unwrap();
    assert_eq!(
        view.policy
            .as_ref()
            .and_then(|policy| policy.mcp_bridge_providers.as_ref())
            .unwrap(),
        &vec![bridge_provider("provider-instance")]
    );

    for (name, providers) in [
        (
            "duplicate-id",
            vec![
                bridge_provider("provider-instance-a"),
                McpBridgeProvider {
                    provider_id: "provider".to_string(),
                    provider_instance_id: "provider-instance-b".to_string(),
                    name: "Provider B".to_string(),
                },
            ],
        ),
        (
            "duplicate-instance",
            vec![
                bridge_provider("provider-instance"),
                McpBridgeProvider {
                    provider_id: "provider-b".to_string(),
                    provider_instance_id: "provider-instance".to_string(),
                    name: "Provider B".to_string(),
                },
            ],
        ),
    ] {
        assert!(registry
            .register(bridge_registration(
                name,
                &format!("{name}-instance"),
                Some(providers),
                true,
            ))
            .await
            .unwrap_err()
            .contains("invalid MCP bridge provider inventory"));
    }

    let excessive = (0..=crate::mcp_bridge::MCP_BRIDGE_MAX_PROVIDERS)
        .map(|index| McpBridgeProvider {
            provider_id: format!("provider-{index}"),
            provider_instance_id: format!("instance-{index}"),
            name: format!("Provider {index}"),
        })
        .collect();
    assert!(registry
        .register(bridge_registration(
            "excessive",
            "excessive-instance",
            Some(excessive),
            true,
        ))
        .await
        .is_err());
    assert!(registry
        .register(bridge_registration(
            "malformed",
            "malformed-instance",
            Some(vec![McpBridgeProvider {
                provider_id: "Bad Provider".to_string(),
                provider_instance_id: "instance".to_string(),
                name: "Bad".to_string(),
            }]),
            true,
        ))
        .await
        .is_err());
    assert!(registry
        .register(bridge_registration(
            "missing-inventory",
            "missing-inventory-instance",
            None,
            true,
        ))
        .await
        .unwrap_err()
        .contains("requires a registered provider inventory"));
}

#[tokio::test]
async fn bridge_enqueue_rechecks_owner_and_exact_runner_instance() {
    let registry = ShellClientRegistry::default();
    register_bridge_runner(&registry).await;

    let mut bob = AuthContext::new(AuthKind::ApiToken);
    bob.username = Some("bob".to_string());
    assert!(registry
        .enqueue_mcp_bridge(
            "bridge-runner",
            "bridge-instance",
            list_request("provider-instance"),
            Some(&bob),
            "bob".to_string(),
        )
        .await
        .unwrap_err()
        .contains("unavailable"));

    let mut alice = AuthContext::new(AuthKind::ApiToken);
    alice.username = Some("alice".to_string());
    assert!(registry
        .enqueue_mcp_bridge(
            "bridge-runner",
            "stale-instance",
            list_request("provider-instance"),
            Some(&alice),
            "alice".to_string(),
        )
        .await
        .unwrap_err()
        .contains("stale Runner"));

    assert!(registry
        .enqueue_mcp_bridge(
            "bridge-runner",
            "bridge-instance",
            list_request("stale-provider-instance"),
            Some(&alice),
            "alice".to_string(),
        )
        .await
        .unwrap_err()
        .contains("stale provider"));

    let inner = registry.inner.lock().await;
    assert!(inner.pending_by_id.is_empty());
    assert!(inner.mcp_bridge_waiters.is_empty());
}

#[tokio::test]
async fn bridge_dequeue_rechecks_exact_runner_instance_after_replacement() {
    let registry = ShellClientRegistry::default();
    register_bridge_runner(&registry).await;
    let mut alice = AuthContext::new(AuthKind::ApiToken);
    alice.username = Some("alice".to_string());
    let (_request_id, receiver) = registry
        .enqueue_mcp_bridge(
            "bridge-runner",
            "bridge-instance",
            list_request("provider-instance"),
            Some(&alice),
            "test".to_string(),
        )
        .await
        .unwrap();

    // Simulate the narrow invariant violation between admission and dequeue.
    // Normal replacement registration already drains synchronous requests, but
    // dequeue itself must carry the exact process fence rather than relying on
    // that separate lifecycle path forever.
    {
        let mut inner = registry.inner.lock().await;
        inner
            .clients
            .get_mut("bridge-runner")
            .unwrap()
            .agent_instance_id = "replacement-instance".to_string();
    }
    let polled = registry
        .poll(ShellAgentPollRequest {
            client_id: "bridge-runner".to_string(),
            agent_instance_id: "replacement-instance".to_string(),
            projects: None,
        })
        .await
        .unwrap();
    assert!(
        polled.is_none(),
        "replacement Runner must not receive stale bridge work"
    );
    let response = receiver.await.unwrap();
    assert_eq!(response.dispatch_state, McpBridgeDispatchState::NotStarted);
    assert_eq!(response.error.as_ref().unwrap().code, "stale_runner");
}

#[tokio::test]
async fn bridge_dequeue_rechecks_exact_provider_instance_after_inventory_change() {
    let registry = ShellClientRegistry::default();
    register_bridge_runner(&registry).await;
    let mut alice = AuthContext::new(AuthKind::ApiToken);
    alice.username = Some("alice".to_string());
    let (_request_id, receiver) = registry
        .enqueue_mcp_bridge(
            "bridge-runner",
            "bridge-instance",
            list_request("provider-instance"),
            Some(&alice),
            "test".to_string(),
        )
        .await
        .unwrap();

    {
        let mut inner = registry.inner.lock().await;
        inner
            .clients
            .get_mut("bridge-runner")
            .unwrap()
            .policy
            .as_mut()
            .unwrap()
            .mcp_bridge_providers = Some(vec![bridge_provider("replacement-provider-instance")]);
    }
    let polled = registry
        .poll(ShellAgentPollRequest {
            client_id: "bridge-runner".to_string(),
            agent_instance_id: "bridge-instance".to_string(),
            projects: None,
        })
        .await
        .unwrap();
    assert!(
        polled.is_none(),
        "changed provider lease must not receive stale work"
    );
    let response = receiver.await.unwrap();
    assert_eq!(response.dispatch_state, McpBridgeDispatchState::NotStarted);
    assert_eq!(response.error.as_ref().unwrap().code, "stale_provider");
}

#[tokio::test]
async fn dispatched_bridge_disconnect_is_outcome_unknown_and_not_replayed() {
    let registry = ShellClientRegistry::default();
    register_bridge_runner(&registry).await;
    let mut alice = AuthContext::new(AuthKind::ApiToken);
    alice.username = Some("alice".to_string());
    let (request_id, receiver) = registry
        .enqueue_mcp_bridge(
            "bridge-runner",
            "bridge-instance",
            McpBridgeRequest::ToolsCall {
                provider_id: "provider".to_string(),
                provider_instance_id: "provider-instance".to_string(),
                name: "effect".to_string(),
                arguments: serde_json::json!({}),
            },
            Some(&alice),
            "test".to_string(),
        )
        .await
        .unwrap();
    let request = registry
        .poll(ShellAgentPollRequest {
            client_id: "bridge-runner".to_string(),
            agent_instance_id: "bridge-instance".to_string(),
            projects: None,
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(request.request_id, request_id);
    assert!(request.mcp_bridge.is_some());

    registry
        .reconcile_disconnect("bridge-runner", "bridge-instance")
        .await;
    let response = receiver.await.unwrap();
    assert_eq!(
        response.dispatch_state,
        McpBridgeDispatchState::OutcomeUnknown
    );
    assert_eq!(response.error.as_ref().unwrap().code, "runner_unavailable");

    let inner = registry.inner.lock().await;
    assert!(inner.pending_by_id.is_empty());
    assert!(inner.mcp_bridge_waiters.is_empty());
    assert!(inner
        .queues_by_client
        .get("bridge-runner")
        .is_none_or(|queue| queue.is_empty()));
}

#[tokio::test]
async fn typed_bridge_result_is_correlated_once() {
    let registry = ShellClientRegistry::default();
    register_bridge_runner(&registry).await;
    let mut alice = AuthContext::new(AuthKind::ApiToken);
    alice.username = Some("alice".to_string());
    let (request_id, receiver) = registry
        .enqueue_mcp_bridge(
            "bridge-runner",
            "bridge-instance",
            list_request("provider-instance"),
            Some(&alice),
            "test".to_string(),
        )
        .await
        .unwrap();
    registry
        .poll(ShellAgentPollRequest {
            client_id: "bridge-runner".to_string(),
            agent_instance_id: "bridge-instance".to_string(),
            projects: None,
        })
        .await
        .unwrap()
        .unwrap();
    let payload = ShellAgentResultPayload {
        result: ShellAgentResultRequest {
            client_id: "bridge-runner".to_string(),
            agent_instance_id: "bridge-instance".to_string(),
            request_id: request_id.clone(),
            exit_code: None,
            stdout: None,
            stderr: None,
            duration_ms: None,
            error: None,
        },
        command_execution_state: None,
        mcp_bridge: Some(McpBridgeResponse::success(
            McpBridgeResponsePayload::Tools { tools: Vec::new() },
        )),
    };
    registry.complete(payload.clone()).await.unwrap();
    let response = receiver.await.unwrap();
    assert!(matches!(
        response.payload,
        Some(McpBridgeResponsePayload::Tools { .. })
    ));
    assert!(registry
        .complete(payload)
        .await
        .unwrap_err()
        .contains("unknown"));
}
