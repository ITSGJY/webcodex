use serde_json::{json, Value};

use super::common::{
    array_schema, nullable_schema, open_object_schema, schema_type, wrapped_output_schema,
};

fn state_schema() -> Value {
    json!({"type":"string","enum":["starting","running","waiting_permission","completed","failed","cancelled","lost"]})
}

fn execution_state_schema() -> Value {
    json!({"type":"string","enum":["not_started","started","outcome_unknown","completed"]})
}

fn terminal_schema() -> Value {
    json!({
        "type":"object",
        "additionalProperties":false,
        "properties":{
            "stop_reason": nullable_schema("string", "Correlated stable ACP v1 stop reason when available."),
            "error_code": nullable_schema("string", "Bounded protocol/provider terminal error code when available."),
            "message": nullable_schema("string", "Bounded terminal diagnostic; never reasoning or a transcript."),
            "completed_at": schema_type("integer", "Unix terminal timestamp.")
        }
    })
}

fn common_run_fields() -> Vec<(&'static str, Value)> {
    vec![
        ("run_id", schema_type("string", "Opaque CodingAgentRun id.")),
        (
            "project",
            schema_type("string", "Exact registered runtime Project id."),
        ),
        (
            "provider_id",
            schema_type("string", "Logical operator-configured provider id."),
        ),
        ("state", state_schema()),
        ("execution_state", execution_state_schema()),
        ("terminal", terminal_schema()),
        (
            "error_kind",
            schema_type(
                "string",
                "Bounded failure classification when unsuccessful.",
            ),
        ),
    ]
}

pub(super) fn output_schema_for_tool(name: &str) -> Option<Value> {
    let mut fields = common_run_fields();
    match name {
        "coding_agent_start" => {
            fields.push((
                "observation_token",
                schema_type("string", "Opaque Run-bound observation token."),
            ));
            Some(wrapped_output_schema(fields))
        }
        "coding_agent_observe" => {
            fields.extend([
                ("events", array_schema(open_object_schema("Bounded normalized CodingAgentRun event; raw ACP JSON is never exposed."), "Only-new retained normalized events.")),
                ("observation_token", schema_type("string", "Opaque Run-bound token for the next observation.")),
                ("has_more", schema_type("boolean", "True when retained newer events remain after this page.")),
                ("history_lost", schema_type("boolean", "True when the requested cursor predates retained history or the Server epoch rebaselined.")),
                ("first_retained_sequence", schema_type("integer", "First currently retained Runner event sequence.")),
            ]);
            Some(wrapped_output_schema(fields))
        }
        "coding_agent_cancel" => {
            fields.push((
                "cancel_requested",
                schema_type(
                    "boolean",
                    "True when cancellation was requested for a nonterminal Run.",
                ),
            ));
            Some(wrapped_output_schema(fields))
        }
        _ => None,
    }
}
