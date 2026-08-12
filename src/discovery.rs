//! 项目根**发现**（ADR-050 决定 1 的上半层）。
//!
//! 与 [`crate::attribution`] 是**性质相反**的一对：
//!
//! | | 需要 I/O | 允许失败 | 必须对每条事件有答案 |
//! | --- | --- | --- | --- |
//! | **发现**（本模块） | ✅ | ✅ | ❌ |
//! | **归属**（`attribution`） | ❌ | ❌ | ✅ |
//!
//! 本模块回答「**这条路径的祖先链上，哪个目录是项目根**」，产物写进注册表；
//! 归属只读注册表，不碰盘。
//!
//! ## 🔴 `.git` 优先，但只找**最近的**那个（决定 5）
//!
//! 先整条链找 `.git`，找不到才回退最近的构建 marker。这实现了「`.git` 优先于构建
//! marker」（于是 `QuotaBar/src-tauri` 归到 `QuotaBar`，而 `third_party/TumeFlow`
//! 有自己的 `.git` 仍归它自己 —— 后者是**对的**，子模块就是独立 repo）。
//!
//! ⚠️ 而它**不引入**原 P3 那个「一路走到根会命中 `~/.git`（dotfiles 仓）」的风险：
//! 路径上任何更近的 `.git` 都会先命中。唯一还能走到 home 的情况是「这条链上一个
//! `.git` 都没有」，所以**显式排除 `$HOME` / 用户主目录本身** —— 把 home 当项目根，
//! 会让它名下每个散落目录都归到同一个「项目」。
//!
//! ## 每个来源都可以缺席（决定 4）
//!
//! 一个探测器坏了（WSL 未安装、发行版关机、路径不可达）只该**少发现几个根**，
//! 不该让整套归属失效。所以每次探测的失败都被收进 [`DiscoveryReport::failed`]
//! 计数，而不是向上抛。
//!
//! 🔴 **但失败要数得出来** —— 「发现覆盖不到哪里」是个该被看见的量，不是该被
//! 藏起来的错误（ADR-050 验收第 3 条）。

use crate::attribution::RootSource;
use crate::pathnorm;
use std::path::Path;

/// 一条路径的探测结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probe {
    /// 找到了项目根。
    Found { root: String, source: RootSource },
    /// 探测成功，但这条链上没有项目根。**不是失败** —— 归属会据此报 `Unattributed`。
    None,
    /// 探测本身失败（WSL 不可达、权限、路径形式不支持…）。
    ///
    /// 🔴 与 `None` **必须分开**：`None` 是「问过了，没有」，`Failed` 是「没问成」。
    /// 混成一个会让「WSL 关机」看起来像「这些路径都不属于任何项目」，
    /// 而那正是本仓那条「降级要降到说不出来」要防的。
    Failed { reason: String },
}

/// 一批探测的汇总。
#[derive(Debug, Clone, Default)]
pub struct DiscoveryReport {
    /// 新发现的根 `(路径, 来源)`。去重后的。
    pub roots: Vec<(String, RootSource)>,
    /// 探测过、确认链上没有根的路径数。
    pub without_root: usize,
    /// 探测失败的路径数 + 首条原因（诊断用；不逐条留，避免 WSL 关机时刷屏）。
    pub failed: usize,
    pub first_failure: Option<String>,
}

impl DiscoveryReport {
    fn record(&mut self, probe: Probe) {
        match probe {
            Probe::Found { root, source } => {
                if !self.roots.iter().any(|(p, _)| p == &root) {
                    self.roots.push((root, source));
                }
            }
            Probe::None => self.without_root += 1,
            Probe::Failed { reason } => {
                self.failed += 1;
                if self.first_failure.is_none() {
                    self.first_failure = Some(reason);
                }
            }
        }
    }
}

