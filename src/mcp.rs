use crate::action_audit::{ActionAudit, ActionAuditRecord};
use crate::auth::AuthContext;
use crate::connector_runtime::{ConnectorRuntime, ConnectorRuntimeSlot, ConnectorTransport};
use crate::json_error;
use crate::model_surface::ModelSurface;
use crate::tool_request_trace::{
    estimate_json_bytes, jsonrpc_id_safe, new_trace_id, ToolRequestLifecycle,
};
use crate::tool_runtime::kernel::{
    HostFileImportTrust, ToolCallContext, ToolCallErrorStatus,
    ToolCallRequest as KernelToolCallRequest, ToolTransport,
};
use crate::tool_runtime::tool_definition::LOCAL_CODING_TOOL_NAMES;
use crate::tool_runtime::{
    registered_tool_specs, validate_project_artifact_export_snapshot,
    ProjectArtifactExportSnapshot, ToolResult, ToolRuntime, ToolSpec, MAX_PROJECT_ARTIFACT_BYTES,
    MAX_READ_PROJECT_ARTIFACT_LENGTH,
};
use base64::{engine::general_purpose, Engine as _};
use futures_util::future::join_all;
use salvo::prelude::*;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;

const MCP_PROTOCOL_VERSION: &str = "2025-06-18";
const MCP_STATELESS_PROTOCOL_VERSION: &str = "2026-07-28";
const MCP_PROTOCOL_VERSION_HEADER: &str = "mcp-protocol-version";
const MCP_METHOD_HEADER: &str = "mcp-method";
const MCP_NAME_HEADER: &str = "mcp-name";
const MCP_ARTIFACT_EXPORT_URI_PREFIX: &str = "webcodex-artifact://export/";
const MCP_ARTIFACT_EXPORT_ID_PREFIX: &str = "wc_export_";
const MCP_ARTIFACT_EXPORT_TTL: Duration = Duration::from_secs(5 * 60);
const MCP_ARTIFACT_EXPORT_ADMISSION_TIMEOUT: Duration = Duration::from_secs(5);
const MCP_ARTIFACT_EXPORT_READ_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_MCP_ARTIFACT_EXPORT_READS: usize = 2;
const MAX_MCP_ARTIFACT_EXPORT_CHUNK_READS: usize = 4;
const MAX_MCP_ARTIFACT_EXPORTS: usize = 128;
const MAX_MCP_ARTIFACT_EXPORTS_PER_CALLER: usize = 16;
const MCP_ARTIFACT_EXPORT_BUSY_CODE: i64 = -32029;
const MCP_HEADER_MISMATCH: i64 = -32020;
const MCP_UNSUPPORTED_PROTOCOL_VERSION: i64 = -32022;
const MCP_SUPPORTED_PROTOCOL_VERSIONS: &[&str] =
    &[MCP_STATELESS_PROTOCOL_VERSION, MCP_PROTOCOL_VERSION];
const MCP_UI_EXTENSION: &str = "io.modelcontextprotocol/ui";
const MCP_COMPUTER_UI_RESOURCE_URI: &str = "ui://webcodex/computer/v9";
const MCP_COMPUTER_UI_RESOURCE_LEGACY_URIS: &[&str] = &[
    "ui://webcodex/computer/v1",
    "ui://webcodex/computer/v2",
    "ui://webcodex/computer/v3",
    "ui://webcodex/computer/v4",
    "ui://webcodex/computer/v5",
    "ui://webcodex/computer/v6",
    "ui://webcodex/computer/v7",
    "ui://webcodex/computer/v8",
];
const MCP_COMPUTER_UI_RESOURCE_TTL_MS: u64 = 24 * 60 * 60 * 1000;
const MCP_COMPUTER_APP_PROBE_TOOL_NAME: &str = "computer_app_probe";
const MCP_COMPUTER_APP_PROBE_RESOURCE_URI: &str = "ui://webcodex/computer-probe/v1";
const MCP_COMPUTER_APP_IMAGE_PROBE_TOOL_NAME: &str = "computer_app_image_probe";
const MCP_COMPUTER_APP_IMAGE_PROBE_RESOURCE_URI: &str = "ui://webcodex/computer-image-probe/v1";
const MCP_COMPUTER_APP_SNAPSHOT_PROBE_TOOL_NAME: &str = "computer_app_snapshot_probe";
const MCP_COMPUTER_APP_SNAPSHOT_PROBE_RESOURCE_URI: &str =
    "ui://webcodex/computer-snapshot-probe/v1";
const MCP_COMPUTER_APP_SNAPSHOT_DECODE_PROBE_TOOL_NAME: &str = "computer_app_snapshot_decode_probe";
const MCP_COMPUTER_APP_SNAPSHOT_DECODE_PROBE_RESOURCE_URI: &str =
    "ui://webcodex/computer-snapshot-decode-probe/v1";
const MCP_COMPUTER_APP_IMAGE_SIZE_PROBE_TOOL_NAME: &str = "computer_app_image_size_probe";
const MCP_COMPUTER_APP_IMAGE_SIZE_PROBE_RESOURCE_URI: &str =
    "ui://webcodex/computer-image-size-probe/v1";
const MCP_COMPUTER_APP_IMAGE_SIZE_PROBE_CHOICES: &[(&str, usize)] = &[
    ("1k", 1024),
    ("16k", 16 * 1024),
    ("64k", 64 * 1024),
    ("128k", 128 * 1024),
    ("256k", 256 * 1024),
    ("512k", 512 * 1024),
];
const MCP_COMPUTER_APP_IMAGE_DIMENSION_PROBE_TOOL_NAME: &str = "computer_app_image_dimension_probe";
const MCP_COMPUTER_APP_IMAGE_DIMENSION_PROBE_RESOURCE_URI: &str =
    "ui://webcodex/computer-image-dimension-probe/v1";
const MCP_COMPUTER_APP_IMAGE_DIMENSION_PROBE_BYTES: usize = 256 * 1024;
const MCP_COMPUTER_APP_IMAGE_DIMENSION_PROBE_CHOICES: &[(&str, u32, u32, &str)] = &[
    ("640x360", 640, 360, "eNrtwQENAAAAwqD3T+3sARQAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAN3HoAAE="),
    ("1280x720", 1280, 720, "eNrtwTEBAAAAwqD1T20JT6AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAeBrE3wAB"),
    ("1920x1080", 1920, 1080, "eNrtwQENAAAAwqD3T20PBxQAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAMCnAfjlAAE="),
    ("2560x1440", 2560, 1440, "eNrtwTEBAAAAwqD1T20ND6AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAACAdwMOCQAB"),
    ("3840x2160", 3840, 2160, "eNrtwQEBAAAAgiD/r25IQAEAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAADArwHbUQAB"),
];
const MCP_COMPUTER_APP_IMAGE_MATRIX_PROBE_TOOL_NAME: &str = "computer_app_image_matrix_probe";
const MCP_COMPUTER_APP_IMAGE_MATRIX_PROBE_RESOURCE_URI: &str =
    "ui://webcodex/computer-image-matrix-probe/v1";
const MCP_COMPUTER_APP_IMAGE_MATRIX_PROBE_PAYLOAD_CHOICES: &[(&str, usize)] = &[
    ("64k", 64 * 1024),
    ("128k", 128 * 1024),
    ("256k", 256 * 1024),
    ("512k", 512 * 1024),
];
const MCP_COMPUTER_APP_IMAGE_MATRIX_PROBE_JPEG_BASES: &[(&str, u32, u32, &[u8])] = &[
    (
        "640x360",
        640,
        360,
        include_bytes!("mcp_probe_assets/matrix-jpeg-640x360.jpg"),
    ),
    (
        "1280x720",
        1280,
        720,
        include_bytes!("mcp_probe_assets/matrix-jpeg-1280x720.jpg"),
    ),
    (
        "1920x1080",
        1920,
        1080,
        include_bytes!("mcp_probe_assets/matrix-jpeg-1920x1080.jpg"),
    ),
    (
        "2560x1440",
        2560,
        1440,
        include_bytes!("mcp_probe_assets/matrix-jpeg-2560x1440.jpg"),
    ),
    (
        "3840x2160",
        3840,
        2160,
        include_bytes!("mcp_probe_assets/matrix-jpeg-3840x2160.jpg"),
    ),
];
const MCP_COMPUTER_APP_IMAGE_PROBE_PNG_BASE64: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAusB9Y9Z0GkAAAAASUVORK5CYII=";
const MCP_COMPUTER_APP_IMAGE_PROBE_PNG_BYTES: u64 = 68;
const MCP_COMPUTER_APP_IMAGE_PROBE_PNG_SHA256: &str =
    "61576be28adeae2826f68b41489eb35502cd873c884ad2acc50b327566503c1c";
const MCP_COMPUTER_UI_DOMAIN: &str = "https://sg4.yyjeqhc.cn";
const MCP_UI_RESOURCE_MIME_TYPE: &str = "text/html;profile=mcp-app";
const MCP_COMPUTER_APP_HTML: &str = include_str!("mcp_computer_app.html");
const MCP_COMPUTER_APP_PROBE_HTML: &str = include_str!("mcp_computer_probe_app.html");
const MCP_COMPUTER_APP_IMAGE_PROBE_HTML: &str = include_str!("mcp_computer_image_probe_app.html");
const MCP_COMPUTER_APP_SNAPSHOT_PROBE_HTML: &str =
    include_str!("mcp_computer_snapshot_probe_app.html");
const MCP_COMPUTER_APP_SNAPSHOT_DECODE_PROBE_HTML: &str =
    include_str!("mcp_computer_snapshot_decode_probe_app.html");
const MCP_COMPUTER_APP_IMAGE_SIZE_PROBE_HTML: &str =
    include_str!("mcp_computer_image_size_probe_app.html");
const MCP_COMPUTER_APP_IMAGE_DIMENSION_PROBE_HTML: &str =
    include_str!("mcp_computer_image_dimension_probe_app.html");
const MCP_COMPUTER_APP_IMAGE_MATRIX_PROBE_HTML: &str =
    include_str!("mcp_computer_image_matrix_probe_app.html");

/// Single source of truth for the JSON-RPC methods advertised by `GET /mcp`.
/// Must match the dispatch arms in `handle_mcp_request_with_lifecycle`;
/// pinned by `mcp_info_advertised_methods_match_dispatch`.
const MCP_INFO_METHODS: &[&str] = &[
    "server/discover",
    "initialize",
    "ping",
    "tools/list",
    "tools/call",
    "resources/list",
    "resources/read",
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum McpProtocolEra {
    Legacy,
    Stateless2026,
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

fn request_protocol_version(params: &Value) -> Option<&str> {
    params
        .get("_meta")
        .and_then(|meta| meta.get("io.modelcontextprotocol/protocolVersion"))
        .and_then(Value::as_str)
}

fn request_client_capabilities(params: &Value) -> Option<&Value> {
    params
        .get("_meta")
        .and_then(|meta| meta.get("io.modelcontextprotocol/clientCapabilities"))
}

fn request_supports_mcp_apps(params: &Value) -> bool {
    let Some(extension) = request_client_capabilities(params)
        .and_then(|capabilities| capabilities.get("extensions"))
        .and_then(|extensions| extensions.get(MCP_UI_EXTENSION))
        .and_then(Value::as_object)
    else {
        return false;
    };
    match extension.get("mimeTypes").and_then(Value::as_array) {
        Some(mime_types) => mime_types
            .iter()
            .any(|mime| mime.as_str() == Some(MCP_UI_RESOURCE_MIME_TYPE)),
        None => false,
    }
}

fn model_surface_supports_computer_app(model_surface: ModelSurface) -> bool {
    model_surface == ModelSurface::FullOperatorRuntime
}

fn request_client_info_is_valid(params: &Value) -> bool {
    let Some(meta) = params.get("_meta").and_then(Value::as_object) else {
        return true;
    };
    let Some(client_info) = meta.get("io.modelcontextprotocol/clientInfo") else {
        return true;
    };
    let Some(client_info) = client_info.as_object() else {
        return false;
    };
    client_info.get("name").is_some_and(Value::is_string)
        && client_info.get("version").is_some_and(Value::is_string)
}

fn request_header<'a>(req: &'a Request, name: &str) -> Option<&'a str> {
    req.headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
}

fn decode_mcp_name_header(value: &str) -> Result<String, ()> {
    let Some(encoded) = value
        .strip_prefix("=?base64?")
        .and_then(|value| value.strip_suffix("?="))
    else {
        return Ok(value.to_string());
    };
    let bytes = base64::Engine::decode(&general_purpose::STANDARD, encoded).map_err(|_| ())?;
    String::from_utf8(bytes).map_err(|_| ())
}

fn request_mcp_name(request: &JsonRpcRequest) -> Option<Option<&str>> {
    match request.method.as_str() {
        "tools/call" | "prompts/get" => Some(request.params.get("name").and_then(Value::as_str)),
        "resources/read" => Some(request.params.get("uri").and_then(Value::as_str)),
        _ => None,
    }
}

fn header_mismatch(id: Option<Value>, message: impl Into<String>) -> Value {
    rpc_error(id, MCP_HEADER_MISMATCH, message)
}

fn unsupported_protocol_version(id: Option<Value>, requested: &str) -> Value {
    rpc_error_with_data(
        id,
        MCP_UNSUPPORTED_PROTOCOL_VERSION,
        format!("Unsupported MCP protocol version: {requested}"),
        json!({
            "supported": MCP_SUPPORTED_PROTOCOL_VERSIONS,
            "requested": requested,
        }),
    )
}

#[cfg(test)]
fn inferred_protocol_era(request: &JsonRpcRequest) -> McpProtocolEra {
    if request_protocol_version(&request.params) == Some(MCP_STATELESS_PROTOCOL_VERSION) {
        McpProtocolEra::Stateless2026
    } else {
        McpProtocolEra::Legacy
    }
}

