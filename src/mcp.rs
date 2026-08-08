use crate::action_audit::{ActionAudit, ActionAuditRecord};
use crate::auth::AuthContext;
use crate::connector_runtime::{ConnectorRuntime, ConnectorRuntimeSlot, ConnectorTransport};
use crate::json_error;
use crate::model_surface::ModelSurface;
use crate::tool_request_trace::{
    estimate_json_bytes, jsonrpc_id_safe, new_trace_id, ToolRequestLifecycle,
};
use crate::tool_runtime::kernel::{
    ToolCallContext, ToolCallErrorStatus, ToolCallRequest as KernelToolCallRequest, ToolTransport,
};
use crate::tool_runtime::tool_definition::LOCAL_CODING_TOOL_NAMES;
use crate::tool_runtime::{registered_tool_specs, ToolResult, ToolRuntime, ToolSpec};
use salvo::prelude::*;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

const MCP_PROTOCOL_VERSION: &str = "2025-06-18";
const MCP_STATELESS_PROTOCOL_VERSION: &str = "2026-07-28";

/// Single source of truth for the JSON-RPC methods advertised by `GET /mcp`.
/// Must match the dispatch arms in `handle_mcp_request_with_lifecycle`;
/// pinned by `mcp_info_advertised_methods_match_dispatch`.
const MCP_INFO_METHODS: &[&str] = &[
    "server/discover",
    "initialize",
    "ping",
    "tools/list",
    "tools/call",
    "notifications/initialized",
];
const MCP_RESERVED_SESSION_ID_FIELD: &str = "_session_id";

/// Hard upper bound on a single MCP JSON-RPC dispatch, applied in `mcp_post`.
///
/// Chosen above every per-tool wait (sync agent waits are clamped to
/// `wait_timeout_secs <= 120` plus a few seconds of margin), so it can only
/// fire when a dispatch path hangs without its own bound. Its job is to turn
/// an otherwise-permanently-silent HTTP request into an explicit JSON-RPC
/// error the client can surface.
const MCP_DISPATCH_HARD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(150);

