#!/usr/bin/env python3
"""变异验证：证明 `scan-all --write-store` 的那些护栏**真的挡着东西**。

    uv run --no-project python scripts/mutate.py

# 🔴 为什么需要它

护栏写完之后，「它有效」和「它空转」在输出上一模一样 —— 都是一片绿。唯一能分开
两者的，是**把它守的那件事破坏掉，看它红不红**。

# 🔴 这个脚本自己也会空转

替换不到目标文本时，`str.replace` 会**静默返回原串**，于是测试照常全绿 ——
一份「全部通过」的报告，实际上一个变异都没做。所以每一条都先断言
「目标文本恰好出现一次」，断不住就整个退出非零，绝不继续。

判据是精确子串（`old in src` + `count == 1`），不是正则：正则会随着周围代码
微调而悄悄匹配到别处，或者匹配不到而无人察觉。

# 对照组

第一条 `control-*` 是一个**必然会红**的变异。它若也绿，说明测试根本没跑到那段
代码，后面所有「变异成功变红」的结论都不作数 —— 那时脚本会直接停下来。
"""

from __future__ import annotations

import subprocess
import sys
from dataclasses import dataclass, field
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
FEATURES = ["--features", "store"]


@dataclass(frozen=True)
class Mutation:
    name: str
    rel_path: str
    old: str
    new: str
    # 这条变异之后**必须变红**的测试。空 = 只要求整体变红。
    must_fail: list[str] = field(default_factory=list)
    why: str = ""

    @property
    def path(self) -> Path:
        return ROOT / self.rel_path


MUTATIONS: list[Mutation] = [
    # ── 对照：必然会红 ────────────────────────────────────────────────────
    Mutation(
        name="control-default-profile",
        rel_path="src/bin/svault.rs",
        old="        (None, false) => Ok(Profile::Metadata),",
        new="        (None, false) => Ok(Profile::Full),",
        must_fail=["writing_the_store_defaults_to_full_but_an_explicit_metadata_is_refused"],
        why="对照组：改掉一个被逐字断言的返回值。它若不红，后面的结论全部不作数",
    ),
    # ── 真正的护栏 ────────────────────────────────────────────────────────
    Mutation(
        name="metadata-guard-off",
        rel_path="src/bin/svault.rs",
        old="        (Some(ProfileArg::Metadata), true) => Err(",
        new="        (Some(ProfileArg::Metadata), true) if false => Err(",
        must_fail=["writing_the_store_defaults_to_full_but_an_explicit_metadata_is_refused"],
        why="放行 `--write-store --profile metadata` ⇒ 无正文事件会永久遮蔽正文",
    ),
    Mutation(
        name="state-flatten-off",
        rel_path="src/bin/svault.rs",
        old="    #[serde(flatten)]\n    cursor: Cursor,",
        new="    cursor: Cursor,",
        must_fail=["an_old_format_state_file_loads_and_claims_no_parser_revision"],
        why="状态文件改成不兼容格式 ⇒ 旧文件解析失败 ⇒ 空表 ⇒ 整机全量重扫",
    ),
    Mutation(
        name="full-read-assertion-off",
        rel_path="src/bin/svault.rs",
        old="        if mode != Projection::Append && obs.source_fingerprint.is_none() {",
        new="        if false && mode != Projection::Append && obs.source_fingerprint.is_none() {",
        must_fail=["opening_a_new_generation_from_an_incremental_read_is_refused"],
        why="允许拿增量的尾巴去取代一整代 ⇒ 前面的事件当场从库里消失",
    ),
    Mutation(
        name="has-prior-inverted",
        rel_path="src/store.rs",
        old="            .optional()?\n            .is_some())",
        new="            .optional()?\n            .is_none())",
        must_fail=["has_projection_separates_no_rows_from_generation_zero"],
        why="`has_projection` 答反 ⇒ 有前代判成没有（回退被记成追加）、没有判成有（续读漏掉前面的事件）",
    ),
    Mutation(
        name="first-backfill-not-detected",
        rel_path="src/bin/svault.rs",
        old="    if has_prior == Some(false) {\n        r |= ScanReasons::INITIAL;",
        new="    if false && has_prior == Some(false) {\n        r |= ScanReasons::INITIAL;",
        must_fail=["a_cursor_without_a_projection_counts_as_a_first_backfill"],
        why="游标在、库里空却不判首次 ⇒ 从游标处续读 ⇒ 游标之前的事件永久漏在库外（#44 本身的形状）",
    ),
    Mutation(
        name="preserve-written-anyway",
        rel_path="src/bin/svault.rs",
        old="            StoreAction::Preserve => return Ok((Committed::Preserved, plan)),",
        new="            StoreAction::Preserve => Projection::Append,",
        must_fail=["an_unreadable_source_is_held_not_written_and_says_which_kind"],
        why="计划说别写却照写 ⇒ 读失败/毒行的空批被当成真实结果落库",
    ),
    Mutation(
        name="lossy-scan-path",
        rel_path="src/bin/svault.rs",
        old="        SourceMode::AppendLog => {",
        new="        SourceMode::AppendLog if false => {",
        must_fail=["scan_one_keeps_the_full_observation_for_append_log"],
        why="退回有损投影 ⇒ 拿不到 should_record/source_change ⇒ 写库一条都不写且不报错",
    ),
    Mutation(
        name="never-recorded-counts-as-stale",
        rel_path="src/bin/svault.rs",
        old="    write_store && recorded.is_some_and(|rev| rev != current)",
        new="    write_store && recorded != Some(current)",
        must_fail=["never_recorded_is_not_stale"],
        why="把「没记过」当欠账 ⇒ 第一次写库就 Reparse ⇒ 删掉宿主已经写在库里的那份",
    ),
    Mutation(
        name="fingerprint-forgotten-on-increment",
        rel_path="src/bin/svault.rs",
        old="        .or_else(|| prev.and_then(|p| p.fingerprint.clone()));",
        new="        .or(None);",
        must_fail=["an_incremental_round_keeps_the_previous_fingerprint"],
        why="增量轮把上一版指纹忘掉 ⇒ 下次全读认不出同尺寸原地重写",
    ),
    Mutation(
        name="streaming-round-claims-revision",
        rel_path="src/bin/svault.rs",
        old="        parser_revision: write_store.then_some(session_vault::PARSER_REVISION),",
        new="        parser_revision: Some(session_vault::PARSER_REVISION),",
        must_fail=["a_streaming_only_round_claims_no_parser_revision"],
        why="只吐流的运行也记版本 ⇒ 日后第一次写库误判成「不欠重投影」",
    ),
]


