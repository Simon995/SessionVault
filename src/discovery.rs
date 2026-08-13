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
    probe_local_with(path, home, |p| p.try_exists())
}

/// [`probe_local_with_home`] 的可测形态 —— **存在性判定注入**。
///
/// 🔴 拆出来是因为「权限错误 ≠ 不存在」这条**在本机测不到**：造一个 `try_exists`
/// 会失败的路径要么要管理员权限、要么依赖平台细节。判定作参数，逻辑才可单测 ——
/// 与 `home` 注入、`probe_mnt_with` 的探测器注入同一个惯例。
pub(crate) fn probe_local_with(
    path: &str,
    home: Option<&Path>,
    exists: impl Fn(&Path) -> std::io::Result<bool>,
) -> Probe {
    // 🔴 **`Path::exists()` 把访问失败折叠成 `false`**（评审 [P2]）。权限拒绝、
    // 句柄耗尽、瞬时 IO 错误全都长得像「这里没有 `.git`」，于是探测要么错误地
    // 落到**外层仓库**（把子项目的事件归到父仓名下），要么报 `Probe::None` 并被
    // 按「确认没有根」写进 6 小时负缓存 —— 而那正是本 ADR 立的 `None` / `Failed`
    // 契约要防的：**「问了、没有」与「没问成」必须分开**。
    // 用 `try_exists()`：只有 `NotFound` 才是「没有」。
    // 🔴 **一层没问成就停在那里，不许继续向父目录找**（ADR-051 §5 规则 ③）。
    //
    // 从前是「记下失败、继续上溯，只在一路没探到时才用它」。那条规则把一个**错误的
    // 归属**说成了成功：`/w/proj/sub` 的 `.git` 读不到、于是上溯到 `/w/proj` 命中 ——
    // 可 `sub` 很可能**本来就有** `.git`，真正的根是它。结果是把子仓库的会话记到父
    // 仓库名下，**而且静默**。
    //
    // 报 `Failed` 会走短退避、下轮重试；错误归属只会安静地留在库里。两者的代价
    // 差着数量级。
    //
    // ⚠️ 命中之后的失败不算：命中即返回，根本不会去探更外层（有测试钉这条 ——
    // 否则「更严格」会变成「一路探到底」，一个无关的外层权限问题又能推翻结论）。
    let probe_layer = |dir: &Path, name: &str| -> Result<bool, String> {
        exists(&dir.join(name)).map_err(|e| format!("{}: {e}", dir.join(name).display()))
    };
    let cannot_tell = |why: String| Probe::Failed {
        reason: format!("cannot tell whether a root exists ({why})"),
    };

    // 第一遍：最近的 `.git`（对子模块的 `.git` **文件**同样为真）。
    //
    // ⚠️ 两遍而不是逐层「`.git` 与 marker 一起看」—— `.git` 全局优先于 marker 是
    // 既有语义（改它是另一个决定，见 #34），本次只改失败处理。
    let mut cur = Some(Path::new(path));
    while let Some(dir) = cur {
        if Some(dir) != home {
            match probe_layer(dir, ".git") {
                Err(why) => return cannot_tell(why),
                Ok(true) => {
                    return Probe::Found {
                        root: dir.to_string_lossy().into_owned(),
                        source: RootSource::Git,
                    }
                }
                Ok(false) => {}
            }
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
                match probe_layer(dir, m) {
                    Err(why) => return cannot_tell(why),
                    Ok(true) => {
                        return Probe::Found {
                            root: dir.to_string_lossy().into_owned(),
                            source: RootSource::Marker,
                        }
                    }
                    Ok(false) => {}
                }
            }
        }
        cur = dir.parent();
    }

    // 一路都答得上来、就是没有 —— 那才是「问了、没有」。
    Probe::None
}

