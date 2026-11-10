//! `RawEvent` 归一化契约（§7）。
//!
//! 这是总库的不可变记录单元，也是 QuotaBar / TumeFlow 两个消费者共同认的 schema。
//! 字段对账见 `docs/rawevent-reconciliation.md`：已剔除 QuotaBar 暂未持久化的乐观字段
//! （正文 `content`、`parent_ref`、`time_confidence` 标为 greenfield，默认空/低置信）。

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// schema 版本。破坏性变更即 +1，并写入 TumeFlow 分库的复现戳。
///
/// v2：新增 [`EventKey`]（ADR-044 决定 4）。
pub const SCHEMA_VERSION: u32 = 2;

/// [`EventKey`] 编码本身的版本。与 [`SCHEMA_VERSION`] 分开，因为「键怎么编」和
/// 「DTO 有哪些字段」是两件会各自变化的事。
pub const EVENT_KEY_VERSION: u32 = 1;

/// 一条事件的**稳定身份** —— 跨解析器升级不变。
///
/// 🔴 为什么不能用 `seq`。`seq` 是「本次解析产出的第几条事件」，逐事件条件递增：
///
/// ```ignore
/// if let Some(text) = extract_claude_thinking(&value) { …; seq += 1; }
/// if let Some(u)    = extract_claude_usage(&value)    { …; seq += 1; }
/// ```
///
/// 下次升级只要**新增、删除或重排任一前序事件类型**，其后所有 `seq` 全部漂移。
/// 一次实测「升级前后 seq 一致」只能说明那一次恰好没改变事件的组成 —— 把它当契约，
/// 既有证据会在下次升级后**静默指向另一条事件**（不是失链，是指向错误内容）。
///
/// 稳定性来自两段各自不依赖产出顺序的坐标：
///
/// - `record_fingerprint` —— 产出这条事件的**源记录**（JSONL 的一行）的指纹。同一行
///   字节永远得到同一个指纹，与解析器怎么读它无关。
/// - `slot_ordinal` —— 该记录内、**同 `event_type`** 的第几条（从 0 起）。新增一种
///   事件类型不会改变既有类型的编号，这正是 `seq` 做不到的。
///
/// `None` 表示这条事件没有稳定身份（v1 遗留行、以及非 append-log 的快照类事件）。
/// 用 `Option<EventKey>` 而不是三个可空字段：要么整套齐备，要么没有 —— 「有指纹但
/// 没版本号」这种半拉状态从类型上就不存在。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventKey {
    pub version: u32,
    /// 源记录的指纹（截断的十六进制 SHA-256）。
    pub record_fingerprint: String,
    /// 该记录内同 `event_type` 的序号，从 0 起。
    pub slot_ordinal: u32,
}

impl EventKey {
    /// 一条源记录（JSONL 的一行）的指纹。
    ///
    /// 截断到 16 个十六进制字符（64 位）：一个文件里的记录数远小于生日界
    /// （约 2^32），而全长会给每条事件多加 48 字节 —— 实测 93 万条事件时那是 44 MB
    /// 的纯开销，换不来任何可分辨性。
    ///
    /// 对 `record.trim()` 求值，与解析器读它时用的是同一个串 —— 否则行尾空白的变化
    /// 会让同一条记录换指纹。
    pub fn fingerprint_of(record: &str) -> String {
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(record.trim().as_bytes());
        digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
    }
}

/// 数据来源 provider。新增 provider = 加一个枚举值 + 一个描述符 + 一个解析器。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceType {
    ClaudeCode,
    Codex,
    Cursor,
    Gemini,
    /// 通用 JSONL（未识别但结构可解析的来源族）。
    Jsonl,
}

/// 物理存放位置：本机本地，或某个 WSL 发行版内。
///
/// `as_key()` 给出参与去重唯一键的稳定字符串：`local` / `wsl:<distro>`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "distro")]
pub enum SourceLocation {
    Local,
    Wsl(String),
}

impl SourceLocation {
    pub fn as_key(&self) -> String {
        match self {
            SourceLocation::Local => "local".to_string(),
            SourceLocation::Wsl(distro) => format!("wsl:{distro}"),
        }
    }

    /// [`Self::as_key`] 的反向。认不出返回 `None`，不猜成 `Local`。
    pub fn from_key(key: &str) -> Option<Self> {
        match key {
            "local" => Some(SourceLocation::Local),
            _ => key
                .strip_prefix("wsl:")
                .filter(|d| !d.is_empty())
                .map(|d| SourceLocation::Wsl(d.to_string())),
        }
    }
}