#[derive(Debug, Deserialize)]
struct JsonRpcRequest {
    #[serde(default)]
    jsonrpc: Option<String>,
    pub method: String,
    #[serde(default)]
    pub params: Value,
    #[serde(default)]
    pub id: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct McpToolCallParams {
    pub name: String,
    #[serde(default)]
    pub arguments: Value,
}

fn runtime(depot: &Depot) -> Option<Arc<ToolRuntime>> {
    depot.obtain::<Arc<ToolRuntime>>().ok().cloned()
}

fn connector_runtime_slot(depot: &Depot) -> Option<ConnectorRuntimeSlot> {
    depot.obtain::<ConnectorRuntimeSlot>().ok().cloned()
}

fn validate_model_surface_state(
    model_surface: ModelSurface,
    connector_present: bool,
) -> Result<(), String> {
    match (model_surface, connector_present) {
        (ModelSurface::CanonicalConnector, true)
        | (ModelSurface::LocalCoding, false)
        | (ModelSurface::FullOperatorRuntime, false) => Ok(()),
        (ModelSurface::CanonicalConnector, false) => Err(
            "canonical_connector surface selected but Connector runtime state is missing"
                .to_string(),
        ),
        (ModelSurface::LocalCoding, true) | (ModelSurface::FullOperatorRuntime, true) => {
            Err(format!(
                "{} surface selected but Connector runtime state is present",
                model_surface.name()
            ))
        }
    }
}

fn tool_name_from_params(params: &Value) -> Option<String> {
    params
        .get("name")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

fn project_from_tool_call_params(params: &Value) -> Option<String> {
    params["arguments"]["project"].as_str().map(str::to_string)
}

fn request_uses_stateless_protocol(params: &Value) -> bool {
    params
        .get("_meta")
        .and_then(|meta| meta.get("io.modelcontextprotocol/protocolVersion"))
        .and_then(Value::as_str)
        == Some(MCP_STATELESS_PROTOCOL_VERSION)
}

fn mcp_stateless_result(mut result: Value, cacheable: bool) -> Value {
    let Some(object) = result.as_object_mut() else {
        return result;
    };
    object
        .entry("resultType".to_string())
        .or_insert_with(|| Value::String("complete".to_string()));
    if cacheable {
        object
            .entry("ttlMs".to_string())
            .or_insert_with(|| Value::from(0));
        object
            .entry("cacheScope".to_string())
            .or_insert_with(|| Value::String("private".to_string()));
    }
    let meta = object
        .entry("_meta".to_string())
        .or_insert_with(|| json!({}));
    if let Some(meta_object) = meta.as_object_mut() {
        meta_object
            .entry("io.modelcontextprotocol/serverInfo".to_string())
            .or_insert_with(|| {
                json!({
                    "name": "webcodex",
                    "version": env!("CARGO_PKG_VERSION")
                })
            });
    }
    result
}

/// MCP tools/list payload for the immutable startup-selected model surface.
fn mcp_tools_list_payload(model_surface: ModelSurface) -> Value {
    let compact = crate::config::mcp_compact_schemas_enabled();
    let specs = match model_surface {
        ModelSurface::CanonicalConnector => crate::connector_runtime::surface::capability_specs(),
        ModelSurface::LocalCoding => crate::model_surface::local_coding_tool_specs(),
        ModelSurface::FullOperatorRuntime => registered_tool_specs(),
    };
    let tools: Vec<Value> = specs
        .into_iter()
        .map(|spec| mcp_tool_spec_json(spec, compact))
        .collect();
    json!({ "tools": tools })
}

fn mcp_tool_spec_json(mut spec: ToolSpec, compact: bool) -> Value {
    if spec.name == "read_project_artifact" {
        if let Some(properties) = spec.input_schema["properties"].as_object_mut() {
            properties.insert(
                "as_image".to_string(),
                json!({
                    "type": "boolean",
                    "description": "MCP-only. When true, read one complete PNG, JPEG, or WebP up to 1 MiB and return it as native image content. Cannot be combined with offset, length, or max_bytes."
                }),
            );
        }
        spec.description.push_str(
            " Over MCP, set as_image=true to return one complete PNG, JPEG, or WebP as native image content; ordinary calls keep the existing chunked base64 response.",
        );
    }
    if compact {
        json!({
            "name": spec.name,
            "description": spec.description,
            "inputSchema": spec.input_schema,
            "annotations": spec.annotations,
        })
    } else {
        // Match ToolSpec's camelCase serde so default behavior is unchanged.
        serde_json::to_value(spec).unwrap_or_else(|_| json!({}))
    }
}

fn mcp_runtime_tool_result(
    tool_name: &str,
    as_image_requested: bool,
    mut result: ToolResult,
) -> Value {
    if tool_name == "read_project_artifact" && as_image_requested && result.success {
        match mcp_native_image_tool_result(&mut result) {
            Ok(value) => return value,
            Err(error) => {
                result = ToolResult::err(format!(
                    "cannot frame read_project_artifact as MCP image content: {error}"
                ));
            }
        }
    }

    let text = serde_json::to_string(&json!({
        "success": result.success,
        "output": result.output.clone(),
        "error": result.error.clone(),
    }))
    .unwrap_or_else(|_| "{}".to_string());
    json!({
        "content": [
            {
                "type": "text",
                "text": text
            }
        ],
        "structuredContent": {
            "success": result.success,
            "output": result.output,
            "error": result.error,
        },
        "isError": !result.success
    })
}

fn mcp_native_image_tool_result(result: &mut ToolResult) -> Result<Value, String> {
    let data = result
        .output
        .get("content_base64")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing content_base64".to_string())?
        .to_string();
    let mime_type = result
        .output
        .get("mime_type")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing mime_type".to_string())?
        .to_string();
    if !matches!(
        mime_type.as_str(),
        "image/png" | "image/jpeg" | "image/webp"
    ) {
        return Err(format!("unsupported MIME type '{mime_type}'"));
    }
    let path = result
        .output
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or("project image");
    let file_bytes = result
        .output
        .get("file_bytes")
        .and_then(Value::as_u64)
        .ok_or_else(|| "missing file_bytes".to_string())?;
    let sha256 = result
        .output
        .get("sha256")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let metadata_text = format!("Image {path}: {mime_type}, {file_bytes} bytes, sha256 {sha256}.");

    let output = result
        .output
        .as_object_mut()
        .ok_or_else(|| "tool output is not an object".to_string())?;
    output.remove("content_base64");
    output.insert("content_delivery".to_string(), json!("mcp_image"));
    let structured_output = result.output.clone();

    Ok(json!({
        "content": [
            {
                "type": "text",
                "text": metadata_text
            },
            {
                "type": "image",
                "data": data,
                "mimeType": mime_type
            }
        ],
        "structuredContent": {
            "success": true,
            "output": structured_output,
            "error": Value::Null,
        },
        "isError": false
    }))
}

/// Outcome of handling a single MCP JSON-RPC request.
///
/// Carries the JSON-RPC response body alongside the HTTP status the HTTP
/// wrapper should render. Keeping this separate from `Response` makes the
/// core protocol logic testable without a live server.
#[derive(Debug)]
enum McpOutcome {
    /// A normal JSON-RPC result. HTTP 200 with the body.
    Ok(Value),
    /// A JSON-RPC protocol error. HTTP 400 with the error body.
    BadRequest(Value),
    /// A JSON-RPC notification (request without an `id` member). Per the
    /// JSON-RPC 2.0 and MCP specs the server MUST NOT reply with a
    /// JSON-RPC response body. The HTTP wrapper acknowledges with 202 and
    /// an empty body.
    Notification,
    /// The HTTP request authenticated, but the OAuth2 bearer token lacks the
    /// delegated scope needed by this JSON-RPC method or tool.
    Forbidden {
        body: Value,
        required_scope: Option<&'static str>,
    },
}

#[handler]
pub async fn mcp_info(depot: &mut Depot, res: &mut Response) {
    let auth_required = crate::auth::get_config(depot)
        .map(|c| c.is_auth_enabled())
        .unwrap_or(false);
    let Some(runtime) = runtime(depot) else {
        res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
        res.render(json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Tool runtime not configured",
        ));
        return;
    };
    let Some(connector_slot) = connector_runtime_slot(depot) else {
        res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
        res.render(json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "MCP model surface state not configured",
        ));
        return;
    };
    let model_surface = runtime.model_surface();
    if let Err(error) = validate_model_surface_state(model_surface, connector_slot.0.is_some()) {
        tracing::error!(%error, "MCP model surface state mismatch");
        res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
        res.render(json_error(StatusCode::INTERNAL_SERVER_ERROR, error));
        return;
    }
    res.render(Json(json!({
        "name": "webcodex",
        "version": env!("CARGO_PKG_VERSION"),
        "modelSurface": model_surface.name(),
        "protocol": "mcp",
        "protocolVersion": MCP_PROTOCOL_VERSION,
        "transport": "streamable-http-jsonrpc",
        "endpoint": "/mcp",
        "methods": MCP_INFO_METHODS,
        "auth": {
            "type": "bearer",
            "required": auth_required,
            "header": "Authorization: Bearer <shared_key_or_wc_pat>"
        }
    })));
}

