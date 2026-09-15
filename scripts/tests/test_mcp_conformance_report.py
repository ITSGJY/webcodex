#!/usr/bin/env python3
from __future__ import annotations

import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path

MODULE_PATH = Path(__file__).resolve().parents[1] / "mcp_conformance_report.py"
SPEC = importlib.util.spec_from_file_location("mcp_conformance_report", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
report_gate = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = report_gate
SPEC.loader.exec_module(report_gate)


class ReportGateTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        self.raw = self.root / "raw"
        self.raw.mkdir()
        self.metadata = self.root / "metadata.json"
        self.baseline = self.root / "baseline.json"
        self.profile = "2026-07-28"

    def tearDown(self) -> None:
        self.tmp.cleanup()

    def write_metadata(
        self,
        scenarios: list[str],
        not_scored: list[dict[str, str]] | None = None,
    ) -> None:
        self.metadata.write_text(
            json.dumps(
                {
                    "profile": self.profile,
                    "server_sha": "a" * 40,
                    "harness_sha": "b" * 40,
                    "harness_exit_code": 0,
                    "required_scenarios": scenarios,
                    "not_scored_scenarios": not_scored or [],
                }
            ),
            encoding="utf-8",
        )

    def write_baseline(self, entries: list[dict[str, str]]) -> None:
        self.baseline.write_text(
            json.dumps(
                {
                    "schema_version": 1,
                    "profiles": {self.profile: {"classifications": entries}},
                }
            ),
            encoding="utf-8",
        )

    def write_checks(self, scenario: str, checks: list[dict[str, str]]) -> None:
        result = self.raw / f"server-{scenario}-2026-09-14T00-00-00-000Z"
        result.mkdir()
        (result / "checks.json").write_text(json.dumps(checks), encoding="utf-8")

    @staticmethod
    def check(check_id: str, status: str) -> dict[str, str]:
        return {
            "id": check_id,
            "name": check_id,
            "description": check_id,
            "status": status,
            "timestamp": "2026-09-14T00:00:00Z",
        }

    @staticmethod
    def classification(check_id: str, kind: str = "genuine_protocol_failure") -> dict[str, str]:
        return {
            "scenario": "scenario-a",
            "check_id": check_id,
            "classification": kind,
            "reason": "reviewed baseline reason",
            "evidence": "MCP 2026-07-28 section/example",
        }

    def evaluate(self):
        return report_gate.evaluate_reports(self.raw, self.metadata, self.baseline, self.profile)

    def test_successful_nonempty_coverage_passes(self) -> None:
        self.write_metadata(["scenario-a"])
        self.write_baseline([])
        self.write_checks("scenario-a", [self.check("wire-shape", "SUCCESS")])
        summary = self.evaluate()
        self.assertTrue(summary["gate_passed"])
        self.assertEqual(summary["applicable_check_count"], 1)

    def test_all_skipped_is_not_mistaken_for_coverage(self) -> None:
        self.write_metadata(["scenario-a"])
        self.write_baseline(
            [self.classification("not-applicable", "inconclusive_infrastructure")]
        )
        self.write_checks("scenario-a", [self.check("not-applicable", "SKIPPED")])
        summary = self.evaluate()
        self.assertFalse(summary["gate_passed"])
        self.assertTrue(any("zero applicable" in problem for problem in summary["problems"]))

    def test_missing_required_scenario_fails(self) -> None:
        self.write_metadata(["scenario-a", "scenario-b"])
        self.write_baseline([])
        self.write_checks("scenario-a", [self.check("ok", "SUCCESS")])
        summary = self.evaluate()
        self.assertFalse(summary["gate_passed"])
        self.assertEqual(summary["missing_scenarios"], ["scenario-b"])

    def test_not_scored_failure_is_visible_but_does_not_require_classification(self) -> None:
        self.write_metadata(
            ["scenario-a"],
            [{"scenario": "extension-a", "reason": "extension"}],
        )
        self.write_baseline([])
        self.write_checks("scenario-a", [self.check("ok", "SUCCESS")])
        self.write_checks("extension-a", [self.check("pending", "FAILURE")])
        summary = self.evaluate()
        self.assertTrue(summary["gate_passed"])
        self.assertEqual(summary["informational_status_counts"], {"FAILURE": 1})
        self.assertEqual(summary["informational_non_success"][0]["reason"], "extension")

    def test_missing_not_scored_scenario_fails_coverage_integrity(self) -> None:
        self.write_metadata(
            ["scenario-a"],
            [{"scenario": "pending-a", "reason": "pending"}],
        )
        self.write_baseline([])
        self.write_checks("scenario-a", [self.check("ok", "SUCCESS")])
        summary = self.evaluate()
        self.assertFalse(summary["gate_passed"])
        self.assertEqual(summary["missing_not_scored_scenarios"], ["pending-a"])

    def test_classified_harness_error_remains_inconclusive(self) -> None:
        self.write_metadata(["scenario-a"])
        self.write_baseline(
            [self.classification("scenario-a", "inconclusive_infrastructure")]
        )
        self.write_checks("scenario-a", [self.check("scenario-a", "FAILURE")])
        summary = self.evaluate()
        self.assertFalse(summary["gate_passed"])
        self.assertTrue(any("inconclusive infrastructure" in p for p in summary["problems"]))

    def test_new_failure_requires_exact_classification(self) -> None:
        self.write_metadata(["scenario-a"])
        self.write_baseline([])
        self.write_checks("scenario-a", [self.check("new-regression", "FAILURE")])
        summary = self.evaluate()
        self.assertFalse(summary["gate_passed"])
        self.assertTrue(any("new-regression" in p for p in summary["problems"]))

    def test_passing_expected_failure_is_stale(self) -> None:
        self.write_metadata(["scenario-a"])
        self.write_baseline([self.classification("fixed-check")])
        self.write_checks("scenario-a", [self.check("fixed-check", "SUCCESS")])
        summary = self.evaluate()
        self.assertFalse(summary["gate_passed"])
        self.assertTrue(any("now passing" in p for p in summary["problems"]))

    def test_info_only_expected_failure_is_uncovered(self) -> None:
        self.write_metadata(["scenario-a"])
        self.write_baseline([self.classification("no-verdict")])
        self.write_checks(
            "scenario-a",
            [self.check("no-verdict", "INFO"), self.check("ok", "SUCCESS")],
        )
        summary = self.evaluate()
        self.assertFalse(summary["gate_passed"])
        self.assertTrue(any("no verdict emitted" in p for p in summary["problems"]))

    def test_broad_expected_failure_masks_are_rejected(self) -> None:
        self.write_metadata(["scenario-a"])
        self.write_baseline(
            [
                {
                    "scenario": "scenario-a",
                    "check_id": "*",
                    "classification": "genuine_protocol_failure",
                    "reason": "too broad",
                    "evidence": "none",
                }
            ]
        )
        self.write_checks("scenario-a", [self.check("failure", "FAILURE")])
        with self.assertRaisesRegex(report_gate.GateError, "broad scenario masks"):
            self.evaluate()


if __name__ == "__main__":
    unittest.main()
