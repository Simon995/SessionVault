# /// script
# requires-python = ">=3.10"
# dependencies = ["zstandard==0.25.0"]
# ///
"""只读核查 DSH session 日志；仅输出计数与白名单字段结构，不输出路径、id 或正文。

    uv run scripts/inspect-dsh.py --root <session-root>
    uv run scripts/inspect-dsh.py --self-test

读取范围由调用方显式指定；不启动 Host、不读配置、不改源文件、不连接总库。
达到预算、文件变化、坏行与不可读均单列，不能把抽样当全量。
complete 仅表示本轮流读取未中断；不证明语义已解码或压缩帧完整。
"""

from __future__ import annotations

import argparse
import contextlib
import io
import json
import os
import re
import stat
import sys
from collections import Counter
from pathlib import Path
from typing import BinaryIO

import zstandard

DEFAULT_FILE_BYTES = 64 * 1024 * 1024
DEFAULT_TOTAL_BYTES = 256 * 1024 * 1024
MAX_LINE_BYTES = 16 * 1024 * 1024
MAX_SCHEMA_DEPTH = 4
CURRENT_FORMAT_VERSION = 4
LOG_NAME = re.compile(r"session(?:[.]v([1-9][0-9]*))?[.]jsonl([.]zstd)?$")
# 不把未知键、枚举值或异常原文输出；未知数据可能含项目名、正文或凭据。
FIELDS = frozenset(
    [
        "type",
        "kind",
        "version",
        "schema_version",
        "id",
        "session_id",
        "sessionId",
        "parent_id",
        "parentId",
        "parent_session_id",
        "parentSessionId",
        "event_id",
        "eventId",
        "timestamp",
        "created_at",
        "updated_at",
        "createdAt",
        "updatedAt",
        "time",
        "sequence",
        "seq",
        "index",
        "event",
        "data",
        "payload",
        "value",
        "events",
        "message",
        "messages",
        "role",
        "content",
        "text",
        "parts",
        "blocks",
        "name",
        "status",
        "usage",
        "input_tokens",
        "output_tokens",
        "cached_tokens",
        "input",
        "output",
        "model",
        "effort",
        "cwd",
        "workspace",
        "metadata",
        "tool_calls",
        "tool_call_id",
        "call_id",
        "arguments",
        "result",
        "tool_name",
        "actor",
        "sender",
        "recipient",
        "source",
        "parent",
        "children",
        "temporary",
        "ephemeral",
        "checkpoint",
        "record",
        "op",
        "operation",
        "path",
        "session",
        "header",
        "patch",
        "object",
        "tokens",
        "reason",
        "request_id",
        "requestId",
        "channel",
        "context",
        "format",
        "format_version",
        "revision",
        "state",
        "changes",
        "key",
        "items",
        "token_usage",
        "isSeeded",
        "delegationDepth",
        "parentSession",
        "origin",
        "agentPreset",
        "seedLength",
        "sourceEventSeqs",
        "surfaceOp",
        "inherited",
        "form",
        "callId",
        "runId",
        "stream",
        "seq0",
        "time0",
    ]
)
ENUM_FIELDS = frozenset({"type", "kind", "role", "op", "operation", "surfaceOp"})
ENUM_VALUES = frozenset(
    [
        "user",
        "assistant",
        "system",
        "tool",
        "message",
        "session",
        "event",
        "meta",
        "metadata",
        "usage",
        "thinking",
        "reasoning",
        "tool_use",
        "tool_result",
        "input_text",
        "output_text",
        "text",
        "token_count",
        "session_meta",
        "checkpoint",
        "create",
        "update",
        "delete",
        "add",
        "remove",
        "replace",
        "set",
        "append",
        "header",
        "snapshot",
        "delta",
        "request",
        "response",
        "developer",
        "user/message",
        "assistant/message",
        "tool/message",
        "system/message",
        "developer/message",
        "session/end-seed",
        "turn/end",
        "subagent/descriptor",
        "subagent/catalog",
        "subagent",
        "agent-message",
        "subagent-settled",
        "tool-call",
        "tool-result",
        "text-chunks",
        "reasoning-chunks",
        "tool-call-chunks",
    ]
)
SAMPLE_TYPES = frozenset(
    {"session", "user/message", "assistant/message", "tool/message"}
)
MAX_SAMPLE_ARRAY_ITEMS = 3