#[handler]
pub async fn mcp_post(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let mut guard = ToolRequestLifecycle::new("mcp", new_trace_id(), "-", "POST /mcp", None);
    guard.received();

    let Some(runtime) = runtime(depot) else {
        // Size unknown without building the json_error body for measurement.
        guard.response_serialized(500, None, Some(false), None, "error_runtime_missing");
        res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
        res.render(json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Tool runtime not configured",
        ));
        guard.handler_returned(500, None, Some(false), None, "error_runtime_missing");
        return;
    };
    let Some(connector_slot) = connector_runtime_slot(depot) else {
        guard.response_serialized(500, None, Some(false), None, "error_surface_state_missing");
        res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
        res.render(json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "MCP model surface state not configured",
        ));
        guard.handler_returned(500, None, Some(false), None, "error_surface_state_missing");
        return;
    };
    let connector = connector_slot.0;
    if let Err(error) = validate_model_surface_state(runtime.model_surface(), connector.is_some()) {
        tracing::error!(%error, "MCP model surface state mismatch");
        guard.response_serialized(500, None, Some(false), None, "error_surface_state_mismatch");
        res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
        res.render(json_error(StatusCode::INTERNAL_SERVER_ERROR, error));
        guard.handler_returned(500, None, Some(false), None, "error_surface_state_mismatch");
        return;
    }
    let request: JsonRpcRequest = match req.parse_json().await {
        Ok(request) => request,
        Err(e) => {
            guard.set_jsonrpc_id("none");
            guard.parsed("parse_error");
            let body = rpc_error(None, -32700, format!("Parse error: {}", e));
            let estimated = estimate_json_bytes(&body);
            guard.response_serialized(400, estimated, Some(false), None, "parse_error");
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(body));
            guard.handler_returned(400, estimated, Some(false), None, "parse_error");
            return;
        }
    };

    guard.set_jsonrpc_id(jsonrpc_id_safe(request.id.as_ref()));
    guard.set_method(request.method.clone());
    let tool_name = if request.method == "tools/call" {
        tool_name_from_params(&request.params)
    } else {
        None
    };
    guard.set_tool_name(tool_name.clone());
    guard.parsed("ok");
    let window = crate::client_window::mcp_window(req, request.method == "initialize");

    // Chat-window MCP tool calls must land in the action audit exactly like
    // the REST surface (they were previously invisible there). Summary-level
    // only: tool name and project — never arguments or outputs. JSON-RPC
    // notifications are acknowledged but never dispatched, so they must not be
    // represented as executed actions.
    let audit = if request.method == "tools/call" && request.id.is_some() {
        Some((
            ActionAudit::start(req, depot, "/mcp", "toolsCall"),
            tool_name.unwrap_or_else(|| "unknown".to_string()),
            project_from_tool_call_params(&request.params),
        ))
    } else {
        None
    };
    let record_audit = |success: bool, status: StatusCode, error: Option<String>| {
        if let Some((audit, tool, project)) = audit.as_ref() {
            let mut event = ActionAuditRecord::new(tool.clone(), success, status)
                .error(error)
                .summary(json!({ "transport": "mcp" }));
            event.project = project.clone();
            audit.record(event);
        }
    };

    let auth = depot.obtain::<crate::auth::AuthContext>().ok().cloned();
    // Defense-in-depth backstop: every tool bounds its own agent/subprocess
    // waits at <= 124s, so this outer limit never preempts a legitimate inner
    // timeout. It only fires if a dispatch path hangs without a bound (the
    // failure mode behind "MCP request never gets a reply"), converting a
    // silently dead HTTP request into an observable JSON-RPC error.
    let request_id = request.id.clone();
    let outcome = match tokio::time::timeout(
        MCP_DISPATCH_HARD_TIMEOUT,
        handle_mcp_request_with_lifecycle(
            &runtime,
            connector.as_deref(),
            request,
            auth.as_ref(),
            window.identity.as_ref(),
            Some(&mut guard),
        ),
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(_) => {
            let body = rpc_error(
                request_id,
                -32000,
                format!(
                    "server-side dispatch exceeded {}s hard limit; the tool may still be running — check session/job status before retrying",
                    MCP_DISPATCH_HARD_TIMEOUT.as_secs()
                ),
            );
            record_audit(
                false,
                StatusCode::INTERNAL_SERVER_ERROR,
                Some("mcp dispatch hard timeout".to_string()),
            );
            let estimated = estimate_json_bytes(&body);
            guard.response_serialized(500, estimated, Some(false), None, "dispatch_hard_timeout");
            res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
            res.render(Json(body));
            guard.handler_returned(500, estimated, Some(false), None, "dispatch_hard_timeout");
            return;
        }
    };

    if matches!(outcome, McpOutcome::Ok(_)) {
        if let Some(session_id) = window.issued_session_id.as_deref() {
            crate::client_window::set_mcp_session_header(res, session_id);
        }
    }

    match outcome {
        McpOutcome::Ok(body) => {
            // Protocol success: valid JSON-RPC result envelope.
            // Tool success: only meaningful for tools/call (isError / structuredContent.success).
            let tool_success = body
                .get("result")
                .and_then(|r| r.get("structuredContent"))
                .and_then(|s| s.get("success").or_else(|| s.get("ok")))
                .and_then(|v| v.as_bool());
            let audit_success = tool_success.unwrap_or(true);
            record_audit(
                audit_success,
                StatusCode::OK,
                if audit_success {
                    None
                } else {
                    body["result"]["structuredContent"]["error"]
                        .as_str()
                        .map(str::to_string)
                },
            );
            let estimated = estimate_json_bytes(&body);
            guard.response_serialized(200, estimated, Some(true), tool_success, "ok");
            res.render(Json(body));
            guard.handler_returned(200, estimated, Some(true), tool_success, "ok");
        }
        McpOutcome::BadRequest(body) => {
            record_audit(
                false,
                StatusCode::BAD_REQUEST,
                body["error"]["message"].as_str().map(str::to_string),
            );
            let estimated = estimate_json_bytes(&body);
            guard.response_serialized(400, estimated, Some(false), None, "bad_request");
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(body));
            guard.handler_returned(400, estimated, Some(false), None, "bad_request");
        }
        McpOutcome::Forbidden {
            body,
            required_scope,
        } => {
            record_audit(
                false,
                StatusCode::FORBIDDEN,
                Some(format!(
                    "insufficient scope: {}",
                    required_scope.unwrap_or("unknown")
                )),
            );
            let estimated = estimate_json_bytes(&body);
            guard.response_serialized(403, estimated, Some(false), None, "forbidden");
            res.status_code(StatusCode::FORBIDDEN);
            let challenge = crate::auth::oauth_insufficient_scope_challenge(required_scope);
            if let Ok(val) = salvo::http::HeaderValue::from_str(&challenge) {
                res.headers_mut().insert("www-authenticate", val);
            }
            res.render(Json(body));
            guard.handler_returned(403, estimated, Some(false), None, "forbidden");
        }
        McpOutcome::Notification => {
            // JSON-RPC notifications carry no `id`; the server MUST NOT reply
            // with a JSON-RPC body. Acknowledge with 202 and an empty body.
            // Empty body size is known (0) without JSON serialization.
            guard.response_serialized(202, Some(0), Some(true), None, "notification");
            res.status_code(StatusCode::ACCEPTED);
            guard.handler_returned(202, Some(0), Some(true), None, "notification");
        }
    }
}

