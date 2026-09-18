use super::config::InstructionsConfig;
use super::configured_skills::metadata_is_link_like;
use super::CommandResult;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::path::Path;
use std::time::Instant;
use webcodex_core::project_instructions::{
    InstructionSourceScope, LoadedInstructionCandidate, ProjectInstructionsSnapshot,
};
use webcodex_core::runner_instruction::{
    RunnerInstructionAction, RunnerInstructionRequest, RunnerInstructionSnapshotResponse,
    RUNNER_INSTRUCTION_RESPONSE_FORMAT, RUNNER_INSTRUCTION_RESPONSE_MAX_BYTES,
};

const MAX_CONFIGURED_INSTRUCTION_FILE_BYTES: u64 = 1024 * 1024;

pub(crate) fn handle_runner_instruction_request(
    generation: u64,
    config: &InstructionsConfig,
    request: RunnerInstructionRequest,
) -> CommandResult {
    let started = Instant::now();
    if request.validate().is_err() {
        return error_result(started, "instruction_invalid_request");
    }
    match request.action {
        RunnerInstructionAction::Snapshot => snapshot(generation, config, started),
    }
}

fn snapshot(generation: u64, config: &InstructionsConfig, started: Instant) -> CommandResult {
    let mut candidates = Vec::with_capacity(config.files.len());
    let mut identities = HashSet::with_capacity(config.files.len());
    let mut scan_complete = true;

    for (index, configured) in config.files.iter().enumerate() {
        let file = match open_instruction_file(configured) {
            Ok(file) => file,
            // A confirmed missing file withdraws its guidance. Permission and
            // other read failures remain unavailable, not deletion evidence.
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(_) => {
                scan_complete = false;
                continue;
            }
        };
        let canonical = match std::fs::canonicalize(configured) {
            Ok(path) if path.is_file() => path,
            _ => {
                scan_complete = false;
                continue;
            }
        };
        let identity = crate::runner_config::paths::normalize_path_identity(&canonical);
        if !identities.insert(identity) {
            scan_complete = false;
            continue;
        }
        let metadata = match file.metadata() {
            Ok(metadata) if metadata.len() <= MAX_CONFIGURED_INSTRUCTION_FILE_BYTES => metadata,
            _ => {
                scan_complete = false;
                continue;
            }
        };
        let bytes = match read_instruction_bytes(file) {
            Ok(bytes) if bytes.len() as u64 == metadata.len() => bytes,
            _ => {
                scan_complete = false;
                continue;
            }
        };
        let content = match String::from_utf8(bytes) {
            Ok(content) => content,
            Err(_) => {
                scan_complete = false;
                continue;
            }
        };
        if content.is_empty() {
            continue;
        }
        let basename = configured
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .unwrap_or("instructions");
        let logical_source = format!("runner/{index}/{basename}");
        let total_lines = line_count(&content);
        let full_sha256 = format!("{:x}", Sha256::digest(content.as_bytes()));
        candidates.push(LoadedInstructionCandidate {
            source_scope: InstructionSourceScope::Runner,
            path: logical_source,
            content,
            total_lines,
            full_sha256: Some(full_sha256),
        });
    }

    let snapshot = ProjectInstructionsSnapshot::from_candidates(candidates, scan_complete);
    let response = RunnerInstructionSnapshotResponse {
        format: RUNNER_INSTRUCTION_RESPONSE_FORMAT.to_string(),
        generation,
        scan_complete: snapshot.scan_complete,
        files: snapshot.files,
    };
    if response.validate().is_err() {
        return error_result(started, "instruction_response_invalid");
    }
    let stdout = match serde_json::to_string(&response) {
        Ok(stdout) if stdout.len() <= RUNNER_INSTRUCTION_RESPONSE_MAX_BYTES => stdout,
        _ => return error_result(started, "instruction_response_too_large"),
    };
    CommandResult {
        exit_code: Some(0),
        stdout: Some(stdout),
        stderr: None,
        duration_ms: Some(started.elapsed().as_millis().try_into().unwrap_or(u64::MAX)),
        error: None,
    }
}