/// Validate the HTTP-only request metadata introduced by MCP 2026-07-28.
/// Requests with no modern markers retain the existing 2025-06-18 behavior.
fn validate_http_protocol(
    req: &Request,
    request: &JsonRpcRequest,
) -> Result<McpProtocolEra, Value> {
    let id = request.id.clone();
    let header_version = request_header(req, MCP_PROTOCOL_VERSION_HEADER);
    let body_version = request_protocol_version(&request.params);

    if let (Some(header), Some(body)) = (header_version, body_version) {
        if header != body {
            return Err(header_mismatch(
                id.clone(),
                format!(
                    "Header mismatch: {MCP_PROTOCOL_VERSION_HEADER} header value '{header}' does not match params._meta protocolVersion '{body}'"
                ),
            ));
        }
    }

    for requested in [header_version, body_version].into_iter().flatten() {
        if !MCP_SUPPORTED_PROTOCOL_VERSIONS.contains(&requested) {
            return Err(unsupported_protocol_version(id.clone(), requested));
        }
    }

    let stateless = header_version == Some(MCP_STATELESS_PROTOCOL_VERSION)
        || body_version == Some(MCP_STATELESS_PROTOCOL_VERSION);
    if !stateless {
        return Ok(McpProtocolEra::Legacy);
    }

    if header_version != Some(MCP_STATELESS_PROTOCOL_VERSION) {
        return Err(header_mismatch(
            id,
            format!(
                "Header mismatch: {MCP_PROTOCOL_VERSION_HEADER} is required and must equal {MCP_STATELESS_PROTOCOL_VERSION}"
            ),
        ));
    }
    if body_version != Some(MCP_STATELESS_PROTOCOL_VERSION) {
        return Err(header_mismatch(
            id,
            format!(
                "Header mismatch: {MCP_PROTOCOL_VERSION_HEADER} does not match params._meta protocolVersion"
            ),
        ));
    }
    if request.id.is_some() {
        if !request_client_capabilities(&request.params).is_some_and(Value::is_object) {
            return Err(rpc_error(
                id.clone(),
                -32602,
                "Invalid params: MCP 2026-07-28 requests require params._meta clientCapabilities",
            ));
        }
        if !request_client_info_is_valid(&request.params) {
            return Err(rpc_error(
                id,
                -32602,
                "Invalid params: params._meta clientInfo must contain string name and version fields when present",
            ));
        }
    }

    match request_header(req, MCP_METHOD_HEADER) {
        Some(method) if method == request.method => {}
        Some(method) => {
            return Err(header_mismatch(
                id,
                format!(
                    "Header mismatch: Mcp-Method header value '{method}' does not match body value '{}'",
                    request.method
                ),
            ));
        }
        None => {
            return Err(header_mismatch(
                id,
                "Header mismatch: required Mcp-Method header is missing or malformed",
            ));
        }
    }

    if let Some(body_name) = request_mcp_name(request) {
        let header_name = request_header(req, MCP_NAME_HEADER)
            .and_then(|value| decode_mcp_name_header(value).ok());
        match (header_name.as_deref(), body_name) {
            (Some(header), Some(body)) if header == body => {}
            (Some(header), Some(body)) => {
                return Err(header_mismatch(
                    id,
                    format!(
                        "Header mismatch: Mcp-Name header value '{header}' does not match body value '{body}'"
                    ),
                ));
            }
            _ => {
                return Err(header_mismatch(
                    id,
                    "Header mismatch: required Mcp-Name header is missing, malformed, or has no matching body value",
                ));
            }
        }
    }

    Ok(McpProtocolEra::Stateless2026)
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
///
/// Env adapter: resolves the `WEBCODEX_MCP_COMPACT_SCHEMAS` switch and
/// delegates to the pure renderer.
fn mcp_tools_list_payload(model_surface: ModelSurface) -> Value {
    mcp_tools_list_payload_with_compact(model_surface, crate::config::mcp_compact_schemas_enabled())
}

/// Pure tools/list rendering with an explicit compact switch; no env access.
/// Production resolves the switch from the env adapter above; tests pass an
/// explicit bool so they never need process-global env. The schema shape is
/// identical to the adapter path: `compact` only omits `outputSchema`.
fn mcp_tools_list_payload_with_compact(model_surface: ModelSurface, compact: bool) -> Value {
    mcp_tools_list_payload_with_features(model_surface, compact, false, false)
}

fn mcp_tools_list_payload_with_compact_and_app(
    model_surface: ModelSurface,
    compact: bool,
    app_enabled: bool,
) -> Value {
    mcp_tools_list_payload_with_features(model_surface, compact, app_enabled, true)
}

fn mcp_tools_list_payload_with_features(
    model_surface: ModelSurface,
    compact: bool,
    app_enabled: bool,
    artifact_export_enabled: bool,
) -> Value {
    let specs = match model_surface {
        ModelSurface::CanonicalConnector => crate::connector_runtime::surface::capability_specs(),
        ModelSurface::LocalCoding => crate::model_surface::local_coding_tool_specs(),
        ModelSurface::FullOperatorRuntime => registered_tool_specs(),
    };
    let mut tools: Vec<Value> = specs
        .into_iter()
        .filter(|spec| artifact_export_enabled || spec.name != "export_project_artifact")
        .map(|spec| mcp_tool_spec_json(spec, compact, app_enabled))
        .collect();
    if app_enabled && model_surface == ModelSurface::FullOperatorRuntime {
        tools.push(mcp_computer_app_probe_tool_spec(compact));
        tools.push(mcp_computer_app_image_probe_tool_spec(compact));
        tools.push(mcp_computer_app_snapshot_probe_tool_spec(compact));
        tools.push(mcp_computer_app_snapshot_decode_probe_tool_spec(compact));
        tools.push(mcp_computer_app_image_size_probe_tool_spec(compact));
        tools.push(mcp_computer_app_image_dimension_probe_tool_spec(compact));
        tools.push(mcp_computer_app_image_matrix_probe_tool_spec(compact));
    }
    json!({ "tools": tools })
}

fn mcp_tool_spec_json(mut spec: ToolSpec, compact: bool, app_enabled: bool) -> Value {
    let tool_name = spec.name.clone();
    if tool_name == "read_project_artifact" {
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
    let mut value = if compact {
        json!({
            "name": spec.name,
            "description": spec.description,
            "inputSchema": spec.input_schema,
            "annotations": spec.annotations,
        })
    } else {
        // Match ToolSpec's camelCase serde so default behavior is unchanged.
        serde_json::to_value(spec).unwrap_or_else(|_| json!({}))
    };
    if tool_name == "import_conversation_files_to_project" {
        if let Some(object) = value.as_object_mut() {
            object.insert(
                "_meta".to_string(),
                json!({"openai/fileParams": ["openaiFileIdRefs"]}),
            );
        }
    }
    if app_enabled && tool_name == "computer_snapshot" {
        if let Some(object) = value.as_object_mut() {
            object.insert(
                "_meta".to_string(),
                json!({
                    "ui": {
                        "resourceUri": MCP_COMPUTER_UI_RESOURCE_URI,
                        "visibility": ["model", "app"]
                    },
                    "openai/outputTemplate": MCP_COMPUTER_UI_RESOURCE_URI
                }),
            );
        }
    }
    value
}

fn mcp_computer_app_probe_tool_spec(compact: bool) -> Value {
    let mut value = json!({
        "name": MCP_COMPUTER_APP_PROBE_TOOL_NAME,
        "description": "Experimental MCP-only control probe for the WebCodex Computer App. Returns one tiny deterministic result without contacting a Runner or producing image content.",
        "inputSchema": {
            "type": "object",
            "properties": {},
            "additionalProperties": false
        },
        "annotations": {
            "title": "Computer App Probe",
            "readOnlyHint": true,
            "destructiveHint": false,
            "idempotentHint": true,
            "openWorldHint": false
        },
        "_meta": {
            "ui": {
                "resourceUri": MCP_COMPUTER_APP_PROBE_RESOURCE_URI,
                "visibility": ["model", "app"]
            },
            "openai/outputTemplate": MCP_COMPUTER_APP_PROBE_RESOURCE_URI
        }
    });
    if !compact {
        value["outputSchema"] = json!({
            "type": "object",
            "properties": {
                "success": { "type": "boolean" },
                "output": {
                    "type": "object",
                    "properties": {
                        "probe": { "type": "string" },
                        "payload": { "type": "string" },
                        "runner_used": { "type": "boolean" }
                    },
                    "required": ["probe", "payload", "runner_used"],
                    "additionalProperties": false
                },
                "error": {}
            },
            "required": ["success", "output", "error"],
            "additionalProperties": false
        });
    }
    value
}

fn mcp_computer_app_image_probe_tool_spec(compact: bool) -> Value {
    let mut value = json!({
        "name": MCP_COMPUTER_APP_IMAGE_PROBE_TOOL_NAME,
        "description": "Experimental MCP-only native-image control probe for the WebCodex Computer App. Returns one built-in 1x1 PNG through the same MCP image framing used by computer_snapshot, without contacting a Runner.",
        "inputSchema": {
            "type": "object",
            "properties": {},
            "additionalProperties": false
        },
        "annotations": {
            "title": "Computer App Image Probe",
            "readOnlyHint": true,
            "destructiveHint": false,
            "idempotentHint": true,
            "openWorldHint": false
        },
        "_meta": {
            "ui": {
                "resourceUri": MCP_COMPUTER_APP_IMAGE_PROBE_RESOURCE_URI,
                "visibility": ["model", "app"]
            },
            "openai/outputTemplate": MCP_COMPUTER_APP_IMAGE_PROBE_RESOURCE_URI
        }
    });
    if !compact {
        value["outputSchema"] = json!({
            "type": "object",
            "properties": {
                "success": { "type": "boolean" },
                "output": {
                    "type": "object",
                    "properties": {
                        "probe": { "type": "string" },
                        "runner_used": { "type": "boolean" },
                        "client_id": { "type": "string" },
                        "surface": { "type": "object" },
                        "width": { "type": "integer" },
                        "height": { "type": "integer" },
                        "mime_type": { "type": "string" },
                        "file_bytes": { "type": "integer" },
                        "sha256": { "type": "string" },
                        "content_delivery": { "type": "string" }
                    },
                    "required": ["probe", "runner_used", "client_id", "surface", "width", "height", "mime_type", "file_bytes", "sha256", "content_delivery"],
                    "additionalProperties": false
                },
                "error": {}
            },
            "required": ["success", "output", "error"],
            "additionalProperties": false
        });
    }
    value
}

fn mcp_computer_app_snapshot_probe_tool_spec(compact: bool) -> Value {
    let mut spec = registered_tool_specs()
        .into_iter()
        .find(|spec| spec.name == "computer_snapshot")
        .expect("computer_snapshot runtime spec");
    spec.name = MCP_COMPUTER_APP_SNAPSHOT_PROBE_TOOL_NAME.to_string();
    spec.description = "Experimental MCP-only real-screenshot control probe. Delegates to the existing computer_snapshot ToolRuntime path, but uses a fresh MCP App tool name and resource binding.".to_string();
    if let Some(annotations) = spec.annotations.as_object_mut() {
        annotations.insert(
            "title".to_string(),
            Value::String("Computer App Snapshot Probe".to_string()),
        );
    }
    let mut value = if compact {
        json!({
            "name": spec.name,
            "description": spec.description,
            "inputSchema": spec.input_schema,
            "annotations": spec.annotations,
        })
    } else {
        serde_json::to_value(spec).unwrap_or_else(|_| json!({}))
    };
    if let Some(object) = value.as_object_mut() {
        object.insert(
            "_meta".to_string(),
            json!({
                "ui": {
                    "resourceUri": MCP_COMPUTER_APP_SNAPSHOT_PROBE_RESOURCE_URI,
                    "visibility": ["model", "app"]
                },
                "openai/outputTemplate": MCP_COMPUTER_APP_SNAPSHOT_PROBE_RESOURCE_URI
            }),
        );
    }
    value
}

fn mcp_computer_app_snapshot_decode_probe_tool_spec(compact: bool) -> Value {
    let mut spec = registered_tool_specs()
        .into_iter()
        .find(|spec| spec.name == "computer_snapshot")
        .expect("computer_snapshot runtime spec");
    spec.name = MCP_COMPUTER_APP_SNAPSHOT_DECODE_PROBE_TOOL_NAME.to_string();
    spec.description = "Experimental MCP-only real-screenshot decode probe. Delegates to the existing computer_snapshot ToolRuntime path and native-image framing, while its fresh App waits for browser image decode and verifies intrinsic dimensions.".to_string();
    if let Some(annotations) = spec.annotations.as_object_mut() {
        annotations.insert(
            "title".to_string(),
            Value::String("Computer Snapshot Decode Probe".to_string()),
        );
    }
    let mut value = if compact {
        json!({
            "name": spec.name,
            "description": spec.description,
            "inputSchema": spec.input_schema,
            "annotations": spec.annotations,
        })
    } else {
        serde_json::to_value(spec).unwrap_or_else(|_| json!({}))
    };
    if let Some(object) = value.as_object_mut() {
        object.insert(
            "_meta".to_string(),
            json!({
                "ui": {
                    "resourceUri": MCP_COMPUTER_APP_SNAPSHOT_DECODE_PROBE_RESOURCE_URI,
                    "visibility": ["model", "app"]
                },
                "openai/outputTemplate": MCP_COMPUTER_APP_SNAPSHOT_DECODE_PROBE_RESOURCE_URI
            }),
        );
    }
    value
}

fn mcp_computer_app_image_size_probe_tool_spec(compact: bool) -> Value {
    let mut value = json!({
        "name": MCP_COMPUTER_APP_IMAGE_SIZE_PROBE_TOOL_NAME,
        "description": "Experimental MCP-only native-image payload-size control probe. Returns a deterministic 1x1 PNG at one of six exact decoded byte sizes through the same MCP image framing used by computer_snapshot, without contacting a Runner.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "size": {
                    "type": "string",
                    "enum": ["1k", "16k", "64k", "128k", "256k", "512k"],
                    "description": "Exact decoded PNG payload size to return."
                }
            },
            "required": ["size"],
            "additionalProperties": false
        },
        "annotations": {
            "title": "Computer App Image Size Probe",
            "readOnlyHint": true,
            "destructiveHint": false,
            "idempotentHint": true,
            "openWorldHint": false
        },
        "_meta": {
            "ui": {
                "resourceUri": MCP_COMPUTER_APP_IMAGE_SIZE_PROBE_RESOURCE_URI,
                "visibility": ["model", "app"]
            },
            "openai/outputTemplate": MCP_COMPUTER_APP_IMAGE_SIZE_PROBE_RESOURCE_URI
        }
    });
    if !compact {
        value["outputSchema"] = json!({
            "type": "object",
            "properties": {
                "success": { "type": "boolean" },
                "output": {
                    "type": "object",
                    "properties": {
                        "probe": { "type": "string" },
                        "size": { "type": "string" },
                        "runner_used": { "type": "boolean" },
                        "client_id": { "type": "string" },
                        "surface": { "type": "object" },
                        "width": { "type": "integer" },
                        "height": { "type": "integer" },
                        "mime_type": { "type": "string" },
                        "file_bytes": { "type": "integer" },
                        "sha256": { "type": "string" },
                        "content_delivery": { "type": "string" }
                    },
                    "required": ["probe", "size", "runner_used", "client_id", "surface", "width", "height", "mime_type", "file_bytes", "sha256", "content_delivery"],
                    "additionalProperties": false
                },
                "error": {}
            },
            "required": ["success", "output", "error"],
            "additionalProperties": false
        });
    }
    value
}

fn mcp_computer_app_image_dimension_probe_tool_spec(compact: bool) -> Value {
    let mut value = json!({
        "name": MCP_COMPUTER_APP_IMAGE_DIMENSION_PROBE_TOOL_NAME,
        "description": "Experimental MCP-only native-image dimension control probe. Returns a deterministic black 1-bit grayscale PNG at one of five intrinsic dimensions while keeping the decoded PNG file exactly 256 KiB, through the same MCP image framing used by computer_snapshot and without contacting a Runner.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "dimension": {
                    "type": "string",
                    "enum": ["640x360", "1280x720", "1920x1080", "2560x1440", "3840x2160"],
                    "description": "Intrinsic PNG dimensions to return; decoded file size remains exactly 256 KiB."
                }
            },
            "required": ["dimension"],
            "additionalProperties": false
        },
        "annotations": {
            "title": "Computer App Image Dimension Probe",
            "readOnlyHint": true,
            "destructiveHint": false,
            "idempotentHint": true,
            "openWorldHint": false
        },
        "_meta": {
            "ui": {
                "resourceUri": MCP_COMPUTER_APP_IMAGE_DIMENSION_PROBE_RESOURCE_URI,
                "visibility": ["model", "app"]
            },
            "openai/outputTemplate": MCP_COMPUTER_APP_IMAGE_DIMENSION_PROBE_RESOURCE_URI
        }
    });
    if !compact {
        value["outputSchema"] = json!({
            "type": "object",
            "properties": {
                "success": { "type": "boolean" },
                "output": {
                    "type": "object",
                    "properties": {
                        "probe": { "type": "string" },
                        "dimension": { "type": "string" },
                        "runner_used": { "type": "boolean" },
                        "client_id": { "type": "string" },
                        "surface": { "type": "object" },
                        "width": { "type": "integer" },
                        "height": { "type": "integer" },
                        "mime_type": { "type": "string" },
                        "file_bytes": { "type": "integer" },
                        "sha256": { "type": "string" },
                        "content_delivery": { "type": "string" }
                    },
                    "required": ["probe", "dimension", "runner_used", "client_id", "surface", "width", "height", "mime_type", "file_bytes", "sha256", "content_delivery"],
                    "additionalProperties": false
                },
                "error": {}
            },
            "required": ["success", "output", "error"],
            "additionalProperties": false
        });
    }
    value
}

