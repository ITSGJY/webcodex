//! Hosted Streamable HTTP MCP facade for exact Runner-owned stdio providers.
//!
//! The hosted surface is deliberately tool-only and stateless. Provider
//! discovery is a separate authenticated JSON endpoint at `/mcp/bridge`; each
//! opaque provider resource lives at `/mcp/bridge/{bridge_id}`.

use crate::auth::{AuthContext, AuthKind};
use crate::mcp_bridge::{
    validate_json_value, validate_request, McpBridgeDispatchState, McpBridgeProvider,
    McpBridgeRequest, McpBridgeResponse, McpBridgeResponsePayload, MCP_BRIDGE_MAX_MESSAGE_BYTES,
};
use crate::shell_client::requested_by_from_auth;
use crate::shell_client::ShellClientRegistry;
use salvo::prelude::*;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::time::Duration;

const MCP_PROTOCOL_VERSION_2025_06_18: &str = "2025-06-18";
const MCP_PROTOCOL_VERSION_2025_11_25: &str = "2025-11-25";
const MCP_LATEST_PROTOCOL_VERSION: &str = MCP_PROTOCOL_VERSION_2025_11_25;
#[cfg(test)]
const MCP_PROTOCOL_VERSION: &str = MCP_PROTOCOL_VERSION_2025_06_18;
const MCP_PROTOCOL_VERSION_HEADER: &str = "mcp-protocol-version";
const BRIDGE_ID_PREFIX: &str = "wc_mcpb_";
const MAX_DISCOVERY_RUNNERS: usize = 16;
const MAX_HOSTED_PROVIDERS: usize = 64;
const BRIDGE_CALL_TIMEOUT: Duration = Duration::from_secs(125);

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
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

fn supported_protocol_version(version: &str) -> bool {
    matches!(
        version,
        MCP_PROTOCOL_VERSION_2025_06_18 | MCP_PROTOCOL_VERSION_2025_11_25
    )
}

fn validate_initialize_params(params: &Value) -> Result<&str, &'static str> {
    let Some(params) = params.as_object() else {
        return Err("initialize params must be an object");
    };
    let Some(protocol_version) = params.get("protocolVersion").and_then(Value::as_str) else {
        return Err("initialize requires protocolVersion");
    };
    if !supported_protocol_version(protocol_version) {
        return Err("unsupported MCP protocol version");
    }
    if !params.get("capabilities").is_some_and(Value::is_object) {
        return Err("initialize requires object capabilities");
    }
    let Some(client_info) = params.get("clientInfo").and_then(Value::as_object) else {
        return Err("initialize requires object clientInfo");
    };
    for field in ["name", "version"] {
        if !client_info
            .get(field)
            .and_then(Value::as_str)
            .is_some_and(|value| {
                !value.trim().is_empty()
                    && value.len() <= 128
                    && !value.chars().any(char::is_control)
            })
        {
            return Err("initialize clientInfo requires bounded name and version");
        }
    }
    Ok(protocol_version)
}

fn validate_protocol_header(req: &Request, method: &str) -> Result<(), &'static str> {
    let version = match req.headers().get(MCP_PROTOCOL_VERSION_HEADER) {
        Some(value) => Some(
            value
                .to_str()
                .map_err(|_| "invalid MCP-Protocol-Version")?
                .trim(),
        ),
        None => None,
    };
    match (method, version) {
        // ChatGPT's MCP host currently sends the post-initialize acknowledgement
        // without the HTTP protocol-version header. This exact notification has
        // no dispatch authority and no response body, so tolerate only that
        // missing header while keeping every request/call strict.
        ("initialize", None) | ("notifications/initialized", None) => Ok(()),
        (_, Some(version)) if supported_protocol_version(version) => Ok(()),
        (_, Some(_)) => Err("unsupported MCP-Protocol-Version"),
        (_, None) => Err("MCP-Protocol-Version is required after initialize"),
    }
}

fn downstream_execution_authorized(depot: &Depot) -> bool {
    depot
        .obtain::<Arc<crate::tool_runtime::ToolRuntime>>()
        .ok()
        .is_some_and(|runtime| runtime.permission_evaluator.config().auto_authorize())
}

