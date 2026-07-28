//! Lightweight repository-context fingerprinting.
//!
//! Fingerprints contain identities and hashes only, never repository file
//! contents. They are suitable for durable continuity records and let callers
//! report exactly which context slices changed between chat turns.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs::File;
use std::io::{Read, Result as IoResult};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const FINGERPRINT_SCHEMA_VERSION: u32 = 1;
const MAX_DISCOVERED_MANIFESTS: usize = 128;
const MAX_FALLBACK_SCAN_ENTRIES: usize = 100_000;
const RULE_CANDIDATES: &[&str] = &[
    "AGENTS.md",
    "agents.md",
    "CLAUDE.md",
    ".codex/AGENTS.md",
    ".github/copilot-instructions.md",
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProjectContextFingerprint {
    pub schema_version: u32,
    /// Hash of the canonical absolute project path. The path itself is never
    /// serialized into continuity state.
    pub project_root_sha256: String,
    pub target_directory: String,
    pub git: GitContextFingerprint,
    pub rules: Vec<ContextFileFingerprint>,
    pub manifests: Vec<ContextFileFingerprint>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GitContextFingerprint {
    pub available: bool,
    pub branch: Option<String>,
    pub head: Option<String>,
    pub worktree_sha256: Option<String>,
    pub dirty: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContextFileFingerprint {
    pub path: String,
    pub sha256: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContextRefreshSummary {
    pub reused: Vec<String>,
    pub refreshed: Vec<String>,
    pub rules: ContextFileRefresh,
    pub manifests: ContextFileRefresh,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContextFileRefresh {
    pub reused: Vec<String>,
    pub refreshed: Vec<String>,
    pub removed: Vec<String>,
}

pub fn capture_project_context(
    project_root: &Path,
    target_path: Option<&str>,
) -> Result<ProjectContextFingerprint, String> {
    let canonical_root = project_root
        .canonicalize()
        .map_err(|error| format!("project root is unavailable: {error}"))?;
    if !canonical_root.is_dir() {
        return Err("project root is not a directory".to_string());
    }
    let normalized_target =
        crate::project_overview::normalize_project_overview_path(target_path.unwrap_or(""))?;
    let target_directory = target_directory(&canonical_root, &normalized_target);

    let root_identity = canonical_root.to_string_lossy();
    let project_root_sha256 = sha256_bytes(root_identity.as_bytes());
    let git = git_fingerprint(&canonical_root);
    let rules = rule_paths(&canonical_root, &target_directory)
        .into_iter()
        .filter_map(|path| fingerprint_file(&canonical_root, &path).ok())
        .collect();
    let manifests = manifest_paths(&canonical_root)
        .into_iter()
        .filter_map(|path| fingerprint_file(&canonical_root, &path).ok())
        .collect();

    Ok(ProjectContextFingerprint {
        schema_version: FINGERPRINT_SCHEMA_VERSION,
        project_root_sha256,
        target_directory,
        git,
        rules,
        manifests,
    })
}

pub fn compare_project_context(
    previous: Option<&ProjectContextFingerprint>,
    current: &ProjectContextFingerprint,
) -> ContextRefreshSummary {
    let Some(previous) = previous else {
        return ContextRefreshSummary {
            reused: Vec::new(),
            refreshed: vec![
                "project_identity".to_string(),
                "git_head".to_string(),
                "worktree".to_string(),
                "target_directory".to_string(),
            ],
            rules: compare_files(&[], &current.rules),
            manifests: compare_files(&[], &current.manifests),
        };
    };

    let mut reused = Vec::new();
    let mut refreshed = Vec::new();
    compare_scalar(
        "project_identity",
        previous.project_root_sha256 == current.project_root_sha256,
        &mut reused,
        &mut refreshed,
    );
    compare_scalar(
        "git_head",
        previous.git.available == current.git.available
            && previous.git.branch == current.git.branch
            && previous.git.head == current.git.head,
        &mut reused,
        &mut refreshed,
    );
    compare_scalar(
        "worktree",
        previous.git.worktree_sha256 == current.git.worktree_sha256
            && previous.git.dirty == current.git.dirty,
        &mut reused,
        &mut refreshed,
    );
    compare_scalar(
        "target_directory",
        previous.target_directory == current.target_directory,
        &mut reused,
        &mut refreshed,
    );
    ContextRefreshSummary {
        reused,
        refreshed,
        rules: compare_files(&previous.rules, &current.rules),
        manifests: compare_files(&previous.manifests, &current.manifests),
    }
}

fn compare_scalar(
    name: &str,
    unchanged: bool,
    reused: &mut Vec<String>,
    refreshed: &mut Vec<String>,
) {
    if unchanged {
        reused.push(name.to_string());
    } else {
        refreshed.push(name.to_string());
    }
}

fn compare_files(
    previous: &[ContextFileFingerprint],
    current: &[ContextFileFingerprint],
) -> ContextFileRefresh {
    let previous = previous
        .iter()
        .map(|file| (file.path.as_str(), file))
        .collect::<BTreeMap<_, _>>();
    let current = current
        .iter()
        .map(|file| (file.path.as_str(), file))
        .collect::<BTreeMap<_, _>>();
    let mut refresh = ContextFileRefresh::default();
    for (path, file) in &current {
        match previous.get(path) {
            Some(old) if old.sha256 == file.sha256 && old.bytes == file.bytes => {
                refresh.reused.push((*path).to_string())
            }
            _ => refresh.refreshed.push((*path).to_string()),
        }
    }
    for path in previous.keys() {
        if !current.contains_key(path) {
            refresh.removed.push((*path).to_string());
        }
    }
    refresh
}

fn target_directory(root: &Path, target: &str) -> String {
    if target.is_empty() {
        return String::new();
    }
    let candidate = root.join(target);
    if candidate.is_dir() {
        return target.to_string();
    }
    Path::new(target)
        .parent()
        .filter(|parent| *parent != Path::new(""))
        .map(|parent| parent.to_string_lossy().replace('\\', "/"))
        .unwrap_or_default()
}

fn git_fingerprint(root: &Path) -> GitContextFingerprint {
    let head = git_output(root, ["rev-parse", "--verify", "HEAD^{commit}"])
        .ok()
        .filter(|output| output.status.success())
        .and_then(output_text);
    let branch = git_output(root, ["symbolic-ref", "--short", "-q", "HEAD"])
        .ok()
        .filter(|output| output.status.success())
        .and_then(output_text);
    let status = git_output(
        root,
        ["status", "--porcelain=v1", "-z", "--untracked-files=all"],
    )
    .ok()
    .filter(|output| output.status.success())
    .map(|output| output.stdout);
    let worktree_sha256 = status
        .as_deref()
        .map(|status| worktree_sha256(root, status, head.is_some()));
    GitContextFingerprint {
        available: head.is_some() || status.is_some(),
        branch,
        head,
        worktree_sha256,
        dirty: status.as_ref().map(|bytes| !bytes.is_empty()),
    }
}

fn worktree_sha256(root: &Path, status: &[u8], has_head: bool) -> String {
    let mut hasher = Sha256::new();
    update_hash_field(&mut hasher, b"status", status);

    if has_head {
        if let Some(diff) = git_stream_sha256(
            root,
            &[
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--binary",
                "HEAD",
                "--",
            ],
        ) {
            update_hash_field(&mut hasher, b"tracked", diff.as_bytes());
        }
    } else {
        for args in [
            &[
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--binary",
                "--cached",
                "--",
            ][..],
            &["diff", "--no-ext-diff", "--no-textconv", "--binary", "--"][..],
        ] {
            if let Some(diff) = git_stream_sha256(root, args) {
                update_hash_field(&mut hasher, b"tracked", diff.as_bytes());
            }
        }
    }
    if let Some(untracked) = untracked_files_sha256(root) {
        update_hash_field(&mut hasher, b"untracked", untracked.as_bytes());
    }
    format!("{:x}", hasher.finalize())
}

fn git_stream_sha256(root: &Path, args: &[&str]) -> Option<String> {
    let mut child = Command::new("git")
        .args(args)
        .current_dir(root)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 16 * 1024];
    let mut read_failed = false;
    loop {
        match stdout.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => hasher.update(&buffer[..read]),
            Err(_) => {
                read_failed = true;
                break;
            }
        }
    }
    let status = child.wait().ok()?;
    (!read_failed && status.success()).then(|| format!("{:x}", hasher.finalize()))
}

fn untracked_files_sha256(root: &Path) -> Option<String> {
    let output = git_output(
        root,
        ["ls-files", "--others", "--exclude-standard", "-z", "--"],
    )
    .ok()?;
    if !output.status.success() {
        return None;
    }
    let mut hasher = Sha256::new();
    for raw_path in output.stdout.split(|byte| *byte == 0) {
        if raw_path.is_empty() {
            continue;
        }
        update_hash_field(&mut hasher, b"path", raw_path);
        let Ok(relative) = std::str::from_utf8(raw_path) else {
            update_hash_field(&mut hasher, b"content", b"non_utf8_path");
            continue;
        };
        if !safe_worktree_relative_path(relative) {
            update_hash_field(&mut hasher, b"content", b"unsafe_path");
            continue;
        }
        let path = root.join(relative);
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            update_hash_field(&mut hasher, b"content", b"unavailable");
            continue;
        };
        if metadata.file_type().is_symlink() {
            match std::fs::read_link(&path) {
                Ok(target) => {
                    update_hash_field(&mut hasher, b"symlink", target.to_string_lossy().as_bytes())
                }
                Err(_) => update_hash_field(&mut hasher, b"symlink", b"unavailable"),
            }
        } else if metadata.is_file() {
            match sha256_file(&path) {
                Ok(content) => update_hash_field(&mut hasher, b"content", content.as_bytes()),
                Err(_) => update_hash_field(&mut hasher, b"content", b"unavailable"),
            }
        } else {
            update_hash_field(&mut hasher, b"content", b"unsupported_type");
        }
    }
    Some(format!("{:x}", hasher.finalize()))
}

fn update_hash_field(hasher: &mut Sha256, label: &[u8], value: &[u8]) {
    hasher.update((label.len() as u64).to_be_bytes());
    hasher.update(label);
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value);
}

fn rule_paths(root: &Path, target_directory: &str) -> Vec<String> {
    let mut paths = Vec::new();
    if let Some(root_rule) = first_rule(root, "") {
        paths.push(root_rule);
    }
    if target_directory.is_empty() {
        return paths;
    }

    let mut current = PathBuf::new();
    for component in Path::new(target_directory).components() {
        current.push(component.as_os_str());
        let relative = current.to_string_lossy().replace('\\', "/");
        if let Some(local_rule) = first_rule(root, &relative) {
            if !paths.contains(&local_rule) {
                paths.push(local_rule);
            }
        }
    }
    paths
}

fn first_rule(root: &Path, directory: &str) -> Option<String> {
    RULE_CANDIDATES.iter().find_map(|candidate| {
        let path = if directory.is_empty() {
            (*candidate).to_string()
        } else {
            format!("{directory}/{candidate}")
        };
        root.join(&path).is_file().then_some(path)
    })
}

fn manifest_paths(root: &Path) -> Vec<String> {
    let mut paths = git_output(root, ["ls-files", "-co", "--exclude-standard", "-z", "--"])
        .ok()
        .filter(|output| output.status.success())
        .map(|output| {
            output
                .stdout
                .split(|byte| *byte == 0)
                .filter_map(|path| std::str::from_utf8(path).ok())
                .filter(|path| manifest_name(path))
                .map(str::to_string)
                .collect::<BTreeSet<_>>()
        })
        .unwrap_or_else(|| fallback_manifest_paths(root));
    paths.retain(|path| safe_relative_path(path));
    paths.into_iter().take(MAX_DISCOVERED_MANIFESTS).collect()
}

fn fallback_manifest_paths(root: &Path) -> BTreeSet<String> {
    let mut manifests = BTreeSet::new();
    let mut pending = vec![root.to_path_buf()];
    let mut visited = 0usize;
    while let Some(directory) = pending.pop() {
        if visited >= MAX_FALLBACK_SCAN_ENTRIES || manifests.len() >= MAX_DISCOVERED_MANIFESTS {
            break;
        }
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            visited += 1;
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_symlink() {
                continue;
            }
            let path = entry.path();
            let Ok(relative) = path.strip_prefix(root) else {
                continue;
            };
            let relative = relative.to_string_lossy().replace('\\', "/");
            if excluded_path(&relative) {
                continue;
            }
            if file_type.is_dir() {
                pending.push(path);
            } else if file_type.is_file() && manifest_name(&relative) {
                manifests.insert(relative);
            }
        }
    }
    manifests
}

