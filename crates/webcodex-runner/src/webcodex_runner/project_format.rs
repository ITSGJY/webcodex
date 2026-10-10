//! Runner-owned explicit-file format planning. This module deliberately does
//! not confer write authority or advertise an executable formatting capability.

use super::config::RunnerPolicy;
use super::file_access::{directory_identity, file_identity, file_link_count, open_regular_file};
use super::projects::find_project_shell_context_by_id;
use super::shell::cwd_allowed;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use webcodex_core::apply_edits_shared::is_sensitive_edit_path;
use webcodex_core::project_format::*;
use webcodex_workspace::project_recipe::{
    resolve_project_recipe_root, ProjectRecipeId, ProjectRecipeResolutionError,
};

fn unavailable(code: &str) -> ProjectFormatPlanningResult {
    ProjectFormatPlanningResult::Unavailable { code: code.into() }
}

fn recipe_error(error: ProjectRecipeResolutionError) -> ProjectFormatPlanningResult {
    unavailable(match error {
        ProjectRecipeResolutionError::Ambiguous { .. } => "format_recipe_ambiguous",
        ProjectRecipeResolutionError::NotFound
        | ProjectRecipeResolutionError::ExecutionRootUnavailable => "format_recipe_not_found",
        ProjectRecipeResolutionError::SourceFileInvalid => "format_manifest_invalid",
        _ => "format_recipe_mismatch",
    })
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn root_digest(root: &Path) -> Result<String, &'static str> {
    let identity = directory_identity(root).map_err(|_| "format_path_invalid")?;
    let mut digest = Sha256::new();
    digest.update(root.as_os_str().as_encoded_bytes());
    digest.update(identity);
    Ok(format!("{:x}", digest.finalize()))
}

fn validate_marker_chain(root: &Path, relative: &str) -> Result<(), &'static str> {
    let directory = checked_path(root, relative, true)?;
    for parent in directory.ancestors() {
        if !parent.starts_with(root) {
            break;
        }
        let mut found = false;
        for marker in ["Cargo.toml", "pyproject.toml", "package.json", "go.mod"] {
            match fs::symlink_metadata(parent.join(marker)) {
                Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
                    found = true
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                _ => return Err("format_manifest_invalid"),
            }
        }
        if found {
            return Ok(());
        }
    }
    Ok(())
}

/// Check every component, including directory aliases within the Project.
/// Canonical containment alone would accept a symlink to another selected file.
fn checked_path(root: &Path, relative: &str, directory: bool) -> Result<PathBuf, &'static str> {
    if !valid_format_relative_path(relative, directory) || is_sensitive_edit_path(relative) {
        return Err("format_path_invalid");
    }
    let mut path = root.to_path_buf();
    if relative != "." {
        for component in relative.split('/') {
            path.push(component);
            let metadata = fs::symlink_metadata(&path).map_err(|_| "format_file_unavailable")?;
            if metadata.file_type().is_symlink() {
                return Err("format_path_invalid");
            }
        }
    }
    let canonical = path.canonicalize().map_err(|_| "format_file_unavailable")?;
    if !canonical.starts_with(root) || canonical != path {
        return Err("format_path_invalid");
    }
    let metadata = fs::symlink_metadata(&path).map_err(|_| "format_file_unavailable")?;
    if if directory {
        !metadata.is_dir()
    } else {
        !metadata.is_file()
    } {
        return Err("format_path_invalid");
    }
    Ok(path)
}

fn read_source(root: &Path, relative: &str, max: usize) -> Result<(String, String), &'static str> {
    let path = checked_path(root, relative, false)?;
    let file = open_regular_file(&path).map_err(|_| "format_file_unavailable")?;
    if file_link_count(&file).map_err(|_| "format_file_unavailable")? != 1 {
        return Err("format_alias_unavailable");
    }
    let identity = file_identity(&file).map_err(|_| "format_file_unavailable")?;
    let mut bytes = Vec::new();
    file.take(max as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "format_file_unavailable")?;
    if bytes.len() > max {
        return Err("format_input_too_large");
    }
    if checked_path(root, relative, false)? != path
        || file_identity(&open_regular_file(&path).map_err(|_| "format_file_unavailable")?)
            .map_err(|_| "format_file_unavailable")?
            != identity
    {
        return Err("format_plan_stale");
    }
    Ok((
        String::from_utf8(bytes).map_err(|_| "format_invalid_utf8")?,
        digest(&identity),
    ))
}

