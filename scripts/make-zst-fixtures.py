# /// script
# requires-python = ">=3.10"
# dependencies = ["zstandard==0.25.0"]
# ///
"""生成 `tests/fixtures/zst/` 下的 zstd 夹具：同一份合成 rollout，用 libzstd 写成四种形态。

    uv run scripts/make-zst-fixtures.py

Codex 用的是 libzstd（Rust `zstd` crate，level 3，写内容长度）；本仓解压用的是纯 Rust 的
`ruzstd`。夹具用 libzstd 写，测的才是「Codex 写的东西我们读不读得懂」，而不是「ruzstd 读不读
得懂 ruzstd 自己写的」。内容是合成的，不含任何真实会话。

- `like-codex`：与 Codex 相同（level 3、单帧、帧头带内容长度、不带校验和）。
- `checksum`：同上，另带校验和。
- `no-size`：流式写入、帧头不带内容长度 —— 长度只能解压了数。
- `two-frames`：按行切成两帧，中间夹一个可跳过帧。
"""

import json
from pathlib import Path

import zstandard

OUT = Path(__file__).resolve().parent.parent / "tests" / "fixtures" / "zst"
SKIPPABLE_MAGIC = 0x184D2A53


def rollout() -> bytes:
    lines = [
        {
            "timestamp": "2026-04-01T08:00:00.000Z",
            "type": "session_meta",
            "payload": {
                "id": "s1",
                "cwd": "/w/p",
                "git": {"repository_url": "https://example.com/team/repo"},
            },
        },
        {
            "timestamp": "2026-04-01T08:00:01.000Z",
            "type": "turn_context",
            "payload": {"cwd": "/w/p", "model": "m1", "effort": "high"},
        },
    ]
    for i in range(40):
        lines.append(
            {
                "timestamp": f"2026-04-01T08:{1 + i // 60:02d}:{i % 60:02d}.000Z",
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "user" if i % 2 == 0 else "assistant",
                    "content": [
                        {"type": "input_text", "text": f"line {i} " + "word " * (i % 7)}
                    ],
                },
            }
        )
        lines.append(
            {
                "timestamp": f"2026-04-01T08:{1 + i // 60:02d}:{i % 60:02d}.500Z",
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {
                        "total_token_usage": {
                            "input_tokens": 100 * (i + 1),
                            "cached_input_tokens": 10 * i,
                            "output_tokens": 5 * (i + 1),
                        }
                    },
                },
            }
        )
    return "".join(
        json.dumps(line, separators=(",", ":")) + "\n" for line in lines
    ).encode()


def skippable(payload: bytes) -> bytes:
    return (
        SKIPPABLE_MAGIC.to_bytes(4, "little")
        + len(payload).to_bytes(4, "little")
        + payload
    )


def main() -> None:
    plain = rollout()
    OUT.mkdir(parents=True, exist_ok=True)
    (OUT / "rollout.jsonl").write_bytes(plain)

    like_codex = zstandard.ZstdCompressor(
        level=3, write_content_size=True, write_checksum=False
    )
    (OUT / "like-codex.jsonl.zst").write_bytes(like_codex.compress(plain))

    checksum = zstandard.ZstdCompressor(
        level=3, write_content_size=True, write_checksum=True
    )
    (OUT / "checksum.jsonl.zst").write_bytes(checksum.compress(plain))

    streaming = zstandard.ZstdCompressor(level=3, write_content_size=False)
    chunker = streaming.chunker()
    no_size = b"".join(chunker.compress(plain)) + b"".join(chunker.finish())
    assert (
        zstandard.get_frame_parameters(no_size).content_size
        == zstandard.CONTENTSIZE_UNKNOWN
    )
    (OUT / "no-size.jsonl.zst").write_bytes(no_size)

    cut = plain.index(b"\n", len(plain) // 2) + 1
    two = (
        like_codex.compress(plain[:cut])
        + skippable(b"not content")
        + like_codex.compress(plain[cut:])
    )
    (OUT / "two-frames.jsonl.zst").write_bytes(two)

    # 自检：libzstd 自己读回来必须是原文（多帧要用流式解压，`decompress` 只读一帧）。
    for name in ["like-codex", "checksum", "no-size", "two-frames"]:
        data = (OUT / f"{name}.jsonl.zst").read_bytes()
        reader = zstandard.ZstdDecompressor().stream_reader(
            data, read_across_frames=True
        )
        assert reader.read() == plain, name
    print(f"wrote 5 files to {OUT.name}/ ({len(plain)} bytes of rollout)")


if __name__ == "__main__":
    main()
