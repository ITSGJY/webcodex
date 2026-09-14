//! Model-facing operations for reading, searching, editing, and executing tasks.

use super::ConnectorRuntime;
use crate::projections::{
    approval_gate_outcome, check_request_hash, command_action_hash, command_request_hash,
    edit_operation_hash, invalid_input, kernel_failure_may_have_applied, paginate_search_output,
    parse_input, parse_search_cursor, search_cursor_signature, short_oid, store_error_outcome,
    validate_operation_id, validate_path, validation_recipe_error, KernelFailure,
};
use crate::wire_models::{
    sanitize_value, ChecksRunInput, CodeImpactInput, CodeNavigateInput, CodeNavigateOperation,
    CommandsRunInput, EditsApplyInput, FilesListInput, FilesReadInput, FilesSearchInput,
    SearchResultMode,
};
use crate::{
    ConnectorCallContext, ConnectorCallOutcome, ConnectorRecipeId, ConnectorSemanticCheck,
    ConnectorToolFailure, ConnectorToolRequest, ConnectorTransport,
};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::Path;
use webcodex_core::lsp_bridge::{
    redact_absolute_paths, MAX_DOCUMENT_DIAGNOSTICS_LIMIT, MAX_DOCUMENT_SYMBOLS_LIMIT,
    MAX_FIND_REFERENCES_LIMIT, MAX_GOTO_DEFINITION_LIMIT, MAX_WORKSPACE_SYMBOLS_LIMIT,
};
use webcodex_core::runner_protocol::{
    RUNNER_CAPABILITY_STRUCTURED_GO_TEST_JSON, RUNNER_CAPABILITY_STRUCTURED_VALIDATION_ARGV,
};
use webcodex_runner_registry::command_preview;
use webcodex_store::{
    ConnectorApprovalGate, ConnectorEditOperationGate, ConnectorExecutionReservation,
    ConnectorTaskSnapshot, ConnectorTaskStoreError,
};
use webcodex_validation::{resolve_validation_recipe, RecipeId, SemanticCheck};

const COMMAND_APPROVAL_TTL_SECS: i64 = 60 * 60;
const CONNECTOR_SEARCH_WINDOW: usize = crate::projections::CONNECTOR_SEARCH_WINDOW;

fn validation_recipe_id(recipe: ConnectorRecipeId) -> RecipeId {
    match recipe {
        ConnectorRecipeId::Rust => RecipeId::Rust,
        ConnectorRecipeId::Node => RecipeId::Node,
        ConnectorRecipeId::Python => RecipeId::Python,
        ConnectorRecipeId::Go => RecipeId::Go,
    }
}

fn validation_semantic_check(check: ConnectorSemanticCheck) -> SemanticCheck {
    match check {
        ConnectorSemanticCheck::Format => SemanticCheck::Format,
        ConnectorSemanticCheck::Check => SemanticCheck::Check,
        ConnectorSemanticCheck::Test => SemanticCheck::Test,
    }
}

impl ConnectorRuntime {
    pub(super) async fn files_read(
        &self,
        arguments: Value,
        subject_id: &str,
        auth: &ConnectorCallContext,
        transport: ConnectorTransport,
        now: i64,
    ) -> ConnectorCallOutcome {
        let input: FilesReadInput = match parse_input("files_read", arguments) {
            Ok(input) => input,
            Err(outcome) => return outcome,
        };
        if input.files.is_empty() || input.files.len() > 8 {
            return invalid_input("files_read", "files must contain 1..=8 entries");
        }
        let task = match self.active_task(&input.task_id, subject_id) {
            Ok(task) => task,
            Err(outcome) => return outcome,
        };

        let mut results = Vec::with_capacity(input.files.len());
        for file in &input.files {
            if let Err(message) = validate_path(&file.path) {
                return invalid_input("files_read", message);
            }
            if file.limit.is_some_and(|limit| !(1..=500).contains(&limit)) {
                return invalid_input("files_read", "file limit must be 1..=500");
            }
            let args = json!({
                "project": task.execution_executor_ref,
                "items": [{
                    "path": file.path,
                    "start_line": file.start_line,
                    "limit": file.limit.unwrap_or(200)
                }],
                "with_line_numbers": file.with_line_numbers.unwrap_or(true),
                "max_result_bytes": 512 * 1024
            });
            match self
                .invoke_kernel("read_files", args, &task, auth, transport)
                .await
                .and_then(|output| Self::single_batch_item_output("read_files", output))
            {
                Ok(mut output) => {
                    output["path"] = json!(file.path);
                    results.push(output);
                }
                Err(error) => {
                    let cursor = self.record_event(
                        &task,
                        "files_read",
                        json!({ "ok": false, "requested": input.files.len(), "completed": results.len() }),
                        now,
                    );
                    return self.kernel_error_outcome(
                        error,
                        &task,
                        cursor,
                        json!({ "files": results }),
                    );
                }
            }
        }
        let cursor = match self.record_event(
            &task,
            "files_read",
            json!({ "ok": true, "file_count": results.len() }),
            now,
        ) {
            Ok(cursor) => cursor,
            Err(outcome) => return outcome,
        };
        ConnectorCallOutcome::success_at(&task, cursor, json!({ "files": results }))
    }

