//! Hosted Streamable HTTP MCP facade for exact Runner-owned stdio providers.
//!
//! The hosted surface is deliberately tool-only and stateless. Provider
//! discovery is a separate authenticated JSON endpoint at `/mcp/bridge`; each
//! opaque provider resource lives at `/mcp/bridge/{bridge_id}`.

use crate::auth::AuthContext;
use crate::mcp_bridge::{
    validate_json_value, validate_request, McpBridgeDispatchState, McpBridgeProvider,
    McpBridgeRequest, McpBridgeResponse, McpBridgeResponsePayload, MCP_BRIDGE_MAX_MESSAGE_BYTES,
};
use crate::shell_client::requested_by_from_auth;
use crate::shell_client::ShellClientRegistry;
use futures_util::future::join_all;
use salvo::prelude::*;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::time::Duration;

const MCP_PROTOCOL_VERSION: &str = "2025-06-18";
const BRIDGE_ID_PREFIX: &str = "wc_mcpb_";
const MAX_DISCOVERY_RUNNERS: usize = 16;
const MAX_HOSTED_PROVIDERS: usize = 64;
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(5);
const BRIDGE_CALL_TIMEOUT: Duration = Duration::from_secs(125);

#[derive(Debug, Deserialize)]
struct JsonRpcRequest {
    #[serde(default)]
    jsonrpc: Option<String>,
    method: String,
    #[serde(default = "empty_object")]
    params: Value,
    #[serde(default)]
    id: Option<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolCallParams {
    name: String,
    #[serde(default = "empty_object")]
    arguments: Value,
}

fn empty_object() -> Value {
    json!({})
}

#[derive(Clone)]
struct BridgeTarget {
    bridge_id: String,
    client_id: String,
    agent_instance_id: String,
    provider: McpBridgeProvider,
}

#[handler]
pub async fn bridge_list(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    if let Err((status, _, message)) = crate::auth::require_same_origin(req) {
        let status = StatusCode::from_u16(status).unwrap_or(StatusCode::FORBIDDEN);
        res.status_code(status);
        res.render(crate::json_error(status, message));
        return;
    }
    let Some(registry) = registry(depot) else {
        render_http_error(
            res,
            StatusCode::INTERNAL_SERVER_ERROR,
            "MCP bridge registry is unavailable",
        );
        return;
    };
    let auth = depot.obtain::<AuthContext>().ok().cloned();
    match discover_targets(&registry, auth.as_ref()).await {
        Ok(targets) => {
            let providers = targets
                .into_iter()
                .map(|target| {
                    json!({
                        "bridge_id": target.bridge_id,
                        "name": target.provider.name,
                        "available": target.provider.available,
                        "endpoint": format!("/mcp/bridge/{}", target.bridge_id),
                    })
                })
                .collect::<Vec<_>>();
            res.render(Json(json!({
                "providers": providers,
                "scope": crate::auth::SCOPE_MCP_BRIDGE,
                "v1": {
                    "supported": ["initialize", "notifications/initialized", "ping", "tools/list", "tools/call"],
                    "unsupported": ["resources", "prompts", "sampling", "elicitation", "roots", "completion", "subscriptions", "server callbacks", "MCP Apps", "SSE"]
                }
            })));
        }
        Err(error) => render_http_error(res, StatusCode::SERVICE_UNAVAILABLE, error),
    }
}

#[handler]
pub async fn bridge_info(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    if let Err((status, _, message)) = crate::auth::require_same_origin(req) {
        let status = StatusCode::from_u16(status).unwrap_or(StatusCode::FORBIDDEN);
        res.status_code(status);
        res.render(crate::json_error(status, message));
        return;
    }
    let Some(bridge_id) = bridge_id_param(req) else {
        render_http_error(res, StatusCode::NOT_FOUND, "MCP bridge resource not found");
        return;
    };
    let Some(registry) = registry(depot) else {
        render_http_error(
            res,
            StatusCode::INTERNAL_SERVER_ERROR,
            "MCP bridge registry is unavailable",
        );
        return;
    };
    let auth = depot.obtain::<AuthContext>().ok().cloned();
    let target = match resolve_target(&registry, auth.as_ref(), &bridge_id).await {
        Ok(Some(target)) => target,
        Ok(None) => {
            render_http_error(res, StatusCode::NOT_FOUND, "MCP bridge resource not found");
            return;
        }
        Err(error) => {
            render_http_error(res, StatusCode::SERVICE_UNAVAILABLE, error);
            return;
        }
    };
    res.render(Json(json!({
        "name": target.provider.name,
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": "mcp",
        "protocolVersion": MCP_PROTOCOL_VERSION,
        "transport": "streamable-http-jsonrpc",
        "endpoint": format!("/mcp/bridge/{}", target.bridge_id),
        "methods": ["initialize", "notifications/initialized", "ping", "tools/list", "tools/call"],
        "available": target.provider.available,
        "auth": {
            "type": "bearer",
            "scope": crate::auth::SCOPE_MCP_BRIDGE
        }
    })));
}

#[handler]
pub async fn bridge_post(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    if let Err((status, _, message)) = crate::auth::require_json_same_origin(req) {
        let status = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_REQUEST);
        res.status_code(status);
        res.render(crate::json_error(status, message));
        return;
    }
    let Some(bridge_id) = bridge_id_param(req) else {
        render_rpc(
            res,
            StatusCode::NOT_FOUND,
            rpc_error(None, -32601, "MCP bridge resource not found", None),
        );
        return;
    };
    let request: JsonRpcRequest = match req.parse_json().await {
        Ok(request) => request,
        Err(_) => {
            render_rpc(
                res,
                StatusCode::BAD_REQUEST,
                rpc_error(None, -32700, "Parse error", None),
            );
            return;
        }
    };
    if serde_json::to_vec(&json!({
        "jsonrpc": &request.jsonrpc,
        "method": &request.method,
        "params": &request.params,
        "id": &request.id
    }))
    .map_or(true, |encoded| encoded.len() > MCP_BRIDGE_MAX_MESSAGE_BYTES)
    {
        render_rpc(
            res,
            StatusCode::BAD_REQUEST,
            rpc_error(
                request.id,
                -32600,
                "MCP bridge request exceeds the bounded message limit",
                None,
            ),
        );
        return;
    }
    if request.jsonrpc.as_deref() != Some("2.0") {
        render_rpc(
            res,
            StatusCode::BAD_REQUEST,
            rpc_error(request.id, -32600, "jsonrpc must be '2.0'", None),
        );
        return;
    }
    if request.method.is_empty()
        || request.method.len() > 128
        || request.method.chars().any(char::is_control)
        || !request.params.is_object()
        || !valid_rpc_id(request.id.as_ref())
        || validate_json_value(
            &request.params,
            MCP_BRIDGE_MAX_MESSAGE_BYTES,
            "hosted MCP params",
        )
        .is_err()
    {
        render_rpc(
            res,
            StatusCode::BAD_REQUEST,
            rpc_error(request.id, -32600, "Invalid bounded JSON-RPC request", None),
        );
        return;
    }
    let Some(registry) = registry(depot) else {
        render_rpc(
            res,
            StatusCode::INTERNAL_SERVER_ERROR,
            rpc_error(
                request.id,
                -32603,
                "MCP bridge registry is unavailable",
                None,
            ),
        );
        return;
    };
    let auth = depot.obtain::<AuthContext>().ok().cloned();
    let target = match resolve_target(&registry, auth.as_ref(), &bridge_id).await {
        Ok(Some(target)) => target,
        Ok(None) => {
            render_rpc(
                res,
                StatusCode::NOT_FOUND,
                rpc_error(request.id, -32601, "MCP bridge resource not found", None),
            );
            return;
        }
        Err(error) => {
            render_rpc(
                res,
                StatusCode::SERVICE_UNAVAILABLE,
                rpc_error(request.id, -32001, error, None),
            );
            return;
        }
    };

