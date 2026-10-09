"""构建并发布 svault（Windows / Linux 两个二进制）。

    uv run python scripts/release.py <tag>            # 试运行：编两个平台、逐项验证，不发布
    uv run python scripts/release.py <tag> --publish  # 再打标签、发 GitHub Release、装到本机

<tag> 形如 v2026.10.09 或 v2026.10.09.1。Linux 版在 WSL 默认发行版里编。
只发已在 origin/main 上、工作区干净的提交；两个二进制的 --version 必须正好是这个提交，
且不能嵌着本机家目录 —— 依赖源码的绝对路径会编进二进制，而本仓公开。
"""

import hashlib
import os
import pathlib
import re
import shutil
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
TAG_RE = re.compile(r"^v\d{4}\.\d{2}\.\d{2}(\.\d+)?$")


def sh(*cmd: str, env=None) -> str:
    r = subprocess.run(cmd, cwd=ROOT, capture_output=True, text=True, encoding="utf-8", errors="replace", env=env)
    if r.returncode != 0:
        sys.exit(f"失败：{' '.join(cmd)}\n{r.stderr.strip()[-2000:]}")
    return r.stdout.strip()


def wsl(*cmd: str) -> str:
    # -e：直接执行，不经默认 shell —— 否则 Windows 路径里的反斜杠会被当转义吃掉。
    return sh("wsl.exe", "-e", *cmd)


def preflight(tag: str, publish: bool) -> str:
    """试运行可在任何提交上跑；发布只认 origin/main 上、工作区干净的提交。"""
    if not TAG_RE.match(tag):
        sys.exit(f"标签 {tag!r} 不合格式（v2026.10.09 或 v2026.10.09.1）")
    sh("git", "fetch", "-q", "origin", "--tags")
    head = sh("git", "rev-parse", "HEAD")
    if sh("git", "tag", "--list", tag):
        sys.exit(f"标签 {tag} 已存在")
    if publish:
        if sh("git", "branch", "--show-current") != "main":
            sys.exit("只从 main 发布")
        if sh("git", "status", "--porcelain"):
            sys.exit("工作区不干净")
        if head != sh("git", "rev-parse", "origin/main"):
            sys.exit("HEAD 不等于 origin/main：只发已发布的提交")
    return head


def build_windows(dist: pathlib.Path, tag: str) -> pathlib.Path:
    env = dict(os.environ, CARGO_TARGET_DIR=str(ROOT / "target" / "dist-build"),
               RUSTFLAGS=f"--remap-path-prefix={os.environ['USERPROFILE']}=~")
    sh("cargo", "build", "--release", "--locked", "--bin", "svault", "--features", "store", env=env)
    out = dist / f"svault-{tag}-x86_64-windows.exe"
    shutil.copyfile(ROOT / "target" / "dist-build" / "release" / "svault.exe", out)
    return out


def build_linux(dist: pathlib.Path, tag: str) -> pathlib.Path:
    out = dist / f"svault-{tag}-x86_64-linux"
    script = dist / "build-linux.sh"
    repo, out_wsl = wsl("wslpath", "-u", str(ROOT)), wsl("wslpath", "-u", str(out))
    script.write_bytes(
        "\n".join([
            "set -eu",
            'export PATH="$HOME/.cargo/bin:$PATH"',
            f"cd '{repo}'",
            f"export RUSTFLAGS=\"--remap-path-prefix=$HOME=~ --remap-path-prefix={repo}=svault\"",
            'CARGO_TARGET_DIR="$HOME/svault-dist-target" cargo build --release --locked --bin svault --features store',
            f"cp \"$HOME/svault-dist-target/release/svault\" '{out_wsl}'",
            "",
        ]).encode()
    )
    wsl("bash", wsl("wslpath", "-u", str(script)))
    script.unlink()
    return out


def verify(binary: pathlib.Path, version: str, needles: list[str]) -> None:
    data = binary.read_bytes().lower()
    leaks = {n: data.count(n.lower().encode()) for n in needles if n}
    if any(leaks.values()):
        sys.exit(f"{binary.name} 嵌着本机路径，拒绝发布：{ {k: v for k, v in leaks.items() if v} }")
    print(f"  {binary.name}: {version}；本机路径 0 处（查了 {len(leaks)} 种写法）")


def main() -> int:
    args = sys.argv[1:]
    if not args or args[0].startswith("-"):
        sys.exit(__doc__)
    tag, publish = args[0], "--publish" in args[1:]
    head = preflight(tag, publish)
    want = f"0.0.0 ({head[:12]})"
    dist = ROOT / "target" / "dist" / tag
    shutil.rmtree(dist, ignore_errors=True)
    dist.mkdir(parents=True)

    win, linux = build_windows(dist, tag), build_linux(dist, tag)
    win_version = sh(str(win), "--version").removeprefix("svault ")
    linux_version = wsl(wsl("wslpath", "-u", str(linux)), "--version").removeprefix("svault ")
    for name, got in (("Windows", win_version), ("Linux", linux_version)):
        if got != want:
            sys.exit(f"{name} 版 --version 是 {got!r}，应为 {want!r}")

    profile, wsl_home = os.environ["USERPROFILE"], wsl("printenv", "HOME")
    needles = [profile, profile.replace("\\", "/"), wsl_home, wsl("wslpath", "-u", profile)]
    verify(win, win_version, needles)
    verify(linux, linux_version, needles)

    lock = dist / f"svault-{tag}-Cargo.lock"
    shutil.copyfile(ROOT / "Cargo.lock", lock)
    assets = [win, linux, lock]
    sums = "".join(f"{hashlib.sha256(a.read_bytes()).hexdigest()}  {a.name}\n" for a in assets)
    (dist / "SHA256SUMS").write_text(sums, encoding="utf-8", newline="\n")
    print(f"产物在 {dist}：\n{sums}", end="")
    if not publish:
        print("试运行结束：没打标签、没发布、没安装。确认后加 --publish。")
        return 0

    notes = dist / "notes.md"
    notes.write_text(
        f"svault，提交 `{head}`。\n\n"
        f"- 校验：`sha256sum -c SHA256SUMS`；`svault --version` 应输出 `svault {want}`。\n"
        "- `-Cargo.lock` 是构建时用的依赖锁（本仓不入库），用于复现。\n"
        "- 无桌面 Linux 需设 `SVAULT_KEY_FILE`，见 README「主密钥从哪来」。\n",
        encoding="utf-8", newline="\n",
    )
    sh("git", "tag", "-a", tag, "-m", f"svault {tag}", head)
    sh("git", "push", "origin", tag)
    sh("gh", "release", "create", tag, *map(str, assets), str(dist / "SHA256SUMS"),
       "--title", f"svault {tag}", "--notes-file", str(notes), "--verify-tag")

    target = pathlib.Path(os.environ["LOCALAPPDATA"]) / "svault" / "bin" / "svault.exe"
    target.parent.mkdir(parents=True, exist_ok=True)
    tmp = target.with_suffix(".exe.new")
    shutil.copyfile(win, tmp)
    os.replace(tmp, target)
    installed = sh(str(target), "--version")
    print(f"已发布 {tag}；已安装 {target}：{installed}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
