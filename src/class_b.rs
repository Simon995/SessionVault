//! Class-B（状态制品）快照来源的枚举 —— **2026-08-16 从 QuotaBar 搬过来的**。
//!
//! Class-B 是「工具的指令/记忆文件」：`CLAUDE.md` / `AGENTS.md` /
//! `~/.claude/projects/<enc>/memory/*.md` 之类。与 Class-A（会话转录）相对。
//!
//! ## 为什么住在这里
//!
//! 这段编排从前在 QuotaBar 的 `memory/sources.rs`，而**它用到的每一个原语本来
//! 就在 SessionVault**：根枚举（[`crate::memory_roots`]）、项目目录名解码
//! （[`crate::project_dir`]）、「该问谁」（[`crate::discover::HostProbe`]）、
//! 快照发现（[`crate::discover_snapshots`]）。宿主只是在外面把它们拼起来 ——
//! 而那份「拼法」正是证据层自己的知识。
//!
//! 后果是具体的：**Python 侧够不着它**。TumeFlow 要读 Class-B 快照，得先有人
//! 把它们同步进总库，而同步要 `TotalStore::sync_snapshots` 这个 Rust 库 API。
//! 于是「记忆的证据层什么时候刷新」被绑在了「QuotaBar 有没有在跑」上 ——
//! 只装 TumeChat 的用户永远刷不到。搬过来之后 `svault sync-snapshots` 就是出口。
//!
//! ## 三态贯穿始终
//!
//! 🔴 **`unreachable` 与「那里没有素材」是两件事**，而这条链上每一层都能把前者
//! 折叠成后者：`read_dir` 失败 `continue`、逐项错误 `flatten()` 吞掉、整次发现
//! 失败 `unwrap_or_default()`。任何一处折叠，下游看到的都只是「这个位置没有快照」
//! —— 与「那里确实没有 CLAUDE.md」无从分辨，而后果是**一个项目的指令文件静默
//! 不进蒸馏，界面上什么都不会说**。
//!
//! ⚠️ `NotFound` **是**事实：这个位置压根没装 Claude ⇒ 没有 `projects/` 目录。
//! 报成不可达会让每个只装了一半工具链的用户永久带着一个假故障。

use std::path::{Path, PathBuf};

use crate::deadline::Deadline;
use crate::discover::{HostProbe, ProjectSnapshotRoot, SourceRef};
use crate::probe::{self, Probed};
use crate::project_dir::{decode_project_dir, DecodedProject};
use crate::{SourceLocation, SourceType};

/// 枚举结果。**两半都要**：只给 `sources` 的调用方无法区分「没有」与「没问成」。
#[derive(Debug, Default)]
pub struct ClassBSources {
    pub sources: Vec<SourceRef>,
    /// 没问成的位置或项目根。调用方**不得**读作「这里没有 CLAUDE.md / 记忆」。
    pub unreachable: Vec<String>,
}

/// 探测项目根的上限。
///
/// 一个停掉的 WSL 发行版会让 UNC 探测挂很久，而这条路径要遍历**每个**项目根。
/// 没有上限时一次枚举可以跑到天荒地老，而调用方（同步任务）只会看起来卡住。
fn probe_deadline() -> Deadline {
    Deadline::after(std::time::Duration::from_secs(20))
}

#[derive(Clone)]
struct SnapshotProject {
    encoded: String,
    location: SourceLocation,
    read_path: PathBuf,
    /// 宿主侧怎么探这个项目根 —— **含「该问谁」**，不只是一条路径。
    /// 在枚举时就定下来，因为 `fs_prefix` 只有这里有。
    host_probe: HostProbe,
    project_root: String,
}

