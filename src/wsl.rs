//! WSL 访问桥（pathnorm 三层里的第②层，Windows 专属）。
//!
//! 动机（移植自 QuotaBar `wsl/mod.rs`）：用户在 Windows 上跑本程序，但 `claude`/`codex`
//! CLI 装在 WSL2 发行版里，JSONL 会话日志落在发行版的 ext4 上。Windows 进程能经
//! `\\wsl$\<distro>\…` UNC 访问，但 9P 协议遍历成百上千个小 `*.jsonl` 极慢
//! （一次 `~/.claude/projects/` 走查可 >10s）。所以重活（`find`/`cat`/`tail`）shell 进
//! `wsl.exe -d <distro> -- bash`，留在 Linux VM 内跑、只让字节过 VM 边界。
//!
//! # 分层（务必分清，呼应 `pathnorm`）
//! - **纯逻辑**（`#[cfg(any(windows, test))]`）：发行版名解析、UTF-16LE 解码、`find -print0`
//!   输出解析、默认发行版选择——无 I/O、可跨平台单测。
//! - **实时层**（`#[cfg(windows)]`）：真正 spawn `wsl.exe`。非 Windows 构建给桩
//!   （`list_*` 返回空、`read_*` 返回 Err），因为本程序若**跑在 WSL 内部**，`~` 已直接
//!   解析到对的位置，调用方走本地 FS 路径即可。
//!
//! # 移植时保留的硬教训（QuotaBar 踩过）
//! - `wsl.exe` 控制台输出是 **UTF-16LE**（带可选 BOM），要先解码。
//! - `wsl.exe -- bash -c '<script>'` 会用 **Windows 侧环境**预替换 argv 里的任何 `$VAR`
//!   （`$HOME` 在 Windows 为空 → bash 收到空串，静默失败）。规避：脚本经 **stdin 喂**给
//!   `bash`（wsl.exe 不碰 stdin），或全程不用 `$`。本模块凡含 `$HOME` 的脚本一律走 stdin。
//! - `find … -print0 | while read` 在 `bash -c` 下 fd0 drain 不到——同样用 stdin 喂脚本规避。

/// 过滤掉 Docker Desktop 的内部发行版，只留用户发行版。
pub fn is_user_distro(distro: &str) -> bool {
    !matches!(distro, "docker-desktop" | "docker-desktop-data")
}

/// 注入给 [`crate::pathnorm::normalize_cwd`] 的「默认发行版」：当且仅当**恰好一个**用户
/// 发行版时返回它；否则 `None`（多发行版无法武断归属裸 Linux 路径，零发行版无可归属）。
///
/// 移植自 QuotaBar `default_wsl_distro_for_bare_posix` 的单发行版启发。注意：WSL **来源**
/// 自身的 cwd 归属用的是该来源的发行版（见 `scan`），不依赖本启发；本启发只兜底
/// 「裸 Linux cwd 记在 local transcript 下」这种 distro 不明的边角。
pub fn default_distro(distros: &[String]) -> Option<String> {
    let mut users = distros.iter().filter(|d| is_user_distro(d));
    let first = users.next()?;
    if users.next().is_none() {
        Some(first.clone())
    } else {
        None
    }
}

// ───────────────────────────── 实时层（Windows 专属） ─────────────────────────────

