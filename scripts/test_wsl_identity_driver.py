"""Unit tests for `scripts/measure-wsl-identity.py` completeness and safety guards."""

from __future__ import annotations

import importlib.util
import json
import subprocess
import tempfile
import unittest
from pathlib import Path


def load_driver():
    script = Path(__file__).resolve().with_name("measure-wsl-identity.py")
    spec = importlib.util.spec_from_file_location("measure_wsl_identity", script)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


DRIVER = load_driver()


class MeasureWslIdentityDriverTests(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.root = Path(self._tmp.name).resolve()
        self.agent_logs = self.root / ".git" / "agent-logs"
        self.agent_logs.mkdir(parents=True)
        self.baseline = self.root / "baseline.exe"
        self.candidate = self.root / "candidate.exe"
        self.baseline.write_bytes(b"baseline-v1")
        self.candidate.write_bytes(b"candidate-v1")

    def tearDown(self) -> None:
        self._tmp.cleanup()

    def make_fake_runner(
        self,
        *,
        candidate_spawns: int = 1,
        corrupt_verdict: bool = False,
        mutate_binary_on_call: int | None = None,
        timeout_on_call: int | None = None,
        leak_in_diag: bool = False,
    ):
        mounted = "/mnt/c/fake-agent-logs/fx"
        calls: list[list[str]] = []
        binary_invocations = 0

        def expected_for_root(
            root: str,
        ) -> tuple[str, str | None, bool, str | None, str]:
            suffix = root.rsplit("/", 1)[-1]
            if suffix == "repo-alpha":
                return (
                    "git:github.com/example/alpha",
                    root,
                    False,
                    "git:github.com/example/alpha",
                    "resolved",
                )
            if suffix == "repo-beta":
                return (
                    "git:github.com/example/beta-repo",
                    root,
                    False,
                    "git:github.com/example/beta-repo",
                    "resolved",
                )
            if suffix.startswith("repo '"):
                return (
                    "git:github.com/example/special-repo",
                    root,
                    False,
                    "git:github.com/example/special-repo",
                    "resolved",
                )
            if suffix == "repo-no-origin":
                return (f"path:{root}", root, False, None, "no_identity")
            if suffix in ("wt-abs", "wt-rel"):
                return (
                    "git:github.com/example/worktree-host",
                    root,
                    False,
                    "git:github.com/example/worktree-host",
                    "resolved",
                )
            if suffix == "sub-child":
                return (
                    "git:github.com/example/sub-child",
                    root,
                    False,
                    "git:github.com/example/sub-child",
                    "resolved",
                )
            if suffix == "plain-folder":
                return (f"path:{root}", None, False, None, "no_identity")
            if suffix == "gone-checkout":
                return (f"path:{root}", None, True, None, "checkout_missing")
            raise AssertionError(f"unexpected synthetic root {root}")

        def fake_run(cmd, **kwargs):
            nonlocal binary_invocations
            calls.append([str(c) for c in cmd])
            if cmd[:2] == ["git", "rev-parse"]:
                return subprocess.CompletedProcess(
                    cmd, 0, stdout=f"{self.agent_logs}\n", stderr=""
                )
            if cmd[:3] == ["wsl.exe", "-l", "-q"]:
                return subprocess.CompletedProcess(
                    cmd, 0, stdout=b"Ubuntu\r\n", stderr=b""
                )
            if len(cmd) >= 5 and cmd[0] == "wsl.exe" and cmd[4] == "wslpath":
                return subprocess.CompletedProcess(
                    cmd, 0, stdout=f"{mounted}\n", stderr=""
                )

            binary_invocations += 1
            if timeout_on_call == binary_invocations:
                raise subprocess.TimeoutExpired(
                    cmd=cmd, timeout=1.0, output="partial-out", stderr="partial-err"
                )
            if mutate_binary_on_call == binary_invocations:
                self.candidate.write_bytes(b"candidate-mutated-mid-run")

            exe = Path(cmd[0]).resolve()
            is_candidate = exe == self.candidate.resolve()
            roots_file = Path(cmd[cmd.index("--roots-file") + 1])
            mode = cmd[cmd.index("--mode") + 1]
            roots = [
                line.strip()
                for line in roots_file.read_text(encoding="utf-8").splitlines()
                if line.strip()
            ]
            env = kwargs.get("env") or {}

            if mode == "single":
                r = roots[0]
                rid, repo_root, missing, _, _ = expected_for_root(r)
                if corrupt_verdict and is_candidate:
                    missing = not missing
                payload = {
                    "mode": "single",
                    "elapsed_ms": 120.0 if is_candidate else 480.0,
                    "spawn_attempts": candidate_spawns if is_candidate else 3,
                    "result": {
                        "root": r,
                        "id": rid,
                        "repo_root": repo_root,
                        "checkout_missing": missing,
                    },
                }
                return subprocess.CompletedProcess(
                    cmd, 0, stdout=json.dumps(payload), stderr=""
                )

            report_rows = []
            for r in roots:
                _, _, _, canon_id, verdict = expected_for_root(r)
                if corrupt_verdict and is_candidate and verdict == "checkout_missing":
                    verdict = "no_identity"
                report_rows.append(
                    {
                        "root_path": r,
                        "canonical_id": canon_id,
                        "verdict": verdict,
                        "detail": None,
                    }
                )
            payload = {
                "mode": "sweep",
                "elapsed_ms": 260.0 if is_candidate else 5200.0,
                "spawn_attempts": candidate_spawns if is_candidate else 30,
                "sweep": {
                    "registered": len(roots),
                    "recorded": 5,
                    "already_probed": 0,
                    "no_remote": 2 + (1 if corrupt_verdict and is_candidate else 0),
                    "checkout_missing": 0 if (corrupt_verdict and is_candidate) else 1,
                    "unresolved": 0,
                    "skipped_out_of_budget": 0,
                    "unreadable": None,
                },
                "report": report_rows,
            }
            stderr = ""
            if env.get("SVAULT_LOG"):
                lines = [
                    "[sv-wsl] identity batch: roots=8 snapshots=16 elapsed_ms=250.125 result=ok"
                ]
                for idx in range(len(roots)):
                    lines.append(
                        f"[sv-wsl] identity completed: root_sha256={idx:012x} identity_call={idx + 1} result=git_identity"
                    )
                if leak_in_diag:
                    lines.append(f"leaked {mounted}")
                stderr = "\n".join(lines) + "\n"
            return subprocess.CompletedProcess(
                cmd, 0, stdout=json.dumps(payload), stderr=stderr
            )

        return fake_run, calls

    def test_interleaved_run_succeeds_and_records_binary_hashes(self) -> None:
        runner, _ = self.make_fake_runner()
        out = self.agent_logs / "out-ok.json"
        report = DRIVER.run_experiment(
            baseline=self.baseline,
            candidate=self.candidate,
            fixture_dir=self.agent_logs / "fx-ok",
            output_path=out,
            rounds=2,
            project_dir=self.root,
            run=runner,
        )
        self.assertTrue(report["complete"])
        self.assertIsNone(report["failure"])
        self.assertEqual(len(report["summaries"]), 6)
        first_case = [
            s for s in report["samples"] if s["case"] == "single_standard_repo"
        ]
        self.assertEqual(
            [(s["iteration"], s["arm"]) for s in first_case],
            [(0, "baseline"), (0, "candidate"), (1, "candidate"), (1, "baseline")],
        )
        self.assertEqual(report["special_chars_smoke"]["spawn_attempts"], 1)
        self.assertEqual(report["binaries"], report["post_binaries"])

    def test_rejects_fixture_outside_agent_logs_before_wsl(self) -> None:
        runner, calls = self.make_fake_runner()
        with self.assertRaises(ValueError):
            DRIVER.run_experiment(
                baseline=self.baseline,
                candidate=self.candidate,
                fixture_dir=self.root / "outside-fixture",
                output_path=self.agent_logs / "out.json",
                rounds=2,
                project_dir=self.root,
                run=runner,
            )
        self.assertFalse(any(c[0] == "wsl.exe" for c in calls))

    def test_rejects_verdict_drift_and_persists_incomplete_report(self) -> None:
        runner, _ = self.make_fake_runner(corrupt_verdict=True)
        out = self.agent_logs / "out-drift.json"
        with self.assertRaises(RuntimeError):
            DRIVER.run_experiment(
                baseline=self.baseline,
                candidate=self.candidate,
                fixture_dir=self.agent_logs / "fx-drift",
                output_path=out,
                rounds=2,
                project_dir=self.root,
                run=runner,
            )
        saved = json.loads(out.read_text(encoding="utf-8"))
        self.assertFalse(saved["complete"])
        self.assertIn("mismatch", saved["failure"])

    def test_rejects_candidate_multi_spawn_regression(self) -> None:
        runner, _ = self.make_fake_runner(candidate_spawns=3)
        out = self.agent_logs / "out-spawns.json"
        with self.assertRaises(RuntimeError):
            DRIVER.run_experiment(
                baseline=self.baseline,
                candidate=self.candidate,
                fixture_dir=self.agent_logs / "fx-spawns",
                output_path=out,
                rounds=2,
                project_dir=self.root,
                run=runner,
            )
        saved = json.loads(out.read_text(encoding="utf-8"))
        self.assertFalse(saved["complete"])
        self.assertIn("spawn_attempts=[1]", saved["failure"])

    def test_retains_timeout_evidence_and_binary_mutation_evidence(self) -> None:
        runner_to, _ = self.make_fake_runner(timeout_on_call=2)
        out_to = self.agent_logs / "out-timeout.json"
        with self.assertRaises(RuntimeError):
            DRIVER.run_experiment(
                baseline=self.baseline,
                candidate=self.candidate,
                fixture_dir=self.agent_logs / "fx-timeout",
                output_path=out_to,
                rounds=2,
                project_dir=self.root,
                run=runner_to,
            )
        saved_to = json.loads(out_to.read_text(encoding="utf-8"))
        self.assertFalse(saved_to["complete"])
        self.assertTrue(any(s.get("timed_out") for s in saved_to["samples"]))

        runner_mut, _ = self.make_fake_runner(mutate_binary_on_call=1)
        out_mut = self.agent_logs / "out-mut.json"
        with self.assertRaises(RuntimeError):
            DRIVER.run_experiment(
                baseline=self.baseline,
                candidate=self.candidate,
                fixture_dir=self.agent_logs / "fx-mut",
                output_path=out_mut,
                rounds=2,
                project_dir=self.root,
                run=runner_mut,
            )
        saved_mut = json.loads(out_mut.read_text(encoding="utf-8"))
        self.assertFalse(saved_mut["complete"])
        self.assertNotEqual(
            saved_mut["binaries"]["candidate"]["sha256"],
            saved_mut["post_binaries"]["candidate"]["sha256"],
        )

    def test_rejects_sensitive_token_leak_in_diagnostics(self) -> None:
        runner, _ = self.make_fake_runner(leak_in_diag=True)
        out = self.agent_logs / "out-leak.json"
        with self.assertRaises(RuntimeError):
            DRIVER.run_experiment(
                baseline=self.baseline,
                candidate=self.candidate,
                fixture_dir=self.agent_logs / "fx-leak",
                output_path=out,
                rounds=2,
                project_dir=self.root,
                run=runner,
            )
        saved = json.loads(out.read_text(encoding="utf-8"))
        self.assertFalse(saved["complete"])
        self.assertIn("leaked sensitive token", saved["failure"])


if __name__ == "__main__":
    unittest.main()
