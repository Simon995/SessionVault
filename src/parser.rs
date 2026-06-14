//! 行级解析（§7）：把完整 JSONL 行映射为 `RawEvent`。
//!
//! 字段映射移植自 QuotaBar `parse_claude_lines` / `parse_codex_lines` /
//! `message_from_*_value` / `extract_text` / `parse_claude_jsonl_entry`（见
//! `docs/rawevent-reconciliation.md` §4 抽取地图）。但产出形态是**逐事件、含正文、
//! 带 actor/event_type 的 RawEvent 流**——这是 QuotaBar 之上的新建能力（reconciliation §3），
//! 非平移：QuotaBar 只在 usage 事件落 fact、正文用完即弃。
//!
//! 与 QuotaBar 的有意差异：无时间戳的事件**不丢弃**，照发但 `time_confidence=low`
//! （reconciliation §3 的设计意图）。Codex 累计 token 的 delta 数学与 QuotaBar 完全一致。

use serde_json::Value;

use crate::catalog::Profile;
use crate::cursor::{CodexState, CodexUsage};
use crate::pathnorm::{self, HostPlatform};
use crate::project_root::{resolve_project_root, ProjectRoot};
use crate::rawevent::{
    Actor, EventType, RawEvent, SourceLocation, SourceMode, SourceType, TimeConfidence, TokenUsage,
    SCHEMA_VERSION,
};

/// 解析产物：本批事件 + 更新后的 Codex 状态 + 跳过计数 + 告警。
#[derive(Debug, Clone, Default)]
pub struct ParseOut {
    pub events: Vec<RawEvent>,
    pub codex_state: Option<CodexState>,
    /// 跳过的坏行数（坏 JSON）。
    pub skipped: u64,
    pub warnings: Vec<String>,
}

/// 解析上下文：填充 `RawEvent` 来源定位字段所需。
pub struct ParseCtx {
    pub source_type: SourceType,
    pub source_location: SourceLocation,
    pub source_path: String,
    pub profile: Profile,
    /// 宿主平台——决定裸 Unix 绝对路径归 `local` 还是 `wsl`（见 `pathnorm`）。
    pub host: HostPlatform,
}

impl ParseCtx {
    fn want_content(&self) -> bool {
        matches!(self.profile, Profile::Full)
    }

    /// 构造一条带公共字段的事件骨架；调用方再覆盖 model/usage/content 等。
    #[allow(clippy::too_many_arguments)]
    fn event(
        &self,
        seq: u64,
        session_id: &str,
        event_type: EventType,
        actor: Option<Actor>,
        occurred_at: Option<String>,
        cwd: Option<String>,
        pr: Option<&ProjectRoot>,
    ) -> RawEvent {
        let has_time = occurred_at.is_some();
        // project_root 已是规范化路径（cwd 在 resolve_cached 里先过 pathnorm::normalize_cwd）。
        // workspace_location 据此 + transcript 位置 + 宿主平台判定工程物理归属。
        let project_root = pr.and_then(|p| p.path.as_ref().map(|x| x.display().to_string()));
        let workspace_location = project_root
            .as_deref()
            .map(|root| pathnorm::workspace_location(root, &self.source_location, self.host));
        RawEvent {
            schema_version: SCHEMA_VERSION,
            source_type: self.source_type,
            source_location: self.source_location.clone(),
            source_path: self.source_path.clone(),
            source_session_id: session_id.to_string(),
            seq,
            source_mode: SourceMode::AppendLog,
            cwd,
            project_root,
            project_root_source: pr.map(|p| p.source.clone()),
            workspace_location,
            event_type,
            actor,
            occurred_at,
            time_confidence: if has_time {
                TimeConfidence::High
            } else {
                TimeConfidence::Low
            },
            model: None,
            effort: None,
            usage: None,
            content: None,
            parent_ref: None,
            message_id: None,
            request_id: None,
        }
    }
}

