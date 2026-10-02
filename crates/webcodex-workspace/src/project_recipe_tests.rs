use super::project_recipe::*;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;

fn write(root: &Path, path: &str, content: &str) {
    let path = root.join(path);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, content).unwrap();
}

#[test]
fn resolves_each_recipe_marker_and_nearest_nested_root() {
    for (marker, expected) in [
        ("Cargo.toml", ProjectRecipeId::Rust),
        ("package.json", ProjectRecipeId::Node),
        ("pyproject.toml", ProjectRecipeId::Python),
        ("go.mod", ProjectRecipeId::Go),
    ] {
        let temp = tempfile::tempdir().unwrap();
        write(temp.path(), marker, "");
        let resolved = resolve_project_recipe_root(temp.path(), None, None).unwrap();
        assert_eq!(resolved.recipe, expected);
        assert_eq!(resolved.relative_root, ".");
    }

    let temp = tempfile::tempdir().unwrap();
    write(temp.path(), "Cargo.toml", "");
    write(temp.path(), "nested/go.mod", "module example.test/nested\n");
    let resolved = resolve_project_recipe_root(temp.path(), Some("nested"), None).unwrap();
    assert_eq!(resolved.recipe, ProjectRecipeId::Go);
    assert_eq!(resolved.relative_root, "nested");
}

#[test]
fn ambiguity_is_sorted_but_explicit_mismatch_preserves_catalog_order() {
    let temp = tempfile::tempdir().unwrap();
    write(temp.path(), "Cargo.toml", "");
    write(temp.path(), "package.json", "{}");
    write(temp.path(), "go.mod", "module example.test/root\n");

    let ambiguous = resolve_project_recipe_root(temp.path(), None, None).unwrap_err();
    assert_eq!(
        ambiguous,
        ProjectRecipeResolutionError::Ambiguous {
            recipe_root: ".".into(),
            candidates: vec![
                ProjectRecipeId::Go,
                ProjectRecipeId::Node,
                ProjectRecipeId::Rust,
            ],
        }
    );

    let python =
        resolve_project_recipe_root(temp.path(), None, Some(ProjectRecipeId::Python)).unwrap();
    assert_eq!(python.recipe, ProjectRecipeId::Python);
    assert_eq!(python.relative_root, ".");

    let node = resolve_project_recipe_root(temp.path(), None, Some(ProjectRecipeId::Node)).unwrap();
    assert_eq!(node.recipe, ProjectRecipeId::Node);

    fs::remove_file(temp.path().join("package.json")).unwrap();
    let mismatch =
        resolve_project_recipe_root(temp.path(), None, Some(ProjectRecipeId::Node)).unwrap_err();
    assert_eq!(
        mismatch,
        ProjectRecipeResolutionError::ExplicitMismatch {
            recipe_root: ".".into(),
            expected: ProjectRecipeId::Node,
            candidates: vec![ProjectRecipeId::Rust, ProjectRecipeId::Go],
        }
    );
}

#[test]
fn explicit_not_found_and_unhinted_not_found_remain_distinct() {
    let temp = tempfile::tempdir().unwrap();
    assert_eq!(
        resolve_project_recipe_root(temp.path(), None, Some(ProjectRecipeId::Rust)).unwrap_err(),
        ProjectRecipeResolutionError::ExplicitNotFound {
            expected: ProjectRecipeId::Rust
        }
    );
    assert_eq!(
        resolve_project_recipe_root(temp.path(), None, None).unwrap_err(),
        ProjectRecipeResolutionError::NotFound
    );
}

#[test]
fn cwd_validation_preserves_existing_recipe_predicate() {
    let temp = tempfile::tempdir().unwrap();
    write(temp.path(), "Cargo.toml", "");
    for cwd in ["", "../outside", "/tmp", "nul\0byte"] {
        assert_eq!(
            resolve_project_recipe_root(temp.path(), Some(cwd), None).unwrap_err(),
            ProjectRecipeResolutionError::CwdMismatch,
            "{cwd:?}"
        );
    }
}

#[cfg(unix)]
#[test]
fn cwd_symlink_escape_is_rejected() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    write(outside.path(), "Cargo.toml", "");
    symlink(outside.path(), root.path().join("outside")).unwrap();
    assert_eq!(
        resolve_project_recipe_root(root.path(), Some("outside"), None).unwrap_err(),
        ProjectRecipeResolutionError::CwdMismatch
    );
}

#[test]
fn explicit_python_keeps_manifestless_cwd_even_with_other_markers() {
    let temp = tempfile::tempdir().unwrap();
    write(temp.path(), "Cargo.toml", "");
    write(
        temp.path(),
        "nested/calculator.py",
        "def add(a, b): return a + b\n",
    );
    let resolved =
        resolve_project_recipe_root(temp.path(), Some("nested"), Some(ProjectRecipeId::Python))
            .unwrap();
    assert_eq!(resolved.recipe, ProjectRecipeId::Python);
    assert_eq!(resolved.relative_root, "nested");
    assert_eq!(resolved.absolute_root, temp.path().join("nested"));
}

#[test]
fn source_reads_anchor_relative_paths_to_execution_root() {
    let temp = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    write(temp.path(), "Cargo.toml", "root-manifest\n");
    write(outside.path(), "Cargo.toml", "outside\n");
    let root = temp.path().canonicalize().unwrap();
    assert_eq!(
        read_project_recipe_file(&root, Path::new("Cargo.toml")).unwrap(),
        b"root-manifest\n"
    );
    assert_eq!(
        read_project_recipe_file(&root, &root.join("Cargo.toml")).unwrap(),
        b"root-manifest\n"
    );
    assert_eq!(
        read_project_recipe_file(&root, &outside.path().join("Cargo.toml")).unwrap_err(),
        ProjectRecipeResolutionError::SourceFileInvalid
    );
    fs::create_dir(root.join("not-a-file")).unwrap();
    assert_eq!(
        read_project_recipe_file(&root, Path::new("not-a-file")).unwrap_err(),
        ProjectRecipeResolutionError::SourceFileInvalid
    );
}

