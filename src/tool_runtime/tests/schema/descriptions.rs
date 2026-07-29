use super::*;

#[test]
fn tool_specs_describe_default_coding_loop_preferences() {
    let specs = registered_tool_specs();

    let desc = |name: &str| spec_named(&specs, name).description.to_lowercase();

    let read_file_desc = desc("read_file");
    for phrase in [
        "default inspect tool",
        "targeted source reading",
        "line numbers",
    ] {
        assert!(
            read_file_desc.contains(phrase),
            "read_file description should mention {phrase}: {read_file_desc}"
        );
    }

    let search_desc = desc("search_project_text");
    for phrase in [
        "default inspect/search tool",
        "rg-first",
        "grep fallback",
        "structured output",
        "matches",
        "context",
        "backend",
        "truncated",
    ] {
        assert!(
            search_desc.contains(phrase),
            "search_project_text description should mention {phrase}: {search_desc}"
        );
    }

    let show_changes_desc = desc("show_changes");
    for phrase in [
        "default inspect/review tool",
        "before final response",
        "bounded hunks",
    ] {
        assert!(
            show_changes_desc.contains(phrase),
            "show_changes description should mention {phrase}: {show_changes_desc}"
        );
    }

    // Canonical transactional edit path.
    let apply_text_edits_desc = desc("apply_text_edits");
    for phrase in [
        "canonical transactional file-change",
        "preferred for ordinary local",
        "current worktree",
        "not head",
        "edit/create/delete/rename",
        "whole batch",
        "prefer over whole-file",
        "compatibility edit",
        "dry_run",
        "per-file hashes",
    ] {
        assert!(
            apply_text_edits_desc.contains(phrase),
            "apply_text_edits description should mention {phrase}: {apply_text_edits_desc}"
        );
    }

    // Canonical checked patch path.
    let apply_patch_checked_desc = desc("apply_patch_checked");
    for phrase in [
        "canonical checked patch",
        "multi-file",
        "preflight",
        "prefer over raw apply_patch",
    ] {
        assert!(
            apply_patch_checked_desc.contains(phrase),
            "apply_patch_checked description should mention {phrase}: {apply_patch_checked_desc}"
        );
    }

    // Advanced/raw apply_patch.
    let apply_patch_desc = desc("apply_patch");
    for phrase in ["advanced/raw", "prefer apply_patch_checked"] {
        assert!(
            apply_patch_desc.contains(phrase),
            "apply_patch description should mention {phrase}: {apply_patch_desc}"
        );
    }

    // Whole-file write is not the ordinary local-edit default.
    let write_file_desc = desc("write_project_file");
    for phrase in [
        "create new files",
        "whole-file",
        "not preferred for ordinary local",
        "prefer apply_text_edits",
        "do not silently clobber",
    ] {
        assert!(
            write_file_desc.contains(phrase),
            "write_project_file description should mention {phrase}: {write_file_desc}"
        );
    }

    // The compatibility line/pattern tools (replace_line_range,
    // insert_at_line, delete_line_range, replace_in_file,
    // replace_exact_block, insert_before_pattern, insert_after_pattern)
    // are ModelHidden: still parser-known and dispatched for back-compat,
    // but no longer carry a model-facing ToolSpec/description. Their hidden
    // status and continued parseability are asserted by
    // `anchor_edit_tools_are_hidden_from_model_surface` and the parser-name
    // gate in `tool_call_parser_name_gate_matches_tool_definitions`.

    for name in ["cargo_check", "cargo_test"] {
        let validation_desc = desc(name);
        assert!(
            validation_desc.contains("preferred structured"),
            "{name} should be described as preferred structured validation: {validation_desc}"
        );
        assert!(
            validation_desc.contains("before raw run_shell"),
            "{name} should steer callers away from raw run_shell first: {validation_desc}"
        );
    }

    let workspace_hygiene_desc = desc("workspace_hygiene_check");
    for phrase in ["pre-final", "workspace hygiene", "read-only"] {
        assert!(
            workspace_hygiene_desc.contains(phrase),
            "workspace_hygiene_check description should mention {phrase}: {workspace_hygiene_desc}"
        );
    }

    let handoff_desc = desc("session_handoff_summary");
    for phrase in ["handoff", "multi-step tasks", "read-only"] {
        assert!(
            handoff_desc.contains(phrase),
            "session_handoff_summary description should mention {phrase}: {handoff_desc}"
        );
    }

    let run_shell_desc = desc("run_shell");
    for phrase in [
        "bounded command escape hatch",
        "validation",
        "diagnostics",
        "do not use as the primary file editing path",
    ] {
        assert!(
            run_shell_desc.contains(phrase),
            "run_shell description should mention {phrase}: {run_shell_desc}"
        );
    }
}

#[test]
fn anchor_edit_tools_are_hidden_from_model_surface() {
    use crate::tool_runtime::tool_definition::is_model_hidden_tool_name;

    let specs = registered_tool_specs();
    let spec_names: std::collections::BTreeSet<&str> =
        specs.iter().map(|s| s.name.as_str()).collect();
    for hidden in [
        "replace_exact_block",
        "insert_before_pattern",
        "insert_after_pattern",
        "replace_in_file",
        "replace_line_range",
        "insert_at_line",
        "delete_line_range",
    ] {
        assert!(
            is_model_hidden_tool_name(hidden),
            "{hidden} must be ModelHidden (canonical apply_text_edits covers it)"
        );
        assert!(
            !spec_names.contains(hidden),
            "{hidden} must not keep a model-facing ToolSpec"
        );
    }
}

#[test]
fn edit_tool_surface_keeps_canonical_tools_visible_and_schemas_stable() {
    let specs = registered_tool_specs();
    let names: std::collections::BTreeSet<&str> =
        specs.iter().map(|spec| spec.name.as_str()).collect();

    for required in [
        "apply_text_edits",
        "apply_patch_checked",
        "apply_patch",
        "write_project_file",
    ] {
        assert!(
            names.contains(required),
            "edit surface must keep {required} model-visible"
        );
        let spec = spec_named(&specs, required);
        assert!(
            spec.input_schema.is_object(),
            "{required} must keep an object input schema"
        );
        assert!(
            !spec.input_schema.as_object().unwrap().is_empty(),
            "{required} input schema must not be empty"
        );
    }

    // Parameter surface smoke checks (names only; not full-schema snapshots).
    let text_edits = &spec_named(&specs, "apply_text_edits").input_schema["properties"];
    for field in ["project", "changes", "dry_run"] {
        assert!(
            text_edits.get(field).is_some(),
            "apply_text_edits must keep field {field}"
        );
    }
    let patch_checked = &spec_named(&specs, "apply_patch_checked").input_schema["properties"];
    for field in ["project", "patch", "deny_sensitive_paths"] {
        assert!(
            patch_checked.get(field).is_some(),
            "apply_patch_checked must keep field {field}"
        );
    }
    let write_file = &spec_named(&specs, "write_project_file").input_schema["properties"];
    for field in ["project", "path", "content"] {
        assert!(
            write_file.get(field).is_some(),
            "write_project_file must keep field {field}"
        );
    }
}