/// 枚举已安装的 WSL 发行版（`wsl.exe -l -q`，按声明顺序）。
///
/// 非 Windows 构建返回 `Ok(vec![])`（静默——这是个发现调用，不该在 Linux/macOS dev 上
/// 用错误污染日志）。
#[cfg(windows)]
pub fn list_distros(deadline: crate::deadline::Deadline) -> Result<Vec<String>, String> {
    use std::process::Command;

    use std::process::Stdio;

    let mut cmd = Command::new("wsl.exe");
    cmd.args(["-l", "-q"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure_no_window(&mut cmd);
    // 🔴 **枚举发行版同样要有上限。** 它是每一轮发现的第一步，`output()` 没有超时 ——
    // 一个卡住的 WSL 会让整轮刷新停在这一行，比后面任何一次探测都更早、更彻底。
    // 修 `run_bash_stdin` 时漏掉它，实测让一次单元测试跑了 10 分钟没结束。
    let budget = deadline
        .budget_for(WSL_LIST_TIMEOUT)
        .ok_or_else(|| "wsl -l -q: round budget exhausted before the call".to_string())?;
    let output = wait_with_deadline(
        cmd.spawn()
            .map_err(|e| format!("spawn wsl.exe failed: {e}"))?,
        budget,
    )?;

    if !output.status.success() {
        let err = decode_utf16le(&output.stderr)
            .unwrap_or_else(|| String::from_utf8_lossy(&output.stderr).into_owned());
        return Err(format!(
            "wsl.exe -l -q exited {:?}: {}",
            output.status.code(),
            err.trim()
        ));
    }
    let text = decode_utf16le(&output.stdout)
        .unwrap_or_else(|| String::from_utf8_lossy(&output.stdout).into_owned());
    Ok(parse_distros(&text))
}

#[cfg(not(windows))]
pub fn list_distros(deadline: crate::deadline::Deadline) -> Result<Vec<String>, String> {
    let _ = deadline;
    Ok(Vec::new())
}

/// 把 `script` 经 stdin 喂给发行版内的 `bash` 并取回 `Output`。
///
/// stdin 喂脚本是关键：wsl.exe 会用 Windows 侧环境预替换 argv 里的 `$VAR`，且
/// `find|while` 在 `bash -c` 下 drain 不到 fd0——脚本走 stdin 两者皆避。含 `$` 的脚本
/// 一律走这里。退出码由调用方判（含 exit-7 哨兵）。
#[cfg(windows)]
fn run_bash_stdin(
    distro: &str,
    script: &str,
    deadline: crate::deadline::Deadline,
) -> Result<std::process::Output, String> {
    use std::io::Write;
    // 🔴 **预算耗尽就根本别 spawn**（ADR-051 §4）。传一个零上限进去只会白付一次
    // 进程创建的代价，还会在日志里留下一条看起来像「超时」的失败。
    let budget = deadline
        .budget_for(WSL_CALL_TIMEOUT)
        .ok_or_else(|| format!("wsl {distro}: round budget exhausted before the call"))?;
    use std::process::{Command, Stdio};

    let mut cmd = Command::new("wsl.exe");
    cmd.args(["-d", distro, "--", "bash"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure_no_window(&mut cmd);

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("spawn wsl.exe failed: {e}"))?;
    {
        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| "wsl.exe stdin pipe missing".to_string())?;
        stdin
            .write_all(script.as_bytes())
            .map_err(|e| format!("write to wsl.exe stdin failed: {e}"))?;
    }
    wait_with_deadline(child, budget)
}

/// 一次 `wsl.exe` 调用的上限。
///
/// 🔴 **不是为了「慢」，是为了「永远」。** 原先用 `wait_with_output()`：一个卡住的
/// WSL（内存耗尽、VM 半死、`E_UNEXPECTED`）会让调用方**无限期挂住** —— 而调用方是
/// 后台刷新循环，于是整轮扫描停在那里，连「有多少个候选探测失败」都写不进日志。
/// 实测判例：一次 WSL 卡死让 Codex 卡片红了 19 分钟（v0.8.0-beta.23）。
///
/// 值取得宽：`find` 遍历一棵大目录树本来就可能几十秒，超时不该把正常的慢判成故障。
const WSL_CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// 枚举发行版的上限。比 [`WSL_CALL_TIMEOUT`] 短得多：`wsl -l -q` 只读注册表，
/// 正常在 100ms 内返回；它慢就说明 WSL 服务本身有问题，等下去没有意义。
const WSL_LIST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// 等子进程结束，超时就杀掉并报错。
///
/// 手写而不用 `wait_with_output()`：后者没有超时。管道必须**在等待期间**被读走，
/// 否则子进程写满管道缓冲区就阻塞，形成一个我们自己造的死锁 —— 所以两个流各起一个
/// 读线程，主线程只轮询退出状态。
#[cfg(windows)]
fn wait_with_deadline(
    mut child: std::process::Child,
    timeout: std::time::Duration,
) -> Result<std::process::Output, String> {
    use std::io::Read;

    // 🔴 **先关 stdin。** 标准库的 `wait_with_output()` 第一件事就是
    // `drop(self.stdin.take())` —— 我手写替代它时漏了，于是 `bash` 一直等 stdin 的 EOF、
    // 等不到，**每一次调用都撞满 60 秒超时**。症状极具误导性：日志说
    // 「timed out (distro wedged?)」，而同一条 `find` 手工跑只要 0.6 秒 ——
    // 我差点据此断定 WSL 坏了、超时值太紧。
    //
    // 一般化：**替换一个标准库辅助函数时，先读它到底替你做了什么。**
    // 它的名字（"wait with output"）说不出「顺带关了 stdin」这件事。
    drop(child.stdin.take());

    let mut out_pipe = child.stdout.take();
    let mut err_pipe = child.stderr.take();
    let out_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(p) = out_pipe.as_mut() {
            let _ = p.read_to_end(&mut buf);
        }
        buf
    });
    let err_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(p) = err_pipe.as_mut() {
            let _ = p.read_to_end(&mut buf);
        }
        buf
    });

    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!(
                        "wsl.exe timed out after {}s (distro wedged?)",
                        timeout.as_secs()
                    ));
                }
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            Err(e) => return Err(format!("wsl.exe wait failed: {e}")),
        }
    };
    let stdout = out_handle.join().unwrap_or_default();
    let stderr = err_handle.join().unwrap_or_default();
    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

/// 列出发行版内 `$HOME/<rel_subpath>` 下的全部 `*.jsonl` 绝对路径（仅发现、不读内容）。
/// 目录不存在 → `Ok(vec![])`（脚本 `exit 0`）。
#[cfg(windows)]
pub fn list_jsonl_under_home(
    distro: &str,
    rel_subpath: &str,
    deadline: crate::deadline::Deadline,
) -> Result<Vec<String>, String> {
    list_files_under_home(distro, rel_subpath, ".jsonl", deadline)
}

