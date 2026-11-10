//! 会话目录规范代选择、同句柄来源捕获、旧前缀核对与保守提交门。
//!
//! 只通过 `probe` 观察文件系统，不直接调用 `std::fs`；
//! `SourceSnapshot` 字段私有，只能由完整通过来源观察、物理核验与原生 admission 的路径构造。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::{hash, limits, verify, AdmittedSnapshot, Blocked, Compression, NativeRuntime};
use crate::deadline::Deadline;
use crate::probe::{self, FileKind, FileStamp, PinnedRead, Probed, RealEntryKind};

const SUPPORTED_PHYSICAL_GENERATION: u32 = 4;

/// 会话目录中按最高规范代选出的物理工件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedGeneration {
    generation: u32,
    file_name: String,
    path: PathBuf,
}

impl SelectedGeneration {
    pub fn generation(&self) -> u32 {
        self.generation
    }

    pub fn file_name(&self) -> &str {
        &self.file_name
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// 识别规范代日志文件名：`session.jsonl[.zstd]` 为 v0，`session.v<1..>.jsonl[.zstd]` 为 v1+。
///
/// 拒绝 `.v0`、前导零（如 `.v04`）、临时后缀及非十进制名称。
pub fn parse_canonical_log_filename(name: &str) -> Option<(u32, Compression)> {
    let (stem, compression) = if let Some(base) = name.strip_suffix(".zstd") {
        (base, Compression::Zstd)
    } else {
        (name, Compression::None)
    };
    if stem == "session.jsonl" {
        return Some((0, compression));
    }
    let middle = stem.strip_prefix("session.v")?.strip_suffix(".jsonl")?;
    let first = middle.as_bytes().first().copied()?;
    if !(b'1'..=b'9').contains(&first) || !middle.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let generation = middle.parse::<u32>().ok()?;
    Some((generation, compression))
}

/// 在显式会话目录上按最高规范代选择物理日志；不按 `mtime` 选择，不回退旧代，不忽略编码冲突。
pub fn select_generation(
    session_dir: &Path,
    expected_compression: Compression,
) -> Result<SelectedGeneration, Blocked> {
    select_generation_internal(session_dir, Some(expected_compression)).map(|(sel, _)| sel)
}

/// 在显式会话目录上自动检测单一一致编码并按最高规范代选择物理日志；
/// 混存 `.jsonl` 与 `.jsonl.zstd` 报 `encoding_conflict`，最高规范代非 `v4`（`v0..v3` 或 `v5+`）
/// 报 `physical_generation` 且不回退旧代。
pub fn select_generation_auto_compression(
    session_dir: &Path,
) -> Result<(SelectedGeneration, Compression), Blocked> {
    select_generation_internal(session_dir, None)
}

fn select_generation_internal(
    session_dir: &Path,
    expected_compression: Option<Compression>,
) -> Result<(SelectedGeneration, Compression), Blocked> {
    if !session_dir.is_absolute() {
        return Err(Blocked::Unavailable("session_directory_not_absolute"));
    }
    match probe::lstat_entry(session_dir, Some(session_dir)) {
        Probed::Found(RealEntryKind::Directory) => {}
        Probed::Found(RealEntryKind::RegularFile(_) | RealEntryKind::Other) => {
            return Err(Blocked::Unsupported("session_directory_kind"));
        }
        Probed::Absent => return Err(Blocked::Unavailable("session_directory_absent")),
        Probed::Unknown(_) => return Err(Blocked::Unavailable("session_directory_unverified")),
    }
    let entries = match probe::read_dir_entries(session_dir, Some(session_dir)) {
        Probed::Found(entries) => entries,
        Probed::Absent => return Err(Blocked::Changed("selection_lost")),
        Probed::Unknown(_) => return Err(Blocked::Unavailable("directory_entries_unverified")),
    };
    let mut best: Option<(SelectedGeneration, Compression)> = None;
    let mut observed_compression: Option<Compression> = expected_compression;
    for item in entries {
        let entry = item.map_err(|_| Blocked::Unavailable("directory_entry_unverified"))?;
        let Some(name) = entry.file_name.to_str() else {
            continue;
        };
        let Some((generation, compression)) = parse_canonical_log_filename(name) else {
            continue;
        };
        match observed_compression {
            Some(expected) if compression != expected => {
                return Err(Blocked::Unsupported("encoding_conflict"));
            }
            None => {
                observed_compression = Some(compression);
            }
            Some(_) => {}
        }
        match entry.kind {
            Probed::Found(FileKind::File) => {}
            Probed::Found(FileKind::Dir | FileKind::Other) => {
                return Err(Blocked::Unsupported("artifact_kind"));
            }
            Probed::Absent => return Err(Blocked::Changed("selection_changed")),
            Probed::Unknown(_) => return Err(Blocked::Unavailable("directory_entry_unverified")),
        }
        if best
            .as_ref()
            .is_none_or(|(current, _)| generation > current.generation)
        {
            best = Some((
                SelectedGeneration {
                    generation,
                    file_name: name.to_string(),
                    path: entry.path,
                },
                compression,
            ));
        }
    }
    let (chosen, chosen_compression) = best.ok_or(Blocked::Changed("selection_lost"))?;
    if chosen.generation != SUPPORTED_PHYSICAL_GENERATION {
        return Err(Blocked::Unsupported("physical_generation"));
    }
    Ok((chosen, chosen_compression))
}

/// DSH 会话根目录下单个会话子目录的发现与规范代判定状态。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum DshDiscoveredSessionStatus {
    ReadyV4 {
        generation: u32,
        compression: Compression,
        file_name: String,
        artifact_path: PathBuf,
    },
    Blocked {
        blocked: Blocked,
    },
}

/// DSH 会话根目录下发现的单个会话子目录条目。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DshDiscoveredSessionEntry {
    pub dir_name: String,
    pub session_dir: PathBuf,
    pub outcome: DshDiscoveredSessionStatus,
}

