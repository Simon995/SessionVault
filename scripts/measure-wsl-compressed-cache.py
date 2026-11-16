"""Exercise EOF cold/warm length-cache paths using only generated Windows files.

Run with uv run --with zstandard python. Build measure_wsl_compressed_cache first.
The fixture must be a new directory below this checkout's Git agent-logs directory.
Files and a report are retained. No OS cache is cleared, no session data is read,
and no WSL filesystem is written. Results describe synthetic inputs, not production.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import random
import re
import subprocess
from pathlib import Path

import zstandard

MIB = 1024 * 1024


def validate_round_diagnostics(
    stderr: str, count: int, expected_mode: str, expected_bytes: int
) -> list[dict]:
    blocks = []
    current = None
    for line in stderr.splitlines():
        if line.startswith("synthetic round begin:"):
            if (
                current is not None
                or line != f"synthetic round begin: round={len(blocks) + 1}"
            ):
                raise RuntimeError("invalid round begin marker")
            current = []
        elif line.startswith("synthetic round end:"):
            if (
                current is None
                or line != f"synthetic round end: round={len(blocks) + 1}"
            ):
                raise RuntimeError("invalid round end marker")
            blocks.append(current)
            current = None
        else:
            event = re.search(r"\bcompressed (length|read|decode):\s*(.*)$", line)
            if event is None:
                continue
            if current is None:
                raise RuntimeError("compressed diagnostic outside a round")
            fields = {}
            for item in event[2].split():
                key, separator, value = item.partition("=")
                if not separator or not value or key in fields:
                    raise RuntimeError("invalid compressed diagnostic fields")
                fields[key] = value
            current.append((event[1], fields))
    if current is not None or len(blocks) != count:
        raise RuntimeError("incomplete round diagnostics")
    diagnostics = []
    source = None
    calls = set()
    for index, block in enumerate(blocks):
        expected_events = ["length", "read", "decode"] if index == 0 else ["length"]
        if [event for event, _ in block] != expected_events:
            raise RuntimeError(
                f"round {index + 1}: unexpected compressed event sequence"
            )
        length = block[0][1]
        round_source = length.get("source", "")
        call = length.get("compressed_call", "")
        if (
            re.fullmatch(r"[0-9a-f]{12}", round_source) is None
            or re.fullmatch(r"[1-9][0-9]*", call) is None
            or (source is not None and round_source != source)
            or call in calls
            or any(
                fields.get("source") != round_source
                or fields.get("compressed_call") != call
                for _, fields in block
            )
        ):
            raise RuntimeError(f"round {index + 1}: mismatched compressed call")
        source = round_source
        calls.add(call)
        if index == 0:
            read, decode = block[1][1], block[2][1]
            valid = (
                length.get("cache") == "not_reused"
                and "result" not in length
                and read.get("mode") == expected_mode
                and read.get("result") == "found"
                and decode.get("result") == "ok"
                and decode.get("logical_bytes") == f"Some({expected_bytes})"
            )
        else:
            valid = (
                length.get("result") == "cache_hit"
                and "cache" not in length
                and length.get("logical_bytes") == str(expected_bytes)
            )
        if not valid:
            raise RuntimeError(
                f"round {index + 1}: expected cold decode then warm hits at {expected_bytes} bytes"
            )
        diagnostics.append(
            {
                "round": index + 1,
                "cache_hits": int(index != 0),
                "cache_misses": int(index == 0),
                "reads": int(index == 0),
                "decodes": int(index == 0),
            }
        )
    return diagnostics


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--distro", required=True)
    parser.add_argument("--fixture-dir", required=True, type=Path)
    args = parser.parse_args()
    project = Path(__file__).resolve().parent.parent
    log_root = Path(
        subprocess.run(
            ["git", "rev-parse", "--path-format=absolute", "--git-path", "agent-logs"],
            cwd=project,
            check=True,
            capture_output=True,
            text=True,
            encoding="utf-8",
        ).stdout.strip()
    ).resolve(strict=True)
    binary = args.binary.resolve(strict=True)
    fixture = args.fixture_dir.resolve()
    if fixture == log_root or not fixture.is_relative_to(log_root):
        parser.error("fixture must be below this checkout's Git agent-logs directory")
    if not fixture.parent.is_dir():
        parser.error("fixture parent must already exist")
    # An atomic new directory prevents accidental use of real data or old fixtures.
    fixture.mkdir(exist_ok=False)

    def binary_sha256() -> str:
        with binary.open("rb") as stream:
            return hashlib.file_digest(stream, "sha256").hexdigest()

    fingerprint = binary_sha256()
    samples = []
    report = {
        "complete": False,
        "binary_sha256": fingerprint,
        "zstandard_version": zstandard.__version__,
        "samples": samples,
    }

    def save_report() -> None:
        (fixture / "report.json").write_text(
            json.dumps(report, indent=2) + "\n", encoding="utf-8"
        )

    def run_logged(command, prefix, timeout, record):
        try:
            result = subprocess.run(
                command,
                check=False,
                capture_output=True,
                text=True,
                encoding="utf-8",
                timeout=timeout,
            )
        except (subprocess.TimeoutExpired, OSError) as error:
            record["status"] = (
                "timeout"
                if isinstance(error, subprocess.TimeoutExpired)
                else "launch_error"
            )
            record["returncode"] = None
            for channel in ("stdout", "stderr"):
                output = getattr(error, channel, None) or ""
                if isinstance(output, bytes):
                    output = output.decode("utf-8", errors="replace")
                (fixture / f"{prefix}.{channel}.log").write_text(
                    output, encoding="utf-8"
                )
            save_report()
            raise
        record.update(status="exited", returncode=result.returncode)
        for channel in ("stdout", "stderr"):
            (fixture / f"{prefix}.{channel}.log").write_text(
                getattr(result, channel), encoding="utf-8"
            )
        save_report()
        return result

    save_report()
    # --exec bypasses the default shell, which otherwise consumes Windows backslashes.
    report["wslpath"] = {}
    converted = run_logged(
        ["wsl.exe", "-d", args.distro, "--exec", "wslpath", "-a", "-u", str(fixture)],
        "wslpath",
        30,
        report["wslpath"],
    )
    report["wslpath_returncode"] = converted.returncode
    save_report()
    if converted.returncode:
        raise RuntimeError(
            f"wslpath failed with rc={converted.returncode}; see fixture log"
        )
    mounted = converted.stdout.strip()
    if not mounted.startswith("/"):
        raise RuntimeError("wslpath did not return an absolute mount path")
    rng = random.Random(0)
    for size, expected_mode in ((4 * MIB, "batch"), (12 * MIB, "single")):
        # Incompressible synthetic bytes exercise below/above the batch-read limit.
        # The scanner is positioned at EOF; this tests length lookup, not parsing.
        compressed = zstandard.ZstdCompressor(write_content_size=False).compress(
            rng.randbytes(size)
        )
        name = f"synthetic-{size}.jsonl"
        path = fixture / (name + ".zst")
        with path.open("xb") as stream:
            stream.write(compressed)
        record = {
            "decoded_bytes": size,
            "compressed_bytes": len(compressed),
            "rounds": [],
        }
        samples.append(record)
        save_report()
        if binary_sha256() != fingerprint:
            raise RuntimeError("probe binary changed before sample; report incomplete")
        result = run_logged(
            [
                str(binary),
                args.distro,
                mounted + "/" + name,
                str(size),
                "--fixture-dir",
                str(fixture),
            ],
            str(size),
            240,  # Independent 30s path guard, three 60s rounds, plus exit margin.
            record,
        )
        record["binary_sha256_after"] = binary_sha256()
        save_report()
        if record["binary_sha256_after"] != fingerprint:
            raise RuntimeError("probe binary changed during sample; report incomplete")
        if result.returncode:
            raise RuntimeError(
                f"probe failed with rc={result.returncode}; see fixture logs"
            )
        record["rounds"] = [json.loads(line) for line in result.stdout.splitlines()]
        rounds = record["rounds"]
        if len(rounds) != 3 or any(
            row["round"] != i
            or row["bytes_read"] != 0
            or row["events"] != 0
            or row["logical_bytes"] != size
            or row["safe_offset"] != size
            for i, row in enumerate(rounds, 1)
        ):
            raise RuntimeError("probe output is incomplete or no longer at EOF")
        diagnostics = validate_round_diagnostics(
            result.stderr, len(rounds), expected_mode, size
        )
        record["diagnostics"] = diagnostics
        save_report()
    report["complete"] = True
    save_report()
    print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
