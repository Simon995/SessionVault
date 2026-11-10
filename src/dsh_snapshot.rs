//! 实验性 DSH v4 字节核验与原生记录位置；不注册 provider、不产生 RawEvent。
//!
//! 物理完整不包含 DSH codec / restore admission、来源选代或生命周期稳定性。
//! 即使成功也不能转换成 AppendLogObservation 或生产推进凭据。

use std::path::Path;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::compressed::{checked_frames, FrameCheckFailure};
use crate::probe::{self, PinnedRead, Probed};
use crate::rawevent::EventKey;

mod native;
mod normalize;
mod source;
pub use native::{AdmittedSnapshot, NativeRuntime};
#[cfg(feature = "store")]
pub use normalize::{
    commit_dsh_source_step, commit_dsh_source_step_with_bindings, plan_dsh_source_step,
    plan_dsh_source_step_with_bindings, scan_dsh_sessions_root_to_store, DshPlannedStep,
    DshRootScanOutcome, DshRootScanSessionOutcome, DshStoreCommitOutcome,
};
pub use normalize::{
    epoch_ms_to_iso8601_utc, normalize_admitted_snapshot, normalize_source_snapshot,
    DshCompactionRecord, DshNormalizeConfig, DshSubagentCatalogEntry, DshSubagentDescriptor,
    DshSurfaceReplacement, DshUsageEntry, DshUsageStatus, NativeEventOwnership,
    NormalizedDshSnapshot, NormalizedEventSpan,
};
pub use source::{
    capture_source, discover_dsh_sessions_in_root, dsh_sessions_root_from_env,
    evaluate_commit_gate, evaluate_saved_commit_gate, parse_canonical_log_filename,
    select_generation, select_generation_auto_compression, CommitGateDecision,
    DshDiscoveredSessionEntry, DshDiscoveredSessionStatus, DshRootDiscoveryReport, DshSavedState,
    HoldReason, RebuildReason, SelectedGeneration, SnapshotRelation, SourceConfig, SourceSnapshot,
};

#[derive(Debug, Clone, Copy, Deserialize)]
struct Limits {
    input_bytes: u64,
    decoded_bytes: u64,
    audit_timeout_ms: u64,
    audit_cleanup_timeout_ms: u64,
    audit_output_bytes: u64,
}

fn limits() -> Limits {
    static LIMITS: OnceLock<Limits> = OnceLock::new();
    *LIMITS.get_or_init(|| {
        serde_json::from_str(include_str!("../scripts/dsh-snapshot-limits.json"))
            .expect("checked-in DSH limits must be valid")
    })
}

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Compression {
    None,
    Zstd,
}

