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
    /// 从不可变总库增量拉取 `--since` offset 之后的 `RawEvent`（NDJSON），供 TumeFlow
    /// 物化分库（P3-③ / §13.2）。**只读**总库（QuotaBar 是默认写者），游标由调用方持久化。
    /// 需 `store` feature（rusqlite）；未启用时本子命令不存在（clap 报未知子命令）。
    #[cfg(feature = "store")]
    Pull {
        /// 只拉 offset 严格大于此值的事件；首次全量同步传 `0`（默认）。
        #[arg(long, default_value_t = 0)]
        since: i64,
        /// 本轮最多吐多少条事件（`0` = 不限，一次拉到追平总库尾）。用于把大回填切成有界批次。
        #[arg(long, default_value_t = 0)]
        limit: u64,
        /// 总库路径（覆盖默认 `<data_local_dir>/svault/total_store.db`，与 QuotaBar 写者同址）。
        #[arg(long)]
        store: Option<PathBuf>,
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
            limit,
            store,
        } => run_pull(since, limit, store),
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
fn run_pull(since: i64, limit: u64, store_arg: Option<PathBuf>) -> i32 {
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
    let store = match session_vault::TotalStore::open(&store_path) {
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
        |cursor, want| store.read_since_page(cursor, want),
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
            message_id: None,
            request_id: None,
        }
    }

    /// 收集 `pull_stream` 的回调到 `(offset, RawEvent)` 列表，便于断言。
    fn collect(store: &TotalStore, since: i64, limit: u64) -> (Vec<(i64, RawEvent)>, bool) {
        let mut out = Vec::new();
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
                false,
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
                false,
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
    }
}