fn mcp_computer_app_image_matrix_probe_tool_spec(compact: bool) -> Value {
    let mut value = json!({
        "name": MCP_COMPUTER_APP_IMAGE_MATRIX_PROBE_TOOL_NAME,
        "description": "Experimental MCP-only synthetic native-image matrix probe. Returns deterministic PNG or JPEG content at one selected intrinsic dimension and exact decoded payload size through the same MCP image framing used by computer_snapshot, without contacting a Runner.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "codec": { "type": "string", "enum": ["png", "jpeg"], "description": "Synthetic image codec." },
                "dimension": { "type": "string", "enum": ["640x360", "1280x720", "1920x1080", "2560x1440", "3840x2160"], "description": "Intrinsic image dimensions." },
                "payload": { "type": "string", "enum": ["64k", "128k", "256k", "512k"], "description": "Exact decoded image-file size to return." }
            },
            "required": ["codec", "dimension", "payload"],
            "additionalProperties": false
        },
        "annotations": {
            "title": "Computer App Image Matrix Probe",
            "readOnlyHint": true,
            "destructiveHint": false,
            "idempotentHint": true,
            "openWorldHint": false
        },
        "_meta": {
            "ui": { "resourceUri": MCP_COMPUTER_APP_IMAGE_MATRIX_PROBE_RESOURCE_URI, "visibility": ["model", "app"] },
            "openai/outputTemplate": MCP_COMPUTER_APP_IMAGE_MATRIX_PROBE_RESOURCE_URI
        }
    });
    if !compact {
        value["outputSchema"] = json!({
            "type": "object",
            "properties": {
                "success": { "type": "boolean" },
                "output": {
                    "type": "object",
                    "properties": {
                        "probe": { "type": "string" },
                        "codec": { "type": "string" },
                        "dimension": { "type": "string" },
                        "payload": { "type": "string" },
                        "runner_used": { "type": "boolean" },
                        "client_id": { "type": "string" },
                        "surface": { "type": "object" },
                        "width": { "type": "integer" },
                        "height": { "type": "integer" },
                        "mime_type": { "type": "string" },
                        "file_bytes": { "type": "integer" },
                        "sha256": { "type": "string" },
                        "content_delivery": { "type": "string" }
                    },
                    "required": ["probe", "codec", "dimension", "payload", "runner_used", "client_id", "surface", "width", "height", "mime_type", "file_bytes", "sha256", "content_delivery"],
                    "additionalProperties": false
                },
                "error": {}
            },
            "required": ["success", "output", "error"],
            "additionalProperties": false
        });
    }
    value
}

fn mcp_computer_app_resource_meta() -> Value {
    json!({
        "ui": {
            "prefersBorder": true,
            "domain": MCP_COMPUTER_UI_DOMAIN,
            "csp": {
                "connectDomains": [],
                "resourceDomains": []
            }
        }
    })
}

fn mcp_computer_app_resources_list() -> Value {
    json!({
        "resources": [
            {
                "uri": MCP_COMPUTER_UI_RESOURCE_URI,
                "name": "WebCodex Computer",
                "description": "Minimal read-only WebCodex Computer screenshot card that performs only the standard MCP Apps handshake and renders the native computer_snapshot image.",
                "mimeType": MCP_UI_RESOURCE_MIME_TYPE,
                "_meta": mcp_computer_app_resource_meta()
            },
            {
                "uri": MCP_COMPUTER_APP_PROBE_RESOURCE_URI,
                "name": "WebCodex Computer App Probe",
                "description": "Experimental control card that performs the same minimal MCP Apps handshake and renders one tiny deterministic tool result without Runner or image content.",
                "mimeType": MCP_UI_RESOURCE_MIME_TYPE,
                "_meta": mcp_computer_app_resource_meta()
            },
            {
                "uri": MCP_COMPUTER_APP_IMAGE_PROBE_RESOURCE_URI,
                "name": "WebCodex Computer App Image Probe",
                "description": "Experimental control card that performs the same minimal MCP Apps handshake and renders one built-in 1x1 PNG delivered through native MCP image content without Runner access.",
                "mimeType": MCP_UI_RESOURCE_MIME_TYPE,
                "_meta": mcp_computer_app_resource_meta()
            },
            {
                "uri": MCP_COMPUTER_APP_SNAPSHOT_PROBE_RESOURCE_URI,
                "name": "WebCodex Computer App Snapshot Probe",
                "description": "Experimental control card that reuses the production Computer App HTML while rendering a real Runner screenshot through a fresh tool and resource binding.",
                "mimeType": MCP_UI_RESOURCE_MIME_TYPE,
                "_meta": mcp_computer_app_resource_meta()
            },
            {
                "uri": MCP_COMPUTER_APP_SNAPSHOT_DECODE_PROBE_RESOURCE_URI,
                "name": "WebCodex Computer Snapshot Decode Probe",
                "description": "Experimental control card that renders a real Runner computer_snapshot and reports whether the browser actually decodes the delivered JPEG with matching intrinsic dimensions.",
                "mimeType": MCP_UI_RESOURCE_MIME_TYPE,
                "_meta": mcp_computer_app_resource_meta()
            },
            {
                "uri": MCP_COMPUTER_APP_IMAGE_SIZE_PROBE_RESOURCE_URI,
                "name": "WebCodex Computer App Image Size Probe",
                "description": "Experimental control card that renders a deterministic 1x1 PNG at one selected decoded payload size through native MCP image content without Runner access.",
                "mimeType": MCP_UI_RESOURCE_MIME_TYPE,
                "_meta": mcp_computer_app_resource_meta()
            },
            {
                "uri": MCP_COMPUTER_APP_IMAGE_DIMENSION_PROBE_RESOURCE_URI,
                "name": "WebCodex Computer App Image Dimension Probe",
                "description": "Experimental control card that renders a deterministic black PNG at one selected intrinsic dimension while keeping the decoded native-image payload exactly 256 KiB and avoiding Runner access.",
                "mimeType": MCP_UI_RESOURCE_MIME_TYPE,
                "_meta": mcp_computer_app_resource_meta()
            },
            {
                "uri": MCP_COMPUTER_APP_IMAGE_MATRIX_PROBE_RESOURCE_URI,
                "name": "WebCodex Computer App Image Matrix Probe",
                "description": "Experimental control card that decodes a selected synthetic PNG or JPEG across a closed dimension and exact-payload matrix without Runner access.",
                "mimeType": MCP_UI_RESOURCE_MIME_TYPE,
                "_meta": mcp_computer_app_resource_meta()
            }
        ]
    })
}

fn is_mcp_computer_app_resource_uri(uri: &str) -> bool {
    uri == MCP_COMPUTER_UI_RESOURCE_URI
        || uri == MCP_COMPUTER_APP_PROBE_RESOURCE_URI
        || uri == MCP_COMPUTER_APP_IMAGE_PROBE_RESOURCE_URI
        || uri == MCP_COMPUTER_APP_SNAPSHOT_PROBE_RESOURCE_URI
        || uri == MCP_COMPUTER_APP_SNAPSHOT_DECODE_PROBE_RESOURCE_URI
        || uri == MCP_COMPUTER_APP_IMAGE_SIZE_PROBE_RESOURCE_URI
        || uri == MCP_COMPUTER_APP_IMAGE_DIMENSION_PROBE_RESOURCE_URI
        || uri == MCP_COMPUTER_APP_IMAGE_MATRIX_PROBE_RESOURCE_URI
        || MCP_COMPUTER_UI_RESOURCE_LEGACY_URIS.contains(&uri)
}

fn mcp_computer_app_resource_read(uri: &str) -> Option<Value> {
    // ChatGPT can retain an older tool descriptor across connector refreshes.
    // Keep prior computer App URIs as hidden read aliases so an already-bound
    // card can fetch the current safe template. resources/list and tools/list
    // still advertise only canonical URIs.
    let text = if uri == MCP_COMPUTER_APP_PROBE_RESOURCE_URI {
        MCP_COMPUTER_APP_PROBE_HTML
    } else if uri == MCP_COMPUTER_APP_IMAGE_PROBE_RESOURCE_URI {
        MCP_COMPUTER_APP_IMAGE_PROBE_HTML
    } else if uri == MCP_COMPUTER_APP_IMAGE_SIZE_PROBE_RESOURCE_URI {
        MCP_COMPUTER_APP_IMAGE_SIZE_PROBE_HTML
    } else if uri == MCP_COMPUTER_APP_IMAGE_DIMENSION_PROBE_RESOURCE_URI {
        MCP_COMPUTER_APP_IMAGE_DIMENSION_PROBE_HTML
    } else if uri == MCP_COMPUTER_APP_IMAGE_MATRIX_PROBE_RESOURCE_URI {
        MCP_COMPUTER_APP_IMAGE_MATRIX_PROBE_HTML
    } else if uri == MCP_COMPUTER_APP_SNAPSHOT_DECODE_PROBE_RESOURCE_URI {
        MCP_COMPUTER_APP_SNAPSHOT_DECODE_PROBE_HTML
    } else if uri == MCP_COMPUTER_APP_SNAPSHOT_PROBE_RESOURCE_URI {
        MCP_COMPUTER_APP_SNAPSHOT_PROBE_HTML
    } else if uri == MCP_COMPUTER_UI_RESOURCE_URI
        || MCP_COMPUTER_UI_RESOURCE_LEGACY_URIS.contains(&uri)
    {
        MCP_COMPUTER_APP_HTML
    } else {
        return None;
    };
    Some(json!({
        "contents": [{
            "uri": uri,
            "mimeType": MCP_UI_RESOURCE_MIME_TYPE,
            "text": text,
            "_meta": mcp_computer_app_resource_meta()
        }]
    }))
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum McpArtifactExportCallerBinding {
    Bootstrap,
    ApiToken {
        api_key_id: String,
    },
    AgentToken {
        api_key_id: String,
    },
    AccountCredential {
        user_id: String,
    },
    OAuthUser {
        user_id: String,
        client_id: String,
    },
    OAuthSharedKey {
        shared_key_hash: String,
        client_id: String,
    },
    SharedKey {
        shared_key_hash: String,
    },
    ProjectCredential {
        project_grant_id: String,
    },
}

fn mcp_artifact_export_caller_binding(
    auth: Option<&AuthContext>,
) -> Result<McpArtifactExportCallerBinding, &'static str> {
    let auth = auth.ok_or("authenticated caller identity is unavailable")?;
    match auth.kind {
        crate::auth::AuthKind::Bootstrap => Ok(McpArtifactExportCallerBinding::Bootstrap),
        crate::auth::AuthKind::ApiToken => auth
            .api_key_id
            .as_ref()
            .filter(|value| !value.is_empty())
            .cloned()
            .map(|api_key_id| McpArtifactExportCallerBinding::ApiToken { api_key_id })
            .ok_or("API token identity is unavailable"),
        crate::auth::AuthKind::AgentToken => auth
            .api_key_id
            .as_ref()
            .filter(|value| !value.is_empty())
            .cloned()
            .map(|api_key_id| McpArtifactExportCallerBinding::AgentToken { api_key_id })
            .ok_or("agent token identity is unavailable"),
        crate::auth::AuthKind::AccountCredential => auth
            .user_id
            .as_ref()
            .filter(|value| !value.is_empty())
            .cloned()
            .map(|user_id| McpArtifactExportCallerBinding::AccountCredential { user_id })
            .ok_or("account identity is unavailable"),
        crate::auth::AuthKind::OAuth2Token => {
            let client_id = auth
                .allowed_client_id
                .as_ref()
                .filter(|value| !value.is_empty())
                .cloned()
                .ok_or("OAuth client identity is unavailable")?;
            if auth.is_oauth_shared_key_subject() {
                let shared_key_hash = auth
                    .shared_key_hash
                    .as_ref()
                    .filter(|value| !value.is_empty())
                    .cloned()
                    .ok_or("OAuth shared-key subject identity is unavailable")?;
                Ok(McpArtifactExportCallerBinding::OAuthSharedKey {
                    shared_key_hash,
                    client_id,
                })
            } else if auth.token_kind.as_deref() == Some("oauth2") {
                let user_id = auth
                    .user_id
                    .as_ref()
                    .filter(|value| !value.is_empty())
                    .cloned()
                    .ok_or("OAuth user identity is unavailable")?;
                Ok(McpArtifactExportCallerBinding::OAuthUser { user_id, client_id })
            } else {
                Err("unsupported OAuth subject identity")
            }
        }
        crate::auth::AuthKind::SharedKey => auth
            .shared_key_hash
            .as_ref()
            .filter(|value| !value.is_empty())
            .cloned()
            .map(|shared_key_hash| McpArtifactExportCallerBinding::SharedKey { shared_key_hash })
            .ok_or("shared-key identity is unavailable"),
        crate::auth::AuthKind::ProjectCredential => auth
            .project_grant_id
            .as_ref()
            .filter(|value| !value.is_empty())
            .cloned()
            .map(
                |project_grant_id| McpArtifactExportCallerBinding::ProjectCredential {
                    project_grant_id,
                },
            )
            .ok_or("project credential identity is unavailable"),
        crate::auth::AuthKind::OpenAnonymous => {
            Err("anonymous MCP callers cannot create artifact export resources")
        }
    }
}

#[derive(Debug, Clone)]
struct McpArtifactExportRecord {
    caller: McpArtifactExportCallerBinding,
    project: String,
    snapshot: ProjectArtifactExportSnapshot,
    expires_at: Instant,
}

#[derive(Default)]
struct McpArtifactExportRegistry {
    entries: HashMap<String, McpArtifactExportRecord>,
    order: VecDeque<String>,
}

impl McpArtifactExportRegistry {
    fn cleanup(&mut self, now: Instant) {
        self.entries.retain(|_, record| record.expires_at > now);
        self.order.retain(|id| self.entries.contains_key(id));
    }

