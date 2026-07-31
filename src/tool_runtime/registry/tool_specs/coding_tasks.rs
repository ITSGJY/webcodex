use super::super::input_schemas::{
    finish_coding_task_input_schema, start_coding_task_input_schema,
};
use super::tool_spec;
use crate::tool_runtime::tool_spec::ToolSpec;

pub(super) fn tool_specs() -> Vec<ToolSpec> {
    vec![
        tool_spec(
            "start_coding_task",
            "Start/continue coding. Pass project for an existing project, or client_id (plus optional temporary_project_name) to create and register a Runner-managed temporary project before binding a new Workflow Session. Stable windows reuse a Workflow Session by default. resume_session_id resumes an active Session across or without windows; failure never creates or falls back. If unbound, tools need session_id. new_session=true isolates and is mutually exclusive. Returns startup context.",
            start_coding_task_input_schema(),
        ),
        tool_spec(
            "finish_coding_task",
            "Deterministic coding-task finish aggregate for an explicit session_id. Returns show_changes, optional hygiene and handoff, validation-like ledger events, workspace warnings, and dirty-state signals. Never calls an LLM, emits raw stdout/stderr, or infers validation root causes.",
            finish_coding_task_input_schema(),
        ),
    ]
}