/// 发行版内的 `$HOME`（POSIX 形式，如 `/home/simon`）。
///
/// 本模块其它函数都是「在 `$HOME` 下做点什么」，从不需要那个值本身。这一个需要：
/// 调用方要用它拼出宿主可达的 UNC 路径 `\\wsl.localhost\<distro>\home\simon`，
/// 而那条路**不再起 `wsl.exe`**（走 9P 共享），是每次读都省一次进程的关键。
///
/// 🔴 **`Err` 是「没问成」，不是「这个发行版没有 HOME」。** 两者的处置完全不同：
/// 前者意味着该发行版**本轮的答案不作数**（调用方必须报出去，见 `roots::enumerate`），
/// 后者不可能发生。返回 `Result` 而不是 `Option` 就是为了让这件事在类型上说得出口 ——
/// 上游 QuotaBar 那份曾是 `Option`，于是「WSL 卡住」与「没这个发行版」在调用点长得一样。
#[cfg(windows)]
pub fn home_of(distro: &str, deadline: crate::deadline::Deadline) -> Result<String, String> {
    // `printf` 而不是 `echo`：后者对以 `-` 开头的值会当成选项。
    // 不用 `$HOME` 之外的任何变量 —— 本模块头部记着 wsl.exe 传参会篡改 `$`。
    let output = run_bash_stdin(distro, "set -eu\nprintf '%s' \"$HOME\"\n", deadline)?;
    if !output.status.success() {
        return Err(format!(
            "wsl.exe -d {distro} $HOME exited {:?}: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let home = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if home.is_empty() || !home.starts_with('/') {
        // 空串是那个已知失败模式（`$HOME` 在某些调用形式下会变空），不是一个合法答案。
        return Err(format!(
            "wsl.exe -d {distro} returned a non-POSIX $HOME: {home:?}"
        ));
    }
    Ok(home)
}

/// 递归列出 `$HOME/<rel_subpath>` 下指定后缀的普通文件。后缀来自内置 catalog，
/// 统一走 `find -print0`，保留空格和非 ASCII 路径。
#[cfg(windows)]
pub fn list_files_under_home(
    distro: &str,
    rel_subpath: &str,
    suffix: &str,
    deadline: crate::deadline::Deadline,
) -> Result<Vec<String>, String> {
    let script = format!(
        "set -eu\nDIR=\"$HOME/{rel}\"\n[ -d \"$DIR\" ] || exit 0\nfind \"$DIR\" -type f -name \"{pattern}\" -print0\n",
        rel = shell_escape(rel_subpath),
        pattern = shell_escape(&format!("*{suffix}")),
    );
    let output = run_bash_stdin(distro, &script, deadline)?;
    if !output.status.success() {
        return Err(format!(
            "wsl.exe -d {distro} find exited {:?}: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(parse_nul_paths(&output.stdout))
}

#[cfg(not(windows))]
pub fn list_jsonl_under_home(
    _distro: &str,
    _rel_subpath: &str,
    _deadline: crate::deadline::Deadline,
) -> Result<Vec<String>, String> {
    Ok(Vec::new())
}

#[cfg(not(windows))]
pub fn list_files_under_home(
    _distro: &str,
    _rel_subpath: &str,
    _suffix: &str,
) -> Result<Vec<String>, String> {
    Ok(Vec::new())
}

/// 取发行版内**绝对路径**文件的 `(size, mtime_secs)`；`Ok(None)` = 文件不存在（exit 7）。
/// 供增量扫描的 `(size,mtime)` 回退检测用（对应本地 `fs::metadata`）。
#[cfg(windows)]
/// 沿 `abs_path` 的祖先链找项目根 —— **一次 `wsl.exe` 调用走完整条链**。
///
/// 返回 `Some((目录, "git" | "marker:<file>"))`，一路没找到则 `None`。
///
/// ## 🔴 为什么把循环放进 WSL 里
///
/// 从 Windows 侧逐级 `stat` 要 N 次 `wsl.exe` 调用（每次都是一次进程启动 + 跨 VM
/// 往返，实测单次约 0.1–0.3s）。一条 8 层深的路径就是 8 次 —— 而全库有 68 个
/// WSL 形式的 `project_root`。把 `while` 写进脚本，**一次调用解决一条链**。
///
/// ## 🔴 `.git` 优先，但只找**最近的那个**
///
/// 先整条链找 `.git`；找不到才回退到最近的构建 marker。这实现了 ADR-050 决定 5
/// （`.git` 优先于构建 marker），而**不引入**原 P3 那个「一路走到根会命中
/// `~/.git`（dotfiles 仓）」的风险 —— 因为路径上任何更近的 `.git` 都会先命中。
///
/// 唯一还能走到 home 的情况是「这条链上一个 `.git` 都没有」，所以脚本**显式排除
/// `$HOME` 本身**：把 home 当项目根，会让它名下每个散落目录都归到同一个「项目」。
///
/// `-e` 而不是 `-d`：子模块的 `.git` 是**文件**（`gitdir: …`），不是目录。
/// 发行版里 **Windows 盘的实际挂载表**：`(Linux 挂载点, Windows 路径)`。
///
/// 例：`[("/mnt/c", "C:\\"), ("/mnt/d", "D:\\")]`
///
/// ## 🔴 读 `mount`，不读 `wsl.conf`
///
/// 想把 `/mnt/d/proj` 换算成 `D:\proj`，需要知道「挂载点 ↔ 盘」的对应。
/// 一个自然的做法是读 `/etc/wsl.conf` 的 `[automount] root`，**而那有三个漏洞**：
///
/// 1. **配置可以不存在** —— 实测本机 `wsl.conf` 只有 `[boot] systemd=true`，
///    没有 `[automount]` 段。那时得靠「默认是 `/mnt/`」这个知识去推，而默认值
///    不是保证；
/// 2. **意图 ≠ 生效** —— 改了 `wsl.conf` 不 `wsl --shutdown` 就不生效，
///    于是配置说的和实际挂的不是一回事；
/// 3. **并非所有 `/mnt/x` 都是 Windows 盘** —— `/mnt/data` 可以是普通 Linux 挂载，
///    按盘符猜会把它误当成 `D:\`（而那个盘可能根本不存在）。
///
/// `mount` 是**运行期事实**，三个漏洞一起没有：
///
/// ```text
/// C:\ on /mnt/c type 9p (rw,noatime,aname=drvfs;path=C:\;uid=1000;…)
/// ```
///
/// 判据用 **device 形如 `<盘符>:\`**：那是 drvfs 挂载最稳的标记，且天然排除了
/// 非 Windows 挂载。`aname=drvfs` 也能用，但 WSL1/WSL2 的 fstype 不同
/// （`drvfs` vs `9p`），device 那一列两代一致。
///
/// 失败 ⇒ `Err`。调用方据此**不做映射**（那些路径照旧「说不出来」），
/// 而不是退回「猜 `/mnt/<字母>` 就是盘」。
#[cfg(windows)]
pub fn drive_mounts(
    distro: &str,
    deadline: crate::deadline::Deadline,
) -> Result<Vec<(String, String)>, String> {
    let out = run_bash_stdin(distro, "set -eu\nmount\n", deadline)?;
    if out.status.code() != Some(0) {
        return Err(format!(
            "wsl mount {distro} exited {:?}: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut mounts = Vec::new();
    for line in text.lines() {
        // `<device> on <mountpoint> type <fstype> (<options>)`
        let Some((device, rest)) = line.split_once(" on ") else {
            continue;
        };
        let Some((mountpoint, _)) = rest.split_once(" type ") else {
            continue;
        };
        let device = device.trim();
        let mountpoint = mountpoint.trim();
        if !is_windows_drive_device(device) || mountpoint.is_empty() {
            continue;
        }
        mounts.push((mountpoint.to_string(), device.to_string()));
    }
    // 最长挂载点优先 —— `/mnt/c/sub` 若也是个挂载点，它比 `/mnt/c` 更精确。
    mounts.sort_by_key(|m| std::cmp::Reverse(m.0.len()));
    Ok(mounts)
}

#[cfg(not(windows))]
pub fn drive_mounts(
    _distro: &str,
    _deadline: crate::deadline::Deadline,
) -> Result<Vec<(String, String)>, String> {
    Err("wsl.exe access is only available on Windows builds".to_string())
}

/// device 是不是 Windows 盘根（`C:\` / `D:\` / 少数场景下无尾斜杠的 `C:`）。
///
/// 纯函数，好让上面那个解析在没有 WSL 的机器上也能测。
pub fn is_windows_drive_device(device: &str) -> bool {
    let b = device.as_bytes();
    match b.len() {
        2 => b[0].is_ascii_alphabetic() && b[1] == b':',
        3 => b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/'),
        _ => false,
    }
}

/// 归属脚本的构造 —— **抽出来是为了能被测到**。
///
/// 内联在 `find_project_root` 里时，「marker 列表是不是真的来自 `MARKERS`」
/// 只能靠读代码确认，而那正是它上一版硬抄一份、无人发现的原因。
fn find_project_root_script(abs_path: &str) -> String {
    let esc = shell_escape(abs_path);
    // 🔴 **marker 列表由 `MARKERS` 生成，不再手抄。**
    //
    // 这里从前硬编码着 `Cargo.toml package.json pyproject.toml go.mod .hg` ——
    // `project_root::MARKERS` 的一份副本。两份都活着的时候，往 `MARKERS` 里加一个
    // 新 marker 不会有任何东西报错：本机路径认得它，WSL 里的同一个项目认不得，
    // 症状是「同一个项目在 Windows 侧是一个根、在 WSL 侧不是」——而这正是本仓
    // 刚为「同一个项目两个身份」付过代价的形状。
    //
    // `.git` 不在这一串里：它由下面**第一遍**单独走（优先级高于所有 marker，
    // 且 `-e` 要覆盖子模块那种 `.git` 是文件的情形）。
    let markers = crate::project_root::MARKERS
        .iter()
        .filter(|m| **m != ".git")
        .map(|m| shell_escape(m))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        r#"set -eu
D="{esc}"
[ -n "$D" ] || exit 7
# 第一遍：最近的 .git（-e：子模块的 .git 是文件）
P="$D"
while [ "$P" != "/" ] && [ -n "$P" ]; do
  if [ "$P" != "$HOME" ] && [ -e "$P/.git" ]; then
    printf 'git	%s
' "$P"
    exit 0
  fi
  P=$(dirname "$P")
done
# 第二遍：最近的构建 marker（顺序即优先级，列表由 `project_root::MARKERS` 生成）
P="$D"
while [ "$P" != "/" ] && [ -n "$P" ]; do
  for M in {markers}; do
    if [ "$P" != "$HOME" ] && [ -e "$P/$M" ]; then
      printf 'marker:%s	%s
' "$M" "$P"
      exit 0
    fi
  done
  P=$(dirname "$P")
done
exit 7
"#
    )
}

#[cfg(windows)]
pub fn find_project_root(
    distro: &str,
    abs_path: &str,
    deadline: crate::deadline::Deadline,
) -> Result<Option<(String, String)>, String> {
    let script = find_project_root_script(abs_path);
    let out = run_bash_stdin(distro, &script, deadline)?;
    match out.status.code() {
        Some(0) => {
            let text = String::from_utf8_lossy(&out.stdout);
            let line = text.trim();
            let (kind, dir) = line.split_once('\t').ok_or_else(|| {
                format!("wsl find_project_root {distro}:{abs_path} bad output: {line:?}")
            })?;
            Ok(Some((dir.to_string(), kind.to_string())))
        }
        // 7 = 这条链上没有项目根。**不是错误** —— 归属会据此报 Unattributed。
        Some(7) => Ok(None),
        other => Err(format!(
            "wsl find_project_root {distro}:{abs_path} exited {other:?}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )),
    }
}

#[cfg(windows)]
pub fn stat(
    distro: &str,
    abs_path: &str,
    deadline: crate::deadline::Deadline,
) -> Result<Option<(u64, i64)>, String> {
    let esc = shell_escape(abs_path);
    let script = format!(
        "set -eu\nF=\"{esc}\"\n[ -f \"$F\" ] || exit 7\nprintf '%s\\t%s\\n' \"$(stat -c %Y \"$F\")\" \"$(stat -c %s \"$F\")\"\n"
    );
    let out = run_bash_stdin(distro, &script, deadline)?;
    match out.status.code() {
        Some(0) => {
            let text = String::from_utf8_lossy(&out.stdout);
            let line = text.trim();
            let (m, s) = line
                .split_once('\t')
                .ok_or_else(|| format!("wsl stat {distro}:{abs_path} bad output: {line:?}"))?;
            let mtime = m
                .trim()
                .parse::<i64>()
                .map_err(|e| format!("wsl stat bad mtime {m:?}: {e}"))?;
            let size = s
                .trim()
                .parse::<u64>()
                .map_err(|e| format!("wsl stat bad size {s:?}: {e}"))?;
            Ok(Some((size, mtime)))
        }
        Some(7) => Ok(None),
        Some(code) => Err(format!(
            "wsl stat {distro}:{abs_path} exited {code}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )),
        None => Err(format!("wsl stat {distro}:{abs_path} terminated by signal")),
    }
}

/// 发行版内一条路径上**有什么东西**。
///
/// 🔴 **与 [`stat`] 的分工是「问什么」，不是「怎么问」。** 那个问 `[ -f ]` ——
/// 它要的是 mtime/size，而这两样只对普通文件有意义。拿它去答「这里有没有目录」，
/// **每一个目录都会被报成「不存在」**：`WslBackend` 的文档从第一天就写着这条边界
/// （「要问『这里有没有目录』的调用方必须先扩访问桥」），本函数就是它说的那次扩。
///
/// `[ -L ]` 那一支不能省：一个**断掉的**符号链接下 `[ -e ]` 为假，而那条路径上
/// 确实有东西。把它报成「没有」，就又造了一个「没问成长得像这里是空的」。
///
/// 退出码即三态：`0` = 有（stdout 是 `dir` / `file` / `other`），`7` = 确认没有，
/// 其余 = 没问成。**「没问成」绝不折进「没有」。**
#[cfg(windows)]
pub fn stat_kind(
    distro: &str,
    abs_path: &str,
    deadline: crate::deadline::Deadline,
) -> Result<Option<PathKind>, String> {
    let esc = shell_escape(abs_path);
    let script = format!(
        "set -eu\nF=\"{esc}\"\n\
         if [ -d \"$F\" ]; then printf 'dir\\n'\n\
         elif [ -f \"$F\" ]; then printf 'file\\n'\n\
         elif [ -e \"$F\" ] || [ -L \"$F\" ]; then printf 'other\\n'\n\
         else exit 7\n\
         fi\n"
    );
    let out = run_bash_stdin(distro, &script, deadline)?;
    match out.status.code() {
        Some(0) => {
            let text = String::from_utf8_lossy(&out.stdout);
            match text.trim() {
                "dir" => Ok(Some(PathKind::Dir)),
                "file" => Ok(Some(PathKind::File)),
                "other" => Ok(Some(PathKind::Other)),
                // 认不出来的输出不是「没有」—— 脚本被谁改坏了也是「没问成」。
                other => Err(format!(
                    "wsl stat_kind {distro}:{abs_path} bad output: {other:?}"
                )),
            }
        }
        Some(7) => Ok(None),
        Some(code) => Err(format!(
            "wsl stat_kind {distro}:{abs_path} exited {code}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )),
        None => Err(format!(
            "wsl stat_kind {distro}:{abs_path} terminated by signal"
        )),
    }
}

/// [`stat_kind`] 的答案。**不是** `probe::FileKind` —— 本模块在它下面一层
/// （`probe.rs` 调用 `wsl.rs`，反向依赖会成环），翻译由 `probe.rs` 做。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathKind {
    Dir,
    File,
    /// 符号链接（含指向宿主看不见处的、以及断掉的）、设备文件、FIFO……
    Other,
}

// 🔴 **非 Windows 的桩曾经是「第二份签名」，而开发机永远不编译它**
// （2026-08-14 CI 实测：`stat` 的真实现丢了 `#[cfg(windows)]` ⇒ 与桩重复定义；
// `list_distros` / `find_project_root` 的桩少了 `deadline` 参数 —— 三处都是加参数时
// 只改了看得见的那一半）。这与本仓反复栽的「两份实现只跑一份」同族，只是这次
// **那一份连编译都没编译过**。
//
// 现在桩由 `stub_on_non_windows!` 生成：**签名只写一次**，真实现与桩共用同一行 ——
// 漂移在类型上表达不出来。要加参数就得改那一行，两边同时跟着走。
#[cfg(not(windows))]
macro_rules! stub_on_non_windows {
    ($( $(#[$m:meta])* pub fn $name:ident ( $($arg:ident : $ty:ty),* $(,)? ) -> $ret:ty ; )+) => {
        $(
            $(#[$m])*
            pub fn $name($($arg : $ty),*) -> $ret {
                $( let _ = $arg; )*
                Err("wsl.exe access is only available on Windows builds".to_string())
            }
        )+
    };
}

#[cfg(not(windows))]
stub_on_non_windows! {
    pub fn home_of(
        distro: &str,
        deadline: crate::deadline::Deadline,
    ) -> Result<String, String>;

    pub fn stat(
        distro: &str,
        abs_path: &str,
        deadline: crate::deadline::Deadline,
    ) -> Result<Option<(u64, i64)>, String>;

    pub fn stat_kind(
        distro: &str,
        abs_path: &str,
        deadline: crate::deadline::Deadline,
    ) -> Result<Option<PathKind>, String>;

    pub fn find_project_root(
        distro: &str,
        abs_path: &str,
        deadline: crate::deadline::Deadline,
    ) -> Result<Option<(String, String)>, String>;

    pub fn read_range(
        distro: &str,
        abs_path: &str,
        start: u64,
        end: u64,
        deadline: crate::deadline::Deadline,
    ) -> Result<Vec<u8>, String>;

    pub fn read_file_at(
        distro: &str,
        abs_path: &str,
        deadline: crate::deadline::Deadline,
    ) -> Result<Option<String>, String>;

    pub fn existing_files(
        distro: &str,
        paths: &[String],
    ) -> Result<std::collections::HashSet<String>, String>;
}

/// 读发行版内绝对路径文件的字节区间 `[start, end)`（对应本地 `read_range`/`Seek`）。
///
/// `tail -c +K`（1-indexed）取 `[start, EOF)`，再 `head -c (end-start)` 截到 `end`——
/// append-only 文件下即精确 `[start, end)`。`end <= start` 直接空。
#[cfg(windows)]
pub fn read_range(
    distro: &str,
    abs_path: &str,
    start: u64,
    end: u64,
    deadline: crate::deadline::Deadline,
) -> Result<Vec<u8>, String> {
    if end <= start {
        return Ok(Vec::new());
    }
    let esc = shell_escape(abs_path);
    let from = start + 1; // tail -c + 是 1-indexed
    let take = end - start;
    let script = format!("set -eu\ntail -c +{from} \"{esc}\" | head -c {take}\n");
    let out = run_bash_stdin(distro, &script, deadline)?;
    if !out.status.success() {
        return Err(format!(
            "wsl read_range {distro}:{abs_path} exited {:?}: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    // 必须拿到恰好 `take` 字节，与本地 `read_exact` 同语义：若文件在 stat 与读取之间被
    // 截断/轮转、或 start 已越过新 EOF，`tail|head` 仍会成功退出但少返回字节——直接返回
    // 短读则上层会按**旧 size** 推进 safe_offset 而跳过数据。故短读即 Err，让上层不前进、
    // 下轮重 stat 检测回退、从头重读。
    let got = out.stdout.len() as u64;
    if got != take {
        return Err(format!(
            "wsl read_range {distro}:{abs_path} short read: got {got} want {take} (truncated/rotated between stat and read?)"
        ));
    }
    Ok(out.stdout)
}

/// 读发行版内**绝对路径**文件的全文。`Ok(None)` = 文件不存在（exit 7 哨兵），
/// 据此区分「该来源没跑过 CLI」与「wsl.exe 挂了」。
///
/// 绝对路径不含 `$`，故安全走 `bash -c`（无 stdin-pipe 需求）。
#[cfg(windows)]
pub fn read_file_at(
    distro: &str,
    abs_path: &str,
    deadline: crate::deadline::Deadline,
) -> Result<Option<String>, String> {
    use std::process::{Command, Stdio};

    let escaped = shell_escape(abs_path);
    let script = format!("[ -f \"{escaped}\" ] || exit 7\ncat \"{escaped}\"\n");

    let mut cmd = Command::new("wsl.exe");
    cmd.args(["-d", distro, "--", "bash", "-c", &script])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure_no_window(&mut cmd);

    // 🔴 与 `run_bash_stdin` / `list_distros` 同一条：**`.output()` 没有上限**。
    // 快照全文读走这里（`scan_snapshot`），一个卡死的 WSL 会让 `svault scan-all`
    // 无限期挂住。评审 [P2] 指出前两处改了、这处漏了 —— 加超时这件事必须**逐个
    // 出站调用**核，不能核到「主要那条路径」为止。
    let budget = deadline
        .budget_for(WSL_CALL_TIMEOUT)
        .ok_or_else(|| format!("wsl read {distro}: round budget exhausted before the call"))?;
    let output = wait_with_deadline(
        cmd.spawn()
            .map_err(|e| format!("spawn wsl.exe failed: {e}"))?,
        budget,
    )?;

    match output.status.code() {
        Some(0) => String::from_utf8(output.stdout)
            .map(Some)
            .map_err(|e| format!("wsl read {distro}:{abs_path} not valid UTF-8: {e}")),
        Some(7) => Ok(None),
        Some(code) => Err(format!(
            "wsl read {distro}:{abs_path} exited {code}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )),
        None => Err(format!("wsl read {distro}:{abs_path} terminated by signal")),
    }
}

/// 一次 WSL 进程批量探测多条绝对路径，避免最新快照查询为每个文件各 spawn
/// 一个 `wsl.exe`。返回实际存在的原始路径集合。
#[cfg(windows)]
pub fn existing_files(
    distro: &str,
    paths: &[String],
) -> Result<std::collections::HashSet<String>, String> {
    let mut script = String::from("set -eu\n");
    for path in paths {
        let escaped = shell_escape(path);
        script.push_str(&format!(
            "[ ! -f \"{escaped}\" ] || printf '%s\\0' \"{escaped}\"\n"
        ));
    }
    let output = run_bash_stdin(distro, &script, crate::deadline::Deadline::unbounded())?;
    if !output.status.success() {
        return Err(format!(
            "wsl batch exists {distro} exited {:?}: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(parse_nul_paths(&output.stdout).into_iter().collect())
}

/// 在 Windows 上给 spawn 打 `CREATE_NO_WINDOW`，避免 GUI 宿主弹出闪烁的控制台窗口。
/// 移植自 QuotaBar `utils::process::configure_no_window`（硬编码常量，不为一个常量拉
/// windows-sys）。
fn configure_no_window(cmd: &mut std::process::Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    let _ = cmd;
}

// ───────────────────────────── 纯逻辑（可跨平台单测） ─────────────────────────────

/// 解码 `wsl.exe` 控制台输出的 UTF-16LE 字节为 String；长度非偶或含非法代理对返回 `None`
/// （调用方回落 lossy UTF-8）。剥除可选 BOM（0xFF 0xFE）。
#[cfg(any(windows, test))]
fn decode_utf16le(bytes: &[u8]) -> Option<String> {
    let trimmed = if bytes.starts_with(&[0xFF, 0xFE]) {
        &bytes[2..]
    } else {
        bytes
    };
    if trimmed.len() % 2 != 0 {
        return None;
    }
    let units: Vec<u16> = trimmed
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    String::from_utf16(&units).ok()
}

/// 解析 `wsl.exe -l -q` 纯文本为去重发行版列表。跳空行、滤 NUL/控制字符
/// （wsl.exe 偶尔在字形间夹 NUL）。
#[cfg(any(windows, test))]
fn parse_distros(text: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for raw in text.lines() {
        let cleaned: String = raw
            .chars()
            .filter(|c| *c != '\0' && !c.is_control())
            .collect();
        let trimmed = cleaned.trim();
        if trimmed.is_empty() {
            continue;
        }
        if seen.insert(trimmed.to_string()) {
            out.push(trimmed.to_string());
        }
    }
    out
}

/// 解析 `find -print0` 的 NUL 分隔输出为路径列表（去空、UTF-8 lossy）。
#[cfg(any(windows, test))]
fn parse_nul_paths(bytes: &[u8]) -> Vec<String> {
    bytes
        .split(|b| *b == 0)
        .filter_map(|chunk| {
            if chunk.is_empty() {
                return None;
            }
            let s = String::from_utf8_lossy(chunk);
            let t = s.trim();
            if t.is_empty() {
                None
            } else {
                Some(t.to_string())
            }
        })
        .collect()
}

/// 为嵌入双引号 bash 串转义 segment：只转义在双引号内仍生效的四个字符（`\ " $ ` `）。
/// 输入是本 crate 自己的常量/已发现路径，非不可信粘贴，故防御性足够。
// 纯字符串变换 —— **不能** gate 到 windows：`find_project_root_script`（无 cfg，
// 为了可测而抽出）在 Linux 构建里也要用它。
fn shell_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' | '"' | '$' | '`' => {
                out.push('\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
// 测试要造 fixture（建目录、写文件、再核一遍），允许直接碰盘 —— 文件系统边界
// 管的是**生产行为**，而 `#[cfg(test)]` 不在生产路径上。
#[allow(clippy::disallowed_methods)]
mod marker_list_tests {
    /// 🔴 **归属脚本的 marker 列表必须来自 `MARKERS`，不能是手抄的第二份。**
    ///
    /// 它曾硬编码着 `Cargo.toml package.json pyproject.toml go.mod .hg`。两份都活着
    /// 的时候，往 `MARKERS` 里加一个新 marker **不会有任何东西报错** —— 本机路径
    /// 认得它、WSL 里的同一个项目认不得，症状是「同一个项目在 Windows 侧是一个根、
    /// 在 WSL 侧不是」。本仓刚为「同一个项目两个身份」付过一次代价。
    ///
    /// 判据按**内容**写，不按当下这八个的拼写 —— 否则下次加 marker 时它自己就成了
    /// 第三份要维护的清单。
    ///
    /// ⚠️ 只在 Windows 上跑：脚本构造本身与平台无关，但它的宿主函数是
    /// `#[cfg(windows)]` 的（`wsl.exe` 只在那儿）。
    #[test]
    #[cfg(windows)]
    fn the_attribution_script_takes_its_markers_from_the_one_list() {
        let script = super::find_project_root_script("/home/u/proj");
        for m in crate::project_root::MARKERS {
            if m == ".git" {
                // `.git` 由第一遍单独走，且优先级高于所有 marker。
                assert!(
                    script.contains(r#"[ -e "$P/.git" ]"#),
                    "第一遍必须单独探 .git（它全局优先于 marker）"
                );
                continue;
            }
            assert!(
                script.contains(m),
                "`MARKERS` 里的 {m:?} 没进 WSL 归属脚本 —— \
                 同一个项目会在 Windows 侧是根、在 WSL 侧不是，而不会有任何东西报错"
            );
        }
    }
}

#[cfg(test)]
// 测试要造 fixture（建目录、写文件、再核一遍），允许直接碰盘 —— 文件系统边界
// 管的是**生产行为**，而 `#[cfg(test)]` 不在生产路径上。
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    #[test]
    fn is_user_distro_filters_docker() {
        assert!(is_user_distro("Ubuntu-22.04"));
        assert!(is_user_distro("Debian"));
        assert!(!is_user_distro("docker-desktop"));
        assert!(!is_user_distro("docker-desktop-data"));
    }

    #[test]
    fn default_distro_single_user_distro_only() {
        assert_eq!(
            default_distro(&["Ubuntu".to_string()]),
            Some("Ubuntu".to_string())
        );
        // docker 内部发行版不计入：只剩一个用户发行版 → 仍返回它。
        assert_eq!(
            default_distro(&["docker-desktop".to_string(), "Ubuntu".to_string()]),
            Some("Ubuntu".to_string())
        );
        // 多个用户发行版 → None（不武断）。
        assert_eq!(
            default_distro(&["Ubuntu".to_string(), "Debian".to_string()]),
            None
        );
        // 零发行版 → None。
        assert_eq!(default_distro(&[]), None);
    }

    #[test]
    fn decode_utf16le_strips_bom_and_rejects_odd() {
        assert_eq!(
            decode_utf16le(b"\xff\xfeU\x00b\x00u\x00n\x00t\x00u\x00").unwrap(),
            "Ubuntu"
        );
        assert_eq!(
            decode_utf16le(b"U\x00b\x00u\x00n\x00t\x00u\x00").unwrap(),
            "Ubuntu"
        );
        assert!(decode_utf16le(b"\x00\x00\x00").is_none());
    }

    #[test]
    fn parse_distros_dedupes_trims_and_strips_nul() {
        assert_eq!(
            parse_distros("Ubuntu-22.04\r\nDebian\r\n\r\nUbuntu-22.04\r\n"),
            vec!["Ubuntu-22.04", "Debian"]
        );
        assert!(parse_distros("\r\n\r\n").is_empty());
        assert_eq!(
            parse_distros("Ubu\0ntu\0\nDeb\0ian\n"),
            vec!["Ubuntu", "Debian"]
        );
    }

    #[test]
    fn parse_nul_paths_splits_and_drops_empty() {
        let bytes = b"/home/u/.claude/projects/a/s1.jsonl\0/home/u/.claude/projects/b/s2.jsonl\0";
        assert_eq!(
            parse_nul_paths(bytes),
            vec![
                "/home/u/.claude/projects/a/s1.jsonl",
                "/home/u/.claude/projects/b/s2.jsonl"
            ]
        );
        assert!(parse_nul_paths(b"").is_empty());
        assert!(parse_nul_paths(b"\0\0").is_empty());
    }

    #[test]
    fn shell_escape_escapes_double_quote_expansion_chars() {
        assert_eq!(shell_escape(".claude/projects"), ".claude/projects");
        assert_eq!(shell_escape(r#"a"b"#), r#"a\"b"#);
        assert_eq!(shell_escape("$HOME/x"), "\\$HOME/x");
        assert_eq!(shell_escape("a`b\\c"), "a\\`b\\\\c");
    }

    /// 实机集成测试：验证 `stat` + `read_range`（含 `start>0` 增量 tail）打通真实
    /// `wsl.exe`。用 `/tmp` 一次性文件（**非**会话数据），用完即删。
    ///
    /// 默认跳过——需 Windows+WSL 实机且置 `SVAULT_WSL_IT=1` 才跑（普通 `cargo test`
    /// 不 spawn wsl.exe）。这正是冷扫覆盖不到的 `read_range(start>0)` 支线的兜底验证。
    #[test]
    #[cfg(windows)]
    fn wsl_stat_and_read_range_roundtrip_it() {
        if std::env::var("SVAULT_WSL_IT").is_err() {
            return;
        }
        let distros = list_distros(crate::deadline::Deadline::unbounded()).expect("list_distros");
        let distro = distros
            .iter()
            .find(|d| is_user_distro(d))
            .expect("need at least one user distro");

        let path = "/tmp/svault-it-roundtrip.txt";
        // 写 18 字节："line1\nline2\nline3\n"。
        run_bash_stdin(
            distro,
            &format!("set -eu\nprintf 'line1\\nline2\\nline3\\n' > {path}\n"),
            crate::deadline::Deadline::unbounded(),
        )
        .expect("write throwaway file");

        let (size, mtime) = stat(distro, path, crate::deadline::Deadline::unbounded())
            .expect("stat ok")
            .expect("file exists");
        assert_eq!(size, 18, "size mismatch");
        assert!(mtime > 0, "mtime should be a real epoch second");

        // 全读 [0, size)。
        let full = read_range(
            distro,
            path,
            0,
            size,
            crate::deadline::Deadline::unbounded(),
        )
        .expect("full read");
        assert_eq!(full, b"line1\nline2\nline3\n");

        // 增量 tail [6, size)：跳过 "line1\n"，得 "line2\nline3\n"（12 字节）。
        let tail = read_range(
            distro,
            path,
            6,
            size,
            crate::deadline::Deadline::unbounded(),
        )
        .expect("tail read");
        assert_eq!(tail, b"line2\nline3\n", "read_range(start>0) wrong");

        // 区间到中段 [6, 12)：恰 "line2\n"。
        let mid = read_range(distro, path, 6, 12, crate::deadline::Deadline::unbounded())
            .expect("mid read");
        assert_eq!(mid, b"line2\n", "bounded read_range wrong");

        // 短读必须报错（P1 修复）：请求超出文件实际字节（截断/轮转/越过 EOF 的模拟）→ Err，
        // 与本地 read_exact 同语义，绝不静默返回少于 take 的字节。
        let short = read_range(
            distro,
            path,
            0,
            1000,
            crate::deadline::Deadline::unbounded(),
        );
        assert!(
            short.is_err(),
            "read_range asking 1000B from an 18B file must Err (short read), got {short:?}"
        );

        // 不存在文件 → stat 返回 None（exit-7 哨兵）。
        assert!(
            stat(
                distro,
                "/tmp/svault-it-does-not-exist-xyz.txt",
                crate::deadline::Deadline::unbounded()
            )
            .expect("stat missing ok")
            .is_none(),
            "missing file should stat to None"
        );

        let _ = run_bash_stdin(
            distro,
            &format!("rm -f {path}\n"),
            crate::deadline::Deadline::unbounded(),
        );
    }
}
