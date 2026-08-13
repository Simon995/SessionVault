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
pub mod deadline;
pub mod discover;
pub mod discovery;
pub mod identity;
pub mod logging;
pub mod observation;
pub mod parser;
pub mod pathnorm;
/// 存在性探测的唯一原语（ADR-051 §5 / §8）——`std::fs` 的存在性调用只许出现在这里。
pub mod probe;
pub mod project_root;
pub mod rawevent;
pub mod report;
pub mod scan;
/// 不可变 RawEvent 总库（§13 / ADR-020）——`store` feature 门控（持久化组件，内核仍无状态）。
#[cfg(feature = "store")]
pub mod store;
#[cfg(feature = "store")]
mod store_crypto;
pub mod token;
pub mod wsl;

pub use catalog::{Artifact, Profile, ProviderDescriptor};
pub use cursor::{Cursor, CursorKind, ScanResult, ScanStatus};
pub use discover::{
    DiscoveryOutcome, ProjectSnapshotOutcome, ProjectSnapshotRoot, SourceRef, LOCAL_LOCATION,
    UNREACHABLE_ALL_WSL,
};
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
    discover::discover_all(crate::deadline::Deadline::unbounded())
}

/// 仅发现宿主系统本机来源，不调用 WSL。用于需要确定性和文件系统隔离的宿主测试。
pub fn discover_local() -> Result<Vec<SourceRef>> {
    discover::discover_local(crate::deadline::Deadline::unbounded())
}

pub fn discover_transcripts() -> Result<Vec<SourceRef>> {
    discover::discover_transcripts(crate::deadline::Deadline::unbounded())
}

/// 同 [`discover_transcripts`]，但报出哪些位置没问成 —— 要据发现结果删存量的调用方
/// **必须**用这个（见 [`DiscoveryOutcome`]）。
///
/// 🔴 **预算由调用方给，这里不再兜底成 `unbounded()`。**（评审 P2-3）
///
/// 同族其它包装都是「注入 `unbounded()`」的便利函数，而这一个的调用方
/// （QuotaBar 的整轮刷新）**手上正好有一份预算**，却因为签名不收就把它丢了 ——
/// 那边的注释写着「一整轮的 deadline 覆盖发现与扫描」，而发现阶段实际不受约束：
/// 一个卡住的 WSL 下，同一 distro 四个 artifact 各等一次 `find` 超时，整轮预算
/// 在扫描开始前就见底了。
///
/// **一个默认参数省下的那点便利，换来的是一条无人知晓的无界路径。**
pub fn discover_transcripts_reported(
    deadline: crate::deadline::Deadline,
) -> Result<DiscoveryOutcome> {
    discover::discover_transcripts_reported(deadline)
}

pub fn discover_transcripts_local() -> Result<Vec<SourceRef>> {
    discover::discover_transcripts_local(crate::deadline::Deadline::unbounded())
}

pub fn discover_snapshots() -> Result<Vec<SourceRef>> {
    discover::discover_snapshots(crate::deadline::Deadline::unbounded())
}

/// 在宿主已确认的项目根内发现 CLAUDE.md / AGENTS.md。
///
/// 🔴 返回 [`discover::ProjectSnapshotOutcome`] 而不是裸列表 —— 「没问成」必须
/// 能说出口；预算由调用方给，理由同 [`discover_transcripts_reported`]。
pub fn discover_project_snapshots(
    roots: &[ProjectSnapshotRoot],
    deadline: crate::deadline::Deadline,
) -> discover::ProjectSnapshotOutcome {
    discover::discover_project_snapshots(roots, deadline)
}

/// 本机 WSL 里 Windows 盘的挂载表 —— **发现与归属共用的一项运行期事实**。
///
/// best-effort：没 WSL / `wsl.exe` 卡住 ⇒ 空表 ⇒ `/mnt/…` 那族不与宿主形式收敛，
/// **不是**退回按盘符猜（那会把事件归到别的项目名下）。
pub fn host_drive_mounts() -> pathnorm::DriveMounts {
    wsl::list_distros(crate::deadline::Deadline::unbounded())
        .ok()
        .and_then(|d| wsl::default_distro(&d))
        .and_then(|d| match wsl::drive_mounts(&d, crate::deadline::Deadline::unbounded()) {
            Ok(m) => Some(m),
            Err(e) => {
                log::debug!(
                    target: logging::tag::SCAN,
                    "drive mounts unavailable: {e} — /mnt paths will not converge with their host form"
                );
                None
            }
        })
        .unwrap_or_default()
}

/// 读出项目根注册表 —— **归属的唯一输入**（ADR-050）。
///
/// 🔴 **收口在这里，因为它有两个客户端**（QuotaBar 的 session index、`svault scan-all`），
/// 而「注册表 = 表里的行 + 本机挂载表」这条规则若各写一遍，两边会对「哪些路径算同一个
/// 根」给出不同答案 —— 而 `project_root` 本该是事件的客观属性，不是「取决于谁跑」。
/// 本仓已有判例：会话枚举曾有两条路径，对「有哪些文件」给出 201 vs 23 两个答案。
///
/// `mounts` 显式传入（而不是在函数里读）：发现侧也要用同一份表，读两次会拿到两份
/// 运行期事实，而它们**可以不同**（中途 `wsl --shutdown`）。
#[cfg(feature = "store")]
pub fn project_root_registry(
    store: &TotalStore,
    mounts: &pathnorm::DriveMounts,
) -> attribution::RootRegistry {
    store.project_root_registry(mounts)
}

/// §9 `scan()`：单来源增量扫描（按 source_mode 分派）。
///
/// `roots` 是已知项目根的注册表 —— 归属的**唯一**输入（ADR-050 步 3）。
/// 🔴 **空注册表合法**，含义是「一个根都不知道」⇒ 每条路径归 `Unattributed`。
/// 它不是「退回旧的 cwd 兜底」：调用方**必须**显式决定给什么，而给不出来时
/// 得到的是「说不出来」，不是另一个答案。
pub fn scan(
    source: &SourceRef,
    cursor_in: Option<Cursor>,
    profile: Profile,
    roots: std::sync::Arc<crate::attribution::RootRegistry>,
) -> ScanResult {
    scan::scan_source(
        source,
        cursor_in,
        profile,
        roots,
        crate::deadline::Deadline::unbounded(),
    )
}
