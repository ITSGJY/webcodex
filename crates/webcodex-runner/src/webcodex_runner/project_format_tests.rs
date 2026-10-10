use super::config::{RunnerPolicy, ShellConfig};
use super::project_format::{format_candidates, handle, plan, replan, PlannedFormat};
use super::shell::PreparedShellProfileCache;
use std::fs;
#[cfg(unix)]
use std::path::Path;
use std::path::PathBuf;
use webcodex_core::project_format::*;

const MANIFEST: &str = "[package]\nname='demo'\nversion='0.1.0'\nedition='2021'\n";

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    registry: PathBuf,
    policy: RunnerPolicy,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("project");
        let registry = temp.path().join("registry");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir(&registry).unwrap();
        fs::write(root.join("Cargo.toml"), MANIFEST).unwrap();
        fs::write(root.join("src/main.rs"), "fn main( ){ }\n").unwrap();
        let fixture = Self {
            _temp: temp,
            root: root.clone(),
            registry,
            policy: RunnerPolicy {
                allowed_roots: vec![root],
                ..Default::default()
            },
        };
        fixture.register(true);
        fixture
    }

    fn register(&self, allow_patch: bool) {
        fs::write(
            self.registry.join("demo.toml"),
            format!(
                "id='demo'\npath={}\nallow_patch={allow_patch}\n",
                serde_json::to_string(self.root.to_str().unwrap()).unwrap(),
            ),
        )
        .unwrap();
    }

    fn request(&self) -> ProjectFormatRequest {
        ProjectFormatRequest {
            project_id: "demo".into(),
            cwd: None,
            adapter: ProjectFormatAdapter::Auto,
            files: vec!["src/main.rs".into()],
        }
    }

    fn plan(&self) -> PlannedFormat {
        plan(&self.policy, &self.registry, &self.request()).unwrap()
    }
}

fn rejection(result: Result<PlannedFormat, ProjectFormatPlanningResult>) -> String {
    match result {
        Err(ProjectFormatPlanningResult::Unavailable { code }) => code,
        _ => panic!("expected unavailable format plan"),
    }
}

#[test]
fn project_format_plan_keeps_source_private_and_replans_exact_bytes() {
    let fixture = Fixture::new();
    let original = fixture.plan();
    assert_eq!(original.sources, ["fn main( ){ }\n"]);
    assert_eq!(original.root, fixture.root.canonicalize().unwrap());
    assert_eq!(original.cwd, original.root);
    assert!(original.plan.is_valid());
    assert!(!serde_json::to_string(&original.plan)
        .unwrap()
        .contains("fn main"));
    assert!(replan(&fixture.policy, &fixture.registry, &original.plan).is_ok());
    fs::write(fixture.root.join("src/main.rs"), "fn main() {}\n").unwrap();
    assert!(matches!(
        replan(&fixture.policy, &fixture.registry, &original.plan),
        Err("format_plan_stale")
    ));
    let updated = fixture.plan();
    fs::write(
        fixture.root.join("Cargo.toml"),
        MANIFEST.replace("2021", "2024"),
    )
    .unwrap();
    assert!(replan(&fixture.policy, &fixture.registry, &updated.plan).is_err());
}

#[test]
fn project_format_replan_rejects_same_path_replacement_with_identical_bytes() {
    let fixture = Fixture::new();
    let original = fixture.plan();
    let retired = fixture.root.with_extension("retired");
    fs::rename(&fixture.root, &retired).unwrap();
    fs::create_dir_all(fixture.root.join("src")).unwrap();
    fs::write(fixture.root.join("Cargo.toml"), MANIFEST).unwrap();
    fs::write(fixture.root.join("src/main.rs"), &original.sources[0]).unwrap();
    let replacement = fixture.plan();
    assert_ne!(replacement.plan.root_digest, original.plan.root_digest);
    assert!(replan(&fixture.policy, &fixture.registry, &original.plan).is_err());
}

#[test]
fn project_format_requires_exact_enabled_writable_project_and_process_policy() {
    let mut fixture = Fixture::new();
    let mut unknown = fixture.request();
    unknown.project_id = "other".into();
    assert_eq!(
        rejection(plan(&fixture.policy, &fixture.registry, &unknown)),
        "unknown_project"
    );
    fixture.register(false);
    assert_eq!(
        rejection(plan(&fixture.policy, &fixture.registry, &fixture.request())),
        "permission_denied"
    );
    fixture.register(true);
    fixture.policy.allow_raw_shell = false;
    assert_eq!(
        rejection(plan(&fixture.policy, &fixture.registry, &fixture.request())),
        "permission_denied"
    );
}