    if request.method == "notifications/initialized" {
        if request.id.is_none()
            && request
                .params
                .as_object()
                .is_some_and(|params| params.is_empty())
        {
            res.status_code(StatusCode::ACCEPTED);
        } else {
            render_rpc(
                res,
                StatusCode::BAD_REQUEST,
                rpc_error(
                    request.id,
                    -32600,
                    "notifications/initialized must be an empty JSON-RPC notification",
                    None,
                ),
            );
        }
        return;
    }
    if request.id.is_none() {
        render_rpc(
            res,
            StatusCode::BAD_REQUEST,
            rpc_error(
                None,
                -32600,
                "MCP bridge requests require a JSON-RPC id",
                None,
            ),
        );
        return;
    }
    let id = request.id.clone();
    let response = match request.method.as_str() {
        "initialize" => {
            if request
                .params
                .get("protocolVersion")
                .and_then(Value::as_str)
                != Some(MCP_PROTOCOL_VERSION)
            {
                rpc_error(
                    id,
                    -32602,
                    format!("Unsupported MCP protocol version; expected {MCP_PROTOCOL_VERSION}"),
                    None,
                )
            } else {
                rpc_result(
                    id,
                    json!({
                        "protocolVersion": MCP_PROTOCOL_VERSION,
                        "capabilities": {"tools": {}},
                        "serverInfo": {
                            "name": format!("WebCodex MCP Bridge: {}", target.provider.name),
                            "version": env!("CARGO_PKG_VERSION")
                        }
                    }),
                )
            }
        }
        "ping" => {
            if request
                .params
                .as_object()
                .is_some_and(|params| params.is_empty())
            {
                rpc_result(id, json!({}))
            } else {
                rpc_error(id, -32602, "ping params must be empty", None)
            }
        }
        "tools/list" => {
            if request.params.as_object().is_some_and(|params| {
                params
                    .iter()
                    .any(|(key, value)| key != "cursor" || !value.is_null())
            }) {
                render_rpc(
                    res,
                    StatusCode::BAD_REQUEST,
                    rpc_error(
                        id,
                        -32602,
                        "tools/list pagination and additional params are unsupported in bridge V1",
                        None,
                    ),
                );
                return;
            }
            let operation = McpBridgeRequest::ToolsList {
                provider_id: target.provider.provider_id.clone(),
                provider_instance_id: target.provider.provider_instance_id.clone(),
            };
            match invoke_exact(&registry, auth.as_ref(), &target, operation).await {
                Ok(McpBridgeResponsePayload::Tools { tools }) => {
                    rpc_result(id, json!({"tools": tools}))
                }
                Ok(_) => rpc_error(
                    id,
                    -32603,
                    "Runner returned the wrong bounded bridge response type",
                    None,
                ),
                Err(error) => bridge_rpc_error(id, error),
            }
        }
        "tools/call" => {
            let params: ToolCallParams = match serde_json::from_value(request.params) {
                Ok(params) => params,
                Err(_) => {
                    render_rpc(
                        res,
                        StatusCode::BAD_REQUEST,
                        rpc_error(id, -32602, "Invalid tools/call params", None),
                    );
                    return;
                }
            };
            let operation = McpBridgeRequest::ToolsCall {
                provider_id: target.provider.provider_id.clone(),
                provider_instance_id: target.provider.provider_instance_id.clone(),
                name: params.name,
                arguments: params.arguments,
            };
            if validate_request(&operation).is_err() {
                rpc_error(
                    id,
                    -32602,
                    "Invalid or excessive tools/call arguments",
                    None,
                )
            } else {
                match invoke_exact(&registry, auth.as_ref(), &target, operation).await {
                    Ok(McpBridgeResponsePayload::ToolResult { result }) => rpc_result(
                        id,
                        serde_json::to_value(result).unwrap_or_else(|_| {
                            json!({
                                "content": [{"type": "text", "text": "Bridge result serialization failed"}],
                                "isError": true
                            })
                        }),
                    ),
                    Ok(_) => rpc_error(
                        id,
                        -32603,
                        "Runner returned the wrong bounded bridge response type",
                        Some(json!({
                            "dispatchState": "outcome_unknown",
                            "retryable": false
                        })),
                    ),
                    Err(error) => bridge_rpc_error(id, error),
                }
            }
        }
        _ => rpc_error(
            id,
            -32601,
            "Method not found on the tool-only MCP bridge V1",
            Some(json!({
                "supported": ["initialize", "notifications/initialized", "ping", "tools/list", "tools/call"]
            })),
        ),
    };
    render_rpc(res, StatusCode::OK, response);
}

