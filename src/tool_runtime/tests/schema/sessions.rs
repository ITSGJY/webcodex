use super::*;

#[test]
fn session_tool_specs_describe_ledger_vs_current_binding() {
    let specs = registered_tool_specs();

    let desc = |name: &str| spec_named(&specs, name).description.to_lowercase();

    // `start_session` and `bind_current_session` are ModelHidden: the model
    // coding line is covered by `start_coding_task` (which creates a workflow
    // session and binds via `resume_session_id` + `bind_current`), and
    // `current_session` is the query-only view of the active binding. These
    // hidden tools keep no public ToolSpec/description; their parser/dispatch
    // and low-level behavior are covered by implementation-level tests.

    let summary_desc = desc("session_summary");
    for phrase in [
        "session ledger",
        "explicit session_id",
        "does not rely on current-session binding",
    ] {
        assert!(
            summary_desc.contains(phrase),
            "session_summary description should mention {phrase}: {summary_desc}"
        );
    }

    let update = spec_named(&specs, "update_session_context");
    assert_eq!(
        update.input_schema["required"],
        serde_json::json!(["session_id", "execution_context"])
    );
    assert_eq!(update.input_schema["additionalProperties"], false);
    assert_eq!(
        update.input_schema["properties"]["execution_context"]["additionalProperties"],
        false
    );
    let update_desc = update.description.to_lowercase();
    for phrase in [
        "atomically replace",
        "active project-scoped",
        "never falls back",
        "never creates",
    ] {
        assert!(
            update_desc.contains(phrase),
            "update_session_context description should mention {phrase}: {update_desc}"
        );
    }

    let handoff_desc = desc("session_handoff_summary");
    for phrase in [
        "session ledger",
        "explicit session_id",
        "ledger-derived validation",
        "bounded tails",
        "safe result metadata",
        "validation.parser.available",
        "does not depend on current-session binding",
    ] {
        assert!(
            handoff_desc.contains(phrase),
            "session_handoff_summary description should mention {phrase}: {handoff_desc}"
        );
    }

    for name in ["current_session", "unbind_current_session"] {
        let current_desc = desc(name);
        for phrase in ["process-local", "hashed durable"] {
            assert!(
                current_desc.contains(phrase),
                "{name} description should mention {phrase}: {current_desc}"
            );
        }
    }

    let current_desc = desc("current_session");
    assert!(
        current_desc.contains("after restart"),
        "current_session description should mention exact restart recovery: {current_desc}"
    );
    assert!(desc("unbind_current_session").contains("keeps workflow session history intact"));
}
