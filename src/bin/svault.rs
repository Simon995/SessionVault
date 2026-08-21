//! `svault` CLI：SessionVault 的跨语言主接口（§12 / ADR-024）。
//!
//! **stdout = NDJSON 结果**（每行一条 JSON，供 TumeFlow 子进程消费）；
//! **stderr = 日志**（env_logger 自装 sink；库本身不装 sink，见 ADR-026）。
//! 日志级别：`SVAULT_LOG`（优先）/ `RUST_LOG`，默认 `info`。

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;

use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use session_vault::catalog::Profile;
use session_vault::cursor::Cursor;
use session_vault::logging::tag;
use session_vault::rawevent::{RawEvent, SourceLocation, SourceMode, SourceType};
// 门就是 `feature = "store"`：`run_scan_all` 的 `--write-store` 路径（task #44）现在
// 无条件用到 `Projection`，它在**所有** store 构建里都在。
//
// 📌 这里从前是三个使用点的**并集**（`run_fixture_append` / `mod tests` / 无），门写成
// `all(store, any(acceptance-fixtures+debug, test))`。那种门每加一个使用点就要重算一次，
// 而算错只在**特定 feature 组合**下现形（少了 ⇒ 某个组合编译失败；多了 ⇒ 另一个组合
// unused import 告警）。使用点覆盖到整个 store feature 之后，那个精算就该退场。
#[cfg(feature = "store")]
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
    ///
    /// 默认**只吐 NDJSON 事件流、不写总库**。带 `--write-store` 才写 —— 见该参数。
    ScanAll {
        /// 扫描 profile。**默认随 `--write-store` 变**：只吐流时 `metadata`，
        /// 写总库时 `full`（总库按 §13.1 以 full 物化，含正文）。
        #[arg(long, value_enum)]
        profile: Option<ProfileArg>,
        /// 游标状态文件路径（覆盖默认 `<data_local_dir>/svault/cursors.json`）。
        #[arg(long)]
        state: Option<PathBuf>,
        /// 无状态：忽略且不写状态文件，每次从头全量扫（调试/一次性用）。
        #[arg(long)]
        stateless: bool,
        /// 把扫到的事件**写进总库** —— Class-A 的写入口（TumeFlow task #44）。
        ///
        /// 🔴 **没有这个口的时候，写总库只能经 `TotalStore` 这个 Rust 库 API，
        /// 于是只有能直链 crate 的消费者做得到（实际上只有 QuotaBar）。** 后果是
        /// 「证据什么时候刷新」被绑在「哪个宿主在跑」上：只装 TumeFlow + TumeChat
        /// 的机器上总库没人写 ⇒ Class-A 恒为 0，而界面上什么都不说。
        /// 这与 `sync-snapshots`（Class-B 的写入口）是同一件事的另一半。
        ///
        /// ⚠️ 带上它之后**不再逐条吐 `event` 行**（一次全量是几个 GB 的 stdout，
        /// 而写库的调用方本来就要从库里读）。观测走每来源一条 `store_write`
        /// 加收尾的 `store_write_summary`，`source_report` 照常吐。
        ///
        /// ⚠️ **不为它加文件锁**：单写者由 SQLite 保证（`busy_timeout` +
        /// `BEGIN IMMEDIATE`，见 `store.rs::write_tx`）。再套一把文件锁会与它
        /// 各管一半，而两套锁的边界不重合时没有任何东西会报错（§13.1）。
        #[cfg(feature = "store")]
        #[arg(long)]
        write_store: bool,
        /// 总库路径（覆盖默认 `<data_local_dir>/svault/total_store.db`）。
        /// 只在 `--write-store` 下有意义；也用于读项目根注册表（归属的唯一输入）。
        #[cfg(feature = "store")]
        #[arg(long)]
        store: Option<PathBuf>,
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
    /// 枚举本机的 Class-B 状态制品（CLAUDE.md / AGENTS.md / 项目 memory）并把它们
    /// 的快照同步进总库 —— **写入侧的对外出口**。
    ///
    /// 🔴 没有这个口的时候，同步只能经 `TotalStore::sync_snapshots` 这个 Rust 库
    /// API，于是**只有能直链 crate 的消费者做得到**（实际上只有 QuotaBar）。后果：
    /// 「记忆的证据层什么时候刷新」被绑在「那个宿主有没有在跑」上 —— 只装
    /// TumeChat 的用户永远刷不到，而界面上什么都不会说。
    ///
    /// ⚠️ 输出里的 `unreachable` **不是**「那里没有素材」。一个权限拒绝或停掉的
    /// WSL 发行版会让整个位置问不成，而 `sync_snapshots` 把「本轮没出现的来源」
    /// 当作已消失处理 —— 调用方必须据此决定要不要信任这一轮。
    #[cfg(feature = "store")]
    SyncSnapshots {
        #[arg(long)]
        store: Option<PathBuf>,
    },
    /// 总库记下的项目根（ADR-050 注册表）—— **项目身份的唯一对外出口**。
    ///
    /// 没有这个口的时候，只有能直链 Rust crate 的消费者拿得到注册表，走 CLI 的那些
    /// 只能自己再发现一遍。实测的后果：同一个项目在记忆库里存成两个身份，各自
    /// 持有一半的记忆且互相看不见。
    ///
    /// 🔴 **不做命名空间翻译。** 给出的是注册表里的原始形式（可能是 `C:\…`，也可能
    /// 是 `wsl:<distro>:/…`）。翻译成「消费方能打开的物理路径」是它自己的事 ——
    /// 在这里替它猜，等于把宿主视野的假设烧进一个跨进程接口。
    #[cfg(feature = "store")]
    Roots {
        #[arg(long)]
        store: Option<PathBuf>,
    },
    /// 这台机器上有哪些**记忆根**（`~/.claude` + `~/.codex` 对），含每个 WSL 发行版。
    ///
    /// 与 `roots` 是两件事，别混：`roots` 回答「**事件被归因到**哪些项目根」（读总库）；
    /// 本命令回答「**agent 的记忆装在哪些 home 里**」（探这台机器）。前者是历史，
    /// 后者是现状，两者的集合可以完全不同。
    ///
    /// 存在的理由：这条规则此前只在 QuotaBar 里有一份，TumeFlow 够不到，于是它在
    /// 拿不到宿主传参时**回落到自己那一个 local 根** —— 「这台机器只有本机根」与
    /// 「宿主没告诉我」在调用点长得一模一样，WSL 里的记忆被静默漏掉。
    ///
    /// 🔴 **输出含 `unreachable` 行，消费方不得把它读作「那里没有根」。**
    /// 一个卡住的 WSL 与一台没装 WSL 的机器**必须**能被分开 —— 上游那份是
    /// `list_distros().unwrap_or_default()`，两者返回完全相同的东西。
    MemoryRoots {
        /// 覆盖 `%USERPROFILE%`（测试用；不传则读环境变量）。
        #[arg(long)]
        userprofile: Option<String>,
        /// 整轮预算上限（秒）。每次 `wsl.exe` 调用从中扣，耗尽即不再发起。
        #[arg(long, default_value = "60")]
        timeout_secs: u64,
    },
    /// 打印本机总库的默认路径 —— 就这一件事。
    ///
    /// 为什么需要它：消费方**不能自己推这个路径**。它是
    /// `dirs_next::data_local_dir()/svault/total_store.db`，而 Python 对
    /// 「data_local_dir」的理解与 Rust 的 `dirs_next` 不一致 —— 各推各的就是又一处
    /// 跨语言「必须推出同一个值」的接缝（ADR-045 那个 `<user>` 标签是它的判例）。
    ///
    /// 多数命令不需要它：不传 `--store` 时它们各自默认到同一个地方。需要它的是
    /// **必须显式指名**的那些 —— 比如不可逆的擦除，它的确认框应当告诉用户
    /// 「这还会从哪个库里删」。
    ///
    /// ⚠️ 只回答「默认在哪」，**不保证那儿有文件**。存不存在是另一个问题，
    /// 由调用方自己探 —— 在这里替它探，等于把两个答案压进一个出口。
    StorePath,
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
    /// `store-path` 的唯一一行：本机总库的默认路径。
    StorePath { path: &'a str },
    /// `memory-roots` 的一行：一个 agent home 对 + 宿主到它的前缀。
    MemoryRoot {
        location: &'a str,
        claude_home: &'a str,
        codex_home: &'a str,
        fs_prefix: &'a str,
    },
    /// 🔴 某个位置**没问成** —— 消费方不得把它读作「那里没有根」。
    ///
    /// 与根走同一个流是有意的：分成两个流（或只往 stderr 记一句）等于赌消费方会去
    /// 看另一处，而它多半不会 —— 本仓已有判例（sidecar 的写盘失败挪进 `daemon.log`
    /// 之后，宿主再没读过那行，坏了三天没人知道）。
    MemoryRootUnreachable { location: &'a str, reason: &'a str },
    /// `memory-roots` 收尾摘要。**`unreachable > 0` 时 `roots` 是一份不完整的答案。**
    MemoryRootsSummary { roots: usize, unreachable: usize },
    /// 一次 Class-B 同步的结果。**`unreachable` 与「没有素材」是两件事**，
    /// 所以它是独立字段而不是「更短的 sources 列表」。
    ///
    /// 与它的唯一构造点 `run_sync_snapshots` 同门 —— 不门控则不带 `store` 的构建里
    /// 它是死变体（`dead_code` 告警）。
    #[cfg(feature = "store")]
    SyncSnapshotsSummary {
        /// 这一轮**枚举到**的来源数。
        sources: usize,
        /// 内容变了、写进新一版快照的来源数。
        changed: u64,
        /// 内容没变、跳过的。
        unchanged: u64,
        /// 🔴 **读失败的来源** —— 与 `unreachable` 不同：那个是「位置/项目根问不成」，
        /// 这个是「文件在，但读它炸了」。两者都不是「没有素材」。
        failed: u64,
        /// 写进总库的事件条数。
        appended: u64,
        unreachable: usize,
    },
    /// 没问成的那些，逐条报出来 —— 一个只给计数的摘要说不出「是哪个位置」。
    #[cfg(feature = "store")]
    SyncUnreachable { reason: &'a str },
    SourceReport {
        report: &'a session_vault::report::SourceReport,
    },
    Summary {
        sources: usize,
        events: u64,
        /// 游标状态是否成功落盘：`Some(true/false)`；`None` = stateless（未持久化）。
        /// `false` 时进程以非 0 退出——下游据此知道本轮增量游标**未推进**，需重试或预期重复。
        state_saved: Option<bool>,
        /// 这一轮**打不打算**写总库（`--write-store`）。
        ///
        /// 🔴 与「写了 0 条」是两件事，所以它是独立字段而不是「`written_events` 恰为 0」。
        /// 一个只报数字的摘要说不出「这轮压根没往库里写」—— 而那正是 Class-A 恒为 0
        /// 时最需要先分清的一件事。
        wrote_store: bool,
        /// 真正写进库的事件条数。`null` = 这轮不写库（不是「写了 0 条」）。
        written_events: Option<u64>,
        /// 有几个来源没写进去。`> 0` 时本轮**不完整**且进程以 3 退出。
        write_failures: Option<u64>,
        /// 计划说「这一轮别写」的来源数：没读成 / 主动拒绝坏行 / 降级到零好行。
        ///
        /// 🔴 **与 `write_failures` 分开**：那是我们想写而没写成（要重试），
        /// 这是我们**决定**不写（下轮自然重来）。压成一个数就没法判断该不该报警。
        held_sources: Option<u64>,
        /// 枚举到、但**不由这条路写**的快照来源数。
        ///
        /// Class-B 的写入口是 `sync-snapshots`（另一套枚举 + 另一套变更检测）。
        /// 报出来是因为「这轮写了 0 条快照」不该被读成「本机没有快照来源」。
        snapshot_sources: Option<u64>,
        /// 🔴 项目根注册表是空的 ⇒ 本轮每条事件的 `project_root` 都是 `Unattributed`。
        /// 它是个**说得出口但没用**的答案，与「这台机器上确实没有项目」长得一样，
        /// 所以进摘要而不只是一行日志。
        roots_empty: bool,
    },
    /// `scan-all --write-store`：一个来源的事件落库了。
    ///
    /// `mode` 与总库 `projections.origin` 记的理由是**同一个词**
    /// （复用 `Projection::origin_key`），所以线上的 `mode` 可以直接和库里的台账对上。
    #[cfg(feature = "store")]
    StoreWrite {
        source_path: &'a str,
        /// `append` / `rollback` / `reparse`。
        mode: &'static str,
        /// `clean` / `degraded` / `poison_line` / `unknown` —— 与 QuotaBar 写进
        /// `record_sync_outcome` 的**同一组串**（同一个 `QualityState::key`）。
        quality: &'static str,
        appended: u64,
        /// 库里已有、被 `INSERT OR IGNORE` 跳过的 —— 与宿主并行扫同一批文件时这个数很大，
        /// **那是正常的**（两个写者各自扫、去重收口），不是错误。
        skipped_dup: u64,
        /// 命中墓碑（用户删过）而被拒的 —— 与 `skipped_dup` 分开，两者处置完全不同。
        skipped_erased: u64,
        /// 头是否指向了新的 `(source_revision, projection_revision)`。
        /// `false` + `mode != append` = 这次被 token 幂等短路了（同一次操作的重放）。
        head_moved: bool,
        superseded_removed: u64,
        /// `[before, after]` = 新投影比被它取代的那份少了事件。此时旧投影**没删**。
        #[serde(skip_serializing_if = "Option::is_none")]
        loses_events: Option<[u64; 2]>,
    },
    /// 🔴 一个来源**没**写进库。它必须是独立的一行，而不是让 `store_write` 缺席 ——
    /// 「没出现」在 NDJSON 里读作什么，取决于消费方记不记得去数，而它多半不会。
    #[cfg(feature = "store")]
    StoreWriteFailed {
        source_path: &'a str,
        reason: &'a str,
    },
    /// 计划说**这一轮不动这个来源的投影**（`CommitPlan` 给出 `StoreAction::Preserve`）。
    ///
    /// 三种情形共用这一行，靠 `quality` 分辨 —— 它们的处置对用户完全不同：
    /// `unknown` = 没读成（等一等 / 查权限），`poison_line` = 读到了但我们主动拒绝
    /// 这一批（去看那一行），`degraded` = 有坏行且**一个好行都没剩**（整代替换会
    /// 用空的覆盖非空的，所以宁可不动）。
    ///
    /// 🔴 它**不是**失败，不进退出码：三种都会在下一轮从同一个偏移重来。
    /// 报成失败会让常态运行里全是红叉，而红叉多了就没人看了。
    #[cfg(feature = "store")]
    StoreHeld {
        source_path: &'a str,
        quality: &'static str,
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<&'a str>,
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
    /// `roots` 的一行。
    #[cfg(feature = "store")]
    ProjectRoot {
        root_key: String,
        root_path: String,
        root_source: String,
        first_seen_ms: i64,
        last_seen_ms: i64,
        /// 同一个根的其它等价写法 —— 消费方手上的路径未必是注册表存的那种形式。
        /// 见 `store::ProjectRootRow::aliases`（含「为什么 `/mnt/…` 不在里面」）。
        aliases: Vec<String>,
        /// 跨系统身份（git origin 归一化）。**与 `aliases` 收敛的是两件不同的事**：
        /// 别名管「同一条路径的不同写法」，这个管「不同路径上的同一个 repo」
        /// （Windows 一份 checkout + WSL 一份）。见 `store::ProjectRootRow::canonical_id`。
        canonical_id: Option<String>,
        /// 这个根在 Claude 侧**可能**的 `projects/<enc>` 目录名（每种写法各一个）。
        ///
        /// 🔴 **给出来，消费方就不必解码。** 解码要探盘消歧（`-` 既可能是分隔符
        /// 也可能是名字的一部分），而编码是纯字符串变换 —— 把问题倒过来之后，
        /// 「哪个目录属于哪个项目」变成一次**存在性检查**。TumeFlow 曾自己实现
        /// 解码，与 Rust 那份漂开过（2026-08-14 实测）。
        claude_project_dirs: Vec<String>,
        /// 宿主**能打开**的那个写法；`null` = 给不出。
        ///
        /// 🔴 规范形 `wsl:<d>:/…` 是标识符不是路径；裸 Linux 路径在 Windows 上会被
        /// 当成当前盘的相对路径（**打开错的东西**，比报错更坏）。所以这里宁可给
        /// `null` 也不拿它们冒充。
        host_path: Option<String>,
        /// 身份探测的结论 —— 回答 `canonical_id` 为 `null` 时**为什么**没有。
        ///
        /// `not_probed`（还没扫到，**等**）／`resolved`（问到了）／
        /// `no_identity`（**确认**不属于任何仓，**接受**）／
        /// `unresolved`（**没问成**，**重试**，且**绝不据此做删除类决定**）。
        ///
        /// 🔴 上面那三个 `null` 从前长得一模一样（本机 20 个根：1 个是「确认没有」、
        /// 3 个是「没问成」）—— 见 `store::IdentityVerdict`。
        identity_verdict: &'a str,
        /// 判决的理由；`resolved` / `not_probed` 没有。
        identity_detail: Option<String>,
    },
    /// `roots` 的收尾摘要。
    ///
    /// 🔴 `attribution_revision` 是消费方的**缓存失效锚**，与上面那些行来自同一次
    /// 持锁读（见 `TotalStore::project_roots_report`）。没有它，消费方要么每次全量
    /// 重算，要么用一份自己也说不清有没有过期的缓存。
    #[cfg(feature = "store")]
    RootsSummary {
        roots: usize,
        attribution_revision: i64,
    },
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
        #[cfg(feature = "store")]
        Command::ScanAll {
            profile,
            state,
            stateless,
            write_store,
            store,
        } => run_scan_all(profile, state, stateless, write_store, store),
        #[cfg(not(feature = "store"))]
        Command::ScanAll {
            profile,
            state,
            stateless,
        } => run_scan_all(profile, state, stateless, false, None),
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
        Command::SyncSnapshots { store } => run_sync_snapshots(store),
        #[cfg(feature = "store")]
        Command::Roots { store } => run_roots(store),
        Command::StorePath => run_store_path(),
        Command::MemoryRoots {
            userprofile,
            timeout_secs,
        } => run_memory_roots(userprofile, timeout_secs),
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
/// 枚举 Class-B 来源并同步进总库。
///
/// 判据与 `run_snapshots`（读侧）一致：**先把「没问成」说出来，再报数字**。
/// 一份漂亮的 `inserted` 配着三个没问成的位置，与一份同样漂亮的、什么都没漏的，
/// 在只看数字时长得一模一样。
#[cfg(feature = "store")]
fn run_sync_snapshots(store_arg: Option<PathBuf>) -> i32 {
    let Some(store_path) = resolve_store_path(store_arg) else {
        log::error!(target: tag::CLI, "no data_local_dir; pass --store");
        return 1;
    };
    let found = session_vault::class_b::enumerate();
    for reason in &found.unreachable {
        emit(&Out::SyncUnreachable { reason });
    }
    // ⚠️ 库那侧建库是幂等的，这里**不**要求库已存在：第一次同步就该能把库建起来，
    // 否则「装完还没扫过」会变成一个需要另一个程序先跑一遍的死结。
    let store = match open_total_store(&store_path) {
        Ok(store) => store,
        Err(e) => {
            log::error!(target: tag::CLI, "snapshot store open failed: {e}");
            return 2;
        }
    };
    let stats = match store.sync_snapshots(&found.sources) {
        Ok(stats) => stats,
        Err(e) => {
            log::error!(target: tag::CLI, "snapshot sync failed: {e}");
            return 2;
        }
    };
    emit(&Out::SyncSnapshotsSummary {
        sources: found.sources.len(),
        changed: stats.changed,
        unchanged: stats.unchanged,
        failed: stats.failed,
        appended: stats.appended,
        unreachable: found.unreachable.len(),
    });
    0
}

#[cfg(feature = "store")]
fn run_snapshots(store_arg: Option<PathBuf>) -> i32 {
    let Some(store_path) = resolve_store_path(store_arg) else {
        log::error!(target: tag::CLI, "no data_local_dir; pass --store");
        return 1;
    };
    if let Some(code) = bail_unless_store_present(&store_path, 1) {
        return code;
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
                // `discover` 只枚举来源、不读内容、不写库，写库那几格都是「不适用」。
                wrote_store: false,
                written_events: None,
                write_failures: None,
                held_sources: None,
                snapshot_sources: None,
                roots_empty: false,
            });
            0
        }
        Err(e) => {
            log::error!(target: tag::CLI, "discover failed: {e}");
            1
        }
    }
}

/// `scan-all` 的 profile 决议：默认随 `--write-store` 变，而**显式的 `metadata`
/// 与写库互斥**——那是一次拒绝，不是一次降级。
///
/// 🔴 互斥的理由是去重键。总库的唯一键是
/// `(source_type, source_location, source_path, source_session_id, seq)`
/// ——**不含正文**（§13 / `SYSTEM_DESIGN` §9.4）。`Profile::Metadata` 产出的事件
/// `content` 恒为 `None`，而 seq 与 `Full` 逐一相同。于是先写进去一轮 metadata，
/// 之后同一批 `Full` 事件会被 `INSERT OR IGNORE` **全部当成重复丢弃**，且不报错：
/// 症状是「会话在库里、条数也对、就是永远没有正文」。
///
/// 而总库是 **append-only、永不删**（§13.1），所以这一步**不可逆** ——
/// 唯一的补救是给那些文件开一代新投影，而调用方根本不会知道要去补。
/// 「写下去看起来成功」正是要防的那种失败，因此宁可在参数这一层就拒绝。
fn resolve_scan_profile(profile: Option<ProfileArg>, write_store: bool) -> Result<Profile, String> {
    match (profile, write_store) {
        (Some(ProfileArg::Metadata), true) => Err(
            "--write-store 要求 --profile full：总库的去重键不含正文，metadata 事件的 seq \
             与 full 逐一相同 ⇒ 先写进去就会把之后的正文永久挡在 INSERT OR IGNORE 外面。\
             去掉 --profile 即可（写库时默认就是 full）"
                .to_string(),
        ),
        (Some(p), _) => Ok(p.into()),
        // 写库按 §13.1「总库以 full 物化（含正文）」；只吐流时保持原默认 metadata。
        (None, true) => Ok(Profile::Full),
        (None, false) => Ok(Profile::Metadata),
    }
}

/// 一个来源扫完之后手上的东西。
///
/// 🔴 **append-log 保留完整观察，不先降级成 `ScanResult`。** `ScanStatus` 三个变体
/// 压掉了四种含义，而写库要用的恰好是被压掉的那些：「文件消失」要整个跳过、
/// 「主动拒绝坏行」要冻游标、「降级到一个好行都没有」不能整代替换。
/// `scan.rs` 自己的注释写着「**新消费方走 `scan_append_log_observed`**」——
/// `--write-store` 就是新消费方，而且是写库的那一种。
///
/// ⚠️ 其余形态**没有「观察」这个概念**，不能硬凑：快照的失败态是「无效 UTF-8」，
/// 塞进 `Unavailable` 会让「内容无效」伪装成「读不到」。
///
/// ⚠️ 事件只存一份（观察是 move 进来的）。`AppendLogObservation` 的注释立过这条：
/// events 存两份会让「改了一份忘了另一份」成为可能。
enum Scanned {
    Observed(session_vault::observation::AppendLogObservation),
    Projected {
        events: Vec<RawEvent>,
        cursor_out: Cursor,
    },
}

impl Scanned {
    fn events(&self) -> &[RawEvent] {
        match self {
            Self::Observed(o) => &o.events,
            Self::Projected { events, .. } => events,
        }
    }

    /// 本轮**全读**时算出的源指纹。
    ///
    /// `None` = 这一轮是增量、没读全文，**算不出** —— 不是「内容没变」。
    /// 下一轮把它传回去（`prior_fingerprint`）才认得出「同尺寸原地重写」：
    /// 那种改动 `size` 与 `mtime` 都可能一字不变。
    fn fingerprint(&self) -> Option<&session_vault::observation::SourceFingerprint> {
        match self {
            Self::Observed(o) => o.source_fingerprint.as_ref(),
            Self::Projected { .. } => None,
        }
    }
}

/// 这个来源欠不欠一次重投影（解析器版本变了 ⇒ 同一份字节现在能读出别的东西）。
///
/// 🔴 判据是「**记下过**一个不同的版本」。`recorded == None` 必须是 `false`：
/// 没有记录 = 这条路还没写过库（旧格式状态文件，或第一次带 `--write-store` 跑）。
/// 那时不该声称欠账 —— `CommitPlan` 会因 `has_prior` 落到 `Append`，靠 seq 把库里
/// 已有的（宿主写的那份）原样跳过。谎称欠账则会开一个新投影版本并**把宿主写的那份
/// 删掉**（`Reparse` 的旧投影按定义可回收），而调用方只会看到一份漂亮的统计。
///
/// ⚠️ 只有写库那条路谈得上重投影：不写库时我们不动总库，没有「哪一代」这回事。
fn owes_reprojection(write_store: bool, recorded: Option<u32>, current: u32) -> bool {
    write_store && recorded.is_some_and(|rev| rev != current)
}

/// 本轮**为什么**扫这个来源 —— 它同时决定「读多少」与「怎么写」，所以只算一次。
///
/// CLI 这侧只可能有两条理由。`FORCE` / `ATTRIBUTION_STALE` / `SYNC_DEBT` 是宿主才有的
/// 概念（用户点刷新、注册表长新根、两个物化层的欠账），这里表达不出来也不该假装有。
///
/// 🔴 **`INITIAL` 的判据是「库里没有前代」，不是「本地没有游标」。** 这一位的含义是
/// 「这个来源要整代重建」，而那是**库**的事：
///
/// | 手上有游标 | 库里有前代 | 判 | 为什么 |
/// | --- | --- | --- | --- |
/// | 否 | 是 | 不是首次 | 宿主已经写过了，我们只是第一次跑 —— 续读即可，靠 seq 去重 |
/// | 是 | 否 | **是首次** | 两层不一致（此前只吐流地跑过，或库被重建过）⇒ **必须从 0 读**，否则游标之前的事件永远漏在库外，而摘要只显示「本轮 0 条新增」 |
///
/// `has_prior == None` = 这一轮不写库，谈不上「哪一代」⇒ 没有任何理由。
#[cfg(feature = "store")]
fn scan_reasons(
    parser_stale: bool,
    has_prior: Option<bool>,
) -> session_vault::scan_plan::ScanReasons {
    use session_vault::scan_plan::ScanReasons;
    let mut r = ScanReasons::NONE;
    if has_prior == Some(false) {
        r |= ScanReasons::INITIAL;
    }
    if parser_stale {
        r |= ScanReasons::PARSER_STALE;
    }
    r
}

/// 本轮扫完之后，这个来源的状态该记成什么。
///
/// 三个字段各带一条判据，**每一条错了都不会报错，只会在很久以后变成坏数据**：
///
/// - `cursor`：照记（即便本轮无新增也写回，刷新 size/mtime）。
/// - `parser_revision`：🔴 **只有真的写过库才记。** 它的含义是「这个来源**在库里的
///   投影**是哪个解析器产出的」。不写库的运行记上它，日后第一次 `--write-store`
///   会误判成「不欠重投影」，于是新解析器的结果永远进不去（同 seq 被去重丢弃）。
/// - `fingerprint`：🔴 **增量轮次要保留上一版，不能覆盖成 `None`。** 指纹只在全读时
///   算得出（`scan.rs`：「全读才算得出整份内容的指纹」），覆盖成 `None` 等于每做一次
///   增量就把「上一版内容是什么」忘掉一次 —— 而下一次全读正需要它来认出
///   「同尺寸原地重写」，那种改动 size 与 mtime 都可能一字不变。
fn next_state(scanned: Scanned, prev: Option<&SourceState>, write_store: bool) -> SourceState {
    let fingerprint = scanned
        .fingerprint()
        .map(|f| StoredFingerprint {
            hash: f.as_str().to_string(),
            covered_len: f.covered_len(),
        })
        .or_else(|| prev.and_then(|p| p.fingerprint.clone()));
    SourceState {
        parser_revision: write_store.then_some(session_vault::PARSER_REVISION),
        fingerprint,
        cursor: match scanned {
            Scanned::Observed(o) => o.cursor,
            Scanned::Projected { cursor_out, .. } => cursor_out,
        },
    }
}

/// 扫一个来源。**append-log 走权威入口并带上一版指纹**，其余走既有的 `scan_source`。
fn scan_one(
    s: &SourceRef,
    cursor_in: Option<Cursor>,
    prior_fingerprint: Option<session_vault::observation::SourceFingerprint>,
    profile: Profile,
    roots: std::sync::Arc<session_vault::attribution::RootRegistry>,
) -> (Scanned, session_vault::report::SourceReport) {
    match s.source_mode {
        SourceMode::AppendLog => {
            let (obs, report) = session_vault::scan::scan_append_log_observed(
                s,
                cursor_in,
                prior_fingerprint,
                profile,
                roots,
                session_vault::deadline::Deadline::unbounded(),
            );
            (Scanned::Observed(obs), report)
        }
        _ => {
            let res = session_vault::scan(s, cursor_in, profile, roots);
            (
                Scanned::Projected {
                    events: res.events,
                    cursor_out: res.cursor_out,
                },
                res.report,
            )
        }
    }
}

/// 写总库时的句柄：库 + 归属修订号。
///
/// `attribution_revision` 一轮只读一次：它是 [`ProjectionToken`] 的一个分量，
/// 而同一轮扫描里的所有来源本就该用同一份注册表算归属。逐来源重读会让同一轮里
/// 前后两个来源拿到不同的修订号 —— 两次内容相同的操作因此算出不同的 token。
///
/// [`ProjectionToken`]: session_vault::token::ProjectionToken
#[cfg(feature = "store")]
struct ScanWriter {
    store: session_vault::TotalStore,
    attribution_revision: i64,
}

/// 一个来源这一轮在总库上发生了什么。
#[cfg(feature = "store")]
#[derive(Debug)]
enum Committed {
    /// 计划说**这一轮什么都别写**（读失败 / 主动拒绝坏行 / 降级到零好行）。
    /// 游标一并冻住 —— 这三种都要下轮从同一个偏移重来。
    Preserved,
    Wrote {
        mode: Projection,
        stats: session_vault::store::ProjectionStats,
    },
}

/// 来源在总库里的身份键。
#[cfg(feature = "store")]
fn source_key_of(source: &SourceRef) -> session_vault::store::SourceKey {
    session_vault::store::SourceKey {
        source_type: source.source_type,
        source_location: source.source_location.clone(),
        source_path: source.path.display().to_string(),
    }
}

#[cfg(feature = "store")]
impl ScanWriter {
    /// 总库里**已经有这个来源的投影了吗**。
    ///
    /// 🔴 **在扫之前问，因为它决定这一轮读多少。** 状态文件里有游标、而库里没有投影
    /// ⇒ 两层不一致（此前只吐流地跑过、或库被重建过）。那时从游标处续读，
    /// **游标之前的那些事件永远不会再被扫到**，而摘要只会显示「本轮 0 条新增」——
    /// 与「确实没有新会话」逐字相同。这正是 #44 要治的病本身，不能在修它的路上重造一个。
    ///
    /// 🔴 **不拿本地游标当代理**：两者是两套状态，一个在调用方手里、一个在库里。
    /// 第一次带 `--write-store` 跑时手上一个游标都没有，而库里可能早已被常驻宿主写满。
    fn has_prior(&self, source: &SourceRef) -> Result<bool, String> {
        self.store
            .has_projection(&source_key_of(source))
            .map_err(|e| format!("has_projection failed: {e}"))
    }

    /// 按 [`CommitPlan`] 把一个来源本轮的观察落进总库。
    ///
    /// 🔴 **四个决定不在这里重算，一律问 `CommitPlan`。** 它在 QuotaBar 那边被 780 条
    /// 测试逼出过两格（`degraded_and_empty` 那格是「用空的替换非空的」，`has_prior`
    /// 那格是「开一个没有前代可取代的空代」），照抄一份必然漂 —— 而漂开之后
    /// **两个写者对同一份字节做出不同的投影决定，且没有任何东西会报错**。
    /// 2026-08-21 它已从 QuotaBar 移进本仓，正是为了这件事。
    ///
    /// [`CommitPlan`]: session_vault::scan_plan::CommitPlan
    fn commit(
        &self,
        source: &SourceRef,
        obs: &session_vault::observation::AppendLogObservation,
        reasons: session_vault::scan_plan::ScanReasons,
        has_prior: bool,
    ) -> Result<(Committed, session_vault::scan_plan::CommitPlan), String> {
        use session_vault::scan_plan::{CommitPlan, StoreAction};

        let source_key = source_key_of(source);
        let plan = CommitPlan::plan(obs, reasons, has_prior);

        let mode = match plan.store() {
            StoreAction::Preserve => return Ok((Committed::Preserved, plan)),
            StoreAction::Project(mode) => mode,
        };

        // 🔴 `Reparse` / `Rollback` 要取代**一整代**，所以它们必须配一次全读。
        //
        // 拿增量的尾巴去「取代」，会把这个文件的当前投影换成只剩尾巴的那一份 ——
        // 前面的事件当场从库里消失，而统计看起来完全正常（appended 有数、
        // head_moved 为真）。`source_fingerprint.is_some()` **等价于**本轮 start==0
        // （`scan.rs`：「全读才算得出整份内容的指纹」），所以它是这件事的正向判据，
        // 不是又一个需要人去维护的 bool。
        if mode != Projection::Append && obs.source_fingerprint.is_none() {
            return Err(format!(
                "refusing to {} on an incremental read (no full-file fingerprint) — \
                 replacing a generation with only the tail would drop everything before it",
                mode.origin_key()
            ));
        }

        // 🔴 `Append` 不传 token（靠 seq 去重，重放天然幂等）；**开新代的两种一律要传**，
        // 否则一次崩溃重放就多留一代，而 `Rollback` 那代按设计永不自动回收（ADR-051 I7）。
        let token = match mode {
            Projection::Append => None,
            Projection::Rollback | Projection::Reparse => {
                Some(session_vault::token::ProjectionToken::new(
                    &source_key,
                    obs.source_fingerprint.as_ref().map(|f| f.as_str()),
                    session_vault::PARSER_REVISION,
                    self.attribution_revision,
                    // 全读 ⇒ 字节范围从 0 起。上面刚断言过这一点。
                    (0, obs.cursor.safe_offset),
                ))
            }
        };
        self.store
            .apply_projection(session_vault::store::FileProjectionBatch {
                source: source_key,
                parser_revision: Some(session_vault::PARSER_REVISION),
                mode,
                token,
                events: obs.events.clone(),
            })
            .map(|stats| (Committed::Wrote { mode, stats }, plan))
            .map_err(|e| e.to_string())
    }
}

fn run_scan_all(
    profile_arg: Option<ProfileArg>,
    state_arg: Option<PathBuf>,
    stateless: bool,
    write_store: bool,
    store_arg: Option<PathBuf>,
) -> i32 {
    let profile = match resolve_scan_profile(profile_arg, write_store) {
        Ok(p) => p,
        Err(why) => {
            log::error!(target: tag::CLI, "{why}");
            return 1;
        }
    };

    // 🔴 **先开库，再扫。** 反过来的话，一轮几分钟的全量扫描跑完才发现库打不开 ——
    // 那一轮的读全白做，而且游标没推进，下一轮还得重来。
    #[cfg(feature = "store")]
    let writer = if write_store {
        match open_scan_writer(store_arg.clone()) {
            Ok(w) => Some(w),
            Err(code) => return code,
        }
    } else {
        None
    };

    let sources = match session_vault::discover() {
        Ok(s) => s,
        Err(e) => {
            log::error!(target: tag::CLI, "discover failed: {e}");
            return 1;
        }
    };

    // 状态：source_key → 游标 + 产出它的解析器版本 + 上一版源指纹。stateless 时为空 map。
    let state_path = if stateless {
        None
    } else {
        resolve_state_path(state_arg)
    };
    let mut cursors: HashMap<String, SourceState> = match &state_path {
        Some(p) => load_cursors(p),
        None => HashMap::new(),
    };

    // 归属的唯一输入。读不出来就是空注册表 ⇒ 一致地 `Unattributed`，**不退回 cwd**。
    // 🔴 空表要说出来：一份静默为空的注册表会让整轮扫描的 `project_root` 全成兜底值，
    // 而那与「本机确实一个项目根都没发现」在输出里长得一模一样。
    let roots = std::sync::Arc::new(project_roots(store_arg));
    let roots_empty = roots.is_empty();
    if roots_empty {
        log::warn!(
            target: tag::CLI,
            "project root registry is empty — every path will be Unattributed"
        );
        // 🔴 写库时这句更重：**归属会跟着事件一起落库**，不再只是流里的一个字段。
        //
        // 它可恢复（注册表补齐后开一代 `Reparse` 重算），但没人会知道要去补 ——
        // 所以它进摘要（`roots_empty`），而不只是一行日志。
        //
        // ⚠️ 本机之所以不空，是因为**注册表只有 QuotaBar 在写**
        // （`session_index.rs::discover_project_roots`）。只装 TumeFlow 的机器上它恒空。
        // 那是 #44 的另一半，本轮不做，见 TumeFlow `docs/BACKLOG.md`。
        if write_store {
            log::warn!(
                target: tag::CLI,
                "…and these events are being written to the total store — \
                 attribution will need a Reparse once the registry is populated"
            );
        }
    }

    let mut total_events = 0u64;
    // 不带 `store` feature 编译时 `--write-store` 这个参数根本不存在，计数器恒为 0。
    // 用 `cfg_attr` 精确关掉那一个组合的 lint，而不是无条件 `allow` —— 后者会连
    // store 构建里真正的「写了但没人读」也一起盖住。
    #[cfg_attr(not(feature = "store"), allow(unused_mut))]
    let mut written_events = 0u64;
    #[cfg_attr(not(feature = "store"), allow(unused_mut))]
    let mut write_failures = 0u64;
    #[cfg_attr(not(feature = "store"), allow(unused_mut))]
    let mut held_sources = 0u64;
    #[cfg_attr(not(feature = "store"), allow(unused_mut))]
    let mut snapshot_sources = 0u64;

    for s in &sources {
        let key = source_key(s);
        let prev = cursors.get(&key).cloned();

        // 🔴 **写库时先问库：这个来源有没有前代 —— 在扫之前问，因为它决定读多少。**
        //
        // 问不出来就**整个跳过这个来源**（游标不推，下轮重来），不猜。猜 `false`
        // 会让下面走全读 + `Append`，看起来无害，但那是拿一次探测失败换一轮全量重读；
        // 猜 `true` 更糟：库里其实没有前代时，续读会把游标之前的事件永久漏在库外。
        #[cfg(feature = "store")]
        let has_prior = match writer.as_ref() {
            None => None,
            Some(w) => match w.has_prior(s) {
                Ok(v) => Some(v),
                Err(why) => {
                    write_failures += 1;
                    log::error!(target: tag::CLI, "cannot tell whether {key} has a prior projection: {why}");
                    let path = s.path.display().to_string();
                    // 这个来源**连扫都没扫**，所以没有 `source_report` 与它配对。
                    // 报出来的理由要说清是哪一步失败的 —— 只贴一条 sqlite 错误，
                    // 读的人会以为是写库时炸的，去查错的地方。
                    let reason = format!("cannot tell whether it has a prior projection: {why}");
                    emit(&Out::StoreWriteFailed {
                        source_path: &path,
                        reason: &reason,
                    });
                    continue;
                }
            },
        };
        #[cfg(not(feature = "store"))]
        let has_prior: Option<bool> = None;

        let parser_stale = owes_reprojection(
            write_store,
            prev.as_ref().and_then(|p| p.parser_revision),
            session_vault::PARSER_REVISION,
        );

        if parser_stale {
            log::info!(
                target: tag::CLI,
                "reprojecting {key}: state was parser rev {:?}, now {}",
                prev.as_ref().and_then(|p| p.parser_revision),
                session_vault::PARSER_REVISION
            );
        }
        if has_prior == Some(false) && prev.is_some() {
            log::info!(
                target: tag::CLI,
                "backfilling {key}: state has a cursor but the store has no projection — reading from 0"
            );
        }
        #[cfg(feature = "store")]
        let reasons = scan_reasons(parser_stale, has_prior);

        // 🔴 **「读多少」与「怎么写」用同一个 `reasons`。**
        //
        // `Reparse` 要取代**一整代**：拿增量的尾巴去取代，会把这个文件的当前投影换成
        // 只剩尾巴的那一份。所以扫之前就得决定全读，而那个判断必须与 planner 用的是
        // **同一个谓词** —— 各算一遍就是「两个真相源，一个加了条件另一个没有」，
        // 正是 `ScanReasons::SYNC_DEBT` 的注释记下的那个缺陷。
        // （`ScanWriter::commit` 里还有一道正向断言兜底：没有全文指纹就拒绝开新代。）
        #[cfg(feature = "store")]
        let full_read = reasons.wants_full_read() || reasons.wants_reparse();
        #[cfg(not(feature = "store"))]
        let full_read = false;

        let cursor_in = if full_read {
            None
        } else {
            prev.as_ref().map(|p| p.cursor.clone())
        };

        // 上一版全文指纹 —— 认「同尺寸原地重写」的唯一线索（`size`/`mtime` 都检不出）。
        // 🔴 **不用 `cursor.content_hash` 代替**：它没有 `covered_len`，而少了覆盖长度，
        // 一次纯追加就会被判成原地重写（`SourceFingerprint` 的注释记着那个缺陷）。
        let prior_fingerprint = prev.as_ref().and_then(|p| p.fingerprint.as_ref()).map(|f| {
            session_vault::observation::SourceFingerprint::from_stored(
                f.hash.clone(),
                f.covered_len,
            )
        });

        let (scanned, report) = scan_one(s, cursor_in, prior_fingerprint, profile, roots.clone());
        total_events += report.events_emitted;

        // 不写库时逐条吐事件（TumeFlow 依赖的既有事件流契约）。
        // 写库时**不吐** —— 一次全量是几个 GB 的 stdout，而写库的调用方要从库里读。
        #[cfg(feature = "store")]
        let streaming = writer.is_none();
        #[cfg(not(feature = "store"))]
        let streaming = true;
        if streaming {
            for ev in scanned.events() {
                emit(&Out::Event { event: ev });
            }
        }
        emit(&Out::SourceReport { report: &report });

        // 写库。**只有计划说该写、且真的写成功了，才推进这个来源的游标** ——
        // 反过来会把没落库的那段字节永久跳过：下一轮从新游标续读，那批事件再也
        // 不会被扫到，而且没有任何报错。
        #[cfg(feature = "store")]
        let advance = match (writer.as_ref(), &scanned) {
            (None, _) => true,
            // 🔴 **快照来源不由这条路写。** Class-B 的写入口是 `sync-snapshots`
            // （走 `class_b::enumerate()` + `TotalStore::sync_snapshots`，有自己的
            // 变更检测）。在这里再写一遍就是第二条 Class-B 写路径，两条的枚举范围
            // 与变更判据都不同 —— 正是「同一件事两份实现」那个形状。
            // 它进摘要（`snapshot_sources`），因为「这轮写了 0 条快照」不该被读成
            // 「本机没有快照来源」。
            (Some(_), Scanned::Projected { .. }) => {
                snapshot_sources += 1;
                true
            }
            // `has_prior` 在扫之前就问过了（它决定了这一轮读多少），这里**复用同一个
            // 答案**而不是再查一次：中间那次扫描可能长达几分钟，重查会让「决定读多少」
            // 与「决定怎么写」用上两个不同代的库状态。
            (Some(w), Scanned::Observed(obs)) => match w.commit(
                s,
                obs,
                reasons,
                has_prior.expect("writer 在场时 has_prior 必已问过（问不出来的那一格已 continue）"),
            ) {
                Ok((committed, plan)) => {
                    let quality = plan.quality();
                    match committed {
                        Committed::Preserved => {
                            held_sources += 1;
                            // 🔴 「这一轮没写」必须有自己的一行，而不是让 `store_write`
                            // 缺席 —— 缺席在 NDJSON 里读作什么，取决于消费方记不记得
                            // 去数，而它多半不会。
                            emit(&Out::StoreHeld {
                                source_path: &report.source_path,
                                quality: quality.key(),
                                detail: quality.detail(),
                            });
                            // 三种 Preserve（没读成 / 主动拒绝坏行 / 降级到零好行）
                            // 都要下轮从同一个偏移重来 —— 游标冻住。
                            false
                        }
                        Committed::Wrote { mode, stats } => {
                            written_events += stats.appended;
                            if let Some((before, after)) = stats.loses_events {
                                // 头照切（当前答案必须是最新那份解析），但旧投影不删 ——
                                // 「新解析器合法地少产出」与「一次退化」在这个观测上一样。
                                log::warn!(
                                    target: tag::CLI,
                                    "{key}: new projection has fewer events ({before} → {after}); \
                                     the superseded one was kept"
                                );
                            }
                            emit(&Out::StoreWrite {
                                source_path: &report.source_path,
                                mode: mode.origin_key(),
                                quality: quality.key(),
                                appended: stats.appended,
                                skipped_dup: stats.skipped_dup,
                                skipped_erased: stats.skipped_erased,
                                head_moved: stats.head_moved,
                                superseded_removed: stats.superseded_removed,
                                loses_events: stats.loses_events.map(|(b, a)| [b, a]),
                            });
                            plan.cursor() == session_vault::scan_plan::CursorAction::Advance
                        }
                    }
                }
                Err(why) => {
                    write_failures += 1;
                    log::error!(target: tag::CLI, "store write failed for {key}: {why}");
                    emit(&Out::StoreWriteFailed {
                        source_path: &report.source_path,
                        reason: &why,
                    });
                    false
                }
            },
        };
        #[cfg(not(feature = "store"))]
        let advance = true;

        if advance {
            // 更新游标（即便本轮无新增也写回，刷新 size/mtime）。
            cursors.insert(key, next_state(scanned, prev.as_ref(), write_store));
        }
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
        // 🔴 `wrote_store=false` 与「写了 0 条」是两件事，所以它是独立字段而不是
        // 「`written_events` 恰好为 0」。一个只报数字的摘要说不出「这一轮压根没打算
        // 写库」—— 而那正是 Class-A 恒为 0 时最需要先分清的一件事。
        wrote_store: write_store,
        written_events: write_store.then_some(written_events),
        write_failures: write_store.then_some(write_failures),
        // 计划说「这轮别写」的来源数（没读成 / 主动拒绝坏行 / 降级到零好行）。
        // **与写失败分开**：那是我们想写而没写成，这是我们决定不写。
        held_sources: write_store.then_some(held_sources),
        // 枚举到但不由这条路写的快照来源数（Class-B 归 `sync-snapshots`）。
        snapshot_sources: write_store.then_some(snapshot_sources),
        roots_empty,
    });

    // 退出码：0 正常；1 起步就没跑成（参数/发现/开库）；2 游标没落盘（下轮会重复）；
    // 3 有来源没写进库（本轮**不完整**）。
    //
    // 🔴 3 必须与 0 分开：调用方据退出码决定要不要重试，而「扫完了但有几个没写进去」
    // 与「扫完了」在数字上都可能是一份漂亮的摘要。
    // ⚠️ `held_sources` **不进退出码**：那不是失败，是计划里的一格（坏行/读不到都会
    // 下轮重来）。报成失败会让常态运行里全是红叉，而红叉多了就没人看了。
    if state_saved == Some(false) {
        2
    } else if write_failures > 0 {
        3
    } else {
        0
    }
}

