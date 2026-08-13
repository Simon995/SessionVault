//! 来源发现（§9 discover）。遍历内置描述符，分离发现 transcript 与 snapshot 来源。
//!
//! 首次只发现、不读内容（供宿主弹授权）。本机 Local + WSL 各发行版（`Wsl(distro)`）。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::catalog::{self, Status};
use crate::logging::tag;
use crate::probe::ProbeBackend;
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
pub fn discover_all(deadline: crate::deadline::Deadline) -> Result<Vec<SourceRef>> {
    discover(false, deadline)
}

pub fn discover_local(deadline: crate::deadline::Deadline) -> Result<Vec<SourceRef>> {
    discover(true, deadline)
}

/// 只发现会话 append_log。QuotaBar 会话索引必须使用本接口，避免 snapshot
/// 状态制品污染 `agent_sessions` 投影。
pub fn discover_transcripts(deadline: crate::deadline::Deadline) -> Result<Vec<SourceRef>> {
    Ok(discover_by_mode(false, SourceMode::AppendLog, deadline)?.sources)
}

/// 同上，但**同时报出哪些位置没问成**。
///
/// 🔴 会话索引必须用这个，不能用上面那个：调用方要据发现结果 prune 存量行，
/// 而「这个位置零文件」与「这个位置问不到」在 `sources` 里长得一模一样。
pub fn discover_transcripts_reported(
    deadline: crate::deadline::Deadline,
) -> Result<DiscoveryOutcome> {
    discover_by_mode(false, SourceMode::AppendLog, deadline)
}

pub fn discover_transcripts_local(deadline: crate::deadline::Deadline) -> Result<Vec<SourceRef>> {
    Ok(discover_by_mode(true, SourceMode::AppendLog, deadline)?.sources)
}

