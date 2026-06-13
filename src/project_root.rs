//! 工程根解析（移植自 QuotaBar `session_index.rs::resolve_project_root`）。
//!
//! 这是从 QuotaBar 抽取的第一个纯函数之一（另一个是 `scan::split_complete_jsonl`）。
//! 判定依据写入 `RawEvent.project_root_source`：git / marker:<file> / cwd / wsl_cwd / missing_cwd。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

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
/// - `cwd` 为 None/空 → `missing_cwd`。
/// - WSL 规范路径（`//wsl$/<distro>/...` 或 `\\wsl.localhost\...`）→ 标 `wsl_cwd`，
///   并在该路径下做 marker 上溯。
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

    let is_wsl = split_canonical_wsl_cwd(cwd).is_some();
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
        source: if is_wsl { "wsl_cwd" } else { "cwd" }.to_string(),
    }
}

/// 识别 WSL 规范路径并拆出 `(distro, rest)`；非 WSL 返回 None。
///
/// 支持 `//wsl$/<distro>/...`、`\\wsl$\<distro>\...`、`\\wsl.localhost\<distro>\...`。
pub fn split_canonical_wsl_cwd(cwd: &str) -> Option<(String, String)> {
    let norm = cwd.replace('\\', "/");
    let rest = norm
        .strip_prefix("//wsl$/")
        .or_else(|| norm.strip_prefix("//wsl.localhost/"))?;
    let mut parts = rest.splitn(2, '/');
    let distro = parts.next()?.to_string();
    if distro.is_empty() {
        return None;
    }
    let tail = parts.next().unwrap_or("").to_string();
    Some((distro, tail))
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
    fn detects_wsl_canonical_paths() {
        assert_eq!(
            split_canonical_wsl_cwd("//wsl$/Ubuntu/home/me/proj"),
            Some(("Ubuntu".to_string(), "home/me/proj".to_string()))
        );
        assert_eq!(
            split_canonical_wsl_cwd(r"\\wsl.localhost\Debian\srv"),
            Some(("Debian".to_string(), "srv".to_string()))
        );
        assert_eq!(split_canonical_wsl_cwd("C:/Users/me/proj"), None);
    }

    #[test]
    fn falls_back_to_cwd_when_no_marker() {
        // 用一个几乎不可能含 marker 的临时路径。
        let r = resolve_project_root(Some("//wsl$/Ubuntu/nonexistent-xyz-123"));
        assert_eq!(r.source, "wsl_cwd");
        assert!(r.path.is_some());
    }
}
