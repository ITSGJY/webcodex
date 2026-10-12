#!/usr/bin/env python3
"""No-build CI preflight for static model-guidance contracts.

Read only repository-owned Rust source. Fail closed if its shape changes:
the canonical Rust tests remain the final contract authority. This checker
rejects obvious wording or boundedness errors *before* any Rust compilation.
"""

from __future__ import annotations

import json
from pathlib import Path
import re
import sys

ROOT = Path(__file__).resolve().parent.parent
CONTRIBUTORS = ROOT / "src/tool_runtime/guidance/contributors.rs"
TESTS = ROOT / "src/tool_runtime/tests/builtin_coding_workflow.rs"
CONTRACT = ROOT / "crates/webcodex-core/src/runtime_contract.rs"


class GuidancePreflightError(ValueError):
    """The static source contract cannot be proved without compiling."""


def _items(source: str, name: str) -> list[str]:
    anchor = f"pub(super) static {name}: GuidanceContributor ="
    if source.count(anchor) != 1:
        raise GuidancePreflightError(f"{name}: guidance contributor not uniquely found")
    section = source.split(anchor, 1)[1].split("};", 1)
    if len(section) != 2 or section[0].count("items: &[") != 1:
        raise GuidancePreflightError(f"{name}: unexpected contributor layout")
    lines = section[0].split("items: &[", 1)[1].splitlines()
    result: list[str] = []
    ended = False
    for line in lines:
        value = line.strip()
        if value == "],":
            ended = True
            break
        if not value:
            continue
        if not (value.startswith('"') and value.endswith('",')):
            raise GuidancePreflightError(f"{name}: unsupported Rust literal layout")
        try:
            item = json.loads(value[:-1])
        except json.JSONDecodeError as exc:
            raise GuidancePreflightError(f"{name}: unparseable guidance literal") from exc
        if not isinstance(item, str):
            raise GuidancePreflightError(f"{name}: non-string guidance literal")
        result.append(item)
    if not ended or not result:
        raise GuidancePreflightError(f"{name}: unterminated or empty guidance list")
    return result


def _required_phrases(tests: str, name: str) -> list[str]:
    anchor = f"fn {name}() {{"
    if tests.count(anchor) != 1:
        raise GuidancePreflightError(f"{name}: cannot locate exact canonical Rust assertion")
    after = tests.split(anchor, 1)[1]
    match = re.search(r"for phrase in \[\s*(.*?)\s*\]\s*\{", after, re.S)
    if match is None or match.start() > 5000:
        raise GuidancePreflightError(f"{name}: cannot parse first literal phrase assertion")
    result = []
    for line in match.group(1).splitlines():
        value = line.strip()
        if not (value.startswith('"') and value.endswith('",')):
            raise GuidancePreflightError(f"{name}: unsupported test phrase layout")
        try:
            phrase = json.loads(value[:-1])
        except json.JSONDecodeError as exc:
            raise GuidancePreflightError(f"{name}: invalid phrase literal") from exc
        if not isinstance(phrase, str):
            raise GuidancePreflightError(f"{name}: invalid non-string phrase")
        result.append(phrase)
    if len(result) < 10:
        raise GuidancePreflightError(f"{name}: incomplete phrase assertions")
    return result


def _limit(contract: str, name: str) -> int:
    expr = rf"(?m)^pub const {re.escape(name)}: usize = (\d+);$"
    values = re.findall(expr, contract)
    if len(values) != 1:
        raise GuidancePreflightError(f"missing canonical Rust limit {name}")
    return int(values[0])


def validate(contributors: str, tests: str, contract: str) -> dict[str, int]:
    max_items = _limit(contract, "BUILTIN_CODING_WORKFLOW_GUIDANCE_TARGET_ITEMS")
    max_chars = _limit(contract, "BUILTIN_CODING_WORKFLOW_GUIDANCE_TARGET_ITEM_CHARS")
    direct = _items(contributors, "DIRECT")
    host = _items(contributors, "HOST_BATCHING") + _items(contributors, "HOST_CONTINUATION")
    for label, entries, test_name in [
        ("direct", direct, "direct_strategy_prefers_structured_edits_and_coalesces_known_work"),
        ("host", host, "host_code_mode_strategy_is_bounded_guidance_only"),
    ]:
        if len(entries) > max_items:
            raise GuidancePreflightError(f"{label}: {len(entries)} items exceed {max_items}")
        if any(len(value) > max_chars for value in entries):
            raise GuidancePreflightError(f"{label}: guidance item exceeds {max_chars} chars")
        # Verify every literal required by the canonical Rust unit tests.
        corpus = "\n".join(entries)
        required = _required_phrases(tests, test_name)
        missing = [phrase for phrase in required if phrase not in corpus]
        if missing:
            raise GuidancePreflightError(
                f"{label}: {len(missing)} Rust-required guidance phrase(s) missing: {missing[0]!r}"
            )
    return {"direct_items": len(direct), "host_items": len(host),
            "direct_phrases": len(_required_phrases(
                tests, "direct_strategy_prefers_structured_edits_and_coalesces_known_work")),
            "host_phrases": len(_required_phrases(
                tests, "host_code_mode_strategy_is_bounded_guidance_only"))}


def main() -> int:
    try:
        result = validate(
            CONTRIBUTORS.read_text(encoding="utf-8"),
            TESTS.read_text(encoding="utf-8"),
            CONTRACT.read_text(encoding="utf-8"),
        )
    except (GuidancePreflightError, OSError, UnicodeError) as exc:
        print(f"[guidance preflight] FAIL: {exc}", file=sys.stderr)
        return 1
    print("[guidance preflight] PASS: "
          f"direct={result['direct_items']} host={result['host_items']} "
          f"assertions={result['direct_phrases']}+{result['host_phrases']}; no Cargo build")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
