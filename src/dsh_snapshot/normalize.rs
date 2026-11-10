//! DSH v4 已验证快照的归一化位置映射、观察与保守写库接线（仅 `dsh-experimental`）。
//!
//! 严格对齐 `docs/dsh-ingest-survey.md` §7–§12 与 `docs/dsh-read-state-design.md` §6–§9：
//! - 只消费已经通过物理核验与原生 admission 的 `AdmittedSnapshot` / `SourceSnapshot`；
//! - `native_seq < inherited_event_count` 的父会话历史标记为 `InheritedPrefix`，不重复发射为当前 child 会话的 `RawEvent`；
//! - 同 `(turn, step)` 的 `usage` 先按是否有配对 `llm/retry` + `llm/retry-started` 决议槽位，只有显式给齐四桶且与 `totalTokens` 一致时才发射 `EventType::Usage`，缺失 cache 桶绝不填 0；
//! - 在 `store` 门控下将 `CommitGateDecision` 映射为 `AppendLogObservation` + `CommitPlan` + `TotalStore::apply_projection`，仅在提交成功且 `CursorAction::Advance` 时推进 `DshSavedState`。

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::native::AdmittedSnapshot;
use super::source::{CommitGateDecision, DshSavedState, SnapshotRelation, SourceSnapshot};
use super::{Blocked, PhysicalSnapshot, RecordPosition};
use crate::attribution::RootRegistry;
use crate::catalog::Profile;
use crate::parser::{attribution_from_cwd, scan_default_distro, PARSER_REVISION};
use crate::pathnorm::HostPlatform;
use crate::rawevent::{
    Actor, EventKey, EventType, RawEvent, SourceLocation, SourceMode, SourceType, TimeConfidence,
    TokenUsage, EVENT_KEY_VERSION, SCHEMA_VERSION,
};

/// DSH 归一化配置：注入来源位置、路径、Profile、宿主平台与项目根注册表。
#[derive(Clone)]
pub struct DshNormalizeConfig {
    pub source_location: SourceLocation,
    pub source_path: String,
    pub profile: Profile,
    pub host: HostPlatform,
    pub roots: Arc<RootRegistry>,
    pub attribution_revision: i64,
}

impl DshNormalizeConfig {
    pub fn local_full(source_path: impl Into<String>) -> Self {
        Self {
            source_location: SourceLocation::Local,
            source_path: source_path.into(),
            profile: Profile::Full,
            host: HostPlatform::current(),
            roots: Arc::new(RootRegistry::new()),
            attribution_revision: 0,
        }
    }

    fn want_content(&self) -> bool {
        matches!(self.profile, Profile::Full)
    }
}

/// 原生事件相对当前会话继承边界的归属。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeEventOwnership {
    /// `native_seq < inherited_event_count`：来自父会话的继承前缀，不发射为当前 child 会话的 `RawEvent`。
    InheritedPrefix,
    /// `native_seq >= inherited_event_count`：当前会话自身产生的事件。
    OwnEvent,
}

/// 一条已发射 `RawEvent` 与底层原生记录位置的精确映射。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NormalizedEventSpan {
    pub raw_seq: u64,
    pub event_type: EventType,
    pub event_key: EventKey,
    pub ownership: NativeEventOwnership,
    pub record_index: usize,
    pub native_seq: u64,
    pub jsonl_start: usize,
    pub jsonl_end: usize,
    pub frame_index: Option<usize>,
    pub record_fingerprint: String,
    /// 原生表面操作（`"append"` | `"replace"` | 非表面事件为 `None`）。
    pub surface_op: Option<String>,
    /// 该记录是否仍在折叠后的当前活跃对话表面（surface）上。
    pub active_on_surface: bool,
    /// 若该表面记录已被后续 `surfaceOp: { op: "replace" }` 折叠替换，记录替换它的 `native_seq`。
    pub shadowed_by_native_seq: Option<u64>,
}

/// DSH v4 `subagent/descriptor` 元数据记录（子会话自身的委托描述符）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DshSubagentDescriptor {
    pub native_seq: u64,
    pub version: u64,
    pub provider: String,
    pub mode: String,
    pub label: Option<String>,
    pub ownership: NativeEventOwnership,
}

/// DSH v4 `subagent/catalog` 元数据记录（父会话登记的子代理目录项）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DshSubagentCatalogEntry {
    pub native_seq: u64,
    pub version: u64,
    pub child_id: String,
    pub child_created_at: Option<String>,
    pub mode: String,
    pub label: Option<String>,
    pub ownership: NativeEventOwnership,
}

/// DSH v4 `compaction/*` 生命周期与摘要事件记录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DshCompactionRecord {
    pub native_seq: u64,
    pub native_type: String,
    pub compaction_id: Option<String>,
    pub turn: Option<u64>,
    pub shadowed_start_seq: Option<u64>,
    pub shadowed_end_seq: Option<u64>,
    pub shadowed_seqs: Vec<u64>,
    pub ownership: NativeEventOwnership,
}

/// DSH v4 `surfaceOp: { op: "replace", startSeq, endSeq }` 表面替换记录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DshSurfaceReplacement {
    pub replacement_native_seq: u64,
    pub native_type: String,
    pub start_seq: u64,
    pub end_seq: u64,
    pub source_event_seqs: Vec<u64>,
    pub replaced_surface_seqs: Vec<u64>,
    pub ownership: NativeEventOwnership,
}

/// 一条 DSH 原生 usage 记录的归一化处置状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DshUsageStatus {
    /// 显式给齐四桶且与 `totalTokens` 一致，已作为 `EventType::Usage` 发射。
    ExactFourBucket,
    /// 缺失 `cacheReadTokens` 或 `cacheWriteTokens`，或四桶之和不等于 `totalTokens`；不填 0 冒充已知四桶。
    PartialOrUnreconciled,
    /// 同 `(turn, step)` 内无确认重试，被同 step 后一条 usage 覆盖。
    SupersededBySameStep,
}

/// 单条原生 usage 记录的审计台账项。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DshUsageEntry {
    pub native_seq: u64,
    pub source_kind: String,
    pub turn: Option<u64>,
    pub step: Option<u64>,
    pub model: Option<String>,
    pub status: DshUsageStatus,
    pub emitted_raw_seq: Option<u64>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
}

/// 已完成原生记录位置到 `RawEvent` 映射的 DSH 快照结果。
#[derive(Debug, Clone)]
pub struct NormalizedDshSnapshot {
    pub session_id: String,
    pub cwd: Option<String>,
    pub parent_session_id: Option<String>,
    pub forked_from_seq: Option<u64>,
    pub is_seeded: bool,
    pub session_origin: Option<String>,
    pub delegation_depth: Option<u64>,
    pub agent_preset: Option<String>,
    pub inherited_event_count: usize,
    pub suppressed_inherited_records: usize,
    pub events: Vec<RawEvent>,
    pub spans: Vec<NormalizedEventSpan>,
    pub usage_ledger: Vec<DshUsageEntry>,
    pub subagent_descriptors: Vec<DshSubagentDescriptor>,
    pub subagent_catalog: Vec<DshSubagentCatalogEntry>,
    pub compaction_records: Vec<DshCompactionRecord>,
    pub surface_replacements: Vec<DshSurfaceReplacement>,
    pub active_surface_native_seqs: Vec<u64>,
}

impl NormalizedDshSnapshot {
    pub fn next_seq(&self) -> u64 {
        self.events.len() as u64
    }

    /// 取出 `native_seq >= start_native_seq` 的增量事件及其对应位置跨度。
    pub fn events_from_native_seq(
        &self,
        start_native_seq: u64,
    ) -> (Vec<RawEvent>, Vec<NormalizedEventSpan>) {
        let mut events = Vec::new();
        let mut spans = Vec::new();
        for (ev, span) in self.events.iter().zip(self.spans.iter()) {
            if span.native_seq >= start_native_seq {
                events.push(ev.clone());
                spans.push(span.clone());
            }
        }
        (events, spans)
    }

    /// 输出包含归一化位置与用量台账的报告（不含对话正文）。
    pub fn report(&self, base_report: Value, include_positions: bool) -> Value {
        let mut report = base_report;
        report["normalized_positions_available"] = json!(true);
        if let Some(arr) = report.get_mut("blockers").and_then(Value::as_array_mut) {
            arr.retain(|item| item.as_str() != Some("normalization_required"));
        }
        let exact_usages = self
            .usage_ledger
            .iter()
            .filter(|u| u.status == DshUsageStatus::ExactFourBucket)
            .count();
        let partial_usages = self
            .usage_ledger
            .iter()
            .filter(|u| u.status == DshUsageStatus::PartialOrUnreconciled)
            .count();
        let superseded_usages = self
            .usage_ledger
            .iter()
            .filter(|u| u.status == DshUsageStatus::SupersededBySameStep)
            .count();
        report["normalization"] = json!({
            "session_id": self.session_id,
            "has_cwd": self.cwd.is_some(),
            "parent_session_id": self.parent_session_id,
            "forked_from_seq": self.forked_from_seq,
            "is_seeded": self.is_seeded,
            "session_origin": self.session_origin,
            "delegation_depth": self.delegation_depth,
            "agent_preset": self.agent_preset,
            "inherited_event_count": self.inherited_event_count,
            "suppressed_inherited_records": self.suppressed_inherited_records,
            "emitted_raw_events": self.events.len(),
            "next_seq": self.next_seq(),
            "subagent_descriptor_count": self.subagent_descriptors.len(),
            "subagent_catalog_count": self.subagent_catalog.len(),
            "compaction_record_count": self.compaction_records.len(),
            "surface_replacement_count": self.surface_replacements.len(),
            "active_surface_event_count": self.active_surface_native_seqs.len(),
            "active_surface_native_seqs": self.active_surface_native_seqs,
            "usage_summary": {
                "total_native_usage_records": self.usage_ledger.len(),
                "emitted_exact_four_bucket": exact_usages,
                "partial_or_unreconciled": partial_usages,
                "superseded_by_same_step": superseded_usages,
            },
        });
        if include_positions {
            report["normalized_event_spans"] = json!(self.spans);
            report["usage_ledger"] = json!(self.usage_ledger);
            report["subagent_descriptors"] = json!(self.subagent_descriptors);
            report["subagent_catalog"] = json!(self.subagent_catalog);
            report["compaction_records"] = json!(self.compaction_records);
            report["surface_replacements"] = json!(self.surface_replacements);
        }
        report
    }
}

/// 将 Unix 毫秒时间戳转换为固定宽度 UTC ISO-8601 字符串（`YYYY-MM-DDTHH:MM:SS.mmmZ`），
/// 与 `crate::rawevent::occurred_at_unix_ms` 精确互逆。
pub fn epoch_ms_to_iso8601_utc(ms: i64) -> String {
    let days = ms.div_euclid(86_400_000);
    let rem_ms = ms.rem_euclid(86_400_000);
    let hour = rem_ms / 3_600_000;
    let minute = (rem_ms % 3_600_000) / 60_000;
    let second = (rem_ms % 60_000) / 1_000;
    let millis = rem_ms % 1_000;
    let (year, month, day) = civil_from_days(days);
    if (0..=9999).contains(&year) {
        format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
    } else {
        format!("{year:+05}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
    }
}

/// Howard Hinnant `civil_from_days`：自 1970-01-01 起的天数 → `(year, month, day)`。
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// 归一化已通过原生 admission 的 `AdmittedSnapshot`。
pub fn normalize_admitted_snapshot(
    admitted: &AdmittedSnapshot,
    cfg: &DshNormalizeConfig,
) -> Result<NormalizedDshSnapshot, Blocked> {
    normalize_verified(
        admitted.physical(),
        admitted.session_id(),
        admitted.inherited_event_count(),
        cfg,
    )
}

/// 归一化已通过来源观察与原生 admission 的 `SourceSnapshot`。
pub fn normalize_source_snapshot(
    snapshot: &SourceSnapshot,
    cfg: &DshNormalizeConfig,
) -> Result<NormalizedDshSnapshot, Blocked> {
    normalize_admitted_snapshot(snapshot.admitted(), cfg)
}

struct ParsedRecord<'a> {
    position: &'a RecordPosition,
    value: Value,
}

