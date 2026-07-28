use serde_json::{json, Value};

use super::sessions::session_mode_schema;

pub(crate) fn start_coding_task_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "project": {
                "type": "string",
                "description": "Required runtime project id. Use a full id from list_projects, such as agent:<client_id>:<project_id>."
            },
            "title": {
                "type": "string",
                "description": "Optional human-readable task title for the created session."
            },
            "mode": session_mode_schema("Optional session mode. Defaults to normal. inspect blocks structured write tools and runs shell/job-like tools in the Linux Landlock inspect sandbox; read_only blocks both write-like and shell/job-like tools."),
            "deny_write_tools": {
                "type": "boolean",
                "description": "Optional task guard for the created session. Defaults to false unless mode=read_only."
            },
            "deny_shell_tools": {
                "type": "boolean",
                "description": "Optional task guard for the created session. Defaults to false unless mode=read_only."
            },
            "detail": {
                "type": "string",
                "enum": ["minimal", "standard", "full"],
                "default": "standard",
                "description": "Startup projection detail. minimal returns the session/project/Git/readiness/navigation essentials; standard adds the permission profile while retaining the compact continuous-coding projection; full explicitly adds full runtime status, recent commits, rules, recommended flow, and compact tool manifest."
            },
            "bind_current": {
                "type": "boolean",
                "description": "If true, bind the new session as the window/caller/transport/project current session. Defaults to false. Binding is process-local in-memory control metadata."
            }
        },
        "required": ["project"],
        "additionalProperties": false,
    })
}

pub(crate) fn finish_coding_task_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "project": {
                "type": "string",
                "description": "Required runtime project id. Use the same project used to start the task."
            },
            "session_id": {
                "type": "string",
                "description": "Required explicit wc_sess_* id returned by start_coding_task or start_session. This is business input, not current-session fallback."
            },
            "include_diff": {
                "type": "boolean",
                "description": "Include bounded diff hunks in show_changes. Defaults to true."
            },
            "include_workspace": {
                "type": "boolean",
                "description": "Defaults to true. When include_handoff=true, controls whether the nested handoff summary includes its workspace block; the top-level finish workspace/show_changes check remains unchanged."
            },
            "include_hygiene": {
                "type": "boolean",
                "description": "Include workspace_hygiene_check output. Defaults to true."
            },
            "include_handoff": {
                "type": "boolean",
                "description": "Include session_handoff_summary output. Defaults to true."
            },
            "include_validation_summary": {
                "type": "boolean",
                "description": "Include deterministic validation-like session ledger event summary when available. Defaults to true; minimal diagnostics require bounded tails or safe result metadata."
            },
            "summary_only": {
                "type": "boolean",
                "description": "When true, return compact closeout fields only: workspace_clean, hygiene_clean, jobs, permissions, tool_failures, validation, task_outcome, evidence_history, evidence_integrity, informational_notes, warnings, and suggested_next_actions. Omits show_changes payloads, handoff details, command text, stdout/stderr, tails, and excerpts."
            }
        },
        "required": ["project", "session_id"],
        "additionalProperties": false,
    })
}
