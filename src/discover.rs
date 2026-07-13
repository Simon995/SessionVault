//! 来源发现（§9 discover）。遍历内置描述符，分离发现 transcript 与 snapshot 来源。
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
    /// 状态制品所属项目。工具全局 memory/rules 为 None；项目 instruction/memory
    /// 由宿主提供的身份映射填入。SessionVault 只携带，不计算跨系统身份。
    #[serde(default)]
    pub project_root: Option<String>,
    /// 快照类别（`memory` / `rules` / `instruction`）；append_log 为 None。
    #[serde(default)]
    pub artifact_kind: Option<String>,
}

/// 发现全部内置 provider 的本地来源。
pub fn discover_all() -> Result<Vec<SourceRef>> {
    discover(false)
}

pub fn discover_local() -> Result<Vec<SourceRef>> {
    discover(true)
}

/// 只发现会话 append_log。QuotaBar 会话索引必须使用本接口，避免 snapshot
/// 状态制品污染 `agent_sessions` 投影。
pub fn discover_transcripts() -> Result<Vec<SourceRef>> {
    discover_by_mode(false, SourceMode::AppendLog)
}

pub fn discover_transcripts_local() -> Result<Vec<SourceRef>> {
    discover_by_mode(true, SourceMode::AppendLog)
}

/// 只发现工具配置根内的 Class-B 状态快照。项目 instruction 由
/// [`discover_project_snapshots`] 接收宿主已经算好的项目身份后发现。
pub fn discover_snapshots() -> Result<Vec<SourceRef>> {
    discover_by_mode(false, SourceMode::SnapshotFile)
}

#[derive(Debug, Clone)]
pub struct ProjectSnapshotRoot {
    pub source_location: SourceLocation,
    /// 在来源文件系统命名空间中的可读路径（WSL 时为 POSIX 路径）。
    pub path: PathBuf,
    /// 可选的宿主可访问探测路径（如 Windows 的 WSL UNC）。只用于 `is_file`，
    /// RawEvent 仍记录 `path` 的来源命名空间路径。
    pub probe_path: Option<PathBuf>,
    /// 宿主已经归一的项目身份/物理路径，原样携带给下游。
    pub project_root: String,
}

/// 在宿主已确认的项目根内发现 CLAUDE.md / AGENTS.md。身份仍由宿主算一次；
/// SessionVault 只负责受限路径发现和读取。
pub fn discover_project_snapshots(roots: &[ProjectSnapshotRoot]) -> Vec<SourceRef> {
    let mut out = Vec::new();
    for root in roots {
        for (source_type, rel) in [
            (SourceType::ClaudeCode, "CLAUDE.md"),
            (SourceType::ClaudeCode, ".claude/CLAUDE.md"),
            (SourceType::Codex, "AGENTS.md"),
        ] {
            let path = match &root.source_location {
                SourceLocation::Local => root.path.join(rel),
                SourceLocation::Wsl(_) => PathBuf::from(format!(
                    "{}/{}",
                    root.path.to_string_lossy().trim_end_matches('/'),
                    rel.replace('\\', "/")
                )),
            };
            let probe = root.probe_path.as_ref().map(|base| base.join(rel));
            let exists = match (&root.source_location, probe) {
                (_, Some(path)) => path.is_file(),
                (SourceLocation::Local, None) => path.is_file(),
                (SourceLocation::Wsl(distro), None) => {
                    crate::wsl::stat(distro, &path.to_string_lossy())
                        .ok()
                        .flatten()
                        .is_some()
                }
            };
            if exists {
                out.push(SourceRef {
                    source_type,
                    source_location: root.source_location.clone(),
                    source_mode: SourceMode::SnapshotFile,
                    path,
                    project_root: Some(root.project_root.clone()),
                    artifact_kind: Some("instruction".to_string()),
                });
            }
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

fn discover(local_only: bool) -> Result<Vec<SourceRef>> {
    let mut out = discover_by_mode(local_only, SourceMode::AppendLog)?;
    out.extend(discover_by_mode(local_only, SourceMode::SnapshotFile)?);
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

fn discover_by_mode(local_only: bool, wanted: SourceMode) -> Result<Vec<SourceRef>> {
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
            if art.source_mode != wanted {
                continue;
            }
            let dir = root.join(&art.subdir);
            if !dir.is_dir() {
                continue;
            }
            let files = collect_artifact_files(&dir, &art.glob, art.recursive);
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
                    project_root: None,
                    artifact_kind: artifact_kind(art.source_mode, &art.glob),
                });
            }
        }
    }
    if local_only {
        log::debug!(target: tag::DISCOVER, "skip WSL discovery: local-only mode");
    } else {
        discover_wsl(&mut out, wanted);
    }
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
/// append_log 与 snapshot_file 均经 WSL bridge 读取；sqlite_store 仍未实现。
#[cfg(windows)]
fn discover_wsl(out: &mut Vec<SourceRef>, wanted: SourceMode) {
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
                if art.source_mode != wanted {
                    continue;
                }
                let rel = format!("{base}/{}", art.subdir);
                let Some(suffix) = artifact_suffix(&art.glob) else {
                    log::warn!(target: tag::DISCOVER, "unsupported artifact glob: {}", art.glob);
                    continue;
                };
                let mut files = match crate::wsl::list_files_under_home(distro, &rel, suffix) {
                    Ok(f) => f,
                    Err(e) => {
                        log::debug!(
                            target: tag::DISCOVER,
                            "wsl find failed: distro={distro} rel={rel} err={e}"
                        );
                        continue;
                    }
                };
                if !art.recursive {
                    files.retain(|p| {
                        p.rsplit_once('/')
                            .is_some_and(|(parent, _)| parent.ends_with(&rel))
                    });
                }
                if art.glob == "**/memory/*.md" {
                    files.retain(|p| {
                        p.rsplit_once('/')
                            .is_some_and(|(parent, _)| parent.ends_with("/memory"))
                    });
                }
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
                        project_root: None,
                        artifact_kind: artifact_kind(art.source_mode, &art.glob),
                    });
                }
            }
        }
    }
}

