//! 扫描报告（§10）。内核无头，但每轮产出结构化报告供宿主渲染 GUI。
//!
//! 报告 ≠ 日志：报告是「这一轮扫了什么」的结构化结果（进 stdout/返回值），
//! 日志是「过程发生了什么」的诊断流（进 stderr / 宿主 sink）。日志规范见 `docs/LOGGING.md`。

use serde::{Deserialize, Serialize};

use crate::cursor::CursorKind;
use crate::rawevent::SourceMode;

/// 单来源报告。字段分两类：**模式无关**（任何 source_mode 都有意义）+ **append_log 专属**（字节口径）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SourceReport {
    pub source_path: String,

    // --- 模式无关（snapshot_file / sqlite_store 同样自然）---
    /// 本来源形态。
    pub source_mode: Option<SourceMode>,
    /// 本轮用的游标形态。
    pub cursor_kind: Option<CursorKind>,
    /// 本轮检视的记录数（append_log=行数 / snapshot=1 / sqlite=取回行数）。
    pub items_examined: u64,
    /// 本轮新增事件数（= items_new，对所有形态成立）。
    pub events_emitted: u64,
    /// 本轮跳过的坏记录数（坏 JSON / schema 不符）。
    pub items_skipped: u64,
    /// snapshot_file：内容指纹是否变化（变化才产 config_snapshot）。
    pub fingerprint_changed: bool,
    /// sqlite_store：schema 指纹是否漂移（漂移即显式失败）。
    pub schema_changed: bool,
    /// 是否检测到截断/回退并触发全量重读。
    pub rollback_detected: bool,

    // --- append_log 专属（字节口径）---
    /// 本轮读取字节数。
    pub bytes_read: u64,
    /// 尾部留待下轮的半行字节数。
    pub pending_tail_bytes: u64,

    /// 跳过/告警信息（非致命）。
    pub warnings: Vec<String>,
}

/// 单 provider 报告（聚合其下多个来源）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderReport {
    pub provider: String,
    pub sources_scanned: u64,
    pub events_emitted: u64,
    pub errors: u64,
    pub sources: Vec<SourceReport>,
}

/// 一轮全量增量扫描报告。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScanReport {
    pub providers: Vec<ProviderReport>,
    pub total_events: u64,
    pub total_errors: u64,
}
