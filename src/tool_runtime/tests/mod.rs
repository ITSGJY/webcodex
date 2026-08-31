//! Domain-organized test modules for tool_runtime.

mod support;

macro_rules! selected_test_modules {
    ($feature:literal => [$($module:ident),+ $(,)?]) => {
        $(
            #[cfg(any(
                not(feature = "selective-unit-tests"),
                feature = $feature,
            ))]
            mod $module;
        )+
    };
}

selected_test_modules!("unit-tool-runtime-files" => [
    apply_text_edits,
    checkpoint,
    files,
    files_helpers,
    read_files,
    search_project_texts,
    unified_diff,
]);

selected_test_modules!("unit-tool-runtime-git" => [git, hygiene, sessions_git]);

selected_test_modules!("unit-tool-runtime-jobs" => [
    jobs,
    observe_jobs,
    process,
    reconnect,
    script,
    session_shells,
    sync_timeout,
    targeted_inventory,
]);

selected_test_modules!("unit-tool-runtime-sessions" => [
    assignment_fence,
    collaboration,
    context_projection,
    continuation_feedback,
    execution_context,
    sessions,
    sessions_guards,
    sessions_instructions,
    sessions_resolver,
]);

selected_test_modules!("unit-tool-runtime-validation" => [
    validation_events,
    validation_handoff,
    validation_identity,
    validation_parser,
    validation_profile,
    validation_summary,
]);

selected_test_modules!("unit-tool-runtime-contracts" => [
    dispatch,
    edit_tool_telemetry,
    metadata,
    permission_gate,
    schema,
    tool_call,
]);

selected_test_modules!("unit-tool-runtime-workflow" => [
    coding_task,
    coding_task_semantic_navigation,
    handoff,
    handoff_brief,
    lsp,
    memory,
    skills,
    startup_brief,
    trusted_smoke,
    work_on_project,
]);