#[cfg(unix)]
#[test]
fn marker_symlink_is_detected_before_source_containment_rejects_it() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    write(
        outside.path(),
        "Cargo.toml",
        "[package]\nname='outside'\nversion='0.1.0'\n",
    );
    symlink(
        outside.path().join("Cargo.toml"),
        root.path().join("Cargo.toml"),
    )
    .unwrap();

    let resolved = resolve_project_recipe_root(root.path(), None, None).unwrap();
    assert_eq!(resolved.recipe, ProjectRecipeId::Rust);
    assert_eq!(
        read_project_recipe_file(&resolved.execution_root, &resolved.marker_path()).unwrap_err(),
        ProjectRecipeResolutionError::SourceFileInvalid
    );
}

#[test]
fn source_digest_preserves_path_length_content_order_and_skips_missing_optional_files() {
    let temp = tempfile::tempdir().unwrap();
    write(temp.path(), "Cargo.toml", "manifest\n");
    write(temp.path(), "Cargo.lock", "lock\n");
    let root = temp.path().canonicalize().unwrap();

    let actual = digest_project_recipe_files(
        &root,
        [
            Path::new("Cargo.toml").to_path_buf(),
            Path::new("missing.lock").to_path_buf(),
            Path::new("Cargo.lock").to_path_buf(),
        ],
    )
    .unwrap();

    let mut hasher = Sha256::new();
    for (path, content) in [
        ("Cargo.toml", b"manifest\n".as_slice()),
        ("Cargo.lock", b"lock\n".as_slice()),
    ] {
        hasher.update(Path::new(path).as_os_str().as_encoded_bytes());
        hasher.update((content.len() as u64).to_be_bytes());
        hasher.update(content);
    }
    assert_eq!(actual, format!("{:x}", hasher.finalize()));
}

#[test]
fn project_recipe_provenance_files_preserve_flat_rust_go_source_truth() {
    for (marker, manifest, recipe, dependency) in [
        (
            "Cargo.toml",
            "[package]\nname='demo'\nversion='0.1.0'\n",
            ProjectRecipeId::Rust,
            "Cargo.lock",
        ),
        (
            "go.mod",
            "module example.test/demo\n",
            ProjectRecipeId::Go,
            "go.sum",
        ),
    ] {
        let temp = tempfile::tempdir().unwrap();
        write(temp.path(), marker, manifest);
        let resolved = resolve_project_recipe_root(temp.path(), None, Some(recipe)).unwrap();
        let root = temp.path().canonicalize().unwrap();
        assert_eq!(
            project_recipe_provenance_files(&resolved).unwrap().unwrap(),
            [root.join(marker), root.join(dependency)]
        );
    }

    for (marker, recipe) in [
        ("package.json", ProjectRecipeId::Node),
        ("pyproject.toml", ProjectRecipeId::Python),
    ] {
        let temp = tempfile::tempdir().unwrap();
        write(temp.path(), marker, "");
        let resolved = resolve_project_recipe_root(temp.path(), None, Some(recipe)).unwrap();
        assert!(project_recipe_provenance_files(&resolved)
            .unwrap()
            .is_none());
    }
}

#[test]
fn rust_member_provenance_includes_workspace_manifest_and_root_lockfile() {
    let temp = tempfile::tempdir().unwrap();
    write(
        temp.path(),
        "Cargo.toml",
        "[workspace]\nmembers=['member']\nresolver='2'\n[workspace.package]\nversion='0.1.0'\nedition='2021'\n",
    );
    write(
        temp.path(),
        "member/Cargo.toml",
        "[package]\nname='member'\nversion.workspace=true\nedition.workspace=true\n",
    );
    let resolved =
        resolve_project_recipe_root(temp.path(), Some("member"), Some(ProjectRecipeId::Rust))
            .unwrap();
    let root = temp.path().canonicalize().unwrap();

    assert_eq!(resolved.absolute_root, root.join("member"));
    assert_eq!(
        project_recipe_provenance_files(&resolved).unwrap().unwrap(),
        [
            root.join("member/Cargo.toml"),
            root.join("Cargo.toml"),
            root.join("Cargo.lock"),
        ]
    );
}

#[test]
fn rust_member_workspace_inheritance_cannot_silently_escape_project_boundary() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("member");
    write(
        &project,
        "Cargo.toml",
        "[package]\nname='member'\nversion.workspace=true\nedition.workspace=true\n[dependencies]\nserde.workspace=true\n",
    );
    let resolved =
        resolve_project_recipe_root(&project, None, Some(ProjectRecipeId::Rust)).unwrap();

    assert_eq!(
        project_recipe_provenance_files(&resolved).unwrap_err(),
        ProjectRecipeResolutionError::SourceFileInvalid
    );

    // Arbitrary package metadata is not Cargo workspace inheritance.
    write(
        &project,
        "Cargo.toml",
        "[package]\nname='member'\nversion='0.1.0'\nedition='2021'\n[package.metadata]\nworkspace=true\n",
    );
    let standalone =
        resolve_project_recipe_root(&project, None, Some(ProjectRecipeId::Rust)).unwrap();
    assert!(project_recipe_provenance_files(&standalone).is_ok());
}

#[test]
fn rust_member_explicit_workspace_path_selects_contained_workspace_sources() {
    let temp = tempfile::tempdir().unwrap();
    write(
        temp.path(),
        "workspace/Cargo.toml",
        "[workspace]\nmembers=['../member']\nresolver='2'\n",
    );
    write(
        temp.path(),
        "member/Cargo.toml",
        "[package]\nname='member'\nversion='0.1.0'\nworkspace='../workspace'\n",
    );
    let resolved =
        resolve_project_recipe_root(temp.path(), Some("member"), Some(ProjectRecipeId::Rust))
            .unwrap();
    let root = temp.path().canonicalize().unwrap();

    assert_eq!(
        project_recipe_provenance_files(&resolved).unwrap().unwrap(),
        [
            root.join("member/Cargo.toml"),
            root.join("workspace/Cargo.toml"),
            root.join("workspace/Cargo.lock"),
        ]
    );
}

#[test]
fn rust_member_explicit_workspace_path_cannot_escape_execution_root() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    let outside = temp.path().join("outside");
    fs::create_dir_all(&project).unwrap();
    fs::create_dir_all(&outside).unwrap();
    write(
        &project,
        "member/Cargo.toml",
        "[package]\nname='member'\nversion='0.1.0'\nworkspace='../../outside'\n",
    );
    write(
        &outside,
        "Cargo.toml",
        "[workspace]\nmembers=['../project/member']\nresolver='2'\n",
    );
    let resolved =
        resolve_project_recipe_root(&project, Some("member"), Some(ProjectRecipeId::Rust)).unwrap();

    assert_eq!(
        project_recipe_provenance_files(&resolved).unwrap_err(),
        ProjectRecipeResolutionError::SourceFileInvalid
    );
}