/// PHYSICAL 路径 → 该 root **会话内**记录的形式。
///
/// 一个 WSL 会话把自己的 cwd 记成 Linux 路径（`/home/u/proj`），而宿主看见的是
/// UNC 物理路径（`\\wsl.localhost\<distro>\home\u\proj`）。两种写法指同一个项目，
/// 而**它们之间的换算只有握着 `fs_prefix` 的这一层做得了**。
///
/// `None` = 换算不适用：本地 root（前缀为空，会话路径本就是物理路径），或者这个
/// 物理路径根本不在该 root 下。**不猜** —— 猜错会把一个项目的记忆挂到另一个项目上。
pub fn in_session_path(physical: &str, fs_prefix: &str) -> Option<String> {
    if fs_prefix.is_empty() {
        return None;
    }
    let norm = |s: &str| s.replace('\\', "/");
    let (p, prefix) = (norm(physical), norm(fs_prefix));
    let rest = p.strip_prefix(&prefix)?;
    let rest = rest.trim_start_matches('/');
    Some(format!("/{rest}"))
}

/// 快照事件里携带的**项目身份** —— 与物理路径分开的那一半。
///
/// 🔴 **两个出口必须给出同一个身份。** `roots` 那侧走
/// [`crate::attribution::registry_key`]，它对 `/mnt/<x>/…` 是
/// `match mnt_to_windows(..) { Some(host) => .. }` —— **用了返回值**。
/// 本函数存在的全部理由就是在这一侧做同一件事。
///
/// 此前这里根本没有这一步：事件直接携带**物理路径**（发行版内的 `/mnt/c/…`），
/// 而 `roots` 给的是 `C:\…` ⇒ 消费方拿事件里的写法去认身份**查不到**，把同一个仓
/// 当成一个新项目单独蒸馏，产出的写入落点还从宿主够不着（2026-08-27 实机）。
///
/// ⚠️ **物理路径不能一起换** —— `read_path` 与 [`crate::discover::HostProbe`]
/// 靠它，而 `C:\…` 在发行版命名空间里没有意义。这正是两者必须分开的原因。
///
/// ⚠️ 挂载表说不出话（WSL 没跑 / 不是盘挂载）⇒ **原样返回**，不按形状猜。
/// 理由与 [`crate::pathnorm::mnt_to_windows`] 那段逐字相同。
fn snapshot_project_identity(physical: &str, mounts: &crate::pathnorm::DriveMounts) -> String {
    crate::pathnorm::mnt_to_windows(physical, mounts).unwrap_or_else(|| physical.to_string())
}

/// 从一个**物理路径**装出一条 `SnapshotProject` —— 身份与物理路径在这里分岔。
///
/// 🔴 **抽成函数是为了让「分岔」这件事可测**（2026-08-27）。此前这段是
/// `enumerate` 里的内联代码，而 `enumerate` 要遍历真实文件系统 ⇒ 单测够不着 ⇒
/// 「算了身份却没存进去」这种接线错误**没有任何测试挡得住**（变异验证当场证明：
/// 把 `project_root: identity` 改回 `project_root`，全套测试照旧全绿）。
///
/// 三个字段的来源必须分开看：
///
/// | 字段 | 用什么 | 为什么 |
/// | --- | --- | --- |
/// | `read_path` | **物理路径**（发行版内） | 真的要拿它去读文件 |
/// | `host_probe` | **物理路径** + `fs_prefix` | `WslUnc` 拿 distro 与前缀去拼 |
/// | `project_root` | **身份**（挂载表换算后） | 消费方拿它去 `roots` 认项目 |
fn snapshot_project(
    encoded: String,
    location: SourceLocation,
    physical: &str,
    fs_prefix: &str,
    mounts: &crate::pathnorm::DriveMounts,
) -> SnapshotProject {
    SnapshotProject {
        encoded,
        location,
        read_path: PathBuf::from(
            in_session_path(physical, fs_prefix).unwrap_or_else(|| physical.to_string()),
        ),
        // 🔴 **在这里就把「该问谁」定下来** —— `fs_prefix` 只有这一层有，
        // 而下游拿不到它。从前只传一条路径，WSL 根的 UNC 形式于是被无条件送进
        // 宿主后端；宿主沿 9P 跟不进符号链接 ⇒ 恒真的「没问成」。
        host_probe: HostProbe::for_root(physical, fs_prefix),
        // ⚠️ 存的是**身份**，不是物理路径 —— 两者的差别就是这次订正的全部内容。
        project_root: snapshot_project_identity(physical, mounts),
    }
}

