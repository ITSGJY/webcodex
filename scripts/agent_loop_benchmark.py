#!/usr/bin/env python3
"""Run reproducible paired Agent Loop benchmark samples over fresh Git worktrees.

The script is orchestration only. A caller-supplied Host driver performs the real
Direct/Code Mode task. This runner owns fresh worktrees, bounded annotation,
existing agent_loop_report summarization/comparison, fixture-side oracles, sample
status retention, and cleanup of its own temporary resources.
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import shlex
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from typing import Any

try:
    from scripts import agent_loop_report as report
except ModuleNotFoundError:
    import agent_loop_report as report

STATUS_VALUES = frozenset(("pass", "fail", "partial", "unsupported"))
DEFAULT_CASES = ("readonly_review", "guarded_multi_file_edit", "long_validation_handoff")
VARIANTS = ("direct", "code_mode")
MAX_DRIVER_RECEIPT_BYTES = 64 * 1024
MAX_DRIVER_SECS = 30 * 60


class BenchmarkError(ValueError):
    pass


def _stable_json(value: Any) -> str:
    return json.dumps(value, indent=2, sort_keys=True, ensure_ascii=False) + "\n"


def _run(argv: list[str], *, cwd: Path, env: dict[str, str] | None = None) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        argv,
        cwd=cwd,
        env=env,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )

def _run_driver(argv: list[str], *, cwd: Path, env: dict[str, str]) -> subprocess.CompletedProcess[bytes]:
    try:
        return subprocess.run(
            argv,
            cwd=cwd,
            env=env,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            timeout=MAX_DRIVER_SECS,
            check=False,
        )
    except subprocess.TimeoutExpired as exc:
        raise BenchmarkError("benchmark Host driver exceeded the 30 minute limit") from exc
    except OSError as exc:
        raise BenchmarkError("could not start benchmark Host driver") from exc


def _require_exact_revision(repo: Path, revision: str) -> str:
    completed = _run(["git", "rev-parse", "--verify", f"{revision}^{{commit}}"], cwd=repo)
    if completed.returncode != 0:
        raise BenchmarkError(f"could not resolve base revision: {revision}")
    resolved = completed.stdout.strip()
    if not report._is_exact_git_revision(resolved):
        raise BenchmarkError("resolved base revision is not an exact 40-hex commit")
    return resolved


def _head_revision(repo: Path) -> str:
    completed = _run(["git", "rev-parse", "HEAD"], cwd=repo)
    if completed.returncode != 0:
        raise BenchmarkError("could not resolve current checkout HEAD")
    head = completed.stdout.strip()
    if not report._is_exact_git_revision(head):
        raise BenchmarkError("current checkout HEAD is not an exact 40-hex commit")
    return head


def _read_manifest_at_revision(repo: Path, revision: str) -> str:
    completed = _run(
        ["git", "show", f"{revision}:scripts/agent_loop_cases.json"],
        cwd=repo,
    )
    if completed.returncode != 0:
        raise BenchmarkError("could not read benchmark case manifest at base revision")
    return completed.stdout


def _case(manifest: dict[str, Any], case_id: str) -> dict[str, Any]:
    return report._case_by_id(manifest, case_id)


def _surface(case: dict[str, Any], variant: str) -> str:
    if variant == "direct":
        return "direct"
    surface = case.get("code_mode_surface")
    canonical = report._canonical_code_mode_surface(surface)
    if canonical is None:
        raise BenchmarkError(f"case {case['id']} has no usable Code Mode surface")
    return canonical


def _worktree_add(repo: Path, path: Path, base_revision: str) -> None:
    completed = _run(["git", "worktree", "add", "--detach", str(path), base_revision], cwd=repo)
    if completed.returncode != 0:
        raise BenchmarkError(completed.stderr.strip() or "git worktree add failed")


def _worktree_remove(repo: Path, path: Path) -> None:
    _run(["git", "worktree", "remove", "--force", str(path)], cwd=repo)
    if path.exists():
        shutil.rmtree(path, ignore_errors=True)


def _workspace_status(workspace: Path) -> str:
    completed = _run(["git", "status", "--porcelain", "--untracked-files=all"], cwd=workspace)
    if completed.returncode != 0:
        raise BenchmarkError(completed.stderr.strip() or "git status failed")
    return completed.stdout


def _fixture_oracle(case: dict[str, Any], workspace: Path) -> dict[str, Any]:
    spec = (case.get("correctness") or {}).get("fixture_oracle")
    if not isinstance(spec, dict):
        return {"available": False, "passed": None, "checks": [], "reason": "case has no fixture_oracle"}

    checks: list[dict[str, Any]] = []
    for rel_path, expected in sorted((spec.get("files") or {}).items()):
        target = workspace / rel_path
        try:
            text = target.read_text(encoding="utf-8")
        except UnicodeDecodeError:
            checks.append({"path": rel_path, "passed": False, "reason": "file is not UTF-8"})
            continue
        except OSError:
            checks.append({"path": rel_path, "passed": False, "reason": "file unavailable"})
            continue
        except UnicodeDecodeError:
            checks.append({"path": rel_path, "passed": False, "reason": "file is not UTF-8"})
            continue
        expected_text = expected.get("expected_text")
        if isinstance(expected_text, str):
            passed = text == expected_text
        else:
            required = expected.get("required_text") or []
            forbidden = expected.get("forbidden_text") or []
            passed = all(item in text for item in required) and all(item not in text for item in forbidden)
        checks.append({"path": rel_path, "passed": passed, "reason": None if passed else "text oracle mismatch"})

    expected_changed = sorted(spec.get("changed_files") or [])
    if expected_changed:
        tracked = _run(["git", "diff", "--name-only", "HEAD", "--"], cwd=workspace)
        untracked = _run(["git", "ls-files", "--others", "--exclude-standard"], cwd=workspace)
        changed = sorted(
            set(line for line in tracked.stdout.splitlines() if line)
            | set(line for line in untracked.stdout.splitlines() if line)
        )
        checks.append(
            {
                "kind": "changed_files",
                "passed": tracked.returncode == 0 and untracked.returncode == 0 and changed == expected_changed,
                "expected": expected_changed,
                "actual": changed,
            }
        )

    passed = bool(checks) and all(check.get("passed") is True for check in checks)
    return {"available": True, "passed": passed, "checks": checks, "reason": None}


def _load_driver_receipt(path: Path) -> dict[str, Any]:
    try:
        if path.stat().st_size > MAX_DRIVER_RECEIPT_BYTES:
            raise BenchmarkError("driver result exceeds the 64 KiB receipt limit")
    except OSError as exc:
        raise BenchmarkError("driver did not create its result receipt") from exc
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except OSError as exc:
        raise BenchmarkError("driver did not create its result receipt") from exc
    except UnicodeDecodeError as exc:
        raise BenchmarkError("driver result must be UTF-8 JSON") from exc
    except json.JSONDecodeError as exc:
        raise BenchmarkError(f"driver result is not valid JSON: {exc}") from exc
    if not isinstance(value, dict):
        raise BenchmarkError("driver result must be a JSON object")
    status = value.get("status")
    if not isinstance(status, str) or status not in STATUS_VALUES:
        raise BenchmarkError("driver result status must be pass, fail, partial, or unsupported")
    return value


def _annotation(
    case: dict[str, Any],
    *,
    variant: str,
    surface: str,
    base_revision: str,
    receipt: dict[str, Any],
    oracle: dict[str, Any],
) -> dict[str, Any]:
    correctness = receipt.get("correctness")
    if correctness is not None and not isinstance(correctness, dict):
        raise BenchmarkError("driver correctness must be an object when present")
    correctness = dict(correctness or {})

    if oracle["available"]:
        if oracle["passed"] is not True:
            correctness["task_verdict"] = "fail"
        elif correctness.get("task_verdict") is None:
            correctness["task_verdict"] = "pass"
        if case["validation"]["required"] is False and correctness.get("validation_verdict") is None:
            correctness["validation_verdict"] = "not_required"

    value: dict[str, Any] = {
        "schema_version": report.RUN_ANNOTATION_SCHEMA_VERSION,
        "case_id": case["id"],
        "variant": variant,
        "surface": surface,
        "base_revision": base_revision,
        "case_fingerprint": report._case_fingerprint(case),
    }
    for field in ("repair_turns", "task_timing"):
        if receipt.get(field) is not None:
            value[field] = receipt[field]
    if correctness:
        value["correctness"] = correctness
    return report.validate_run_annotation(value)


def _summary_for_receipt(
    case: dict[str, Any],
    *,
    variant: str,
    surface: str,
    base_revision: str,
    receipt: dict[str, Any],
    annotation_path: Path,
    case_manifest: Path,
) -> dict[str, Any] | None:
    if receipt["status"] == "unsupported":
        return None
    audit_db = receipt.get("audit_db")
    workflow_session_id = receipt.get("workflow_session_id")
    trace_root = receipt.get("trace_root")
    if not isinstance(audit_db, str) or not audit_db:
        raise BenchmarkError("non-unsupported driver result requires audit_db")
    if not isinstance(workflow_session_id, str) or not workflow_session_id:
        raise BenchmarkError("non-unsupported driver result requires workflow_session_id")
    return report.summarize(
        trace_root=Path(trace_root) if isinstance(trace_root, str) and trace_root else None,
        audit_db=Path(audit_db),
        workflow_session_id=workflow_session_id,
        case_manifest=case_manifest,
        case_id=case["id"],
        variant=variant,
        surface=surface,
        base_revision=base_revision,
        run_annotation=annotation_path,
    )


def _tool_count(summary: dict[str, Any], variant: str, tool_name: str) -> int | None:
    if variant == "direct":
        value = report._get_path(summary, f"canonical_calls.by_name.{tool_name}")
        return value if isinstance(value, int) else 0
    if variant == "code_mode":
        available = report._get_path(summary, "availability.code_mode_composition.available")
        if available is not True:
            return None
        value = report._get_path(summary, f"composition.nested_tool_counts.{tool_name}")
        return value if isinstance(value, int) else 0
    return None


def _guarded_multi_file_contract_proven(summary: dict[str, Any], variant: str) -> bool:
    edit_count = _tool_count(summary, variant, "edit_project_files")
    read_count = _tool_count(summary, variant, "read_files")
    search_counts = [
        count
        for name in ("search_project_texts", "search_and_read")
        if (count := _tool_count(summary, variant, name)) is not None
    ]
    return (
        edit_count == 1
        and read_count is not None
        and read_count >= 2
        and sum(search_counts) >= 1
    )


def _run_sample(
    repo: Path,
    root: Path,
    driver_argv: list[str],
    case: dict[str, Any],
    *,
    variant: str,
    pair_index: int,
    ordinal: int,
    base_revision: str,
    case_manifest: Path,
) -> dict[str, Any]:
    label = f"{case['id']}-p{pair_index + 1}-{ordinal + 1}-{variant}"
    workspace = root / "worktrees" / label
    receipt_path = root / "receipts" / f"{label}.json"
    annotation_path = root / "annotations" / f"{label}.json"
    surface: str | None = None
    worktree_added = False
    started = time.time_ns() // 1_000_000
    try:
        surface = _surface(case, variant)
        _worktree_add(repo, workspace, base_revision)
        worktree_added = True
        if _workspace_status(workspace):
            raise BenchmarkError("fresh benchmark worktree is not clean")
        env = dict(os.environ)
        env.update(
            {
                "WEBCODEX_BENCH_WORKSPACE": str(workspace),
                "WEBCODEX_BENCH_CASE_ID": case["id"],
                "WEBCODEX_BENCH_VARIANT": variant,
                "WEBCODEX_BENCH_SURFACE": surface,
                "WEBCODEX_BENCH_BASE_REVISION": base_revision,
                "WEBCODEX_BENCH_CASE_FINGERPRINT": report._case_fingerprint(case),
                "WEBCODEX_BENCH_PROMPT": case["prompt"],
                "WEBCODEX_BENCH_DRIVER_RESULT": str(receipt_path),
            }
        )
        completed = _run_driver(driver_argv, cwd=workspace, env=env)
        ended = time.time_ns() // 1_000_000
        if not receipt_path.exists():
            return {
                "case_id": case["id"],
                "variant": variant,
                "surface": surface,
                "status": "fail",
                "driver_exit_code": completed.returncode,
                "reason_code": "driver_receipt_missing",
                "fixture_oracle": {"available": False, "passed": None, "checks": [], "reason": "driver receipt missing"},
                "summary": None,
            }

        receipt = _load_driver_receipt(receipt_path)
        if receipt.get("task_timing") is None:
            receipt["task_timing"] = {"started_at_ms": started, "ended_at_ms": ended}
        oracle = _fixture_oracle(case, workspace)
        annotation = _annotation(
            case,
            variant=variant,
            surface=surface,
            base_revision=base_revision,
            receipt=receipt,
            oracle=oracle,
        )
        annotation_path.write_text(_stable_json(annotation), encoding="utf-8")
        summary = _summary_for_receipt(
            case,
            variant=variant,
            surface=surface,
            base_revision=base_revision,
            receipt=receipt,
            annotation_path=annotation_path,
            case_manifest=case_manifest,
        )
        effective_status = receipt["status"]
        if completed.returncode != 0 and effective_status in ("pass", "partial"):
            effective_status = "fail"
        if oracle["available"] and oracle["passed"] is not True and effective_status in ("pass", "partial"):
            effective_status = "fail"
        if (
            (case.get("correctness") or {}).get("workspace_must_remain_clean") is True
            and _workspace_status(workspace)
            and effective_status in ("pass", "partial")
        ):
            effective_status = "fail"
        if summary is not None and effective_status == "pass":
            correctness = summary.get("correctness") or {}
            task_verdict = correctness.get("task_verdict")
            validation_verdict = correctness.get("validation_verdict")
            if task_verdict == "fail" or validation_verdict == "fail":
                effective_status = "fail"
            elif task_verdict != "pass":
                effective_status = "partial"
            elif case["validation"]["required"] and validation_verdict != "pass":
                effective_status = "partial"
            elif not case["validation"]["required"] and validation_verdict not in ("pass", "not_required"):
                effective_status = "partial"

        if (
            case["id"] == "long_validation_handoff"
            and summary is not None
            and effective_status == "pass"
            and report._get_path(summary, "job_convergence.pending_handoff_count") in (None, 0)
        ):
            effective_status = "partial"

        if (
            case["id"] == "guarded_multi_file_edit"
            and summary is not None
            and effective_status == "pass"
            and not _guarded_multi_file_contract_proven(summary, variant)
        ):
            effective_status = "partial"
        return {
            "case_id": case["id"],
            "variant": variant,
            "surface": surface,
            "status": effective_status,
            "driver_exit_code": completed.returncode,
            "fixture_oracle": oracle,
            "summary": summary,
        }
    except (BenchmarkError, report.ReportError):
        return {
            "case_id": case["id"],
            "variant": variant,
            "surface": surface,
            "status": "fail",
            "driver_exit_code": None,
            "reason_code": "sample_contract_error",
            "fixture_oracle": {"available": False, "passed": None, "checks": [], "reason": "sample failed before oracle"},
            "summary": None,
        }
    finally:
        if worktree_added:
            _worktree_remove(repo, workspace)


def _comparison(direct: dict[str, Any], code_mode: dict[str, Any]) -> dict[str, Any] | None:
    if direct.get("status") != "pass" or code_mode.get("status") != "pass":
        return None
    if direct.get("summary") is None or code_mode.get("summary") is None:
        return None
    return report.compare_reports(direct["summary"], code_mode["summary"])


def _pair_order(pair_index: int) -> tuple[str, str]:
    return VARIANTS if pair_index % 2 == 0 else tuple(reversed(VARIANTS))


def run_benchmark(
    *,
    repo: Path,
    base_revision: str,
    driver_argv: list[str],
    case_ids: list[str],
    pairs: int,
) -> dict[str, Any]:
    if pairs < 1:
        raise BenchmarkError("pairs must be at least 1")
    resolved = _require_exact_revision(repo, base_revision)
    if _head_revision(repo) != resolved:
        raise BenchmarkError(
            "base revision must equal the current checkout HEAD; check out the target commit before benchmarking"
        )
    temp_owner = tempfile.TemporaryDirectory(prefix="webcodex-agent-loop-bench-")
    root = Path(temp_owner.name)
    for name in ("worktrees", "receipts", "annotations"):
        (root / name).mkdir(parents=True, exist_ok=True)
    case_manifest = root / "agent_loop_cases.json"
    case_manifest.write_text(_read_manifest_at_revision(repo, resolved), encoding="utf-8")
    manifest = report.load_case_manifest(case_manifest)
    selected = [_case(manifest, case_id) for case_id in case_ids]

    cases_out: list[dict[str, Any]] = []
    try:
        for case in selected:
            pair_outputs: list[dict[str, Any]] = []
            for pair_index in range(pairs):
                samples: list[dict[str, Any]] = []
                order = _pair_order(pair_index)
                for ordinal, variant in enumerate(order):
                    samples.append(
                        _run_sample(
                            repo,
                            root,
                            driver_argv,
                            case,
                            variant=variant,
                            pair_index=pair_index,
                            ordinal=ordinal,
                            base_revision=resolved,
                            case_manifest=case_manifest,
                        )
                    )
                by_variant = {sample["variant"]: sample for sample in samples}
                pair_outputs.append(
                    {
                        "pair_index": pair_index,
                        "order": list(order),
                        "samples": samples,
                        "comparison": _comparison(by_variant["direct"], by_variant["code_mode"]),
                    }
                )
            cases_out.append({"case_id": case["id"], "pairs": pair_outputs})

        statuses = [
            sample["status"]
            for case in cases_out
            for pair in case["pairs"]
            for sample in pair["samples"]
        ]
        return {
            "schema_version": 1,
            "kind": "agent_loop_benchmark",
            "base_revision": resolved,
            "pair_count": pairs,
            "case_ids": case_ids,
            "sample_count": len(statuses),
            "status_counts": {status: statuses.count(status) for status in sorted(STATUS_VALUES)},
            "cases": cases_out,
            "notes": [
                "each sample ran in a fresh detached Git worktree",
                "pair order alternates direct/code_mode then code_mode/direct",
                "driver-reported unsupported, partial, and failed samples remain in the result",
                "comparisons reuse agent_loop_report exact base/case fingerprint/correctness gates",
                "outer-call metrics remain proxies; exact model round trips stay unavailable without Host turn evidence",
            ],
        }
    finally:
        if temp_owner is not None:
            temp_owner.cleanup()


def _human_summary(result: dict[str, Any]) -> str:
    counts = result["status_counts"]
    lines = [
        f"Agent Loop benchmark {result['base_revision'][:8]}",
        f"cases={len(result['case_ids'])} pairs={result['pair_count']} samples={result['sample_count']}",
        "status " + " ".join(f"{key}={counts[key]}" for key in sorted(counts)),
    ]
    for case in result["cases"]:
        orders = ["/".join(pair["order"]) for pair in case["pairs"]]
        comparable = sum(
            1
            for pair in case["pairs"]
            if isinstance(pair.get("comparison"), dict)
            and (pair["comparison"].get("throughput_compatibility") or {}).get("comparable") is True
        )
        lines.append(f"{case['case_id']}: orders={','.join(orders)} comparable_pairs={comparable}/{len(orders)}")
    return "\n".join(lines)


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, default=Path("."))
    parser.add_argument("--base-revision", required=True)
    parser.add_argument("--driver", required=True, help="Host driver command, parsed with shlex; no shell is invoked")
    parser.add_argument("--case-id", action="append", dest="case_ids")
    parser.add_argument("--pairs", type=int, default=2)
    parser.add_argument("--output", type=Path)
    return parser


def main(argv: list[str] | None = None) -> int:
    args = _parser().parse_args(argv)
    try:
        driver_argv = shlex.split(args.driver)
        if not driver_argv:
            raise BenchmarkError("--driver must name an executable")
        result = run_benchmark(
            repo=args.repo.resolve(),
            base_revision=args.base_revision,
            driver_argv=driver_argv,
            case_ids=args.case_ids or list(DEFAULT_CASES),
            pairs=args.pairs,
        )
        if args.output:
            args.output.parent.mkdir(parents=True, exist_ok=True)
            args.output.write_text(_stable_json(result), encoding="utf-8")
        print(_human_summary(result))
        if not args.output:
            print(_stable_json(result), end="")
        return 0 if result["status_counts"]["fail"] == 0 else 1
    except (BenchmarkError, report.ReportError) as exc:
        print(f"agent_loop_benchmark: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