fn manifest_name(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    matches!(
        name,
        "Cargo.toml"
            | "package.json"
            | "pyproject.toml"
            | "setup.py"
            | "setup.cfg"
            | "requirements.txt"
            | "Pipfile"
            | "go.mod"
            | "pom.xml"
            | "build.gradle"
            | "build.gradle.kts"
            | "Gemfile"
            | "composer.json"
            | "CMakeLists.txt"
            | "meson.build"
    ) || name.ends_with(".sln")
        || name.ends_with(".csproj")
}

fn excluded_path(path: &str) -> bool {
    path.split('/').any(|component| {
        matches!(
            component.to_ascii_lowercase().as_str(),
            ".git"
                | "target"
                | "node_modules"
                | "vendor"
                | "dist"
                | "build"
                | ".venv"
                | "venv"
                | "__pycache__"
                | ".cache"
                | "secrets"
                | "credentials"
        )
    })
}

fn safe_relative_path(path: &str) -> bool {
    !path.is_empty()
        && !Path::new(path).is_absolute()
        && !Path::new(path)
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
        && !excluded_path(path)
}

fn safe_worktree_relative_path(path: &str) -> bool {
    !path.is_empty()
        && !Path::new(path).is_absolute()
        && Path::new(path).components().all(|component| {
            matches!(
                component,
                std::path::Component::Normal(_) | std::path::Component::CurDir
            )
        })
}