fn registry(depot: &Depot) -> Option<Arc<ShellClientRegistry>> {
    depot.obtain::<Arc<ShellClientRegistry>>().ok().cloned()
}

fn valid_rpc_id(id: Option<&Value>) -> bool {
    match id {
        None => true,
        Some(Value::Number(number)) => number.as_i64().is_some() || number.as_u64().is_some(),
        Some(Value::String(value)) => {
            !value.is_empty() && value.len() <= 128 && !value.chars().any(char::is_control)
        }
        Some(_) => false,
    }
}

fn bridge_id_param(req: &Request) -> Option<String> {
    let value = req.param::<String>("bridge_id")?;
    let suffix = value.strip_prefix(BRIDGE_ID_PREFIX)?;
    (suffix.len() == 64
        && suffix
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)))
    .then_some(value)
}

fn opaque_bridge_id(
    client_id: &str,
    agent_instance_id: &str,
    provider: &McpBridgeProvider,
) -> String {
    let mut hasher = Sha256::new();
    for value in [
        client_id,
        agent_instance_id,
        &provider.provider_id,
        &provider.provider_instance_id,
    ] {
        hasher.update((value.len() as u64).to_be_bytes());
        hasher.update(value.as_bytes());
    }
    format!("{BRIDGE_ID_PREFIX}{:x}", hasher.finalize())
}

