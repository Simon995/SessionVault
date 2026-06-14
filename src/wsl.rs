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
pub fn list_distros() -> Result<Vec<String>, String> {
    use std::process::Command;

    let mut cmd = Command::new("wsl.exe");
    cmd.args(["-l", "-q"]);
    configure_no_window(&mut cmd);
    let output = cmd
        .output()
        .map_err(|e| format!("spawn wsl.exe failed: {e}"))?;

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
pub fn list_distros() -> Result<Vec<String>, String> {
    Ok(Vec::new())
}

/// 把 `script` 经 stdin 喂给发行版内的 `bash` 并取回 `Output`。
///
/// stdin 喂脚本是关键：wsl.exe 会用 Windows 侧环境预替换 argv 里的 `$VAR`，且
/// `find|while` 在 `bash -c` 下 drain 不到 fd0——脚本走 stdin 两者皆避。含 `$` 的脚本
/// 一律走这里。退出码由调用方判（含 exit-7 哨兵）。
#[cfg(windows)]
fn run_bash_stdin(distro: &str, script: &str) -> Result<std::process::Output, String> {
    use std::io::Write;
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
    child
        .wait_with_output()
        .map_err(|e| format!("wsl.exe wait failed: {e}"))
}

/// 列出发行版内 `$HOME/<rel_subpath>` 下的全部 `*.jsonl` 绝对路径（仅发现、不读内容）。
/// 目录不存在 → `Ok(vec![])`（脚本 `exit 0`）。
#[cfg(windows)]
pub fn list_jsonl_under_home(distro: &str, rel_subpath: &str) -> Result<Vec<String>, String> {
    let script = format!(
        "set -eu\nDIR=\"$HOME/{rel}\"\n[ -d \"$DIR\" ] || exit 0\nfind \"$DIR\" -type f -name '*.jsonl' -print0\n",
        rel = shell_escape(rel_subpath)
    );
    let output = run_bash_stdin(distro, &script)?;
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
pub fn list_jsonl_under_home(_distro: &str, _rel_subpath: &str) -> Result<Vec<String>, String> {
    Ok(Vec::new())
}

/// 取发行版内**绝对路径**文件的 `(size, mtime_secs)`；`Ok(None)` = 文件不存在（exit 7）。
/// 供增量扫描的 `(size,mtime)` 回退检测用（对应本地 `fs::metadata`）。
#[cfg(windows)]
pub fn stat(distro: &str, abs_path: &str) -> Result<Option<(u64, i64)>, String> {
    let esc = shell_escape(abs_path);
    let script = format!(
        "set -eu\nF=\"{esc}\"\n[ -f \"$F\" ] || exit 7\nprintf '%s\\t%s\\n' \"$(stat -c %Y \"$F\")\" \"$(stat -c %s \"$F\")\"\n"
    );
    let out = run_bash_stdin(distro, &script)?;
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

#[cfg(not(windows))]
pub fn stat(_distro: &str, _abs_path: &str) -> Result<Option<(u64, i64)>, String> {
    Err("wsl.exe access is only available on Windows builds".to_string())
}

/// 读发行版内绝对路径文件的字节区间 `[start, end)`（对应本地 `read_range`/`Seek`）。
///
/// `tail -c +K`（1-indexed）取 `[start, EOF)`，再 `head -c (end-start)` 截到 `end`——
/// append-only 文件下即精确 `[start, end)`。`end <= start` 直接空。
#[cfg(windows)]
pub fn read_range(distro: &str, abs_path: &str, start: u64, end: u64) -> Result<Vec<u8>, String> {
    if end <= start {
        return Ok(Vec::new());
    }
    let esc = shell_escape(abs_path);
    let from = start + 1; // tail -c + 是 1-indexed
    let take = end - start;
    let script = format!("set -eu\ntail -c +{from} \"{esc}\" | head -c {take}\n");
    let out = run_bash_stdin(distro, &script)?;
    if !out.status.success() {
        return Err(format!(
            "wsl read_range {distro}:{abs_path} exited {:?}: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(out.stdout)
}

#[cfg(not(windows))]
pub fn read_range(_distro: &str, _abs_path: &str, _start: u64, _end: u64) -> Result<Vec<u8>, String> {
    Err("wsl.exe access is only available on Windows builds".to_string())
}

/// 读发行版内**绝对路径**文件的全文。`Ok(None)` = 文件不存在（exit 7 哨兵），
/// 据此区分「该来源没跑过 CLI」与「wsl.exe 挂了」。
///
/// 绝对路径不含 `$`，故安全走 `bash -c`（无 stdin-pipe 需求）。
#[cfg(windows)]
pub fn read_file_at(distro: &str, abs_path: &str) -> Result<Option<String>, String> {
    use std::process::{Command, Stdio};

    let escaped = shell_escape(abs_path);
    let script = format!("[ -f \"{escaped}\" ] || exit 7\ncat \"{escaped}\"\n");

    let mut cmd = Command::new("wsl.exe");
    cmd.args(["-d", distro, "--", "bash", "-c", &script])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure_no_window(&mut cmd);

    let output = cmd
        .output()
        .map_err(|e| format!("spawn wsl.exe failed: {e}"))?;

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

#[cfg(not(windows))]
pub fn read_file_at(_distro: &str, _abs_path: &str) -> Result<Option<String>, String> {
    Err("wsl.exe access is only available on Windows builds".to_string())
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
#[cfg(any(windows, test))]
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
}
