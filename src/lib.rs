//! SessionVault — 共享摄取内核 + RawEvent 契约。
//!
//! 设计契约见 `docs/INGEST_KERNEL.md`；字段对账见 `docs/rawevent-reconciliation.md`；
//! 日志规范见 `docs/LOGGING.md`（对齐 QuotaBar）。
//!
//! 本 crate 是 **lib + CLI(`svault`)** 双形态。lib 只发 `log` 事件、**不装 sink**
//! （ADR-026），由宿主（QuotaBar / `svault`）决定输出。
#![allow(dead_code)]

pub mod attribution;
pub mod catalog;
pub mod cursor;
pub mod discover;
pub mod discovery;
pub mod identity;
pub mod logging;
pub mod parser;
pub mod pathnorm;
pub mod project_root;
pub mod rawevent;
pub mod report;
pub mod scan;
/// 不可变 RawEvent 总库（§13 / ADR-020）——`store` feature 门控（持久化组件，内核仍无状态）。
#[cfg(feature = "store")]
pub mod store;
#[cfg(feature = "store")]
mod store_crypto;
pub mod wsl;

pub use catalog::{Artifact, Profile, ProviderDescriptor};
pub use cursor::{Cursor, CursorKind, ScanResult, ScanStatus};
pub use discover::{ProjectSnapshotRoot, SourceRef};
pub use parser::PARSER_REVISION;
pub use pathnorm::HostPlatform;
pub use rawevent::{
    Actor, EventType, RawEvent, SourceLocation, SourceMode, SourceType, TimeConfidence,
};
pub use report::ScanReport;
#[cfg(feature = "store")]
pub use store::{
    AppendStats, EraseStats, FileProjectionBatch, GcStats, Projection, ProjectionChange,
    ProjectionStats, ReadPage, RecentSession, SessionRead, SnapshotSyncStats, SourceKey,
    StoreStatus, TombstoneScope, TotalStore,
};
#[cfg(feature = "store")]
pub use store_crypto::StoreKey;

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

/// §9 `discover()`：发现 transcript + snapshot 来源（本地 + WSL）。
pub fn discover() -> Result<Vec<SourceRef>> {
    discover::discover_all()
}

/// 仅发现宿主系统本机来源，不调用 WSL。用于需要确定性和文件系统隔离的宿主测试。
pub fn discover_local() -> Result<Vec<SourceRef>> {
    discover::discover_local()
}

pub fn discover_transcripts() -> Result<Vec<SourceRef>> {
    discover::discover_transcripts()
}

pub fn discover_transcripts_local() -> Result<Vec<SourceRef>> {
    discover::discover_transcripts_local()
}

pub fn discover_snapshots() -> Result<Vec<SourceRef>> {
    discover::discover_snapshots()
}

pub fn discover_project_snapshots(roots: &[ProjectSnapshotRoot]) -> Vec<SourceRef> {
    discover::discover_project_snapshots(roots)
}

/// §9 `scan()`：单来源增量扫描（按 source_mode 分派）。
pub fn scan(source: &SourceRef, cursor_in: Option<Cursor>, profile: Profile) -> ScanResult {
    scan::scan_source(source, cursor_in, profile)
}
