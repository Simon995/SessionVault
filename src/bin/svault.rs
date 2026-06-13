//! `svault` CLI：SessionVault 的跨语言主接口（§12 / ADR-024）。
//!
//! **stdout = NDJSON 结果**（每行一条 JSON，供 TumeFlow 子进程消费）；
//! **stderr = 日志**（env_logger 自装 sink；库本身不装 sink，见 ADR-026）。
//! 日志级别：`SVAULT_LOG`（优先）/ `RUST_LOG`，默认 `info`。

use std::io::Write;

use clap::{Parser, Subcommand};
use serde::Serialize;
use session_vault::catalog::Profile;
use session_vault::logging::tag;
use session_vault::rawevent::{RawEvent, SourceLocation, SourceMode, SourceType};

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
    /// 一轮全量增量扫描（骨架：无外部游标，从头扫）。
    ScanAll {
        /// 扫描 profile。
        #[arg(long, value_enum, default_value = "metadata")]
        profile: ProfileArg,
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
        Command::ScanAll { profile } => run_scan_all(profile.into()),
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

fn run_scan_all(profile: Profile) -> i32 {
    let sources = match session_vault::discover() {
        Ok(s) => s,
        Err(e) => {
            log::error!(target: tag::CLI, "discover failed: {e}");
            return 1;
        }
    };
    let mut total_events = 0u64;
    for s in &sources {
        let res = session_vault::scan(s, None, profile);
        total_events += res.report.events_emitted;
        // 先逐条吐事件（NDJSON 事件流，TumeFlow 据此消费），再吐该来源的报告。
        for ev in &res.events {
            emit(&Out::Event { event: ev });
        }
        emit(&Out::SourceReport { report: &res.report });
    }
    emit(&Out::Summary {
        sources: sources.len(),
        events: total_events,
    });
    0
}