/// DSH 会话根目录（如 `<dsh_home>/sessions`）的批量发现与规范代核查报告。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DshRootDiscoveryReport {
    pub sessions_root: PathBuf,
    pub entries: Vec<DshDiscoveredSessionEntry>,
    pub ready_v4_count: usize,
    pub blocked_count: usize,
}

/// 从显式环境变量解析 DSH 会话根目录（优先 `SVAULT_DSH_SESSIONS_DIR`，其次 `SVAULT_DSH_HOME` 下的 `sessions`）。
///
/// 未配置环境变量时返回 `None`，绝不默认扫描未显式授权的目录，保证既有消费者行为零变化。
pub fn dsh_sessions_root_from_env() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("SVAULT_DSH_SESSIONS_DIR") {
        let trimmed = p.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }
    if let Ok(home) = std::env::var("SVAULT_DSH_HOME") {
        let trimmed = home.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed).join("sessions"));
        }
    }
    None
}

/// 在显式给出的 DSH 会话根目录（如 `<dsh_home>/sessions`）下枚举所有会话子目录并执行最高规范代选择。
///
/// 首版正式规则（2026-11-09 决定）：
/// - 仅最高规范代为 `v4`（`session.v4.jsonl[.zstd]`）的子目录记为 `ReadyV4`；
/// - 仅存旧格式代 `v0..v3` 或出现未来格式代 `v5+` 的子目录显式记为 `Blocked(Unsupported("physical_generation"))`，绝不回退旧代；
/// - 混存 `.jsonl` 与 `.jsonl.zstd` 显式记为 `Blocked(Unsupported("encoding_conflict"))`。
pub fn discover_dsh_sessions_in_root(
    sessions_root: &Path,
    expected_compression: Option<Compression>,
) -> Result<DshRootDiscoveryReport, Blocked> {
    if !sessions_root.is_absolute() {
        return Err(Blocked::Unavailable("sessions_root_not_absolute"));
    }
    match probe::lstat_entry(sessions_root, Some(sessions_root)) {
        Probed::Found(RealEntryKind::Directory) => {}
        Probed::Found(RealEntryKind::RegularFile(_) | RealEntryKind::Other) => {
            return Err(Blocked::Unsupported("sessions_root_kind"));
        }
        Probed::Absent => return Err(Blocked::Unavailable("sessions_root_absent")),
        Probed::Unknown(_) => return Err(Blocked::Unavailable("sessions_root_unverified")),
    }
    let raw_entries = match probe::read_dir_entries(sessions_root, Some(sessions_root)) {
        Probed::Found(entries) => entries,
        Probed::Absent => return Err(Blocked::Unavailable("sessions_root_absent")),
        Probed::Unknown(_) => return Err(Blocked::Unavailable("sessions_root_entries_unverified")),
    };

    let mut subdirs: Vec<(String, PathBuf)> = Vec::new();
    for item in raw_entries {
        let entry = item.map_err(|_| Blocked::Unavailable("sessions_root_entry_unverified"))?;
        let Some(name) = entry.file_name.to_str() else {
            continue;
        };
        match entry.kind {
            Probed::Found(FileKind::Dir) => {
                subdirs.push((name.to_string(), entry.path));
            }
            Probed::Found(FileKind::File | FileKind::Other) => continue,
            Probed::Absent => continue,
            Probed::Unknown(_) => {
                return Err(Blocked::Unavailable("sessions_root_entry_unverified"))
            }
        }
    }
    subdirs.sort_by(|a, b| a.0.cmp(&b.0));

    let mut entries = Vec::with_capacity(subdirs.len());
    let mut ready_v4_count = 0usize;
    let mut blocked_count = 0usize;

    for (dir_name, session_dir) in subdirs {
        let outcome = match select_generation_internal(&session_dir, expected_compression) {
            Ok((selected, compression)) => {
                ready_v4_count += 1;
                DshDiscoveredSessionStatus::ReadyV4 {
                    generation: selected.generation,
                    compression,
                    file_name: selected.file_name,
                    artifact_path: selected.path,
                }
            }
            Err(blocked) => {
                blocked_count += 1;
                DshDiscoveredSessionStatus::Blocked { blocked }
            }
        };
        entries.push(DshDiscoveredSessionEntry {
            dir_name,
            session_dir,
            outcome,
        });
    }

    Ok(DshRootDiscoveryReport {
        sessions_root: sessions_root.to_path_buf(),
        entries,
        ready_v4_count,
        blocked_count,
    })
}

/// 显式配置的会话目录捕获目标。
#[derive(Debug, Clone)]
pub struct SourceConfig {
    source_instance: String,
    session_dir: PathBuf,
    expected_session_id: Option<String>,
    compression: Compression,
}

impl SourceConfig {
    pub fn new(
        source_instance: impl Into<String>,
        session_dir: impl Into<PathBuf>,
        expected_session_id: Option<String>,
        compression: Compression,
    ) -> Result<Self, Blocked> {
        let source_instance = source_instance.into();
        let session_dir = session_dir.into();
        if source_instance.is_empty() {
            return Err(Blocked::Unavailable("source_instance_empty"));
        }
        if !session_dir.is_absolute() {
            return Err(Blocked::Unavailable("session_directory_not_absolute"));
        }
        if expected_session_id.as_deref().is_some_and(str::is_empty) {
            return Err(Blocked::Unavailable("expected_session_id_empty"));
        }
        Ok(Self {
            source_instance,
            session_dir,
            expected_session_id,
            compression,
        })
    }
}