/// Core MCP JSON-RPC dispatch. Pure (no HTTP types) so it can be unit tested.
///
/// Business logic stays in `ToolRuntime`; this function only frames the
/// JSON-RPC envelope and translates tool results into MCP content blocks.
/// Test-friendly wrapper: no lifecycle hooks.
#[cfg_attr(not(test), allow(dead_code))]
async fn handle_mcp_request(
    runtime: &ToolRuntime,
    request: JsonRpcRequest,
    auth: Option<&AuthContext>,
) -> McpOutcome {
    handle_mcp_request_with_lifecycle(runtime, None, request, auth, None, None).await
}

async fn handle_mcp_request_with_lifecycle(
    runtime: &ToolRuntime,
    connector: Option<&ConnectorRuntime>,
    request: JsonRpcRequest,
    auth: Option<&AuthContext>,
    window: Option<&crate::client_window::ClientWindow>,
    mut lifecycle: Option<&mut ToolRequestLifecycle>,
) -> McpOutcome {
    let is_oauth2 = auth.is_some_and(|ctx| ctx.is_oauth_token());
    let stateless_2026 =
        request.method == "server/discover" || request_uses_stateless_protocol(&request.params);

    if is_oauth2
        && matches!(
            request.method.as_str(),
            "server/discover" | "initialize" | "ping" | "tools/list" | "notifications/initialized"
        )
    {
        if let Some(outcome) = require_mcp_oauth_scope(auth, crate::auth::SCOPE_RUNTIME_READ) {
            return outcome;
        }
    }

    if is_oauth2
        && !matches!(
            request.method.as_str(),
            "server/discover"
                | "initialize"
                | "ping"
                | "tools/list"
                | "tools/call"
                | "notifications/initialized"
        )
    {
        return oauth_forbidden(None, "OAuth2 access tokens cannot call unknown MCP methods");
    }

    // A JSON-RPC request without an `id` member is a notification. Per the
    // JSON-RPC 2.0 and MCP specs the server MUST NOT reply with a JSON-RPC
    // response body, even if the method is unknown or malformed. We accept
    // the notification silently. `notifications/initialized` is the common
    // case sent by MCP clients after `initialize` completes.
    if request.id.is_none() {
        return McpOutcome::Notification;
    }

    if request.jsonrpc.as_deref().unwrap_or("2.0") != "2.0" {
        return McpOutcome::BadRequest(rpc_error(request.id, -32600, "jsonrpc must be '2.0'"));
    }

    if let Err(error) = validate_model_surface_state(runtime.model_surface(), connector.is_some()) {
        return McpOutcome::BadRequest(rpc_error(request.id, -32603, error));
    }

    let id = request.id.clone();
    let response = match request.method.as_str() {
        // MCP 2026-07-28 clients discover capabilities before issuing ordinary
        // requests. WebCodex supports the stateless tools path required by
        // modern clients while retaining its existing 2025-06-18
        // initialize/session lifecycle for legacy clients.
        "server/discover" => rpc_result(
            id,
            json!({
                "resultType": "complete",
                "ttlMs": 0,
                "cacheScope": "private",
                "supportedVersions": [MCP_STATELESS_PROTOCOL_VERSION, MCP_PROTOCOL_VERSION],
                "capabilities": {
                    "tools": {
                        "listChanged": false
                    }
                },
                "_meta": {
                    "io.modelcontextprotocol/serverInfo": {
                        "name": "webcodex",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                }
            }),
        ),
        "initialize" => rpc_result(
            id,
            json!({
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": {
                    "tools": {
                        "listChanged": false
                    }
                },
                "serverInfo": {
                    "name": "webcodex",
                    "version": env!("CARGO_PKG_VERSION"),
                    "modelSurface": runtime.model_surface().name()
                }
            }),
        ),
        "ping" => rpc_result(
            id,
            if stateless_2026 {
                mcp_stateless_result(json!({}), false)
            } else {
                json!({})
            },
        ),
        "tools/list" => {
            let result = mcp_tools_list_payload(runtime.model_surface());
            rpc_result(
                id,
                if stateless_2026 {
                    mcp_stateless_result(result, true)
                } else {
                    result
                },
            )
        }
        "tools/call" => {
            let mut params: McpToolCallParams = match serde_json::from_value(request.params) {
                Ok(params) => params,
                Err(e) => {
                    return McpOutcome::BadRequest(rpc_error(
                        id,
                        -32602,
                        format!("Invalid params: {}", e),
                    ));
                }
            };
            // Emit dispatch_started only after params parse succeeds and before
            // ToolRuntime work begins.
            if let Some(lc) = lifecycle.as_deref_mut() {
                lc.set_tool_name(Some(params.name.clone()));
                lc.dispatch_started();
            }
            // The local_coding model surface rejects tools it does not
            // advertise at the MCP boundary, before ToolRuntime dispatch. The
            // full operator runtime and the canonical Connector keep their
            // existing behavior unchanged.
            if runtime.model_surface() == ModelSurface::LocalCoding
                && !LOCAL_CODING_TOOL_NAMES.contains(&params.name.as_str())
            {
                if let Some(lc) = lifecycle.as_deref() {
                    lc.dispatch_failed("surface_denied");
                    lc.dispatch_finished(false, Some(false), "surface_denied");
                }
                return McpOutcome::BadRequest(rpc_error(
                    id,
                    -32602,
                    format!(
                        "tool '{}' is not available on the local_coding MCP surface; the full operator runtime must be selected explicitly with WEBCODEX_MCP_MODEL_SURFACE=full-operator-v1",
                        params.name
                    ),
                ));
            }
            if runtime.model_surface() == ModelSurface::CanonicalConnector {
                let connector = connector.expect("validated canonical Connector state");
                if params.name == "task_start" && window.is_none() {
                    if let Some(lc) = lifecycle.as_deref() {
                        lc.dispatch_failed("window_identity_unavailable");
                        lc.dispatch_finished(false, Some(false), "window_identity_unavailable");
                    }
                    return McpOutcome::BadRequest(rpc_error(
                        id,
                        -32600,
                        "MCP session identity is unavailable; initialize the connection before starting or continuing project work",
                    ));
                }
                let outcome = connector
                    .call_for_window(
                        &params.name,
                        params.arguments,
                        auth,
                        ConnectorTransport::Mcp,
                        window,
                    )
                    .await;
                if let Some(required_scope) = outcome.required_scope {
                    if let Some(lc) = lifecycle.as_deref() {
                        lc.dispatch_failed("forbidden");
                        lc.dispatch_finished(false, Some(false), "forbidden");
                    }
                    let description = outcome
                        .body
                        .pointer("/error/message")
                        .and_then(Value::as_str)
                        .unwrap_or("connector credential lacks the required scope")
                        .to_string();
                    return oauth_forbidden(Some(required_scope), description);
                }
                if outcome.protocol_error {
                    if let Some(lc) = lifecycle.as_deref() {
                        lc.dispatch_failed("invalid_arguments");
                        lc.dispatch_finished(false, Some(false), "invalid_arguments");
                    }
                    let message = outcome
                        .body
                        .pointer("/error/message")
                        .and_then(Value::as_str)
                        .unwrap_or("invalid connector capability arguments")
                        .to_string();
                    return McpOutcome::BadRequest(rpc_error(id, -32602, message));
                }
                if let Some(lc) = lifecycle.as_deref() {
                    let category = if outcome.ok { "success" } else { "tool_error" };
                    lc.dispatch_finished(true, Some(outcome.ok), category);
                }
                let text =
                    serde_json::to_string(&outcome.body).unwrap_or_else(|_| "{}".to_string());
                let result = json!({
                    "content": [{ "type": "text", "text": text }],
                    "structuredContent": outcome.body,
                    "isError": !outcome.ok
                });
                return McpOutcome::Ok(rpc_result(
                    id,
                    if stateless_2026 {
                        mcp_stateless_result(result, false)
                    } else {
                        result
                    },
                ));
            }
            let session_id = strip_reserved_session_id(&mut params.arguments);
            let as_image_requested = params.name == "read_project_artifact"
                && params.arguments.get("as_image").and_then(Value::as_bool) == Some(true);
            let outcome = runtime
                .call_tool_with_context(
                    KernelToolCallRequest {
                        tool_name: params.name.clone(),
                        arguments: params.arguments,
                    },
                    ToolCallContext {
                        transport: ToolTransport::Mcp,
                        session_id: session_id.as_deref(),
                        auth,
                        window,
                        record_oauth_scope_denials: false,
                    },
                )
                .await;
            let result = match outcome.error_status {
                Some(ToolCallErrorStatus::InsufficientScope {
                    required_scope,
                    description,
                }) => {
                    if let Some(lc) = lifecycle.as_deref() {
                        lc.dispatch_failed("forbidden");
                        lc.dispatch_finished(false, Some(false), "forbidden");
                    }
                    return oauth_forbidden(required_scope, description);
                }
                Some(ToolCallErrorStatus::InvalidArguments { message }) => {
                    if let Some(lc) = lifecycle.as_deref() {
                        lc.dispatch_failed("invalid_arguments");
                        lc.dispatch_finished(false, Some(false), "invalid_arguments");
                    }
                    return McpOutcome::BadRequest(rpc_error(id, -32602, message));
                }
                None => outcome
                    .result
                    .expect("tool kernel outcome without error must include result"),
            };
            debug_assert_eq!(outcome.success, result.success);
            if let Some(lc) = lifecycle.as_deref() {
                // Protocol layer produced a JSON-RPC result (not -32xxx).
                // Tool kernel success is independent (isError / structuredContent).
                let category = if result.success {
                    "success"
                } else {
                    "tool_error"
                };
                if result.success {
                    lc.dispatch_finished(true, Some(true), category);
                } else {
                    lc.dispatch_finished(true, Some(false), category);
                }
            }
            let result = mcp_runtime_tool_result(&params.name, as_image_requested, result);
            rpc_result(
                id,
                if stateless_2026 {
                    mcp_stateless_result(result, false)
                } else {
                    result
                },
            )
        }
        "notifications/initialized" => rpc_result(id, json!({})),
        _ => {
            return McpOutcome::BadRequest(rpc_error(
                id,
                -32601,
                format!("Method not found: {}", request.method),
            ));
        }
    };
    McpOutcome::Ok(response)
}

