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
    pub inherited_event_count: usize,
    pub suppressed_inherited_records: usize,
    pub events: Vec<RawEvent>,
    pub spans: Vec<NormalizedEventSpan>,
    pub usage_ledger: Vec<DshUsageEntry>,
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
            "inherited_event_count": self.inherited_event_count,
            "suppressed_inherited_records": self.suppressed_inherited_records,
            "emitted_raw_events": self.events.len(),
            "next_seq": self.next_seq(),
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
                Some(crate::session_binding::classify_user_input_origin(
                    Some(&rec.value),
                    content.as_deref().unwrap_or(""),
                    &cfg.source_path,
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
        inherited_event_count,
        suppressed_inherited_records,
        events,
        spans,
        usage_ledger,
    })
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
        .and_then(|d| d.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)
    {
        if let Some(text) = join_content_blocks(items, block_type) {
            return Some(text);
        }
    }
    if block_type == "text" {
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
        .and_then(|d| d.get("name"))
        .and_then(Value::as_str)
        .or_else(|| value.get("toolName").and_then(Value::as_str))
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("unknown")
}

fn extract_dsh_tool_result(value: &Value) -> Option<String> {
    if let Some(text) = extract_dsh_message_content(value, "text") {
        return Some(text);
    }
    if let Some(output) = non_empty_str(value.get("output")) {
        return Some(output);
    }
    let err = value.get("error")?;
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

    /// 将持久化状态（若有）与当前来源捕获结果转换为 `CommitGateDecision`、`AppendLogObservation` 与 `CommitPlan`。
    pub fn plan_dsh_source_step(
        prior_state: Option<&DshSavedState>,
        current: Result<&SourceSnapshot, &Blocked>,
        reasons: ScanReasons,
        prior_projection: PriorProjection,
        cfg: &DshNormalizeConfig,
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
                })
            }
            CommitGateDecision::InitialProjectionCandidate { .. } => {
                let snapshot = current.expect("InitialProjectionCandidate requires Ok snapshot");
                let normalized = normalize_source_snapshot(snapshot, cfg)?;
                let next_state = DshSavedState::from_verified(
                    snapshot,
                    normalized.next_seq(),
                    PARSER_REVISION,
                    cfg.attribution_revision,
                );
                let observation = AppendLogObservation {
                    source_change: SourceChange::Appended,
                    quality: ParseQuality::Clean {
                        deferred_tail_bytes: 0,
                    },
                    events: normalized.events.clone(),
                    cursor: cursor_from_verified(snapshot, normalized.next_seq()),
                    source_fingerprint: Some(SourceFingerprint::of(&semantic_stream_bytes(
                        snapshot,
                    ))),
                };
                let plan = CommitPlan::plan(&observation, effective_reasons, prior_projection);
                Ok(DshPlannedStep {
                    relation,
                    gate,
                    effective_reasons,
                    observation,
                    plan,
                    normalized: Some(normalized),
                    candidate_state: Some(next_state),
                })
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
                let next_state = DshSavedState::from_verified(
                    snapshot,
                    normalized.next_seq(),
                    PARSER_REVISION,
                    cfg.attribution_revision,
                );
                let observation = AppendLogObservation {
                    source_change: SourceChange::Appended,
                    quality: ParseQuality::Clean {
                        deferred_tail_bytes: 0,
                    },
                    events,
                    cursor: cursor_from_verified(snapshot, normalized.next_seq()),
                    source_fingerprint: Some(SourceFingerprint::of(&semantic_stream_bytes(
                        snapshot,
                    ))),
                };
                let plan = CommitPlan::plan(&observation, effective_reasons, prior_projection);
                Ok(DshPlannedStep {
                    relation,
                    gate,
                    effective_reasons,
                    observation,
                    plan,
                    normalized: Some(normalized),
                    candidate_state: Some(next_state),
                })
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
                let next_state = DshSavedState::from_verified(
                    snapshot,
                    normalized.next_seq(),
                    PARSER_REVISION,
                    cfg.attribution_revision,
                );
                let observation = AppendLogObservation {
                    source_change: SourceChange::Appended,
                    quality: ParseQuality::Clean {
                        deferred_tail_bytes: 0,
                    },
                    events,
                    cursor: cursor_from_verified(snapshot, normalized.next_seq()),
                    source_fingerprint: need_full_events
                        .then(|| SourceFingerprint::of(&semantic_stream_bytes(snapshot))),
                };
                let plan = CommitPlan::plan(&observation, effective_reasons, prior_projection);
                Ok(DshPlannedStep {
                    relation,
                    gate,
                    effective_reasons,
                    observation,
                    plan,
                    normalized: Some(normalized),
                    candidate_state: Some(next_state),
                })
            }
            CommitGateDecision::RebuildProjectionCandidate { .. } => {
                let snapshot = current.expect("RebuildProjectionCandidate requires Ok snapshot");
                let normalized = normalize_source_snapshot(snapshot, cfg)?;
                let next_state = DshSavedState::from_verified(
                    snapshot,
                    normalized.next_seq(),
                    PARSER_REVISION,
                    cfg.attribution_revision,
                );
                let observation = AppendLogObservation {
                    source_change: SourceChange::RollbackOrRewrite,
                    quality: ParseQuality::Clean {
                        deferred_tail_bytes: 0,
                    },
                    events: normalized.events.clone(),
                    cursor: cursor_from_verified(snapshot, normalized.next_seq()),
                    source_fingerprint: Some(SourceFingerprint::of(&semantic_stream_bytes(
                        snapshot,
                    ))),
                };
                let plan = CommitPlan::plan(&observation, effective_reasons, prior_projection);
                Ok(DshPlannedStep {
                    relation,
                    gate,
                    effective_reasons,
                    observation,
                    plan,
                    normalized: Some(normalized),
                    candidate_state: Some(next_state),
                })
            }
        }
    }

    /// 执行单步 DSH 观察、规划并写入 `TotalStore`；仅在写库成功且 `CursorAction::Advance` 时推进 `DshSavedState`。
    pub fn commit_dsh_source_step(
        store: &TotalStore,
        prior_state: Option<&DshSavedState>,
        current: Result<&SourceSnapshot, &Blocked>,
        reasons: ScanReasons,
        cfg: &DshNormalizeConfig,
    ) -> StoreResult<DshStoreCommitOutcome> {
        let source_key = SourceKey {
            source_type: SourceType::Jsonl,
            source_location: cfg.source_location.clone(),
            source_path: cfg.source_path.clone(),
        };
        let prior_projection = PriorProjection::ask(store, &source_key)?;
        let planned = plan_dsh_source_step(prior_state, current, reasons, prior_projection, cfg)
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

        let advanced_saved_state = planned.plan.cursor() == CursorAction::Advance;
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
}

#[cfg(feature = "store")]
pub use store_wiring::{
    commit_dsh_source_step, plan_dsh_source_step, DshPlannedStep, DshStoreCommitOutcome,
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
            out.projection_stats.unwrap().appended,
            0,
            "临时 DSH 会话的 Message 与 Usage 均不得写进总库"
        );
        assert_eq!(store.read_since_page(0, 50).unwrap().events.len(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
