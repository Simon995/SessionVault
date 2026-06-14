//! 工程根解析（移植自 QuotaBar `session_index.rs::resolve_project_root`）。
//!
//! 这是从 QuotaBar 抽取的第一个纯函数之一（另一个是 `scan::split_complete_jsonl`）。
//! 判定依据写入 `RawEvent.project_root_source`：git / marker:<file> / cwd / wsl_cwd / missing_cwd。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::pathnorm::{self, HostPlatform};

/// 工程根标记文件（命中即视为工程根；顺序即优先级展示，实际取最近祖先）。
pub const MARKERS: [&str; 6] = [
    ".git",
    "Cargo.toml",
    "package.json",
    "pyproject.toml",
    "go.mod",
    ".hg",
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
fn find_upward(start: &Path) -> Option<(PathBuf, &'static str)> {
    let mut cur = Some(start);
    while let Some(dir) = cur {
        for marker in MARKERS {
            if dir.join(marker).exists() {
                return Some((dir.to_path_buf(), marker));
            }
        }
        cur = dir.parent();
    }
    None
}

#[cfg(test)]
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