fn require_mcp_oauth_scope(auth: Option<&AuthContext>, scope: &'static str) -> Option<McpOutcome> {
    let auth = auth?;
    if !auth.is_oauth_token() || auth.has_scope(scope) {
        return None;
    }
    Some(oauth_forbidden(
        Some(scope),
        format!("missing required scope: {}", scope),
    ))
}

fn strip_reserved_session_id(arguments: &mut Value) -> Option<String> {
    arguments
        .as_object_mut()
        .and_then(|obj| obj.remove(MCP_RESERVED_SESSION_ID_FIELD))
        .and_then(|value| value.as_str().map(str::to_string))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn oauth_forbidden(
    required_scope: Option<&'static str>,
    description: impl Into<String>,
) -> McpOutcome {
    McpOutcome::Forbidden {
        body: crate::auth::oauth_insufficient_scope_body(description),
        required_scope,
    }
}

fn rpc_result(id: Option<Value>, result: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id.unwrap_or(Value::Null),
        "result": result,
    })
}

fn rpc_error(id: Option<Value>, code: i64, message: impl Into<String>) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id.unwrap_or(Value::Null),
        "error": {
            "code": code,
            "message": message.into(),
        }
    })
}

#[cfg(test)]
#[path = "mcp_tests.rs"]
mod tests;
