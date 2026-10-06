from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path

from scripts import agent_loop_benchmark as benchmark
from scripts import agent_loop_report as report


class AgentLoopBenchmarkTests(unittest.TestCase):
    def _install_manifest(self, repo: Path) -> None:
        target = repo / "scripts/agent_loop_cases.json"
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(
            report.DEFAULT_CASE_MANIFEST.read_text(encoding="utf-8"),
            encoding="utf-8",
        )

    def test_pair_order_alternates(self) -> None:
        self.assertEqual(benchmark._pair_order(0), ("direct", "code_mode"))
        self.assertEqual(benchmark._pair_order(1), ("code_mode", "direct"))
        self.assertEqual(benchmark._pair_order(2), ("direct", "code_mode"))

    def test_driver_stdin_is_closed(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            output = Path(tmp) / "stdin.txt"
            benchmark._run_driver(
                [
                    sys.executable,
                    "-c",
                    "from pathlib import Path; import sys; Path(sys.argv[1]).write_text(sys.stdin.read())",
                    str(output),
                ],
                cwd=Path(tmp),
                env={},
            )
            self.assertEqual(output.read_text(encoding="utf-8"), "")

    def test_multi_file_contract_requires_one_edit_search_and_post_read_capacity(self) -> None:
        direct = {
            "canonical_calls": {
                "by_name": {
                    "edit_project_files": 1,
                    "search_project_texts": 1,
                    "read_files": 2,
                }
            }
        }
        self.assertTrue(benchmark._guarded_multi_file_contract_proven(direct, "direct"))
        direct["canonical_calls"]["by_name"]["edit_project_files"] = 2
        self.assertFalse(benchmark._guarded_multi_file_contract_proven(direct, "direct"))

        code_mode = {
            "availability": {"code_mode_composition": {"available": True}},
            "composition": {
                "nested_tool_counts": {
                    "edit_project_files": 1,
                    "search_project_texts": 1,
                    "read_files": 2,
                }
            },
        }
        self.assertTrue(benchmark._guarded_multi_file_contract_proven(code_mode, "code_mode"))
        code_mode["composition"]["nested_tool_counts"]["read_files"] = 1
        self.assertFalse(benchmark._guarded_multi_file_contract_proven(code_mode, "code_mode"))

    def test_fixture_oracle_checks_both_files_and_exact_diff(self) -> None:
        manifest = report.load_case_manifest(report.DEFAULT_CASE_MANIFEST)
        case = report._case_by_id(manifest, "guarded_multi_file_edit")
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            subprocess.run(["git", "init", "-b", "main"], cwd=root, check=True, stdout=subprocess.DEVNULL)
            subprocess.run(["git", "config", "user.email", "bench@test.local"], cwd=root, check=True)
            subprocess.run(["git", "config", "user.name", "Bench"], cwd=root, check=True)
            fixture = root / "tests/fixtures/agent-loop-multi-file"
            fixture.mkdir(parents=True)
            (fixture / "alpha.txt").write_text(
                "BENCH_TARGET_ALPHA=before\nALPHA_UNRELATED_SENTINEL=keep\n",
                encoding="utf-8",
            )
            (fixture / "beta.txt").write_text(
                "BENCH_TARGET_BETA=before\nBETA_UNRELATED_SENTINEL=keep\n",
                encoding="utf-8",
            )
            subprocess.run(["git", "add", "."], cwd=root, check=True)
            subprocess.run(["git", "commit", "-m", "fixture"], cwd=root, check=True, stdout=subprocess.DEVNULL)
            (fixture / "alpha.txt").write_text(
                "BENCH_TARGET_ALPHA=after\nALPHA_UNRELATED_SENTINEL=keep\n",
                encoding="utf-8",
            )
            (fixture / "beta.txt").write_text(
                "BENCH_TARGET_BETA=after\nBETA_UNRELATED_SENTINEL=keep\n",
                encoding="utf-8",
            )
            result = benchmark._fixture_oracle(case, root)
            self.assertTrue(result["available"])
            self.assertTrue(result["passed"])

            subprocess.run(["git", "add", "."], cwd=root, check=True)
            staged = benchmark._fixture_oracle(case, root)
            self.assertTrue(staged["passed"])

            (fixture / "beta.txt").write_text(
                "BENCH_TARGET_BETA=after\nBETA_UNRELATED_SENTINEL=lost\n",
                encoding="utf-8",
            )
            result = benchmark._fixture_oracle(case, root)
            self.assertFalse(result["passed"])

            (fixture / "beta.txt").write_text(
                "BENCH_TARGET_BETA=after\nBETA_UNRELATED_SENTINEL=keep\n",
                encoding="utf-8",
            )
            (root / "unexpected.txt").write_text("unexpected\n", encoding="utf-8")
            result = benchmark._fixture_oracle(case, root)
            self.assertFalse(result["passed"])
            changed_check = next(check for check in result["checks"] if check.get("kind") == "changed_files")
            self.assertIn("unexpected.txt", changed_check["actual"])

            (root / "unexpected.txt").unlink()
            (fixture / "beta.txt").write_bytes(b"\xff\xfe")
            result = benchmark._fixture_oracle(case, root)
            self.assertFalse(result["passed"])
            beta_check = next(check for check in result["checks"] if check.get("path", "").endswith("beta.txt"))
            self.assertEqual(beta_check["reason"], "file is not UTF-8")

            (fixture / "beta.txt").write_bytes(b"x" * (benchmark.MAX_FIXTURE_ORACLE_BYTES + 1))
            result = benchmark._fixture_oracle(case, root)
            self.assertFalse(result["passed"])
            beta_check = next(check for check in result["checks"] if check.get("path", "").endswith("beta.txt"))
            self.assertEqual(beta_check["reason"], "file exceeds oracle byte limit")

    def test_malformed_receipt_status_is_a_contract_error(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            receipt = Path(tmp) / "receipt.json"
            receipt.write_text('{"status": []}', encoding="utf-8")
            with self.assertRaisesRegex(benchmark.BenchmarkError, "status must be"):
                benchmark._load_driver_receipt(receipt)

    def test_comparison_requires_two_effective_pass_samples(self) -> None:
        summary = {"benchmark": {}}
        self.assertIsNone(
            benchmark._comparison(
                {"status": "partial", "summary": summary},
                {"status": "pass", "summary": summary},
            )
        )
        self.assertIsNone(
            benchmark._comparison(
                {"status": "pass", "summary": None},
                {"status": "pass", "summary": summary},
            )
        )

    def test_missing_receipts_are_retained_as_failed_samples(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            repo = root / "repo"
            repo.mkdir()
            subprocess.run(["git", "init", "-b", "main"], cwd=repo, check=True, stdout=subprocess.DEVNULL)
            subprocess.run(["git", "config", "user.email", "bench@test.local"], cwd=repo, check=True)
            subprocess.run(["git", "config", "user.name", "Bench"], cwd=repo, check=True)
            (repo / "README.md").write_text("fixture\n", encoding="utf-8")
            self._install_manifest(repo)
            subprocess.run(["git", "add", "."], cwd=repo, check=True)
            subprocess.run(["git", "commit", "-m", "init"], cwd=repo, check=True, stdout=subprocess.DEVNULL)
            base = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=repo, text=True).strip()
            result = benchmark.run_benchmark(
                repo=repo,
                base_revision=base,
                driver_argv=[sys.executable, "-c", "pass"],
                case_ids=["readonly_review"],
                pairs=1,
            )
            self.assertEqual(result["status_counts"]["fail"], 2)
            for sample in result["cases"][0]["pairs"][0]["samples"]:
                self.assertEqual(sample["reason_code"], "driver_receipt_missing")
                self.assertIsNone(sample["summary"])
            self.assertIsNone(result["cases"][0]["pairs"][0]["comparison"])

    def test_unsupported_driver_samples_are_retained_and_worktrees_cleaned(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            repo = root / "repo"
            repo.mkdir()
            subprocess.run(["git", "init", "-b", "main"], cwd=repo, check=True, stdout=subprocess.DEVNULL)
            subprocess.run(["git", "config", "user.email", "bench@test.local"], cwd=repo, check=True)
            subprocess.run(["git", "config", "user.name", "Bench"], cwd=repo, check=True)
            (repo / "README.md").write_text("fixture\n", encoding="utf-8")
            self._install_manifest(repo)
            subprocess.run(["git", "add", "."], cwd=repo, check=True)
            subprocess.run(["git", "commit", "-m", "init"], cwd=repo, check=True, stdout=subprocess.DEVNULL)
            base = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=repo, text=True).strip()

            driver = root / "driver.py"
            driver.write_text(
                "import json, os\n"
                "from pathlib import Path\n"
                "Path(os.environ['WEBCODEX_BENCH_DRIVER_RESULT']).write_text("
                "json.dumps({'status':'unsupported','message':'fixture driver'}), encoding='utf-8')\n",
                encoding="utf-8",
            )
            result = benchmark.run_benchmark(
                repo=repo,
                base_revision=base,
                driver_argv=[sys.executable, str(driver)],
                case_ids=["readonly_review"],
                pairs=2,
            )
            self.assertEqual(result["sample_count"], 4)
            self.assertEqual(result["status_counts"]["unsupported"], 4)
            self.assertEqual(result["cases"][0]["pairs"][0]["order"], ["direct", "code_mode"])
            self.assertEqual(result["cases"][0]["pairs"][1]["order"], ["code_mode", "direct"])
            self.assertIsNone(result["cases"][0]["pairs"][0]["comparison"])

            worktrees = subprocess.check_output(["git", "worktree", "list", "--porcelain"], cwd=repo, text=True)
            self.assertEqual(worktrees.count("worktree "), 1)

    def test_cleanup_does_not_prune_unrelated_worktree(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            repo = root / "repo"
            repo.mkdir()
            subprocess.run(["git", "init", "-b", "main"], cwd=repo, check=True, stdout=subprocess.DEVNULL)
            subprocess.run(["git", "config", "user.email", "bench@test.local"], cwd=repo, check=True)
            subprocess.run(["git", "config", "user.name", "Bench"], cwd=repo, check=True)
            (repo / "README.md").write_text("fixture\n", encoding="utf-8")
            self._install_manifest(repo)
            subprocess.run(["git", "add", "."], cwd=repo, check=True)
            subprocess.run(["git", "commit", "-m", "init"], cwd=repo, check=True, stdout=subprocess.DEVNULL)
            base = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=repo, text=True).strip()

            unrelated = root / "unrelated"
            subprocess.run(["git", "worktree", "add", "--detach", str(unrelated), base], cwd=repo, check=True, stdout=subprocess.DEVNULL)
            driver = root / "driver.py"
            driver.write_text(
                "import json, os\n"
                "from pathlib import Path\n"
                "Path(os.environ['WEBCODEX_BENCH_DRIVER_RESULT']).write_text("
                "json.dumps({'status':'unsupported'}), encoding='utf-8')\n",
                encoding="utf-8",
            )
            benchmark.run_benchmark(
                repo=repo,
                base_revision=base,
                driver_argv=[sys.executable, str(driver)],
                case_ids=["readonly_review"],
                pairs=1,
            )
            status = subprocess.run(
                ["git", "status", "--porcelain"],
                cwd=unrelated,
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                check=False,
            )
            self.assertEqual(status.returncode, 0, status.stderr)
            subprocess.run(["git", "worktree", "remove", "--force", str(unrelated)], cwd=repo, check=True)

    def test_case_manifest_is_pinned_to_base_revision(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            repo = root / "repo"
            repo.mkdir()
            subprocess.run(["git", "init", "-b", "main"], cwd=repo, check=True, stdout=subprocess.DEVNULL)
            subprocess.run(["git", "config", "user.email", "bench@test.local"], cwd=repo, check=True)
            subprocess.run(["git", "config", "user.name", "Bench"], cwd=repo, check=True)
            (repo / "README.md").write_text("fixture\n", encoding="utf-8")
            self._install_manifest(repo)
            subprocess.run(["git", "add", "."], cwd=repo, check=True)
            subprocess.run(["git", "commit", "-m", "init"], cwd=repo, check=True, stdout=subprocess.DEVNULL)
            base = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=repo, text=True).strip()

            manifest_path = repo / "scripts/agent_loop_cases.json"
            dirty = json.loads(manifest_path.read_text(encoding="utf-8"))
            dirty["cases"][0]["prompt"] = "dirty prompt must not be observed"
            manifest_path.write_text(json.dumps(dirty), encoding="utf-8")

            observed = root / "observed.txt"
            driver = root / "driver.py"
            driver.write_text(
                "import os, sys\n"
                "from pathlib import Path\n"
                "Path(sys.argv[1]).write_text(os.environ['WEBCODEX_BENCH_PROMPT'], encoding='utf-8')\n"
                "Path(os.environ['WEBCODEX_BENCH_DRIVER_RESULT']).write_text('{\"status\":\"unsupported\"}', encoding='utf-8')\n",
                encoding="utf-8",
            )
            benchmark.run_benchmark(
                repo=repo,
                base_revision=base,
                driver_argv=[sys.executable, str(driver), str(observed)],
                case_ids=["readonly_review"],
                pairs=1,
            )
            pinned = report.load_case_manifest(report.DEFAULT_CASE_MANIFEST)
            expected = report._case_by_id(pinned, "readonly_review")["prompt"]
            self.assertEqual(observed.read_text(encoding="utf-8"), expected)

    def test_base_revision_must_match_checkout_head(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            repo = root / "repo"
            repo.mkdir()
            subprocess.run(["git", "init", "-b", "main"], cwd=repo, check=True, stdout=subprocess.DEVNULL)
            subprocess.run(["git", "config", "user.email", "bench@test.local"], cwd=repo, check=True)
            subprocess.run(["git", "config", "user.name", "Bench"], cwd=repo, check=True)
            (repo / "README.md").write_text("one\n", encoding="utf-8")
            self._install_manifest(repo)
            subprocess.run(["git", "add", "."], cwd=repo, check=True)
            subprocess.run(["git", "commit", "-m", "one"], cwd=repo, check=True, stdout=subprocess.DEVNULL)
            old = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=repo, text=True).strip()
            (repo / "README.md").write_text("two\n", encoding="utf-8")
            subprocess.run(["git", "commit", "-am", "two"], cwd=repo, check=True, stdout=subprocess.DEVNULL)
            with self.assertRaisesRegex(benchmark.BenchmarkError, "current checkout HEAD"):
                benchmark.run_benchmark(
                    repo=repo,
                    base_revision=old,
                    driver_argv=[sys.executable, "-c", "raise SystemExit(0)"],
                    case_ids=["readonly_review"],
                    pairs=1,
                )

    def test_fixture_oracle_uses_recorded_base_when_head_advances(self) -> None:
        manifest = report.load_case_manifest(report.DEFAULT_CASE_MANIFEST)
        case = report._case_by_id(manifest, "guarded_multi_file_edit")
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            subprocess.run(["git", "init", "-b", "main"], cwd=root, check=True, stdout=subprocess.DEVNULL)
            subprocess.run(["git", "config", "user.email", "bench@test.local"], cwd=root, check=True)
            subprocess.run(["git", "config", "user.name", "Bench"], cwd=root, check=True)
            fixture = root / "tests/fixtures/agent-loop-multi-file"
            fixture.mkdir(parents=True)
            (fixture / "alpha.txt").write_text(
                "BENCH_TARGET_ALPHA=before\nALPHA_UNRELATED_SENTINEL=keep\n",
                encoding="utf-8",
            )
            (fixture / "beta.txt").write_text(
                "BENCH_TARGET_BETA=before\nBETA_UNRELATED_SENTINEL=keep\n",
                encoding="utf-8",
            )
            subprocess.run(["git", "add", "."], cwd=root, check=True)
            subprocess.run(["git", "commit", "-m", "base"], cwd=root, check=True, stdout=subprocess.DEVNULL)
            base = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root, text=True).strip()
            (fixture / "alpha.txt").write_text(
                "BENCH_TARGET_ALPHA=after\nALPHA_UNRELATED_SENTINEL=keep\n",
                encoding="utf-8",
            )
            (fixture / "beta.txt").write_text(
                "BENCH_TARGET_BETA=after\nBETA_UNRELATED_SENTINEL=keep\n",
                encoding="utf-8",
            )
            subprocess.run(["git", "add", "."], cwd=root, check=True)
            subprocess.run(["git", "commit", "-m", "driver commit"], cwd=root, check=True, stdout=subprocess.DEVNULL)
            oracle = benchmark._fixture_oracle(case, root, base)
            changed = next(check for check in oracle["checks"] if check.get("kind") == "changed_files")
            self.assertTrue(changed["passed"])
            self.assertEqual(changed["actual"], sorted(case["correctness"]["changed_files"]))
            self.assertNotEqual(subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root, text=True).strip(), base)

    @unittest.skipIf(os.name == "nt", "POSIX process-group regression")
    def test_driver_timeout_terminates_descendant_process_group(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            marker = root / "descendant-wrote"
            driver = root / "driver.py"
            driver.write_text(
                "import subprocess, sys, time\n"
                "subprocess.Popen([sys.executable, '-c', "
                "'import pathlib,time,sys; time.sleep(1); pathlib.Path(sys.argv[1]).write_text(\\\"late\\\")', "
                "sys.argv[1]])\n"
                "time.sleep(10)\n",
                encoding="utf-8",
            )
            old_timeout = benchmark.MAX_DRIVER_SECS
            benchmark.MAX_DRIVER_SECS = 0.1
            try:
                with self.assertRaisesRegex(benchmark.BenchmarkError, "exceeded"):
                    benchmark._run_driver(
                        [sys.executable, str(driver), str(marker)],
                        cwd=root,
                        env=dict(os.environ),
                    )
            finally:
                benchmark.MAX_DRIVER_SECS = old_timeout
            time.sleep(1.2)
            self.assertFalse(marker.exists())

    def test_duplicate_case_ids_are_rejected_before_driver_execution(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            repo = root / "repo"
            repo.mkdir()
            subprocess.run(["git", "init", "-b", "main"], cwd=repo, check=True, stdout=subprocess.DEVNULL)
            subprocess.run(["git", "config", "user.email", "bench@test.local"], cwd=repo, check=True)
            subprocess.run(["git", "config", "user.name", "Bench"], cwd=repo, check=True)
            (repo / "README.md").write_text("fixture\n", encoding="utf-8")
            self._install_manifest(repo)
            subprocess.run(["git", "add", "."], cwd=repo, check=True)
            subprocess.run(["git", "commit", "-m", "init"], cwd=repo, check=True, stdout=subprocess.DEVNULL)
            base = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=repo, text=True).strip()

            marker = root / "driver-ran"
            driver = root / "driver.py"
            driver.write_text(
                "from pathlib import Path; import sys; Path(sys.argv[1]).write_text('ran')\n",
                encoding="utf-8",
            )
            with self.assertRaisesRegex(benchmark.BenchmarkError, "duplicate case ids"):
                benchmark.run_benchmark(
                    repo=repo,
                    base_revision=base,
                    driver_argv=[sys.executable, str(driver), str(marker)],
                    case_ids=["readonly_review", "readonly_review"],
                    pairs=1,
                )
            self.assertFalse(marker.exists())

    def test_run_sample_clears_stale_receipt_before_driver(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            repo = root / "repo"
            repo.mkdir()
            subprocess.run(["git", "init", "-b", "main"], cwd=repo, check=True, stdout=subprocess.DEVNULL)
            subprocess.run(["git", "config", "user.email", "bench@test.local"], cwd=repo, check=True)
            subprocess.run(["git", "config", "user.name", "Bench"], cwd=repo, check=True)
            (repo / "README.md").write_text("fixture\n", encoding="utf-8")
            self._install_manifest(repo)
            subprocess.run(["git", "add", "."], cwd=repo, check=True)
            subprocess.run(["git", "commit", "-m", "init"], cwd=repo, check=True, stdout=subprocess.DEVNULL)
            base = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=repo, text=True).strip()
            manifest = report.load_case_manifest(repo / "scripts/agent_loop_cases.json")
            case = report._case_by_id(manifest, "readonly_review")

            bench_root = root / "bench"
            for name in ("worktrees", "receipts", "annotations"):
                (bench_root / name).mkdir(parents=True, exist_ok=True)
            stale = bench_root / "receipts" / "readonly_review-p1-1-direct.json"
            stale.write_text('{"status":"unsupported"}', encoding="utf-8")
            sample = benchmark._run_sample(
                repo,
                bench_root,
                [sys.executable, "-c", "pass"],
                case,
                variant="direct",
                pair_index=0,
                ordinal=0,
                base_revision=base,
                case_manifest=repo / "scripts/agent_loop_cases.json",
            )
            self.assertEqual(sample["status"], "fail")
            self.assertEqual(sample["reason_code"], "driver_receipt_missing")

    def test_contract_error_preserves_driver_exit_code(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            repo = root / "repo"
            repo.mkdir()
            subprocess.run(["git", "init", "-b", "main"], cwd=repo, check=True, stdout=subprocess.DEVNULL)
            subprocess.run(["git", "config", "user.email", "bench@test.local"], cwd=repo, check=True)
            subprocess.run(["git", "config", "user.name", "Bench"], cwd=repo, check=True)
            (repo / "README.md").write_text("fixture\n", encoding="utf-8")
            self._install_manifest(repo)
            subprocess.run(["git", "add", "."], cwd=repo, check=True)
            subprocess.run(["git", "commit", "-m", "init"], cwd=repo, check=True, stdout=subprocess.DEVNULL)
            base = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=repo, text=True).strip()
            manifest = report.load_case_manifest(repo / "scripts/agent_loop_cases.json")
            case = report._case_by_id(manifest, "readonly_review")
            bench_root = root / "bench"
            for name in ("worktrees", "receipts", "annotations"):
                (bench_root / name).mkdir(parents=True, exist_ok=True)
            driver = root / "driver.py"
            driver.write_text(
                "import os\n"
                "from pathlib import Path\n"
                "Path(os.environ['WEBCODEX_BENCH_DRIVER_RESULT']).write_text('not-json', encoding='utf-8')\n"
                "raise SystemExit(7)\n",
                encoding="utf-8",
            )
            sample = benchmark._run_sample(
                repo,
                bench_root,
                [sys.executable, str(driver)],
                case,
                variant="direct",
                pair_index=0,
                ordinal=0,
                base_revision=base,
                case_manifest=repo / "scripts/agent_loop_cases.json",
            )
            self.assertEqual(sample["status"], "fail")
            self.assertEqual(sample["reason_code"], "sample_contract_error")
            self.assertEqual(sample["driver_exit_code"], 7)

    def test_fallback_task_timing_starts_after_worktree_setup(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            repo = root / "repo"
            repo.mkdir()
            subprocess.run(["git", "init", "-b", "main"], cwd=repo, check=True, stdout=subprocess.DEVNULL)
            subprocess.run(["git", "config", "user.email", "bench@test.local"], cwd=repo, check=True)
            subprocess.run(["git", "config", "user.name", "Bench"], cwd=repo, check=True)
            (repo / "README.md").write_text("fixture\n", encoding="utf-8")
            self._install_manifest(repo)
            subprocess.run(["git", "add", "."], cwd=repo, check=True)
            subprocess.run(["git", "commit", "-m", "init"], cwd=repo, check=True, stdout=subprocess.DEVNULL)
            base = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=repo, text=True).strip()
            manifest_path = repo / "scripts/agent_loop_cases.json"
            manifest = report.load_case_manifest(manifest_path)
            case = report._case_by_id(manifest, "readonly_review")
            bench_root = root / "bench"
            for name in ("worktrees", "receipts", "annotations"):
                (bench_root / name).mkdir(parents=True, exist_ok=True)
            driver = root / "driver.py"
            driver.write_text(
                "import os\n"
                "from pathlib import Path\n"
                "Path(os.environ['WEBCODEX_BENCH_DRIVER_RESULT']).write_text('{\"status\":\"unsupported\"}', encoding='utf-8')\n",
                encoding="utf-8",
            )

            setup_done_ms: list[int] = []
            original = benchmark._worktree_add

            def recorded_add(repo_arg: Path, path_arg: Path, revision_arg: str) -> None:
                original(repo_arg, path_arg, revision_arg)
                setup_done_ms.append(time.time_ns() // 1_000_000)

            benchmark._worktree_add = recorded_add
            try:
                benchmark._run_sample(
                    repo,
                    bench_root,
                    [sys.executable, str(driver)],
                    case,
                    variant="direct",
                    pair_index=0,
                    ordinal=0,
                    base_revision=base,
                    case_manifest=manifest_path,
                )
            finally:
                benchmark._worktree_add = original

            annotation = json.loads(
                (bench_root / "annotations" / "readonly_review-p1-1-direct.json").read_text(encoding="utf-8")
            )
            self.assertGreaterEqual(annotation["task_timing"]["started_at_ms"], setup_done_ms[0])

    def test_non_repository_case_target_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            repo = root / "repo"
            repo.mkdir()
            subprocess.run(["git", "init", "-b", "main"], cwd=repo, check=True, stdout=subprocess.DEVNULL)
            subprocess.run(["git", "config", "user.email", "bench@test.local"], cwd=repo, check=True)
            subprocess.run(["git", "config", "user.name", "Bench"], cwd=repo, check=True)
            (repo / "README.md").write_text("fixture\n", encoding="utf-8")
            self._install_manifest(repo)
            subprocess.run(["git", "add", "."], cwd=repo, check=True)
            subprocess.run(["git", "commit", "-m", "init"], cwd=repo, check=True, stdout=subprocess.DEVNULL)
            base = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=repo, text=True).strip()
            with self.assertRaisesRegex(benchmark.BenchmarkError, "supports only target=webcodex_repository"):
                benchmark.run_benchmark(
                    repo=repo,
                    base_revision=base,
                    driver_argv=[sys.executable, "-c", "pass"],
                    case_ids=["focused_edit_validation"],
                    pairs=1,
                )

    def test_repo_argument_is_canonicalized_from_subdirectory(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            repo = root / "repo"
            repo.mkdir()
            subprocess.run(["git", "init", "-b", "main"], cwd=repo, check=True, stdout=subprocess.DEVNULL)
            subprocess.run(["git", "config", "user.email", "bench@test.local"], cwd=repo, check=True)
            subprocess.run(["git", "config", "user.name", "Bench"], cwd=repo, check=True)
            (repo / "README.md").write_text("fixture\n", encoding="utf-8")
            self._install_manifest(repo)
            subprocess.run(["git", "add", "."], cwd=repo, check=True)
            subprocess.run(["git", "commit", "-m", "init"], cwd=repo, check=True, stdout=subprocess.DEVNULL)
            base = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=repo, text=True).strip()
            driver = root / "driver.py"
            driver.write_text(
                "import os\n"
                "from pathlib import Path\n"
                "Path(os.environ['WEBCODEX_BENCH_DRIVER_RESULT']).write_text('{\"status\":\"unsupported\"}', encoding='utf-8')\n",
                encoding="utf-8",
            )
            result = benchmark.run_benchmark(
                repo=repo / "scripts",
                base_revision=base,
                driver_argv=[sys.executable, str(driver)],
                case_ids=["readonly_review"],
                pairs=1,
            )
            self.assertEqual(result["status_counts"]["unsupported"], 2)

    def test_cli_prints_human_summary_and_json_output(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            repo = root / "repo"
            repo.mkdir()
            subprocess.run(["git", "init", "-b", "main"], cwd=repo, check=True, stdout=subprocess.DEVNULL)
            subprocess.run(["git", "config", "user.email", "bench@test.local"], cwd=repo, check=True)
            subprocess.run(["git", "config", "user.name", "Bench"], cwd=repo, check=True)
            (repo / "README.md").write_text("fixture\n", encoding="utf-8")
            self._install_manifest(repo)
            subprocess.run(["git", "add", "."], cwd=repo, check=True)
            subprocess.run(["git", "commit", "-m", "init"], cwd=repo, check=True, stdout=subprocess.DEVNULL)
            base = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=repo, text=True).strip()
            driver = root / "driver.py"
            driver.write_text(
                "import json, os\n"
                "from pathlib import Path\n"
                "Path(os.environ['WEBCODEX_BENCH_DRIVER_RESULT']).write_text("
                "json.dumps({'status':'unsupported'}), encoding='utf-8')\n",
                encoding="utf-8",
            )
            output = root / "result.json"
            completed = subprocess.run(
                [
                    sys.executable,
                    str(Path(benchmark.__file__)),
                    "--repo",
                    str(repo),
                    "--base-revision",
                    base,
                    "--driver",
                    f"{sys.executable} {driver}",
                    "--case-id",
                    "readonly_review",
                    "--pairs",
                    "1",
                    "--output",
                    str(output),
                ],
                cwd=Path(__file__).resolve().parents[2],
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                check=False,
            )
            self.assertEqual(completed.returncode, 0, completed.stderr)
            self.assertIn("Agent Loop benchmark", completed.stdout)
            value = json.loads(output.read_text(encoding="utf-8"))
            self.assertEqual(value["status_counts"]["unsupported"], 2)


if __name__ == "__main__":
    unittest.main()