/// 绑定了来源选代、同句柄文件身份、物理核验与原生语义 admission 的快照。
#[derive(Debug)]
pub struct SourceSnapshot {
    session_dir: PathBuf,
    selected: SelectedGeneration,
    compression: Compression,
    stamp: FileStamp,
    source_binding: String,
    // 接受同一逻辑来源的另一种物理编码；兼容既有含编码的持久化绑定，不放宽来源身份。
    alternate_encoding_binding: Option<String>,
    admitted: AdmittedSnapshot,
}

/// 两份已验证来源快照之间的演化关系。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "relation", rename_all = "snake_case")]
pub enum SnapshotRelation {
    DifferentSource,
    LifecycleChanged,
    PrefixShrank {
        prior_events: usize,
        current_events: usize,
        shared_prefix_intact: bool,
    },
    PrefixRewritten {
        prior_events: usize,
        current_events: usize,
        first_diff_seq: usize,
    },
    PrefixPreserved {
        prior_events: usize,
        current_events: usize,
        appended_events: usize,
    },
}

impl SnapshotRelation {
    pub fn kind_str(&self) -> &'static str {
        match self {
            Self::DifferentSource => "different_source",
            Self::LifecycleChanged => "lifecycle_changed",
            Self::PrefixShrank { .. } => "prefix_shrank",
            Self::PrefixRewritten { .. } => "prefix_rewritten",
            Self::PrefixPreserved { .. } => "prefix_preserved",
        }
    }
}

/// 保留旧投影不推进的具体原因。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "hold_kind", rename_all = "snake_case")]
pub enum HoldReason {
    CaptureBlocked {
        blocked: Blocked,
    },
    PrefixShrank {
        current_events: usize,
        shared_prefix_intact: bool,
    },
}

/// 使旧推进凭据失效并重建目标投影的原因。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "rebuild_kind", rename_all = "snake_case")]
pub enum RebuildReason {
    LifecycleChanged,
    PrefixRewritten { first_diff_seq: usize },
}

/// 进入观察 / `CommitPlan` 前的保守提交门判定。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum CommitGateDecision {
    RejectInitial {
        blocked: Blocked,
    },
    HoldPriorProjection {
        prior_events: usize,
        reason: HoldReason,
    },
    RejectDifferentSource,
    InitialProjectionCandidate {
        verified_events: usize,
        inherited_event_count: usize,
    },
    NoOpUnchanged {
        verified_events: usize,
    },
    IncrementalAppendCandidate {
        prior_events: usize,
        verified_events: usize,
        appended_events: usize,
    },
    RebuildProjectionCandidate {
        prior_events: usize,
        verified_events: usize,
        invalidates_prior_credential: bool,
        reason: RebuildReason,
    },
}

fn observe_again(
    session_dir: &Path,
    compression: Compression,
    selected: &SelectedGeneration,
    stamp: FileStamp,
) -> Result<(), Blocked> {
    let current = select_generation(session_dir, compression)?;
    if current != *selected {
        return Err(Blocked::Changed("selection_changed"));
    }
    match probe::lstat_entry(&selected.path, Some(session_dir)) {
        Probed::Found(RealEntryKind::RegularFile(actual)) if actual == stamp => Ok(()),
        Probed::Found(RealEntryKind::RegularFile(_)) | Probed::Absent => {
            Err(Blocked::Changed("file_changed"))
        }
        Probed::Found(RealEntryKind::Directory | RealEntryKind::Other) => {
            Err(Blocked::Changed("artifact_kind_changed"))
        }
        Probed::Unknown(_) => Err(Blocked::Unavailable("source_recheck_failed")),
    }
}

fn source_binding_for(
    config: &SourceConfig,
    session_id: &str,
    compression: Compression,
    generation: u32,
) -> String {
    hash(
        &serde_json::to_vec(&json!([
            &config.source_instance,
            config.session_dir.to_string_lossy(),
            session_id,
            compression.as_str(),
            generation,
        ]))
        .expect("serializable source binding"),
    )
}

/// 从显式会话目录执行选代、同句柄有界读取、物理核验、原生 admission 与前后复查。
pub fn capture_source(
    config: &SourceConfig,
    runtime: &NativeRuntime,
    deadline: Deadline,
) -> Result<SourceSnapshot, Blocked> {
    if deadline.expired() {
        return Err(Blocked::Unavailable("native_deadline_exhausted"));
    }
    let selected = select_generation(&config.session_dir, config.compression)?;
    let (stamp, bytes) = match probe::read_pinned_regular_file(
        &selected.path,
        limits().input_bytes,
        Some(&config.session_dir),
    ) {
        Probed::Found(PinnedRead::Complete { stamp, bytes }) => (stamp, bytes),
        Probed::Found(PinnedRead::NotRegularFile) => {
            return Err(Blocked::Unsupported("artifact_kind"));
        }
        Probed::Found(PinnedRead::ExceedsBudget { .. }) => {
            return Err(Blocked::BudgetExceeded("input_budget"));
        }
        Probed::Found(PinnedRead::Changed(reason)) => return Err(Blocked::Changed(reason)),
        Probed::Absent => return Err(Blocked::Changed("file_changed")),
        Probed::Unknown(_) => return Err(Blocked::Unavailable("source_read_failed")),
    };
    observe_again(&config.session_dir, config.compression, &selected, stamp)?;
    let physical = verify(&bytes, config.compression)?;
    if config
        .expected_session_id
        .as_deref()
        .is_some_and(|expected| expected != physical.header_id)
    {
        return Err(Blocked::Changed("header_binding_changed"));
    }
    let admitted = runtime.admit(physical, deadline)?;
    observe_again(&config.session_dir, config.compression, &selected, stamp)?;
    let source_binding = source_binding_for(
        config,
        admitted.session_id(),
        config.compression,
        selected.generation,
    );
    let other_encoding = match config.compression {
        Compression::None => Compression::Zstd,
        Compression::Zstd => Compression::None,
    };
    let alternate_encoding_binding = Some(source_binding_for(
        config,
        admitted.session_id(),
        other_encoding,
        selected.generation,
    ));
    Ok(SourceSnapshot {
        session_dir: config.session_dir.clone(),
        selected,
        compression: config.compression,
        stamp,
        source_binding,
        alternate_encoding_binding,
        admitted,
    })
}