    /// Discovery: what does this project contain?
    ///
    /// Read-only and available in `read_only` tasks, which have no shell — so
    /// for those this is the only way to learn the project's shape instead of
    /// guessing paths for `files_read`.
    pub(super) async fn files_list(
        &self,
        arguments: Value,
        subject_id: &str,
        auth: &ConnectorCallContext,
        transport: ConnectorTransport,
        now: i64,
    ) -> ConnectorCallOutcome {
        let input: FilesListInput = match parse_input("files_list", arguments) {
            Ok(input) => input,
            Err(outcome) => return outcome,
        };
        if let Some(path) = input.path.as_deref() {
            if let Err(message) = validate_path(path) {
                return invalid_input("files_list", message);
            }
        }
        if input.globs.len() > 20 {
            return invalid_input("files_list", "globs are limited to 20 entries");
        }
        if input
            .globs
            .iter()
            .any(|glob| glob.is_empty() || glob.len() > 256)
        {
            return invalid_input("files_list", "each glob must be 1..=256 bytes");
        }
        if input
            .limit
            .is_some_and(|limit| !(1..=1000).contains(&limit))
        {
            return invalid_input("files_list", "limit must be 1..=1000");
        }
        if input.depth.is_some_and(|depth| !(1..=16).contains(&depth)) {
            return invalid_input("files_list", "depth must be 1..=16");
        }
        let task = match self.active_task(&input.task_id, subject_id) {
            Ok(task) => task,
            Err(outcome) => return outcome,
        };
        let args = json!({
            "project": task.execution_executor_ref,
            "path": input.path,
            "globs": input.globs,
            "depth": input.depth,
            "limit": input.limit.unwrap_or(200),
            "offset": input.offset.unwrap_or(0),
        });
        match self
            .invoke_kernel("list_project_tracked_files", args, &task, auth, transport)
            .await
        {
            Ok(output) => {
                let cursor = match self.record_event(
                    &task,
                    "files_list",
                    json!({ "ok": true, "returned": output.get("returned").cloned() }),
                    now,
                ) {
                    Ok(cursor) => cursor,
                    Err(outcome) => return outcome,
                };
                ConnectorCallOutcome::success_at(&task, cursor, output)
            }
            Err(error) => {
                let cursor = self.record_event(&task, "files_list", json!({ "ok": false }), now);
                self.kernel_error_outcome(error, &task, cursor, Value::Null)
            }
        }
    }

    pub(super) async fn files_search(
        &self,
        arguments: Value,
        subject_id: &str,
        auth: &ConnectorCallContext,
        transport: ConnectorTransport,
        now: i64,
    ) -> ConnectorCallOutcome {
        let input: FilesSearchInput = match parse_input("files_search", arguments) {
            Ok(input) => input,
            Err(outcome) => return outcome,
        };
        if input.pattern.trim().is_empty() || input.pattern.len() > 500 {
            return invalid_input("files_search", "pattern must be 1..=500 bytes");
        }
        if let Some(path) = input.path.as_deref() {
            if let Err(message) = validate_path(path) {
                return invalid_input("files_search", message);
            }
        }
        if input.limit.is_some_and(|limit| !(1..=100).contains(&limit)) {
            return invalid_input("files_search", "limit must be 1..=100");
        }
        if input.context_before.unwrap_or(0) > 5 || input.context_after.unwrap_or(0) > 5 {
            return invalid_input("files_search", "search context must be 0..=5 lines");
        }
        if input.include_globs.len() > 20 || input.exclude_globs.len() > 20 {
            return invalid_input(
                "files_search",
                "include/exclude globs are limited to 20 each",
            );
        }
        let task = match self.active_task(&input.task_id, subject_id) {
            Ok(task) => task,
            Err(outcome) => return outcome,
        };
        let page_limit = input.limit.unwrap_or(50);
        let signature = search_cursor_signature(&input, page_limit);
        let offset = match input.cursor.as_deref() {
            Some(cursor) => match parse_search_cursor(cursor, &signature) {
                Ok(offset) if offset < CONNECTOR_SEARCH_WINDOW => offset,
                _ => {
                    return invalid_input(
                        "files_search",
                        "cursor is invalid, belongs to a different query, or exceeds the bounded search window",
                    )
                }
            },
            None => 0,
        };
        let fetch_limit = offset
            .saturating_add(page_limit)
            .min(CONNECTOR_SEARCH_WINDOW);
        let args = json!({
            "project": task.execution_executor_ref,
            "queries": [{
                "pattern": input.pattern,
                "path": input.path,
                "limit": fetch_limit,
                "context_before": input.context_before.unwrap_or(0),
                "context_after": input.context_after.unwrap_or(0),
                "include_globs": input.include_globs,
                "exclude_globs": input.exclude_globs,
                "result_mode": input.result_mode.unwrap_or(SearchResultMode::Matches),
                "timeout_secs": 20
            }],
            "max_result_bytes": 512 * 1024
        });
        match self
            .invoke_kernel("search_project_texts", args, &task, auth, transport)
            .await
            .and_then(|output| Self::single_batch_item_output("search_project_texts", output))
        {
            Ok(output) => {
                let output = paginate_search_output(
                    output,
                    input.result_mode.unwrap_or(SearchResultMode::Matches),
                    offset,
                    page_limit,
                    &signature,
                );
                let cursor = match self.record_event(
                    &task,
                    "files_search",
                    json!({ "ok": true, "offset": offset, "limit": page_limit }),
                    now,
                ) {
                    Ok(cursor) => cursor,
                    Err(outcome) => return outcome,
                };
                ConnectorCallOutcome::success_at(&task, cursor, output)
            }
            Err(error) => {
                let cursor = self.record_event(&task, "files_search", json!({ "ok": false }), now);
                self.kernel_error_outcome(error, &task, cursor, Value::Null)
            }
        }
    }