/// 本机文件系统上的祖先链探测。
///
/// 只对**本机可 stat** 的路径有意义 —— WSL 规范形 / UNC / Windows 宿主上的裸 Linux
/// 路径交给 [`probe_wsl`]。分派见 [`discover`]。
pub fn probe_local(path: &str) -> Probe {
    probe_local_with_home(path, dirs_next::home_dir().as_deref())
}

/// [`probe_local`] 的可测形态 —— **home 显式注入，不读全局状态**。
///
/// 🔴 拆出来是因为「home 不算项目根」这条规则**在本机测不到**：本机 home 是不是
/// git 仓完全看用户，而测试不能依赖那个。变异验证当场暴露了它 —— 删掉 home 排除，
/// 8 条测试全绿。
///
/// 与本仓一贯的做法同源（`currency.anchor_fn` 注入、`resolve_project_root` 的
/// `HostPlatform` 注入）：**平台事实作参数，逻辑才可单测**。
pub(crate) fn probe_local_with_home(path: &str, home: Option<&Path>) -> Probe {
    let mut cur = Some(Path::new(path));
    // 第一遍：最近的 `.git`（`exists()` 对子模块的 `.git` **文件**同样为真）。
    while let Some(dir) = cur {
        if Some(dir) != home && dir.join(".git").exists() {
            return Probe::Found {
                root: dir.to_string_lossy().into_owned(),
                source: RootSource::Git,
            };
        }
        cur = dir.parent();
    }
    // 第二遍：最近的构建 marker。
    let mut cur = Some(Path::new(path));
    while let Some(dir) = cur {
        if Some(dir) != home {
            for m in crate::project_root::MARKERS
                .iter()
                .filter(|m| **m != ".git")
            {
                if dir.join(m).exists() {
                    return Probe::Found {
                        root: dir.to_string_lossy().into_owned(),
                        source: RootSource::Marker,
                    };
                }
            }
        }
        cur = dir.parent();
    }
    Probe::None
}

/// 经访问桥在 WSL 内探测 —— **一次 `wsl.exe` 调用走完整条祖先链**。
///
/// 🔴 **这条是 ADR-050 最大的一块收益**：实测全库 `wsl_cwd` 有 68 个 root /
/// 707,386 条事件（71.4%），它们在本机 stat 不了，于是**从没做过项目根解析**。
///
/// 逐级从 Windows 侧 stat 要 N 次跨 VM 往返（每次约 0.1–0.3s），所以循环写在
/// 脚本里 —— 见 [`crate::wsl::find_project_root`]。
pub fn probe_wsl(distro: &str, linux_path: &str) -> Probe {
    match crate::wsl::find_project_root(distro, linux_path) {
        Ok(Some((dir, kind))) => {
            // 探测结果里的路径是 WSL 内的 Linux 路径，要还原成规范形才与
            // `project_root` 同形 —— 否则注册表里的根匹配不上任何事件。
            let root =
                pathnorm::normalize_cwd(Some(&dir), pathnorm::HostPlatform::Windows, Some(distro))
                    .unwrap_or(dir);
            Probe::Found {
                root,
                source: if kind == "git" {
                    RootSource::Git
                } else {
                    RootSource::Marker
                },
            }
        }
        Ok(None) => Probe::None,
        Err(e) => Probe::Failed { reason: e },
    }
}

/// 对一批候选路径做发现，按路径形式自动分派探测器。
///
/// 候选通常就是**总库里已有的 `project_root` 值** —— 那些是人真的工作过的目录，
/// 项目根在它们的祖先链上。
///
/// `default_distro` 是访问桥注入的运行期事实（Windows 上「唯一用户发行版」时才有值）；
/// 缺席时 WSL 形式的路径**探测不了**，计入 `failed` 而不是当作「没有根」。
pub fn discover<I, S>(candidates: I, default_distro: Option<&str>) -> DiscoveryReport
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut report = DiscoveryReport::default();
    let mut seen: std::collections::BTreeSet<String> = Default::default();
    for c in candidates {
        let path = c.as_ref().trim();
        if path.is_empty() || !seen.insert(path.to_string()) {
            continue;
        }
        report.record(probe_path(path, default_distro));
    }
    report
}

