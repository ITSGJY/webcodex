use super::*;
use std::path::Path;

fn observe(path: &Path) -> RunnerInstructionSnapshotResponse {
    let result = handle_runner_instruction_request(
        1,
        &InstructionsConfig {
            files: vec![path.to_path_buf()],
        },
        RunnerInstructionRequest::snapshot(),
    );
    assert_eq!(result.exit_code, Some(0), "{result:?}");
    serde_json::from_str(result.stdout.as_deref().unwrap()).unwrap()
}

#[test]
fn instruction_snapshot_bounds_utf8_lines_and_detects_tail_changes() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().canonicalize().unwrap().join("global.md");
    let prefix = "bounded guidance\n".repeat(400);
    std::fs::write(&path, format!("{prefix}first tail\n")).unwrap();
    let first = observe(&path);
    assert!(first.scan_complete);
    assert_eq!(first.files[0].content.lines().count(), 400);
    assert!(first.files[0].truncated);
    assert!(first.files[0].read_more.is_none());
    std::fs::write(&path, format!("{prefix}second tail\n")).unwrap();
    let second = observe(&path);
    assert_eq!(first.files[0].content, second.files[0].content);
    assert_ne!(first.files[0].fingerprint, second.files[0].fingerprint);
    std::fs::write(&path, [0xff, 0xfe, 0x00]).unwrap();
    let invalid = observe(&path);
    assert!(!invalid.scan_complete);
    assert!(invalid.files.is_empty());
    std::fs::write(
        &path,
        vec![b'x'; MAX_CONFIGURED_INSTRUCTION_FILE_BYTES as usize + 1],
    )
    .unwrap();
    let oversized = observe(&path);
    assert!(!oversized.scan_complete);
    assert!(oversized.files.is_empty());
}

#[test]
fn instruction_reader_stops_at_byte_cap_even_when_the_source_grows() {
    let cap = MAX_CONFIGURED_INSTRUCTION_FILE_BYTES;
    let mut reader = io::Cursor::new(vec![b'x'; (cap * 2) as usize]);
    assert!(read_instruction_bytes(&mut reader).is_err());
    assert_eq!(reader.position(), cap + 1);
}

#[cfg(windows)]
#[test]
fn instruction_snapshot_reads_unicode_crlf_and_canonical_windows_paths() {
    let tmp = tempfile::tempdir().unwrap();
    let directory = tmp.path().join("规则 with spaces");
    std::fs::create_dir(&directory).unwrap();
    let path = directory.join("AGENTS.md");
    std::fs::write(&path, "# 全局规则\r\n保留用户修改。\r\n").unwrap();
    let plain = observe(&path);
    let canonical = observe(&path.canonicalize().unwrap());
    assert!(plain.scan_complete && canonical.scan_complete);
    assert_eq!(plain.files[0].content, "# 全局规则\n保留用户修改。");
    assert_eq!(plain.files[0].fingerprint, canonical.files[0].fingerprint);
    assert_eq!(plain.files[0].path, "runner/0/AGENTS.md");
}

#[cfg(windows)]
#[test]
fn instruction_snapshot_rejects_parent_junction_redirection() {
    use std::process::{Command, Stdio};
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("unconfigured");
    let junction = tmp.path().join("configured-directory");
    std::fs::create_dir(&target).unwrap();
    std::fs::write(
        target.join("AGENTS.md"),
        "unconfigured target must stay private",
    )
    .unwrap();
    let created = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command",
            "$ErrorActionPreference='Stop'; New-Item -ItemType Junction -Path $env:WC_JUNCTION_PATH -Target $env:WC_JUNCTION_TARGET | Out-Null"])
        .env("WC_JUNCTION_PATH", &junction)
        .env("WC_JUNCTION_TARGET", &target)
        .stdin(Stdio::null())
        .output()
        .expect("create isolated junction fixture");
    assert!(created.status.success(), "junction fixture creation failed");
    let result = observe(&junction.join("AGENTS.md"));
    std::fs::remove_file(target.join("AGENTS.md")).unwrap();
    let missing_leaf = observe(&junction.join("AGENTS.md"));
    assert!(!missing_leaf.scan_complete);
    assert!(missing_leaf.files.is_empty());
    std::fs::remove_dir(&target).unwrap();
    let dangling_parent = observe(&junction.join("AGENTS.md"));
    assert!(!dangling_parent.scan_complete);
    assert!(dangling_parent.files.is_empty());
    std::fs::remove_dir(&junction).unwrap();
    assert!(
        !result.scan_complete,
        "a parent reparse point must not widen instruction authority"
    );
    assert!(
        result.files.is_empty(),
        "unconfigured target content must not be returned"
    );
}

#[cfg(unix)]
#[test]
fn instruction_snapshot_rejects_parent_symlink_redirection() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let target = root.join("unconfigured");
    let link = root.join("configured-directory");
    std::fs::create_dir(&target).unwrap();
    std::fs::write(
        target.join("AGENTS.md"),
        "unconfigured target must stay private",
    )
    .unwrap();
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let result = observe(&link.join("AGENTS.md"));
    assert!(!result.scan_complete);
    assert!(result.files.is_empty());
}

#[test]
fn instruction_snapshot_observes_confirmed_removal_and_empty_files() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().canonicalize().unwrap().join("AGENTS.md");
    std::fs::write(&path, "global guidance").unwrap();
    assert_eq!(observe(&path).files.len(), 1);
    std::fs::remove_file(&path).unwrap();
    let removed = observe(&path);
    assert!(removed.scan_complete);
    assert!(removed.files.is_empty());
    std::fs::write(&path, "").unwrap();
    let empty = observe(&path);
    assert!(empty.scan_complete);
    assert!(empty.files.is_empty());
}

#[cfg(unix)]
#[test]
fn missing_leaf_under_redirected_or_dangling_parent_stays_unavailable() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let target = root.join("outside");
    let link = root.join("configured");
    std::fs::create_dir(&target).unwrap();
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let missing_leaf = observe(&link.join("AGENTS.md"));
    assert!(!missing_leaf.scan_complete);
    assert!(missing_leaf.files.is_empty());
    std::fs::remove_dir(target).unwrap();
    let dangling_parent = observe(&link.join("AGENTS.md"));
    assert!(!dangling_parent.scan_complete);
    assert!(dangling_parent.files.is_empty());
}
