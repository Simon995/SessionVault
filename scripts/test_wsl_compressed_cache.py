"""Regression checks for round completeness and retained driver failures.

Run with uv run --with zstandard python scripts/test_wsl_compressed_cache.py.
No WSL command is executed. Failed-run fixtures stay below Git agent-logs.
"""

from __future__ import annotations

import importlib.util
import json
import subprocess
import sys
import unittest
import uuid
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

PROJECT = Path(__file__).resolve().parent.parent
DRIVER = PROJECT / "scripts" / "measure-wsl-compressed-cache.py"
spec = importlib.util.spec_from_file_location("compressed_cache_driver", DRIVER)
assert spec and spec.loader
driver = importlib.util.module_from_spec(spec)
spec.loader.exec_module(driver)


def round_log(number: int, cold: bool = False, mode: str = "batch") -> str:
    events = (
        "compressed length: cache=not_reused\n"
        f"compressed read: mode={mode} result=found read_ms=1\n"
        "compressed decode: result=ok decode_ms=1\n"
        if cold
        else "compressed length: result=cache_hit\n"
    )
    return (
        f"synthetic round begin: round={number}\n"
        + events
        + f"synthetic round end: round={number}\n"
    )


class RoundCompleteness(unittest.TestCase):
    def test_valid_cold_then_warm(self):
        for mode in ("batch", "single"):
            with self.subTest(mode=mode):
                text = round_log(1, True, mode) + round_log(2) + round_log(3)
                result = driver.validate_round_diagnostics(text, 3, mode)
                self.assertEqual([row["reads"] for row in result], [1, 0, 0])

    def test_aggregate_counts_do_not_allow_swapped_cold_round(self):
        text = round_log(1) + round_log(2, True) + round_log(3)
        with self.assertRaises(RuntimeError):
            driver.validate_round_diagnostics(text, 3, "batch")

    def test_incomplete_duplicate_and_outside_events_are_rejected(self):
        valid = round_log(1, True) + round_log(2) + round_log(3)
        bad = (
            valid.replace("synthetic round end: round=3", ""),
            valid + round_log(3),
            valid.replace(
                "synthetic round begin: round=2", "synthetic round begin: round=3"
            ),
            "compressed read: mode=batch result=found\n" + valid,
            valid.replace("result=ok decode_ms=1", "result=error decode_ms=1"),
        )
        for text in bad:
            with self.subTest(text=text), self.assertRaises(RuntimeError):
                driver.validate_round_diagnostics(text, 3, "batch")

    def test_wrong_read_mode_is_rejected(self):
        text = round_log(1, True, "single") + round_log(2) + round_log(3)
        with self.assertRaises(RuntimeError):
            driver.validate_round_diagnostics(text, 3, "batch")


class RetainedFailure(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.log_root = Path(
            subprocess.run(
                [
                    "git",
                    "rev-parse",
                    "--path-format=absolute",
                    "--git-path",
                    "agent-logs",
                ],
                cwd=PROJECT,
                check=True,
                capture_output=True,
                text=True,
                encoding="utf-8",
            ).stdout.strip()
        ).resolve(strict=True)

    def test_timeout_and_launch_failure_preserve_incomplete_report(self):
        cases = (
            (
                "wslpath",
                subprocess.TimeoutExpired(
                    "fixture", 30, b"partial-out", b"partial-err"
                ),
            ),
            (
                "sample",
                subprocess.TimeoutExpired(
                    "fixture", 240, b"partial-out", b"partial-err"
                ),
            ),
            ("sample", OSError("synthetic launch failure")),
        )
        for stage, error in cases:
            with self.subTest(stage=stage, error=type(error).__name__):
                fixture = self.log_root / f"cache-driver-control-{uuid.uuid4().hex}"
                git_answer = SimpleNamespace(stdout=str(self.log_root))
                converted = SimpleNamespace(
                    returncode=0, stdout="/synthetic-fixture\n", stderr=""
                )
                responses = [git_answer]
                if stage == "sample":
                    responses.append(converted)
                responses.append(error)
                argv = [
                    str(DRIVER),
                    "--binary",
                    str(DRIVER),
                    "--distro",
                    "fixture",
                    "--fixture-dir",
                    str(fixture),
                ]
                with (
                    patch.object(sys, "argv", argv),
                    patch.object(
                        driver.subprocess, "run", side_effect=responses
                    ) as run,
                    self.assertRaises(type(error)),
                ):
                    driver.main()
                self.assertEqual(run.call_count, len(responses))
                report = json.loads(
                    (fixture / "report.json").read_text(encoding="utf-8")
                )
                self.assertFalse(report["complete"])
                record = (
                    report["wslpath"] if stage == "wslpath" else report["samples"][0]
                )
                self.assertEqual(
                    record["status"],
                    "timeout"
                    if isinstance(error, subprocess.TimeoutExpired)
                    else "launch_error",
                )
                self.assertIsNone(record["returncode"])
                prefix = (
                    "wslpath" if stage == "wslpath" else str(record["decoded_bytes"])
                )
                for channel in ("stdout", "stderr"):
                    expected = (
                        {"stdout": "partial-out", "stderr": "partial-err"}[channel]
                        if isinstance(error, subprocess.TimeoutExpired)
                        else ""
                    )
                    self.assertEqual(
                        (fixture / f"{prefix}.{channel}.log").read_text(
                            encoding="utf-8"
                        ),
                        expected,
                    )


if __name__ == "__main__":
    unittest.main()