/// 只发现工具配置根内的 Class-B 状态快照。项目 instruction 由
/// [`discover_project_snapshots`] 接收宿主已经算好的项目身份后发现。
pub fn discover_snapshots(deadline: crate::deadline::Deadline) -> Result<Vec<SourceRef>> {
    Ok(discover_by_mode(false, SourceMode::SnapshotFile, deadline)?.sources)
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
/// 一次项目内指令文件发现的结果。
///
/// 🔴 **`unreachable` 与「这个根下没有指令文件」是两件事。** 上一版返回一个裸
/// `Vec<SourceRef>`，把两者压成同一个「不在列表里」：`is_file()` 把权限拒绝、
/// 句柄耗尽、瞬时 IO 错误全返回 `false`，WSL 那支更是
/// `.ok().flatten().is_some()` —— 一次 `wsl.exe` 卡死与「这个项目没写过
/// CLAUDE.md」在调用方眼里一模一样。
///
/// 这是本仓反复栽的同一个形状（`Probe::{Seen,Absent,Unreachable}`、
/// `DiscoveryOutcome`、`Probe::None` vs `Probe::Failed`）在又一层的重演。
pub struct ProjectSnapshotOutcome {
    pub sources: Vec<SourceRef>,
    /// 探测失败的项目根（`project_root` 原样）。调用方**不得**把它读作
    /// 「这个项目没有指令文件」。
    pub unreachable: Vec<String>,
}

/// 一次文件探测的三态。`NotFound` 是**事实**（那里确实没有），其余 IO 错误是
/// 「没问成」—— 按 AGENTS.md：把不存在报成不可达，会让每个没写过 CLAUDE.md 的
/// 项目永久带着一个假故障。
enum FileProbe {
    Present,
    Absent,
    Failed,
}

/// ⚠️ 这里从前自带一份 `match std::fs::metadata` —— 与 `discovery.rs`、
/// `memory/sources.rs` 各一份，共四份手抄本。判据现在只在 `probe::classify` 里，
/// 本函数只剩「我要的是**文件**」这个本地决定。
fn probe_host_file(path: &std::path::Path) -> FileProbe {
    match crate::probe::LocalBackend.probe(path, crate::deadline::Deadline::unbounded()) {
        crate::probe::Probed::Found(crate::probe::FileKind::File) => FileProbe::Present,
        // 存在但不是文件（目录 / 断掉的符号链）—— 那个**文件**确实没有，是事实。
        crate::probe::Probed::Found(_) => FileProbe::Absent,
        crate::probe::Probed::Absent => FileProbe::Absent,
        crate::probe::Probed::Unknown(_) => FileProbe::Failed,
    }
}

pub fn discover_project_snapshots(
    roots: &[ProjectSnapshotRoot],
    deadline: crate::deadline::Deadline,
) -> ProjectSnapshotOutcome {
    let mut out = Vec::new();
    let mut unreachable: Vec<String> = Vec::new();
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
            let seen = match (&root.source_location, probe) {
                (_, Some(p)) => probe_host_file(&p),
                (SourceLocation::Local, None) => probe_host_file(&path),
                (SourceLocation::Wsl(distro), None) => {
                    match crate::wsl::stat(distro, &path.to_string_lossy(), deadline) {
                        Ok(Some(_)) => FileProbe::Present,
                        Ok(None) => FileProbe::Absent,
                        // 问不到那台 VM —— 不是「这个项目没有 CLAUDE.md」。
                        Err(_) => FileProbe::Failed,
                    }
                }
            };
            match seen {
                FileProbe::Present => out.push(SourceRef {
                    source_type,
                    source_location: root.source_location.clone(),
                    source_mode: SourceMode::SnapshotFile,
                    path,
                    project_root: Some(root.project_root.clone()),
                    artifact_kind: Some("instruction".to_string()),
                }),
                FileProbe::Absent => {}
                FileProbe::Failed => {
                    // 按**根**记账：一个根下三个候选，任一没问成就说不出
                    // 「这个项目的指令文件是哪些」。
                    if !unreachable.contains(&root.project_root) {
                        unreachable.push(root.project_root.clone());
                    }
                }
            }
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    ProjectSnapshotOutcome {
        sources: out,
        unreachable,
    }
}

fn discover(local_only: bool, deadline: crate::deadline::Deadline) -> Result<Vec<SourceRef>> {
    Ok(discover_reported(local_only, deadline)?.sources)
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

fn discover_reported(
    local_only: bool,
    deadline: crate::deadline::Deadline,
) -> Result<DiscoveryOutcome> {
    let mut first = discover_by_mode(local_only, SourceMode::AppendLog, deadline)?;
    let second = discover_by_mode(local_only, SourceMode::SnapshotFile, deadline)?;
    first.sources.extend(second.sources);
    for u in second.unreachable {
        if !first.unreachable.contains(&u) {
            first.unreachable.push(u);
        }
    }
    first.sources.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(first)
}

fn discover_by_mode(
    local_only: bool,
    wanted: SourceMode,
    deadline: crate::deadline::Deadline,
) -> Result<DiscoveryOutcome> {
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
            match crate::probe::LocalBackend.probe(&dir, crate::deadline::Deadline::unbounded()) {
                crate::probe::Probed::Found(crate::probe::FileKind::Dir) => {}
                crate::probe::Probed::Found(_) => continue, // 存在但不是目录 —— 那是事实，不是故障
                crate::probe::Probed::Absent => continue,
                crate::probe::Probed::Unknown(e) => {
                    log::warn!(target: tag::DISCOVER, "stat failed: {e}");
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
        discover_wsl(&mut out, &mut unreachable, wanted, deadline);
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
fn discover_wsl(
    out: &mut Vec<SourceRef>,
    unreachable: &mut Vec<String>,
    wanted: SourceMode,
    deadline: crate::deadline::Deadline,
) {
    let distros = match crate::wsl::list_distros(deadline) {
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
                let mut files =
                    match crate::wsl::list_files_under_home(distro, &rel, suffix, deadline) {
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
fn discover_wsl(
    _out: &mut Vec<SourceRef>,
    _unreachable: &mut Vec<String>,
    _wanted: SourceMode,
    _deadline: crate::deadline::Deadline,
) {
}

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
        discover_wsl(
            &mut out,
            &mut unreachable,
            SourceMode::AppendLog,
            crate::deadline::Deadline::unbounded(),
        );
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
        let found = discover_project_snapshots(&roots, crate::deadline::Deadline::unbounded());
        assert_eq!(found.sources.len(), 1);
        assert_eq!(
            found.sources[0].path.to_string_lossy(),
            "/home/u/project/AGENTS.md"
        );
        assert_eq!(
            found.sources[0].project_root.as_deref(),
            Some(roots[0].project_root.as_str())
        );
        // 探测**成功**且只是没找到另外两个候选 ⇒ 不算不可达。
        assert!(
            found.unreachable.is_empty(),
            "问到了、答案是「没有」—— 那不是故障：{:?}",
            found.unreachable
        );
        let _ = std::fs::remove_dir_all(probe);
    }

    /// 🔴 **「没问成」不许长得像「这里没有」**（评审 P2-5）。
    ///
    /// 上一版整个函数是 `is_file()` / `.ok().flatten().is_some()` —— 权限拒绝、
    /// 句柄耗尽、`wsl.exe` 卡死，全部返回 `false`，与「这个项目没写过 CLAUDE.md」
    /// 一模一样。调用方（记忆源枚举）据此决定要不要蒸馏这个项目的指令文件，
    /// 于是一次瞬时故障 = 这个项目的规则**静默不进记忆**。
    ///
    /// 驱动方式：一个**不存在的发行版**（`wsl.exe` 约 0.1s 失败），与
    /// `unreachable_client()` / `WSL_E_DISTRO_NOT_FOUND` 是同一个手法 ——
    /// 打真实路径，而不是断言某个 helper 返回什么。
    #[test]
    fn a_probe_that_could_not_be_made_is_not_reported_as_absent() {
        let roots = [ProjectSnapshotRoot {
            source_location: SourceLocation::Wsl("svault-no-such-distro-9c1f".into()),
            // 没有 `probe_path` ⇒ 走 WSL 那一支，也就是被测的那条。
            path: "/home/u/project".into(),
            probe_path: None,
            project_root: "wsl:svault-no-such-distro-9c1f:/home/u/project".into(),
        }];
        let found = discover_project_snapshots(&roots, crate::deadline::Deadline::unbounded());

        assert!(
            found.sources.is_empty(),
            "问不到就不该产出来源 —— 那会凭空造一个读不了的文件"
        );
        assert_eq!(
            found.unreachable,
            vec!["wsl:svault-no-such-distro-9c1f:/home/u/project".to_string()],
            "🔴 但它必须**说出来**：空列表 + 空 unreachable 读作「这个项目没有指令文件」"
        );
    }

    /// 同一条判据的**本机**那一半。
    ///
    /// 🔴 头一版只测了 WSL 支，于是把本机支的 `Err(_) => Absent` 变异掉照样全绿 ——
    /// 「WSL 做对了不等于 local 做对了」，AGENTS.md 里这句话是有判例的。
    ///
    /// 真正要防的是 `probe_path` 指向一个**死掉的 UNC**（`\\wsl.localhost\<distro>`
    /// 在发行版停了之后）或一个 ACL 拒绝的目录 —— 两者都构造不成确定性测试，
    /// 所以这里用路径里的 NUL 字节：Rust 在发系统调用前就返回 `InvalidInput`，
    /// 瞬时、跨平台，且**恰好是那一类**「不是 NotFound 的 IO 错误」。
    #[test]
    fn a_host_probe_that_errors_is_not_reported_as_absent() {
        let roots = [ProjectSnapshotRoot {
            source_location: SourceLocation::Local,
            path: std::path::PathBuf::from("/tmp/svault-probe-err\u{0}x"),
            probe_path: None,
            project_root: "/tmp/svault-probe-err".into(),
        }];
        let found = discover_project_snapshots(&roots, crate::deadline::Deadline::unbounded());

        assert!(found.sources.is_empty(), "问不成就不产出来源");
        assert_eq!(
            found.unreachable,
            vec!["/tmp/svault-probe-err".to_string()],
            "🔴 本机 IO 错误同样是「没问成」—— 只有 NotFound 才是「确实没有」"
        );
    }

    /// 反方向：`NotFound` **必须**读作「确实没有」。
    ///
    /// ⚠️ 少了这一条，把整个 `probe_host_file` 写成恒 `Failed` 也能通过上面那条 ——
    /// 而那会让每个没写过 CLAUDE.md 的项目永久带着一个假故障，正是 AGENTS.md
    /// 里那句「目录不存在是事实」在防的。
    #[test]
    fn a_root_without_instruction_files_is_absent_not_unreachable() {
        let dir = std::env::temp_dir().join(format!(
            "svault-empty-root-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let roots = [ProjectSnapshotRoot {
            source_location: SourceLocation::Local,
            path: dir.clone(),
            probe_path: None,
            project_root: dir.to_string_lossy().into_owned(),
        }];
        let found = discover_project_snapshots(&roots, crate::deadline::Deadline::unbounded());

        assert!(found.sources.is_empty(), "这个根下确实一个指令文件都没有");
        assert!(
            found.unreachable.is_empty(),
            "问到了、答案是「没有」—— 报成不可达会让每个干净项目永久挂个假故障：{:?}",
            found.unreachable
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
