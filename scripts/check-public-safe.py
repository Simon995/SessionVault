#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""本仓是**公开仓** —— 扫一遍有没有把真实身份写进去。

    python scripts/check-public-safe.py        # 干净则退出 0

## 为什么要有这个脚本

AGENTS.md 开头就写着「不得出现真实用户名、个人路径、邮箱」。而 2026-08-08 做过一次
「安全审计」并**判它干净** —— 那次查的是密钥 / 邮箱 / 账号 id，**没查用户名与路径**，
于是 15 个文件里的 `/home/<真名>/…`、私有项目名一直留着，直到 2026-08-24 才被翻出来。

**一条只写在文档里的纪律，等于一条会被违反的纪律。** 这个脚本是那条纪律的执行者。

## 🔴 它自己会先证明自己没坏

一个坏掉的扫描器和一个干净的仓库，输出**一模一样**（都是「没发现问题」）。所以
`self_test()` 先拿一份**已知有问题**的样本喂给同一套规则，检不出来就直接退出非零、
根本不去扫仓库。这与本仓「护栏要正向断言，『没有那句报错』证明不了『它在』」同一条。

## 判据是**白名单**，不是黑名单

黑名单只拦得住上次见过的那个名字。这里反过来：任何 `/home/X`、`Users/X` 里的 `X`
只要不在白名单里就报出来 —— 于是新加一个占位名是一个**要写下来的决定**，而不是
一次谁都不会注意到的漂移。