/// 开写库句柄。**库不存在就建**——与 `sync-snapshots` 同一条判据：
/// 「装完还没扫过」不该变成一个需要另一个程序先跑一遍的死结。
///
/// 失败返回该用的退出码（1 = 起步就没跑成）。
#[cfg(feature = "store")]
fn open_scan_writer(store_arg: Option<PathBuf>) -> Result<ScanWriter, i32> {
    let Some(store_path) = resolve_store_path(store_arg) else {
        log::error!(target: tag::CLI, "no data_local_dir; pass --store to locate the total store");
        return Err(1);
    };
    let store = match open_total_store(&store_path) {
        Ok(s) => s,
        Err(e) => {
            log::error!(target: tag::CLI, "open total store for write failed: path={} err={e}", store_path.display());
            return Err(1);
        }
    };
    let attribution_revision = store.attribution_revision();
    log::info!(
        target: tag::CLI,
        "writing scan results to total store: path={} attribution_revision={attribution_revision}",
        store_path.display()
    );
    Ok(ScanWriter {
        store,
        attribution_revision,
    })
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
    // 而**探不动**是第三种情况，不能说成「还没扫过」—— 两者的处置完全不同。
    match total_store_present(&store_path) {
        Ok(true) => {}
        Ok(false) => {
            log::error!(
                target: tag::CLI,
                "total store not found: path={} (host writes it on first scan)",
                store_path.display()
            );
            return 1;
        }
        Err(why) => {
            log::error!(target: tag::CLI, "{why}");
            return 1;
        }
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
        // 🔴 erase 是**不可逆**操作。「探不动」绝不能长得像「没有可删的库」——
        // 后者会让用户以为已经删干净了。
        Some(path) if matches!(total_store_present(&path), Ok(true)) => path,
        Some(path) => {
            match total_store_present(&path) {
                Ok(_) => log::error!(target: tag::CLI, "total store not found for erase"),
                Err(why) => log::error!(target: tag::CLI, "erase aborted: {why}"),
            }
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
    let bytes = match session_vault::probe::read_bytes(&event_file, None) {
        session_vault::probe::Probed::Found(bytes) => bytes,
        session_vault::probe::Probed::Absent => {
            log::error!(target: tag::CLI, "synthetic fixture not found: {}", event_file.display());
            return 1;
        }
        session_vault::probe::Probed::Unknown(e) => {
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
/// 📌 这里从前写着「`scan-all` 没有 `--store` 参数（它是流式的、不写库），所以走
/// 默认路径」。`--write-store`（task #44）落地之后**那个理由过期了**：同一次运行会
/// 往 `--store` 指的那个库里写，归属却从**另一个**库读 —— 两个库的注册表不同时，
/// 写进去的 `project_root` 会来自一份与目标库无关的注册表，而且不报错。
/// 所以路径必须透传。
#[cfg(feature = "store")]
fn project_roots(store_arg: Option<PathBuf>) -> session_vault::attribution::RootRegistry {
    let empty = session_vault::attribution::RootRegistry::new;
    let Some(p) = resolve_store_path(store_arg) else {
        return empty();
    };
    match total_store_present(&p) {
        Ok(true) => {}
        // 没有注册表就是空注册表 —— 事实。
        Ok(false) => return empty(),
        // 探不动也只能返回空（本函数的返回类型说不出第三种），但**要留一行**：
        // 否则「归属注册表突然空了」与「这台机器还没建库」在日志里一模一样。
        Err(why) => {
            log::warn!(target: tag::CLI, "project roots unavailable: {why}");
            return empty();
        }
    }
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
fn project_roots(_store_arg: Option<PathBuf>) -> session_vault::attribution::RootRegistry {
    session_vault::attribution::RootRegistry::new()
}

/// 解析总库路径：`--store` 优先，否则 `<data_local_dir>/svault/total_store.db`
/// 总库在不在 —— **三态收口**（task #43）。
///
/// 🔴 这里从前是九处 `if !store_path.exists()`。一次探测失败（权限、盘没挂上、
/// UNC 不通）会让 CLI 打印「total store not found」并退出 —— 一个**说得出口但错误**
/// 的诊断，而 QuotaBar / TumeFlow 那侧的调用点是 `catch SvaultError` 后静默降级
/// （beta.31 那次装机 svault 落后一个月、界面全程正常，正是这个接缝）。
///
/// `Ok(true)` 在；`Ok(false)` **确认**不在；`Err(msg)` 没问成，消息可直接打印。
#[cfg(feature = "store")]
fn total_store_present(path: &std::path::Path) -> Result<bool, String> {
    use session_vault::probe::{LocalBackend, ProbeBackend, Probed};
    match LocalBackend::unanchored().probe(path, session_vault::deadline::Deadline::unbounded()) {
        Probed::Found(_) => Ok(true),
        Probed::Absent => Ok(false),
        Probed::Unknown(e) => Err(format!("cannot tell whether the total store exists: {e}")),
    }
}

/// 九个「库不在就报错退出」的调用点共用的那段。返回 `Some(exit_code)` 表示该退出。
#[cfg(feature = "store")]
fn bail_unless_store_present(path: &std::path::Path, missing_code: i32) -> Option<i32> {
    match total_store_present(path) {
        Ok(true) => None,
        Ok(false) => {
            log::error!(target: tag::CLI, "total store not found: {}", path.display());
            Some(missing_code)
        }
        Err(why) => {
            log::error!(target: tag::CLI, "{why}");
            Some(missing_code)
        }
    }
}

/// （与 QuotaBar 写者 `main.rs` 同址）。无法确定数据目录时返回 `None`。
///
/// 🔴 **不门控 `store`**：它是纯路径运算（`dirs_next`），一行 rusqlite 都不碰，
/// 而 `store-path` 子命令**在所有构建里都存在**（它的整个用途就是「不打开库、
/// 只说出库该在哪」）。门控它会让不带 feature 的构建直接编不过 —— 实测如此。
fn resolve_store_path(arg: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(p) = arg {
        return Some(p);
    }
    dirs_next::data_local_dir().map(|d| d.join("svault").join("total_store.db"))
}

/// 状态文件里的一条：游标 + **产出它的解析器版本**。
///
/// 🔴 `parser_revision` 放在这里而不是放进 `Cursor`，有一个硬约束：`Cursor` 是库的
/// 公开类型，QuotaBar 用**结构体字面量**构造它（`svault_bridge.rs::row_to_cursor`），
/// 给它加字段会直接让宿主编不过。而这个字段只有「写总库」这一条路用得上。
///
/// `#[serde(flatten)]` 让盘上形状与旧格式**逐字兼容**（旧文件就是一张
/// `source_key → Cursor` 的表）：旧文件缺这个键 ⇒ `None` ⇒ 按「这条路还没写过库」
/// 处理，见 `run_scan_all` 里的 `owes_reparse`。**换个不兼容的格式会让整机全量重扫
/// 一遍**，而那次重扫看起来只是「今天特别慢」。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SourceState {
    #[serde(flatten)]
    cursor: Cursor,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    parser_revision: Option<u32>,
    /// 上一次**全读**时算出的源指纹。认「同尺寸原地重写」的唯一线索。
    ///
    /// 🔴 **不能拿 `cursor.content_hash` 代替。** 「从哪里开始读」与「上一版内容是
    /// 什么」是两件事，而 `cursor_in: None`（强制全读）同时抹掉了它们 ——
    /// `scan.rs` 的注释逐字记着后果：「QuotaBar 的生产路径**永远**传不进上一版指纹，
    /// 同尺寸原地重写从来没有被识别过」。存在游标之外，那条路才通。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    fingerprint: Option<StoredFingerprint>,
}

/// 持久化形态的源指纹。**`(哈希, 覆盖长度)` 缺一不可** ——
/// 只存哈希就退回「全读 N 字节 → 追加到 M → 再全读」时把一次纯追加判成原地重写，
/// 于是总库开一代**按设计永不自动回收**的旧版本（见 `SourceFingerprint::covered_len`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredFingerprint {
    hash: String,
    covered_len: u64,
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
fn load_cursors(path: &std::path::Path) -> HashMap<String, SourceState> {
    let bytes = match session_vault::probe::read_bytes(path, None) {
        session_vault::probe::Probed::Found(b) => b,
        // 没有状态文件 = 第一次跑，是事实。
        session_vault::probe::Probed::Absent => return HashMap::new(),
        // 读不成也退回空表（游标丢了只是重扫一遍，不丢数据），但要留一行 ——
        // 「第一次跑」与「读不了」在结果上一样，在日志里必须分得开。
        session_vault::probe::Probed::Unknown(e) => {
            log::warn!(target: tag::CLI, "read state failed (starting empty): {e}");
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
fn save_cursors(
    path: &std::path::Path,
    cursors: &HashMap<String, SourceState>,
) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        session_vault::probe::create_dir_all(parent)?;
    }
    let json = serde_json::to_vec_pretty(cursors)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let tmp = path.with_extension("json.tmp");
    session_vault::probe::write_bytes(&tmp, &json)?;
    session_vault::probe::rename(&tmp, path)?;
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

    // ── task #44：`scan-all --write-store` ──────────────────────────────────

    const SRC_PATH: &str = "/p/file.jsonl";

    fn mk_source() -> SourceRef {
        SourceRef {
            source_type: SourceType::ClaudeCode,
            source_location: SourceLocation::Local,
            source_mode: SourceMode::AppendLog,
            path: PathBuf::from(SRC_PATH),
            project_root: None,
            artifact_kind: None,
        }
    }

    /// 造一次 append-log 观察。`fingerprint` 为 `Some` ⟺ 这一轮是**全读**
    /// （`scan.rs`：「全读才算得出整份内容的指纹」）。
    fn mk_obs(
        change: session_vault::observation::SourceChange,
        quality: session_vault::observation::ParseQuality,
        events: Vec<RawEvent>,
        full_read: bool,
    ) -> session_vault::observation::AppendLogObservation {
        session_vault::observation::AppendLogObservation {
            source_change: change,
            quality,
            events,
            cursor: Cursor::new_byte_offset(),
            source_fingerprint: full_read
                .then(|| session_vault::observation::SourceFingerprint::of(b"whole file")),
        }
    }

    fn clean() -> session_vault::observation::ParseQuality {
        session_vault::observation::ParseQuality::Clean {
            deferred_tail_bytes: 0,
        }
    }

    fn writer_over(store: session_vault::TotalStore) -> ScanWriter {
        let attribution_revision = store.attribution_revision();
        ScanWriter {
            store,
            attribution_revision,
        }
    }

    #[test]
    fn writing_the_store_defaults_to_full_but_an_explicit_metadata_is_refused() {
        // 不写库：默认不变（既有调用方依赖它）。
        assert_eq!(
            resolve_scan_profile(None, false).unwrap(),
            Profile::Metadata
        );
        // 写库：默认升到 full（§13.1「总库以 full 物化」），不必让调用方记住加参数。
        assert_eq!(resolve_scan_profile(None, true).unwrap(), Profile::Full);
        assert_eq!(
            resolve_scan_profile(Some(ProfileArg::Full), true).unwrap(),
            Profile::Full
        );
        // 🔴 显式的 metadata + 写库 = **拒绝**，不是降级成 full。
        // 悄悄改掉调用方显式写下的参数，比报错更坏。
        let err = resolve_scan_profile(Some(ProfileArg::Metadata), true).unwrap_err();
        assert!(
            err.contains("--profile full"),
            "报错要说出该怎么改，实际：{err}"
        );
        // 不写库时 metadata 仍然合法。
        assert_eq!(
            resolve_scan_profile(Some(ProfileArg::Metadata), false).unwrap(),
            Profile::Metadata
        );
    }

    /// 🔴 **先证明上面那个守卫挡的是一件真事。**
    ///
    /// `resolve_scan_profile` 拒绝 `--write-store --profile metadata`，理由是
    /// 「总库的去重键不含正文」。这条测试把那个理由**跑出来**：同一个 `seq` 先以
    /// `content=None` 落库，再以带正文的形态落库 —— 正文进不去，`appended` 为 0，
    /// **而且一个错都不报**。
    ///
    /// 没有它，守卫只是注释里的一句声明；有了它，那句声明是可执行的。
    /// 而总库 append-only、永不删 ⇒ 这一步**不可逆**，所以只能在参数层拦。
    #[test]
    fn a_metadata_event_permanently_shadows_the_full_one_that_follows() {
        let store = TotalStore::open_in_memory().unwrap();

        let mut metadata_only = mk_event(0, "s1");
        metadata_only.content = None;
        store
            .append_events(&[metadata_only], Projection::Append)
            .unwrap();

        // 同一个 (source, session, seq)，这次带正文 —— 唯一键完全相同。
        let with_body = mk_event(0, "s1");
        assert!(with_body.content.is_some());
        let stats = store
            .append_events(&[with_body], Projection::Append)
            .unwrap();

        assert_eq!(stats.appended, 0, "同 seq ⇒ INSERT OR IGNORE 全丢");
        assert_eq!(stats.skipped_dup, 1);

        let back = store.read_since(0, 10).unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(
            back[0].1.content, None,
            "🔴 正文永远进不去了，而调用方只看到一份没有错误的统计"
        );
    }

    /// 旧格式状态文件（一张 `source_key → Cursor` 的表）必须原样读得出来。
    ///
    /// 变异判据：去掉 `SourceState` 的 `#[serde(flatten)]`，这条必红 ——
    /// 而它红的方式恰好是生产里最贵的那种：解析失败 ⇒ 空表 ⇒ **整机全量重扫一遍**，
    /// 而那次重扫看起来只是「今天特别慢」。
    #[test]
    fn an_old_format_state_file_loads_and_claims_no_parser_revision() {
        let key = "claude_code|local|/p/a.jsonl".to_string();
        let mut cursor = Cursor::new_byte_offset();
        cursor.safe_offset = 42;
        cursor.size = 42;
        cursor.next_seq = 7;
        // 用**旧类型本身**序列化，而不是手写一段 JSON —— 手写的那份会跟着
        // `Cursor` 改字段一起过期，然后这条测试就开始测一个不存在的旧格式。
        let old: HashMap<String, Cursor> = [(key.clone(), cursor)].into_iter().collect();
        let on_disk = serde_json::to_string(&old).unwrap();

        let loaded: HashMap<String, SourceState> = serde_json::from_str(&on_disk).unwrap();
        let st = &loaded[&key];
        assert_eq!(st.cursor.safe_offset, 42);
        assert_eq!(st.cursor.next_seq, 7);
        // 🔴 旧文件说不出「库里那份是哪个解析器写的」⇒ `None`，**不是**「当前版本」。
        // 记成当前版本，第一次 `--write-store` 就会误判成「不欠重投影」。
        assert_eq!(st.parser_revision, None);
        assert!(st.fingerprint.is_none());
    }

    #[test]
    fn a_new_state_file_round_trips_cursor_revision_and_fingerprint() {
        let key = "claude_code|local|/p/a.jsonl".to_string();
        let mut cursor = Cursor::new_byte_offset();
        cursor.safe_offset = 99;
        let st = SourceState {
            cursor,
            parser_revision: Some(session_vault::PARSER_REVISION),
            fingerprint: Some(StoredFingerprint {
                hash: "sha256:abc".to_string(),
                covered_len: 99,
            }),
        };
        let round: HashMap<String, SourceState> = serde_json::from_str(
            &serde_json::to_string(&[(key.clone(), st)].into_iter().collect::<HashMap<_, _>>())
                .unwrap(),
        )
        .unwrap();
        let back = &round[&key];
        assert_eq!(back.cursor.safe_offset, 99);
        assert_eq!(back.parser_revision, Some(session_vault::PARSER_REVISION));
        // 🔴 覆盖长度必须一起活下来：只留哈希会让一次纯追加被判成原地重写。
        let fp = back.fingerprint.as_ref().unwrap();
        assert_eq!(fp.hash, "sha256:abc");
        assert_eq!(fp.covered_len, 99);
    }

    /// 🔴 开新代（`Rollback` / `Reparse`）**必须配一次全读**。
    ///
    /// 拿增量的尾巴去「取代一整代」，会把这个文件的当前投影换成只剩尾巴的那一份 ——
    /// 前面的事件当场从库里消失，而统计看起来完全正常。所以在写之前正向断言
    /// 「手上有全文指纹」（⟺ 本轮 start==0），断不住就**拒绝**而不是照写。
    #[test]
    fn opening_a_new_generation_from_an_incremental_read_is_refused() {
        use session_vault::observation::SourceChange;
        let store = TotalStore::open_in_memory().unwrap();
        // 先有一代，`has_prior` 才为真（否则 planner 会落到 Append，测不到这条）。
        store
            .append_events(&[mk_event(0, "s1")], Projection::Append)
            .unwrap();
        let w = writer_over(store);

        let obs = mk_obs(
            SourceChange::RollbackOrRewrite,
            clean(),
            vec![mk_event(1, "s1")],
            false, // 增量：没有全文指纹
        );
        let err = w
            .commit(
                &mk_source(),
                &obs,
                session_vault::scan_plan::ScanReasons::NONE,
                true,
            )
            .unwrap_err();
        assert!(err.contains("rollback"), "要说出是哪种开新代，实际：{err}");
        assert!(
            err.contains("incremental"),
            "要说出为什么不能写，实际：{err}"
        );
    }

    /// 同样的观察，这一轮是全读 ⇒ 放行，并且**真的开了新的源版本**。
    ///
    /// 它是上一条的对照：没有它，上一条也可能只是「commit 恒失败」。
    #[test]
    fn the_same_rewrite_on_a_full_read_opens_a_new_source_revision() {
        use session_vault::observation::SourceChange;
        let store = TotalStore::open_in_memory().unwrap();
        store
            .append_events(&[mk_event(0, "s1")], Projection::Append)
            .unwrap();
        let before = store
            .current_head("claude_code", "local", SRC_PATH)
            .unwrap();
        let w = writer_over(store);

        let obs = mk_obs(
            SourceChange::RollbackOrRewrite,
            clean(),
            vec![mk_event(0, "s2")],
            true, // 全读
        );
        let (committed, plan) = w
            .commit(
                &mk_source(),
                &obs,
                session_vault::scan_plan::ScanReasons::NONE,
                true,
            )
            .unwrap();
        match committed {
            Committed::Wrote { mode, stats } => {
                assert_eq!(mode, Projection::Rollback);
                assert!(stats.head_moved);
                // 源版本 +1、投影版本归零 —— 磁盘上那段字节已经不在了。
                assert_eq!(
                    (stats.source_revision, stats.projection_revision),
                    (before.0 + 1, 0)
                );
                // `Rollback` 的旧代是那段内容的唯一副本，永不回收。
                assert_eq!(stats.superseded_removed, 0);
            }
            Committed::Preserved => panic!("全读的重写该写进去"),
        }
        assert_eq!(
            plan.cursor(),
            session_vault::scan_plan::CursorAction::Advance
        );
    }

    /// 🔴 第一次见到一个文件时，即使观察报了源变化也**没有旧代可回退** ——
    /// 那时是 `Append`，不是 `Rollback`（`has_prior` 那一格）。
    ///
    /// `has_prior` 由**调用方在扫之前问总库**得出（`ScanWriter::has_prior`），
    /// 因为它同时决定这一轮读多少 —— 见 `scan_reasons` 与
    /// `has_projection_separates_no_rows_from_generation_zero`。
    #[test]
    fn a_rewrite_with_nothing_in_the_store_yet_is_just_an_append() {
        use session_vault::observation::SourceChange;
        let w = writer_over(TotalStore::open_in_memory().unwrap());
        let obs = mk_obs(
            SourceChange::RollbackOrRewrite,
            clean(),
            vec![mk_event(0, "s1")],
            true,
        );
        let (committed, _) = w
            .commit(
                &mk_source(),
                &obs,
                session_vault::scan_plan::ScanReasons::INITIAL,
                false,
            )
            .unwrap();
        match committed {
            Committed::Wrote { mode, stats } => {
                assert_eq!(mode, Projection::Append, "没有前代可取代 ⇒ 追加");
                assert_eq!(stats.appended, 1);
                // 🔴 没开新代：`Rollback` 那一代按设计永不自动回收，凭空开一个
                // 「没有前代可取代的空代」就是往总库里塞一件永远清不掉的垃圾。
                assert_eq!((stats.source_revision, stats.projection_revision), (0, 0));
            }
            Committed::Preserved => panic!("首次写入该落库"),
        }
    }

    /// 「没读成」不写、不推游标，而且**说得出是哪一种**。
    ///
    /// 三种 `Preserve` 共用一行输出，靠 `quality` 区分 —— 用户能做的事完全不同：
    /// `unknown` 是等一等/查权限，`poison_line` 是去看那一行。
    #[test]
    fn an_unreadable_source_is_held_not_written_and_says_which_kind() {
        use session_vault::observation::{ParseQuality, ScanFailure, SourceChange};
        let store = TotalStore::open_in_memory().unwrap();
        store
            .append_events(&[mk_event(0, "s1")], Projection::Append)
            .unwrap();
        let w = writer_over(store);

        for (quality, want) in [
            (
                ParseQuality::Unavailable(ScanFailure::Stat("gone".into())),
                "unknown",
            ),
            (
                ParseQuality::Unavailable(ScanFailure::Read("io".into())),
                "unknown",
            ),
            (
                ParseQuality::RejectedPoisonLine(session_vault::observation::ParseDiagnostics {
                    skipped_lines: 1,
                    first_warning: Some("bad line 3".into()),
                }),
                "poison_line",
            ),
        ] {
            let obs = mk_obs(SourceChange::Appended, quality, vec![], false);
            let (committed, plan) = w
                .commit(
                    &mk_source(),
                    &obs,
                    session_vault::scan_plan::ScanReasons::NONE,
                    true,
                )
                .unwrap();
            assert!(
                matches!(committed, Committed::Preserved),
                "{want}: 读不成/主动拒绝都不该写库"
            );
            assert_eq!(plan.quality().key(), want, "要说得出是哪一种");
            // 🔴 游标冻住 —— 推进会把这段字节永久跳过，而且不报错。
            assert_eq!(
                plan.cursor(),
                session_vault::scan_plan::CursorAction::Freeze
            );
        }
    }

    /// 🔴 **「首次回填」的判据是「库里没有前代」，不是「本地没有游标」。**
    ///
    /// 漏掉这一条会重造 #44 本身：状态文件里有游标、库里却没有投影（此前只吐流地
    /// 跑过，或库被删过/重建过）⇒ 从游标处续读 ⇒ **游标之前的事件永远不会再被扫到**，
    /// 而摘要只显示「本轮 0 条新增」，与「确实没有新会话」逐字相同。
    ///
    /// 反向那一格同样要钉住：手上没游标但库里有前代（宿主写的）**不是**首次 ——
    /// 判成首次就会对全机每个文件做一次没必要的全读。
    #[test]
    fn a_cursor_without_a_projection_counts_as_a_first_backfill() {
        use session_vault::scan_plan::ScanReasons;
        let full_read = |r: ScanReasons| r.wants_full_read() || r.wants_reparse();

        // 游标在、库里空 ⇒ 首次回填 ⇒ 全读。
        let r = scan_reasons(false, Some(false));
        assert!(r.contains(ScanReasons::INITIAL));
        assert!(full_read(r), "🔴 不全读就会把游标之前的事件永久漏在库外");

        // 库里有前代 ⇒ 不是首次 ⇒ 增量（靠 seq 去重，重放幂等）。
        let r = scan_reasons(false, Some(true));
        assert!(r.is_empty());
        assert!(!full_read(r), "库里有就别全读 —— 那是一次没必要的全机重读");

        // 欠重投影 ⇒ 必须全读（`Reparse` 取代一整代，尾巴取代不了）。
        let r = scan_reasons(true, Some(true));
        assert!(r.contains(ScanReasons::PARSER_STALE));
        assert!(full_read(r));

        // 不写库 ⇒ 谈不上「哪一代」⇒ 没有任何理由，走增量。
        let r = scan_reasons(false, None);
        assert!(r.is_empty());
        assert!(!full_read(r));
    }

    /// 🔴 **「没记过」不是「欠账」。**
    ///
    /// 第一次带 `--write-store` 跑时状态里没有解析器版本。把它当成欠账，就会开一个
    /// 新投影版本并**删掉宿主已经写在库里的那一份**（`Reparse` 的旧投影按定义可回收），
    /// 而调用方只会看到一份漂亮的统计。
    #[test]
    fn never_recorded_is_not_stale() {
        assert!(
            !owes_reprojection(true, None, 4),
            "🔴 没记过 ⇒ 这条路还没写过库 ⇒ 该 Append，不该 Reparse"
        );
        assert!(
            owes_reprojection(true, Some(3), 4),
            "记过且不同 ⇒ 欠一次重投影"
        );
        assert!(!owes_reprojection(true, Some(4), 4), "记过且相同 ⇒ 不欠");
        // 不写库就不动总库，没有「哪一代」这回事。
        assert!(!owes_reprojection(false, Some(3), 4));
    }

    /// 🔴 增量轮次算不出指纹，那时必须**保留上一版**，不能覆盖成 `None`。
    ///
    /// 覆盖掉的后果不会当场报错：下一次全读时手上没有上一版内容，于是
    /// 「同尺寸原地重写」认不出来 ⇒ 走 `Append` ⇒ 新内容按旧 seq 续写、
    /// 撞上的被 `INSERT OR IGNORE` 丢掉。而 `size`/`mtime` 那一层对这种改动全盲。
    #[test]
    fn an_incremental_round_keeps_the_previous_fingerprint() {
        let prev = SourceState {
            cursor: Cursor::new_byte_offset(),
            parser_revision: Some(session_vault::PARSER_REVISION),
            fingerprint: Some(StoredFingerprint {
                hash: "sha256:old".to_string(),
                covered_len: 10,
            }),
        };
        // 增量轮：`Projected` 与「没读全文的 Observed」都给不出指纹。
        let incremental = Scanned::Projected {
            events: vec![],
            cursor_out: Cursor::new_byte_offset(),
        };
        let next = next_state(incremental, Some(&prev), true);
        let fp = next
            .fingerprint
            .as_ref()
            .expect("增量轮不该把上一版指纹忘掉");
        assert_eq!(fp.hash, "sha256:old");
        assert_eq!(fp.covered_len, 10);

        // 全读轮：用本轮算出的那一版**覆盖**上一版（它才是最新的）。
        let full = Scanned::Observed(mk_obs(
            session_vault::observation::SourceChange::Appended,
            clean(),
            vec![],
            true,
        ));
        let next = next_state(full, Some(&prev), true);
        assert_ne!(next.fingerprint.as_ref().unwrap().hash, "sha256:old");
    }

    /// 🔴 **不写库的运行不记解析器版本。**
    ///
    /// 那个字段的含义是「这个来源**在库里的投影**是哪个解析器产出的」。只吐流的
    /// 运行根本没动库，记上它就是一句假话 —— 而它会让日后第一次 `--write-store`
    /// 误判成「不欠重投影」，于是新解析器的结果永远进不去（同 seq 被去重丢弃）。
    #[test]
    fn a_streaming_only_round_claims_no_parser_revision() {
        let scanned = || Scanned::Projected {
            events: vec![],
            cursor_out: Cursor::new_byte_offset(),
        };
        assert_eq!(next_state(scanned(), None, false).parser_revision, None);
        assert_eq!(
            next_state(scanned(), None, true).parser_revision,
            Some(session_vault::PARSER_REVISION)
        );
    }

    /// 🔴 **写库这条路必须拿到完整观察。**
    ///
    /// `scan_one` 对 append-log 走 `scan_append_log_observed`（权威），对其余形态走
    /// `scan_source`（有损投影）。这条分流写错的症状极难认：所有来源都会变成
    /// `Projected`，于是 `--write-store` 全部落进「快照不由这条路写」那一格 ——
    /// **一条都不写、一个错都不报**，摘要里只是 `written_events: 0`，
    /// 而那与「本机确实没有新会话」长得一模一样。
    ///
    /// 用 `probe::` 而不是 `std::fs::` 造 fixture：本仓的 `clippy.toml` 禁的是整个
    /// `std::fs` 模块面，测试虽可 `#[allow]`，但既然有现成的透传封装就不必开例外。
    #[test]
    fn scan_one_keeps_the_full_observation_for_append_log() {
        let dir = std::env::temp_dir().join("svault-scan-one-dispatch");
        session_vault::probe::create_dir_all(&dir).unwrap();
        let path = dir.join("t.jsonl");
        session_vault::probe::write_bytes(&path, b"{\"type\":\"user\"}\n").unwrap();
        let roots = || std::sync::Arc::new(session_vault::attribution::RootRegistry::new());

        let src = SourceRef {
            path: path.clone(),
            ..mk_source()
        };
        let (scanned, _) = scan_one(&src, None, None, Profile::Full, roots());
        assert!(
            matches!(scanned, Scanned::Observed(_)),
            "append-log 必须走观察入口 —— ScanStatus 压掉的四种含义正是写库要用的"
        );
        // 全读（`cursor_in = None`）⇒ 算得出全文指纹。下一轮把它传回去，
        // 才认得出「同尺寸原地重写」——那种改动 size 与 mtime 都可能一字不变。
        assert!(scanned.fingerprint().is_some(), "全读该算出指纹");

        // 同一个文件、声明成快照形态 ⇒ 走有损投影那条（它没有「观察」这个概念：
        // 快照的失败态是「无效 UTF-8」，塞进 `Unavailable` 会让「内容无效」
        // 伪装成「读不到」）。
        let snap = SourceRef {
            source_mode: SourceMode::SnapshotFile,
            ..src
        };
        let (scanned, _) = scan_one(&snap, None, None, Profile::Full, roots());
        assert!(matches!(scanned, Scanned::Projected { .. }));
        assert!(scanned.fingerprint().is_none(), "投影那条给不出指纹");
    }

    /// 「库里已经有这个来源的投影了吗」必须问库，而 `current_head` 答不了 ——
    /// 它对「一条记录都没有」和「第一代 `(0,0)`」返回**同一个值**。
    #[test]
    fn has_projection_separates_no_rows_from_generation_zero() {
        let store = TotalStore::open_in_memory().unwrap();
        let key = session_vault::store::SourceKey {
            source_type: SourceType::ClaudeCode,
            source_location: SourceLocation::Local,
            source_path: SRC_PATH.to_string(),
        };
        assert!(!store.has_projection(&key).unwrap());
        assert_eq!(
            store
                .current_head("claude_code", "local", SRC_PATH)
                .unwrap(),
            (0, 0)
        );

        store
            .append_events(&[mk_event(0, "s1")], Projection::Append)
            .unwrap();

        assert!(store.has_projection(&key).unwrap(), "现在有了");
        // 🔴 而头**还是** (0,0) —— 这正是它当不了 `has_prior` 判据的原因。
        assert_eq!(
            store
                .current_head("claude_code", "local", SRC_PATH)
                .unwrap(),
            (0, 0)
        );
    }
}

/// `sessions-recent`：按事件真实时间列出最近活跃的会话。
#[cfg(feature = "store")]
fn run_sessions_recent(limit: usize, since_ms: Option<i64>, store_arg: Option<PathBuf>) -> i32 {
    let Some(store_path) = resolve_store_path(store_arg) else {
        log::error!(target: tag::CLI, "no data_local_dir; pass --store");
        return 1;
    };
    if let Some(code) = bail_unless_store_present(&store_path, 1) {
        return code;
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

/// `roots`：列出注册表里的项目根 + 归属修订号。
///
/// 🔴 **每一条失败路径都退非零**，一条都不吞。这个命令的全部价值就是让消费方
/// 不必自己再发现一遍项目 —— 而一个「成功但空」的输出会让它读作「这台机器上没有
/// 项目」，然后**心安理得地回退到自己那套发现**，正好把这个命令要消除的第二份
/// 实现重新请回来。空列表只有一个合法含义：读到了，注册表里确实没有根。
fn run_store_path() -> i32 {
    match resolve_store_path(None) {
        Some(p) => {
            emit(&Out::StorePath {
                path: &p.to_string_lossy(),
            });
            0
        }
        // 🔴 推不出来是「说不出来」，不是「在当前目录」。给一个兜底路径会让调用方
        // 拿着一个看起来正常的错答案去删东西。
        None => {
            log::error!(target: tag::CLI, "no data_local_dir on this platform");
            1
        }
    }
}

fn run_memory_roots(userprofile: Option<String>, timeout_secs: u64) -> i32 {
    let profile = userprofile.or_else(|| std::env::var("USERPROFILE").ok());
    let enumeration = session_vault::memory_roots::enumerate(
        profile.as_deref(),
        session_vault::deadline::Deadline::after(std::time::Duration::from_secs(timeout_secs)),
    );
    for r in &enumeration.roots {
        emit(&Out::MemoryRoot {
            location: &r.location,
            claude_home: &r.claude_home,
            codex_home: &r.codex_home,
            fs_prefix: &r.fs_prefix,
        });
    }
    for u in &enumeration.unreachable {
        emit(&Out::MemoryRootUnreachable {
            location: &u.location,
            reason: &u.reason,
        });
    }
    emit(&Out::MemoryRootsSummary {
        roots: enumeration.roots.len(),
        unreachable: enumeration.unreachable.len(),
    });
    // 🔴 退出码 0 **即使有 unreachable**：那不是本命令的失败，它诚实地报告了。
    // 非零会让调用方走「命令挂了」那条路，把一份有效的部分答案整个丢掉。
    0
}

#[cfg(feature = "store")]
fn run_roots(store_arg: Option<PathBuf>) -> i32 {
    let Some(store_path) = resolve_store_path(store_arg) else {
        log::error!(target: tag::CLI, "no data_local_dir; pass --store");
        return 1;
    };
    if let Some(code) = bail_unless_store_present(&store_path, 1) {
        return code;
    }
    let store = match open_total_store(&store_path) {
        Ok(s) => s,
        Err(e) => {
            log::error!(target: tag::CLI, "open total store failed: {e}");
            return 1;
        }
    };
    let (roots, attribution_revision) = match store.project_roots_report() {
        Ok(v) => v,
        Err(e) => {
            log::error!(target: tag::CLI, "project_roots_report failed: {e}");
            return 1;
        }
    };
    for r in &roots {
        emit(&Out::ProjectRoot {
            root_key: r.root_key.clone(),
            root_path: r.root_path.clone(),
            root_source: r.root_source.clone(),
            first_seen_ms: r.first_seen_ms,
            last_seen_ms: r.last_seen_ms,
            aliases: r.aliases.clone(),
            canonical_id: r.canonical_id.clone(),
            claude_project_dirs: session_vault::project_dir::claude_project_dirs(
                &r.root_path,
                &r.aliases,
            ),
            host_path: session_vault::project_dir::host_openable_form(
                &r.root_path,
                &r.aliases,
                session_vault::pathnorm::HostPlatform::current(),
            ),
            // 拼写取自 `IdentityVerdict::as_str` —— 与库里 `outcome` 列同一份，
            // 在这里再写一遍 `match` 就是第二份实现（本仓当天刚为此栽过两次）。
            identity_verdict: r.identity_verdict.as_str(),
            identity_detail: r.identity_verdict.why().map(str::to_string),
        });
    }
    emit(&Out::RootsSummary {
        roots: roots.len(),
        attribution_revision,
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
    if let Some(code) = bail_unless_store_present(&store_path, 1) {
        return code;
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
    if let Some(code) = bail_unless_store_present(&store_path, 1) {
        return code;
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
    if let Some(code) = bail_unless_store_present(&store_path, 1) {
        return code;
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