#[test]
fn project_format_rejects_cross_recipe_and_ambiguous_roots_without_adapter_fallback() {
    let fixture = Fixture::new();
    fs::create_dir(fixture.root.join("nested")).unwrap();
    fs::write(fixture.root.join("nested/Cargo.toml"), MANIFEST).unwrap();
    fs::write(fixture.root.join("nested/main.rs"), "fn main() {}\n").unwrap();
    let mut request = fixture.request();
    request.files.push("nested/main.rs".into());
    assert_eq!(
        rejection(plan(&fixture.policy, &fixture.registry, &request)),
        "format_scope_mismatch"
    );
    request.files.remove(0);
    request.cwd = Some("nested".into());
    assert_eq!(
        plan(&fixture.policy, &fixture.registry, &request)
            .unwrap()
            .plan
            .recipe_root,
        "nested"
    );
    fs::write(
        fixture.root.join("nested/pyproject.toml"),
        "[tool.ruff]\ntarget-version='py311'\n",
    )
    .unwrap();
    request.adapter = ProjectFormatAdapter::Rust;
    assert_eq!(
        rejection(plan(&fixture.policy, &fixture.registry, &request)),
        "format_recipe_ambiguous"
    );
}

#[test]
fn project_format_invalid_nested_markers_never_fall_back_to_parent() {
    let fixture = Fixture::new();
    fs::create_dir(fixture.root.join("src/Cargo.toml")).unwrap();
    assert_eq!(
        rejection(plan(&fixture.policy, &fixture.registry, &fixture.request())),
        "format_manifest_invalid"
    );
}

#[test]
fn project_format_rejects_custom_config_inherited_edition_and_large_inputs() {
    let fixture = Fixture::new();
    fs::write(fixture.root.join("rustfmt.toml"), "max_width=80\n").unwrap();
    assert_eq!(
        rejection(plan(&fixture.policy, &fixture.registry, &fixture.request())),
        "format_config_unsupported"
    );
    fs::remove_file(fixture.root.join("rustfmt.toml")).unwrap();
    fs::write(
        fixture.root.join("Cargo.toml"),
        "[package]\nname='demo'\nedition.workspace=true\n",
    )
    .unwrap();
    assert_eq!(
        rejection(plan(&fixture.policy, &fixture.registry, &fixture.request())),
        "format_manifest_unsupported"
    );
    fs::write(fixture.root.join("Cargo.toml"), MANIFEST).unwrap();
    fs::write(
        fixture.root.join("src/main.rs"),
        vec![b' '; PROJECT_FORMAT_FILE_MAX_BYTES + 1],
    )
    .unwrap();
    assert_eq!(
        rejection(plan(&fixture.policy, &fixture.registry, &fixture.request())),
        "format_input_too_large"
    );
}

#[test]
fn project_format_bounds_total_bytes_and_rejects_invalid_utf8() {
    let fixture = Fixture::new();
    let mut request = fixture.request();
    request.files.clear();
    for index in 0..5 {
        let path = format!("src/file{index}.rs");
        fs::write(
            fixture.root.join(&path),
            vec![b' '; PROJECT_FORMAT_FILE_MAX_BYTES],
        )
        .unwrap();
        request.files.push(path);
    }
    assert_eq!(
        rejection(plan(&fixture.policy, &fixture.registry, &request)),
        "format_input_too_large"
    );
    fs::write(fixture.root.join("src/main.rs"), [0xff]).unwrap();
    assert_eq!(
        rejection(plan(&fixture.policy, &fixture.registry, &fixture.request())),
        "format_invalid_utf8"
    );
}

