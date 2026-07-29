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
                "maxLength": 4000,
                "description": "Optional current user instruction. On creation it is retained as the root task title; on continuation it is appended to the existing ledger and never overwrites the root title."
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
            "resume_session_id": {
                "type": "string",
                "pattern": "^wc_sess_[A-Za-z0-9_]+$",
                "description": "Optional explicit Workflow Session recovery id. When present, start_coding_task only resumes that known active Session after exact project, lifecycle, access, and capability checks; failure never falls back to a current binding or creates a Session. Without a stable window, resume still succeeds but no current binding is created and later project tools must pass session_id explicitly. Distinct from project-tool session_id and wrapper recording_session_id. Mutually exclusive with new_session=true."
            },
            "bind_current": {
                "type": "boolean",
                "default": true,
                "description": "Ensure and bind the exact window/caller/transport/project/canonical-root current session. Defaults to true. A stable transport window is required; the process-local cache and hashed durable ledger projection support automatic reuse across restart without credential-wide fallback."
            },
            "new_session": {
                "type": "boolean",
                "default": false,
                "description": "Explicit advanced isolation request. When true, create and bind a new Workflow Session without closing or rewriting the previous one. Title differences never imply a new session. Mutually exclusive with resume_session_id."
            }
        },
        "required": ["project"],
        "additionalProperties": false,
        "not": {
            "required": ["resume_session_id", "new_session"],
            "properties": {
                "new_session": {"const": true}
            }
        },
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