fn rust_profile(manifest: &toml::Value) -> Result<ProjectFormatProfile, &'static str> {
    let package = manifest
        .get("package")
        .and_then(toml::Value::as_table)
        .ok_or("format_manifest_unsupported")?;
    // Inherited edition and explicit external workspace topology require an
    // independent profile. Do not silently choose a default edition for them.
    if package.contains_key("workspace") {
        return Err("format_manifest_unsupported");
    }
    let edition = match package.get("edition") {
        None => "2015",
        Some(value) => value.as_str().ok_or("format_manifest_unsupported")?,
    };
    let profile = ProjectFormatProfile::Rust {
        edition: edition.into(),
    };
    if !profile.is_valid() {
        return Err("format_manifest_invalid");
    }
    Ok(profile)
}

fn python_profile(manifest: &toml::Value) -> Result<ProjectFormatProfile, &'static str> {
    let ruff = manifest
        .get("tool")
        .and_then(|tool| tool.get("ruff"))
        .and_then(toml::Value::as_table)
        .ok_or("format_manifest_unsupported")?;
    if ruff.contains_key("extend") {
        return Err("format_manifest_invalid");
    }
    let target_version = ruff
        .get("target-version")
        .and_then(toml::Value::as_str)
        .ok_or("format_manifest_invalid")?;
    let profile = ProjectFormatProfile::Python {
        target_version: target_version.into(),
    };
    if !profile.is_valid() {
        return Err("format_manifest_invalid");
    }
    Ok(profile)
}

fn reject_custom_rustfmt_config(root: &Path) -> Result<(), &'static str> {
    // rustfmt searches parents for configuration. Only metadata is inspected
    // outside the Project; no external configuration is read or authorized.
    for (depth, parent) in root.ancestors().enumerate() {
        if depth >= 64 {
            return Err("format_scope_unavailable");
        }
        for name in ["rustfmt.toml", ".rustfmt.toml"] {
            match fs::symlink_metadata(parent.join(name)) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                _ => return Err("format_config_unsupported"),
            }
        }
    }
    Ok(())
}

/// Private snapshots are retained only by the planning/execution owner. Do not
/// derive Debug/Serialize: these bytes are never a transport or log payload.
pub(crate) struct PlannedFormat {
    pub(crate) plan: ProjectFormatPlan,
    pub(crate) root: PathBuf,
    pub(crate) cwd: PathBuf,
    pub(crate) sources: Vec<String>,
}

