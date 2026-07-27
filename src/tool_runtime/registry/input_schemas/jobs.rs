use serde_json::{json, Value};

use super::common::{object_schema, with_optional_session_id};

pub(crate) fn run_shell_input_schema() -> Value {
    let mut schema = object_schema(with_optional_session_id(vec![
        ("project", "string", "Configured project id.", true),
        ("command", "string", "Shell command to run.", true),
        (
            "timeout_secs",
            "integer",
            "Synchronous command timeout in seconds (minimum 1, maximum 120, default 60). Out-of-range values are rejected before the command starts; use run_job for longer work.",
            false,
        ),
        (
            "cwd",
            "string",
            "Working directory contract: omit, empty string, or '.' selects the project root; any other value is project-relative and may not escape the project root.",
            false,
        ),
        (
            "purpose",
            "string",
            "Declared execution intent: validation, test, build, format, release, diagnostic, operation, or other. This records evidence and never changes authorization.",
            false,
        ),
        (
            "shell",
            "string",
            "Optional explicit command language: sh or bash. When omitted, local run_shell uses sh and an agent-backed run_shell uses that Agent's configured shell. The response always records the actual selection.",
            false,
        ),
    ]));
    schema["properties"]["purpose"]["enum"] = json!([
        "validation",
        "test",
        "build",
        "format",
        "release",
        "diagnostic",
        "operation",
        "other"
    ]);
    schema["properties"]["shell"]["enum"] = json!(["sh", "bash"]);
    schema["properties"]["timeout_secs"]["minimum"] = json!(1);
    schema["properties"]["timeout_secs"]["maximum"] = json!(120);
    schema["properties"]["timeout_secs"]["default"] = json!(60);
    schema
}

pub(crate) fn run_job_input_schema() -> Value {
    let mut schema = object_schema(with_optional_session_id(vec![
        ("project", "string", "Configured project id.", true),
        (
            "command",
            "string",
            "Shell command to run asynchronously.",
            true,
        ),
        (
            "timeout_secs",
            "integer",
            "Maximum runtime in seconds.",
            false,
        ),
        (
            "cwd",
            "string",
            "Working directory contract: omit, empty string, or '.' selects the project root; any other value is project-relative and may not escape the project root.",
            false,
        ),
        (
            "purpose",
            "string",
            "Declared execution intent: validation, test, build, format, release, diagnostic, operation, or other. This records evidence and never changes authorization.",
            false,
        ),
        (
            "shell",
            "string",
            "Optional explicit command language: sh or bash. When omitted, local run_job preserves its existing bash contract and an agent-backed run_job uses that Agent's configured shell. The response always records the actual selection.",
            false,
        ),
    ]));
    schema["properties"]["purpose"]["enum"] = json!([
        "validation",
        "test",
        "build",
        "format",
        "release",
        "diagnostic",
        "operation",
        "other"
    ]);
    schema["properties"]["shell"]["enum"] = json!(["sh", "bash"]);
    schema
}

pub(crate) fn stop_job_input_schema() -> Value {
    object_schema(with_optional_session_id(vec![
        (
            "project",
            "string",
            "Configured project id that must match the job project.",
            true,
        ),
        ("job_id", "string", "Runtime job id returned by run_job.", true),
        (
            "confirm",
            "boolean",
            "Must be true to stop or no-op an already-finished job; false returns confirmation_required.",
            false,
        ),
    ]))
}

pub(crate) fn job_status_input_schema() -> Value {
    object_schema(vec![
        ("job_id", "string", "Job id.", true),
        (
            "include_command_preview",
            "boolean",
            "Optional debug flag. Defaults to false; when true, includes bounded command_preview metadata. stdout/stderr bodies are never included.",
            false,
        ),
    ])
}

pub(crate) fn job_log_input_schema() -> Value {
    object_schema(vec![
        ("job_id", "string", "Job id.", true),
        (
            "offset",
            "integer",
            "Optional 1-based cursor returned by a previous call. Reads the next bounded segment.",
            false,
        ),
        (
            "tail_lines",
            "integer",
            "Optional number of trailing lines per stream. Defaults to 200 and is capped at 500.",
            false,
        ),
    ])
}

pub(crate) fn list_jobs_input_schema() -> Value {
    object_schema(vec![
        (
            "limit",
            "integer",
            "Maximum number of job summaries to return.",
            false,
        ),
        (
            "status",
            "string",
            "Optional status filter (e.g. running, completed, failed).",
            false,
        ),
    ])
}

pub(crate) fn job_tail_input_schema() -> Value {
    object_schema(vec![
        ("job_id", "string", "Job id.", true),
        (
            "tail_lines",
            "integer",
            "Optional number of trailing lines to return per stream. Defaults to 200 and is capped at 500.",
            false,
        ),
    ])
}
