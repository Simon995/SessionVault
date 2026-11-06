# /// script
# requires-python = ">=3.10"
# dependencies = ["zstandard==0.25.0"]
# ///
"""核对一次读取所得字节的分行、逐帧结束与 checksum，不恢复 DSH 语义。

    uv run scripts/check-dsh-snapshot.py --self-test

供 verify-dsh-snapshot.mjs 通过 --batch 传入合成快照路径。每个文件只读取
一次；输出摘要、计数和原因，不输出路径或正文。不写文件、不连接总库。
complete 只描述该字节快照的物理与 JSONL 完整性，不是生产推进凭据。
"""

from __future__ import annotations

import argparse
import hashlib
import json
import sys
from pathlib import Path
from typing import TypedDict

import zstandard

MAX_INPUT_BYTES = 8 * 1024 * 1024
MAX_DECODED_BYTES = 32 * 1024 * 1024
DECODE_CHUNK_BYTES = 64
ZSTD_MAGIC = b"\x28\xb5\x2f\xfd"


class Progress(TypedDict):
    snapshot_sha256: str
    physical_bytes: int
    verified_physical_bytes: int
    decoded_bytes: int
    records: int
    frames: int
    checksummed_frames: int


def reject_constant(_value: str) -> None:
    raise ValueError("non-JSON constant")


def inspect_bytes(data: bytes, compression: str) -> dict:
    """所有证据来自同一个不可变 bytes；不把另一次文件读取的结果拼进来。"""
    result: Progress = {
        "snapshot_sha256": hashlib.sha256(data).hexdigest(),
        "physical_bytes": len(data),
        "verified_physical_bytes": 0,
        "decoded_bytes": 0,
        "records": 0,
        "frames": 0,
        "checksummed_frames": 0,
    }
    logical = hashlib.sha256()

    def finish(status: str, reason: str) -> dict:
        return {
            **result,
            "status": status,
            "reason": reason,
            "verified_jsonl_sha256": logical.hexdigest(),
        }

    def records(payload: bytes, first_frame: bool = False) -> str | None:
        if not payload and not first_frame:
            return None  # 空事件帧仍停在既有记录边界，不能冒充空 header。
        if not payload.endswith(b"\n"):
            return "unterminated_record"
        lines = payload[:-1].split(b"\n")
        if first_frame and len(lines) != 1:
            return "header_frame_not_single_record"
        try:
            for line in lines:
                value = json.loads(line.decode("utf-8"), parse_constant=reject_constant)
                if not isinstance(value, dict):
                    return "non_object_record"
        except (UnicodeDecodeError, ValueError, RecursionError):
            return "invalid_json_or_utf8"
        result["records"] += len(lines)
        result["decoded_bytes"] += len(payload)
        logical.update(payload)
        return None

    if len(data) > MAX_INPUT_BYTES:
        return finish("budget_exceeded", "input_budget")
    if not data:
        return finish("incomplete", "empty")
    if compression == "none":
        if not data.endswith(b"\n"):
            return finish("incomplete", "unterminated_record")
        problem = records(data)
        if problem:
            return finish("invalid", problem)
        result["verified_physical_bytes"] = len(data)
        return finish("complete", "verified")
    if compression != "zstd":
        return finish("unsupported", "compression")

    offset = 0
    while offset < len(data):
        remaining = data[offset:]
        if not remaining.startswith(ZSTD_MAGIC):
            if ZSTD_MAGIC.startswith(remaining):
                return finish("incomplete", "torn_frame_header")
            return finish("invalid", "frame_magic")
        try:
            parameters = zstandard.get_frame_parameters(remaining)
        except zstandard.ZstdError:
            # 参数不够或不合法尚未区分；不能直接认成永久损坏。
            return finish("unverified", "frame_parameters_unavailable")
        if parameters.window_size > MAX_DECODED_BYTES:
            return finish("budget_exceeded", "window_budget")
        if not parameters.has_checksum:
            return finish("unsupported", "checksum_absent")
        if parameters.dict_id:
            return finish("unsupported", "dictionary_required")
        if (
            parameters.content_size != zstandard.CONTENTSIZE_UNKNOWN
            and parameters.content_size > MAX_DECODED_BYTES - result["decoded_bytes"]
        ):
            return finish("budget_exceeded", "decoded_budget")
        decoder = zstandard.ZstdDecompressor().decompressobj(read_across_frames=False)
        chunks = []
        decoded = 0
        fed = 0
        try:
            while fed < len(remaining) and not decoder.eof:
                chunk = remaining[fed : fed + DECODE_CHUNK_BYTES]
                fed += len(chunk)
                plaintext = decoder.decompress(chunk)
                decoded += len(plaintext)
                if decoded > MAX_DECODED_BYTES - result["decoded_bytes"]:
                    return finish("budget_exceeded", "decoded_budget")
                chunks.append(plaintext)
        except zstandard.ZstdError:
            return finish("invalid", "frame_decode_or_checksum")
        if not decoder.eof:
            # 即使 chunks 含全部事件，未到帧尾就没有完成 checksum 验证。
            return finish("incomplete", "torn_frame")
        consumed = fed - len(decoder.unused_data)
        if consumed <= 0:
            return finish("invalid", "frame_did_not_advance")
        problem = records(b"".join(chunks), first_frame=(offset == 0))
        if problem:
            return finish("invalid", problem)
        offset += consumed
        result["frames"] += 1
        result["checksummed_frames"] += 1
        result["verified_physical_bytes"] = offset
    return finish("complete", "verified")