    fn insert(&mut self, record: McpArtifactExportRecord) -> String {
        self.cleanup(Instant::now());
        while self
            .entries
            .values()
            .filter(|existing| existing.caller == record.caller)
            .count()
            >= MAX_MCP_ARTIFACT_EXPORTS_PER_CALLER
        {
            let Some(position) = self.order.iter().position(|id| {
                self.entries
                    .get(id)
                    .is_some_and(|existing| existing.caller == record.caller)
            }) else {
                break;
            };
            if let Some(id) = self.order.remove(position) {
                self.entries.remove(&id);
            }
        }
        while self.entries.len() >= MAX_MCP_ARTIFACT_EXPORTS {
            if let Some(id) = self.order.pop_front() {
                self.entries.remove(&id);
            } else {
                break;
            }
        }
        let id = loop {
            let candidate = format!(
                "{MCP_ARTIFACT_EXPORT_ID_PREFIX}{}",
                uuid::Uuid::new_v4().simple()
            );
            if !self.entries.contains_key(&candidate) {
                break candidate;
            }
        };
        self.order.push_back(id.clone());
        self.entries.insert(id.clone(), record);
        format!("{MCP_ARTIFACT_EXPORT_URI_PREFIX}{id}")
    }

    fn get_for_caller(
        &mut self,
        uri: &str,
        caller: &McpArtifactExportCallerBinding,
    ) -> Option<McpArtifactExportRecord> {
        self.cleanup(Instant::now());
        let id = mcp_artifact_export_id_from_uri(uri)?;
        self.entries
            .get(id)
            .filter(|record| &record.caller == caller)
            .cloned()
    }
}

static MCP_ARTIFACT_EXPORT_REGISTRY: OnceLock<Mutex<McpArtifactExportRegistry>> = OnceLock::new();
static MCP_ARTIFACT_EXPORT_READ_SEMAPHORE: OnceLock<Semaphore> = OnceLock::new();

fn mcp_artifact_export_registry() -> &'static Mutex<McpArtifactExportRegistry> {
    MCP_ARTIFACT_EXPORT_REGISTRY.get_or_init(|| Mutex::new(McpArtifactExportRegistry::default()))
}

fn mcp_artifact_export_read_semaphore() -> &'static Semaphore {
    MCP_ARTIFACT_EXPORT_READ_SEMAPHORE.get_or_init(|| Semaphore::new(MAX_MCP_ARTIFACT_EXPORT_READS))
}

fn mcp_artifact_export_id_from_uri(uri: &str) -> Option<&str> {
    let id = uri.strip_prefix(MCP_ARTIFACT_EXPORT_URI_PREFIX)?;
    let hex = id.strip_prefix(MCP_ARTIFACT_EXPORT_ID_PREFIX)?;
    (hex.len() == 32
        && hex
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()))
    .then_some(id)
}

fn mcp_issue_artifact_export(
    caller: McpArtifactExportCallerBinding,
    result: &ToolResult,
) -> Result<(String, ProjectArtifactExportSnapshot), String> {
    let project = result
        .output
        .get("project")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "export result is missing canonical project identity".to_string())?
        .to_string();
    let path = result
        .output
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| "export result is missing artifact path".to_string())?;
    let snapshot = validate_project_artifact_export_snapshot(path, &result.output)?;
    if result.output.get("name").and_then(Value::as_str) != Some(snapshot.name.as_str()) {
        return Err(
            "export result basename does not match validated artifact metadata".to_string(),
        );
    }
    let record = McpArtifactExportRecord {
        caller,
        project,
        snapshot: snapshot.clone(),
        expires_at: Instant::now() + MCP_ARTIFACT_EXPORT_TTL,
    };
    let uri = mcp_artifact_export_registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(record);
    Ok((uri, snapshot))
}

fn mcp_artifact_export_tool_result(
    result: ToolResult,
    caller: McpArtifactExportCallerBinding,
) -> Value {
    if !result.success {
        return mcp_runtime_tool_result("export_project_artifact", false, result);
    }
    let (uri, snapshot) = match mcp_issue_artifact_export(caller, &result) {
        Ok(value) => value,
        Err(error) => {
            return mcp_runtime_tool_result(
                "export_project_artifact",
                false,
                ToolResult::err(format!("cannot frame artifact export resource: {error}")),
            )
        }
    };
    json!({
        "content": [{
            "type": "resource_link",
            "uri": uri,
            "name": snapshot.name,
            "mimeType": snapshot.mime_type,
            "description": "Short-lived authenticated WebCodex project artifact export. Read this URI with MCP resources/read to retrieve the complete bounded binary."
        }],
        "structuredContent": {
            "success": true,
            "output": result.output,
            "error": Value::Null,
        },
        "isError": false
    })
}

#[cfg(test)]
fn mcp_expire_artifact_export_for_test(uri: &str) {
    if let Some(id) = mcp_artifact_export_id_from_uri(uri) {
        let mut registry = mcp_artifact_export_registry()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(record) = registry.entries.get_mut(id) {
            record.expires_at = Instant::now();
        }
    }
}

fn mcp_computer_app_probe_tool_result() -> Value {
    json!({
        "content": [{
            "type": "text",
            "text": "WebCodex Computer App probe OK"
        }],
        "structuredContent": {
            "success": true,
            "output": {
                "probe": "ok",
                "payload": "tiny",
                "runner_used": false
            },
            "error": Value::Null
        },
        "isError": false
    })
}

fn mcp_computer_app_image_probe_tool_result() -> Value {
    let mut result = ToolResult::ok(json!({
        "probe": "image",
        "runner_used": false,
        "client_id": "control-plane",
        "surface": {
            "surface_id": "image_probe",
            "application": "WebCodex",
            "title": "Built-in tiny PNG",
            "width": 1,
            "height": 1,
            "focused": false,
            "active": false
        },
        "width": 1,
        "height": 1,
        "mime_type": "image/png",
        "file_bytes": MCP_COMPUTER_APP_IMAGE_PROBE_PNG_BYTES,
        "sha256": MCP_COMPUTER_APP_IMAGE_PROBE_PNG_SHA256,
        "content_base64": MCP_COMPUTER_APP_IMAGE_PROBE_PNG_BASE64
    }));
    // Deliberately exercise the exact Computer snapshot image framing branch;
    // only the image bytes are synthetic and no Runner is contacted.
    match mcp_native_image_tool_result("computer_snapshot", &mut result) {
        Ok(value) => value,
        Err(error) => mcp_runtime_tool_result(
            MCP_COMPUTER_APP_IMAGE_PROBE_TOOL_NAME,
            false,
            ToolResult::err(format!("cannot frame built-in image probe: {error}")),
        ),
    }
}

fn mcp_computer_app_image_size_probe_target(size: &str) -> Option<usize> {
    MCP_COMPUTER_APP_IMAGE_SIZE_PROBE_CHOICES
        .iter()
        .find_map(|(label, bytes)| (*label == size).then_some(*bytes))
}

fn mcp_png_crc32(parts: &[&[u8]]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for part in parts {
        for &byte in *part {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                let mask = (crc & 1).wrapping_neg();
                crc = (crc >> 1) ^ (0xedb8_8320 & mask);
            }
        }
    }
    !crc
}

fn mcp_png_append_chunk(
    png: &mut Vec<u8>,
    chunk_type: &[u8; 4],
    data: &[u8],
) -> Result<(), String> {
    let length = u32::try_from(data.len())
        .map_err(|_| "PNG probe chunk exceeds the portable chunk length".to_string())?;
    png.extend_from_slice(&length.to_be_bytes());
    png.extend_from_slice(chunk_type);
    png.extend_from_slice(data);
    png.extend_from_slice(&mcp_png_crc32(&[chunk_type, data]).to_be_bytes());
    Ok(())
}

fn mcp_computer_app_image_size_probe_png(target_bytes: usize) -> Result<Vec<u8>, String> {
    const PNG_CHUNK_OVERHEAD: usize = 12;
    const PNG_IEND_CHUNK_BYTES: usize = 12;
    const PADDING_CHUNK_TYPE: &[u8; 4] = b"vpAg";

    if target_bytes > crate::artifact_policy::MAX_MCP_IMAGE_BYTES {
        return Err("requested image-size probe payload exceeds the MCP image bound".to_string());
    }
    let base = general_purpose::STANDARD
        .decode(MCP_COMPUTER_APP_IMAGE_PROBE_PNG_BASE64)
        .map_err(|error| format!("cannot decode built-in PNG: {error}"))?;
    let iend_offset = base
        .len()
        .checked_sub(PNG_IEND_CHUNK_BYTES)
        .ok_or_else(|| "built-in PNG is too short".to_string())?;
    if &base[iend_offset + 4..iend_offset + 8] != b"IEND" {
        return Err("built-in PNG does not end with IEND".to_string());
    }
    let padding_len = target_bytes
        .checked_sub(base.len() + PNG_CHUNK_OVERHEAD)
        .ok_or_else(|| "requested image-size probe payload is too small".to_string())?;
    let padding_len_u32 = u32::try_from(padding_len)
        .map_err(|_| "image-size probe padding is too large".to_string())?;

    let mut png = Vec::with_capacity(target_bytes);
    png.extend_from_slice(&base[..iend_offset]);
    png.extend_from_slice(&padding_len_u32.to_be_bytes());
    png.extend_from_slice(PADDING_CHUNK_TYPE);
    let padding_start = png.len();
    png.resize(padding_start + padding_len, 0);
    let crc = mcp_png_crc32(&[PADDING_CHUNK_TYPE, &png[padding_start..]]);
    png.extend_from_slice(&crc.to_be_bytes());
    png.extend_from_slice(&base[iend_offset..]);
    if png.len() != target_bytes {
        return Err("image-size probe payload length mismatch".to_string());
    }
    Ok(png)
}

fn mcp_computer_app_image_size_probe_tool_result(size: &str) -> Value {
    let Some(target_bytes) = mcp_computer_app_image_size_probe_target(size) else {
        return mcp_runtime_tool_result(
            MCP_COMPUTER_APP_IMAGE_SIZE_PROBE_TOOL_NAME,
            false,
            ToolResult::err("unsupported image-size probe choice"),
        );
    };
    let png = match mcp_computer_app_image_size_probe_png(target_bytes) {
        Ok(png) => png,
        Err(error) => {
            return mcp_runtime_tool_result(
                MCP_COMPUTER_APP_IMAGE_SIZE_PROBE_TOOL_NAME,
                false,
                ToolResult::err(format!("cannot build image-size probe PNG: {error}")),
            )
        }
    };
    let sha256 = format!("{:x}", Sha256::digest(&png));
    let content_base64 = general_purpose::STANDARD.encode(&png);
    let mut result = ToolResult::ok(json!({
        "probe": "image_size",
        "size": size,
        "runner_used": false,
        "client_id": "control-plane",
        "surface": {
            "surface_id": "image_size_probe",
            "application": "WebCodex",
            "title": "Synthetic PNG payload size probe",
            "width": 1,
            "height": 1,
            "focused": false,
            "active": false
        },
        "width": 1,
        "height": 1,
        "mime_type": "image/png",
        "file_bytes": png.len() as u64,
        "sha256": sha256,
        "content_base64": content_base64
    }));
    match mcp_native_image_tool_result("computer_snapshot", &mut result) {
        Ok(value) => value,
        Err(error) => mcp_runtime_tool_result(
            MCP_COMPUTER_APP_IMAGE_SIZE_PROBE_TOOL_NAME,
            false,
            ToolResult::err(format!("cannot frame image-size probe: {error}")),
        ),
    }
}

fn mcp_computer_app_image_dimension_probe_target(
    dimension: &str,
) -> Option<(u32, u32, &'static str)> {
    MCP_COMPUTER_APP_IMAGE_DIMENSION_PROBE_CHOICES
        .iter()
        .find_map(|(label, width, height, idat)| {
            (*label == dimension).then_some((*width, *height, *idat))
        })
}

fn mcp_computer_app_image_dimension_probe_png_with_bytes(
    dimension: &str,
    target_bytes: usize,
) -> Result<(Vec<u8>, u32, u32), String> {
    const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
    const PNG_CHUNK_OVERHEAD: usize = 12;
    const PNG_IEND_CHUNK_BYTES: usize = 12;
    const PADDING_CHUNK_TYPE: &[u8; 4] = b"vpAg";

    if target_bytes > crate::artifact_policy::MAX_MCP_IMAGE_BYTES {
        return Err("dimension probe payload exceeds the MCP image bound".to_string());
    }
    let Some((width, height, idat_base64)) =
        mcp_computer_app_image_dimension_probe_target(dimension)
    else {
        return Err("unsupported image-dimension probe choice".to_string());
    };
    let idat = general_purpose::STANDARD
        .decode(idat_base64)
        .map_err(|error| format!("cannot decode precomputed dimension-probe IDAT: {error}"))?;

    let mut ihdr = [0u8; 13];
    ihdr[..4].copy_from_slice(&width.to_be_bytes());
    ihdr[4..8].copy_from_slice(&height.to_be_bytes());
    ihdr[8] = 1;
    ihdr[9] = 0;
    ihdr[10] = 0;
    ihdr[11] = 0;
    ihdr[12] = 0;

    let mut png = Vec::with_capacity(target_bytes);
    png.extend_from_slice(PNG_SIGNATURE);
    mcp_png_append_chunk(&mut png, b"IHDR", &ihdr)?;
    mcp_png_append_chunk(&mut png, b"IDAT", &idat)?;

    let padding_len = target_bytes
        .checked_sub(png.len() + PNG_CHUNK_OVERHEAD + PNG_IEND_CHUNK_BYTES)
        .ok_or_else(|| "dimension probe PNG does not fit the fixed payload size".to_string())?;
    let padding_len_u32 = u32::try_from(padding_len)
        .map_err(|_| "dimension probe padding is too large".to_string())?;
    png.extend_from_slice(&padding_len_u32.to_be_bytes());
    png.extend_from_slice(PADDING_CHUNK_TYPE);
    let padding_start = png.len();
    png.resize(padding_start + padding_len, 0);
    let padding_crc = mcp_png_crc32(&[PADDING_CHUNK_TYPE, &png[padding_start..]]);
    png.extend_from_slice(&padding_crc.to_be_bytes());
    mcp_png_append_chunk(&mut png, b"IEND", &[])?;

    if png.len() != target_bytes {
        return Err("dimension probe payload length mismatch".to_string());
    }
    Ok((png, width, height))
}

fn mcp_computer_app_image_dimension_probe_png(
    dimension: &str,
) -> Result<(Vec<u8>, u32, u32), String> {
    mcp_computer_app_image_dimension_probe_png_with_bytes(
        dimension,
        MCP_COMPUTER_APP_IMAGE_DIMENSION_PROBE_BYTES,
    )
}

fn mcp_computer_app_image_dimension_probe_tool_result(dimension: &str) -> Value {
    let (png, width, height) = match mcp_computer_app_image_dimension_probe_png(dimension) {
        Ok(value) => value,
        Err(error) => {
            return mcp_runtime_tool_result(
                MCP_COMPUTER_APP_IMAGE_DIMENSION_PROBE_TOOL_NAME,
                false,
                ToolResult::err(format!("cannot build image-dimension probe PNG: {error}")),
            )
        }
    };
    let sha256 = format!("{:x}", Sha256::digest(&png));
    let content_base64 = general_purpose::STANDARD.encode(&png);
    let mut result = ToolResult::ok(json!({
        "probe": "image_dimension",
        "dimension": dimension,
        "runner_used": false,
        "client_id": "control-plane",
        "surface": {
            "surface_id": "image_dimension_probe",
            "application": "WebCodex",
            "title": "Synthetic PNG dimension probe",
            "width": width,
            "height": height,
            "focused": false,
            "active": false
        },
        "width": width,
        "height": height,
        "mime_type": "image/png",
        "file_bytes": png.len() as u64,
        "sha256": sha256,
        "content_base64": content_base64
    }));
    match mcp_native_image_tool_result("computer_snapshot", &mut result) {
        Ok(value) => value,
        Err(error) => mcp_runtime_tool_result(
            MCP_COMPUTER_APP_IMAGE_DIMENSION_PROBE_TOOL_NAME,
            false,
            ToolResult::err(format!("cannot frame image-dimension probe: {error}")),
        ),
    }
}

