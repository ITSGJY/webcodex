use super::super::input_schemas::{
    finish_coding_task_input_schema, start_coding_task_input_schema,
};
use super::tool_spec;
use crate::tool_runtime::tool_spec::ToolSpec;

pub(super) fn tool_specs() -> Vec<ToolSpec> {
    vec![
        tool_spec(
            "start_coding_task",
            "Start or continue coding. By default, a stable window reuses its active Workflow Session for the same project and canonical repository, appends the instruction, and applies safe capability upgrades. Set new_session=true for isolation. Returns bounded startup context. Never calls an LLM.",
            start_coding_task_input_schema(),
        ),
        tool_spec(
            "finish_coding_task",
            "Deterministic coding-task finish aggregate for an explicit session_id. Returns show_changes, optional hygiene and handoff, validation-like ledger events, workspace warnings, and dirty-state signals. Never calls an LLM, emits raw stdout/stderr, or infers validation root causes.",
            finish_coding_task_input_schema(),
        ),
    ]
}
