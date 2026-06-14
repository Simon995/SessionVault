//! 来源发现（§9 discover）。遍历内置描述符的配置根，递归发现 `*.jsonl` 来源。
//!
//! 首次只发现、不读内容（供宿主弹授权）。本机 Local + WSL 各发行版（`Wsl(distro)`）。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::catalog::{self, Status};
use crate::logging::tag;
use crate::rawevent::{SourceLocation, SourceMode, SourceType};
use crate::Result;

/// 一个待扫描来源的引用（发现产物；scan 的入参）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceRef {
    pub source_type: SourceType,
    pub source_location: SourceLocation,
    pub source_mode: SourceMode,
    /// 转录文件绝对路径。
    pub path: PathBuf,
}

/// 发现全部内置 provider 的本地来源。
pub fn discover_all() -> Result<Vec<SourceRef>> {
    let mut out = Vec::new();
    for desc in catalog::builtin_descriptors() {
        let Some(root) = desc.config_dir.as_ref() else {
            log::debug!(
                target: tag::DISCOVER,
                "skip provider: name={} reason=no_config_dir",
                desc.name
            );
            continue;
        };
        for art in &desc.artifacts {
            if matches!(art.status, Status::Planned) {
                continue;
            }
            let dir = root.join(&art.subdir);
            if !dir.is_dir() {
                continue;
            }
            let files = collect_jsonl(&dir, art.recursive);
            log::debug!(
                target: tag::DISCOVER,
                "scanned subdir: provider={} subdir={} files={}",
                desc.name,
                art.subdir,
                files.len()
            );
            for path in files {
                out.push(SourceRef {
                    source_type: desc.source_type,
                    source_location: SourceLocation::Local,
                    source_mode: art.source_mode,
                    path,
                });
            }
        }
    }
    discover_wsl(&mut out);
    log::info!(target: tag::DISCOVER, "discover done: sources={}", out.len());
    Ok(out)
}

/// provider 在发行版内 `$HOME` 下的配置根基名（WSL 约定恒 `~/.claude`/`~/.codex`，
/// 不随 Windows 侧 `CLAUDE_CONFIG_DIR` 覆盖而变）。
fn home_rel_base(source_type: SourceType) -> Option<&'static str> {
    match source_type {
        SourceType::ClaudeCode => Some(".claude"),
        SourceType::Codex => Some(".codex"),
        _ => None,
    }
}

/// 发现各 WSL 用户发行版内的来源（`Wsl(distro)` 标记）。
///
/// Windows 专属：经 `wsl::list_distros` 枚举发行版，对每个 provider 的每个子目录
/// 在发行版内 `find *.jsonl`。非 Windows 构建为 no-op（见 `wsl` 的桩）。
/// 注意：读取这些来源的事件路径尚未接线（见 `scan::scan_source` 的 WSL 短路）。
#[cfg(windows)]
fn discover_wsl(out: &mut Vec<SourceRef>) {
    let distros = match crate::wsl::list_distros() {
        Ok(d) => d,
        Err(e) => {
            log::debug!(target: tag::DISCOVER, "wsl list_distros failed: {e}");
            return;
        }
    };
    for distro in distros.iter().filter(|d| crate::wsl::is_user_distro(d)) {
        for desc in catalog::builtin_descriptors() {
            let Some(base) = home_rel_base(desc.source_type) else {
                continue;
            };
            for art in &desc.artifacts {
                if matches!(art.status, Status::Planned) {
                    continue;
                }
                let rel = format!("{base}/{}", art.subdir);
                let files = match crate::wsl::list_jsonl_under_home(distro, &rel) {
                    Ok(f) => f,
                    Err(e) => {
                        log::debug!(
                            target: tag::DISCOVER,
                            "wsl find failed: distro={distro} rel={rel} err={e}"
                        );
                        continue;
                    }
                };
                log::debug!(
                    target: tag::DISCOVER,
                    "wsl scanned: distro={distro} rel={rel} files={}",
                    files.len()
                );
                for p in files {
                    out.push(SourceRef {
                        source_type: desc.source_type,
                        source_location: SourceLocation::Wsl(distro.clone()),
                        source_mode: art.source_mode,
                        path: PathBuf::from(p),
                    });
                }
            }
        }
    }
}

#[cfg(not(windows))]
fn discover_wsl(_out: &mut Vec<SourceRef>) {}

/// 递归（或单层）收集目录下的 `*.jsonl`。骨架用 std 遍历，不引第三方 glob。
pub fn collect_jsonl(dir: &Path, recursive: bool) -> Vec<PathBuf> {
    let mut out = Vec::new();
    collect_jsonl_into(dir, recursive, &mut out);
    out.sort();
    out
}

fn collect_jsonl_into(dir: &Path, recursive: bool, out: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if recursive {
                collect_jsonl_into(&path, recursive, out);
            }
        } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            out.push(path);
        }
    }
}
