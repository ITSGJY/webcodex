use super::super::input_schemas::{
    finish_coding_task_input_schema, start_coding_task_input_schema,
};
use super::tool_spec;
use crate::tool_runtime::tool_spec::ToolSpec;

pub(super) fn tool_specs() -> Vec<ToolSpec> {
    vec![
        tool_spec(
            "start_coding_task",
            "Start or continue coding in an existing or Runner-managed temporary project. Stable windows reuse their Workflow Session. Use resume_session_id for explicit recovery or new_session=true for isolation. Returns deterministic startup context; unbound callers pass session_id to later project tools.",
            start_coding_task_input_schema(),
        ),
        tool_spec(
            "finish_coding_task",
            "Deterministic coding-task finish aggregate for an explicit session_id. Returns show_changes, optional hygiene and handoff, validation-like ledger events, workspace warnings, and dirty-state signals. Never calls an LLM, emits raw stdout/stderr, or infers validation root causes.",
            finish_coding_task_input_schema(),
        ),
    ]
}
