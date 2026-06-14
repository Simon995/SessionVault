//! `svault` CLI：SessionVault 的跨语言主接口（§12 / ADR-024）。
//!
//! **stdout = NDJSON 结果**（每行一条 JSON，供 TumeFlow 子进程消费）；
//! **stderr = 日志**（env_logger 自装 sink；库本身不装 sink，见 ADR-026）。
//! 日志级别：`SVAULT_LOG`（优先）/ `RUST_LOG`，默认 `info`。

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;

use clap::{Parser, Subcommand};
use serde::Serialize;
use session_vault::catalog::Profile;
use session_vault::cursor::Cursor;
use session_vault::logging::tag;
use session_vault::rawevent::{RawEvent, SourceLocation, SourceMode, SourceType};
use session_vault::SourceRef;

#[derive(Parser)]
#[command(name = "svault", version, about = "SessionVault ingestion CLI")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 发现本机内置 provider 的来源清单（不读内容）。
    Discover,
    /// 一轮增量扫描。游标默认持久化到状态文件，跨运行续扫（真正增量）。
    ScanAll {
        /// 扫描 profile。
        #[arg(long, value_enum, default_value = "metadata")]
        profile: ProfileArg,
        /// 游标状态文件路径（覆盖默认 `<data_local_dir>/svault/cursors.json`）。
        #[arg(long)]
        state: Option<PathBuf>,
        /// 无状态：忽略且不写状态文件，每次从头全量扫（调试/一次性用）。
        #[arg(long)]
        stateless: bool,
    },
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum ProfileArg {
    Metadata,
    Full,
}

impl From<ProfileArg> for Profile {
    fn from(p: ProfileArg) -> Self {
        match p {
            ProfileArg::Metadata => Profile::Metadata,
            ProfileArg::Full => Profile::Full,
        }
    }
}

/// 一行 NDJSON 输出包络。`kind` 区分记录类型，下游按 `kind` 分流。
#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Out<'a> {
    /// discover 产物。枚举直接 serde 序列化为稳定 snake_case（`claude_code` 等），
    /// 与 `RawEvent` 的序列化一致——不再用 `{:?}` Debug 输出 Rust 变体名。
    Source {
        source_type: SourceType,
        source_location: SourceLocation,
        source_mode: SourceMode,
        path: String,
    },
    /// scan 产出的一条归一化事件（TumeFlow 依赖的事件流契约）。
    Event {
        event: &'a RawEvent,
    },
    SourceReport {
        report: &'a session_vault::report::SourceReport,
    },
    Summary {
        sources: usize,
        events: u64,
    },
}

fn main() {
    init_logging();
    let cli = Cli::parse();
    let code = match cli.command {
        Command::Discover => run_discover(),
        Command::ScanAll {
            profile,
            state,
            stateless,
        } => run_scan_all(profile.into(), state, stateless),
    };
    std::process::exit(code);
}

/// env_logger sink 到 **stderr**，stdout 留给 NDJSON。
fn init_logging() {
    let filter = std::env::var("SVAULT_LOG")
        .or_else(|_| std::env::var("RUST_LOG"))
        .unwrap_or_else(|_| "info".to_string());
    env_logger::Builder::new()
        .parse_filters(&filter)
        .format(|buf, record| {
            writeln!(buf, "[{}] {}", record.target(), record.args())
        })
        .target(env_logger::Target::Stderr)
        .init();
}

fn emit(out: &Out) {
    match serde_json::to_string(out) {
        Ok(s) => println!("{s}"),
        Err(e) => log::error!(target: tag::CLI, "serialize failed: {e}"),
    }
}

fn run_discover() -> i32 {
    match session_vault::discover() {
        Ok(sources) => {
            for s in &sources {
                emit(&Out::Source {
                    source_type: s.source_type,
                    source_location: s.source_location.clone(),
                    source_mode: s.source_mode,
                    path: s.path.display().to_string(),
                });
            }
            emit(&Out::Summary {
                sources: sources.len(),
                events: 0,
            });
            0
        }
        Err(e) => {
            log::error!(target: tag::CLI, "discover failed: {e}");
            1
        }
    }
}

fn run_scan_all(profile: Profile, state_arg: Option<PathBuf>, stateless: bool) -> i32 {
    let sources = match session_vault::discover() {
        Ok(s) => s,
        Err(e) => {
            log::error!(target: tag::CLI, "discover failed: {e}");
            return 1;
        }
    };

    // 状态：source_key → Cursor。stateless 时为空 map 且不落盘（每次全量）。
    let state_path = if stateless { None } else { resolve_state_path(state_arg) };
    let mut cursors: HashMap<String, Cursor> = match &state_path {
        Some(p) => load_cursors(p),
        None => HashMap::new(),
    };

    let mut total_events = 0u64;
    for s in &sources {
        let key = source_key(s);
        let cursor_in = cursors.get(&key).cloned();
        let res = session_vault::scan(s, cursor_in, profile);
        total_events += res.report.events_emitted;
        // 先逐条吐事件（NDJSON 事件流，TumeFlow 据此消费），再吐该来源的报告。
        for ev in &res.events {
            emit(&Out::Event { event: ev });
        }
        emit(&Out::SourceReport { report: &res.report });
        // 更新游标（即便本轮无新增也写回，刷新 size/mtime）。
        cursors.insert(key, res.cursor_out);
    }

    if let Some(p) = &state_path {
        if let Err(e) = save_cursors(p, &cursors) {
            log::error!(target: tag::CLI, "save state failed: path={} err={e}", p.display());
        } else {
            log::info!(target: tag::CLI, "state saved: path={} entries={}", p.display(), cursors.len());
        }
    }

    emit(&Out::Summary {
        sources: sources.len(),
        events: total_events,
    });
    0
}

/// 来源的稳定身份键（跨运行定位游标）：`<type>|<location>|<path>`。
fn source_key(s: &SourceRef) -> String {
    let st = match s.source_type {
        SourceType::ClaudeCode => "claude_code",
        SourceType::Codex => "codex",
        SourceType::Cursor => "cursor",
        SourceType::Gemini => "gemini",
        SourceType::Jsonl => "jsonl",
    };
    format!("{st}|{}|{}", s.source_location.as_key(), s.path.display())
}

/// 解析状态文件路径：`--state` 优先，否则 `<data_local_dir>/svault/cursors.json`。
/// 无法确定数据目录时返回 None（退化为无状态，发警告）。
fn resolve_state_path(arg: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(p) = arg {
        return Some(p);
    }
    match dirs_next::data_local_dir() {
        Some(d) => Some(d.join("svault").join("cursors.json")),
        None => {
            log::warn!(
                target: tag::CLI,
                "no data_local_dir; running stateless (pass --state to persist cursors)"
            );
            None
        }
    }
}

/// 读状态文件 → 游标表。不存在或损坏 → 空表（发警告，不崩）。
fn load_cursors(path: &std::path::Path) -> HashMap<String, Cursor> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return HashMap::new(),
        Err(e) => {
            log::warn!(target: tag::CLI, "read state failed (starting empty): path={} err={e}", path.display());
            return HashMap::new();
        }
    };
    match serde_json::from_slice(&bytes) {
        Ok(m) => m,
        Err(e) => {
            log::warn!(target: tag::CLI, "parse state failed (starting empty): path={} err={e}", path.display());
            HashMap::new()
        }
    }
}

/// 原子写状态文件：先写 `.tmp` 再 rename，避免半写损坏。
fn save_cursors(path: &std::path::Path, cursors: &HashMap<String, Cursor>) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_vec_pretty(cursors)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &json)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}