    pub(super) async fn code_navigate(
        &self,
        arguments: Value,
        subject_id: &str,
        auth: &ConnectorCallContext,
        transport: ConnectorTransport,
        now: i64,
    ) -> ConnectorCallOutcome {
        // Preserve field presence before serde maps explicit JSON nulls to None.
        // Operation-specific fields are strict even when an irrelevant field is
        // supplied as null rather than omitted.
        let supplied_fields = arguments.as_object().map(|object| {
            [
                ("path", object.contains_key("path")),
                ("query", object.contains_key("query")),
                ("line", object.contains_key("line")),
                ("column", object.contains_key("column")),
                (
                    "include_declaration",
                    object.contains_key("include_declaration"),
                ),
                ("limit", object.contains_key("limit")),
            ]
        });
        let input: CodeNavigateInput = match parse_input("code_navigate", arguments) {
            Ok(input) => input,
            Err(outcome) => return outcome,
        };
        let allowed_fields: &[&str] = match input.operation {
            CodeNavigateOperation::Status => &[],
            CodeNavigateOperation::DocumentSymbols => &["path", "limit"],
            CodeNavigateOperation::WorkspaceSymbols => &["query", "limit"],
            CodeNavigateOperation::Definition => &["path", "line", "column", "limit"],
            CodeNavigateOperation::References => {
                &["path", "line", "column", "include_declaration", "limit"]
            }
            CodeNavigateOperation::Diagnostics => &["path", "limit"],
            CodeNavigateOperation::Hover => &["path", "line", "column"],
        };
        if let Some((field, _)) = supplied_fields
            .into_iter()
            .flatten()
            .find(|(field, present)| *present && !allowed_fields.contains(field))
        {
            return invalid_input(
                "code_navigate",
                format!(
                    "{field} is not valid for operation {}",
                    input.operation.as_str()
                ),
            );
        }
        let operation = input.operation.as_str();
        let (tool_name, mut args) = match code_navigation_tool_call(&input) {
            Ok(call) => call,
            Err(message) => return invalid_input("code_navigate", message),
        };
        let task = match self.active_task(&input.task_id, subject_id) {
            Ok(task) => task,
            Err(outcome) => return outcome,
        };
        args["project"] = json!(task.execution_executor_ref);
        match self
            .invoke_kernel(tool_name, args, &task, auth, transport)
            .await
        {
            Ok(output) => {
                let cursor = match self.record_event(
                    &task,
                    "code_navigate",
                    json!({ "ok": true, "operation": operation }),
                    now,
                ) {
                    Ok(cursor) => cursor,
                    Err(outcome) => return outcome,
                };
                ConnectorCallOutcome::success_at(&task, cursor, output)
            }
            Err(error) => {
                let cursor = self.record_event(
                    &task,
                    "code_navigate",
                    json!({ "ok": false, "operation": operation }),
                    now,
                );
                self.kernel_error_outcome(error, &task, cursor, Value::Null)
            }
        }
    }

    pub(super) async fn code_impact(
        &self,
        arguments: Value,
        subject_id: &str,
        auth: &ConnectorCallContext,
        transport: ConnectorTransport,
        now: i64,
    ) -> ConnectorCallOutcome {
        let input: CodeImpactInput = match parse_input("code_impact", arguments) {
            Ok(input) => input,
            Err(outcome) => return outcome,
        };
        if let Err(message) = validate_path(&input.path) {
            return invalid_input("code_impact", message);
        }
        if redact_absolute_paths(&input.path) != input.path {
            return invalid_input("code_impact", "path must be project-relative");
        }
        if input.line < 1 || input.column < 1 {
            return invalid_input("code_impact", "line and column must be >= 1");
        }
        if !(1..=2).contains(&input.depth) {
            return invalid_input("code_impact", "depth must be 1..=2");
        }
        if !(1..=100).contains(&input.limit) {
            return invalid_input("code_impact", "limit must be 1..=100");
        }
        let task = match self.active_task(&input.task_id, subject_id) {
            Ok(task) => task,
            Err(outcome) => return outcome,
        };
        let arguments = json!({
            "project": task.execution_executor_ref,
            "path": input.path,
            "line": input.line,
            "column": input.column,
            "direction": input.direction,
            "depth": input.depth,
            "limit": input.limit,
        });
        match self
            .invoke_kernel("call_hierarchy", arguments, &task, auth, transport)
            .await
        {
            Ok(output) => {
                let cursor = match self.record_event(
                    &task,
                    "code_impact",
                    json!({
                        "ok": true,
                        "direction": input.direction,
                        "depth": input.depth,
                    }),
                    now,
                ) {
                    Ok(cursor) => cursor,
                    Err(outcome) => return outcome,
                };
                ConnectorCallOutcome::success_at(&task, cursor, output)
            }
            Err(error) => {
                let cursor = self.record_event(
                    &task,
                    "code_impact",
                    json!({
                        "ok": false,
                        "direction": input.direction,
                        "depth": input.depth,
                    }),
                    now,
                );
                self.kernel_error_outcome(error, &task, cursor, Value::Null)
            }
        }
    }

