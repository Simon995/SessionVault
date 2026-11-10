"""Compare metadata SQL on a synthetic store schema; never opens a real store."""

from __future__ import annotations

import argparse
import json
import os
import re
import sqlite3
import statistics
import tempfile
import time
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
LEGACY_SQL = (
    "SELECT COUNT(*), COALESCE(MAX(offset), 0), MAX(ingested_at) FROM raw_events"
)


def source_constant(source: str, name: str) -> str:
    prefix = rf"const {re.escape(name)}:\s*&str\s*=\s*"
    raw = re.search(prefix + r'r#"(.*?)"#;', source, re.DOTALL)
    if raw:
        return raw.group(1)
    normal = re.search(prefix + r'("(?:\\.|[^"\\])*")\s*;', source)
    if normal:
        return json.loads(normal.group(1))
    raise ValueError(f"Rust SQL constant not found: {name}")


def measure(conn: sqlite3.Connection, sql: str, repeats: int) -> dict:
    durations = []
    result = None
    for _ in range(repeats):
        started = time.perf_counter()
        result = conn.execute(sql).fetchone()
        durations.append((time.perf_counter() - started) * 1000)
    return {
        "result": result,
        "median_ms": statistics.median(durations),
        "samples_ms": durations,
        "query_plan": conn.execute("EXPLAIN QUERY PLAN " + sql).fetchall(),
        "opcodes": sorted({row[1] for row in conn.execute("EXPLAIN " + sql)}),
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--rows", type=int, default=4096)
    parser.add_argument("--payload-bytes", type=int, default=16384)
    parser.add_argument("--repeats", type=int, default=3)
    args = parser.parse_args()
    if min(args.rows, args.payload_bytes, args.repeats) <= 0:
        parser.error("row, payload and repeat counts must be positive")
    source = (REPO / "src/store.rs").read_text(encoding="utf-8")
    summary_sql = source_constant(source, "EVENT_SUMMARY_SQL")
    maximum_sql = source_constant(source, "MAX_OFFSET_SQL")
    parent = Path(os.environ.get("CARGO_TARGET_DIR", REPO / "target")).resolve()
    parent.mkdir(parents=True, exist_ok=True)
    run_dir = Path(tempfile.mkdtemp(prefix="store-metadata-", dir=parent))
    db = run_dir / "synthetic.db"
    with sqlite3.connect(db) as conn:
        conn.executescript(source_constant(source, "RAW_EVENTS_DDL"))
        conn.executemany(
            "INSERT INTO raw_events (ingested_at, schema_version, source_type, "
            "source_location, source_path, source_session_id, seq, event_type, "
            "occurred_at_unix_ms, event_json) VALUES (?, 1, 'jsonl', 'local', "
            "'fixture', 's', ?, 'message', ?, ?)",
            ((seq, seq, seq, "x" * args.payload_bytes) for seq in range(args.rows)),
        )
        conn.executescript(source_constant(source, "RAW_EVENTS_INDEX_DDL"))
        conn.commit()
        legacy = measure(conn, LEGACY_SQL, args.repeats)
        summary = measure(conn, summary_sql, args.repeats)
        maximum = measure(conn, maximum_sql, args.repeats)
    assert legacy["result"][:2] == summary["result"]
    assert maximum["result"][0] == summary["result"][1]
    assert "Count" in summary["opcodes"], "COUNT should use SQLite's count opcode"
    report = {
        "kind": "store_metadata_sql_probe",
        "scope": "Synthetic SQL only; not whole-CLI latency or a production-store measurement",
        "sqlite_version": sqlite3.sqlite_version,
        "fixture": str(db),
        "rows": args.rows,
        "payload_bytes": args.payload_bytes,
        "database_bytes": db.stat().st_size,
        "legacy": legacy,
        "summary": summary,
        "maximum": maximum,
        "ok": True,
    }
    (run_dir / "report.json").write_text(
        json.dumps(report, indent=2) + "\n", encoding="utf-8"
    )
    print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