impl SourceSnapshot {
    pub(super) fn admitted(&self) -> &AdmittedSnapshot {
        &self.admitted
    }

    pub fn session_dir(&self) -> &Path {
        &self.session_dir
    }

    pub fn selected(&self) -> &SelectedGeneration {
        &self.selected
    }

    pub fn compression(&self) -> Compression {
        self.compression
    }

    pub fn source_binding(&self) -> &str {
        &self.source_binding
    }

    pub fn session_id(&self) -> &str {
        self.admitted.session_id()
    }

    pub fn lifecycle_sha256(&self) -> &str {
        self.admitted.lifecycle_sha256()
    }

    pub fn native_events_sha256(&self) -> &str {
        self.admitted.native_events_sha256()
    }

    pub fn event_sha256s(&self) -> &[String] {
        self.admitted.event_sha256s()
    }

    pub fn dsh_version(&self) -> &str {
        self.admitted.dsh_version()
    }

    pub fn native_events(&self) -> usize {
        self.admitted.event_sha256s().len()
    }

    pub fn inherited_event_count(&self) -> usize {
        self.admitted.inherited_event_count()
    }

    /// 再次观察目录选代与路径身份是否仍与捕获时一致；不重读正文，也不升级为生产推进许可。
    pub fn recheck(&self) -> Result<(), Blocked> {
        observe_again(
            &self.session_dir,
            self.compression,
            &self.selected,
            self.stamp,
        )
    }

    /// 对照两份已验证来源快照的来源绑定、生命周期及完整旧事件前缀。
    pub fn compare(&self, next: &Self) -> SnapshotRelation {
        compare_parts(
            &self.source_binding,
            self.lifecycle_sha256(),
            self.admitted.event_sha256s(),
            next,
        )
    }

    /// 输出包含来源观察与原生 admission 的摘要；仍保留 `production_advance_permit=false`。
    pub fn report(&self, include_positions: bool) -> Value {
        let mut report = self.admitted.report(include_positions);
        report["kind"] = json!("source_observed_snapshot");
        report["source_observation_validated"] = json!(true);
        report["blockers"] = json!(["normalization_required"]);
        report["source_observation"] = json!({
            "source_binding": self.source_binding,
            "session_id": self.session_id(),
            "physical_generation": self.selected.generation,
            "selected_file": self.selected.file_name,
            "compression": self.compression.as_str(),
        });
        report
    }
}

/// 跨进程持久化的 DSH 来源推进凭据：绑定来源、生命周期、物理格式代、完整旧前缀摘要与解析/归属版本。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DshSavedState {
    pub source_binding: String,
    pub session_id: String,
    pub compression: Compression,
    pub physical_generation: u32,
    pub dsh_version: String,
    pub parser_revision: u32,
    pub attribution_revision: i64,
    pub inherited_event_count: usize,
    pub lifecycle_sha256: String,
    pub native_events_sha256: String,
    pub event_sha256s: Vec<String>,
    pub normalized_next_seq: u64,
    /// 归一化事件在控制面丰富前的完整摘要；闭合 turn 可能只改变旧事件的证据而不改变条数。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub normalized_events_sha256: Option<String>,
    /// 完整落库事件（含控制面丰富与 profile）的摘要，区分同原生内容的不同投影目标。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub projected_events_sha256: Option<String>,
}

impl DshSavedState {
    /// 从已验证的来源快照与归一化计数构造新的持久化状态。
    pub fn from_verified(
        snapshot: &SourceSnapshot,
        normalized_next_seq: u64,
        parser_revision: u32,
        attribution_revision: i64,
    ) -> Self {
        Self {
            source_binding: snapshot.source_binding().to_string(),
            session_id: snapshot.session_id().to_string(),
            compression: snapshot.compression(),
            physical_generation: snapshot.selected().generation(),
            dsh_version: snapshot.dsh_version().to_string(),
            parser_revision,
            attribution_revision,
            inherited_event_count: snapshot.inherited_event_count(),
            lifecycle_sha256: snapshot.lifecycle_sha256().to_string(),
            native_events_sha256: snapshot.native_events_sha256().to_string(),
            event_sha256s: snapshot.event_sha256s().to_vec(),
            normalized_next_seq,
            normalized_events_sha256: None,
            projected_events_sha256: None,
        }
    }

    /// 校验从磁盘加载的状态文件结构是否完整。
    pub fn is_well_formed(&self) -> bool {
        let is_hex64 = |s: &str| {
            s.len() == 64
                && s.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        };
        !self.session_id.is_empty()
            && !self.dsh_version.is_empty()
            && self.physical_generation == SUPPORTED_PHYSICAL_GENERATION
            && self.inherited_event_count <= self.event_sha256s.len()
            && is_hex64(&self.source_binding)
            && is_hex64(&self.lifecycle_sha256)
            && is_hex64(&self.native_events_sha256)
            && self.event_sha256s.iter().all(|item| is_hex64(item))
    }

    pub fn native_events(&self) -> usize {
        self.event_sha256s.len()
    }

