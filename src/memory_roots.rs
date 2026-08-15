//! 「这台机器上有哪些记忆根」—— **这条判据的唯一实现**。
//!
//! 一个记忆根 = 一个 agent 的 home 对（`~/.claude` + `~/.codex`），加上宿主到它的
//! 文件系统前缀。Windows 上有两种：本机 `%USERPROFILE%`，以及每个 WSL 发行版经
//! `\\wsl.localhost\<distro>` 的 9P 共享。
//!
//! # 为什么它在 SessionVault 而不是在消费方
//!
//! 消费方有两个：QuotaBar（Rust，直链本 crate）与 TumeFlow（Python，只能走
//! `svault` 子命令）。这条规则此前**只在 QuotaBar 里有一份**，而 TumeFlow 因此
//! 根本枚举不了 —— 它的 `_resolve_roots` 在拿不到宿主传参时**回落到自己那一个
//! local 根**，于是「这台机器只有本机根」与「宿主没告诉我」在调用点长得一模一样，
//! WSL 里的记忆被静默漏掉。
//!
//! 收口到这里，两个消费方问的是同一个东西 —— 与 `project_dir::decode_project_dir`
//! 和 `identity::repo_id_for_root` 同一个模式（Rust 直调、Python 经 CLI）。
//!
//! # 🔴 三态：`unreachable` 不是「那里没有根」
//!
//! 枚举会失败，而失败必须说得出口：`wsl.exe` 卡住、发行版没起来、`$HOME` 解析不出。
//! 上游那份是 `list_distros().unwrap_or_default()` —— **一个卡死的 WSL 与一台没装
//! WSL 的机器返回完全相同的东西**。调用方据此做的每个决定都建立在一个它无法察觉
//! 的假设上（QuotaBar 的 prune 曾因同类形状删掉 369 个文件的会话）。
//!
//! 所以 [`Enumeration`] 把两者分开，且**每个位置各自报**：一个发行版问不到，
//! 不该让别的发行版的答案也作废。

use crate::deadline::Deadline;

/// 一个记忆根，形状与消费方的 `SourceRoot` 对齐（ADR-033）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MemoryRoot {
    /// 宿主可打开的 `~/.claude`。
    pub claude_home: String,
    /// 宿主可打开的 `~/.codex`。
    pub codex_home: String,
    /// 打在这个根产出的每条记录上的来源标签：`local` / `wsl-<distro>`。
    pub location: String,
    /// 宿主 → 根的文件系统前缀。本机为空串；WSL 为 `\\wsl.localhost\<distro>`。
    ///
    /// 解码探测与 sidecar 吐出的 `project_path` 都经它，**这正是一个 Windows 宿主
    /// 能找到 WSL 项目的原因**。
    pub fs_prefix: String,
}

/// 某个位置没问成。**调用方不得把它读作「那里没有根」。**
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UnreachableRoot {
    /// `wsl` 整体没问成时为 `wsl`；单个发行版没问成时为 `wsl-<distro>`。
    pub location: String,
    pub reason: String,
}

/// 枚举结果：问到的 + **没问成的**。
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Enumeration {
    pub roots: Vec<MemoryRoot>,
    /// 空 = 全部位置都问到了。非空 = 这些位置**本轮的答案不作数**。
    pub unreachable: Vec<UnreachableRoot>,
}

/// 本机 `%USERPROFILE%` 那个根。`None` = 环境变量不在（不是失败，是无从谈起）。
pub fn local_root(userprofile: Option<&str>) -> Option<MemoryRoot> {
    let home = userprofile?;
    if home.trim().is_empty() {
        return None;
    }
    Some(MemoryRoot {
        claude_home: format!("{home}\\.claude"),
        codex_home: format!("{home}\\.codex"),
        location: "local".into(),
        fs_prefix: String::new(),
    })
}

/// 一个 WSL 发行版的根。`wsl_home` 是发行版内的 POSIX `$HOME`。
///
/// 9P 共享把它暴露在 `\\wsl.localhost\<distro>\home\<user>` —— 走这条路读文件
/// **不再起 `wsl.exe`**，所以这一次 `$HOME` 解析的代价换来后续每次读都省一个进程。
pub fn wsl_root(distro: &str, wsl_home: &str) -> MemoryRoot {
    let base = format!("\\\\wsl.localhost\\{distro}");
    let home = format!("{base}{}", wsl_home.replace('/', "\\"));
    MemoryRoot {
        claude_home: format!("{home}\\.claude"),
        codex_home: format!("{home}\\.codex"),
        location: format!("wsl-{distro}"),
        fs_prefix: base,
    }
}

