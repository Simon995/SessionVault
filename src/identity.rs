//! 项目的**规范身份**——一个跨系统、跨 checkout 稳定的 id（ADR-032）。
//!
//! ## 为什么身份必须与路径分开
//!
//! 同一个仓库在这台机器上可能有好几份 checkout：Windows 一份、WSL 一份、外接盘一份。
//! 它们的**路径互不相同**，而用户心里那是**一个项目** —— 在 WSL 里学到的东西，切到
//! Windows 那份接着干时理应还在。所以「哪些路径是同一个项目」不能靠路径字符串判断，
//! 得靠仓库自己的身份：**git origin remote**。
//!
//! [`resolve_project_root`](crate::project_root::resolve_project_root) 回答的是
//! 「这次会话在哪个**目录**」；本模块回答的是「那个目录属于哪个**项目**」。两者都需要，
//! 但它们是不同的问题，混在一起会让「同一个项目的两份 checkout」永远合不到一起。
//!
//! ## 只读 `.git/config`，从不 spawn git
//!
//! 一次 `git remote get-url` 是一个进程；扫描时每个项目都要问一次，而 SessionVault
//! 的调用方里有 GUI 的 setup 路径。读那个 ini 文件是纯文件 IO，跨平台、可测、
//! 在没装 git 的机器上照样工作 —— 而**存在 `.git/config` 却没装 git** 是常见的
//! （容器、精简镜像、别人拷来的目录）。
//!
//! ## 🔴 拿不到 origin 时退回路径，且**这件事要看得出来**
//!
//! 没有 remote 的本地仓库（`git init` 之后没 add remote）是完全正常的状态。那时
//! 身份退回 `path:<git root>` —— 它**不跨 checkout 稳定**（同一个仓的另一份拷贝会得到
//! 另一个 id），但那不是缺陷，是「这个仓库确实没有可跨机器识别的身份」的忠实反映。
//! 前缀 `git:` / `path:` 让消费方一眼看得出自己拿到的是哪一种，而不是拿到一个
//! 看起来一样、稳定性却完全不同的串。
//!
//! ## 边界：本模块**只认磁盘上现在有什么**
//!
//! checkout 被删除之后，`.git/config` 也没了 ⇒ 这里再也算不出它的 `git:` 身份。
//! 实测（2026-08-11，QuotaBar 侧）：事件量最大的那个项目**没有别名组**，正是因为它的
//! WSL checkout 已被删除，而总库里还留着它 16 万条历史事件。
//!
//! ⇒ **要让身份活过 checkout 的删除，得在扫描时把它记下来**（那时 `.git` 还在），
//! 而不是每次现算。那是本模块之后的一步，不在这里。

use std::path::{Path, PathBuf};

use crate::deadline::Deadline;
use crate::probe::{ProbeBackend, Probed};

/// 一次 git 根查找的结果 —— **三态**。
///
/// 🔴 从前是 `Option<PathBuf>`，而两处 `.exists()` 把「没问成」折叠进了 `None`。
/// 后果不是崩溃，是**静默**：`store.rs` 的 `note_project_identity` 拿到 `None` 就
/// `return`，且它**先记后算**（`identity_seen` 在计算前就插了 key）—— 于是一次
/// 权限错误 / UNC 不通让这个项目在本进程生命周期内**永远**算不出 `git:` 身份，
/// 没有别名组，跨 checkout 的 Class-A 证据在 project 作用域里蒸发。
/// 而界面上什么都不会说。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitRoot {
    Found(PathBuf),
    /// 探明白了：起点已不在磁盘上，或整条链上没有 `.git`。**这是事实。**
    Absent,
    /// **没问成** —— 本轮这个答案不作数，别据此写「这个项目没有身份」。
    Unknown(crate::probe::ProbeError),
}

/// 从 `start` 向上找最近的含 `.git` 的目录。
///
/// **起点不存在时返回 [`GitRoot::Absent`]**，而不是继续向上走 —— 一个已被删除的
/// checkout 的父目录可能恰好是另一个仓库，那样会把它的身份安到一个不相干的项目上。
///
/// 🔴 **一层没问成就停在那里**（ADR-051 §5 规则 ③，与 `discovery::probe_local_with`
/// 同一条）：继续上溯会把一个**错误的归属**说成成功 —— `/w/proj/sub` 的 `.git` 读不到、
/// 于是上溯到 `/w/proj` 命中，可 `sub` 很可能本来就有 `.git`。报 `Unknown` 走重试，
/// 错误归属只会安静地留在库里。
pub fn find_git_root(start: &Path) -> GitRoot {
    find_git_root_with(start, &crate::probe::LocalBackend)
}

