use super::super::input_schemas::{
    apply_patch_checked_input_schema, apply_patch_input_schema, apply_text_edits_input_schema,
    validate_patch_input_schema, write_project_file_input_schema,
};
use super::tool_spec;
use crate::tool_runtime::tool_spec::ToolSpec;

pub(super) fn tool_specs() -> Vec<ToolSpec> {
    vec![
        tool_spec(
            "apply_patch",
            "Advanced/raw unified-diff apply. Prefer apply_patch_checked for new workflows; this lower-level path does not provide the full checked preflight and diff_summary package.".to_string(),
            apply_patch_input_schema(),
        ),
        tool_spec(
            "apply_patch_checked",
            "Canonical checked patch tool for complex multi-file unified diffs. Runs patch preflight first and applies only when validation passes. Prefer over raw apply_patch; for ordinary local edits prefer apply_text_edits.",
            apply_patch_checked_input_schema(),
        ),
        tool_spec(
            "validate_patch",
            "Dry-run a unified diff with git apply --check/--stat through the owning agent; never writes files.",
            validate_patch_input_schema(),
        ),
        tool_spec(
            "write_project_file",
            "Create new files or intentional whole-file rewrites. Not preferred for ordinary local edits—prefer apply_text_edits. Inspect current content and worktree changes before overwriting; do not silently clobber user edits.",
            write_project_file_input_schema(),
        ),
        tool_spec(
            "apply_text_edits",
            "Canonical transactional file-change tool; preferred for ordinary local edits. Applies edit/create/delete/rename changes to current worktree, not HEAD. SHA-256 guards preflight the whole batch before mutation. Prefer over whole-file tools. Supports dry_run and per-file hashes.",
            apply_text_edits_input_schema(),
        ),
    ]
}