/// 来源物理形态（ADR-025 保险①）。决定游标形态与增量读取策略。
///
/// 注意：`OpaqueFamily` 只用于 catalog/discover 层登记「保留来源族」，
/// **不产生 `RawEvent`**——因此挂在 `RawEvent.source_mode` 上的取值只会是
/// `{AppendLog, SnapshotFile, SqliteStore}` 三者之一（见 §7 与 `scan::scan_source`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceMode {
    /// 追加型日志（JSONL，只增不改）——字节偏移游标。
    AppendLog,
    /// 快照文件（整文件覆盖写）——指纹游标。
    SnapshotFile,
    /// SQLite 库——rowid 游标。
    SqliteStore,
    /// 已知来源族但实现未验证——仅登记、不增量、**不进 RawEvent**。
    OpaqueFamily,
}

/// 事件发起方。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Actor {
    User,
    Assistant,
    Tool,
    System,
}

/// 输入消息的真实发送方类别（H.1 / H.3）。
///
/// 🔴 `Actor::User` 只表示模型上下文里的 user 槽位，**不等于人类亲自输入或授权**。
/// 原生日志中的 `role=user`、正文里的自称标签、或未绑定的 `principal="human"`
/// 均不得自证为 [`Self::HumanVerified`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SenderKind {
    /// 经控制面受控入口绑定核实的人类直接输入，且未见同侪/派单/子会话注入特征。
    HumanVerified,
    /// 跨会话同侪（peer）消息（如 `<cross-session-message ...>`、`[xs id=...]`）。
    PeerSession,
    /// 控制面协调器派单或跨执行者显式交接（coordinator dispatch / handoff）。
    CoordinatorDispatch,
    /// 子会话（subagent / sidechain）或运行时合成注入的上下文提示。
    SubagentOrRuntime,
    /// 仅凭当前证据无法核实真实发送方（如普通未绑定 CLI 日志的 `role=user`，或信封残缺/冲突）。
    Unknown,
}

impl SenderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::HumanVerified => "human_verified",
            Self::PeerSession => "peer_session",
            Self::CoordinatorDispatch => "coordinator_dispatch",
            Self::SubagentOrRuntime => "subagent_or_runtime",
            Self::Unknown => "unknown",
        }
    }
}

/// 来源证据的可信度等级（H.1 / H.3）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputTrustLevel {
    /// 经控制面受控入口绑定核实的人类输入。
    HumanVerified,
    /// 经控制面绑定核实的协调器派单或跨执行者显式交接。
    ControlPlaneBound,
    /// 入站同侪或子代理消息：不可授予审批、不可放宽权限、不可改配置。
    PeerUntrusted,
    /// 仅来自本地日志观测、未经控制面绑定核实（或信封不完整）。
    Unverified,
}

impl InputTrustLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::HumanVerified => "human_verified",
            Self::ControlPlaneBound => "control_plane_bound",
            Self::PeerUntrusted => "peer_untrusted",
            Self::Unverified => "unverified",
        }
    }
}

/// 单条输入事件的真实发送方与消息级血缘证据（H.1 / H.3）。
///
/// 挂在 [`RawEvent::input_origin`] 上；非用户输入事件（如 `Assistant` / `Tool` / `Usage`）
/// 为 `None`。未知或不完整时显式标 `sender_kind = Unknown` / `trust_level = Unverified`，
/// **绝不伪造 `parent_ref`**。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputOrigin {
    pub sender_kind: SenderKind,
    pub trust_level: InputTrustLevel,
    /// 判定依据（如 `"xs_header"`、`"cross_session_envelope"`、`"control_plane_verified"`、
    /// `"log_role_only"`、`"cross_session_envelope_incomplete"` 等）。
    pub basis: String,
    /// 同侪发送方会话标识（如 `<cross-session-message from-session="...">`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender_session: Option<String>,
    /// 同侪发送方地址或管道标识（如 `from="..."` 或 `from-name="..."`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender_address: Option<String>,
    /// `xs` 跨会话消息 ID（如 `[xs id=...]` 中的 ID）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub xs_message_id: Option<String>,
    /// 控制面执行绑定 Assignment ID（如 `namespace:id`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignment_id: Option<String>,
    /// 跨执行者显式交接 Handoff ID（如 `namespace:id`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff_id: Option<String>,
}

impl InputOrigin {
    /// 该输入是否由上下文自动压缩产生（如 Claude `isCompactSummary: true`、
    /// Codex `compacted` 或 DSH `compact-checkpoint` / `surfaceOp: "replace"`）。
    ///
    /// 用途（G.1）：
    /// 1. 供下游切出「最近一次压缩后的当前有效上下文窗口（`VIEW`）」；
    /// 2. 防止记忆层把模型生成的二手摘要当作用户原话重复提炼；
    /// 3. 防止会话标题/首条意图预览被压缩长文占据。
    pub fn is_compaction_summary(&self) -> bool {
        matches!(
            self.basis.as_str(),
            "claude_compact_summary"
                | "claude_compact_boundary"
                | "codex_compacted"
                | "dsh_compaction_checkpoint"
                | "dsh_surface_replace"
        )
    }
}