#[cfg(feature = "runner-real-process-tests")]
#[test]
#[ignore = "requires installed rustfmt; exercises the owned native process boundary"]
fn runner_real_process_project_format_rustfmt_stdin_leaves_sources_untouched() {
    use super::config::ShellConfig;
    use super::shell::{run_process_with_profiles_and_execution_state, PreparedShellProfileCache};
    let fixture = Fixture::new();
    fs::write(
        fixture.root.join("src/main.rs"),
        "mod child;\nfn main( ){ }\n",
    )
    .unwrap();
    fs::write(fixture.root.join("child.rs"), "fn child( ){ }\n").unwrap();
    let planned = fixture.plan();
    let command = planned.plan.profile.process("src/main.rs").unwrap();
    let result = run_process_with_profiles_and_execution_state(
        1,
        &fixture.policy,
        &ShellConfig::default(),
        &fixture.registry,
        &PreparedShellProfileCache::default(),
        planned.cwd.to_str(),
        &command.executable,
        &command.args,
        Some(&planned.sources[0]),
        10,
        None,
    );
    assert_eq!(
        result.result.exit_code,
        Some(0),
        "{:?}",
        result.result.error
    );
    assert_eq!(
        result.result.stdout.as_deref(),
        Some("mod child;\nfn main() {}\n")
    );
    assert!(!result.stdout_truncated && !result.stderr_truncated);
    assert!(result
        .result
        .stderr
        .as_deref()
        .unwrap_or_default()
        .is_empty());
    assert_eq!(
        fs::read_to_string(fixture.root.join("src/main.rs")).unwrap(),
        planned.sources[0]
    );
    assert_eq!(
        fs::read_to_string(fixture.root.join("child.rs")).unwrap(),
        "fn child( ){ }\n"
    );
}

#[test]
fn project_format_python_pins_local_target_and_rejects_extend() {
    let fixture = Fixture::new();
    fs::remove_file(fixture.root.join("Cargo.toml")).unwrap();
    fs::write(fixture.root.join("src/main.py"), "x=1\n").unwrap();
    let mut request = fixture.request();
    request.adapter = ProjectFormatAdapter::Python;
    request.files = vec!["src/main.py".into()];
    for manifest in [
        "[tool.ruff]\n",
        "[tool.ruff]\ntarget-version='ambient'\n",
        "[tool.ruff]\ntarget-version='py311'\nextend='other.toml'\n",
    ] {
        fs::write(fixture.root.join("pyproject.toml"), manifest).unwrap();
        assert_eq!(
            rejection(plan(&fixture.policy, &fixture.registry, &request)),
            "format_manifest_invalid"
        );
    }
    fs::write(
        fixture.root.join("pyproject.toml"),
        "[tool.ruff]\ntarget-version='py311'\n",
    )
    .unwrap();
    let planned = plan(&fixture.policy, &fixture.registry, &request).unwrap();
    assert_eq!(
        planned.plan.profile,
        ProjectFormatProfile::Python {
            target_version: "py311".into()
        }
    );
    // A valid static profile is not evidence of installed formatter support.
    let result = handle(&fixture.policy, &fixture.registry, &request);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(result.stdout.as_deref().unwrap()).unwrap()
            ["code"],
        "format_execution_unavailable"
    );
}

#[cfg(unix)]
#[test]
fn project_format_rejects_symlink_hardlink_and_fifo_inputs() {
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new();
    let mut request = fixture.request();
    symlink("main.rs", fixture.root.join("src/link.rs")).unwrap();
    request.files = vec!["src/link.rs".into()];
    assert_eq!(
        rejection(plan(&fixture.policy, &fixture.registry, &request)),
        "format_path_invalid"
    );
    fs::hard_link(
        fixture.root.join("src/main.rs"),
        fixture.root.join("src/alias.rs"),
    )
    .unwrap();
    request.files = vec!["src/main.rs".into()];
    // Even a single selected path must not mutate an inode with other aliases.
    assert_eq!(
        rejection(plan(&fixture.policy, &fixture.registry, &request)),
        "format_alias_unavailable"
    );
    let fifo = fixture.root.join("src/pipe.rs");
    make_fifo(&fifo);
    request.files = vec!["src/pipe.rs".into()];
    assert_eq!(
        rejection(plan(&fixture.policy, &fixture.registry, &request)),
        "format_path_invalid"
    );
    // Deterministically replace a checked regular leaf at the protected open
    // boundary. Opening the FIFO must return an error without waiting for stdin.
    let target = fixture.root.canonicalize().unwrap().join("src/main.rs");
    let opened = super::file_access::open_regular_file_unix(&target, || {
        fs::remove_file(&target).unwrap();
        make_fifo(&target);
    });
    assert!(opened.is_err());
}

