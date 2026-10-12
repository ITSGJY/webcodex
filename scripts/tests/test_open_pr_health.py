"""One gh request for complete multi-PR health: bounded, fail-closed fixtures."""

import contextlib
import io
import json
import subprocess
import unittest
from unittest.mock import patch

from scripts.open_pr_health import (
    PrHealthError,
    load_open_prs,
    main,
    report,
    safe_cell,
    summarize,
)

FIXTURE = [
    {
        "number": 44,
        "title": "perf(dev): faster batching",
        "mergeStateStatus": "CLEAN",
        "statusCheckRollup": [
            {"status": "COMPLETED", "conclusion": "SUCCESS", "name": "linux"},
            {"status": "COMPLETED", "conclusion": "SUCCESS", "name": "windows"},
            {"status": "COMPLETED", "conclusion": "SKIPPED", "name": "macos"},
        ],
    },
    {
        "number": 43,
        "title": "fix: unexpected | table\nnewline",
        "mergeStateStatus": "BLOCKED",
        "statusCheckRollup": [
            {"status": "IN_PROGRESS", "conclusion": None, "name": "linux"},
            {"status": "COMPLETED", "conclusion": "FAILURE", "name": "test"},
        ],
    },
    {
        "number": 42,
        "title": "docs: no CI yet",
        "mergeStateStatus": "UNKNOWN",
        "statusCheckRollup": None,
    },
]


