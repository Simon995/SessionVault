//! 会话目录规范代选择、同句柄来源捕获、旧前缀核对与保守提交门。
//!
//! 只通过 `probe` 观察文件系统，不直接调用 `std::fs`；
//! `SourceSnapshot` 字段私有，只能由完整通过来源观察、物理核验与原生 admission 的路径构造。

use std::path::{Path, PathBuf};

use serde::Serialize;
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
    let mut best: Option<SelectedGeneration> = None;
    for item in entries {
        let entry = item.map_err(|_| Blocked::Unavailable("directory_entry_unverified"))?;
        let Some(name) = entry.file_name.to_str() else {
            continue;
        };
        let Some((generation, compression)) = parse_canonical_log_filename(name) else {
            continue;
        };
        if compression != expected_compression {
            return Err(Blocked::Unsupported("encoding_conflict"));
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
            .is_none_or(|current| generation > current.generation)
        {
            best = Some(SelectedGeneration {
                generation,
                file_name: name.to_string(),
                path: entry.path,
            });
        }
    }
    let chosen = best.ok_or(Blocked::Changed("selection_lost"))?;
    if chosen.generation != SUPPORTED_PHYSICAL_GENERATION {
        return Err(Blocked::Unsupported("physical_generation"));
    }
    Ok(chosen)
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
    let binding_payload = serde_json::to_vec(&json!([
        &config.source_instance,
        config.session_dir.to_string_lossy(),
        admitted.session_id(),
        config.compression.as_str(),
        selected.generation,
    ]))
    .expect("serializable source binding");
    Ok(SourceSnapshot {
        session_dir: config.session_dir.clone(),
        selected,
        compression: config.compression,
        stamp,
        source_binding: hash(&binding_payload),
        admitted,
    })
}

impl SourceSnapshot {
    pub fn source_binding(&self) -> &str {
        &self.source_binding
    }

    pub fn session_id(&self) -> &str {
        self.admitted.session_id()
    }

    pub fn lifecycle_sha256(&self) -> &str {
        self.admitted.lifecycle_sha256()
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
        if self.source_binding != next.source_binding {
            return SnapshotRelation::DifferentSource;
        }
        if self.lifecycle_sha256() != next.lifecycle_sha256() {
            return SnapshotRelation::LifecycleChanged;
        }
        let old = self.admitted.event_sha256s();
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
        (Some(old), Ok(next)) => match old.compare(next) {
            SnapshotRelation::DifferentSource => CommitGateDecision::RejectDifferentSource,
            SnapshotRelation::PrefixShrank {
                current_events,
                shared_prefix_intact,
                ..
            } => CommitGateDecision::HoldPriorProjection {
                prior_events: old.native_events(),
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
                prior_events: old.native_events(),
                verified_events: next.native_events(),
                invalidates_prior_credential: true,
                reason: RebuildReason::LifecycleChanged,
            },
            SnapshotRelation::PrefixRewritten { first_diff_seq, .. } => {
                CommitGateDecision::RebuildProjectionCandidate {
                    prior_events: old.native_events(),
                    verified_events: next.native_events(),
                    invalidates_prior_credential: true,
                    reason: RebuildReason::PrefixRewritten { first_diff_seq },
                }
            }
        },
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

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
}
