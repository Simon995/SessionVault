//! 多形态游标 + 扫描结果（§8 / ADR-025 保险②）。
//!
//! 游标随 `source_mode` 取不同形态；并携带截断/回退检测所需的 `(mtime, size)`。
//! Codex 累计 token 状态（input/cached/output）跨增量批次延续，单列在 `CodexState`。

use serde::{Deserialize, Serialize};

use crate::rawevent::RawEvent;
use crate::report::SourceReport;

/// 游标形态（与 `SourceMode` 对应）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CursorKind {
    ByteOffset,
    Fingerprint,
    SqliteRowid,
    /// 不可增量（opaque family / 快照首扫）。
    NoCursor,
}

/// Codex 累计用量状态。Codex 每条 usage 是「累计总量」，需减去上一条得增量。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexUsage {
    pub input: u64,
    pub cached: u64,
    pub output: u64,
}

/// 跨批次延续的 Codex 解析状态（模型/effort/cwd/session 在文件内可变）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CodexState {
    pub current_model: Option<String>,
    pub current_effort: Option<String>,
    pub current_cwd: Option<String>,
    pub current_session_id: Option<String>,
    /// 上一条累计用量；下一条减它得增量。
    pub previous_total: CodexUsage,
}

/// 单来源增量游标。按 `kind` 取用对应字段；其余为 None / 默认。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cursor {
    pub kind: CursorKind,

    // --- append_log ---
    /// 已安全消费到的字节偏移（只在「完整行」边界推进）。
    pub safe_offset: u64,
    /// 上轮文件大小，配合 mtime 检测截断/重写。
    pub size: u64,
    /// 上轮 mtime（秒），变小或 size 变小即判定回退 → 全量重读。
    pub mtime: Option<i64>,

    // --- snapshot_file ---
    pub content_hash: Option<String>,

    // --- sqlite_store ---
    pub last_rowid: Option<i64>,
    pub schema_fingerprint: Option<String>,

    // --- provider 私有 ---
    pub codex_state: Option<CodexState>,

    /// 下一条事件的 `seq` 起点（文件内单调，增量批次间延续）。回退重读时归零。
    pub next_seq: u64,
}

impl Cursor {
    /// 全新 append_log 游标（从头扫）。
    pub fn new_byte_offset() -> Self {
        Cursor {
            kind: CursorKind::ByteOffset,
            safe_offset: 0,
            size: 0,
            mtime: None,
            content_hash: None,
            last_rowid: None,
            schema_fingerprint: None,
            codex_state: None,
            next_seq: 0,
        }
    }
}

/// 单来源扫描状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScanStatus {
    Ok,
    /// 部分成功（如尾部半行留待下轮）。
    Partial,
    Error,
}

/// 单来源扫描结果：事件 + 新游标 + 报告。无状态内核的纯函数返回值。
#[derive(Debug, Clone)]
pub struct ScanResult {
    pub status: ScanStatus,
    pub events: Vec<RawEvent>,
    pub cursor_out: Cursor,
    pub report: SourceReport,
}
