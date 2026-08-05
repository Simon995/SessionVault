//! `RawEvent` 归一化契约（§7）。
//!
//! 这是总库的不可变记录单元，也是 QuotaBar / TumeFlow 两个消费者共同认的 schema。
//! 字段对账见 `docs/rawevent-reconciliation.md`：已剔除 QuotaBar 暂未持久化的乐观字段
//! （正文 `content`、`parent_ref`、`time_confidence` 标为 greenfield，默认空/低置信）。

use serde::{Deserialize, Serialize};

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
    /// 解析出的工程根路径（`resolve_project_root`）。
    pub project_root: Option<String>,
    /// 工程根判定依据：git / marker:<file> / cwd / wsl_cwd / missing_cwd。
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

    // --- Claude-only ---
    pub message_id: Option<String>,
    pub request_id: Option<String>,
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
}