#[derive(Clone)]
struct RawUsageCandidate {
    native_seq: u64,
    source_kind: String,
    turn: Option<u64>,
    step: Option<u64>,
    model: Option<String>,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_read_tokens: Option<u64>,
    cache_write_tokens: Option<u64>,
    total_tokens: Option<u64>,
    exact_usage: Option<TokenUsage>,
}

struct RecordSlotCounter {
    fingerprint: String,
    counts: Vec<(EventType, u32)>,
}

impl RecordSlotCounter {
    fn new(fingerprint: &str) -> Self {
        Self {
            fingerprint: fingerprint.to_string(),
            counts: Vec::new(),
        }
    }

    fn next(&mut self, event_type: EventType) -> EventKey {
        let ordinal = match self.counts.iter_mut().find(|(ty, _)| *ty == event_type) {
            Some((_, count)) => {
                *count += 1;
                *count - 1
            }
            None => {
                self.counts.push((event_type, 1));
                0
            }
        };
        EventKey {
            version: EVENT_KEY_VERSION,
            record_fingerprint: self.fingerprint.clone(),
            slot_ordinal: ordinal,
        }
    }
}

fn normalize_verified(
    physical: &PhysicalSnapshot,
    session_id: &str,
    inherited_event_count: usize,
    cfg: &DshNormalizeConfig,
) -> Result<NormalizedDshSnapshot, Blocked> {
    let bytes = physical.jsonl_bytes();
    let positions = physical.positions();
    if inherited_event_count > positions.len() {
        return Err(Blocked::Unverified("native_binding_mismatch"));
    }

    let first_end = positions
        .first()
        .map(|pos| pos.jsonl_start())
        .unwrap_or(bytes.len());
    if first_end == 0 || bytes.get(first_end - 1) != Some(&b'\n') {
        return Err(Blocked::Invalid("missing_header"));
    }
    let header_text = std::str::from_utf8(&bytes[..first_end - 1])
        .map_err(|_| Blocked::Invalid("invalid_utf8"))?;
    let header_val: Value =
        serde_json::from_str(header_text).map_err(|_| Blocked::Invalid("invalid_json"))?;
    let header_obj = header_val
        .as_object()
        .ok_or(Blocked::Invalid("non_object_record"))?;

    let cwd = header_obj
        .get("cwd")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let parent_session_id = header_obj
        .get("parentSession")
        .or_else(|| header_obj.get("parentSessionId"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let forked_from_seq = header_obj.get("forkedFromSeq").and_then(Value::as_u64);
    let is_seeded = header_obj
        .get("isSeeded")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let session_origin = header_obj
        .get("origin")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let delegation_depth = header_obj.get("delegationDepth").and_then(Value::as_u64);
    let agent_preset = header_obj
        .get("agentPreset")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let default_distro = scan_default_distro(&cfg.source_location);
    let attribution = attribution_from_cwd(
        cwd.as_deref(),
        &cfg.source_location,
        cfg.host,
        default_distro.as_deref(),
        &cfg.roots,
    );

    let mut parsed_records = Vec::with_capacity(positions.len());
    for pos in positions {
        let slice = bytes
            .get(pos.jsonl_start()..pos.jsonl_end())
            .ok_or(Blocked::Invalid("record_position_out_of_bounds"))?;
        if slice.last() != Some(&b'\n') {
            return Err(Blocked::Invalid("unterminated_record"));
        }
        let line = std::str::from_utf8(&slice[..slice.len() - 1])
            .map_err(|_| Blocked::Invalid("invalid_utf8"))?;
        let value: Value =
            serde_json::from_str(line).map_err(|_| Blocked::Invalid("invalid_json"))?;
        parsed_records.push(ParsedRecord {
            position: pos,
            value,
        });
    }

    // 预演完整事件流（含继承前缀）的 DSH v4 对话表面折叠（surface fold），并收集复杂语义台账：
    // - 表面事件类型：`system/message`、`developer/message`、`user/message`、`assistant/message`、`tool/result`
    // - `surfaceOp: "append"`（或未指定）追加到表面尾部；
    // - `surfaceOp: { op: "replace", startSeq, endSeq }` 将当前表面上 `[startSeq..=endSeq]` 的闭区间节点
    //   替换为当前事件（遵循 DSH `session.js`：若区间起点为索引 0 的 `system/message`，且替换者不是仅替换索引 0
    //   的 `system/message`，则保护索引 0 的 `system/message` 不被覆盖，仅替换 `1..=end_idx`）。
    let mut surface_nodes: Vec<(u64, String)> = Vec::new();
    let mut shadowed_by_map: BTreeMap<u64, u64> = BTreeMap::new();
    let mut surface_op_by_seq: BTreeMap<u64, String> = BTreeMap::new();
    let mut subagent_descriptors: Vec<DshSubagentDescriptor> = Vec::new();
    let mut subagent_catalog: Vec<DshSubagentCatalogEntry> = Vec::new();
    let mut compaction_records: Vec<DshCompactionRecord> = Vec::new();
    let mut surface_replacements: Vec<DshSurfaceReplacement> = Vec::new();

    for (idx, rec) in parsed_records.iter().enumerate() {
        let native_seq = rec.position.native_seq();
        let ownership = if idx < inherited_event_count {
            NativeEventOwnership::InheritedPrefix
        } else {
            NativeEventOwnership::OwnEvent
        };
        let event_type_str = rec
            .value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();

        if is_surface_event_type(event_type_str) {
            if let Some((start_seq, end_seq, source_event_seqs)) =
                extract_surface_replace_spec(&rec.value)
            {
                surface_op_by_seq.insert(native_seq, "replace".to_string());
                let start_idx = surface_nodes.iter().position(|(s, _)| *s == start_seq);
                let end_idx = surface_nodes.iter().position(|(s, _)| *s == end_seq);
                let mut replaced_surface_seqs = Vec::new();
                if let (Some(s_idx), Some(e_idx)) = (start_idx, end_idx) {
                    if s_idx <= e_idx {
                        let protect_leading_system = event_type_str != "tool/result"
                            && s_idx == 0
                            && surface_nodes
                                .first()
                                .is_some_and(|(_, ty)| ty == "system/message")
                            && !(e_idx == 0 && event_type_str == "system/message");
                        let replace_from = if protect_leading_system { 1 } else { s_idx };
                        if replace_from <= e_idx {
                            for (old_seq, _) in &surface_nodes[replace_from..=e_idx] {
                                shadowed_by_map.insert(*old_seq, native_seq);
                                replaced_surface_seqs.push(*old_seq);
                            }
                            surface_nodes.splice(
                                replace_from..=e_idx,
                                std::iter::once((native_seq, event_type_str.to_string())),
                            );
                        } else {
                            // 仅索引 0 被保护且 e_idx == 0 时，新节点直接插入索引 1
                            surface_nodes.insert(1, (native_seq, event_type_str.to_string()));
                        }
                    }
                }
                surface_replacements.push(DshSurfaceReplacement {
                    replacement_native_seq: native_seq,
                    native_type: event_type_str.to_string(),
                    start_seq,
                    end_seq,
                    source_event_seqs,
                    replaced_surface_seqs,
                    ownership,
                });
            } else {
                surface_op_by_seq.insert(native_seq, "append".to_string());
                surface_nodes.push((native_seq, event_type_str.to_string()));
            }
        }

        match event_type_str {
            "subagent/descriptor" => {
                if let Some(desc) = extract_subagent_descriptor(native_seq, &rec.value, ownership) {
                    subagent_descriptors.push(desc);
                }
            }
            "subagent/catalog" => {
                if let Some(entry) =
                    extract_subagent_catalog_entry(native_seq, &rec.value, ownership)
                {
                    subagent_catalog.push(entry);
                }
            }
            "compaction/start" | "compaction/summary" | "compaction/end" | "compaction/prune" => {
                compaction_records.push(extract_compaction_record(
                    native_seq,
                    event_type_str,
                    &rec.value,
                    ownership,
                ));
            }
            _ => {}
        }
    }

    let active_surface_native_seqs: Vec<u64> = surface_nodes.iter().map(|(seq, _)| *seq).collect();
    let active_surface_set: BTreeSet<u64> = active_surface_native_seqs.iter().copied().collect();

    // 第一遍扫描本会话自有记录（native_seq >= inherited_event_count），决议同 (turn, step) 的 usage 槽位。
    // 规则（docs/dsh-ingest-survey.md §9–§11）：
    // - 同一 (turn, step) 内若出现成对的 `llm/retry` -> `llm/retry-started`，则保留重试前的失败 attempt usage；
    // - 否则同一 (turn, step) 内的后一条 usage（如成功的 `assistant/message`）覆盖前一条槽位；
    // - 失败 attempt 不借用后续成功 `assistant/message` 的 `model`。
    let mut pending_retry_steps: BTreeSet<(u64, u64)> = BTreeSet::new();
    let mut confirmed_retry_steps: BTreeSet<(u64, u64)> = BTreeSet::new();
    let mut usage_candidates: BTreeMap<u64, RawUsageCandidate> = BTreeMap::new();
    let mut latest_slot_winner: BTreeMap<(u64, u64), u64> = BTreeMap::new();

    for rec in parsed_records.iter().skip(inherited_event_count) {
        let event_type_str = rec
            .value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let (turn, step) = extract_turn_step(&rec.value);
        match event_type_str {
            "llm/retry" => {
                if let (Some(t), Some(s)) = (turn, step) {
                    pending_retry_steps.insert((t, s));
                }
            }
            "llm/retry-started" => {
                if let (Some(t), Some(s)) = (turn, step) {
                    if pending_retry_steps.remove(&(t, s)) {
                        confirmed_retry_steps.insert((t, s));
                    }
                }
            }
            "assistant/attempt" | "assistant/message" => {
                if let Some(usage_val) = extract_dsh_usage_object(&rec.value) {
                    let candidate = parse_usage_candidate(
                        rec.position.native_seq(),
                        event_type_str,
                        turn,
                        step,
                        extract_dsh_model(&rec.value).as_deref(),
                        usage_val,
                    );
                    if let (Some(t), Some(s)) = (turn, step) {
                        latest_slot_winner.insert((t, s), rec.position.native_seq());
                    }
                    usage_candidates.insert(rec.position.native_seq(), candidate);
                }
            }
            _ => {}
        }
    }

    let mut events = Vec::new();
    let mut spans = Vec::new();
    let mut usage_ledger = Vec::new();
    let mut suppressed_inherited_records = 0usize;

    for (idx, rec) in parsed_records.iter().enumerate() {
        if idx < inherited_event_count {
            suppressed_inherited_records += 1;
            continue;
        }
        let pos = rec.position;
        let ownership = NativeEventOwnership::OwnEvent;
        let mut slots = RecordSlotCounter::new(pos.record_fingerprint());
        let occurred_at = extract_dsh_time_ms(&rec.value).map(epoch_ms_to_iso8601_utc);
        let model = extract_dsh_model(&rec.value);
        let kind = rec
            .value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();

        let rec_surface_op = surface_op_by_seq.get(&pos.native_seq()).cloned();
        let rec_active_on_surface = active_surface_set.contains(&pos.native_seq());
        let rec_shadowed_by = shadowed_by_map.get(&pos.native_seq()).copied();

        let mut push_event = |event_type: EventType,
                              actor: Option<Actor>,
                              content: Option<String>,
                              ev_model: Option<String>,
                              usage: Option<TokenUsage>|
         -> u64 {
            let raw_seq = events.len() as u64;
            let event_key = slots.next(event_type);
            let has_time = occurred_at.is_some();
            let input_origin = if event_type == EventType::Message && actor == Some(Actor::User) {
                Some(crate::session_binding::classify_dsh_user_input_origin(
                    Some(&rec.value),
                    content.as_deref().unwrap_or(""),
                    &cfg.source_path,
                    session_origin.as_deref(),
                    parent_session_id.as_deref(),
                ))
            } else {
                None
            };
            let ev = RawEvent {
                schema_version: SCHEMA_VERSION,
                source_type: SourceType::Jsonl,
                source_location: cfg.source_location.clone(),
                source_path: cfg.source_path.clone(),
                source_session_id: session_id.to_string(),
                seq: raw_seq,
                event_key: Some(event_key.clone()),
                source_mode: SourceMode::AppendLog,
                cwd: cwd.clone(),
                project_root: attribution.project_root.clone(),
                project_root_source: attribution.project_root_source.clone(),
                workspace_location: attribution.workspace_location.clone(),
                event_type,
                actor,
                occurred_at: occurred_at.clone(),
                time_confidence: if has_time {
                    TimeConfidence::High
                } else {
                    TimeConfidence::Low
                },
                model: ev_model,
                effort: None,
                usage,
                content: if cfg.want_content() { content } else { None },
                parent_ref: parent_session_id.clone(),
                content_hash: None,
                artifact_kind: None,
                observed_at: None,
                modified_at: None,
                message_id: None,
                request_id: None,
                input_origin,
            };
            events.push(ev);
            spans.push(NormalizedEventSpan {
                raw_seq,
                event_type,
                event_key,
                ownership,
                record_index: pos.record_index(),
                native_seq: pos.native_seq(),
                jsonl_start: pos.jsonl_start(),
                jsonl_end: pos.jsonl_end(),
                frame_index: pos.frame_index(),
                record_fingerprint: pos.record_fingerprint().to_string(),
                surface_op: rec_surface_op.clone(),
                active_on_surface: rec_active_on_surface,
                shadowed_by_native_seq: rec_shadowed_by,
            });
            raw_seq
        };

        match kind {
            "system/message" => {
                if let Some(text) = extract_dsh_message_content(&rec.value, "text") {
                    push_event(
                        EventType::Message,
                        Some(Actor::System),
                        Some(text),
                        None,
                        None,
                    );
                }
            }
            "developer/message" => {
                if let Some(text) = extract_dsh_message_content(&rec.value, "text") {
                    // developer/message 与 system/message 在 DSH 恢复语义中角色不同，actor 置 None 不冒充 System。
                    push_event(EventType::Message, None, Some(text), None, None);
                }
            }
            "user/message" => {
                if let Some(text) = extract_dsh_user_text(&rec.value) {
                    push_event(
                        EventType::Message,
                        Some(Actor::User),
                        Some(text),
                        None,
                        None,
                    );
                }
            }
            "assistant/attempt" => {
                if let Some(candidate) = usage_candidates.get(&pos.native_seq()) {
                    record_usage_resolution(
                        candidate,
                        &confirmed_retry_steps,
                        &latest_slot_winner,
                        &mut usage_ledger,
                        &mut push_event,
                    );
                }
            }
            "assistant/message" => {
                if let Some(thinking) = extract_dsh_assistant_reasoning(&rec.value) {
                    push_event(
                        EventType::Thinking,
                        Some(Actor::Assistant),
                        Some(thinking),
                        model.clone(),
                        None,
                    );
                }
                if let Some(text) = extract_dsh_message_content(&rec.value, "text") {
                    push_event(
                        EventType::Message,
                        Some(Actor::Assistant),
                        Some(text),
                        model.clone(),
                        None,
                    );
                }
                if let Some(candidate) = usage_candidates.get(&pos.native_seq()) {
                    record_usage_resolution(
                        candidate,
                        &confirmed_retry_steps,
                        &latest_slot_winner,
                        &mut usage_ledger,
                        &mut push_event,
                    );
                }
            }
            "tool/call" => {
                let tool_name = extract_dsh_tool_name(&rec.value);
                push_event(
                    EventType::ToolUse,
                    Some(Actor::Assistant),
                    Some(format!("[Tool: {tool_name}]")),
                    None,
                    None,
                );
            }
            "tool/result" => {
                if let Some(output) = extract_dsh_tool_result(&rec.value) {
                    push_event(
                        EventType::ToolResult,
                        Some(Actor::Tool),
                        Some(output),
                        None,
                        None,
                    );
                }
            }
            _ => {}
        }
    }

    Ok(NormalizedDshSnapshot {
        session_id: session_id.to_string(),
        cwd,
        parent_session_id,
        forked_from_seq,
        is_seeded,
        session_origin,
        delegation_depth,
        agent_preset,
        inherited_event_count,
        suppressed_inherited_records,
        events,
        spans,
        usage_ledger,
        subagent_descriptors,
        subagent_catalog,
        compaction_records,
        surface_replacements,
        active_surface_native_seqs,
    })
}

fn is_surface_event_type(event_type: &str) -> bool {
    matches!(
        event_type,
        "system/message"
            | "developer/message"
            | "user/message"
            | "assistant/message"
            | "tool/result"
    )
}

fn extract_surface_replace_spec(value: &Value) -> Option<(u64, u64, Vec<u64>)> {
    let op_obj = value.get("surfaceOp")?.as_object()?;
    if op_obj.get("op").and_then(Value::as_str) != Some("replace") {
        return None;
    }
    let start_seq = op_obj.get("startSeq").and_then(Value::as_u64)?;
    let end_seq = op_obj.get("endSeq").and_then(Value::as_u64)?;
    let source_event_seqs = value
        .get("sourceEventSeqs")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().filter_map(Value::as_u64).collect())
        .unwrap_or_default();
    Some((start_seq, end_seq, source_event_seqs))
}

fn extract_subagent_descriptor(
    native_seq: u64,
    value: &Value,
    ownership: NativeEventOwnership,
) -> Option<DshSubagentDescriptor> {
    let data = value.get("data").unwrap_or(value);
    let version = data.get("version").and_then(Value::as_u64).unwrap_or(1);
    let provider = data.get("provider").and_then(Value::as_str)?.to_string();
    let mode = data.get("mode").and_then(Value::as_str)?.to_string();
    let label = non_empty_str(data.get("label"));
    Some(DshSubagentDescriptor {
        native_seq,
        version,
        provider,
        mode,
        label,
        ownership,
    })
}

fn extract_subagent_catalog_entry(
    native_seq: u64,
    value: &Value,
    ownership: NativeEventOwnership,
) -> Option<DshSubagentCatalogEntry> {
    let data = value.get("data").unwrap_or(value);
    let version = data.get("version").and_then(Value::as_u64).unwrap_or(1);
    let child_id = data.get("childId").and_then(Value::as_str)?.to_string();
    let child_created_at = non_empty_str(data.get("childCreatedAt"));
    let mode = data.get("mode").and_then(Value::as_str)?.to_string();
    let label = non_empty_str(data.get("label"));
    Some(DshSubagentCatalogEntry {
        native_seq,
        version,
        child_id,
        child_created_at,
        mode,
        label,
        ownership,
    })
}

fn extract_compaction_record(
    native_seq: u64,
    native_type: &str,
    value: &Value,
    ownership: NativeEventOwnership,
) -> DshCompactionRecord {
    let data = value.get("data").unwrap_or(value);
    let compaction_id = non_empty_str(data.get("compactionId"));
    let turn = data.get("turn").and_then(Value::as_u64);
    let shadowed_range = data.get("shadowedRange");
    let shadowed_start_seq = shadowed_range
        .and_then(|r| r.get("start"))
        .and_then(Value::as_u64);
    let shadowed_end_seq = shadowed_range
        .and_then(|r| r.get("end"))
        .and_then(Value::as_u64);
    let shadowed_seqs = data
        .get("shadowedSeqs")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().filter_map(Value::as_u64).collect())
        .unwrap_or_default();
    DshCompactionRecord {
        native_seq,
        native_type: native_type.to_string(),
        compaction_id,
        turn,
        shadowed_start_seq,
        shadowed_end_seq,
        shadowed_seqs,
        ownership,
    }
}