def select_generation(names: list[str]) -> dict:
    """诊断已提交代；不回退未知新版，不按 mtime 选择，不读写会话。"""
    candidates = []
    for name in names:
        match = LOG_NAME.fullmatch(name)
        if match:
            version = int(match.group(1) or 0)
            candidates.append((version, "zstd" if match.group(2) else "plain"))
    if not candidates:
        return {"status": "no_canonical_generation"}
    if len({encoding for _, encoding in candidates}) > 1:
        return {"status": "encoding_conflict"}
    version, encoding = max(candidates)
    return {
        "status": "selected" if version <= CURRENT_FORMAT_VERSION else "future_format",
        "version": version,
        "compression": encoding,
    }


def safe_projection(value: object, key: str = "", depth: int = 0) -> object:
    """只保留白名单结构；不是完整会话，也不声称能被 DSH 原样恢复。"""
    if isinstance(value, str):
        if key in ENUM_FIELDS and value in ENUM_VALUES:
            return value
        if key == "cwd":
            return "/work/repo"
        return "redacted"
    if depth >= MAX_SCHEMA_DEPTH:
        return None
    if isinstance(value, dict):
        return {
            name: safe_projection(item, name, depth + 1)
            for name, item in value.items()
            if name in FIELDS
        }
    if isinstance(value, list):
        return [
            safe_projection(item, key, depth + 1)
            for item in value[:MAX_SAMPLE_ARRAY_ITEMS]
        ]
    return value


def shape_of(
    value: object, path: str, fields: Counter, enums: Counter, depth: int
) -> None:
    if value is None:
        kind = "null"
    elif isinstance(value, bool):
        kind = "bool"
    elif isinstance(value, dict):
        kind = "object"
    elif isinstance(value, list):
        kind = "array"
    elif isinstance(value, str):
        kind = "string"
    elif isinstance(value, (int, float)):
        kind = "number"
    else:
        kind = "other"
    fields[f"{path}:{kind}"] += 1
    if depth >= MAX_SCHEMA_DEPTH:
        return
    if isinstance(value, dict):
        for key, item in value.items():
            safe_key = key if key in FIELDS else "<other-key>"
            child = f"{path}.{safe_key}"
            if key in ENUM_FIELDS and isinstance(item, str):
                safe_value = item if item in ENUM_VALUES else "<other-value>"
                enums[f"{child}={safe_value}"] += 1
            shape_of(item, child, fields, enums, depth + 1)
    elif isinstance(value, list):
        for item in value:
            shape_of(item, f"{path}[]", fields, enums, depth + 1)


def inspect_records(stream: BinaryIO, budget: int) -> dict:
    counts = Counter()
    fields = Counter()
    enums = Counter()
    samples = {}
    complete = True
    try:
        while True:
            remaining = budget - counts["bytes_read"]
            if remaining <= 0:
                # 多读一个字节只用于区别恰好到尾与被预算截断。
                if stream.read(1):
                    counts["byte_budget_hit"] += 1
                    complete = False
                break
            line = stream.readline(min(MAX_LINE_BYTES, remaining) + 1)
            if not line:
                break
            counts["bytes_read"] += len(line)
            if len(line) > remaining:
                counts["byte_budget_hit"] += 1
                complete = False
                break
            if len(line) > MAX_LINE_BYTES:
                counts["line_budget_hit"] += 1
                complete = False
                break
            counts["lines"] += 1
            if not line.endswith(b"\n"):
                counts["unterminated_tail"] += 1
            if not line.strip():
                counts["empty_lines"] += 1
                continue
            try:
                record = json.loads(line.decode("utf-8"))
            except UnicodeDecodeError:
                counts["invalid_utf8"] += 1
                continue
            except (ValueError, RecursionError):
                counts["invalid_json"] += 1
                continue
            if not isinstance(record, dict):
                counts["non_object_records"] += 1
                continue
            counts["objects"] += 1
            record_type = record.get("type")
            if record_type == "session":
                version = record.get("version")
                if type(version) is int and 0 <= version <= CURRENT_FORMAT_VERSION:
                    counts[f"header_version_{version}"] += 1
                else:
                    counts["header_version_unknown"] += 1
            if (
                isinstance(record_type, str)
                and record_type in SAMPLE_TYPES
                and record_type not in samples
            ):
                samples[record_type] = safe_projection(record)
            shape_of(record, "$", fields, enums, 0)
    except (OSError, zstandard.ZstdError):
        counts["read_failed"] += 1
        complete = False
    return {
        "complete": complete,
        "valid_jsonl": not any(
            counts[name]
            for name in (
                "invalid_utf8",
                "invalid_json",
                "non_object_records",
                "unterminated_tail",
            )
        ),
        "counts": dict(sorted(counts.items())),
        "fields": dict(sorted(fields.items())),
        "discriminators": dict(sorted(enums.items())),
        "sample_kind": "sanitized_structure_projection",
        "samples": samples,
    }


