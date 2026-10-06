from __future__ import annotations

import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

from scripts import agent_loop_benchmark as benchmark
from scripts import agent_loop_report as report


class AgentLoopBenchmarkTests(unittest.TestCase):
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

    def test_unsupported_driver_samples_are_retained_and_worktrees_cleaned(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            repo = root / "repo"
            repo.mkdir()
            subprocess.run(["git", "init", "-b", "main"], cwd=repo, check=True, stdout=subprocess.DEVNULL)
            subprocess.run(["git", "config", "user.email", "bench@test.local"], cwd=repo, check=True)
            subprocess.run(["git", "config", "user.name", "Bench"], cwd=repo, check=True)
            (repo / "README.md").write_text("fixture\n", encoding="utf-8")
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

    def test_base_revision_must_match_checkout_head(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            repo = root / "repo"
            repo.mkdir()
            subprocess.run(["git", "init", "-b", "main"], cwd=repo, check=True, stdout=subprocess.DEVNULL)
            subprocess.run(["git", "config", "user.email", "bench@test.local"], cwd=repo, check=True)
            subprocess.run(["git", "config", "user.name", "Bench"], cwd=repo, check=True)
            (repo / "README.md").write_text("one\n", encoding="utf-8")
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

    def test_cli_prints_human_summary_and_json_output(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            repo = root / "repo"
            repo.mkdir()
            subprocess.run(["git", "init", "-b", "main"], cwd=repo, check=True, stdout=subprocess.DEVNULL)
            subprocess.run(["git", "config", "user.email", "bench@test.local"], cwd=repo, check=True)
            subprocess.run(["git", "config", "user.name", "Bench"], cwd=repo, check=True)
            (repo / "README.md").write_text("fixture\n", encoding="utf-8")
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