fn mcp_computer_app_image_matrix_probe_payload_bytes(payload: &str) -> Option<usize> {
    MCP_COMPUTER_APP_IMAGE_MATRIX_PROBE_PAYLOAD_CHOICES
        .iter()
        .find_map(|(label, bytes)| (*label == payload).then_some(*bytes))
}

fn mcp_computer_app_image_matrix_probe_jpeg_base(
    dimension: &str,
) -> Option<(u32, u32, &'static [u8])> {
    MCP_COMPUTER_APP_IMAGE_MATRIX_PROBE_JPEG_BASES
        .iter()
        .find_map(|(label, width, height, jpeg)| {
            (*label == dimension).then_some((*width, *height, *jpeg))
        })
}

fn mcp_computer_app_image_matrix_probe_jpeg(
    dimension: &str,
    target_bytes: usize,
) -> Result<(Vec<u8>, u32, u32), String> {
    const JPEG_EOI: &[u8; 2] = &[0xff, 0xd9];
    const APP15_MARKER: &[u8; 2] = &[0xff, 0xef];
    const MAX_APP15_DATA_BYTES: usize = u16::MAX as usize - 2;
    const MAX_APP15_SEGMENT_BYTES: usize = MAX_APP15_DATA_BYTES + 4;
    const MIN_APP15_SEGMENT_BYTES: usize = 4;

    if target_bytes > crate::artifact_policy::MAX_MCP_IMAGE_BYTES {
        return Err("matrix JPEG payload exceeds the MCP image bound".to_string());
    }
    let Some((width, height, base)) = mcp_computer_app_image_matrix_probe_jpeg_base(dimension)
    else {
        return Err("unsupported matrix JPEG dimension".to_string());
    };
    if !base.starts_with(&[0xff, 0xd8, 0xff]) || !base.ends_with(JPEG_EOI) {
        return Err("matrix JPEG baseline is malformed".to_string());
    }
    let padding_bytes = target_bytes
        .checked_sub(base.len())
        .ok_or_else(|| "matrix JPEG baseline exceeds the selected payload size".to_string())?;
    if padding_bytes == 0 {
        return Ok((base.to_vec(), width, height));
    }
    let segment_count = (padding_bytes + MAX_APP15_SEGMENT_BYTES - 1) / MAX_APP15_SEGMENT_BYTES;
    if padding_bytes < segment_count * MIN_APP15_SEGMENT_BYTES {
        return Err("matrix JPEG padding cannot be represented by APP15 segments".to_string());
    }
    let segment_base_bytes = padding_bytes / segment_count;
    let segment_extra = padding_bytes % segment_count;

    let mut jpeg = Vec::with_capacity(target_bytes);
    jpeg.extend_from_slice(&base[..base.len() - JPEG_EOI.len()]);
    for index in 0..segment_count {
        let segment_bytes = segment_base_bytes + usize::from(index < segment_extra);
        let data_bytes = segment_bytes
            .checked_sub(MIN_APP15_SEGMENT_BYTES)
            .ok_or_else(|| "matrix JPEG APP15 segment is too small".to_string())?;
        if data_bytes > MAX_APP15_DATA_BYTES {
            return Err("matrix JPEG APP15 segment exceeds the JPEG length bound".to_string());
        }
        let length_field = u16::try_from(data_bytes + 2)
            .map_err(|_| "matrix JPEG APP15 length does not fit u16".to_string())?;
        jpeg.extend_from_slice(APP15_MARKER);
        jpeg.extend_from_slice(&length_field.to_be_bytes());
        let data_start = jpeg.len();
        jpeg.resize(data_start + data_bytes, 0);
    }
    jpeg.extend_from_slice(JPEG_EOI);
    if jpeg.len() != target_bytes {
        return Err("matrix JPEG payload length mismatch".to_string());
    }
    Ok((jpeg, width, height))
}

fn mcp_computer_app_image_matrix_probe_image(
    codec: &str,
    dimension: &str,
    payload: &str,
) -> Result<(Vec<u8>, u32, u32, &'static str), String> {
    let target_bytes = mcp_computer_app_image_matrix_probe_payload_bytes(payload)
        .ok_or_else(|| "unsupported image-matrix payload choice".to_string())?;
    match codec {
        "png" => {
            let (image, width, height) =
                mcp_computer_app_image_dimension_probe_png_with_bytes(dimension, target_bytes)?;
            Ok((image, width, height, "image/png"))
        }
        "jpeg" => {
            let (image, width, height) =
                mcp_computer_app_image_matrix_probe_jpeg(dimension, target_bytes)?;
            Ok((image, width, height, "image/jpeg"))
        }
        _ => Err("unsupported image-matrix codec".to_string()),
    }
}

fn mcp_computer_app_image_matrix_probe_tool_result(
    codec: &str,
    dimension: &str,
    payload: &str,
) -> Value {
    let (image, width, height, mime_type) =
        match mcp_computer_app_image_matrix_probe_image(codec, dimension, payload) {
            Ok(value) => value,
            Err(error) => {
                return mcp_runtime_tool_result(
                    MCP_COMPUTER_APP_IMAGE_MATRIX_PROBE_TOOL_NAME,
                    false,
                    ToolResult::err(format!("cannot build image-matrix probe: {error}")),
                )
            }
        };
    let sha256 = format!("{:x}", Sha256::digest(&image));
    let content_base64 = general_purpose::STANDARD.encode(&image);
    let mut result = ToolResult::ok(json!({
        "probe": "image_matrix",
        "codec": codec,
        "dimension": dimension,
        "payload": payload,
        "runner_used": false,
        "client_id": "control-plane",
        "surface": {
            "surface_id": "image_matrix_probe",
            "application": "WebCodex",
            "title": "Synthetic native-image matrix probe",
            "width": width,
            "height": height,
            "focused": false,
            "active": false
        },
        "width": width,
        "height": height,
        "mime_type": mime_type,
        "file_bytes": image.len() as u64,
        "sha256": sha256,
        "content_base64": content_base64
    }));
    match mcp_native_image_tool_result("computer_snapshot", &mut result) {
        Ok(value) => value,
        Err(error) => mcp_runtime_tool_result(
            MCP_COMPUTER_APP_IMAGE_MATRIX_PROBE_TOOL_NAME,
            false,
            ToolResult::err(format!("cannot frame image-matrix probe: {error}")),
        ),
    }
}