    pub(super) async fn edits_apply(
        &self,
        arguments: Value,
        subject_id: &str,
        auth: &ConnectorCallContext,
        transport: ConnectorTransport,
        now: i64,
    ) -> ConnectorCallOutcome {
        let input: EditsApplyInput = match parse_input("edits_apply", arguments) {
            Ok(input) => input,
            Err(outcome) => return outcome,
        };
        if let Err(message) = validate_operation_id(&input.operation_id) {
            return invalid_input("edits_apply", message);
        }
        if input.changes.is_empty() || input.changes.len() > 16 {
            return invalid_input("edits_apply", "changes must contain 1..=16 entries");
        }
        for change in &input.changes {
            if let Err(message) = validate_path(&change.path) {
                return invalid_input("edits_apply", message);
            }
            if let Some(to_path) = change.to_path.as_deref() {
                if let Err(message) = validate_path(to_path) {
                    return invalid_input("edits_apply", message);
                }
            }
        }
        let change_bytes = serde_json::to_vec(&input.changes)
            .map(|bytes| bytes.len())
            .unwrap_or(usize::MAX);
        if change_bytes > 1024 * 1024 {
            return invalid_input("edits_apply", "serialized changes exceed 1 MiB");
        }
        #[cfg(test)]
        if let Some(entered) = self.mutation_before_task_lock.lock().unwrap().clone() {
            entered.add_permits(1);
        }
        let task_lock = self.task_lock(&input.task_id);
        let _task_guard = task_lock.lock().await;
        let task = match self.active_writable_task(&input.task_id, subject_id, "edits_apply", now) {
            Ok(task) => task,
            Err(outcome) => return outcome,
        };
        let request_sha256 =
            edit_operation_hash(&task, &input.changes, input.dry_run.unwrap_or(false));
        match self.db.begin_connector_edit_operation(
            &task.task_id,
            &self.context.project_id,
            &task.owner_subject_id,
            &input.operation_id,
            &request_sha256,
            now,
        ) {
            Ok(ConnectorEditOperationGate::Started) => {}
            Ok(ConnectorEditOperationGate::Replay(mut output)) => {
                output["operation_id"] = json!(input.operation_id);
                output["idempotent_replay"] = json!(true);
                let cursor = match self.record_event(
                    &task,
                    "edits_apply",
                    json!({ "ok": true, "replay": true, "operation_id": input.operation_id }),
                    now,
                ) {
                    Ok(cursor) => cursor,
                    Err(outcome) => return outcome,
                };
                return ConnectorCallOutcome::success_at(&task, cursor, output);
            }
            Ok(ConnectorEditOperationGate::Pending) => {
                let cursor = self.record_event(
                    &task,
                    "edits_apply",
                    json!({ "ok": false, "operation_pending": true, "operation_id": input.operation_id }),
                    now,
                );
                return ConnectorCallOutcome::error_for_task_at(
                    409,
                    "edit_operation_uncertain",
                    "this operation did not reach a durable result; it will not be replayed automatically",
                    false,
                    true,
                    Some("Inspect task_review and the affected files, then use a new operation_id with fresh hashes only if another edit is needed."),
                    &task,
                    match cursor { Ok(cursor) => cursor, Err(outcome) => return outcome },
                    json!({ "operation_id": input.operation_id }),
                );
            }
            Ok(ConnectorEditOperationGate::Conflict) => {
                return ConnectorCallOutcome::error_for_task(
                    409,
                    "operation_id_conflict",
                    "operation_id was already used with different changes or preconditions",
                    false,
                    false,
                    Some("Use a new operation_id for a logically different edit batch."),
                    &task,
                    json!({ "operation_id": input.operation_id }),
                )
            }
            Err(error) => return store_error_outcome(error, Some(&task)),
        }
        let args = json!({
            "project": task.execution_executor_ref,
            "changes": input.changes,
            "dry_run": input.dry_run.unwrap_or(false)
        });
        match self
            .invoke_kernel("apply_text_edits", args, &task, auth, transport)
            .await
        {
            Ok(mut output) => {
                output["operation_id"] = json!(input.operation_id);
                output["idempotent_replay"] = json!(false);
                if let Err(error) = self.db.complete_connector_edit_operation(
                    &task.task_id,
                    &self.context.project_id,
                    &task.owner_subject_id,
                    &input.operation_id,
                    &request_sha256,
                    &output,
                    now,
                ) {
                    return store_error_outcome(error, Some(&task));
                }
                // Paths are part of the durable event so review surfaces can
                // show what changed without a workspace scan (bounded by the
                // 16-change schema limit).
                let mut changed_paths: Vec<&str> = Vec::new();
                for change in &input.changes {
                    for path in [Some(change.path.as_str()), change.to_path.as_deref()]
                        .into_iter()
                        .flatten()
                    {
                        if !changed_paths.contains(&path) {
                            changed_paths.push(path);
                        }
                    }
                }
                let cursor = match self.record_event(
                    &task,
                    "edits_apply",
                    json!({
                        "ok": true,
                        "dry_run": input.dry_run.unwrap_or(false),
                        "operation_id": input.operation_id,
                        "change_count": input.changes.len(),
                        "changed_paths": changed_paths
                    }),
                    now,
                ) {
                    Ok(cursor) => cursor,
                    Err(outcome) => return outcome,
                };
                self.attach_pending_guidance(&task, &mut output);
                ConnectorCallOutcome::success_at(&task, cursor, output)
            }
            Err(error) => {
                let uncertain = kernel_failure_may_have_applied(&error);
                if !uncertain {
                    if let Err(store_error) = self.db.fail_connector_edit_operation(
                        &task.task_id,
                        &input.operation_id,
                        &request_sha256,
                        now,
                    ) {
                        return store_error_outcome(store_error, Some(&task));
                    }
                }
                let cursor = self.record_event(
                    &task,
                    "edits_apply",
                    json!({
                        "ok": false,
                        "dry_run": input.dry_run.unwrap_or(false),
                        "operation_id": input.operation_id,
                        "operation_uncertain": uncertain
                    }),
                    now,
                );
                if uncertain {
                    return ConnectorCallOutcome::error_for_task_at(
                        409,
                        "edit_operation_uncertain",
                        "the edit did not reach a confirmed completed or fully rolled-back state; automatic replay is disabled",
                        false,
                        true,
                        Some("Inspect task_review and affected files before issuing any new edit operation."),
                        &task,
                        match cursor { Ok(cursor) => cursor, Err(outcome) => return outcome },
                        json!({ "operation_id": input.operation_id }),
                    );
                }
                self.kernel_error_outcome(error, &task, cursor, Value::Null)
            }
        }
    }

