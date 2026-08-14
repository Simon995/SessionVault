//! 工程根解析（移植自 QuotaBar `session_index.rs::resolve_project_root`）。
//!
//! 这是从 QuotaBar 抽取的第一个纯函数之一（另一个是 `scan::split_complete_jsonl`）。
//! 判定依据写入 `RawEvent.project_root_source`：git / marker:<file> / cwd / wsl_cwd / missing_cwd。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::pathnorm::{self, HostPlatform};

/// 工程根标记文件（命中即视为工程根；顺序即优先级展示，实际取最近祖先）。
///
/// 🔴 **这是唯一的清单。** `wsl.rs::find_project_root` 的 shell 脚本从这里生成 ——
/// 它曾硬抄一份，而两份都活着的时候往这里加一个 marker 不会有任何东西报错：
/// 本机路径认得它、WSL 里的同一个项目认不得，症状是「同一个项目在 Windows 侧是
/// 一个根、在 WSL 侧不是」。`.git` 由两处的**第一遍**单独走（全局优先于 marker），
/// 所以两处都把它从 marker 那一遍滤掉。
///
/// 🔴 **`CLAUDE.md` / `AGENTS.md` 在列，理由和其它六个不同**（2026-08-14）。
///
/// 前六个是**开发者产物**。一个用 Claude Code 写作、记笔记、整理数据的人一个都
/// 不会命中 ⇒ 他的**全部**用量都进不了项目页。实测本机就有这一族
/// （`Dropbox\工作笔记`、`Documents\Codex\…`）：19,553 事件 / 50 会话。
///
/// 而 agent 指令文件是个**强信号且几乎不会误判**：用户亲手为那个文件夹写过说明，
/// 等于已经说过「这是我的一个工作区」。它还有两个 QuotaBar 侧声明比不了的性质
/// —— **跟着文件夹走**（换机器、重装都在），**别的工具也看得见**。
///
/// ⚠️ 它治标不治本：只用 Claude Code 写小说、一个 `.md` 都没写过的用户仍然什么都
/// 没有。根治是让用户显式声明（`RootSource::Configured`）+ 项目页诚实地分出
/// 「其它文件夹」一档，见 MASTER_PLAN 的 A/C 两项。
///
/// ⚠️ **别加 `README.md`**：到处都是，会把每一个子目录都变成「项目」。
pub const MARKERS: [&str; 8] = [
    ".git",
    "Cargo.toml",
    "package.json",
    "pyproject.toml",
    "go.mod",
    ".hg",
    "CLAUDE.md",
    "AGENTS.md",
];

/// 工程根解析结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectRoot {
    pub path: Option<PathBuf>,
    /// 判定依据：`git` / `marker:<file>` / `cwd` / `wsl_cwd` / `missing_cwd`。
    pub source: String,
}

/// 从对话记录里的 cwd 解析工程根（宿主感知）。
///
/// 入参 `cwd` 应是 [`pathnorm::normalize_cwd`] 的产物（规范形或本机路径）；本函数只在
/// 其上做 marker 上溯，不再自带 UNC 解析（已收敛到 `pathnorm`，见该模块文档）。
/// `host` 决定**裸 Unix 绝对路径**是否可做本地 marker 上溯（见下）。
///
/// - `cwd` 为 None/空 → `missing_cwd`。
/// - WSL 路径（规范形 `wsl:<distro>:/...` 或 UNC `//wsl$/...`）→ 标 `wsl_cwd`，不上溯。
/// - **Windows 宿主上的裸 Linux 绝对路径**（`/home/...`，distro 未知未能打标）→ 同样标
///   `wsl_cwd`、不上溯：它本质是 WSL 路径，本地 `PathBuf` 会把它当当前盘根相对路径，
///   `find_upward` 会去错盘 walk marker（甚至命中无关仓库）——正是要避免的误判。
///   （Unix 宿主上同一个 `/home/...` 是**真实本机路径**，照常上溯。）
/// - 其余本机路径：命中 `.git` → `git`；命中其它 marker → `marker:<file>`；都没有 → `cwd`。
pub fn resolve_project_root(cwd: Option<&str>, host: HostPlatform) -> ProjectRoot {
    let cwd = match cwd {
        Some(c) if !c.trim().is_empty() => c,
        _ => {
            return ProjectRoot {
                path: None,
                source: "missing_cwd".to_string(),
            }
        }
    };

    // 不可本地 stat 的 WSL 路径一律回落 `wsl_cwd`、不做 find_upward：
    // ① 规范形 `wsl:distro:/p` / UNC `//wsl$/..`（与宿主无关，恒是 WSL）；
    // ② Windows 宿主上 distro 未知的裸 Linux 路径 `/home/..`（host-dependent：
    //    Unix 宿主上它是真实本机路径，不在此列）。
    // 跨发行版的真实 marker 上溯需经访问桥 stat（`wsl.rs` 已有 stat/read，但 project_root
    // 暂不为 WSL 做逐级 marker 上溯——直接回落 wsl_cwd，避免本地误判）。
    let is_unstattable_wsl = pathnorm::split_canonical_wsl(cwd).is_some()
        || pathnorm::canonical_wsl_unc(cwd).is_some()
        || (host == HostPlatform::Windows && pathnorm::is_bare_linux_path(cwd));
    if is_unstattable_wsl {
        return ProjectRoot {
            path: Some(PathBuf::from(cwd)),
            source: "wsl_cwd".to_string(),
        };
    }

    let base = PathBuf::from(cwd);
    if let Some((dir, marker)) = find_upward(&base) {
        let source = if marker == ".git" {
            "git".to_string()
        } else {
            format!("marker:{marker}")
        };
        return ProjectRoot {
            path: Some(dir),
            source,
        };
    }

    ProjectRoot {
        path: Some(base),
        source: "cwd".to_string(),
    }
}