/// 枚举这台机器上的全部记忆根。
///
/// `deadline` 是**整轮**预算，原样传下去。
///
/// ⚠️ **这里不再自己切一份。** `wsl.rs` 的每个出站调用内部已经 `budget_for` 并在
/// 耗尽时返回 `Err`（ADR-051 §4：耗尽 ⇒ 根本不发起，而不是发一个立刻被杀的进程）。
/// 在这一层再切一次就是同一判据的第二处实现 —— 而它会与 `wsl.rs` 里的上限**各自
/// 演化**，最后没人说得清一次调用到底有多少时间。预算耗尽因此表现为一条带原因的
/// `unreachable`，与「WSL 卡住」走同一条路，正合适。
#[cfg(windows)]
pub fn enumerate(userprofile: Option<&str>, deadline: Deadline) -> Enumeration {
    let mut out = Enumeration::default();
    out.roots.extend(local_root(userprofile));

    let distros = match crate::wsl::list_distros(deadline) {
        Ok(d) => d,
        Err(why) => {
            // 🔴 整个 WSL 侧没问成 —— 报出来，不是返回一个更短的列表。
            out.unreachable.push(UnreachableRoot {
                location: "wsl".into(),
                reason: why,
            });
            return out;
        }
    };

    for distro in distros {
        let d = distro.trim();
        if d.is_empty() || !crate::wsl::is_user_distro(d) {
            continue;
        }
        match crate::wsl::home_of(d, deadline) {
            Ok(home) => out.roots.push(wsl_root(d, &home)),
            // 这一个发行版本轮不作数；别的发行版的答案仍然有效。
            Err(why) => out.unreachable.push(UnreachableRoot {
                location: format!("wsl-{d}"),
                reason: why,
            }),
        }
    }
    out
}

#[cfg(not(windows))]
pub fn enumerate(userprofile: Option<&str>, deadline: Deadline) -> Enumeration {
    let _ = deadline;
    // 非 Windows：没有 `\\wsl.localhost` 这条路，本机根就是全部。
    // 不报 `unreachable` —— 这里**没有**「问不到」，是压根不存在这个位置。
    Enumeration {
        roots: local_root(userprofile).into_iter().collect(),
        unreachable: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_local_root_points_at_the_two_agent_homes() {
        let r = local_root(Some(r"C:\Users\u")).expect("userprofile present");
        assert_eq!(r.claude_home, r"C:\Users\u\.claude");
        assert_eq!(r.codex_home, r"C:\Users\u\.codex");
        assert_eq!(r.location, "local");
        assert_eq!(r.fs_prefix, "", "本机根没有前缀 —— 路径已经是宿主形式");
    }

    #[test]
    fn an_absent_userprofile_yields_no_root_rather_than_a_root_at_the_drive_root() {
        assert_eq!(local_root(None), None);
        assert_eq!(
            local_root(Some("   ")),
            None,
            "空白的 USERPROFILE 会拼出 `\\.claude`"
        );
    }

    #[test]
    fn a_wsl_root_is_reachable_over_unc_and_tagged_by_distro() {
        let r = wsl_root("Ubuntu-22.04", "/home/simon");
        assert_eq!(r.fs_prefix, r"\\wsl.localhost\Ubuntu-22.04");
        assert_eq!(
            r.claude_home,
            r"\\wsl.localhost\Ubuntu-22.04\home\simon\.claude"
        );
        assert_eq!(
            r.codex_home,
            r"\\wsl.localhost\Ubuntu-22.04\home\simon\.codex"
        );
        assert_eq!(r.location, "wsl-Ubuntu-22.04");
    }

    #[test]
    fn a_wsl_root_translates_every_separator_not_just_the_first() {
        // `/home/a/b` 里有三个分隔符；只换第一个会得到 `\home/a/b`，而那条路在
        // Windows 上打得开一部分、错得很安静。
        let r = wsl_root("D", "/home/a/b");
        assert_eq!(r.claude_home, r"\\wsl.localhost\D\home\a\b\.claude");
    }

    #[test]
    fn an_unreachable_location_is_not_the_same_value_as_having_no_roots() {
        // 这条钉的是类型本身：两种情况必须能被调用方分开。
        let none = Enumeration::default();
        let failed = Enumeration {
            roots: Vec::new(),
            unreachable: vec![UnreachableRoot {
                location: "wsl".into(),
                reason: "wsl.exe timed out".into(),
            }],
        };
        assert_ne!(none, failed, "「没有根」与「没问成」不得是同一个值");
        assert!(none.unreachable.is_empty());
        assert!(!failed.unreachable.is_empty());
    }
}