#[test]
fn cargo_all_packages_provenance_tracks_explicit_member_beneath_target() {
    let temp = tempfile::tempdir().unwrap();
    write(
        temp.path(),
        "Cargo.toml",
        "[workspace]\nmembers=['target/member']\nresolver='2'\n",
    );
    write(
        temp.path(),
        "target/member/Cargo.toml",
        "[package]\nname='member'\nversion='0.1.0'\nedition='2021'\n",
    );
    let resolved =
        resolve_project_recipe_root(temp.path(), None, Some(ProjectRecipeId::Rust)).unwrap();

    let before = digest_project_cargo_all_packages_provenance(&resolved).unwrap();
    write(
        temp.path(),
        "target/member/Cargo.toml",
        "[package]\nname='member'\nversion='0.1.0'\nedition='2024'\n",
    );
    let after = digest_project_cargo_all_packages_provenance(&resolved).unwrap();

    assert_ne!(before, after);
}

#[cfg(unix)]
#[test]
fn cargo_all_packages_provenance_tracks_member_symlink_retarget() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().unwrap();
    write(
        temp.path(),
        "Cargo.toml",
        "[workspace]\nmembers=['selected']\nresolver='2'\n",
    );
    for package in ["a", "b"] {
        write(
            temp.path(),
            &format!("{package}/Cargo.toml"),
            "[package]\nname='member'\nversion='0.1.0'\nedition='2021'\n",
        );
    }
    symlink("a", temp.path().join("selected")).unwrap();
    let resolved =
        resolve_project_recipe_root(temp.path(), None, Some(ProjectRecipeId::Rust)).unwrap();

    let before = digest_project_cargo_all_packages_provenance(&resolved).unwrap();
    fs::remove_file(temp.path().join("selected")).unwrap();
    symlink("b", temp.path().join("selected")).unwrap();
    let after = digest_project_cargo_all_packages_provenance(&resolved).unwrap();

    assert_ne!(before, after);
}

#[cfg(unix)]
#[test]
fn cargo_all_packages_explicit_member_symlink_escape_fails_closed() {
    use std::os::unix::fs::symlink;

    let project = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    write(
        project.path(),
        "Cargo.toml",
        "[workspace]\nmembers=['target/member']\nresolver='2'\n",
    );
    write(
        outside.path(),
        "Cargo.toml",
        "[package]\nname='outside'\nversion='0.1.0'\nedition='2021'\n",
    );
    fs::create_dir_all(project.path().join("target")).unwrap();
    symlink(outside.path(), project.path().join("target/member")).unwrap();
    let resolved =
        resolve_project_recipe_root(project.path(), None, Some(ProjectRecipeId::Rust)).unwrap();

    assert_eq!(
        digest_project_cargo_all_packages_provenance(&resolved).unwrap_err(),
        ProjectRecipeResolutionError::SourceFileInvalid
    );
}

#[test]
fn cargo_all_packages_glob_ignores_matching_plain_files() {
    let temp = tempfile::tempdir().unwrap();
    write(
        temp.path(),
        "Cargo.toml",
        "[workspace]\nmembers=['crates/*']\nresolver='2'\n",
    );
    write(
        temp.path(),
        "crates/a/Cargo.toml",
        "[package]\nname='a'\nversion='0.1.0'\nedition='2021'\n",
    );
    write(temp.path(), "crates/README.md", "note one\n");
    let resolved =
        resolve_project_recipe_root(temp.path(), None, Some(ProjectRecipeId::Rust)).unwrap();

    let before = digest_project_cargo_all_packages_provenance(&resolved).unwrap();
    write(temp.path(), "crates/README.md", "note two\n");
    let after = digest_project_cargo_all_packages_provenance(&resolved).unwrap();
    assert_eq!(before, after);
}

#[test]
fn cargo_all_packages_recursive_glob_respects_component_separators() {
    let temp = tempfile::tempdir().unwrap();
    write(
        temp.path(),
        "Cargo.toml",
        "[workspace]
members=['target/**/p?g']
resolver='2'
",
    );
    write(
        temp.path(),
        "target/a/pkg/Cargo.toml",
        "[package]
name='pkg'
version='0.1.0'
edition='2021'
",
    );
    write(
        temp.path(),
        "target/a/p/g/Cargo.toml",
        "[package]
name='not-selected'
version='0.1.0'
edition='2021'
",
    );
    let resolved =
        resolve_project_recipe_root(temp.path(), None, Some(ProjectRecipeId::Rust)).unwrap();

    let before = digest_project_cargo_all_packages_provenance(&resolved).unwrap();
    write(
        temp.path(),
        "target/a/p/g/Cargo.toml",
        "[package]
name='not-selected'
version='0.1.0'
edition='2024'
",
    );
    let unselected_changed = digest_project_cargo_all_packages_provenance(&resolved).unwrap();

    // The broad scan intentionally skips target/. The topology witness must
    // therefore match Cargo's component-aware glob semantics and ignore p/g.
    assert_eq!(before, unselected_changed);

    write(
        temp.path(),
        "target/a/pkg/Cargo.toml",
        "[package]
name='pkg'
version='0.1.0'
edition='2024'
",
    );
    let selected_changed = digest_project_cargo_all_packages_provenance(&resolved).unwrap();
    assert_ne!(unselected_changed, selected_changed);
}

#[test]
fn cargo_all_packages_provenance_tracks_glob_membership_beneath_target() {
    let temp = tempfile::tempdir().unwrap();
    write(
        temp.path(),
        "Cargo.toml",
        "[workspace]\nmembers=['target/*']\nresolver='2'\n",
    );
    write(
        temp.path(),
        "target/a/Cargo.toml",
        "[package]\nname='a'\nversion='0.1.0'\nedition='2021'\n",
    );
    let resolved =
        resolve_project_recipe_root(temp.path(), None, Some(ProjectRecipeId::Rust)).unwrap();

    let before = digest_project_cargo_all_packages_provenance(&resolved).unwrap();
    write(
        temp.path(),
        "target/b/Cargo.toml",
        "[package]\nname='b'\nversion='0.1.0'\nedition='2021'\n",
    );
    let after = digest_project_cargo_all_packages_provenance(&resolved).unwrap();

    assert_ne!(before, after);
}