/// [`find_git_root`] 的可测形态 —— **backend 注入**（「探测失败」在本机造不出来）。
pub fn find_git_root_with(start: &Path, backend: &dyn ProbeBackend) -> GitRoot {
    let d = Deadline::unbounded();
    match backend.probe(start, d) {
        Probed::Found(_) => {}
        Probed::Absent => return GitRoot::Absent,
        Probed::Unknown(e) => return GitRoot::Unknown(e),
    }
    let mut cur = Some(start);
    while let Some(dir) = cur {
        match backend.probe(&dir.join(".git"), d) {
            // `.git` 是**文件**时同样成立（子模块 / worktree）。
            Probed::Found(_) => return GitRoot::Found(dir.to_path_buf()),
            Probed::Absent => {}
            Probed::Unknown(e) => return GitRoot::Unknown(e),
        }
        cur = dir.parent();
    }
    GitRoot::Absent
}

/// 读 `.git/config` 里 `[remote "origin"]` 的 `url`。
///
/// 手写的极小 ini 扫描：只认小节头与 `url =`，够用且无依赖。**大小写不敏感地**匹配
/// 小节头 —— git 自己接受 `[REMOTE "origin"]`。
pub fn read_origin_url(git_root: &Path) -> Option<String> {
    let text = std::fs::read_to_string(git_root.join(".git").join("config")).ok()?;
    let mut in_origin = false;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            in_origin = t.eq_ignore_ascii_case("[remote \"origin\"]");
            continue;
        }
        if !in_origin {
            continue;
        }
        if let Some(rest) = t.strip_prefix("url") {
            if let Some(v) = rest.trim_start().strip_prefix('=') {
                let v = v.trim();
                if !v.is_empty() {
                    return Some(v.to_string());
                }
            }
        }
    }
    None
}

/// 把同一个仓库的各种 remote 写法收敛成一个串。
///
/// 🔴 **判据是「同一个仓的不同写法必须collapse 成同一个串，不同的仓永不相同」。**
/// 同一个远端可以被写成至少四种样子，而它们指的是同一个仓库：
///
/// ```text
/// https://github.com/o/r.git
/// git@github.com:o/r.git          ← scp-like，冒号不是端口
/// ssh://git@github.com/o/r
/// https://github.com/O/R/         ← 大小写 + 末尾斜杠
/// ```
///
/// 收敛掉：scheme、用户名、末尾 `/`、末尾 `.git`、大小写。**不**收敛主机名 ——
/// `github.com/o/r` 与 `gitlab.com/o/r` 是两个仓库。
pub fn normalize_remote(url: &str) -> Option<String> {
    let u = url.trim();
    if u.is_empty() {
        return None;
    }
    // scp-like (`git@host:owner/repo`) vs `scheme://[user@]host/path`.
    let body = if let Some(rest) = u.strip_prefix("git@") {
        rest.replacen(':', "/", 1)
    } else {
        let after_scheme = u.rsplit("://").next().unwrap_or(u);
        match after_scheme.split_once('@') {
            Some((_, host_path)) => host_path.to_string(),
            None => after_scheme.to_string(),
        }
    };
    let body = body.trim_end_matches('/').trim_end_matches(".git");
    if body.is_empty() {
        None
    } else {
        Some(body.to_lowercase())
    }
}