/// 事件类型。`Usage` 是 QuotaBar 当前唯一持久化的类别；其余多为 TumeFlow 增量需求。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventType {
    Message,
    ToolUse,
    ToolResult,
    Usage,
    Meta,
    /// snapshot_file 内容指纹变化时产出（带新旧 hash）。
    ConfigSnapshot,
    /// 思考/推理块（Claude `thinking`、Codex `reasoning.summary[].text`）。
    ///
    /// 统一建模：`actor = Assistant`、`event_type = Thinking`（不另设 thinking actor，
    /// 与 QuotaBar 的 role=thinking 气泡对齐但归一到 actor/event_type 二维）。
    /// **opaque（明文不可得）**：Codex `encrypted_content` 等无明文场景，仍发 `Thinking`
    /// 事件但 `content = None`——表示「推理发生过、但无正文」，下游据此区分明文思考与加密思考。
    Thinking,
}

/// 时间置信度（§7）。有 `occurred_at` 判 `High`、无则 `Low`；后续可按来源进一步细化。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimeConfidence {
    High,
    Low,
}

/// token 计量（与 QuotaBar `UsageFactRow` 无损对齐：四段）。
///
/// Claude 直接取 `message.usage` 四字段；Codex 由累计 delta 拆分：
/// `cached = min(delta.cached, delta.input)`、`input = delta.input − cached`、
/// `cache_read = cached`、`cache_creation = 0`（Codex 无 creation 概念）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input: u64,
    pub output: u64,
    pub cache_creation: u64,
    pub cache_read: u64,
}

// ===========================================================================
// H.4 / H.7 细粒度用量保真证据（`docs/dsh-ingest-survey.md` §11）
// ===========================================================================

/// JavaScript `Number.MAX_SAFE_INTEGER`（$2^{53} - 1$），用于校验来自 JS runtime 的非负安全整数计量值。
pub const JS_MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

/// 原生计量样本在单条源记录中的采集位置。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UsageSampleOrigin {
    /// 来自外层消息/尝试记录的 `data.usage`（或顶层 `usage`）。
    OuterMessage,
    /// 来自内嵌流数组 `data.stream[stream_index].chunk.usage`。
    StreamChunk { stream_index: usize },
}

/// 单条原生计量样本记录（保留原始 JSON 载荷与采集位置）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageSampleRecord {
    pub origin: UsageSampleOrigin,
    pub raw_usage: Value,
}

/// 精确 `total_tokens` 的产生方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TotalTokenProvenance {
    /// 来源显式报告了合法 `totalTokens`，且与已知分桶不矛盾。
    Reported,
    /// 来源未显式报告 `totalTokens`，但四桶（`input`、`output`、`cache_read`、`cache_write`）
    /// 全部显式报告且合法，由四桶之和精确推导（`reasoning_tokens` 已含在 `output` 内，不重复加）。
    DerivedFromCompleteBuckets,
}

/// 单条计量尝试的可用状态（三态/多态区分：四桶精确 vs 仅 total 精确但 cache 未知 vs 残缺/矛盾/被覆盖）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageAvailability {
    /// 四桶（`input`、`output`、`cache_read`、`cache_write`）与精确 `total` 全部齐全且自洽；
    /// 仅此状态允许投影为 `EventType::Usage`（`TokenUsage` 四段）。
    ExactFourBucket,
    /// `input`、`output` 与显式报告的 `totalTokens` 合法且 `total >= known_sum`，
    /// 但至少一个 cache 桶未报告（`None`）：精确 total 与未知 cache **同时成立**，
    /// 绝不填 `0` 冒充已知，也不自行把差额猜入某个 cache 桶。
    ExactTotalWithUnknownCache,
    /// `input` 与 `output` 合法，但缺失至少一个 cache 桶且未报告 `totalTokens`，无法得出精确 total。
    PartialInsufficientForTotal,
    /// 出现负数、非整数、超过 `MAX_SAFE_INTEGER`、缺失必填 `input`/`output`、加和溢出、
    /// `total < known_sum`、四桶齐全但 `total != sum`、或 `reasoning > output` 等矛盾。
    InvalidOrContradictory,
    /// 尝试记录上缺失 `usage` 对象且无可采信的 `stream` usage chunk。
    MissingUsageReport,
    /// 所在 turn 缺失闭合（如无 `turn/end`）或存在未配对 `llm/retry`，生命周期不完整。
    IncompleteTurnLifecycle,
    /// 同一 `(turn, step)` 内未经配对 `llm/retry-started` 确认即被后一条尝试覆盖。
    SupersededBySameStep,
}

impl UsageAvailability {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ExactFourBucket => "exact_four_bucket",
            Self::ExactTotalWithUnknownCache => "exact_total_with_unknown_cache",
            Self::PartialInsufficientForTotal => "partial_insufficient_for_total",
            Self::InvalidOrContradictory => "invalid_or_contradictory",
            Self::MissingUsageReport => "missing_usage_report",
            Self::IncompleteTurnLifecycle => "incomplete_turn_lifecycle",
            Self::SupersededBySameStep => "superseded_by_same_step",
        }
    }
}