pub(crate) fn plan(
    policy: &RunnerPolicy,
    registry: &Path,
    request: &ProjectFormatRequest,
) -> Result<PlannedFormat, ProjectFormatPlanningResult> {
    request
        .validate()
        .map_err(|_| unavailable("invalid_arguments"))?;
    let project = find_project_shell_context_by_id(registry, &request.project_id)
        .ok_or_else(|| unavailable("unknown_project"))?;
    if !project.allow_patch || !policy.allow_raw_shell {
        return Err(unavailable("permission_denied"));
    }
    let root = PathBuf::from(&project.path);
    cwd_allowed(policy, &root).map_err(|_| unavailable("invalid_project_path"))?;
    let root = root
        .canonicalize()
        .map_err(|_| unavailable("invalid_project_path"))?;
    let initial_root_digest = root_digest(&root).map_err(unavailable)?;
    checked_path(&root, request.cwd.as_deref().unwrap_or("."), true).map_err(unavailable)?;
    // No manifestless Python fallback and no explicit-adapter bypass of an
    // ambiguous root. Formatting needs a concrete local manifest in all cases.
    validate_marker_chain(&root, request.cwd.as_deref().unwrap_or(".")).map_err(unavailable)?;
    let resolved =
        resolve_project_recipe_root(&root, request.cwd.as_deref(), None).map_err(recipe_error)?;
    let adapter = match resolved.recipe {
        ProjectRecipeId::Rust => ProjectFormatAdapter::Rust,
        ProjectRecipeId::Python => ProjectFormatAdapter::Python,
        _ => return Err(unavailable("format_adapter_unavailable")),
    };
    if request.adapter != ProjectFormatAdapter::Auto && request.adapter != adapter {
        return Err(unavailable("format_recipe_mismatch"));
    }
    let marker = resolved
        .marker_path()
        .strip_prefix(&root)
        .map_err(|_| unavailable("format_path_invalid"))?
        .to_string_lossy()
        .replace('\\', "/");
    let (manifest, manifest_identity) =
        read_source(&root, &marker, PROJECT_FORMAT_MANIFEST_MAX_BYTES).map_err(unavailable)?;
    let value: toml::Value =
        toml::from_str(&manifest).map_err(|_| unavailable("format_manifest_invalid"))?;
    let profile = match adapter {
        ProjectFormatAdapter::Rust => {
            reject_custom_rustfmt_config(&resolved.absolute_root).map_err(unavailable)?;
            rust_profile(&value).map_err(unavailable)?
        }
        ProjectFormatAdapter::Python => python_profile(&value).map_err(unavailable)?,
        ProjectFormatAdapter::Auto => unreachable!(),
    };
    let mut sources = Vec::new();
    let mut files = Vec::new();
    let mut identities = Vec::new();
    let mut total = 0usize;
    for relative in &request.files {
        let path = checked_path(&root, relative, false).map_err(unavailable)?;
        let parent = path
            .parent()
            .ok_or_else(|| unavailable("format_path_invalid"))?;
        let parent = parent
            .strip_prefix(&root)
            .map_err(|_| unavailable("format_path_invalid"))?;
        let parent = if parent.as_os_str().is_empty() {
            ".".into()
        } else {
            parent.to_string_lossy().replace('\\', "/")
        };
        validate_marker_chain(&root, &parent).map_err(unavailable)?;
        let owner =
            resolve_project_recipe_root(&root, Some(&parent), None).map_err(recipe_error)?;
        if owner != resolved {
            return Err(unavailable("format_scope_mismatch"));
        }
        let (source, identity) =
            read_source(&root, relative, PROJECT_FORMAT_FILE_MAX_BYTES).map_err(unavailable)?;
        if identities.contains(&identity) {
            return Err(unavailable("format_duplicate_file"));
        }
        identities.push(identity.clone());
        total += source.len();
        if total > PROJECT_FORMAT_TOTAL_MAX_BYTES {
            return Err(unavailable("format_input_too_large"));
        }
        files.push(ProjectFormatFileWitness {
            path: relative.clone(),
            bytes: source.len(),
            sha256: digest(source.as_bytes()),
            identity_digest: identity,
        });
        sources.push(source);
    }
    if root_digest(&root).map_err(unavailable)? != initial_root_digest {
        return Err(unavailable("format_plan_stale"));
    }
    let plan = ProjectFormatPlan {
        request: request.clone(),
        recipe_root: resolved.relative_root,
        root_digest: initial_root_digest,
        manifest_digest: digest(&[manifest_identity.as_bytes(), manifest.as_bytes()].concat()),
        profile,
        files,
    };
    plan.digest()
        .map_err(|_| unavailable("format_scope_mismatch"))?;
    Ok(PlannedFormat {
        plan,
        root,
        cwd: resolved.absolute_root,
        sources,
    })
}

/// Admission and worker must independently obtain fresh snapshots through this
/// same planner. A digest is a stale-plan fence, never permission to replay.
pub(crate) fn replan(
    policy: &RunnerPolicy,
    registry: &Path,
    expected: &ProjectFormatPlan,
) -> Result<PlannedFormat, &'static str> {
    expected.digest().map_err(|_| "format_plan_stale")?;
    let current = plan(policy, registry, &expected.request).map_err(|_| "format_plan_stale")?;
    if current.plan != *expected {
        return Err("format_plan_stale");
    }
    Ok(current)
}