// Instruction authority names ordinary files, not redirectable filesystem
// trees. Check every component, not only the final AGENTS.md entry. Open the
// leaf without following links and keep that handle for metadata and content.
fn open_instruction_file(path: &Path) -> io::Result<File> {
    // Establish parent authority before classifying a missing leaf. A dangling
    // or redirected parent must remain unavailable, not evidence of removal.
    let components = path.ancestors().collect::<Vec<_>>();
    for component in components.into_iter().rev() {
        let metadata = std::fs::symlink_metadata(component).map_err(|error| {
            if component != path && error.kind() == io::ErrorKind::NotFound {
                // A missing directory may be an unavailable mount or a
                // temporarily moved tree, not a confirmed leaf-file removal.
                io::Error::other("instruction parent is unavailable")
            } else {
                error
            }
        })?;
        if metadata_is_link_like(&metadata)
            || (component == path && !metadata.is_file())
            || (component != path && !metadata.is_dir())
        {
            return Err(io::Error::other(
                "instruction path is not an ordinary file path",
            ));
        }
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ,
        };
        options
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .share_mode(FILE_SHARE_READ);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata_is_link_like(&metadata) {
        return Err(io::Error::other(
            "instruction handle is not an ordinary file",
        ));
    }
    Ok(file)
}

fn read_instruction_bytes(reader: impl Read) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    // A pre-read metadata length is not a resource bound: the file can grow.
    reader
        .take(MAX_CONFIGURED_INSTRUCTION_FILE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_CONFIGURED_INSTRUCTION_FILE_BYTES {
        return Err(io::Error::other("instruction file exceeds the byte limit"));
    }
    Ok(bytes)
}

fn line_count(content: &str) -> usize {
    if content.is_empty() {
        0
    } else {
        content.bytes().filter(|byte| *byte == b'\n').count()
            + usize::from(!content.ends_with('\n'))
    }
}

fn error_result(started: Instant, code: &str) -> CommandResult {
    CommandResult {
        exit_code: Some(1),
        stdout: None,
        stderr: None,
        duration_ms: Some(started.elapsed().as_millis().try_into().unwrap_or(u64::MAX)),
        error: Some(code.to_string()),
    }
}

#[cfg(test)]
#[path = "runner_instruction_tests.rs"]
mod safety_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn omitted_config_produces_complete_empty_snapshot() {
        let response = handle_runner_instruction_request(
            1,
            &InstructionsConfig::default(),
            RunnerInstructionRequest::snapshot(),
        );
        let parsed: RunnerInstructionSnapshotResponse =
            serde_json::from_str(response.stdout.as_deref().unwrap()).unwrap();
        assert!(parsed.scan_complete);
        assert!(parsed.files.is_empty());
    }

    #[test]
    fn snapshot_uses_logical_sources_and_live_content() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().canonicalize().unwrap().join("AGENTS.md");
        std::fs::write(&path, "first\n").unwrap();
        let config = InstructionsConfig {
            files: vec![path.clone()],
        };

        let first =
            handle_runner_instruction_request(7, &config, RunnerInstructionRequest::snapshot());
        let first_stdout = first.stdout.as_deref().unwrap();
        assert!(!first_stdout.contains(path.to_string_lossy().as_ref()));
        let first: RunnerInstructionSnapshotResponse = serde_json::from_str(first_stdout).unwrap();
        assert_eq!(first.files.len(), 1);
        assert_eq!(first.files[0].path, "runner/0/AGENTS.md");
        assert_eq!(first.files[0].content, "first");
        assert!(first.files[0].read_more.is_none());
        let first_fingerprint = first.files[0].fingerprint.clone();

        std::fs::write(&path, "second\n").unwrap();
        let second =
            handle_runner_instruction_request(7, &config, RunnerInstructionRequest::snapshot());
        let second: RunnerInstructionSnapshotResponse =
            serde_json::from_str(second.stdout.as_deref().unwrap()).unwrap();
        assert_eq!(second.generation, 7);
        assert_eq!(second.files[0].content, "second");
        assert_ne!(second.files[0].fingerprint, first_fingerprint);
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_rejects_configured_symlink_instead_of_following_it() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("secret.txt");
        let link = tmp.path().join("AGENTS.md");
        std::fs::write(&target, "must not be projected\n").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let config = InstructionsConfig { files: vec![link] };

        let response =
            handle_runner_instruction_request(1, &config, RunnerInstructionRequest::snapshot());
        assert_eq!(response.exit_code, Some(0));
        let stdout = response.stdout.as_deref().unwrap();
        assert!(!stdout.contains("must not be projected"));
        let parsed: RunnerInstructionSnapshotResponse = serde_json::from_str(stdout).unwrap();
        assert!(!parsed.scan_complete);
        assert!(parsed.files.is_empty());
    }

    #[test]
    fn snapshot_does_not_require_project_allowed_roots() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().canonicalize().unwrap().join("global.md");
        std::fs::write(&path, "runner only").unwrap();
        let config = InstructionsConfig {
            files: vec![PathBuf::from(&path)],
        };
        let response =
            handle_runner_instruction_request(1, &config, RunnerInstructionRequest::snapshot());
        assert_eq!(response.exit_code, Some(0));
    }
}
