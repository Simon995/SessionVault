"""Interleave two production-bridge binaries on identical synthetic Windows files.

Run from the repository on Windows with uv run python. Each binary must implement
examples/measure_wsl_read_whole.rs. Files stay under Git's agent-logs directory
for inspection (locate it with git rev-parse --git-path agent-logs, including in
linked worktrees). This script never reads session data or writes into a WSL filesystem. The WSL
mount path is supplied explicitly, because mount mappings are machine-specific.
New processes reset application state, not the operating-system page cache.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import statistics
import subprocess
from pathlib import Path


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline", required=True, type=Path)
    parser.add_argument("--candidate", required=True, type=Path)
    parser.add_argument("--fixture-dir", required=True, type=Path)
    parser.add_argument("--wsl-fixture-dir", required=True)
    parser.add_argument("--distro", default="-")
    parser.add_argument("--sizes", required=True, nargs="+", type=int)
    parser.add_argument("--rounds", type=int, default=5)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    if args.rounds < 2 or any(size < 0 for size in args.sizes):
        parser.error("need at least two rounds and nonnegative sizes")
    if len(set(args.sizes)) != len(args.sizes):
        parser.error("sizes must be unique")
    if not args.wsl_fixture_dir.startswith("/"):
        parser.error("WSL fixture directory must be absolute")
    project = Path(__file__).resolve().parent.parent
    git_logs = subprocess.run(
        ["git", "rev-parse", "--path-format=absolute", "--git-path", "agent-logs"],
        cwd=project,
        check=True,
        capture_output=True,
        text=True,
        encoding="utf-8",
    )
    log_root = Path(git_logs.stdout.strip()).resolve()
    fixture = args.fixture_dir.resolve()
    if not fixture.is_relative_to(log_root) or fixture == log_root:
        parser.error(
            "fixture directory must be below this checkout's git agent-logs directory"
        )
    fixture.mkdir(parents=True, exist_ok=False)
    binaries = {
        "baseline": args.baseline.resolve(),
        "candidate": args.candidate.resolve(),
    }
    block = bytes(range(256)) * 4096
    samples = []
    summaries = []
    for size in args.sizes:
        name = f"binary ' $; 中文-{size}.dat"
        local = fixture / name
        digest = hashlib.sha256()
        with local.open("xb") as stream:
            remaining = size
            while remaining:
                chunk = block[: min(remaining, len(block))]
                stream.write(chunk)
                digest.update(chunk)
                remaining -= len(chunk)
        expected_hash = digest.hexdigest()
        expected_mtime = int(local.stat().st_mtime)
        wsl_path = args.wsl_fixture_dir.rstrip("/") + "/" + name
        group = {arm: [] for arm in binaries}
        for iteration in range(args.rounds):
            order = (
                ("baseline", "candidate")
                if iteration % 2 == 0
                else ("candidate", "baseline")
            )
            for arm in order:
                result = subprocess.run(
                    [str(binaries[arm]), args.distro, wsl_path],
                    capture_output=True,
                    text=True,
                    encoding="utf-8",
                    timeout=90,
                    check=False,
                )
                if result.returncode != 0:
                    raise RuntimeError(f"{arm} rc={result.returncode}: {result.stderr}")
                record = json.loads(result.stdout)
                if (record["bytes"], record["sha256"], record["mtime"]) != (
                    size,
                    expected_hash,
                    expected_mtime,
                ):
                    raise RuntimeError(f"{arm}: bytes/hash/mtime mismatch: {record}")
                record.update(
                    arm=arm,
                    iteration=iteration,
                    size=size,
                    returncode=result.returncode,
                )
                samples.append(record)
                group[arm].append(record)
        medians = {
            arm: statistics.median(r["elapsed_ms"] for r in rows)
            for arm, rows in group.items()
        }
        summary = {
            "size": size,
            "median_ms": medians,
            "spawn_attempts": {
                arm: sorted({r["spawn_attempts"] for r in rows})
                for arm, rows in group.items()
            },
            "median_reduction_percent": 100
            * (1 - medians["candidate"] / medians["baseline"]),
        }
        summaries.append(summary)
        print(json.dumps(summary), flush=True)
    args.output.write_text(
        json.dumps(
            {
                "scope": "synthetic files on Windows-mounted filesystem; fresh process per sample; OS caches uncontrolled; no production refresh measured",
                "rounds_per_arm": args.rounds,
                "summaries": summaries,
                "samples": samples,
            },
            indent=2,
        )
        + "\n",
        encoding="utf-8",
    )


if __name__ == "__main__":
    main()
