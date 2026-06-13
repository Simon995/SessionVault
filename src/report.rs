//! 扫描报告（§10）。内核无头，但每轮产出结构化报告供宿主渲染 GUI。
//!
//! 报告 ≠ 日志：报告是「这一轮扫了什么」的结构化结果（进 stdout/返回值），
//! 日志是「过程发生了什么」的诊断流（进 stderr / 宿主 sink）。日志规范见 `docs/LOGGING.md`。

use serde::{Deserialize, Serialize};

/// 单来源报告。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SourceReport {
    pub source_path: String,
    /// 本轮新增事件数。
    pub events_emitted: u64,
    /// 本轮读取字节数。
    pub bytes_read: u64,
    /// 是否检测到截断/回退并触发全量重读。
    pub rollback_detected: bool,
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
