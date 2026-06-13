//! 日志 tag 常量（ADR-026 / `docs/LOGGING.md`）。
//!
//! 库只发 `log` 事件、**不装 sink**：QuotaBar 复用 `tauri-plugin-log` 的 sink，
//! `svault` CLI 自己把日志 sink 到 stderr（stdout 留给 NDJSON 结果）。
//! 格式：`[<module-tag>][<scope>] <event>: <key>=<value>`；正文永不进日志。

/// 模块 tag。统一 `sv-` 前缀，便于在宿主混合日志里过滤本 crate 来源。
pub mod tag {
    pub const DISCOVER: &str = "sv-discover";
    pub const SCAN: &str = "sv-scan";
    pub const CURSOR: &str = "sv-cursor";
    pub const CLAUDE: &str = "sv-claude";
    pub const CODEX: &str = "sv-codex";
    pub const WSL: &str = "sv-wsl";
    pub const SNAPSHOT: &str = "sv-snapshot";
    pub const SQLITE: &str = "sv-sqlite";
    pub const CLI: &str = "sv-cli";
}