#[cfg(unix)]
#[test]
fn cargo_all_packages_glob_symlink_alias_tracks_member_manifest() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().unwrap();
    write(
        temp.path(),
        "Cargo.toml",
        "[workspace]\nmembers=['target/0*/pkg']\nresolver='2'\n",
    );
    write(
        temp.path(),
        "target/zreal/pkg/Cargo.toml",
        "[package]\nname='aliased_pkg'\nversion='0.1.0'\nedition='2021'\n",
    );
    symlink("zreal", temp.path().join("target/0link")).unwrap();
    let resolved =
        resolve_project_recipe_root(temp.path(), None, Some(ProjectRecipeId::Rust)).unwrap();

    let before = digest_project_cargo_all_packages_provenance(&resolved).unwrap();
    write(
        temp.path(),
        "target/zreal/pkg/Cargo.toml",
        "[package]\nname='aliased_pkg'\nversion='0.1.0'\nedition='2024'\n",
    );
    let after = digest_project_cargo_all_packages_provenance(&resolved).unwrap();

    assert_ne!(before, after);
}

#[test]
fn cargo_all_packages_supports_single_package_without_workspace_table() {
    let temp = tempfile::tempdir().unwrap();
    write(
        temp.path(),
        "Cargo.toml",
        "[package]\nname='single'\nversion='0.1.0'\nedition='2021'\n",
    );
    let resolved =
        resolve_project_recipe_root(temp.path(), None, Some(ProjectRecipeId::Rust)).unwrap();

    assert!(digest_project_cargo_all_packages_provenance(&resolved).is_ok());
}

#[cfg(unix)]
#[test]
fn cargo_all_packages_path_dependency_symlink_escape_fails_closed() {
    use std::os::unix::fs::symlink;

    let project = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    write(
        project.path(),
        "Cargo.toml",
        "[workspace]\nmembers=['app']\nresolver='2'\n",
    );
    write(
        project.path(),
        "app/Cargo.toml",
        "[package]\nname='app'\nversion='0.1.0'\nedition='2021'\n\n[dependencies]\noutside_dep={path='../vendor/dep'}\n",
    );
    write(
        outside.path(),
        "Cargo.toml",
        "[package]\nname='outside_dep'\nversion='0.1.0'\nedition='2021'\n",
    );
    fs::create_dir_all(project.path().join("vendor")).unwrap();
    symlink(outside.path(), project.path().join("vendor/dep")).unwrap();
    let resolved =
        resolve_project_recipe_root(project.path(), None, Some(ProjectRecipeId::Rust)).unwrap();

    assert_eq!(
        digest_project_cargo_all_packages_provenance(&resolved).unwrap_err(),
        ProjectRecipeResolutionError::SourceFileInvalid
    );
}

#[test]
fn cargo_all_packages_unproven_external_path_membership_fails_closed() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    let outside = temp.path().join("outside");
    fs::create_dir_all(&project).unwrap();
    fs::create_dir_all(&outside).unwrap();
    write(
        &project,
        "Cargo.toml",
        "[workspace]\nmembers=['app']\nresolver='2'\n",
    );
    write(
        &project,
        "app/Cargo.toml",
        "[package]\nname='app'\nversion='0.1.0'\nedition='2021'\n\n[dependencies]\noutside_dep={path='../../outside'}\n",
    );
    write(
        &outside,
        "Cargo.toml",
        "[package]\nname='outside_dep'\nversion='0.1.0'\nedition='2021'\n",
    );
    let resolved =
        resolve_project_recipe_root(&project, None, Some(ProjectRecipeId::Rust)).unwrap();

    assert!(project_recipe_provenance_files(&resolved).is_ok());
    assert_eq!(
        digest_project_cargo_all_packages_provenance(&resolved).unwrap_err(),
        ProjectRecipeResolutionError::SourceFileInvalid
    );
}

#[test]
fn cargo_all_packages_unlisted_child_cannot_borrow_workspace_authority() {
    let temp = tempfile::tempdir().unwrap();
    write(
        temp.path(),
        "Cargo.toml",
        "[workspace]\nmembers=['app']\nresolver='2'\n",
    );
    for name in ["app", "tool"] {
        write(
            temp.path(),
            &format!("{name}/Cargo.toml"),
            &format!("[package]\nname='{name}'\nversion='0.1.0'\nedition='2021'\n"),
        );
    }
    let resolved =
        resolve_project_recipe_root(temp.path(), Some("tool"), Some(ProjectRecipeId::Rust))
            .unwrap();

    assert_eq!(
        digest_project_cargo_all_packages_provenance(&resolved).unwrap_err(),
        ProjectRecipeResolutionError::SourceFileInvalid
    );
}

#[test]
fn cargo_all_packages_auto_path_dependency_cwd_is_a_workspace_member() {
    for inherited in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let root_manifest = if inherited {
            "[workspace]\nmembers=['app']\nresolver='2'\n[workspace.dependencies]\ntool={path='tool'}\n"
        } else {
            "[workspace]\nmembers=['app']\nresolver='2'\n"
        };
        write(temp.path(), "Cargo.toml", root_manifest);
        write(
            temp.path(),
            "app/Cargo.toml",
            if inherited {
                "[package]\nname='app'\nversion='0.1.0'\nedition='2021'\n[dependencies]\ntool.workspace=true\n"
            } else {
                "[package]\nname='app'\nversion='0.1.0'\nedition='2021'\n[dependencies]\ntool={path='../tool'}\n"
            },
        );
        write(
            temp.path(),
            "tool/Cargo.toml",
            "[package]\nname='tool'\nversion='0.1.0'\nedition='2021'\n",
        );
        let resolved =
            resolve_project_recipe_root(temp.path(), Some("tool"), Some(ProjectRecipeId::Rust))
                .unwrap();
        assert!(
            digest_project_cargo_all_packages_provenance(&resolved).is_ok(),
            "inherited={inherited}"
        );
    }
}