pub(crate) fn mcp_runtime_tool_result(
    tool_name: &str,
    as_image_requested: bool,
    mut result: ToolResult,
) -> Value {
    let native_image_requested = (tool_name == "read_project_artifact" && as_image_requested)
        || tool_name == "computer_snapshot";
    if native_image_requested && result.success {
        match mcp_native_image_tool_result(tool_name, &mut result) {
            Ok(value) => return value,
            Err(error) => {
                result = ToolResult::err(format!(
                    "cannot frame {tool_name} as MCP image content: {error}"
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

fn mcp_native_image_tool_result(tool_name: &str, result: &mut ToolResult) -> Result<Value, String> {
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
    let decoded = general_purpose::STANDARD
        .decode(&data)
        .map_err(|error| format!("invalid image base64: {error}"))?;
    if decoded.is_empty() || decoded.len() > crate::artifact_policy::MAX_MCP_IMAGE_BYTES {
        return Err(format!(
            "image payload exceeds {} decoded bytes",
            crate::artifact_policy::MAX_MCP_IMAGE_BYTES
        ));
    }
    let detected = if decoded.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if decoded.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if decoded.len() >= 12 && decoded.starts_with(b"RIFF") && &decoded[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    };
    if detected != Some(mime_type.as_str()) {
        return Err("image MIME does not match decoded content".to_string());
    }
    let image_label = if tool_name == "computer_snapshot" {
        result
            .output
            .pointer("/surface/surface_id")
            .and_then(Value::as_str)
            .unwrap_or("desktop surface")
    } else {
        result
            .output
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or("project image")
    };
    let file_bytes = result
        .output
        .get("file_bytes")
        .and_then(Value::as_u64)
        .ok_or_else(|| "missing file_bytes".to_string())?;
    if file_bytes != decoded.len() as u64 {
        return Err("file_bytes does not match decoded image payload".to_string());
    }
    let sha256 = result
        .output
        .get("sha256")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let metadata_text = if tool_name == "computer_snapshot" {
        let width = result
            .output
            .get("width")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let height = result
            .output
            .get("height")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if width == 0 || height == 0 || width > 4096 || height > 4096 {
            return Err("computer snapshot dimensions are invalid".to_string());
        }
        format!("Image {image_label}: {mime_type}, {width}x{height}, {file_bytes} bytes.")
    } else {
        format!("Image {image_label}: {mime_type}, {file_bytes} bytes, sha256 {sha256}.")
    };

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

#[derive(Debug)]
enum McpArtifactExportReadError {
    Unavailable,
    Forbidden {
        required_scope: Option<&'static str>,
        description: String,
    },
    SnapshotChanged,
    Unsafe,
    Busy,
    Timeout,
}

fn mcp_artifact_export_lookup(
    uri: &str,
    auth: Option<&AuthContext>,
) -> Result<McpArtifactExportRecord, McpArtifactExportReadError> {
    let caller = mcp_artifact_export_caller_binding(auth)
        .map_err(|_| McpArtifactExportReadError::Unavailable)?;
    mcp_artifact_export_registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get_for_caller(uri, &caller)
        .ok_or(McpArtifactExportReadError::Unavailable)
}

async fn mcp_artifact_export_metadata_recheck(
    runtime: &ToolRuntime,
    record: &McpArtifactExportRecord,
    auth: Option<&AuthContext>,
) -> Result<ProjectArtifactExportSnapshot, McpArtifactExportReadError> {
    let outcome = runtime
        .call_tool_with_context(
            KernelToolCallRequest {
                tool_name: "read_project_artifact_metadata".to_string(),
                arguments: json!({
                    "project": record.project,
                    "path": record.snapshot.path,
                    "allow_missing": false,
                }),
            },
            ToolCallContext {
                transport: ToolTransport::Mcp,
                session_id: None,
                auth,
                window: None,
                record_oauth_scope_denials: false,
                host_file_import_trust: HostFileImportTrust::Untrusted,
            },
        )
        .await;
    if let Some(error_status) = outcome.error_status {
        return match error_status {
            ToolCallErrorStatus::InsufficientScope {
                required_scope,
                description,
            } => Err(McpArtifactExportReadError::Forbidden {
                required_scope,
                description,
            }),
            ToolCallErrorStatus::InvalidArguments { .. } => Err(McpArtifactExportReadError::Unsafe),
        };
    }
    let result = outcome.result.ok_or(McpArtifactExportReadError::Unsafe)?;
    if !result.success {
        return Err(McpArtifactExportReadError::Unavailable);
    }
    let snapshot = validate_project_artifact_export_snapshot(&record.snapshot.path, &result.output)
        .map_err(|_| McpArtifactExportReadError::Unsafe)?;
    if snapshot != record.snapshot {
        return Err(McpArtifactExportReadError::SnapshotChanged);
    }
    Ok(snapshot)
}

fn mcp_artifact_export_decode_chunk(
    record: &McpArtifactExportRecord,
    offset: usize,
    length: usize,
    output: &Value,
    require_complete_metadata: bool,
) -> Result<Vec<u8>, McpArtifactExportReadError> {
    if output.get("error_kind").and_then(Value::as_str) == Some("snapshot_changed") {
        return Err(McpArtifactExportReadError::SnapshotChanged);
    }
    if output.get("error").and_then(Value::as_str).is_some() {
        return Err(McpArtifactExportReadError::Unsafe);
    }
    if output.get("path").and_then(Value::as_str) != Some(record.snapshot.path.as_str())
        || output.get("file_bytes").and_then(Value::as_u64) != Some(record.snapshot.bytes as u64)
    {
        return Err(McpArtifactExportReadError::SnapshotChanged);
    }
    if require_complete_metadata
        && (output.get("mime_type").and_then(Value::as_str)
            != Some(record.snapshot.mime_type.as_str())
            || output.get("sha256").and_then(Value::as_str)
                != Some(record.snapshot.sha256.as_str()))
    {
        return Err(McpArtifactExportReadError::SnapshotChanged);
    }
    if output.get("offset").and_then(Value::as_u64) != Some(offset as u64) {
        return Err(McpArtifactExportReadError::Unsafe);
    }
    let encoded = output
        .get("content_base64")
        .and_then(Value::as_str)
        .ok_or(McpArtifactExportReadError::Unsafe)?;
    let decoded = general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| McpArtifactExportReadError::Unsafe)?;
    let bytes_returned = output
        .get("bytes_returned")
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .ok_or(McpArtifactExportReadError::Unsafe)?;
    let next_offset = output
        .get("next_offset")
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .ok_or(McpArtifactExportReadError::Unsafe)?;
    let eof = output
        .get("eof")
        .and_then(Value::as_bool)
        .ok_or(McpArtifactExportReadError::Unsafe)?;
    let truncated = output
        .get("truncated")
        .and_then(Value::as_bool)
        .ok_or(McpArtifactExportReadError::Unsafe)?;
    let expected_next = offset
        .checked_add(decoded.len())
        .ok_or(McpArtifactExportReadError::Unsafe)?;
    if decoded.len() != bytes_returned
        || decoded.len() > length
        || expected_next != next_offset
        || next_offset > record.snapshot.bytes
        || (decoded.is_empty() && offset < record.snapshot.bytes)
        || eof != (next_offset == record.snapshot.bytes)
        || truncated == eof
    {
        return Err(McpArtifactExportReadError::Unsafe);
    }
    Ok(decoded)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum McpArtifactExportChunkRoute {
    Optimized,
    Legacy,
}

async fn mcp_artifact_export_read_optimized_chunk(
    runtime: &ToolRuntime,
    record: &McpArtifactExportRecord,
    auth: Option<&AuthContext>,
    offset: usize,
    length: usize,
) -> Result<Option<Vec<u8>>, McpArtifactExportReadError> {
    match runtime
        .read_project_artifact_export_chunk_internal(
            &record.project,
            &record.snapshot.path,
            record.snapshot.bytes,
            offset,
            length,
            auth,
        )
        .await
    {
        Ok(Some(output)) => {
            mcp_artifact_export_decode_chunk(record, offset, length, &output, false).map(Some)
        }
        Ok(None) => Ok(None),
        Err(_) => Err(McpArtifactExportReadError::Unavailable),
    }
}

async fn mcp_artifact_export_read_legacy_chunk(
    runtime: &ToolRuntime,
    record: &McpArtifactExportRecord,
    auth: Option<&AuthContext>,
    offset: usize,
    length: usize,
) -> Result<Vec<u8>, McpArtifactExportReadError> {
    let outcome = runtime
        .call_tool_with_context(
            KernelToolCallRequest {
                tool_name: "read_project_artifact".to_string(),
                arguments: json!({
                    "project": record.project,
                    "path": record.snapshot.path,
                    "encoding": "base64",
                    "offset": offset,
                    "length": length,
                    "max_bytes": length,
                }),
            },
            ToolCallContext {
                transport: ToolTransport::Mcp,
                session_id: None,
                auth,
                window: None,
                record_oauth_scope_denials: false,
                host_file_import_trust: HostFileImportTrust::Untrusted,
            },
        )
        .await;
    if let Some(error_status) = outcome.error_status {
        return match error_status {
            ToolCallErrorStatus::InsufficientScope {
                required_scope,
                description,
            } => Err(McpArtifactExportReadError::Forbidden {
                required_scope,
                description,
            }),
            ToolCallErrorStatus::InvalidArguments { .. } => Err(McpArtifactExportReadError::Unsafe),
        };
    }
    let result = outcome.result.ok_or(McpArtifactExportReadError::Unsafe)?;
    if !result.success {
        return Err(McpArtifactExportReadError::Unavailable);
    }
    mcp_artifact_export_decode_chunk(record, offset, length, &result.output, true)
}

async fn mcp_artifact_export_read_chunk(
    runtime: &ToolRuntime,
    record: &McpArtifactExportRecord,
    auth: Option<&AuthContext>,
    offset: usize,
    length: usize,
) -> Result<(Vec<u8>, McpArtifactExportChunkRoute), McpArtifactExportReadError> {
    if let Some(chunk) =
        mcp_artifact_export_read_optimized_chunk(runtime, record, auth, offset, length).await?
    {
        return Ok((chunk, McpArtifactExportChunkRoute::Optimized));
    }

    // Rolling-upgrade compatibility: an old Runner cannot receive the optimized
    // request kind because capability check + enqueue are atomic. Observe that
    // route once on the first chunk; the resource read then stays sequential on
    // this public compatibility path rather than amplifying legacy whole-file
    // work with Control-side concurrency.
    let chunk =
        mcp_artifact_export_read_legacy_chunk(runtime, record, auth, offset, length).await?;
    Ok((chunk, McpArtifactExportChunkRoute::Legacy))
}

async fn mcp_artifact_export_resource_read_inner(
    runtime: &ToolRuntime,
    uri: &str,
    auth: Option<&AuthContext>,
    record: McpArtifactExportRecord,
) -> Result<Value, McpArtifactExportReadError> {
    let snapshot = mcp_artifact_export_metadata_recheck(runtime, &record, auth).await?;
    let mut bytes = Vec::with_capacity(snapshot.bytes);
    let max_chunks = MAX_PROJECT_ARTIFACT_BYTES
        .div_ceil(MAX_READ_PROJECT_ARTIFACT_LENGTH)
        .saturating_add(1);
    let mut offset = 0usize;
    let mut chunks = 0usize;
    let mut route = None;

    if offset < snapshot.bytes {
        chunks = 1;
        let length = snapshot.bytes.min(MAX_READ_PROJECT_ARTIFACT_LENGTH);
        let (chunk, first_route) =
            mcp_artifact_export_read_chunk(runtime, &record, auth, offset, length).await?;
        offset = offset
            .checked_add(chunk.len())
            .ok_or(McpArtifactExportReadError::Unsafe)?;
        bytes.extend_from_slice(&chunk);
        if bytes.len() > snapshot.bytes {
            return Err(McpArtifactExportReadError::Unsafe);
        }
        route = Some(first_route);
    }

    match route {
        Some(McpArtifactExportChunkRoute::Legacy) => {
            while offset < snapshot.bytes {
                if chunks >= max_chunks {
                    return Err(McpArtifactExportReadError::Unsafe);
                }
                chunks = chunks.saturating_add(1);
                let length = (snapshot.bytes - offset).min(MAX_READ_PROJECT_ARTIFACT_LENGTH);
                let chunk =
                    mcp_artifact_export_read_legacy_chunk(runtime, &record, auth, offset, length)
                        .await?;
                offset = offset
                    .checked_add(chunk.len())
                    .ok_or(McpArtifactExportReadError::Unsafe)?;
                bytes.extend_from_slice(&chunk);
                if bytes.len() > snapshot.bytes {
                    return Err(McpArtifactExportReadError::Unsafe);
                }
            }
        }
        Some(McpArtifactExportChunkRoute::Optimized) => {
            while offset < snapshot.bytes {
                let mut batch = Vec::with_capacity(MAX_MCP_ARTIFACT_EXPORT_CHUNK_READS);
                let mut batch_offset = offset;
                while batch.len() < MAX_MCP_ARTIFACT_EXPORT_CHUNK_READS
                    && batch_offset < snapshot.bytes
                {
                    if chunks >= max_chunks {
                        return Err(McpArtifactExportReadError::Unsafe);
                    }
                    chunks = chunks.saturating_add(1);
                    let length =
                        (snapshot.bytes - batch_offset).min(MAX_READ_PROJECT_ARTIFACT_LENGTH);
                    batch.push((batch_offset, length));
                    batch_offset = batch_offset
                        .checked_add(length)
                        .ok_or(McpArtifactExportReadError::Unsafe)?;
                }

                let runtime = runtime;
                let record = &record;
                let results = join_all(batch.iter().map(|&(batch_offset, length)| async move {
                    mcp_artifact_export_read_optimized_chunk(
                        runtime,
                        record,
                        auth,
                        batch_offset,
                        length,
                    )
                    .await
                }))
                .await;

                // `join_all` drains every already-dispatched request in this
                // bounded batch and preserves input order. Only after the full
                // batch is drained do we surface the first deterministic error
                // in requested-offset order or append successful bytes.
                for ((requested_offset, _), result) in batch.into_iter().zip(results) {
                    if requested_offset != offset {
                        return Err(McpArtifactExportReadError::Unsafe);
                    }
                    let chunk = result?.ok_or(McpArtifactExportReadError::Unavailable)?;
                    offset = offset
                        .checked_add(chunk.len())
                        .ok_or(McpArtifactExportReadError::Unsafe)?;
                    bytes.extend_from_slice(&chunk);
                    if bytes.len() > snapshot.bytes {
                        return Err(McpArtifactExportReadError::Unsafe);
                    }
                }
            }
        }
        None => {}
    }
    if bytes.len() != snapshot.bytes {
        return Err(McpArtifactExportReadError::Unsafe);
    }
    let sha256 = format!("{:x}", Sha256::digest(&bytes));
    if sha256 != snapshot.sha256 {
        return Err(McpArtifactExportReadError::SnapshotChanged);
    }
    Ok(json!({
        "contents": [{
            "uri": uri,
            "mimeType": snapshot.mime_type,
            "blob": general_purpose::STANDARD.encode(bytes),
        }]
    }))
}

async fn mcp_artifact_export_resource_read_with_gate_timeout(
    runtime: &ToolRuntime,
    uri: &str,
    auth: Option<&AuthContext>,
    gate: &Semaphore,
    admission_timeout: Duration,
    read_timeout: Duration,
) -> Result<Value, McpArtifactExportReadError> {
    let record = mcp_artifact_export_lookup(uri, auth)?;
    if auth.is_some_and(|auth| !auth.has_scope(crate::auth::SCOPE_PROJECT_READ)) {
        return Err(McpArtifactExportReadError::Forbidden {
            required_scope: Some(crate::auth::SCOPE_PROJECT_READ),
            description: format!(
                "missing required scope: {}",
                crate::auth::SCOPE_PROJECT_READ
            ),
        });
    }
    let _permit = match tokio::time::timeout(admission_timeout, gate.acquire()).await {
        Ok(Ok(permit)) => permit,
        Ok(Err(_)) | Err(_) => return Err(McpArtifactExportReadError::Busy),
    };
    let outcome = {
        let read = mcp_artifact_export_resource_read_inner(runtime, uri, auth, record);
        tokio::pin!(read);
        tokio::time::timeout(read_timeout, &mut read).await
    };
    match outcome {
        Ok(result) => result,
        Err(_) => {
            // `outcome` is observed only after the aggregate read future above
            // has been dropped. Any Runner-backed synchronous request still in
            // the registry therefore has a closed oneshot receiver and can be
            // removed without guessing whether a live caller still needs it.
            runtime.shell_clients.cancel_abandoned_sync_requests().await;
            Err(McpArtifactExportReadError::Timeout)
        }
    }
}

async fn mcp_artifact_export_resource_read_with_gate(
    runtime: &ToolRuntime,
    uri: &str,
    auth: Option<&AuthContext>,
    gate: &Semaphore,
    admission_timeout: Duration,
) -> Result<Value, McpArtifactExportReadError> {
    mcp_artifact_export_resource_read_with_gate_timeout(
        runtime,
        uri,
        auth,
        gate,
        admission_timeout,
        MCP_ARTIFACT_EXPORT_READ_TIMEOUT,
    )
    .await
}

async fn mcp_artifact_export_resource_read(
    runtime: &ToolRuntime,
    uri: &str,
    auth: Option<&AuthContext>,
) -> Result<Value, McpArtifactExportReadError> {
    mcp_artifact_export_resource_read_with_gate(
        runtime,
        uri,
        auth,
        mcp_artifact_export_read_semaphore(),
        MCP_ARTIFACT_EXPORT_ADMISSION_TIMEOUT,
    )
    .await
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
    /// A modern MCP method is not implemented. HTTP 404 with JSON-RPC -32601.
    NotFound(Value),
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

fn mcp_protocol_era_label(protocol_era: McpProtocolEra) -> &'static str {
    match protocol_era {
        McpProtocolEra::Legacy => "legacy",
        McpProtocolEra::Stateless2026 => "stateless_2026",
    }
}

fn log_mcp_computer_app_resource_delivery(
    uri: &str,
    protocol_era: &str,
    ui_capability_present: bool,
    http_status: u16,
    mcp_error_code: Option<i64>,
) {
    tracing::info!(
        target: "webcodex::mcp",
        uri,
        protocol_era,
        ui_capability_present,
        http_status,
        mcp_error_code = mcp_error_code.unwrap_or(-1),
        "mcp_computer_app_resource_delivery"
    );
}

fn log_mcp_computer_app_resource_outcome(
    uri: &str,
    protocol_era: McpProtocolEra,
    ui_capability_present: bool,
    outcome: &McpOutcome,
) {
    let (http_status, mcp_error_code) = match outcome {
        McpOutcome::Ok(_) => (200, None),
        McpOutcome::BadRequest(body) => (400, body["error"]["code"].as_i64()),
        McpOutcome::NotFound(body) => (404, body["error"]["code"].as_i64()),
        McpOutcome::Notification => (202, None),
        McpOutcome::Forbidden { .. } => (403, None),
    };
    log_mcp_computer_app_resource_delivery(
        uri,
        mcp_protocol_era_label(protocol_era),
        ui_capability_present,
        http_status,
        mcp_error_code,
    );
}

#[handler]
pub async fn mcp_info(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    if let Err((status, _, message)) = crate::auth::require_same_origin(req) {
        let status = StatusCode::from_u16(status).unwrap_or(StatusCode::FORBIDDEN);
        res.status_code(status);
        res.render(json_error(status, message));
        return;
    }
    if request_header(req, MCP_PROTOCOL_VERSION_HEADER) == Some(MCP_STATELESS_PROTOCOL_VERSION) {
        res.status_code(StatusCode::METHOD_NOT_ALLOWED);
        return;
    }
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

    if let Err((status, _, message)) = crate::auth::require_json_same_origin(req) {
        let status = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_REQUEST);
        guard.parsed("http_validation_error");
        guard.response_serialized(
            status.as_u16(),
            None,
            Some(false),
            None,
            "http_validation_error",
        );
        res.status_code(status);
        res.render(json_error(status, message));
        guard.handler_returned(
            status.as_u16(),
            None,
            Some(false),
            None,
            "http_validation_error",
        );
        return;
    }

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
    let computer_app_resource_uri = if request.method == "resources/read" {
        request
            .params
            .get("uri")
            .and_then(Value::as_str)
            .filter(|uri| is_mcp_computer_app_resource_uri(uri))
            .map(str::to_string)
    } else {
        None
    };
    let computer_app_ui_capability_present = request_supports_mcp_apps(&request.params);
    let protocol_era = match validate_http_protocol(req, &request) {
        Ok(protocol_era) => protocol_era,
        Err(body) => {
            guard.parsed("protocol_error");
            if let Some(uri) = computer_app_resource_uri.as_deref() {
                log_mcp_computer_app_resource_delivery(
                    uri,
                    "validation_failed",
                    computer_app_ui_capability_present,
                    400,
                    body["error"]["code"].as_i64(),
                );
            }
            let estimated = estimate_json_bytes(&body);
            guard.response_serialized(400, estimated, Some(false), None, "protocol_error");
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(body));
            guard.handler_returned(400, estimated, Some(false), None, "protocol_error");
            return;
        }
    };
    guard.parsed("ok");
    let window = match protocol_era {
        McpProtocolEra::Legacy => {
            crate::client_window::mcp_window(req, request.method == "initialize")
        }
        McpProtocolEra::Stateless2026 => crate::client_window::McpWindow {
            identity: None,
            issued_session_id: None,
        },
    };

    // Chat-window MCP tool calls must land in the action audit exactly like
    // the REST surface (they were previously invisible there). Summary-level
    // only: tool name and project — never arguments or outputs. JSON-RPC
    // notifications are acknowledged but never dispatched, so they must not be
    // represented as executed actions.
    let audit = if request.method == "tools/call" && request.id.is_some() {
        Some((
            ActionAudit::start(req, depot, "/mcp", "toolsCall"),
            tool_name.clone().unwrap_or_else(|| "unknown".to_string()),
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
    let host_file_import_trust =
        if tool_name.as_deref() == Some("import_conversation_files_to_project") {
            let decision = mcp_host_file_import_trust_decision(depot, auth.as_ref());
            log_mcp_host_file_import_trust_decision(auth.as_ref(), &decision);
            decision.trust
        } else {
            HostFileImportTrust::Untrusted
        };
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
            protocol_era,
            host_file_import_trust,
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
            if let Some(uri) = computer_app_resource_uri.as_deref() {
                log_mcp_computer_app_resource_delivery(
                    uri,
                    mcp_protocol_era_label(protocol_era),
                    computer_app_ui_capability_present,
                    500,
                    Some(-32000),
                );
            }
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

    if let Some(uri) = computer_app_resource_uri.as_deref() {
        log_mcp_computer_app_resource_outcome(
            uri,
            protocol_era,
            computer_app_ui_capability_present,
            &outcome,
        );
    }

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
        McpOutcome::NotFound(body) => {
            record_audit(
                false,
                StatusCode::NOT_FOUND,
                body["error"]["message"].as_str().map(str::to_string),
            );
            let estimated = estimate_json_bytes(&body);
            guard.response_serialized(404, estimated, Some(false), None, "not_found");
            res.status_code(StatusCode::NOT_FOUND);
            res.render(Json(body));
            guard.handler_returned(404, estimated, Some(false), None, "not_found");
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
            if auth.as_ref().is_some_and(AuthContext::is_oauth_token) {
                let challenge = crate::auth::oauth_insufficient_scope_challenge(required_scope);
                if let Ok(val) = salvo::http::HeaderValue::from_str(&challenge) {
                    res.headers_mut().insert("www-authenticate", val);
                }
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
#[cfg(test)]
async fn handle_mcp_request(
    runtime: &ToolRuntime,
    request: JsonRpcRequest,
    auth: Option<&AuthContext>,
) -> McpOutcome {
    let protocol_era = inferred_protocol_era(&request);
    handle_mcp_request_with_lifecycle(
        runtime,
        None,
        request,
        auth,
        protocol_era,
        HostFileImportTrust::Untrusted,
        None,
        None,
    )
    .await
}

async fn handle_mcp_request_with_lifecycle(
    runtime: &ToolRuntime,
    connector: Option<&ConnectorRuntime>,
    request: JsonRpcRequest,
    auth: Option<&AuthContext>,
    protocol_era: McpProtocolEra,
    host_file_import_trust: HostFileImportTrust,
    window: Option<&crate::client_window::ClientWindow>,
    mut lifecycle: Option<&mut ToolRequestLifecycle>,
) -> McpOutcome {
    let stateless_2026 = protocol_era == McpProtocolEra::Stateless2026;
    let artifact_export_resource_read = stateless_2026
        && request.method == "resources/read"
        && request
            .params
            .get("uri")
            .and_then(Value::as_str)
            .is_some_and(|uri| uri.starts_with(MCP_ARTIFACT_EXPORT_URI_PREFIX));
    let mcp_app_enabled = stateless_2026
        && model_surface_supports_computer_app(runtime.model_surface())
        && request_supports_mcp_apps(&request.params);

    if auth.is_some()
        && (matches!(
            request.method.as_str(),
            "server/discover" | "tools/list" | "resources/list"
        ) || (request.method == "resources/read" && !artifact_export_resource_read)
            || (!stateless_2026
                && matches!(
                    request.method.as_str(),
                    "initialize" | "ping" | "notifications/initialized"
                )))
    {
        if let Some(outcome) = require_mcp_scope(auth, crate::auth::SCOPE_RUNTIME_READ) {
            return outcome;
        }
    }

    if auth.is_some_and(|auth| !auth.is_bootstrap())
        && !stateless_2026
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
        return scope_forbidden(
            auth,
            None,
            "authenticated caller cannot call unknown MCP methods",
        );
    }

    // A JSON-RPC request without an `id` member is a notification. Per the
    // JSON-RPC 2.0 and MCP specs the server MUST NOT reply with a JSON-RPC
    // response body, even if the method is unknown or malformed. We accept
    // the notification silently. `notifications/initialized` is the common
    // case sent by MCP clients after `initialize` completes.
    if request.id.is_none() {
        return McpOutcome::Notification;
    }

    let jsonrpc_valid = if stateless_2026 {
        request.jsonrpc.as_deref() == Some("2.0")
    } else {
        request.jsonrpc.as_deref().unwrap_or("2.0") == "2.0"
    };
    if !jsonrpc_valid {
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
        "server/discover" if stateless_2026 => rpc_result(
            id,
            json!({
                "resultType": "complete",
                "ttlMs": 0,
                "cacheScope": "private",
                "supportedVersions": [MCP_STATELESS_PROTOCOL_VERSION, MCP_PROTOCOL_VERSION],
                "capabilities": if model_surface_supports_computer_app(runtime.model_surface()) {
                    json!({
                        "tools": { "listChanged": false },
                        "resources": { "listChanged": false, "subscribe": false },
                        "extensions": {
                            MCP_UI_EXTENSION: {
                                "mimeTypes": [MCP_UI_RESOURCE_MIME_TYPE]
                            }
                        }
                    })
                } else {
                    json!({ "tools": { "listChanged": false } })
                },
                "_meta": {
                    "io.modelcontextprotocol/serverInfo": {
                        "name": "webcodex",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                }
            }),
        ),
        "initialize" if !stateless_2026 => rpc_result(
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
        "ping" if !stateless_2026 => rpc_result(id, json!({})),
        "tools/list" => {
            let result = if stateless_2026 {
                mcp_tools_list_payload_with_compact_and_app(
                    runtime.model_surface(),
                    crate::config::mcp_compact_schemas_enabled(),
                    model_surface_supports_computer_app(runtime.model_surface()),
                )
            } else {
                mcp_tools_list_payload(runtime.model_surface())
            };
            rpc_result(
                id,
                if stateless_2026 {
                    mcp_stateless_result(result, true)
                } else {
                    result
                },
            )
        }
        "resources/list" if stateless_2026 => {
            let result = if mcp_app_enabled {
                mcp_computer_app_resources_list()
            } else {
                json!({ "resources": [] })
            };
            rpc_result(id, mcp_stateless_result(result, true))
        }
        "resources/read" if stateless_2026 => {
            let Some(uri) = request.params.get("uri").and_then(Value::as_str) else {
                return McpOutcome::BadRequest(rpc_error(
                    id,
                    -32602,
                    "Invalid params: uri is required",
                ));
            };
            if uri.starts_with(MCP_ARTIFACT_EXPORT_URI_PREFIX) {
                let result = match mcp_artifact_export_resource_read(runtime, uri, auth).await {
                    Ok(result) => result,
                    Err(McpArtifactExportReadError::Forbidden {
                        required_scope,
                        description,
                    }) => return scope_forbidden(auth, required_scope, description),
                    Err(McpArtifactExportReadError::Unavailable) => {
                        return McpOutcome::BadRequest(rpc_error(
                            id,
                            -32602,
                            "Artifact export resource is unavailable",
                        ))
                    }
                    Err(McpArtifactExportReadError::SnapshotChanged) => {
                        return McpOutcome::BadRequest(rpc_error(
                            id,
                            -32602,
                            "Exported artifact no longer matches its snapshot",
                        ))
                    }
                    Err(McpArtifactExportReadError::Unsafe) => {
                        return McpOutcome::BadRequest(rpc_error(
                            id,
                            -32603,
                            "Artifact export resource failed bounded safety validation",
                        ))
                    }
                    Err(McpArtifactExportReadError::Busy) => {
                        return McpOutcome::BadRequest(rpc_error(
                            id,
                            MCP_ARTIFACT_EXPORT_BUSY_CODE,
                            "Artifact export is temporarily busy; retry later",
                        ))
                    }
                    Err(McpArtifactExportReadError::Timeout) => {
                        return McpOutcome::BadRequest(rpc_error(
                            id,
                            -32603,
                            "Artifact export resource read timed out",
                        ))
                    }
                };
                return McpOutcome::Ok(rpc_result(id, mcp_stateless_result(result, false)));
            }
            // Tool descriptors on the full-operator surface advertise the App
            // resource independently of whether a later resource fetch repeats
            // the UI client-capability metadata. Keep resources/list negotiated,
            // but allow a client to dereference an already-advertised resource.
            if !model_surface_supports_computer_app(runtime.model_surface()) {
                return McpOutcome::BadRequest(rpc_error(
                    id,
                    -32602,
                    "MCP App resource is unavailable on this model surface",
                ));
            }
            let Some(result) = mcp_computer_app_resource_read(uri) else {
                return McpOutcome::BadRequest(rpc_error(
                    id,
                    -32602,
                    format!("Resource not found: {uri}"),
                ));
            };
            let mut result = mcp_stateless_result(result, true);
            // Canonical versioned App URIs are immutable for caching. Hidden
            // legacy Computer URIs intentionally alias the current HTML and remain stale.
            if uri == MCP_COMPUTER_UI_RESOURCE_URI
                || uri == MCP_COMPUTER_APP_PROBE_RESOURCE_URI
                || uri == MCP_COMPUTER_APP_IMAGE_PROBE_RESOURCE_URI
                || uri == MCP_COMPUTER_APP_SNAPSHOT_PROBE_RESOURCE_URI
                || uri == MCP_COMPUTER_APP_SNAPSHOT_DECODE_PROBE_RESOURCE_URI
                || uri == MCP_COMPUTER_APP_IMAGE_SIZE_PROBE_RESOURCE_URI
                || uri == MCP_COMPUTER_APP_IMAGE_DIMENSION_PROBE_RESOURCE_URI
                || uri == MCP_COMPUTER_APP_IMAGE_MATRIX_PROBE_RESOURCE_URI
            {
                result["ttlMs"] = Value::from(MCP_COMPUTER_UI_RESOURCE_TTL_MS);
            }
            rpc_result(id, result)
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
            if params.name == MCP_COMPUTER_APP_PROBE_TOOL_NAME {
                if !stateless_2026 || runtime.model_surface() != ModelSurface::FullOperatorRuntime {
                    if let Some(lc) = lifecycle.as_deref() {
                        lc.dispatch_failed("surface_denied");
                        lc.dispatch_finished(false, Some(false), "surface_denied");
                    }
                    return McpOutcome::BadRequest(rpc_error(
                        id,
                        -32602,
                        "computer_app_probe requires the stateless-2026 full-operator MCP surface",
                    ));
                }
                let arguments_valid = params.arguments.is_null()
                    || params
                        .arguments
                        .as_object()
                        .is_some_and(|arguments| arguments.is_empty());
                if !arguments_valid {
                    if let Some(lc) = lifecycle.as_deref() {
                        lc.dispatch_failed("invalid_arguments");
                        lc.dispatch_finished(false, Some(false), "invalid_arguments");
                    }
                    return McpOutcome::BadRequest(rpc_error(
                        id,
                        -32602,
                        "computer_app_probe accepts no arguments",
                    ));
                }
                if let Some(outcome) = require_mcp_scope(auth, crate::auth::SCOPE_RUNTIME_READ) {
                    if let Some(lc) = lifecycle.as_deref() {
                        lc.dispatch_failed("forbidden");
                        lc.dispatch_finished(false, Some(false), "forbidden");
                    }
                    return outcome;
                }
                if let Some(lc) = lifecycle.as_deref() {
                    lc.dispatch_finished(true, Some(true), "success");
                }
                return McpOutcome::Ok(rpc_result(
                    id,
                    mcp_stateless_result(mcp_computer_app_probe_tool_result(), false),
                ));
            }
            if params.name == MCP_COMPUTER_APP_IMAGE_PROBE_TOOL_NAME {
                if !stateless_2026 || runtime.model_surface() != ModelSurface::FullOperatorRuntime {
                    if let Some(lc) = lifecycle.as_deref() {
                        lc.dispatch_failed("surface_denied");
                        lc.dispatch_finished(false, Some(false), "surface_denied");
                    }
                    return McpOutcome::BadRequest(rpc_error(
                        id,
                        -32602,
                        "computer_app_image_probe requires the stateless-2026 full-operator MCP surface",
                    ));
                }
                let arguments_valid = params.arguments.is_null()
                    || params
                        .arguments
                        .as_object()
                        .is_some_and(|arguments| arguments.is_empty());
                if !arguments_valid {
                    if let Some(lc) = lifecycle.as_deref() {
                        lc.dispatch_failed("invalid_arguments");
                        lc.dispatch_finished(false, Some(false), "invalid_arguments");
                    }
                    return McpOutcome::BadRequest(rpc_error(
                        id,
                        -32602,
                        "computer_app_image_probe accepts no arguments",
                    ));
                }
                if let Some(outcome) = require_mcp_scope(auth, crate::auth::SCOPE_RUNTIME_READ) {
                    if let Some(lc) = lifecycle.as_deref() {
                        lc.dispatch_failed("forbidden");
                        lc.dispatch_finished(false, Some(false), "forbidden");
                    }
                    return outcome;
                }
                if let Some(lc) = lifecycle.as_deref() {
                    lc.dispatch_finished(true, Some(true), "success");
                }
                return McpOutcome::Ok(rpc_result(
                    id,
                    mcp_stateless_result(mcp_computer_app_image_probe_tool_result(), false),
                ));
            }
            if params.name == MCP_COMPUTER_APP_IMAGE_SIZE_PROBE_TOOL_NAME {
                if !stateless_2026 || runtime.model_surface() != ModelSurface::FullOperatorRuntime {
                    if let Some(lc) = lifecycle.as_deref() {
                        lc.dispatch_failed("surface_denied");
                        lc.dispatch_finished(false, Some(false), "surface_denied");
                    }
                    return McpOutcome::BadRequest(rpc_error(
                        id,
                        -32602,
                        "computer_app_image_size_probe requires the stateless-2026 full-operator MCP surface",
                    ));
                }
                let size = params.arguments.as_object().and_then(|arguments| {
                    if arguments.len() != 1 {
                        return None;
                    }
                    arguments
                        .get("size")
                        .and_then(Value::as_str)
                        .filter(|size| mcp_computer_app_image_size_probe_target(size).is_some())
                        .map(str::to_string)
                });
                let Some(size) = size else {
                    if let Some(lc) = lifecycle.as_deref() {
                        lc.dispatch_failed("invalid_arguments");
                        lc.dispatch_finished(false, Some(false), "invalid_arguments");
                    }
                    return McpOutcome::BadRequest(rpc_error(
                        id,
                        -32602,
                        "computer_app_image_size_probe requires exactly one supported size",
                    ));
                };
                if let Some(outcome) = require_mcp_scope(auth, crate::auth::SCOPE_RUNTIME_READ) {
                    if let Some(lc) = lifecycle.as_deref() {
                        lc.dispatch_failed("forbidden");
                        lc.dispatch_finished(false, Some(false), "forbidden");
                    }
                    return outcome;
                }
                let result = mcp_computer_app_image_size_probe_tool_result(&size);
                let success = result.get("isError").and_then(Value::as_bool) == Some(false);
                if let Some(lc) = lifecycle.as_deref() {
                    lc.dispatch_finished(
                        true,
                        Some(success),
                        if success { "success" } else { "tool_error" },
                    );
                }
                return McpOutcome::Ok(rpc_result(id, mcp_stateless_result(result, false)));
            }
            if params.name == MCP_COMPUTER_APP_IMAGE_DIMENSION_PROBE_TOOL_NAME {
                if !stateless_2026 || runtime.model_surface() != ModelSurface::FullOperatorRuntime {
                    if let Some(lc) = lifecycle.as_deref() {
                        lc.dispatch_failed("surface_denied");
                        lc.dispatch_finished(false, Some(false), "surface_denied");
                    }
                    return McpOutcome::BadRequest(rpc_error(
                        id,
                        -32602,
                        "computer_app_image_dimension_probe requires the stateless-2026 full-operator MCP surface",
                    ));
                }
                let dimension = params.arguments.as_object().and_then(|arguments| {
                    if arguments.len() != 1 {
                        return None;
                    }
                    arguments
                        .get("dimension")
                        .and_then(Value::as_str)
                        .filter(|dimension| {
                            mcp_computer_app_image_dimension_probe_target(dimension).is_some()
                        })
                        .map(str::to_string)
                });
                let Some(dimension) = dimension else {
                    if let Some(lc) = lifecycle.as_deref() {
                        lc.dispatch_failed("invalid_arguments");
                        lc.dispatch_finished(false, Some(false), "invalid_arguments");
                    }
                    return McpOutcome::BadRequest(rpc_error(
                        id,
                        -32602,
                        "computer_app_image_dimension_probe requires exactly one supported dimension",
                    ));
                };
                if let Some(outcome) = require_mcp_scope(auth, crate::auth::SCOPE_RUNTIME_READ) {
                    if let Some(lc) = lifecycle.as_deref() {
                        lc.dispatch_failed("forbidden");
                        lc.dispatch_finished(false, Some(false), "forbidden");
                    }
                    return outcome;
                }
                let result = mcp_computer_app_image_dimension_probe_tool_result(&dimension);
                let success = result.get("isError").and_then(Value::as_bool) == Some(false);
                if let Some(lc) = lifecycle.as_deref() {
                    lc.dispatch_finished(
                        true,
                        Some(success),
                        if success { "success" } else { "tool_error" },
                    );
                }
                return McpOutcome::Ok(rpc_result(id, mcp_stateless_result(result, false)));
            }
            if params.name == MCP_COMPUTER_APP_IMAGE_MATRIX_PROBE_TOOL_NAME {
                if !stateless_2026 || runtime.model_surface() != ModelSurface::FullOperatorRuntime {
                    if let Some(lc) = lifecycle.as_deref() {
                        lc.dispatch_failed("surface_denied");
                        lc.dispatch_finished(false, Some(false), "surface_denied");
                    }
                    return McpOutcome::BadRequest(rpc_error(
                        id,
                        -32602,
                        "computer_app_image_matrix_probe requires the stateless-2026 full-operator MCP surface",
                    ));
                }
                let matrix = params.arguments.as_object().and_then(|arguments| {
                    if arguments.len() != 3 {
                        return None;
                    }
                    let codec = arguments.get("codec").and_then(Value::as_str)?;
                    let dimension = arguments.get("dimension").and_then(Value::as_str)?;
                    let payload = arguments.get("payload").and_then(Value::as_str)?;
                    if !matches!(codec, "png" | "jpeg")
                        || mcp_computer_app_image_dimension_probe_target(dimension).is_none()
                        || mcp_computer_app_image_matrix_probe_payload_bytes(payload).is_none()
                    {
                        return None;
                    }
                    Some((
                        codec.to_string(),
                        dimension.to_string(),
                        payload.to_string(),
                    ))
                });
                let Some((codec, dimension, payload)) = matrix else {
                    if let Some(lc) = lifecycle.as_deref() {
                        lc.dispatch_failed("invalid_arguments");
                        lc.dispatch_finished(false, Some(false), "invalid_arguments");
                    }
                    return McpOutcome::BadRequest(rpc_error(
                        id,
                        -32602,
                        "computer_app_image_matrix_probe requires exactly one supported codec, dimension, and payload",
                    ));
                };
                if let Some(outcome) = require_mcp_scope(auth, crate::auth::SCOPE_RUNTIME_READ) {
                    if let Some(lc) = lifecycle.as_deref() {
                        lc.dispatch_failed("forbidden");
                        lc.dispatch_finished(false, Some(false), "forbidden");
                    }
                    return outcome;
                }
                let result =
                    mcp_computer_app_image_matrix_probe_tool_result(&codec, &dimension, &payload);
                let success = result.get("isError").and_then(Value::as_bool) == Some(false);
                if let Some(lc) = lifecycle.as_deref() {
                    lc.dispatch_finished(
                        true,
                        Some(success),
                        if success { "success" } else { "tool_error" },
                    );
                }
                return McpOutcome::Ok(rpc_result(id, mcp_stateless_result(result, false)));
            }
            if params.name == MCP_COMPUTER_APP_SNAPSHOT_PROBE_TOOL_NAME
                || params.name == MCP_COMPUTER_APP_SNAPSHOT_DECODE_PROBE_TOOL_NAME
            {
                let snapshot_probe_name = params.name.clone();
                if !stateless_2026 || runtime.model_surface() != ModelSurface::FullOperatorRuntime {
                    if let Some(lc) = lifecycle.as_deref() {
                        lc.dispatch_failed("surface_denied");
                        lc.dispatch_finished(false, Some(false), "surface_denied");
                    }
                    return McpOutcome::BadRequest(rpc_error(
                        id,
                        -32602,
                        format!(
                            "{snapshot_probe_name} requires the stateless-2026 full-operator MCP surface"
                        ),
                    ));
                }
                let session_id = strip_reserved_session_id(&mut params.arguments);
                let outcome = runtime
                    .call_tool_with_context(
                        KernelToolCallRequest {
                            tool_name: "computer_snapshot".to_string(),
                            arguments: params.arguments,
                        },
                        ToolCallContext {
                            transport: ToolTransport::Mcp,
                            session_id: session_id.as_deref(),
                            auth,
                            window,
                            record_oauth_scope_denials: false,
                            host_file_import_trust,
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
                        return scope_forbidden(auth, required_scope, description);
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
                    let category = if result.success {
                        "success"
                    } else {
                        "tool_error"
                    };
                    lc.dispatch_finished(true, Some(result.success), category);
                }
                let result = mcp_runtime_tool_result("computer_snapshot", false, result);
                return McpOutcome::Ok(rpc_result(id, mcp_stateless_result(result, false)));
            }
            let artifact_export_caller = if params.name == "export_project_artifact" {
                if !stateless_2026 || runtime.model_surface() != ModelSurface::FullOperatorRuntime {
                    return McpOutcome::BadRequest(rpc_error(
                        id,
                        -32602,
                        "export_project_artifact requires the stateless-2026 full-operator MCP surface",
                    ));
                }
                match mcp_artifact_export_caller_binding(auth) {
                    Ok(caller) => Some(caller),
                    Err(error) => {
                        return McpOutcome::BadRequest(rpc_error(
                            id,
                            -32602,
                            format!("export_project_artifact cannot bind this caller: {error}"),
                        ))
                    }
                }
            } else {
                None
            };
            if runtime.model_surface() == ModelSurface::CanonicalConnector {
                let connector = connector.expect("validated canonical Connector state");
                if !stateless_2026 && params.name == "task_start" && window.is_none() {
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
                    return scope_forbidden(auth, Some(required_scope), description);
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
                        host_file_import_trust,
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
                    return scope_forbidden(auth, required_scope, description);
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
            let result = if params.name == "export_project_artifact" {
                mcp_artifact_export_tool_result(
                    result,
                    artifact_export_caller.expect("validated artifact export caller binding"),
                )
            } else {
                mcp_runtime_tool_result(&params.name, as_image_requested, result)
            };
            rpc_result(
                id,
                if stateless_2026 {
                    mcp_stateless_result(result, false)
                } else {
                    result
                },
            )
        }
        "notifications/initialized" if !stateless_2026 => rpc_result(id, json!({})),
        _ => {
            let body = rpc_error(id, -32601, format!("Method not found: {}", request.method));
            return if stateless_2026 {
                McpOutcome::NotFound(body)
            } else {
                McpOutcome::BadRequest(body)
            };
        }
    };
    McpOutcome::Ok(response)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HostFileImportTrustReason {
    Trusted,
    MissingConfig,
    MissingDatabase,
    MissingAuth,
    NotOAuthToken,
    MissingAllowedClientId,
    OAuthDisabled,
    ClientIdNotConfigured,
    ClientRegistrationMissingOrRevoked,
    ClientRegistrationLookupFailed,
}

impl HostFileImportTrustReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::Trusted => "trusted",
            Self::MissingConfig => "missing_config",
            Self::MissingDatabase => "missing_database",
            Self::MissingAuth => "missing_auth",
            Self::NotOAuthToken => "not_oauth_token",
            Self::MissingAllowedClientId => "missing_allowed_client_id",
            Self::OAuthDisabled => "oauth_disabled",
            Self::ClientIdNotConfigured => "client_id_not_configured",
            Self::ClientRegistrationMissingOrRevoked => "client_registration_missing_or_revoked",
            Self::ClientRegistrationLookupFailed => "client_registration_lookup_failed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HostFileImportTrustDecision {
    trust: HostFileImportTrust,
    reason: HostFileImportTrustReason,
    config_present: bool,
    database_present: bool,
    oauth_enabled: bool,
    configured_trusted_client_count: usize,
    client_id_configured: Option<bool>,
    active_client_registration_found: Option<bool>,
}

#[cfg(test)]
static LAST_MCP_HOST_FILE_IMPORT_TRUST_DECISION: std::sync::OnceLock<
    std::sync::Mutex<Option<HostFileImportTrustDecision>>,
> = std::sync::OnceLock::new();

#[cfg(test)]
fn take_last_mcp_host_file_import_trust_decision() -> Option<HostFileImportTrustDecision> {
    LAST_MCP_HOST_FILE_IMPORT_TRUST_DECISION
        .get_or_init(|| std::sync::Mutex::new(None))
        .lock()
        .unwrap()
        .take()
}

impl HostFileImportTrustDecision {
    fn unavailable(reason: HostFileImportTrustReason) -> Self {
        Self {
            trust: HostFileImportTrust::Untrusted,
            reason,
            config_present: false,
            database_present: false,
            oauth_enabled: false,
            configured_trusted_client_count: 0,
            client_id_configured: None,
            active_client_registration_found: None,
        }
    }

    fn from_config(reason: HostFileImportTrustReason, config: &crate::Config) -> Self {
        Self {
            trust: HostFileImportTrust::Untrusted,
            reason,
            config_present: true,
            database_present: false,
            oauth_enabled: config.oauth2.enabled,
            configured_trusted_client_count: config.oauth2.trusted_mcp_file_client_ids.len(),
            client_id_configured: None,
            active_client_registration_found: None,
        }
    }
}

fn mcp_host_file_import_trust_decision_from_state(
    config: &crate::Config,
    db: &crate::Database,
    auth: Option<&AuthContext>,
) -> HostFileImportTrustDecision {
    let base = HostFileImportTrustDecision {
        trust: HostFileImportTrust::Untrusted,
        reason: HostFileImportTrustReason::MissingAuth,
        config_present: true,
        database_present: true,
        oauth_enabled: config.oauth2.enabled,
        configured_trusted_client_count: config.oauth2.trusted_mcp_file_client_ids.len(),
        client_id_configured: None,
        active_client_registration_found: None,
    };
    let Some(auth) = auth else {
        return base;
    };
    if !auth.is_oauth_token() {
        return HostFileImportTrustDecision {
            reason: HostFileImportTrustReason::NotOAuthToken,
            ..base
        };
    }
    let Some(client_id) = auth
        .allowed_client_id
        .as_deref()
        .map(str::trim)
        .filter(|client_id| !client_id.is_empty())
    else {
        return HostFileImportTrustDecision {
            reason: HostFileImportTrustReason::MissingAllowedClientId,
            ..base
        };
    };
    if !config.oauth2.enabled {
        return HostFileImportTrustDecision {
            reason: HostFileImportTrustReason::OAuthDisabled,
            ..base
        };
    }
    let client_id_configured = config
        .oauth2
        .trusted_mcp_file_client_ids
        .iter()
        .any(|trusted_client_id| trusted_client_id == client_id);
    if !client_id_configured {
        return HostFileImportTrustDecision {
            reason: HostFileImportTrustReason::ClientIdNotConfigured,
            client_id_configured: Some(false),
            ..base
        };
    }
    match db.get_oauth_client_by_client_id(client_id) {
        Ok(Some(client)) if client.client_id == client_id => HostFileImportTrustDecision {
            trust: HostFileImportTrust::TrustedOAuthClient,
            reason: HostFileImportTrustReason::Trusted,
            client_id_configured: Some(true),
            active_client_registration_found: Some(true),
            ..base
        },
        Ok(_) => HostFileImportTrustDecision {
            reason: HostFileImportTrustReason::ClientRegistrationMissingOrRevoked,
            client_id_configured: Some(true),
            active_client_registration_found: Some(false),
            ..base
        },
        Err(_) => HostFileImportTrustDecision {
            reason: HostFileImportTrustReason::ClientRegistrationLookupFailed,
            client_id_configured: Some(true),
            active_client_registration_found: None,
            ..base
        },
    }
}

#[cfg(test)]
fn mcp_host_file_import_trust_from_state(
    config: &crate::Config,
    db: &crate::Database,
    auth: Option<&AuthContext>,
) -> HostFileImportTrust {
    mcp_host_file_import_trust_decision_from_state(config, db, auth).trust
}

fn mcp_host_file_import_trust_decision(
    depot: &Depot,
    auth: Option<&AuthContext>,
) -> HostFileImportTrustDecision {
    let Some(config) = crate::auth::get_config(depot) else {
        return HostFileImportTrustDecision::unavailable(HostFileImportTrustReason::MissingConfig);
    };
    let Some(db) = crate::auth::get_db(depot) else {
        return HostFileImportTrustDecision::from_config(
            HostFileImportTrustReason::MissingDatabase,
            config.as_ref(),
        );
    };
    mcp_host_file_import_trust_decision_from_state(config.as_ref(), db.as_ref(), auth)
}

fn mcp_auth_kind_classification(auth: Option<&AuthContext>) -> &'static str {
    match auth.map(|auth| auth.kind) {
        None => "none",
        Some(crate::auth::AuthKind::OAuth2Token) => "oauth2",
        Some(crate::auth::AuthKind::ApiToken) => "api_token",
        Some(crate::auth::AuthKind::Bootstrap) => "bootstrap",
        Some(crate::auth::AuthKind::AgentToken) => "agent_token",
        Some(crate::auth::AuthKind::AccountCredential) => "account_credential",
        Some(crate::auth::AuthKind::SharedKey) => "shared_key",
        Some(crate::auth::AuthKind::ProjectCredential) => "project_credential",
        Some(crate::auth::AuthKind::OpenAnonymous) => "open_anonymous",
    }
}

fn mcp_token_kind_classification(auth: Option<&AuthContext>) -> &'static str {
    match auth.and_then(|auth| auth.token_kind.as_deref()) {
        None => "none",
        Some("oauth2") => "oauth2",
        Some("oauth2_shared_key") => "oauth2_shared_key",
        Some("user") => "user",
        Some("agent") => "agent",
        Some(_) => "other",
    }
}

fn log_mcp_host_file_import_trust_decision(
    auth: Option<&AuthContext>,
    decision: &HostFileImportTrustDecision,
) {
    #[cfg(test)]
    {
        *LAST_MCP_HOST_FILE_IMPORT_TRUST_DECISION
            .get_or_init(|| std::sync::Mutex::new(None))
            .lock()
            .unwrap() = Some(*decision);
    }
    let allowed_client_id_present = auth
        .and_then(|auth| auth.allowed_client_id.as_deref())
        .is_some_and(|client_id| !client_id.trim().is_empty());
    tracing::info!(
        target: "webcodex::mcp",
        trust = decision.trust.is_trusted(),
        reason = decision.reason.as_str(),
        auth_kind = mcp_auth_kind_classification(auth),
        token_kind = mcp_token_kind_classification(auth),
        allowed_client_id_present,
        config_present = decision.config_present,
        database_present = decision.database_present,
        oauth_enabled = decision.oauth_enabled,
        configured_trusted_client_count = decision.configured_trusted_client_count,
        client_id_configured = ?decision.client_id_configured,
        active_client_registration_found = ?decision.active_client_registration_found,
        "mcp_host_file_import_trust_decision"
    );
}

fn require_mcp_scope(auth: Option<&AuthContext>, scope: &'static str) -> Option<McpOutcome> {
    let auth = auth?;
    if auth.has_scope(scope) {
        return None;
    }
    Some(scope_forbidden(
        Some(auth),
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

fn scope_forbidden(
    auth: Option<&AuthContext>,
    required_scope: Option<&'static str>,
    description: impl Into<String>,
) -> McpOutcome {
    McpOutcome::Forbidden {
        body: crate::auth::scope_forbidden_body(auth, description),
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

fn rpc_error_with_data(
    id: Option<Value>,
    code: i64,
    message: impl Into<String>,
    data: Value,
) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id.unwrap_or(Value::Null),
        "error": {
            "code": code,
            "message": message.into(),
            "data": data,
        }
    })
}

#[cfg(test)]
#[path = "mcp_tests.rs"]
mod tests;