fn source_location(location: &str) -> SourceLocation {
    location
        .strip_prefix("wsl-")
        .map(|distro| SourceLocation::Wsl(distro.to_string()))
        .unwrap_or(SourceLocation::Local)
}

/// Claude 的 project identity：`…/projects/<enc>/memory/…` 里的 `<enc>` 对应哪个
/// 真实项目根。SessionVault 只**携带**这个身份，不跨系统计算它。
fn attach_project_identities(sources: &mut [SourceRef], projects: &[SnapshotProject]) {
    for source in sources {
        if source.source_type != SourceType::ClaudeCode {
            continue;
        }
        let normalized = source.path.to_string_lossy().replace('\\', "/");
        let Some((_, rest)) = normalized.split_once("/projects/") else {
            continue;
        };
        let Some((encoded, _)) = rest.split_once("/memory/") else {
            continue;
        };
        if let Some(project) = projects
            .iter()
            .find(|p| p.encoded == encoded && p.location == source.source_location)
        {
            source.project_root = Some(project.project_root.clone());
        }
    }
}

/// 枚举本机能看到的全部 Class-B 快照来源（本机 + 每个 WSL 发行版）。
/// `mounts` **显式传入**（不在函数里读）—— 与 `crate::project_root_registry` 同一条
/// 理由：发现侧和归属侧必须用**同一份**运行期事实，各读一次可以拿到两份
/// （中途 `wsl --shutdown` 就变）。它决定 `/mnt/<drive>/…` 的标识去不去 WSL 前缀。
pub fn enumerate(mounts: &crate::pathnorm::DriveMounts) -> ClassBSources {
    // 根枚举与项目探测共用同一个上限：一个停掉的发行版会在两处都挂住，
    // 而两个独立的上限意味着最坏情况是它们之和。
    let found = crate::memory_roots::enumerate(None, probe_deadline());
    // 🔴 根本身就没数全时，**先把这件事带上**。此前这条路径拿到的是一个更短的
    // 根列表，而下游只会看到「这个位置没有快照」—— 与「那里确实没有」无从分辨。
    let mut unreachable: Vec<String> = found
        .unreachable
        .iter()
        .map(|u| format!("{} (root enumeration): {}", u.location, u.reason))
        .collect();

    let mut projects: Vec<SnapshotProject> = Vec::new();
    for root in &found.roots {
        let location = source_location(&root.location);
        let projects_dir = Path::new(&root.claude_home).join("projects");
        // 🔴 **`NotFound` 是事实，其余每一种 IO 错误都是「没问成」。**
        let entries = match probe::read_dir_entries(&projects_dir, None) {
            Probed::Found(entries) => entries,
            Probed::Absent => continue,
            Probed::Unknown(e) => {
                unreachable.push(e.to_string());
                continue;
            }
        };
        for entry in entries {
            // 逐项失败同样是「没问成」—— `flatten()` 会把它变成「这一项不存在」。
            let entry = match entry {
                Ok(entry) => entry,
                Err(e) => {
                    unreachable.push(format!("{}: {e}", projects_dir.display()));
                    continue;
                }
            };
            let Some(encoded) = entry.file_name.to_str().map(str::to_string) else {
                continue;
            };
            let project_root = match decode_project_dir(&encoded, &root.fs_prefix, mounts) {
                DecodedProject::Found(p) => p,
                DecodedProject::Absent => continue,
                DecodedProject::Unresolvable(why) => {
                    unreachable.push(why);
                    continue;
                }
            };
            projects.push(snapshot_project(
                encoded,
                location.clone(),
                &project_root,
                &root.fs_prefix,
                mounts,
            ));
        }
    }

    // 整次快照发现失败 ⇒ **不是**「一个快照来源都没有」。
    let mut sources = match crate::discover_snapshots() {
        Ok(found) => found,
        Err(e) => {
            unreachable.push(format!("snapshot discovery failed: {e}"));
            Vec::new()
        }
    };
    attach_project_identities(&mut sources, &projects);

    let project_roots: Vec<_> = projects
        .iter()
        .map(|p| ProjectSnapshotRoot {
            source_location: p.location.clone(),
            path: p.read_path.clone(),
            host_probe: Some(p.host_probe.clone()),
            project_root: p.project_root.clone(),
        })
        .collect();
    // 🔴 **「探测失败」不许长成「这个项目没写过 CLAUDE.md」**：两者压成同一个
    // 「不在列表里」的后果是一个项目的指令文件静默不进蒸馏。
    let probed = crate::discover_project_snapshots(&project_roots, probe_deadline());
    unreachable.extend(
        probed
            .unreachable
            .iter()
            .map(|u| format!("{} ({})", u.project_root, u.reason)),
    );
    sources.extend(probed.sources);

    sources.sort_by(|a, b| {
        a.source_location
            .as_key()
            .cmp(&b.source_location.as_key())
            .then(a.path.cmp(&b.path))
    });
    sources.dedup_by(|a, b| {
        a.source_type == b.source_type && a.source_location == b.source_location && a.path == b.path
    });
    ClassBSources {
        sources,
        unreachable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_session_path_declines_rather_than_guessing() {
        // 本地根：会话路径本就是物理路径，没有换算可做。
        assert_eq!(in_session_path("C:/x/proj", ""), None);
        // 物理路径不在这个根下 ⇒ **不猜**。猜错会把记忆挂到另一个项目上。
        assert_eq!(
            in_session_path("C:/x/proj", r"\\wsl.localhost\Ubuntu"),
            None
        );
        // 正常换算：UNC 物理形 → 发行版内部的 POSIX 形。
        assert_eq!(
            in_session_path(
                r"\\wsl.localhost\Ubuntu\home\u\proj",
                r"\\wsl.localhost\Ubuntu"
            ),
            Some("/home/u/proj".to_string())
        );
    }

    /// 测试用的挂载表 —— `/mnt/c` 确认是 Windows 的 C 盘。
    fn mounts_with_c() -> crate::pathnorm::DriveMounts {
        vec![("/mnt/c".to_string(), r"C:\".to_string())]
    }

    /// 🔴 **身份要用挂载表算出来的那个值，不是只拿它当谓词。**
    ///
    /// `roots` 那侧走 `attribution::registry_key`，它是
    /// `match mnt_to_windows(..) { Some(host) => .. }` —— 用了返回值。
    /// 本侧此前**根本没有这一步**，事件直接携带物理路径 `/mnt/c/…`。
    #[test]
    fn the_identity_is_the_mount_tables_answer() {
        let mounts = mounts_with_c();
        let authority = crate::pathnorm::mnt_to_windows("/mnt/c/Users/dev/proj", &mounts)
            .expect("挂载表里有 /mnt/c");
        assert_eq!(
            snapshot_project_identity("/mnt/c/Users/dev/proj", &mounts),
            authority
        );
    }

    /// 🔴 **身份换算了，物理路径没有** —— 这条同时钉住两半，也钉住**接线**。
    ///
    /// ⚠️ 上一版这里写的是
    /// `registry_key(from_snapshot) == registry_key(r"C:\Users\dev\proj")`，
    /// 想表达「两个出口归到同一个身份」。**那是一条假护栏**：`registry_key` 自己
    /// 就会把 `/mnt/c/…` 换算掉，所以加不加这次修复它都绿（变异验证当场证明
    /// `rc=0`）。真正要断言的是**逐字**的形态 —— 消费方是拿事件里的写法直接去查
    /// `roots` 发布的那张表，不会先跑一遍 `registry_key`。
    ///
    /// ⚠️ 而「算了身份却没存进去」这种**接线**错误，只有从构造函数这一层看才挡得住：
    /// 变异把 `project_root: snapshot_project_identity(..)` 改回物理路径时，
    /// 只测函数的那几条一条都不红。
    #[test]
    fn the_project_carries_the_identity_but_reads_the_physical_path() {
        let p = snapshot_project(
            "-mnt-c-Users-dev-proj".into(),
            SourceLocation::Wsl("D".into()),
            "/mnt/c/Users/dev/proj",
            r"\\wsl.localhost\D",
            &mounts_with_c(),
        );
        assert_eq!(
            p.project_root, r"C:\Users\dev\proj",
            "事件携带的是**身份** —— 要与 roots 发布的写法逐字一致"
        );
        assert_eq!(
            p.read_path,
            PathBuf::from("/mnt/c/Users/dev/proj"),
            "读文件用的仍是**物理路径** —— C:\\… 在发行版命名空间里没有意义"
        );
    }

    /// **成对的另一半**：挂载表说不出话 ⇒ 原样返回，不按形状猜。
    ///
    /// 一个「见到 `/mnt/<单字母>` 就换算」的实现同样能让上面两条通过，
    /// 而它在 `automount.root` 改过、或 `/mnt/data` 这类普通挂载上会把事件归到
    /// 一个**别的项目**（甚至一个不存在的盘）名下。
    #[test]
    fn without_the_mount_table_the_identity_is_unchanged() {
        assert_eq!(
            snapshot_project_identity("/mnt/c/Users/dev/proj", &Vec::new()),
            "/mnt/c/Users/dev/proj"
        );
        assert_eq!(
            snapshot_project_identity("/mnt/data/proj", &mounts_with_c()),
            "/mnt/data/proj",
            "/mnt/data 不在表里 —— 它是普通 Linux 挂载，不是盘"
        );
    }

    /// **成对的另一半**：发行版内的路径不该被动。
    #[test]
    fn an_in_distro_identity_is_untouched() {
        assert_eq!(
            snapshot_project_identity("/home/dev/proj", &mounts_with_c()),
            "/home/dev/proj"
        );
    }

    #[test]
    fn source_location_reads_the_root_tag() {
        assert_eq!(source_location("local"), SourceLocation::Local);
        assert_eq!(
            source_location("wsl-Ubuntu-22.04"),
            SourceLocation::Wsl("Ubuntu-22.04".into())
        );
    }

    #[test]
    fn identity_is_attached_only_to_the_matching_root_and_location() {
        // 🔴 `encoded` 相同但**位置不同**不许互相认领 —— 本机与 WSL 里可以有
        // 同名的编码目录，认错的后果是一个项目的记忆挂到另一台机器的项目上。
        let projects = vec![SnapshotProject {
            encoded: "C--x-proj".into(),
            location: SourceLocation::Local,
            read_path: PathBuf::from("C:/x/proj"),
            host_probe: HostProbe::for_root("C:/x/proj", ""),
            project_root: "C:/x/proj".into(),
        }];
        let mut sources = vec![
            SourceRef {
                source_type: SourceType::ClaudeCode,
                source_location: SourceLocation::Local,
                source_mode: crate::SourceMode::SnapshotFile,
                path: PathBuf::from("C:/u/.claude/projects/C--x-proj/memory/MEMORY.md"),
                project_root: None,
                artifact_kind: Some("memory".into()),
            },
            SourceRef {
                source_type: SourceType::ClaudeCode,
                source_location: SourceLocation::Wsl("Ubuntu".into()),
                source_mode: crate::SourceMode::SnapshotFile,
                path: PathBuf::from("/home/u/.claude/projects/C--x-proj/memory/MEMORY.md"),
                project_root: None,
                artifact_kind: Some("memory".into()),
            },
        ];
        attach_project_identities(&mut sources, &projects);
        assert_eq!(sources[0].project_root.as_deref(), Some("C:/x/proj"));
        assert_eq!(sources[1].project_root, None, "位置不同不许认领");
    }
}