/// 一个 git 仓库根的规范身份：`git:<host>/<owner>/<repo>`，拿不到 remote 时
/// `path:<git root>`。前缀是契约的一部分 —— 见模块文档「拿不到 origin 时」。
pub fn canonical_repo_id(git_root: &Path) -> String {
    if let Some(url) = read_origin_url(git_root) {
        if let Some(norm) = normalize_remote(&url) {
            return format!("git:{norm}");
        }
    }
    format!("path:{}", git_root.to_string_lossy())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sv-identity-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn seed_repo(root: &Path, origin: Option<&str>) {
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let body = match origin {
            Some(url) => format!("[core]\n\tbare = false\n[remote \"origin\"]\n\turl = {url}\n"),
            None => "[core]\n\tbare = false\n".to_string(),
        };
        std::fs::write(root.join(".git").join("config"), body).unwrap();
    }

    #[test]
    fn the_same_repo_written_four_ways_collapses_to_one_id() {
        // 🔴 这是本模块存在的理由：同一个仓的不同 remote 写法**必须**同 id，
        // 否则同一个项目的两份 checkout 会被当成两个项目 —— 而那正是 ADR-032 要防的。
        let want = Some("github.com/o/r".to_string());
        assert_eq!(normalize_remote("https://github.com/o/r.git"), want);
        assert_eq!(normalize_remote("git@github.com:o/r.git"), want);
        assert_eq!(normalize_remote("ssh://git@github.com/o/r"), want);
        assert_eq!(normalize_remote("https://github.com/O/R/"), want);
    }

    #[test]
    fn different_hosts_are_different_repos() {
        // 收敛不能过头：主机名是身份的一部分。
        assert_ne!(
            normalize_remote("https://github.com/o/r"),
            normalize_remote("https://gitlab.com/o/r")
        );
    }

    #[test]
    fn blank_and_empty_remotes_decline_rather_than_inventing_an_id() {
        assert_eq!(normalize_remote("   "), None);
        assert_eq!(normalize_remote(""), None);
        // 只有 scheme、没有主体 —— 造一个 `git:` 身份出来比说不知道更糟。
        assert_eq!(normalize_remote("https://"), None);
    }

    #[test]
    fn a_repo_without_a_remote_falls_back_to_path_and_says_so() {
        // `git init` 之后没 add remote 是正常状态。身份退回 path 前缀 ——
        // 它不跨 checkout 稳定，而前缀让消费方看得出这一点。
        let root = scratch("no-remote");
        seed_repo(&root, None);
        let id = canonical_repo_id(&root);
        assert!(id.starts_with("path:"), "{id}");
        assert!(
            !id.starts_with("git:"),
            "没有 remote 时不得伪造 git 身份：{id}"
        );
    }

    #[test]
    fn origin_is_read_case_insensitively_from_the_section_header() {
        let root = scratch("upper-section");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(
            root.join(".git").join("config"),
            "[REMOTE \"origin\"]\n\turl = git@github.com:o/r.git\n",
        )
        .unwrap();
        assert_eq!(canonical_repo_id(&root), "git:github.com/o/r");
    }

    #[test]
    fn a_non_origin_remote_is_not_mistaken_for_origin() {
        // 只有 origin 算数：upstream / fork 指向别的仓，拿它当身份会把两个项目合并。
        let root = scratch("upstream-only");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(
            root.join(".git").join("config"),
            "[remote \"upstream\"]\n\turl = git@github.com:someone/else.git\n",
        )
        .unwrap();
        assert_eq!(read_origin_url(&root), None);
        assert!(canonical_repo_id(&root).starts_with("path:"));
    }

    #[test]
    fn find_git_root_declines_on_a_path_that_no_longer_exists() {
        // 🔴 已删除的 checkout 的父目录可能恰好是另一个仓库 —— 继续向上走会把它的
        // 身份安到一个不相干的项目上。实测就有这种形状（QuotaBar 侧，2026-08-11）。
        let root = scratch("deleted-child");
        seed_repo(&root, Some("git@github.com:o/outer.git"));
        let gone = root.join("was-a-checkout");
        assert_eq!(
            find_git_root(&gone),
            GitRoot::Absent,
            "路径不存在时必须拒绝，不能上溯到 {root:?}"
        );
    }

    #[test]
    fn find_git_root_walks_up_from_a_subdirectory() {
        let root = scratch("nested");
        seed_repo(&root, Some("git@github.com:o/r.git"));
        let sub = root.join("src").join("deep");
        std::fs::create_dir_all(&sub).unwrap();
        assert_eq!(find_git_root(&sub), GitRoot::Found(root.clone()));
        let GitRoot::Found(found) = find_git_root(&sub) else {
            panic!("应当找到 git 根");
        };
        assert_eq!(canonical_repo_id(&found), "git:github.com/o/r");
    }

    /// 🔴 **探测失败不是「这个项目没有 git 根」。**
    ///
    /// 两条边都钉：起点探不动、以及链上某一层探不动 —— 从前两处都是 `.exists()`，
    /// 一次权限拒绝会让调用方（`store::note_project_identity`）当成 `Absent` 静默
    /// 放弃，而它**先记后算**，于是这个项目在本进程里再也不会被重试。
    ///
    /// ⚠️ 反向那条（真的没有 ⇒ `Absent`）由上面两条测试钉着 —— 少了它，一个
    /// 恒 `Unknown` 的实现照样能让本测试通过。
    #[test]
    fn a_probe_failure_is_unknown_not_absent() {
        struct Failing;
        impl ProbeBackend for Failing {
            fn probe(&self, p: &Path, _d: Deadline) -> Probed<crate::probe::FileKind> {
                Probed::Unknown(crate::probe::ProbeError::new(p, "permission denied"))
            }
        }
        assert!(matches!(
            find_git_root_with(Path::new("/w/proj/sub"), &Failing),
            GitRoot::Unknown(_)
        ));

        // 起点探得到、`.git` 探不动 —— 这条链上**可能**有根，只是没问成。
        // 从前它会一路上溯，把子仓库的会话记到父仓库名下（ADR-051 §5 规则 ③）。
        struct StartOkThenFailing;
        impl ProbeBackend for StartOkThenFailing {
            fn probe(&self, p: &Path, _d: Deadline) -> Probed<crate::probe::FileKind> {
                if p.file_name().is_some_and(|n| n == ".git") {
                    Probed::Unknown(crate::probe::ProbeError::new(p, "handle exhausted"))
                } else {
                    Probed::Found(crate::probe::FileKind::Dir)
                }
            }
        }
        assert!(matches!(
            find_git_root_with(Path::new("/w/proj/sub"), &StartOkThenFailing),
            GitRoot::Unknown(_)
        ));
    }
}