#[test]
fn cargo_all_packages_provenance_tracks_inherited_workspace_path_dependency() {
    let temp = tempfile::tempdir().unwrap();
    write(
        temp.path(),
        "Cargo.toml",
        "[workspace]\nmembers=['app']\nresolver='2'\n\n[workspace.dependencies]\ndep={path='target/dep'}\n",
    );
    write(
        temp.path(),
        "app/Cargo.toml",
        "[package]\nname='app'\nversion='0.1.0'\nedition='2021'\n\n[dependencies]\ndep.workspace=true\n",
    );
    write(
        temp.path(),
        "target/dep/Cargo.toml",
        "[package]\nname='dep'\nversion='0.1.0'\nedition='2021'\n",
    );
    let resolved =
        resolve_project_recipe_root(temp.path(), None, Some(ProjectRecipeId::Rust)).unwrap();

    let before = digest_project_cargo_all_packages_provenance(&resolved).unwrap();
    write(
        temp.path(),
        "target/dep/Cargo.toml",
        "[package]\nname='dep'\nversion='0.1.0'\nedition='2024'\n",
    );
    let after = digest_project_cargo_all_packages_provenance(&resolved).unwrap();

    assert_ne!(before, after);
}

#[test]
fn cargo_all_packages_provenance_recurses_through_nested_path_dependencies() {
    let temp = tempfile::tempdir().unwrap();
    write(
        temp.path(),
        "Cargo.toml",
        "[workspace]\nmembers=['app']\nresolver='2'\n",
    );
    write(
        temp.path(),
        "app/Cargo.toml",
        "[package]\nname='app'\nversion='0.1.0'\nedition='2021'\n\n[dependencies]\ndep={path='../target/dep'}\n",
    );
    write(
        temp.path(),
        "target/dep/Cargo.toml",
        "[package]\nname='dep'\nversion='0.1.0'\nedition='2021'\n\n[dependencies]\nleaf={path='../leaf'}\n",
    );
    write(
        temp.path(),
        "target/leaf/Cargo.toml",
        "[package]\nname='leaf'\nversion='0.1.0'\nedition='2021'\n",
    );
    let resolved =
        resolve_project_recipe_root(temp.path(), None, Some(ProjectRecipeId::Rust)).unwrap();

    let before = digest_project_cargo_all_packages_provenance(&resolved).unwrap();
    write(
        temp.path(),
        "target/leaf/Cargo.toml",
        "[package]\nname='leaf'\nversion='0.1.0'\nedition='2024'\n",
    );
    let after = digest_project_cargo_all_packages_provenance(&resolved).unwrap();

    assert_ne!(before, after);
}

#[cfg(unix)]
#[test]
fn cargo_all_packages_recursive_glob_cycle_fails_closed() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().unwrap();
    write(
        temp.path(),
        "Cargo.toml",
        "[workspace]\nmembers=['a/**/again/target']\nresolver='2'\n",
    );
    write(
        temp.path(),
        "a/target/Cargo.toml",
        "[package]\nname='member'\nversion='0.1.0'\nedition='2021'\n",
    );
    symlink(".", temp.path().join("a/again")).unwrap();
    let resolved =
        resolve_project_recipe_root(temp.path(), None, Some(ProjectRecipeId::Rust)).unwrap();

    // Silently dropping this route omits a/again/target while the broad scan
    // prunes a/target. Incomplete recursive witnesses must fail closed.
    assert_eq!(
        digest_project_cargo_all_packages_provenance(&resolved).unwrap_err(),
        ProjectRecipeResolutionError::SourceFileInvalid
    );
}

#[cfg(unix)]
#[test]
fn cargo_all_packages_manifest_aliases_share_the_canonical_content_budget() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().unwrap();
    write(
        temp.path(),
        "Cargo.toml",
        "[workspace]\nmembers=['member']\nresolver='2'\n",
    );
    write(
        temp.path(),
        "member/Cargo.toml",
        "[package]\nname='member'\nversion='0.1.0'\nedition='2021'\n",
    );
    for index in 0..256 {
        let directory = temp.path().join(format!("alias-{index}"));
        fs::create_dir_all(&directory).unwrap();
        symlink("../member/Cargo.toml", directory.join("Cargo.toml")).unwrap();
    }
    let resolved =
        resolve_project_recipe_root(temp.path(), None, Some(ProjectRecipeId::Rust)).unwrap();

    let files = project_cargo_all_packages_provenance_files(&resolved).unwrap();
    assert_eq!(
        files.len(),
        3,
        "two canonical manifests and the root lockfile"
    );
    let before = digest_project_cargo_all_packages_provenance(&resolved).unwrap();
    write(
        temp.path(),
        "member/Cargo.toml",
        "[package]\nname='member'\nversion='0.1.0'\nedition='2024'\n",
    );
    assert_ne!(
        before,
        digest_project_cargo_all_packages_provenance(&resolved).unwrap()
    );
}

#[cfg(unix)]
#[test]
fn cargo_all_packages_recursive_glob_fails_closed_before_traversing_outside_project() {
    use std::os::unix::fs::symlink;

    let project = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    write(
        project.path(),
        "Cargo.toml",
        "[workspace]\nmembers=['crates/**/pkg']\nresolver='2'\n",
    );
    write(
        project.path(),
        "crates/local/pkg/Cargo.toml",
        "[package]\nname='local_pkg'\nversion='0.1.0'\nedition='2021'\n",
    );
    write(
        outside.path(),
        "pkg/Cargo.toml",
        "[package]\nname='outside_pkg'\nversion='0.1.0'\nedition='2021'\n",
    );
    fs::create_dir_all(project.path().join("crates")).unwrap();
    let resolved =
        resolve_project_recipe_root(project.path(), None, Some(ProjectRecipeId::Rust)).unwrap();
    assert!(digest_project_cargo_all_packages_provenance(&resolved).is_ok());

    symlink(outside.path(), project.path().join("crates/vendor")).unwrap();
    assert_eq!(
        digest_project_cargo_all_packages_provenance(&resolved).unwrap_err(),
        ProjectRecipeResolutionError::SourceFileInvalid
    );
}

#[cfg(unix)]
#[test]
fn cargo_all_packages_member_manifest_symlink_escape_fails_closed() {
    use std::os::unix::fs::symlink;

    let project = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    write(
        project.path(),
        "Cargo.toml",
        "[workspace]\nmembers=['target/member']\nresolver='2'\n",
    );
    write(
        outside.path(),
        "Cargo.toml",
        "[package]\nname='outside'\nversion='0.1.0'\nedition='2021'\n",
    );
    fs::create_dir_all(project.path().join("target/member")).unwrap();
    symlink(
        outside.path().join("Cargo.toml"),
        project.path().join("target/member/Cargo.toml"),
    )
    .unwrap();
    let resolved =
        resolve_project_recipe_root(project.path(), None, Some(ProjectRecipeId::Rust)).unwrap();

    assert_eq!(
        digest_project_cargo_all_packages_provenance(&resolved).unwrap_err(),
        ProjectRecipeResolutionError::SourceFileInvalid
    );
}