/// 计量不可直接作四桶精确结算或存在生命周期异常的具体诊断原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageDiagnosticReason {
    MissingUsageObject,
    MissingRequiredInputOrOutput,
    NegativeCount,
    UnsafeInteger,
    NonIntegerOrMalformedField,
    DerivedSumOverflow,
    TotalBelowKnownBuckets,
    TotalMismatchCompleteBuckets,
    ReasoningExceedsOutput,
    UnknownCacheWithoutTotal,
    MissingTurnEnd,
    UnpairedRetry,
    SupersededInSameStep,
}

/// 该计量样本在 `(turn, step)` 结算中的角色。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageSettlementRole {
    /// 被选为当前尝试（含经 `llm/retry-started` 确认的独立失败尝试，或最终 `assistant/message`）的结算样本。
    SelectedForSettlement,
    /// 同 `(turn, step)` 内被后一条尝试覆盖，不计入结算总额。
    SupersededSameStep,
    /// 因缺失、矛盾或未闭合仅作诊断保留，不计入精确结算总额。
    DiagnosticOnly,
}

/// 计量样本自身的模型路由归属状态。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum UsageRouteStatus {
    /// 该条尝试记录自身显式携带了 `provider` 和/或 `model`。
    Known {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
    },
    /// 该条尝试（如失败的 `assistant/attempt`）自身未携带模型信息；
    /// **严禁借用同 step 后续成功消息的 provider/model**。
    UnknownNotReported,
}

/// 计量样本所属会话的血缘状态（区分根会话、抑制继承前缀后的自有事件与未知父血缘）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum UsageLineageStatus {
    RootSession,
    SeededChildOwnEvent {
        parent_session_id: String,
        inherited_event_count: usize,
    },
    UnseededChildOwnEvent {
        parent_session_id: String,
    },
    ParentLineageUnknown {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_session_id: Option<String>,
    },
}

/// 计量样本所属会话的控制面执行绑定状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageExecutionBindingStatus {
    /// 命中持久控制面会话绑定（`SessionBindingRegistry`）。
    ControlPlaneBound,
    /// 未绑定控制面的普通持久会话。
    Unbound,
}

/// 单条计量尝试的细粒度保真证据（`docs/dsh-ingest-survey.md` §11 五项事实）。
///
/// 挂载在 [`RawEvent::usage_evidence`] 上：
/// - 当 `settlement_role == SelectedForSettlement` 且 `availability == ExactFourBucket` 时，
///   随 `EventType::Usage`（同时填 `usage: Some(TokenUsage)`）落库；
/// - 当属于 `ExactTotalWithUnknownCache`、`PartialInsufficientForTotal`、`InvalidOrContradictory`、
///   `MissingUsageReport`、`IncompleteTurnLifecycle` 或 `SupersededBySameStep` 时，
///   随 `EventType::Meta`（`usage: None`）落库——既保证 `TotalStore` 加密存储与 `open_read_only`
///   完整保留计量证据与诊断原因，又保证只认四段 `TokenUsage` 的消费方（如 QuotaBar）绝不把未知桶当 `0` 误计。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageEvidence {
    // --- (5) 计量归属 ---
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_instance: Option<String>,
    pub physical_generation: u32,
    pub native_seq: u64,
    /// `"assistant/message"` 或 `"assistant/attempt"`。
    pub source_event_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<u64>,
    /// 同一 `(turn, step)` 内的尝试序号（从 `0` 起算；每次经配对 `llm/retry-started` 后递增）。
    pub attempt_index: u32,
    pub settlement_role: UsageSettlementRole,
    pub route_status: UsageRouteStatus,
    pub lineage_status: UsageLineageStatus,
    pub execution_binding_status: UsageExecutionBindingStatus,

    // --- (1) 原生计量样本 ---
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_sample: Option<UsageSampleRecord>,
    /// 同一源记录内被外层 `data.usage` 或更晚 `stream` chunk 覆盖的流式 usage 样本。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub overridden_samples: Vec<UsageSampleRecord>,

    // --- (2) 逐字段值（`Some(0)` = 显式报告 0；`None` = 未报告/未知）---
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,

    // --- (3) 精确 total ---
    /// 来源原始报告的 `totalTokens`（若为非负安全整数则保留，即使与分桶矛盾也留作诊断证据）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reported_total_tokens: Option<u64>,
    /// 经过自洽校验后可采信的精确总 token 数（矛盾或无法推导时为 `None`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exact_total_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_provenance: Option<TotalTokenProvenance>,

    // --- (4) 可用状态与诊断 ---
    pub availability: UsageAvailability,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostic_reasons: Vec<UsageDiagnosticReason>,
}