fn record_usage_resolution<F>(
    candidate: &RawUsageCandidate,
    confirmed_retry_steps: &BTreeSet<(u64, u64)>,
    latest_slot_winner: &BTreeMap<(u64, u64), u64>,
    usage_ledger: &mut Vec<DshUsageEntry>,
    push_event: &mut F,
) where
    F: FnMut(EventType, Option<Actor>, Option<String>, Option<String>, Option<TokenUsage>) -> u64,
{
    let is_superseded = match (candidate.turn, candidate.step) {
        (Some(t), Some(s)) if !confirmed_retry_steps.contains(&(t, s)) => latest_slot_winner
            .get(&(t, s))
            .is_some_and(|&winner_seq| winner_seq != candidate.native_seq),
        _ => false,
    };

    let (status, emitted_raw_seq) = if is_superseded {
        (DshUsageStatus::SupersededBySameStep, None)
    } else if let Some(exact) = candidate.exact_usage {
        let seq = push_event(
            EventType::Usage,
            Some(Actor::Assistant),
            None,
            candidate.model.clone(),
            Some(exact),
        );
        (DshUsageStatus::ExactFourBucket, Some(seq))
    } else {
        (DshUsageStatus::PartialOrUnreconciled, None)
    };

    usage_ledger.push(DshUsageEntry {
        native_seq: candidate.native_seq,
        source_kind: candidate.source_kind.clone(),
        turn: candidate.turn,
        step: candidate.step,
        model: candidate.model.clone(),
        status,
        emitted_raw_seq,
        input_tokens: candidate.input_tokens,
        output_tokens: candidate.output_tokens,
        cache_read_tokens: candidate.cache_read_tokens,
        cache_write_tokens: candidate.cache_write_tokens,
        total_tokens: candidate.total_tokens,
    });
}

fn parse_usage_candidate(
    native_seq: u64,
    source_kind: &str,
    turn: Option<u64>,
    step: Option<u64>,
    model: Option<&str>,
    usage_val: &Value,
) -> RawUsageCandidate {
    let input_tokens = usage_val.get("inputTokens").and_then(Value::as_u64);
    let output_tokens = usage_val.get("outputTokens").and_then(Value::as_u64);
    let cache_read_tokens = usage_val.get("cacheReadTokens").and_then(Value::as_u64);
    let cache_write_tokens = usage_val.get("cacheWriteTokens").and_then(Value::as_u64);
    let total_tokens = usage_val.get("totalTokens").and_then(Value::as_u64);

    let exact_usage = match (
        input_tokens,
        output_tokens,
        cache_read_tokens,
        cache_write_tokens,
    ) {
        (Some(input), Some(output), Some(cache_read), Some(cache_write)) => {
            let sum = input
                .checked_add(output)
                .and_then(|v| v.checked_add(cache_read))
                .and_then(|v| v.checked_add(cache_write));
            let total_matches = match (sum, total_tokens) {
                (Some(s), Some(total)) => s == total,
                (Some(_), None) => true,
                (None, _) => false,
            };
            if total_matches {
                Some(TokenUsage {
                    input,
                    output,
                    cache_creation: cache_write,
                    cache_read,
                })
            } else {
                None
            }
        }
        _ => None,
    };

    RawUsageCandidate {
        native_seq,
        source_kind: source_kind.to_string(),
        turn,
        step,
        model: model
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        input_tokens,
        output_tokens,
        cache_read_tokens,
        cache_write_tokens,
        total_tokens,
        exact_usage,
    }
}

fn non_empty_str(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
}

fn extract_turn_step(value: &Value) -> (Option<u64>, Option<u64>) {
    let data = value.get("data");
    let turn = data
        .and_then(|d| d.get("turn"))
        .and_then(Value::as_u64)
        .or_else(|| value.get("turn").and_then(Value::as_u64));
    let step = data
        .and_then(|d| d.get("step"))
        .and_then(Value::as_u64)
        .or_else(|| value.get("step").and_then(Value::as_u64));
    (turn, step)
}