⚠️ **数字要保留。** 脱敏脱的是身份，不是证据：「96.8 万条事件一条没被看过」这种
数字就是那些规则的价值本身，本脚本一个数字都不碰。
"""
from __future__ import annotations

import os
import subprocess
import re
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SKIP_DIRS = {".git", "target", "node_modules", ".idea", ".vscode"}
TEXT_EXT = {".rs", ".md", ".toml", ".yml", ".yaml", ".json", ".txt", ".sh", ".ps1", ".py"}
EXTRA_FILES = {"LICENSE", "README.md"}

# 允许出现在 `/home/X` / `Users/X` 位置上的名字。
#   `<user>`  —— 散文与注释里的占位符（AGENTS.md 的约定）
#   `dev`/`u` —— 测试 fixture 用的中性字面量（把 `<user>` 放进一条当作真路径用的
#                字符串里，读起来像模板没渲染）
#   `user`    —— Windows 侧的通用名，本身不指向任何人
#   `me`/`a`  —— 本仓 `pathnorm.rs` 早就在用的中性 fixture 名
#   其余是 CI / 容器里的标准账户
#
# ⚠️ 另有两类**由构造**就是占位、不必逐个登记：`<…>` 尖括号形态，以及纯点号
# （`/home/...` 这种省略）。第一版把它们也报了出来 —— 47 条里一条真问题都没有，
# 而**一个满屏误报的扫描器等于一个关掉的扫描器**：噪声淹掉真信号，然后没人再看它。
ALLOWED_HOME_NAMES = {
    "<user>", "dev", "u", "user", "me", "a", "ubuntu", "runner", "root", "vscode",
}

# 故意保留的公开身份：MIT 许可证要求保留版权行，仓库 URL 是它自己的地址。
# 写在这里而不是靠正则绕开 —— 一个例外应当看得见、有理由。
ALLOWED_LITERALS = ("Simon Ma", "Simon995")

# 三种形态各自的边界不同 —— 合成一条正则时 `-home-<user>-workspace-Proj` 会把
# `<user>-workspace-Proj` 整个当成用户名（第一版就是这么误报的）。
#   `/home/X/…`、`Users/X/…`  X 到下一个分隔符为止
#   `-home-X-…`（Claude 的目录编码）X 到下一个 `-` 为止
HOME_PAT = re.compile(r"(?:/home/|[/\\]Users[/\\])([^/\\\s\"\'`,;:)\]}]+)")
ENCODED_PAT = re.compile(r"-home-([A-Za-z0-9_<>]+)")
EMAIL_PAT = re.compile(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}")
# 🔴 **每一条都要锚定。** 裸子串 `noreply` 会放过任何含它的**真实**地址
# （`noreply@某公司.com` 是常见的真实发件地址）—— 那是**假阴**，比假阳更坏：
# 假阳会被看见并烦人，假阴什么都不说。2026-09-02 实测确认过这一格。
#
# ⚠️ 白名单里每一条都必须是「**必然假阳**」——每次提交必然出现、且不可能是
# 真实身份的那些。挡不住这个判据的，就该收窄而不是留着。
EMAIL_OK = re.compile(
    r"(@users\.noreply\.github\.com$"          # GitHub 署名（本仓提交者）
    r"|^noreply@anthropic\.com$"                # Co-Authored-By 署名
    r"|^noreply@github\.com$"                   # GitHub 系统提交
    r"|^git@github\.com$"                       # SSH remote URL，不是邮箱
    r"|[@.]example\.(com|net|org)$"             # RFC 2606 保留域（含 corp.example.net 这类子域）
    r"|@ts-|localhost)"
)


def is_placeholder(name: str) -> bool:
    """由构造就是占位符的两类，不必逐个登记进白名单。"""
    return (name.startswith("<") and name.endswith(">")) or set(name) <= {".", "…"}


def findings_in(text: str) -> list[str]:
    """同一套规则，自检与真扫共用 —— 两份实现会各自演化，而漂开时不会有人报错。"""
    out = []
    for i, line in enumerate(text.splitlines(), 1):
        for pat in (HOME_PAT, ENCODED_PAT):
            for m in pat.finditer(line):
                name = m.group(1)
                if is_placeholder(name) or name in ALLOWED_HOME_NAMES:
                    continue
                out.append(f"line {i}: 家目录里出现 {name!r} —— {m.group(0)!r}")
        for m in EMAIL_PAT.finditer(line):
            if not EMAIL_OK.search(m.group(0)):
                out.append(f"line {i}: 邮箱 {m.group(0)!r}")
    return out


def self_test() -> None:
    """检不出已知有问题的样本，就别去扫仓库 —— 那时的『干净』什么也不说明。"""
    bad = "\n".join(
        [
            "let p = \"/home/alice/workspace/Secret\";",
            r'let w = r"C:\Users\bob\code";',
            "// 联系 someone@corp.example.net 之前先读 ADR",  # 允许：example.*
            "// 真实的 contact: person@realcompany.io",
        ]
    )
    hits = findings_in(bad)
    want = [("alice" in h) for h in hits], [("bob" in h) for h in hits]
    if not any(want[0]):
        sys.exit("SELF-TEST 失败：漏掉了 /home/alice —— 扫描器坏了，本次结果不作数")
    if not any(want[1]):
        sys.exit("SELF-TEST 失败：漏掉了 Users\\bob —— 扫描器坏了，本次结果不作数")
    if not any("realcompany.io" in h for h in hits):
        sys.exit("SELF-TEST 失败：漏掉了真实邮箱 —— 扫描器坏了，本次结果不作数")
    if any("example.net" in h for h in hits):
        sys.exit("SELF-TEST 失败：把 example.* 当成了真实邮箱 —— 会淹掉真信号")

    clean = "\n".join(
        [
            'let p = "/home/dev/workspace/Proj";',
            "// 见 /home/<user>/… 与 -home-<user>-workspace-Proj",
            "// 省略形态 /home/... 也不该报",
            'let e = "-home-me-notes";',
        ]
    )
    noise = findings_in(clean)
    if noise:
        sys.exit("SELF-TEST 失败：占位形态被误报 %s —— 会淹掉真信号" % noise)

    # 🔴 反向：换成真名，同样的形态**必须**被抓住。少了这条，一个「什么都不报」
    # 的实现照样能通过上面每一条。
    encoded_bad = 'let e = "-home-carol-notes";'
    if not any("carol" in h for h in findings_in(encoded_bad)):
        sys.exit("SELF-TEST 失败：漏掉了编码形态 -home-carol- —— 扫描器坏了")


def scan_commit_messages(rev_range: str) -> tuple[list[str], int]:
    """扫一个 revision range 的 **commit message**。

    🔴 为什么需要它：`git push` 上传的不只是工作区文件，**commit message 同样进
    公开历史**，而本脚本此前只 `os.walk` 文件树 —— 那一半从来没被检查过。
    2026-09-02 核出这个缺口时历史恰好是干净的；**「这次没事」不是「以后没事」**。

    ⚠️ 返回 `(问题, 实际检查的提交数)` —— **必须把条数报出去**：取不到提交时
    `git log` 返回空，而「零个提交、没有问题」与「一个都没检查」在布尔上一模一样，
    正是 AGENTS.md「第二条地基」那个形状。调用方据条数判断这句「干净」覆盖了什么。
    """
    # 🔴 **不只 `%B`**：author / committer 邮箱同样进公开历史，而它们
    # **不在 message 正文里** —— 2026-09-02 实测本仓早期有 34 个提交的署名是
    # 真实私人 / 工作邮箱，而当时只扫 `%B` 的闸对它们完全沉默。
    fmt = "%H" + chr(31) + "%an <%ae>%n%cn <%ce>%n%B" + chr(30)
    try:
        out = subprocess.run(
            ["git", "log", "--format=" + fmt, rev_range],
            cwd=ROOT, capture_output=True, text=True,
            encoding="utf-8", errors="replace",
        )
    except OSError as e:
        return ([("  <git 不可用：%s>" % e)], 0)
    if out.returncode != 0:
        # 🔴 「范围解析失败」不是「没有问题」—— 说出来，别静默当成干净。
        return ([("  <git log %s 失败：%s>" % (rev_range, out.stderr.strip()[:200]))], 0)
    problems: list[str] = []
    checked = 0
    for record in out.stdout.split(chr(30)):
        record = record.strip()
        if not record:
            continue
        sha, _, body = record.partition(chr(31))
        checked += 1
        for hit in findings_in(body):
            problems.append("  commit %s: %s" % (sha[:8], hit))
    return (problems, checked)


def main() -> int:
    self_test()

    problems: list[str] = []
    scanned = 0
    for dirpath, dirnames, filenames in os.walk(ROOT):
        dirnames[:] = [d for d in dirnames if d not in SKIP_DIRS]
        for fn in filenames:
            if os.path.splitext(fn)[1].lower() not in TEXT_EXT and fn not in EXTRA_FILES:
                continue
            path = os.path.join(dirpath, fn)
            rel = os.path.relpath(path, ROOT).replace("\\", "/")
            if rel == "scripts/check-public-safe.py":
                continue  # 自检样本本来就带着「坏」字符串
            try:
                text = open(path, encoding="utf-8", newline="").read()
            except (UnicodeDecodeError, PermissionError):
                continue
            scanned += 1
            for hit in findings_in(text):
                problems.append(f"  {rel}: {hit}")

    if problems:
        print("公开仓里发现可能的真实身份（%d 处）：" % len(problems))
        print("\n".join(problems))
        print(
            "\n判据见 AGENTS.md「这是一个公开仓」。散文与注释写 `<user>`；"
            "测试 fixture 用中性字面量（白名单在本脚本顶部，加名字要连理由一起加）。"
        )
        return 1

    # 🔴 **推送前还要扫 commit message** —— 它和文件一样进公开历史。
    # 默认范围「本地领先 origin/main 的那些」= 这次 push 会上传的正是它们。
    rev_range = sys.argv[1] if len(sys.argv) > 1 else "origin/main..HEAD"
    msg_problems, checked = scan_commit_messages(rev_range)
    if msg_problems:
        print("commit message 里发现可能的真实身份（%d 处）：" % len(msg_problems))
        print(*msg_problems, sep=chr(10))
        print("⚠️ message 推上去就是公开历史的一部分，改它要 rebase 重写。")
        return 1

    print("public-safe: 扫了 %d 个文件，未发现真实用户名 / 个人路径 / 邮箱。" % scanned)
    # ⚠️ **报条数，不报「有没有」**：0 条待推提交与「一条都没检查」在布尔上一样，
    # 在计数上不一样。
    print("public-safe: 另扫了 %d 条 commit message（范围 %s）。" % (checked, rev_range))
    print("（自检先通过，所以这句『干净』是有依据的。）")
    return 0


if __name__ == "__main__":
    sys.exit(main())