/// 会话级细粒度用量保真汇总（区分四桶精确合计、含未知 cache 的精确 total 合计与各类异常计数）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionUsageSummary {
    pub total_evidence_records: usize,
    pub selected_for_settlement_count: usize,
    pub exact_four_bucket_count: usize,
    pub exact_total_with_unknown_cache_count: usize,
    pub partial_insufficient_for_total_count: usize,
    pub invalid_or_contradictory_count: usize,
    pub missing_usage_report_count: usize,
    pub incomplete_turn_lifecycle_count: usize,
    pub superseded_by_same_step_count: usize,
    pub unknown_route_settled_count: usize,
    /// 仅对 `SelectedForSettlement` 且 `ExactFourBucket` 的记录求和。
    pub exact_four_bucket_totals: TokenUsage,
    /// 对所有 `SelectedForSettlement` 且具备 `exact_total_tokens`（含 `ExactFourBucket` 与
    /// `ExactTotalWithUnknownCache`）的记录求和。
    pub settled_exact_total_tokens: u64,
    /// `SelectedForSettlement` 中是否存在任何 `cache_read_tokens.is_none()` 或 `cache_write_tokens.is_none()` 的记录。
    pub has_unknown_cache_buckets_in_settlement: bool,
}

/// 从一组 [`RawEvent`]（如 `TotalStore::open_read_only` 读出的会话事件）汇总细粒度用量证据。
pub fn summarize_session_usage_evidence(events: &[RawEvent]) -> SessionUsageSummary {
    let mut summary = SessionUsageSummary::default();
    for ev in events {
        let Some(ref u) = ev.usage_evidence else {
            continue;
        };
        summary.total_evidence_records += 1;
        match u.availability {
            UsageAvailability::ExactFourBucket => summary.exact_four_bucket_count += 1,
            UsageAvailability::ExactTotalWithUnknownCache => {
                summary.exact_total_with_unknown_cache_count += 1;
            }
            UsageAvailability::PartialInsufficientForTotal => {
                summary.partial_insufficient_for_total_count += 1;
            }
            UsageAvailability::InvalidOrContradictory => {
                summary.invalid_or_contradictory_count += 1;
            }
            UsageAvailability::MissingUsageReport => summary.missing_usage_report_count += 1,
            UsageAvailability::IncompleteTurnLifecycle => {
                summary.incomplete_turn_lifecycle_count += 1;
            }
            UsageAvailability::SupersededBySameStep => {
                summary.superseded_by_same_step_count += 1;
            }
        }
        if u.settlement_role == UsageSettlementRole::SelectedForSettlement {
            summary.selected_for_settlement_count += 1;
            if u.route_status == UsageRouteStatus::UnknownNotReported {
                summary.unknown_route_settled_count += 1;
            }
            if u.cache_read_tokens.is_none() || u.cache_write_tokens.is_none() {
                summary.has_unknown_cache_buckets_in_settlement = true;
            }
            if let Some(total) = u.exact_total_tokens {
                summary.settled_exact_total_tokens =
                    summary.settled_exact_total_tokens.saturating_add(total);
            }
            if u.availability == UsageAvailability::ExactFourBucket {
                summary.exact_four_bucket_totals.input = summary
                    .exact_four_bucket_totals
                    .input
                    .saturating_add(u.input_tokens.unwrap_or(0));
                summary.exact_four_bucket_totals.output = summary
                    .exact_four_bucket_totals
                    .output
                    .saturating_add(u.output_tokens.unwrap_or(0));
                summary.exact_four_bucket_totals.cache_read = summary
                    .exact_four_bucket_totals
                    .cache_read
                    .saturating_add(u.cache_read_tokens.unwrap_or(0));
                summary.exact_four_bucket_totals.cache_creation = summary
                    .exact_four_bucket_totals
                    .cache_creation
                    .saturating_add(u.cache_write_tokens.unwrap_or(0));
            }
        }
    }
    summary
}

