//! SessionVault — 共享摄取内核 + RawEvent 契约（骨架）。
//!
//! 设计契约见 `docs/INGEST_KERNEL.md`；字段对账见 `docs/rawevent-reconciliation.md`；
//! 日志规范见 `docs/LOGGING.md`（对齐 QuotaBar）。
//!
//! 本 crate 是 **lib + CLI(`svault`)** 双形态。lib 只发 `log` 事件、**不装 sink**
//! （ADR-026），由宿主（QuotaBar / `svault`）决定输出。
#![allow(dead_code)]

pub mod catalog;
pub mod cursor;
pub mod discover;
pub mod logging;
pub mod parser;
pub mod pathnorm;
pub mod project_root;
pub mod rawevent;
pub mod report;
pub mod scan;
pub mod wsl;

pub use catalog::{Artifact, Profile, ProviderDescriptor};
pub use cursor::{Cursor, CursorKind, ScanResult, ScanStatus};
pub use discover::SourceRef;
pub use pathnorm::HostPlatform;
pub use rawevent::{
    Actor, EventType, RawEvent, SourceLocation, SourceMode, SourceType, TimeConfidence,
};
pub use report::ScanReport;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("parse error: {0}")]
    Parse(String),
    #[error("unsupported provider: {0}")]
    UnsupportedProvider(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// §9 `catalog()`：返回内置 provider 描述符（后续接 user_config 覆盖）。
pub fn catalog() -> Vec<ProviderDescriptor> {
    catalog::builtin_descriptors()
}

/// §9 `discover()`：发现来源清单（骨架：仅本地内置 session 根）。
pub fn discover() -> Result<Vec<SourceRef>> {
    discover::discover_all()
}

/// §9 `scan()`：单来源增量扫描（按 source_mode 分派）。
pub fn scan(source: &SourceRef, cursor_in: Option<Cursor>, profile: Profile) -> ScanResult {
    scan::scan_source(source, cursor_in, profile)
}
