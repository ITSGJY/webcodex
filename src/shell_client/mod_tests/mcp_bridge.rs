use super::*;
use crate::auth::{AuthContext, AuthKind};
use crate::mcp_bridge::{
    McpBridgeDispatchState, McpBridgeProvider, McpBridgeRequest, McpBridgeResponse,
    McpBridgeResponsePayload,
};
use crate::shell_protocol::{
    ShellAgentPollRequest, ShellAgentResultPayload, ShellAgentResultRequest,
    ShellClientRegisterRequest,
};

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
            policy: None,
            process_started_at: None,
            build: None,
            job_concurrency_limit: None,
            job_inventory: None,
        })
        .await
        .unwrap();
}

fn discover_request() -> McpBridgeRequest {
    McpBridgeRequest::Discover
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
            discover_request(),
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
            discover_request(),
            Some(&alice),
            "alice".to_string(),
        )
        .await
        .unwrap_err()
        .contains("stale Runner"));

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
            discover_request(),
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
            discover_request(),
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
            McpBridgeResponsePayload::Providers {
                providers: vec![McpBridgeProvider {
                    provider_id: "provider".to_string(),
                    provider_instance_id: "provider-instance".to_string(),
                    name: "Provider".to_string(),
                    available: true,
                }],
            },
        )),
    };
    registry.complete(payload.clone()).await.unwrap();
    let response = receiver.await.unwrap();
    assert!(matches!(
        response.payload,
        Some(McpBridgeResponsePayload::Providers { .. })
    ));
    assert!(registry
        .complete(payload)
        .await
        .unwrap_err()
        .contains("unknown"));
}