/// 从 `start` 向上逐级找最近的 marker 命中，返回 `(命中目录, marker 文件名)`。
///
/// 🔴 **探测失败当作「这一层没有 marker」**，继续上溯。这与 `discovery::probe_local_with`
/// 的规则（ADR-051 §5 规则 ③：没问成就停下）**故意不同** —— 本模块是 ADR-050 之前的
/// 旧解析器，**已不在任何生产路径上**（唯一消费者是 `examples/project_root_scope.rs`，
/// 它存在的目的正是量出旧行为的影响范围）。改成三态会让那个诊断量的不再是它要量的
/// 东西；而把它接回生产路径是 ADR-050 明确要消除的方向。
///
/// 探测经 [`crate::probe`] 而不是 `Path::exists()`：**折叠这件事要写出来**，
/// 不能由一个看不出取舍的 `.exists()` 顺手完成。
fn find_upward(start: &Path) -> Option<(PathBuf, &'static str)> {
    use crate::probe::{ProbeBackend, Probed};
    let mut cur = Some(start);
    while let Some(dir) = cur {
        for marker in MARKERS {
            match crate::probe::LocalBackend::unanchored()
                .probe(&dir.join(marker), crate::deadline::Deadline::unbounded())
            {
                Probed::Found(_) => return Some((dir.to_path_buf(), marker)),
                // 旧解析器的既有行为：两者都当「这一层没有」。见上面的说明。
                Probed::Absent | Probed::Unknown(_) => {}
            }
        }
        cur = dir.parent();
    }
    None
}

#[cfg(test)]
// 测试要造 fixture（建目录、写文件、再核一遍），允许直接碰盘 —— 文件系统边界
// 管的是**生产行为**，而 `#[cfg(test)]` 不在生产路径上。
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    #[test]
    fn missing_cwd_when_none_or_blank() {
        assert_eq!(
            resolve_project_root(None, HostPlatform::Unix).source,
            "missing_cwd"
        );
        assert_eq!(
            resolve_project_root(Some("   "), HostPlatform::Unix).source,
            "missing_cwd"
        );
    }

    #[test]
    fn labels_wsl_paths_as_wsl_cwd() {
        // UNC 形与规范形都该被标 wsl_cwd（无 marker 时回落 cwd 本身），与宿主无关。
        // 用「不存在的发行版 + 不存在的路径」保证 find_upward 必然落空，
        // 不依赖本机是否真有某发行版可达（曾因 \\wsl$\Ubuntu 实际可达而误命中 .git）。
        for host in [HostPlatform::Windows, HostPlatform::Unix] {
            let unc =
                resolve_project_root(Some("//wsl$/NoSuchDistro_xyz/nonexistent-abc-123"), host);
            assert_eq!(unc.source, "wsl_cwd");
            assert!(unc.path.is_some());

            let canonical =
                resolve_project_root(Some("wsl:NoSuchDistro_xyz:/nonexistent-abc-123"), host);
            assert_eq!(canonical.source, "wsl_cwd");
            assert!(canonical.path.is_some());
        }
    }

    #[test]
    fn bare_linux_path_is_host_dependent() {
        // 用「不存在的根目录」下的路径，保证两种宿主上祖先都无 marker（避免命中真实
        // Linux 上 /home/<user>/.git 之类 dotfiles 仓库）。
        let p = "/nonexistent-root-xyz-abc/sub/dir";

        // Windows 宿主：distro 未知的裸 Linux 路径不做本地上溯，直接 wsl_cwd
        //（否则会被当当前盘根相对路径去错盘 walk marker —— P2 修复点）。
        let win = resolve_project_root(Some(p), HostPlatform::Windows);
        assert_eq!(win.source, "wsl_cwd");
        assert_eq!(win.path.as_deref(), Some(Path::new(p)));

        // Unix 宿主：同一路径是真实本机路径，照常上溯（祖先无 marker → 回落 cwd）。
        let nix = resolve_project_root(Some(p), HostPlatform::Unix);
        assert_eq!(nix.source, "cwd");
    }
}
