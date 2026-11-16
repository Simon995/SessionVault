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


DECODED_BYTES = 4194304
SOURCE = "0123456789ab"


def round_log(number: int, cold: bool = False, mode: str = "batch") -> str:
    context = f"source={SOURCE} compressed_call={number}"
    events = (
        f"[DEBUG sv-scan] compressed length: {context} cache=not_reused identity_available=true\n"
        f"[DEBUG sv-scan] compressed read: {context} mode={mode} result=found read_ms=1\n"
        f"[DEBUG sv-scan] compressed decode: {context} compressed_bytes=4194406 "
        f"logical_bytes=Some({DECODED_BYTES}) result=ok decode_ms=1 elapsed_through_decode_ms=2\n"
        if cold
        else f"[DEBUG sv-scan] compressed length: {context} result=cache_hit "
        f"logical_bytes={DECODED_BYTES} elapsed_ms=1\n"
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
                # Batch summaries are independent diagnostics, including the historical fields.
                text = text.replace(
                    "synthetic round end: round=1",
                    "[DEBUG sv-wsl] whole batch: requested=1 fetched=1 absent=0 "
                    "failed=0 unanswered=0 bytes=4194406 elapsed_ms=1 result=ok\n"
                    "synthetic round end: round=1",
                )
                result = driver.validate_round_diagnostics(text, 3, mode, DECODED_BYTES)
                self.assertEqual([row["reads"] for row in result], [1, 0, 0])
                self.assertEqual([row["cache_hits"] for row in result], [0, 1, 1])

    def test_aggregate_counts_do_not_allow_swapped_cold_round(self):
        text = round_log(1) + round_log(2, True) + round_log(3)
        with self.assertRaises(RuntimeError):
            driver.validate_round_diagnostics(text, 3, "batch", DECODED_BYTES)

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
                driver.validate_round_diagnostics(text, 3, "batch", DECODED_BYTES)

    def test_wrong_read_mode_is_rejected(self):
        text = round_log(1, True, "single") + round_log(2) + round_log(3)
        with self.assertRaises(RuntimeError):
            driver.validate_round_diagnostics(text, 3, "batch", DECODED_BYTES)

    def test_extra_missing_and_reordered_events_are_rejected(self):
        lines = round_log(1, True).splitlines(keepends=True)
        warm = round_log(2) + round_log(3)
        extra_events = (
            "compressed length: result=absent\n",
            "compressed length: result=absent_or_non_regular\n",
            lines[1],
            lines[2],
            lines[3],
        )
        bad = [
            "".join(lines[:4]) + event + lines[4] + warm for event in extra_events
        ] + [
            "".join(lines)
            + round_log(2).replace(
                "synthetic round end: round=2", event + "synthetic round end: round=2"
            )
            + round_log(3)
            for event in extra_events
        ]
        bad.extend("".join(lines[:i] + lines[i + 1 :]) + warm for i in (1, 2, 3))
        bad.append("".join([lines[0], lines[1], lines[3], lines[2], lines[4]]) + warm)
        for text in bad:
            with self.subTest(text=text), self.assertRaises(RuntimeError):
                driver.validate_round_diagnostics(text, 3, "batch", DECODED_BYTES)

    def test_wrong_missing_and_reused_call_context_is_rejected(self):
        valid = round_log(1, True) + round_log(2) + round_log(3)
        read_context = f"compressed read: source={SOURCE} compressed_call=1"
        bad = (
            valid.replace(
                read_context, "compressed read: source=abcdef012345 compressed_call=1"
            ),
            valid.replace(
                read_context, f"compressed read: source={SOURCE} compressed_call=99"
            ),
            valid.replace(read_context, "compressed read: compressed_call=1"),
            valid.replace("compressed_call=1", "compressed_call=0"),
            valid.replace("compressed_call=2", "compressed_call=1"),
            valid.replace(
                f"source={SOURCE} compressed_call=2",
                "source=abcdef012345 compressed_call=2",
            ),
            valid.replace(f"source={SOURCE}", "source=invalid"),
            valid.replace(
                "compressed_call=1", "compressed_call=1 compressed_call=1", 1
            ),
        )
        for text in bad:
            with self.subTest(text=text), self.assertRaises(RuntimeError):
                driver.validate_round_diagnostics(text, 3, "batch", DECODED_BYTES)

    def test_wrong_or_missing_logical_lengths_are_rejected(self):
        valid = round_log(1, True) + round_log(2) + round_log(3)
        bad = (
            valid.replace(
                f"logical_bytes=Some({DECODED_BYTES})", "logical_bytes=Some(1)"
            ),
            valid.replace(f"logical_bytes=Some({DECODED_BYTES})", "logical_bytes=None"),
            valid.replace(f"logical_bytes=Some({DECODED_BYTES}) ", ""),
            valid.replace(f"logical_bytes={DECODED_BYTES}", "logical_bytes=1", 1),
            valid.replace(f"logical_bytes={DECODED_BYTES} ", "", 1),
            valid.replace("cache=not_reused", "cache=not_reused result=absent"),
            valid.replace("result=cache_hit", "result=cache_hit cache=not_reused", 1),
        )
        for text in bad:
            with self.subTest(text=text), self.assertRaises(RuntimeError):
                driver.validate_round_diagnostics(text, 3, "batch", DECODED_BYTES)
        with self.assertRaises(RuntimeError):
            driver.validate_round_diagnostics(valid, 3, "batch", DECODED_BYTES + 1)


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

    def test_binary_replacement_rejects_mixed_build_report(self):
        for stage in ("before", "during"):
            with self.subTest(stage=stage):
                fixture = self.log_root / f"cache-binding-{uuid.uuid4().hex}"
                binary = self.log_root / f"cache-binary-{uuid.uuid4().hex}"
                binary.write_bytes(b"original synthetic binary")
                calls = []

                def run(command, *, calls=calls, stage=stage, binary=binary, **kwargs):
                    calls.append(command)
                    if command[0] == "git":
                        return SimpleNamespace(stdout=str(self.log_root))
                    if command[0] == "wsl.exe":
                        if stage == "before":
                            binary.write_bytes(b"replacement synthetic binary")
                        return SimpleNamespace(
                            returncode=0, stdout="/synthetic-fixture\n", stderr=""
                        )
                    binary.write_bytes(b"replacement synthetic binary")
                    return SimpleNamespace(returncode=0, stdout="", stderr="")

                argv = [
                    str(DRIVER),
                    "--binary",
                    str(binary),
                    "--distro",
                    "fixture",
                    "--fixture-dir",
                    str(fixture),
                ]
                with (
                    patch.object(sys, "argv", argv),
                    patch.object(driver.subprocess, "run", side_effect=run),
                    self.assertRaisesRegex(RuntimeError, f"changed {stage}"),
                ):
                    driver.main()
                report = json.loads(
                    (fixture / "report.json").read_text(encoding="utf-8")
                )
                self.assertFalse(report["complete"])
                self.assertEqual(len(report["binary_sha256"]), 64)
                self.assertEqual(len(calls), 2 if stage == "before" else 3)
                if stage == "during":
                    self.assertNotEqual(
                        report["binary_sha256"],
                        report["samples"][0]["binary_sha256_after"],
                    )

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