    pub(super) async fn checks_run(
        &self,
        arguments: Value,
        subject_id: &str,
        auth: &ConnectorCallContext,
        _transport: ConnectorTransport,
        now: i64,
        defer_execution_guidance: bool,
    ) -> ConnectorCallOutcome {
        let input: ChecksRunInput = match parse_input("checks_run", arguments) {
            Ok(input) => input,
            Err(outcome) => return outcome,
        };
        if let Err(message) = validate_operation_id(&input.operation_id) {
            return invalid_input("checks_run", message);
        }
        if input.checks.is_empty() || input.checks.len() > 3 {
            return invalid_input("checks_run", "checks must contain 1..=3 entries");
        }
        let unique = input.checks.iter().copied().collect::<HashSet<_>>();
        if unique.len() != input.checks.len() {
            return invalid_input("checks_run", "checks must not contain duplicates");
        }
        if input
            .timeout_secs
            .is_some_and(|value| !(1..=120).contains(&value))
        {
            return invalid_input("checks_run", "timeout_secs must be 1..=120");
        }
        if let Some(cwd) = input.cwd.as_deref() {
            if let Err(message) = validate_path(cwd) {
                return invalid_input("checks_run", message);
            }
        }
        if input
            .test_filter
            .as_deref()
            .is_some_and(|filter| filter.len() > 500)
        {
            return invalid_input("checks_run", "test_filter must be at most 500 bytes");
        }
        #[cfg(test)]
        if let Some(entered) = self.mutation_before_task_lock.lock().unwrap().clone() {
            entered.add_permits(1);
        }
        let task_lock = self.task_lock(&input.task_id);
        let task_guard = task_lock.lock().await;
        let task = match self.active_executable_task(&input.task_id, subject_id, "checks_run", now)
        {
            Ok(task) => task,
            Err(outcome) => return outcome,
        };
        let host = auth.host.clone();
        let recipe = input.recipe.map(validation_recipe_id);
        let checks = input
            .checks
            .iter()
            .copied()
            .map(validation_semantic_check)
            .collect::<Vec<_>>();
        let resolved = match resolve_validation_recipe(
            Path::new(&task.execution_root),
            input.cwd.as_deref(),
            recipe,
            &checks,
            input.test_filter.as_deref(),
        ) {
            Ok(resolved) => resolved,
            Err(error) => return validation_recipe_error(&task, error),
        };
        let client_id = task
            .execution_executor_ref
            .strip_prefix("agent:")
            .and_then(|rest| rest.split_once(':'))
            .map(|(client_id, _)| client_id);
        let requires_go_test_json = resolved.steps.iter().any(|step| {
            step.name == "test" && step.program == "go" && step.args == ["test", "-json", "./..."]
        });
        let mut validation_steps = resolved.steps.clone();
        // Steer cargo at the shared cache outside the slot: reset uses
        // `git clean -ffdx`, which would otherwise wipe target/ and force a
        // cold build on every task.
        let shared_cargo_target = std::path::Path::new(&self.context.runs_root)
            .parent()
            .map(|state| state.join("cache/cargo-target"));
        if let Some(shared_cargo_target) = shared_cargo_target {
            for step in &mut validation_steps {
                if step.program == "cargo" {
                    step.env.push((
                        "CARGO_TARGET_DIR".to_string(),
                        shared_cargo_target.to_string_lossy().to_string(),
                    ));
                }
            }
        }
        let recipe_identity = resolved.durable_identity();
        let timeout_secs = input.timeout_secs.unwrap_or(120);
        let request_sha256 = check_request_hash(
            &task,
            &recipe_identity,
            input.cwd.as_deref(),
            resolved.test_filter.as_deref(),
            timeout_secs,
        );
        let existing = match self.db.latest_connector_execution(
            &task.task_id,
            &self.context.project_id,
            subject_id,
            Some(&input.operation_id),
        ) {
            Ok(Some(execution)) if execution.request_sha256 != request_sha256 => {
                return store_error_outcome(
                    ConnectorTaskStoreError::OperationIdConflict(input.operation_id),
                    Some(&task),
                )
            }
            Ok(execution) => execution.map(ConnectorExecutionReservation::Existing),
            Err(error) => return store_error_outcome(error, Some(&task)),
        };
        let plan = input
            .checks
            .iter()
            .map(|check| check.as_str().to_string())
            .collect::<Vec<_>>();
        let reservation = match existing {
            Some(existing) => existing,
            None => {
                let access = Some(auth.access.runner_access.clone());
                let supports_structured_validation = match client_id {
                    Some(client_id) => self
                        .runner_registry
                        .runner_supports_for_auth(
                            client_id,
                            RUNNER_CAPABILITY_STRUCTURED_VALIDATION_ARGV,
                            access.as_ref(),
                        )
                        .await
                        .unwrap_or(false),
                    None => false,
                };
                if !supports_structured_validation {
                    return ConnectorCallOutcome::error_for_task(
                        409,
                        "structured_validation_unavailable",
                        "the selected local Runner does not support structured validation jobs",
                        false,
                        true,
                        Some("Upgrade and reconnect the WebCodex Runner, then retry checks_run."),
                        &task,
                        json!({
                            "required_capability":
                                RUNNER_CAPABILITY_STRUCTURED_VALIDATION_ARGV
                        }),
                    );
                }
                if requires_go_test_json {
                    let supported = match client_id {
                        Some(client_id) => self
                            .runner_registry
                            .runner_supports_for_auth(
                                client_id,
                                RUNNER_CAPABILITY_STRUCTURED_GO_TEST_JSON,
                                access.as_ref(),
                            )
                            .await
                            .unwrap_or(false),
                        None => false,
                    };
                    if !supported {
                        return ConnectorCallOutcome::error_for_task(
                            409,
                            "structured_go_test_json_unavailable",
                            "the selected local Runner does not support machine-readable Go test validation",
                            false,
                            true,
                            Some("Upgrade and reconnect the WebCodex Runner, then retry checks_run."),
                            &task,
                            json!({
                                "required_capability":
                                    RUNNER_CAPABILITY_STRUCTURED_GO_TEST_JSON
                            }),
                        );
                    }
                }
                let check_workspace_sha256 =
                    match self.workspace_fingerprint(&task, "checks_run").await {
                        Ok(fingerprint) => fingerprint,
                        Err(outcome) => return outcome,
                    };
                match self.executions.reserve(
                    &task,
                    "check",
                    &input.operation_id,
                    &request_sha256,
                    &plan,
                    Some(&recipe_identity),
                    Some(&check_workspace_sha256),
                    timeout_secs,
                    now,
                ) {
                    Ok(reservation) => reservation,
                    Err(error) => return store_error_outcome(error, Some(&task)),
                }
            }
        };
        let execution_cwd = Path::new(&task.execution_root)
            .join(&resolved.recipe_root_relative)
            .to_string_lossy()
            .into_owned();
        drop(task_guard);
        self.execution_outcome(
            self.executions
                .execute(
                    reservation,
                    task.clone(),
                    "structured validation".to_string(),
                    Some(execution_cwd),
                    timeout_secs,
                    host,
                    auth.access.runner_access.clone(),
                    validation_steps,
                )
                .await,
            &task,
            auth,
            defer_execution_guidance,
        )
        .await
    }