    /// 常规 CLI 与原生提交入口共用同一投影 token，跨写者崩溃重放不会重复开代。
    #[cfg(feature = "store")]
    pub fn projection_token(
        &self,
        source: &crate::store::SourceKey,
    ) -> crate::token::ProjectionToken {
        let fingerprint = format!(
            "dsh4:target:{}:{}:{}",
            self.lifecycle_sha256,
            self.native_events_sha256,
            self.projected_events_sha256.as_deref().unwrap_or("legacy")
        );
        crate::token::ProjectionToken::new(
            source,
            Some(&fingerprint),
            self.parser_revision,
            self.attribution_revision,
            (0, self.native_events() as u64),
        )
    }

    pub fn compare(&self, next: &SourceSnapshot) -> SnapshotRelation {
        compare_parts(
            &self.source_binding,
            &self.lifecycle_sha256,
            &self.event_sha256s,
            next,
        )
    }
}

fn compare_parts(
    old_binding: &str,
    old_lifecycle: &str,
    old: &[String],
    next: &SourceSnapshot,
) -> SnapshotRelation {
    if old_binding != next.source_binding
        && next.alternate_encoding_binding.as_deref() != Some(old_binding)
    {
        return SnapshotRelation::DifferentSource;
    }
    if old_lifecycle != next.lifecycle_sha256() {
        return SnapshotRelation::LifecycleChanged;
    }
    let current = next.admitted.event_sha256s();
    if current.len() < old.len() {
        return SnapshotRelation::PrefixShrank {
            prior_events: old.len(),
            current_events: current.len(),
            shared_prefix_intact: old[..current.len()] == current[..],
        };
    }
    if let Some(first_diff_seq) = old
        .iter()
        .zip(current.iter())
        .position(|(left, right)| left != right)
    {
        return SnapshotRelation::PrefixRewritten {
            prior_events: old.len(),
            current_events: current.len(),
            first_diff_seq,
        };
    }
    SnapshotRelation::PrefixPreserved {
        prior_events: old.len(),
        current_events: current.len(),
        appended_events: current.len() - old.len(),
    }
}

fn gate_from_relation(
    prior_events: usize,
    next_events: usize,
    relation: SnapshotRelation,
) -> CommitGateDecision {
    match relation {
        SnapshotRelation::DifferentSource => CommitGateDecision::RejectDifferentSource,
        SnapshotRelation::PrefixShrank {
            current_events,
            shared_prefix_intact,
            ..
        } => CommitGateDecision::HoldPriorProjection {
            prior_events,
            reason: HoldReason::PrefixShrank {
                current_events,
                shared_prefix_intact,
            },
        },
        SnapshotRelation::PrefixPreserved {
            current_events,
            appended_events: 0,
            ..
        } => CommitGateDecision::NoOpUnchanged {
            verified_events: current_events,
        },
        SnapshotRelation::PrefixPreserved {
            prior_events,
            current_events,
            appended_events,
        } => CommitGateDecision::IncrementalAppendCandidate {
            prior_events,
            verified_events: current_events,
            appended_events,
        },
        SnapshotRelation::LifecycleChanged => CommitGateDecision::RebuildProjectionCandidate {
            prior_events,
            verified_events: next_events,
            invalidates_prior_credential: true,
            reason: RebuildReason::LifecycleChanged,
        },
        SnapshotRelation::PrefixRewritten { first_diff_seq, .. } => {
            CommitGateDecision::RebuildProjectionCandidate {
                prior_events,
                verified_events: next_events,
                invalidates_prior_credential: true,
                reason: RebuildReason::PrefixRewritten { first_diff_seq },
            }
        }
    }
}

/// 根据上一轮已验证来源快照（若有）与本轮捕获结果，给出进入观察 / `CommitPlan` 前的保守门判定。
pub fn evaluate_commit_gate(
    prior: Option<&SourceSnapshot>,
    current: Result<&SourceSnapshot, &Blocked>,
) -> CommitGateDecision {
    match (prior, current) {
        (None, Err(blocked)) => CommitGateDecision::RejectInitial {
            blocked: blocked.clone(),
        },
        (None, Ok(snapshot)) => CommitGateDecision::InitialProjectionCandidate {
            verified_events: snapshot.native_events(),
            inherited_event_count: snapshot.inherited_event_count(),
        },
        (Some(old), Err(blocked)) => CommitGateDecision::HoldPriorProjection {
            prior_events: old.native_events(),
            reason: HoldReason::CaptureBlocked {
                blocked: blocked.clone(),
            },
        },
        (Some(old), Ok(next)) => {
            gate_from_relation(old.native_events(), next.native_events(), old.compare(next))
        }
    }
}

/// 根据磁盘持久化的 `DshSavedState`（若有）与本轮捕获结果，计算关系与保守提交门判定。
pub fn evaluate_saved_commit_gate(
    prior: Option<&DshSavedState>,
    current: Result<&SourceSnapshot, &Blocked>,
) -> (Option<SnapshotRelation>, CommitGateDecision) {
    match (prior, current) {
        (None, Err(blocked)) => (
            None,
            CommitGateDecision::RejectInitial {
                blocked: blocked.clone(),
            },
        ),
        (None, Ok(snapshot)) => (
            None,
            CommitGateDecision::InitialProjectionCandidate {
                verified_events: snapshot.native_events(),
                inherited_event_count: snapshot.inherited_event_count(),
            },
        ),
        (Some(old), Err(blocked)) => (
            None,
            CommitGateDecision::HoldPriorProjection {
                prior_events: old.native_events(),
                reason: HoldReason::CaptureBlocked {
                    blocked: blocked.clone(),
                },
            },
        ),
        (Some(old), Ok(next)) => {
            if !old.is_well_formed() {
                return (
                    None,
                    CommitGateDecision::HoldPriorProjection {
                        prior_events: old.native_events(),
                        reason: HoldReason::CaptureBlocked {
                            blocked: Blocked::Invalid("corrupt_saved_state"),
                        },
                    },
                );
            }
            let relation = old.compare(next);
            let gate =
                gate_from_relation(old.native_events(), next.native_events(), relation.clone());
            (Some(relation), gate)
        }
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)]
pub(super) mod tests {
    use super::*;

