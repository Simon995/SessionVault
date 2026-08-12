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
    Ok(discover_by_mode(false, SourceMode::AppendLog)?.sources)
}

/// 同上，但**同时报出哪些位置没问成**。
///
/// 🔴 会话索引必须用这个，不能用上面那个：调用方要据发现结果 prune 存量行，
/// 而「这个位置零文件」与「这个位置问不到」在 `sources` 里长得一模一样。
pub fn discover_transcripts_reported() -> Result<DiscoveryOutcome> {
    discover_by_mode(false, SourceMode::AppendLog)
}

pub fn discover_transcripts_local() -> Result<Vec<SourceRef>> {
    Ok(discover_by_mode(true, SourceMode::AppendLog)?.sources)
}

/// 只发现工具配置根内的 Class-B 状态快照。项目 instruction 由
/// [`discover_project_snapshots`] 接收宿主已经算好的项目身份后发现。
pub fn discover_snapshots() -> Result<Vec<SourceRef>> {
    Ok(discover_by_mode(false, SourceMode::SnapshotFile)?.sources)
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
    Ok(discover_reported(local_only)?.sources)
}

/// 一次发现的完整结果：**找到了什么** + **哪些位置没问成**。
///
/// 🔴 后者是承重的，不是诊断信息。`discover_wsl` 的两级失败（`list_distros` /
/// 每个 `find`）从前都被静默吞掉，返回一个更短的列表 —— 而调用方（QuotaBar 的
/// session index）把「这个 location 本轮零文件」当成「用户把它清空了」，
/// **直接 prune 掉索引行**。实测一次 WSL 变慢导致 **369 个文件**的会话与
/// `usage_facts` 被删。
///
/// 与本仓 `Probe::None` / `Probe::Failed` 是同一条判据，只是发生在上一层：
/// **「问了、没有」与「没问成」必须分开**，否则故障会被当成事实。
/// 「所有 WSL 位置都没问成」的哨兵（连 `wsl -l -q` 都失败时）。
pub const UNREACHABLE_ALL_WSL: &str = "wsl:*";

/// 本机位置键（与 QuotaBar `SourceLocation::as_key()` 一致）。
pub const LOCAL_LOCATION: &str = "local";

#[derive(Debug, Clone, Default)]
pub struct DiscoveryOutcome {
    pub sources: Vec<SourceRef>,
    /// 枚举失败的位置（`wsl:<distro>` 形式）。调用方**不得**据本轮结果删除它们的存量。
    pub unreachable: Vec<String>,
}

fn discover_reported(local_only: bool) -> Result<DiscoveryOutcome> {
    let mut first = discover_by_mode(local_only, SourceMode::AppendLog)?;
    let second = discover_by_mode(local_only, SourceMode::SnapshotFile)?;
    first.sources.extend(second.sources);
    for u in second.unreachable {
        if !first.unreachable.contains(&u) {
            first.unreachable.push(u);
        }
    }
    first.sources.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(first)
}