    pub(super) async fn commands_run(
        &self,
        arguments: Value,
        subject_id: &str,
        auth: &ConnectorCallContext,
        _transport: ConnectorTransport,
        now: i64,
        defer_execution_guidance: bool,
    ) -> ConnectorCallOutcome {
        let input: CommandsRunInput = match parse_input("commands_run", arguments) {
            Ok(input) => input,
            Err(outcome) => return outcome,
        };
        if let Err(message) = validate_operation_id(&input.operation_id) {
            return invalid_input("commands_run", message);
        }
        if input.command.trim().is_empty() || input.command.len() > 32768 {
            return invalid_input("commands_run", "command must be 1..=32768 bytes");
        }
        if input
            .timeout_secs
            .is_some_and(|value| !(1..=120).contains(&value))
        {
            return invalid_input("commands_run", "timeout_secs must be 1..=120");
        }
        if let Some(cwd) = input.cwd.as_deref() {
            if let Err(message) = validate_path(cwd) {
                return invalid_input("commands_run", message);
            }
        }
        #[cfg(test)]
        if let Some(entered) = self.mutation_before_task_lock.lock().unwrap().clone() {
            entered.add_permits(1);
        }
        let task_lock = self.task_lock(&input.task_id);
        let task_guard = task_lock.lock().await;
        let task =
            match self.active_executable_task(&input.task_id, subject_id, "commands_run", now) {
                Ok(task) => task,
                Err(outcome) => return outcome,
            };
        let timeout_secs = input.timeout_secs.unwrap_or(120);
        let request_sha256 =
            command_request_hash(&task, &input.command, input.cwd.as_deref(), timeout_secs);
        let existing = match self.db.latest_connector_execution(
            &task.task_id,
            &self.context.project_id,
            subject_id,
            Some(&input.operation_id),
        ) {
            Ok(Some(execution)) if execution.request_sha256 != request_sha256 => {
                return store_error_outcome(
                    ConnectorTaskStoreError::OperationIdConflict(input.operation_id),
                    Some(&task),
                )
            }
            Ok(execution) => execution.map(ConnectorExecutionReservation::Existing),
            Err(error) => return store_error_outcome(error, Some(&task)),
        };
        let reservation = match existing {
            Some(existing) => existing,
            None => {
                let manager = self.workspace.clone();
                let task_for_precondition = task.clone();
                let precondition = match tokio::task::spawn_blocking(move || {
                    manager.action_precondition(&task_for_precondition)
                })
                .await
                {
                    Ok(Ok(precondition)) => precondition,
                    Ok(Err(message)) => {
                        let cursor = self.record_event(
                            &task,
                            "commands_run",
                            json!({ "ok": false, "stage": "approval_precondition" }),
                            now,
                        );
                        let cursor = cursor.unwrap_or(task.event_cursor);
                        return ConnectorCallOutcome::error_for_task_at(
                            409,
                            "approval_precondition_failed",
                            self.sanitize_task_string(&task, &message),
                            false,
                            true,
                            Some("Resolve the Git workspace issue, then retry."),
                            &task,
                            cursor,
                            Value::Null,
                        );
                    }
                    Err(error) => {
                        tracing::error!(error = %error, "connector approval precondition task failed");
                        return ConnectorCallOutcome::error_for_task(
                            500,
                            "approval_precondition_failed",
                            "connector could not capture the command precondition",
                            false,
                            true,
                            Some("Inspect server logs before retrying the command request."),
                            &task,
                            Value::Null,
                        );
                    }
                };
                let action_hash = command_action_hash(&request_sha256, &precondition);
                // The human decides on this summary: it must show what runs.
                // The preview is first-line/120-char bounded, never the full
                // command body.
                let action_summary = format!(
                    "raw project command ({} bytes{}, workspace {}): {}",
                    input.command.len(),
                    input
                        .cwd
                        .as_deref()
                        .map(|cwd| format!(", cwd {cwd}"))
                        .unwrap_or_default(),
                    short_oid(&precondition),
                    command_preview(&input.command)
                );
                let authority = auth.execution_authority.clone();
                if authority.auto_authorize {
                    // Trusted agent authority: no human approval interruption
                    // and no pending approval record. The auto-authorization is
                    // still a durable audit fact on the task event stream.
                    let _ = self.record_event(
                        &task,
                        "authority_auto_authorized",
                        json!({
                            "action_kind": "commands_run",
                            "action_hash": action_hash,
                            "action_summary": action_summary,
                            "authority_mode": authority.mode,
                            "authority_source": authority.source,
                            "resolved_rule": authority.resolved_rule,
                            "risk": "shell",
                            "principal": subject_id,
                            "project": self.context.project_id,
                        }),
                        now,
                    );
                } else {
                    let gate = match self.db.request_or_consume_connector_approval(
                        &task.task_id,
                        &self.context.project_id,
                        subject_id,
                        "commands_run",
                        &action_hash,
                        &action_summary,
                        now,
                        now + COMMAND_APPROVAL_TTL_SECS,
                    ) {
                        Ok(gate) => gate,
                        Err(error) => return store_error_outcome(error, Some(&task)),
                    };
                    if !matches!(&gate, ConnectorApprovalGate::Authorized(_)) {
                        let current = self.task(&task.task_id, subject_id).unwrap_or(task);
                        return approval_gate_outcome(gate, &current);
                    }
                }
                match self.executions.reserve(
                    &task,
                    "command",
                    &input.operation_id,
                    &request_sha256,
                    &[],
                    None,
                    None,
                    timeout_secs,
                    chrono::Utc::now().timestamp(),
                ) {
                    Ok(reservation) => reservation,
                    Err(error) => return store_error_outcome(error, Some(&task)),
                }
            }
        };
        drop(task_guard);
        self.execution_outcome(
            self.executions
                .execute(
                    reservation,
                    task.clone(),
                    input.command,
                    input.cwd,
                    timeout_secs,
                    auth.host.clone(),
                    auth.access.runner_access.clone(),
                    Vec::new(),
                )
                .await,
            &task,
            auth,
            defer_execution_guidance,
        )
        .await
    }

    async fn execution_outcome(
        &self,
        result: Result<webcodex_store::ConnectorExecution, ConnectorTaskStoreError>,
        task: &ConnectorTaskSnapshot,
        auth: &ConnectorCallContext,
        defer_execution_guidance: bool,
    ) -> ConnectorCallOutcome {
        let current = self
            .task(&task.task_id, &task.owner_subject_id)
            .unwrap_or_else(|_| task.clone());
        match result {
            Ok(execution) => {
                let runner_access = auth.access.runner_access.clone();
                let projection = self
                    .executions
                    .projection(&execution, &runner_access, true)
                    .await;
                let mut data = json!({ "execution": projection });
                if !defer_execution_guidance {
                    self.attach_pending_guidance(&current, &mut data);
                }
                ConnectorCallOutcome::success_blocking_at(
                    &current,
                    current.event_cursor,
                    data,
                    execution.blocks_finish(),
                )
            }
            Err(error) => store_error_outcome(error, Some(&current)),
        }
    }