    pub(in crate::dsh_snapshot) fn synthetic_source_snapshot(
        dir: &Path,
        session_id: &str,
        compression: Compression,
        jsonl_lines: &[&str],
        inherited: usize,
        lifecycle: &str,
        event_tags: &[&str],
    ) -> SourceSnapshot {
        let mut raw = String::new();
        for line in jsonl_lines {
            raw.push_str(line);
            raw.push('\n');
        }
        let file = dir.join(match compression {
            Compression::None => "session.v4.jsonl",
            Compression::Zstd => "session.v4.jsonl.zstd",
        });
        std::fs::write(&file, raw.as_bytes()).unwrap();
        let stamp = match probe::lstat_entry(&file, None) {
            Probed::Found(RealEntryKind::RegularFile(s)) => s,
            other => panic!("unexpected lstat: {other:?}"),
        };
        let physical = super::super::verify(raw.as_bytes(), Compression::None).unwrap();
        let event_hashes: Vec<String> = event_tags.iter().map(|e| hash(e.as_bytes())).collect();
        let reply = json!({
            "kind": "native_admitted",
            "session_id": session_id,
            "jsonl_sha256": physical.jsonl_hash,
            "header_sha256": physical.header_hash,
            "native_events": event_hashes.len(),
            "inherited_event_count": inherited,
            "lifecycle_sha256": hash(lifecycle.as_bytes()),
            "native_events_sha256": hash(serde_json::to_string(&event_hashes).unwrap().as_bytes()),
            "event_sha256s": event_hashes,
            "dsh_version": "0.2.1-rc.5",
        });
        let admitted = super::super::native::tests::bind_verified_reply(physical, &reply);
        SourceSnapshot {
            session_dir: dir.to_path_buf(),
            selected: SelectedGeneration {
                generation: 4,
                file_name: file.file_name().unwrap().to_string_lossy().into_owned(),
                path: file,
            },
            compression,
            stamp,
            source_binding: hash(format!("{}:{session_id}", dir.display()).as_bytes()),
            alternate_encoding_binding: None,
            admitted,
        }
    }

