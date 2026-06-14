//! 工程根解析（移植自 QuotaBar `session_index.rs::resolve_project_root`）。
//!
//! 这是从 QuotaBar 抽取的第一个纯函数之一（另一个是 `scan::split_complete_jsonl`）。
//! 判定依据写入 `RawEvent.project_root_source`：git / marker:<file> / cwd / wsl_cwd / missing_cwd。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::pathnorm;

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

/// 从对话记录里的 cwd 解析工程根。
///
/// 入参 `cwd` 应是 [`pathnorm::normalize_cwd`] 的产物（规范形或本机路径）；本函数只在
/// 其上做 marker 上溯，不再自带 UNC 解析（已收敛到 `pathnorm`，见该模块文档）。
///
/// - `cwd` 为 None/空 → `missing_cwd`。
/// - WSL 路径（规范形 `wsl:<distro>:/...` 或 UNC `//wsl$/...`）→ 标 `wsl_cwd`，
///   并在该路径下做 marker 上溯（Windows 宿主上多半 stat 不到，回落 cwd 本身）。
/// - 命中 `.git` → `git`；命中其它 marker → `marker:<file>`；都没有 → `cwd`（用 cwd 本身）。
pub fn resolve_project_root(cwd: Option<&str>) -> ProjectRoot {
    let cwd = match cwd {
        Some(c) if !c.trim().is_empty() => c,
        _ => {
            return ProjectRoot {
                path: None,
                source: "missing_cwd".to_string(),
            }
        }
    };

    // WSL 路径（规范形或 UNC）在 **Windows 宿主上不可本地 stat**：
    // - 规范形 `wsl:distro:/p` 会被 `PathBuf` 当成相对路径，`find_upward` 会误walk
    //   进程 CWD（很可能是个 git 仓库）而误判 project_root —— 真实碰到过的坑。
    // - 跨发行版的 marker 上溯属于**访问桥**（经 `\\wsl$\` stat，Windows 专属，未实装）。
    // 故 v0 直接回落 `wsl_cwd`，不做本地 find_upward。
    if pathnorm::split_canonical_wsl(cwd).is_some() || pathnorm::canonical_wsl_unc(cwd).is_some() {
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
        assert_eq!(resolve_project_root(None).source, "missing_cwd");
        assert_eq!(resolve_project_root(Some("   ")).source, "missing_cwd");
    }

    #[test]
    fn labels_wsl_paths_as_wsl_cwd() {
        // UNC 形与规范形都该被标 wsl_cwd（无 marker 时回落 cwd 本身）。
        // 用「不存在的发行版 + 不存在的路径」保证 find_upward 必然落空，
        // 不依赖本机是否真有某发行版可达（曾因 \\wsl$\Ubuntu 实际可达而误命中 .git）。
        let unc = resolve_project_root(Some("//wsl$/NoSuchDistro_xyz/nonexistent-abc-123"));
        assert_eq!(unc.source, "wsl_cwd");
        assert!(unc.path.is_some());

        let canonical = resolve_project_root(Some("wsl:NoSuchDistro_xyz:/nonexistent-abc-123"));
        assert_eq!(canonical.source, "wsl_cwd");
        assert!(canonical.path.is_some());
    }
}