    fn single_batch_item_output(tool_name: &str, output: Value) -> Result<Value, KernelFailure> {
        let Some(items) = output.get("items").and_then(Value::as_array) else {
            return Err(KernelFailure::Adapter(format!(
                "{tool_name} returned a malformed batch result without items"
            )));
        };
        if items.len() != 1 {
            return Err(KernelFailure::Adapter(format!(
                "{tool_name} returned {} items for a one-item connector request",
                items.len()
            )));
        }
        let item = &items[0];
        match item.get("success").and_then(Value::as_bool) {
            Some(true) => item.get("output").cloned().ok_or_else(|| {
                KernelFailure::Adapter(format!(
                    "{tool_name} returned a successful item without output"
                ))
            }),
            Some(false) => Err(KernelFailure::Tool {
                error: item
                    .get("error")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                output: item.get("output").cloned().unwrap_or(Value::Null),
            }),
            None => Err(KernelFailure::Adapter(format!(
                "{tool_name} returned an item without a boolean success field"
            ))),
        }
    }

    pub(super) async fn invoke_kernel(
        &self,
        tool_name: &str,
        arguments: Value,
        task: &ConnectorTaskSnapshot,
        auth: &ConnectorCallContext,
        transport: ConnectorTransport,
    ) -> Result<Value, KernelFailure> {
        let host = auth.host.clone();
        match host
            .invoke_tool(ConnectorToolRequest {
                tool_name: tool_name.to_string(),
                arguments,
                transport: transport.into(),
            })
            .await
        {
            Ok(output) => Ok(self.sanitize_task_value(task, output)),
            Err(ConnectorToolFailure::Permission { required, message }) => {
                Err(KernelFailure::Scope {
                    required_permission: required,
                    message,
                })
            }
            Err(ConnectorToolFailure::InvalidArguments(message))
            | Err(ConnectorToolFailure::Adapter(message)) => Err(KernelFailure::Adapter(message)),
            Err(ConnectorToolFailure::Tool { error, output }) => {
                Err(KernelFailure::Tool { error, output })
            }
        }
    }

    fn kernel_error_outcome(
        &self,
        error: KernelFailure,
        task: &ConnectorTaskSnapshot,
        cursor: Result<i64, ConnectorCallOutcome>,
        partial_data: Value,
    ) -> ConnectorCallOutcome {
        let cursor = match cursor {
            Ok(cursor) => cursor,
            Err(outcome) => return outcome,
        };
        match error {
            KernelFailure::Scope {
                required_permission,
                message,
            } => ConnectorCallOutcome::error_for_task_at_with_scope(
                403,
                "insufficient_scope",
                message,
                false,
                true,
                Some(
                    "Grant the required connector scope and retry only after checking task_review.",
                ),
                task,
                cursor,
                partial_data,
                required_permission,
            ),
            KernelFailure::Adapter(message) => ConnectorCallOutcome::error_for_task_at(
                500,
                "connector_adapter_error",
                format!(
                    "connector could not translate the capability: {}",
                    self.sanitize_task_string(task, &message)
                ),
                false,
                true,
                Some("Inspect server logs; do not retry a consequential call automatically."),
                task,
                cursor,
                partial_data,
            ),
            KernelFailure::Tool { error, output } => {
                let message = error
                    .as_deref()
                    .map(|message| self.sanitize_task_string(task, message))
                    .unwrap_or_else(|| "executor rejected the capability".to_string());
                let output = self.sanitize_task_value(task, output);
                ConnectorCallOutcome::error_for_task_at(
                    400,
                    "capability_failed",
                    message,
                    false,
                    false,
                    Some("Use the returned diagnostics, inspect if needed, then retry with a corrected call."),
                    task,
                    cursor,
                    json!({ "partial": partial_data, "executor": output }),
                )
            }
        }
    }

    pub(super) fn sanitize_task_value(
        &self,
        task: &ConnectorTaskSnapshot,
        mut value: Value,
    ) -> Value {
        sanitize_value(
            &mut value,
            &task.execution_executor_ref,
            &self.context.project_id,
            &task.execution_root,
        );
        if let Some(client_id) = executor_client_id(&task.execution_executor_ref) {
            replace_string_material(&mut value, client_id, "<agent>");
        }
        value
    }

    pub(super) fn sanitize_task_string(&self, task: &ConnectorTaskSnapshot, value: &str) -> String {
        let value = value
            .replace(&task.execution_executor_ref, &self.context.project_id)
            .replace(&task.execution_root, ".")
            .replace(&self.context.runs_root, "<managed-runs>")
            .replace(&self.context.results_root, "<managed-results>")
            .replace(
                &self.context.project_registry_dir,
                "<managed-project-registry>",
            );
        match executor_client_id(&task.execution_executor_ref) {
            Some(client_id) => value.replace(client_id, "<agent>"),
            None => value,
        }
    }
}

fn executor_client_id(executor_ref: &str) -> Option<&str> {
    let (client_id, project_id) = executor_ref.strip_prefix("agent:")?.split_once(':')?;
    (!client_id.is_empty() && !project_id.is_empty()).then_some(client_id)
}

