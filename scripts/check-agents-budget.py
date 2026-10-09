"""AGENTS.md 体量闸。

- 每个 AGENTS.md ≤200 行、≤24 KB，单行 ≤320 字符；
- 仓库根到任一子目录，沿途 AGENTS.md 合计 ≤28 KB（Codex 只读合计 32 KiB，超出部分静默截掉）；
- CLAUDE.md 只能是一行 `@AGENTS.md`（块级 HTML 注释不算内容）。

先自检：坏样本必须被报、好样本必须通过，否则退出非零、不去量仓库 ——
一个坏掉的闸和一个合规的仓库，输出一模一样。
"""

import pathlib
import re
import sys

MAX_LINES = 200
MAX_BYTES = 24 * 1024
MAX_LINE_CHARS = 320
MAX_PATH_BYTES = 28 * 1024
SKIP_DIRS = {".git", "target", "node_modules", ".idea", ".vscode"}
HTML_COMMENT = re.compile(r"<!--.*?-->", re.S)


def agents_findings(text: str) -> list[str]:
    out = []
    lines = text.split("\n")
    if lines and lines[-1] == "":
        lines.pop()
    if len(lines) > MAX_LINES:
        out.append(f"{len(lines)} 行，上限 {MAX_LINES}")
    size = len(text.encode("utf-8"))
    if size > MAX_BYTES:
        out.append(f"{size} 字节，上限 {MAX_BYTES}")
    for i, line in enumerate(lines, 1):
        if len(line) > MAX_LINE_CHARS:
            out.append(f"第 {i} 行 {len(line)} 字符，上限 {MAX_LINE_CHARS}")
    return out


def claude_findings(text: str) -> list[str]:
    body = [l.strip() for l in HTML_COMMENT.sub("", text).split("\n") if l.strip()]
    return [] if body == ["@AGENTS.md"] else [f"不是导入壳：内容为 {body!r}"]


def path_findings(root: pathlib.Path) -> list[str]:
    """沿途合计：每个含 AGENTS.md 的目录，把它到仓库根之间所有 AGENTS.md 加起来。"""
    sizes = {}
    for f in root.rglob("AGENTS.md"):
        if not SKIP_DIRS.intersection(f.relative_to(root).parts):
            sizes[f.parent] = f.stat().st_size
    out = []
    for d in sizes:
        total = sum(s for p, s in sizes.items() if p == d or p in d.parents)
        if total > MAX_PATH_BYTES:
            out.append(f"{d.relative_to(root) or '.'}：沿途 AGENTS.md 合计 {total} 字节，上限 {MAX_PATH_BYTES}")
    return out


def self_test() -> None:
    cases = [
        ("超行数", agents_findings("x\n" * (MAX_LINES + 1)), True),
        ("超字节", agents_findings(("汉" * 100 + "\n") * 90), True),
        ("超长行", agents_findings("y" * (MAX_LINE_CHARS + 1) + "\n"), True),
        ("合规正文", agents_findings("# AGENTS.md\n\n- 一条规则\n"), False),
        ("CLAUDE.md 是副本", claude_findings("# 规则\n- 一条\n"), True),
        ("CLAUDE.md 带注释的导入壳", claude_findings("\n".join(["<!-- 说明", "多行 -->", "@AGENTS.md", ""])), False),
    ]
    broken = [name for name, found, must_report in cases if bool(found) != must_report]
    if broken:
        sys.exit(f"check-agents-budget: 自检失败（{', '.join(broken)}）—— 闸本身坏了，不量仓库")


def main() -> int:
    self_test()
    root = pathlib.Path(__file__).resolve().parent.parent
    problems = [f"AGENTS.md: {m}" for m in agents_findings((root / "AGENTS.md").read_text(encoding="utf-8"))]
    claude = root / "CLAUDE.md"
    if claude.exists():
        problems += [f"CLAUDE.md: {m}" for m in claude_findings(claude.read_text(encoding="utf-8"))]
    problems += path_findings(root)
    if problems:
        print("check-agents-budget: 未过", *problems, sep="\n  ", file=sys.stderr)
        return 1
    text = (root / "AGENTS.md").read_text(encoding="utf-8")
    print(f"check-agents-budget: AGENTS.md {text.count(chr(10))} 行、{len(text.encode('utf-8'))} 字节，在预算内（自检先通过）。")
    return 0


if __name__ == "__main__":
    sys.exit(main())