pub(crate) fn handle(
    policy: &RunnerPolicy,
    registry: &Path,
    request: &ProjectFormatRequest,
) -> super::output::CommandResult {
    // Planning is implemented, but a Ready wire response would imply a worker
    // can consume it. Keep native execution unavailable until exact output,
    // guarded commit and durable mutation-receipt gates are implemented.
    let result = match plan(policy, registry, request) {
        Ok(_) => unavailable("format_execution_unavailable"),
        Err(error) => error,
    };
    super::output::CommandResult {
        exit_code: Some(0),
        stdout: Some(serde_json::to_string(&result).expect("typed format planning response")),
        stderr: None,
        duration_ms: Some(0),
        error: None,
    }
}

/// Generate bounded formatter candidates without changing project files.
///
/// Runner-owned Job execution supplies its generation, total deadline and stop
/// signal. A single request never receives a fresh timeout for every file.
pub(crate) fn format_candidates(
    policy: &RunnerPolicy,
    shell: &super::config::ShellConfig,
    registry: &Path,
    planned: &PlannedFormat,
    cache: &super::shell::PreparedShellProfileCache,
    generation: u64,
    timeout_secs: u64,
    stop_requested: Option<&std::sync::atomic::AtomicBool>,
) -> Result<Vec<String>, &'static str> {
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};
    use webcodex_core::runner_protocol::ShellCommandExecutionState;

    if !(1..=PROJECT_FORMAT_TIMEOUT_MAX_SECS).contains(&timeout_secs) {
        return Err("format_timeout_invalid");
    }
    if !planned.plan.is_valid() || planned.sources.len() != planned.plan.files.len() {
        return Err("format_plan_invalid");
    }

    let started = Instant::now();
    let total_budget = Duration::from_secs(timeout_secs);
    let mut total_output = 0usize;
    let mut candidates = Vec::with_capacity(planned.sources.len());
    for (index, witness) in planned.plan.files.iter().enumerate() {
        if stop_requested.is_some_and(|stop| stop.load(Ordering::SeqCst)) {
            return Err("format_cancelled");
        }
        let remaining = total_budget.saturating_sub(started.elapsed()).as_secs();
        if remaining == 0 {
            return Err("format_timeout");
        }
        let source = &planned.sources[index];
        let relative = planned
            .plan
            .recipe_relative_file(&witness.path)
            .ok_or("format_path_invalid")?;
        let command = planned
            .plan
            .profile
            .process(relative)
            .map_err(|_| "format_profile_invalid")?;
        let cwd = planned.cwd.to_str().ok_or("format_path_invalid")?;

        let result = super::shell::run_process_with_profiles_and_execution_state(
            generation,
            policy,
            shell,
            registry,
            cache,
            Some(cwd),
            &command.executable,
            &command.args,
            Some(source),
            remaining,
            stop_requested,
        );
        if stop_requested.is_some_and(|stop| stop.load(Ordering::SeqCst)) {
            return Err("format_cancelled");
        }
        match result.execution_state {
            ShellCommandExecutionState::Completed => {}
            ShellCommandExecutionState::NotStarted => return Err("format_process_failed"),
            ShellCommandExecutionState::TimedOut => return Err("format_timeout"),
            ShellCommandExecutionState::OutcomeUnknown => return Err("format_execution_unknown"),
        }
        if result.stdout_truncated || result.stderr_truncated {
            return Err("format_output_truncated");
        }
        if result.result.exit_code != Some(0) || result.result.error.is_some() {
            return Err("format_process_failed");
        }
        let stdout = result.result.stdout.ok_or("format_process_failed")?;
        total_output = total_output
            .checked_add(stdout.len())
            .ok_or("format_candidate_too_large")?;
        if stdout.len() > PROJECT_FORMAT_FILE_MAX_BYTES
            || total_output > PROJECT_FORMAT_TOTAL_MAX_BYTES
        {
            return Err("format_candidate_too_large");
        }
        if started.elapsed() > total_budget {
            return Err("format_timeout");
        }
        candidates.push(stdout);
    }
    Ok(candidates)
}