/// 把 `occurred_at` 的原始串归一成 **UTC 毫秒**。无法解析 → `None`。
///
/// 🔴 为什么必须归一，而不是直接给原始串建索引排序：
///
/// - `2026-08-05T01:00:00Z` 与 `2026-08-05T09:00:00+08:00` 是**同一时刻**，字典序却
///   相差八小时；
/// - 小数秒位数不同（`.1` / `.123` / 无）会让同一秒内的顺序按位数而非按时间排；
/// - ADR-020 早就写明「`occurred_at` 统一 UTC unix 秒」——这不是新决定，是把一个
///   写下了却没实现的决定补上（本字段的注释自己都写着「归一到 UTC unix 秒是后续细化」）。
///
/// 支持 `YYYY-MM-DD` + `T`/空格 + `HH:MM[:SS[.frac]]` + 可选时区（`Z` / `±HH:MM` /
/// `±HHMM` / `±HH`）。无时区视为 UTC —— 来源都是 agent 自己写的日志，实测全是 `Z`。
///
/// 不引 chrono：这是一个有界的、可穷举测试的转换，而本仓一贯 stdlib 优先。
pub fn occurred_at_unix_ms(raw: &str) -> Option<i64> {
    let s = raw.trim();
    let bytes = s.as_bytes();
    if bytes.len() < 16 {
        return None;
    }
    let num = |a: usize, b: usize| -> Option<i64> { s.get(a..b)?.parse::<i64>().ok() };
    if bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    let (year, month, day) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    if !matches!(bytes[10], b'T' | b't' | b' ') || bytes[13] != b':' {
        return None;
    }
    let (hour, minute) = (num(11, 13)?, num(14, 16)?);
    let mut idx = 16;
    let mut second = 0i64;
    if bytes.get(idx) == Some(&b':') {
        second = num(idx + 1, idx + 3)?;
        idx += 3;
    }
    let mut millis = 0i64;
    if bytes.get(idx) == Some(&b'.') || bytes.get(idx) == Some(&b',') {
        idx += 1;
        let start = idx;
        while idx < bytes.len() && bytes[idx].is_ascii_digit() {
            idx += 1;
        }
        // 只取前三位，不足补零 —— 纳秒精度的输入不该因为多几位就解析失败。
        let frac = s.get(start..idx)?;
        let head: String = frac.chars().chain("000".chars()).take(3).collect();
        millis = head.parse().ok()?;
    }
    let offset_minutes = match bytes.get(idx) {
        None => 0,
        Some(b'Z') | Some(b'z') => 0,
        Some(sign @ (b'+' | b'-')) => {
            let rest = s.get(idx + 1..)?;
            let digits: String = rest.chars().filter(char::is_ascii_digit).collect();
            let (oh, om) = match digits.len() {
                2 => (digits.parse::<i64>().ok()?, 0),
                4 => (digits[..2].parse().ok()?, digits[2..].parse().ok()?),
                _ => return None,
            };
            let magnitude = oh * 60 + om;
            if *sign == b'+' {
                magnitude
            } else {
                -magnitude
            }
        }
        Some(_) => return None,
    };
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    if hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let days = days_from_civil(year, month, day);
    let secs = days * 86_400 + hour * 3_600 + minute * 60 + second - offset_minutes * 60;
    Some(secs * 1_000 + millis)
}