/// 单条路径的分派：WSL 形式走访问桥，其余走本机。
///
/// 分派判据与 `project_root::resolve_project_root` 的守卫**同源**（都问「本机能不能
/// stat 它」），但**结论相反**：那里 stat 不了就放弃并拿 cwd 冒充答案，这里
/// stat 不了就**换一条能问的路**（访问桥）。这正是 ADR-050 根因一说的那个区别。
pub fn probe_path(path: &str, default_distro: Option<&str>) -> Probe {
    if let Some((distro, linux)) = pathnorm::split_canonical_wsl(path) {
        return probe_wsl(distro, linux);
    }
    if let Some(canonical) = pathnorm::canonical_wsl_unc(path) {
        if let Some((distro, linux)) = pathnorm::split_canonical_wsl(&canonical) {
            return probe_wsl(distro, linux);
        }
    }
    // Windows 宿主上的裸 Linux 路径（`/home/…`）：本机 stat 会去错盘，
    // 只有知道 distro 才问得动。
    if cfg!(windows) && pathnorm::is_bare_linux_path(path) {
        return match default_distro {
            Some(d) => probe_wsl(d, path),
            None => Probe::Failed {
                reason: format!("bare linux path with no known distro: {path}"),
            },
        };
    }
    probe_local(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_nearest_git_not_the_farthest() {
        let tmp = std::env::temp_dir().join(format!("sv-disc-{}", std::process::id()));
        let outer = tmp.join("outer");
        let inner = outer.join("sub").join("inner");
        std::fs::create_dir_all(inner.join("deep")).unwrap();
        std::fs::create_dir_all(outer.join(".git")).unwrap();
        std::fs::create_dir_all(inner.join(".git")).unwrap();

        // 🔴 最近的 .git 赢 —— 子模块就是独立 repo，不该被归到父仓。
        match probe_local(&inner.join("deep").to_string_lossy()) {
            Probe::Found { root, source } => {
                assert_eq!(source, RootSource::Git);
                assert!(root.ends_with("inner"), "got {root}");
            }
            other => panic!("expected Found, got {other:?}"),
        }
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn git_wins_over_a_nearer_build_marker() {
        // 🔴 这条正是原 P3 要的：src-tauri 有 Cargo.toml 但没有 .git ⇒ 归父仓。
        let tmp = std::env::temp_dir().join(format!("sv-disc-p3-{}", std::process::id()));
        let repo = tmp.join("repo");
        let sub = repo.join("src-tauri");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::write(sub.join("Cargo.toml"), "[package]").unwrap();

        match probe_local(&sub.to_string_lossy()) {
            Probe::Found { root, source } => {
                assert_eq!(source, RootSource::Git);
                assert!(root.ends_with("repo"), "got {root}");
            }
            other => panic!("expected Found, got {other:?}"),
        }
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn falls_back_to_a_build_marker_when_there_is_no_git_anywhere() {
        let tmp = std::env::temp_dir().join(format!("sv-disc-mk-{}", std::process::id()));
        let proj = tmp.join("proj");
        let deep = proj.join("a").join("b");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(proj.join("package.json"), "{}").unwrap();

        match probe_local(&deep.to_string_lossy()) {
            Probe::Found { root, source } => {
                assert_eq!(source, RootSource::Marker);
                assert!(root.ends_with("proj"), "got {root}");
            }
            other => panic!("expected Found, got {other:?}"),
        }
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn a_chain_with_nothing_reports_none_not_a_failure() {
        // 🔴 「问过了，没有」与「没问成」必须分开 —— 后者不该看起来像前者。
        let tmp = std::env::temp_dir().join(format!("sv-disc-empty-{}", std::process::id()));
        let deep = tmp.join("x").join("y");
        std::fs::create_dir_all(&deep).unwrap();
        assert_eq!(probe_local(&deep.to_string_lossy()), Probe::None);
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn a_bare_linux_path_without_a_distro_is_a_failure_not_a_none() {
        // WSL 未安装 / distro 未知 ⇒ **说不出来**，而不是「这条路径没有项目」。
        // 混成 None 会让归属把它们静默记成 Unattributed，掩盖掉一个可修的配置问题。
        if !cfg!(windows) {
            return; // 非 Windows 上裸 Linux 路径是本机路径，走 probe_local
        }
        match probe_path("/home/simon/workspace/EyeVLM/docs", None) {
            Probe::Failed { reason } => assert!(reason.contains("no known distro")),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn home_itself_is_never_a_project_root() {
        // 🔴 这条规则一度**测不到**：其余测试都用 temp_dir 下的路径，home 不在链上，
        // 于是「删掉 home 排除」这个变异 8 条全绿。修法不是多写一条测试，是把 home
        // 变成**显式参数** —— 本机 home 是不是 git 仓完全看用户，测试不该依赖那个。
        //
        // 风险是真的：用 dotfiles 仓管理 home 很常见，而一旦把 home 当项目根，
        // 它名下每个散落目录都会归到同一个「项目」。
        let tmp = std::env::temp_dir().join(format!("sv-home-{}", std::process::id()));
        let fake_home = tmp.join("home");
        let stray = fake_home.join("stray-dir");
        std::fs::create_dir_all(&stray).unwrap();
        std::fs::create_dir_all(fake_home.join(".git")).unwrap(); // home 是个 dotfiles 仓

        let got = probe_local_with_home(&stray.to_string_lossy(), Some(&fake_home));
        assert_eq!(
            got,
            Probe::None,
            "home 是 git 仓时，它名下的散落目录该报「没有根」，而不是归到 home"
        );

        // 而 home **之下**的真项目仍然认得出来 —— 排除的是 home 本身，不是整棵子树。
        let proj = fake_home.join("proj");
        std::fs::create_dir_all(proj.join("deep")).unwrap();
        std::fs::create_dir_all(proj.join(".git")).unwrap();
        match probe_local_with_home(&proj.join("deep").to_string_lossy(), Some(&fake_home)) {
            Probe::Found { root, .. } => assert!(root.ends_with("proj"), "got {root}"),
            other => panic!("expected Found, got {other:?}"),
        }
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn report_dedupes_roots_and_counts_the_rest() {
        let mut r = DiscoveryReport::default();
        r.record(Probe::Found {
            root: "/w/P".into(),
            source: RootSource::Git,
        });
        r.record(Probe::Found {
            root: "/w/P".into(),
            source: RootSource::Marker,
        });
        r.record(Probe::None);
        r.record(Probe::Failed {
            reason: "wsl down".into(),
        });
        r.record(Probe::Failed {
            reason: "wsl still down".into(),
        });
        assert_eq!(r.roots.len(), 1, "同一个根不该重复");
        assert_eq!(r.without_root, 1);
        assert_eq!(r.failed, 2);
        // 只留首条原因 —— WSL 关机时逐条留会刷屏，而它们说的是同一件事。
        assert_eq!(r.first_failure.as_deref(), Some("wsl down"));
    }

    #[test]
    fn discover_skips_duplicates_in_the_candidate_list() {
        let tmp = std::env::temp_dir().join(format!("sv-disc-dup-{}", std::process::id()));
        std::fs::create_dir_all(tmp.join("r").join("s")).unwrap();
        std::fs::create_dir_all(tmp.join("r").join(".git")).unwrap();
        let p = tmp.join("r").join("s").to_string_lossy().into_owned();
        let rep = discover([p.clone(), p.clone(), p], None);
        assert_eq!(rep.roots.len(), 1);
        std::fs::remove_dir_all(&tmp).ok();
    }
}