impl Compression {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Zstd => "zstd",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", content = "reason", rename_all = "snake_case")]
pub enum Blocked {
    Unavailable(&'static str),
    Changed(&'static str),
    Incomplete(&'static str),
    Unverified(&'static str),
    Unsupported(&'static str),
    BudgetExceeded(&'static str),
    Invalid(&'static str),
    Rejected(&'static str),
}

/// JSONL 的字节范围包含尾部 LF；native_seq 不是归一化 RawEvent 的 seq。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RecordPosition {
    record_index: usize,
    native_seq: u64,
    jsonl_start: usize,
    jsonl_end: usize,
    frame_index: Option<usize>,
    record_fingerprint: String,
}

impl RecordPosition {
    pub fn record_index(&self) -> usize {
        self.record_index
    }

    pub fn native_seq(&self) -> u64 {
        self.native_seq
    }

    pub fn jsonl_start(&self) -> usize {
        self.jsonl_start
    }

    pub fn jsonl_end(&self) -> usize {
        self.jsonl_end
    }

    pub fn frame_index(&self) -> Option<usize> {
        self.frame_index
    }

    pub fn record_fingerprint(&self) -> &str {
        &self.record_fingerprint
    }
}

#[derive(Debug, Serialize)]
struct FrameSpan {
    physical_start: usize,
    physical_end: usize,
    jsonl_start: usize,
    jsonl_end: usize,
}

/// 只能由实际捕获/验证路径构造；摘要与位置表不提供可变访问。
#[derive(Debug)]
pub struct PhysicalSnapshot {
    header_id: String,
    physical_hash: String,
    jsonl_hash: String,
    header_hash: String,
    physical_bytes: usize,
    decoded_bytes: usize,
    frames: Vec<FrameSpan>,
    positions: Vec<RecordPosition>,
    jsonl: VerifiedJsonl,
}

struct VerifiedJsonl(Vec<u8>);
impl std::fmt::Debug for VerifiedJsonl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VerifiedJsonl")
            .field("len", &self.0.len())
            .finish_non_exhaustive()
    }
}

impl PhysicalSnapshot {
    pub(super) fn header_id(&self) -> &str {
        &self.header_id
    }

    pub(super) fn jsonl_bytes(&self) -> &[u8] {
        &self.jsonl.0
    }

    pub(super) fn positions(&self) -> &[RecordPosition] {
        &self.positions
    }

    /// 输出计数、摘要与可选的原生记录位置；不输出 header 或正文。
    pub fn report(&self, include_positions: bool) -> Value {
        let mut report = json!({
            "kind": "physically_validated",
            "snapshot_sha256": self.physical_hash,
            "jsonl_sha256": self.jsonl_hash,
            "header_sha256": self.header_hash,
            "physical_bytes": self.physical_bytes,
            "decoded_bytes": self.decoded_bytes,
            "records": self.positions.len() + 1,
            "native_events": self.positions.len(),
            "frame_spans": self.frames,
            "semantic_admission_validated": false,
            "source_observation_validated": false,
            "normalized_positions_available": false,
            "atomic_source_snapshot_proven": false,
            "production_advance_permit": false,
            "blockers": ["semantic_admission_required", "source_observation_required", "normalization_required"],
        });
        if include_positions {
            report["record_positions"] = json!(self.positions);
        }
        report
    }
}

/// 一次打开、同句柄身份核对并有界取得字节，再在同一 Vec 上校验；不把 open 的 NotFound 当命名空间事实。
pub fn capture(path: &Path, compression: Compression) -> Result<PhysicalSnapshot, Blocked> {
    let limit = limits().input_bytes;
    match probe::read_pinned_regular_file(path, limit, None) {
        Probed::Found(PinnedRead::Complete { bytes, .. }) => verify(&bytes, compression),
        Probed::Found(PinnedRead::ExceedsBudget { .. }) => {
            Err(Blocked::BudgetExceeded("input_budget"))
        }
        Probed::Found(PinnedRead::Changed(reason)) => Err(Blocked::Changed(reason)),
        Probed::Found(PinnedRead::NotRegularFile) => {
            Err(Blocked::Unavailable("source_read_failed"))
        }
        Probed::Absent => Err(Blocked::Unavailable("open_not_found_unconfirmed")),
        Probed::Unknown(_) => Err(Blocked::Unavailable("source_read_failed")),
    }
}

fn verify(bytes: &[u8], compression: Compression) -> Result<PhysicalSnapshot, Blocked> {
    let limits = limits();
    if bytes.len() as u64 > limits.input_bytes {
        return Err(Blocked::BudgetExceeded("input_budget"));
    }
    if bytes.is_empty() {
        return Err(Blocked::Incomplete("empty"));
    }
    let (plain, frames) = match compression {
        Compression::None => (bytes.to_vec(), Vec::new()),
        Compression::Zstd => {
            let checked = checked_frames(bytes, limits.decoded_bytes).map_err(|failure| {
                use FrameCheckFailure as F;
                match failure {
                    F::Incomplete => Blocked::Incomplete("torn_frame_header"),
                    F::Unverified => Blocked::Unverified("frame_decode_or_checksum_unverified"),
                    F::ChecksumAbsent => Blocked::Unsupported("checksum_absent"),
                    F::DictionaryRequired => Blocked::Unsupported("dictionary_required"),
                    F::UnsupportedFrame => Blocked::Unsupported("frame_kind"),
                    F::BudgetExceeded => Blocked::BudgetExceeded("decoded_or_window_budget"),
                }
            })?;
            let mut plain = Vec::new();
            let mut frames = Vec::new();
            for (index, frame) in checked.into_iter().enumerate() {
                if index == 0 && frame.decoded.iter().filter(|&&b| b == b'\n').count() != 1 {
                    return Err(Blocked::Invalid("header_frame_not_single_record"));
                }
                if !frame.decoded.is_empty() && !frame.decoded.ends_with(b"\n") {
                    return Err(Blocked::Invalid("frame_record_boundary"));
                }
                let start = plain.len();
                plain.extend_from_slice(&frame.decoded);
                frames.push(FrameSpan {
                    physical_start: frame.physical_start,
                    physical_end: frame.physical_end,
                    jsonl_start: start,
                    jsonl_end: plain.len(),
                });
            }
            (plain, frames)
        }
    };
    if plain.len() as u64 > limits.decoded_bytes {
        return Err(Blocked::BudgetExceeded("decoded_budget"));
    }
    if !plain.ends_with(b"\n") {
        return Err(Blocked::Incomplete("unterminated_record"));
    }
    let mut start = 0;
    let mut frame_index = 0;
    let mut positions = Vec::new();
    let mut header_id = None;
    let mut header_hash = None;
    for (record_index, raw) in plain.split_inclusive(|&b| b == b'\n').enumerate() {
        let text = std::str::from_utf8(&raw[..raw.len() - 1])
            .map_err(|_| Blocked::Invalid("invalid_utf8"))?;
        let value: Value =
            serde_json::from_str(text).map_err(|_| Blocked::Invalid("invalid_json"))?;
        let object = value
            .as_object()
            .ok_or(Blocked::Invalid("non_object_record"))?;
        if record_index == 0 {
            if object.get("type").and_then(Value::as_str) != Some("session") {
                return Err(Blocked::Rejected("header_envelope"));
            }
            if object.get("version").and_then(Value::as_u64) != Some(4) {
                return Err(Blocked::Unsupported("header_generation"));
            }
            let Some(id) = object
                .get("id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            else {
                return Err(Blocked::Rejected("header_identity"));
            };
            header_id = Some(id.to_string());
            header_hash = Some(hash(raw));
        } else {
            let seq = object
                .get("seq")
                .and_then(Value::as_u64)
                .ok_or(Blocked::Rejected("native_sequence"))?;
            if seq != positions.len() as u64
                || object
                    .get("type")
                    .and_then(Value::as_str)
                    .is_none_or(|t| t.is_empty() || t == "session")
            {
                return Err(Blocked::Rejected("event_envelope"));
            }
            while frames
                .get(frame_index)
                .is_some_and(|frame| start >= frame.jsonl_end)
            {
                frame_index += 1;
            }
            positions.push(RecordPosition {
                record_index,
                native_seq: seq,
                jsonl_start: start,
                jsonl_end: start + raw.len(),
                frame_index: (compression == Compression::Zstd).then_some(frame_index),
                record_fingerprint: EventKey::fingerprint_of(text),
            });
        }
        start += raw.len();
    }
    Ok(PhysicalSnapshot {
        header_id: header_id.ok_or(Blocked::Incomplete("missing_header"))?,
        physical_hash: hash(bytes),
        jsonl_hash: hash(&plain),
        header_hash: header_hash.ok_or(Blocked::Incomplete("missing_header"))?,
        physical_bytes: bytes.len(),
        decoded_bytes: plain.len(),
        frames,
        positions,
        jsonl: VerifiedJsonl(plain),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEADER: &[u8] = b"{\"type\":\"session\",\"version\":4,\"id\":\"s0\"}\n";

    #[test]
    fn positions_are_utf8_byte_ranges_and_raw_sequences() {
        let event = "{\"type\":\"fixture/event\",\"seq\":0,\"text\":\"中文\"}\n".as_bytes();
        let bytes = [HEADER, event].concat();
        let snapshot = verify(&bytes, Compression::None).unwrap();
        assert_eq!(snapshot.positions.len(), 1);
        let position = &snapshot.positions[0];
        assert_eq!(&bytes[position.jsonl_start..position.jsonl_end], event);
        assert_eq!(position.native_seq, 0);
        assert_eq!(position.frame_index, None);
        let report = snapshot.report(false);
        assert_eq!(report["production_advance_permit"], false);
        assert_eq!(report["semantic_admission_validated"], false);
        assert!(report.get("record_positions").is_none());
    }

    #[test]
    fn invalid_and_incomplete_input_do_not_construct_snapshots() {
        assert_eq!(
            verify(b"", Compression::None).unwrap_err(),
            Blocked::Incomplete("empty")
        );
        assert_eq!(
            verify(&HEADER[..HEADER.len() - 1], Compression::None).unwrap_err(),
            Blocked::Incomplete("unterminated_record")
        );
        let invalid = [HEADER, b"{\"seq\":0,\"type\":\"x\",\"text\":\"\xff\"}\n"].concat();
        assert_eq!(
            verify(&invalid, Compression::None).unwrap_err(),
            Blocked::Invalid("invalid_utf8")
        );
        let hole = [HEADER, b"{\"seq\":1,\"type\":\"x\"}\n"].concat();
        assert_eq!(
            verify(&hole, Compression::None).unwrap_err(),
            Blocked::Rejected("event_envelope")
        );
    }
}