def inspect_file(request: dict) -> dict:
    try:
        # limit + 1 区分达到预算与恰好读完；只打开一次，不补读第二份快照。
        with Path(request["path"]).open("rb") as source:
            data = source.read(MAX_INPUT_BYTES + 1)
    except OSError:
        return {"status": "unavailable", "reason": "read_failed"}
    if len(data) > MAX_INPUT_BYTES:
        # 没有读完整，不能把预算内前缀的 hash 冒充整个文件的摘要。
        return {"status": "budget_exceeded", "reason": "input_budget"}
    return inspect_bytes(data, request["compression"])


def self_test() -> None:
    header = b'{"type":"session","version":4}\n'
    event = b'{"type":"fixture/event","seq":0}\n'
    compressor = zstandard.ZstdCompressor(write_checksum=True)
    first = compressor.compress(header)
    second = compressor.compress(event)
    complete = first + second
    baseline = inspect_bytes(complete, "zstd")
    assert baseline["status"] == "complete"
    assert baseline["frames"] == baseline["checksummed_frames"] == 2
    assert baseline["records"] == 2
    assert baseline["verified_physical_bytes"] == len(complete)
    assert (
        baseline["verified_jsonl_sha256"] == hashlib.sha256(header + event).hexdigest()
    )
    # 每个非空截断点都必须阻断；完整帧边界上的独立前缀另作完整快照处理。
    for cut in range(1, len(second)):
        result = inspect_bytes(first + second[:cut], "zstd")
        assert result["status"] != "complete", cut
        assert result["verified_physical_bytes"] == len(first), cut
    for index in range(1, 5):
        corrupt = bytearray(complete)
        corrupt[-index] ^= 1
        assert inspect_bytes(bytes(corrupt), "zstd")["status"] == "invalid"
    assert inspect_bytes(first, "zstd")["status"] == "complete"
    empty_frame = compressor.compress(b"")
    empty_tail = inspect_bytes(first + empty_frame, "zstd")
    assert empty_tail["status"] == "complete" and empty_tail["records"] == 1
    assert empty_tail["frames"] == 2
    assert inspect_bytes(empty_frame, "zstd")["status"] == "invalid"
    assert inspect_bytes(header + event, "none")["status"] == "complete"
    assert inspect_bytes((header + event)[:-1], "none")["status"] == "incomplete"
    assert inspect_bytes(header + b"invalid\n", "none")["status"] == "invalid"
    assert inspect_bytes(header + b'{"text":"\xff"}\n', "none")["status"] == "invalid"
    assert inspect_bytes(b"", "none")["status"] == "incomplete"
    assert (
        inspect_bytes(first + compressor.compress(event[:-1]), "zstd")["status"]
        == "invalid"
    )
    assert (
        inspect_bytes(compressor.compress(header + event), "zstd")["status"]
        == "invalid"
    )
    unchecked = zstandard.ZstdCompressor(write_checksum=False).compress(header)
    assert inspect_bytes(unchecked, "zstd")["status"] == "unsupported"
    assert inspect_bytes(header, "other")["status"] == "unsupported"
    assert (
        inspect_file({"path": __file__ + ".missing", "compression": "none"})["status"]
        == "unavailable"
    )
    print(
        json.dumps(
            {
                "self_test": "passed",
                "second_frame_truncation_points": len(second) - 1,
                "checksum_mutations": 4,
                "zstandard_version": zstandard.__version__,
            }
        )
    )


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--self-test", action="store_true")
    mode.add_argument("--batch", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        self_test()
    else:
        requests = json.load(sys.stdin)
        print(
            json.dumps(
                {
                    "results": [inspect_file(request) for request in requests],
                    "zstandard_version": zstandard.__version__,
                }
            )
        )


if __name__ == "__main__":
    main()
