use super::*;

#[test]
fn read_project_artifact_rejects_retired_max_bytes_alias() {
    let error = ToolCall::from_tool_name(
        "read_project_artifact",
        json!({"project": "agent:test:demo", "path": "artifact.bin", "max_bytes": 32}),
    )
    .unwrap_err();
    assert!(error.contains("max_bytes"));
    assert!(error.contains("no longer supported"));
    assert!(error.contains("length"));
}

#[test]
fn read_project_artifact_expected_sha256_parser_requires_lowercase_64_hex() {
    let digest = "a".repeat(64);
    let call = ToolCall::from_tool_name(
        "read_project_artifact",
        json!({
            "project": "agent:test:demo",
            "path": "artifact.bin",
            "expected_sha256": digest,
        }),
    )
    .unwrap();
    match call {
        ToolCall::ReadProjectArtifact {
            expected_sha256, ..
        } => {
            assert_eq!(
                expected_sha256.as_deref(),
                Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
            );
        }
        other => panic!("expected ReadProjectArtifact, got {other:?}"),
    }
    for invalid in [
        "abc".to_string(),
        "A".repeat(64),
        format!("g{}", "a".repeat(63)),
    ] {
        let error = ToolCall::from_tool_name(
            "read_project_artifact",
            json!({
                "project": "agent:test:demo",
                "path": "artifact.bin",
                "expected_sha256": invalid,
            }),
        )
        .unwrap_err();
        assert!(error.contains("expected_sha256"), "{error}");
        assert!(error.contains("64 lowercase hexadecimal"), "{error}");
    }
}