fn replace_string_material(value: &mut Value, needle: &str, replacement: &str) {
    match value {
        Value::String(string) => {
            if string.contains(needle) {
                *string = string.replace(needle, replacement);
            }
        }
        Value::Array(items) => {
            for item in items {
                replace_string_material(item, needle, replacement);
            }
        }
        Value::Object(object) => {
            for item in object.values_mut() {
                replace_string_material(item, needle, replacement);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn code_navigation_tool_call(input: &CodeNavigateInput) -> Result<(&'static str, Value), String> {
    let irrelevant = |fields: &[(&str, bool)]| {
        fields
            .iter()
            .find_map(|(name, present)| present.then_some(*name))
            .map(|name| {
                format!(
                    "{name} is not valid for operation {}",
                    input.operation.as_str()
                )
            })
    };
    let require_path = || -> Result<&str, String> {
        let path = input.path.as_deref().ok_or_else(|| {
            format!(
                "path is required for operation {}",
                input.operation.as_str()
            )
        })?;
        validate_path(path).map_err(str::to_string)?;
        if redact_absolute_paths(path) != path {
            return Err("path must be project-relative".to_string());
        }
        Ok(path)
    };
    let require_position = || -> Result<(usize, usize), String> {
        let line = input.line.ok_or_else(|| {
            format!(
                "line is required for operation {}",
                input.operation.as_str()
            )
        })?;
        let column = input.column.ok_or_else(|| {
            format!(
                "column is required for operation {}",
                input.operation.as_str()
            )
        })?;
        if line < 1 || column < 1 {
            return Err("line and column must be >= 1".to_string());
        }
        Ok((line, column))
    };
    let validate_limit = |maximum: usize| -> Result<(), String> {
        if input
            .limit
            .is_some_and(|limit| !(1..=maximum).contains(&limit))
        {
            return Err(format!(
                "limit for operation {} must be 1..={maximum}",
                input.operation.as_str()
            ));
        }
        Ok(())
    };

    match input.operation {
        CodeNavigateOperation::Status => {
            if let Some(message) = irrelevant(&[
                ("path", input.path.is_some()),
                ("query", input.query.is_some()),
                ("line", input.line.is_some()),
                ("column", input.column.is_some()),
                ("include_declaration", input.include_declaration.is_some()),
                ("limit", input.limit.is_some()),
            ]) {
                return Err(message);
            }
            Ok(("lsp_status", json!({})))
        }
        CodeNavigateOperation::DocumentSymbols => {
            if let Some(message) = irrelevant(&[
                ("query", input.query.is_some()),
                ("line", input.line.is_some()),
                ("column", input.column.is_some()),
                ("include_declaration", input.include_declaration.is_some()),
            ]) {
                return Err(message);
            }
            let path = require_path()?;
            validate_limit(MAX_DOCUMENT_SYMBOLS_LIMIT)?;
            Ok((
                "document_symbols",
                json!({ "path": path, "limit": input.limit }),
            ))
        }
        CodeNavigateOperation::WorkspaceSymbols => {
            if let Some(message) = irrelevant(&[
                ("path", input.path.is_some()),
                ("line", input.line.is_some()),
                ("column", input.column.is_some()),
                ("include_declaration", input.include_declaration.is_some()),
            ]) {
                return Err(message);
            }
            let query = input.query.as_deref().unwrap_or_default().trim();
            if query.is_empty() || query.chars().count() > 200 {
                return Err(
                    "query for operation workspace_symbols must contain 1..=200 non-whitespace characters"
                        .to_string(),
                );
            }
            if redact_absolute_paths(query) != query {
                return Err(
                    "query for operation workspace_symbols must not contain absolute path material"
                        .to_string(),
                );
            }
            validate_limit(MAX_WORKSPACE_SYMBOLS_LIMIT)?;
            Ok((
                "workspace_symbols",
                json!({ "query": query, "limit": input.limit }),
            ))
        }
        CodeNavigateOperation::Definition => {
            if let Some(message) = irrelevant(&[
                ("query", input.query.is_some()),
                ("include_declaration", input.include_declaration.is_some()),
            ]) {
                return Err(message);
            }
            let path = require_path()?;
            let (line, column) = require_position()?;
            validate_limit(MAX_GOTO_DEFINITION_LIMIT)?;
            Ok((
                "goto_definition",
                json!({
                    "path": path,
                    "line": line,
                    "column": column,
                    "limit": input.limit
                }),
            ))
        }
        CodeNavigateOperation::References => {
            if let Some(message) = irrelevant(&[("query", input.query.is_some())]) {
                return Err(message);
            }
            let path = require_path()?;
            let (line, column) = require_position()?;
            validate_limit(MAX_FIND_REFERENCES_LIMIT)?;
            Ok((
                "find_references",
                json!({
                    "path": path,
                    "line": line,
                    "column": column,
                    "include_declaration": input.include_declaration.unwrap_or(true),
                    "limit": input.limit
                }),
            ))
        }
        CodeNavigateOperation::Diagnostics => {
            if let Some(message) = irrelevant(&[
                ("query", input.query.is_some()),
                ("line", input.line.is_some()),
                ("column", input.column.is_some()),
                ("include_declaration", input.include_declaration.is_some()),
            ]) {
                return Err(message);
            }
            let path = require_path()?;
            validate_limit(MAX_DOCUMENT_DIAGNOSTICS_LIMIT)?;
            Ok((
                "document_diagnostics",
                json!({ "path": path, "limit": input.limit }),
            ))
        }
        CodeNavigateOperation::Hover => {
            if let Some(message) = irrelevant(&[
                ("query", input.query.is_some()),
                ("include_declaration", input.include_declaration.is_some()),
                ("limit", input.limit.is_some()),
            ]) {
                return Err(message);
            }
            let path = require_path()?;
            let (line, column) = require_position()?;
            Ok((
                "hover",
                json!({ "path": path, "line": line, "column": column }),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_batch_item_output_preserves_canonical_runtime_envelope() {
        let success = json!({
            "items": [{
                "index": 0,
                "success": true,
                "output": { "text": "ready" }
            }]
        });
        assert_eq!(
            ConnectorRuntime::single_batch_item_output("read_files", success).unwrap(),
            json!({ "text": "ready" })
        );

        let failure = json!({
            "items": [{
                "index": 0,
                "success": false,
                "error": "read failed",
                "output": { "reason_code": "not_found" }
            }]
        });
        match ConnectorRuntime::single_batch_item_output("read_files", failure).unwrap_err() {
            KernelFailure::Tool { error, output } => {
                assert_eq!(error.as_deref(), Some("read failed"));
                assert_eq!(output, json!({ "reason_code": "not_found" }));
            }
            other => panic!("expected tool failure, got {other:?}"),
        }
    }
}