def inspect_file(path: Path, budget: int) -> dict:
    try:
        before = path.stat()
        with path.open("rb") as raw:
            compressed = path.suffix in {".zstd", ".zst"}
            reader = (
                zstandard.ZstdDecompressor().stream_reader(raw, read_across_frames=True)
                if compressed
                else contextlib.nullcontext(raw)
            )
            with reader as stream:
                # ZstdDecompressionReader 不提供 readline，BufferedReader 统一逐行读取。
                buffered = io.BufferedReader(stream) if compressed else stream
                with (
                    contextlib.closing(buffered)
                    if compressed
                    else contextlib.nullcontext()
                ):
                    result = inspect_records(buffered, budget)
        after = path.stat()
        result["changed_during_read"] = (before.st_size, before.st_mtime_ns) != (
            after.st_size,
            after.st_mtime_ns,
        )
        result["physical_bytes"] = before.st_size
        result["compression_frame_validation"] = (
            "not_performed" if compressed else "not_applicable"
        )
        return result
    except (OSError, zstandard.ZstdError):
        return {"complete": False, "counts": {"open_or_stat_failed": 1}}


def survey(root: Path, file_budget: int, total_budget: int) -> dict:
    try:
        root_stat = root.lstat()
    except FileNotFoundError:
        return {"root_status": "absent", "complete": False}
    except OSError:
        return {"root_status": "unreadable", "complete": False}
    if stat.S_ISLNK(root_stat.st_mode) or getattr(root_stat, "st_reparse_tag", 0):
        return {"root_status": "link_refused", "complete": False}
    if not stat.S_ISDIR(root_stat.st_mode):
        return {"root_status": "not_directory", "complete": False}
    files = []
    root_encodings = set()
    walk_errors = []
    symlinks_skipped = 0
    for folder, directories, names in os.walk(
        root, onerror=walk_errors.append, followlinks=False
    ):
        parent = Path(folder)
        kept = []
        for name in directories:
            if (parent / name).is_symlink():
                symlinks_skipped += 1
            else:
                kept.append(name)
        directories[:] = kept
        for name in names:
            match = LOG_NAME.fullmatch(name)
            if match:
                path = parent / name
                if path.is_symlink():
                    symlinks_skipped += 1
                else:
                    files.append(path)
                    root_encodings.add(bool(match.group(2)))
    files.sort()
    groups = {}
    for path in files:
        groups.setdefault(path.parent, []).append(path.name)
    selections = Counter()
    for names in groups.values():
        selected = select_generation(names)
        label = selected["status"]
        if label == "selected":
            label = f"v{selected['version']}:{selected['compression']}"
        selections[label] += 1
    result = {
        "root_status": "found",
        "complete": not walk_errors and symlinks_skipped == 0,
        "walk_errors": len(walk_errors),
        "symlinks_skipped": symlinks_skipped,
        "files_found": len(files),
        "file_byte_budget": file_budget,
        "total_byte_budget": total_budget,
        "formats": dict(sorted(Counter(path.name for path in files).items())),
        "generation_selection": dict(sorted(selections.items())),
        "root_encoding_consistent": len(root_encodings) <= 1,
        "session_directories": len({path.parent for path in files}),
        "coexisting_log_directories": sum(
            count > 1 for count in Counter(path.parent for path in files).values()
        ),
        "totals": {},
        "files": [],
    }
    consumed = 0
    for index, path in enumerate(files):
        available = min(file_budget, total_budget - consumed)
        if available <= 0:
            result["complete"] = False
            result["files_unread_due_to_budget"] = len(files) - index
            break
        item = inspect_file(path, available)
        item["ordinal"] = index + 1
        item["format"] = path.name
        consumed += item["counts"].get("bytes_read", 0)
        if not item["complete"] or item.get("changed_during_read"):
            result["complete"] = False
        result["files"].append(item)
    result["bytes_read"] = consumed
    totals = Counter()
    for item in result["files"]:
        totals.update(item["counts"])
    result["totals"] = dict(sorted(totals.items()))
    result["records_valid"] = all(
        item.get("valid_jsonl", False) for item in result["files"]
    )
    result["selection_supported"] = result["root_encoding_consistent"] and all(
        select_generation(names)["status"] == "selected" for names in groups.values()
    )
    return result