fn extract_dsh_time_ms(value: &Value) -> Option<i64> {
    value
        .get("time")
        .and_then(Value::as_i64)
        .or_else(|| value.get("ts").and_then(Value::as_i64))
}

fn extract_dsh_model(value: &Value) -> Option<String> {
    value
        .get("data")
        .and_then(|d| d.get("message"))
        .and_then(|m| m.get("source"))
        .and_then(|s| s.get("model"))
        .and_then(Value::as_str)
        .or_else(|| {
            value
                .get("data")
                .and_then(|d| d.get("model"))
                .and_then(Value::as_str)
        })
        .or_else(|| value.get("model").and_then(Value::as_str))
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
}

fn extract_dsh_usage_object(value: &Value) -> Option<&Value> {
    if let Some(u) = value
        .get("data")
        .and_then(|d| d.get("usage"))
        .filter(|v| v.is_object())
    {
        return Some(u);
    }
    if let Some(stream) = value
        .get("data")
        .and_then(|d| d.get("stream"))
        .and_then(Value::as_array)
    {
        if let Some(u) = stream.iter().rev().find_map(|item| {
            let chunk = item.get("chunk")?;
            (chunk.get("type").and_then(Value::as_str) == Some("usage"))
                .then(|| chunk.get("usage"))
                .flatten()
                .filter(|v| v.is_object())
        }) {
            return Some(u);
        }
    }
    value.get("usage").filter(|v| v.is_object())
}

fn join_content_blocks(items: &[Value], block_type: &str) -> Option<String> {
    let joined = items
        .iter()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some(block_type))
        .filter_map(|item| item.get("text").and_then(Value::as_str))
        .filter(|s| !s.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    (!joined.trim().is_empty()).then_some(joined)
}

fn extract_dsh_message_content(value: &Value, block_type: &str) -> Option<String> {
    if let Some(items) = value
        .get("data")
        .and_then(|d| {
            d.get("message")
                .and_then(|m| m.get("content"))
                .or_else(|| d.get("content"))
        })
        .and_then(Value::as_array)
    {
        if let Some(text) = join_content_blocks(items, block_type) {
            return Some(text);
        }
    }
    if block_type == "text" {
        if let Some(s) = value
            .get("data")
            .and_then(|d| d.get("content"))
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
        {
            return Some(s.to_string());
        }
        return non_empty_str(value.get("text"));
    }
    None
}

fn extract_dsh_assistant_reasoning(value: &Value) -> Option<String> {
    if let Some(text) = extract_dsh_message_content(value, "reasoning") {
        return Some(text);
    }
    non_empty_str(value.get("reasoningText"))
}

