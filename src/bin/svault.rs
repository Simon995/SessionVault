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
// 🔴 门必须是两个使用点的**并集**，一个都不能多、不能少：
//   - `run_fixture_append`  → `all(feature = "acceptance-fixtures", debug_assertions)`
//   - `mod tests`           → `all(test, feature = "store")`
//
// 只按前者开 ⇒ `--features store` 单独编译时测试模块缺 import（编译失败）；
// 只按 `feature = "store"` 开 ⇒ 不带 acceptance-fixtures 的 bin 目标里它未被使用
// （unused import 告警）。两种错法都只在**特定 feature 组合**下现形，与昨天那个漏改的
// 调用点同源 —— feature 门后的代码不在默认闸的覆盖里，改它必须逐个组合编一遍。
#[cfg(all(
    feature = "store",
    any(all(feature = "acceptance-fixtures", debug_assertions), test)
))]
use session_vault::store::Projection;
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
    /// 从不可变总库增量拉取 `--since` offset 之后的 `RawEvent`（NDJSON），供 TumeFlow
    /// 物化分库（P3-③ / §13.2）。**只读**总库（QuotaBar 是默认写者），游标由调用方持久化。
    /// 需 `store` feature（rusqlite）；未启用时本子命令不存在（clap 报未知子命令）。
    #[cfg(feature = "store")]
    Pull {
        /// 只拉 offset 严格大于此值的事件；首次全量同步传 `0`（默认）。
        #[arg(long, default_value_t = 0)]
        since: i64,
        /// 发哪一份投影。**默认 `all` 是刻意的**：给一个已发布 CLI 静默换掉返回内容，
        /// 按旧语义写的消费者不会报错，只会悄悄拿到不同的数据（ADR-044 决定 6 的
        /// 兼容性铁律）。想要「只发当前解析」必须显式选 `current`。
        #[arg(long, value_enum, default_value = "all")]
        projection: ProjectionArg,
        /// 本轮最多吐多少条事件（`0` = 不限，一次拉到追平总库尾）。用于把大回填切成有界批次。
        #[arg(long, default_value_t = 0)]
        limit: u64,
        /// 总库路径（覆盖默认 `<data_local_dir>/svault/total_store.db`，与 QuotaBar 写者同址）。
        #[arg(long)]
        store: Option<PathBuf>,
    },
    /// 最近活跃的 N 个会话（按**事件真实时间**降序，不是按写入顺序）。
    ///
    /// 消费者此前用 `[max_offset - N, max_offset]` 当「最近窗口」，而 `offset` 是写入
    /// 顺序 —— 一次全量重扫之后，那个窗口实测横跨九个多月、当天的会话被挤了出去。
    /// 这个子命令的存在就是让正确的做法比错误的做法更好用。
    #[cfg(feature = "store")]
    SessionsRecent {
        /// 最多返回多少个会话。
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// 只看这个 UTC 毫秒之后活跃过的会话。
        #[arg(long)]
        since_ms: Option<i64>,
        #[arg(long)]
        store: Option<PathBuf>,
    },
    /// 按会话身份读它们的**当前投影全部事件**。
    ///
    /// 与 `pull --limit N` 的差别不是效率是正确性：消费者先用 `sessions-recent` 选出
    /// 要处理的会话，再用这个命令拿它们的完整事件。用 offset 前缀去凑，选中的会话若
    /// 排在前缀之后就整个不在结果里，而且不报错 —— 只是那个会话静默消失。
    #[cfg(feature = "store")]
    SessionsRead {
        /// `<type>/<location>/<path>/<session_id>` 形式，可重复。
        #[arg(long = "session", value_name = "SPEC")]
        sessions: Vec<String>,
        /// 事件总数上界（防一个超大会话吃光内存 / LLM 上下文）。
        #[arg(long, default_value_t = 50000)]
        max_events: usize,
        #[arg(long)]
        store: Option<PathBuf>,
    },
    /// 返回总库中每个 snapshot source 的最新版本，供 TumeFlow Class-B 主路径消费。
    #[cfg(feature = "store")]
    Snapshots {
        #[arg(long)]
        store: Option<PathBuf>,
    },
    /// 投影替换的变更流（ADR-044 决定 6）。
    ///
    /// 只读「当前投影」不足以让消费者收敛：一次重投影把 `{A,B}` 换成 `{A}` 之后，
    /// 增量流最多重发 A，**没有任何记录要求删除已物化的 B**。这条流补的就是那半边。
    ///
    /// `--since-seq` 是这条流自己的游标，与 `pull --since` 的 offset 无关。
    #[cfg(feature = "store")]
    Changes {
        #[arg(long, default_value_t = 0)]
        since_seq: i64,
        #[arg(long, default_value_t = 1000)]
        limit: usize,
        #[arg(long)]
        store: Option<PathBuf>,
    },
    /// 回收**已被取代且来源明确**的投影（ADR-044 决定 7）。
    ///
    /// 只碰台账里 `origin = 'reparse'` 且不是当前头的那些。**不碰** `rollback`
    /// （磁盘上已消失内容的唯一副本）与 `unknown`（ADR-044 之前产生、无从判断当初
    /// 是回退还是重解析的行）。
    ///
    /// 决定 2 落地后不再产生新的被取代投影，所以这基本是一次性清扫。**不自动执行**
    /// —— 它删的是历史，该由人按一次按钮。
    #[cfg(feature = "store")]
    Gc {
        /// 只统计不删。
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        store: Option<PathBuf>,
    },
    /// 用户主动彻底删除：同一事务写不含正文的墓碑并物理删除命中事件（ADR-027）。
    #[cfg(feature = "store")]
    Erase {
        #[arg(long, value_enum)]
        scope: EraseScopeArg,
        #[arg(long)]
        key: String,
        /// 防止误调用；编排方必须显式传入 `ERASE`。
        #[arg(long)]
        confirm: String,
        #[arg(long)]
        store: Option<PathBuf>,
    },
    /// 仅用于 ADR-027 隔离验收构建；正式发布不包含此命令。
    #[cfg(all(feature = "acceptance-fixtures", debug_assertions))]
    #[command(hide = true)]
    FixtureAppend {
        #[arg(long)]
        event_file: PathBuf,
        #[arg(long)]
        store: PathBuf,
    },
}