def self_test() -> None:
    assert (
        select_generation(["session.jsonl.zstd", "session.v4.jsonl.zstd"])["version"]
        == CURRENT_FORMAT_VERSION
    )
    assert (
        select_generation(
            ["session.v0.jsonl", "session.v04.jsonl", "session.v4.jsonl.tmp"]
        )["status"]
        == "no_canonical_generation"
    )
    assert (
        select_generation(["session.jsonl", "session.v4.jsonl.zstd"])["status"]
        == "encoding_conflict"
    )
    assert (
        select_generation(
            ["session.jsonl.zstd", f"session.v{CURRENT_FORMAT_VERSION + 1}.jsonl.zstd"]
        )["status"]
        == "future_format"
    )
    sentinel = "PRIVATE_SENTINEL_DO_NOT_EXPORT"
    data = {
        "type": sentinel,
        "message": {"role": "user", "content": sentinel},
        sentinel: {"text": sentinel},
    }
    line = json.dumps(data).encode() + b"\n"
    result = inspect_records(io.BytesIO(line), len(line))
    assert result["complete"] and result["counts"]["objects"] == 1
    serialized = json.dumps(result)
    assert sentinel not in serialized
    assert result["discriminators"]["$.message.role=user"] == 1
    assert result["discriminators"]["$.type=<other-value>"] == 1
    bad = inspect_records(io.BytesIO(b"{bad}\n\xff\n{}"), DEFAULT_FILE_BYTES)
    assert bad["counts"]["invalid_json"] == 1
    assert bad["counts"]["invalid_utf8"] == 1
    assert bad["counts"]["unterminated_tail"] == 1
    limited = inspect_records(io.BytesIO(line), len(line) - 1)
    assert not limited["complete"] and limited["counts"]["byte_budget_hit"] == 1
    packed = zstandard.ZstdCompressor().compress(line)
    with (
        zstandard.ZstdDecompressor().stream_reader(io.BytesIO(packed)) as raw,
        io.BufferedReader(raw) as stream,
    ):
        assert inspect_records(stream, len(line)) == result
    broken = inspect_records(io.BytesIO(b"[]\n"), DEFAULT_FILE_BYTES)
    assert broken["counts"]["non_object_records"] == 1
    assert not broken["valid_jsonl"] and not bad["valid_jsonl"]
    assert limited["counts"]["bytes_read"] == len(line)
    sample = {
        "type": "user/message",
        "seq": 0,
        "time": 1,
        "data": {
            "id": sentinel,
            "role": "user",
            "source": {"kind": sentinel},
            "content": [{"type": "text", "text": sentinel}],
            sentinel: sentinel,
        },
    }
    projected = inspect_records(
        io.BytesIO(json.dumps(sample).encode() + b"\n"), DEFAULT_FILE_BYTES
    )
    assert "user/message" in projected["samples"]
    assert sentinel not in json.dumps(projected)
    frames = packed + zstandard.ZstdCompressor().compress(line)
    with (
        zstandard.ZstdDecompressor().stream_reader(
            io.BytesIO(frames), read_across_frames=True
        ) as raw,
        io.BufferedReader(raw) as stream,
    ):
        both = inspect_records(stream, DEFAULT_FILE_BYTES)
        assert both["counts"]["objects"] == 2
    damaged = bytearray(zstandard.ZstdCompressor(write_checksum=True).compress(line))
    damaged[-1] ^= 1
    with (
        zstandard.ZstdDecompressor().stream_reader(io.BytesIO(damaged)) as raw,
        io.BufferedReader(raw) as stream,
    ):
        failed = inspect_records(stream, DEFAULT_FILE_BYTES)
        assert not failed["complete"] and failed["counts"]["read_failed"] == 1


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path)
    parser.add_argument("--file-bytes", type=int, default=DEFAULT_FILE_BYTES)
    parser.add_argument("--total-bytes", type=int, default=DEFAULT_TOTAL_BYTES)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    self_test()
    if args.self_test:
        print("inspect-dsh: self-test passed")
        return 0
    if args.root is None or args.file_bytes <= 0 or args.total_bytes <= 0:
        parser.error("--root and positive byte budgets are required")
    result = survey(args.root, args.file_bytes, args.total_bytes)
    json.dump(result, sys.stdout, indent=2, ensure_ascii=True)
    print()
    return (
        0
        if (
            result["complete"]
            and result.get("records_valid", False)
            and result.get("selection_supported", False)
        )
        else 2
    )


if __name__ == "__main__":
    raise SystemExit(main())
