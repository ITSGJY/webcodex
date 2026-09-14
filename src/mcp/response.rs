use crate::connector_runtime::ConnectorCallOutcome;
use crate::tool_runtime::ToolResult;
use serde_json::{json, Value};

pub(super) fn mcp_stateless_result(mut result: Value, cacheable: bool) -> Value {
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

fn mcp_tool_text_content(structured: &Value, concise: String) -> String {
    if crate::config::mcp_text_json_compat_enabled() {
        serde_json::to_string(structured).unwrap_or(concise)
    } else {
        concise
    }
}

pub(super) fn connector_call_tool_result(outcome: ConnectorCallOutcome) -> Value {
    // Connector output follows the same MCP layering as Runtime tools: the body
    // is canonical in structuredContent and content.text is compact by default.
    let concise = if outcome.ok {
        "WebCodex connector tool completed successfully.".to_string()
    } else {
        outcome
            .body
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("WebCodex connector tool failed.")
            .to_string()
    };
    let structured = outcome.body;
    let text = mcp_tool_text_content(&structured, concise);
    json!({
        "content": [{ "type": "text", "text": text }],
        "structuredContent": structured,
        "isError": !outcome.ok
    })
}

pub(super) fn mcp_runtime_tool_result_fallback(result: ToolResult) -> Value {
    // `structuredContent` is the canonical machine-readable result. Repeating
    // that full JSON object in `content.text` doubles model context, so the
    // compatibility copy is explicit opt-in rather than the default.
    let concise = if result.success {
        "WebCodex tool completed successfully.".to_string()
    } else {
        result
            .error
            .clone()
            .unwrap_or_else(|| "WebCodex tool failed.".to_string())
    };
    let success = result.success;
    let structured = json!({
        "success": success,
        "output": result.output,
        "error": result.error,
    });
    let text = mcp_tool_text_content(&structured, concise);
    json!({
        "content": [{ "type": "text", "text": text }],
        "structuredContent": structured,
        "isError": !success
    })
}

pub(super) fn rpc_result(id: Option<Value>, result: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id.unwrap_or(Value::Null),
        "result": result,
    })
}

pub(super) fn rpc_error(id: Option<Value>, code: i64, message: impl Into<String>) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id.unwrap_or(Value::Null),
        "error": {
            "code": code,
            "message": message.into(),
        }
    })
}

pub(super) fn rpc_error_with_data(
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
mod tests {
    use super::*;
    use serde_json::json;

    const TEXT_JSON_COMPAT_ENV: &str = "WEBCODEX_MCP_TEXT_JSON_COMPAT";

    #[test]
    fn runtime_result_keeps_compact_text_by_default() {
        let mut env = crate::test_support::TestEnvGuard::new();
        env.remove(TEXT_JSON_COMPAT_ENV);

        let rendered = mcp_runtime_tool_result_fallback(ToolResult::ok(json!({ "count": 2 })));
        assert_eq!(
            rendered["content"][0]["text"],
            "WebCodex tool completed successfully."
        );
        assert_eq!(rendered["structuredContent"]["output"]["count"], 2);
    }

    #[test]
    fn text_json_compat_mirrors_runtime_and_connector_structured_content() {
        let mut env = crate::test_support::TestEnvGuard::new();
        env.set(TEXT_JSON_COMPAT_ENV, "true");

        let runtime = mcp_runtime_tool_result_fallback(ToolResult::ok(json!({ "count": 2 })));
        assert_eq!(
            runtime["content"][0]["text"],
            serde_json::to_string(&runtime["structuredContent"]).unwrap()
        );

        let connector = connector_call_tool_result(ConnectorCallOutcome {
            ok: true,
            body: json!({ "ok": true, "data": { "task": "ready" } }),
            http_status: 200,
            required_scope: None,
            protocol_error: false,
        });
        assert_eq!(
            connector["content"][0]["text"],
            serde_json::to_string(&connector["structuredContent"]).unwrap()
        );
    }
}