class MultiPrBatchHealthTests(unittest.TestCase):
    def test_one_native_gh_invocation_returns_full_ordered_snapshot(self):
        calls = []

        def fake_run(args, **kwargs):
            calls.append((args, kwargs))
            return subprocess.CompletedProcess(args, 0, stdout=json.dumps(FIXTURE), stderr="")

        got = load_open_prs("yyjeqhc/webcodex", 50, runner=fake_run)
        self.assertEqual(got, FIXTURE)
        self.assertEqual(len(calls), 1)
        argv, kwargs = calls[0]
        self.assertEqual(argv[:5], ["gh", "pr", "list", "--repo", "yyjeqhc/webcodex"])
        self.assertEqual(argv[5:], [
            "--state", "open", "--limit", "50", "--json",
            "number,title,mergeStateStatus,statusCheckRollup",
        ])
        self.assertEqual(kwargs["timeout"], 60)
        self.assertFalse(kwargs["check"])
        self.assertIs(kwargs["stdin"], subprocess.DEVNULL)
        self.assertIs(kwargs["stdout"], subprocess.PIPE)

    def test_exact_ci_accounting_no_skipped_as_success(self):
        ok, error, unknown = (summarize(item) for item in FIXTURE)
        self.assertEqual((ok["success"], ok["skipped"], ok["failed"], ok["pending"]),
                         (2, 1, 0, 0))
        self.assertEqual((error["success"], error["skipped"], error["failed"], error["pending"]),
                         (0, 0, 1, 1))
        self.assertFalse(unknown["checks_known"])

    def test_report_filter_and_explicit_ci_uncertainty(self):
        text = report(FIXTURE, limit=50)
        self.assertIn("reported-success", text)
        self.assertIn("pending", text)
        self.assertIn("unknown", text)
        self.assertIn(r"unexpected \| table newline", text)
        self.assertLess(text.index("#44"), text.index("#43"))
        self.assertEqual(len([line for line in text.splitlines() if line.startswith("| #")]), 3)
        perf = report(FIXTURE, only_perf=True, limit=50)
        self.assertIn("#44", perf)
        self.assertNotIn("#43", perf)
        selected = report(FIXTURE, numbers={43}, limit=50)
        self.assertIn("#43", selected)
        self.assertNotIn("#44", selected)

    def test_skipped_only_checks_are_not_marked_green(self):
        entry = dict(FIXTURE[0])
        entry["statusCheckRollup"] = [
            {"status": "COMPLETED", "conclusion": "SKIPPED"}
        ]
        self.assertIn("skipped-only", report([entry], limit=50))
        self.assertNotIn("reported-success", report([entry], limit=50))

    def test_limit_warning_does_not_claim_completeness(self):
        self.assertIn("additional open PRs may exist", report(FIXTURE[:2], limit=2))
        self.assertNotIn("additional open PRs may exist", report(FIXTURE[:2], limit=3))

    def test_status_context_success_and_failure_are_not_marked_pending(self):
        entry = dict(FIXTURE[0])
        entry["statusCheckRollup"] = [
            {"__typename": "StatusContext", "state": "SUCCESS"},
            {"__typename": "StatusContext", "state": "FAILURE"},
            {"__typename": "StatusContext", "state": "PENDING"},
            {"__typename": "StatusContext", "state": "ERROR"},
        ]
        counts = summarize(entry)
        self.assertEqual(
            (counts["success"], counts["failed"], counts["pending"]),
            (1, 2, 1),
        )
        self.assertIn("failed+pending", report([entry]))

    def test_failed_or_missing_check_is_not_marked_green(self):
        base = dict(FIXTURE[0])
        base["statusCheckRollup"] = [
            {"status": "COMPLETED", "conclusion": None},
            {"status": "COMPLETED", "conclusion": "CANCELLED"},
        ]
        row = summarize(base)
        self.assertEqual(row["failed"], 2)
        self.assertNotIn("reported-success", report([base], limit=50))

    def test_reject_invalid_scope_or_unbounded_limit_before_gh(self):
        fake = lambda *a, **kw: self.fail("Should not run gh")
        for repo in ("", "repo", "owner/repo; echo secret", "a/b/c"):
            with self.subTest(repo=repo), self.assertRaises(PrHealthError):
                load_open_prs(repo, 50, runner=fake)
        for limit in (0, -1, 101):
            with self.subTest(limit=limit), self.assertRaises(PrHealthError):
                load_open_prs("owner/repo", limit, runner=fake)

    def test_fail_closed_on_malformed_payload_and_duplicate_numbers(self):
        values = [
            "not json",
            '{"unexpected":"shape"}',
            json.dumps([{"number": 1}]),
            json.dumps([FIXTURE[0], FIXTURE[0]]),
            json.dumps([{"number": True, "title": "", "mergeStateStatus": "",
                         "statusCheckRollup": []}]),
            json.dumps([{"number": 1, "title": "", "mergeStateStatus": "",
                         "statusCheckRollup": [{"malformed": False}, 4]}]),
        ]
        for payload in values:
            with self.subTest(payload=payload[:80]):
                fake = lambda argv, **kw: subprocess.CompletedProcess(
                    argv, 0, stdout=payload, stderr=""
                )
                with self.assertRaises(PrHealthError):
                    load_open_prs("owner/repo", 50, runner=fake)

    def test_failed_gh_does_not_expose_cli_stderr(self):
        secret = "ACCESS_KEY_MUST_NOT_LEAK"
        fake = lambda argv, **kw: subprocess.CompletedProcess(
            argv, 1, stdout="", stderr=secret
        )
        with self.assertRaises(PrHealthError) as cm:
            load_open_prs("owner/repo", 50, runner=fake)
        self.assertNotIn(secret, str(cm.exception))

    def test_timeout_and_spawn_failure_rejected(self):
        for exc in (OSError("not found"), subprocess.TimeoutExpired(["gh"], 60)):
            def fail(*args, **kwargs):
                raise exc
            with self.assertRaises(PrHealthError):
                load_open_prs("owner/repo", 50, runner=fail)

    def test_entrypoint_one_snapshot_and_invalid_number_rejected(self):
        with patch("scripts.open_pr_health.load_open_prs", return_value=FIXTURE) as load:
            out = io.StringIO()
            with contextlib.redirect_stdout(out):
                self.assertEqual(main(["--only-perf", "--numbers", "44"]), 0)
            self.assertEqual(load.call_count, 1)
            self.assertIn("#44", out.getvalue())
            self.assertNotIn("#43", out.getvalue())
        with patch("scripts.open_pr_health.load_open_prs") as load:
            with contextlib.redirect_stderr(io.StringIO()):
                self.assertEqual(main(["--numbers", "1,-2"]), 1)
            load.assert_not_called()

    def test_untrusted_title_is_bounded_and_table_safe(self):
        text = safe_cell("hello|world\n" + ("x" * 600))
        self.assertNotIn("\n", text)
        self.assertIn(r"\|", text)
        self.assertLessEqual(len(text), 161)


if __name__ == "__main__":
    unittest.main()