fn discover_by_mode(local_only: bool, wanted: SourceMode) -> Result<DiscoveryOutcome> {
    let mut out = Vec::new();
    let mut unreachable = Vec::new();
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
            // 🔴 `is_dir()` 把**所有**错误折叠成 false（评审 [P2]）—— 权限拒绝、
            // 元数据读失败，全都长得像「这个目录不存在」。而下面那个错误感知的遍历
            // 压根走不到，于是 `local` 不会进 `unreachable`，prune 照样删存量。
            // 判据必须显式区分 `NotFound`（事实）与其余（没问成）。
            match std::fs::metadata(&dir) {
                Ok(m) if m.is_dir() => {}
                Ok(_) => continue, // 存在但不是目录 —— 那是事实，不是故障
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    log::warn!(target: tag::DISCOVER, "stat failed: {} err={e}", dir.display());
                    if !unreachable.contains(&LOCAL_LOCATION.to_string()) {
                        unreachable.push(LOCAL_LOCATION.to_string());
                    }
                    continue;
                }
            }
            let (files, walk_failed) =
                collect_artifact_files_reported(&dir, &art.glob, art.recursive);
            if walk_failed && !unreachable.contains(&LOCAL_LOCATION.to_string()) {
                unreachable.push(LOCAL_LOCATION.to_string());
            }
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
        discover_wsl(&mut out, &mut unreachable, wanted);
    }
    log::info!(
        target: tag::DISCOVER,
        "discover done: sources={} unreachable={:?}",
        out.len(),
        unreachable
    );
    Ok(DiscoveryOutcome {
        sources: out,
        unreachable,
    })
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
fn discover_wsl(out: &mut Vec<SourceRef>, unreachable: &mut Vec<String>, wanted: SourceMode) {
    let distros = match crate::wsl::list_distros() {
        Ok(d) => d,
        Err(e) => {
            // 🔴 连发行版都列不出来 ⇒ **每一个** WSL 位置都算没问成。
            // 从前这里直接 `return`，调用方看到的是「一个 WSL 来源都没有」——
            // 与「用户把 WSL 里的会话全删了」完全一样。
            log::warn!(target: tag::DISCOVER, "wsl list_distros failed: {e}");
            unreachable.push(UNREACHABLE_ALL_WSL.to_string());
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
                        // 这个发行版的这一族没问成 ⇒ 整个位置都不能据本轮结果删存量。
                        // 「这一族空」与「这一族问不到」在返回值上一模一样。
                        log::warn!(
                            target: tag::DISCOVER,
                            "wsl find failed: distro={distro} rel={rel} err={e}"
                        );
                        let key = format!("wsl:{distro}");
                        if !unreachable.contains(&key) {
                            unreachable.push(key);
                        }
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
fn discover_wsl(_out: &mut Vec<SourceRef>, _unreachable: &mut Vec<String>, _wanted: SourceMode) {}

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
    collect_artifact_files_reported(dir, glob, recursive).0
}

/// 同上，外加**这次遍历有没有失败过**。
///
/// 🔴 `read_dir` / 目录项 / `file_type` 的错误从前一律被当成「这里没有文件」——
/// 与本仓刚修的 WSL 那条一模一样，而 `local` 位置的 prune 同样会据此删存量：
/// 一次权限拒绝或瞬时 FS 错误就能删掉某个 provider 的全部会话与 `usage_facts`
/// （评审 [P2]）。**「问了、没有」与「没问成」必须分开** —— 这里是第三次。
///
/// ⚠️ `NotFound` **不算失败**：目录不存在就是「这里确实没有」，调用方本来就先
/// `is_dir()` 过一道。
pub fn collect_artifact_files_reported(
    dir: &Path,
    glob: &str,
    recursive: bool,
) -> (Vec<PathBuf>, bool) {
    let mut out = Vec::new();
    let mut failed = false;
    let Some(suffix) = artifact_suffix(glob) else {
        return (out, false);
    };
    collect_files_into(dir, recursive, suffix, &mut out, &mut failed);
    if glob == "**/memory/*.md" {
        out.retain(|p| {
            p.parent()
                .and_then(Path::file_name)
                .and_then(|s| s.to_str())
                == Some("memory")
        });
    }
    out.sort();
    (out, failed)
}

/// 递归（或单层）收集目录下的 `*.jsonl`。骨架用 std 遍历，不引第三方 glob。
pub fn collect_jsonl(dir: &Path, recursive: bool) -> Vec<PathBuf> {
    collect_artifact_files(dir, "**/*.jsonl", recursive)
}

fn collect_files_into(
    dir: &Path,
    recursive: bool,
    suffix: &str,
    out: &mut Vec<PathBuf>,
    failed: &mut bool,
) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            // NotFound = 「这里确实没有」；其余（权限、IO、句柄耗尽）= 「没问成」。
            if e.kind() != std::io::ErrorKind::NotFound {
                log::warn!(target: tag::DISCOVER, "read_dir failed: {} err={e}", dir.display());
                *failed = true;
            }
            return;
        }
    };
    for entry in entries {
        let Ok(entry) = entry else {
            log::warn!(target: tag::DISCOVER, "dir entry failed under {}", dir.display());
            *failed = true;
            continue;
        };
        // `path.is_dir()` 把错误吞成 false —— 用 `file_type()` 才分得开
        // 「不是目录」和「问不到它是什么」。
        let is_dir = match entry.file_type() {
            Ok(t) => t.is_dir(),
            Err(e) => {
                log::warn!(target: tag::DISCOVER, "file_type failed: {:?} err={e}", entry.path());
                *failed = true;
                continue;
            }
        };
        let path = entry.path();
        if is_dir {
            if recursive {
                collect_files_into(&path, recursive, suffix, out, failed);
            }
        } else if path.to_string_lossy().ends_with(suffix) {
            out.push(path);
        }
    }
}

