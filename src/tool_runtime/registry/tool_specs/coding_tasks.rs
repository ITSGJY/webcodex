use super::super::input_schemas::{
    finish_coding_task_input_schema, start_coding_task_input_schema, work_on_project_input_schema,
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
            "work_on_project",
            "Start a normal coding task with practical defaults, or continue one by session_id. Returns compact startup context. Use start_coding_task for advanced modes, guards, execution context, temporary projects, or binding controls.",
            work_on_project_input_schema(),
        ),
        tool_spec(
            "finish_coding_task",
            "Return an optional deterministic evidence snapshot for model review, including workspace, validation, jobs, and recorded tool events. The result is advisory: it does not decide task completion, replace direct diff or test review, or generate the user-facing final report.",
            finish_coding_task_input_schema(),
        ),
    ]
}
