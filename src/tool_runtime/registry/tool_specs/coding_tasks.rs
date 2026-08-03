use super::super::input_schemas::{
    finish_coding_task_input_schema, start_coding_task_input_schema, work_on_project_input_schema,
};
use super::tool_spec;
use crate::tool_runtime::tool_spec::ToolSpec;

pub(super) fn tool_specs() -> Vec<ToolSpec> {
    vec![
        tool_spec(
            "start_coding_task",
            "Start or continue coding from an existing project, a Runner-owned absolute path, or a managed temporary project. Paths are policy-checked and reused or permanently registered before exact Session handling. Stable windows reuse; resume_session_id recovers exactly; new_session=true isolates.",
            start_coding_task_input_schema(),
        ),
        tool_spec(
            "work_on_project",
            "Start normal coding from an existing project or Runner-owned absolute path, or continue exactly by session_id. Unregistered paths are permanently registered before Session handling. Returns compact context; use start_coding_task for advanced modes, guards, temporary projects, or binding.",
            work_on_project_input_schema(),
        ),
        tool_spec(
            "finish_coding_task",
            "Return an optional deterministic evidence snapshot for model review, including workspace, validation, jobs, and recorded tool events. The result is advisory: it does not decide task completion, replace direct diff or test review, or generate the user-facing final report.",
            finish_coding_task_input_schema(),
        ),
    ]
}