async fn discover_targets(
    registry: &Arc<ShellClientRegistry>,
    auth: Option<&AuthContext>,
) -> Result<Vec<BridgeTarget>, &'static str> {
    let clients = registry
        .list_clients_for_auth(auth)
        .await
        .into_iter()
        .filter(|client| client.connected && client.capabilities.mcp_bridge)
        .collect::<Vec<_>>();
    if clients.len() > MAX_DISCOVERY_RUNNERS {
        return Err("MCP bridge Runner discovery bound exceeded");
    }
    let requested_by = requested_by_from_auth(auth);
    let calls = clients.into_iter().map(|client| {
        let registry = Arc::clone(registry);
        let auth = auth.cloned();
        let requested_by = requested_by.clone();
        async move {
            let (request_id, receiver) = registry
                .enqueue_mcp_bridge(
                    &client.client_id,
                    &client.agent_instance_id,
                    McpBridgeRequest::Discover,
                    auth.as_ref(),
                    requested_by,
                )
                .await
                .ok()?;
            let response = match tokio::time::timeout(DISCOVERY_TIMEOUT, receiver).await {
                Ok(Ok(response)) => response,
                _ => {
                    let _ = registry.cancel_request_dispatch_state(&request_id).await;
                    return None;
                }
            };
            let McpBridgeResponsePayload::Providers { providers } = response.payload? else {
                return None;
            };
            Some((client, providers))
        }
    });
    let mut targets = Vec::new();
    for (client, providers) in join_all(calls).await.into_iter().flatten() {
        for provider in providers {
            targets.push(BridgeTarget {
                bridge_id: opaque_bridge_id(
                    &client.client_id,
                    &client.agent_instance_id,
                    &provider,
                ),
                client_id: client.client_id.clone(),
                agent_instance_id: client.agent_instance_id.clone(),
                provider,
            });
            if targets.len() > MAX_HOSTED_PROVIDERS {
                return Err("MCP bridge provider discovery bound exceeded");
            }
        }
    }
    targets.sort_by(|left, right| left.bridge_id.cmp(&right.bridge_id));
    Ok(targets)
}