fn render_execution_authority_denied(res: &mut Response, id: Option<Value>, operation: &str) {
    render_rpc(
        res,
        StatusCode::FORBIDDEN,
        rpc_error(
            id,
            -32022,
            format!(
                "MCP bridge {operation} is not authorized by the current execution authority mode"
            ),
            Some(json!({
                "dispatchState": "not_started",
                "retryable": false
            })),
        ),
    );
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
    match resolve_target(&registry, auth.as_ref(), &bridge_id).await {
        Ok(Some(_)) => {
            // Streamable HTTP GET is the SSE-listening operation. V1 does not
            // implement SSE, so an existing exact endpoint must return 405.
            res.status_code(StatusCode::METHOD_NOT_ALLOWED);
        }
        Ok(None) => {
            render_http_error(res, StatusCode::NOT_FOUND, "MCP bridge resource not found");
        }
        Err(error) => render_http_error(res, StatusCode::SERVICE_UNAVAILABLE, error),
    }
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
    let bytes = match req
        .payload_with_max_size(MCP_BRIDGE_MAX_MESSAGE_BYTES)
        .await
    {
        Ok(bytes) => bytes,
        Err(_) => {
            render_rpc(
                res,
                StatusCode::PAYLOAD_TOO_LARGE,
                rpc_error(
                    None,
                    -32600,
                    "MCP bridge request exceeds the bounded message limit",
                    None,
                ),
            );
            return;
        }
    };
    let raw: Value = match serde_json::from_slice(bytes) {
        Ok(raw) => raw,
        Err(_) => {
            render_rpc(
                res,
                StatusCode::BAD_REQUEST,
                rpc_error(None, -32700, "Parse error", None),
            );
            return;
        }
    };
    let request: JsonRpcRequest = match serde_json::from_value(raw) {
        Ok(request) => request,
        Err(_) => {
            render_rpc(
                res,
                StatusCode::BAD_REQUEST,
                rpc_error(None, -32600, "Invalid JSON-RPC envelope", None),
            );
            return;
        }
    };
    if request.jsonrpc.as_deref() != Some("2.0") {
        render_rpc(
            res,
            StatusCode::BAD_REQUEST,
            rpc_error(request.id, -32600, "jsonrpc must be '2.0'", None),
        );
        return;
    }
    if let Err(message) = validate_protocol_header(req, &request.method) {
        render_rpc(
            res,
            StatusCode::BAD_REQUEST,
            rpc_error(
                request.id.clone(),
                -32600,
                message,
                Some(json!({"supportedProtocolVersion": MCP_LATEST_PROTOCOL_VERSION})),
            ),
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
        "initialize" => match validate_initialize_params(&request.params) {
            Ok(protocol_version) => rpc_result(
                id,
                json!({
                    "protocolVersion": protocol_version,
                    "capabilities": {"tools": {}},
                    "serverInfo": {
                        "name": format!("WebCodex MCP Bridge: {}", target.provider.name),
                        "version": env!("CARGO_PKG_VERSION")
                    }
                }),
            ),
            Err(message) => rpc_error(
                id,
                -32602,
                message,
                Some(json!({"supportedProtocolVersion": MCP_LATEST_PROTOCOL_VERSION})),
            ),
        },
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
            if !downstream_execution_authorized(depot) {
                render_execution_authority_denied(res, id, "tools/list");
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
                if !downstream_execution_authorized(depot) {
                    render_execution_authority_denied(res, id, "tools/call");
                    return;
                }
                match invoke_exact(&registry, auth.as_ref(), &target, operation).await {
                    Ok(McpBridgeResponsePayload::ToolResult { result }) => {
                        match serde_json::to_value(result) {
                            Ok(result) => rpc_result(id, result),
                            Err(_) => rpc_error(
                                id,
                                -32603,
                                "Failed to encode completed MCP tool result",
                                Some(json!({
                                    "dispatchState": "completed",
                                    "retryable": false
                                })),
                            ),
                        }
                    }
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
    is_valid_bridge_id(&value).then_some(value)
}

pub(crate) fn is_valid_bridge_id(value: &str) -> bool {
    let Some(suffix) = value.strip_prefix(BRIDGE_ID_PREFIX) else {
        return false;
    };
    suffix.len() == 64
        && suffix
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub(crate) fn opaque_bridge_id(
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

    let mut targets = Vec::new();
    for client in clients {
        let providers = client
            .policy
            .as_ref()
            .and_then(|policy| policy.mcp_bridge_providers.as_ref())
            .ok_or("MCP bridge Runner inventory is unavailable")?;
        for provider in providers {
            targets.push(BridgeTarget {
                bridge_id: opaque_bridge_id(&client.client_id, &client.agent_instance_id, provider),
                client_id: client.client_id.clone(),
                agent_instance_id: client.agent_instance_id.clone(),
                provider: provider.clone(),
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

/// Resolve an opaque hosted bridge as internal resource-server state. This is
/// used by public RFC 9728 metadata and OAuth grant validation, where there is
/// not yet a bearer-token `AuthContext`. The opaque id is checked against the
/// exact provider inventory on the current registered Runner lease, so this
/// public/resource-validation path never executes a Runner RPC and a restarted
/// Runner or provider cannot inherit an older audience.
pub(crate) async fn hosted_bridge_is_current(
    registry: &Arc<ShellClientRegistry>,
    bridge_id: &str,
) -> Result<bool, &'static str> {
    if !is_valid_bridge_id(bridge_id) {
        return Ok(false);
    }
    let mut internal = AuthContext::new(AuthKind::Bootstrap);
    internal.is_bootstrap = true;
    resolve_target(registry, Some(&internal), bridge_id)
        .await
        .map(|target| target.is_some())
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
