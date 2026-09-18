use super::*;

fn source(scope: InstructionSourceScope, path: &str, body: &str) -> ProjectInstructionsSnapshot {
    ProjectInstructionsSnapshot::from_candidates(
        vec![LoadedInstructionCandidate {
            source_scope: scope,
            path: path.into(),
            content: body.into(),
            total_lines: body.lines().count(),
            full_sha256: None,
        }],
        true,
    )
}

#[test]
fn global_character_budget_cannot_erase_project_guidance() {
    let runner = source(
        InstructionSourceScope::Runner,
        "runner/0/rules.md",
        &"x".repeat(MAX_TOTAL_CHARS),
    );
    let project = source(
        InstructionSourceScope::Project,
        "AGENTS.md",
        "local guidance\nsecond line",
    );
    let expected = project.files[0].clone();
    let combined = ProjectInstructionsSnapshot::with_runner_files(runner.files, project, true);
    assert_eq!(
        combined.files[0].source_scope,
        InstructionSourceScope::Runner
    );
    assert!(combined.files[0].truncated);
    assert!(combined.files[0].read_more.is_none());
    assert_eq!(combined.files[1].content, expected.content);
    assert_eq!(combined.files[1].fingerprint, expected.fingerprint);
    assert!(!combined.files[1].truncated);
    assert!(combined.total_chars <= MAX_TOTAL_CHARS);
}

#[test]
fn composing_runner_sources_preserves_project_continuation() {
    let body = "local\n".repeat(MAX_LINES_PER_FILE + 1);
    let project = source(InstructionSourceScope::Project, "AGENTS.md", &body);
    let original_hint = project.files[0].read_more.as_ref().unwrap().start_line;
    let runner = source(
        InstructionSourceScope::Runner,
        "runner/0/rules.md",
        &"global\n".repeat(400),
    );
    let combined = ProjectInstructionsSnapshot::with_runner_files(runner.files, project, true);
    assert_eq!(
        combined.files[1].read_more.as_ref().unwrap().start_line,
        original_hint
    );
    assert!(combined.files[1].truncated);
    assert!(combined.files[0].read_more.is_none());
    assert_eq!(
        combined.files[1].content.lines().count(),
        MAX_LINES_PER_FILE
    );
}