/// 解析一批完整行。`base_seq` 是本批首个事件的 `seq` 起点（增量批次间延续）。
pub fn parse_lines(
    ctx: &ParseCtx,
    lines: &[&str],
    base_seq: u64,
    codex_state: Option<CodexState>,
) -> ParseOut {
    match ctx.source_type {
        SourceType::ClaudeCode => parse_claude(ctx, lines, base_seq),
        SourceType::Codex => parse_codex(ctx, lines, base_seq, codex_state),
        // 其它 provider v0 未实装解析器：仅校验 JSON、计 skipped，状态透传。
        _ => {
            let mut out = ParseOut {
                codex_state,
                ..Default::default()
            };
            for (idx, raw) in lines.iter().enumerate() {
                let line = raw.trim();
                if line.is_empty() {
                    continue;
                }
                if let Err(e) = serde_json::from_str::<Value>(line) {
                    record_skip(&mut out, &ctx.source_path, idx, &e);
                }
            }
            out
        }
    }
}

// ---------------------------------------------------------------------------
// Claude
// ---------------------------------------------------------------------------

fn parse_claude(ctx: &ParseCtx, lines: &[&str], base_seq: u64) -> ParseOut {
    let fallback = session_id_from_path(&ctx.source_path);
    let mut out = ParseOut::default();
    let mut seq = base_seq;
    let mut cache: Option<(String, ProjectRoot)> = None;

    for (idx, raw) in lines.iter().enumerate() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let value: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                record_skip(&mut out, &ctx.source_path, idx, &e);
                continue;
            }
        };

        let session_id = extract_claude_session_id(&value)
            .map(str::to_string)
            .unwrap_or_else(|| fallback.clone());
        let cwd = value.get("cwd").and_then(Value::as_str).map(str::to_string);
        let occurred_at = value
            .get("timestamp")
            .and_then(Value::as_str)
            .map(str::to_string);
        let pr = resolve_cached(&mut cache, cwd.as_deref(), ctx.host);

        // 1) thinking（Claude `message.content[].type=thinking`）。
        if let Some(text) = extract_claude_thinking(&value) {
            let mut ev = ctx.event(
                seq,
                &session_id,
                EventType::Thinking,
                Some(Actor::Assistant),
                occurred_at.clone(),
                cwd.clone(),
                pr.as_ref(),
            );
            if ctx.want_content() {
                ev.content = Some(text);
            }
            out.events.push(ev);
            seq += 1;
        }

        // 2) message（含正文，按 profile）。
        if let Some((actor, content)) = claude_message(&value) {
            let mut ev = ctx.event(
                seq,
                &session_id,
                EventType::Message,
                actor,
                occurred_at.clone(),
                cwd.clone(),
                pr.as_ref(),
            );
            ev.model = extract_claude_model(&value);
            if ctx.want_content() {
                ev.content = Some(content);
            }
            out.events.push(ev);
            seq += 1;
        }

        // 3) usage（`type=assistant` 带 `message.usage`）。
        if let Some(u) = claude_usage(&value) {
            let mut ev = ctx.event(
                seq,
                &session_id,
                EventType::Usage,
                Some(Actor::Assistant),
                occurred_at.clone(),
                cwd.clone(),
                pr.as_ref(),
            );
            ev.model = u.model;
            ev.usage = Some(u.usage);
            ev.message_id = u.message_id;
            ev.request_id = u.request_id;
            out.events.push(ev);
            seq += 1;
        }
    }

    out.codex_state = None;
    out
}

fn extract_claude_session_id(value: &Value) -> Option<&str> {
    value
        .get("sessionId")
        .and_then(Value::as_str)
        .or_else(|| value.get("session_id").and_then(Value::as_str))
}

