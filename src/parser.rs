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

use std::sync::Arc;

use serde_json::Value;

use crate::attribution::{attribute, Attribution, RootRegistry};
use crate::catalog::Profile;
use crate::cursor::{CodexState, CodexUsage};
use crate::pathnorm::{self, HostPlatform};
use crate::project_root::ProjectRoot;
use crate::rawevent::{
    Actor, EventKey, EventType, RawEvent, SourceLocation, SourceMode, SourceType, TimeConfidence,
    TokenUsage, EVENT_KEY_VERSION, SCHEMA_VERSION,
};

/// 解析器语义版本。**从同一份字节里能提取出什么**发生变化时 +1。
///
/// 🔴 与 [`SCHEMA_VERSION`] 是两件事，不能互相顶替：
///
/// - `SCHEMA_VERSION` 是 `RawEvent` **DTO 的契约版本**，面向 TumeFlow 等消费者；
///   它变了意味着字段形状变了，下游要改代码。
/// - `PARSER_REVISION` 是**提取能力的版本**。DTO 一字未动，但同一行 JSONL 现在
///   能解析出以前解析不出的东西，所以**已经扫过的文件需要重扫**。
///
/// 为什么必须分开：修 Claude `effort` 时 DTO 完全没变，若挪用 `SCHEMA_VERSION`
/// 就会向 TumeFlow 谎报一次破坏性变更；而若不加版本号，增量扫描按
/// `(path, mtime, size)` 判定命中，文件没动就永不重解析 —— 一年的历史数据会
/// 永远停在旧解析器的结果上，而界面上看不出任何异常。
///
/// 消费者（QuotaBar 的 session index）把它连同游标一起持久化，发现存量 revision
/// 落后于当前值时按 full rebuild 处理该文件。
///
/// | rev | 变更 |
/// |-----|------|
/// | 1   | 基线 |
/// | 2   | Claude assistant 行的顶层 `effort` 开始被提取（此前整条链路丢弃） |
/// | 3   | 每条事件带上 [`EventKey`]（记录指纹 + 记录内槽位），ADR-044 决定 4 |
///
/// 🔴 rev 3 是必须的，不是形式（评审 [P1]）：加 `EventKey` 时若不提版本，从 rev 2
/// 升上来的安装会认为既有文件**没过期**，只扫增量尾巴 —— 于是绝大多数历史事件永远
/// `event_key = None`，而 `EvidenceRefV2` 正是靠它定位。表现是「新写的记忆能核实、
/// 老记忆全部不可核实」，且不报任何错。
///
/// 判据就是本常量文档第一句：**从同一份字节里能提取出什么发生了变化** —— 加一个新
/// 字段正是这种变化。
pub const PARSER_REVISION: u32 = 4;

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
    /// 注入给 `normalize_cwd` 的默认 WSL 发行版：Windows 宿主上把 distro 未知的裸
    /// Linux cwd 打成 `wsl:<distro>:..` 而非泛 `wsl`。WSL 来源取自身发行版；本地来源
    /// 一般为 None（除非宿主只有一个用户发行版，见 `wsl::default_distro`）。
    pub default_distro: Option<String>,
    /// 已知项目根的注册表 —— **归属的唯一输入**（ADR-050 步 3）。
    ///
    /// 🔴 **空注册表是合法输入，含义是「一个根都不知道」** ⇒ 每条路径归 `Unattributed`。
    /// 它**不是**「退回旧的 `resolve_project_root`」：那条路会在发现失效时静默给出
    /// 另一个答案（把 cwd 当根），而这一层的契约是「说不出来就说不出来」。
    /// 所以本字段无 `Option`，也没有任何回退分支。
    ///
    /// 用 `Arc` 而不是引用：`ParseCtx` 每文件构造一次并在 `parse_lines` 里长期借出，
    /// 加生命周期参数会污染整条调用链；克隆一个 `Arc` 是一次原子自增。
    pub roots: Arc<RootRegistry>,
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
            event_key: None,
            parent_ref: None,
            content_hash: None,
            artifact_kind: None,
            observed_at: None,
            message_id: None,
            request_id: None,
        }
    }
}