def run_tests(names: list[str]) -> tuple[bool, str]:
    """跑指定测试。返回 (是否全绿, 输出摘要)。"""
    cmd = ["cargo", "test", *FEATURES, "--bin", "svault"]
    if names:
        # 一次只跑一个过滤器；多个就逐个跑（cargo test 只收一个 filter）。
        ok = True
        tails: list[str] = []
        for n in names:
            p = subprocess.run(
                [*cmd, n], cwd=ROOT, capture_output=True, text=True, encoding="utf-8"
            )
            ok = ok and p.returncode == 0
            tails.append(f"{n}: rc={p.returncode}")
        return ok, "; ".join(tails)
    p = subprocess.run(cmd, cwd=ROOT, capture_output=True, text=True, encoding="utf-8")
    return p.returncode == 0, f"rc={p.returncode}"


def main() -> int:
    # 0. 未变异时必须全绿 —— 否则「变红」说明不了任何事。
    ok, detail = run_tests([])
    if not ok:
        print(f"✗ 基线就不是绿的（{detail}）—— 先修好再谈变异", file=sys.stderr)
        return 1
    print("✓ 基线全绿")

    failures: list[str] = []
    for i, m in enumerate(MUTATIONS):
        # 🔴 **按字节读写，不用 `read_text` / `write_text`。**
        #
        # 那两个在 Windows 上会做换行翻译：读进来 CRLF 变 LF、写回去 LF 变 CRLF。
        # 于是「已还原」这个声明变成**假的** —— 内容相同、**字节不同**，工作区从此
        # 一直脏，并且会挡住 `git checkout`（实测撞过一次）。而本仓 `.gitattributes`
        # 钉着 `*.rs text eol=lf`，字节口径必须原样往返。
        #
        # ⚠️ 一个「验证脚本」把工作区改脏而自称已还原，比没有这个脚本更坏。
        raw = m.path.read_bytes()
        src = raw.decode("utf-8")
        # 目标串在本文件里按 LF 写。若这份 checkout 是 CRLF，就把目标也换成 CRLF ——
        # 否则多行目标一条都匹配不上，而那会被读成「脚本过期」，不是「换行不同」。
        eol = "\r\n" if "\r\n" in src else "\n"
        old = m.old.replace("\n", eol)
        new = m.new.replace("\n", eol)

        # 🔴 空转防线：目标文本必须**恰好出现一次**。
        count = src.count(old)
        if count != 1:
            print(
                f"✗ [{m.name}] 目标文本出现 {count} 次（要求恰好 1 次）——"
                f" 变异脚本已过期，**不是**护栏有效。\n    目标：{old[:80]!r}",
                file=sys.stderr,
            )
            return 2

        mutated = src.replace(old, new)
        if mutated == src:
            print(f"✗ [{m.name}] 替换后文本未变 —— 空转", file=sys.stderr)
            return 2

        try:
            m.path.write_bytes(mutated.encode("utf-8"))
            ok, detail = run_tests(m.must_fail)
            if ok:
                # 绿 = 这条护栏空转（或测试没跑到它守的那段）。
                print(f"✗ [{m.name}] 变异后仍全绿 —— 护栏没挡住它。{m.why}")
                failures.append(m.name)
            else:
                print(f"✓ [{m.name}] 如期变红（{detail}） — {m.why}")
        finally:
            m.path.write_bytes(raw)
            # 🔴 **还原要断言，不能只是「我写回去了」。** 上面那个换行缺陷正是
            # 「写回去了但字节不同」，而它不会报错，只会让下一条变异从一个被污染的
            # 基线出发 —— 那时全绿/全红都说明不了任何事。
            if m.path.read_bytes() != raw:
                print(f"✗ [{m.name}] 还原后字节与读入时不同 —— 工作区已被污染", file=sys.stderr)
                return 6

        # 对照组不红 ⇒ 后面的结论全部不作数，立刻停。
        if i == 0 and m.name in failures:
            print(
                "✗ 对照组没变红 —— 测试根本没跑到那段代码，后面的结论不作数",
                file=sys.stderr,
            )
            return 3

    # 收尾：确认已还原（不是「我记得还原了」）。
    ok, detail = run_tests([])
    if not ok:
        print(f"✗ 还原之后不是绿的（{detail}）—— 工作区可能被留在变异态", file=sys.stderr)
        return 4

    if failures:
        print(f"\n✗ {len(failures)} 条护栏空转：{', '.join(failures)}", file=sys.stderr)
        return 5
    print(f"\n✓ {len(MUTATIONS)} 条变异全部如期变红，且已还原")
    return 0


if __name__ == "__main__":
    sys.exit(main())