#[cfg(unix)]
#[test]
fn cargo_all_packages_path_dependency_manifest_symlink_escape_fails_closed() {
    use std::os::unix::fs::symlink;

    let project = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    write(
        project.path(),
        "Cargo.toml",
        "[workspace]\nmembers=['app']\nresolver='2'\n",
    );
    write(
        project.path(),
        "app/Cargo.toml",
        "[package]\nname='app'\nversion='0.1.0'\nedition='2021'\n\n[dependencies]\ndep={path='../target/dep'}\n",
    );
    write(
        outside.path(),
        "Cargo.toml",
        "[package]\nname='outside'\nversion='0.1.0'\nedition='2021'\n",
    );
    fs::create_dir_all(project.path().join("target/dep")).unwrap();
    symlink(
        outside.path().join("Cargo.toml"),
        project.path().join("target/dep/Cargo.toml"),
    )
    .unwrap();
    let resolved =
        resolve_project_recipe_root(project.path(), None, Some(ProjectRecipeId::Rust)).unwrap();

    assert_eq!(
        digest_project_cargo_all_packages_provenance(&resolved).unwrap_err(),
        ProjectRecipeResolutionError::SourceFileInvalid
    );
}

#[test]
fn cargo_all_packages_path_dependency_witnesses_are_bounded() {
    let temp = tempfile::tempdir().unwrap();
    write(
        temp.path(),
        "Cargo.toml",
        "[workspace]\nmembers=['app']\nresolver='2'\n",
    );
    let mut manifest =
        String::from("[package]\nname='app'\nversion='0.1.0'\nedition='2021'\n\n[dependencies]\n");
    for index in 0..=1024 {
        manifest.push_str(&format!("dep{index}={{path='../missing/{index}'}}\n"));
    }
    write(temp.path(), "app/Cargo.toml", &manifest);
    let resolved =
        resolve_project_recipe_root(temp.path(), None, Some(ProjectRecipeId::Rust)).unwrap();

    assert_eq!(
        digest_project_cargo_all_packages_provenance(&resolved).unwrap_err(),
        ProjectRecipeResolutionError::SourceFileInvalid
    );
}

#[test]
fn cargo_all_packages_member_glob_accepts_cargo_path_spelling_variants() {
    for pattern in ["./target/*", "target/*/", "target//./*/"] {
        let temp = tempfile::tempdir().unwrap();
        write(
            temp.path(),
            "Cargo.toml",
            &format!("[workspace]\nmembers=['{pattern}']\nresolver='2'\n"),
        );
        write(
            temp.path(),
            "target/a/Cargo.toml",
            "[package]\nname='a'\nversion='0.1.0'\nedition='2021'\n",
        );
        let resolved =
            resolve_project_recipe_root(temp.path(), None, Some(ProjectRecipeId::Rust)).unwrap();

        let before = digest_project_cargo_all_packages_provenance(&resolved).unwrap();
        write(
            temp.path(),
            "target/a/Cargo.toml",
            "[package]\nname='a'\nversion='0.1.0'\nedition='2024'\n",
        );
        let after = digest_project_cargo_all_packages_provenance(&resolved).unwrap();

        assert_ne!(before, after, "member pattern {pattern}");
    }
}

#[cfg(unix)]
#[test]
fn cargo_all_packages_manifest_symlink_uses_logical_member_directory_for_path_dependencies() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().unwrap();
    write(
        temp.path(),
        "Cargo.toml",
        "[workspace]\nmembers=['target/member']\nresolver='2'\n",
    );
    write(
        temp.path(),
        "shared/member.toml",
        "[package]\nname='member'\nversion='0.1.0'\nedition='2021'\n\n[dependencies]\ndep={path='../dep'}\n",
    );
    write(
        temp.path(),
        "target/dep/Cargo.toml",
        "[package]\nname='dep'\nversion='0.1.0'\nedition='2021'\n",
    );
    fs::create_dir_all(temp.path().join("target/member")).unwrap();
    symlink(
        "../../shared/member.toml",
        temp.path().join("target/member/Cargo.toml"),
    )
    .unwrap();
    let resolved =
        resolve_project_recipe_root(temp.path(), None, Some(ProjectRecipeId::Rust)).unwrap();

    let before = digest_project_cargo_all_packages_provenance(&resolved).unwrap();
    write(
        temp.path(),
        "target/dep/Cargo.toml",
        "[package]\nname='dep'\nversion='0.1.0'\nedition='2024'\n",
    );
    let after = digest_project_cargo_all_packages_provenance(&resolved).unwrap();

    assert_ne!(before, after);
}

#[cfg(unix)]
#[test]
fn cargo_all_packages_manifest_symlink_retarget_changes_route_digest() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().unwrap();
    write(
        temp.path(),
        "Cargo.toml",
        "[workspace]\nmembers=['target/member']\nresolver='2'\n",
    );
    for name in ["a", "b"] {
        write(
            temp.path(),
            &format!("shared/{name}/Cargo.toml"),
            "[package]\nname='member'\nversion='0.1.0'\nedition='2021'\n",
        );
    }
    fs::create_dir_all(temp.path().join("target/member")).unwrap();
    symlink(
        "../../shared/a/Cargo.toml",
        temp.path().join("target/member/Cargo.toml"),
    )
    .unwrap();
    let resolved =
        resolve_project_recipe_root(temp.path(), None, Some(ProjectRecipeId::Rust)).unwrap();

    let before = digest_project_cargo_all_packages_provenance(&resolved).unwrap();
    fs::remove_file(temp.path().join("target/member/Cargo.toml")).unwrap();
    symlink(
        "../../shared/b/Cargo.toml",
        temp.path().join("target/member/Cargo.toml"),
    )
    .unwrap();
    let after = digest_project_cargo_all_packages_provenance(&resolved).unwrap();

    assert_ne!(before, after);
}