/// `pull --projection` 的取值。
///
/// `all` = 历史上所有投影（旧语义，默认）；`current` = 只发每个源文件的当前投影。
/// 一次重投影之后两者差别巨大：`all` 会把同一批会话发两遍（旧解析 + 新解析），
/// 消费者若不自己按身份去重就会把同一段历史物化两次。
#[cfg(feature = "store")]
#[derive(Clone, Copy, clap::ValueEnum)]
enum ProjectionArg {
    All,
    Current,
}

#[cfg(feature = "store")]
#[derive(Clone, Copy, clap::ValueEnum)]
enum EraseScopeArg {
    Session,
    SourcePath,
    ProjectRoot,
}

#[cfg(feature = "store")]
impl From<EraseScopeArg> for session_vault::TombstoneScope {
    fn from(value: EraseScopeArg) -> Self {
        match value {
            EraseScopeArg::Session => Self::Session,
            EraseScopeArg::SourcePath => Self::SourcePath,
            EraseScopeArg::ProjectRoot => Self::ProjectRoot,
        }
    }
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
    Event { event: &'a RawEvent },
    SourceReport {
        report: &'a session_vault::report::SourceReport,
    },
    Summary {
        sources: usize,
        events: u64,
        /// 游标状态是否成功落盘：`Some(true/false)`；`None` = stateless（未持久化）。
        /// `false` 时进程以非 0 退出——下游据此知道本轮增量游标**未推进**，需重试或预期重复。
        state_saved: Option<bool>,
    },
    /// `pull` 产出的一条带 `offset` 的总库事件（P3-③）。`offset` 是消费者（TumeFlow）
    /// 持久化的**游标 token**：下次 `pull --since <offset>` 从此续拉。比 `Event` 多 `offset`，
    /// 因为增量同步靠 offset 定位，而 `scan` 的事件流靠各来源游标、无全局 offset。
    #[cfg(feature = "store")]
    Pulled { offset: i64, event: &'a RawEvent },
    /// `pull` 收尾摘要。消费者据 `last_offset` 持久化游标、据 `caught_up` 判断是否已追平总库尾。
    /// `caught_up=false` 仅因 `--limit` 截断（可能还有），消费者据此决定是否再拉一轮。
    #[cfg(feature = "store")]
    PullSummary {
        /// 本轮请求的起点（回显入参）。
        since: i64,
        /// 本轮吐出的最大 offset（无事件时 = `since`）——消费者据此推进游标。
        last_offset: i64,
        /// 本轮吐出的事件条数。
        events: u64,
        /// 当前总库最大 offset（信息性：宿主可显示「落后多少」）。
        store_max_offset: i64,
        /// 是否已读尽 `since` 之后的可读事件（`false` = 被 `--limit` 截断，需再拉）。
        caught_up: bool,
    },
    /// `sessions-recent` 的一行。`last_occurred_at_unix_ms = null` = 这个会话的事件
    /// 全都没有可解析的时间 —— 它排在最后但**照常返回**，因为「不知道什么时候发生」
    /// 是需要被看见的事实，不是「很久以前」。
    #[cfg(feature = "store")]
    RecentSession {
        source_type: String,
        source_location: String,
        source_path: String,
        session_id: String,
        last_occurred_at_unix_ms: Option<i64>,
        first_occurred_at_unix_ms: Option<i64>,
        events: u64,
    },
    #[cfg(feature = "store")]
    RecentSessionsSummary { sessions: usize },
    /// `sessions-read` 的收尾摘要。`truncated = true` 说明撞到了 `--max-events`
    /// 上界，**还有事件没发** —— 消费者必须据此判断结果是否完整，而不是默认它完整。
    #[cfg(feature = "store")]
    SessionsReadSummary {
        sessions: usize,
        events: u64,
        truncated: bool,
    },
    #[cfg(feature = "store")]
    Snapshot { offset: i64, event: &'a RawEvent },
    #[cfg(feature = "store")]
    SnapshotSummary { snapshots: u64 },
    /// 一条投影替换记录。消费者读到它就按 `source_*` **原子替换**自己那份物化。
    #[cfg(feature = "store")]
    ProjectionReplaced {
        seq: i64,
        at: i64,
        source_type: String,
        source_location: String,
        source_path: String,
        old_source_revision: Option<i64>,
        old_projection_revision: Option<i64>,
        new_source_revision: i64,
        new_projection_revision: i64,
        reason: String,
    },
    #[cfg(feature = "store")]
    ChangesSummary {
        since_seq: i64,
        last_seq: i64,
        changes: u64,
        /// `false` = 被 `--limit` 截断，还有记录没发 —— 消费者必须再拉一轮。
        caught_up: bool,
    },
    #[cfg(feature = "store")]
    GcSummary {
        projections: u64,
        events: u64,
        dry_run: bool,
    },
    #[cfg(feature = "store")]
    EraseSummary {
        deleted_events: u64,
        keys_destroyed: u64,
        tombstone_written: bool,
    },
    #[cfg(all(feature = "acceptance-fixtures", debug_assertions))]
    FixtureSummary {
        appended: u64,
        skipped_dup: u64,
        skipped_erased: u64,
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
        #[cfg(feature = "store")]
        Command::Pull {
            since,
            projection,
            limit,
            store,
        } => run_pull(since, projection, limit, store),
        #[cfg(feature = "store")]
        Command::SessionsRecent {
            limit,
            since_ms,
            store,
        } => run_sessions_recent(limit, since_ms, store),
        #[cfg(feature = "store")]
        Command::SessionsRead {
            sessions,
            max_events,
            store,
        } => run_sessions_read(sessions, max_events, store),
        #[cfg(feature = "store")]
        Command::Snapshots { store } => run_snapshots(store),
        #[cfg(feature = "store")]
        Command::Changes {
            since_seq,
            limit,
            store,
        } => run_changes(since_seq, limit, store),
        #[cfg(feature = "store")]
        Command::Gc { dry_run, store } => run_gc(dry_run, store),
        #[cfg(feature = "store")]
        Command::Erase {
            scope,
            key,
            confirm,
            store,
        } => run_erase(scope, key, confirm, store),
        #[cfg(all(feature = "acceptance-fixtures", debug_assertions))]
        Command::FixtureAppend { event_file, store } => run_fixture_append(event_file, store),
    };
    std::process::exit(code);
}

#[cfg(feature = "store")]
fn run_snapshots(store_arg: Option<PathBuf>) -> i32 {
    let Some(store_path) = resolve_store_path(store_arg) else {
        log::error!(target: tag::CLI, "no data_local_dir; pass --store");
        return 1;
    };
    if !store_path.exists() {
        log::error!(target: tag::CLI, "total store not found: {}", store_path.display());
        return 1;
    }
    let store = match open_total_store(&store_path) {
        Ok(store) => store,
        Err(e) => {
            log::error!(target: tag::CLI, "snapshot store open failed: {e}");
            return 2;
        }
    };
    let rows = match store.read_active_latest_snapshots() {
        Ok(rows) => rows,
        Err(e) => {
            log::error!(target: tag::CLI, "snapshot read failed: {e}");
            return 2;
        }
    };
    for (offset, event) in &rows {
        emit(&Out::Snapshot {
            offset: *offset,
            event,
        });
    }
    emit(&Out::SnapshotSummary {
        snapshots: rows.len() as u64,
    });
    0
}

/// env_logger sink 到 **stderr**，stdout 留给 NDJSON。
fn init_logging() {
    let filter = std::env::var("SVAULT_LOG")
        .or_else(|_| std::env::var("RUST_LOG"))
        .unwrap_or_else(|_| "info".to_string());
    env_logger::Builder::new()
        .parse_filters(&filter)
        .format(|buf, record| writeln!(buf, "[{}] {}", record.target(), record.args()))
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
                state_saved: None,
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
    let state_path = if stateless {
        None
    } else {
        resolve_state_path(state_arg)
    };
    let mut cursors: HashMap<String, Cursor> = match &state_path {
        Some(p) => load_cursors(p),
        None => HashMap::new(),
    };

    // 归属的唯一输入。读不出来就是空注册表 ⇒ 一致地 `Unattributed`，**不退回 cwd**。
    // 🔴 空表要说出来：一份静默为空的注册表会让整轮扫描的 `project_root` 全成兜底值，
    // 而那与「本机确实一个项目根都没发现」在输出里长得一模一样。
    let roots = std::sync::Arc::new(project_roots());
    if roots.is_empty() {
        log::warn!(
            target: tag::CLI,
            "project root registry is empty — every path will be Unattributed"
        );
    }

    let mut total_events = 0u64;
    for s in &sources {
        let key = source_key(s);
        let cursor_in = cursors.get(&key).cloned();
        let res = session_vault::scan(s, cursor_in, profile, roots.clone());
        total_events += res.report.events_emitted;
        // 先逐条吐事件（NDJSON 事件流，TumeFlow 据此消费），再吐该来源的报告。
        for ev in &res.events {
            emit(&Out::Event { event: ev });
        }
        emit(&Out::SourceReport {
            report: &res.report,
        });
        // 更新游标（即便本轮无新增也写回，刷新 size/mtime）。
        cursors.insert(key, res.cursor_out);
    }

    // 状态持久化结果：None=stateless；Some(true/false)=尝试落盘的成败。
    let state_saved = match &state_path {
        None => None,
        Some(p) => match save_cursors(p, &cursors) {
            Ok(()) => {
                log::info!(target: tag::CLI, "state saved: path={} entries={}", p.display(), cursors.len());
                Some(true)
            }
            Err(e) => {
                log::error!(target: tag::CLI, "save state failed: path={} err={e}", p.display());
                Some(false)
            }
        },
    };

    emit(&Out::Summary {
        sources: sources.len(),
        events: total_events,
        state_saved,
    });

    // 游标保存失败 → 非 0 退出（码 2，区别于 discover 失败的 1）。否则调用方会把本轮
    // 当成功，下轮因游标未推进而重复吐已消费事件——尤其权限/磁盘/rename 失败时极难发现。
    if state_saved == Some(false) {
        2
    } else {
        0
    }
}

/// `pull`：从总库增量拉 `since` 之后的事件，流式吐 NDJSON，收尾报摘要。
///
/// 退出码：`0` 正常（含「无新事件」）；`1` 定位/打开/读取失败。游标推进是**调用方**的事
/// （持久化 `last_offset` 作下次 `--since`）——本命令无状态、只读，符合 §8「内核不落盘游标」。
#[cfg(feature = "store")]
fn run_pull(since: i64, projection: ProjectionArg, limit: u64, store_arg: Option<PathBuf>) -> i32 {
    let store_path = match resolve_store_path(store_arg) {
        Some(p) => p,
        None => {
            log::error!(
                target: tag::CLI,
                "no data_local_dir; pass --store to locate the total store"
            );
            return 1;
        }
    };
    // 库不存在 = 宿主还没扫过一轮（写者尚未建库）。明确报错而非静默吐空，便于排查。
    if !store_path.exists() {
        log::error!(
            target: tag::CLI,
            "total store not found: path={} (host writes it on first scan)",
            store_path.display()
        );
        return 1;
    }
    let store = match open_total_store(&store_path) {
        Ok(s) => s,
        Err(e) => {
            log::error!(target: tag::CLI, "open total store failed: path={} err={e}", store_path.display());
            return 1;
        }
    };
    let store_max_offset = match store.status() {
        Ok(s) => s.max_offset,
        Err(e) => {
            log::error!(target: tag::CLI, "store status failed: {e}");
            return 1;
        }
    };

    let mut events = 0u64;
    let mut last_offset = since;
    let pulled = pull_stream(
        |cursor, want| match projection {
            // 默认路径原样不动 —— 见 `ProjectionArg` 的文档。
            ProjectionArg::All => store.read_since_page(cursor, want),
            ProjectionArg::Current => store.read_current_since_page(cursor, want),
        },
        since,
        limit,
        |offset, ev| {
            emit(&Out::Pulled { offset, event: ev });
            last_offset = offset;
            events += 1;
        },
    );
    let caught_up = match pulled {
        Ok(hit_limit) => !hit_limit,
        Err(e) => {
            log::error!(target: tag::CLI, "pull read failed: {e}");
            return 1;
        }
    };

    emit(&Out::PullSummary {
        since,
        last_offset,
        events,
        store_max_offset,
        caught_up,
    });
    log::info!(
        target: tag::CLI,
        "pull done: since={since} last_offset={last_offset} events={events} caught_up={caught_up}"
    );
    0
}

#[cfg(feature = "store")]
fn run_erase(
    scope: EraseScopeArg,
    key: String,
    confirm: String,
    store_arg: Option<PathBuf>,
) -> i32 {
    if confirm != "ERASE" {
        log::error!(target: tag::CLI, "erase rejected: explicit --confirm ERASE is required");
        return 2;
    }
    if key.trim().is_empty() {
        log::error!(target: tag::CLI, "erase rejected: --key must not be empty");
        return 2;
    }
    let store_path = match resolve_store_path(store_arg) {
        Some(path) if path.exists() => path,
        Some(_) => {
            log::error!(target: tag::CLI, "total store not found for erase");
            return 1;
        }
        None => {
            log::error!(target: tag::CLI, "no data_local_dir; pass --store to locate the total store");
            return 1;
        }
    };
    let store = match open_total_store(&store_path) {
        Ok(store) => store,
        Err(e) => {
            log::error!(target: tag::CLI, "open total store for erase failed: {e}");
            return 1;
        }
    };
    match store.tombstone(scope.into(), &key) {
        Ok(stats) => {
            emit(&Out::EraseSummary {
                deleted_events: stats.deleted_events,
                keys_destroyed: stats.keys_destroyed,
                tombstone_written: stats.tombstone_written,
            });
            0
        }
        Err(e) => {
            log::error!(target: tag::CLI, "erase failed: {e}");
            1
        }
    }
}

#[cfg(feature = "store")]
fn open_total_store(
    path: &std::path::Path,
) -> session_vault::store::StoreResult<session_vault::TotalStore> {
    #[cfg(all(feature = "acceptance-fixtures", debug_assertions))]
    if let Ok(encoded) = std::env::var("SVAULT_ACCEPTANCE_KEY") {
        let key = session_vault::StoreKey::from_encoded(&encoded)?;
        return session_vault::TotalStore::open_with_key(path, key);
    }
    session_vault::TotalStore::open(path)
}

#[cfg(all(feature = "acceptance-fixtures", debug_assertions))]
fn run_fixture_append(event_file: PathBuf, store_path: PathBuf) -> i32 {
    let bytes = match std::fs::read(&event_file) {
        Ok(bytes) => bytes,
        Err(e) => {
            log::error!(target: tag::CLI, "read synthetic fixture failed: {e}");
            return 1;
        }
    };
    let event: RawEvent = match serde_json::from_slice(&bytes) {
        Ok(event) => event,
        Err(e) => {
            log::error!(target: tag::CLI, "parse synthetic fixture failed: {e}");
            return 1;
        }
    };
    let store = match open_total_store(&store_path) {
        Ok(store) => store,
        Err(e) => {
            log::error!(target: tag::CLI, "open isolated fixture store failed: {e}");
            return 1;
        }
    };
    // 固件只发一条事件、走增量语义。
    //
    // 🔴 这个调用点（连同下方两个测试里的）在 `acceptance-fixtures` feature 后面，
    // 默认构建与 `cargo check --bins` 都不编译它 —— 所以把签名从 `bool` 换成
    // `Projection` 时，它静默失配了一整个提交，`cargo test --lib` 全绿。
    // feature 门后的代码不在默认闸的覆盖里，改公共签名时要单独编一遍。
    match store.append_events(&[event], Projection::Append) {
        Ok(stats) => {
            emit(&Out::FixtureSummary {
                appended: stats.appended,
                skipped_dup: stats.skipped_dup,
                skipped_erased: stats.skipped_erased,
            });
            0
        }
        Err(e) => {
            log::error!(target: tag::CLI, "append isolated fixture failed: {e}");
            1
        }
    }
}

/// `pull` 的可测核心：循环翻页（`read_page(cursor, want)` 注入，便于脱库单测），逐条回调
/// `on_event(offset, event)`。
///
/// 返回 `Ok(true)` = 被 `limit` 截断（可能还有，调用方应再拉）；`Ok(false)` = 读尽
/// `since` 之后的可读事件（已追平）。
///
/// **追平判定只认 `max_scanned_offset==None`（SQL 零行），不认 `events` 空**（评审 [P1]）：
/// `read_since` 在 SQL `LIMIT` **之后**才 skip 反序列化失败的行，故一窗全是坏行（schema drift）
/// 时 `events=[]` 但 `max_scanned_offset=Some(...)`——若把它当追平，坏行之后的有效事件将
/// **永久不可达**。因此每轮把游标推进到 `max_scanned_offset`（越过坏行）而非最后一条**好**事件的
/// offset。`read_since` 只返 `offset>cursor` 的行 → `max_scanned>cursor` → 游标严格增 → 必然终止。
#[cfg(feature = "store")]
fn pull_stream<F>(
    mut read_page: F,
    since: i64,
    limit: u64,
    mut on_event: impl FnMut(i64, &RawEvent),
) -> Result<bool, session_vault::store::StoreError>
where
    F: FnMut(i64, usize) -> Result<session_vault::ReadPage, session_vault::store::StoreError>,
{
    const BATCH: usize = 1000;
    let mut cursor = since;
    let mut emitted = 0u64;
    loop {
        if limit != 0 && emitted >= limit {
            return Ok(true); // 已吐满 limit，可能还有 → 调用方据 caught_up=false 再拉
        }
        let want = if limit == 0 {
            BATCH
        } else {
            ((limit - emitted) as usize).min(BATCH)
        };
        let page = read_page(cursor, want)?;
        match page.max_scanned_offset {
            None => return Ok(false), // SQL 零行 → 真追平
            Some(max) => {
                for (offset, ev) in &page.events {
                    on_event(*offset, ev);
                    emitted += 1;
                }
                cursor = max; // 推进到扫描到的最大 offset（越过整窗坏行）
            }
        }
    }
}

/// 打开总库读项目根注册表；**任何一步拿不到就是空注册表**（一致地说不出来）。
///
/// `scan-all` 没有 `--store` 参数（它是流式的、不写库），所以走默认路径。
#[cfg(feature = "store")]
fn project_roots() -> session_vault::attribution::RootRegistry {
    let empty = session_vault::attribution::RootRegistry::new;
    let Some(p) = resolve_store_path(None).filter(|p| p.exists()) else {
        return empty();
    };
    match open_total_store(&p) {
        Ok(store) => {
            session_vault::project_root_registry(&store, &session_vault::host_drive_mounts())
        }
        Err(e) => {
            log::warn!(target: tag::CLI, "open total store for roots failed: {e}");
            empty()
        }
    }
}

/// 无 `store` feature 时**没有注册表可读** —— 归属一致地说不出来。
///
/// 🔴 这不是「补个空桩让它编译过」：默认 feature 下 `scan-all` 会把每条路径记成
/// `unattributed`，那是**真话**（这个构建确实无从知道项目根），但它对消费方几乎没用。
/// 所以调用点会 `warn` 一行说出来 —— 见 `run_scan_all`。**要有归属就带 `--features store`。**
#[cfg(not(feature = "store"))]
fn project_roots() -> session_vault::attribution::RootRegistry {
    session_vault::attribution::RootRegistry::new()
}

/// 解析总库路径：`--store` 优先，否则 `<data_local_dir>/svault/total_store.db`
/// （与 QuotaBar 写者 `main.rs` 同址）。无法确定数据目录时返回 `None`。
#[cfg(feature = "store")]
fn resolve_store_path(arg: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(p) = arg {
        return Some(p);
    }
    dirs_next::data_local_dir().map(|d| d.join("svault").join("total_store.db"))
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

#[cfg(all(test, feature = "store"))]
mod tests {
    use super::*;
    use session_vault::rawevent::{Actor, EventType, TimeConfidence, TokenUsage, SCHEMA_VERSION};
    use session_vault::TotalStore;

    fn mk_event(seq: u64, session: &str) -> RawEvent {
        RawEvent {
            schema_version: SCHEMA_VERSION,
            source_type: SourceType::ClaudeCode,
            source_location: SourceLocation::Local,
            source_path: "/p/file.jsonl".to_string(),
            source_session_id: session.to_string(),
            seq,
            event_key: None,
            source_mode: SourceMode::AppendLog,
            cwd: Some("/work".to_string()),
            project_root: Some("/work".to_string()),
            project_root_source: Some("cwd".to_string()),
            workspace_location: Some("local".to_string()),
            event_type: EventType::Message,
            actor: Some(Actor::User),
            occurred_at: Some("2026-06-01T10:00:00Z".to_string()),
            time_confidence: TimeConfidence::High,
            model: None,
            effort: None,
            usage: Some(TokenUsage::default()),
            content: Some(format!("c{seq}")),
            parent_ref: None,
            content_hash: None,
            artifact_kind: None,
            observed_at: None,
            message_id: None,
            request_id: None,
        }
    }

    /// 收集 `pull_stream` 的回调到 `(offset, RawEvent)` 列表，便于断言。
    fn collect(store: &TotalStore, since: i64, limit: u64) -> (Vec<(i64, RawEvent)>, bool) {
        let mut out = Vec::new();
        // 这个 helper 测的是 `pull_stream` 的翻页/追平语义，与选哪份投影无关，
        // 所以固定走默认（`all`）那一路。
        let hit_limit = pull_stream(
            |cursor, want| store.read_since_page(cursor, want),
            since,
            limit,
            |offset, ev| out.push((offset, ev.clone())),
        )
        .unwrap();
        (out, hit_limit)
    }

    #[test]
    fn pull_advances_past_all_bad_row_window() {
        // 评审 [P1]：一窗全是坏行时 read_since 返回 events=[] 但 max_scanned_offset=Some(_)。
        // pull_stream 必须据 max_scanned 推进游标越过坏行，最终拉到坏行之后的好事件——
        // 不能把空 events 误判成追平。用注入式 pager 精确复现该场景（不依赖真库注坏行）。
        use session_vault::ReadPage;
        let pages = vec![
            // 窗口1：两条好事件（offset 1、2）。
            ReadPage {
                events: vec![(1, mk_event(0, "s")), (2, mk_event(1, "s"))],
                max_scanned_offset: Some(2),
            },
            // 窗口2：整窗坏行 → events 空，但扫描到了 offset 4。
            ReadPage {
                events: vec![],
                max_scanned_offset: Some(4),
            },
            // 窗口3：坏行之后的好事件（offset 5）。
            ReadPage {
                events: vec![(5, mk_event(2, "s"))],
                max_scanned_offset: Some(5),
            },
            // 窗口4：SQL 零行 → 真追平。
            ReadPage {
                events: vec![],
                max_scanned_offset: None,
            },
        ];
        let mut it = pages.into_iter();
        let mut got: Vec<i64> = Vec::new();
        let hit_limit = pull_stream(
            |_cursor, _want| Ok(it.next().expect("pager called more than expected")),
            0,
            0,
            |offset, _ev| got.push(offset),
        )
        .unwrap();
        assert_eq!(got, vec![1, 2, 5], "必须越过坏行窗口拉到 offset 5");
        assert!(!hit_limit, "最终 SQL 零行 → 追平（caught_up=true）");
    }

    #[test]
    fn pull_since_filters_and_offsets_are_monotonic() {
        let store = TotalStore::open_in_memory().unwrap();
        store
            .append_events(
                &[mk_event(0, "s"), mk_event(1, "s"), mk_event(2, "s")],
                Projection::Append,
            )
            .unwrap();

        // since=0：拉全部 3 条，offset 严格递增。
        let (all, caught) = collect(&store, 0, 0);
        assert_eq!(all.len(), 3, "since=0 拉全部");
        assert!(!caught, "limit=0 读尽 → hit_limit=false（已追平）");
        assert!(
            all[0].0 < all[1].0 && all[1].0 < all[2].0,
            "offset 单调递增"
        );
        assert_eq!(all[0].1.content.as_deref(), Some("c0"));

        // since=第 1 条的 offset：只拉其后的 2 条（严格大于，不含等于）。
        let after_first = all[0].0;
        let (rest, _) = collect(&store, after_first, 0);
        assert_eq!(rest.len(), 2, "since=offset0 → 只剩 c1/c2");
        assert_eq!(rest[0].1.content.as_deref(), Some("c1"));
    }

    #[test]
    fn pull_limit_caps_batch_and_reports_hit_limit() {
        let store = TotalStore::open_in_memory().unwrap();
        store
            .append_events(
                &[
                    mk_event(0, "s"),
                    mk_event(1, "s"),
                    mk_event(2, "s"),
                    mk_event(3, "s"),
                ],
                Projection::Append,
            )
            .unwrap();

        // limit=2：只吐前 2 条，hit_limit=true（可能还有，调用方据此再拉）。
        let (first, caught) = collect(&store, 0, 2);
        assert_eq!(first.len(), 2);
        assert!(caught, "被 limit 截断 → caught_up=false");

        // 从上一轮 last_offset 续拉，把剩下的拉完。
        let next_since = first.last().unwrap().0;
        let (second, caught2) = collect(&store, next_since, 2);
        assert_eq!(second.len(), 2, "续拉剩余 2 条");
        assert!(
            caught2,
            "恰好 limit=2 取完 4 条中后 2 条 → 仍报截断（下一轮空确认追平）"
        );

        // 再拉一轮 → 空，确认追平。
        let (third, caught3) = collect(&store, second.last().unwrap().0, 2);
        assert!(third.is_empty());
        assert!(!caught3, "空批 → 已追平");
    }

    #[test]
    fn pull_empty_store_is_caught_up_immediately() {
        let store = TotalStore::open_in_memory().unwrap();
        let (out, caught) = collect(&store, 0, 0);
        assert!(out.is_empty());
        assert!(!caught, "空库即追平");
    }

    /// 锁定 NDJSON 线契约：TumeFlow（P3-③ 消费侧）按 `kind` 分流并读这些字段名，
    /// 改名 = 破坏跨语言契约，故用断言钉死 `pulled` / `pull_summary` 的外形。
    #[test]
    fn pull_ndjson_wire_shape_is_stable() {
        let ev = mk_event(0, "s");
        let pulled = serde_json::to_value(Out::Pulled {
            offset: 42,
            event: &ev,
        })
        .unwrap();
        assert_eq!(pulled["kind"], "pulled");
        assert_eq!(pulled["offset"], 42);
        assert_eq!(pulled["event"]["source_session_id"], "s");

        let summary = serde_json::to_value(Out::PullSummary {
            since: 10,
            last_offset: 42,
            events: 5,
            store_max_offset: 42,
            caught_up: true,
        })
        .unwrap();
        assert_eq!(summary["kind"], "pull_summary");
        assert_eq!(summary["since"], 10);
        assert_eq!(summary["last_offset"], 42);
        assert_eq!(summary["events"], 5);
        assert_eq!(summary["store_max_offset"], 42);
        assert_eq!(summary["caught_up"], true);

        let snapshot = serde_json::to_value(Out::Snapshot {
            offset: 43,
            event: &ev,
        })
        .unwrap();
        assert_eq!(snapshot["kind"], "snapshot");
        assert_eq!(snapshot["offset"], 43);
        let snapshot_summary = serde_json::to_value(Out::SnapshotSummary { snapshots: 1 }).unwrap();
        assert_eq!(snapshot_summary["kind"], "snapshot_summary");
        assert_eq!(snapshot_summary["snapshots"], 1);

        let erased = serde_json::to_value(Out::EraseSummary {
            deleted_events: 3,
            keys_destroyed: 2,
            tombstone_written: true,
        })
        .unwrap();
        assert_eq!(erased["kind"], "erase_summary");
        assert_eq!(erased["deleted_events"], 3);
        assert_eq!(erased["keys_destroyed"], 2);
        assert_eq!(erased["tombstone_written"], true);
    }
}

/// `sessions-recent`：按事件真实时间列出最近活跃的会话。
#[cfg(feature = "store")]
fn run_sessions_recent(limit: usize, since_ms: Option<i64>, store_arg: Option<PathBuf>) -> i32 {
    let Some(store_path) = resolve_store_path(store_arg) else {
        log::error!(target: tag::CLI, "no data_local_dir; pass --store");
        return 1;
    };
    if !store_path.exists() {
        log::error!(target: tag::CLI, "total store not found: {}", store_path.display());
        return 1;
    }
    let store = match open_total_store(&store_path) {
        Ok(s) => s,
        Err(e) => {
            log::error!(target: tag::CLI, "open total store failed: {e}");
            return 1;
        }
    };
    let sessions = match store.recent_sessions(limit, since_ms) {
        Ok(v) => v,
        Err(e) => {
            log::error!(target: tag::CLI, "recent_sessions failed: {e}");
            return 1;
        }
    };
    for s in &sessions {
        emit(&Out::RecentSession {
            source_type: s.source_type.clone(),
            source_location: s.source_location.clone(),
            source_path: s.source_path.clone(),
            session_id: s.session_id.clone(),
            last_occurred_at_unix_ms: s.last_occurred_at_unix_ms,
            first_occurred_at_unix_ms: s.first_occurred_at_unix_ms,
            events: s.event_count,
        });
    }
    emit(&Out::RecentSessionsSummary {
        sessions: sessions.len(),
    });
    0
}

/// `gc`：回收已被取代且来源明确的投影。
#[cfg(feature = "store")]
fn run_gc(dry_run: bool, store_arg: Option<PathBuf>) -> i32 {
    let Some(store_path) = resolve_store_path(store_arg) else {
        log::error!(target: tag::CLI, "no data_local_dir; pass --store");
        return 1;
    };
    if !store_path.exists() {
        log::error!(target: tag::CLI, "total store not found: {}", store_path.display());
        return 1;
    }
    let store = match open_total_store(&store_path) {
        Ok(s) => s,
        Err(e) => {
            log::error!(target: tag::CLI, "open total store failed: {e}");
            return 1;
        }
    };
    match store.gc_superseded_projections(dry_run) {
        Ok(stats) => {
            emit(&Out::GcSummary {
                projections: stats.projections,
                events: stats.events,
                dry_run: stats.dry_run,
            });
            0
        }
        Err(e) => {
            log::error!(target: tag::CLI, "gc failed: {e}");
            1
        }
    }
}

/// `sessions-read`：按会话身份读当前投影的全部事件。
#[cfg(feature = "store")]
fn run_sessions_read(specs: Vec<String>, max_events: usize, store_arg: Option<PathBuf>) -> i32 {
    let Some(store_path) = resolve_store_path(store_arg) else {
        log::error!(target: tag::CLI, "no data_local_dir; pass --store");
        return 1;
    };
    if !store_path.exists() {
        log::error!(target: tag::CLI, "total store not found: {}", store_path.display());
        return 1;
    }
    // `<type>/<location>/<path>/<session>`：path 可能含 `/`，所以从两端切 —— 前两段
    // 与最后一段取值受限且不含分隔符，中间全归 path。与 EvidenceRef v1 同一个道理。
    let mut sessions = Vec::new();
    for spec in &specs {
        let parts: Vec<&str> = spec.split('/').collect();
        if parts.len() < 4 {
            log::error!(target: tag::CLI, "bad --session spec (need type/loc/path/session): {spec}");
            return 2;
        }
        sessions.push((
            parts[0].to_string(),
            parts[1].to_string(),
            parts[2..parts.len() - 1].join("/"),
            parts[parts.len() - 1].to_string(),
        ));
    }
    let store = match open_total_store(&store_path) {
        Ok(s) => s,
        Err(e) => {
            log::error!(target: tag::CLI, "open total store failed: {e}");
            return 1;
        }
    };
    match store.read_sessions(&sessions, max_events) {
        Ok(events) => {
            for (offset, ev) in &events {
                emit(&Out::Pulled {
                    offset: *offset,
                    event: ev,
                });
            }
            emit(&Out::SessionsReadSummary {
                sessions: sessions.len(),
                events: events.len() as u64,
                truncated: events.len() >= max_events,
            });
            0
        }
        Err(e) => {
            log::error!(target: tag::CLI, "read_sessions failed: {e}");
            1
        }
    }
}

/// `changes`：投影替换的变更流。
#[cfg(feature = "store")]
fn run_changes(since_seq: i64, limit: usize, store_arg: Option<PathBuf>) -> i32 {
    let Some(store_path) = resolve_store_path(store_arg) else {
        log::error!(target: tag::CLI, "no data_local_dir; pass --store");
        return 1;
    };
    if !store_path.exists() {
        log::error!(target: tag::CLI, "total store not found: {}", store_path.display());
        return 1;
    }
    let store = match open_total_store(&store_path) {
        Ok(s) => s,
        Err(e) => {
            log::error!(target: tag::CLI, "open total store failed: {e}");
            return 1;
        }
    };
    match store.read_projection_changes(since_seq, limit) {
        Ok(changes) => {
            let mut last_seq = since_seq;
            for c in &changes {
                last_seq = c.seq;
                emit(&Out::ProjectionReplaced {
                    seq: c.seq,
                    at: c.at,
                    source_type: c.source_type.clone(),
                    source_location: c.source_location.clone(),
                    source_path: c.source_path.clone(),
                    old_source_revision: c.old_source_revision,
                    old_projection_revision: c.old_projection_revision,
                    new_source_revision: c.new_source_revision,
                    new_projection_revision: c.new_projection_revision,
                    reason: c.reason.clone(),
                });
            }
            emit(&Out::ChangesSummary {
                since_seq,
                last_seq,
                changes: changes.len() as u64,
                caught_up: changes.len() < limit,
            });
            0
        }
        Err(e) => {
            log::error!(target: tag::CLI, "read_projection_changes failed: {e}");
            1
        }
    }
}