#[cfg(unix)]
fn make_fifo(path: &Path) {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
}

#[test]
fn project_format_candidates_returns_formatted_output_and_unchanged_detection() {
    let fixture = Fixture::new();
    let planned = fixture.plan();
    let shell = ShellConfig::default();
    let cache = PreparedShellProfileCache::default();
    let candidates = format_candidates(
        &fixture.policy,
        &shell,
        &fixture.registry,
        &planned,
        &cache,
        1,
        10,
        None,
    )
    .expect("dry-run rustfmt candidate formatting succeeded");
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0], "fn main() {}\n");
    assert_ne!(candidates[0], planned.sources[0]);

    // Already formatted -> candidate equals original source
    fs::write(fixture.root.join("src/main.rs"), "fn main() {}\n").unwrap();
    let planned_formatted = fixture.plan();
    let candidates_formatted = format_candidates(
        &fixture.policy,
        &shell,
        &fixture.registry,
        &planned_formatted,
        &cache,
        1,
        10,
        None,
    )
    .expect("dry-run formatting formatted source succeeds");
    assert_eq!(candidates_formatted.len(), 1);
    assert_eq!(candidates_formatted[0], planned_formatted.sources[0]);

    // Original file content was untouched by candidate dry-run
    assert_eq!(
        fs::read_to_string(fixture.root.join("src/main.rs")).unwrap(),
        "fn main() {}\n"
    );
}

#[test]
fn project_format_candidates_fails_on_nonzero_exit_syntax_error() {
    let fixture = Fixture::new();
    fs::write(fixture.root.join("src/main.rs"), "fn broken syntax {{{ \n").unwrap();
    let planned = fixture.plan();
    let shell = ShellConfig::default();
    let cache = PreparedShellProfileCache::default();
    let result = format_candidates(
        &fixture.policy,
        &shell,
        &fixture.registry,
        &planned,
        &cache,
        1,
        10,
        None,
    );
    assert_eq!(result, Err("format_process_failed"));
}

#[test]
fn project_format_candidates_fails_on_truncated_output() {
    let fixture = Fixture::new();
    let planned = fixture.plan();
    let mut policy = fixture.policy.clone();
    // Set max_output_bytes very low so the formatter output triggers truncation
    policy.max_output_bytes = 4;
    let shell = ShellConfig::default();
    let cache = PreparedShellProfileCache::default();
    let result = format_candidates(
        &policy,
        &shell,
        &fixture.registry,
        &planned,
        &cache,
        1,
        10,
        None,
    );
    assert_eq!(result, Err("format_output_truncated"));
}

#[test]
fn project_format_candidates_fails_on_missing_rustfmt() {
    let fixture = Fixture::new();
    let planned = fixture.plan();
    let mut shell = ShellConfig::default();
    // Restrict PATH to an empty directory so rustfmt cannot be located
    shell
        .env
        .insert("PATH".into(), "/nonexistent_bin_path_12345".into());
    shell.environment_mode = super::config::ShellEnvironmentMode::Isolated;
    let cache = PreparedShellProfileCache::default();
    let result = format_candidates(
        &fixture.policy,
        &shell,
        &fixture.registry,
        &planned,
        &cache,
        1,
        10,
        None,
    );
    assert_eq!(result, Err("format_process_failed"));
}

#[test]
fn project_format_candidates_enforces_deadline_and_stop_before_dispatch() {
    let fixture = Fixture::new();
    let planned = fixture.plan();
    let shell = ShellConfig::default();
    let cache = PreparedShellProfileCache::default();
    let cancel = std::sync::atomic::AtomicBool::new(true);
    let cancelled = format_candidates(
        &fixture.policy,
        &shell,
        &fixture.registry,
        &planned,
        &cache,
        1,
        10,
        Some(&cancel),
    );
    assert_eq!(cancelled, Err("format_cancelled"));
    let invalid_budget = format_candidates(
        &fixture.policy,
        &shell,
        &fixture.registry,
        &planned,
        &cache,
        1,
        0,
        None,
    );
    assert_eq!(invalid_budget, Err("format_timeout_invalid"));
    assert_eq!(
        fs::read_to_string(fixture.root.join("src/main.rs")).unwrap(),
        planned.sources[0]
    );
}
