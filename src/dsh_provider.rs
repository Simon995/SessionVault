//! 显式配置的 DSH v4 来源：以会话目录为逻辑来源，物理代与编码由核验器选择。
//!
//! 存储沿用 `Jsonl + AppendLog`，让旧版总库读者仍能识别事件；DSH 的原生来源
//! 由独立描述符、配置根、绑定及计量证据保留，不把物理日志交给通用 JSONL parser。

use std::path::Path;
#[cfg(feature = "store")]
use std::path::PathBuf;

use crate::attribution::registry_key;
use crate::catalog::{Artifact, ProviderDescriptor, Status};
use crate::deadline::Deadline;
use crate::discover::SourceRef;
use crate::dsh_snapshot::dsh_sessions_root_from_env;
use crate::probe::{self, FileKind, Probed, RealEntryKind};
use crate::rawevent::{SourceLocation, SourceMode, SourceType};

pub(crate) const PROVIDER_NAME: &str = "DeepSeek Harness";

pub(crate) fn descriptor() -> ProviderDescriptor {
    ProviderDescriptor {
        source_type: SourceType::Jsonl,
        name: PROVIDER_NAME.to_string(),
        config_dir: dsh_sessions_root_from_env(),
        artifacts: vec![Artifact {
            subdir: String::new(),
            // 物理日志位于会话子目录；逻辑目录选代与 .zstd 编码仍由专用发现/扫描入口处理。
            glob: "session.v4.jsonl".to_string(),
            source_mode: SourceMode::AppendLog,
            status: Status::Stable,
            recursive: true,
            kind: None,
            may_be_compressed: false,
        }],
    }
}

/// 配置根的直接子目录才是逻辑来源；不按文件名、叶子目录名或日志内容猜 provider。
pub fn is_source(source: &SourceRef) -> bool {
    dsh_sessions_root_from_env().is_some_and(|root| is_source_in_root(source, &root))
}

fn is_source_in_root(source: &SourceRef, root: &Path) -> bool {
    root.is_absolute()
        && source.path.is_absolute()
        && source.source_type == SourceType::Jsonl
        && source.source_mode == SourceMode::AppendLog
        && source.source_location == SourceLocation::Local
        && relative_key(root, &source.path)
            .is_some_and(|relative| !relative.is_empty() && !relative.contains('/'))
}

/// 配置根与发现路径均为宿主路径；复用目录身份规则，空挂载表不猜测发行版路径映射。
fn relative_key(root: &Path, path: &Path) -> Option<String> {
    let mounts = crate::pathnorm::DriveMounts::default();
    let root = registry_key(root.to_str()?, &mounts);
    let path = registry_key(path.to_str()?, &mounts);
    if path == root {
        return Some(String::new());
    }
    let prefix = if root.ends_with('/') {
        root
    } else {
        format!("{root}/")
    };
    path.strip_prefix(&prefix).map(str::to_string)
}

/// 通用 JSONL 必须排除配置根本身及其全部后代，包括选代不支持的物理日志。
pub(crate) fn contains_path(root: &Path, path: &Path) -> bool {
    root.is_absolute() && path.is_absolute() && relative_key(root, path).is_some()
}

pub(crate) fn discover(root: &Path, deadline: Deadline) -> crate::Result<Vec<SourceRef>> {
    let blocked = |reason: &str| crate::Error::Parse(format!("DSH discovery blocked: {reason}"));
    let check_budget = || {
        if deadline.expired() {
            Err(blocked("deadline_exhausted"))
        } else {
            Ok(())
        }
    };
    check_budget()?;
    if !root.is_absolute() {
        return Err(blocked("sessions_root_not_absolute"));
    }
    match probe::lstat_entry(root, Some(root)) {
        Probed::Found(RealEntryKind::Directory) => {}
        Probed::Found(RealEntryKind::RegularFile(_) | RealEntryKind::Other) => {
            return Err(blocked("sessions_root_kind"));
        }
        Probed::Absent => return Ok(Vec::new()),
        Probed::Unknown(error) => return Err(blocked(&error.to_string())),
    }
    check_budget()?;
    let entries = match probe::read_dir_entries(root, Some(root)) {
        Probed::Found(entries) => entries,
        Probed::Absent => return Ok(Vec::new()),
        Probed::Unknown(error) => return Err(blocked(&error.to_string())),
    };
    let mut sources = Vec::new();
    for entry in entries {
        check_budget()?;
        let entry = entry.map_err(|error| blocked(&error.to_string()))?;
        if entry.file_name.to_str().is_none() {
            continue;
        }
        match entry.kind {
            Probed::Found(FileKind::Dir) => sources.push(SourceRef {
                source_type: SourceType::Jsonl,
                source_location: SourceLocation::Local,
                source_mode: SourceMode::AppendLog,
                path: entry.path,
                project_root: None,
                artifact_kind: None,
            }),
            Probed::Found(FileKind::File | FileKind::Other) | Probed::Absent => {}
            Probed::Unknown(error) => return Err(blocked(&error.to_string())),
        }
    }
    // 不在发现阶段重复选代；旧代、未来代及空目录也交给扫描入口作保守观察。
    sources.sort_by(|left, right| left.path.cmp(&right.path));
    check_budget()?;
    Ok(sources)
}