fn fingerprint_file(root: &Path, relative: &str) -> Result<ContextFileFingerprint, String> {
    if !safe_relative_path(relative) {
        return Err("context file path is unsafe".to_string());
    }
    let path = root.join(relative);
    let canonical = path
        .canonicalize()
        .map_err(|error| format!("context file is unavailable: {error}"))?;
    if !canonical.starts_with(root) || !canonical.is_file() {
        return Err("context file escapes the project root".to_string());
    }
    let bytes = canonical
        .metadata()
        .map_err(|error| format!("context file metadata failed: {error}"))?
        .len();
    let sha256 = sha256_file(&canonical)
        .map_err(|error| format!("context file fingerprint failed: {error}"))?;
    Ok(ContextFileFingerprint {
        path: relative.to_string(),
        sha256,
        bytes,
    })
}

fn sha256_file(path: &Path) -> IoResult<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 16 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn sha256_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn git_output<const N: usize>(root: &Path, args: [&str; N]) -> std::io::Result<Output> {
    Command::new("git")
        .args(args.map(OsStr::new))
        .current_dir(root)
        .output()
}

fn output_text(output: Output) -> Option<String> {
    String::from_utf8(output.stdout)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(root: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn repo(name: &str) -> tempfile::TempDir {
        let temp = tempfile::Builder::new().prefix(name).tempdir().unwrap();
        git(temp.path(), &["init", "-q"]);
        git(temp.path(), &["config", "user.name", "WebCodex Test"]);
        git(
            temp.path(),
            &["config", "user.email", "webcodex@example.invalid"],
        );
        std::fs::write(temp.path().join("Cargo.toml"), "[package]\nname='demo'\n").unwrap();
        std::fs::write(temp.path().join("AGENTS.md"), "root rules\n").unwrap();
        std::fs::create_dir_all(temp.path().join("src/nested")).unwrap();
        std::fs::write(temp.path().join("src/lib.rs"), "pub fn demo() {}\n").unwrap();
        git(temp.path(), &["add", "."]);
        git(temp.path(), &["commit", "-qm", "initial"]);
        temp
    }

    #[test]
    fn unchanged_context_is_fully_reused() {
        let repo = repo("context-reuse");
        let first = capture_project_context(repo.path(), Some("src")).unwrap();
        let second = capture_project_context(repo.path(), Some("src")).unwrap();
        let refresh = compare_project_context(Some(&first), &second);
        assert!(refresh.refreshed.is_empty());
        assert!(refresh.rules.refreshed.is_empty());
        assert!(refresh.manifests.refreshed.is_empty());
        assert_eq!(refresh.rules.reused, ["AGENTS.md"]);
    }

    #[test]
    fn head_and_worktree_refresh_independently() {
        let repo = repo("context-git");
        let first = capture_project_context(repo.path(), None).unwrap();
        std::fs::write(repo.path().join("src/lib.rs"), "pub fn changed() {}\n").unwrap();
        let dirty = capture_project_context(repo.path(), None).unwrap();
        let refresh = compare_project_context(Some(&first), &dirty);
        assert!(refresh.refreshed.contains(&"worktree".to_string()));
        assert!(refresh.reused.contains(&"git_head".to_string()));

        std::fs::write(repo.path().join("src/lib.rs"), "pub fn altered() {}\n").unwrap();
        let dirty_again = capture_project_context(repo.path(), None).unwrap();
        assert_ne!(
            dirty.git.worktree_sha256, dirty_again.git.worktree_sha256,
            "content changes must refresh the worktree even when porcelain status is unchanged"
        );

        git(repo.path(), &["add", "src/lib.rs"]);
        git(repo.path(), &["commit", "-qm", "change head"]);
        let committed = capture_project_context(repo.path(), None).unwrap();
        let refresh = compare_project_context(Some(&dirty_again), &committed);
        assert!(refresh.refreshed.contains(&"git_head".to_string()));
        assert!(refresh.refreshed.contains(&"worktree".to_string()));
    }

    #[test]
    fn untracked_content_changes_refresh_worktree_with_the_same_status_path() {
        let repo = repo("context-untracked");
        std::fs::write(repo.path().join("notes.tmp"), "one\n").unwrap();
        let first = capture_project_context(repo.path(), None).unwrap();
        std::fs::write(repo.path().join("notes.tmp"), "two\n").unwrap();
        let second = capture_project_context(repo.path(), None).unwrap();
        assert_ne!(first.git.worktree_sha256, second.git.worktree_sha256);
        let refresh = compare_project_context(Some(&first), &second);
        assert!(refresh.refreshed.contains(&"worktree".to_string()));
    }

    #[test]
    fn branch_change_refreshes_git_baseline_without_refreshing_worktree() {
        let repo = repo("context-branch");
        let first = capture_project_context(repo.path(), None).unwrap();
        git(repo.path(), &["switch", "-qc", "feature"]);
        let second = capture_project_context(repo.path(), None).unwrap();
        let refresh = compare_project_context(Some(&first), &second);
        assert!(refresh.refreshed.contains(&"git_head".to_string()));
        assert!(refresh.reused.contains(&"worktree".to_string()));
        assert_eq!(first.git.head, second.git.head);
        assert_ne!(first.git.branch, second.git.branch);
    }

    #[test]
    fn changed_and_new_local_rules_refresh_only_their_paths() {
        let repo = repo("context-rules");
        let first = capture_project_context(repo.path(), Some("src/nested")).unwrap();
        std::fs::write(repo.path().join("AGENTS.md"), "updated root rules\n").unwrap();
        std::fs::write(repo.path().join("src/AGENTS.md"), "local rules\n").unwrap();
        let second = capture_project_context(repo.path(), Some("src/nested")).unwrap();
        let refresh = compare_project_context(Some(&first), &second);
        assert_eq!(
            refresh.rules.refreshed,
            ["AGENTS.md".to_string(), "src/AGENTS.md".to_string()]
        );
        assert!(refresh.manifests.refreshed.is_empty());
        assert!(second.rules.iter().any(|rule| rule.path == "src/AGENTS.md"));
    }

    #[test]
    fn target_directory_change_discovers_only_newly_applicable_rules() {
        let repo = repo("context-target");
        std::fs::write(repo.path().join("src/AGENTS.md"), "source rules\n").unwrap();
        let root_target = capture_project_context(repo.path(), None).unwrap();
        let nested_target =
            capture_project_context(repo.path(), Some("src/nested/lib.rs")).unwrap();
        let refresh = compare_project_context(Some(&root_target), &nested_target);
        assert!(refresh.refreshed.contains(&"target_directory".to_string()));
        assert_eq!(refresh.rules.refreshed, ["src/AGENTS.md"]);
        assert_eq!(refresh.rules.reused, ["AGENTS.md"]);
        assert!(refresh.manifests.refreshed.is_empty());
    }

    #[test]
    fn manifest_changes_do_not_refresh_rules() {
        let repo = repo("context-manifest");
        let first = capture_project_context(repo.path(), None).unwrap();
        std::fs::write(
            repo.path().join("Cargo.toml"),
            "[package]\nname='changed'\n",
        )
        .unwrap();
        let second = capture_project_context(repo.path(), None).unwrap();
        let refresh = compare_project_context(Some(&first), &second);
        assert_eq!(refresh.manifests.refreshed, ["Cargo.toml"]);
        assert!(refresh.rules.refreshed.is_empty());
    }

    #[test]
    fn similar_repository_names_never_share_identity() {
        let first_repo = repo("same-name");
        let second_repo = repo("same-name");
        let first = capture_project_context(first_repo.path(), None).unwrap();
        let second = capture_project_context(second_repo.path(), None).unwrap();
        assert_ne!(first.project_root_sha256, second.project_root_sha256);
        let refresh = compare_project_context(Some(&first), &second);
        assert!(refresh.refreshed.contains(&"project_identity".to_string()));
    }
}
