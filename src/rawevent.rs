//! `RawEvent` 归一化契约（§7）。
//!
//! 这是总库的不可变记录单元，也是 QuotaBar / TumeFlow 两个消费者共同认的 schema。
//! 字段对账见 `docs/rawevent-reconciliation.md`：已剔除 QuotaBar 暂未持久化的乐观字段
//! （正文 `content`、`parent_ref`、`time_confidence` 标为 greenfield，默认空/低置信）。

use serde::{Deserialize, Serialize};

/// schema 版本。破坏性变更即 +1，并写入 TumeFlow 分库的复现戳。
pub const SCHEMA_VERSION: u32 = 1;

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

/// 时间置信度（§7 greenfield）。骨架阶段一律 `Low`，待 occurred_at 来源细化后提升。
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
    /// 文件内单调序号（行号 / 解析序）。
    pub seq: u64,

    // --- 来源形态 ---
    pub source_mode: SourceMode,

    // --- 工程定位 ---
    /// 对话记录里的原始 cwd（provenance）。
    pub cwd: Option<String>,
    /// 解析出的工程根路径（`resolve_project_root`）。
    pub project_root: Option<String>,
    /// 工程根判定依据：git / marker:<file> / cwd / wsl_cwd / missing_cwd。
    pub project_root_source: Option<String>,
    /// 工程物理位置（local | wsl:<distro>）与 transcript 存储位置的二分——
    /// QuotaBar 有此分，但 WSL 项目记在 local transcript 下的检测是 greenfield，v0 恒 None。
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