#[cfg(not(windows))]
fn discover_wsl(_out: &mut Vec<SourceRef>, _wanted: SourceMode) {}

fn artifact_kind(mode: SourceMode, glob: &str) -> Option<String> {
    if mode != SourceMode::SnapshotFile {
        return None;
    }
    Some(
        if glob.ends_with(".rules") {
            "rules"
        } else {
            "memory"
        }
        .to_string(),
    )
}

fn artifact_suffix(glob: &str) -> Option<&str> {
    if glob.ends_with(".rules") {
        Some(".rules")
    } else if glob.ends_with(".md") {
        Some(".md")
    } else if glob.ends_with(".jsonl") {
        Some(".jsonl")
    } else {
        None
    }
}

pub fn collect_artifact_files(dir: &Path, glob: &str, recursive: bool) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Some(suffix) = artifact_suffix(glob) else {
        return out;
    };
    collect_files_into(dir, recursive, suffix, &mut out);
    if glob == "**/memory/*.md" {
        out.retain(|p| {
            p.parent()
                .and_then(Path::file_name)
                .and_then(|s| s.to_str())
                == Some("memory")
        });
    }
    out.sort();
    out
}

/// 递归（或单层）收集目录下的 `*.jsonl`。骨架用 std 遍历，不引第三方 glob。
pub fn collect_jsonl(dir: &Path, recursive: bool) -> Vec<PathBuf> {
    collect_artifact_files(dir, "**/*.jsonl", recursive)
}

fn collect_files_into(dir: &Path, recursive: bool, suffix: &str, out: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if recursive {
                collect_files_into(&path, recursive, suffix, out);
            }
        } else if path.to_string_lossy().ends_with(suffix) {
            out.push(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{collect_artifact_files, discover_project_snapshots, ProjectSnapshotRoot};
    use crate::rawevent::SourceLocation;

    #[test]
    fn claude_memory_glob_excludes_other_markdown() {
        let root = std::env::temp_dir().join(format!(
            "svault-discover-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let memory = root.join("encoded").join("memory");
        std::fs::create_dir_all(&memory).unwrap();
        std::fs::write(memory.join("fact.md"), "fact").unwrap();
        std::fs::write(root.join("encoded").join("other.md"), "other").unwrap();
        let files = collect_artifact_files(&root, "**/memory/*.md", true);
        assert_eq!(files, vec![memory.join("fact.md")]);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn project_snapshot_uses_host_probe_but_keeps_source_namespace_path() {
        let probe = std::env::temp_dir().join(format!(
            "svault-project-probe-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&probe).unwrap();
        std::fs::write(probe.join("AGENTS.md"), "rules").unwrap();
        let roots = [ProjectSnapshotRoot {
            source_location: SourceLocation::Wsl("Ubuntu-22.04".into()),
            path: "/home/u/project".into(),
            probe_path: Some(probe.clone()),
            project_root: r"\\wsl.localhost\Ubuntu-22.04\home\u\project".into(),
        }];
        let sources = discover_project_snapshots(&roots);
        assert_eq!(sources.len(), 1);
        assert_eq!(
            sources[0].path.to_string_lossy(),
            "/home/u/project/AGENTS.md"
        );
        assert_eq!(
            sources[0].project_root.as_deref(),
            Some(roots[0].project_root.as_str())
        );
        let _ = std::fs::remove_dir_all(probe);
    }
}