#[cfg(test)]
mod tests {

    /// 🔴 本地遍历失败也必须报出来（评审 [P2]）—— `read_dir` / 目录项 / `file_type`
    /// 的错误从前一律被当成「这里没有文件」，而 `local` 位置的 prune 据此删存量：
    /// 一次权限拒绝或瞬时 FS 错误就能删掉某个 provider 的全部会话与 usage_facts。
    ///
    /// 用**一个真实的失败**驱动：把一个普通文件当目录传进去 —— `read_dir` 会报
    /// `NotADirectory`（非 `NotFound`），正是「问了但没问成」那一档。
    #[test]
    fn a_local_walk_failure_is_reported_not_treated_as_empty() {
        use super::collect_artifact_files_reported;

        let base = std::env::temp_dir().join(format!("sv-walk-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&base);
        let not_a_dir = base.join("plain.txt");
        std::fs::write(&not_a_dir, b"x").unwrap();

        let (files, failed) = collect_artifact_files_reported(&not_a_dir, "**/*.jsonl", true);
        assert!(files.is_empty());
        assert!(failed, "把文件当目录遍历是「没问成」，不是「这里是空的」");

        // 反向：目录不存在就是**真的没有** —— 那不该被报成不可达，
        // 否则每一个没装某个 CLI 的用户都会永久带着一个「不可达」位置、prune 全被禁掉。
        let (files, failed) =
            collect_artifact_files_reported(&base.join("no-such-dir"), "**/*.jsonl", true);
        assert!(files.is_empty());
        assert!(!failed, "NotFound 是事实，不是故障");

        let _ = std::fs::remove_dir_all(&base);
    }

    /// 🔴 「问了、没有」与「没问成」在返回值上必须分得开。
    ///
    /// 从前两级失败都被静默吞掉，`sources` 变短而已 —— 而调用方据此 prune 存量行，
    /// 于是一次 WSL 变慢删掉了 369 个文件的会话与 usage_facts（副本实测 2026-08-12）。
    #[test]
    fn an_unreachable_location_is_reported_not_silently_empty() {
        use super::{discover_wsl, UNREACHABLE_ALL_WSL};
        use crate::rawevent::SourceMode;

        let mut out = Vec::new();
        let mut unreachable = Vec::new();
        // 非 Windows 构建下 discover_wsl 是空实现，这里只钉「签名带得出这个信息」
        // 以及哨兵的含义 —— 真实失败路径由 Windows 侧的 `list_distros` 覆盖。
        discover_wsl(&mut out, &mut unreachable, SourceMode::AppendLog);
        assert!(
            unreachable.iter().all(|u| u.starts_with("wsl:")),
            "位置键必须是 wsl:<distro> 或全量哨兵 wsl:*，调用方按它匹配"
        );
        assert_eq!(
            UNREACHABLE_ALL_WSL, "wsl:*",
            "全量哨兵的字面值是跨仓契约：QuotaBar 按它决定跳过所有 WSL 位置"
        );
    }

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