#[test]
fn cargo_all_packages_provenance_tracks_underscore_build_dependency_alias() {
    let temp = tempfile::tempdir().unwrap();
    write(
        temp.path(),
        "Cargo.toml",
        "[workspace]\nmembers=['app']\nresolver='2'\n",
    );
    write(
        temp.path(),
        "app/Cargo.toml",
        "[package]\nname='app'\nversion='0.1.0'\nedition='2021'\n\n[build_dependencies]\nhelper={path='../target/helper'}\n",
    );
    write(
        temp.path(),
        "target/helper/Cargo.toml",
        "[package]\nname='helper'\nversion='0.1.0'\nedition='2021'\n",
    );
    let resolved =
        resolve_project_recipe_root(temp.path(), None, Some(ProjectRecipeId::Rust)).unwrap();

    let before = digest_project_cargo_all_packages_provenance(&resolved).unwrap();
    write(
        temp.path(),
        "target/helper/Cargo.toml",
        "[package]\nname='helper'\nversion='0.1.0'\nedition='2024'\n",
    );
    let after = digest_project_cargo_all_packages_provenance(&resolved).unwrap();

    assert_ne!(before, after);
}

#[test]
fn rust_member_underscore_workspace_dependency_alias_finds_workspace_root() {
    let temp = tempfile::tempdir().unwrap();
    write(
        temp.path(),
        "Cargo.toml",
        "[workspace]\nmembers=['member']\nresolver='2'\n\n[workspace.dependencies]\nhelper={path='target/helper'}\n",
    );
    write(
        temp.path(),
        "member/Cargo.toml",
        "[package]\nname='member'\nversion='0.1.0'\nedition='2021'\n\n[build_dependencies]\nhelper.workspace=true\n",
    );
    write(
        temp.path(),
        "target/helper/Cargo.toml",
        "[package]\nname='helper'\nversion='0.1.0'\nedition='2021'\n",
    );
    let resolved =
        resolve_project_recipe_root(temp.path(), Some("member"), Some(ProjectRecipeId::Rust))
            .unwrap();
    let root = temp.path().canonicalize().unwrap();

    assert_eq!(
        project_recipe_provenance_files(&resolved).unwrap().unwrap(),
        [
            root.join("member/Cargo.toml"),
            root.join("Cargo.toml"),
            root.join("Cargo.lock"),
        ]
    );
}

#[test]
fn rust_member_package_underscore_workspace_alias_finds_workspace_root() {
    for key in ["rust_version", "license_file"] {
        let temp = tempfile::tempdir().unwrap();
        write(
            temp.path(),
            "Cargo.toml",
            "[workspace]\nmembers=['member']\nresolver='2'\n\n[workspace.package]\nrust-version='1.85'\nlicense-file='LICENSE'\n",
        );
        write(temp.path(), "LICENSE", "test\n");
        write(
            temp.path(),
            "member/Cargo.toml",
            &format!(
                "[package]\nname='member'\nversion='0.1.0'\nedition='2021'\n{key}.workspace=true\n"
            ),
        );
        let resolved =
            resolve_project_recipe_root(temp.path(), Some("member"), Some(ProjectRecipeId::Rust))
                .unwrap();
        let root = temp.path().canonicalize().unwrap();

        assert_eq!(
            project_recipe_provenance_files(&resolved).unwrap().unwrap(),
            [
                root.join("member/Cargo.toml"),
                root.join("Cargo.toml"),
                root.join("Cargo.lock"),
            ],
            "package key {key}",
        );
    }
}

#[test]
fn cargo_all_packages_path_dependency_cycle_does_not_exhaust_route_bound() {
    let temp = tempfile::tempdir().unwrap();
    write(
        temp.path(),
        "Cargo.toml",
        "[workspace]\nmembers=['a','b']\nresolver='2'\n",
    );
    write(
        temp.path(),
        "a/Cargo.toml",
        "[package]\nname='a'\nversion='0.1.0'\nedition='2021'\n\n[dev-dependencies]\nb={path='../b'}\n",
    );
    write(
        temp.path(),
        "b/Cargo.toml",
        "[package]\nname='b'\nversion='0.1.0'\nedition='2021'\n\n[dev-dependencies]\na={path='../a'}\n",
    );
    let resolved =
        resolve_project_recipe_root(temp.path(), None, Some(ProjectRecipeId::Rust)).unwrap();

    assert!(digest_project_cargo_all_packages_provenance(&resolved).is_ok());
}

#[cfg(unix)]
#[test]
fn cargo_all_packages_manifest_bound_counts_symlink_targets_by_identity() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().unwrap();
    write(
        temp.path(),
        "Cargo.toml",
        "[workspace]\nmembers=['target/*']\nresolver='2'\n",
    );
    for index in 0..=256 {
        let manifest = format!("manifests/member-{index}.toml");
        write(
            temp.path(),
            &manifest,
            &format!("[package]\nname='member-{index}'\nversion='0.1.0'\nedition='2021'\n"),
        );
        let member_dir = temp.path().join(format!("target/member-{index}"));
        fs::create_dir_all(&member_dir).unwrap();
        symlink(format!("../../{manifest}"), member_dir.join("Cargo.toml")).unwrap();
    }
    let resolved =
        resolve_project_recipe_root(temp.path(), None, Some(ProjectRecipeId::Rust)).unwrap();

    assert_eq!(
        digest_project_cargo_all_packages_provenance(&resolved).unwrap_err(),
        ProjectRecipeResolutionError::SourceFileInvalid
    );
}

#[test]
fn cargo_all_packages_requires_authority_over_ancestor_workspaces() {
    let temp = tempfile::tempdir().unwrap();
    write(
        temp.path(),
        "Cargo.toml",
        "[workspace]\nmembers=['app','sibling']\nresolver='2'\n",
    );
    write(
        temp.path(),
        "app/Cargo.toml",
        "[package]\nname='app'\nversion='0.1.0'\nedition='2021'\n",
    );
    write(
        temp.path(),
        "sibling/Cargo.toml",
        "[package]\nname='sibling'\nversion='0.1.0'\nedition='2021'\n",
    );
    let resolved =
        resolve_project_recipe_root(&temp.path().join("app"), None, Some(ProjectRecipeId::Rust))
            .unwrap();
    // Explicit package fields do not opt out of Cargo's parent workspace search.
    assert!(project_recipe_provenance_files(&resolved).is_ok());
    assert_eq!(
        digest_project_cargo_all_packages_provenance(&resolved).unwrap_err(),
        ProjectRecipeResolutionError::SourceFileInvalid
    );
    // An explicit project-local workspace is positive selection authority.
    write(
        temp.path(),
        "app/Cargo.toml",
        "[package]\nname='app'\nversion='0.1.0'\nedition='2021'\n[workspace]\n",
    );
    assert!(digest_project_cargo_all_packages_provenance(&resolved).is_ok());
}