fn extract_claude_model(value: &Value) -> Option<String> {
    value
        .get("message")
        .and_then(|m| m.get("model"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// 思考块文本（`message.content[].type=thinking` 的 `.thinking`，拼接）。
fn extract_claude_thinking(value: &Value) -> Option<String> {
    if value.get("isMeta").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    let items = value.get("message")?.get("content")?.as_array()?;
    let text = items
        .iter()
        .filter(|i| i.get("type").and_then(Value::as_str) == Some("thinking"))
        .filter_map(|i| i.get("thinking").and_then(Value::as_str))
        .filter(|t| !t.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    (!text.trim().is_empty()).then_some(text)
}

/// 可见消息：`(actor, content)`；mirror `message_from_claude_value`。
fn claude_message(value: &Value) -> Option<(Option<Actor>, String)> {
    if value.get("isMeta").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    let message = value.get("message")?;
    let mut role = message
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    if role == "user" {
        if let Some(Value::Array(items)) = message.get("content") {
            let all_tool = !items.is_empty()
                && items
                    .iter()
                    .all(|i| i.get("type").and_then(Value::as_str) == Some("tool_result"));
            if all_tool {
                role = "tool".to_string();
            }
        }
    }
    let content = message.get("content").map(extract_text).unwrap_or_default();
    if content.trim().is_empty() {
        return None;
    }
    Some((actor_from_role(&role), content))
}

struct ClaudeUsage {
    model: Option<String>,
    usage: TokenUsage,
    message_id: Option<String>,
    request_id: Option<String>,
}

/// usage 提取；mirror `parse_claude_jsonl_entry`（但不因缺时间戳而丢弃）。
fn claude_usage(value: &Value) -> Option<ClaudeUsage> {
    if value.get("type").and_then(Value::as_str) != Some("assistant") {
        return None;
    }
    let message = value.get("message")?;
    let usage = message.get("usage")?;
    Some(ClaudeUsage {
        model: message
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_string),
        usage: TokenUsage {
            input: read_u64(usage, "input_tokens"),
            output: read_u64(usage, "output_tokens"),
            cache_creation: read_u64(usage, "cache_creation_input_tokens"),
            cache_read: read_u64(usage, "cache_read_input_tokens"),
        },
        message_id: message.get("id").and_then(Value::as_str).map(str::to_string),
        request_id: value
            .get("requestId")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

// ---------------------------------------------------------------------------
// Codex
// ---------------------------------------------------------------------------

fn parse_codex(
    ctx: &ParseCtx,
    lines: &[&str],
    base_seq: u64,
    initial_state: Option<CodexState>,
) -> ParseOut {
    let mut state = initial_state.unwrap_or_default();
    if state.current_session_id.is_none() {
        state.current_session_id = Some(session_id_from_path(&ctx.source_path));
    }
    let mut out = ParseOut::default();
    let mut seq = base_seq;
    let mut cache: Option<(String, ProjectRoot)> = None;
    let null = Value::Null;

    for (idx, raw) in lines.iter().enumerate() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let value: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                record_skip(&mut out, &ctx.source_path, idx, &e);
                continue;
            }
        };
        let entry_type = value.get("type").and_then(Value::as_str);
        let payload = value.get("payload").unwrap_or(&null);
        let occurred_at = value
            .get("timestamp")
            .and_then(Value::as_str)
            .map(str::to_string);

        // 元信息行：更新状态，不产事件。
        match entry_type {
            Some("session_meta") => {
                if let Some(id) = payload.get("id").and_then(Value::as_str) {
                    // 同一文件可含多个 session_meta（黄金语料用例）。切到**新** session 时
                    // 必须重置 session 绑定状态——否则新 session 从 0 起算的 total_token_usage
                    // 会被减去上一 session 的累计值，delta 变 0 或错值；model/effort/cwd 同理。
                    if state.current_session_id.as_deref() != Some(id) {
                        state.previous_total = CodexUsage::default();
                        state.current_model = None;
                        state.current_effort = None;
                        state.current_cwd = None;
                    }
                    state.current_session_id = Some(id.to_string());
                }
                if let Some(c) = payload.get("cwd").and_then(Value::as_str) {
                    state.current_cwd = Some(c.to_string());
                }
                continue;
            }
            Some("turn_context") => {
                if let Some(m) = extract_codex_model(payload) {
                    state.current_model = Some(m);
                }
                if let Some(e) = extract_codex_effort(payload) {
                    state.current_effort = Some(e);
                }
                if let Some(c) = payload.get("cwd").and_then(Value::as_str) {
                    state.current_cwd = Some(c.to_string());
                }
                continue;
            }
            _ => {}
        }

        let session_id = state
            .current_session_id
            .clone()
            .unwrap_or_else(|| session_id_from_path(&ctx.source_path));
        let cwd = state.current_cwd.clone();
        let pr = resolve_cached(&mut cache, cwd.as_deref(), ctx.host);

        // response_item：reasoning→thinking / message / tool_use / tool_result。
        if entry_type == Some("response_item") {
            let ptype = payload.get("type").and_then(Value::as_str).unwrap_or("");
            if ptype == "reasoning" {
                // 含明文 summary 则带正文；仅 encrypted_content（无明文）→ content None（opaque）。
                let text = codex_reasoning_text(payload);
                let mut ev = ctx.event(
                    seq,
                    &session_id,
                    EventType::Thinking,
                    Some(Actor::Assistant),
                    occurred_at.clone(),
                    cwd.clone(),
                    pr.as_ref(),
                );
                if ctx.want_content() {
                    ev.content = text;
                }
                ev.model = state.current_model.clone();
                ev.effort = state.current_effort.clone();
                out.events.push(ev);
                seq += 1;
            } else if let Some((event_type, actor, content)) = codex_message(payload, ptype) {
                let mut ev = ctx.event(
                    seq,
                    &session_id,
                    event_type,
                    actor,
                    occurred_at.clone(),
                    cwd.clone(),
                    pr.as_ref(),
                );
                ev.model = state.current_model.clone();
                ev.effort = state.current_effort.clone();
                if ctx.want_content() {
                    ev.content = Some(content);
                }
                out.events.push(ev);
                seq += 1;
            }
        }

        // event_msg：**只取 token_count 出 usage**。正文（user_message / agent_message /
        // agent_reasoning 等 event_msg 类型）是上面 response_item（message / reasoning）的
        // UI 镜像——Codex rollout 同时写两套，正文权威源是 response_item（QuotaBar 实证：
        // 正文仅从 response_item 提取、event_msg 仅取 token_count）。若也从 event_msg 取正文
        // 会与 response_item **重复计数**。如将来出现 event_msg-only 的格式（无 response_item），
        // 应作两层契约的「已验证实现」补充并配去重，而非在此盲目展开。
        if entry_type == Some("event_msg")
            && payload.get("type").and_then(Value::as_str) == Some("token_count")
        {
            if let Some(usage) = codex_usage_delta(payload, &mut state) {
                let mut ev = ctx.event(
                    seq,
                    &session_id,
                    EventType::Usage,
                    Some(Actor::Assistant),
                    occurred_at.clone(),
                    cwd.clone(),
                    pr.as_ref(),
                );
                ev.model = state.current_model.clone();
                ev.effort = state.current_effort.clone();
                ev.usage = Some(usage);
                out.events.push(ev);
                seq += 1;
            }
        }
    }

    out.codex_state = Some(state);
    out
}

/// Codex reasoning 明文（`payload.summary[].text` 拼接）；无明文返回 None（opaque）。
fn codex_reasoning_text(payload: &Value) -> Option<String> {
    let summary = payload.get("summary")?.as_array()?;
    let text = summary
        .iter()
        .filter_map(|s| s.get("text").and_then(Value::as_str))
        .filter(|t| !t.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    (!text.trim().is_empty()).then_some(text)
}

/// Codex 可见 response_item → `(event_type, actor, content)`；mirror `message_from_codex_value`。
fn codex_message(payload: &Value, ptype: &str) -> Option<(EventType, Option<Actor>, String)> {
    match ptype {
        "message" => {
            let role = payload
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let content = payload.get("content").map(extract_text).unwrap_or_default();
            if content.trim().is_empty() {
                return None;
            }
            Some((EventType::Message, actor_from_role(role), content))
        }
        "function_call" => {
            let name = payload
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            Some((
                EventType::ToolUse,
                Some(Actor::Assistant),
                format!("[Tool: {name}]"),
            ))
        }
        "function_call_output" => {
            let content = payload
                .get("output")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if content.trim().is_empty() {
                return None;
            }
            Some((EventType::ToolResult, Some(Actor::Tool), content))
        }
        _ => None,
    }
}

/// Codex 累计 token → 本条 delta 的四段计量；并推进 `state.previous_total`。
/// `last_token_usage` 存在时直接取，否则 `total − previous`；与 QuotaBar 完全一致。
fn codex_usage_delta(payload: &Value, state: &mut CodexState) -> Option<TokenUsage> {
    let info = payload.get("info").unwrap_or(&Value::Null).clone();
    let last = info.get("last_token_usage").and_then(parse_codex_usage);
    let total = info.get("total_token_usage").and_then(parse_codex_usage);
    let delta = match (last, total) {
        (Some(l), _) => Some(l),
        (None, Some(t)) => Some(subtract_codex_usage(&t, &state.previous_total)),
        _ => None,
    };
    if let Some(t) = total {
        state.previous_total = t;
    }
    let delta = delta?;
    if delta.input == 0 && delta.cached == 0 && delta.output == 0 {
        return None;
    }
    let cached = delta.cached.min(delta.input);
    Some(TokenUsage {
        input: delta.input.saturating_sub(cached),
        output: delta.output,
        cache_creation: 0,
        cache_read: cached,
    })
}

fn parse_codex_usage(v: &Value) -> Option<CodexUsage> {
    Some(CodexUsage {
        input: read_u64(v, "input_tokens"),
        cached: read_u64(v, "cached_input_tokens"),
        output: read_u64(v, "output_tokens"),
    })
}

fn subtract_codex_usage(total: &CodexUsage, prev: &CodexUsage) -> CodexUsage {
    CodexUsage {
        input: total.input.saturating_sub(prev.input),
        cached: total.cached.saturating_sub(prev.cached),
        output: total.output.saturating_sub(prev.output),
    }
}

fn extract_codex_model(v: &Value) -> Option<String> {
    v.get("model")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| v.get("model_slug").and_then(Value::as_str).map(str::to_string))
}

fn extract_codex_effort(v: &Value) -> Option<String> {
    v.get("effort")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            v.get("collaboration_mode")
                .and_then(|c| c.get("reasoning_effort"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
}

// ---------------------------------------------------------------------------
// 共享
// ---------------------------------------------------------------------------

/// 把 `content`（字符串 / 块数组 / 对象）展平为可读文本；mirror QuotaBar `extract_text`。
fn extract_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(extract_text_item)
            .filter(|t| !t.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(map) => map
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        _ => String::new(),
    }
}

fn extract_text_item(item: &Value) -> Option<String> {
    match item.get("type").and_then(Value::as_str).unwrap_or("") {
        "tool_use" => {
            let name = item.get("name").and_then(Value::as_str).unwrap_or("unknown");
            return Some(format!("[Tool: {name}]"));
        }
        "tool_result" => {
            if let Some(inner) = item.get("content") {
                let text = extract_text(inner);
                if !text.is_empty() {
                    return Some(text);
                }
            }
            return None;
        }
        _ => {}
    }
    for key in ["text", "input_text", "output_text"] {
        if let Some(text) = item.get(key).and_then(Value::as_str) {
            return Some(text.to_string());
        }
    }
    if let Some(inner) = item.get("content") {
        let text = extract_text(inner);
        if !text.is_empty() {
            return Some(text);
        }
    }
    None
}

fn actor_from_role(role: &str) -> Option<Actor> {
    match role {
        "user" => Some(Actor::User),
        "assistant" => Some(Actor::Assistant),
        "tool" => Some(Actor::Tool),
        "system" => Some(Actor::System),
        _ => None,
    }
}

fn read_u64(v: &Value, key: &str) -> u64 {
    v.get(key)
        .and_then(Value::as_u64)
        .or_else(|| v.get(key).and_then(Value::as_f64).map(|n| n as u64))
        .unwrap_or(0)
}

/// 文件名（去扩展名）作 session_id 回退；真实 UUID 解析留细化。
fn session_id_from_path(path: &str) -> String {
    std::path::Path::new(path)
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(path)
        .to_string()
}

/// 按 cwd 缓存工程根解析，避免逐行重复 find_upward 文件系统遍历。
/// 解析工程根（带按原始 cwd 缓存）。先把原始 cwd 过 [`pathnorm::normalize_cwd`]
/// 归一到规范形，再上溯 marker——这样产出的 `project_root` 是规范化路径，
/// 供 `workspace_location` 正确判定 local/wsl。
///
/// `default_distro` 暂传 `None`（访问桥未实装，不枚举 WSL 发行版）：UNC 路径仍能精确
/// 还原 distro，裸 Linux 路径在 Windows 宿主上回落泛 `wsl`。
fn resolve_cached(
    cache: &mut Option<(String, ProjectRoot)>,
    cwd: Option<&str>,
    host: HostPlatform,
) -> Option<ProjectRoot> {
    let cwd = cwd?;
    if let Some((c, pr)) = cache.as_ref() {
        if c == cwd {
            return Some(pr.clone());
        }
    }
    let normalized = pathnorm::normalize_cwd(Some(cwd), host, None);
    let pr = resolve_project_root(normalized.as_deref(), host);
    *cache = Some((cwd.to_string(), pr.clone()));
    Some(pr)
}

fn record_skip(out: &mut ParseOut, path: &str, idx: usize, e: &serde_json::Error) {
    out.skipped += 1;
    if out.skipped == 1 {
        out.warnings
            .push(format!("{path}:{}: invalid json: {e}", idx + 1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(source_type: SourceType, profile: Profile) -> ParseCtx {
        ParseCtx {
            source_type,
            source_location: SourceLocation::Local,
            source_path: "/tmp/abc-session.jsonl".to_string(),
            profile,
            host: HostPlatform::current(),
        }
    }

    #[test]
    fn claude_assistant_emits_message_and_usage_with_content() {
        let line = serde_json::json!({
            "type": "assistant",
            "sessionId": "sess-1",
            "cwd": "/work/proj",
            "timestamp": "2026-06-01T10:00:00.000Z",
            "requestId": "req_1",
            "message": {
                "id": "msg_1",
                "role": "assistant",
                "model": "claude-opus-4-8",
                "content": [{"type": "text", "text": "hello world"}],
                "usage": {
                    "input_tokens": 100, "output_tokens": 50,
                    "cache_creation_input_tokens": 5, "cache_read_input_tokens": 20
                }
            }
        })
        .to_string();
        let out = parse_lines(&ctx(SourceType::ClaudeCode, Profile::Full), &[line.as_str()], 0, None);
        assert_eq!(out.skipped, 0);
        assert_eq!(out.events.len(), 2, "assistant line → message + usage");

        let msg = &out.events[0];
        assert_eq!(msg.event_type, EventType::Message);
        assert_eq!(msg.actor, Some(Actor::Assistant));
        assert_eq!(msg.content.as_deref(), Some("hello world"));
        assert_eq!(msg.source_session_id, "sess-1");
        assert_eq!(msg.seq, 0);
        assert_eq!(msg.time_confidence, TimeConfidence::High);

        let usage = &out.events[1];
        assert_eq!(usage.event_type, EventType::Usage);
        assert_eq!(usage.seq, 1);
        let u = usage.usage.unwrap();
        assert_eq!((u.input, u.output, u.cache_creation, u.cache_read), (100, 50, 5, 20));
        assert_eq!(usage.message_id.as_deref(), Some("msg_1"));
        assert_eq!(usage.request_id.as_deref(), Some("req_1"));
    }

    #[test]
    fn workspace_location_populated_from_cwd() {
        // UNC cwd → 规范化 → project_root 标 wsl_cwd、workspace_location = wsl:<distro>。
        // 这条断言锁住 cwd → normalize_cwd → resolve_project_root → workspace_location 全链路。
        let unc = serde_json::json!({
            "type": "user",
            "sessionId": "s",
            "cwd": r"\\wsl$\Ubuntu\home\me\proj",
            "message": {"role": "user", "content": "hi"}
        })
        .to_string();
        let out = parse_lines(&ctx(SourceType::ClaudeCode, Profile::Full), &[unc.as_str()], 0, None);
        let ev = &out.events[0];
        assert_eq!(ev.project_root_source.as_deref(), Some("wsl_cwd"));
        assert_eq!(ev.workspace_location.as_deref(), Some("wsl:Ubuntu"));

        // /mnt/<drive> 是挂载的 Windows 盘 → local（不被误标 wsl）。
        let mnt = serde_json::json!({
            "type": "user",
            "sessionId": "s",
            "cwd": "/mnt/c/code/proj",
            "message": {"role": "user", "content": "hi"}
        })
        .to_string();
        let out = parse_lines(&ctx(SourceType::ClaudeCode, Profile::Full), &[mnt.as_str()], 0, None);
        assert_eq!(out.events[0].workspace_location.as_deref(), Some("local"));
    }

    #[test]
    fn metadata_profile_drops_content() {
        let line = serde_json::json!({
            "type": "user",
            "sessionId": "s",
            "message": {"role": "user", "content": "secret text"}
        })
        .to_string();
        let out = parse_lines(&ctx(SourceType::ClaudeCode, Profile::Metadata), &[line.as_str()], 0, None);
        assert_eq!(out.events.len(), 1);
        assert_eq!(out.events[0].event_type, EventType::Message);
        assert_eq!(out.events[0].content, None, "metadata 档不带正文");
        assert_eq!(out.events[0].time_confidence, TimeConfidence::Low, "无时间戳→low");
    }

    #[test]
    fn claude_thinking_event() {
        let line = serde_json::json!({
            "sessionId": "s",
            "message": {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "let me reason"},
                {"type": "text", "text": "answer"}
            ]}
        })
        .to_string();
        let out = parse_lines(&ctx(SourceType::ClaudeCode, Profile::Full), &[line.as_str()], 0, None);
        assert_eq!(out.events.len(), 2, "thinking + message");
        assert_eq!(out.events[0].event_type, EventType::Thinking);
        assert_eq!(out.events[0].content.as_deref(), Some("let me reason"));
        assert_eq!(out.events[1].event_type, EventType::Message);
    }

    #[test]
    fn codex_cumulative_token_delta() {
        let meta = serde_json::json!({
            "type": "session_meta",
            "payload": {"id": "cdx-1", "cwd": "/c/proj"}
        })
        .to_string();
        let tok1 = serde_json::json!({
            "type": "event_msg",
            "timestamp": "2026-06-01T10:00:00Z",
            "payload": {"type": "token_count", "info": {
                "total_token_usage": {"input_tokens": 100, "cached_input_tokens": 20, "output_tokens": 50}
            }}
        })
        .to_string();
        let tok2 = serde_json::json!({
            "type": "event_msg",
            "timestamp": "2026-06-01T10:01:00Z",
            "payload": {"type": "token_count", "info": {
                "total_token_usage": {"input_tokens": 150, "cached_input_tokens": 30, "output_tokens": 80}
            }}
        })
        .to_string();
        let lines = [meta.as_str(), tok1.as_str(), tok2.as_str()];
        let out = parse_lines(&ctx(SourceType::Codex, Profile::Full), &lines, 0, None);

        let usages: Vec<_> = out
            .events
            .iter()
            .filter(|e| e.event_type == EventType::Usage)
            .collect();
        assert_eq!(usages.len(), 2);
        assert_eq!(usages[0].source_session_id, "cdx-1");
        // 第一条：total-0 = {100,20,50} → cached=min(20,100)=20, input=80, read=20
        let u0 = usages[0].usage.unwrap();
        assert_eq!((u0.input, u0.output, u0.cache_read), (80, 50, 20));
        // 第二条：delta = {50,10,30} → cached=min(10,50)=10, input=40, read=10
        let u1 = usages[1].usage.unwrap();
        assert_eq!((u1.input, u1.output, u1.cache_read), (40, 30, 10));
    }

    #[test]
    fn codex_multi_session_resets_cumulative_state() {
        // 同一文件两个 session_meta：第二 session 的 total 从 0 起算，
        // 不应减去第一 session 的累计值（否则 delta 归零、usage 事件丢失）。
        let s1 = serde_json::json!({"type": "session_meta", "payload": {"id": "s1"}}).to_string();
        let t1 = serde_json::json!({
            "type": "event_msg", "timestamp": "2026-06-01T10:00:00Z",
            "payload": {"type": "token_count", "info": {
                "total_token_usage": {"input_tokens": 100, "cached_input_tokens": 20, "output_tokens": 50}
            }}
        })
        .to_string();
        let s2 = serde_json::json!({"type": "session_meta", "payload": {"id": "s2"}}).to_string();
        let t2 = serde_json::json!({
            "type": "event_msg", "timestamp": "2026-06-01T11:00:00Z",
            "payload": {"type": "token_count", "info": {
                "total_token_usage": {"input_tokens": 30, "cached_input_tokens": 5, "output_tokens": 10}
            }}
        })
        .to_string();
        let lines = [s1.as_str(), t1.as_str(), s2.as_str(), t2.as_str()];
        let out = parse_lines(&ctx(SourceType::Codex, Profile::Full), &lines, 0, None);
        let usages: Vec<_> = out
            .events
            .iter()
            .filter(|e| e.event_type == EventType::Usage)
            .collect();
        assert_eq!(usages.len(), 2, "两个 session 各出一条 usage（无重置则第二条会被减成 0 而丢失）");
        assert_eq!(usages[1].source_session_id, "s2");
        // s2 从 0 起算：delta={30,5,10} → cached=min(5,30)=5, input=25, read=5
        let u = usages[1].usage.unwrap();
        assert_eq!((u.input, u.output, u.cache_read), (25, 10, 5));
    }

    #[test]
    fn codex_reasoning_opaque_when_no_plaintext() {
        // 只有 encrypted_content、无 summary 明文 → thinking 事件但 content=None（opaque）。
        let meta = serde_json::json!({"type": "session_meta", "payload": {"id": "c"}}).to_string();
        let reasoning = serde_json::json!({
            "type": "response_item",
            "payload": {"type": "reasoning", "encrypted_content": "AAAA"}
        })
        .to_string();
        let lines = [meta.as_str(), reasoning.as_str()];
        let out = parse_lines(&ctx(SourceType::Codex, Profile::Full), &lines, 0, None);
        let thinking: Vec<_> = out
            .events
            .iter()
            .filter(|e| e.event_type == EventType::Thinking)
            .collect();
        assert_eq!(thinking.len(), 1);
        assert_eq!(thinking[0].content, None, "无明文 reasoning → opaque");
    }

    #[test]
    fn seq_continues_from_base() {
        let line = serde_json::json!({
            "type": "user", "sessionId": "s",
            "message": {"role": "user", "content": "hi"}
        })
        .to_string();
        let out = parse_lines(&ctx(SourceType::ClaudeCode, Profile::Full), &[line.as_str()], 42, None);
        assert_eq!(out.events[0].seq, 42, "seq 从 base_seq 起");
    }
}
