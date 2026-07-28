from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

from scripts import workspace_boundary_check as boundary


def metadata_with_required_packages() -> dict:
    packages = []
    members = []
    for name in sorted(boundary.REQUIRED_PACKAGES):
        package_id = f"path+file:///fixture#{name}@0.3.0"
        packages.append({"id": package_id, "name": name, "dependencies": []})
        members.append(package_id)
    return {"packages": packages, "workspace_members": members}


def package(metadata: dict, name: str) -> dict:
    return next(entry for entry in metadata["packages"] if entry["name"] == name)


class MetadataBoundaryTests(unittest.TestCase):
    def test_valid_workspace_passes(self) -> None:
        self.assertEqual(boundary.check_metadata(metadata_with_required_packages()), [])

    def test_each_forbidden_direct_dependency_is_reported(self) -> None:
        for package_name, dependencies in boundary.FORBIDDEN_DIRECT_DEPENDENCIES.items():
            for dependency_name in dependencies:
                with self.subTest(
                    package=package_name, dependency=dependency_name
                ):
                    metadata = metadata_with_required_packages()
                    package(metadata, package_name)["dependencies"] = [
                        {"name": dependency_name}
                    ]
                    violations = boundary.check_metadata(metadata)
                    self.assertEqual(len(violations), 1)
                    self.assertIn(package_name, violations[0])
                    self.assertIn(dependency_name, violations[0])

    def test_missing_required_package_is_reported(self) -> None:
        metadata = metadata_with_required_packages()
        missing = "webcodex-core"
        missing_id = package(metadata, missing)["id"]
        metadata["workspace_members"].remove(missing_id)
        violations = boundary.check_metadata(metadata)
        self.assertTrue(any(missing in violation for violation in violations))


class ParentSourcePathTests(unittest.TestCase):
    def test_both_cross_parent_path_spellings_are_reported_in_rust_sources(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            (root / "src").mkdir()
            (root / "tests").mkdir()
            (root / "src" / "lib.rs").write_text(
                '#[path = "../shared.rs"]\nmod shared;\n', encoding="utf-8"
            )
            (root / "tests" / "integration.rs").write_text(
                '#[path="../support.rs"]\nmod support;\n', encoding="utf-8"
            )
            violations = boundary.check_parent_source_paths(root)
            self.assertEqual(len(violations), 2)
            self.assertTrue(any("src/lib.rs:1" in item for item in violations))
            self.assertTrue(
                any("tests/integration.rs:1" in item for item in violations)
            )

    def test_generated_and_non_rust_files_are_excluded(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            for excluded in ("target", "generated", "dist", "node_modules"):
                directory = root / excluded
                directory.mkdir()
                (directory / "fixture.rs").write_text(
                    '#[path = "../shared.rs"]\n', encoding="utf-8"
                )
            (root / "docs").mkdir()
            (root / "docs" / "historical.rs").write_text(
                '#[path = "../historical.rs"]\n', encoding="utf-8"
            )
            (root / "history.md").write_text(
                '#[path = "../historical.rs"]\n', encoding="utf-8"
            )
            self.assertEqual(boundary.check_parent_source_paths(root), [])


if __name__ == "__main__":
    unittest.main()
