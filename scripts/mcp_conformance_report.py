#!/usr/bin/env python3
"""Coverage-aware gate for pinned MCP conformance reports.

The upstream conformance runner intentionally answers a different question from
this gate: it executes scenarios and reports their checks.  WebCodex keeps the
raw upstream output, then this script verifies that the expected scenario set was
actually exercised and that every non-success result has a narrow, reviewed
classification.  A zero upstream process exit is never treated as proof of
coverage or compliance.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from collections import Counter
from dataclasses import dataclass
from pathlib import Path
from typing import Any

CLASSIFICATIONS = {
    "genuine_protocol_failure",
    "missing_harness_fixture",
    "optional_capability_not_implemented",
    "harness_limitation_pending",
    "inconclusive_infrastructure",
}
NON_SUCCESS_STATUSES = {"FAILURE", "WARNING", "SKIPPED"}
APPLICABLE_STATUSES = {"SUCCESS", "FAILURE", "WARNING"}
SCENARIO_DIR_RE = re.compile(r"^server-(.+)-\d{4}-\d{2}-\d{2}T.*Z$")


class GateError(RuntimeError):
    """Raised when report or baseline input is malformed."""


@dataclass(frozen=True)
class Classification:
    scenario: str
    check_id: str
    classification: str
    reason: str
    evidence: str

    @property
    def key(self) -> tuple[str, str]:
        return (self.scenario, self.check_id)


def _load_json(path: Path) -> Any:
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        raise GateError(f"cannot read JSON {path}: {exc}") from exc


def _scenario_from_result_dir(path: Path) -> str:
    match = SCENARIO_DIR_RE.match(path.parent.name)
    if not match:
        raise GateError(
            f"unexpected upstream result directory {path.parent.name!r}; "
            "expected server-<scenario>-<timestamp>"
        )
    return match.group(1)


def load_reports(report_root: Path) -> dict[str, list[dict[str, Any]]]:
    reports: dict[str, list[dict[str, Any]]] = {}
    for checks_path in sorted(report_root.glob("server-*/checks.json")):
        scenario = _scenario_from_result_dir(checks_path)
        if scenario in reports:
            raise GateError(
                f"multiple raw reports found for scenario {scenario!r}; clean the output directory"
            )
        value = _load_json(checks_path)
        if not isinstance(value, list):
            raise GateError(f"{checks_path} must contain a JSON array")
        checks: list[dict[str, Any]] = []
        for index, check in enumerate(value):
            if not isinstance(check, dict):
                raise GateError(f"{checks_path}[{index}] is not an object")
            check_id = check.get("id")
            status = check.get("status")
            if not isinstance(check_id, str) or not check_id:
                raise GateError(f"{checks_path}[{index}] has no non-empty check id")
            if not isinstance(status, str) or not status:
                raise GateError(f"{checks_path}[{index}] has no non-empty status")
            checks.append(check)
        reports[scenario] = checks
    return reports


def load_classifications(baseline_path: Path, profile: str) -> dict[tuple[str, str], Classification]:
    baseline = _load_json(baseline_path)
    if not isinstance(baseline, dict) or baseline.get("schema_version") != 1:
        raise GateError("baseline must be an object with schema_version=1")
    profiles = baseline.get("profiles")
    if not isinstance(profiles, dict) or profile not in profiles:
        raise GateError(f"baseline has no profile {profile!r}")
    profile_config = profiles[profile]
    if not isinstance(profile_config, dict):
        raise GateError(f"baseline profile {profile!r} must be an object")
    raw_entries = profile_config.get("classifications", [])
    if not isinstance(raw_entries, list):
        raise GateError(f"baseline profile {profile!r} classifications must be an array")

    entries: dict[tuple[str, str], Classification] = {}
    for index, raw in enumerate(raw_entries):
        if not isinstance(raw, dict):
            raise GateError(f"classification #{index + 1} must be an object")
        scenario = raw.get("scenario")
        check_id = raw.get("check_id")
        kind = raw.get("classification")
        reason = raw.get("reason")
        evidence = raw.get("evidence")
        if not isinstance(scenario, str) or not scenario:
            raise GateError(f"classification #{index + 1} needs a non-empty scenario")
        # Whole-scenario masks are deliberately unsupported. They can hide a newly
        # failing check when an older, unrelated check is already expected to fail.
        if not isinstance(check_id, str) or not check_id or check_id == "*":
            raise GateError(
                f"classification {scenario!r} must name one exact check_id; broad scenario masks are forbidden"
            )
        if kind not in CLASSIFICATIONS:
            raise GateError(
                f"classification {scenario}:{check_id} has unsupported kind {kind!r}"
            )
        if not isinstance(reason, str) or not reason.strip():
            raise GateError(f"classification {scenario}:{check_id} needs a reason")
        if not isinstance(evidence, str) or not evidence.strip():
            raise GateError(f"classification {scenario}:{check_id} needs evidence")
        entry = Classification(scenario, check_id, kind, reason.strip(), evidence.strip())
        if entry.key in entries:
            raise GateError(f"duplicate classification for {scenario}:{check_id}")
        entries[entry.key] = entry
    return entries


def evaluate_reports(
    report_root: Path,
    metadata_path: Path,
    baseline_path: Path,
    profile: str,
) -> dict[str, Any]:
    metadata = _load_json(metadata_path)
    if not isinstance(metadata, dict):
        raise GateError("metadata must be a JSON object")
    if metadata.get("profile") != profile:
        raise GateError(
            f"metadata profile {metadata.get('profile')!r} does not match requested {profile!r}"
        )
    required = metadata.get("required_scenarios")
    if not isinstance(required, list) or not required or not all(isinstance(item, str) and item for item in required):
        raise GateError("metadata required_scenarios must be a non-empty string array")
    if len(required) != len(set(required)):
        raise GateError("metadata required_scenarios contains duplicates")

    reports = load_reports(report_root)
    classifications = load_classifications(baseline_path, profile)
    problems: list[str] = []
    missing_scenarios = sorted(set(required) - set(reports))
    unexpected_scenarios = sorted(set(reports) - set(required))
    if missing_scenarios:
        problems.append("missing required scenario reports: " + ", ".join(missing_scenarios))
    if unexpected_scenarios:
        problems.append("unexpected scenario reports: " + ", ".join(unexpected_scenarios))

    statuses = Counter()
    emitted: dict[tuple[str, str], set[str]] = {}
    applicable_checks = 0
    unclassified: list[str] = []
    expected: list[dict[str, str]] = []
    inconclusive: list[str] = []

    for scenario, checks in sorted(reports.items()):
        for check in checks:
            check_id = check["id"]
            status = check["status"]
            statuses[status] += 1
            key = (scenario, check_id)
            emitted.setdefault(key, set()).add(status)
            if status in APPLICABLE_STATUSES:
                applicable_checks += 1
            if status in NON_SUCCESS_STATUSES:
                entry = classifications.get(key)
                label = f"{scenario}:{check_id} [{status}]"
                if entry is None:
                    unclassified.append(label)
                    continue
                expected.append(
                    {
                        "scenario": scenario,
                        "check_id": check_id,
                        "status": status,
                        "classification": entry.classification,
                        "reason": entry.reason,
                        "evidence": entry.evidence,
                    }
                )
                if entry.classification == "inconclusive_infrastructure":
                    inconclusive.append(label)

    if applicable_checks == 0:
        problems.append("zero applicable SUCCESS/FAILURE/WARNING checks were emitted")
    if unclassified:
        problems.append("unclassified non-success checks: " + ", ".join(sorted(unclassified)))
    if inconclusive:
        problems.append(
            "inconclusive infrastructure results cannot satisfy the gate: "
            + ", ".join(sorted(inconclusive))
        )

    stale: list[str] = []
    for key, entry in sorted(classifications.items()):
        states = emitted.get(key)
        label = f"{entry.scenario}:{entry.check_id}"
        if not states:
            stale.append(label + " (check not emitted)")
        elif states <= {"INFO"}:
            stale.append(label + " (no verdict emitted)")
        elif states <= {"SUCCESS", "INFO"} and "SUCCESS" in states:
            stale.append(label + " (now passing)")
    if stale:
        problems.append("stale or uncovered classifications: " + ", ".join(stale))

    return {
        "schema_version": 1,
        "profile": profile,
        "server_sha": metadata.get("server_sha"),
        "harness_sha": metadata.get("harness_sha"),
        "harness_exit_code": metadata.get("harness_exit_code"),
        "required_scenario_count": len(required),
        "reported_scenario_count": len(reports),
        "applicable_check_count": applicable_checks,
        "status_counts": dict(sorted(statuses.items())),
        "classified_non_success": expected,
        "missing_scenarios": missing_scenarios,
        "unexpected_scenarios": unexpected_scenarios,
        "problems": problems,
        "gate_passed": not problems,
    }


def _parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--reports", required=True, type=Path)
    parser.add_argument("--metadata", required=True, type=Path)
    parser.add_argument("--baseline", required=True, type=Path)
    parser.add_argument("--profile", required=True)
    parser.add_argument("--summary", type=Path)
    return parser.parse_args()


def main() -> int:
    args = _parse_args()
    try:
        summary = evaluate_reports(args.reports, args.metadata, args.baseline, args.profile)
    except GateError as exc:
        print(f"MCP conformance report gate input error: {exc}", file=sys.stderr)
        return 2

    rendered = json.dumps(summary, indent=2, sort_keys=True) + "\n"
    if args.summary:
        args.summary.parent.mkdir(parents=True, exist_ok=True)
        args.summary.write_text(rendered, encoding="utf-8")
    print(rendered, end="")
    return 0 if summary["gate_passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
