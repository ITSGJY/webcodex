#!/usr/bin/env python3
"""Read many GitHub PR/CI statuses using one gh CLI invocation (no mutations).

An ordinary multi-PR review should not perform one gh pr view network request
per item. Read exactly one bounded, structured gh pr list snapshot instead.
"""
from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys

DEFAULT_REPO = "yyjeqhc/webcodex"
PR_FIELDS = "number,title,mergeStateStatus,statusCheckRollup"
MAX_OUTPUT_BYTES = 4 * 1024 * 1024
REPO_PATTERN = re.compile(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+\Z")


class PrHealthError(ValueError):
    """Cannot safely summarize a GitHub PR snapshot."""


def load_open_prs(repo: str, limit: int, *, runner=subprocess.run) -> list[dict]:
    if not REPO_PATTERN.fullmatch(repo):
        raise PrHealthError("repository must be a literal owner/name")
    if not 1 <= limit <= 100:
        raise PrHealthError("limit must be 1..100")
    argv = [
        "gh", "pr", "list", "--repo", repo, "--state", "open",
        "--limit", str(limit), "--json", PR_FIELDS,
    ]
    try:
        result = runner(
            argv,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=60,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired) as exc:
        raise PrHealthError("GitHub PR list could not be retrieved") from exc
    if result.returncode != 0:
        # Do not mirror CLI stderr: it may contain private URLs or tokens.
        raise PrHealthError("GitHub PR list failed; check gh login and repo access")
    if len(result.stdout.encode("utf-8")) > MAX_OUTPUT_BYTES:
        raise PrHealthError("GitHub PR list exceeded the bounded JSON budget")
    try:
        data = json.loads(result.stdout)
    except (ValueError, TypeError) as exc:
        raise PrHealthError("GitHub PR list did not return JSON") from exc
    if not isinstance(data, list) or len(data) > limit:
        raise PrHealthError("GitHub PR list returned an invalid or unbounded array")
    seen: set[int] = set()
    for entry in data:
        if not isinstance(entry, dict):
            raise PrHealthError("GitHub PR list contains a malformed record")
        number = entry.get("number")
        if type(number) is not int or number <= 0 or number in seen:
            raise PrHealthError("GitHub PR list contains invalid or duplicate PR numbers")
        seen.add(number)
        if (not isinstance(entry.get("title"), str)
                or not isinstance(entry.get("mergeStateStatus"), str)
                or not isinstance(entry.get("statusCheckRollup"), (list, type(None)))):
            raise PrHealthError("GitHub PR list is missing expected structured fields")
        for check in entry["statusCheckRollup"] or []:
            if not isinstance(check, dict):
                raise PrHealthError("GitHub PR contains an invalid CI status")
    return data


def summarize(pr: dict) -> dict:
    checks = pr["statusCheckRollup"]
    counts = {"success": 0, "pending": 0, "failed": 0, "skipped": 0}
    for check in checks or []:
        if check.get("__typename") == "StatusContext":
            # GitHub status contexts carry state, not CheckRun status/conclusion.
            state = check.get("state")
            if state == "SUCCESS":
                counts["success"] += 1
            elif state == "PENDING":
                counts["pending"] += 1
            else:
                counts["failed"] += 1
        elif check.get("status") != "COMPLETED":
            counts["pending"] += 1
        elif check.get("conclusion") == "SUCCESS":
            counts["success"] += 1
        elif check.get("conclusion") in ("SKIPPED", "NEUTRAL"):
            counts["skipped"] += 1
        else:
            # Missing and non-success terminal conclusions are not all-green.
            counts["failed"] += 1
    return {
        "number": pr["number"],
        "title": pr["title"],
        "merge_status": pr["mergeStateStatus"],
        "checks_known": checks is not None and len(checks) > 0,
        **counts,
    }


def safe_cell(value: str) -> str:
    """Avoid untrusted PR titles breaking Markdown tables or terminals."""
    cleaned = "".join(c if 32 <= ord(c) != 127 else " " for c in value)
    return cleaned.replace("|", r"\|").strip()[:160]


def report(prs: list[dict], *, only_perf: bool = False,
           numbers: set[int] | None = None, limit: int = 50) -> str:
    rows = sorted((summarize(pr) for pr in prs), key=lambda row: row["number"], reverse=True)
    if only_perf:
        rows = [row for row in rows if row["title"].casefold().startswith("perf(")]
    if numbers is not None:
        rows = [row for row in rows if row["number"] in numbers]
    lines = [
        f"Open PR health: {len(rows)} selected, {len(prs)} fetched (one gh call)",
        "| PR | Title | Merge | Passed | Pending | Failed | Skipped | CI |",
        "| --- | --- | --- | ---: | ---: | ---: | ---: | --- |",
    ]
    for row in rows:
        ci = "unknown" if not row["checks_known"] else (
            "failed+pending" if row["failed"] and row["pending"] else
            "failed" if row["failed"] else
            "pending" if row["pending"] else
            "skipped-only" if not row["success"] else
            "reported-success"
        )
        lines.append(
            f"| #{row['number']} | {safe_cell(row['title'])} | "
            f"{safe_cell(row['merge_status'])} | {row['success']} | "
            f"{row['pending']} | {row['failed']} | {row['skipped']} | {ci} |"
        )
    if len(prs) == limit:
        lines.append("WARNING: fetch limit reached; additional open PRs may exist.")
    lines.append("CI statuses are a live snapshot, not an approval or merge decision.")
    return "\n".join(lines)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", default=DEFAULT_REPO, help="Explicit owner/repository")
    parser.add_argument("--limit", type=int, default=50, help="Fetch at most 1..100 open PRs")
    parser.add_argument("--only-perf", action="store_true", help="Filter titles starting perf(")
    parser.add_argument("--numbers", help="Comma-separated PR numbers to display")
    options = parser.parse_args(argv)
    try:
        ids: set[int] | None = None
        if options.numbers is not None:
            tokens = options.numbers.split(",")
            if not tokens or len(tokens) > 100 or any(
                not token.isdecimal() or int(token) <= 0 for token in tokens
            ):
                raise PrHealthError("--numbers expects comma-separated positive integers")
            ids = {int(token) for token in tokens}
        prs = load_open_prs(options.repo, options.limit)
        print(report(prs, only_perf=options.only_perf,
                     numbers=ids, limit=options.limit))
    except PrHealthError as exc:
        print(f"[open-pr-health] {exc}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