fn extract_dsh_user_text(value: &Value) -> Option<String> {
    if let Some(items) = value
        .get("data")
        .and_then(|d| d.get("content"))
        .and_then(Value::as_array)
    {
        if let Some(text) = join_content_blocks(items, "text") {
            return Some(text);
        }
    }
    if let Some(text) = extract_dsh_message_content(value, "text") {
        return Some(text);
    }
    let parts = value.get("parts")?.as_array()?;
    let joined = parts
        .iter()
        .filter_map(|part| {
            part.get("text")
                .and_then(Value::as_str)
                .or_else(|| part.get("path").and_then(Value::as_str))
        })
        .filter(|s| !s.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    (!joined.trim().is_empty()).then_some(joined)
}

fn extract_dsh_tool_name(value: &Value) -> &str {
    value
        .get("data")
        .and_then(|d| {
            d.get("call")
                .and_then(|c| c.get("name"))
                .or_else(|| d.get("name"))
        })
        .and_then(Value::as_str)
        .or_else(|| value.get("toolName").and_then(Value::as_str))
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("unknown")
}

fn extract_dsh_tool_result(value: &Value) -> Option<String> {
    if let Some(text) = extract_dsh_message_content(value, "text") {
        return Some(text);
    }
    if let Some(output) = value
        .get("data")
        .and_then(|d| non_empty_str(d.get("output")))
        .or_else(|| non_empty_str(value.get("output")))
    {
        return Some(output);
    }
    let err = value
        .get("data")
        .and_then(|d| d.get("error"))
        .or_else(|| value.get("error"))?;
    if let Some(msg) = non_empty_str(err.get("message")) {
        return Some(msg);
    }
    non_empty_str(Some(err))
}

#[cfg(feature = "store")]
mod store_wiring {
    use super::*;
    use crate::cursor::Cursor;
    use crate::observation::{
        AppendLogObservation, ParseQuality, ScanFailure, SourceChange, SourceFingerprint,
    };
    use crate::scan_plan::{
        CommitPlan, CursorAction, PriorProjection, PriorState, ScanReasons, StoreAction,
    };
    use crate::session_binding::{IngestAdmissionVerdict, SessionBindingRegistry};
    use crate::store::{
        FileProjectionBatch, ProjectionStats, SourceKey, StoreError, StoreResult, TotalStore,
    };
    use crate::token::ProjectionToken;

    /// 单步 DSH 来源观察与提交规划产物。
    #[derive(Debug, Clone)]
    pub struct DshPlannedStep {
        pub relation: Option<SnapshotRelation>,
        pub gate: CommitGateDecision,
        pub effective_reasons: ScanReasons,
        pub observation: AppendLogObservation,
        pub plan: CommitPlan,
        pub normalized: Option<NormalizedDshSnapshot>,
        pub candidate_state: Option<DshSavedState>,
        pub admission_status: Option<String>,
        pub admission_detail: Option<String>,
        pub events_excluded_temporary: u64,
    }

    /// 单步 DSH 来源观察 + `TotalStore::apply_projection` 提交结果。
    #[derive(Debug, Clone)]
    pub struct DshStoreCommitOutcome {
        pub planned: DshPlannedStep,
        pub projection_stats: Option<ProjectionStats>,
        pub next_state: Option<DshSavedState>,
        pub advanced_saved_state: bool,
    }

    fn semantic_stream_bytes(snapshot: &SourceSnapshot) -> Vec<u8> {
        format!(
            "dsh4:{}:{}:{}",
            snapshot.session_id(),
            snapshot.lifecycle_sha256(),
            snapshot.native_events_sha256()
        )
        .into_bytes()
    }

    fn cursor_from_saved(prior: Option<&DshSavedState>) -> Cursor {
        let mut cursor = Cursor::new_byte_offset();
        if let Some(saved) = prior {
            cursor.safe_offset = saved.native_events() as u64;
            cursor.size = saved.native_events() as u64;
            cursor.content_hash = Some(format!("sha256:{}", saved.native_events_sha256));
            cursor.next_seq = saved.normalized_next_seq;
        }
        cursor
    }

    fn cursor_from_verified(snapshot: &SourceSnapshot, next_seq: u64) -> Cursor {
        let mut cursor = Cursor::new_byte_offset();
        cursor.safe_offset = snapshot.native_events() as u64;
        cursor.size = snapshot.native_events() as u64;
        cursor.content_hash = Some(format!("sha256:{}", snapshot.native_events_sha256()));
        cursor.next_seq = next_seq;
        cursor
    }

    /// 对候选归一化产物与待提交事件列表执行控制面绑定准入与 `InputOrigin` 丰富。
    #[allow(clippy::too_many_arguments)]
    fn finalize_verified_step(
        relation: Option<SnapshotRelation>,
        gate: CommitGateDecision,
        effective_reasons: ScanReasons,
        prior_state: Option<&DshSavedState>,
        prior_projection: PriorProjection,
        snapshot: &SourceSnapshot,
        mut normalized: NormalizedDshSnapshot,
        candidate_events: Vec<RawEvent>,
        source_change: SourceChange,
        include_fingerprint: bool,
        cfg: &DshNormalizeConfig,
        bindings: &SessionBindingRegistry,
    ) -> DshPlannedStep {
        let mut session_parents = std::collections::HashMap::new();
        if let Some(ref p) = normalized.parent_session_id {
            session_parents.insert(normalized.session_id.clone(), p.clone());
        }
        // 同时将 `normalized.events` 与 `candidate_events` 经过绑定准入与 InputOrigin 升级，
        // 使报告与落库事件完全一致。
        if let Ok(full_filtered) = bindings.filter_and_enrich_events(
            SourceType::Jsonl,
            &cfg.source_location,
            &cfg.source_path,
            normalized.events.clone(),
            &session_parents,
        ) {
            if !full_filtered.kept_events.is_empty() {
                normalized.events = full_filtered.kept_events;
            }
        }

        let pre_verdict = bindings.evaluate(
            SourceType::Jsonl,
            &cfg.source_location,
            &cfg.source_path,
            Some(&normalized.session_id),
            normalized.parent_session_id.as_deref(),
        );
        if let IngestAdmissionVerdict::UnknownHold { reason } = pre_verdict {
            let detail = reason.to_string();
            let observation = AppendLogObservation {
                source_change,
                quality: ParseQuality::Unavailable(ScanFailure::Read(format!(
                    "session binding uncertain (hold): {detail}"
                ))),
                events: Vec::new(),
                cursor: cursor_from_saved(prior_state),
                source_fingerprint: None,
            };
            let plan = CommitPlan::plan(&observation, effective_reasons, prior_projection);
            return DshPlannedStep {
                relation,
                gate,
                effective_reasons,
                observation,
                plan,
                normalized: Some(normalized),
                candidate_state: None,
                admission_status: Some("unknown_hold".to_string()),
                admission_detail: Some(detail),
                events_excluded_temporary: 0,
            };
        }

        match bindings.filter_and_enrich_events(
            SourceType::Jsonl,
            &cfg.source_location,
            &cfg.source_path,
            candidate_events,
            &session_parents,
        ) {
            Err(uncertainty) => {
                let detail = uncertainty.to_string();
                let observation = AppendLogObservation {
                    source_change,
                    quality: ParseQuality::Unavailable(ScanFailure::Read(format!(
                        "session binding uncertain (hold): {detail}"
                    ))),
                    events: Vec::new(),
                    cursor: cursor_from_saved(prior_state),
                    source_fingerprint: None,
                };
                let plan = CommitPlan::plan(&observation, effective_reasons, prior_projection);
                DshPlannedStep {
                    relation,
                    gate,
                    effective_reasons,
                    observation,
                    plan,
                    normalized: Some(normalized),
                    candidate_state: None,
                    admission_status: Some("unknown_hold".to_string()),
                    admission_detail: Some(detail),
                    events_excluded_temporary: 0,
                }
            }
            Ok(filtered) => {
                if filtered.admission_status == "temporary_excluded"
                    || matches!(
                        pre_verdict,
                        IngestAdmissionVerdict::TemporaryExcluded { .. }
                    )
                {
                    let detail = filtered.admission_detail.or_else(|| match pre_verdict {
                        IngestAdmissionVerdict::TemporaryExcluded { basis } => {
                            Some(basis.to_string())
                        }
                        _ => None,
                    });
                    let observation = AppendLogObservation {
                        source_change: SourceChange::Appended,
                        quality: ParseQuality::Clean {
                            deferred_tail_bytes: 0,
                        },
                        events: Vec::new(),
                        cursor: cursor_from_saved(prior_state),
                        source_fingerprint: None,
                    };
                    // 临时会话不落库、不推进 DshSavedState，确保后续追加或绑定变更可完整重检。
                    let plan = CommitPlan::plan(
                        &observation,
                        ScanReasons::NONE,
                        PriorProjection::no_store(),
                    );
                    return DshPlannedStep {
                        relation,
                        gate,
                        effective_reasons,
                        observation,
                        plan,
                        normalized: Some(normalized),
                        candidate_state: None,
                        admission_status: Some("temporary_excluded".to_string()),
                        admission_detail: detail,
                        events_excluded_temporary: filtered.excluded_events,
                    };
                }

                let next_state = DshSavedState::from_verified(
                    snapshot,
                    normalized.next_seq(),
                    PARSER_REVISION,
                    cfg.attribution_revision,
                );
                let observation = AppendLogObservation {
                    source_change,
                    quality: ParseQuality::Clean {
                        deferred_tail_bytes: 0,
                    },
                    events: filtered.kept_events,
                    cursor: cursor_from_verified(snapshot, normalized.next_seq()),
                    source_fingerprint: include_fingerprint
                        .then(|| SourceFingerprint::of(&semantic_stream_bytes(snapshot))),
                };
                let plan = CommitPlan::plan(&observation, effective_reasons, prior_projection);
                DshPlannedStep {
                    relation,
                    gate,
                    effective_reasons,
                    observation,
                    plan,
                    normalized: Some(normalized),
                    candidate_state: Some(next_state),
                    admission_status: Some(filtered.admission_status.to_string()),
                    admission_detail: filtered.admission_detail,
                    events_excluded_temporary: filtered.excluded_events,
                }
            }
        }
    }

    /// 将持久化状态（若有）与当前来源捕获结果转换为 `CommitGateDecision`、`AppendLogObservation` 与 `CommitPlan`（无额外控制面绑定）。
    pub fn plan_dsh_source_step(
        prior_state: Option<&DshSavedState>,
        current: Result<&SourceSnapshot, &Blocked>,
        reasons: ScanReasons,
        prior_projection: PriorProjection,
        cfg: &DshNormalizeConfig,
    ) -> Result<DshPlannedStep, Blocked> {
        plan_dsh_source_step_with_bindings(
            prior_state,
            current,
            reasons,
            prior_projection,
            cfg,
            &SessionBindingRegistry::empty(),
        )
    }

    /// 携带控制面会话绑定（`SessionBindingRegistry`）规划单步 DSH 来源观察与提交。
    pub fn plan_dsh_source_step_with_bindings(
        prior_state: Option<&DshSavedState>,
        current: Result<&SourceSnapshot, &Blocked>,
        reasons: ScanReasons,
        prior_projection: PriorProjection,
        cfg: &DshNormalizeConfig,
        bindings: &SessionBindingRegistry,
    ) -> Result<DshPlannedStep, Blocked> {
        let (relation, gate) =
            super::super::source::evaluate_saved_commit_gate(prior_state, current);
        let mut effective_reasons = reasons;
        if let Some(saved) = prior_state {
            if saved.parser_revision != PARSER_REVISION {
                effective_reasons |= ScanReasons::PARSER_STALE;
            }
            if saved.attribution_revision != cfg.attribution_revision {
                effective_reasons |= ScanReasons::ATTRIBUTION_STALE;
            }
        } else {
            effective_reasons |= ScanReasons::INITIAL;
        }

        match &gate {
            CommitGateDecision::RejectInitial { blocked } => {
                let observation = AppendLogObservation {
                    source_change: SourceChange::Appended,
                    quality: ParseQuality::Unavailable(ScanFailure::Read(format!(
                        "dsh_reject_initial:{blocked:?}"
                    ))),
                    events: Vec::new(),
                    cursor: cursor_from_saved(prior_state),
                    source_fingerprint: None,
                };
                let plan = CommitPlan::plan(&observation, effective_reasons, prior_projection);
                Ok(DshPlannedStep {
                    relation,
                    gate,
                    effective_reasons,
                    observation,
                    plan,
                    normalized: None,
                    candidate_state: None,
                    admission_status: None,
                    admission_detail: None,
                    events_excluded_temporary: 0,
                })
            }
            CommitGateDecision::HoldPriorProjection { reason, .. } => {
                let observation = AppendLogObservation {
                    source_change: SourceChange::Appended,
                    quality: ParseQuality::Unavailable(ScanFailure::Read(format!(
                        "dsh_hold_prior:{reason:?}"
                    ))),
                    events: Vec::new(),
                    cursor: cursor_from_saved(prior_state),
                    source_fingerprint: None,
                };
                let plan = CommitPlan::plan(&observation, effective_reasons, prior_projection);
                Ok(DshPlannedStep {
                    relation,
                    gate,
                    effective_reasons,
                    observation,
                    plan,
                    normalized: None,
                    candidate_state: None,
                    admission_status: None,
                    admission_detail: None,
                    events_excluded_temporary: 0,
                })
            }
            CommitGateDecision::RejectDifferentSource => {
                let observation = AppendLogObservation {
                    source_change: SourceChange::Appended,
                    quality: ParseQuality::Unavailable(ScanFailure::Read(
                        "dsh_reject_different_source".to_string(),
                    )),
                    events: Vec::new(),
                    cursor: cursor_from_saved(prior_state),
                    source_fingerprint: None,
                };
                let plan = CommitPlan::plan(&observation, effective_reasons, prior_projection);
                Ok(DshPlannedStep {
                    relation,
                    gate,
                    effective_reasons,
                    observation,
                    plan,
                    normalized: None,
                    candidate_state: None,
                    admission_status: None,
                    admission_detail: None,
                    events_excluded_temporary: 0,
                })
            }
            CommitGateDecision::InitialProjectionCandidate { .. } => {
                let snapshot = current.expect("InitialProjectionCandidate requires Ok snapshot");
                let normalized = normalize_source_snapshot(snapshot, cfg)?;
                let events = normalized.events.clone();
                Ok(finalize_verified_step(
                    relation,
                    gate,
                    effective_reasons,
                    prior_state,
                    prior_projection,
                    snapshot,
                    normalized,
                    events,
                    SourceChange::Appended,
                    true,
                    cfg,
                    bindings,
                ))
            }
            CommitGateDecision::NoOpUnchanged { .. } => {
                let snapshot = current.expect("NoOpUnchanged requires Ok snapshot");
                let normalized = normalize_source_snapshot(snapshot, cfg)?;
                let saved = prior_state.expect("NoOpUnchanged requires prior_state");
                if saved.normalized_next_seq != normalized.next_seq() {
                    effective_reasons |= ScanReasons::PARSER_STALE;
                }
                let need_full_events = effective_reasons.wants_reparse()
                    || effective_reasons.wants_full_read()
                    || prior_projection.state() == PriorState::Absent;
                let events = if need_full_events {
                    normalized.events.clone()
                } else {
                    Vec::new()
                };
                Ok(finalize_verified_step(
                    relation,
                    gate,
                    effective_reasons,
                    prior_state,
                    prior_projection,
                    snapshot,
                    normalized,
                    events,
                    SourceChange::Appended,
                    true,
                    cfg,
                    bindings,
                ))
            }
            CommitGateDecision::IncrementalAppendCandidate { prior_events, .. } => {
                let snapshot = current.expect("IncrementalAppendCandidate requires Ok snapshot");
                let normalized = normalize_source_snapshot(snapshot, cfg)?;
                let saved = prior_state.expect("IncrementalAppendCandidate requires prior_state");
                let prefix_emitted = normalized
                    .spans
                    .iter()
                    .filter(|span| (span.native_seq as usize) < *prior_events)
                    .count() as u64;
                if prefix_emitted != saved.normalized_next_seq {
                    // 前缀归一化产出的事件数发生变化（例如同 step 的 usage 被新追加的事件覆盖，或解析规则升级）
                    // 必须重出整代，防止增量 seq 断号或旧 attempt usage 残留在当前头代。
                    effective_reasons |= ScanReasons::PARSER_STALE;
                }
                let need_full_events = effective_reasons.wants_reparse()
                    || effective_reasons.wants_full_read()
                    || prior_projection.state() == PriorState::Absent;
                let events = if need_full_events {
                    normalized.events.clone()
                } else {
                    normalized.events_from_native_seq(*prior_events as u64).0
                };
                Ok(finalize_verified_step(
                    relation,
                    gate,
                    effective_reasons,
                    prior_state,
                    prior_projection,
                    snapshot,
                    normalized,
                    events,
                    SourceChange::Appended,
                    need_full_events,
                    cfg,
                    bindings,
                ))
            }
            CommitGateDecision::RebuildProjectionCandidate { .. } => {
                let snapshot = current.expect("RebuildProjectionCandidate requires Ok snapshot");
                let normalized = normalize_source_snapshot(snapshot, cfg)?;
                let events = normalized.events.clone();
                Ok(finalize_verified_step(
                    relation,
                    gate,
                    effective_reasons,
                    prior_state,
                    prior_projection,
                    snapshot,
                    normalized,
                    events,
                    SourceChange::RollbackOrRewrite,
                    true,
                    cfg,
                    bindings,
                ))
            }
        }
    }

    /// 执行单步 DSH 观察、规划并写入 `TotalStore`（自动从 `store.load_session_bindings()` 加载控制面绑定）；
    /// 仅在写库成功且 `CursorAction::Advance` 时推进 `DshSavedState`。
    pub fn commit_dsh_source_step(
        store: &TotalStore,
        prior_state: Option<&DshSavedState>,
        current: Result<&SourceSnapshot, &Blocked>,
        reasons: ScanReasons,
        cfg: &DshNormalizeConfig,
    ) -> StoreResult<DshStoreCommitOutcome> {
        let bindings = store.load_session_bindings();
        commit_dsh_source_step_with_bindings(store, prior_state, current, reasons, cfg, &bindings)
    }

    /// 显式指定 `SessionBindingRegistry` 执行单步 DSH 观察、规划并写入 `TotalStore`。
    pub fn commit_dsh_source_step_with_bindings(
        store: &TotalStore,
        prior_state: Option<&DshSavedState>,
        current: Result<&SourceSnapshot, &Blocked>,
        reasons: ScanReasons,
        cfg: &DshNormalizeConfig,
        bindings: &SessionBindingRegistry,
    ) -> StoreResult<DshStoreCommitOutcome> {
        let source_key = SourceKey {
            source_type: SourceType::Jsonl,
            source_location: cfg.source_location.clone(),
            source_path: cfg.source_path.clone(),
        };
        let prior_projection = PriorProjection::ask(store, &source_key)?;
        let planned = plan_dsh_source_step_with_bindings(
            prior_state,
            current,
            reasons,
            prior_projection,
            cfg,
            bindings,
        )
        .map_err(|blocked| {
            StoreError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("dsh normalization blocked: {blocked:?}"),
            ))
        })?;

        let projection_stats = match planned.plan.store() {
            StoreAction::Preserve => None,
            StoreAction::Project(mode) => {
                let snapshot = current.expect("StoreAction::Project requires verified snapshot");
                let fp = format!(
                    "dsh4:{}:{}",
                    snapshot.lifecycle_sha256(),
                    snapshot.native_events_sha256()
                );
                let token = ProjectionToken::new(
                    &source_key,
                    Some(&fp),
                    PARSER_REVISION,
                    cfg.attribution_revision,
                    (0, snapshot.native_events() as u64),
                );
                let stats = store.apply_projection(FileProjectionBatch {
                    source: source_key,
                    parser_revision: Some(PARSER_REVISION),
                    mode,
                    events: planned.observation.events.clone(),
                    token: Some(token),
                })?;
                Some(stats)
            }
        };

        let advanced_saved_state = planned.plan.cursor() == CursorAction::Advance
            && planned.admission_status.as_deref() != Some("temporary_excluded")
            && planned.admission_status.as_deref() != Some("unknown_hold");
        let next_state = if advanced_saved_state {
            planned.candidate_state.clone()
        } else {
            prior_state.cloned()
        };

        Ok(DshStoreCommitOutcome {
            planned,
            projection_stats,
            next_state,
            advanced_saved_state,
        })
    }

    /// DSH 会话根目录批量扫描中单个会话子目录的扫描与写库结果。
    #[derive(Debug, Clone)]
    pub struct DshRootScanSessionOutcome {
        pub dir_name: String,
        pub session_dir: std::path::PathBuf,
        pub discovery_status: crate::dsh_snapshot::source::DshDiscoveredSessionStatus,
        pub commit_outcome: DshStoreCommitOutcome,
    }

    /// DSH 会话根目录批量扫描与总库提交汇总报告。
    #[derive(Debug, Clone)]
    pub struct DshRootScanOutcome {
        pub sessions_root: std::path::PathBuf,
        pub total_session_dirs: usize,
        pub ready_v4_count: usize,
        pub blocked_count: usize,
        pub projected_sessions: usize,
        pub preserved_sessions: usize,
        pub temporary_excluded_sessions: usize,
        pub total_events_appended: u64,
        pub entries: Vec<DshRootScanSessionOutcome>,
    }

    /// 对显式给出的 DSH 会话根目录（如 `<dsh_home>/sessions`）执行批量发现、v4 捕获、归一化、控制面准入与总库提交。
    ///
    /// 首版正式规则：
    /// - 仅最高规范代为 `v4` 的子目录进入 `capture_source` + `commit_dsh_source_step_with_bindings`；
    /// - 仅存旧格式代 `v0..v3` 或出现未来格式代 `v5+` 的子目录显式按 `Err(Blocked::Unsupported("physical_generation"))`
    ///   进入保守提交门（无前代时 `RejectInitial`，已有 v4 前代时 `HoldPriorProjection` 保守冻结），绝不回退旧代。
    #[allow(clippy::too_many_arguments)]
    pub fn scan_dsh_sessions_root_to_store(
        store: &TotalStore,
        sessions_root: &std::path::Path,
        source_instance_prefix: &str,
        runtime: &crate::dsh_snapshot::NativeRuntime,
        saved_states: &mut std::collections::BTreeMap<String, DshSavedState>,
        reasons: ScanReasons,
        source_location: crate::rawevent::SourceLocation,
        attribution_revision: i64,
        deadline: crate::deadline::Deadline,
    ) -> StoreResult<DshRootScanOutcome> {
        let discovery =
            crate::dsh_snapshot::source::discover_dsh_sessions_in_root(sessions_root, None)
                .map_err(|blocked| {
                    StoreError::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        format!("dsh sessions root discovery blocked: {blocked:?}"),
                    ))
                })?;
        let bindings = store.load_session_bindings();

        let mut projected_sessions = 0usize;
        let mut preserved_sessions = 0usize;
        let mut temporary_excluded_sessions = 0usize;
        let mut total_events_appended = 0u64;
        let mut entries_out = Vec::with_capacity(discovery.entries.len());

        for entry in discovery.entries {
            let mut cfg = DshNormalizeConfig::local_full(entry.session_dir.display().to_string());
            cfg.source_location = source_location.clone();
            cfg.attribution_revision = attribution_revision;
            let prior_state = saved_states.get(&entry.dir_name);
            let commit_outcome = match &entry.outcome {
                crate::dsh_snapshot::source::DshDiscoveredSessionStatus::ReadyV4 {
                    compression,
                    ..
                } => {
                    let src_cfg = match crate::dsh_snapshot::source::SourceConfig::new(
                        format!("{source_instance_prefix}:{}", entry.dir_name),
                        entry.session_dir.clone(),
                        None,
                        *compression,
                    ) {
                        Ok(c) => c,
                        Err(blocked) => {
                            let co = commit_dsh_source_step_with_bindings(
                                store,
                                prior_state,
                                Err(&blocked),
                                reasons,
                                &cfg,
                                &bindings,
                            )?;
                            preserved_sessions += 1;
                            entries_out.push(DshRootScanSessionOutcome {
                                dir_name: entry.dir_name,
                                session_dir: entry.session_dir,
                                discovery_status: entry.outcome,
                                commit_outcome: co,
                            });
                            continue;
                        }
                    };
                    let captured =
                        crate::dsh_snapshot::source::capture_source(&src_cfg, runtime, deadline);
                    commit_dsh_source_step_with_bindings(
                        store,
                        prior_state,
                        captured.as_ref(),
                        reasons,
                        &cfg,
                        &bindings,
                    )?
                }
                crate::dsh_snapshot::source::DshDiscoveredSessionStatus::Blocked { blocked } => {
                    commit_dsh_source_step_with_bindings(
                        store,
                        prior_state,
                        Err(blocked),
                        reasons,
                        &cfg,
                        &bindings,
                    )?
                }
            };

            if commit_outcome.advanced_saved_state {
                if let Some(ref next_state) = commit_outcome.next_state {
                    saved_states.insert(entry.dir_name.clone(), next_state.clone());
                }
            }

            if commit_outcome.planned.admission_status.as_deref() == Some("temporary_excluded") {
                temporary_excluded_sessions += 1;
            }
            match commit_outcome.planned.plan.store() {
                StoreAction::Preserve => preserved_sessions += 1,
                StoreAction::Project(_) => projected_sessions += 1,
            }
            if let Some(ref st) = commit_outcome.projection_stats {
                total_events_appended += st.appended;
            }

            entries_out.push(DshRootScanSessionOutcome {
                dir_name: entry.dir_name,
                session_dir: entry.session_dir,
                discovery_status: entry.outcome,
                commit_outcome,
            });
        }

        Ok(DshRootScanOutcome {
            sessions_root: discovery.sessions_root,
            total_session_dirs: entries_out.len(),
            ready_v4_count: discovery.ready_v4_count,
            blocked_count: discovery.blocked_count,
            projected_sessions,
            preserved_sessions,
            temporary_excluded_sessions,
            total_events_appended,
            entries: entries_out,
        })
    }
}