#[cfg(feature = "store")]
fn required_runtime_path(name: &str) -> Result<PathBuf, crate::dsh_snapshot::Blocked> {
    let value = std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .ok_or(crate::dsh_snapshot::Blocked::Unavailable(
            "native_runtime_not_configured",
        ))?;
    let path = PathBuf::from(value);
    if !path.is_absolute() {
        return Err(crate::dsh_snapshot::Blocked::Unavailable(
            "native_runtime_paths_not_absolute",
        ));
    }
    Ok(path)
}

/// 常规扫描复用 DSH 唯一的观察/规划路径。运行时配置缺失、捕获失败或未决绑定均冻结旧状态。
#[cfg(feature = "store")]
#[allow(clippy::too_many_arguments)]
pub fn scan_source(
    source: &SourceRef,
    cursor: Option<crate::Cursor>,
    profile: crate::Profile,
    roots: std::sync::Arc<crate::attribution::RootRegistry>,
    bindings: &crate::SessionBindingRegistry,
    reasons: crate::scan_plan::ScanReasons,
    prior: crate::scan_plan::PriorProjection,
    attribution_revision: i64,
    deadline: crate::deadline::Deadline,
) -> (
    crate::dsh_snapshot::DshPlannedStep,
    crate::report::SourceReport,
) {
    use crate::dsh_snapshot::{
        capture_source, plan_dsh_source_step_with_bindings, select_generation_auto_compression,
        Blocked, DshNormalizeConfig, NativeRuntime, SourceConfig,
    };
    let prior_state = cursor
        .as_ref()
        .and_then(|cursor| cursor.dsh_state.as_deref());
    let mut cfg = DshNormalizeConfig::local_full(source.path.display().to_string());
    cfg.profile = profile;
    cfg.roots = roots;
    cfg.attribution_revision = attribution_revision;
    let instance = format!("dsh:{}", source.path.display());
    cfg.source_instance = Some(instance.clone());
    let captured = (|| {
        if !is_source(source) {
            return Err(Blocked::Unavailable("source_outside_configured_root"));
        }
        let runtime = NativeRuntime::new(
            required_runtime_path("SVAULT_DSH_NODE")?,
            required_runtime_path("SVAULT_DSH_PACKAGE")?,
        )?;
        let (_, compression) = select_generation_auto_compression(&source.path)?;
        let config = SourceConfig::new(instance, source.path.clone(), None, compression)?;
        capture_source(&config, &runtime, deadline)
    })();
    let result = plan_dsh_source_step_with_bindings(
        prior_state,
        captured.as_ref(),
        reasons,
        prior,
        &cfg,
        bindings,
    );
    let mut planned = match result {
        Ok(planned) => planned,
        Err(blocked) => plan_dsh_source_step_with_bindings(
            prior_state,
            Err(&blocked),
            reasons,
            prior,
            &cfg,
            bindings,
        )
        .expect("blocked capture always produces a frozen plan"),
    };
    if planned.candidate_state.is_none() {
        // 旧写者可能移除了 dsh_state；失败时仍须保留调用方的原始游标，不能重建成零值。
        if let Some(cursor) = cursor.as_ref() {
            planned.observation.cursor = cursor.clone();
        }
    }
    let mut report = crate::report::SourceReport {
        source_path: cfg.source_path,
        source_mode: Some(SourceMode::AppendLog),
        cursor_kind: Some(planned.observation.cursor.kind),
        events_emitted: planned.observation.events.len() as u64,
        rollback_detected: planned.observation.source_change
            == crate::observation::SourceChange::RollbackOrRewrite,
        admission_status: planned.admission_status.clone(),
        admission_detail: planned.admission_detail.clone(),
        events_excluded_temporary: planned.events_excluded_temporary,
        ..Default::default()
    };
    match captured {
        Ok(snapshot) => {
            report.items_examined = snapshot.native_events() as u64;
            report.bytes_read = snapshot.report(false)["physical_bytes"]
                .as_u64()
                .expect("verified snapshot reports physical bytes");
        }
        Err(blocked) => report
            .warnings
            .push(format!("DSH capture blocked: {blocked:?}")),
    }
    if prior_state.is_none()
        && prior.state() == crate::scan_plan::PriorState::Present
        && planned.candidate_state.is_some()
    {
        report.warnings.push(
            "DSH prior projection has no saved prefix evidence; rebuilding while retaining the prior source version"
                .to_string(),
        );
    }
    if !planned.observation.events_are_usable() {
        report.warnings.push(format!(
            "DSH projection held: {:?}",
            planned.observation.quality
        ));
    }
    (planned, report)
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    fn source(path: impl Into<std::path::PathBuf>) -> SourceRef {
        SourceRef {
            source_type: SourceType::Jsonl,
            source_location: SourceLocation::Local,
            source_mode: SourceMode::AppendLog,
            path: path.into(),
            project_root: None,
            artifact_kind: None,
        }
    }

    fn fixture_path() -> std::path::PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "sv-dsh-provider-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ))
    }

    #[test]
    fn dsh_source_identity_requires_a_direct_child_of_the_configured_root() {
        let root = fixture_path();
        assert!(is_source_in_root(&source(root.join("s0")), &root));
        assert!(!is_source_in_root(&source(root.clone()), &root));
        assert!(!is_source_in_root(
            &source(root.join("s0/session.v4.jsonl")),
            &root
        ));
        let sibling = root.with_extension("other").join("s0");
        assert!(!contains_path(&root, &sibling));
        assert!(!is_source_in_root(&source(sibling), &root));
        let mut other = source(root.join("s0"));
        other.source_type = SourceType::Codex;
        assert!(!is_source_in_root(&other, &root));
        other.source_type = SourceType::Jsonl;
        other.source_location = SourceLocation::Wsl("dev".to_string());
        assert!(!is_source_in_root(&other, &root));
        assert!(!is_source_in_root(
            &source("sessions/s0"),
            Path::new("sessions")
        ));
    }

    #[cfg(windows)]
    #[test]
    fn dsh_source_identity_uses_registry_keys_for_case_and_separators() {
        let root = std::path::PathBuf::from("C:/data/DSH/sessions/");
        let other_spelling = source(r"c:\DATA\dsh\SESSIONS\s0");
        assert!(is_source_in_root(&other_spelling, &root));
        assert!(contains_path(
            &root,
            Path::new(r"c:\DATA\dsh\SESSIONS\s0\session.v5.jsonl")
        ));
        assert!(!contains_path(
            &root,
            Path::new(r"c:\DATA\dsh\SESSIONS-other\s0\session.v4.jsonl")
        ));
    }

    #[test]
    fn dsh_discovery_keeps_unsupported_and_empty_session_directories() {
        let root = fixture_path();
        std::fs::create_dir(&root).unwrap();
        let sessions = ["legacy", "future", "empty", "current"];
        for name in sessions {
            std::fs::create_dir(root.join(name)).unwrap();
        }
        let logs = [
            ("legacy", "session.v3.jsonl"),
            ("future", "session.v5.jsonl"),
            ("current", "session.v4.jsonl"),
        ];
        for (session, name) in logs {
            std::fs::write(root.join(session).join(name), "not a parsed log").unwrap();
        }
        std::fs::create_dir(root.join("empty/nested")).unwrap();
        let sources = discover(&root, Deadline::unbounded()).unwrap();
        let mut expected: Vec<_> = sessions.iter().map(|name| root.join(name)).collect();
        expected.sort();
        assert_eq!(
            sources
                .into_iter()
                .map(|source| source.path)
                .collect::<Vec<_>>(),
            expected
        );
        let artifact = descriptor().artifacts.remove(0);
        assert_eq!(
            crate::discover::collect_artifact_files(&root, &artifact.glob, artifact.recursive),
            vec![root.join("current/session.v4.jsonl")]
        );
        for (session, name) in logs {
            std::fs::remove_file(root.join(session).join(name)).unwrap();
        }
        std::fs::remove_dir(root.join("empty/nested")).unwrap();
        for name in sessions {
            std::fs::remove_dir(root.join(name)).unwrap();
        }
        std::fs::remove_dir(root).unwrap();
    }

    #[cfg(feature = "store")]
    #[test]
    fn blocked_scan_preserves_a_cursor_whose_dsh_evidence_was_lost() {
        let mut cursor = crate::Cursor::new_byte_offset();
        cursor.safe_offset = 7;
        cursor.size = 11;
        cursor.next_seq = 17;
        let expected = serde_json::to_value(&cursor).unwrap();
        let (planned, _) = scan_source(
            &source(fixture_path()),
            Some(cursor),
            crate::Profile::Full,
            std::sync::Arc::new(crate::attribution::RootRegistry::new()),
            &crate::SessionBindingRegistry::empty(),
            crate::scan_plan::ScanReasons::NONE,
            crate::scan_plan::PriorProjection::no_store(),
            0,
            Deadline::unbounded(),
        );
        assert_eq!(
            planned.plan.cursor(),
            crate::scan_plan::CursorAction::Freeze
        );
        assert!(planned.candidate_state.is_none());
        assert_eq!(
            serde_json::to_value(&planned.observation.cursor).unwrap(),
            expected
        );
    }

    #[test]
    fn dsh_discovery_distinguishes_absence_from_exhausted_budget() {
        let root = fixture_path();
        assert!(discover(&root, Deadline::unbounded()).unwrap().is_empty());
        let exhausted = Deadline::at(std::time::Instant::now());
        assert!(discover(&root, exhausted)
            .unwrap_err()
            .to_string()
            .contains("deadline_exhausted"));
        assert!(discover(Path::new("sessions"), Deadline::unbounded()).is_err());
    }
}