/// 一条源记录（JSONL 的一行）内的槽位分配器。
///
/// 🔴 **按 `event_type` 分别计数**，不是一个总计数。这一条就是 [`EventKey`] 稳定而 `seq`
/// 不稳定的全部差别：`seq` 数「本次产出的第几条」，所以在既有事件**之前**新增一种类型
/// 会把其后所有编号推走；按类型计数则各算各的，新增一种类型不影响任何既有类型的编号。
struct RecordSlots {
    fingerprint: String,
    /// `(event_type, 已分配数)`。类型只有个位数，线性扫比 HashMap 便宜且无需 Hash 约束。
    counts: Vec<(EventType, u32)>,
}

impl RecordSlots {
    fn new(record: &str) -> Self {
        Self {
            fingerprint: EventKey::fingerprint_of(record),
            counts: Vec::new(),
        }
    }

    /// 取这条记录里下一个该类型的槽位。
    fn next(&mut self, event_type: EventType) -> EventKey {
        let ordinal = match self.counts.iter_mut().find(|(ty, _)| *ty == event_type) {
            Some((_, n)) => {
                *n += 1;
                *n - 1
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

        let mut slots = RecordSlots::new(line);
        let session_id = extract_claude_session_id(&value)
            .map(str::to_string)
            .unwrap_or_else(|| fallback.clone());
        let cwd = value.get("cwd").and_then(Value::as_str).map(str::to_string);
        let occurred_at = value
            .get("timestamp")
            .and_then(Value::as_str)
            .map(str::to_string);
        let pr = resolve_cached(
            &mut cache,
            cwd.as_deref(),
            ctx.host,
            ctx.default_distro.as_deref(),
            &ctx.roots,
        );

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
            // 槽位取自  本身，不重复写字面量 —— 六个发射点里有一个
            // 用的是变量类型（codex 的 message 分支），照抄字面量会静默配错槽位。
            ev.event_key = Some(slots.next(ev.event_type));
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
            // 槽位取自  本身，不重复写字面量 —— 六个发射点里有一个
            // 用的是变量类型（codex 的 message 分支），照抄字面量会静默配错槽位。
            ev.event_key = Some(slots.next(ev.event_type));
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
            ev.effort = u.effort;
            ev.usage = Some(u.usage);
            ev.message_id = u.message_id;
            ev.request_id = u.request_id;
            // 槽位取自  本身，不重复写字面量 —— 六个发射点里有一个
            // 用的是变量类型（codex 的 message 分支），照抄字面量会静默配错槽位。
            ev.event_key = Some(slots.next(ev.event_type));
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
    effort: Option<String>,
    usage: TokenUsage,
    message_id: Option<String>,
    request_id: Option<String>,
}

/// usage 提取；mirror `parse_claude_jsonl_entry`（但不因缺时间戳而丢弃）。
///
/// 🔴 `effort` 在 assistant 行的**顶层**，与 `requestId` 同层，**不在 `message`
/// 里** —— 和 Codex 把它埋进 `turn_context` 的形状不同，所以不能套用
/// `extract_codex_effort`。
///
/// 这个字段此前被整条链路丢弃：本函数不读，QuotaBar 的回退解析器则硬编码
/// `effort: None`，注释还写着「Anthropic JSONL doesn't record reasoning effort
/// today」。那句话写的时候大概是真的，之后 Claude Code 加了 `/effort` 而没有
/// 任何东西会因此报错 —— 又一次「注释断言外部世界，世界变了却无人知道」。
///
/// 取值不枚举：实测样本里有 `max` / `xhigh` / `high`，且 `max` 是 Claude 独有
/// （Codex 只有 low/medium/high/xhigh）。任何非空字符串都原样透传，展示层按
/// effective token 排序，未来新增档位不需要改解析器。
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
        effort: value
            .get("effort")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        usage: TokenUsage {
            input: read_u64(usage, "input_tokens"),
            output: read_u64(usage, "output_tokens"),
            cache_creation: read_u64(usage, "cache_creation_input_tokens"),
            cache_read: read_u64(usage, "cache_read_input_tokens"),
        },
        message_id: message
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string),
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

        let mut slots = RecordSlots::new(line);
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
        let pr = resolve_cached(
            &mut cache,
            cwd.as_deref(),
            ctx.host,
            ctx.default_distro.as_deref(),
            &ctx.roots,
        );

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
                // 槽位取自  本身，不重复写字面量 —— 六个发射点里有一个
                // 用的是变量类型（codex 的 message 分支），照抄字面量会静默配错槽位。
                ev.event_key = Some(slots.next(ev.event_type));
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
                // 槽位取自  本身，不重复写字面量 —— 六个发射点里有一个
                // 用的是变量类型（codex 的 message 分支），照抄字面量会静默配错槽位。
                ev.event_key = Some(slots.next(ev.event_type));
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
                // 槽位取自  本身，不重复写字面量 —— 六个发射点里有一个
                // 用的是变量类型（codex 的 message 分支），照抄字面量会静默配错槽位。
                ev.event_key = Some(slots.next(ev.event_type));
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
        .or_else(|| {
            v.get("model_slug")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
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
            let name = item
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
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

/// 归属工程根（带按原始 cwd 缓存）。先把原始 cwd 过 [`pathnorm::normalize_cwd`]
/// 归一到规范形，再交给 [`attribute`] 对着注册表做**纯字符串**最长匹配。
///
/// 🔴 **ADR-050 步 3：这里不再做 I/O。** 从前是 `project_root::resolve_project_root`
/// —— 它用「我此刻能不能 stat 这个路径」决定「要不要回答归属问题」，stat 不了就把
/// cwd 原样当答案交出去。实测后果：71.4% 的事件（`wsl_cwd`）从没做过项目根解析，
/// 同一个项目被记成 11 个 `project_root`。现在**发现**（慢、可 I/O、允许失败）在别处
/// 先跑完，这里只做归属（快、纯函数、必然有答案 —— 含「说不出来」那一种）。
///
/// `default_distro` 由调用方注入（WSL 来源取其自身发行版，见 `scan`）：有值时裸 Linux
/// cwd 在 Windows 宿主上被打成精确 `wsl:<distro>`，无值则回落泛 `wsl`；UNC 路径恒能精确还原。
fn resolve_cached(
    cache: &mut Option<(String, ProjectRoot)>,
    cwd: Option<&str>,
    host: HostPlatform,
    default_distro: Option<&str>,
    roots: &RootRegistry,
) -> Option<ProjectRoot> {
    let cwd = cwd?;
    if let Some((c, pr)) = cache.as_ref() {
        if c == cwd {
            return Some(pr.clone());
        }
    }
    let normalized = pathnorm::normalize_cwd(Some(cwd), host, default_distro);
    let pr = project_root_of(attribute(normalized.as_deref(), roots));
    *cache = Some((cwd.to_string(), pr.clone()));
    Some(pr)
}

/// [`Attribution`] → [`ProjectRoot`]（落库形态）。
///
/// 🔴 **`Unattributed` 必须在 `source` 上留下痕迹。** 它的 `path` 走
/// [`Attribution::storage_path`]（总得往 `project_root` 列里写点什么，粗粒度查询要它），
/// 但 `source` 记成 `unattributed` —— 否则「归到了一个根」与「没归到、拿原路径顶上」
/// 在库里长得一模一样，而那正是本 ADR 要消灭的东西。
fn project_root_of(a: Attribution) -> ProjectRoot {
    let source = match &a {
        Attribution::Root { source, .. } => source.as_str().to_string(),
        Attribution::Unattributed { .. } => "unattributed".to_string(),
        Attribution::NoPath => "missing_cwd".to_string(),
    };
    ProjectRoot {
        path: a.storage_path().map(std::path::PathBuf::from),
        source,
    }
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
            default_distro: None,
            roots: Arc::new(RootRegistry::new()),
        }
    }

    /// 显式宿主 + default_distro 的构造（路径归属断言用，避免依赖运行环境）。
    fn ctx_host(
        source_type: SourceType,
        host: HostPlatform,
        default_distro: Option<&str>,
    ) -> ParseCtx {
        ParseCtx {
            source_type,
            source_location: SourceLocation::Local,
            source_path: "/tmp/abc-session.jsonl".to_string(),
            profile: Profile::Metadata,
            host,
            default_distro: default_distro.map(str::to_string),
            roots: Arc::new(RootRegistry::new()),
        }
    }

    /// 🔴 Claude 的 `effort` 曾被整条链路丢弃。
    ///
    /// 它在 assistant 行的**顶层**（与 `requestId` 同层），不在 `message` 里；
    /// 本函数此前根本不读它，而 QuotaBar 的回退解析器硬编码 `effort: None`，
    /// 注释还写着 Anthropic 不记录它 —— 那句话曾经是真的。
    ///
    /// 断言用 `max`：实测样本里最常见的取值，且是 **Claude 独有**（Codex 只有
    /// low/medium/high/xhigh），所以任何「照抄 Codex 枚举」的实现都会在这里红。
    #[test]
    fn claude_usage_carries_top_level_effort() {
        let line = serde_json::json!({
            "type": "assistant",
            "sessionId": "sess-effort",
            "timestamp": "2026-08-04T10:00:00.000Z",
            "requestId": "req_e",
            "effort": "max",
            "message": {
                "id": "msg_e",
                "role": "assistant",
                "model": "claude-opus-5",
                "usage": {"input_tokens": 1, "output_tokens": 2,
                          "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0}
            }
        })
        .to_string();
        let out = parse_lines(
            &ctx(SourceType::ClaudeCode, Profile::Full),
            &[line.as_str()],
            0,
            None,
        );
        let usage = out
            .events
            .iter()
            .find(|e| e.event_type == EventType::Usage)
            .expect("usage event");
        assert_eq!(usage.effort.as_deref(), Some("max"));
    }

    /// 没有 `effort` 的行必须留 `None`，不能落成空串 —— 下游按「有没有标注」
    /// 分母，空串会把未标注的行算进已标注里。
    #[test]
    fn claude_usage_without_effort_stays_none() {
        for line in [
            serde_json::json!({
                "type": "assistant", "sessionId": "s", "timestamp": "2026-08-04T10:00:00.000Z",
                "message": {"id": "m", "role": "assistant", "model": "claude-opus-5",
                            "usage": {"input_tokens": 1, "output_tokens": 1,
                                      "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0}}
            }),
            // 空串等同于缺失。
            serde_json::json!({
                "type": "assistant", "sessionId": "s", "timestamp": "2026-08-04T10:00:00.000Z",
                "effort": "",
                "message": {"id": "m", "role": "assistant", "model": "claude-opus-5",
                            "usage": {"input_tokens": 1, "output_tokens": 1,
                                      "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0}}
            }),
        ] {
            let s = line.to_string();
            let out = parse_lines(
                &ctx(SourceType::ClaudeCode, Profile::Full),
                &[s.as_str()],
                0,
                None,
            );
            let usage = out
                .events
                .iter()
                .find(|e| e.event_type == EventType::Usage)
                .expect("usage event");
            assert_eq!(usage.effort, None, "line: {s}");
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
        let out = parse_lines(
            &ctx(SourceType::ClaudeCode, Profile::Full),
            &[line.as_str()],
            0,
            None,
        );
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
        assert_eq!(
            (u.input, u.output, u.cache_creation, u.cache_read),
            (100, 50, 5, 20)
        );
        assert_eq!(usage.message_id.as_deref(), Some("msg_1"));
        assert_eq!(usage.request_id.as_deref(), Some("req_1"));
    }

    #[test]
    fn workspace_location_populated_from_cwd() {
        // UNC cwd → 规范化 → 归属 → workspace_location = wsl:<distro>。
        // 这条断言锁住 cwd → normalize_cwd → attribute → workspace_location 全链路。
        //
        // 🔴 ADR-050 步 3 起 source 是 `unattributed` 而不是旧的 `wsl_cwd`：
        // 后者的含义正是「我 stat 不了这个路径，所以拒绝回答、拿 cwd 顶上」，
        // 而那个「拒绝」从前在库里与「归到了一个根」长得一模一样。
        let unc = serde_json::json!({
            "type": "user",
            "sessionId": "s",
            "cwd": r"\\wsl$\Ubuntu\home\me\proj",
            "message": {"role": "user", "content": "hi"}
        })
        .to_string();
        let out = parse_lines(
            &ctx(SourceType::ClaudeCode, Profile::Full),
            &[unc.as_str()],
            0,
            None,
        );
        let ev = &out.events[0];
        assert_eq!(ev.project_root_source.as_deref(), Some("unattributed"));
        assert_eq!(ev.workspace_location.as_deref(), Some("wsl:Ubuntu"));
        // 归不到根**不丢路径** —— 粗粒度查询还要它。
        assert_eq!(ev.project_root.as_deref(), Some("wsl:Ubuntu:/home/me/proj"));

        // /mnt/<drive> 是挂载的 Windows 盘 → local（不被误标 wsl）。
        let mnt = serde_json::json!({
            "type": "user",
            "sessionId": "s",
            "cwd": "/mnt/c/code/proj",
            "message": {"role": "user", "content": "hi"}
        })
        .to_string();
        let out = parse_lines(
            &ctx(SourceType::ClaudeCode, Profile::Full),
            &[mnt.as_str()],
            0,
            None,
        );
        assert_eq!(out.events[0].workspace_location.as_deref(), Some("local"));
    }

    #[test]
    fn default_distro_upgrades_bare_linux_to_precise_wsl() {
        // Windows 宿主 + 已知 default_distro：distro 未知的裸 /home cwd 被精确打标，
        // workspace_location 升级成 wsl:<distro> 而非泛 wsl。
        let line = serde_json::json!({
            "type": "user",
            "sessionId": "s",
            "cwd": "/home/me/proj",
            "message": {"role": "user", "content": "hi"}
        })
        .to_string();

        let with = parse_lines(
            &ctx_host(
                SourceType::ClaudeCode,
                HostPlatform::Windows,
                Some("Ubuntu"),
            ),
            &[line.as_str()],
            0,
            None,
        );
        assert_eq!(
            with.events[0].workspace_location.as_deref(),
            Some("wsl:Ubuntu")
        );
        assert_eq!(
            with.events[0].project_root_source.as_deref(),
            Some("unattributed"),
            "注册表为空 ⇒ 说不出来，而不是拿 cwd 冒充项目根"
        );

        // 无 default_distro：回落泛 wsl（仍不做错盘本地上溯，P2 修复仍生效）。
        let without = parse_lines(
            &ctx_host(SourceType::ClaudeCode, HostPlatform::Windows, None),
            &[line.as_str()],
            0,
            None,
        );
        assert_eq!(without.events[0].workspace_location.as_deref(), Some("wsl"));
        assert_eq!(
            without.events[0].project_root_source.as_deref(),
            Some("unattributed")
        );
    }

    /// 🔴 上面两条都是「空注册表 ⇒ 说不出来」。这条是**另一半**：注册表里有根时
    /// 必须真的归上去 —— 否则把 `attribute` 换成一个恒返回 `Unattributed` 的桩，
    /// 那两条照样全绿，而整个步 3 等于没做。
    #[test]
    fn a_known_root_actually_attributes_and_collapses_subdirs() {
        let mut reg = RootRegistry::new();
        reg.insert(
            "wsl:Ubuntu:/home/me/proj",
            crate::attribution::RootSource::Git,
        );
        let roots = Arc::new(reg);

        let mut seen = Vec::new();
        for cwd in [
            r"\\wsl$\Ubuntu\home\me\proj",
            r"\\wsl$\Ubuntu\home\me\proj\docs",
            r"\\wsl$\Ubuntu\home\me\proj\src\deep\er",
        ] {
            let line = serde_json::json!({
                "type": "user",
                "sessionId": "s",
                "cwd": cwd,
                "message": {"role": "user", "content": "hi"}
            })
            .to_string();
            let mut c = ctx(SourceType::ClaudeCode, Profile::Full);
            c.roots = roots.clone();
            let out = parse_lines(&c, &[line.as_str()], 0, None);
            let ev = &out.events[0];
            assert_eq!(ev.project_root_source.as_deref(), Some("git"), "cwd={cwd}");
            seen.push(ev.project_root.clone().unwrap_or_default());
        }
        // 三个子目录塌成**同一个** project_root —— 这正是 ADR 要治的
        // 「同一个项目被记成 11 个 root」。
        assert_eq!(
            seen.iter().collect::<std::collections::BTreeSet<_>>().len(),
            1
        );
        assert_eq!(seen[0], "wsl:Ubuntu:/home/me/proj");
    }

    #[test]
    fn metadata_profile_drops_content() {
        let line = serde_json::json!({
            "type": "user",
            "sessionId": "s",
            "message": {"role": "user", "content": "secret text"}
        })
        .to_string();
        let out = parse_lines(
            &ctx(SourceType::ClaudeCode, Profile::Metadata),
            &[line.as_str()],
            0,
            None,
        );
        assert_eq!(out.events.len(), 1);
        assert_eq!(out.events[0].event_type, EventType::Message);
        assert_eq!(out.events[0].content, None, "metadata 档不带正文");
        assert_eq!(
            out.events[0].time_confidence,
            TimeConfidence::Low,
            "无时间戳→low"
        );
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
        let out = parse_lines(
            &ctx(SourceType::ClaudeCode, Profile::Full),
            &[line.as_str()],
            0,
            None,
        );
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
        assert_eq!(
            usages.len(),
            2,
            "两个 session 各出一条 usage（无重置则第二条会被减成 0 而丢失）"
        );
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
        let out = parse_lines(
            &ctx(SourceType::ClaudeCode, Profile::Full),
            &[line.as_str()],
            42,
            None,
        );
        assert_eq!(out.events[0].seq, 42, "seq 从 base_seq 起");
    }

    /// 🔴 **护栏 G1（ADR-044 决定 4）：解析器在既有事件之前新增一种类型时，既有
    /// `EventKey` 必须一字不变 —— 而 `seq` 会全部漂移。**
    ///
    /// 这是整条 EventKey 存在的理由，也是评审推翻「用 `(identity, seq)` 当身份」那个
    /// 结论的依据。`seq` 数的是「本次产出的第几条」，所以任何前插都会把其后编号推走；
    /// 一次实测「升级前后 seq 一致」只能说明那次恰好没改变事件组成。
    ///
    /// 🔴 **测试的形状很关键**：不能只断言「同一份输入两次解析结果相同」——那对 `seq`
    /// 也成立，测了等于没测。必须**模拟一次会改变事件组成的升级**：这里直接用
    /// `RecordSlots` 走两遍同一条记录，第二遍在既有类型之前先分配一个新类型，
    /// 然后断言既有类型的槽位号没动。
    #[test]
    fn a_new_event_type_inserted_before_existing_ones_does_not_move_their_keys() {
        let record = r#"{"sessionId":"s","message":{"role":"assistant"}}"#;

        // 升级前：这条记录产出 Thinking、Message、Usage 各一条。
        let before: Vec<EventKey> = {
            let mut slots = RecordSlots::new(record);
            [EventType::Thinking, EventType::Message, EventType::Usage]
                .into_iter()
                .map(|ty| slots.next(ty))
                .collect()
        };

        // 升级后：新解析器学会了在最前面先产出一条 ToolUse。
        let after: Vec<EventKey> = {
            let mut slots = RecordSlots::new(record);
            [
                EventType::ToolUse, // ← 新增，插在最前
                EventType::Thinking,
                EventType::Message,
                EventType::Usage,
            ]
            .into_iter()
            .map(|ty| slots.next(ty))
            .collect()
        };

        assert_eq!(
            &after[1..],
            &before[..],
            "既有三种事件的 EventKey 必须完全不变；变了说明槽位是按产出顺序编的，\
             那和 seq 是同一个东西，等于什么都没修"
        );
        assert_eq!(
            after[0].slot_ordinal, 0,
            "新类型自己从 0 起，不占用别人的编号"
        );
    }

    /// 同一记录内**同类型**的多条各自递增；不同记录的同类型互不干扰。
    ///
    /// ⚠️ **覆盖边界，明写而不是假装**：这条测的是分配器本身。今天**没有任何解析器**会在
    /// 一条记录里产出同类型的两条事件（Claude 的多个 thinking 块被 `join("\n")` 成一条，
    /// Codex 的两个发射点分属不同 `response_item` 行），所以「每条事件都重建一次分配器」
    /// 这种退化在解析器层**不可观测** —— 变异验证时它确实没能变红，原因是没有可分辨的
    /// 输出，不是护栏失效。
    ///
    /// 一旦哪个解析器开始逐块发 thinking，`slot_ordinal` 立刻变得可观测，届时应补一条
    /// 解析器层的用例。在此之前，把边界写在这里比留一个看起来全覆盖的假象好。
    #[test]
    fn slots_count_per_type_and_reset_per_record() {
        let mut a = RecordSlots::new("line-a");
        assert_eq!(a.next(EventType::Message).slot_ordinal, 0);
        assert_eq!(a.next(EventType::Message).slot_ordinal, 1);
        assert_eq!(a.next(EventType::Usage).slot_ordinal, 0, "换类型从 0 起");
        assert_eq!(a.next(EventType::Message).slot_ordinal, 2);

        let mut b = RecordSlots::new("line-b");
        assert_eq!(b.next(EventType::Message).slot_ordinal, 0, "换记录从 0 起");
        assert_ne!(
            a.fingerprint, b.fingerprint,
            "不同记录必须有不同指纹，否则两条记录的槽位会互相冒充"
        );
    }

    /// 指纹只认记录的字节，不认它周围的空白 —— 解析器读的是 `trim()` 之后的串，
    /// 指纹若认原始串，行尾多一个空格就会让整行事件换身份。
    #[test]
    fn the_fingerprint_ignores_surrounding_whitespace() {
        let a = EventKey::fingerprint_of(r#"{"a":1}"#);
        let b = EventKey::fingerprint_of("  {\"a\":1}\t\n");
        assert_eq!(a, b);
        assert_ne!(a, EventKey::fingerprint_of(r#"{"a":2}"#));
        assert_eq!(a.len(), 16, "截断到 64 位；全长只是纯开销");
    }

    /// 端到端：真实解析出来的事件都带 key，且同一行内按类型分槽。
    #[test]
    fn parsed_events_carry_keys_scoped_to_their_record() {
        let ctx = ctx(SourceType::ClaudeCode, Profile::Full);
        let line = r#"{"sessionId":"s","cwd":"/w","timestamp":"2026-06-01T10:00:00Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"t"},{"type":"text","text":"a"}],"usage":{"input_tokens":1,"output_tokens":1,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}"#;
        let out = parse_lines(&ctx, &[line], 0, None);

        assert!(!out.events.is_empty());
        let expected = EventKey::fingerprint_of(line);
        for ev in &out.events {
            let key = ev.event_key.as_ref().expect("append-log 事件必须带 key");
            assert_eq!(key.record_fingerprint, expected);
            assert_eq!(key.version, EVENT_KEY_VERSION);
        }
        // 每种类型各自从 0 起。
        for ty in [EventType::Thinking, EventType::Message, EventType::Usage] {
            let ordinals: Vec<u32> = out
                .events
                .iter()
                .filter(|e| e.event_type == ty)
                .map(|e| e.event_key.as_ref().unwrap().slot_ordinal)
                .collect();
            if !ordinals.is_empty() {
                assert_eq!(
                    ordinals,
                    (0..ordinals.len() as u32).collect::<Vec<_>>(),
                    "{ty:?} 的槽位应是 0..n"
                );
            }
        }
    }
}
