"""Interleave baseline and candidate WSL identity binaries on synthetic fixtures.

Run from the repository on Windows with `uv run python scripts/measure-wsl-identity.py`.
Both binaries must implement `examples/measure_wsl_identity.rs`. Fixtures and outputs
stay strictly below this checkout's Git `agent-logs` directory. No session data, real
store, keychain, or WSL ext4 user directory is read or written.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import statistics
import subprocess
from collections.abc import Callable
from pathlib import Path
from typing import Any

Runner = Callable[..., subprocess.CompletedProcess[Any]]


def file_identity(path: Path) -> dict[str, int | str]:
    data = path.read_bytes()
    return {
        "path": str(path),
        "bytes": len(data),
        "sha256": hashlib.sha256(data).hexdigest(),
    }


def resolve_git_agent_logs(project_dir: Path, run: Runner = subprocess.run) -> Path:
    git_logs = run(
        ["git", "rev-parse", "--path-format=absolute", "--git-path", "agent-logs"],
        cwd=project_dir,
        check=True,
        capture_output=True,
        text=True,
        encoding="utf-8",
    )
    return Path(git_logs.stdout.strip()).resolve()


def ensure_strict_descendant(root: Path, target: Path, label: str) -> Path:
    resolved = target.resolve()
    if not resolved.is_relative_to(root) or resolved == root:
        raise ValueError(f"{label} must be strictly below {root}")
    return resolved


def resolve_distro_and_mount(
    distro_arg: str, fixture_dir: Path, run: Runner = subprocess.run
) -> tuple[str, str]:
    distro = distro_arg
    if distro == "-":
        listed = run(
            ["wsl.exe", "-l", "-q"],
            capture_output=True,
            text=False,
            timeout=20,
            check=False,
        )
        if listed.returncode != 0:
            raise RuntimeError(f"wsl -l -q failed rc={listed.returncode}")
        out_bytes = (
            listed.stdout
            if isinstance(listed.stdout, bytes)
            else str(listed.stdout).encode("utf-8")
        )
        raw = (
            out_bytes.decode("utf-16-le", errors="ignore")
            if b"\x00" in out_bytes
            else out_bytes.decode("utf-8", errors="ignore")
        )
        names = [
            line.strip()
            for line in raw.splitlines()
            if line.strip() and not line.strip().lower().startswith("docker-desktop")
        ]
        if len(names) != 1:
            raise RuntimeError(f"expected 1 user distro for '-', got {names!r}")
        distro = names[0]
    converted = run(
        ["wsl.exe", "-d", distro, "--exec", "wslpath", "-a", "-u", str(fixture_dir)],
        capture_output=True,
        text=True,
        encoding="utf-8",
        timeout=30,
        check=False,
    )
    if converted.returncode != 0:
        raise RuntimeError(
            f"wslpath failed rc={converted.returncode}: {converted.stderr.strip()}"
        )
    mounted = converted.stdout.strip().rstrip("/")
    if not mounted.startswith("/") or not mounted:
        raise RuntimeError(f"unexpected wslpath output: {converted.stdout!r}")
    return distro, mounted


def write_git_config(config_path: Path, remote_url: str | None) -> None:
    config_path.parent.mkdir(parents=True, exist_ok=True)
    body = "[core]\n\trepositoryformatversion = 0\n\tfilemode = true\n"
    if remote_url is not None:
        body += f'[remote "origin"]\n\turl = {remote_url}\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n'
    config_path.write_text(body, encoding="utf-8", newline="\n")


def build_synthetic_fixtures(fixture_dir: Path, distro: str, mounted: str) -> dict:
    special_name = "repo ' $` 中文"
    wt_host = fixture_dir / "wt-main"
    write_git_config(
        wt_host / ".git" / "config", "ssh://git@github.com/example/worktree-host"
    )
    for wt_name in ("wt-abs", "wt-rel"):
        wt_gitdir = wt_host / ".git" / "worktrees" / wt_name
        wt_gitdir.mkdir(parents=True, exist_ok=True)
        (wt_gitdir / "commondir").write_text("../..\n", encoding="utf-8", newline="\n")

    write_git_config(
        fixture_dir / "repo-alpha" / ".git" / "config",
        "git@github.com:example/alpha.git",
    )
    write_git_config(
        fixture_dir / "repo-beta" / ".git" / "config",
        "https://github.com/Example/Beta-Repo.git/",
    )
    write_git_config(
        fixture_dir / special_name / ".git" / "config",
        "https://github.com/Example/Special-Repo.git/",
    )
    write_git_config(fixture_dir / "repo-no-origin" / ".git" / "config", None)

    wt_abs_dir = fixture_dir / "wt-abs"
    wt_abs_dir.mkdir(parents=True, exist_ok=True)
    (wt_abs_dir / ".git").write_text(
        f"gitdir: {mounted}/wt-main/.git/worktrees/wt-abs\n",
        encoding="utf-8",
        newline="\n",
    )

    wt_rel_dir = fixture_dir / "wt-rel"
    wt_rel_dir.mkdir(parents=True, exist_ok=True)
    (wt_rel_dir / ".git").write_text(
        "gitdir: ../wt-main/.git/worktrees/wt-rel\n",
        encoding="utf-8",
        newline="\n",
    )

    write_git_config(
        fixture_dir / "sub-host" / ".git" / "modules" / "sub-child" / "config",
        "https://github.com/example/sub-child.git",
    )
    sub_dir = fixture_dir / "sub-child"
    sub_dir.mkdir(parents=True, exist_ok=True)
    (sub_dir / ".git").write_text(
        "gitdir: ../sub-host/.git/modules/sub-child\n",
        encoding="utf-8",
        newline="\n",
    )

    (fixture_dir / "plain-folder").mkdir(parents=True, exist_ok=True)
    # `gone-checkout` is intentionally not created.

    def canon(rel: str) -> str:
        return f"wsl:{distro}:{mounted}/{rel}"

    root_specs = [
        {
            "rel": "repo-alpha",
            "root": canon("repo-alpha"),
            "single": {
                "id": "git:github.com/example/alpha",
                "repo_root": canon("repo-alpha"),
                "checkout_missing": False,
            },
            "sweep_row": {
                "canonical_id": "git:github.com/example/alpha",
                "verdict": "resolved",
            },
        },
        {
            "rel": "repo-beta",
            "root": canon("repo-beta"),
            "single": {
                "id": "git:github.com/example/beta-repo",
                "repo_root": canon("repo-beta"),
                "checkout_missing": False,
            },
            "sweep_row": {
                "canonical_id": "git:github.com/example/beta-repo",
                "verdict": "resolved",
            },
        },
        {
            "rel": "repo-no-origin",
            "root": canon("repo-no-origin"),
            "single": {
                "id": f"path:{canon('repo-no-origin')}",
                "repo_root": canon("repo-no-origin"),
                "checkout_missing": False,
            },
            "sweep_row": {
                "canonical_id": None,
                "verdict": "no_identity",
            },
        },
        {
            "rel": "wt-abs",
            "root": canon("wt-abs"),
            "single": {
                "id": "git:github.com/example/worktree-host",
                "repo_root": canon("wt-abs"),
                "checkout_missing": False,
            },
            "sweep_row": {
                "canonical_id": "git:github.com/example/worktree-host",
                "verdict": "resolved",
            },
        },
        {
            "rel": "wt-rel",
            "root": canon("wt-rel"),
            "single": {
                "id": "git:github.com/example/worktree-host",
                "repo_root": canon("wt-rel"),
                "checkout_missing": False,
            },
            "sweep_row": {
                "canonical_id": "git:github.com/example/worktree-host",
                "verdict": "resolved",
            },
        },
        {
            "rel": "sub-child",
            "root": canon("sub-child"),
            "single": {
                "id": "git:github.com/example/sub-child",
                "repo_root": canon("sub-child"),
                "checkout_missing": False,
            },
            "sweep_row": {
                "canonical_id": "git:github.com/example/sub-child",
                "verdict": "resolved",
            },
        },
        {
            "rel": "plain-folder",
            "root": canon("plain-folder"),
            "single": {
                "id": f"path:{canon('plain-folder')}",
                "repo_root": None,
                "checkout_missing": False,
            },
            "sweep_row": {
                "canonical_id": None,
                "verdict": "no_identity",
            },
        },
        {
            "rel": "gone-checkout",
            "root": canon("gone-checkout"),
            "single": {
                "id": f"path:{canon('gone-checkout')}",
                "repo_root": None,
                "checkout_missing": True,
            },
            "sweep_row": {
                "canonical_id": None,
                "verdict": "checkout_missing",
            },
        },
    ]
    by_rel = {item["rel"]: item for item in root_specs}

    cases = [
        ("single_standard_repo", "single", [by_rel["repo-beta"]]),
        ("single_linked_worktree", "single", [by_rel["wt-abs"]]),
        ("single_submodule", "single", [by_rel["sub-child"]]),
        ("single_plain_dir", "single", [by_rel["plain-folder"]]),
        ("single_missing_checkout", "single", [by_rel["gone-checkout"]]),
        ("sweep_mixed_8_roots", "sweep", root_specs),
    ]

    special_spec = {
        "rel": special_name,
        "root": canon(special_name),
        "single": {
            "id": "git:github.com/example/special-repo",
            "repo_root": canon(special_name),
            "checkout_missing": False,
        },
    }
    special_roots_file = fixture_dir / "roots-special-chars.txt"
    special_roots_file.write_text(
        special_spec["root"] + "\n", encoding="utf-8", newline="\n"
    )

    prepared = []
    for name, mode, specs in cases:
        roots_file = fixture_dir / f"roots-{name}.txt"
        roots_file.write_text(
            "\n".join(str(item["root"]) for item in specs) + "\n",
            encoding="utf-8",
            newline="\n",
        )
        prepared.append(
            {
                "name": name,
                "mode": mode,
                "roots_file": roots_file,
                "specs": specs,
            }
        )
    return {
        "special_name": special_name,
        "special_case": {
            "name": "candidate_special_chars_smoke",
            "mode": "single",
            "roots_file": special_roots_file,
            "specs": [special_spec],
        },
        "cases": prepared,
        "sensitive_tokens": [
            mounted,
            "github.com/example",
            special_name,
        ],
    }


def verify_case_payload(record: dict, case: dict) -> None:
    mode = case["mode"]
    specs = case["specs"]
    if record.get("mode") != mode:
        raise RuntimeError(
            f"{case['name']}: expected mode={mode}, got {record.get('mode')!r}"
        )
    elapsed = record.get("elapsed_ms")
    spawns = record.get("spawn_attempts")
    if not isinstance(elapsed, (int, float)) or elapsed < 0:
        raise RuntimeError(f"{case['name']}: invalid elapsed_ms={elapsed!r}")
    if not isinstance(spawns, int) or spawns < 0:
        raise RuntimeError(f"{case['name']}: invalid spawn_attempts={spawns!r}")

    if mode == "single":
        spec = specs[0]
        expected = {
            "root": spec["root"],
            **spec["single"],
        }
        if record.get("result") != expected:
            raise RuntimeError(
                f"{case['name']}: single result mismatch: got {record.get('result')!r}, expected {expected!r}"
            )
    else:
        expected_sweep = {
            "registered": len(specs),
            "recorded": sum(
                1 for s in specs if s["sweep_row"]["verdict"] == "resolved"
            ),
            "already_probed": 0,
            "no_remote": sum(
                1 for s in specs if s["sweep_row"]["verdict"] == "no_identity"
            ),
            "checkout_missing": sum(
                1 for s in specs if s["sweep_row"]["verdict"] == "checkout_missing"
            ),
            "unresolved": 0,
            "skipped_out_of_budget": 0,
            "unreadable": None,
        }
        if record.get("sweep") != expected_sweep:
            raise RuntimeError(
                f"{case['name']}: sweep summary mismatch: got {record.get('sweep')!r}, expected {expected_sweep!r}"
            )
        rows = record.get("report")
        if not isinstance(rows, list) or len(rows) != len(specs):
            raise RuntimeError(f"{case['name']}: unexpected report row count")
        got_by_root = {
            row["root_path"]: {
                "canonical_id": row["canonical_id"],
                "verdict": row["verdict"],
            }
            for row in rows
        }
        expected_by_root = {s["root"]: s["sweep_row"] for s in specs}
        if got_by_root != expected_by_root:
            raise RuntimeError(
                f"{case['name']}: report mismatch: got {got_by_root!r}, expected {expected_by_root!r}"
            )


def verify_diagnostic_stderr(
    stderr: str, expected_roots: int, sensitive_tokens: list[str]
) -> dict:
    for token in sensitive_tokens:
        if token and token in stderr:
            raise RuntimeError(f"diagnostic stderr leaked sensitive token {token!r}")
    batch_matches = re.findall(
        r"\bidentity batch: roots=(\d+) snapshots=(\d+) elapsed_ms=([0-9.]+) result=(ok|err)\b",
        stderr,
    )
    if len(batch_matches) != 1:
        raise RuntimeError(
            f"expected exactly 1 identity batch log, got {len(batch_matches)}"
        )
    roots_s, snapshots_s, batch_ms_s, batch_res = batch_matches[0]
    if (
        int(roots_s) != expected_roots
        or int(snapshots_s) < expected_roots
        or batch_res != "ok"
    ):
        raise RuntimeError(f"unexpected identity batch fields: {batch_matches[0]!r}")
    completed = re.findall(
        r"\bidentity completed: root_sha256=([0-9a-f]{12}) identity_call=(\d+) result=(\w+)",
        stderr,
    )
    if len(completed) != expected_roots:
        raise RuntimeError(
            f"expected {expected_roots} identity completed lines, got {len(completed)}"
        )
    if len({call_id for _, call_id, _ in completed}) != expected_roots:
        raise RuntimeError("duplicate identity_call in diagnostic smoke")
    return {
        "batch_roots": int(roots_s),
        "batch_snapshots": int(snapshots_s),
        "batch_elapsed_ms": float(batch_ms_s),
        "completed_roots": len(completed),
    }


def clean_child_env(extra: dict[str, str] | None = None) -> dict[str, str]:
    env = dict(os.environ)
    for key in ("SVAULT_LOG", "RUST_LOG", "SVAULT_SESSION_BINDINGS"):
        env.pop(key, None)
    if extra:
        env.update(extra)
    return env


def run_experiment(
    *,
    baseline: Path,
    candidate: Path,
    fixture_dir: Path,
    output_path: Path,
    distro_arg: str = "-",
    rounds: int = 6,
    timeout_secs: float = 90.0,
    project_dir: Path | None = None,
    run: Runner = subprocess.run,
) -> dict:
    if rounds < 2:
        raise ValueError("rounds must be at least 2")
    root_project = (project_dir or Path(__file__).resolve().parent.parent).resolve()
    log_root = resolve_git_agent_logs(root_project, run=run)
    guarded_fixture = ensure_strict_descendant(log_root, fixture_dir, "fixture_dir")
    guarded_output = ensure_strict_descendant(log_root, output_path, "output")
    if guarded_fixture.exists():
        raise ValueError(f"fixture_dir must not already exist: {guarded_fixture}")
    if guarded_output.exists():
        raise ValueError(f"output must not already exist: {guarded_output}")

    baseline_resolved = baseline.resolve()
    candidate_resolved = candidate.resolve()
    if baseline_resolved == candidate_resolved:
        raise ValueError("baseline and candidate must be distinct binaries")
    if not baseline_resolved.is_file() or not candidate_resolved.is_file():
        raise ValueError("baseline and candidate binaries must exist")

    initial_binaries = {
        "baseline": file_identity(baseline_resolved),
        "candidate": file_identity(candidate_resolved),
    }
    binaries = {
        "baseline": baseline_resolved,
        "candidate": candidate_resolved,
    }

    guarded_fixture.mkdir(parents=True, exist_ok=False)
    guarded_output.parent.mkdir(parents=True, exist_ok=True)

    report: dict = {
        "complete": False,
        "scope": (
            "synthetic git fixtures under Git agent-logs on Windows-mounted filesystem; "
            "fresh process per sample; interleaved baseline/candidate order; OS page cache "
            "not cleared; in-memory store for sweep; not a full application refresh percentage"
        ),
        "rounds_per_arm": rounds,
        "binaries": initial_binaries,
        "distro": None,
        "summaries": [],
        "samples": [],
        "diagnostic_smoke": None,
        "failure": None,
    }

    def check_binaries_unchanged(stage: str) -> None:
        for arm, path in binaries.items():
            current = file_identity(path)
            if current["sha256"] != initial_binaries[arm]["sha256"]:
                report["post_binaries"] = {
                    k: file_identity(v) if v.exists() else None
                    for k, v in binaries.items()
                }
                raise RuntimeError(f"{arm} binary changed {stage}")

    try:
        distro, mounted = resolve_distro_and_mount(distro_arg, guarded_fixture, run=run)
        report["distro"] = distro
        built = build_synthetic_fixtures(guarded_fixture, distro, mounted)

        for case in built["cases"]:
            grouped: dict[str, list[dict]] = {"baseline": [], "candidate": []}
            for iteration in range(rounds):
                order = (
                    ("baseline", "candidate")
                    if iteration % 2 == 0
                    else ("candidate", "baseline")
                )
                for arm in order:
                    check_binaries_unchanged(
                        f"before {case['name']} round {iteration} {arm}"
                    )
                    cmd = [
                        str(binaries[arm]),
                        "--distro",
                        distro,
                        "--fixture-dir",
                        str(guarded_fixture),
                        "--roots-file",
                        str(case["roots_file"]),
                        "--mode",
                        case["mode"],
                    ]
                    try:
                        proc = run(
                            cmd,
                            capture_output=True,
                            text=True,
                            encoding="utf-8",
                            env=clean_child_env(),
                            timeout=timeout_secs,
                            check=False,
                        )
                    except subprocess.TimeoutExpired as exc:
                        report["samples"].append(
                            {
                                "case": case["name"],
                                "arm": arm,
                                "iteration": iteration,
                                "timed_out": True,
                                "stdout": exc.stdout or "",
                                "stderr": exc.stderr or "",
                            }
                        )
                        raise RuntimeError(
                            f"{case['name']} {arm} round {iteration} timed out after {timeout_secs}s"
                        ) from exc
                    check_binaries_unchanged(
                        f"after {case['name']} round {iteration} {arm}"
                    )
                    if proc.returncode != 0:
                        report["samples"].append(
                            {
                                "case": case["name"],
                                "arm": arm,
                                "iteration": iteration,
                                "returncode": proc.returncode,
                                "stdout": proc.stdout,
                                "stderr": proc.stderr,
                            }
                        )
                        raise RuntimeError(
                            f"{case['name']} {arm} rc={proc.returncode}: {proc.stderr.strip()}"
                        )
                    record = json.loads(proc.stdout)
                    verify_case_payload(record, case)
                    sample = {
                        "case": case["name"],
                        "mode": case["mode"],
                        "arm": arm,
                        "iteration": iteration,
                        "returncode": proc.returncode,
                        "elapsed_ms": record["elapsed_ms"],
                        "spawn_attempts": record["spawn_attempts"],
                    }
                    report["samples"].append(sample)
                    grouped[arm].append(sample)

            medians = {
                arm: statistics.median(row["elapsed_ms"] for row in rows)
                for arm, rows in grouped.items()
            }
            spawns_by_arm = {
                arm: sorted({row["spawn_attempts"] for row in rows})
                for arm, rows in grouped.items()
            }
            if spawns_by_arm["candidate"] != [1]:
                raise RuntimeError(
                    f"{case['name']}: expected candidate spawn_attempts=[1], got {spawns_by_arm['candidate']!r}"
                )
            summary = {
                "case": case["name"],
                "mode": case["mode"],
                "roots": len(case["specs"]),
                "median_ms": medians,
                "min_ms": {
                    arm: min(row["elapsed_ms"] for row in rows)
                    for arm, rows in grouped.items()
                },
                "max_ms": {
                    arm: max(row["elapsed_ms"] for row in rows)
                    for arm, rows in grouped.items()
                },
                "spawn_attempts": spawns_by_arm,
                "median_reduction_percent": 100.0
                * (1.0 - medians["candidate"] / medians["baseline"]),
            }
            report["summaries"].append(summary)

        sweep_case = built["cases"][-1]
        check_binaries_unchanged("before diagnostic_smoke")
        diag_proc = run(
            [
                str(binaries["candidate"]),
                "--distro",
                distro,
                "--fixture-dir",
                str(guarded_fixture),
                "--roots-file",
                str(sweep_case["roots_file"]),
                "--mode",
                "sweep",
            ],
            capture_output=True,
            text=True,
            encoding="utf-8",
            env=clean_child_env({"SVAULT_LOG": "sv-wsl=debug"}),
            timeout=timeout_secs,
            check=False,
        )
        check_binaries_unchanged("after diagnostic_smoke")
        if diag_proc.returncode != 0:
            raise RuntimeError(
                f"diagnostic_smoke failed rc={diag_proc.returncode}: {diag_proc.stderr.strip()}"
            )
        verify_case_payload(json.loads(diag_proc.stdout), sweep_case)
        report["diagnostic_smoke"] = verify_diagnostic_stderr(
            diag_proc.stderr,
            len(sweep_case["specs"]),
            built["sensitive_tokens"],
        )

        special_case = built["special_case"]
        check_binaries_unchanged("before special_chars_smoke")
        special_proc = run(
            [
                str(binaries["candidate"]),
                "--distro",
                distro,
                "--fixture-dir",
                str(guarded_fixture),
                "--roots-file",
                str(special_case["roots_file"]),
                "--mode",
                "single",
            ],
            capture_output=True,
            text=True,
            encoding="utf-8",
            env=clean_child_env(),
            timeout=timeout_secs,
            check=False,
        )
        check_binaries_unchanged("after special_chars_smoke")
        if special_proc.returncode != 0:
            raise RuntimeError(
                f"special_chars_smoke failed rc={special_proc.returncode}: {special_proc.stderr.strip()}"
            )
        special_record = json.loads(special_proc.stdout)
        verify_case_payload(special_record, special_case)
        if special_record["spawn_attempts"] != 1:
            raise RuntimeError(
                f"special_chars_smoke expected 1 spawn, got {special_record['spawn_attempts']}"
            )
        report["special_chars_smoke"] = {
            "spawn_attempts": special_record["spawn_attempts"],
            "elapsed_ms": special_record["elapsed_ms"],
            "verified_id": special_record["result"]["id"],
        }
        report["post_binaries"] = {k: file_identity(v) for k, v in binaries.items()}
        report["complete"] = True
    except Exception as exc:
        report["failure"] = str(exc)
        if "post_binaries" not in report:
            report["post_binaries"] = {
                k: file_identity(v) if v.exists() else None for k, v in binaries.items()
            }
        guarded_output.write_text(
            json.dumps(report, indent=2, ensure_ascii=False) + "\n",
            encoding="utf-8",
        )
        raise

    guarded_output.write_text(
        json.dumps(report, indent=2, ensure_ascii=False) + "\n",
        encoding="utf-8",
    )
    return report


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline", required=True, type=Path)
    parser.add_argument("--candidate", required=True, type=Path)
    parser.add_argument("--fixture-dir", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--distro", default="-")
    parser.add_argument("--rounds", type=int, default=6)
    parser.add_argument("--timeout-secs", type=float, default=90.0)
    args = parser.parse_args()
    report = run_experiment(
        baseline=args.baseline,
        candidate=args.candidate,
        fixture_dir=args.fixture_dir,
        output_path=args.output,
        distro_arg=args.distro,
        rounds=args.rounds,
        timeout_secs=args.timeout_secs,
    )
    for row in report["summaries"]:
        print(json.dumps(row, ensure_ascii=False), flush=True)


if __name__ == "__main__":
    main()