/// 经访问桥在 WSL 内探测 —— **一次 `wsl.exe` 调用走完整条祖先链**。
///
/// 🔴 **这条是 ADR-050 最大的一块收益**：实测全库 `wsl_cwd` 有 68 个 root /
/// 707,386 条事件（71.4%），它们在本机 stat 不了，于是**从没做过项目根解析**。
///
/// 逐级从 Windows 侧 stat 要 N 次跨 VM 往返（每次约 0.1–0.3s），所以循环写在
/// 脚本里 —— 见 [`crate::wsl::find_project_root`]。
pub fn probe_wsl(distro: &str, linux_path: &str, canonical_form: bool) -> Probe {
    match crate::wsl::find_project_root(distro, linux_path) {
        Ok(Some((dir, kind))) => {
            // 🔴 **结果的形式必须跟随输入的形式。**
            //
            // WSL 里返回的永远是 Linux 路径（`/home/u/P`），而同一个项目在总库里
            // 有多种写法。第一版无条件转成规范形（`wsl:<distro>:/home/u/P`），于是
            // 注册表里**只有规范形的根**，而裸 Linux 形式的 `project_root`
            // （实测 `/home/simon/workspace/EyeVLM` 一条就有 106,814 条事件）
            // 匹配不上任何根 —— 归属把它们全报成 `Unattributed`。
            //
            // 干跑当场抓到：同一个项目的两种形式，规范形归到了、裸形式没有。
            // 归属是纯字符串匹配，所以**登记什么形式，就只认什么形式**。
            let root = if canonical_form {
                pathnorm::normalize_cwd(Some(&dir), pathnorm::HostPlatform::Windows, Some(distro))
                    .unwrap_or(dir)
            } else {
                dir
            };
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

pub use crate::pathnorm::{mnt_to_windows, DriveMounts};

/// 对一批候选路径做发现，按路径形式自动分派探测器。
///
/// 候选通常就是**总库里已有的 `project_root` 值** —— 那些是人真的工作过的目录，
/// 项目根在它们的祖先链上。
///
/// `default_distro` 是访问桥注入的运行期事实（Windows 上「唯一用户发行版」时才有值）；
/// 缺席时 WSL 形式的路径**探测不了**，计入 `failed` 而不是当作「没有根」。
pub fn discover<I, S>(
    candidates: I,
    default_distro: Option<&str>,
    mounts: &DriveMounts,
) -> DiscoveryReport
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
        report.record(probe_path(path, default_distro, mounts));
    }
    report
}

/// 单条路径的分派：WSL 形式走访问桥，其余走本机。
///
/// 分派判据与 `project_root::resolve_project_root` 的守卫**同源**（都问「本机能不能
/// stat 它」），但**结论相反**：那里 stat 不了就放弃并拿 cwd 冒充答案，这里
/// stat 不了就**换一条能问的路**（访问桥）。这正是 ADR-050 根因一说的那个区别。
/// 🔴 **第三个参数 `canonical_form` 决定探测结果用哪种形式表达** —— 它跟随输入。
/// 规范形 / UNC 的输入拿规范形的根，裸 Linux 输入拿裸 Linux 的根。
/// 归属是纯字符串匹配，**登记什么形式就只认什么形式**；无条件转规范形会让裸形式
/// 的路径全部归不到根（干跑实测：一条 `/home/simon/workspace/EyeVLM` 就是 106,814
/// 条事件）。
pub fn probe_path(path: &str, default_distro: Option<&str>, mounts: &DriveMounts) -> Probe {
    if let Some((distro, linux)) = pathnorm::split_canonical_wsl(path) {
        return probe_wsl(distro, linux, true);
    }
    if let Some(canonical) = pathnorm::canonical_wsl_unc(path) {
        if let Some((distro, linux)) = pathnorm::split_canonical_wsl(&canonical) {
            // UNC 输入：总库里 UNC 与规范形是同一族（`canonical_wsl_unc` 已经把
            // 前者归一到后者），所以用规范形登记。
            return probe_wsl(distro, linux, true);
        }
    }
    // Windows 宿主上的裸 Linux 路径（`/home/…`）：本机 stat 会去错盘，
    // 只有知道 distro 才问得动 —— 但**结果要保持裸形式**。
    if cfg!(windows) && pathnorm::is_bare_linux_path(path) {
        return match default_distro {
            Some(d) => probe_wsl(d, path, false),
            None => Probe::Failed {
                reason: format!("bare linux path with no known distro: {path}"),
            },
        };
    }
    // 🔴 `/mnt/<drive>/…`：WSL 里访问 Windows 盘。**本机能 stat 它对应的盘符路径**，
    // 所以换算过去探测，比走访问桥便宜得多（零 `wsl.exe` 调用）。
    //
    // ⚠️ 换算必须靠**实测的挂载表**（`wsl::drive_mounts` 读 `mount`），不能按
    // 「`/mnt/<单字母>` 就是盘符」猜 —— 那在 `automount.root` 被改过、配置改了没
    // 重启、以及 `/mnt/data` 这类普通 Linux 挂载三种情况下都是错的，而猜错的后果
    // 是把事件归到**别的项目**（甚至不存在的盘）名下。
    //
    // 🔴 **换算不出来是「没问成」，不是「没有根」**（评审 [P2]）。
    //
    // 原先落到 `probe_local`：`Path::new("/mnt/d/…")` 在 Windows 上是**当前盘根的
    // 相对路径**，于是要么探不到（报 `None`）、要么更糟 —— 误中当前盘上真实存在的
    // `\mnt\…`。而 `None` 会被调用方按「确认无根」缓存 24 小时，
    // 一次 WSL 超时就让这一族路径整天归不到根。
    //
    // 与本 ADR 的主线同一条：`Probe::None`（问了、没有）与 `Probe::Failed`（没问成）
    // 必须分开 —— 这里属于后者，退避该按「暂时故障」的那档走。
    if cfg!(windows) && pathnorm::is_windows_drive_mount(path) {
        return match probe_mnt_with(path, mounts, probe_local) {
            Some(p) => p,
            None => Probe::Failed {
                reason: format!(
                    "no drive mount covers {path} (mount table unavailable or this is a plain Linux mount)"
                ),
            },
        };
    }
    probe_local(path)
}

/// `/mnt/…` 分支的可测形态 —— **探测器显式注入**。
///
/// `None` = 「这条路径不在实测挂载表里」，由调用者落回 [`probe_local`]。
///
/// 🔴 **探到的根按宿主形式（`D:\…`）返回，不转回 `/mnt/…`。** `/mnt/c/X` 与
/// `C:\X` 在本机是同一个目录、同一个项目，收敛由注册表的比较键负责
/// （[`crate::attribution::RootRegistry::with_mounts`]）—— 发现侧转回去只会让
/// `/mnt` 那一族自成一个根，干跑实测过：`/mnt/c/…/QuotaBar` 与 `C:\…\QuotaBar`
/// 分成两条，且前者在 Windows 上 stat 不到 `.git/config`，`canonical_repo_id`
/// 只能落 `path:` id，身份层跟着分家。
///
/// 拆出来（而不是内联在 `probe_path` 里）的理由和 [`probe_local_with_home`] 一样：
/// **探测器注入了才测得到调用点**。变异验证当场证明它是必要的 —— 改坏这里的接线，
/// 直接调两个映射函数的那几条测试一条都不红。
fn probe_mnt_with(
    path: &str,
    mounts: &DriveMounts,
    probe: impl Fn(&str) -> Probe,
) -> Option<Probe> {
    let win = mnt_to_windows(path, mounts)?;
    Some(probe(&win))
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
        match probe_path("/home/u/workspace/proj/docs", None, &Vec::new()) {
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
    fn the_probe_result_keeps_the_form_of_its_input() {
        // 🔴 这条是**干跑抓出来的真 bug**：第一版无条件把 WSL 探测结果转成规范形，
        // 于是注册表里只有规范形的根，而裸 Linux 形式的 project_root 匹配不上任何
        // 根 —— 实测一条 `/home/simon/workspace/EyeVLM` 就是 106,814 条事件被报成
        // Unattributed，而同一个项目的规范形 `wsl:Ubuntu-22.04:/home/…` 归到了。
        //
        // 归属是纯字符串匹配：**登记什么形式，就只认什么形式**。
        //
        // 这里不跑真 WSL（CI 没有），只钉住 `probe_path` 的分派意图：两种形式
        // 走的是同一个 distro、同一条链，区别只在结果的表达形式。
        use crate::pathnorm;
        // 规范形能被拆出 (distro, linux)，裸形式不能 —— 分派据此走两条路。
        assert!(pathnorm::split_canonical_wsl("wsl:U:/home/u/P").is_some());
        assert!(pathnorm::split_canonical_wsl("/home/u/P").is_none());
        assert!(pathnorm::is_bare_linux_path("/home/u/P"));
    }

    // ── /mnt/… 那一族：挂载表驱动的映射 ──────────────────────────────
    fn mounts() -> DriveMounts {
        vec![
            ("/mnt/c".to_string(), r"C:\".to_string()),
            ("/mnt/d".to_string(), r"D:\".to_string()),
        ]
    }

    #[test]
    fn mnt_maps_to_windows_using_the_measured_table() {
        let m = mounts();
        assert_eq!(
            mnt_to_windows("/mnt/d/work/code/proj", &m).as_deref(),
            Some(r"D:\work\code\proj")
        );
        assert_eq!(mnt_to_windows("/mnt/c", &m).as_deref(), Some(r"C:\"));
    }

    #[test]
    fn an_unmapped_mount_point_is_none_not_a_guess() {
        // 🔴 `/mnt/data` 可以是普通 Linux 挂载 —— 按「单字母就是盘符」猜会把它
        // 当成某个盘，而猜错的后果是把事件归到**别的项目**名下。
        let m = mounts();
        assert_eq!(mnt_to_windows("/mnt/data/stuff", &m), None);
        assert_eq!(
            mnt_to_windows("/mnt/e/x", &m),
            None,
            "表里没有 e 盘就不该编一个"
        );
    }

    #[test]
    fn an_empty_mount_table_maps_nothing() {
        // 读不到 mount ⇒ 不映射，而不是退回猜。
        assert_eq!(mnt_to_windows("/mnt/d/proj", &Vec::new()), None);
    }

    #[test]
    fn the_mnt_branch_probes_the_host_form_and_keeps_it() {
        // 🔴 这条打的是**调用点**，不是映射函数本身：探测器必须收到 Windows 形式，
        // 而探到的根**原样返回**（不转回 `/mnt`）—— 收敛由注册表的比较键做，
        // 见 `RootRegistry::with_mounts`。断言纯函数各自对，证明不了接线对。
        let m = mounts();
        let probed = std::cell::RefCell::new(String::new());
        let got = probe_mnt_with("/mnt/d/work/code/proj/sub", &m, |p| {
            *probed.borrow_mut() = p.to_string();
            Probe::Found {
                root: r"D:\work\code\proj".to_string(),
                source: RootSource::Git,
            }
        });
        assert_eq!(
            probed.borrow().as_str(),
            r"D:\work\code\proj\sub",
            "探测器该收到 Windows 形式"
        );
        match got {
            Some(Probe::Found { root, source }) => {
                assert_eq!(root, r"D:\work\code\proj", "根按宿主形式登记");
                assert_eq!(source, RootSource::Git);
            }
            other => panic!("expected Found, got {other:?}"),
        }
    }

    #[test]
    fn the_mnt_branch_passes_through_none_and_failed() {
        // 「没有根」和「探不动」都要原样穿过去 —— 不能被转换环节改写成别的答案。
        let m = mounts();
        assert!(matches!(
            probe_mnt_with("/mnt/d/x", &m, |_| Probe::None),
            Some(Probe::None)
        ));
        assert!(matches!(
            probe_mnt_with("/mnt/d/x", &m, |_| Probe::Failed {
                reason: "boom".into()
            }),
            Some(Probe::Failed { .. })
        ));
    }

    #[test]
    fn an_unmapped_mount_falls_through_instead_of_probing() {
        // 表里没有 ⇒ `None`（交回调用者），**不是**拿原路径去探。
        let called = std::cell::Cell::new(false);
        let got = probe_mnt_with("/mnt/data/x", &mounts(), |_| {
            called.set(true);
            Probe::None
        });
        assert!(got.is_none());
        assert!(!called.get(), "映射不出来时不该调探测器");
    }

    /// 🔴 访问失败 ≠ 「这里没有根」（评审 [P2]）。
    ///
    /// `Path::exists()` 把权限拒绝折叠成 `false`，于是探测要么错误地落到**外层仓库**，
    /// 要么报 `None` 并被按「确认没有根」写进 6 小时负缓存 —— 违反的正是本 ADR 立的
    /// `None` / `Failed` 契约。
    #[test]
    fn a_permission_error_while_probing_is_failed_not_rootless() {
        use std::io::{Error, ErrorKind};

        let deny = |_: &Path| -> std::io::Result<bool> {
            Err(Error::new(ErrorKind::PermissionDenied, "denied"))
        };
        match probe_local_with("/w/proj/sub", None, deny) {
            Probe::Failed { reason } => assert!(reason.contains("cannot tell"), "got {reason}"),
            other => panic!("权限失败必须报 Failed，得到 {other:?}"),
        }

        // 🔴 **内层没问成 ⇒ Failed，即使外层探得到根**（ADR-051 §5 规则 ③）。
        //
        // ⚠️ 这一段从前断言的是 `Found(/w/proj)`，理由写着「否则一个无关目录的权限
        // 问题会把一次成功的归属打成失败」。**那个「无关」不成立**：上溯路径上的每
        // 一层都可能就是真正的根。`/w/proj/sub` 的 `.git` 读不到，而它很可能本来就有
        // —— 归到 `/w/proj` 就是把子仓库的会话记到父仓库名下，**而且静默**。
        //
        // 一条把错误行为钉死的测试比没有测试更糟：它让后来者以为这是想要的。
        let inner_denied = |p: &Path| -> std::io::Result<bool> {
            if p.to_string_lossy().contains("sub") {
                Err(Error::new(ErrorKind::PermissionDenied, "denied"))
            } else {
                Ok(p.file_name().is_some_and(|n| n == ".git"))
            }
        };
        match probe_local_with("/w/proj/sub", None, inner_denied) {
            Probe::Failed { reason } => assert!(reason.contains("sub"), "got {reason}"),
            other => panic!("内层没问成时不能归到外层根，得到 {other:?}"),
        }

        // 🔴 反向：**命中之后的失败不算** —— 命中即返回，根本不会去探更外层。
        //
        // 没有这一条，「更严格」会滑成「一路探到底」：一个无关的外层权限问题又能
        // 推翻一个已经确定的结论。
        let outer_denied = |p: &Path| -> std::io::Result<bool> {
            let s = p.to_string_lossy().into_owned();
            if s.contains("sub") {
                Ok(p.file_name().is_some_and(|n| n == ".git")) // sub/.git 确实在
            } else {
                Err(Error::new(ErrorKind::PermissionDenied, "denied"))
            }
        };
        match probe_local_with("/w/proj/sub", None, outer_denied) {
            Probe::Found { root, source } => {
                assert_eq!(source, RootSource::Git);
                assert!(root.ends_with("sub"), "最近的那个根就是答案，got {root}");
            }
            other => panic!("已经命中就不该再往外探，得到 {other:?}"),
        }

        // 🔴 **marker 那一遍同样适用** —— 两遍是两条独立的循环，改一条不改另一条
        // 不会有任何东西报错。
        //
        // ⚠️ 这条是变异验证逼出来的：上面几段的探测器都在 `.git` 那遍就失败了，
        // **从没走到 marker 那遍**，于是「marker 失败后继续上溯」这个变异全绿。
        let marker_denied = |p: &Path| -> std::io::Result<bool> {
            let s = p.to_string_lossy().into_owned();
            if s.ends_with(".git") {
                return Ok(false); // `.git` 那遍一路答得上来、就是没有
            }
            if s.contains("sub") {
                Err(Error::new(ErrorKind::PermissionDenied, "denied"))
            } else {
                Ok(s.ends_with("Cargo.toml"))
            }
        };
        match probe_local_with("/w/proj/sub", None, marker_denied) {
            Probe::Failed { reason } => assert!(reason.contains("sub"), "got {reason}"),
            other => panic!("marker 那遍内层没问成也不能归到外层，得到 {other:?}"),
        }

        // 反向：一路都答得上来、就是没有 ⇒ `None`（那才是「问了、没有」）。
        assert!(matches!(
            probe_local_with("/w/proj/sub", None, |_: &Path| Ok(false)),
            Probe::None
        ));
    }

    #[test]
    fn an_unmappable_mnt_path_is_failed_not_rootless() {
        // 🔴 换算不出来是「没问成」，不是「没有根」（评审 [P2]）。
        //
        // 原先落到 `probe_local`：`/mnt/d/…` 在 Windows 上是**当前盘根的相对路径**，
        // 于是要么探不到（报 `None`）、要么更糟 —— 误中当前盘上真实存在的 `\mnt\…`。
        // 而 `None` 会被调用方按「确认无根」缓存 24 小时，一次 WSL 超时就让这一族路径
        // 整天归不到根。
        //
        // ⚠️ 只在 Windows 上成立：非 Windows 宿主上 `/mnt/d/…` 是真实本机路径，
        // 该走 `probe_local`。
        if !cfg!(windows) {
            return;
        }
        match probe_path("/mnt/d/work/proj", None, &Vec::new()) {
            Probe::Failed { reason } => {
                assert!(
                    reason.contains("/mnt/d/work/proj"),
                    "要说出是哪条路径：{reason}"
                )
            }
            other => panic!("空挂载表下必须报 Failed（暂时故障档退避），得到 {other:?}"),
        }
        // 表里有它就正常走探测，不该恒 Failed。
        let m = mounts();
        assert!(
            !matches!(
                probe_path("/mnt/d/work/proj", None, &m),
                Probe::Failed { .. }
            ),
            "映射得出来时不该报失败"
        );
    }

    #[test]
    fn is_windows_drive_device_accepts_only_drive_roots() {
        use crate::wsl::is_windows_drive_device;
        assert!(is_windows_drive_device(r"C:\"));
        assert!(is_windows_drive_device("D:/"));
        assert!(is_windows_drive_device("E:"));
        assert!(!is_windows_drive_device("/dev/sda1"));
        assert!(!is_windows_drive_device("tmpfs"));
        assert!(!is_windows_drive_device(r"C:\Users"));
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
        let rep = discover([p.clone(), p.clone(), p], None, &Vec::new());
        assert_eq!(rep.roots.len(), 1);
        std::fs::remove_dir_all(&tmp).ok();
    }
}