/// 民用日历 → 自 1970-01-01 起的天数（Howard Hinnant 的 `days_from_civil`）。
/// 纯整数算术，闰年/世纪规则全在里面，不需要日期库。
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// 归一化事件。去重唯一键见 `dedup_key()`。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawEvent {
    pub schema_version: u32,

    // --- 来源定位（参与去重唯一键）---
    pub source_type: SourceType,
    pub source_location: SourceLocation,
    /// 转录文件物理路径（transcript 存放处，非工程目录）。
    pub source_path: String,
    pub source_session_id: String,
    /// 文件内单调序号（行号 / 解析序）。**只做排序，不是身份** —— 见 [`EventKey`]。
    pub seq: u64,
    /// 稳定身份 —— **凡是要活过解析器升级的引用都必须用它，不能用 `seq`**。
    ///
    /// `None`：v1 遗留事件，以及非 append-log 的快照类事件（它们没有「源记录内的槽位」
    /// 这个概念）。见 [`EventKey`]。
    #[serde(default)]
    pub event_key: Option<EventKey>,

    // --- 来源形态 ---
    pub source_mode: SourceMode,

    // --- 工程定位 ---
    /// 对话记录里的原始 cwd（provenance）。
    pub cwd: Option<String>,
    /// 归属到的工程根（`attribution::attribute`，ADR-050）。
    ///
    /// 归不到根时**仍然是那条原始路径**（`Attribution::storage_path`）—— 下游要能按它
    /// 做粗粒度查询。所以「归到了」与「没归到」只能靠下面那个字段分辨。
    pub project_root: Option<String>,
    /// 归属判定依据：`git` / `marker` / `scan` / `configured` / `unattributed` /
    /// `missing_cwd`。
    ///
    /// 🔴 ADR-050 换过一次取值集合：旧的 `cwd` / `wsl_cwd` 表示「stat 不了这条路径，
    /// 所以拿 cwd 当答案」，现在那种情况一律是 `unattributed` —— **说不出来要说出来**。
    /// `marker:<file>` 那种带后缀的形式也随之退役（发现层只记「是 marker」，不记哪一个）。
    pub project_root_source: Option<String>,
    /// 工程物理位置（`local` | `wsl:<distro>`）与 transcript 存储位置的二分。
    /// 由 `pathnorm::workspace_location` 据规范化后的 project_root + 宿主平台判定
    /// （见 `pathnorm` 模块的三层分离说明）；cwd 缺失时为 None。
    pub workspace_location: Option<String>,

    // --- 事件语义 ---
    pub event_type: EventType,
    pub actor: Option<Actor>,
    /// 对话内时间（原始时间戳字符串，多为 ISO8601）；latest-wins 唯一权威，非入库顺序 / offset。
    /// v0 存原始串，归一到 UTC unix 秒是后续细化。
    pub occurred_at: Option<String>,
    pub time_confidence: TimeConfidence,

    pub model: Option<String>,
    /// Codex 推理 effort（low/medium/high/…）；Claude 当前无，恒 None。
    pub effort: Option<String>,
    pub usage: Option<TokenUsage>,

    // --- greenfield（QuotaBar 暂未持久化）---
    /// 正文：QuotaBar 不落盘（仅 ephemeral）。`metadata` profile 下恒为 None。
    pub content: Option<String>,
    /// 父事件引用（线程/分支重建）——greenfield。
    pub parent_ref: Option<String>,

    // --- snapshot_file-only ---
    /// 快照正文的 SHA-256；用于变更检测与下游版本证据，不是语义时间。
    #[serde(default)]
    pub content_hash: Option<String>,
    /// `memory` / `rules` / `instruction`；append_log 为 None。
    #[serde(default)]
    pub artifact_kind: Option<String>,
    /// SessionVault 观测到该快照版本的时间（Unix 秒字符串）。它不是文件的
    /// 真实生效时间，因此 `time_confidence` 保持 low、`occurred_at` 不伪造。
    #[serde(default)]
    pub observed_at: Option<String>,
    /// 快照源文件的 **mtime**（Unix 秒字符串）。`None` = 这一轮**没问出来**
    /// （stat 失败 / WSL 桥不可用 / 平台不支持），**不是**「这个文件没有 mtime」。
    ///
    /// 🔴 **它与 [`Self::observed_at`] 不是二选一，是互补的**，因为它们分得开的
    /// 东西不同（实测，消费者侧 206 个快照）：
    ///
    /// | | 它说什么 | 分得开什么 | 分不开什么 |
    /// | --- | --- | --- | --- |
    /// | `observed_at` | 我**什么时候看见**这个内容版本 | 扫描批次之间 | **批次之内** —— 首次上线时一次收进来的存量文件全挤在同一秒（实测 110 个取值 / 45.8 天，而 **60% 与别人同秒**，最大一堆 34 个） |
    /// | `modified_at` | 这个文件**什么时候被写的** | 存量文件彼此 | 被 `git checkout` / 复制**清掉过**的那些 |
    ///
    /// ⚠️ **仍然不写进 `occurred_at`。** 一个文件没有「事件时间」，而
    /// `recent_sessions` 按 `occurred_at_unix_ms` 排序且**不过滤 `config_snapshot`**
    /// —— 把 mtime 塞进去，每个快照文件都会作为一行「最近会话」参与排序，
    /// 而下游正是靠那个判「用户最近有没有活动」。**巩固写的就是这些文件**，
    /// 于是它会把自己的写入读成新活动。⇒ 独立字段，由消费者显式选择怎么用。
    #[serde(default)]
    pub modified_at: Option<String>,

    // --- Claude-only ---
    pub message_id: Option<String>,
    pub request_id: Option<String>,

    // --- 真实发送方与同侪/派单证据（H.1 / H.3）---
    /// 用户侧输入消息的真实发送方、可信度与消息级来源证据。
    ///
    /// 🔴 `actor == Some(Actor::User)` 只表示上下文里的 user 槽位，不等于人类亲自输入。
    /// 下游判断「是否人类亲自发出/授权」必须看此字段而非 `actor`。
    #[serde(default)]
    pub input_origin: Option<InputOrigin>,

    // --- 细粒度用量保真证据（H.4 / H.7）---
    /// 单次模型调用/尝试的细粒度 token 计量证据（五项事实：原生样本、逐字段 `Option<u64>`、
    /// 精确 total 及其来源、可用性三态/多态与诊断原因、计量归属）。
    ///
    /// - 四桶齐全且已结算（`ExactFourBucket`）时随 `EventType::Usage` 落库；
    /// - 仅 total 精确但 cache 未知（`ExactTotalWithUnknownCache`）或残缺/矛盾/被同 step 覆盖时
    ///   随 `EventType::Meta`（`usage: None`）落库，防止下游把未知桶静默当 `0` 计入四桶账本。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_evidence: Option<UsageEvidence>,
}

impl RawEvent {
    /// 去重唯一键：`(source_type, source_location, source_path, source_session_id, seq)`。
    pub fn dedup_key(&self) -> String {
        format!(
            "{:?}|{}|{}|{}|{}",
            self.source_type,
            self.source_location.as_key(),
            self.source_path,
            self.source_session_id,
            self.seq
        )
    }

    /// 这条事件的归属三项。
    pub fn attribution(&self) -> EventAttribution {
        EventAttribution {
            project_root: self.project_root.clone(),
            project_root_source: self.project_root_source.clone(),
            workspace_location: self.workspace_location.clone(),
        }
    }

    pub fn set_attribution(&mut self, a: EventAttribution) {
        self.project_root = a.project_root;
        self.project_root_source = a.project_root_source;
        self.workspace_location = a.workspace_location;
    }