#[test]
fn cargo_all_packages_external_dependency_cannot_claim_workspace_membership() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    write(
        &project,
        "Cargo.toml",
        "[workspace]\nmembers=['app']\nresolver='2'\n",
    );
    write(&project, "app/Cargo.toml", "[package]\nname='app'\nversion='0.1.0'\nedition='2021'\n[dependencies]\nshared={path='../../outside'}\n");
    write(
        temp.path(),
        "outside/Cargo.toml",
        "[package]\nname='shared'\nversion='0.1.0'\nedition='2021'\nworkspace='../project'\n",
    );
    let resolved =
        resolve_project_recipe_root(&project, None, Some(ProjectRecipeId::Rust)).unwrap();
    // Cargo admits this external dependency as a workspace member. Its manifest
    // cannot be witnessed under the registered Project's source authority.
    assert_eq!(
        digest_project_cargo_all_packages_provenance(&resolved).unwrap_err(),
        ProjectRecipeResolutionError::SourceFileInvalid
    );
}

#[cfg(unix)]
#[test]
fn cargo_all_packages_accepts_contained_root_manifest_symlink() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    write(
        temp.path(),
        "manifests/workspace.toml",
        "[workspace]\nmembers=['member']\nresolver='2'\n",
    );
    write(
        temp.path(),
        "member/Cargo.toml",
        "[package]\nname='member'\nversion='0.1.0'\nedition='2021'\n",
    );
    symlink("manifests/workspace.toml", temp.path().join("Cargo.toml")).unwrap();
    let resolved =
        resolve_project_recipe_root(temp.path(), None, Some(ProjectRecipeId::Rust)).unwrap();
    let before = digest_project_cargo_all_packages_provenance(&resolved).unwrap();
    write(
        temp.path(),
        "manifests/workspace.toml",
        "[workspace]\nmembers=['member']\nresolver='3'\n",
    );
    assert_ne!(
        before,
        digest_project_cargo_all_packages_provenance(&resolved).unwrap()
    );
}

#[cfg(unix)]
#[test]
fn cargo_all_packages_ignores_unrelated_non_manifest_symlinks() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    write(
        temp.path(),
        "Cargo.toml",
        "[workspace]\nmembers=['member']\nresolver='2'\n",
    );
    write(
        temp.path(),
        "member/Cargo.toml",
        "[package]\nname='member'\nversion='0.1.0'\nedition='2021'\n",
    );
    let resolved =
        resolve_project_recipe_root(temp.path(), None, Some(ProjectRecipeId::Rust)).unwrap();
    let before = digest_project_cargo_all_packages_provenance(&resolved).unwrap();
    symlink(outside.path(), temp.path().join("documentation")).unwrap();
    symlink("missing-notes", temp.path().join("notes-link")).unwrap();
    assert_eq!(
        before,
        digest_project_cargo_all_packages_provenance(&resolved).unwrap()
    );
}

#[test]
fn cargo_all_packages_excluded_cwd_cannot_borrow_workspace_authority() {
    let temp = tempfile::tempdir().unwrap();
    write(
        temp.path(),
        "Cargo.toml",
        "[workspace]\nmembers=['app']\nexclude=['tool']\nresolver='2'\n",
    );
    for name in ["app", "tool"] {
        write(
            temp.path(),
            &format!("{name}/Cargo.toml"),
            &format!("[package]\nname='{name}'\nversion='0.1.0'\nedition='2021'\n"),
        );
    }
    let app =
        resolve_project_recipe_root(temp.path(), Some("app"), Some(ProjectRecipeId::Rust)).unwrap();
    assert!(digest_project_cargo_all_packages_provenance(&app).is_ok());
    let excluded =
        resolve_project_recipe_root(temp.path(), Some("tool"), Some(ProjectRecipeId::Rust))
            .unwrap();
    assert_eq!(
        digest_project_cargo_all_packages_provenance(&excluded).unwrap_err(),
        ProjectRecipeResolutionError::SourceFileInvalid
    );
    // Cargo gives an explicit member declaration precedence over exclusion.
    write(
        temp.path(),
        "Cargo.toml",
        "[workspace]\nmembers=['app','tool']\nexclude=['tool']\nresolver='2'\n",
    );
    assert!(digest_project_cargo_all_packages_provenance(&excluded).is_ok());
}

#[cfg(unix)]
#[test]
fn cargo_all_packages_finite_glob_prunes_unrelated_symlink_routes() {
    use std::os::unix::fs::symlink;
    for (pattern, member, irrelevant) in [
        ("crates/app*", "crates/app", "crates/docs"),
        ("crates/a*/pkg?", "crates/app/pkg1", "crates/app/docs"),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        write(
            temp.path(),
            "Cargo.toml",
            &format!("[workspace]\nmembers=['{pattern}']\nresolver='2'\n"),
        );
        write(
            temp.path(),
            &format!("{member}/Cargo.toml"),
            "[package]\nname='app'\nversion='0.1.0'\nedition='2021'\n",
        );
        fs::create_dir_all(temp.path().join("notes")).unwrap();
        let resolved =
            resolve_project_recipe_root(temp.path(), None, Some(ProjectRecipeId::Rust)).unwrap();
        let before = digest_project_cargo_all_packages_provenance(&resolved).unwrap();
        let link = temp.path().join(irrelevant);
        symlink(outside.path(), &link).unwrap();
        assert_eq!(
            before,
            digest_project_cargo_all_packages_provenance(&resolved).unwrap(),
            "{pattern}"
        );
        fs::remove_file(&link).unwrap();
        symlink(temp.path().join("notes"), &link).unwrap();
        assert_eq!(
            before,
            digest_project_cargo_all_packages_provenance(&resolved).unwrap(),
            "{pattern}"
        );
        symlink(outside.path(), temp.path().join("crates/app-escape")).unwrap();
        assert_eq!(
            digest_project_cargo_all_packages_provenance(&resolved).unwrap_err(),
            ProjectRecipeResolutionError::SourceFileInvalid
        );
    }
}