    #[cfg(any(windows, unix))]
    #[test]
    fn saved_state_accepts_an_encoding_switch_but_still_rejects_another_source() {
        let dir = std::env::temp_dir().join(format!("sv-dsh-encoding-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let records = [
            r#"{"type":"session","version":4,"id":"s0"}"#,
            r#"{"seq":0,"type":"x"}"#,
        ];
        let cfg = SourceConfig::new("instance", &dir, None, Compression::None).unwrap();
        let mut plain =
            synthetic_source_snapshot(&dir, "s0", Compression::None, &records, 0, "life", &["e0"]);
        plain.source_binding = source_binding_for(&cfg, "s0", Compression::None, 4);
        let saved = DshSavedState::from_verified(&plain, 0, 1, 0);
        let mut compressed =
            synthetic_source_snapshot(&dir, "s0", Compression::Zstd, &records, 0, "life", &["e0"]);
        compressed.source_binding = source_binding_for(&cfg, "s0", Compression::Zstd, 4);
        compressed.alternate_encoding_binding =
            Some(source_binding_for(&cfg, "s0", Compression::None, 4));
        assert!(matches!(
            saved.compare(&compressed),
            SnapshotRelation::PrefixPreserved {
                appended_events: 0,
                ..
            }
        ));
        let compressed_saved = DshSavedState::from_verified(&compressed, 0, 1, 0);
        plain.alternate_encoding_binding =
            Some(source_binding_for(&cfg, "s0", Compression::Zstd, 4));
        assert!(matches!(
            compressed_saved.compare(&plain),
            SnapshotRelation::PrefixPreserved { .. }
        ));
        let other = SourceConfig::new("another-instance", &dir, None, Compression::Zstd).unwrap();
        compressed.source_binding = source_binding_for(&other, "s0", Compression::Zstd, 4);
        compressed.alternate_encoding_binding =
            Some(source_binding_for(&other, "s0", Compression::None, 4));
        assert_eq!(
            saved.compare(&compressed),
            SnapshotRelation::DifferentSource
        );
        std::fs::remove_file(dir.join("session.v4.jsonl")).unwrap();
        std::fs::remove_file(dir.join("session.v4.jsonl.zstd")).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn canonical_log_filename_parser_rejects_non_canonical_names() {
        assert_eq!(
            parse_canonical_log_filename("session.jsonl"),
            Some((0, Compression::None))
        );
        assert_eq!(
            parse_canonical_log_filename("session.jsonl.zstd"),
            Some((0, Compression::Zstd))
        );
        assert_eq!(
            parse_canonical_log_filename("session.v1.jsonl"),
            Some((1, Compression::None))
        );
        assert_eq!(
            parse_canonical_log_filename("session.v4.jsonl.zstd"),
            Some((4, Compression::Zstd))
        );
        assert_eq!(
            parse_canonical_log_filename("session.v12.jsonl"),
            Some((12, Compression::None))
        );
        for invalid in [
            "session.v0.jsonl",
            "session.v04.jsonl",
            "session.v4.jsonl.tmp",
            "session.v4.jsonl.zst",
            "session.v-1.jsonl",
            "other.jsonl",
        ] {
            assert_eq!(parse_canonical_log_filename(invalid), None, "{invalid}");
        }
    }

    #[test]
    fn generation_selection_picks_highest_and_blocks_conflicts_or_unsupported() {
        let dir =
            std::env::temp_dir().join(format!("sv-dsh-select-{}-{}", std::process::id(), line!()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        assert_eq!(
            select_generation(&dir, Compression::None).unwrap_err(),
            Blocked::Changed("selection_lost")
        );

        // 非规范文件名忽略，v0 与 v4 并存时选最高规范代 v4。
        std::fs::write(dir.join("session.v0.jsonl"), b"ignored").unwrap();
        std::fs::write(dir.join("session.v4.jsonl.tmp"), b"ignored").unwrap();
        std::fs::write(dir.join("session.jsonl"), b"v0").unwrap();
        assert_eq!(
            select_generation(&dir, Compression::None).unwrap_err(),
            Blocked::Unsupported("physical_generation")
        );
        std::fs::write(dir.join("session.v4.jsonl"), b"v4").unwrap();
        let selected = select_generation(&dir, Compression::None).unwrap();
        assert_eq!(selected.generation(), 4);
        assert_eq!(selected.file_name(), "session.v4.jsonl");

        // 同目录混入 .zstd 编码冲突。
        std::fs::write(dir.join("session.v4.jsonl.zstd"), b"zstd").unwrap();
        assert_eq!(
            select_generation(&dir, Compression::None).unwrap_err(),
            Blocked::Unsupported("encoding_conflict")
        );
        std::fs::remove_file(dir.join("session.v4.jsonl.zstd")).unwrap();

        // 发布未来代 v5 不回退到 v4。
        std::fs::write(dir.join("session.v5.jsonl"), b"v5").unwrap();
        assert_eq!(
            select_generation(&dir, Compression::None).unwrap_err(),
            Blocked::Unsupported("physical_generation")
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(any(windows, unix))]
    #[test]
    fn compare_and_commit_gate_enforce_conservative_transition_rules() {
        let dir =
            std::env::temp_dir().join(format!("sv-dsh-gate-{}-{}", std::process::id(), line!()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("session.v4.jsonl");
        std::fs::write(&file, b"placeholder").unwrap();
        let Probed::Found(RealEntryKind::RegularFile(stamp)) =
            probe::lstat_entry(&file, Some(&dir))
        else {
            panic!("expected regular file stamp");
        };

        let make = |source_binding: &str, lifecycle: &str, inherited: usize, events: &[&str]| {
            let mut raw = String::from("{\"type\":\"session\",\"version\":4,\"id\":\"s0\"}\n");
            for (index, item) in events.iter().enumerate() {
                raw.push_str(&format!(
                    "{{\"seq\":{index},\"type\":\"x\",\"text\":\"{item}\"}}\n"
                ));
            }
            let physical = verify(raw.as_bytes(), Compression::None).unwrap();
            let event_hashes: Vec<String> = events.iter().map(|e| hash(e.as_bytes())).collect();
            let reply = json!({
                "kind": "native_admitted",
                "session_id": "s0",
                "jsonl_sha256": physical.jsonl_hash,
                "header_sha256": physical.header_hash,
                "native_events": events.len(),
                "inherited_event_count": inherited,
                "lifecycle_sha256": hash(lifecycle.as_bytes()),
                "native_events_sha256": hash(serde_json::to_string(&event_hashes).unwrap().as_bytes()),
                "event_sha256s": event_hashes,
                "dsh_version": "0.2.1-rc.5",
            });
            let admitted = super::super::native::tests::bind_verified_reply(physical, &reply);
            SourceSnapshot {
                session_dir: dir.clone(),
                selected: SelectedGeneration {
                    generation: 4,
                    file_name: "session.v4.jsonl".to_string(),
                    path: file.clone(),
                },
                compression: Compression::None,
                stamp,
                source_binding: hash(source_binding.as_bytes()),
                alternate_encoding_binding: None,
                admitted,
            }
        };

        let base = make("src-a", "life-1", 0, &["e0", "e1", "e2"]);
        let same = make("src-a", "life-1", 0, &["e0", "e1", "e2"]);
        let grown = make("src-a", "life-1", 0, &["e0", "e1", "e2", "e3"]);
        let shrunk_intact = make("src-a", "life-1", 0, &["e0", "e1"]);
        let shrunk_altered = make("src-a", "life-1", 0, &["e0", "changed"]);
        let rewritten = make("src-a", "life-1", 0, &["e0", "changed", "e2"]);
        let relifecycled = make("src-a", "life-2", 2, &["e0", "e1", "e2", "e3"]);
        let other_src = make("src-b", "life-1", 0, &["e0", "e1", "e2"]);

        assert_eq!(
            evaluate_commit_gate(None, Err(&Blocked::Incomplete("torn_frame_header"))),
            CommitGateDecision::RejectInitial {
                blocked: Blocked::Incomplete("torn_frame_header"),
            }
        );
        assert_eq!(
            evaluate_commit_gate(None, Ok(&base)),
            CommitGateDecision::InitialProjectionCandidate {
                verified_events: 3,
                inherited_event_count: 0,
            }
        );
        assert_eq!(
            evaluate_commit_gate(Some(&base), Err(&Blocked::Changed("file_changed"))),
            CommitGateDecision::HoldPriorProjection {
                prior_events: 3,
                reason: HoldReason::CaptureBlocked {
                    blocked: Blocked::Changed("file_changed"),
                },
            }
        );
        assert_eq!(base.compare(&other_src), SnapshotRelation::DifferentSource);
        assert_eq!(
            evaluate_commit_gate(Some(&base), Ok(&other_src)),
            CommitGateDecision::RejectDifferentSource
        );
        assert_eq!(
            base.compare(&same),
            SnapshotRelation::PrefixPreserved {
                prior_events: 3,
                current_events: 3,
                appended_events: 0,
            }
        );
        assert_eq!(
            evaluate_commit_gate(Some(&base), Ok(&same)),
            CommitGateDecision::NoOpUnchanged { verified_events: 3 }
        );
        assert_eq!(
            base.compare(&grown),
            SnapshotRelation::PrefixPreserved {
                prior_events: 3,
                current_events: 4,
                appended_events: 1,
            }
        );
        assert_eq!(
            evaluate_commit_gate(Some(&base), Ok(&grown)),
            CommitGateDecision::IncrementalAppendCandidate {
                prior_events: 3,
                verified_events: 4,
                appended_events: 1,
            }
        );
        assert_eq!(
            base.compare(&shrunk_intact),
            SnapshotRelation::PrefixShrank {
                prior_events: 3,
                current_events: 2,
                shared_prefix_intact: true,
            }
        );
        assert_eq!(
            evaluate_commit_gate(Some(&base), Ok(&shrunk_intact)),
            CommitGateDecision::HoldPriorProjection {
                prior_events: 3,
                reason: HoldReason::PrefixShrank {
                    current_events: 2,
                    shared_prefix_intact: true,
                },
            }
        );
        assert_eq!(
            base.compare(&shrunk_altered),
            SnapshotRelation::PrefixShrank {
                prior_events: 3,
                current_events: 2,
                shared_prefix_intact: false,
            }
        );
        assert_eq!(
            base.compare(&rewritten),
            SnapshotRelation::PrefixRewritten {
                prior_events: 3,
                current_events: 3,
                first_diff_seq: 1,
            }
        );
        assert_eq!(
            evaluate_commit_gate(Some(&base), Ok(&rewritten)),
            CommitGateDecision::RebuildProjectionCandidate {
                prior_events: 3,
                verified_events: 3,
                invalidates_prior_credential: true,
                reason: RebuildReason::PrefixRewritten { first_diff_seq: 1 },
            }
        );
        assert_eq!(
            base.compare(&relifecycled),
            SnapshotRelation::LifecycleChanged
        );
        assert_eq!(
            evaluate_commit_gate(Some(&base), Ok(&relifecycled)),
            CommitGateDecision::RebuildProjectionCandidate {
                prior_events: 3,
                verified_events: 4,
                invalidates_prior_credential: true,
                reason: RebuildReason::LifecycleChanged,
            }
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn discover_dsh_sessions_in_root_enumerates_v4_and_reports_legacy_future_or_conflict() {
        let root = std::env::temp_dir().join(format!(
            "sv-dsh-root-disc-{}-{}",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        // 1. s_a_v4_zstd：含 v2、v3、v4（zstd）-> 选最高代 v4 ReadyV4
        let s_a = root.join("s_a_v4_zstd");
        std::fs::create_dir_all(&s_a).unwrap();
        std::fs::write(s_a.join("session.v2.jsonl.zstd"), b"v2").unwrap();
        std::fs::write(s_a.join("session.v3.jsonl.zstd"), b"v3").unwrap();
        std::fs::write(s_a.join("session.v4.jsonl.zstd"), b"v4").unwrap();

        // 2. s_b_v4_plain：仅 v4（plain）-> ReadyV4
        let s_b = root.join("s_b_v4_plain");
        std::fs::create_dir_all(&s_b).unwrap();
        std::fs::write(s_b.join("session.v4.jsonl"), b"v4_plain").unwrap();

        // 3. s_c_legacy_v3：仅 v0..v3 -> Blocked(Unsupported("physical_generation"))
        let s_c = root.join("s_c_legacy_v3");
        std::fs::create_dir_all(&s_c).unwrap();
        std::fs::write(s_c.join("session.jsonl.zstd"), b"v0").unwrap();
        std::fs::write(s_c.join("session.v3.jsonl.zstd"), b"v3").unwrap();

        // 4. s_d_future_v5：v4 与 v5 并存 -> 绝不回退 v4，Blocked(Unsupported("physical_generation"))
        let s_d = root.join("s_d_future_v5");
        std::fs::create_dir_all(&s_d).unwrap();
        std::fs::write(s_d.join("session.v4.jsonl.zstd"), b"v4").unwrap();
        std::fs::write(s_d.join("session.v5.jsonl.zstd"), b"v5").unwrap();

        // 5. s_e_conflict：同目录混存 plain 与 zstd -> Blocked(Unsupported("encoding_conflict"))
        let s_e = root.join("s_e_conflict");
        std::fs::create_dir_all(&s_e).unwrap();
        std::fs::write(s_e.join("session.v4.jsonl"), b"plain").unwrap();
        std::fs::write(s_e.join("session.v4.jsonl.zstd"), b"zstd").unwrap();

        let report = discover_dsh_sessions_in_root(&root, None).unwrap();
        assert_eq!(report.ready_v4_count, 2);
        assert_eq!(report.blocked_count, 3);
        assert_eq!(report.entries.len(), 5);

        assert_eq!(
            report.entries[0].outcome,
            DshDiscoveredSessionStatus::ReadyV4 {
                generation: 4,
                compression: Compression::Zstd,
                file_name: "session.v4.jsonl.zstd".to_string(),
                artifact_path: s_a.join("session.v4.jsonl.zstd"),
            }
        );
        assert_eq!(
            report.entries[1].outcome,
            DshDiscoveredSessionStatus::ReadyV4 {
                generation: 4,
                compression: Compression::None,
                file_name: "session.v4.jsonl".to_string(),
                artifact_path: s_b.join("session.v4.jsonl"),
            }
        );
        assert_eq!(
            report.entries[2].outcome,
            DshDiscoveredSessionStatus::Blocked {
                blocked: Blocked::Unsupported("physical_generation"),
            }
        );
        assert_eq!(
            report.entries[3].outcome,
            DshDiscoveredSessionStatus::Blocked {
                blocked: Blocked::Unsupported("physical_generation"),
            }
        );
        assert_eq!(
            report.entries[4].outcome,
            DshDiscoveredSessionStatus::Blocked {
                blocked: Blocked::Unsupported("encoding_conflict"),
            }
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