    /// 该事件是否为上下文自动压缩产生的合成摘要或压缩边界检查点（G.1）。
    pub fn is_compaction_summary(&self) -> bool {
        self.input_origin
            .as_ref()
            .is_some_and(InputOrigin::is_compaction_summary)
    }
}

/// 返回事件序列中最近一次上下文压缩摘要/检查点的起始下标（G.1 有效上下文 `VIEW` 切点）。
///
/// 若序列中存在一处或多处 `ev.is_compaction_summary() == true`，返回**最后一条**的下标；
/// 若从未发生过压缩，返回 `0`（完整序列即为当前有效窗口）。
pub fn effective_view_start_index(events: &[RawEvent]) -> usize {
    events
        .iter()
        .rposition(RawEvent::is_compaction_summary)
        .unwrap_or(0)
}

/// 切出最近一次上下文压缩起算的当前有效事件窗口（G.1 `VIEW`）。
pub fn effective_view_slice(events: &[RawEvent]) -> &[RawEvent] {
    let start = effective_view_start_index(events);
    &events[start..]
}

/// 一条事件的归属三项：解析器按 `cwd` 一起写下（`parser::attribution_from_cwd`），
/// 按库重算归属时一起改（`TotalStore::recompute_attribution`）。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct EventAttribution {
    pub project_root: Option<String>,
    pub project_root_source: Option<String>,
    pub workspace_location: Option<String>,
}

#[cfg(test)]
// 测试要造 fixture（建目录、写文件、再核一遍），允许直接碰盘 —— 文件系统边界
// 管的是**生产行为**，而 `#[cfg(test)]` 不在生产路径上。
#[allow(clippy::disallowed_methods)]
mod time_tests {
    use super::occurred_at_unix_ms as ms;

    /// 🔴 同一时刻的两种写法必须归到同一个数 —— 而它们的**字典序相差八小时**。
    /// 这正是「直接给原始串建索引排序」会静默排错的地方。
    #[test]
    fn the_same_instant_in_two_zones_normalizes_to_one_number() {
        assert_eq!(ms("2026-08-05T01:00:00Z"), ms("2026-08-05T09:00:00+08:00"));
        assert_eq!(ms("2026-08-05T01:00:00Z"), ms("2026-08-04T20:00:00-05:00"));
        // 而字典序会把它们排反：
        assert!("2026-08-04T20:00:00-05:00" < "2026-08-05T01:00:00Z");
    }

    /// 小数秒位数不同不该改变同一秒内的先后。
    #[test]
    fn fractional_second_width_does_not_reorder() {
        let a = ms("2026-08-05T01:00:00.9Z").unwrap();
        let b = ms("2026-08-05T01:00:00.123Z").unwrap();
        assert!(b < a, "0.123s 早于 0.9s");
        // 字典序反过来："0.123" < "0.9" 成立纯属巧合；位数一变就翻车：
        assert!("2026-08-05T01:00:00.9Z" > "2026-08-05T01:00:00.123Z");
        assert_eq!(
            ms("2026-08-05T01:00:00.123456789Z"),
            Some(1_785_891_600_123)
        );
    }

    #[test]
    fn epoch_and_civil_calendar_edges() {
        assert_eq!(ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(ms("1969-12-31T23:59:59Z"), Some(-1_000));
        // 闰年与世纪规则（2000 是闰年，1900 不是）由 days_from_civil 保证。
        assert_eq!(
            ms("2000-03-01T00:00:00Z").unwrap() - ms("2000-02-28T00:00:00Z").unwrap(),
            2 * 86_400_000
        );
        assert_eq!(
            ms("2026-03-01T00:00:00Z").unwrap() - ms("2026-02-28T00:00:00Z").unwrap(),
            86_400_000
        );
    }

    #[test]
    fn accepts_the_shapes_real_logs_emit() {
        // 实测真库 4 万条采样全是这一种。
        assert!(ms("2026-06-01T10:00:02.123Z").is_some());
        assert!(ms("2026-06-01T10:00:02Z").is_some());
        assert!(ms("2026-06-01T10:00Z").is_some());
        assert!(ms("2026-06-01 10:00:02+0800").is_some());
        assert!(ms("  2026-06-01T10:00:02Z  ").is_some(), "两端空白应被容忍");
    }

    /// 🔴 解析不了就返回 None，**不猜**。一个猜出来的时间戳会让事件排到错误的位置，
    /// 而排序本身不会报错 —— 那正是本轮在修的那类缺陷。
    #[test]
    fn unparseable_input_is_none_not_a_guess() {
        for bad in [
            "",
            "not a time",
            "2026-13-01T00:00:00Z",
            "2026-01-32T00:00:00Z",
            "2026-01-01T25:00:00Z",
            "2026-01-01T00:61:00Z",
            "2026-01-01",
            "2026-01-01T00:00:00+9",
            "2026/01/01T00:00:00Z",
        ] {
            assert_eq!(ms(bad), None, "{bad:?} 不该被解析出时间");
        }
    }
}