async fn resolve_target(
    registry: &Arc<ShellClientRegistry>,
    auth: Option<&AuthContext>,
    bridge_id: &str,
) -> Result<Option<BridgeTarget>, &'static str> {
    Ok(discover_targets(registry, auth)
        .await?
        .into_iter()
        .find(|target| target.bridge_id == bridge_id))
}

async fn invoke_exact(
    registry: &Arc<ShellClientRegistry>,
    auth: Option<&AuthContext>,
    target: &BridgeTarget,
    operation: McpBridgeRequest,
) -> Result<McpBridgeResponsePayload, McpBridgeResponse> {
    let (request_id, receiver) = registry
        .enqueue_mcp_bridge(
            &target.client_id,
            &target.agent_instance_id,
            operation,
            auth,
            requested_by_from_auth(auth),
        )
        .await
        .map_err(|_| {
            McpBridgeResponse::error(
                McpBridgeDispatchState::NotStarted,
                "exact_target_unavailable",
                "Exact Runner/provider target is unavailable; request was not started",
            )
        })?;
    let response = match tokio::time::timeout(BRIDGE_CALL_TIMEOUT, receiver).await {
        Ok(Ok(response)) => response,
        _ => {
            let dispatched = registry.cancel_request_dispatch_state(&request_id).await;
            let state = if dispatched == Some(false) {
                McpBridgeDispatchState::NotStarted
            } else {
                McpBridgeDispatchState::OutcomeUnknown
            };
            McpBridgeResponse::error(
                state,
                "bridge_timeout",
                if state == McpBridgeDispatchState::NotStarted {
                    "Bridge request timed out before Runner dispatch"
                } else {
                    "Bridge request timed out after possible dispatch; outcome is unknown and must not be retried automatically"
                },
            )
        }
    };
    if response.error.is_none() {
        if let Some(payload) = response.payload.clone() {
            return Ok(payload);
        }
    }
    Err(response)
}

fn bridge_rpc_error(id: Option<Value>, response: McpBridgeResponse) -> Value {
    let state = match response.dispatch_state {
        McpBridgeDispatchState::NotStarted => "not_started",
        McpBridgeDispatchState::OutcomeUnknown => "outcome_unknown",
        McpBridgeDispatchState::Completed => "completed",
    };
    let error = response.error.unwrap_or(crate::mcp_bridge::McpBridgeError {
        code: "bridge_failure".to_string(),
        message: "MCP bridge request failed".to_string(),
    });
    rpc_error(
        id,
        -32001,
        error.message,
        Some(json!({
            "code": error.code,
            "dispatchState": state,
            "retryable": false,
            "reconciliationRequired": response.dispatch_state == McpBridgeDispatchState::OutcomeUnknown
        })),
    )
}

fn rpc_result(id: Option<Value>, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id.unwrap_or(Value::Null), "result": result})
}

fn rpc_error(
    id: Option<Value>,
    code: i64,
    message: impl Into<String>,
    data: Option<Value>,
) -> Value {
    let mut error = json!({"code": code, "message": message.into()});
    if let Some(data) = data {
        error["data"] = data;
    }
    json!({"jsonrpc": "2.0", "id": id.unwrap_or(Value::Null), "error": error})
}

fn render_rpc(res: &mut Response, status: StatusCode, body: Value) {
    res.status_code(status);
    res.render(Json(body));
}

fn render_http_error(res: &mut Response, status: StatusCode, message: impl Into<String>) {
    res.status_code(status);
    res.render(Json(json!({"error": message.into()})));
}

#[cfg(test)]
#[path = "mcp_bridge_http/tests.rs"]
mod tests;