#[cfg(feature = "store")]
pub use store_wiring::{
    commit_dsh_source_step, commit_dsh_source_step_with_bindings, plan_dsh_source_step,
    plan_dsh_source_step_with_bindings, scan_dsh_sessions_root_to_store, DshPlannedStep,
    DshRootScanOutcome, DshRootScanSessionOutcome, DshStoreCommitOutcome,
};

#[cfg(all(test, any(windows, unix)))]
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use crate::dsh_snapshot::source::tests::synthetic_source_snapshot;
    use crate::dsh_snapshot::Compression;
    use crate::rawevent::occurred_at_unix_ms;

    #[test]
    fn epoch_ms_to_iso8601_utc_roundtrips_with_occurred_at_unix_ms() {
        for ms in [
            0_i64,
            -1_000,
            1_700_000_000_000,
            1_775_900_123_456,
            946_684_800_000,
            951_782_400_999,
        ] {
            let iso = epoch_ms_to_iso8601_utc(ms);
            assert_eq!(
                occurred_at_unix_ms(&iso),
                Some(ms),
                "roundtrip failed for {ms} -> {iso}"
            );
        }
    }

    #[test]
    fn normalize_suppresses_inherited_prefix_and_keeps_usage_honest() {
        let dir = std::env::temp_dir().join(format!(
            "sv-dsh-norm-child-{}-{}",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let records = [
            r#"{"type":"session","version":4,"id":"child1","cwd":"/workspace/demo","parentSessionId":"parent0","forkedFromSeq":1}"#,
            r#"{"type":"user/message","seq":0,"turn":1,"ts":1700000001000,"text":"parent question"}"#,
            r#"{"type":"assistant/message","seq":1,"turn":1,"step":0,"ts":1700000002000,"model":"deepseek-chat","text":"parent answer","usage":{"inputTokens":10,"outputTokens":5,"cacheReadTokens":2,"cacheWriteTokens":1,"totalTokens":18}}"#,
            r#"{"type":"developer/message","seq":2,"turn":2,"ts":1700000003000,"text":"developer guidance"}"#,
            r#"{"type":"user/message","seq":3,"turn":2,"ts":1700000004000,"text":"child question"}"#,
            // 同 (turn=2, step=0) 先有一条无配对 retry-started 的 attempt usage，后有成功 assistant/message usage：前者应被覆盖
            r#"{"type":"assistant/attempt","seq":4,"turn":2,"step":0,"ts":1700000005000,"model":"attempt-only-model","usage":{"inputTokens":20,"outputTokens":4,"cacheReadTokens":0,"cacheWriteTokens":0,"totalTokens":24}}"#,
            r#"{"type":"assistant/message","seq":5,"turn":2,"step":0,"ts":1700000006000,"model":"deepseek-reasoner","reasoningText":"step0 thought","text":"step0 answer","usage":{"inputTokens":30,"outputTokens":10,"cacheReadTokens":5,"cacheWriteTokens":2,"totalTokens":47}}"#,
            // (turn=2, step=1) 有配对 llm/retry + llm/retry-started，且 attempt 自身无 model，后续成功 message 缺失 cache 桶
            r#"{"type":"assistant/attempt","seq":6,"turn":2,"step":1,"ts":1700000007000,"usage":{"inputTokens":12,"outputTokens":3,"cacheReadTokens":1,"cacheWriteTokens":0,"totalTokens":16}}"#,
            r#"{"type":"llm/retry","seq":7,"turn":2,"step":1,"ts":1700000007100,"attempt":1}"#,
            r#"{"type":"llm/retry-started","seq":8,"turn":2,"step":1,"ts":1700000007200,"attempt":1}"#,
            r#"{"type":"assistant/message","seq":9,"turn":2,"step":1,"ts":1700000008000,"model":"deepseek-chat","text":"step1 answer after retry","usage":{"inputTokens":50,"outputTokens":20,"totalTokens":70}}"#,
            r#"{"type":"tool/call","seq":10,"turn":2,"step":1,"ts":1700000009000,"toolCallId":"tc1","toolName":"bash"}"#,
            r#"{"type":"tool/result","seq":11,"turn":2,"step":1,"ts":1700000010000,"toolCallId":"tc1","toolName":"bash","output":"ok\n"}"#,
        ];
        let snap = synthetic_source_snapshot(
            &dir,
            "child1",
            Compression::None,
            &records,
            2,
            "lc_child1",
            &[
                "e0", "e1", "e2", "e3", "e4", "e5", "e6", "e7", "e8", "e9", "e10", "e11",
            ],
        );
        let cfg = DshNormalizeConfig::local_full(dir.display().to_string());
        let norm = normalize_source_snapshot(&snap, &cfg).unwrap();

        assert_eq!(norm.inherited_event_count, 2);
        assert_eq!(norm.suppressed_inherited_records, 2);
        // 继承前缀 seq 0, 1 不得发射
        assert!(norm.spans.iter().all(|s| s.native_seq >= 2));

        // 核对发射序列：
        // seq 2 (developer/message) -> Message(actor=None)
        // seq 3 (user/message) -> Message(actor=Some(User))
        // seq 4 (assistant/attempt, step 0) -> 被 seq 5 覆盖，不发射
        // seq 5 (assistant/message, step 0) -> Thinking + Message + Usage(ExactFourBucket)
        // seq 6 (assistant/attempt, step 1, 确认重试) -> Usage(ExactFourBucket, model=None，绝不借用 seq 9 的 model)
        // seq 9 (assistant/message, step 1, 缺 cache 桶) -> Message（Usage 为 PartialOrUnreconciled，绝不发射填 0 的 Usage）
        // seq 10 (tool/call) -> ToolUse
        // seq 11 (tool/result) -> ToolResult
        let summary: Vec<(u64, EventType, Option<Actor>, Option<&str>)> = norm
            .events
            .iter()
            .zip(norm.spans.iter())
            .map(|(ev, sp)| (sp.native_seq, ev.event_type, ev.actor, ev.model.as_deref()))
            .collect();
        assert_eq!(
            summary,
            vec![
                (2, EventType::Message, None, None),
                (3, EventType::Message, Some(Actor::User), None),
                (
                    5,
                    EventType::Thinking,
                    Some(Actor::Assistant),
                    Some("deepseek-reasoner")
                ),
                (
                    5,
                    EventType::Message,
                    Some(Actor::Assistant),
                    Some("deepseek-reasoner")
                ),
                (
                    5,
                    EventType::Usage,
                    Some(Actor::Assistant),
                    Some("deepseek-reasoner")
                ),
                (6, EventType::Usage, Some(Actor::Assistant), None),
                (
                    9,
                    EventType::Message,
                    Some(Actor::Assistant),
                    Some("deepseek-chat")
                ),
                (10, EventType::ToolUse, Some(Actor::Assistant), None),
                (11, EventType::ToolResult, Some(Actor::Tool), None),
            ]
        );

        // 核对 usage 台账四条记录的状态
        let ledger_statuses: Vec<(u64, DshUsageStatus, Option<&str>)> = norm
            .usage_ledger
            .iter()
            .map(|u| (u.native_seq, u.status, u.model.as_deref()))
            .collect();
        assert_eq!(
            ledger_statuses,
            vec![
                (
                    4,
                    DshUsageStatus::SupersededBySameStep,
                    Some("attempt-only-model")
                ),
                (
                    5,
                    DshUsageStatus::ExactFourBucket,
                    Some("deepseek-reasoner")
                ),
                (6, DshUsageStatus::ExactFourBucket, None),
                (
                    9,
                    DshUsageStatus::PartialOrUnreconciled,
                    Some("deepseek-chat")
                ),
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(feature = "store")]
    #[test]
    fn store_commit_handles_append_rewrite_lifecycle_shrink_and_replay_idempotence() {
        use crate::dsh_snapshot::source::HoldReason;
        use crate::scan_plan::{IndexAction, ScanReasons, StoreAction};
        use crate::store::{Projection, TotalStore};

        let dir = std::env::temp_dir().join(format!(
            "sv-dsh-norm-store-{}-{}",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = TotalStore::open_in_memory().unwrap();
        let cfg = DshNormalizeConfig::local_full(dir.join("session-s1").display().to_string());

        let r0 = [
            r#"{"type":"session","version":4,"id":"s1","cwd":"/workspace/demo"}"#,
            r#"{"type":"user/message","seq":0,"turn":1,"ts":1700000001000,"text":"hello"}"#,
            r#"{"type":"assistant/message","seq":1,"turn":1,"step":0,"ts":1700000002000,"model":"deepseek-chat","text":"world","usage":{"inputTokens":10,"outputTokens":5,"cacheReadTokens":0,"cacheWriteTokens":0,"totalTokens":15}}"#,
        ];
        let snap1 =
            synthetic_source_snapshot(&dir, "s1", Compression::None, &r0, 0, "lc1", &["h0", "h1"]);

        // 1. 首次提交：InitialProjectionCandidate -> Append，状态推进
        let out1 =
            commit_dsh_source_step(&store, None, Ok(&snap1), ScanReasons::NONE, &cfg).unwrap();
        assert!(out1.advanced_saved_state);
        assert_eq!(
            out1.planned.plan.store(),
            StoreAction::Project(Projection::Append)
        );
        assert_eq!(out1.projection_stats.unwrap().appended, 3);
        let state1 = out1.next_state.unwrap();
        assert_eq!(state1.native_events(), 2);
        assert_eq!(state1.normalized_next_seq, 3);

        // 2. 无变化重扫：NoOpUnchanged -> Append 0 条，幂等不增代
        let out_noop =
            commit_dsh_source_step(&store, Some(&state1), Ok(&snap1), ScanReasons::NONE, &cfg)
                .unwrap();
        assert!(out_noop.advanced_saved_state);
        assert_eq!(out_noop.planned.observation.events.len(), 0);
        assert_eq!(out_noop.projection_stats.unwrap().appended, 0);

        // 3. 增量追加两条新事件：IncrementalAppendCandidate -> 只发新增事件，seq 从 3 连续递增
        let r_append = [
            r0[0],
            r0[1],
            r0[2],
            r#"{"type":"user/message","seq":2,"turn":2,"ts":1700000003000,"text":"follow up"}"#,
            r#"{"type":"assistant/message","seq":3,"turn":2,"step":0,"ts":1700000004000,"model":"deepseek-chat","text":"reply 2"}"#,
        ];
        let snap2 = synthetic_source_snapshot(
            &dir,
            "s1",
            Compression::Zstd,
            &r_append,
            0,
            "lc1",
            &["h0", "h1", "h2", "h3"],
        );
        let out2 =
            commit_dsh_source_step(&store, Some(&state1), Ok(&snap2), ScanReasons::NONE, &cfg)
                .unwrap();
        assert!(out2.advanced_saved_state);
        assert_eq!(out2.planned.plan.index(), IndexAction::AppendFacts);
        assert_eq!(out2.planned.observation.events.len(), 2);
        assert_eq!(out2.planned.observation.events[0].seq, 3);
        assert_eq!(out2.planned.observation.events[1].seq, 4);
        assert_eq!(out2.projection_stats.unwrap().appended, 2);
        let state2 = out2.next_state.unwrap();
        assert_eq!(state2.normalized_next_seq, 5);

        // 4. 修尾中间态变短（PrefixShrank）：HoldPriorProjection -> StoreAction::Preserve + CursorAction::Freeze，状态保持 state2
        let out_shrank =
            commit_dsh_source_step(&store, Some(&state2), Ok(&snap1), ScanReasons::FORCE, &cfg)
                .unwrap();
        assert!(!out_shrank.advanced_saved_state);
        assert_eq!(out_shrank.planned.plan.store(), StoreAction::Preserve);
        assert_eq!(
            out_shrank.planned.gate,
            CommitGateDecision::HoldPriorProjection {
                prior_events: 4,
                reason: HoldReason::PrefixShrank {
                    current_events: 2,
                    shared_prefix_intact: true,
                },
            }
        );
        assert_eq!(out_shrank.next_state.as_ref(), Some(&state2));

        // 5. 捕获阻断（如未来代 v5 / 帧损坏）：HoldPriorProjection -> StoreAction::Preserve + CursorAction::Freeze
        let blocked = Blocked::Unsupported("unsupported_physical_generation");
        let out_blocked = commit_dsh_source_step(
            &store,
            Some(&state2),
            Err(&blocked),
            ScanReasons::FORCE,
            &cfg,
        )
        .unwrap();
        assert!(!out_blocked.advanced_saved_state);
        assert_eq!(out_blocked.planned.plan.store(), StoreAction::Preserve);
        assert_eq!(out_blocked.next_state.as_ref(), Some(&state2));

        // 6. 同事件数改写第 1 条事件（PrefixRewritten）：RebuildProjectionCandidate -> StoreAction::Project(Projection::Rollback)
        let r_rewritten = [
            r0[0],
            r0[1],
            r#"{"type":"assistant/message","seq":1,"turn":1,"step":0,"ts":1700000002000,"model":"deepseek-chat","text":"rewritten world"}"#,
            r_append[3],
            r_append[4],
        ];
        let snap_rewrite = synthetic_source_snapshot(
            &dir,
            "s1",
            Compression::Zstd,
            &r_rewritten,
            0,
            "lc1",
            &["h0", "h1_rewritten", "h2", "h3"],
        );
        let out_rewrite1 = commit_dsh_source_step(
            &store,
            Some(&state2),
            Ok(&snap_rewrite),
            ScanReasons::NONE,
            &cfg,
        )
        .unwrap();
        assert!(out_rewrite1.advanced_saved_state);
        assert_eq!(
            out_rewrite1.planned.plan.store(),
            StoreAction::Project(Projection::Rollback)
        );
        let stats_rewrite1 = out_rewrite1.projection_stats.unwrap();
        assert_eq!(stats_rewrite1.source_revision, 1);
        assert!(stats_rewrite1.head_moved);

        // 7. 模拟写库成功但未保存 state2 -> state3 时崩溃重放：用旧 state2 重放同一份 snap_rewrite，ProjectionToken 保证幂等不开新代
        let out_rewrite_replay = commit_dsh_source_step(
            &store,
            Some(&state2),
            Ok(&snap_rewrite),
            ScanReasons::NONE,
            &cfg,
        )
        .unwrap();
        assert!(out_rewrite_replay.advanced_saved_state);
        let stats_rewrite_replay = out_rewrite_replay.projection_stats.unwrap();
        assert_eq!(
            stats_rewrite_replay.source_revision,
            stats_rewrite1.source_revision,
            "crash replay before saving DshSavedState must be idempotent and not open a new generation"
        );
        assert!(!stats_rewrite_replay.head_moved);
        let state3 = out_rewrite_replay.next_state.unwrap();

        // 8. 生命周期变化（LifecycleChanged）：RebuildProjectionCandidate -> Projection::Rollback 开新代
        let snap_lifecycle = synthetic_source_snapshot(
            &dir,
            "s1",
            Compression::Zstd,
            &r_rewritten,
            0,
            "lc2_compacted",
            &["h0", "h1_rewritten", "h2", "h3"],
        );
        let out_lc = commit_dsh_source_step(
            &store,
            Some(&state3),
            Ok(&snap_lifecycle),
            ScanReasons::NONE,
            &cfg,
        )
        .unwrap();
        assert!(out_lc.advanced_saved_state);
        assert_eq!(
            out_lc.planned.plan.store(),
            StoreAction::Project(Projection::Rollback)
        );
        let stats_lc = out_lc.projection_stats.unwrap();
        assert_eq!(stats_lc.source_revision, 2);
        assert!(stats_lc.head_moved);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(feature = "store")]
    #[test]
    fn dsh_normalize_and_store_commit_respects_session_bindings_and_input_origin() {
        use crate::rawevent::{InputTrustLevel, SenderKind};
        use crate::scan_plan::ScanReasons;
        use crate::session_binding::SessionBindingRegistry;
        use crate::store::TotalStore;

        let dir =
            std::env::temp_dir().join(format!("sv-dsh-h123-{}-{}", std::process::id(), line!()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let records = [
            r#"{"type":"session","version":4,"id":"dsh_s1","cwd":"/workspace/demo"}"#,
            r#"{"type":"user/message","seq":0,"turn":1,"ts":1700000001000,"text":"[human-verified] self-claimed","source":{"principal":"human","deterministic":true}}"#,
            r#"{"type":"user/message","seq":1,"turn":2,"ts":1700000002000,"text":"[xs id=dsh_xs_9 from=win/claude/coord session=cl_1]\npeer command","source":{"principal":"human","deterministic":true}}"#,
            r#"{"type":"assistant/message","seq":2,"turn":2,"step":0,"ts":1700000003000,"model":"deepseek-chat","text":"ok","usage":{"inputTokens":40,"outputTokens":10,"cacheReadTokens":0,"cacheWriteTokens":0,"totalTokens":50}}"#,
        ];
        let snap = synthetic_source_snapshot(
            &dir,
            "dsh_s1",
            Compression::None,
            &records,
            0,
            "lc_dsh_s1",
            &["e0", "e1", "e2"],
        );
        let cfg = DshNormalizeConfig::local_full(dir.display().to_string());
        let norm = normalize_source_snapshot(&snap, &cfg).unwrap();
        assert_eq!(norm.events.len(), 4);

        // 1) 单边 `principal: "human"` + `[human-verified]` 正文标签不得自证为 HumanVerified
        let o0 = norm.events[0].input_origin.as_ref().unwrap();
        assert_eq!(o0.sender_kind, SenderKind::Unknown);
        assert_eq!(o0.trust_level, InputTrustLevel::Unverified);

        // 2) `[xs id=dsh_xs_9 ...]` 同侪信封即使带 `principal: "human"` 也判为 PeerSession + PeerUntrusted
        let o1 = norm.events[1].input_origin.as_ref().unwrap();
        assert_eq!(o1.sender_kind, SenderKind::PeerSession);
        assert_eq!(o1.trust_level, InputTrustLevel::PeerUntrusted);
        assert_eq!(o1.xs_message_id.as_deref(), Some("dsh_xs_9"));

        // 3) 若控制面绑定该 DSH 会话为 temporary: true，commit_dsh_source_step 入库时排除全部事件（含 Usage）
        let store = TotalStore::open_in_memory().unwrap();
        let reg = SessionBindingRegistry::from_json_str(
            &serde_json::json!({
                "schemaVersion": 1,
                "bindings": [
                    {
                        "schemaVersion": 1,
                        "owner": { "namespace": "tumechat.user", "id": "u_1" },
                        "host": { "namespace": "tumechat.host", "id": "h_1" },
                        "session": { "namespace": "tumechat.session", "id": "s_dsh_tmp" },
                        "nativeSource": {
                            "backend": "dsh-native",
                            "sourceLocation": "local",
                            "sourcePath": dir.display().to_string(),
                            "nativeSessionId": "dsh_s1"
                        },
                        "temporary": true,
                        "readMemory": false,
                        "recordedAt": "2026-11-09T08:00:00Z"
                    }
                ]
            })
            .to_string(),
        );
        store.set_session_bindings(&reg).unwrap();
        let out =
            commit_dsh_source_step(&store, None, Ok(&snap), ScanReasons::INITIAL, &cfg).unwrap();
        assert_eq!(
            out.planned.admission_status.as_deref(),
            Some("temporary_excluded")
        );
        assert_eq!(out.planned.events_excluded_temporary, 4);
        assert!(!out.advanced_saved_state);
        assert_eq!(
            out.projection_stats.map_or(0, |s| s.appended),
            0,
            "临时 DSH 会话的 Message 与 Usage 均不得写进总库"
        );
        assert_eq!(store.read_since_page(0, 50).unwrap().events.len(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dsh_complex_semantics_surface_fold_compaction_subagent_and_fork() {
        use crate::rawevent::{InputTrustLevel, SenderKind};

        let dir =
            std::env::temp_dir().join(format!("sv-dsh-complex-{}-{}", std::process::id(), line!()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // 构造包含继承前缀（seq 0..=2，其中 seq 2 为 session/end-seed）、
        // 自有 subagent/descriptor、subagent/catalog、工具调用与表面重写 tool/result、
        // 以及 compaction 事务（带 startSeq=0 的 compact-checkpoint 表面替换，测试索引 0 system/message 保护）的快照。
        let records = [
            r#"{"type":"session","version":4,"id":"s_complex","cwd":"/workspace/demo","parentSession":"s_parent","forkedFromSeq":1,"isSeeded":true,"origin":"subagent","delegationDepth":2,"agentPreset":"coder"}"#,
            r#"{"seq":0,"type":"system/message","time":1700000000000,"surfaceOp":"append","data":{"id":"sys-0","role":"system","content":[{"type":"text","text":"root system prompt"}]}}"#,
            r#"{"seq":1,"type":"user/message","time":1700000001000,"surfaceOp":"append","data":{"id":"u-1","role":"user","source":{"kind":"user"},"content":[{"type":"text","text":"inherited question"}]}}"#,
            r#"{"seq":2,"type":"session/end-seed","time":1700000001500,"data":{"inherited":true}}"#,
            r#"{"seq":3,"type":"subagent/descriptor","time":1700000001600,"data":{"version":3,"provider":"deepseek","mode":"foreground","label":"researcher"}}"#,
            r#"{"seq":4,"type":"user/message","time":1700000002000,"surfaceOp":"append","data":{"id":"u-4","role":"user","source":{"kind":"user"},"content":[{"type":"text","text":"subagent delegated prompt"}]}}"#,
            r#"{"seq":5,"type":"assistant/message","time":1700000003000,"surfaceOp":"append","data":{"turn":1,"step":0,"message":{"id":"a-5","role":"assistant","source":{"kind":"llm","provider":"deepseek","model":"deepseek-chat"},"content":[{"type":"text","text":"running tool"}]},"usage":{"inputTokens":20,"outputTokens":10,"cacheReadTokens":0,"cacheWriteTokens":0,"totalTokens":30}}}"#,
            r#"{"seq":6,"type":"compaction/start","time":1700000004000,"data":{"compactionId":"cmp-1","turn":2}}"#,
            r#"{"seq":7,"type":"compaction/summary","time":1700000004100,"data":{"compactionId":"cmp-1","turn":2,"shadowedRange":{"start":1,"end":5},"shadowedSeqs":[1,4,5],"summary":{"goal":"test"}}}"#,
            r#"{"seq":8,"type":"user/message","time":1700000004200,"surfaceOp":{"op":"replace","startSeq":0,"endSeq":5},"sourceEventSeqs":[1,4,5,7],"data":{"id":"u-8","role":"user","source":{"kind":"compact-checkpoint","compactionId":"cmp-1"},"content":[{"type":"text","text":"checkpoint summary"}]}}"#,
            r#"{"seq":9,"type":"compaction/end","time":1700000004300,"data":{"compactionId":"cmp-1","turn":2}}"#,
            r#"{"seq":10,"type":"subagent/catalog","time":1700000005000,"data":{"version":2,"childId":"s_grandchild","childCreatedAt":"2026-11-09T09:00:00.000Z","mode":"background","label":"verifier"}}"#,
            r#"{"seq":11,"type":"user/message","time":1700000005500,"surfaceOp":"append","data":{"id":"u-11","role":"user","source":{"kind":"subagent-settled","subagentSessionId":"s_grandchild"},"content":[{"type":"text","text":"grandchild settled"}]}}"#,
            r#"{"seq":12,"type":"tool/call","time":1700000006000,"data":{"turn":2,"step":0,"call":{"id":"call-1","name":"read","input":{"path":"a.txt"}}}}"#,
            r#"{"seq":13,"type":"tool/result","time":1700000006500,"surfaceOp":"append","data":{"id":"tr-13","role":"tool","toolCallId":"call-1","content":[{"type":"text","text":"raw large tool output"}]}}"#,
            r#"{"seq":14,"type":"tool/result","time":1700000006800,"surfaceOp":{"op":"replace","startSeq":13,"endSeq":13},"sourceEventSeqs":[13],"data":{"id":"tr-14","role":"tool","toolCallId":"call-1","content":[{"type":"text","text":"pruned tool output"}]}}"#,
        ];

        let snap = synthetic_source_snapshot(
            &dir,
            "s_complex",
            Compression::None,
            &records,
            3,
            "lc_complex",
            &[
                "e0", "e1", "e2", "e3", "e4", "e5", "e6", "e7", "e8", "e9", "e10", "e11", "e12",
                "e13", "e14",
            ],
        );
        let cfg = DshNormalizeConfig::local_full(dir.display().to_string());
        let norm = normalize_source_snapshot(&snap, &cfg).unwrap();

        assert!(norm.is_seeded);
        assert_eq!(norm.session_origin.as_deref(), Some("subagent"));
        assert_eq!(norm.delegation_depth, Some(2));
        assert_eq!(norm.agent_preset.as_deref(), Some("coder"));
        assert_eq!(norm.inherited_event_count, 3);
        assert_eq!(norm.suppressed_inherited_records, 3);

        // 核对 subagent/descriptor 与 subagent/catalog
        assert_eq!(norm.subagent_descriptors.len(), 1);
        assert_eq!(norm.subagent_descriptors[0].native_seq, 3);
        assert_eq!(norm.subagent_descriptors[0].provider, "deepseek");
        assert_eq!(norm.subagent_descriptors[0].mode, "foreground");
        assert_eq!(
            norm.subagent_descriptors[0].label.as_deref(),
            Some("researcher")
        );

        assert_eq!(norm.subagent_catalog.len(), 1);
        assert_eq!(norm.subagent_catalog[0].native_seq, 10);
        assert_eq!(norm.subagent_catalog[0].child_id, "s_grandchild");
        assert_eq!(norm.subagent_catalog[0].mode, "background");

        // 核对 compaction_records 与 surface_replacements
        assert_eq!(norm.compaction_records.len(), 3);
        assert_eq!(norm.compaction_records[1].native_type, "compaction/summary");
        assert_eq!(norm.compaction_records[1].shadowed_seqs, vec![1, 4, 5]);

        assert_eq!(norm.surface_replacements.len(), 2);
        // 第 1 次替换：startSeq=0, endSeq=5，因 seq 0 是 system/message 受保护，仅替换 [1, 4, 5]
        assert_eq!(norm.surface_replacements[0].replacement_native_seq, 8);
        assert_eq!(
            norm.surface_replacements[0].replaced_surface_seqs,
            vec![1, 4, 5]
        );
        // 第 2 次替换：tool/result seq 14 替换 seq 13
        assert_eq!(norm.surface_replacements[1].replacement_native_seq, 14);
        assert_eq!(norm.surface_replacements[1].replaced_surface_seqs, vec![13]);

        // 最终活跃表面应为：[0 (system), 8 (checkpoint), 11 (subagent-settled), 14 (replaced tool/result)]
        assert_eq!(norm.active_surface_native_seqs, vec![0, 8, 11, 14]);

        // 核对自有 spans 的 active_on_surface 与 shadowed_by_native_seq
        let span_by_seq = |nseq: u64, ty: EventType| -> &NormalizedEventSpan {
            norm.spans
                .iter()
                .find(|s| s.native_seq == nseq && s.event_type == ty)
                .unwrap()
        };
        let sp4 = span_by_seq(4, EventType::Message);
        assert!(!sp4.active_on_surface);
        assert_eq!(sp4.shadowed_by_native_seq, Some(8));

        let sp8 = span_by_seq(8, EventType::Message);
        assert!(sp8.active_on_surface);
        assert_eq!(sp8.surface_op.as_deref(), Some("replace"));
        assert_eq!(sp8.shadowed_by_native_seq, None);

        let sp13 = span_by_seq(13, EventType::ToolResult);
        assert!(!sp13.active_on_surface);
        assert_eq!(sp13.shadowed_by_native_seq, Some(14));

        let sp14 = span_by_seq(14, EventType::ToolResult);
        assert!(sp14.active_on_surface);
        assert_eq!(sp14.surface_op.as_deref(), Some("replace"));

        // 核对各条 user/message 的 InputOrigin：
        // - seq 4：位于 origin="subagent" 会话中的 source.kind="user" -> dsh_subagent_origin (SubagentOrRuntime)
        // - seq 8：source.kind="compact-checkpoint" -> dsh_compaction_checkpoint (SubagentOrRuntime)
        // - seq 11：source.kind="subagent-settled" -> dsh_subagent_settlement (SubagentOrRuntime, sender_session=s_grandchild)
        let ev4 = &norm.events[sp4.raw_seq as usize];
        let o4 = ev4.input_origin.as_ref().unwrap();
        assert_eq!(o4.sender_kind, SenderKind::SubagentOrRuntime);
        assert_eq!(o4.trust_level, InputTrustLevel::PeerUntrusted);
        assert_eq!(o4.basis, "dsh_subagent_origin");
        assert_eq!(o4.sender_session.as_deref(), Some("s_parent"));

        let ev8 = &norm.events[sp8.raw_seq as usize];
        let o8 = ev8.input_origin.as_ref().unwrap();
        assert_eq!(o8.sender_kind, SenderKind::SubagentOrRuntime);
        assert_eq!(o8.basis, "dsh_compaction_checkpoint");

        let sp11 = span_by_seq(11, EventType::Message);
        let ev11 = &norm.events[sp11.raw_seq as usize];
        let o11 = ev11.input_origin.as_ref().unwrap();
        assert_eq!(o11.sender_kind, SenderKind::SubagentOrRuntime);
        assert_eq!(o11.basis, "dsh_subagent_settlement");
        assert_eq!(o11.sender_session.as_deref(), Some("s_grandchild"));

        // 核对 tool/call 提取到了 data.call.name == "read"
        let sp12 = span_by_seq(12, EventType::ToolUse);
        let ev12 = &norm.events[sp12.raw_seq as usize];
        assert_eq!(ev12.content.as_deref(), Some("[Tool: read]"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
