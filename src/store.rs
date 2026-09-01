//! 不可变 `RawEvent` 总库（§13 / TumeFlow ADR-020 的 "Vault"）。
//!
//! **append-only、按 `dedup_key` 幂等、`offset` 单调**——把扫描器（`scan`）产出的逐事件
//! `RawEvent` 流持久化，作为两个消费者（QuotaBar / TumeFlow）共同认的证据归宿。
//!
//! 形态约束（与设计契约一致）：
//! - 解析内核仍**无状态**（§14）；本模块是「同仓库内的独立持久化组件」，经 `store` feature
//!   门控——纯 parser 用户（`parse_lines`）不被迫拉 `rusqlite`。
//! - `offset`（追加序）是**同步游标，不是时间**；冲突裁决（latest-wins）由下游按 `occurred_at`
//!   裁，store 只忠实记录（§13.1 / ADR-020）。
//! - 永不删/不压缩/不过期是默认保留策略（ADR-016）；用户主动 erase 在同一事务写墓碑并物理
//!   删除命中正文，后续重建按墓碑跳过，禁止复活。
//! - `event_json` 只以版本化 AES-256-GCM 信封落盘；密钥由 OS keychain 持久化，SQLite 内不留
//!   密钥或明文。旧明文库首次打开时原地迁移并清理空闲页（ADR-027）。

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OpenFlags, OptionalExtension, TransactionBehavior};
use serde::Serialize;

use crate::cursor::{Cursor, ScanStatus};
use crate::deadline::Deadline;
use crate::discover::SourceRef;
use crate::probe::{ProbeBackend, Probed};
use crate::rawevent::{EventType, RawEvent, SourceLocation, SourceMode, SourceType};
use crate::store_crypto::{
    create_os_key, data_key_id, is_envelope, load_os_key, new_data_key_id, CryptoError,
    StoreCipher, StoreKey,
};
use crate::Profile;

/// 总库错误。
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("serde: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("crypto: {0}")]
    Crypto(#[from] CryptoError),
    /// 批里有事件不属于它声明的 [`SourceKey`]。
    ///
    /// 从前作用域是从第一条事件反推的，所以这种情况**无法被发现** —— 混进来的事件会被
    /// 安静地写进另一个文件的投影里。作用域一旦显式，它就变成一个可以拒绝的错误。
    #[error("batch declares source {declared} but carries an event from {found}")]
    ForeignEvent { declared: String, found: String },
    /// 新投影比它要取代的那份**少了事件** —— 拒绝替换，旧投影原样保留。
    ///
    /// 这是解析器退化最直接的可观测信号。退化本身可以修，但「退化 + 此后源文件被
    /// 自动清理」就再也回不去了（ADR-016：原始来源会定时自删）。
    // 🔴 字段不能叫 `source`：`thiserror` 会把它当成错误源链（`Error::source()`），
    // 于是要求 `String: Error`。命名与派生宏的约定冲突时，改名比加标注清楚。
    #[error(
        "reparse of {source_path} would drop events ({before} → {after}); refusing to replace"
    )]
    ProjectionLosesEvents {
        source_path: String,
        before: u64,
        after: u64,
    },
}

pub type StoreResult<T> = std::result::Result<T, StoreError>;

/// 一批事件相对该文件既有事件的关系 —— 决定它们落在**哪一代**。
///
/// 🔴 这里曾是一个裸 `is_rollback: bool`，而两种「需要开新代」的原因在语义上
/// 完全不同：一种是外部世界变了（文件被截断/重写），另一种是我们变了（解析器
/// 升级，同一份字节现在能读出更多东西）。用 bool 表达时，第二种只能伪装成
/// 第一种 —— 于是 `rollback` 这个词会开始撒谎，而日志、报表、以后每一个读
/// 这段代码的人都被误导。
///
/// 具名之后，调用点必须说出理由，且新增第三种原因时编译器会指出所有分支。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Projection {
    /// 增量：并入文件当前代，与既有事件一起被 `read_session` 读到。
    Append,
    /// 扫描器检测到文件变小/被重写，已从 `seq=0` 重建。
    ///
    /// 开一个**新的源版本** —— 磁盘上那段内容已经不存在了，前一个源版本是它**唯一的
    /// 副本**，因此永不自动回收（ADR-044 决定 1）。
    Rollback,
    /// **同一份字节，更好的解析器**（`PARSER_REVISION` 提升后的重扫）。
    ///
    /// 开一个**新的投影版本**，源版本不动 —— 字节没变，变的是我们。必须开新投影，
    /// 否则 `INSERT OR IGNORE` 会把每一条重发事件当成重复全部丢弃：文件没变、seq
    /// 没变、投影也没变，唯一键完全相同。结果是解析器修好了而总库纹丝不动，且没有
    /// 任何错误可查。
    ///
    /// 被它取代的投影是**可再生的**（源字节还在——`Reparse` 按定义就是重读它们），
    /// 所以可回收，与 `Rollback` 相反。
    Reparse,
}

/// 一个源文件当前的 `(源版本, 投影版本)`。没有任何记录时是 `(0, 0)`。
pub type HeadRevisions = (i64, i64);

impl Projection {
    /// 这一批该落在哪个 `(source_revision, projection_revision)`。
    ///
    /// 🔴 三个分支各推进**不同的维度**，这正是把 `generation` 拆开的全部意义：
    /// 「源字节变了」与「解析器变了」留存价值相反，用一个整数表达时无法区分，
    /// 于是任何回收都只能一刀切。
    fn target_revisions(self, head: HeadRevisions) -> HeadRevisions {
        let (source_revision, projection_revision) = head;
        match self {
            Projection::Append => (source_revision, projection_revision),
            Projection::Rollback => (source_revision + 1, 0),
            Projection::Reparse => (source_revision, projection_revision + 1),
        }
    }

    /// 写进 `projections.origin` 的理由。**不是** `Debug` 输出：这个串是持久化的，
    /// 换个 derive 就会悄悄改掉库里的历史含义。
    ///
    /// `pub` 是给 CLI 的：`scan-all --write-store` 要在 NDJSON 里报「这批落在哪种
    /// 投影上」。让它复用**同一个**函数而不是另写一张映射表 —— 否则线上的词与库里
    /// 记的理由会各自演化，而两者不一致时没有任何东西会报错。
    pub fn origin_key(self) -> &'static str {
        match self {
            Projection::Append => "append",
            Projection::Rollback => "rollback",
            Projection::Reparse => "reparse",
        }
    }
}

/// 一个源文件的稳定标识：`(类型, 位置, 路径)`。
///
/// 🔴 存在的理由是**显式**。此前作用域从批里第一条事件反推，于是空批既拿不到作用域、
/// 也做不了任何事 —— 而「把当前投影替换为空」正需要一个空批（新解析器合法地对某文件
/// 产出零事件）。作用域一旦显式，那个语义就自然可表达，不必再为它开特例。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceKey {
    pub source_type: SourceType,
    pub source_location: SourceLocation,
    pub source_path: String,
}

impl SourceKey {
    /// 从一条事件取它的作用域。**仅供旧的 `append_events` 兼容层使用** —— 新代码应当
    /// 从调用点本来就有的 `SourceRef` 构造，而不是从数据里反推。
    fn from_event(ev: &RawEvent) -> Self {
        Self {
            source_type: ev.source_type,
            source_location: ev.source_location.clone(),
            source_path: ev.source_path.clone(),
        }
    }

    fn parts(&self) -> (&'static str, String, &str) {
        (
            source_type_key(self.source_type),
            self.source_location.as_key(),
            self.source_path.as_str(),
        )
    }
}

/// 一次投影应用的输入。
///
/// `events` **允许为空**：对 `Rollback` / `Reparse` 而言，空批表达的是「这个源文件的
/// 当前投影就是空的」，不是「无事可做」。区分这两者的是 `mode`，不是批的长度。
pub struct FileProjectionBatch {
    pub source: SourceKey,
    /// 产出这批事件的解析器版本，记进台账供日后判断哪些投影可被取代。
    pub parser_revision: Option<u32>,
    pub mode: Projection,
    pub events: Vec<RawEvent>,
    /// 这次**开新代**操作的稳定身份（ADR-051 I7）。
    ///
    /// 给了它，同一个 token 的第二次应用直接返回**原来的头**，不开新代 ——
    /// 崩溃重放因此不再每次留一代不可回收的源版本。
    ///
    /// 🔴 **`Append` 不需要它**（靠 `seq` 去重，天然幂等），传 `None` 即可。
    /// `Rollback` / `Reparse` 传 `None` 也能工作 —— 只是退回到旧的、不幂等的行为，
    /// 所以**新调用点一律要传**。
    pub token: Option<crate::token::ProjectionToken>,
}

/// 一次 [`TotalStore::apply_projection`] 的结果。比 [`AppendStats`] 多出「落在哪个投影」
/// 与「头动没动」—— 调用方据此判断重投影是否真的落库（QuotaBar 的
/// `parser_revision` 就靠这个决定推不推进）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProjectionStats {
    pub appended: u64,
    pub skipped_dup: u64,
    pub skipped_erased: u64,
    pub max_offset: i64,
    pub source_revision: i64,
    pub projection_revision: i64,
    /// 头是否指向了一个新的 `(source_revision, projection_revision)`。
    pub head_moved: bool,
    /// 被这次 `Reparse` 取代并删除的旧投影行数。`Rollback` 恒为 0 —— 它的旧版本是
    /// 已消失内容的唯一副本，永不回收。
    pub superseded_removed: u64,
    /// 新投影比被它取代的那份**少了事件** → `Some((before, after))`。
    ///
    /// 此时头照切（当前答案必须是最新那份解析），但**旧投影不删** —— 事件变少既可能
    /// 是新解析器合法地不再产出某类事件，也可能是一次退化，而两者在这个观测上完全
    /// 一样。不可逆的那一步在可疑时不做。调用方据此上报/告警。
    pub loses_events: Option<(u64, u64)>,
}

/// change-feed 的一条记录：某个源文件的当前投影被换掉了。
///
/// 消费者据此按 source **原子替换**自己的物化 —— 而不是逐事件 upsert，那样永远删不掉
/// 新投影里不再出现的事件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectionChange {
    /// change-feed 自己的游标。**与 `raw_events.offset` 无关** —— 后者会被重投影重铸，
    /// 而这条流记的正是「重投影发生了」。
    pub seq: i64,
    pub at: i64,
    pub source_type: String,
    pub source_location: String,
    pub source_path: String,
    pub old_source_revision: Option<i64>,
    pub old_projection_revision: Option<i64>,
    pub new_source_revision: i64,
    pub new_projection_revision: i64,
    /// `append` / `rollback` / `reparse` —— 与 `projections.origin` 同口径。
    pub reason: String,
}

/// [`TotalStore::gc_superseded_projections`] 的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GcStats {
    /// 被回收（或将被回收）的投影份数。
    pub projections: u64,
    /// 涉及的事件行数。
    pub events: u64,
    pub dry_run: bool,
}

/// [`TotalStore::recent_sessions`] 的一行。
///
/// `last_occurred_at_unix_ms` 为 `None` = 这个会话的事件全都没有可解析的时间。它排在
/// 最后并**照常返回** —— 消费者需要知道「有这么个会话但不知道它什么时候发生」，
/// 而不是发现它凭空消失了。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecentSession {
    pub source_type: String,
    pub source_location: String,
    pub source_path: String,
    pub session_id: String,
    pub last_occurred_at_unix_ms: Option<i64>,
    pub first_occurred_at_unix_ms: Option<i64>,
    pub event_count: u64,
}

impl RecentSession {
    /// 这个会话有没有可用的时间。`false` 时它排在末尾，且任何按时间做的判断都不该
    /// 把它当作「很久以前」——那是**未知**，不是**旧**。
    pub fn has_time(&self) -> bool {
        self.last_occurred_at_unix_ms.is_some()
    }
}

/// [`TotalStore::project_roots_report`] 的一行 —— 注册表里一个项目根。
///
/// `root_key` 是归一化后的比较键（小写、正斜杠、无尾斜杠），`root_path` 是原始形式。
/// **两个都给**：消费方要拿 `root_key` 做稳定标识（跨视角不变），拿 `root_path` 显示
/// 给人看。只给后者会逼每个消费方自己再归一化一遍 —— 那正是同一条规则长出第二份
/// 实现的入口。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectRootRow {
    pub root_key: String,
    pub root_path: String,
    /// `git` / `marker` / `scan` / `configured`（[`crate::attribution::RootSource`]）。
    /// **原样透传，不解析** —— 一个本层不认识的来源标签不该在报告里消失或被改写成
    /// 别的值；消费方看得见「有个我不认识的来源」，比看见一个伪造的 `scan` 好。
    pub root_source: String,
    pub first_seen_ms: i64,
    pub last_seen_ms: i64,
    /// 同一个根的**其它等价写法**（不含 `root_path` 自身），由 [`crate::pathnorm`] 推出。
    ///
    /// # 为什么这属于报告面
    ///
    /// 消费方手上的路径未必是注册表存的那种形式：一个 Windows 进程枚举出的是
    /// `\\wsl.localhost\<distro>\…`，而注册表可能存着 `wsl:<distro>:/…`。**同一个项目、
    /// 两个字符串**，用 `==` 一比就是两个项目 —— 实测后果是同一个项目在记忆库里存成
    /// 两个身份，各持一半记忆且互相看不见。
    ///
    /// 让消费方自己换算，等于把这条规则复制一份到每个客户端。给出来，规则就还是一份。
    ///
    /// ⚠️ **`/mnt/<drive>/…` 那一族不在里面。** 它要挂载表才能换算
    /// （[`crate::pathnorm::mnt_to_windows`]），而挂载表是**运行期发现的事实**，
    /// 这个进程手上没有。缺席是诚实的「我算不出来」；按盘符猜出一个 `C:\…` 才是
    /// 编造 —— `automount.root` 可以被改，猜错就把两个不相干的项目并成一个。
    pub aliases: Vec<String>,
    /// 这个根的**跨系统身份**（git origin 归一化后的 id），没有就是 `None`。
    ///
    /// # 与 `aliases` 是两件不同的事，缺一不可
    ///
    /// | | 收敛什么 | 例子 |
    /// | --- | --- | --- |
    /// | `aliases` | 同一条路径的**不同写法** | `wsl:U:/home/u/p` ⇄ `\\wsl.localhost\U\home\u\p` |
    /// | `canonical_id` | **不同路径**上的同一个 repo | Windows 一份 checkout + WSL 一份 |
    ///
    /// 路径归一认不出两份 checkout（它们的路径毫不相干），git 身份认不出写法差异
    /// （同一个 checkout 换个写法，`.git` 还是那个）。消费方要把同一个项目聚成一组，
    /// **两样都得有**。
    ///
    /// 🔴 **这里从前写着「`None` 的两种含义调用方通常不必区分（没扫到过 / 扫到时
    /// 就没有 git remote），都表示『说不出它的跨系统身份』—— 而那是诚实的答案」。
    /// 那句话有两处不成立**（2026-08-21 实测，task #56）：
    ///
    /// 1. **不是两种，是三种** —— 漏掉了「**没问成**」（本机 20 个根里占 3 个：
    ///    裸 POSIX ×2 + `/mnt/c/…` ×1，在 Windows 上根本没有命名空间）。
    /// 2. 「没问成」**不是**诚实的答案。它与「确认没有」的下游处置相反：一个该
    ///    重试且**绝不能触发删除**，另一个该被接受、别再算。
    ///
    /// 这正是本仓反复记的那条 —— **把没被遵守的纪律写成既成事实，比不写更糟**：
    /// 这段注释让压平看起来是**有意的、安全的**，于是没人去查。
    ///
    /// 区别现在由 [`Self::identity_verdict`] 说出来。
    pub canonical_id: Option<String>,
    /// 身份**探测的结论** —— 回答 `canonical_id` 为 `None` 时**为什么**没有。
    ///
    /// 🔴 **它与 `canonical_id` 不是同一个事实的两种说法。** 身份行活过 checkout
    /// 被删（那是 `project_identity` 存在的全部理由），而判决说的是**最后一次探测**
    /// 的结果 —— 「有身份 + 本轮没问成」是一个真实且有用的状态。
    pub identity_verdict: IdentityVerdict,
}

/// 一个根路径的其它等价写法。
///
/// 只做**双向的 WSL 规范形 ⇄ UNC**：两个方向都可能是注册表里存的那一种，取决于
/// 归属发生在哪一侧。纯 Windows 路径与纯 Linux 路径没有第二种写法，返回空。
fn alias_forms_of(root_path: &str) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(unc) = crate::pathnorm::canonical_wsl_to_unc(root_path) {
        out.push(unc);
    }
    if let Some(canonical) = crate::pathnorm::canonical_wsl_unc(root_path) {
        out.push(canonical);
    }
    out
}

/// 某个源文件的当前头；无记录时 `(0, 0)`。
///
/// 读 `current_head` 而不是 `MAX(source_revision, projection_revision)`：后者表达不了
/// 「当前投影是空的」——而一个新解析器合法地对某文件产出零事件时正需要那个语义。
fn head_of(
    conn: &Connection,
    source_type: &str,
    source_location: &str,
    source_path: &str,
) -> StoreResult<HeadRevisions> {
    Ok(conn
        .query_row(
            r#"SELECT source_revision, projection_revision FROM current_head
                WHERE source_type = ?1 AND source_location = ?2 AND source_path = ?3"#,
            params![source_type, source_location, source_path],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?
        .unwrap_or((0, 0)))
}

/// 一次 `append_events` 的结果。`skipped_dup` = 命中 `dedup_key` 唯一约束被忽略的条数
/// （force 全量重扫时旧事件全走这里 → 幂等）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct AppendStats {
    pub appended: u64,
    pub skipped_dup: u64,
    pub skipped_erased: u64,
    pub max_offset: i64,
}

/// 总库状态（宿主渲染 / 验证用）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct StoreStatus {
    pub count: u64,
    pub max_offset: i64,
    pub last_ingested_at: Option<i64>,
    pub encrypted: bool,
    pub encryption_version: u8,
    pub active_data_keys: u64,
    pub key_scheme: &'static str,
}

/// 一次用户主动擦除的结果。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct EraseStats {
    pub deleted_events: u64,
    pub keys_destroyed: u64,
    pub tombstone_written: bool,
}

struct EncryptedRow {
    offset: i64,
    source_type: String,
    source_location: String,
    source_path: String,
    source_session_id: String,
    seq: i64,
    source_revision: i64,
    projection_revision: i64,
    aad_version: i64,
    project_root: String,
    envelope: String,
}

impl EncryptedRow {
    /// 列序必须与 [`ENCRYPTED_ROW_COLUMNS`] 一致 —— 两处各写一份是它们分叉的开始。
    fn from_sql(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            offset: row.get(0)?,
            source_type: row.get(1)?,
            source_location: row.get(2)?,
            source_path: row.get(3)?,
            source_session_id: row.get(4)?,
            seq: row.get(5)?,
            source_revision: row.get(6)?,
            projection_revision: row.get(7)?,
            aad_version: row.get(8)?,
            project_root: row.get::<_, Option<String>>(9)?.unwrap_or_default(),
            envelope: row.get(10)?,
        })
    }

    fn aad(&self) -> Vec<u8> {
        event_aad(
            self.aad_version,
            &self.source_type,
            &self.source_location,
            &self.source_path,
            &self.source_session_id,
            self.seq,
            self.source_revision,
            self.projection_revision,
        )
    }
}

/// [`EncryptedRow::from_sql`] 期望的列与顺序。所有 SELECT 都必须用它，否则列序对不上时
/// 不会报错，只会把 `projection_revision` 读成 `aad_version` 之类 —— 静默且难查。
const ENCRYPTED_ROW_COLUMNS: &str = "r.offset, r.source_type, r.source_location, r.source_path, \
     r.source_session_id, r.seq, r.source_revision, r.projection_revision, r.aad_version, \
     r.project_root, r.event_json";

#[derive(Clone)]
struct DataKeyGroup {
    source_type: String,
    source_location: String,
    source_path: String,
    project_root: String,
}

impl DataKeyGroup {
    fn from_event(event: &RawEvent) -> Self {
        Self {
            source_type: source_type_key(event.source_type).to_string(),
            source_location: event.source_location.as_key(),
            source_path: event.source_path.clone(),
            project_root: event.project_root.clone().unwrap_or_default(),
        }
    }

    fn from_row(row: &EncryptedRow) -> Self {
        Self {
            source_type: row.source_type.clone(),
            source_location: row.source_location.clone(),
            source_path: row.source_path.clone(),
            project_root: row.project_root.clone(),
        }
    }
}

/// [`TotalStore::read_session`] 的结果。`skipped > 0` = 有行反序列化失败被跳过，事件流**不完整**。
///
/// 为什么不直接静默返回 `Vec`：transcript 这类「正确性优先 + 有完整 live 回落」的消费者，宁可
/// 回落 live JSONL 也不该展示缺气泡的半截 transcript——故把不完整性**显式暴露**给调用方决策，
/// 而非降级成「看似成功的部分结果」。（`read_since` 走 pull 流则相反：单坏行不能中断整条增量同步，
/// 所以那边内部 skip+warn；两个读 API 的策略按消费者需求分流。）
#[derive(Debug, Clone, Default)]
pub struct SessionRead {
    pub events: Vec<RawEvent>,
    pub skipped: usize,
}

/// [`TotalStore::read_since`] 的一页结果。
///
/// `max_scanned_offset` 是本页 SQL **实际返回**的最大 `offset`（无论该行是否反序列化成功），
/// `None` = SQL 返回零行。pull 流消费者据此区分「真追平」与「整窗坏行」：`read_since` 在 SQL
/// `LIMIT` **之后**才 skip 反序列化失败的行，故 `events` 为空**不**代表其后无更多行——一窗全是
/// 坏行时 `events=[]` 但 `max_scanned_offset=Some(...)`。消费者必须把游标推进到
/// `max_scanned_offset`（越过坏行），只在 `max_scanned_offset==None` 时才判定追平；否则坏行之后
/// 的有效事件将**永久不可达**（评审 [P1]）。
#[derive(Debug, Clone, Default)]
pub struct ReadPage {
    /// 成功反序列化的事件（坏行已 skip+warn）。
    pub events: Vec<(i64, RawEvent)>,
    /// 本页 SQL 扫描到的最大 offset（含坏行）；`None` = SQL 零行（真追平）。
    pub max_scanned_offset: Option<i64>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct SnapshotSyncStats {
    pub sources: u64,
    pub changed: u64,
    pub unchanged: u64,
    pub failed: u64,
    pub appended: u64,
}

/// `raw_events` 表 DDL（建库 + 迁移共用）。
///
/// 🔴 **`generation` 已被 `(source_revision, projection_revision)` 取代**（ADR-044 决定 1）。
/// 那一个整数曾同时表达两件留存价值**相反**的事：
///
/// - **源字节被重写/截断** → 旧内容在磁盘上已不存在，那一代是**唯一副本**，永不自动删；
/// - **换了个解析器** → 源字节没变，旧代只是**更差的、可再生的**解析。
///
/// 用一个数表达时，任何「删除非当前代」的回收都会混删前者。拆开之后，留存策略可以跟着
/// 理由走，而不是一刀切。
///
/// `aad_version` 是自描述的加密上下文版本，见 [`event_aad`]。
const RAW_EVENTS_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS raw_events (
    offset              INTEGER PRIMARY KEY AUTOINCREMENT,
    ingested_at         INTEGER NOT NULL,
    schema_version      INTEGER NOT NULL,
    source_type         TEXT    NOT NULL,
    source_location     TEXT    NOT NULL,
    source_path         TEXT    NOT NULL,
    source_session_id   TEXT    NOT NULL,
    seq                 INTEGER NOT NULL,
    source_revision     INTEGER NOT NULL DEFAULT 0,
    projection_revision INTEGER NOT NULL DEFAULT 0,
    aad_version         INTEGER NOT NULL DEFAULT 1,
    event_type          TEXT    NOT NULL,
    occurred_at         TEXT,
    -- 归一化的 UTC 毫秒。排序与索引**只认这一列**；`occurred_at` 保留原始串供溯源。
    -- 直接给原始串排序会出两种错：不同时区偏移的字典序与时刻序不一致，小数秒位数
    -- 不同会打乱同一秒内的先后。见 `rawevent::occurred_at_unix_ms`。
    occurred_at_unix_ms INTEGER,
    project_root        TEXT,
    event_json          TEXT    NOT NULL,
    UNIQUE (source_type, source_location, source_path, source_session_id, seq,
            source_revision, projection_revision)
);
"#;

/// 二级索引，**与建表分开**。
///
/// 🔴 分开的原因是迁移的代价，不是洁癖。这三条原本就写在 `RAW_EVENTS_DDL` 里，于是
/// 「先建表再灌 93 万行」这条路上每一次 `INSERT` 都要维护 **4 棵 B 树**（唯一约束的
/// 隐式索引 + 这三条），而后三条对灌数据本身毫无用处 —— 它们服务的是之后的查询。
/// 实测 2.8 GB 真库上这让重写阶段多写数 GB 的 WAL。
///
/// 建索引放在数据搬完之后、且**无条件执行**（`IF NOT EXISTS` 幂等）。无条件是关键：
/// 放在迁移分支里的话，一旦进程在「表已改形状、索引还没建」之间被杀，下次启动会因为
/// `source_revision` 已存在而跳过整个分支 —— 库从此没有索引，而这不报错，只是所有
/// 会话/项目查询退化成全表扫。
/// 项目的规范身份。**提成常量是为了让迁移能复用它** —— 与 `RAW_EVENTS_DDL` 同一个理由。
const PROJECT_IDENTITY_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS project_identity (
    project_root    TEXT    NOT NULL,
    canonical_id    TEXT    NOT NULL,
    -- 🔴 毫秒，不是秒：`last_seen_ms` 是**排序键**（默认查询取最新的那条），
    -- 而秒级精度下同一秒内的两个身份会平局、退化成按 id 字母序。
    first_seen_ms   INTEGER NOT NULL,
    last_seen_ms    INTEGER NOT NULL,
    PRIMARY KEY (project_root, canonical_id)
);
"#;

const PROJECT_IDENTITY_INDEX_DDL: &str = r#"
CREATE INDEX IF NOT EXISTS idx_identity_cid ON project_identity(canonical_id);
"#;

/// 一个根的身份记录结果。
///
/// 🔴 **四态，不是 `bool`/`Option`。** 「问过了没变」「刚记上」「确认没有 remote」
/// 「没问成」对调用方是四种不同的处置，压成两态就又造一个「没问成长得像没有」。
///
/// 🔴 **而它此前只活在这一轮扫描的内存里**（2026-08-21，task #56）——
/// 只有 `Recorded` 会留下痕迹（`project_identity` 的一行），另外两个算完就没了。
/// 于是 `svault roots` 的 `canonical_id: null` 同时是三件事，见
/// [`IdentityVerdict`]。现在每一次探测的结论都落 `project_identity_probe`。
#[derive(Debug, Clone, PartialEq, Eq)]
enum IdentityOutcome {
    /// 本进程已经问过这个根（终态缓存命中）。
    AlreadyProbed,
    /// 身份行已落库。
    Recorded,
    /// **确认**这个根没有可用的 remote —— 事实，不写 `path:` 兜底行（约束 1）。
    /// 带上是哪一种：没有 `.git`，还是有 `.git` 但里面没 origin。
    NoRemote(&'static str),
    /// 🔴 **那个目录本身不在磁盘上了** —— 与 `NoRemote` 分开，见
    /// [`IdentityVerdict::CheckoutMissing`]。同样是终态（重试不会变），
    /// 但**已记下的 `canonical_id` 仍然有效**。
    CheckoutMissing,
    /// **没问成**：探测失败 / 拿不到锁 / 写失败。缓存已撤回，下一轮重试。
    Unresolved(String),
}

/// 一个根的身份**探测结论** —— [`ProjectRootRow::identity_verdict`] 的取值。
///
/// # 为什么需要它（2026-08-21 实测，task #56）
///
/// 本机 20 个注册根现算身份，`repo_id_for_root` 给出三种答案：
///
/// | 返回 | 数量 | 真实情况 | 下游该做什么 |
/// | --- | --- | --- | --- |
/// | `Ok("git:…")` | 16 | 有身份 | 用它 |
/// | `Ok("path:…")` | 1 | **探明白了**，不属于任何仓 | **接受**，别再算 |
/// | `Err(_)` | 3 | **没问成**（裸 POSIX ×2 + `/mnt/c/…` ×1，在 Windows 上没有命名空间） | **重试**，且**绝不据此做删除类决定** |
///
/// 而 `project_identity` 表**只存 `git:` 行**（`record_identity_for_root` 的约束 1，
/// 理由仍然成立：`path:` id 不跨 checkout 稳定，记下来会让「查得到身份」变成一句
/// 不能信的话）。**于是后两种在报告里都渲染成 `canonical_id: null`**，第三种
/// 「还没扫到」也一样 —— 三个不同的事实、三种不同的处置，一个值。
///
/// 🔴 **类型本来就分得开**（`Result<RepoIdentity, ProbeError>`，`path:` vs `Err`），
/// 是**注册表这一层**把它压平的。这与本仓反复记的那条同一个判据、只是层不同：
/// `Probe::{Seen,Absent,Unreachable}`、`Probed<T>` 故意不给 `is_found()`、
/// 「降级要降到说不出来，不是降到另一个答案」。
///
/// # 为什么没有 `Option`
///
/// 「还没扫到」是 [`Self::NotProbed`] 这个**变体**，不是 `None`。一个
/// `Option<IdentityVerdict>` 会立刻长出 `.unwrap_or(NoIdentity)` 之类的调用点，
/// 而那正是把「没问成」挤进「没有」的那一步。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdentityVerdict {
    /// 判决表里没有这个根 —— **还没扫到**。等下一轮，别据此下任何结论。
    ///
    /// ⚠️ 也包括「这个库是 task #56 之前建的」：那时根本没有这张表。
    NotProbed,
    /// 问到了 —— 身份在 [`ProjectRootRow::canonical_id`] 里。
    ///
    /// ⚠️ 它与 `canonical_id` **不是同一个事实的两种说法**：身份行活过 checkout
    /// 被删（那是这张表存在的全部理由），而判决说的是**最后一次探测**的结果。
    /// 「有身份 + 本轮没问成」是一个真实且有用的状态。
    Resolved,
    /// **确认**这个根说不出跨系统身份。**接受**，别再算。
    ///
    /// 🔴 **它在两侧的含义不同，读 `roots` 输出的人必须知道**（2026-09-01）：
    ///
    /// | 探测路径 | `no_identity` 说的是 |
    /// | --- | --- |
    /// | 本机（Windows / Linux 本地 stat） | **确认**没有可用 remote —— 目录不在会走 [`Self::CheckoutMissing`] |
    /// | 走访问桥（WSL） | 「确认没有 remote」**或**「目录不在」，**分不出** |
    ///
    /// 原因：桥那一侧的 `git_config_path` 只有二态，把「起点不在」和「链上没有
    /// `.git`」压在同一个 `Absent` 里（本机那一侧 2026-09-01 已拆开）。
    ///
    /// ⚠️ **后果是不能把两侧的计数直接相加** —— 那等于把一个含义窄的数和一个
    /// 含义宽的数当成同一种。要么分侧报，要么等桥那一侧也做三态。
    NoIdentity {
        /// 哪一种：没有 `.git`，还是有 `.git` 但里面没 origin。
        why: String,
    },
    /// 🔴 **这个根的目录本身已不在磁盘上** —— checkout 被删 / 搬走 / 换了盘。
    ///
    /// 与 [`Self::NoIdentity`] 是两件事，而它们从前是同一格：
    ///
    /// | | 说的是 | 处置 |
    /// | --- | --- | --- |
    /// | `NoIdentity` | **确认**这个仓没有可用 remote | 接受，别再算 |
    /// | `CheckoutMissing` | 那个**目录没了** | 🔴 **已记下的 `canonical_id` 仍然有效**；别据此说这个项目没有身份 |
    ///
    /// 实测（2026-09-01）：某项目搬出宿主之后，它旧位置的两条根拿到
    /// `no_identity` + `why = "no .git anywhere on this path"`，**同时**带着搬走
    /// 之前记下的 `git:` 身份 —— 消费者看到一行自相矛盾的数据，而真相是
    /// 「目录没了」被说成了「确认这个仓没有 remote」。
    ///
    /// ⚠️ 也**不是** [`Self::Unresolved`]：那条说「没问成，重试」，而这里问成了 ——
    /// 答案就是「不在了」，重试不会变。
    CheckoutMissing,
    /// **没问成**（命名空间够不着 / 桥不通 / 权限 / 写库失败）。
    /// **重试**，且**绝不据此做删除类决定**。
    Unresolved {
        /// 探测报回来的原话 —— 「没问成」和「没问成：wsl.exe 超时」不是一回事。
        why: String,
    },
}

impl IdentityVerdict {
    /// 线上取值。**`project_identity_probe.outcome` 与 `svault roots` 共用这一份拼写**
    /// —— 两处各写一份必然漂开，而漂开时只表现为消费方看见一个不认识的值。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::NotProbed => "not_probed",
            Self::Resolved => "resolved",
            Self::NoIdentity { .. } => "no_identity",
            Self::CheckoutMissing => "checkout_missing",
            Self::Unresolved { .. } => "unresolved",
        }
    }

    /// 判决的理由；`resolved` / `not_probed` 没有理由可说。
    pub fn why(&self) -> Option<&str> {
        match self {
            Self::NoIdentity { why } | Self::Unresolved { why } => Some(why.as_str()),
            // 🔴 **不用 `_` 兜底**：将来加一个「问到一半」之类的变体时，
            // 这里要**编译不过**，而不是静默给出 `None`。
            // ⚠️ `CheckoutMissing` 归这一档：**理由就是判决本身**（那个目录不在了），
            // 再编一句 why 只会多一处要维护的措辞。这条闸 2026-09-01 加该变体时
            // 真的挡住了一次「顺手写个 `_ => None`」。
            Self::NotProbed | Self::Resolved | Self::CheckoutMissing => None,
        }
    }

    /// 从库里读回来。
    ///
    /// 🔴 **不认识的 outcome 读作 [`Self::Unresolved`]，不是 [`Self::NotProbed`]。**
    /// 一个未来版本写下的新变体意味着「那一侧知道点什么而我读不懂」——
    /// 报成「还没扫到」是**编造**，报成「没问成」至少方向是对的（重试、别删）。
    fn from_row(outcome: &str, detail: Option<String>) -> Self {
        // detail 为 NULL 只可能来自手工写的行（本模块每次都写）。回显 outcome
        // 会得到「no_identity: no_identity」这种同义反复 —— 说清楚它没有理由更好。
        let why = || {
            detail
                .clone()
                .unwrap_or_else(|| "(no detail recorded)".to_string())
        };
        match outcome {
            "resolved" => Self::Resolved,
            "no_identity" => Self::NoIdentity { why: why() },
            "checkout_missing" => Self::CheckoutMissing,
            "unresolved" => Self::Unresolved { why: why() },
            other => Self::Unresolved {
                why: format!("unrecognized verdict from a newer writer: {other}"),
            },
        }
    }
}

/// 一轮 [`TotalStore::sweep_registered_root_identities`] 的统计。
///
/// 🔴 `skipped_out_of_budget` 必须报出来：「本轮只看了一半」和「本轮全看过了」
/// 在结果上长得一模一样，静默截断读起来就是「覆盖完了」。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IdentitySweep {
    pub registered: usize,
    pub recorded: usize,
    pub already_probed: usize,
    pub no_remote: usize,
    /// 🔴 那个目录**不在磁盘上了** —— 与 `no_remote` 分开计数：一个说「这个仓
    /// 确认没有 remote」，一个说「那条路径上的东西没了」，而后者往往意味着
    /// **项目搬走了**，是个该被看见的信号。
    pub checkout_missing: usize,
    pub unresolved: usize,
    pub skipped_out_of_budget: usize,
    /// 连注册表都没读成 —— 这一轮**什么都没扫**，与「扫了但零结果」不同。
    pub unreadable: Option<String>,
}

const RAW_EVENTS_INDEX_DDL: &str = r#"
CREATE INDEX IF NOT EXISTS idx_raw_events_session ON raw_events(source_session_id);
CREATE INDEX IF NOT EXISTS idx_raw_events_project ON raw_events(project_root);
CREATE INDEX IF NOT EXISTS idx_raw_events_occurred ON raw_events(occurred_at_unix_ms);
"#;

/// 把 `rawevent::occurred_at_unix_ms` 暴露成一个 SQL 标量函数 `iso8601_to_unix_ms`。
///
/// 🔴 **目的是让归一化在数据搬运的同一趟里完成**，而不是搬完再回来逐行 `UPDATE`。
/// 后者的代价被行的形状放大得离谱：`raw_events` 的一行装着 `event_json`（真库上平均
/// 约 3 KB），而 SQLite 更新任意一列都要**重写整条记录** —— 93 万次 UPDATE ≈ 又一遍
/// 2.8 GB 的整表重写，还是随机顺序，外加 `secure_delete` 把每个旧页清零。实测这让迁移
/// 从「一趟」变成「三趟」。
///
/// 🔴 **解析仍然只有 Rust 那一处实现**。这里注册的是同一个函数的 SQL 门面，不是第二份
/// 规则 —— 用 SQL 表达式去切时间串必然与 Rust 分叉，而分叉的表现是**排序悄悄不对**，
/// 不报错。归一化规则（时区偏移、小数秒位数、闰年）见 [`crate::rawevent::occurred_at_unix_ms`]。
fn register_sql_functions(conn: &Connection) -> StoreResult<()> {
    use rusqlite::functions::FunctionFlags;
    conn.create_scalar_function(
        "iso8601_to_unix_ms",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let raw: Option<String> = ctx.get(0)?;
            Ok(raw
                .as_deref()
                .and_then(crate::rawevent::occurred_at_unix_ms))
        },
    )?;
    Ok(())
}

/// 投影台账：每一份「某 parser 对某源版本的解析」在这里有一行。
///
/// `origin` 记录这份投影**为什么**存在，因为留存策略跟着理由走（ADR-044 决定 7）：
///
/// - `rollback` —— 源字节变了，前一个源版本是唯一副本，**永不自动回收**；
/// - `reparse`  —— 源字节没变，被它取代的投影可回收；
/// - `append`   —— 并入当前投影，不新开；
/// - `unknown`  —— 🔴 **ADR-044 之前产生的行**。当时只有 `generation`，无从判断它当初是
///   回退还是重解析，因此**一律不进通用 GC**，只能由显式、可审计的用户操作清理。
///   这是「先动手后规范化」的直接代价，明写在这里而不是被悄悄吸收掉。
const PROJECTIONS_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS projections (
    source_type         TEXT    NOT NULL,
    source_location     TEXT    NOT NULL,
    source_path         TEXT    NOT NULL,
    source_revision     INTEGER NOT NULL,
    projection_revision INTEGER NOT NULL,
    parser_revision     INTEGER,
    origin              TEXT    NOT NULL,
    created_at          INTEGER NOT NULL,
    PRIMARY KEY (source_type, source_location, source_path,
                 source_revision, projection_revision)
);
"#;

/// 已经应用过的**开新代**操作（ADR-051 I7）。
///
/// # 为什么非有不可
///
/// `Rollback` 与 `Reparse` 开新代。开新代的操作不幂等时，一次崩溃就留下一代垃圾：
///
/// ```text
/// ① 总库成功 Rollback，推进 source_revision
/// ② UI 索引提交前进程退出
/// ③ UI 仍是旧游标 ⇒ 下轮再次检出 rollback
/// ④ 总库再次推进 source_revision
/// ⑤ Rollback 的旧版本**按设计永不自动回收**（见 `Projection::Rollback` 的注释：
///    磁盘上那段内容已经不存在，前一个源版本是它的唯一副本）
/// ```
///
/// 于是**每崩一次留一代不可回收的源版本**。`Reparse` 稍好（它取代被超越的那代），
/// 但也会多开一代。
///
/// 🔴 **`Append` 不在这里** —— 它靠 `seq` 去重，重放同一批事件天然幂等。需要 token
/// 的恰恰是「开新代」这个动作本身。
///
/// `token` 作主键即唯一约束：同一个 token 第二次应用直接命中，返回**原来的头**。
const APPLIED_PROJECTIONS_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS applied_projections (
    token               TEXT    PRIMARY KEY,
    source_type         TEXT    NOT NULL,
    source_location     TEXT    NOT NULL,
    source_path         TEXT    NOT NULL,
    -- 那一次操作产生的头。重复应用时原样返回它 —— 而不是「当前头」：
    -- 中间可能已经有别的操作推进过，返回当前头会让调用方以为自己的操作生效了。
    source_revision     INTEGER NOT NULL,
    projection_revision INTEGER NOT NULL,
    applied_at          INTEGER NOT NULL
);
"#;

/// 每个源文件的当前头：哪个源版本、以及该源版本的哪份投影是**当前**的。
///
/// 🔴 它取代的是 `read_session` 里那个 `MAX(generation)` 相关子查询。区别不是性能，是
/// **可表达性**：`MAX` 只能表达「编号最大的那一代」，表达不了「这份投影被替换成了空」
/// ——而一个新解析器合法地对某文件产出零事件时，正需要表达它（ADR-044 决定 2 / G2）。
/// 投影替换日志 —— **change-feed 的底座**（ADR-044 决定 6）。
///
/// 🔴 为什么「只读当前投影」不够：消费者持久化 offset 游标之后，若一次重投影把旧集合
/// `{A, B}` 换成 `{A}`，增量流最多重发 A，**没有任何记录要求它删除已物化的 B** ——
/// 消费者会永久保留一条已经不存在的事件。仅靠 upsert 永远删不掉「不再出现」的东西。
///
/// 所以替换本身必须成为一条**可拉取的记录**：消费者读到它，就按 source 原子替换自己
/// 那一份物化，而不是逐事件比对。
///
/// 这张表随 `raw_events` 一起被 erase 清理吗？**不**：它不含正文，只有坐标与计数，
/// 且删掉它会让落后的消费者永远收不到「该删了」的通知。ADR-027 的墓碑走另一条路。
const PROJECTION_LOG_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS projection_log (
    seq                 INTEGER PRIMARY KEY AUTOINCREMENT,
    at                  INTEGER NOT NULL,
    source_type         TEXT    NOT NULL,
    source_location     TEXT    NOT NULL,
    source_path         TEXT    NOT NULL,
    old_source_revision     INTEGER,
    old_projection_revision INTEGER,
    new_source_revision     INTEGER NOT NULL,
    new_projection_revision INTEGER NOT NULL,
    reason              TEXT    NOT NULL
);
"#;

const CURRENT_HEAD_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS current_head (
    source_type         TEXT    NOT NULL,
    source_location     TEXT    NOT NULL,
    source_path         TEXT    NOT NULL,
    source_revision     INTEGER NOT NULL,
    projection_revision INTEGER NOT NULL,
    updated_at          INTEGER NOT NULL,
    PRIMARY KEY (source_type, source_location, source_path)
);
"#;

const DATA_KEYS_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS data_keys (
    key_id          TEXT PRIMARY KEY,
    source_type     TEXT NOT NULL,
    source_location TEXT NOT NULL,
    source_path     TEXT NOT NULL,
    project_root    TEXT NOT NULL,
    wrapped_key     TEXT NOT NULL,
    created_at      INTEGER NOT NULL,
    UNIQUE (source_type, source_location, source_path, project_root)
);
CREATE INDEX IF NOT EXISTS idx_data_keys_source ON data_keys(source_path);
CREATE INDEX IF NOT EXISTS idx_data_keys_project ON data_keys(project_root);
"#;

/// 不可变 RawEvent 总库句柄。`.clone()` 不提供——单写者持有（ADR-020：同一时刻单写者）；
/// 读者经只读连接或 WAL 并发读，不与写竞争。
pub struct TotalStore {
    conn: Mutex<Connection>,
    cipher: StoreCipher,
    /// 本进程已为哪些 `(source_type, source_location, project_root)` 记过身份。
    ///
    /// 🔴 **它省掉的是文件 IO，不是正确性**：算身份要读 `.git/config`，而
    /// `apply_projection` 是**每个文件一次**的热路径 —— 一个大仓的几百个会话文件会把
    /// 同一个 `.git/config` 读几百遍。写入本身是 upsert（幂等），缓存只是别再问一遍。
    ///
    /// ⚠️ 代价写在这里：**同一进程内身份变更不会被发现**（改了 remote 要重启才看得到）。
    /// 可接受 —— 身份变更极罕见，而 upsert 保证下一个进程立刻纠正；反过来，
    /// 为了捕捉一件几乎不发生的事而每个文件读一次盘，是明确的坏交易。
    /// 本进程已经问过身份的项目根。**键只有根** —— 身份是根的属性，与谁扫到它无关
    /// （同 `project_identity` 主键那次收敛）。
    identity_seen: Mutex<std::collections::HashSet<String>>,
}

/// 递增注册表修订号。**只在真的插入了新根时调用**（见 `register_project_root`）。
///
/// 失败静默：修订号停在原地只会让 token 少区分一个维度（偏保守），
/// 而让注册失败会让整套发现停摆 —— 后者严重得多。
fn bump_attribution_revision(conn: &Connection) {
    let _ = conn.execute(
        "INSERT INTO store_meta (k, v) VALUES ('attribution_revision', '1')          ON CONFLICT(k) DO UPDATE SET v = CAST(CAST(v AS INTEGER) + 1 AS TEXT)",
        [],
    );
}

impl TotalStore {
    /// 打开（或新建）磁盘总库，WAL 模式，建表幂等。父目录自动创建。
    ///
    /// 密钥只从 OS keychain 读取。全新库或既有明文库可创建首把密钥；若检测到已有密文但
    /// keychain 中没有对应密钥则硬失败，绝不生成新钥匙覆盖并造成静默数据丢失。
    pub fn open(path: &Path) -> StoreResult<Self> {
        if let Some(parent) = path.parent() {
            crate::probe::create_dir_all(parent)?;
            restrict_permissions(parent, 0o700);
        }
        let encrypted_store = store_has_encrypted_rows(path)?;
        let key = match load_os_key()? {
            Some(key) => key,
            None if encrypted_store => return Err(CryptoError::MissingKey.into()),
            None => create_os_key()?,
        };
        let conn = Connection::open(path)?;
        restrict_permissions(path, 0o600);
        Self::from_conn(conn, key)
    }

    /// **只读地**打开一个已存在的总库 —— 不存在就说不存在，**绝不建**。
    ///
    /// 🔴 [`Self::open`] 的签名**说不出「不存在」**：它 `create_dir_all` + 让 SQLite
    /// 建文件 + 必要时 `create_os_key()`。也就是说「我只想读一下」的调用方一旦直接
    /// 用它，就会**凭空给用户造一个空库和一把 OS 密钥，而且不报错**。
    ///
    /// 后果不是理论上的：QuotaBar 的 `known_project_identities` 为此在调用点自己
    /// 先探一次，并在注释里写明「探测在这里不是为了决定有没有身份，而是为了不让
    /// `TotalStore::open` 顺手建一个空库」—— **一条本该由 API 消化掉的知识外泄到了
    /// 消费者身上**，而忘了探不会编译失败，只会安静地建库。
    ///
    /// 三态照 [`Probed`] 的既有语义：`Absent` 是事实（这台机器还没建过总库，常态）；
    /// `Unknown` 是**没问成**（权限/句柄/UNC/密钥），调用方不得把它当成「没有」。
    ///
    /// ⚠️ 想要「不在就建」的仍然用 [`Self::open`] —— 写入方（扫描器、同步器）本来
    /// 就该建。这里加的是**读**的那一半，不是替换。
    pub fn open_existing(path: &Path) -> Probed<Self> {
        // 存在性探测走 `probe.rs`，不裸调 `std::fs::metadata` —— 那条边界由
        // `clippy::disallowed_methods` + `verify-agents-md.mjs` 守着，而它在这次
        // 改动里**当场抓到了我**：第一版就是裸调 metadata。
        match crate::probe::LocalBackend::unanchored().probe(path, Deadline::unbounded()) {
            Probed::Found(_) => {}
            Probed::Absent => return Probed::Absent,
            Probed::Unknown(e) => return Probed::Unknown(e),
        }
        match Self::open(path) {
            Ok(store) => Probed::Found(store),
            // 文件在、却打不开 —— 密钥缺失 / 库损坏 / 权限。这**不是**「没有总库」。
            Err(e) => Probed::Unknown(crate::probe::ProbeError::new(path, e)),
        }
    }

    /// 使用宿主提供的密钥打开数据库。适用于测试和不由默认 OS keychain 管理密钥的嵌入方。
    pub fn open_with_key(path: &Path, key: StoreKey) -> StoreResult<Self> {
        if let Some(parent) = path.parent() {
            crate::probe::create_dir_all(parent)?;
            restrict_permissions(parent, 0o700);
        }
        let conn = Connection::open(path)?;
        restrict_permissions(path, 0o600);
        Self::from_conn(conn, key)
    }

    /// 内存库（测试用）。
    pub fn open_in_memory() -> StoreResult<Self> {
        Self::from_conn(Connection::open_in_memory()?, StoreKey::generate())
    }

    /// 开一个**写**事务 —— `BEGIN IMMEDIATE`，不是 rusqlite 默认的 `DEFERRED`。
    ///
    /// 🔴 DEFERRED 先拿读锁，写第一行时才升级成写锁。两个写者同时走到升级那一步
    /// 就是**死锁**，而 SQLite 对它会**立刻**返回 `SQLITE_BUSY` —— `busy_timeout`
    /// 管不了这一种（继续等只会互相等下去，所以 SQLite 干脆不等）。
    ///
    /// IMMEDIATE 一开始就拿写锁：排队变成「等」而不是「撞」，`busy_timeout` 这才
    /// 真正生效。**两者缺一不可**，所以规则放在同一处，让调用点没得选。
    fn write_tx(conn: &mut Connection) -> StoreResult<rusqlite::Transaction<'_>> {
        Ok(conn.transaction_with_behavior(TransactionBehavior::Immediate)?)
    }

    fn from_conn(conn: Connection, key: StoreKey) -> StoreResult<Self> {
        // WAL 让读不挡写（QuotaBar 常驻写、未来 TumeFlow 并发读）。
        conn.pragma_update(None, "journal_mode", "WAL")?;
        // 🔴 **两个写者时，缺的不是「锁」，是「等」。**
        //
        // WAL 已经保证同一时刻只有一个写者，且写不挡读 —— 原子性 SQLite 自己管。
        // 但 `busy_timeout` 默认是 **0**：第二个写者**立刻**拿到 `SQLITE_BUSY`
        // 而不是稍等一下。于是「TumeFlow 也开始扫会话」（QuotaBar task #44）会
        // 变成随机失败，而失败点离原因很远。
        //
        // ⚠️ **在这里加一把自己的文件锁是错的方向** —— 那会与 SQLite 自己的锁
        // 各管一半，而两套锁的边界不重合时没有任何东西会报错（本仓判例：
        // 「编译器已经守住的事，不要再用正则守一遍」的同族）。
        //
        // 30 秒：一次 `apply_projection` 的写事务是毫秒级，30 秒足够穿过任何正常
        // 的排队；真的等满说明对面卡住了，那时报 `BUSY` 才是**信息**而不是噪声。
        conn.busy_timeout(std::time::Duration::from_secs(30))?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "secure_delete", "ON")?;
        // 🔴 WAL 文件默认**停在历史最高水位**（`journal_size_limit = -1`），checkpoint 只
        // 把页搬回主库、不截断文件。一次 schema 迁移是一个覆盖全表的大事务，于是 2.8 GB
        // 的真库把 WAL 撑到 **5.33 GB 并且再也不还** —— 实测那之后 WAL 里有效的只剩 653 页
        // （2.6 MB），也就是 5.3 GB 纯占盘。它不拖慢读（SQLite 只读有效区），所以**没有任何
        // 迹象**会提示这件事，只有 `du` 看得见。
        //
        // 每提一次 `PARSER_REVISION` 就会再撑一次，因此这里必须设上限而不是事后手工收拾。
        // 64 MB 足够常态写入批量用，超出的部分在 checkpoint 后归还。
        conn.pragma_update(None, "journal_size_limit", 64 * 1024 * 1024)?;
        register_sql_functions(&conn)?;
        let store = Self {
            conn: Mutex::new(conn),
            cipher: StoreCipher::new(key),
            identity_seen: Mutex::new(std::collections::HashSet::new()),
        };
        store.migrate()?;
        store.validate_cipher()?;
        Ok(store)
    }

    fn validate_cipher(&self) -> StoreResult<()> {
        let conn = self.conn.lock().unwrap();
        let sample: Option<EncryptedRow> = conn
            .query_row(
                "SELECT offset, source_type, source_location, source_path, source_session_id, seq, source_revision, projection_revision, aad_version, project_root, event_json FROM raw_events ORDER BY offset LIMIT 1",
                [],
                EncryptedRow::from_sql,
            )
            .optional()?;
        if let Some(row) = sample {
            let mut cache = HashMap::new();
            self.decode_event_on(&conn, &mut cache, &row)?;
        }
        Ok(())
    }

    fn migrate(&self) -> StoreResult<()> {
        let mut conn = self.conn.lock().unwrap();
        conn.execute_batch(
            r#"
            -- 墓碑带作用域：同一字符串值在不同维度（会话 vs 路径 vs 项目根）含义不同，
            -- 不带 scope 会让删 project_root=/work 误连带隐藏 source_path=/work 的无关事件。
            CREATE TABLE IF NOT EXISTS tombstones (
                scope         TEXT    NOT NULL,
                key           TEXT    NOT NULL,
                tombstoned_at INTEGER NOT NULL,
                PRIMARY KEY (scope, key)
            );
            -- 总库自身的元数据（回填/catch-up 状态等）。
            CREATE TABLE IF NOT EXISTS store_meta (
                k TEXT PRIMARY KEY,
                v TEXT NOT NULL
            );

            -- 项目的规范身份见 [`PROJECT_IDENTITY_DDL`]（提成常量，迁移要复用它）。

            -- 🔴 **项目根注册表（ADR-050 决定 3）—— 与 `project_identity` 分开，
            -- 因为两者回答的是不同的问题。**
            --
            --   project_identity  这个项目在**别的系统里**叫什么（跨 checkout 的身份）
            --   project_root_registry  这个路径**是不是**一个项目根（归属的输入）
            --
            -- 合成一张表看起来省事，但 `project_identity` 有一条刻意的约束：
            -- **不写 `path:` 兜底行**（那种 id 不跨 checkout 稳定，记下来会让「查得到
            -- 身份」变成一句不能信的话）。而归属恰恰需要那些没有 remote 的项目根 ——
            -- 「这是个根」不要求它跨 checkout 稳定。约束冲突 ⇒ 两张表。
            --
            -- 🔴 **不带 source_type / source_location**：一个路径是不是项目根，与
            -- 「谁在什么位置扫到它」无关。带上它们会让同一个根按发现者分裂成多行，
            -- 而归属时又得决定听谁的 —— 那是凭空造出来的分歧。
            CREATE TABLE IF NOT EXISTS project_root_registry (
                -- 归一化后的比较键（小写、正斜杠、无尾斜杠）。主键用它，
                -- 于是同一个根的不同写法不会重复入表。
                root_key        TEXT PRIMARY KEY,
                -- 原始形式 —— 归属结果返回它，归一化只是本层的内部细节。
                root_path       TEXT NOT NULL,
                -- 怎么发现的：git / marker / scan / configured（`attribution::RootSource`）。
                root_source     TEXT NOT NULL,
                first_seen_ms   INTEGER NOT NULL,
                last_seen_ms    INTEGER NOT NULL
            );

            -- 探测过的候选路径及其结果（ADR-050，评审 [P2]）。
            --
            -- 🔴 **`None` 与 `Failed` 也要落**。发现只过滤「已归属」的候选，于是
            -- 「确认没有根」的那些每一轮后台刷新都会被重新探测一遍 —— 实测 36 条，
            -- 其中 WSL 形式的每条一次跨 VM 往返。候选集因此永远降不到零。
            --
            -- 三态分开记，因为它们的**有效期不同**：`none` 是一个稳定的事实
            -- （那个目录确实没有根），`unreachable` 是一个暂时的故障（WSL 卡住了）。
            -- 拿同一个 TTL 套两者，要么让故障闷太久，要么让稳定事实白探。
            CREATE TABLE IF NOT EXISTS project_root_probe (
                path          TEXT PRIMARY KEY,
                outcome       TEXT NOT NULL,   -- 'none' | 'unreachable'
                last_probe_ms INTEGER NOT NULL
            );

            -- 身份探测的**结论**（task #56）。与上面那张是同一个形状、不同的问题：
            --
            --   project_root_probe      这条路径**是不是**项目根（归属的输入）
            --   project_identity_probe  这个根的**身份问出来没有**（报告的输入）
            --
            -- 🔴 **与 `project_identity` 分开**，理由和 `project_root_registry`
            -- 与它分开是同一条：那张表有一条刻意的约束（**只写 `git:` 行**），
            -- 而「确认没有身份」「没问成」恰恰是**没有** `git:` 的那些。塞进去就得
            -- 造一个假的 canonical_id，那会污染 `all_project_identities()`。
            --
            -- 主键是**根**、一行、后写覆盖：判决问的是「**最后一次**探测怎么样」，
            -- 与 `project_identity` 刻意保留多行（仓库迁移 vs 路径被复用，两者
            -- 看起来一样而后果相反）是两种不同的时间语义。
            CREATE TABLE IF NOT EXISTS project_identity_probe (
                project_root  TEXT PRIMARY KEY,
                -- 'resolved' | 'no_identity' | 'unresolved'（`IdentityVerdict::as_str`）
                outcome       TEXT NOT NULL,
                -- 为什么 —— 「没问成」和「没问成：桥超时」不是一回事。
                detail        TEXT,
                last_probe_ms INTEGER NOT NULL
            );
            "#,
        )?;

        // ── project_identity：去掉观察者列（2026-08-14） ─────────────────────────
        //
        // 🔴 **主键曾是 `(source_type, source_location, project_root, canonical_id)`。**
        // 隔壁 `project_root_registry` 的注释早就写下了反对它的理由，只是写给了另一张表：
        // 「一个路径是不是项目根，与谁在什么位置扫到它无关；带上它们会让同一个根按
        // 发现者分裂成多行」。**这句话对身份一字不差地成立** —— 一个路径属于哪个仓库，
        // 由那里的 `.git/config` 决定，与观察者无关。实测就是那个形状：同一个
        // QuotaBar 根、同一个 `git:` 身份、**三行**（claude_code@local /
        // claude_code@wsl / codex@local），而唯一的消费者 `project_roots_report`
        // 把这两列**整个忽略**（它建的是 `root → cid`）。
        //
        // 🔴 **更要紧的是它让一件事根本没法表达**：注册表只有根、没有观察者，所以
        // 「为注册表里每个根记一次身份」写不出来。身份记录因此只能挂在**事件投影**上，
        // 于是一个近期没有活动的项目 —— 哪怕注册表认得它、`.git` 就在那儿 ——
        // 永远拿不到身份。实测：`wsl:Ubuntu-22.04:/home/<user>/workspace/QuotaBar` 是
        // `root_source=git` 的注册根，桥读得到它的 origin，而身份表里 **0 行**，
        // 因为那个项目最后一个会话文件停在一个月前。
        //
        // 这与本表自己的文档也是矛盾的：它写着身份要「**在扫描时**记下来，趁 `.git`
        // 还在」，而实现挂在投影时。现在两者对齐。
        //
        // 合并规则：`MIN(first_seen_ms)` / `MAX(last_seen_ms)` —— 观察者不同不代表
        // 看到的是不同的东西，取并集是对「什么时候第一次/最后一次看到这个身份」的
        // 忠实回答。
        //
        // `idx_identity_root` 一并删除：`project_root` 现在是主键的前导列，
        // SQLite 自带索引，再建一棵是纯开销。
        let identity_has_observer: bool = conn
            .prepare(
                "SELECT 1 FROM pragma_table_info('project_identity') WHERE name='source_type'",
            )?
            .query_row([], |_| Ok(true))
            .optional()?
            .unwrap_or(false);
        if identity_has_observer {
            // 整个重写包在一个事务里 —— 与 `raw_events` 那次迁移同一条理由：
            // 中途夭折会留下一个空的新表加一个藏着数据的旧表，而下次启动看不出区别。
            let tx = Self::write_tx(&mut conn)?;
            tx.execute_batch(&format!(
                r#"
                DROP INDEX IF EXISTS idx_identity_root;
                DROP INDEX IF EXISTS idx_identity_cid;
                ALTER TABLE project_identity RENAME TO project_identity_pre_rootkey;
                {PROJECT_IDENTITY_DDL}
                INSERT INTO project_identity
                       (project_root, canonical_id, first_seen_ms, last_seen_ms)
                SELECT project_root, canonical_id, MIN(first_seen_ms), MAX(last_seen_ms)
                  FROM project_identity_pre_rootkey
                 GROUP BY project_root, canonical_id;
                DROP TABLE project_identity_pre_rootkey;
                {PROJECT_IDENTITY_INDEX_DDL}
                "#
            ))?;
            tx.commit()?;
        } else {
            conn.execute_batch(PROJECT_IDENTITY_DDL)?;
            conn.execute_batch(PROJECT_IDENTITY_INDEX_DDL)?;
        }

        let raw_exists: bool = conn
            .prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name='raw_events'")?
            .query_row([], |_| Ok(true))
            .optional()?
            .unwrap_or(false);
        // 一次性把要用的列探测算完 —— 闭包持有 `&conn` 的借用会和后面的
        // `conn.transaction()`（需要 `&mut`）打架，而事务是这段代码的核心。
        let (has_generation, has_source_revision, has_occurred_ms) = if raw_exists {
            let probe = |name: &str| -> StoreResult<bool> {
                Ok(conn
                    .prepare("SELECT 1 FROM pragma_table_info('raw_events') WHERE name = ?1")?
                    .query_row([name], |_| Ok(true))
                    .optional()?
                    .unwrap_or(false))
            };
            (
                probe("generation")?,
                probe("source_revision")?,
                probe("occurred_at_unix_ms")?,
            )
        } else {
            (false, false, false)
        };

        if !raw_exists {
            // 全新库：直接建当前形状的表。
            conn.execute_batch(RAW_EVENTS_DDL)?;
        } else if !has_source_revision {
            // 🔴 既有库 → 拆 `generation` 为 `(source_revision, projection_revision)`（ADR-044 决定 1）。
            //
            // **映射刻意保守**：`source_revision = generation`，`projection_revision = 0`。
            // 也就是说每一个旧代都被当作**独立的源版本**，而源版本永不自动回收。
            // 保守是必需的：当时只有一个整数，无从判断某一代当初是「文件被重写」还是
            // 「换了解析器」，而这两者留存价值相反。猜错一次就是不可逆的数据损坏，
            // 所以一律按「可能是唯一副本」处理（`origin = 'unknown'`，见 PROJECTIONS_DDL）。
            //
            // 🔴 **`aad_version = 1` 是这次迁移能便宜完成的全部原因**：AAD 把 `generation`
            // 编进了 AES-GCM 的认证数据，而它是长度前缀拼接的，字段一变既有密文全部解不开。
            // 由于 `source_revision` 取的就是 `generation` 的原值，v1 的 AAD 字节**逐字节
            // 不变** —— 一行都不用重新加密。新写入的行用 v2（含两个 revision），由这一列
            // 自描述，不必靠推断。见 [`event_aad`]。
            //
            // 更早的库（`generation` 之前的五列唯一）在这里一并落到当前形状：它没有
            // `generation` 列，全部归为第 0 版本。
            //
            // 🔴 **整个重写包在一个事务里**（评审 [P1]）。`execute_batch` 不自带事务，
            // 每条语句独立提交 —— 进程在中途被杀、磁盘写满或复制失败时，磁盘上会留下
            // 一个**空的新 `raw_events`** 加一个藏着全部数据的 `raw_events_pre_srev`。
            // 下次启动看到 `source_revision` 已存在，于是**跳过迁移**：数据还在，但从此
            // 不可见。失败必须整体回滚。
            //
            // 🔴 **索引要显式重建**（评审 [P2]）。SQLite 里 `ALTER TABLE … RENAME` 会把
            // 索引一起带到旧表名下、**索引名不变**；随后 `CREATE INDEX IF NOT EXISTS`
            // 因为同名索引仍存在而**静默跳过**；最后 `DROP TABLE` 把它们一并删掉。
            // 净效果：迁移后的库一个索引都没有，而这不会报错 —— 只是会话/项目查询与
            // erase 全部退化成全表扫。实测复现过。
            //
            // 🔴 **索引不在这里建**（见 [`RAW_EVENTS_INDEX_DDL`]）。它们原本写在
            // `RAW_EVENTS_DDL` 里，于是这条路上每灌一行都要维护 4 棵 B 树，而其中三棵
            // 对灌数据本身毫无用处。现在只剩唯一约束那棵（它由表定义带来，躲不掉）。
            //
            // 🔴 **`occurred_at_unix_ms` 在这一趟里就算好**（`iso8601_to_unix_ms`）。
            // 搬完再逐行 `UPDATE` 是又一遍整表重写 —— 这一列不参与唯一键、也不参与
            // 排序以外的任何判断，放进 SELECT 是纯粹的白拿。
            let generation_expr = if has_generation { "generation" } else { "0" };
            let tx = Self::write_tx(&mut conn)?;
            tx.execute_batch(&format!(
                r#"
                DROP INDEX IF EXISTS idx_raw_events_session;
                DROP INDEX IF EXISTS idx_raw_events_project;
                DROP INDEX IF EXISTS idx_raw_events_occurred;
                ALTER TABLE raw_events RENAME TO raw_events_pre_srev;
                {RAW_EVENTS_DDL}
                INSERT INTO raw_events
                    (offset, ingested_at, schema_version, source_type, source_location,
                     source_path, source_session_id, seq, source_revision, projection_revision,
                     aad_version, event_type, occurred_at, occurred_at_unix_ms,
                     project_root, event_json)
                SELECT offset, ingested_at, schema_version, source_type, source_location,
                       source_path, source_session_id, seq, {generation_expr}, 0,
                       1, event_type, occurred_at, iso8601_to_unix_ms(occurred_at),
                       project_root, event_json
                  FROM raw_events_pre_srev;
                DROP TABLE raw_events_pre_srev;
                "#
            ))?;
            tx.commit()?;
        }
        // 🔴 上一轮迁移半途夭折留下的残骸：新表已建但数据还在临时表里。
        //
        // 有了事务这**不该**发生，但「不该发生」不是「不会发生」—— 事务是新加的，
        // 而磁盘上可能已经存在旧代码留下的残骸。检测到就把数据搬回来，而不是让它
        // 静默地永远不可见。搬完再删，顺序反了就是真丢数据。
        let orphan: bool = conn
            .prepare(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='raw_events_pre_srev'",
            )?
            .query_row([], |_| Ok(true))
            .optional()?
            .unwrap_or(false);
        if orphan {
            let stranded: i64 =
                conn.query_row("SELECT COUNT(*) FROM raw_events_pre_srev", [], |r| r.get(0))?;
            log::warn!(
                "[total-store] found {stranded} rows stranded in raw_events_pre_srev \
                 from an interrupted migration; recovering"
            );
            // 🔴 按残骸**自己的形状**搬，不能假定。残骸可能来自两种中断：
            //
            // - 旧形状（只有 `generation`）→ 映射到 `(generation, 0)`，AAD v1；
            // - 新形状（已有三列）→ **原样带过来**。
            //
            // 硬写 `aad_version = 1` 会让新形状的行解不开（AAD 字节对不上），
            // 而失败形态是 `read_session` 返回空、不报错 —— 第一版正是这么写的。
            let orphan_has_srev: bool = conn
                .prepare(
                    "SELECT 1 FROM pragma_table_info('raw_events_pre_srev')                      WHERE name = 'source_revision'",
                )?
                .query_row([], |_| Ok(true))
                .optional()?
                .unwrap_or(false);
            let orphan_has_gen: bool = conn
                .prepare(
                    "SELECT 1 FROM pragma_table_info('raw_events_pre_srev')                      WHERE name = 'generation'",
                )?
                .query_row([], |_| Ok(true))
                .optional()?
                .unwrap_or(false);
            let coords = if orphan_has_srev {
                "source_revision, projection_revision, aad_version"
            } else if orphan_has_gen {
                "generation, 0, 1"
            } else {
                "0, 0, 1"
            };
            let tx = Self::write_tx(&mut conn)?;
            let recovered = tx.execute(
                &format!(
                    r#"INSERT OR IGNORE INTO raw_events
                         (offset, ingested_at, schema_version, source_type, source_location,
                          source_path, source_session_id, seq, source_revision,
                          projection_revision, aad_version, event_type, occurred_at,
                          occurred_at_unix_ms, project_root, event_json)
                       SELECT offset, ingested_at, schema_version, source_type, source_location,
                              source_path, source_session_id, seq, {coords},
                              event_type, occurred_at, iso8601_to_unix_ms(occurred_at),
                              project_root, event_json
                         FROM raw_events_pre_srev"#
                ),
                [],
            )?;
            tx.execute_batch("DROP TABLE raw_events_pre_srev;")?;
            tx.commit()?;
            log::info!("[total-store] recovered {recovered} stranded rows");
        }
        // 归一化时间列可能单独缺席（库已是新形状、只是早于决定 5）。ALTER 加列比
        // 重建整表便宜得多，且这一列不参与唯一键，加了不影响既有行的可读性。
        // 索引不在这里建 —— 统一由末尾的 `RAW_EVENTS_INDEX_DDL` 无条件补，否则这个
        // 分支跑过一次之后索引就再没有第二次机会被建出来。
        if raw_exists && !has_occurred_ms && has_source_revision {
            conn.execute_batch("ALTER TABLE raw_events ADD COLUMN occurred_at_unix_ms INTEGER;")?;
        }
        conn.execute_batch(PROJECTIONS_DDL)?;
        conn.execute_batch(APPLIED_PROJECTIONS_DDL)?;
        conn.execute_batch(CURRENT_HEAD_DDL)?;
        conn.execute_batch(PROJECTION_LOG_DDL)?;
        // 🔴 **回填是一次性的，用标记闸住，别靠 `INSERT OR IGNORE` 幂等来兜。**
        //
        // 幂等只保证结果不错，不保证不做功：下面两条都要在 93 万行上 `GROUP BY`
        // 七列（实测 4.3s，冷缓存更久），而这在**每次启动**都会重来一遍，只为算出一份
        // 与上次逐字相同的答案。`apply_projection` 之后自己维护这两张表，所以这段只服务
        // ADR-044 之前的旧行 —— 一次做完就再也不需要。
        //
        // 判据用显式标记而不是「`current_head` 是不是空的」：一个合法为空的库（还没有
        // 任何事件）每次启动都会重跑，而它恰恰是最不需要回填的那种。
        let coords_backfilled: bool = conn
            .query_row(
                "SELECT 1 FROM store_meta WHERE k = 'adr044_coords_backfilled'",
                [],
                |_| Ok(true),
            )
            .optional()?
            .unwrap_or(false);
        if !coords_backfilled {
            // 台账与头的回填：对既有行是一次性补齐，对新库是空操作。放在建表之后、且用
            // `INSERT OR IGNORE`，所以重复启动幂等。
            //
            // `current_head` 取每个源文件的 `MAX(source_revision, projection_revision)` ——
            // 与 `read_session` 此前的 `MAX(generation)` 语义**完全一致**，所以这次迁移
            // 不改变任何一个已有查询的答案。这是刻意的：数据模型分层与行为变更分两步走，
            // 各自可独立回退。
            conn.execute_batch(
                r#"
            INSERT OR IGNORE INTO projections
                (source_type, source_location, source_path,
                 source_revision, projection_revision, parser_revision, origin, created_at)
            SELECT source_type, source_location, source_path,
                   source_revision, projection_revision, NULL, 'unknown',
                   COALESCE(MIN(ingested_at), 0)
              FROM raw_events
             GROUP BY source_type, source_location, source_path,
                      source_revision, projection_revision;

            -- 🔴 头从**分组结果**里选，不要在 `raw_events` 上写关联子查询。
            --
            -- 原先那版对外层的每一行都跑一次「按该 source_path 取最大 (srev, prev)」的
            -- 子查询。唯一索引是 `(type, loc, path, session, seq, srev, prev)` —— 过滤列
            -- 是前缀，但要取的两列被 `session, seq` 隔在后面，所以每次子查询都得把该路径
            -- 下的行全扫一遍再排序。93 万行 × 每路径上千行 ≈ 十亿级，实测在 2.8 GB 真库
            -- 上跑了 90 秒还没出结果，而这发生在同步的 `open()` 里 —— 界面全程不出来。
            --
            -- 分组本身上面那条语句已经做过一次（1445 组），头只是「每个 path 里 (srev,
            -- prev) 最大的那一组」。窗口函数在**那 1445 行**上选，是常数级的。
            INSERT OR IGNORE INTO current_head
                (source_type, source_location, source_path,
                 source_revision, projection_revision, updated_at)
            SELECT source_type, source_location, source_path,
                   source_revision, projection_revision, updated_at
              FROM (
                SELECT source_type, source_location, source_path,
                       source_revision, projection_revision,
                       COALESCE(MAX(ingested_at), 0) AS updated_at,
                       ROW_NUMBER() OVER (
                           PARTITION BY source_type, source_location, source_path
                           ORDER BY source_revision DESC, projection_revision DESC) AS rn
                  FROM raw_events
                 GROUP BY source_type, source_location, source_path,
                          source_revision, projection_revision)
             WHERE rn = 1;
            "#,
            )?;
            conn.execute(
                "INSERT OR REPLACE INTO store_meta (k, v) VALUES ('adr044_coords_backfilled', '1')",
                [],
            )?;
        }
        // 回填归一化时间：只补 NULL 的行，所以重复启动幂等，且一次全库回填之后
        // 常态启动是一次索引扫、零写入。
        //
        // 解析规则仍然只有 Rust 那一处 —— `iso8601_to_unix_ms` 是它的 SQL 门面
        // （见 [`register_sql_functions`]），不是第二份实现。用 SQL 表达式去切时间串
        // 必然与 Rust 分叉，而分叉的表现是**排序悄悄不对**，不报错。
        //
        // 🔴 走**一条 UPDATE**，不是「读进 Vec 再逐行 execute」。后者在真库上是 93 万
        // 次往返 + 93 万条记录重写（行里装着 `event_json`，改任意一列都要重写整条），
        // 而重写路径在上面已经把这一列在搬运那一趟里算好了 —— 到这里通常一行都不剩。
        // 这条只服务「库已是新形状、只差这一列」那个分支。
        let filled = conn.execute(
            "UPDATE raw_events SET occurred_at_unix_ms = iso8601_to_unix_ms(occurred_at)
              WHERE occurred_at IS NOT NULL AND occurred_at_unix_ms IS NULL",
            [],
        )?;
        if filled > 0 {
            log::info!("[total-store] normalized occurred_at for {filled} rows");
        }
        // 🔴 二级索引**无条件**在这里建（`IF NOT EXISTS` 幂等，已存在时是零成本的
        // 元数据查询）。放在数据搬完之后，灌数据时就不必维护它们；放在所有分支之外，
        // 「表已改形状但索引没建成」的中断就还有第二次机会被补上 —— 放进迁移分支里的话
        // 下次启动会因为 `source_revision` 已存在而整段跳过，库从此没有索引且不报错。
        conn.execute_batch(RAW_EVENTS_INDEX_DDL)?;
        conn.execute_batch(DATA_KEYS_DDL)?;
        drop(conn);
        self.migrate_event_envelopes()?;
        Ok(())
    }

    fn migrate_event_envelopes(&self) -> StoreResult<()> {
        let mut conn = self.conn.lock().unwrap();
        // 🔴 **完成标记要被当闸读，不然它只是个装饰。**
        //
        // 下面那条 `WHERE event_json NOT LIKE 'sv2:%'` 用不上任何索引，且 SELECT 出的是
        // 整行（含 `event_json`）—— 在 2.8 GB 真库上是一次 93 万行、数 GB 的扫描，**每次
        // 启动都跑一遍**，为的是得到「零行待迁移」这个上一次启动就已经知道的答案。而
        // `encryption_version = '2'` 早就在写了，只是从来没有人读它：实测每次 `open()`
        // 白白花掉十几秒，而 `open()` 在 QuotaBar 里是同步的 setup hook，界面就卡在这。
        //
        // 跳过是安全的，因为**读路径本来就认两种信封**（[`Self::decode_event_on`]：带
        // `sv2:` 前缀走每组密钥，否则走 store 主密钥）。所以即便有哪条路径在标记落地后
        // 又写进了 v1 行（比如用户降级跑了一次旧版），那些行仍然读得出来 —— 代价只是没被
        // 重新封装，不是数据不可用。
        let done: bool = conn
            .query_row(
                "SELECT v FROM store_meta WHERE k = 'encryption_version'",
                [],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .is_some_and(|v| v == "2");
        if done {
            return Ok(());
        }
        let legacy_rows = {
            let mut stmt = conn.prepare(
                "SELECT offset, source_type, source_location, source_path, source_session_id, seq, source_revision, projection_revision, aad_version, project_root, event_json FROM raw_events WHERE event_json NOT LIKE 'sv2:%'",
            )?;
            let rows = stmt.query_map([], EncryptedRow::from_sql)?;
            rows.collect::<Result<Vec<_>, _>>()?
        };

        if legacy_rows.is_empty() {
            conn.execute(
                "INSERT OR REPLACE INTO store_meta (k, v) VALUES ('encryption_version', '2')",
                [],
            )?;
            return Ok(());
        }

        log::info!(
            target: crate::logging::tag::SQLITE,
            "migrating {} total-store rows to per-group encrypted envelopes",
            legacy_rows.len()
        );
        let tx = Self::write_tx(&mut conn)?;
        {
            let mut update =
                tx.prepare("UPDATE raw_events SET event_json = ?1 WHERE offset = ?2")?;
            for row in &legacy_rows {
                let plaintext = if row.envelope.starts_with("sv1:") {
                    self.cipher.decrypt(&row.envelope, &row.aad())?
                } else {
                    row.envelope.as_bytes().to_vec()
                };
                let (key_id, data_cipher) =
                    self.data_cipher_for_group(&tx, &DataKeyGroup::from_row(row))?;
                let envelope = data_cipher.encrypt_data(&key_id, &plaintext, &row.aad())?;
                update.execute(params![envelope, row.offset])?;
            }
        }
        tx.execute(
            "INSERT OR REPLACE INTO store_meta (k, v) VALUES ('encryption_version', '2')",
            [],
        )?;
        tx.commit()?;
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); VACUUM;")?;
        log::info!(
            target: crate::logging::tag::SQLITE,
            "total-store per-group envelope migration completed"
        );
        Ok(())
    }

    fn data_cipher_for_group(
        &self,
        conn: &Connection,
        group: &DataKeyGroup,
    ) -> StoreResult<(String, StoreCipher)> {
        let existing: Option<(String, String)> = conn
            .query_row(
                r#"SELECT key_id, wrapped_key FROM data_keys
                    WHERE source_type = ?1 AND source_location = ?2
                      AND source_path = ?3 AND project_root = ?4"#,
                params![
                    group.source_type,
                    group.source_location,
                    group.source_path,
                    group.project_root,
                ],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((key_id, wrapped)) = existing {
            let aad = data_key_aad(&key_id, group);
            let key = self.cipher.unwrap_key(&wrapped, &aad)?;
            return Ok((key_id, StoreCipher::new(key)));
        }

        let key_id = new_data_key_id();
        let key = StoreKey::generate();
        let aad = data_key_aad(&key_id, group);
        let wrapped = self.cipher.wrap_key(&key, &aad)?;
        conn.execute(
            r#"INSERT INTO data_keys
                 (key_id, source_type, source_location, source_path, project_root,
                  wrapped_key, created_at)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)"#,
            params![
                key_id,
                group.source_type,
                group.source_location,
                group.source_path,
                group.project_root,
                wrapped,
                now_unix_secs(),
            ],
        )?;
        Ok((key_id, StoreCipher::new(key)))
    }

    fn data_cipher_by_id(&self, conn: &Connection, key_id: &str) -> StoreResult<StoreCipher> {
        let (wrapped, group): (String, DataKeyGroup) = conn.query_row(
            r#"SELECT wrapped_key, source_type, source_location, source_path, project_root
                 FROM data_keys WHERE key_id = ?1"#,
            params![key_id],
            |row| {
                Ok((
                    row.get(0)?,
                    DataKeyGroup {
                        source_type: row.get(1)?,
                        source_location: row.get(2)?,
                        source_path: row.get(3)?,
                        project_root: row.get(4)?,
                    },
                ))
            },
        )?;
        let key = self
            .cipher
            .unwrap_key(&wrapped, &data_key_aad(key_id, &group))?;
        Ok(StoreCipher::new(key))
    }

    /// 批量追加事件（一批 = 一个文件，所有事件共享 source_type/location/path）。
    ///
    /// 落在哪个 `(source_revision, projection_revision)` 由 [`Projection`] 决定，见其文档。
    ///
    /// `INSERT OR IGNORE` 仍保幂等：force 全量重扫时同投影内的旧事件全 skip、增量只落新尾。
    ///
    /// ⚠️ **作用域从第一条事件反推，因此空批只能是 no-op** —— 它表达不了「把当前投影替换
    /// 为空」。一个新解析器合法地对某文件产出零事件时正需要那个语义（ADR-044 决定 2）。
    /// 该缺口由显式携带 [`SourceKey`] 的 [`TotalStore::apply_projection`] 补上；本函数是
    /// 它的兼容层，保留给作用域推断无害的调用点（测试、固件）。
    pub fn append_events(
        &self,
        events: &[RawEvent],
        projection: Projection,
    ) -> StoreResult<AppendStats> {
        let Some(first) = events.first() else {
            return Ok(AppendStats::default());
        };
        let stats = self.apply_projection(FileProjectionBatch {
            source: SourceKey::from_event(first),
            parser_revision: None,
            mode: projection,
            // 这条兼容入口的调用方（CLI fixture / 老路径）不提供 token —— 保持
            // 旧的、不幂等的行为。**新调用点一律要传**，见 `FileProjectionBatch::token`。
            token: None,
            events: events.to_vec(),
        })?;
        Ok(AppendStats {
            appended: stats.appended,
            skipped_dup: stats.skipped_dup,
            skipped_erased: stats.skipped_erased,
            max_offset: stats.max_offset,
        })
    }

    /// 把一份投影应用到一个源文件上 —— 一个显式的文件级事务。
    ///
    /// 与 [`TotalStore::append_events`] 的三处差别，每一处都对应一个曾经出过事的形状：
    ///
    /// 1. **作用域显式**（`batch.source`），不从第一条事件反推。于是**空批有意义**：
    ///    `Rollback` / `Reparse` 带空批 = 「这个文件的当前投影就是空的」。区分「替换为空」
    ///    与「无事可做」的是 `mode`，不是批的长度。
    /// 2. **校验每条事件都属于 `batch.source`**。此前作用域是猜的，猜错不会报错，只会把
    ///    事件写进别的文件的投影里。
    /// 3. **切头与写事件同一事务**。失败则头与旧投影原样保留 —— 与 QuotaBar 那条
    ///    「`parser_revision` 只在投影真的落库后才推进」的不变式配套：重投影没落库时下一轮
    ///    仍须是 `Reparse`，否则退化成 `Append`，同 seq 被 dedup 丢弃，新解析永久丢失。
    ///
    /// `INSERT OR IGNORE` 仍保幂等：force 全量重扫时同投影内的旧事件全 skip、增量只落新尾。
    ///
    /// 🔴 **本函数不再记项目身份**（2026-08-14）。那件事现在由
    /// [`sweep_registered_root_identities`] 按注册表驱动 —— 挂在这里的盲区是
    /// 「近期没有活动的项目永远拿不到身份」，理由写在那个函数上。
    ///
    /// [`sweep_registered_root_identities`]: TotalStore::sweep_registered_root_identities

    /// 把一条「问过了」从 `identity_seen` 里撤回。
    ///
    /// 🔴 **「问过了」这个缓存只该记住终态。** `identity_seen` 是**先记后算**的
    /// （避免一个读不到的项目每轮扫描都重付一次探测代价），代价是任何在计算中途
    /// 退出的分支都必须把它撤回 —— 否则那不是「至多问一次」，是「永远不再问」。
    /// 现在有三个这样的分支（探不到 `.git`、拿不到锁、写失败），所以收成一个函数：
    /// 下一个人加第四个分支时，撤回这件事有名字可用。
    fn forget_identity_probe(&self, root: &str) {
        if let Ok(mut seen) = self.identity_seen.lock() {
            seen.remove(root);
        }
    }

    /// 给注册表里**每一个**项目根记一次身份 —— 一轮扫描调一次。
    ///
    /// 🔴 **为什么由注册表驱动，而不是由事件驱动**（2026-08-14 实测改的）。
    ///
    /// 从前这件事挂在 `apply_projection` 上：有事件落库才顺带记一次身份。那条路
    /// 有一个静默的盲区 —— **一个近期没有活动的项目永远拿不到身份**，哪怕注册表
    /// 认得它、`.git` 就在那儿、桥读得到它的 origin。实测：
    /// `wsl:Ubuntu-22.04:/home/<user>/workspace/QuotaBar` 是 `root_source=git` 的
    /// 注册根，而身份表里 **0 行** —— 因为那个项目最后一个会话文件停在一个月前。
    ///
    /// 后果不是「少一行」：TumeFlow 的 merge key 是「有身份用身份，没有退回路径」，
    /// 所以身份在与不在会给出**两个不同的 key**。于是同一份记忆会随身份表的有无
    /// 落进不同的桶，而没有任何东西会说出它们不一致。
    ///
    /// 本表自己的文档一直写着身份要「**在扫描时**记下来，趁 `.git` 还在」——
    /// 那是**扫描**时的关注点，不是**投影**时的。现在两者对齐。
    ///
    /// 🔴 **锁不跨探测。** 读根（持锁）→ 放锁 → 逐个算身份（每个 WSL 根要起
    /// `wsl.exe`，实测一次往返 ≈1.5 秒）→ 写回（持锁）。把探测放在锁里，一个卡住的
    /// 发行版会连带冻住所有读总库的路径。
    ///
    /// `deadline` 是**整轮**预算：耗尽后剩下的根这一轮不问，`identity_seen` 里也不
    /// 留记录 ⇒ 下一轮自然重试。
    /// 🔴 **`default_distro` / `mounts` 是这一轮的运行期事实，由调用方给。**
    ///
    /// 身份解析的形态分派（`pathnorm::reach_of`）要它们才认得出裸 Linux 路径与
    /// `/mnt/<drive>/…` —— 少了它们，这两族在 Windows 上会被当成本机相对路径，
    /// stat 不到 `.git/config` ⇒ 落 `path:` id ⇒ 被丢弃（实测 20 个根里 3 个）。
    ///
    /// **不在本函数里自己去问**：那会在每轮多起两次 `wsl.exe`，而调用方的发现阶段
    /// 刚刚问过。同一轮用同一份事实，也免得两处答案不一致。
    pub fn sweep_registered_root_identities(
        &self,
        default_distro: Option<&str>,
        mounts: &crate::pathnorm::DriveMounts,
        deadline: Deadline,
    ) -> IdentitySweep {
        let mut sweep = IdentitySweep::default();
        let roots: Vec<String> = {
            let Ok(conn) = self.conn.lock() else {
                sweep.unreadable = Some("total store mutex poisoned".to_string());
                return sweep;
            };
            let Ok(mut stmt) = conn.prepare("SELECT root_path FROM project_root_registry") else {
                sweep.unreadable = Some("prepare project_root_registry failed".to_string());
                return sweep;
            };
            // 先绑定再离开块 —— `conn` / `stmt` 是块内局部，让 `match` 直接当块的值
            // 会借用它们到块外。
            let collected = match stmt.query_map([], |r| r.get::<_, String>(0)) {
                Ok(rows) => rows.flatten().collect::<Vec<_>>(),
                Err(e) => {
                    sweep.unreadable = Some(format!("query project_root_registry failed: {e}"));
                    return sweep;
                }
            };
            collected
        };
        sweep.registered = roots.len();
        for root in roots {
            if deadline.expired() {
                // 说出来，不要静默截断 —— 「本轮只看了一半」和「本轮全看过了」在
                // 结果上长得一模一样。
                sweep.skipped_out_of_budget += 1;
                continue;
            }
            match self.record_identity_for_root(&root, default_distro, mounts, deadline) {
                IdentityOutcome::AlreadyProbed => sweep.already_probed += 1,
                IdentityOutcome::Recorded => sweep.recorded += 1,
                IdentityOutcome::NoRemote(_) => sweep.no_remote += 1,
                IdentityOutcome::CheckoutMissing => sweep.checkout_missing += 1,
                IdentityOutcome::Unresolved(_) => sweep.unresolved += 1,
            }
        }
        sweep
    }

    /// 一个根的身份 —— [`sweep_registered_root_identities`] 的单步。
    ///
    /// 🔴 **两件事，分开做**（task #56）：`probe_identity_for_root` 算出结论，
    /// 这里把结论**落库**。从前只有「问到了」留得下痕迹，另外两种算完就没了 ——
    /// 于是报告只能说 `null`，而 `null` 同时是「确认没有」「没问成」「还没扫到」。
    ///
    /// ⚠️ **判决没落库就不配保留「问过了」** —— 与身份行同一条纪律（见
    /// `probe_identity_for_root` 里那三个 `forget_identity_probe`）。理由也同一个：
    /// `identity_seen` 是**先记后算**的，任何中途退出都必须撤回，否则一次瞬时故障
    /// 让这个根在本进程内再也不被问起。
    ///
    /// [`sweep_registered_root_identities`]: TotalStore::sweep_registered_root_identities
    fn record_identity_for_root(
        &self,
        root: &str,
        default_distro: Option<&str>,
        mounts: &crate::pathnorm::DriveMounts,
        deadline: Deadline,
    ) -> IdentityOutcome {
        let outcome = self.probe_identity_for_root(root, default_distro, mounts, deadline);
        let verdict = match &outcome {
            // 缓存命中 ⇒ 本进程这一轮之前已经落过判决，不重复写。
            //
            // ⚠️ 写成 `if matches!(…) { return }` + 后面一个 `unreachable!()` 也能跑，
            // 但那是**断言**这一格不会发生；直接在这里 `return`，那一格就不存在。
            IdentityOutcome::AlreadyProbed => return outcome,
            IdentityOutcome::Recorded => IdentityVerdict::Resolved,
            IdentityOutcome::NoRemote(why) => IdentityVerdict::NoIdentity {
                why: (*why).to_string(),
            },
            IdentityOutcome::CheckoutMissing => IdentityVerdict::CheckoutMissing,
            IdentityOutcome::Unresolved(why) => IdentityVerdict::Unresolved { why: why.clone() },
        };
        if !self.note_identity_verdict(root, &verdict) {
            self.forget_identity_probe(root);
            return IdentityOutcome::Unresolved("identity verdict write failed".to_string());
        }
        outcome
    }

    /// 落一条判决 —— **返回是否落成**。
    ///
    /// 两态是对的：这里只有「写进去了」和「没写进去」，没有第三种。
    /// （对比 [`IdentityVerdict`] 本身 —— 那里两态就装不下。）
    #[must_use]
    fn note_identity_verdict(&self, root: &str, verdict: &IdentityVerdict) -> bool {
        let now = now_unix_millis();
        let Ok(conn) = self.conn.lock() else {
            return false;
        };
        match conn.execute(
            "INSERT INTO project_identity_probe
                 (project_root, outcome, detail, last_probe_ms)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(project_root)
             DO UPDATE SET outcome = ?2, detail = ?3, last_probe_ms = ?4",
            rusqlite::params![root, verdict.as_str(), verdict.why(), now],
        ) {
            Ok(_) => true,
            Err(e) => {
                log::debug!(
                    target: crate::logging::tag::SQLITE,
                    "identity verdict insert failed, will retry: root={root} err={e}"
                );
                false
            }
        }
    }

    /// 算一个根的身份，**不管报告** —— [`record_identity_for_root`] 的内核。
    ///
    /// [`record_identity_for_root`]: TotalStore::record_identity_for_root
    fn probe_identity_for_root(
        &self,
        root: &str,
        default_distro: Option<&str>,
        mounts: &crate::pathnorm::DriveMounts,
        deadline: Deadline,
    ) -> IdentityOutcome {
        if root.is_empty() {
            return IdentityOutcome::NoRemote("empty root path");
        }
        {
            let Ok(mut seen) = self.identity_seen.lock() else {
                return IdentityOutcome::Unresolved("identity_seen mutex poisoned".to_string());
            };
            // 🔴 **先记后算**：算不出来的（无 `.git`、UNC 回环读不到）也算「问过了」，
            // 否则一个读不到的项目每一轮扫描都要重付一次探测代价。
            if !seen.insert(root.to_string()) {
                return IdentityOutcome::AlreadyProbed;
            }
        }

        // 🔴 **身份解析按「根的形态」分派**（2026-08-14）—— `repo_id_for_root`：
        // 本机路径走 FS，`wsl:<distro>:/abs` 规范形走 `wsl.exe` 访问桥。
        //
        // 这里原本写着「不做任何路径改写：改写等于猜，而猜错会把身份安到别的项目上」。
        // 那句话对**猜**是对的，但它的后果是：**每一个 WSL 根的 `canonical_id` 恒为
        // `null`**，而同一个仓的 Windows checkout 有 `git:github.com/…` ⇒ 两份
        // checkout 永远不同身份。记忆库里因此 QuotaBar 被拆成 37 条 + 24 条，
        // 各持一半互相看不见；而按 `root_key` 迁移**修不了它**（干跑：79 条换 key、
        // 合并 0 组）—— 两份 checkout 的 root_key 本来就不同，能合并它们的只有身份。
        //
        // 三态各有各的处置：`Unknown` 是「本轮没问成」，它**已经**被上面的
        // `identity_seen` 记成「问过了」，所以必须撤回 —— 否则一次瞬时故障让这个
        // 项目在本进程内永远算不出身份。
        let identity =
            match crate::identity::repo_id_for_root(root, default_distro, mounts, deadline) {
                Ok(identity) => identity,
                Err(e) => {
                    log::debug!(
                        target: crate::logging::tag::SQLITE,
                        "project identity unresolved, will retry: {e}"
                    );
                    self.forget_identity_probe(root);
                    return IdentityOutcome::Unresolved(e.to_string());
                }
            };
        // 🔴 「目录没了」先于「没有 remote」判 —— 后者说的是「这个仓确认没有
        // remote」，而目录都不在了，那句话没有依据。
        if identity.checkout_missing {
            return IdentityOutcome::CheckoutMissing;
        }
        if !identity.id.starts_with("git:") {
            // 约束 1：不写 `path:` 兜底行（这是**确认**没有 remote 的情形）。
            // 「问过了」保留 —— 它是终态，下一轮不必再问。
            //
            // 🔴 **哪一种要说出来。** `RepoIdentity::repo_root` 分得开，而调用方
            // 从 `path:` 前缀**反推不出来**（它同时盖住两种）—— 那正是它的文档
            // 早就写下的理由，只是从前这里把整个 `RepoIdentity` 扔了。
            return IdentityOutcome::NoRemote(match identity.repo_root {
                Some(_) => "found a git root, but it has no usable origin remote",
                None => "no .git anywhere on this path",
            });
        }
        let cid = identity.id;

        let now = now_unix_millis();
        // 🔴 **第三个失败出口**（三轮评审 P2）。前两个（探不到 `.git`、读不了 config）
        // 已经会撤回缓存，而这里从前是 `let _ = conn.execute(...)` —— 拿不到锁、
        // 并发写、磁盘 IO 或任何 SQLite 错误都被丢弃，**而 `identity_seen` 已经先记过**。
        // 扫描随后照常成功，这个项目却在本进程内**再也不会尝试写身份**。
        //
        // 判据统一成一句：**只有身份行确实落库，才配保留「问过了」。**
        let Ok(conn) = self.conn.lock() else {
            self.forget_identity_probe(root);
            return IdentityOutcome::Unresolved("total store mutex poisoned".to_string());
        };
        let written = conn.execute(
            "INSERT INTO project_identity
                 (project_root, canonical_id, first_seen_ms, last_seen_ms)
             VALUES (?1, ?2, ?3, ?3)
             ON CONFLICT(project_root, canonical_id)
             DO UPDATE SET last_seen_ms = ?3",
            rusqlite::params![root, cid, now],
        );
        if let Err(e) = written {
            log::debug!(
                target: crate::logging::tag::SQLITE,
                "project identity insert failed, will retry: root={root} err={e}"
            );
            self.forget_identity_probe(root);
            return IdentityOutcome::Unresolved(format!("identity insert failed: {e}"));
        }
        IdentityOutcome::Recorded
    }

    /// 这个项目当前的规范身份 —— `last_seen` 最大的那条。
    ///
    /// 🔴 **即使 checkout 已经从磁盘上消失，这里依然答得出来**，只要它被扫描过一次。
    /// 那正是 `project_identity` 表存在的全部理由。
    ///
    /// ⚠️ **`None` 现在盖着三种情况**：没扫到过、扫到时没有 git remote（不写行，见
    /// `record_identity_for_root` 的约束 1）、以及**这次查询本身失败了**（下面
    /// `.ok().flatten()`）。前两种都是「说不出这个项目的身份」，是诚实的答案；
    /// 第三种不是 —— 它是本仓那条「没问成长得像这里是空的」在一个公开 API 上的
    /// 残留。**本轮不改**（改它要三态返回，会波及每个调用点），但写在这里，
    /// 免得下一个人以为 `None` 就等于「确实没有」。
    ///
    /// 🔴 **不带 `source_type` / `source_location`**：一个路径属于哪个仓库，由那里的
    /// `.git/config` 决定，与谁在什么位置扫到它无关（与 `project_root_registry`
    /// 同一条理由，见 `migrate()`）。带着它们的那一版让同一个根按发现者分裂成三行，
    /// 而且让「为注册表里每个根记身份」根本没法表达。
    pub fn project_identity(&self, project_root: &str) -> Option<String> {
        let conn = self.conn.lock().ok()?;
        conn.query_row(
            "SELECT canonical_id FROM project_identity
              WHERE project_root = ?1
              ORDER BY last_seen_ms DESC, canonical_id ASC
              LIMIT 1",
            rusqlite::params![project_root],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .ok()
        .flatten()
    }

    /// 全部已知的 `(project_root → 当前 canonical_id)`。
    ///
    /// 消费侧（QuotaBar 的别名表）要的就是这一张表：目录扫描只看得见现在还在磁盘上的
    /// checkout，而这里回答的是**扫描时记下来的**那一半。同一 `project_root` 有多个
    /// 身份时取 `last_seen_ms` 最新的（与 [`project_identity`] 同口径）。
    ///
    /// ⚠️ key 只用 `project_root`：调用方按路径查，而同一个路径在两个 source_type 下
    /// 指的是同一个目录。**真出现分歧时后写的赢** —— 那与单点查询的「取最新」一致。
    pub fn all_project_identities(&self) -> std::collections::BTreeMap<String, String> {
        let mut out = std::collections::BTreeMap::new();
        let Ok(conn) = self.conn.lock() else {
            return out;
        };
        let Ok(mut stmt) = conn.prepare(
            "SELECT project_root, canonical_id FROM project_identity
              ORDER BY last_seen_ms ASC",
        ) else {
            return out;
        };
        let Ok(rows) = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        else {
            return out;
        };
        // ASC + 覆盖插入 ⇒ 最后写进 map 的是 last_seen_ms 最大的那条。
        for (root, cid) in rows.flatten() {
            out.insert(root, cid);
        }
        out
    }

    // ── 项目根注册表（ADR-050）───────────────────────────────────────────
    //
    // 🔴 **写盘，不只在内存里。** QuotaBar 与 TumeFlow 各自摄取，而它们能发现的根
    // 不一样（前者有 WSL 访问桥）。一个只在本进程内存里生效的「已发现根」会让另一个
    // 进程看不见它 —— 那正是 ADR-050 根因二（同一知识分散多处、互相看不见）在**进程
    // 之间**的重演。共享靠这张表。

    /// 登记一个已知项目根。同一个根重复登记只更新 `last_seen_ms` 与来源。
    ///
    /// **失败静默**：注册表是**加法**能力 —— 它坏了该少发现几个根，不该让摄取停下。
    /// 与 `record_project_identity` 同一条。
    pub fn register_project_root(&self, path: &str, source: crate::attribution::RootSource) {
        let path = path.trim();
        if path.is_empty() {
            return;
        }
        // 🔴 **不做 `/mnt/…` 收敛**（传空表）：这个键只用于本表去重，
        // **归属不读它** —— `project_root_registry()` 是拿 `root_path` 重算键的，
        // 用的是那份带挂载表的注册表。所以即便有人登记了一条 `/mnt/…` 形式的根，
        // 它顶多在本表多占一行，归属结果照样与宿主形式那条收敛到一起。
        // 这不是靠推理保证的，见 `a_mnt_root_still_attributes_after_a_round_trip`。
        let key = crate::attribution::registry_key(path, &Vec::new());
        let now = now_unix_millis();
        let Ok(conn) = self.conn.lock() else { return };
        // 🔴 **插入与更新要分开**（ADR-051 I7）：`attribution_revision` 只在真的
        // **多了一个根**时递增。用 `ON CONFLICT DO UPDATE` 一条写完的话，
        // `changes()` 对两种情况都返回 1，分不出来 —— 而每轮刷新都会把已知的根
        // 重登记一遍，于是修订号会随刷新次数疯长，把全库 token 全部作废。
        let inserted = conn
            .execute(
                "INSERT INTO project_root_registry
                     (root_key, root_path, root_source, first_seen_ms, last_seen_ms)
                 VALUES (?1, ?2, ?3, ?4, ?4)
                 ON CONFLICT(root_key) DO NOTHING",
                rusqlite::params![key, path, source.as_str(), now],
            )
            .unwrap_or(0);
        if inserted > 0 {
            bump_attribution_revision(&conn);
        } else {
            let _ = conn.execute(
                "UPDATE project_root_registry
                    SET root_path = ?2, root_source = ?3, last_seen_ms = ?4
                  WHERE root_key = ?1",
                rusqlite::params![key, path, source.as_str(), now],
            );
        }
    }

    /// 注册表的修订号 —— [`crate::token::ProjectionToken`] 的一个分量。
    ///
    /// 🔴 **它不是「注册表被写过几次」，是「注册表长出过几个根」。** 前者会随每轮
    /// 刷新递增（注册是幂等的，已知的根每轮都重登记一遍），于是全库 token 每分钟
    /// 作废一次 —— 那等于没有幂等。
    ///
    /// 读不出来返回 0：那会让 token 退化成「不区分归属版本」，**偏保守**
    /// （两次不同归属的操作被当成同一次），所以调用方在 token 之外仍要靠
    /// `parser_revision` 与字节范围兜底。⚠️ 这是已知的降级，写在这里而不是假装没有。
    pub fn attribution_revision(&self) -> i64 {
        let Ok(conn) = self.conn.lock() else { return 0 };
        conn.query_row(
            "SELECT v FROM store_meta WHERE k = 'attribution_revision'",
            [],
            |r| r.get::<_, String>(0),
        )
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(0)
    }

    /// 库里出现过的所有 `project_root` 取值 —— **发现的候选清单**。
    ///
    /// 归属只认注册表，而注册表要有东西才认得出来；这些历史取值就是「人在哪些目录
    /// 里工作过」的全部记录，是发现的天然起点。
    ///
    /// ⚠️ 它们**不是**项目根 —— 恰恰相反，其中大多是子目录（同一个项目被记成 11 个
    /// `project_root` 正是本 ADR 的症状）。发现侧要对每一条上溯到真正的根。
    ///
    /// 🔴 **读不出来返回空，不报错**：发现是加法能力，它坏了该少发现几个根，
    /// 不该让摄取停下（与 [`Self::register_project_root`] 同一条）。
    pub fn distinct_project_roots(&self) -> Vec<String> {
        let Ok(conn) = self.conn.lock() else {
            return Vec::new();
        };
        let Ok(mut stmt) = conn.prepare(
            "SELECT DISTINCT project_root FROM raw_events              WHERE project_root IS NOT NULL AND project_root <> ''",
        ) else {
            return Vec::new();
        };
        let Ok(rows) = stmt.query_map([], |r| r.get::<_, String>(0)) else {
            return Vec::new();
        };
        rows.flatten().collect()
    }

    /// 记下一次「没探到根」的结果（`none` = 确认没有，`unreachable` = 没问成）。
    ///
    /// **失败静默**：与 [`Self::register_project_root`] 同一条 —— 这是加法能力，
    /// 它坏了该多探几次，不该让摄取停下。
    pub fn record_probe_miss(&self, path: &str, outcome: &str) {
        let now = now_unix_millis();
        let Ok(conn) = self.conn.lock() else { return };
        let _ = conn.execute(
            "INSERT INTO project_root_probe (path, outcome, last_probe_ms) VALUES (?1, ?2, ?3)              ON CONFLICT(path) DO UPDATE SET outcome = ?2, last_probe_ms = ?3",
            rusqlite::params![path, outcome, now],
        );
    }

    /// 读出探测记录：`path → (outcome, last_probe_ms)`。读不出来返回空 —— 那只意味着
    /// 这一轮把所有候选都重探一遍，是**慢**，不是错。
    pub fn probe_misses(&self) -> std::collections::HashMap<String, (String, i64)> {
        let mut out = std::collections::HashMap::new();
        let Ok(conn) = self.conn.lock() else {
            return out;
        };
        let Ok(mut stmt) =
            conn.prepare("SELECT path, outcome, last_probe_ms FROM project_root_probe")
        else {
            return out;
        };
        let Ok(rows) = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
            ))
        }) else {
            return out;
        };
        for (path, outcome, ms) in rows.flatten() {
            out.insert(path, (outcome, ms));
        }
        out
    }

    /// 读出整份注册表，供 [`attribution::attribute`] 使用。
    ///
    /// 🔴 **读不到就返回空注册表，不是报错。** 空注册表下每个路径都归到
    /// `Unattributed` —— 一致地说不出来，而不是退回「用 cwd 当根」那个老答案。
    ///
    /// `mounts` 是 WSL 里 Windows 盘的挂载表（[`crate::wsl::drive_mounts`]）——
    /// 给了就让 `/mnt/c/X` 与 `C:\X` 归到同一个根，空表就不收敛。**每条根的比较键
    /// 在这里由 `root_path` 重算**，所以本表里存的那个 `root_key` 不参与归属。
    pub fn project_root_registry(
        &self,
        mounts: &crate::pathnorm::DriveMounts,
    ) -> crate::attribution::RootRegistry {
        let mut reg = crate::attribution::RootRegistry::with_mounts(mounts.clone());
        let Ok(conn) = self.conn.lock() else {
            return reg;
        };
        let Ok(mut stmt) = conn.prepare("SELECT root_path, root_source FROM project_root_registry")
        else {
            return reg;
        };
        let Ok(rows) = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        else {
            return reg;
        };
        for row in rows.flatten() {
            // 来源解析不出来时用 `Scan` —— 一个不认识的来源标签不该让这个根整个消失。
            let src = crate::attribution::RootSource::parse(&row.1)
                .unwrap_or(crate::attribution::RootSource::Scan);
            reg.insert(&row.0, src);
        }
        reg
    }

    /// 这个来源在总库里**已经有当前投影了吗** —— [`crate::scan_plan::CommitPlan::plan`]
    /// 的 `has_prior`。
    ///
    /// 🔴 **不能用 [`Self::current_head`] 代替。** 它对「一条记录都没有」和「第一代
    /// `(0, 0)`」返回**同一个值**（`unwrap_or((0, 0))`），两者在返回类型上分不开。
    /// 而这个判断决定 `Rollback` / `Reparse` 有没有意义：没有前代时它们会开一个
    /// **没有前代可取代的空代**，而 `Rollback` 那一代按设计**永不自动回收**。
    ///
    /// 🔴 也**不能**拿调用方自己的游标/缓存里有没有这一条来代替。游标与投影是两套
    /// 状态，一个在调用方手里、一个在库里，它们可以各自存在 —— `svault scan-all`
    /// 第一次跑时手上一个游标都没有，而库里可能早已被常驻宿主写满了。
    /// 拿游标当代理，那一轮就会把「其实有前代」判成「没有」。
    pub fn has_projection(&self, source: &SourceKey) -> StoreResult<bool> {
        let (type_key, location_key, path_str) = source.parts();
        let conn = self
            .conn
            .lock()
            .map_err(|_| StoreError::Sqlite(rusqlite::Error::InvalidQuery))?;
        Ok(conn
            .query_row(
                r#"SELECT 1 FROM current_head
                    WHERE source_type = ?1 AND source_location = ?2 AND source_path = ?3"#,
                params![type_key, location_key, path_str],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    /// 某个源文件的当前头 `(source_revision, projection_revision)`。
    ///
    /// 对外暴露是为了让**跨层的崩溃恢复**可断言（ADR-051 §9 性质测试）：
    /// 「总库提交后、UI 索引提交前崩溃 ⇒ 恢复后两边收敛且**不留额外代**」——
    /// 后半句只有读得到头才验得了，而那正是最容易悄悄退化的一半
    /// （每崩一次多留一代，界面上完全看不出来）。
    ///
    /// 无记录 ⇒ `(0, 0)`，与内部的 `head_of` 同一口径。
    pub fn current_head(
        &self,
        source_type: &str,
        source_location: &str,
        source_path: &str,
    ) -> StoreResult<(i64, i64)> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| StoreError::Sqlite(rusqlite::Error::InvalidQuery))?;
        head_of(&conn, source_type, source_location, source_path)
    }

    /// 注册表全部条目 + 当前归属修订号，**读不到就报错**（ADR-050 对外报告面）。
    ///
    /// # 为什么不复用 [`Self::project_root_registry`]
    ///
    /// 那个服务**归属计算**：读不到 ⇒ 空注册表 ⇒ 每个路径一致地归到 `Unattributed`，
    /// 一个诚实的「说不出来」（论证见它自己的注释）。
    ///
    /// 这个服务**对外报告**，而消费方会据结果决定要不要自己去发现项目。同一个空列表
    /// 在那边读作「这台机器上没有项目」—— **一个说得出口但错误的答案**，正是本仓
    /// 「降级要降到说不出来，不是降到另一个答案」那条要防的。所以它返回 `Result`：
    /// 读到了、有 0 个根 ⇒ `Ok(空)`；**读不到 ⇒ `Err`**。两者在类型上就不同。
    ///
    /// # 为什么清单与修订号是一次调用
    ///
    /// `attribution_revision` 是消费方的**缓存失效锚**：它没变就可以继续用上次的
    /// 清单。分成两次调用，中间的锁间隙足够一次注册写入挤进来 —— 消费方于是拿到
    /// 「新清单 + 旧修订号」或反过来，而**两种都会让它认为缓存仍然有效**。
    /// 一次持锁读完，两者必然同代。
    ///
    /// ⚠️ 修订号这里**不能**沿用 [`Self::attribution_revision`] 的 `unwrap_or(0)`：
    /// 那个降级在归属计算里无害（0 只是个比较基准），在这里却会让消费方读作
    /// 「归属从没变过」⇒ 缓存**永不失效**。同一个值，两种语境，两种正确的错误处理。
    ///
    /// 🔴 **不分页**：根是 O(100) 的有界集合。加 `--limit` 只会引入一条静默截断的
    /// 路径（`sessions-read` 为此不得不带 `truncated` 标志），收益为零。
    pub fn project_roots_report(
        &self,
        mounts: &crate::pathnorm::DriveMounts,
    ) -> Result<(Vec<ProjectRootRow>, i64), String> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| format!("total store mutex poisoned: {e}"))?;
        // 身份先读进来 —— 同一次持锁，理由与修订号相同。
        //
        // 🔴 **按 `registry_key` 匹配，不按字面相等**：两张表的路径来自不同时刻的
        // 归属，写法可能不同（大小写、斜杠方向）。字面比会让一个明明记着身份的项目
        // 报成「说不出身份」，而那是**看得见的功能缺失**（两份 checkout 不再聚成
        // 一组），却不会有任何东西报错。用同一条归一化规则，写法差异就不参与。
        let mut identity_by_key: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        {
            let mut stmt = conn
                .prepare(
                    "SELECT project_root, canonical_id FROM project_identity \
                       ORDER BY last_seen_ms ASC",
                )
                .map_err(|e| format!("prepare project_identity failed: {e}"))?;
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
                .map_err(|e| format!("query project_identity failed: {e}"))?;
            for row in rows {
                let (root, cid) =
                    row.map_err(|e| format!("decode project_identity row failed: {e}"))?;
                // ASC + 覆盖插入 ⇒ 留下的是 `last_seen_ms` 最大的那条（与
                // `all_project_identities` 同一条规则）。
                identity_by_key.insert(crate::attribution::registry_key(&root, &Vec::new()), cid);
            }
        }
        // 判决同一次持锁读进来 —— 与身份、与修订号同一个理由：分两次读，中间的
        // 锁间隙足够一轮扫描挤进去，消费方于是拿到「新判决 + 旧身份」这种从未存在过
        // 的组合。
        //
        // 🔴 **按 `registry_key` 匹配，理由与上面身份那段逐字相同**：两张表的路径
        // 来自不同时刻，写法可能不同。字面比会让一个明明探测过的根报成「还没扫到」
        // —— 而那是**看得见的**功能缺失（三态又塌回一态），却不会有任何东西报错。
        let mut verdict_by_key: std::collections::HashMap<String, IdentityVerdict> =
            std::collections::HashMap::new();
        {
            let mut stmt = conn
                .prepare(
                    "SELECT project_root, outcome, detail FROM project_identity_probe \
                       ORDER BY last_probe_ms ASC",
                )
                .map_err(|e| format!("prepare project_identity_probe failed: {e}"))?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<String>>(2)?,
                    ))
                })
                .map_err(|e| format!("query project_identity_probe failed: {e}"))?;
            for row in rows {
                let (root, outcome, detail) =
                    row.map_err(|e| format!("decode project_identity_probe row failed: {e}"))?;
                verdict_by_key.insert(
                    crate::attribution::registry_key(&root, &Vec::new()),
                    IdentityVerdict::from_row(&outcome, detail),
                );
            }
        }
        let mut stmt = conn
            .prepare(
                "SELECT root_key, root_path, root_source, first_seen_ms, last_seen_ms \
                   FROM project_root_registry ORDER BY root_key",
            )
            .map_err(|e| format!("prepare project_root_registry failed: {e}"))?;
        let rows = stmt
            .query_map([], |r| {
                let root_path: String = r.get(1)?;
                let key: String = r.get(0)?;
                let by_path = crate::attribution::registry_key(&root_path, &Vec::new());
                Ok(ProjectRootRow {
                    canonical_id: identity_by_key
                        .get(&by_path)
                        .or_else(|| identity_by_key.get(&key))
                        .cloned(),
                    // 🔴 **表里没有这一行 ⇒ `NotProbed`，一个显式的变体。**
                    // 这不是「用默认值兜底」：缺席在这里就是「还没扫到」，而
                    // **查询失败走不到这里**（上面逐行 `?`，一行坏掉整份报告失败）。
                    identity_verdict: verdict_by_key
                        .get(&by_path)
                        .or_else(|| verdict_by_key.get(&key))
                        .cloned()
                        .unwrap_or(IdentityVerdict::NotProbed),
                    root_key: key,
                    aliases: alias_forms_of(&root_path),
                    root_path,
                    root_source: r.get(2)?,
                    first_seen_ms: r.get(3)?,
                    last_seen_ms: r.get(4)?,
                })
            })
            .map_err(|e| format!("query project_root_registry failed: {e}"))?;
        // `rows.flatten()` 会把逐行的解码失败悄悄跳过 —— 那正是「少几行」冒充
        // 「就这么多」。逐行 `?`，一行坏掉整份报告就失败。
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|e| format!("decode project_root_registry row failed: {e}"))?);
        }
        // 🔴 **收敛 `/mnt/<drive>/…` 与它的宿主形式 —— 这是本出口的责任。**
        //
        // `register_project_root` **故意**用空挂载表算存储键（那个键只用于本表去重），
        // 并在注释里把收敛的责任交了出去：「归属结果照样与宿主形式那条收敛到一起」。
        // 那句话对**归属**成立 —— 归属走 `project_root_registry()`，它重算键时带表。
        // 但**本出口不走归属**，它直接吐存储行，于是收敛从未在这里发生。
        //
        // 实机后果（2026-08-26）：同一个仓的 `/mnt/c/users/<user>/workspace/<repo>`
        // 与 `c:/users/<user>/workspace/<repo>` 作为两条根发出去，消费方看到两个项目。
        // 而 `snapshots` 那个出口（走 class_b，握着 `fs_prefix`）给的是**能打开的**
        // UNC 形 —— **同一个 svault 的两个出口对同一个项目给了不同答案**。
        //
        // ⚠️ 复用 `registry_key`，不在这里写第二套归一化：它是「同一个目录只能有
        // 一个键」这条规则的唯一定义点，而本仓的头号复发缺陷正是同一条规则两份实现。
        let out = Self::converge_mnt_roots(out, mounts);

        // 同一次持锁 —— 见上方「为什么清单与修订号是一次调用」。
        let revision: i64 = conn
            .query_row(
                "SELECT v FROM store_meta WHERE k = 'attribution_revision'",
                [],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .map_err(|e| format!("read attribution_revision failed: {e}"))?
            // 行不存在 = 还没注册过任何根，修订号就是初始值 0。这与「读失败」不同，
            // 上一行的 `?` 已经把后者分了出去。
            .map_or(Ok(0), |v| {
                v.parse::<i64>()
                    .map_err(|e| format!("attribution_revision is not an integer ({v:?}): {e}"))
            })?;
        Ok((out, revision))
    }

    /// 把 `/mnt/<drive>/…` 与它的宿主形式合成一行 —— **`roots` 出口的收敛**。
    ///
    /// # 为什么在这里，而不是在存储层
    ///
    /// 存储键**故意**不收敛（[`TotalStore::register_project_root`] 传空挂载表：那个键
    /// 只用于本表去重）。收敛需要挂载表，而挂载表是**运行期事实** —— WSL 没在跑时
    /// 取不到。让它参与持久键，同一条路径就会因为「当时 WSL 在不在」算出两个键，
    /// 而键写进去就不再变。**一个取决于瞬时状态的值没有资格当键。**
    ///
    /// 所以收敛属于**读出来的那一刻**：这一轮拿得到表就并，拿不到就照实分开，
    /// 下一轮拿到了自动并上 —— 没有需要迁移的历史包袱。
    ///
    /// # 合并规则（每条都要能说出判据）
    ///
    /// | 字段 | 取谁 | 判据 |
    /// | --- | --- | --- |
    /// | `root_path` | **宿主打得开**的那个；都打不开取字典序最小 | 消费方拿它去 `open()`；字典序保证全序，否则同组每次换代表、界面看起来在跳 |
    /// | `aliases` | 各成员写法的并集（不含 `root_path`） | 「同一个项目的其它写法」正是本字段的语义 |
    /// | `root_source` | `git` 优先 | 它是更强的证据；`marker`（有 CLAUDE.md）是回退 |
    /// | `canonical_id` | 非空的那个 | 🔴 **两个都非空且不同 ⇒ 不合并**，见下 |
    /// | `identity_verdict` | 跟着 `canonical_id` 走 | 判决与身份必须同源，否则「有身份但判决说没有」 |
    /// | `first_seen_ms` / `last_seen_ms` | min / max | 组的存活区间 |
    ///
    /// 🔴 **身份冲突时不合并。** 两条根算出同一个挂载键、却带着**不同**的
    /// `canonical_id`，说明有个前提错了（挂载表过期？两个仓恰好挂在同一处？）。
    /// 合并会把两个项目的记忆混在一起，而且**不报错** —— 那正是本仓
    /// 「合错了没有任何东西会报错」那条判例的形状。宁可多一行，不可错并。
    fn converge_mnt_roots(
        rows: Vec<ProjectRootRow>,
        mounts: &crate::pathnorm::DriveMounts,
    ) -> Vec<ProjectRootRow> {
        if mounts.is_empty() {
            // 表为空 ⇒ `registry_key` 一律走 `None` 分支 ⇒ 分组等于按原键分，
            // 白跑一趟。**照实分开**，并由调用方把「这一轮没能收敛」说出去。
            return rows;
        }
        let mut order: Vec<String> = Vec::new();
        let mut groups: std::collections::HashMap<String, Vec<ProjectRootRow>> =
            std::collections::HashMap::new();
        for row in rows {
            let key = crate::attribution::registry_key(&row.root_path, mounts);
            if !groups.contains_key(&key) {
                order.push(key.clone());
            }
            groups.entry(key).or_default().push(row);
        }
        let mut out = Vec::with_capacity(order.len());
        for key in order {
            let group = groups.remove(&key).unwrap_or_default();
            match Self::merge_root_group(group) {
                Ok(row) => out.push(row),
                Err(unmerged) => out.extend(unmerged),
            }
        }
        out
    }

    /// 一组同键的根 → 一行。身份冲突时原样返回整组（`Err`），**不猜**。
    fn merge_root_group(
        mut group: Vec<ProjectRootRow>,
    ) -> Result<ProjectRootRow, Vec<ProjectRootRow>> {
        if group.len() <= 1 {
            return match group.pop() {
                Some(row) => Ok(row),
                // 空组进不来（分组时每个键至少一行），但**不用 `unwrap`**：
                // 真进来了该是「这一组没有行」，不是 panic 掉整份报告。
                None => Err(Vec::new()),
            };
        }
        // 🔴 身份冲突 ⇒ 整组不动（见 `converge_mnt_roots` 的表）。
        let mut ids = group.iter().filter_map(|r| r.canonical_id.as_deref());
        if let Some(first) = ids.next() {
            if ids.any(|other| other != first) {
                log::warn!(
                    target: crate::logging::tag::SQLITE,
                    "root convergence skipped: same mount key, conflicting identities ({} rows)",
                    group.len()
                );
                return Err(group);
            }
        }
        // 代表：宿主打得开的优先；其次字典序最小（全序，不跳）。
        group.sort_by(|a, b| {
            let openable = |r: &ProjectRootRow| {
                crate::project_dir::host_openable_form(
                    &r.root_path,
                    &r.aliases,
                    crate::pathnorm::HostPlatform::current(),
                )
                .is_some()
            };
            openable(b)
                .cmp(&openable(a))
                .then_with(|| a.root_path.cmp(&b.root_path))
        });
        let mut head = group.remove(0);
        for r in group {
            for form in std::iter::once(r.root_path.clone()).chain(r.aliases) {
                if form != head.root_path && !head.aliases.contains(&form) {
                    head.aliases.push(form);
                }
            }
            if r.root_source == "git" {
                head.root_source = "git".to_string();
            }
            // 🔴 **不把别人的身份嫁接给代表行**（2026-08-26 外部 review 逮到）。
            //
            // 上一版是「代表行没有身份就取组里任意一个非空的」。看似无害 ——
            // 同一个目录的两种写法当然该有同一个身份。但 `NotProbed` 的含义是
            // **「这个写法还没被扫过」**，不是「它没有身份」：
            //
            //   旧 `/mnt/c/w/p` 带着项目 A 的身份（扫过，但那是过去）
            //   新 `C:\w\p` 是 NotProbed（刚出现，还没扫）
            //   ⇒ 代表取新的（宿主可开）⇒ 继承了 A ⇒ 若那个目录如今是项目 B，
            //      **B 的记忆全归到 A**
            //
            // ⚠️ 而**不合并时反倒没有这个问题**：消费方看到两行，B 的记忆归到
            // 那个无身份的行，各自成行。所以合并**让情况变糟了** —— 这正是
            // 「合错了没有任何东西会报错」那条判例的形状。
            //
            // ⇒ 代表行说不出身份就报说不出。下一轮 probe 会给它一个**当前**的
            // 身份，那才是可信的。本仓的一贯立场：**宁可说不出来，不可给错的答案**。
            //
            // ⚠️ 身份行本身不受影响：`project_roots_report` 是按 `registry_key`
            // 去 `project_identity` 表里查的，代表行**查得到它自己的**那条。
            head.first_seen_ms = head.first_seen_ms.min(r.first_seen_ms);
            head.last_seen_ms = head.last_seen_ms.max(r.last_seen_ms);
        }
        head.aliases.sort();
        Ok(head)
    }

    /// 注册表里有多少个根。诊断用（验收第 3 条：`Unattributed` 的数量要报得出来）。
    pub fn project_root_count(&self) -> usize {
        let Ok(conn) = self.conn.lock() else { return 0 };
        conn.query_row("SELECT COUNT(*) FROM project_root_registry", [], |r| {
            r.get::<_, i64>(0)
        })
        .map(|n| n as usize)
        .unwrap_or(0)
    }

    /// 一个项目**观察到过的全部**身份，新到旧。多于一条即意味着这个路径的身份变过 ——
    /// 可能是改了 remote，也可能是路径被另一个仓复用，**本层不替调用方判断**
    /// （见 `migrate()` 里那段注释）。
    pub fn project_identity_history(&self, project_root: &str) -> Vec<(String, i64, i64)> {
        let Ok(conn) = self.conn.lock() else {
            return Vec::new();
        };
        let Ok(mut stmt) = conn.prepare(
            "SELECT canonical_id, first_seen_ms, last_seen_ms FROM project_identity
              WHERE project_root = ?1
              ORDER BY last_seen_ms DESC",
        ) else {
            return Vec::new();
        };
        let rows = stmt.query_map(rusqlite::params![project_root], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        });
        match rows {
            Ok(it) => it.flatten().collect(),
            Err(_) => Vec::new(),
        }
    }

    pub fn apply_projection(&self, batch: FileProjectionBatch) -> StoreResult<ProjectionStats> {
        let now = now_unix_secs();
        let (type_key, location_key, path_str) = batch.source.parts();
        let source_type = type_key.to_string();
        let source_location = location_key;
        let source_path = path_str.to_string();

        // 🔴 先校验再动库。事件若不属于声明的作用域，写下去不会报错，只会污染另一个文件
        // 的投影 —— 而那正是「作用域靠猜」时无法察觉的失败。
        if let Some(bad) = batch.events.iter().find(|ev| {
            source_type_key(ev.source_type) != type_key
                || ev.source_location.as_key() != source_location
                || ev.source_path != source_path
        }) {
            return Err(StoreError::ForeignEvent {
                declared: source_path.clone(),
                found: bad.source_path.clone(),
            });
        }

        let events = &batch.events;
        let projection = batch.mode;
        let mut conn = self.conn.lock().unwrap();

        // ── 幂等短路（ADR-051 I7）────────────────────────────────────────
        //
        // 同一个 token 已经应用过 ⇒ 返回**那一次**的头，一个字都不写。
        //
        // 🔴 返回的是记录里的头，**不是当前头**：中间可能已经有别的操作推进过，
        // 返回当前头会让调用方以为自己这次操作生效了。
        if let Some(token) = batch.token.as_ref() {
            let prior: Option<(i64, i64)> = conn
                .query_row(
                    "SELECT source_revision, projection_revision FROM applied_projections \
                       WHERE token = ?1",
                    params![token.as_str()],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            if let Some((sr, pr)) = prior {
                log::info!(
                    target: crate::logging::tag::SQLITE,
                    "projection already applied, returning prior head: path={source_path} rev={sr}/{pr}"
                );
                let max_offset: i64 =
                    conn.query_row("SELECT COALESCE(MAX(offset), 0) FROM raw_events", [], |r| {
                        r.get(0)
                    })?;
                return Ok(ProjectionStats {
                    appended: 0,
                    skipped_dup: 0,
                    skipped_erased: 0,
                    max_offset,
                    source_revision: sr,
                    projection_revision: pr,
                    // 头没动 —— 这次什么都没做。谎报 true 会让 QuotaBar 推进
                    // `parser_revision`，而那正是「重投影没落库却以为落了」。
                    head_moved: false,
                    superseded_removed: 0,
                    // 没做任何取代，谈不上「事件变少」。
                    loses_events: None,
                });
            }
        }

        let head = head_of(&conn, &source_type, &source_location, &source_path)?;
        let (source_revision, projection_revision) = projection.target_revisions(head);
        let tx = Self::write_tx(&mut conn)?;
        // token 记账与事件写入**同一事务** —— 分开写就会造出「token 记了但事件没写」
        // （下轮短路返回一个从未存在的头）或反之（崩溃重放又开一代）的窗口。
        if let Some(token) = batch.token.as_ref() {
            tx.execute(
                r#"INSERT INTO applied_projections
                     (token, source_type, source_location, source_path,
                      source_revision, projection_revision, applied_at)
                   VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)"#,
                params![
                    token.as_str(),
                    source_type,
                    source_location,
                    source_path,
                    source_revision,
                    projection_revision,
                    now,
                ],
            )?;
        }
        // 台账：这份投影为什么存在。`INSERT OR IGNORE` 因为增量 append 会反复落在同一
        // 投影上 —— 第一次记账即可。
        tx.execute(
            r#"INSERT OR IGNORE INTO projections
                 (source_type, source_location, source_path, source_revision,
                  projection_revision, parser_revision, origin, created_at)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)"#,
            params![
                source_type,
                source_location,
                source_path,
                source_revision,
                projection_revision,
                batch.parser_revision,
                projection.origin_key(),
                now,
            ],
        )?;
        let mut appended = 0u64;
        let mut skipped_dup = 0u64;
        let mut skipped_erased = 0u64;
        {
            let mut tombstoned = tx.prepare(
                r#"SELECT EXISTS(
                       SELECT 1 FROM tombstones t
                        WHERE (t.scope = 'session'      AND t.key = ?1)
                           OR (t.scope = 'source_path'  AND t.key = ?2)
                           OR (t.scope = 'project_root' AND t.key = ?3)
                   )"#,
            )?;
            let mut stmt = tx.prepare(
                r#"INSERT OR IGNORE INTO raw_events
                     (ingested_at, schema_version, source_type, source_location,
                      source_path, source_session_id, seq, source_revision, projection_revision,
                      aad_version, event_type, occurred_at, occurred_at_unix_ms,
                      project_root, event_json)
                   VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)"#,
            )?;
            for ev in events {
                let erased: bool = tombstoned.query_row(
                    params![ev.source_session_id, ev.source_path, ev.project_root],
                    |row| row.get(0),
                )?;
                if erased {
                    skipped_erased += 1;
                    continue;
                }
                let json = serde_json::to_string(ev)?;
                let aad = event_aad(
                    AAD_VERSION_CURRENT,
                    source_type_key(ev.source_type),
                    &ev.source_location.as_key(),
                    &ev.source_path,
                    &ev.source_session_id,
                    ev.seq as i64,
                    source_revision,
                    projection_revision,
                );
                let group = DataKeyGroup::from_event(ev);
                let (key_id, data_cipher) = self.data_cipher_for_group(&tx, &group)?;
                let envelope = data_cipher.encrypt_data(&key_id, json.as_bytes(), &aad)?;
                let changed = stmt.execute(params![
                    now,
                    ev.schema_version,
                    source_type_key(ev.source_type),
                    ev.source_location.as_key(),
                    ev.source_path,
                    ev.source_session_id,
                    ev.seq as i64,
                    source_revision,
                    projection_revision,
                    AAD_VERSION_CURRENT,
                    event_type_key(ev.event_type),
                    ev.occurred_at,
                    ev.occurred_at
                        .as_deref()
                        .and_then(crate::rawevent::occurred_at_unix_ms),
                    ev.project_root,
                    envelope,
                ])?;
                if changed == 1 {
                    appended += 1;
                } else {
                    skipped_dup += 1;
                }
            }
        }
        // 🔴 `Reparse` 取代被它超越的那份投影（ADR-044 决定 2）。
        //
        // 为什么只有 Reparse 能删：
        //
        // - `Rollback` 的旧版本是磁盘上**已消失内容的唯一副本** —— 删了真没了；
        // - `Reparse` 的旧投影是**同一批字节的一份更差的解析**，可再生。`Reparse`
        //   按定义就是重读源字节，源没了根本不会发生 —— 所以替换那一刻字节必然可读。
        //
        // 🔴 **丢事件时：头照切，旧投影不删。**
        //
        // ADR-044 决定 2 原本写作「拒绝替换」，而实现时发现那句话内部矛盾：一个
        // **合法产出零事件**的新解析器（G2）与一次**解析器退化**（G3）在「事件变少」
        // 这个观测上完全一样。按字面拒绝，G2 就永远做不成 —— 库会一直服务旧解析，
        // 正是本 ADR 要治的病；而 `parser_revision` 不推进，还会无限重试。
        //
        // 拆开之后两个目标都能满足：
        //
        // - **当前答案**永远是最新那份解析（正确、诚实）；
        // - **不可逆的那一步**（删旧行）在可疑时不做，于是退化可恢复。
        //
        // 护栏保护的是删除，不是切换。代价是可疑情形下磁盘多留一份 —— 只在可疑情形，
        // 不是常态。
        //
        // 与插入同一事务：失败时头与旧投影一并原样保留。
        let mut superseded_removed = 0u64;
        let mut loses_events: Option<(u64, u64)> = None;
        if projection == Projection::Reparse && (source_revision, projection_revision) != head {
            let (prev_source, prev_projection) = head;
            let prev_count: i64 = tx.query_row(
                "SELECT COUNT(*) FROM raw_events
                  WHERE source_type = ?1 AND source_location = ?2 AND source_path = ?3
                    AND source_revision = ?4 AND projection_revision = ?5",
                params![
                    source_type,
                    source_location,
                    source_path,
                    prev_source,
                    prev_projection
                ],
                |r| r.get(0),
            )?;
            let new_count = appended as i64;
            if new_count < prev_count {
                loses_events = Some((prev_count as u64, new_count as u64));
                log::warn!(
                    "[total-store] reparse of {source_path} yields fewer events                      ({prev_count} → {new_count}); keeping the superseded projection                      so the regression stays recoverable"
                );
            }
            superseded_removed = if loses_events.is_some() {
                0
            } else {
                tx.execute(
                    "DELETE FROM raw_events
                  WHERE source_type = ?1 AND source_location = ?2 AND source_path = ?3
                    AND source_revision = ?4 AND projection_revision = ?5",
                    params![
                        source_type,
                        source_location,
                        source_path,
                        prev_source,
                        prev_projection
                    ],
                )? as u64
            };
            if loses_events.is_none() {
                tx.execute(
                    "DELETE FROM projections
                  WHERE source_type = ?1 AND source_location = ?2 AND source_path = ?3
                    AND source_revision = ?4 AND projection_revision = ?5",
                    params![
                        source_type,
                        source_location,
                        source_path,
                        prev_source,
                        prev_projection
                    ],
                )?;
            }
        }

        // 头真的动了才记 change-feed —— 增量 append 每轮都会走到这里，记下来只是噪声。
        // 与切头同一事务：消费者绝不会看到「头动了但没有对应记录」，反之亦然。
        if (source_revision, projection_revision) != head {
            tx.execute(
                r#"INSERT INTO projection_log
                     (at, source_type, source_location, source_path,
                      old_source_revision, old_projection_revision,
                      new_source_revision, new_projection_revision, reason)
                   VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)"#,
                params![
                    now,
                    source_type,
                    source_location,
                    source_path,
                    head.0,
                    head.1,
                    source_revision,
                    projection_revision,
                    projection.origin_key(),
                ],
            )?;
        }

        // 切头。与事件插入同一事务：插入失败则头原样保留，读侧永远看不到「头指向一份
        // 没写成的投影」这种中间态。
        tx.execute(
            r#"INSERT INTO current_head
                 (source_type, source_location, source_path,
                  source_revision, projection_revision, updated_at)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6)
               ON CONFLICT(source_type, source_location, source_path)
               DO UPDATE SET source_revision = excluded.source_revision,
                             projection_revision = excluded.projection_revision,
                             updated_at = excluded.updated_at"#,
            params![
                source_type,
                source_location,
                source_path,
                source_revision,
                projection_revision,
                now,
            ],
        )?;
        tx.commit()?;
        let max_offset = max_offset_on(&conn)?;
        Ok(ProjectionStats {
            appended,
            skipped_dup,
            skipped_erased,
            max_offset,
            source_revision,
            projection_revision,
            head_moved: (source_revision, projection_revision) != head,
            superseded_removed,
            loses_events,
        })
    }

    /// 按会话身份读它的**当前投影全部事件**（seq 升序）。
    ///
    /// 🔴 与「拉前 N 条 offset」的差别不是效率，是**正确性**。消费者先用
    /// [`recent_sessions`] 选出要处理的会话，然后必须能拿到**那些会话的完整事件**；
    /// 用 offset 前缀去凑，选中的会话若排在前缀之后就整个不在结果里，而且这不报错
    /// —— 只是那个会话静默消失。
    ///
    /// 与 [`read_session`] 的差别：这个按 `(source, session)` 批量取，一次读事务内
    /// 完成，避免「列会话」与「逐个读」跨越一次并发 `Reparse` 而拿到不一致的快照。
    pub fn read_sessions(
        &self,
        sessions: &[(String, String, String, String)],
        max_events: usize,
    ) -> StoreResult<Vec<(i64, RawEvent)>> {
        let conn = self.conn.lock().unwrap();
        let mut out = Vec::new();
        let mut key_cache = HashMap::new();
        for (st, loc, path, sid) in sessions {
            if out.len() >= max_events {
                break;
            }
            let mut stmt = conn.prepare(&format!(
                r#"SELECT {ENCRYPTED_ROW_COLUMNS}
                     FROM raw_events r
                     LEFT JOIN current_head h
                            ON h.source_type = r.source_type
                           AND h.source_location = r.source_location
                           AND h.source_path = r.source_path
                    WHERE r.source_type = ?1 AND r.source_location = ?2
                      AND r.source_path = ?3 AND r.source_session_id = ?4
                      AND r.source_revision = COALESCE(h.source_revision, 0)
                      AND r.projection_revision = COALESCE(h.projection_revision, 0)
                      AND NOT EXISTS (
                          SELECT 1 FROM tombstones t
                           WHERE (t.scope = 'session'      AND t.key = r.source_session_id)
                              OR (t.scope = 'source_path'  AND t.key = r.source_path)
                              OR (t.scope = 'project_root' AND t.key = r.project_root)
                      )
                    ORDER BY r.seq ASC, r.offset ASC
                    LIMIT ?5"#
            ))?;
            let remaining = (max_events - out.len()) as i64;
            let rows = stmt.query_map(
                params![st, loc, path, sid, remaining],
                EncryptedRow::from_sql,
            )?;
            for row in rows {
                let row = row?;
                match self.decode_event_on(&conn, &mut key_cache, &row) {
                    Ok(ev) => out.push((row.offset, ev)),
                    // 单行解不开只 skip+warn，不让整个会话失败 —— 与 `read_since_page`
                    // 同一套韧性策略。
                    Err(e) => log::warn!(
                        target: crate::logging::tag::SQLITE,
                        "raw_events offset={} skipped in read_sessions: {e}", row.offset
                    ),
                }
            }
        }
        Ok(out)
    }

    /// 读 `after_seq` 之后的投影替换记录 —— **change-feed**（ADR-044 决定 6）。
    ///
    /// 消费者拿到一条就按 `source_key` **原子替换**自己那份物化：先删掉该 source 的
    /// 全部旧内容，再从当前投影重建。这一步不能用逐事件 upsert 代替 —— upsert 永远
    /// 删不掉「新投影里不再出现」的那些事件。
    ///
    /// `seq` 是这条流自己的游标，与 `raw_events.offset` 无关：后者会被重投影重铸，
    /// 而这条流记的正是「重投影发生了」。
    pub fn read_projection_changes(
        &self,
        after_seq: i64,
        limit: usize,
    ) -> StoreResult<Vec<ProjectionChange>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            r#"SELECT seq, at, source_type, source_location, source_path,
                      old_source_revision, old_projection_revision,
                      new_source_revision, new_projection_revision, reason
                 FROM projection_log
                WHERE seq > ?1
                ORDER BY seq ASC
                LIMIT ?2"#,
        )?;
        let rows = stmt.query_map(params![after_seq, limit as i64], |r| {
            Ok(ProjectionChange {
                seq: r.get(0)?,
                at: r.get(1)?,
                source_type: r.get(2)?,
                source_location: r.get(3)?,
                source_path: r.get(4)?,
                old_source_revision: r.get(5)?,
                old_projection_revision: r.get(6)?,
                new_source_revision: r.get(7)?,
                new_projection_revision: r.get(8)?,
                reason: r.get(9)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    /// 回收**已被取代且来源明确**的投影（ADR-044 决定 7）。
    ///
    /// 🔴 只碰满足两个条件的行：
    ///
    /// 1. 它不是该源文件的当前投影（`current_head` 说了算）；
    /// 2. 它在台账里的 `origin` 是 **`reparse`** —— 也就是「同一批字节的一份更差的
    ///    解析」，可再生。
    ///
    /// **禁止**碰的两类，各有各的理由：
    ///
    /// - `origin = 'rollback'`：那是磁盘上**已消失内容的唯一副本**，删了真没了；
    /// - `origin = 'unknown'`：本 ADR 落地**之前**产生的行。当时只有一个 `generation`
    ///   整数，无从判断某一代当初是回退还是重解析。实测作者本机 930,056 行里有
    ///   383,426 行（41.2%，约 1.12 GB）属于这一类。它们**不进通用 GC**，只能由显式的、
    ///   可审计的用户操作清理 —— 这是「先动手后规范化」的直接代价，明写而不是悄悄吸收。
    ///
    /// `dry_run` 时只统计不删，供 CLI 先给人看一眼。
    pub fn gc_superseded_projections(&self, dry_run: bool) -> StoreResult<GcStats> {
        let mut conn = self.conn.lock().unwrap();
        let tx = Self::write_tx(&mut conn)?;
        // 候选：台账里 origin='reparse'、且不是当前头指向的那一份。
        const CANDIDATES: &str = r#"
            SELECT p.source_type, p.source_location, p.source_path,
                   p.source_revision, p.projection_revision
              FROM projections p
              LEFT JOIN current_head h
                     ON h.source_type = p.source_type
                    AND h.source_location = p.source_location
                    AND h.source_path = p.source_path
             WHERE p.origin = 'reparse'
               AND (p.source_revision, p.projection_revision)
                   IS NOT (COALESCE(h.source_revision, 0), COALESCE(h.projection_revision, 0))
        "#;
        let candidates: Vec<(String, String, String, i64, i64)> = {
            let mut stmt = tx.prepare(CANDIDATES)?;
            let rows = stmt.query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        let mut stats = GcStats {
            projections: candidates.len() as u64,
            events: 0,
            dry_run,
        };
        for (st, loc, path, srev, prev) in &candidates {
            let n: i64 = tx.query_row(
                "SELECT COUNT(*) FROM raw_events
                  WHERE source_type = ?1 AND source_location = ?2 AND source_path = ?3
                    AND source_revision = ?4 AND projection_revision = ?5",
                params![st, loc, path, srev, prev],
                |r| r.get(0),
            )?;
            stats.events += n as u64;
            if !dry_run {
                tx.execute(
                    "DELETE FROM raw_events
                      WHERE source_type = ?1 AND source_location = ?2 AND source_path = ?3
                        AND source_revision = ?4 AND projection_revision = ?5",
                    params![st, loc, path, srev, prev],
                )?;
                tx.execute(
                    "DELETE FROM projections
                      WHERE source_type = ?1 AND source_location = ?2 AND source_path = ?3
                        AND source_revision = ?4 AND projection_revision = ?5",
                    params![st, loc, path, srev, prev],
                )?;
            }
        }
        tx.commit()?;
        Ok(stats)
    }

    /// 最近活跃的 N 个会话，按各自最后一条事件的**真实时间**降序。
    ///
    /// 🔴 存在的理由：消费者此前用 `[max_offset - N, max_offset]` 当「最近窗口」，而
    /// `offset` 是**写入顺序** —— 全量重扫时等于文件遍历顺序，与时间无关。实测一次
    /// 重投影之后，那个「最近 5 万条」的窗口横跨九个多月，而当天的会话被挤了出去。
    ///
    /// `INGEST_KERNEL.md` §13.1 早就写着「`offset` 仅作同步游标，**不代表**时间先后」。
    /// 规则写对了、写下了、然后被违反了，而没有任何东西会报错 —— 这个 API 的作用是让
    /// 正确的做法比错误的做法更好用。
    ///
    /// 排序键是 `(last_occurred_at_unix_ms, source_path, session_id)`：**稳定键**，
    /// 同时间戳不会在翻页时抖动。无时间的会话排在最后并标 `has_time = false`，
    /// 而不是被静默丢弃 —— 「没有时间」是一个需要被看见的事实。
    ///
    /// 只看**当前投影**（复用 `current_head`）：已被取代的旧解析不该参与「最近」。
    pub fn recent_sessions(
        &self,
        limit: usize,
        since_unix_ms: Option<i64>,
    ) -> StoreResult<Vec<RecentSession>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            r#"SELECT r.source_type, r.source_location, r.source_path, r.source_session_id,
                      MAX(r.occurred_at_unix_ms) AS last_ms,
                      MIN(r.occurred_at_unix_ms) AS first_ms,
                      COUNT(*) AS events
                 FROM raw_events r
                 LEFT JOIN current_head h
                        ON h.source_type = r.source_type
                       AND h.source_location = r.source_location
                       AND h.source_path = r.source_path
                WHERE r.source_revision = COALESCE(h.source_revision, 0)
                  AND r.projection_revision = COALESCE(h.projection_revision, 0)
                  AND (?1 IS NULL OR r.occurred_at_unix_ms >= ?1)
                  AND NOT EXISTS (
                      SELECT 1 FROM tombstones t
                       WHERE (t.scope = 'session'      AND t.key = r.source_session_id)
                          OR (t.scope = 'source_path'  AND t.key = r.source_path)
                          OR (t.scope = 'project_root' AND t.key = r.project_root)
                  )
                GROUP BY r.source_type, r.source_location, r.source_path, r.source_session_id
                ORDER BY last_ms IS NULL, last_ms DESC, r.source_path DESC,
                         r.source_session_id DESC
                LIMIT ?2"#,
        )?;
        let rows = stmt.query_map(params![since_unix_ms, limit as i64], |row| {
            Ok(RecentSession {
                source_type: row.get(0)?,
                source_location: row.get(1)?,
                source_path: row.get(2)?,
                session_id: row.get(3)?,
                last_occurred_at_unix_ms: row.get(4)?,
                first_occurred_at_unix_ms: row.get(5)?,
                event_count: row.get::<_, i64>(6)? as u64,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    /// 读 `offset` 之后的事件（升序、最多 `limit` 条），跳过被**按作用域**墓碑标记的来源。
    /// 这是最小读 API——验证总库可读。**返回成功反序列化的事件**；需要「SQL 扫描进度」
    /// （翻页越过坏行窗口）的 pull 流走 [`read_since_page`]。保持此签名向后兼容，QuotaBar
    /// 既有调用方零改动。
    pub fn read_since(&self, after_offset: i64, limit: usize) -> StoreResult<Vec<(i64, RawEvent)>> {
        Ok(self.read_since_page(after_offset, limit)?.events)
    }

    /// [`read_since`] 的富返回版：除事件外还报告 `max_scanned_offset`（SQL 实际扫描到的最大
    /// offset，含坏行）。P3-③ TumeFlow `pull --since` 的种子——`pull_stream` 据此推进游标
    /// 越过**整窗坏行**，只在 SQL 零行时判追平（评审 [P1]；详见 [`ReadPage`]）。
    ///
    /// **韧性**：单行 `event_json` 反序列化失败（损坏 / 未来不兼容 `schema_version`）只 **skip+warn**，
    /// 不让整批失败。跨版本升级 DTO（把旧 `schema_version` 行 up-convert 到当前）是首次破坏性 schema
    /// 升级前的前置工作（届时按 `schema_version` 分派解析），当前 v1 单版本不需要。
    /// [`read_since_page`] 的**当前投影版**（ADR-044 决定 6 / D1）。
    ///
    /// 🔴 与旧接口的唯一差别：只发**当前投影**的行。`read_session` 一直是这么做的，
    /// 而 `read_since_page` 不是 —— 两个读 API 对「什么算数」给出不同答案，而 pull
    /// 走的是不过滤的那个。一次重投影之后，pull 流里同一批会话各有两份。
    ///
    /// 🔴 **旧接口保持原样**，不是懒。给一个已发布 CLI 静默换掉返回内容，按旧语义
    /// 写的消费者不会报错，只会悄悄拿到不同的数据 —— 那是最难查的一类回归。新语义
    /// 走新入口，由调用方显式选择（`pull --projection current`）。
    pub fn read_current_since_page(
        &self,
        after_offset: i64,
        limit: usize,
    ) -> StoreResult<ReadPage> {
        self.read_page_impl(after_offset, limit, true)
    }

    pub fn read_since_page(&self, after_offset: i64, limit: usize) -> StoreResult<ReadPage> {
        self.read_page_impl(after_offset, limit, false)
    }

    fn read_page_impl(
        &self,
        after_offset: i64,
        limit: usize,
        current_only: bool,
    ) -> StoreResult<ReadPage> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            r#"SELECT r.offset, r.source_type, r.source_location, r.source_path,
                      r.source_session_id, r.seq, r.source_revision, r.projection_revision, r.aad_version,
                      r.project_root, r.event_json
                 FROM raw_events r
                 LEFT JOIN current_head h
                        ON h.source_type = r.source_type
                       AND h.source_location = r.source_location
                       AND h.source_path = r.source_path
                WHERE r.offset > ?1
                  AND (NOT ?3
                       OR (r.source_revision = COALESCE(h.source_revision, 0)
                           AND r.projection_revision = COALESCE(h.projection_revision, 0)))
                  AND NOT EXISTS (
                      SELECT 1 FROM tombstones t
                       WHERE (t.scope = 'session'      AND t.key = r.source_session_id)
                          OR (t.scope = 'source_path'  AND t.key = r.source_path)
                          OR (t.scope = 'project_root' AND t.key = r.project_root)
                  )
                ORDER BY r.offset ASC
                LIMIT ?2"#,
        )?;
        let rows = stmt.query_map(
            params![after_offset, limit as i64, current_only],
            EncryptedRow::from_sql,
        )?;
        let mut out = Vec::new();
        let mut key_cache = HashMap::new();
        // 记录 SQL 实际扫描到的最大 offset（含坏行）——行按 offset ASC，故最后一行即最大。
        // 即便整窗都反序列化失败，max_scanned 仍非 None，让 pull 流推进游标越过坏行窗口。
        let mut max_scanned: Option<i64> = None;
        for row in rows {
            let row = row?;
            max_scanned = Some(row.offset);
            match self.decode_event_on(&conn, &mut key_cache, &row) {
                Ok(ev) => out.push((row.offset, ev)),
                Err(e) => log::warn!(
                    target: crate::logging::tag::SQLITE,
                    "raw_events offset={} skipped (authentication/decode failed): {e}", row.offset
                ),
            }
        }
        Ok(ReadPage {
            events: out,
            max_scanned_offset: max_scanned,
        })
    }

    /// 每个 snapshot source 的最新版本。按 `(type, location, path)` 分组取最大
    /// offset，并遵守 erase 墓碑。TumeFlow 用它读取当前 Class-B 状态，不必从总库头扫。
    pub fn read_latest_snapshots(&self) -> StoreResult<Vec<(i64, RawEvent)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            r#"SELECT r.offset, r.source_type, r.source_location, r.source_path,
                      r.source_session_id, r.seq, r.source_revision, r.projection_revision, r.aad_version,
                      r.project_root, r.event_json
                 FROM raw_events r
                WHERE r.event_type = 'config_snapshot'
                  AND r.offset = (
                      SELECT MAX(r2.offset) FROM raw_events r2
                       WHERE r2.event_type = 'config_snapshot'
                         AND r2.source_type = r.source_type
                         AND r2.source_location = r.source_location
                         AND r2.source_path = r.source_path
                  )
                  AND NOT EXISTS (
                      SELECT 1 FROM tombstones t
                       WHERE (t.scope = 'source_path' AND t.key = r.source_path)
                          OR (t.scope = 'project_root' AND t.key = r.project_root)
                  )
                ORDER BY r.source_type, r.source_location, r.source_path"#,
        )?;
        let rows = stmt.query_map([], EncryptedRow::from_sql)?;
        let mut out = Vec::new();
        let mut cache = HashMap::new();
        for row in rows {
            let row = row?;
            match self.decode_event_on(&conn, &mut cache, &row) {
                Ok(event) => out.push((row.offset, event)),
                Err(e) => log::warn!(
                    target: crate::logging::tag::SQLITE,
                    "latest snapshot offset={} skipped (decode failed): {e}", row.offset
                ),
            }
        }
        Ok(out)
    }

    /// `read_latest_snapshots` 的当前可见视图：已明确删除的源文件不返回；探测**失败**
    /// 时保守保留，避免一次问不成被误判成删除。
    ///
    /// 🔴 **这条规矩从前只有 WSL 那一支守着。** 本机支是
    /// `Path::new(&event.source_path).is_file()` —— 权限拒绝、句柄耗尽、外接盘没挂上、
    /// 网络盘断开全都折叠成 `false` ⇒ 那条快照**被读作「用户删了这个文件」**，
    /// 于是该项目的 `CLAUDE.md` / `AGENTS.md` 规则静默退出视图（`svault snapshots`
    /// → TumeFlow Class-B）。而**同一个函数里**、隔四行的 WSL 支写着
    /// 「keeping last versions」，连日志文案都把判据说对了。
    ///
    /// 两支的差别不是谁更细心，是本机那支的判定**内联在调用点**、没有类型逼它表态。
    /// 现在两支都经 [`crate::probe`]：三态里只有 `Absent` 才是删除。
    pub fn read_active_latest_snapshots(&self) -> StoreResult<Vec<(i64, RawEvent)>> {
        self.read_active_latest_snapshots_with(&crate::probe::LocalBackend::unanchored())
    }

    /// [`Self::read_active_latest_snapshots`] 的可测形态 —— **本机 backend 注入**。
    ///
    /// 🔴 拆出来是因为「探测失败 ⇒ 保留」这条**在本机造不出来**：要让一次
    /// `std::fs::metadata` 返回 `NotFound` 以外的错误，得靠权限、句柄耗尽或
    /// 断开的网络盘，三者都不可在单测里确定性构造。判定作参数，逻辑才可单测 ——
    /// 与 `probe_local_with` 的探测器注入、`probe_local_with_home` 的 home 注入同一个惯例。
    ///
    /// ⚠️ 注入的是 backend **而不是** `bool`：测试驱动的必须是生产那段 `match`
    /// 本身，否则钉住的只是我自己造的映射（AGENTS.md：「纯函数的测试钉的是映射，
    /// 永远说不了输入的来路」）。
    pub(crate) fn read_active_latest_snapshots_with(
        &self,
        local: &dyn ProbeBackend,
    ) -> StoreResult<Vec<(i64, RawEvent)>> {
        let rows = self.read_latest_snapshots()?;
        let mut by_distro: HashMap<String, Vec<String>> = HashMap::new();
        for (_, event) in &rows {
            if let SourceLocation::Wsl(distro) = &event.source_location {
                by_distro
                    .entry(distro.clone())
                    .or_default()
                    .push(event.source_path.clone());
            }
        }
        let mut existing: HashMap<String, Option<HashSet<String>>> = HashMap::new();
        for (distro, paths) in by_distro {
            match crate::wsl::existing_files(&distro, &paths) {
                Ok(found) => {
                    existing.insert(distro, Some(found));
                }
                Err(error) => {
                    log::warn!(
                        target: crate::logging::tag::SNAPSHOT,
                        "snapshot batch existence probe failed; keeping last versions: distro={} error={}",
                        distro,
                        error
                    );
                    existing.insert(distro, None);
                }
            }
        }
        Ok(rows
            .into_iter()
            .filter(|(_, event)| match &event.source_location {
                SourceLocation::Local => {
                    match local.probe(Path::new(&event.source_path), Deadline::unbounded()) {
                        // 存在但不是普通文件（被换成目录/符号链）—— 也是**事实**，
                        // 那个快照的源确实不在了。
                        crate::probe::Probed::Found(crate::probe::FileKind::File) => true,
                        crate::probe::Probed::Found(_) | crate::probe::Probed::Absent => false,
                        // 🔴 没问成 ⇒ **保留**，与 WSL 支同一判据。
                        crate::probe::Probed::Unknown(e) => {
                            log::warn!(
                                target: crate::logging::tag::SNAPSHOT,
                                "local snapshot existence probe failed; keeping last version: {e}"
                            );
                            true
                        }
                    }
                }
                SourceLocation::Wsl(distro) => existing
                    .get(distro)
                    .and_then(Option::as_ref)
                    .is_none_or(|paths| paths.contains(&event.source_path)),
            })
            .collect())
    }

    /// 在**已读好的**「每来源最新快照」集合里定位 `source`，据此算增量游标。
    ///
    /// 🔴 **`latest` 由调用方读一次、循环内复用。** 这里曾是 `self.read_latest_snapshots()?`，
    /// 而 [`Self::sync_snapshots`] 每个 source 调它一次 —— 那条查询是 `MAX(offset)` 相关
    /// 子查询 + tombstone `NOT EXISTS`，`raw_events` 上没有可用索引，于是每次都是全表扫。
    /// 实测 2026-08-09（3.08 GB / 968,029 行 / 176 个 source）：单次 **5.6 s** × 176
    /// ⇒ **16 分 06 秒**，其间 QuotaBar 的「蒸馏」满核空转、界面只显示「蒸馏中…」。
    /// 一次读之后是 176 条的线性匹配，量级从 O(source × 全表) 落到 O(全表 + source²)。
    ///
    /// 纯函数（不借 `&self`），所以下面的单测能直接钉住那几条分支。
    ///
    /// 🔴 **`mtime_probe` 是注入的，不是在这里直接 stat。** 加 `modified_at` 的
    /// 补齐分支时我第一版就地调了 `crate::scan::snapshot_mtime` —— 那一下把上面
    /// 那句「纯函数」变成了假话，而这个函数的可测性正建立在它上面。
    /// 与 [`Self::read_active_latest_snapshots_with`] 注入 backend 同一个惯例：
    /// **注入的是探测器而不是一个 `bool`**，否则测试钉住的是我自己造的映射。
    ///
    /// ⚠️ 它**只在缺 mtime 时被调用**（`is_none()` 短路在前）。稳态下一次都不调；
    /// WSL 侧一次 stat 要拉一个 shell，无条件探会让整轮同步慢一个量级。
    fn snapshot_cursor(
        latest: &[(i64, RawEvent)],
        source: &SourceRef,
        mtime_probe: &dyn Fn(&SourceRef) -> Option<i64>,
    ) -> Cursor {
        let found = latest.iter().map(|(_, event)| event).find(|event| {
            event.source_type == source.source_type
                && event.source_location == source.source_location
                && event.source_path == source.path.to_string_lossy()
        });
        let mut cursor = Cursor::new_fingerprint();
        if let Some(event) = found {
            // 内容未变但宿主补齐/修正了项目身份或 artifact kind 时也要发新版本；
            // 否则早期无身份快照会永久挡住后续规范化元数据。
            //
            // 🔴 **`modified_at` 归同一类**（2026-08-29）。存量快照是加这个字段
            // 之前写的，而快照只在 `content_hash` 变了才重发 ⇒ 它们会**永久**停在
            // `modified_at: None`，正是上面那句话说的「永久挡住后续规范化元数据」。
            //
            // ⚠️ **自限**：条件里带上「现在**取得到**」。只写 `is_none()` 的话，
            // 一台 WSL 桥不通的机器会**每一轮都重发一版**（新版本同样没有 mtime
            // ⇒ 下一轮再判一次「缺」）—— 快照版本没有上限，那是一个无声的
            // 无限增长。取不到就不动它，等桥通了自然会补上。
            //
            // ⚠️ 这个探测**只在缺 mtime 时发生**：补齐之后 `is_some()` 短路，
            // 稳态下一次 stat 都不多做。WSL 侧一次 stat 要拉一个 shell，
            // 无条件探会让 `sync-snapshots` 慢一个量级。
            let meta_stale = event.project_root != source.project_root
                || event.artifact_kind != source.artifact_kind
                || (event.modified_at.is_none() && mtime_probe(source).is_some());
            if !meta_stale {
                cursor.content_hash = event.content_hash.clone();
            }
            cursor.next_seq = event.seq.saturating_add(1);
        }
        cursor
    }

    /// 用 SessionVault 的 snapshot scanner 同步一组已授权来源到总库。宿主只负责
    /// 触发与提供项目身份；发现、读取、指纹、增量和持久化都在本仓完成。
    pub fn sync_snapshots(&self, sources: &[SourceRef]) -> StoreResult<SnapshotSyncStats> {
        let mut stats = SnapshotSyncStats::default();
        // 🔴 一次读、循环内复用 —— 理由与实测数字见 `snapshot_cursor` 的注释。
        let mut latest = self.read_latest_snapshots()?;
        for source in sources {
            if source.source_mode != SourceMode::SnapshotFile {
                continue;
            }
            stats.sources += 1;
            let cursor = Self::snapshot_cursor(&latest, source, &crate::scan::snapshot_mtime);
            // 空注册表在这条路上是**惰性**的，不是降级：快照（Class-B）的
            // `project_root` 由宿主直接填在 `SourceRef` 上，不从 cwd 归属而来 ——
            // `scan_snapshot_file` 压根不碰 `roots`。给非空的反而会让人以为它参与了判定。
            let result = crate::scan::scan_source(
                source,
                Some(cursor),
                Profile::Full,
                std::sync::Arc::new(crate::attribution::RootRegistry::new()),
                // 快照同步是一次性维护动作，不属于宿主任何一轮刷新的预算。
                crate::deadline::Deadline::unbounded(),
            );
            if result.status == ScanStatus::Error {
                stats.failed += 1;
                log::warn!(
                    target: crate::logging::tag::SNAPSHOT,
                    "snapshot sync failed: path={} warnings={:?}",
                    source.path.display(), result.report.warnings
                );
                continue;
            }
            if result.events.is_empty() {
                stats.unchanged += 1;
                continue;
            }
            let appended = self.append_events(&result.events, Projection::Append)?;
            stats.changed += 1;
            stats.appended += appended.appended;
            // 同一个 source 在 `sources` 里出现两次时，第二次必须看到刚写进去的那一版 ——
            // 否则它会拿旧游标重扫一遍。append 本身幂等（`skipped_dup` 走唯一约束），所以
            // 漏掉这一步不会写重，只会白扫一次并让 `changed` 虚高。缓存既然提到了循环外，
            // 就得由循环负责让它保持新鲜。
            if let Some(newest) = result.events.last() {
                let hit = latest.iter().position(|(_, event)| {
                    event.source_type == source.source_type
                        && event.source_location == source.source_location
                        && event.source_path == source.path.to_string_lossy()
                });
                let fresh = (appended.max_offset, newest.clone());
                match hit {
                    Some(i) => latest[i] = fresh,
                    None => latest.push(fresh),
                }
            }
        }
        log::info!(
            target: crate::logging::tag::SNAPSHOT,
            "snapshot sync done: sources={} changed={} unchanged={} failed={} appended={}",
            stats.sources, stats.changed, stats.unchanged, stats.failed, stats.appended
        );
        Ok(stats)
    }

    /// 读单个 (file, session) 的全部事件（按 `seq` 升序 = 文件内事件顺序）。作用域**四列精确**：
    /// 一张会话卡 = 一个 `(source_type, source_location, source_path, session_id)` 对——session_id
    /// 可跨文件 replay（Claude `--resume`），故必须连 `source_path` 一起 scope，不能只按 session_id
    /// 串话。供 QuotaBar transcript 从总库重建（不再重读 JSONL）。墓碑此处**不过滤**：transcript 是
    /// 宿主对自己已索引会话的展示，erase 语义作用于下游 pull（`read_since`），不该让某条墓碑令一张
    /// 仍在列表里的卡片打不开。
    ///
    /// 反序列化失败的行 skip 但**计入 [`SessionRead::skipped`]**（不静默吞）——调用方据此判断
    /// 事件流是否完整、是否回落 live（见 `SessionRead` 文档）。
    pub fn read_session(
        &self,
        source_type: SourceType,
        source_location: &SourceLocation,
        source_path: &str,
        session_id: &str,
    ) -> StoreResult<SessionRead> {
        let conn = self.conn.lock().unwrap();
        // 🔴 只取该文件的**当前投影**，且以 `current_head` 为准 —— 不是
        // `MAX(source_revision, projection_revision)`。
        //
        // 差别不是性能，是可表达性：`MAX` 只能指向「存在事件的最大编号」，因此当一份投影
        // 是**空的**时（新解析器合法地对该文件产出零事件），`MAX` 会退回到上一份非空投影，
        // 把已被取代的旧解析当成当前答案端出去。头是显式的，空投影就是空。
        //
        // `LEFT JOIN` + `IS NULL` 兜住「还没有头」的文件（迁移前的库、或刚建的库）：
        // 那时按 `(0, 0)` 读，与旧行为一致。
        let mut stmt = conn.prepare(
            r#"SELECT r.offset, r.source_type, r.source_location, r.source_path,
                      r.source_session_id, r.seq, r.source_revision, r.projection_revision, r.aad_version,
                      r.project_root, r.event_json
                 FROM raw_events r
                 LEFT JOIN current_head h
                        ON h.source_type = r.source_type
                       AND h.source_location = r.source_location
                       AND h.source_path = r.source_path
                WHERE r.source_type = ?1
                  AND r.source_location = ?2
                  AND r.source_path = ?3
                  AND r.source_session_id = ?4
                  AND r.source_revision = COALESCE(h.source_revision, 0)
                  AND r.projection_revision = COALESCE(h.projection_revision, 0)
                ORDER BY r.seq ASC, r.offset ASC"#,
        )?;
        let rows = stmt.query_map(
            params![
                source_type_key(source_type),
                source_location.as_key(),
                source_path,
                session_id,
            ],
            EncryptedRow::from_sql,
        )?;
        let mut events = Vec::new();
        let mut skipped = 0usize;
        let mut key_cache = HashMap::new();
        for row in rows {
            let row = row?;
            match self.decode_event_on(&conn, &mut key_cache, &row) {
                Ok(ev) => events.push(ev),
                Err(e) => {
                    skipped += 1;
                    log::warn!(
                        target: crate::logging::tag::SQLITE,
                        "raw_events offset={} skipped (authentication/decode failed): {e}", row.offset
                    );
                }
            }
        }
        Ok(SessionRead { events, skipped })
    }

    fn decode_event_on(
        &self,
        conn: &Connection,
        key_cache: &mut HashMap<String, StoreCipher>,
        row: &EncryptedRow,
    ) -> StoreResult<RawEvent> {
        let plaintext = if row.envelope.starts_with("sv2:") {
            let key_id = data_key_id(&row.envelope)?.to_string();
            if !key_cache.contains_key(&key_id) {
                key_cache.insert(key_id.clone(), self.data_cipher_by_id(conn, &key_id)?);
            }
            key_cache
                .get(&key_id)
                .expect("data key inserted")
                .decrypt_data(&row.envelope, &key_id, &row.aad())?
        } else {
            self.cipher.decrypt(&row.envelope, &row.aad())?
        };
        Ok(serde_json::from_slice(&plaintext)?)
    }

    /// 总库状态（条数 / 最大 offset / 最近入库时间）。
    pub fn status(&self) -> StoreResult<StoreStatus> {
        let conn = self.conn.lock().unwrap();
        let (count, max_offset, last_ingested_at) = conn.query_row(
            "SELECT COUNT(*), COALESCE(MAX(offset), 0), MAX(ingested_at) FROM raw_events",
            [],
            |r| {
                Ok((
                    r.get::<_, i64>(0)? as u64,
                    r.get::<_, i64>(1)?,
                    r.get::<_, Option<i64>>(2)?,
                ))
            },
        )?;
        let active_data_keys: u64 =
            conn.query_row("SELECT COUNT(*) FROM data_keys", [], |row| {
                Ok(row.get::<_, i64>(0)? as u64)
            })?;
        Ok(StoreStatus {
            count,
            max_offset,
            last_ingested_at,
            encrypted: true,
            encryption_version: 2,
            active_data_keys,
            key_scheme: "per-source-project",
        })
    }

    /// 在同一事务写墓碑并物理删除命中正文。墓碑不含正文，确保后续增量和全量重建都不会复活。
    pub fn tombstone(&self, scope: TombstoneScope, key: &str) -> StoreResult<EraseStats> {
        if key.trim().is_empty() {
            return Err(StoreError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "erase key must not be empty",
            )));
        }
        let mut conn = self.conn.lock().unwrap();
        let tx = Self::write_tx(&mut conn)?;
        tx.execute(
            "INSERT OR REPLACE INTO tombstones (scope, key, tombstoned_at) VALUES (?1, ?2, ?3)",
            params![scope.as_str(), key, now_unix_secs()],
        )?;
        let deleted = tx.execute(
            &format!("DELETE FROM raw_events WHERE {} = ?1", scope.column()),
            params![key],
        )?;
        let keys_destroyed = tx.execute(
            r#"DELETE FROM data_keys
                WHERE NOT EXISTS (
                    SELECT 1 FROM raw_events r
                     WHERE r.source_type = data_keys.source_type
                       AND r.source_location = data_keys.source_location
                       AND r.source_path = data_keys.source_path
                       AND COALESCE(r.project_root, '') = data_keys.project_root
                )"#,
            [],
        )?;
        tx.commit()?;
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
        log::info!(
            target: crate::logging::tag::SQLITE,
            "total-store erase committed: scope={} deleted_events={deleted} keys_destroyed={keys_destroyed}",
            scope.as_str(),
        );
        Ok(EraseStats {
            deleted_events: deleted as u64,
            keys_destroyed: keys_destroyed as u64,
            tombstone_written: true,
        })
    }

    /// 回填标志（写者侧 catch-up 用，见 QuotaBar `refresh_index`）：宿主据此判断总库是否已与
    /// 索引一致。新建库默认 `false` → 宿主触发一次 force 全量回填；任一 append 失败时宿主 `set` 回
    /// `false`，下轮再 force 重发（dedup 幂等补回丢失批）。
    pub fn is_backfilled(&self) -> StoreResult<bool> {
        let conn = self.conn.lock().unwrap();
        let v: Option<String> = conn
            .query_row("SELECT v FROM store_meta WHERE k = 'backfilled'", [], |r| {
                r.get(0)
            })
            .optional()?;
        Ok(v.as_deref() == Some("1"))
    }

    /// 设置回填标志（`true` = 已与索引一致；`false` = 需下轮 force 回填/补偿）。
    pub fn set_backfilled(&self, done: bool) -> StoreResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO store_meta (k, v) VALUES ('backfilled', ?1)",
            params![if done { "1" } else { "0" }],
        )?;
        Ok(())
    }
}

/// 墓碑作用域（`read_since` 按此精确匹配）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TombstoneScope {
    Session,
    SourcePath,
    ProjectRoot,
}

impl TombstoneScope {
    fn as_str(self) -> &'static str {
        match self {
            TombstoneScope::Session => "session",
            TombstoneScope::SourcePath => "source_path",
            TombstoneScope::ProjectRoot => "project_root",
        }
    }

    fn column(self) -> &'static str {
        match self {
            TombstoneScope::Session => "source_session_id",
            TombstoneScope::SourcePath => "source_path",
            TombstoneScope::ProjectRoot => "project_root",
        }
    }
}

fn store_has_encrypted_rows(path: &Path) -> StoreResult<bool> {
    // 🔴 本函数返回 `Result`，所以「没问成」有地方可去 —— 从前的 `!path.exists()`
    // 把它折成 `Ok(false)`＝「这个库没有加密行」，而调用方据此决定要不要按明文处理。
    match crate::probe::LocalBackend::unanchored().probe(path, Deadline::unbounded()) {
        crate::probe::Probed::Found(_) => {}
        crate::probe::Probed::Absent => return Ok(false),
        crate::probe::Probed::Unknown(e) => {
            return Err(StoreError::Io(std::io::Error::other(format!(
                "cannot tell whether the store exists: {e}"
            ))))
        }
    }
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let raw_exists: bool = conn
        .prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name='raw_events'")?
        .query_row([], |_| Ok(true))
        .optional()?
        .unwrap_or(false);
    if !raw_exists {
        return Ok(false);
    }
    let sample: Option<String> = conn
        .query_row(
            "SELECT event_json FROM raw_events WHERE event_json LIKE 'sv1:%' OR event_json LIKE 'sv2:%' LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    Ok(sample.as_deref().is_some_and(is_envelope))
}

/// unix 下把路径权限收窄到 `mode`（目录 0700 / 文件 0600）；非 unix no-op（Windows 依赖
/// `%LOCALAPPDATA%` 的按用户 ACL）。best-effort——设权限失败不致命（warn）。
#[cfg(unix)]
fn restrict_permissions(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    if let Err(e) = crate::probe::set_permissions(path, std::fs::Permissions::from_mode(mode)) {
        log::warn!(
            target: crate::logging::tag::SQLITE,
            "set permissions {mode:o} on {} failed: {e}",
            path.display()
        );
    }
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path, _mode: u32) {}

fn max_offset_on(conn: &Connection) -> StoreResult<i64> {
    Ok(
        conn.query_row("SELECT COALESCE(MAX(offset), 0) FROM raw_events", [], |r| {
            r.get(0)
        })?,
    )
}

/// 新写入的行用的 AAD 版本。见 [`event_aad`]。
const AAD_VERSION_CURRENT: i64 = 2;

/// 事件密文的认证上下文（AES-GCM 的 AAD），把密文钉在它的逻辑位置上，防止行被调换
/// （ADR-027）。
///
/// 🔴 **`version` 必须来自行自己的 `aad_version` 列，不能推断。**
///
/// - **v1**（ADR-044 之前写入，以及那次迁移保留的全部既有行）：末位是 `generation`。
///   迁移把 `source_revision` 取为 `generation` 的原值，所以对这些行传
///   `source_revision` 得到的字节与当初**逐字节相同** —— 1.66 GB 密文一行都不用重加密。
/// - **v2**（之后写入）：`source_revision` 与 `projection_revision` 都进 AAD。
///
/// 为什么不能只用 v1 了事：v1 下同一事件的两份**不同解析**共享同一个 AAD，于是把旧投影
/// 的密文换进新投影的行是**不可检测**的 —— 而「旧解析冒充当前」正是 ADR-044 在治的病。
/// 加一列自描述的版本，比日后反复推理「哪些行是哪种 AAD」便宜得多。
// 参数多是刻意的：AAD 的每一段都必须是**调用点显式传入的值**。包成结构体会让
// `RawEvent` 与 `EncryptedRow` 两条路径各自去构造它，而它们对同一行必须产出完全
// 相同的字节 —— 少一层转换就少一处能悄悄分叉的地方。
#[allow(clippy::too_many_arguments)]
fn event_aad(
    version: i64,
    source_type: &str,
    source_location: &str,
    source_path: &str,
    source_session_id: &str,
    seq: i64,
    source_revision: i64,
    projection_revision: i64,
) -> Vec<u8> {
    let tag: &[u8] = if version >= AAD_VERSION_CURRENT {
        b"session-vault:event:v2"
    } else {
        b"session-vault:event:v1"
    };
    let mut aad = tag.to_vec();
    let seq_bytes = seq.to_be_bytes();
    let source_revision_bytes = source_revision.to_be_bytes();
    let projection_revision_bytes = projection_revision.to_be_bytes();
    let mut parts: Vec<&[u8]> = vec![
        source_type.as_bytes(),
        source_location.as_bytes(),
        source_path.as_bytes(),
        source_session_id.as_bytes(),
        &seq_bytes,
        &source_revision_bytes,
    ];
    if version >= AAD_VERSION_CURRENT {
        parts.push(&projection_revision_bytes);
    }
    for part in parts {
        aad.extend_from_slice(&(part.len() as u64).to_be_bytes());
        aad.extend_from_slice(part);
    }
    aad
}

fn data_key_aad(key_id: &str, group: &DataKeyGroup) -> Vec<u8> {
    let mut aad = b"session-vault:data-key:v1".to_vec();
    for part in [
        key_id.as_bytes(),
        group.source_type.as_bytes(),
        group.source_location.as_bytes(),
        group.source_path.as_bytes(),
        group.project_root.as_bytes(),
    ] {
        aad.extend_from_slice(&(part.len() as u64).to_be_bytes());
        aad.extend_from_slice(part);
    }
    aad
}

fn now_unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 毫秒时间戳 —— **给需要排序的时间用**。
///
/// 🔴 `project_identity.last_seen_ms` 用它而不是秒，因为那一列是**排序键**：秒级精度下
/// 同一秒内观察到的两个身份会平局，于是「取最新」实际退化成「取字母序靠前的」。
/// 这不是假想 —— 一条测试当场撞上了它（同一路径先后两个 remote，查询返回了旧的那个）。
/// 排序键不该依赖时钟精度。
fn now_unix_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// `SourceType` → 稳定 snake_case 键（与 serde 序列化一致；用于索引列，避免存 Debug 形）。
fn source_type_key(t: SourceType) -> &'static str {
    match t {
        SourceType::ClaudeCode => "claude_code",
        SourceType::Codex => "codex",
        SourceType::Cursor => "cursor",
        SourceType::Gemini => "gemini",
        SourceType::Jsonl => "jsonl",
    }
}

/// `EventType` → 稳定 snake_case 键。
fn event_type_key(t: EventType) -> &'static str {
    match t {
        EventType::Message => "message",
        EventType::ToolUse => "tool_use",
        EventType::ToolResult => "tool_result",
        EventType::Usage => "usage",
        EventType::Meta => "meta",
        EventType::ConfigSnapshot => "config_snapshot",
        EventType::Thinking => "thinking",
    }
}

#[cfg(test)]
// 测试要造 fixture（建目录、写文件、再核一遍），允许直接碰盘 —— 存在性边界管的是
// **生产行为**，而 `#[cfg(test)]` 不在生产路径上。允许写在模块上而不是逐个函数：
// 下一条测试不必再想一遍这件事，而生产代码里加一行照样会被 clippy 拦。
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use crate::rawevent::{Actor, SourceLocation, TimeConfidence, TokenUsage, SCHEMA_VERSION};

    // ── 根收敛（`converge_mnt_roots` / `merge_root_group`）─────────────────
    //
    // 🔴 **这一族此前零测试** —— 合并逻辑是 2026-08-25 加的，而「合错了没有任何
    // 东西会报错」正是它最危险的地方：报告照常出，只是两个项目变成了一个。

    fn root_row(path: &str, id: Option<&str>, verdict: IdentityVerdict) -> ProjectRootRow {
        ProjectRootRow {
            root_key: crate::attribution::registry_key(path, &Vec::new()),
            root_path: path.to_string(),
            root_source: "scan".to_string(),
            first_seen_ms: 1_000,
            last_seen_ms: 2_000,
            aliases: Vec::new(),
            canonical_id: id.map(str::to_string),
            identity_verdict: verdict,
        }
    }

    fn c_drive() -> crate::pathnorm::DriveMounts {
        vec![("/mnt/c".to_string(), r"C:\".to_string())]
    }

    /// **前提**：两种写法确实收敛成一行。少了这条，下面两条断言可以在一个
    /// 「根本没合并」的实现上照样绿 —— 本仓那条「护栏只挡它见过的形状」的反面。
    #[test]
    fn two_spellings_of_one_directory_converge_into_a_single_row() {
        let rows = vec![
            root_row("/mnt/c/w/p", None, IdentityVerdict::NotProbed),
            root_row(r"C:\w\p", None, IdentityVerdict::NotProbed),
        ];
        let out = TotalStore::converge_mnt_roots(rows, &c_drive());
        assert_eq!(
            out.len(),
            1,
            "同一个目录的两种写法应收敛成一行，得到 {out:?}"
        );
        let head = &out[0];
        for form in ["/mnt/c/w/p", r"C:\w\p"] {
            assert!(
                head.root_path == form || head.aliases.iter().any(|a| a == form),
                "{form} 既不是代表也不在 aliases 里 ⇒ 消费方手上那种写法对不上号：{head:?}"
            );
        }
    }

    /// 🔴 **代表行说不出身份就报说不出，不继承组里别人的。**
    ///
    /// 上一版是「代表行没身份就取组里任意一个非空的」。`NotProbed` 的含义是
    /// 「这个写法还没被扫过」，不是「它没有身份」 —— 于是一条**过去**扫到的
    /// 身份会被嫁接到一条**当下**没扫过的行上。那个目录如今若是另一个项目，
    /// 它的记忆全归到旧项目名下，而**没有任何东西会报错**。
    ///
    /// ⚠️ 断言写成「代表行的身份 == 它自己那条输入的身份」而不是写死 `None`：
    /// 代表由 `host_openable_form` 选，跨平台会选中不同的行，而**要钉住的性质
    /// 与选中谁无关** —— 身份归属于量到它的那一行。
    #[test]
    fn a_representative_reports_its_own_identity_never_a_groupmates() {
        let inputs = vec![
            root_row("/mnt/c/w/p", Some("gh:acme/old"), IdentityVerdict::Resolved),
            root_row(r"C:\w\p", None, IdentityVerdict::NotProbed),
        ];
        let expected: Vec<(String, Option<String>)> = inputs
            .iter()
            .map(|r| (r.root_path.clone(), r.canonical_id.clone()))
            .collect();

        let out = TotalStore::converge_mnt_roots(inputs, &c_drive());
        assert_eq!(out.len(), 1, "前提没成立：这两条没收敛，本条断言是空的");
        let head = &out[0];
        let own = expected
            .iter()
            .find(|(path, _)| *path == head.root_path)
            .map(|(_, id)| id.clone())
            .expect("代表行的路径必须来自输入之一");
        assert_eq!(
            head.canonical_id, own,
            "代表行报了一个不属于它自己那条的身份 ⇒ 跨项目错误归属：{head:?}"
        );
        if own.is_none() {
            assert_eq!(
                head.identity_verdict,
                IdentityVerdict::NotProbed,
                "身份没继承但判决继承了 ⇒ 「没有身份」与「说它有」自相矛盾"
            );
        }
    }

    /// 身份**冲突**时整组不动 —— 合并要靠证据，猜一个等于替两个项目做决定。
    #[test]
    fn conflicting_identities_leave_the_group_unmerged() {
        let rows = vec![
            root_row("/mnt/c/w/p", Some("gh:acme/one"), IdentityVerdict::Resolved),
            root_row(r"C:\w\p", Some("gh:acme/two"), IdentityVerdict::Resolved),
        ];
        let out = TotalStore::converge_mnt_roots(rows, &c_drive());
        assert_eq!(out.len(), 2, "同键但身份互斥 ⇒ 不许合并，得到 {out:?}");
    }

    /// 挂载表说不出话（WSL 没跑）⇒ **照实分开**，不按路径形状猜。
    #[test]
    fn without_a_mount_table_nothing_converges() {
        let rows = vec![
            root_row("/mnt/c/w/p", None, IdentityVerdict::NotProbed),
            root_row(r"C:\w\p", None, IdentityVerdict::NotProbed),
        ];
        let out = TotalStore::converge_mnt_roots(rows, &Vec::new());
        assert_eq!(out.len(), 2, "没有挂载表就不该断定这两条是同一个目录");
    }

    // ── snapshot_cursor：元数据补齐要重发，但不许无限重发（2026-08-29）──────
    //
    // ⚠️ 这个函数的文档一直写着「纯函数……下面的单测能直接钉住三条分支」，
    // 而在此之前**一条直接调它的测试都没有**。补上，那句话才成立。

    fn snap_source(project_root: Option<&str>) -> crate::discover::SourceRef {
        crate::discover::SourceRef {
            source_type: SourceType::ClaudeCode,
            source_location: SourceLocation::Local,
            source_mode: crate::rawevent::SourceMode::SnapshotFile,
            path: std::path::PathBuf::from("/p/CLAUDE.md"),
            project_root: project_root.map(|s| s.to_string()),
            artifact_kind: Some("memory".to_string()),
        }
    }

    fn snap_event(project_root: Option<&str>, modified_at: Option<&str>) -> RawEvent {
        let mut e = mk_event(7, "snapshot", Some("body"));
        e.source_mode = crate::rawevent::SourceMode::SnapshotFile;
        e.source_path = "/p/CLAUDE.md".to_string();
        e.source_session_id = "snapshot".to_string();
        e.event_type = EventType::ConfigSnapshot;
        e.project_root = project_root.map(|s| s.to_string());
        e.artifact_kind = Some("memory".to_string());
        e.content_hash = Some("sha256:deadbeef".to_string());
        e.occurred_at = None;
        e.time_confidence = TimeConfidence::Low;
        e.observed_at = Some("1787884118".to_string());
        e.modified_at = modified_at.map(|s| s.to_string());
        e
    }

    /// 元数据齐了 ⇒ 种上内容指纹 ⇒ 内容没变就不重发。**稳态不churn。**
    #[test]
    fn a_snapshot_with_complete_metadata_seeds_the_fingerprint_cursor() {
        let latest = vec![(1, snap_event(Some("/p"), Some("1787000000")))];
        // `Fn` 不能改捕获变量 —— 用 Cell 记次数（比换成 `FnMut` 轻）。
        let probed = std::cell::Cell::new(0u32);
        let cur = TotalStore::snapshot_cursor(&latest, &snap_source(Some("/p")), &|_| {
            probed.set(probed.get() + 1);
            Some(1)
        });
        assert_eq!(cur.content_hash.as_deref(), Some("sha256:deadbeef"));
        // 🔴 **一次都不许探。** WSL 侧一次 stat 拉一个 shell，无条件探会让整轮
        // 同步慢一个量级 —— 而那个代价不会有任何东西报出来。
        assert_eq!(probed.get(), 0, "已经有 mtime 就不该再去 stat");
    }

    /// 存量快照缺 `modified_at`、而现在取得到 ⇒ **不种游标** ⇒ 重发一版补齐。
    /// 与既有的「宿主补齐了 project_root」同一条路径。
    #[test]
    fn a_snapshot_missing_mtime_is_re_emitted_when_we_can_now_get_one() {
        let latest = vec![(1, snap_event(Some("/p"), None))];
        let cur = TotalStore::snapshot_cursor(&latest, &snap_source(Some("/p")), &|_| Some(1));
        assert!(
            cur.content_hash.is_none(),
            "缺 mtime 且现在取得到 ⇒ 该重发一版，而不是永久停在没有 mtime"
        );
        // seq 仍要接着走 —— 重发的是**新版本**，不是从头开始。
        assert_eq!(cur.next_seq, 8);
    }

    /// 🔴 **自限**：取不到就别动它。
    ///
    /// 只写 `is_none()` 的话，一台 WSL 桥不通的机器会**每一轮都重发一版**
    /// （新版本同样没有 mtime ⇒ 下一轮再判一次「缺」）—— 而快照版本**没有上限**，
    /// 那是一个无声的无限增长：`git status` 不会说，界面不会说，只有 `du` 看得见。
    #[test]
    fn a_snapshot_missing_mtime_is_left_alone_when_we_still_cannot_get_one() {
        let latest = vec![(1, snap_event(Some("/p"), None))];
        let cur = TotalStore::snapshot_cursor(&latest, &snap_source(Some("/p")), &|_| None);
        assert_eq!(
            cur.content_hash.as_deref(),
            Some("sha256:deadbeef"),
            "取不到 mtime 就不该重发 —— 否则每轮一版，无限增长"
        );
    }

    /// 既有那条分支没被这次改动弄坏：身份变了照样重发。
    #[test]
    fn a_snapshot_whose_project_root_changed_is_still_re_emitted() {
        let latest = vec![(1, snap_event(None, Some("1787000000")))];
        let cur = TotalStore::snapshot_cursor(&latest, &snap_source(Some("/p")), &|_| Some(1));
        assert!(cur.content_hash.is_none(), "宿主补齐了身份 ⇒ 该重发");
    }

    pub(super) fn mk_event(seq: u64, session: &str, content: Option<&str>) -> RawEvent {
        RawEvent {
            schema_version: SCHEMA_VERSION,
            source_type: SourceType::ClaudeCode,
            source_location: SourceLocation::Local,
            source_path: "/p/file.jsonl".to_string(),
            source_session_id: session.to_string(),
            seq,
            event_key: None,
            source_mode: crate::rawevent::SourceMode::AppendLog,
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
            content: content.map(|s| s.to_string()),
            parent_ref: None,
            content_hash: None,
            artifact_kind: None,
            observed_at: None,
            modified_at: None,
            message_id: None,
            request_id: None,
        }
    }

    fn mk_event_at(seq: u64, session: &str, source_path: &str) -> RawEvent {
        let mut ev = mk_event(seq, session, None);
        ev.source_path = source_path.to_string();
        ev
    }

    #[test]
    fn snapshot_sync_is_incremental_and_latest_query_returns_current_version() {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("svault-store-snapshot-{nanos}.md"));
        std::fs::write(&path, "# memory\nv1\n").unwrap();
        let source = SourceRef {
            source_type: SourceType::Codex,
            source_location: SourceLocation::Local,
            source_mode: SourceMode::SnapshotFile,
            path: path.clone(),
            project_root: None,
            artifact_kind: Some("memory".into()),
        };
        let store = TotalStore::open_in_memory().unwrap();
        let first = store.sync_snapshots(std::slice::from_ref(&source)).unwrap();
        assert_eq!((first.changed, first.appended), (1, 1));
        let second = store.sync_snapshots(std::slice::from_ref(&source)).unwrap();
        assert_eq!((second.unchanged, second.appended), (1, 0));

        std::fs::write(&path, "# memory\nv2\n").unwrap();
        let third = store.sync_snapshots(std::slice::from_ref(&source)).unwrap();
        assert_eq!((third.changed, third.appended), (1, 1));
        let mut identified = source.clone();
        identified.project_root = Some("C:/work/project".into());
        let metadata = store
            .sync_snapshots(std::slice::from_ref(&identified))
            .unwrap();
        assert_eq!((metadata.changed, metadata.appended), (1, 1));

        let latest = store.read_latest_snapshots().unwrap();
        assert_eq!(latest.len(), 1);
        assert_eq!(latest[0].1.seq, 2);
        assert_eq!(latest[0].1.project_root.as_deref(), Some("C:/work/project"));
        assert_eq!(latest[0].1.content.as_deref(), Some("# memory\nv2\n"));
        assert_eq!(
            store.status().unwrap().count,
            3,
            "历史版本仍 append-only 保留"
        );
        std::fs::remove_file(path).unwrap();
        assert!(store.read_active_latest_snapshots().unwrap().is_empty());
        assert_eq!(store.read_latest_snapshots().unwrap().len(), 1);
    }

    /// 🔴 **本机快照的「没问成」必须保留，与 WSL 支同一判据。**
    ///
    /// 从前本机支是 `Path::new(&event.source_path).is_file()` —— 权限拒绝、句柄耗尽、
    /// 外接盘没挂上全折叠成「文件没了」，那条快照就此退出 `svault snapshots` 的输出，
    /// 该项目的 `CLAUDE.md` 规则静默不再进记忆。而**同一个函数里**隔四行的 WSL 支
    /// 早就写对了（`existing.insert(distro, None)` + `is_none_or`）。
    ///
    /// ⚠️ **两端都断言。** 只钉「没问成要保留」的话，一个恒 `true` 的实现照样绿
    /// （本轮评审里这个形状出现过两次）；只钉「确认没了要删」则恒 `false` 照样绿。
    /// 两条一起，任何与探测结果无关的实现都至少红一条。
    #[test]
    fn an_unprobeable_local_snapshot_is_kept_not_treated_as_deleted() {
        struct Fixed(fn(&Path) -> crate::probe::Probed<crate::probe::FileKind>);
        impl ProbeBackend for Fixed {
            fn probe(
                &self,
                p: &Path,
                _d: Deadline,
            ) -> crate::probe::Probed<crate::probe::FileKind> {
                (self.0)(p)
            }
            /// 本 fixture **只答探测**。读到这里说明测试的形状变了 —— 见 `ProbeBackend::read_text`。
            fn read_text(&self, p: &Path, _d: Deadline) -> crate::probe::Probed<String> {
                panic!("{p:?}: this fixture only answers probes; a read here means the test changed shape")
            }
        }

        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("svault-store-unprobeable-{nanos}.md"));
        std::fs::write(&path, "# rules\n").unwrap();
        let source = SourceRef {
            source_type: SourceType::ClaudeCode,
            source_location: SourceLocation::Local,
            source_mode: SourceMode::SnapshotFile,
            path: path.clone(),
            project_root: Some("C:/work/project".into()),
            artifact_kind: Some("memory".into()),
        };
        let store = TotalStore::open_in_memory().unwrap();
        store.sync_snapshots(std::slice::from_ref(&source)).unwrap();
        assert_eq!(store.read_latest_snapshots().unwrap().len(), 1);

        // 文件真的还在，但探测「没问成」⇒ 必须保留。
        let unknown = Fixed(|p| {
            crate::probe::Probed::Unknown(crate::probe::ProbeError::new(p, "permission denied"))
        });
        assert_eq!(
            store
                .read_active_latest_snapshots_with(&unknown)
                .unwrap()
                .len(),
            1,
            "探测失败被当成删除 —— 这个项目的指令文件会静默退出视图"
        );

        // 反向：探明白了没有 ⇒ 才是删除。少了这条，恒 `true` 的实现也能绿。
        let absent = Fixed(|_| crate::probe::Probed::Absent);
        assert!(
            store
                .read_active_latest_snapshots_with(&absent)
                .unwrap()
                .is_empty(),
            "确认不存在的源文件仍被返回 —— 已删除的项目规则会一直挂在视图里"
        );

        std::fs::remove_file(path).unwrap();
    }

    /// 🔴 同一个 source 在**一次** `sync_snapshots` 里出现两次。
    ///
    /// 上面那条测试每次 `sync_snapshots` 只给一个 source，所以「每来源最新快照」这份
    /// 缓存在每次调用开头都是新读的 —— 它**测不到**缓存提到循环外之后新增的那个风险：
    /// 循环内写了库，缓存却还是进来时的样子。第二次于是拿旧游标重扫。
    ///
    /// `append_events` 幂等（撞 `dedup_key` 唯一约束走 `skipped_dup`），所以症状不是写重，
    /// 而是**白扫一遍 + `changed` 虚高** —— 一个只看事件条数的断言会放它过去。
    #[test]
    fn a_source_listed_twice_in_one_sync_is_scanned_once() {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("svault-store-snapshot-dup-{nanos}.md"));
        std::fs::write(&path, "# memory\nv1\n").unwrap();
        let source = SourceRef {
            source_type: SourceType::Codex,
            source_location: SourceLocation::Local,
            source_mode: SourceMode::SnapshotFile,
            path: path.clone(),
            project_root: None,
            artifact_kind: Some("memory".into()),
        };
        let store = TotalStore::open_in_memory().unwrap();
        let stats = store
            .sync_snapshots(&[source.clone(), source.clone()])
            .unwrap();

        assert_eq!(stats.sources, 2, "两个条目都该被处理");
        assert_eq!(
            (stats.changed, stats.unchanged, stats.appended),
            (1, 1, 1),
            "第二个条目必须命中循环内刷新过的缓存 → unchanged，而不是再扫一次"
        );
        assert_eq!(store.status().unwrap().count, 1, "总库里只该有一条快照事件");
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn read_session_orders_by_offset_and_scopes_by_file() {
        let store = TotalStore::open_in_memory().unwrap();
        // 文件 A 的 session "s"（seq 乱序入库，验证按 offset/seq 升序取回）。
        let a0 = mk_event_at(0, "s", "/a.jsonl");
        let mut a1 = mk_event_at(1, "s", "/a.jsonl");
        a1.content = Some("second".to_string());
        let mut a0c = a0.clone();
        a0c.content = Some("first".to_string());
        // 文件 B 的同名 session "s"（--resume replay）+ 文件 A 的另一 session "t"。
        let b0 = mk_event_at(0, "s", "/b.jsonl");
        let t0 = mk_event_at(0, "t", "/a.jsonl");
        // 🔴 按文件分批。「一批 = 一个文件」早就写在 append_events 的文档里，但从前
        // 没有任何东西检查，于是这条测试一直混批。作用域显式之后它被当场拒绝
        // （ForeignEvent），这里改回契约本来的样子。
        store
            .append_events(&[a1, a0c, t0], Projection::Append)
            .unwrap();
        store.append_events(&[b0], Projection::Append).unwrap();

        let got = store
            .read_session(
                SourceType::ClaudeCode,
                &SourceLocation::Local,
                "/a.jsonl",
                "s",
            )
            .unwrap();
        // 只 A 文件的 session s 两条，按 seq 升序，无跳过。
        assert_eq!(got.events.len(), 2, "只取 (A, s)，不串 (B, s) / (A, t)");
        assert_eq!(got.skipped, 0);
        assert_eq!(got.events[0].content.as_deref(), Some("first"));
        assert_eq!(got.events[1].content.as_deref(), Some("second"));
        assert!(got.events[0].seq < got.events[1].seq, "按 seq 升序");

        // 跨文件 replay 的同名 session 各自独立。
        let from_b = store
            .read_session(
                SourceType::ClaudeCode,
                &SourceLocation::Local,
                "/b.jsonl",
                "s",
            )
            .unwrap();
        assert_eq!(from_b.events.len(), 1);
    }

    #[test]
    fn read_session_reports_skipped_on_corrupt_row() {
        // 评审 [P2]：损坏的 event_json 行不得静默吞成「部分成功」——须计入 skipped，让 transcript
        // 调用方据此回落 live（而非展示缺气泡的半截 transcript）。
        let store = TotalStore::open_in_memory().unwrap();
        store
            .append_events(
                &[
                    mk_event(0, "s", Some("ok")),
                    mk_event(1, "s", Some("also ok")),
                ],
                Projection::Append,
            )
            .unwrap();
        // 直接往库里塞一行无法反序列化为 RawEvent 的 event_json（模拟损坏 / 未来不兼容 schema）。
        {
            let conn = store.conn.lock().unwrap();
            conn.execute(
                r#"INSERT INTO raw_events
                     (ingested_at, schema_version, source_type, source_location, source_path,
                      source_session_id, seq, event_type, occurred_at, project_root, event_json)
                   VALUES (0, 1, 'claude_code', 'local', '/p/file.jsonl', 's', 2, 'message',
                           NULL, NULL, '{ not valid json for RawEvent }')"#,
                [],
            )
            .unwrap();
        }
        let read = store
            .read_session(
                SourceType::ClaudeCode,
                &SourceLocation::Local,
                "/p/file.jsonl",
                "s",
            )
            .unwrap();
        assert_eq!(read.events.len(), 2, "好行仍取回");
        assert_eq!(read.skipped, 1, "损坏行计入 skipped，不静默");
    }

    #[test]
    fn read_since_page_reports_max_scanned_offset_across_a_bad_row_window() {
        // 评审 [P1]：read_since_page 在 SQL LIMIT 之后才 skip 坏行，故一窗全坏时 events 为空，
        // 但 max_scanned_offset 必须仍指向扫描到的最大 offset——否则 pull 流会把「整窗坏行」
        // 误判成「追平」，坏行之后的有效事件永久不可达。
        let store = TotalStore::open_in_memory().unwrap();
        store
            .append_events(&[mk_event(0, "s", Some("good-0"))], Projection::Append)
            .unwrap();
        // 直接塞两行坏 event_json（offset 紧随 good-0）。
        {
            let conn = store.conn.lock().unwrap();
            for seq in 1..=2 {
                conn.execute(
                    r#"INSERT INTO raw_events
                         (ingested_at, schema_version, source_type, source_location, source_path,
                          source_session_id, seq, event_type, occurred_at, project_root, event_json)
                       VALUES (0, 1, 'claude_code', 'local', '/p/file.jsonl', 's', ?1, 'message',
                               NULL, NULL, '{ corrupt }')"#,
                    params![seq],
                )
                .unwrap();
            }
        }
        store
            .append_events(&[mk_event(3, "s", Some("good-3"))], Projection::Append)
            .unwrap();

        // good-0 在 offset 1；坏行在 offset 2、3；good-3 在 offset 4。
        // 用 limit=2 的窗口，从 good-0 之后取 → 命中两条坏行：events 空、max_scanned=Some(3)。
        let g0 = store.read_since_page(0, 1).unwrap();
        assert_eq!(g0.events.len(), 1);
        let after_g0 = g0.max_scanned_offset.unwrap();

        let bad_window = store.read_since_page(after_g0, 2).unwrap();
        assert!(bad_window.events.is_empty(), "整窗坏行 → events 空");
        assert!(
            bad_window.max_scanned_offset.is_some()
                && bad_window.max_scanned_offset.unwrap() > after_g0,
            "但 max_scanned_offset 仍推进，让消费者越过坏行"
        );

        // 从坏行窗口的 max_scanned 续读 → 拿到 good-3（证明坏行之后的事件可达）。
        let after_bad = store
            .read_since_page(bad_window.max_scanned_offset.unwrap(), 100)
            .unwrap();
        assert_eq!(after_bad.events.len(), 1);
        assert_eq!(after_bad.events[0].1.content.as_deref(), Some("good-3"));
    }

    #[test]
    fn append_is_idempotent_by_identity() {
        let store = TotalStore::open_in_memory().unwrap();
        let batch = vec![
            mk_event(0, "s1", Some("hello")),
            mk_event(1, "s1", Some("world")),
        ];
        let first = store.append_events(&batch, Projection::Append).unwrap();
        assert_eq!(first.appended, 2);
        assert_eq!(first.skipped_dup, 0);

        // 重放同批（force 重扫场景）→ 全部 dedup，count 不变。
        let again = store.append_events(&batch, Projection::Append).unwrap();
        assert_eq!(again.appended, 0);
        assert_eq!(again.skipped_dup, 2);
        assert_eq!(store.status().unwrap().count, 2);
    }

    #[test]
    fn identity_uses_composite_key_not_ambiguous_concat() {
        // 回归 [P1]：字符串拼接 `path|session|seq` 会让 (`/a|b`,`c`) 撞 (`/a`,`b|c`)，
        // 静默丢一条。五列复合 UNIQUE 不歧义——两条都得保留。
        let store = TotalStore::open_in_memory().unwrap();
        let a = mk_event_at(0, "c", "/a|b");
        let b = mk_event_at(0, "b|c", "/a");
        // 两条属于**不同文件**（`/a|b` 与 `/a`），所以必须分两批 —— 这正是本条测试要
        // 说的事：`|` 出现在路径里时，把身份拼成一个串会歧义。
        let first = store.append_events(&[a], Projection::Append).unwrap();
        let second = store.append_events(&[b], Projection::Append).unwrap();
        assert_eq!(
            first.appended + second.appended,
            2,
            "含 `|` 的两条不同身份必须都入库（不碰撞）"
        );
        assert_eq!(store.status().unwrap().count, 2);
    }

    #[test]
    fn offset_is_monotonic_and_read_since_paginates() {
        let store = TotalStore::open_in_memory().unwrap();
        for seq in 0..5u64 {
            store
                .append_events(
                    &[mk_event(seq, "s1", Some(&format!("m{seq}")))],
                    Projection::Append,
                )
                .unwrap();
        }
        let all = store.read_since(0, 100).unwrap();
        assert_eq!(all.len(), 5);
        // offset 严格升序。
        let offsets: Vec<i64> = all.iter().map(|(o, _)| *o).collect();
        assert!(
            offsets.windows(2).all(|w| w[0] < w[1]),
            "offset 须单调升: {offsets:?}"
        );
        // 正文无损往返（明文 MVP）。
        assert_eq!(all[0].1.content.as_deref(), Some("m0"));

        // 分页：从第 2 条 offset 之后取 2 条。
        let page = store.read_since(offsets[1], 2).unwrap();
        assert_eq!(page.len(), 2);
        assert_eq!(page[0].0, offsets[2]);
    }

    #[test]
    fn tombstoned_source_is_skipped_on_read() {
        let store = TotalStore::open_in_memory().unwrap();
        store
            .append_events(
                &[mk_event(0, "keep", None), mk_event(0, "drop", None)],
                Projection::Append,
            )
            .unwrap();
        assert_eq!(store.read_since(0, 100).unwrap().len(), 2);
        let erased = store.tombstone(TombstoneScope::Session, "drop").unwrap();
        assert_eq!(erased.deleted_events, 1);
        assert_eq!(
            erased.keys_destroyed, 0,
            "同组仍有事件时不得销毁共享数据密钥"
        );
        let visible = store.read_since(0, 100).unwrap();
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].1.source_session_id, "keep");
        // 用户主动 erase 是默认 append-only 的例外：正文物理删除，只留不含正文的墓碑。
        assert_eq!(store.status().unwrap().count, 1);
        let replay = store
            .append_events(
                &[mk_event(0, "drop", Some("must not return"))],
                Projection::Append,
            )
            .unwrap();
        assert_eq!(replay.appended, 0);
        assert_eq!(replay.skipped_erased, 1);
        assert_eq!(store.status().unwrap().count, 1, "墓碑阻止源文件重扫复活");
    }

    #[test]
    fn tombstone_scope_does_not_cross_dimensions() {
        // 回归 [P2]：墓碑带 scope。session 名恰等于另一条的 project_root 值时，
        // 删 project_root 不得连带隐藏 session（反之亦然）。
        let store = TotalStore::open_in_memory().unwrap();
        let mut by_session = mk_event(0, "/work", None); // session_id 恰为 "/work"
        by_session.project_root = Some("/other".to_string());
        let mut by_project = mk_event(0, "sess-y", None);
        by_project.project_root = Some("/work".to_string());
        store
            .append_events(&[by_session, by_project], Projection::Append)
            .unwrap();

        // 只墓碑 project_root=/work → 只隐藏 by_project，不碰 session 名为 /work 的那条。
        store
            .tombstone(TombstoneScope::ProjectRoot, "/work")
            .unwrap();
        let visible = store.read_since(0, 100).unwrap();
        assert_eq!(visible.len(), 1);
        assert_eq!(
            visible[0].1.source_session_id, "/work",
            "session 维度不应被 project 墓碑误伤"
        );
    }

    #[test]
    fn backfilled_flag_defaults_false_and_round_trips() {
        let store = TotalStore::open_in_memory().unwrap();
        assert!(!store.is_backfilled().unwrap(), "新库默认未回填");
        store.set_backfilled(true).unwrap();
        assert!(store.is_backfilled().unwrap());
        store.set_backfilled(false).unwrap();
        assert!(!store.is_backfilled().unwrap(), "append 失败后可清回未回填");
    }

    #[test]
    fn status_reports_count_and_max_offset() {
        let store = TotalStore::open_in_memory().unwrap();
        assert_eq!(store.status().unwrap().count, 0);
        let stats = store
            .append_events(
                &[mk_event(0, "s", None), mk_event(1, "s", None)],
                Projection::Append,
            )
            .unwrap();
        let st = store.status().unwrap();
        assert_eq!(st.count, 2);
        assert_eq!(st.max_offset, stats.max_offset);
        assert!(st.last_ingested_at.is_some());
        assert!(st.encrypted);
        assert_eq!(st.encryption_version, 2);
        assert_eq!(st.active_data_keys, 1);
        assert_eq!(st.key_scheme, "per-source-project");
    }

    #[test]
    fn event_json_is_authenticated_ciphertext_at_rest() {
        let store = TotalStore::open_in_memory().unwrap();
        store
            .append_events(
                &[mk_event(0, "s", Some("private-marker-027"))],
                Projection::Append,
            )
            .unwrap();
        let stored: String = store
            .conn
            .lock()
            .unwrap()
            .query_row("SELECT event_json FROM raw_events", [], |row| row.get(0))
            .unwrap();
        assert!(stored.starts_with("sv2:"));
        assert!(!stored.contains("private-marker-027"));
        assert_eq!(
            store.read_since(0, 10).unwrap()[0].1.content.as_deref(),
            Some("private-marker-027")
        );
    }

    #[test]
    fn plaintext_body_is_absent_from_sqlite_wal_and_shm() {
        let dir = std::env::temp_dir().join(format!(
            "sv_encrypted_files_{}_{}",
            std::process::id(),
            now_unix_secs()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("total_store.db");
        let marker = b"synthetic-private-marker-027-disk";
        {
            let store = TotalStore::open_with_key(&path, StoreKey::from_bytes([11u8; 32])).unwrap();
            store
                .append_events(
                    &[mk_event(
                        0,
                        "synthetic-session",
                        Some(std::str::from_utf8(marker).unwrap()),
                    )],
                    Projection::Append,
                )
                .unwrap();

            for candidate in [
                path.clone(),
                path.with_extension("db-wal"),
                path.with_extension("db-shm"),
            ] {
                if !candidate.exists() {
                    continue;
                }
                let bytes = std::fs::read(&candidate).unwrap();
                assert!(
                    !bytes.windows(marker.len()).any(|window| window == marker),
                    "plaintext body leaked into {}",
                    candidate.display()
                );
            }
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn project_and_source_erase_destroy_only_orphaned_group_keys() {
        let store = TotalStore::open_in_memory().unwrap();
        let mut project_a = mk_event(0, "a", Some("project-a-secret"));
        project_a.project_root = Some("/work/a".into());
        let mut project_b = mk_event(1, "b", Some("project-b-secret"));
        project_b.project_root = Some("/work/b".into());
        store
            .append_events(&[project_a.clone(), project_b.clone()], Projection::Append)
            .unwrap();
        assert_eq!(store.status().unwrap().active_data_keys, 2);

        let erased = store
            .tombstone(TombstoneScope::ProjectRoot, "/work/a")
            .unwrap();
        assert_eq!(erased.deleted_events, 1);
        assert_eq!(erased.keys_destroyed, 1);
        assert_eq!(store.status().unwrap().active_data_keys, 1);
        let visible = store.read_since(0, 10).unwrap();
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].1.content.as_deref(), Some("project-b-secret"));

        let erased = store
            .tombstone(TombstoneScope::SourcePath, &project_b.source_path)
            .unwrap();
        assert_eq!(erased.deleted_events, 1);
        assert_eq!(erased.keys_destroyed, 1);
        assert_eq!(store.status().unwrap().active_data_keys, 0);
    }

    #[test]
    fn ciphertext_cannot_be_swapped_between_rows() {
        let store = TotalStore::open_in_memory().unwrap();
        store
            .append_events(
                &[
                    mk_event(0, "s", Some("first")),
                    mk_event(1, "s", Some("second")),
                ],
                Projection::Append,
            )
            .unwrap();
        {
            let conn = store.conn.lock().unwrap();
            conn.execute_batch(
                "CREATE TEMP TABLE swap(v TEXT);\
                 INSERT INTO swap SELECT event_json FROM raw_events WHERE seq = 0;\
                 UPDATE raw_events SET event_json = (SELECT event_json FROM raw_events WHERE seq = 1) WHERE seq = 0;\
                 UPDATE raw_events SET event_json = (SELECT v FROM swap) WHERE seq = 1;",
            )
            .unwrap();
        }
        assert!(
            store.read_since(0, 10).unwrap().is_empty(),
            "row identity is authenticated as AAD; swapped ciphertext must fail closed"
        );
    }

    #[test]
    fn wrapped_data_keys_cannot_be_swapped_between_groups() {
        let store = TotalStore::open_in_memory().unwrap();
        let mut first = mk_event_at(0, "first", "/a.jsonl");
        first.content = Some("first-secret".into());
        let mut second = mk_event_at(0, "second", "/b.jsonl");
        second.content = Some("second-secret".into());
        // 两个文件 → 两批（「一批 = 一个文件」，现在由 `apply_projection` 强制）。
        store.append_events(&[first], Projection::Append).unwrap();
        store.append_events(&[second], Projection::Append).unwrap();
        {
            let conn = store.conn.lock().unwrap();
            conn.execute_batch(
                "CREATE TEMP TABLE swap_key(v TEXT);\
                 INSERT INTO swap_key SELECT wrapped_key FROM data_keys WHERE source_path = '/a.jsonl';\
                 UPDATE data_keys SET wrapped_key = (SELECT wrapped_key FROM data_keys WHERE source_path = '/b.jsonl') WHERE source_path = '/a.jsonl';\
                 UPDATE data_keys SET wrapped_key = (SELECT v FROM swap_key) WHERE source_path = '/b.jsonl';",
            )
            .unwrap();
        }
        assert!(
            store.read_since(0, 10).unwrap().is_empty(),
            "wrapped data keys are authenticated to their source/project group"
        );
    }

    #[test]
    fn wrong_key_fails_open_instead_of_returning_empty_data() {
        let dir = std::env::temp_dir().join(format!("sv_wrong_key_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("total_store.db");
        let _ = std::fs::remove_file(&path);
        {
            let store = TotalStore::open_with_key(&path, StoreKey::from_bytes([1u8; 32])).unwrap();
            store
                .append_events(&[mk_event(0, "s", Some("secret"))], Projection::Append)
                .unwrap();
        }
        let reopened = TotalStore::open_with_key(&path, StoreKey::from_bytes([2u8; 32]));
        assert!(
            matches!(reopened, Err(StoreError::Crypto(CryptoError::Decrypt))),
            "wrong key must fail closed"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn sv1_master_envelopes_migrate_to_sv2_group_keys() {
        let dir = std::env::temp_dir().join(format!("sv_v2_mig_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("total_store.db");
        let event = mk_event(0, "legacy-v1", Some("legacy-v1-private"));
        let aad = event_aad(
            // 这个 fixture 造的就是一行 **v1 遗留行**：AAD 里没有 projection_revision，
            // 且 aad_version 列为 1。它同时钉住迁移的核心承诺 —— v1 行的 AAD 字节不变，
            // 所以 1.66 GB 密文一行都不用重新加密。
            1,
            source_type_key(event.source_type),
            &event.source_location.as_key(),
            &event.source_path,
            &event.source_session_id,
            event.seq as i64,
            0,
            0,
        );
        let legacy = StoreCipher::new(StoreKey::from_bytes([9u8; 32]))
            .encrypt(serde_json::to_string(&event).unwrap().as_bytes(), &aad)
            .unwrap();
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(RAW_EVENTS_DDL).unwrap();
            conn.execute(
                r#"INSERT INTO raw_events
                     (ingested_at, schema_version, source_type, source_location, source_path,
                      source_session_id, seq, source_revision, projection_revision, aad_version,
                      event_type, occurred_at, project_root, event_json)
                   VALUES (0, 1, 'claude_code', 'local', '/p/file.jsonl', 'legacy-v1', 0, 0, 0, 1,
                           'message', NULL, NULL, ?1)"#,
                params![legacy],
            )
            .unwrap();
        }

        let store = TotalStore::open_with_key(&path, StoreKey::from_bytes([9u8; 32])).unwrap();
        let envelope: String = store
            .conn
            .lock()
            .unwrap()
            .query_row("SELECT event_json FROM raw_events", [], |row| row.get(0))
            .unwrap();
        assert!(envelope.starts_with("sv2:"));
        assert_eq!(store.status().unwrap().active_data_keys, 1);
        assert_eq!(
            store.read_since(0, 10).unwrap()[0].1.content.as_deref(),
            Some("legacy-v1-private")
        );
        drop(store);
        let bytes = std::fs::read(&path).unwrap();
        assert!(!bytes
            .windows(b"legacy-v1-private".len())
            .any(|window| window == b"legacy-v1-private"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rollback_supersedes_old_generation_in_read_session() {
        // 评审 [P1]：文件回退（截断/重写）后，总库不能再展示旧内容。
        let store = TotalStore::open_in_memory().unwrap();
        // 第 0 代：原内容（seq 0/1）。
        store
            .append_events(
                &[
                    mk_event(0, "s", Some("old-0")),
                    mk_event(1, "s", Some("old-1")),
                ],
                Projection::Append,
            )
            .unwrap();
        // 文件被重写 → 扫描器 is_rollback=true，新代同 seq 但不同内容。
        let stats = store
            .append_events(
                &[
                    mk_event(0, "s", Some("new-0")),
                    mk_event(1, "s", Some("new-1")),
                ],
                Projection::Rollback,
            )
            .unwrap();
        assert_eq!(
            stats.appended, 2,
            "新代事件不被旧代 dedup 挡（唯一键含 generation）"
        );

        // read_session 只取当前代 → 重写后的内容，不是旧的。
        let read = store
            .read_session(
                SourceType::ClaudeCode,
                &SourceLocation::Local,
                "/p/file.jsonl",
                "s",
            )
            .unwrap();
        assert_eq!(read.events.len(), 2);
        assert_eq!(read.events[0].content.as_deref(), Some("new-0"));
        assert_eq!(read.events[1].content.as_deref(), Some("new-1"));

        // 旧代仍物理留存（append-only 不可变；TumeFlow pull 经 read_since 见全历史 4 条）。
        assert_eq!(store.status().unwrap().count, 4);
        assert_eq!(store.read_since(0, 100).unwrap().len(), 4);

        // 再增量（非回退）→ 并入当前代，与新代一起读。
        store
            .append_events(&[mk_event(2, "s", Some("new-2"))], Projection::Append)
            .unwrap();
        let read2 = store
            .read_session(
                SourceType::ClaudeCode,
                &SourceLocation::Local,
                "/p/file.jsonl",
                "s",
            )
            .unwrap();
        assert_eq!(read2.events.len(), 3, "增量并入当前代");
        assert_eq!(read2.events[2].content.as_deref(), Some("new-2"));
    }

    #[test]
    fn migration_from_pre_generation_store_preserves_data() {
        // 模拟 generation 之前的库（五列唯一、无 generation 列），open 时应保数据重建。
        let dir = std::env::temp_dir().join(format!("sv_store_mig_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("total_store.db");
        let _ = std::fs::remove_file(&path);
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                r#"
                CREATE TABLE raw_events (
                    offset INTEGER PRIMARY KEY AUTOINCREMENT,
                    ingested_at INTEGER NOT NULL, schema_version INTEGER NOT NULL,
                    source_type TEXT NOT NULL, source_location TEXT NOT NULL,
                    source_path TEXT NOT NULL, source_session_id TEXT NOT NULL,
                    seq INTEGER NOT NULL, event_type TEXT NOT NULL, occurred_at TEXT,
                    project_root TEXT, event_json TEXT NOT NULL,
                    UNIQUE (source_type, source_location, source_path, source_session_id, seq)
                );
                "#,
            )
            .unwrap();
            let ev = mk_event(0, "s", Some("legacy"));
            conn.execute(
                r#"INSERT INTO raw_events (ingested_at, schema_version, source_type, source_location,
                       source_path, source_session_id, seq, event_type, occurred_at, project_root, event_json)
                   VALUES (0, 1, 'claude_code', 'local', '/p/file.jsonl', 's', 0, 'message', NULL, NULL, ?1)"#,
                params![serde_json::to_string(&ev).unwrap()],
            )
            .unwrap();
        }
        // open → migrate 重建为含 generation 的六列唯一，数据保留。
        let store = TotalStore::open_with_key(&path, StoreKey::from_bytes([7u8; 32])).unwrap();
        let read = store
            .read_session(
                SourceType::ClaudeCode,
                &SourceLocation::Local,
                "/p/file.jsonl",
                "s",
            )
            .unwrap();
        assert_eq!(read.events.len(), 1, "迁移保留旧数据");
        assert_eq!(read.events[0].content.as_deref(), Some("legacy"));
        // 迁移后 generation 机制可用：回退取代。
        store
            .append_events(&[mk_event(0, "s", Some("rewritten"))], Projection::Rollback)
            .unwrap();
        let read2 = store
            .read_session(
                SourceType::ClaudeCode,
                &SourceLocation::Local,
                "/p/file.jsonl",
                "s",
            )
            .unwrap();
        assert_eq!(read2.events[0].content.as_deref(), Some("rewritten"));
        drop(store);
        let bytes = std::fs::read(&path).unwrap();
        assert!(
            !bytes.windows(b"legacy".len()).any(|w| w == b"legacy"),
            "迁移并 VACUUM 后数据库文件不得残留旧正文"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 🔴 解析器升级必须开新代，否则总库纹丝不动而没有任何错误可查。
    ///
    /// 这是本次改动的全部理由：文件没变、seq 没变，若沿用当前代，
    /// `INSERT OR IGNORE` 会把每一条重发事件都当成旧代的重复丢弃。
    /// 断言写成「读到的是新内容」而不是「代号 +1」—— 后者是实现细节，
    /// 前者才是用户/下游真正依赖的性质。
    #[test]
    fn reparse_opens_a_new_generation_so_re_emitted_events_are_not_deduped() {
        let store = TotalStore::open_in_memory().unwrap();
        let old = mk_event(0, "s", Some("parsed-by-rev-1"));
        assert_eq!(
            store
                .append_events(&[old], Projection::Append)
                .unwrap()
                .appended,
            1
        );

        // 同一个 (source, session, seq)，只是解析器更好了。
        let reparsed = mk_event(0, "s", Some("parsed-by-rev-2"));

        // 先证明「当成增量」会静默丢弃 —— 没有这一半，下面那一半可能只是碰巧过。
        let as_append = store
            .append_events(std::slice::from_ref(&reparsed), Projection::Append)
            .unwrap();
        assert_eq!(
            as_append.appended, 0,
            "同代同 seq 必然被 INSERT OR IGNORE 丢掉"
        );
        assert_eq!(as_append.skipped_dup, 1);

        let as_reparse = store
            .append_events(&[reparsed], Projection::Reparse)
            .unwrap();
        assert_eq!(as_reparse.appended, 1, "Reparse 必须落库");

        let read = store
            .read_session(
                SourceType::ClaudeCode,
                &SourceLocation::Local,
                "/p/file.jsonl",
                "s",
            )
            .unwrap();
        assert_eq!(
            read.events[0].content.as_deref(),
            Some("parsed-by-rev-2"),
            "读到的应是新解析器的结果"
        );
    }

    /// 🔴 ADR-044 决定 1：`Rollback` 与 `Reparse` 推进**不同的维度**。
    ///
    /// 这是把 `generation` 拆成两个字段的全部意义。用一个整数时两者都只是「+1」，
    /// 于是任何回收都只能一刀切 —— 而它们的留存价值相反：`Rollback` 的旧版本是磁盘上
    /// 已消失内容的唯一副本，`Reparse` 的旧投影是同一批字节的更差解析、可再生。
    ///
    /// 断言维度而不是断言编号：编号怎么排是实现细节，「哪个维度动了」是契约。
    #[test]
    fn rollback_and_reparse_advance_different_dimensions() {
        let store = TotalStore::open_in_memory().unwrap();
        let ev = |seq: u64, body: &str| mk_event(seq, "s", Some(body));
        let head = |s: &TotalStore| {
            let conn = s.conn.lock().unwrap();
            head_of(&conn, "claude_code", "local", "/p/file.jsonl").unwrap()
        };

        store
            .append_events(&[ev(0, "v1")], Projection::Append)
            .unwrap();
        assert_eq!(head(&store), (0, 0), "首批落在 (0, 0)");

        store
            .append_events(&[ev(1, "v1")], Projection::Append)
            .unwrap();
        assert_eq!(head(&store), (0, 0), "增量不开新版本");

        store
            .append_events(&[ev(0, "v2")], Projection::Reparse)
            .unwrap();
        assert_eq!(
            head(&store),
            (0, 1),
            "Reparse 推进**投影**版本，源版本不动 —— 字节没变，变的是我们"
        );

        store
            .append_events(&[ev(0, "v3")], Projection::Rollback)
            .unwrap();
        assert_eq!(
            head(&store),
            (1, 0),
            "Rollback 推进**源**版本并把投影归零 —— 磁盘上的内容变了"
        );
    }

    // ── 开新代的幂等（ADR-051 I7 / 步 4）────────────────────────────

    fn key() -> SourceKey {
        SourceKey {
            source_type: crate::rawevent::SourceType::ClaudeCode,
            source_location: crate::rawevent::SourceLocation::Local,
            source_path: "/p/file.jsonl".to_string(),
        }
    }

    fn token_for(fp: &str) -> crate::token::ProjectionToken {
        crate::token::ProjectionToken::new(&key(), Some(fp), 4, 1, (0, 100))
    }

    fn batch(
        mode: Projection,
        seq: u64,
        body: &str,
        token: Option<crate::token::ProjectionToken>,
    ) -> FileProjectionBatch {
        FileProjectionBatch {
            source: key(),
            parser_revision: Some(4),
            mode,
            events: vec![mk_event(seq, "s", Some(body))],
            token,
        }
    }

    fn head_of_store(s: &TotalStore) -> (i64, i64) {
        let conn = s.conn.lock().unwrap();
        head_of(&conn, "claude_code", "local", "/p/file.jsonl").unwrap()
    }

    /// 🔴 **ADR-051 描述的那个崩溃序列，逐步复现。**
    ///
    /// ```text
    /// ① 总库成功 Rollback，推进 source_revision
    /// ② UI 索引提交前进程退出
    /// ③ UI 仍是旧游标 ⇒ 下轮再次检出 rollback
    /// ④ 总库再次推进 source_revision   ← 这一步必须被挡住
    /// ⑤ Rollback 的旧版本永不自动回收 ⇒ 每崩一次留一代垃圾
    /// ```
    #[test]
    fn a_replayed_rollback_does_not_open_another_generation() {
        let store = TotalStore::open_in_memory().unwrap();
        store
            .apply_projection(batch(Projection::Append, 0, "v1", None))
            .unwrap();
        assert_eq!(head_of_store(&store), (0, 0));

        let t = token_for("fp-after-rewrite");
        let first = store
            .apply_projection(batch(Projection::Rollback, 0, "v2", Some(t.clone())))
            .unwrap();
        assert_eq!(head_of_store(&store), (1, 0), "第一次 Rollback 开新源版本");
        assert!(first.head_moved);

        // ② 崩溃 → ③ 下轮同一个 token 重放。
        let second = store
            .apply_projection(batch(Projection::Rollback, 0, "v2", Some(t)))
            .unwrap();
        assert_eq!(
            head_of_store(&store),
            (1, 0),
            "重放不得再开一代 —— Rollback 的旧版本永不回收，每崩一次留一代垃圾"
        );
        assert_eq!(
            (second.source_revision, second.projection_revision),
            (1, 0),
            "返回的是**那一次**的头"
        );
        assert!(!second.head_moved, "头没动就不能说动了");
        assert_eq!(second.appended, 0, "一个字都没写");
    }

    /// `Reparse` 同理 —— 它会取代被超越的那代，但仍然多开一代。
    #[test]
    fn a_replayed_reparse_does_not_open_another_generation() {
        let store = TotalStore::open_in_memory().unwrap();
        store
            .apply_projection(batch(Projection::Append, 0, "v1", None))
            .unwrap();
        let t = token_for("same-bytes-better-parser");
        store
            .apply_projection(batch(Projection::Reparse, 0, "v2", Some(t.clone())))
            .unwrap();
        assert_eq!(head_of_store(&store), (0, 1));
        store
            .apply_projection(batch(Projection::Reparse, 0, "v2", Some(t)))
            .unwrap();
        assert_eq!(head_of_store(&store), (0, 1), "重放不得再开一代");
    }

    /// 🔴 **不同的操作必须各自开代** —— 幂等不能退化成「一律不做」。
    ///
    /// 少一个 token 分量就会把两次不同的操作当成同一次，第二次被静默忽略；
    /// 那比多开一代更糟 —— 它会让新解析器的结果永远进不去。
    #[test]
    fn a_genuinely_different_operation_still_opens_its_generation() {
        let store = TotalStore::open_in_memory().unwrap();
        store
            .apply_projection(batch(Projection::Append, 0, "v1", None))
            .unwrap();
        store
            .apply_projection(batch(
                Projection::Rollback,
                0,
                "v2",
                Some(token_for("fp-a")),
            ))
            .unwrap();
        assert_eq!(head_of_store(&store), (1, 0));
        // 又一次真实的重写 —— 指纹不同 ⇒ 另一个 token ⇒ 必须开新代。
        store
            .apply_projection(batch(
                Projection::Rollback,
                0,
                "v3",
                Some(token_for("fp-b")),
            ))
            .unwrap();
        assert_eq!(head_of_store(&store), (2, 0), "不同的操作要各自开代");
    }

    /// `Append` 不带 token 也照旧幂等（靠 seq 去重）—— 不因这套机制而改变。
    #[test]
    fn append_stays_idempotent_without_a_token() {
        let store = TotalStore::open_in_memory().unwrap();
        store
            .apply_projection(batch(Projection::Append, 0, "v1", None))
            .unwrap();
        let again = store
            .apply_projection(batch(Projection::Append, 0, "v1", None))
            .unwrap();
        assert_eq!(head_of_store(&store), (0, 0));
        assert_eq!(again.appended, 0, "同 seq 同内容按重复丢弃");
        assert_eq!(again.skipped_dup, 1);
    }

    /// 🔴 **短路返回的是「那一次」的头，不是「当前」头。**
    ///
    /// 中间可能已经有别的操作推进过。返回当前头会让调用方以为自己这次操作生效了 ——
    /// 它拿着一个别人造的版本号去 ack，两层从此对「当前是哪一代」各执一词。
    ///
    /// ⚠️ 这条是补写的：原先只有一条「中间没有别的操作」的测试，而它在两种实现下
    /// 都绿 —— 规则被写下、被相信、且没有被测。变异验证抓出来的。
    #[test]
    fn a_replay_reports_its_own_head_even_after_the_store_moved_on() {
        let store = TotalStore::open_in_memory().unwrap();
        store
            .apply_projection(batch(Projection::Append, 0, "v1", None))
            .unwrap();

        let first = token_for("fp-first");
        store
            .apply_projection(batch(Projection::Rollback, 0, "v2", Some(first.clone())))
            .unwrap();
        assert_eq!(head_of_store(&store), (1, 0));

        // 又一次**真实的**重写把库推到了下一代。
        store
            .apply_projection(batch(
                Projection::Rollback,
                0,
                "v3",
                Some(token_for("fp-second")),
            ))
            .unwrap();
        assert_eq!(head_of_store(&store), (2, 0), "前提：库已经往前走了");

        // 现在重放第一个 token（崩溃恢复的典型形状）。
        let replay = store
            .apply_projection(batch(Projection::Rollback, 0, "v2", Some(first)))
            .unwrap();
        assert_eq!(
            (replay.source_revision, replay.projection_revision),
            (1, 0),
            "要报**它自己那次**的头，不是当前头 —— 后者会让调用方拿着别人的版本号去 ack"
        );
        assert_eq!(
            head_of_store(&store),
            (2, 0),
            "重放不得把库拉回去，也不得再推进"
        );
    }

    /// token 与事件写在同一事务：短路返回的头必须真的存在于库里。
    ///
    /// ⚠️ 这条**只**钉「记录的头不是凭空的」；「不是当前头」由上一条钉
    /// —— 本例中间没有别的操作，两者恰好相等，所以它证明不了那一半。
    #[test]
    fn the_recorded_head_matches_what_the_store_actually_has() {
        let store = TotalStore::open_in_memory().unwrap();
        store
            .apply_projection(batch(Projection::Append, 0, "v1", None))
            .unwrap();
        let t = token_for("fp-x");
        store
            .apply_projection(batch(Projection::Rollback, 0, "v2", Some(t.clone())))
            .unwrap();
        let replay = store
            .apply_projection(batch(Projection::Rollback, 0, "v2", Some(t)))
            .unwrap();
        assert_eq!(
            (replay.source_revision, replay.projection_revision),
            head_of_store(&store),
            "短路返回的头要与库里的当前头一致（本例中间没有别的操作）"
        );
    }

    /// 台账记下每份投影**为什么**存在。留存策略跟着理由走（ADR-044 决定 7），
    /// 所以理由必须是持久化的事实，不能事后从编号反推 —— 反推正是本次事故的形状。
    #[test]
    fn every_projection_records_why_it_exists() {
        let store = TotalStore::open_in_memory().unwrap();
        let ev = |seq: u64| mk_event(seq, "s", Some("x"));
        store.append_events(&[ev(0)], Projection::Append).unwrap();
        store.append_events(&[ev(0)], Projection::Reparse).unwrap();
        store.append_events(&[ev(0)], Projection::Rollback).unwrap();

        let conn = store.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT source_revision, projection_revision, origin FROM projections
                  ORDER BY source_revision, projection_revision",
            )
            .unwrap();
        let rows: Vec<(i64, i64, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(
            rows,
            vec![
                // `append` 那份**已被 reparse 取代并删除**（ADR-044 决定 2）——
                // 台账与行一起走，不留悬空的账。
                (0, 1, "reparse".to_string()),
                // `rollback` 开的是新**源版本**：前一个源版本是磁盘上已消失内容的
                // 唯一副本，所以 reparse 那份仍在。
                (1, 0, "rollback".to_string()),
            ]
        );
    }

    /// 🔴 迁移的核心承诺：**既有行照常解得开、`read_session` 的答案不变**。
    ///
    /// 数据模型分层与行为变更分两步走，各自可独立回退。这条钉住第一步没有偷偷做第二步。
    ///
    /// 它同时钉住 `aad_version = 1` 这个设计的全部理由：`generation` 被编进了 AES-GCM 的
    /// AAD，而 AAD 是长度前缀拼接的，字段一变既有密文全部解不开。迁移把
    /// `source_revision` 取为 `generation` 的原值、并把行标成 v1，于是 AAD 字节逐字节
    /// 相同 —— 实机 1.66 GB 密文一行都不用重加密。若这条前提被破坏，下面的
    /// `read_session` 会返回**空**（而不是报错），是一个静默的、全库规模的数据损坏。
    ///
    /// 🔴 fixture 必须是**真正的迁移前形态**：旧 schema（只有 `generation`）+ **v1 密文**。
    /// 第一版图省事，用新代码写出 v2 密文再把表「降级」成旧 schema —— 那种库现实中不
    /// 存在，测出来的 `Crypto(Decrypt)` 只是 fixture 自己造的假象。
    #[test]
    fn migration_from_generation_store_keeps_events_readable() {
        let dir = std::env::temp_dir().join(format!(
            "sv_srev_mig_{}_{}",
            std::process::id(),
            now_unix_secs()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("total_store.db");
        let key = || StoreKey::from_bytes([7u8; 32]);

        // 两代同一个 session：gen 0 是旧解析，gen 1 取代它。迁移后 `read_session`
        // 必须只看到 gen 1 那条 —— 与迁移前 `MAX(generation)` 的语义完全一致。
        let rows = [(0i64, "old-parse"), (1i64, "new-parse")];
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                r#"CREATE TABLE raw_events (
                       offset            INTEGER PRIMARY KEY AUTOINCREMENT,
                       ingested_at       INTEGER NOT NULL,
                       schema_version    INTEGER NOT NULL,
                       source_type       TEXT    NOT NULL,
                       source_location   TEXT    NOT NULL,
                       source_path       TEXT    NOT NULL,
                       source_session_id TEXT    NOT NULL,
                       seq               INTEGER NOT NULL,
                       generation        INTEGER NOT NULL DEFAULT 0,
                       event_type        TEXT    NOT NULL,
                       occurred_at       TEXT,
                       project_root      TEXT,
                       event_json        TEXT    NOT NULL,
                       UNIQUE (source_type, source_location, source_path,
                               source_session_id, seq, generation)
                   );"#,
            )
            .unwrap();
            for (generation, body) in rows {
                let event = mk_event(0, "s", Some(body));
                // v1 AAD：末位是 generation，没有 projection_revision。
                let aad = event_aad(
                    1,
                    source_type_key(event.source_type),
                    &event.source_location.as_key(),
                    &event.source_path,
                    &event.source_session_id,
                    event.seq as i64,
                    generation,
                    0,
                );
                let envelope = StoreCipher::new(key())
                    .encrypt(serde_json::to_string(&event).unwrap().as_bytes(), &aad)
                    .unwrap();
                conn.execute(
                    r#"INSERT INTO raw_events
                         (ingested_at, schema_version, source_type, source_location, source_path,
                          source_session_id, seq, generation, event_type, occurred_at,
                          project_root, event_json)
                       VALUES (0, 1, 'claude_code', 'local', '/p/file.jsonl', 's', 0, ?1,
                               'message', NULL, NULL, ?2)"#,
                    params![generation, envelope],
                )
                .unwrap();
            }
        }

        let store = TotalStore::open_with_key(&path, key()).unwrap();

        // ① 两行都还在（迁移不删数据）。
        let total: i64 = store
            .conn
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM raw_events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(total, 2, "迁移不得删除任何行");

        // ② 答案不变：只看到当前投影那一条，且**解得开**。
        let read = store
            .read_session(
                SourceType::ClaudeCode,
                &SourceLocation::Local,
                "/p/file.jsonl",
                "s",
            )
            .unwrap();
        assert_eq!(
            read.events.len(),
            1,
            "read_session 应只返回当前投影；返回 0 通常意味着 AAD 变了、既有密文解不开"
        );
        assert_eq!(read.events[0].content.as_deref(), Some("new-parse"));

        // ③ 保守映射：每个旧代成为独立的**源版本**，且理由标为 unknown ——
        //    当时只有一个整数，无从判断当初是回退还是重解析，一律按「可能是唯一副本」
        //    处理，永不进通用 GC（ADR-044 决定 7）。
        {
            let conn = store.conn.lock().unwrap();
            let mut stmt = conn
                .prepare(
                    "SELECT source_revision, projection_revision, origin FROM projections
                      ORDER BY source_revision",
                )
                .unwrap();
            let ledger: Vec<(i64, i64, String)> = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .unwrap()
                .map(Result::unwrap)
                .collect();
            assert_eq!(
                ledger,
                vec![(0, 0, "unknown".to_string()), (1, 0, "unknown".to_string()),]
            );
            assert_eq!(
                head_of(&conn, "claude_code", "local", "/p/file.jsonl").unwrap(),
                (1, 0),
                "头应指向原 MAX(generation) 那一代"
            );
        }

        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 🔴 ADR-044 决定 2 / 护栏 G2：**`Reparse` 带空批，当前投影就变成空的。**
    ///
    /// 这是 `apply_projection` 存在的首要理由。`append_events` 从第一条事件反推作用域，
    /// 空批既拿不到作用域也做不了任何事 —— 于是「新解析器合法地对这个文件产出零事件」
    /// 无法表达。QuotaBar 侧曾因此把空批直接记成「已重投影」并推进 `parser_revision`，
    /// 旧投影却原样留着，永久残留且不再有重试机会。
    ///
    /// 断言用 `read_session` 的返回，不是内部状态：用户看到的是「这个会话现在没有内容」，
    /// 而不是「头指向了 (0, 1)」。
    #[test]
    fn a_reparse_with_no_events_empties_the_current_projection() {
        let store = TotalStore::open_in_memory().unwrap();
        let source = SourceKey {
            source_type: SourceType::ClaudeCode,
            source_location: SourceLocation::Local,
            source_path: "/p/file.jsonl".to_string(),
        };
        let read = |s: &TotalStore| {
            s.read_session(
                SourceType::ClaudeCode,
                &SourceLocation::Local,
                "/p/file.jsonl",
                "s",
            )
            .unwrap()
            .events
            .len()
        };

        store
            .apply_projection(FileProjectionBatch {
                source: source.clone(),
                parser_revision: Some(1),
                mode: Projection::Append,
                token: None,
                events: vec![mk_event(0, "s", Some("v1"))],
            })
            .unwrap();
        assert_eq!(read(&store), 1);

        let stats = store
            .apply_projection(FileProjectionBatch {
                source: source.clone(),
                parser_revision: Some(2),
                mode: Projection::Reparse,
                token: None,
                events: vec![],
            })
            .unwrap();

        assert!(stats.head_moved, "空的重投影也必须切头");
        assert_eq!((stats.source_revision, stats.projection_revision), (0, 1));
        assert_eq!(
            read(&store),
            0,
            "当前投影是空的；仍返回旧事件说明头退回了上一份非空投影"
        );

        // 旧投影的行还在（本步不删数据），只是不再是当前的。
        let total: i64 = store
            .conn
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM raw_events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(total, 1, "旧投影留存 —— 回收是第 7 步的事，不是这一步");
    }

    /// `Append` 带空批是**真的无事可做** —— 不切头、不记账。
    ///
    /// 区分「替换为空」与「无事可做」的是 `mode`，不是批的长度。增量扫描每一轮都会对
    /// 几百个未变动的文件产出空批，若它们也切头，头会被无谓地推着走。
    #[test]
    fn an_append_with_no_events_changes_nothing() {
        let store = TotalStore::open_in_memory().unwrap();
        let source = SourceKey {
            source_type: SourceType::ClaudeCode,
            source_location: SourceLocation::Local,
            source_path: "/p/file.jsonl".to_string(),
        };
        store
            .apply_projection(FileProjectionBatch {
                source: source.clone(),
                parser_revision: Some(1),
                mode: Projection::Append,
                token: None,
                events: vec![mk_event(0, "s", Some("v1"))],
            })
            .unwrap();

        let stats = store
            .apply_projection(FileProjectionBatch {
                source,
                parser_revision: Some(1),
                mode: Projection::Append,
                token: None,
                events: vec![],
            })
            .unwrap();

        assert!(!stats.head_moved, "空的增量不该切头");
        assert_eq!((stats.source_revision, stats.projection_revision), (0, 0));
    }

    /// 🔴 批里混进别的文件的事件 → **拒绝**，且什么都不写。
    ///
    /// 从前作用域是从第一条事件反推的，所以这种情况根本无法被发现：混进来的事件会被
    /// 安静地写进第一条事件那个文件的投影里。作用域显式之后它变成一个可以拒绝的错误。
    ///
    /// 同时断言「什么都没写」——校验必须在动库之前，否则会留下半批。
    #[test]
    fn a_batch_carrying_another_files_event_is_rejected_whole() {
        let store = TotalStore::open_in_memory().unwrap();
        let err = store
            .apply_projection(FileProjectionBatch {
                source: SourceKey {
                    source_type: SourceType::ClaudeCode,
                    source_location: SourceLocation::Local,
                    source_path: "/p/a.jsonl".to_string(),
                },
                parser_revision: Some(1),
                mode: Projection::Append,
                token: None,
                events: vec![
                    mk_event_at(0, "s", "/p/a.jsonl"),
                    mk_event_at(1, "s", "/p/b.jsonl"),
                ],
            })
            .unwrap_err();

        assert!(
            matches!(err, StoreError::ForeignEvent { .. }),
            "应为 ForeignEvent，实得 {err:?}"
        );
        assert_eq!(
            store.status().unwrap().count,
            0,
            "校验必须在动库之前 —— 否则会留下半批"
        );
    }

    /// 台账记下产出这份投影的解析器版本。回收的判据是「同一源版本上有更新的投影」，
    /// 而「更新」得有个可比的东西 —— 那就是这一列（ADR-044 决定 7 的前置）。
    #[test]
    fn the_ledger_records_which_parser_produced_the_projection() {
        let store = TotalStore::open_in_memory().unwrap();
        let source = SourceKey {
            source_type: SourceType::ClaudeCode,
            source_location: SourceLocation::Local,
            source_path: "/p/file.jsonl".to_string(),
        };
        for (parser_revision, mode) in [(1u32, Projection::Append), (2, Projection::Reparse)] {
            store
                .apply_projection(FileProjectionBatch {
                    source: source.clone(),
                    parser_revision: Some(parser_revision),
                    mode,
                    token: None,
                    events: vec![mk_event(0, "s", Some("x"))],
                })
                .unwrap();
        }

        let conn = store.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT projection_revision, parser_revision, origin FROM projections
                  ORDER BY projection_revision",
            )
            .unwrap();
        let rows: Vec<(i64, Option<i64>, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(
            rows,
            vec![(1, Some(2), "reparse".to_string())],
            "被取代的那份投影连同它的台账一起消失 —— 留着台账而删了行，会造出一条             指向不存在数据的记录"
        );
    }

    /// 🔴 **护栏 G6（ADR-044 决定 5）：全量重扫之后，最近的会话仍在「最近 N」里。**
    ///
    /// fixture 刻意让 **offset 顺序与时间顺序相反** —— 先写「新会话」再写「旧会话」，
    /// 于是按 offset 取最近 N 会取到旧的那个。不这么造，两种实现给出同样答案、测试恒绿
    /// （这是本仓反复踩过的形状）。
    ///
    /// 这正是实机上发生过的事：一次重投影按文件遍历顺序重写全库，「最近 5 万条」的
    /// offset 窗口因此横跨九个多月，而当天的会话被挤了出去。
    #[test]
    fn recency_follows_event_time_not_write_order() {
        let store = TotalStore::open_in_memory().unwrap();
        let at = |path: &str, sid: &str, ts: &str| {
            let mut ev = mk_event_at(0, sid, path);
            ev.occurred_at = Some(ts.to_string());
            store.append_events(&[ev], Projection::Append).unwrap();
        };
        // 写入顺序：新 → 旧。offset 因此与时间**相反**。
        at("/p/new.jsonl", "recent", "2026-08-05T10:00:00Z");
        at("/p/old.jsonl", "ancient", "2025-01-01T00:00:00Z");

        let by_offset_last: String = {
            let conn = store.conn.lock().unwrap();
            conn.query_row(
                "SELECT source_session_id FROM raw_events ORDER BY offset DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(
            by_offset_last, "ancient",
            "fixture 前提：按 offset 取「最近」会取到最旧的那个；否则本条恒真"
        );

        let recent = store.recent_sessions(1, None).unwrap();
        assert_eq!(recent.len(), 1);
        assert_eq!(
            recent[0].session_id, "recent",
            "最近 N 必须按 occurred_at 排；取到 ancient 说明又用回了写入顺序"
        );
    }

    /// 同一时刻的两种时区写法排在一起 —— 字典序会把它们排开八小时。
    #[test]
    fn recency_compares_instants_not_strings() {
        let store = TotalStore::open_in_memory().unwrap();
        let at = |path: &str, sid: &str, ts: &str| {
            let mut ev = mk_event_at(0, sid, path);
            ev.occurred_at = Some(ts.to_string());
            store.append_events(&[ev], Projection::Append).unwrap();
        };
        // b 比 a 晚一小时，但它的**字符串**更小。
        at("/p/a.jsonl", "a", "2026-08-05T09:00:00+08:00"); // = 01:00Z
        at("/p/b.jsonl", "b", "2026-08-05T02:00:00Z");
        assert!(
            "2026-08-05T02:00:00Z" < "2026-08-05T09:00:00+08:00",
            "fixture 前提：晚发生的那个字符串更小 —— 字典序会把它们排反"
        );

        let recent = store.recent_sessions(2, None).unwrap();
        assert_eq!(
            recent
                .iter()
                .map(|r| r.session_id.as_str())
                .collect::<Vec<_>>(),
            vec!["b", "a"],
            "b（02:00Z）晚于 a（01:00Z）；顺序反了说明在比字符串而不是比时刻"
        );
    }

    /// 没有可解析时间的会话**排在末尾并照常返回** —— 不是被静默丢掉。
    ///
    /// 「没有时间」是一个需要被看见的事实：消费者得知道有这么个会话、但不知道它什么
    /// 时候发生，而不是发现它凭空消失。
    #[test]
    fn sessions_without_time_sort_last_but_are_not_dropped() {
        let store = TotalStore::open_in_memory().unwrap();
        let mut timed = mk_event_at(0, "timed", "/p/a.jsonl");
        timed.occurred_at = Some("2020-01-01T00:00:00Z".into());
        store.append_events(&[timed], Projection::Append).unwrap();
        let mut untimed = mk_event_at(0, "untimed", "/p/b.jsonl");
        untimed.occurred_at = None;
        store.append_events(&[untimed], Projection::Append).unwrap();

        let recent = store.recent_sessions(10, None).unwrap();
        let ids: Vec<&str> = recent.iter().map(|r| r.session_id.as_str()).collect();
        assert_eq!(ids, vec!["timed", "untimed"], "无时间的排最后");
        assert!(recent[0].has_time());
        assert!(
            !recent[1].has_time(),
            "无时间必须能被识别 —— 它是**未知**，不是**很旧**"
        );
    }

    /// 🔴 **D1：`read_current_since_page` 只发当前投影；旧的 `read_since_page` 保持原样。**
    ///
    /// 两条一起断言。只测新接口过滤对了，证明不了旧接口没被静默改掉 —— 而给一个已发布
    /// CLI 换掉返回内容，按旧语义写的消费者不会报错，只会悄悄拿到不同的数据。
    #[test]
    fn the_current_projection_read_filters_superseded_rows_while_the_legacy_one_does_not() {
        // 🔴 用 `Rollback` 而不是 `Reparse` 造「两代并存」。
        //
        // 决定 2 落地后，`Reparse` 会**删掉**被取代的投影，所以它造不出并存状态了 ——
        // 用它当 fixture，这条测试会变成一条恒真的空断言。而 `Rollback` 的旧版本是
        // 已消失内容的唯一副本，永远留存，正是「并存」的真实来源。
        let store = TotalStore::open_in_memory().unwrap();
        store
            .append_events(
                &[mk_event(0, "s", Some("before-rewrite"))],
                Projection::Append,
            )
            .unwrap();
        store
            .append_events(
                &[mk_event(0, "s", Some("after-rewrite"))],
                Projection::Rollback,
            )
            .unwrap();

        let current = store.read_current_since_page(0, 100).unwrap();
        assert_eq!(current.events.len(), 1, "当前投影只有一条");
        assert_eq!(
            current.events[0].1.content.as_deref(),
            Some("after-rewrite")
        );

        let legacy = store.read_since_page(0, 100).unwrap();
        assert_eq!(
            legacy.events.len(),
            2,
            "旧接口必须原样返回两个源版本 —— 静默改变它的语义比不修 D1 更糟"
        );
    }

    /// 🔴 **护栏 G7（ADR-044 决定 1/2）：`Reparse` 取代旧投影，`Rollback` 保留旧版本。**
    ///
    /// 两条**互为镜像**，必须一起断言。只测其中一条，把另一条写成同样的语义也照样绿 ——
    /// 而它们的留存价值恰好相反：
    ///
    /// - `Reparse` 的旧投影是同一批字节的更差解析，可再生 → 删；
    /// - `Rollback` 的旧版本是磁盘上**已消失内容的唯一副本** → 留。
    #[test]
    fn reparse_replaces_while_rollback_retains() {
        let rows = |s: &TotalStore| -> i64 {
            s.conn
                .lock()
                .unwrap()
                .query_row("SELECT COUNT(*) FROM raw_events", [], |r| r.get(0))
                .unwrap()
        };

        let store = TotalStore::open_in_memory().unwrap();
        store
            .append_events(
                &[mk_event(0, "s", Some("v1")), mk_event(1, "s", Some("v1b"))],
                Projection::Append,
            )
            .unwrap();
        assert_eq!(rows(&store), 2);

        let stats = store
            .append_events(
                &[mk_event(0, "s", Some("v2")), mk_event(1, "s", Some("v2b"))],
                Projection::Reparse,
            )
            .unwrap();
        assert_eq!(rows(&store), 2, "Reparse 取代 —— 总行数不增长");
        assert_eq!(stats.appended, 2);

        let store2 = TotalStore::open_in_memory().unwrap();
        store2
            .append_events(&[mk_event(0, "s", Some("v1"))], Projection::Append)
            .unwrap();
        store2
            .append_events(&[mk_event(0, "s", Some("rewritten"))], Projection::Rollback)
            .unwrap();
        assert_eq!(rows(&store2), 2, "Rollback 保留 —— 旧版本是唯一副本");
    }

    /// 🔴 **解析器升级带来的存储增长必须是零** —— 这是决定 2 的全部意义。
    ///
    /// 实机上一次重投影把总库从 1.54 GB 推到 2.72 GB，并承诺「每次 PARSER_REVISION
    /// 升版再攒一份」。断言「连续多次重投影后行数不变」，而不是断言某个具体数字：
    /// 前者说的是**无界增长被消除了**，后者只是一次快照。
    #[test]
    fn repeated_reparses_do_not_accumulate() {
        let store = TotalStore::open_in_memory().unwrap();
        let batch = |tag: &str| {
            vec![
                mk_event(0, "s", Some(tag)),
                mk_event(1, "s", Some(tag)),
                mk_event(2, "s", Some(tag)),
            ]
        };
        store
            .append_events(&batch("v1"), Projection::Append)
            .unwrap();
        let baseline = store.status().unwrap().count;
        for tag in ["v2", "v3", "v4", "v5"] {
            store
                .append_events(&batch(tag), Projection::Reparse)
                .unwrap();
        }
        assert_eq!(
            store.status().unwrap().count,
            baseline,
            "四次重投影之后行数必须不变；增长了说明旧投影仍在累积"
        );
    }

    /// 🔴 **丢事件时：头照切，旧投影不删。**
    ///
    /// ADR 原本写作「拒绝替换」，而实现时发现那句话内部矛盾：一个**合法产出零事件**的
    /// 新解析器与一次**解析器退化**，在「事件变少」这个观测上完全一样。按字面拒绝，
    /// 前者就永远做不成 —— 库会一直服务旧解析，正是本 ADR 要治的病。
    ///
    /// 拆开之后两个目标都满足：当前答案是最新那份解析（正确），不可逆的删除在可疑时
    /// 不做（可恢复）。这条同时断言两半 —— 只测一半，另一半可以被写反而不被发现。
    #[test]
    fn a_shrinking_reparse_switches_the_head_but_keeps_the_old_rows() {
        let store = TotalStore::open_in_memory().unwrap();
        store
            .append_events(
                &[mk_event(0, "s", Some("a")), mk_event(1, "s", Some("b"))],
                Projection::Append,
            )
            .unwrap();

        // 用 `apply_projection`：兼容层 `append_events` 返回的 `AppendStats` 里没有
        // `loses_events` / `head_moved` —— 而这条测试要断言的正是那两个。
        let stats = store
            .apply_projection(FileProjectionBatch {
                source: SourceKey {
                    source_type: SourceType::ClaudeCode,
                    source_location: SourceLocation::Local,
                    source_path: "/p/file.jsonl".to_string(),
                },
                parser_revision: Some(2),
                mode: Projection::Reparse,
                token: None,
                events: vec![mk_event(0, "s", Some("only-a"))],
            })
            .unwrap();

        assert_eq!(stats.loses_events, Some((2, 1)), "必须如实上报「变少了」");
        assert_eq!(stats.superseded_removed, 0, "可疑时不做不可逆的那一步");
        assert!(stats.head_moved, "但头照切 —— 当前答案必须是最新那份解析");

        let read = store
            .read_session(
                SourceType::ClaudeCode,
                &SourceLocation::Local,
                "/p/file.jsonl",
                "s",
            )
            .unwrap();
        assert_eq!(read.events.len(), 1);
        assert_eq!(
            read.events[0].content.as_deref(),
            Some("only-a"),
            "当前答案是新解析；返回旧内容说明头没切，库还在服务已被取代的解析"
        );

        let total: i64 = store
            .conn
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM raw_events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(total, 3, "旧投影的两行仍在 —— 退化可恢复");
    }

    /// 空的重投影是「丢事件」的极端情形：头切到空投影，旧行留着。
    ///
    /// 这条与 `a_reparse_with_no_events_empties_the_current_projection` 互补：那条说
    /// 「当前投影变空了」，这条说「变空的同时旧数据没被销毁」。
    #[test]
    fn an_empty_reparse_is_the_extreme_case_of_shrinking() {
        let store = TotalStore::open_in_memory().unwrap();
        let source = SourceKey {
            source_type: SourceType::ClaudeCode,
            source_location: SourceLocation::Local,
            source_path: "/p/file.jsonl".to_string(),
        };
        store
            .apply_projection(FileProjectionBatch {
                source: source.clone(),
                parser_revision: Some(1),
                mode: Projection::Append,
                token: None,
                events: vec![mk_event(0, "s", Some("v1"))],
            })
            .unwrap();
        let stats = store
            .apply_projection(FileProjectionBatch {
                source,
                parser_revision: Some(2),
                mode: Projection::Reparse,
                token: None,
                events: vec![],
            })
            .unwrap();
        assert_eq!(stats.loses_events, Some((1, 0)));
        assert_eq!(stats.superseded_removed, 0);
        assert!(stats.head_moved);
    }

    /// 🔴 **护栏 G8（ADR-044 决定 7）：GC 只碰「已被取代且来源明确」的投影。**
    ///
    /// 三类并存，只有一类该被回收 —— 这条测试的价值全在**另外两类必须活下来**：
    ///
    /// - `reparse` 且非当前 → 回收（同一批字节的更差解析，可再生）；
    /// - `rollback`         → **保留**（磁盘上已消失内容的唯一副本）；
    /// - `unknown`          → **保留**（ADR-044 之前产生，无从判断当初是哪一种）。
    ///
    /// 首稿计划里的「删除每个文件的非当前代」正是会把后两类一并删掉的形状。
    #[test]
    fn gc_spares_rollback_history_and_rows_of_unknown_origin() {
        let store = TotalStore::open_in_memory().unwrap();
        let at = |path: &str| SourceKey {
            source_type: SourceType::ClaudeCode,
            source_location: SourceLocation::Local,
            source_path: path.to_string(),
        };
        let put = |src: &SourceKey, mode: Projection, tag: &str| {
            let mut ev = mk_event_at(0, "s", &src.source_path);
            ev.content = Some(tag.to_string());
            store
                .apply_projection(FileProjectionBatch {
                    source: src.clone(),
                    parser_revision: Some(1),
                    mode,
                    token: None,
                    events: vec![ev],
                })
                .unwrap()
        };
        let count = |s: &TotalStore| -> i64 {
            s.conn
                .lock()
                .unwrap()
                .query_row("SELECT COUNT(*) FROM raw_events", [], |r| r.get(0))
                .unwrap()
        };

        // ① 一个走过 rollback 的文件：旧源版本必须留下。
        let rolled = at("/p/rolled.jsonl");
        put(&rolled, Projection::Append, "before-rewrite");
        put(&rolled, Projection::Rollback, "after-rewrite");

        // ② 「来源不明」的旧代 —— 直接造出 ADR-044 之前的形态：有行、台账标 unknown、
        //    且不是当前头。迁移正是这么标的。
        // ③ 一份历史遗留的、被取代的 reparse 投影 —— 决定 2 之后不再产生新的，
        //    GC 存在的意义正是清扫此前攒下的。
        {
            let conn = store.conn.lock().unwrap();
            conn.execute_batch(
                r#"
                INSERT INTO projections (source_type, source_location, source_path,
                    source_revision, projection_revision, parser_revision, origin, created_at)
                VALUES ('claude_code','local','/p/rolled.jsonl',9,0,NULL,'unknown',0);
                INSERT INTO raw_events (ingested_at, schema_version, source_type,
                    source_location, source_path, source_session_id, seq, source_revision,
                    projection_revision, aad_version, event_type, occurred_at,
                    occurred_at_unix_ms, project_root, event_json)
                VALUES (0,1,'claude_code','local','/p/rolled.jsonl','s',0,9,0,1,
                        'message',NULL,NULL,NULL,'sv2:unknown-origin');

                INSERT INTO projections (source_type, source_location, source_path,
                    source_revision, projection_revision, parser_revision, origin, created_at)
                VALUES ('claude_code','local','/p/rolled.jsonl',0,7,1,'reparse',0);
                INSERT INTO raw_events (ingested_at, schema_version, source_type,
                    source_location, source_path, source_session_id, seq, source_revision,
                    projection_revision, aad_version, event_type, occurred_at,
                    occurred_at_unix_ms, project_root, event_json)
                VALUES (0,1,'claude_code','local','/p/rolled.jsonl','s',0,0,7,1,
                        'message',NULL,NULL,NULL,'sv2:superseded');
                "#,
            )
            .unwrap();
        }

        let before = count(&store);

        // dry-run 先看一眼：只统计，一行不动。
        let plan = store.gc_superseded_projections(true).unwrap();
        assert_eq!(plan.projections, 1, "只有那一份 reparse 旧投影是候选");
        assert_eq!(plan.events, 1);
        assert!(plan.dry_run);
        assert_eq!(count(&store), before, "dry-run 不许改任何东西");

        let done = store.gc_superseded_projections(false).unwrap();
        assert_eq!((done.projections, done.events), (1, 1));

        let survivors: Vec<(String, i64, i64)> = {
            let conn = store.conn.lock().unwrap();
            let mut stmt = conn
                .prepare(
                    "SELECT origin, source_revision, projection_revision FROM projections
                      ORDER BY source_revision, projection_revision",
                )
                .unwrap();
            let rows = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .unwrap();
            rows.map(Result::unwrap).collect()
        };
        let origins: Vec<&str> = survivors.iter().map(|(o, _, _)| o.as_str()).collect();
        assert!(
            origins.contains(&"rollback"),
            "rollback 历史被删了 —— 那是磁盘上已消失内容的唯一副本。实得 {survivors:?}"
        );
        assert!(
            origins.contains(&"unknown"),
            "来源不明的旧代被删了 —— 无从判断它当初是回退还是重解析，只能保守保留。实得 {survivors:?}"
        );
        assert!(
            !survivors
                .iter()
                .any(|(o, sr, pr)| o == "reparse" && (*sr, *pr) == (0, 7)),
            "被取代的 reparse 投影应已回收"
        );
    }

    /// 当前投影永远不进候选 —— 哪怕它的 origin 是 `reparse`。
    ///
    /// 单独一条，因为「只回收非当前的」这个条件最容易在重写 SQL 时丢掉，而丢掉之后的
    /// 表现是**当前数据被删**，不是报错。
    #[test]
    fn gc_never_touches_the_current_projection() {
        let store = TotalStore::open_in_memory().unwrap();
        let source = SourceKey {
            source_type: SourceType::ClaudeCode,
            source_location: SourceLocation::Local,
            source_path: "/p/file.jsonl".to_string(),
        };
        for (mode, tag) in [(Projection::Append, "v1"), (Projection::Reparse, "v2")] {
            store
                .apply_projection(FileProjectionBatch {
                    source: source.clone(),
                    parser_revision: Some(1),
                    mode,
                    token: None,
                    events: vec![mk_event(0, "s", Some(tag))],
                })
                .unwrap();
        }
        // 当前头是那份 reparse。
        let stats = store.gc_superseded_projections(false).unwrap();
        assert_eq!(stats.projections, 0, "当前投影不是候选");
        let read = store
            .read_session(
                SourceType::ClaudeCode,
                &SourceLocation::Local,
                "/p/file.jsonl",
                "s",
            )
            .unwrap();
        assert_eq!(read.events.len(), 1);
        assert_eq!(read.events[0].content.as_deref(), Some("v2"));
    }

    /// 🔴 **迁移之后索引必须还在**（评审 [P2]）。
    ///
    /// SQLite 的陷阱：`ALTER TABLE … RENAME` 把索引一起带到旧表名下、**索引名不变**；
    /// 随后 `CREATE INDEX IF NOT EXISTS` 因为同名索引仍存在而**静默跳过**；最后
    /// `DROP TABLE` 把它们一并删掉。净效果是迁移后一个索引都没有 —— 而这不报错，
    /// 只是会话/项目查询与 erase 全部退化成全表扫。
    ///
    /// 断言索引**存在且绑在新表上**，不只是「名字还在」：名字可能还挂在残骸上。
    #[test]
    fn migration_keeps_the_indexes_attached_to_the_new_table() {
        let dir =
            std::env::temp_dir().join(format!("sv_idx_{}_{}", std::process::id(), now_unix_secs()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("total_store.db");

        // 造一个「只有 generation」的旧库，带着当时的两个索引。
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                r#"CREATE TABLE raw_events (
                       offset            INTEGER PRIMARY KEY AUTOINCREMENT,
                       ingested_at       INTEGER NOT NULL,
                       schema_version    INTEGER NOT NULL,
                       source_type       TEXT    NOT NULL,
                       source_location   TEXT    NOT NULL,
                       source_path       TEXT    NOT NULL,
                       source_session_id TEXT    NOT NULL,
                       seq               INTEGER NOT NULL,
                       generation        INTEGER NOT NULL DEFAULT 0,
                       event_type        TEXT    NOT NULL,
                       occurred_at       TEXT,
                       project_root      TEXT,
                       event_json        TEXT    NOT NULL,
                       UNIQUE (source_type, source_location, source_path,
                               source_session_id, seq, generation)
                   );
                   CREATE INDEX idx_raw_events_session ON raw_events(source_session_id);
                   CREATE INDEX idx_raw_events_project ON raw_events(project_root);"#,
            )
            .unwrap();
        }

        let store = TotalStore::open_with_key(&path, StoreKey::from_bytes([3u8; 32])).unwrap();
        let attached: Vec<(String, String)> = {
            let conn = store.conn.lock().unwrap();
            let mut stmt = conn
                .prepare(
                    "SELECT name, tbl_name FROM sqlite_master
                      WHERE type = 'index' AND name LIKE 'idx_raw_events_%' ORDER BY name",
                )
                .unwrap();
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
            rows.map(Result::unwrap).collect()
        };
        let names: Vec<&str> = attached.iter().map(|(n, _)| n.as_str()).collect();
        for want in [
            "idx_raw_events_occurred",
            "idx_raw_events_project",
            "idx_raw_events_session",
        ] {
            assert!(
                names.contains(&want),
                "{want} 在迁移后消失了；相关查询与 erase 会退化成全表扫。实得 {attached:?}"
            );
        }
        assert!(
            attached.iter().all(|(_, tbl)| tbl == "raw_events"),
            "索引没绑在新表上：{attached:?}"
        );

        // 临时表不许留下 —— 留着说明重写没跑完，而数据在它里面就是不可见的。
        {
            let conn = store.conn.lock().unwrap();
            let leftover: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE name = 'raw_events_pre_srev'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(leftover, 0, "迁移残骸未清理");
        }

        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 🔴 上一轮迁移半途夭折留下的残骸必须被**搬回来**，而不是静默地永远不可见。
    ///
    /// 有了事务这不该发生，但「不该发生」不是「不会发生」—— 事务是新加的，磁盘上
    /// 可能已经存在旧代码留下的残骸。检测不到就等于数据丢了：新表是空的、库能正常
    /// 打开、一个错都不报。
    #[test]
    fn an_interrupted_migration_recovers_its_stranded_rows() {
        let dir = std::env::temp_dir().join(format!(
            "sv_orphan_{}_{}",
            std::process::id(),
            now_unix_secs()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("total_store.db");
        let key = || StoreKey::from_bytes([4u8; 32]);

        // 先正常建库写一条，拿到一份真实可解密的行。
        {
            let store = TotalStore::open_with_key(&path, key()).unwrap();
            store
                .append_events(&[mk_event(0, "s", Some("stranded"))], Projection::Append)
                .unwrap();
        }
        // 手工模拟「重写跑到一半被杀」：数据搬进临时表、新表清空。
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("ALTER TABLE raw_events RENAME TO raw_events_pre_srev;")
                .unwrap();
            conn.execute_batch(RAW_EVENTS_DDL).unwrap();
        }

        let store = TotalStore::open_with_key(&path, key()).unwrap();
        let read = store
            .read_session(
                SourceType::ClaudeCode,
                &SourceLocation::Local,
                "/p/file.jsonl",
                "s",
            )
            .unwrap();
        assert_eq!(
            read.events.len(),
            1,
            "残骸里的行没被搬回来 —— 库能正常打开、一个错不报，而数据从此不可见"
        );
        assert_eq!(read.events[0].content.as_deref(), Some("stranded"));

        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 🔴 **change-feed 必须能表达「消失」**（ADR-044 决定 6 / 评审 [P1]）。
    ///
    /// 只读「当前投影」不足以让消费者收敛：一次重投影把 `{A, B}` 换成 `{A}` 之后，
    /// 增量流最多重发 A，**没有任何记录要求删除已物化的 B** —— 消费者会永久保留一条
    /// 已经不存在的事件。仅靠逐事件 upsert 永远删不掉「不再出现」的东西。
    ///
    /// 这条测试正是那个场景：断言重投影产生了一条变更记录，且它带着 source 坐标 ——
    /// 消费者据此按 source **原子替换**，而不是比对事件。
    #[test]
    fn a_reparse_that_drops_an_event_still_tells_consumers_to_replace() {
        let store = TotalStore::open_in_memory().unwrap();
        let source = SourceKey {
            source_type: SourceType::ClaudeCode,
            source_location: SourceLocation::Local,
            source_path: "/p/file.jsonl".to_string(),
        };
        // 旧投影 {A, B}
        store
            .apply_projection(FileProjectionBatch {
                source: source.clone(),
                parser_revision: Some(1),
                mode: Projection::Append,
                token: None,
                events: vec![mk_event(0, "s", Some("A")), mk_event(1, "s", Some("B"))],
            })
            .unwrap();
        // 新投影只剩 {A}
        store
            .apply_projection(FileProjectionBatch {
                source,
                parser_revision: Some(2),
                mode: Projection::Reparse,
                token: None,
                events: vec![mk_event(0, "s", Some("A"))],
            })
            .unwrap();

        let changes = store.read_projection_changes(0, 100).unwrap();
        let replaced: Vec<&ProjectionChange> =
            changes.iter().filter(|c| c.reason == "reparse").collect();
        assert_eq!(
            replaced.len(),
            1,
            "重投影必须产生一条变更记录；没有它，消费者永远不知道 B 该删了"
        );
        let c = replaced[0];
        assert_eq!(c.source_path, "/p/file.jsonl");
        assert_eq!(
            (c.old_source_revision, c.old_projection_revision),
            (Some(0), Some(0)),
            "记录必须带上被取代的那一份 —— 消费者据此知道自己手里的是哪一版"
        );
        assert_eq!((c.new_source_revision, c.new_projection_revision), (0, 1));
    }

    /// 增量 append **不**产生变更记录 —— 头没动，没有任何东西需要被替换。
    ///
    /// 与上一条互为镜像。只测「替换会记」，把「不替换也记」写进去也照样绿，而那会让
    /// 消费者每一轮都做一次全量重建。
    #[test]
    fn plain_appends_do_not_enter_the_change_feed() {
        let store = TotalStore::open_in_memory().unwrap();
        let source = SourceKey {
            source_type: SourceType::ClaudeCode,
            source_location: SourceLocation::Local,
            source_path: "/p/file.jsonl".to_string(),
        };
        for seq in 0..3u64 {
            store
                .apply_projection(FileProjectionBatch {
                    source: source.clone(),
                    parser_revision: Some(1),
                    mode: Projection::Append,
                    token: None,
                    events: vec![mk_event(seq, "s", Some("x"))],
                })
                .unwrap();
        }
        let changes = store.read_projection_changes(0, 100).unwrap();
        assert!(
            changes.is_empty(),
            "增量不该进变更流。**首次建头也不该** —— 那时没有任何东西被替换，消费者             只需照常收事件；每轮都记会让它每轮做一次全量重建。实得 {changes:?}"
        );
    }

    /// 变更流的游标是它**自己的** `seq`，与 `raw_events.offset` 无关。
    ///
    /// 这条单独存在，因为「用 offset 当游标」正是这一整轮在修的病 —— 而重投影恰恰会
    /// 重铸 offset，用它当变更流的游标会让消费者要么重复处理、要么漏掉。
    #[test]
    fn the_change_feed_cursor_is_independent_of_event_offsets() {
        let store = TotalStore::open_in_memory().unwrap();
        let source = SourceKey {
            source_type: SourceType::ClaudeCode,
            source_location: SourceLocation::Local,
            source_path: "/p/file.jsonl".to_string(),
        };
        for (mode, tag) in [
            (Projection::Append, "v1"),
            (Projection::Reparse, "v2"),
            (Projection::Rollback, "v3"),
        ] {
            store
                .apply_projection(FileProjectionBatch {
                    source: source.clone(),
                    parser_revision: Some(1),
                    mode,
                    token: None,
                    events: vec![mk_event(0, "s", Some(tag))],
                })
                .unwrap();
        }
        let all = store.read_projection_changes(0, 100).unwrap();
        // Append 不记（头没动），Reparse 与 Rollback 各一条。
        assert_eq!(all.len(), 2, "实得 {all:?}");
        let seqs: Vec<i64> = all.iter().map(|c| c.seq).collect();
        assert!(seqs.windows(2).all(|w| w[0] < w[1]), "seq 必须严格递增");

        // 从中间续拉：只拿到之后的那些。
        let rest = store.read_projection_changes(seqs[0], 100).unwrap();
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0].seq, seqs[1]);
    }

    // ------------------------------------------------------------------
    // 迁移代价（2026-08-05 实测：2.8 GB / 93 万行真库）
    //
    // 这一组钉的不是「结果对不对」，而是「有没有在每次启动时把同一份答案重算一遍」。
    // 起因：`TotalStore::open` 在 QuotaBar 里是**同步的 setup hook**，首次迁移实测
    // 103 秒、此后每次启动仍白花 12–13 秒 —— 而这三处代价都不会报错，只会表现为
    // 「双击图标后很久什么都不出来」。
    // ------------------------------------------------------------------

    /// 造一个只有 `generation` 的旧库，带 `n` 条真实加密行（同一 session，逐代递增）。
    #[cfg(feature = "store")]
    fn legacy_generation_store(path: &std::path::Path, key: StoreKey, gens: &[(i64, &str)]) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            r#"CREATE TABLE raw_events (
                   offset            INTEGER PRIMARY KEY AUTOINCREMENT,
                   ingested_at       INTEGER NOT NULL,
                   schema_version    INTEGER NOT NULL,
                   source_type       TEXT    NOT NULL,
                   source_location   TEXT    NOT NULL,
                   source_path       TEXT    NOT NULL,
                   source_session_id TEXT    NOT NULL,
                   seq               INTEGER NOT NULL,
                   generation        INTEGER NOT NULL DEFAULT 0,
                   event_type        TEXT    NOT NULL,
                   occurred_at       TEXT,
                   project_root      TEXT,
                   event_json        TEXT    NOT NULL,
                   UNIQUE (source_type, source_location, source_path,
                           source_session_id, seq, generation)
               );"#,
        )
        .unwrap();
        let cipher = StoreCipher::new(key);
        for (generation, occurred_at) in gens {
            let event = mk_event(0, "s", Some("body"));
            let aad = event_aad(
                1,
                source_type_key(event.source_type),
                &event.source_location.as_key(),
                &event.source_path,
                &event.source_session_id,
                event.seq as i64,
                *generation,
                0,
            );
            let envelope = cipher
                .encrypt(serde_json::to_string(&event).unwrap().as_bytes(), &aad)
                .unwrap();
            conn.execute(
                r#"INSERT INTO raw_events
                     (ingested_at, schema_version, source_type, source_location, source_path,
                      source_session_id, seq, generation, event_type, occurred_at,
                      project_root, event_json)
                   VALUES (0, 1, 'claude_code', 'local', '/p/file.jsonl', 's', 0, ?1,
                           'message', ?2, NULL, ?3)"#,
                params![generation, occurred_at, envelope],
            )
            .unwrap();
        }
    }

    /// 迁移之后每一行都必须带上归一化时间戳 —— 缺了它，「最近」的排序会整段落空，
    /// 而 `recent_sessions` 只会返回空，不报错。
    ///
    /// 🔴 **覆盖边界明写：这条测不到「在哪一趟里算的」。** 把 `iso8601_to_unix_ms(...)`
    /// 从搬运那趟的 SELECT 里拿掉，后面那条兜底 `UPDATE` 会把同样的值补上 —— 结果逐字
    /// 相同，这条照样绿（实测变异 M1 如此）。它锁的是**结果在场**，不是代价。
    ///
    /// 而代价那一半是真的：`raw_events` 一行装着 `event_json`（真库平均约 3 KB），SQLite
    /// 改任意一列都要重写整条记录，93 万次 UPDATE ≈ 又一遍 2.8 GB 的随机顺序整表重写。
    /// 支撑它的是**测量**（2026-08-05：一趟 103s；分两趟时连 `current_head` 回填都跑不完），
    /// 不是这条测试。要把它也钉住，得让「兜底 UPDATE 改了几行」变成可观测量。
    #[test]
    #[cfg(feature = "store")]
    fn migration_leaves_every_row_with_a_normalized_timestamp() {
        let dir = std::env::temp_dir().join(format!(
            "sv_occ_mig_{}_{}",
            std::process::id(),
            now_unix_secs()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("total_store.db");
        let key = || StoreKey::from_bytes([11u8; 32]);
        legacy_generation_store(&path, key(), &[(0, "2026-06-01T10:00:00Z")]);

        let store = TotalStore::open_with_key(&path, key()).unwrap();
        let ms: Option<i64> = store
            .conn
            .lock()
            .unwrap()
            .query_row("SELECT occurred_at_unix_ms FROM raw_events", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            ms,
            crate::rawevent::occurred_at_unix_ms("2026-06-01T10:00:00Z"),
            "迁移后 occurred_at_unix_ms 还是空的 —— 「最近」的排序会整段落空，而这不报错"
        );

        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 🔴 `current_head` 必须是每个源文件里 `(source_revision, projection_revision)`
    /// **最大**的那一组 —— 与迁移前 `MAX(generation)` 的语义一致。
    ///
    /// 这条在窗口函数重写之后单独立起来：原先那版对外层每一行都跑一次关联子查询，
    /// 实测在真库上跑了 90 秒仍未出结果。改写只有在**答案不变**的前提下才算优化，
    /// 而「答案变了」的表现是读到一份被取代的旧解析，不是报错。
    #[test]
    #[cfg(feature = "store")]
    fn the_backfilled_head_is_the_highest_revision_of_each_source() {
        let dir = std::env::temp_dir().join(format!(
            "sv_head_bf_{}_{}",
            std::process::id(),
            now_unix_secs()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("total_store.db");
        let key = || StoreKey::from_bytes([12u8; 32]);
        // 三代：迁移把 generation 映射成 source_revision，projection_revision 恒 0。
        legacy_generation_store(
            &path,
            key(),
            &[
                (0, "2026-06-01T10:00:00Z"),
                (2, "2026-06-01T10:00:02Z"),
                (1, "2026-06-01T10:00:01Z"),
            ],
        );

        let store = TotalStore::open_with_key(&path, key()).unwrap();
        let head: (i64, i64) = store
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT source_revision, projection_revision FROM current_head",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            head,
            (2, 0),
            "头没取到最高代 —— 读到的会是一份已被取代的旧解析，且不报错"
        );

        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 🔴 回填是一次性的：标记落地之后，重开**不得**再重算一遍。
    ///
    /// 与上一条互为镜像。只测「结果对」的话，一个每次启动都重算的实现照样全绿 ——
    /// 而那正是被修掉的东西（93 万行上 `GROUP BY` 七列，实测 4.3s，每次启动）。
    /// 判据用「手动删掉一行头之后它不会被补回来」：能补回来就说明闸没起作用。
    #[test]
    #[cfg(feature = "store")]
    fn the_coordinate_backfill_does_not_run_again_once_marked() {
        let dir = std::env::temp_dir().join(format!(
            "sv_bf_once_{}_{}",
            std::process::id(),
            now_unix_secs()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("total_store.db");
        let key = || StoreKey::from_bytes([13u8; 32]);
        legacy_generation_store(&path, key(), &[(0, "2026-06-01T10:00:00Z")]);

        {
            let store = TotalStore::open_with_key(&path, key()).unwrap();
            let conn = store.conn.lock().unwrap();
            let marked: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM store_meta WHERE k = 'adr044_coords_backfilled'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(marked, 1, "第一次迁移必须留下完成标记，否则闸永远关不上");
            conn.execute("DELETE FROM current_head", []).unwrap();
        }

        let store = TotalStore::open_with_key(&path, key()).unwrap();
        let heads: i64 = store
            .conn
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM current_head", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            heads, 0,
            "头被重新算了一遍 —— 说明标记没被当闸读，每次启动都在 93 万行上重跑 GROUP BY"
        );

        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 🔴 信封迁移的完成标记同样必须被当闸读。
    ///
    /// 那条 `WHERE event_json NOT LIKE 'sv2:%'` 用不上索引且 SELECT 整行，在真库上是
    /// 每次启动数 GB 的扫描，只为得到「零行待迁移」这个上次就已经知道的答案。
    ///
    /// 跳过安全的前提是**读路径本来就认两种信封**（[`TotalStore::decode_event_on`]），
    /// 所以这条同时断言那条 v1 行仍然读得出来 —— 只测「没被重新封装」而不测「还能读」，
    /// 会把一个真正的数据不可用当成优化放过去。
    #[test]
    #[cfg(feature = "store")]
    fn a_marked_store_skips_the_envelope_rescan_but_still_reads_v1_rows() {
        let dir = std::env::temp_dir().join(format!(
            "sv_env_gate_{}_{}",
            std::process::id(),
            now_unix_secs()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("total_store.db");
        let key = || StoreKey::from_bytes([14u8; 32]);
        legacy_generation_store(&path, key(), &[(0, "2026-06-01T10:00:00Z")]);

        // 第一次开：没有标记 ⇒ 必须真的迁移（sv1 → sv2）。
        {
            let store = TotalStore::open_with_key(&path, key()).unwrap();
            let conn = store.conn.lock().unwrap();
            let sv2: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM raw_events WHERE event_json LIKE 'sv2:%'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(sv2, 1, "没有标记时必须迁移 —— 闸不能反过来把该做的事跳掉");
        }

        // 再塞一条 v1 信封进去（模拟用户降级跑过一次旧版），标记仍在。
        {
            let conn = Connection::open(&path).unwrap();
            let event = mk_event(1, "s", Some("v1-again"));
            let aad = event_aad(
                2,
                source_type_key(event.source_type),
                &event.source_location.as_key(),
                &event.source_path,
                &event.source_session_id,
                event.seq as i64,
                0,
                0,
            );
            let envelope = StoreCipher::new(key())
                .encrypt(serde_json::to_string(&event).unwrap().as_bytes(), &aad)
                .unwrap();
            conn.execute(
                r#"INSERT INTO raw_events
                     (ingested_at, schema_version, source_type, source_location, source_path,
                      source_session_id, seq, source_revision, projection_revision, aad_version,
                      event_type, occurred_at, occurred_at_unix_ms, project_root, event_json)
                   VALUES (0, 1, 'claude_code', 'local', '/p/file.jsonl', 's', 1, 0, 0, 2,
                           'message', NULL, NULL, NULL, ?1)"#,
                params![envelope],
            )
            .unwrap();
        }

        let store = TotalStore::open_with_key(&path, key()).unwrap();
        let still_v1: i64 = store
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM raw_events WHERE event_json NOT LIKE 'sv2:%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            still_v1, 1,
            "那条 v1 行被重新封装了 —— 说明整库又被扫了一遍，标记形同虚设"
        );
        // 而它仍然读得出来：这才是跳过安全的依据。
        let events = store
            .read_session(
                SourceType::ClaudeCode,
                &SourceLocation::Local,
                "/p/file.jsonl",
                "s",
            )
            .unwrap()
            .events;
        assert_eq!(events.len(), 2, "跳过重封装不该让任何一行变得读不出来");

        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 🔴 WAL 必须有大小上限，否则一次迁移撑出的高水位**再也不还**。
    ///
    /// 默认 `journal_size_limit = -1`：checkpoint 只把页搬回主库、不截断文件。一个覆盖
    /// 全表的迁移事务因此在 2.8 GB 的真库上留下 **5.33 GB 的 WAL**，而其中有效的只有
    /// 653 页（2.6 MB）。它不拖慢读，所以没有任何迹象会提示 —— 只有 `du` 看得见，而每提
    /// 一次 `PARSER_REVISION` 就再撑一次。
    #[test]
    #[cfg(feature = "store")]
    fn the_wal_has_a_size_limit_so_a_migration_high_water_mark_is_returned() {
        let store = TotalStore::open_in_memory().unwrap();
        let limit: i64 = store
            .conn
            .lock()
            .unwrap()
            .query_row("PRAGMA journal_size_limit", [], |r| r.get(0))
            .unwrap();
        assert!(
            limit > 0,
            "journal_size_limit = {limit}（-1 = 不限）—— WAL 会停在历史最高水位，             一次全表迁移之后就是几 GB 纯占盘，且没有任何迹象提示"
        );
    }
}

#[cfg(test)]
// 测试要造 fixture（建目录、写文件、再核一遍），允许直接碰盘 —— 文件系统边界
// 管的是**生产行为**，而 `#[cfg(test)]` 不在生产路径上。
#[allow(clippy::disallowed_methods)]
mod project_identity_tests {
    use super::*;
    use crate::rawevent::{
        Actor, EventType, SourceLocation, SourceMode, TimeConfidence, TokenUsage, SCHEMA_VERSION,
    };

    /// 造一个真实的 git 工作目录（只要 `.git/config` —— 身份从不 spawn git）。
    fn seed_repo(root: &std::path::Path, origin: Option<&str>) {
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let body = match origin {
            Some(url) => format!("[remote \"origin\"]\n\turl = {url}\n"),
            None => "[core]\n\tbare = false\n".to_string(),
        };
        std::fs::write(root.join(".git").join("config"), body).unwrap();
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("sv-pident-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn event_in(project_root: &std::path::Path, seq: u64) -> RawEvent {
        RawEvent {
            schema_version: SCHEMA_VERSION,
            source_type: SourceType::ClaudeCode,
            source_location: SourceLocation::Local,
            source_path: "/p/f.jsonl".to_string(),
            source_session_id: "s1".to_string(),
            seq,
            event_key: None,
            source_mode: SourceMode::AppendLog,
            cwd: Some(project_root.to_string_lossy().into_owned()),
            project_root: Some(project_root.to_string_lossy().into_owned()),
            project_root_source: Some("git".to_string()),
            workspace_location: Some("local".to_string()),
            event_type: EventType::Message,
            actor: Some(Actor::User),
            occurred_at: Some("2026-06-01T10:00:00Z".to_string()),
            time_confidence: TimeConfidence::High,
            model: None,
            effort: None,
            usage: Some(TokenUsage::default()),
            content: None,
            parent_ref: None,
            content_hash: None,
            artifact_kind: None,
            observed_at: None,
            modified_at: None,
            message_id: None,
            request_id: None,
        }
    }

    /// 走**生产的那条路**（`apply_projection`），不是直接调 `record_project_identity`
    /// —— 后者只能证明我理解得对，证明不了它在真实写入路径上被调到。
    fn ingest(store: &TotalStore, root: &std::path::Path, seq: u64) {
        let ev = event_in(root, seq);
        let root_str = ev
            .project_root
            .clone()
            .expect("event carries a project root");
        store
            .apply_projection(FileProjectionBatch {
                source: SourceKey::from_event(&ev),
                parser_revision: None,
                mode: Projection::Append,
                token: None,
                events: vec![ev],
            })
            .unwrap();
        // 🔴 **身份不再由 `apply_projection` 顺带记**（2026-08-14）——它现在由**注册表**
        // 驱动。助手必须跟着改，否则这些测试测的是一条生产不再走的路（而它们会
        // 全部变绿，因为「什么都不写」也满足不了断言 —— 那一半是运气）。
        //
        // 生产里登记根的是归属；这里复述那一步，再跑一轮扫描。
        store.register_project_root(&root_str, crate::attribution::RootSource::Git);
        store.sweep_registered_root_identities(None, &Vec::new(), Deadline::unbounded());
    }

    /// 🔴 **身份行没落库，就不配保留「问过了」**（三轮评审 P2）。
    ///
    /// `identity_seen` 是**先记后算**的（避免一个读不到的项目让它名下每个文件都试盘），
    /// 代价是**每一个**中途退出的分支都必须撤回。上一轮补了两个（探不到 `.git`、
    /// 读不了 config），漏了第三个：`let _ = conn.execute(...)` 把 INSERT 的失败
    /// 整个丢掉，而缓存已经记过 ⇒ 事件照常写进总库，这个项目却在本进程内**再也不会**
    /// 尝试写身份。
    ///
    /// 用 SQLite trigger 精确阻断 `project_identity` 的 INSERT 来驱动 —— 只挡身份，
    /// 事件投影照常成功，正是那个真实形状。
    #[test]
    fn a_failed_identity_insert_is_retried_on_the_next_pass() {
        let root = scratch("identity-insert-fails");
        let proj = root.join("Proj");
        std::fs::create_dir_all(&proj).unwrap();
        seed_repo(&proj, Some("git@github.com:o/Proj.git"));
        let root_str = proj.to_string_lossy().into_owned();

        let store = TotalStore::open_in_memory().unwrap();
        store
            .conn
            .lock()
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER block_identity BEFORE INSERT ON project_identity
                 BEGIN SELECT RAISE(ABORT, 'blocked'); END;",
            )
            .unwrap();

        ingest(&store, &proj, 0);
        assert_eq!(
            store.project_identity(&root_str),
            None,
            "前提：这一轮身份确实没写进去"
        );

        store
            .conn
            .lock()
            .unwrap()
            .execute_batch("DROP TRIGGER block_identity;")
            .unwrap();

        // 🔴 第二轮：`identity_seen` 若没撤回，这里永远补不上。
        ingest(&store, &proj, 1);
        assert_eq!(
            store.project_identity(&root_str),
            Some("git:github.com/o/proj".to_string()),
            "身份没落库却保留了「问过了」—— 这个项目在本进程内再也拿不到跨 checkout 身份"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    /// 🔴 **一个近期没有活动的项目照样要拿到身份 —— 这是本轮改动的全部理由。**
    ///
    /// 身份从前挂在 `apply_projection` 上：有事件落库才顺带记一次。于是注册表认得的
    /// 根、`.git` 就在那儿、origin 读得出来，而身份表里 **0 行**，只因为那个项目
    /// 最近没人动过。实测形状：`wsl:Ubuntu-22.04:/home/<user>/workspace/QuotaBar`
    /// 最后一个会话文件停在一个月前。
    ///
    /// 后果不是「少一行」：TumeFlow 的 merge key 是「有身份用身份、没有退回路径」，
    /// 所以身份在与不在给出**两个不同的 key** —— 同一份记忆会随身份表的有无落进
    /// 不同的桶，而没有任何东西会说出它们不一致。
    ///
    /// 判据故意**一条事件都不摄取**：把 `sweep_registered_root_identities` 改回
    /// 由投影驱动，这条当场变红。
    #[test]
    fn a_project_with_no_recent_activity_still_gets_its_identity() {
        let root = scratch("no-activity");
        seed_repo(&root, Some("git@github.com:o/dormant.git"));
        let st = TotalStore::open_in_memory().unwrap();
        let root_str = root.to_string_lossy().into_owned();

        // 归属登记了这个根 —— 而它名下**一条事件都没有**。
        st.register_project_root(&root_str, RootSource::Git);
        assert_eq!(
            st.project_identity(&root_str),
            None,
            "前提：扫描之前本来就没有身份行"
        );

        let sweep = st.sweep_registered_root_identities(None, &Vec::new(), Deadline::unbounded());
        assert_eq!(sweep.registered, 1);
        assert_eq!(sweep.recorded, 1, "注册表里的根必须被问到");
        assert_eq!(
            st.project_identity(&root_str).as_deref(),
            Some("git:github.com/o/dormant"),
            "没有活动的项目照样要有身份"
        );

        // 第二轮不再重复问盘，但也不该把已有的行弄丢。
        let again = st.sweep_registered_root_identities(None, &Vec::new(), Deadline::unbounded());
        assert_eq!(
            (again.recorded, again.already_probed),
            (0, 1),
            "终态缓存命中，不重复付探测代价"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    /// 预算耗尽必须**说出来**：「本轮只看了一半」与「本轮全看过了」在结果上一样。
    #[test]
    fn a_sweep_that_ran_out_of_budget_says_so() {
        let st = TotalStore::open_in_memory().unwrap();
        st.register_project_root("/w/a", RootSource::Git);
        st.register_project_root("/w/b", RootSource::Git);
        let sweep = st.sweep_registered_root_identities(
            None,
            &Vec::new(),
            Deadline::after(std::time::Duration::from_millis(0)),
        );
        assert_eq!(sweep.registered, 2);
        assert_eq!(
            sweep.skipped_out_of_budget, 2,
            "预算为零时一个都不该问，而且要报出来"
        );
        assert_eq!((sweep.recorded, sweep.unresolved), (0, 0));
    }

    /// 🔴 **checkout 没了 ≠ 确认这个仓没有 remote。**
    ///
    /// 这是 2026-09-01 修的那个 bug 的护栏：从前两者都落 `no_identity`
    /// （`why = "no .git anywhere on this path"`），于是消费者看到
    /// **`no_identity` + 一个 `git:` 身份**并存 —— 一行自相矛盾的数据，
    /// 而正确读法是「那个目录不在了，已记下的身份仍然有效」。
    #[test]
    fn a_deleted_checkout_reports_checkout_missing_not_no_identity() {
        let root = scratch("gone-vs-not-a-repo");
        let proj = root.join("Proj");
        std::fs::create_dir_all(&proj).unwrap();
        seed_repo(&proj, Some("git@github.com:o/Proj.git"));
        let store = TotalStore::open_in_memory().unwrap();
        ingest(&store, &proj, 0);
        let root_str = proj.to_string_lossy().into_owned();

        std::fs::remove_dir_all(&proj).unwrap();
        store.forget_identity_probe(&root_str); // 逼它重新判一次
        let outcome = store.record_identity_for_root(
            &root_str,
            None,
            &Vec::new(),
            crate::deadline::Deadline::unbounded(),
        );
        assert_eq!(outcome, IdentityOutcome::CheckoutMissing, "{outcome:?}");

        // ⚠️ 反面同样要钉：一个**存在但不是仓库**的目录仍然是 `NoRemote` ——
        // 只写正面的话，「一律报 CheckoutMissing」也能让上面那条绿。
        let plain = root.join("just-a-folder");
        std::fs::create_dir_all(&plain).unwrap();
        let plain_str = plain.to_string_lossy().into_owned();
        let outcome = store.record_identity_for_root(
            &plain_str,
            None,
            &Vec::new(),
            crate::deadline::Deadline::unbounded(),
        );
        assert!(
            matches!(outcome, IdentityOutcome::NoRemote(_)),
            "存在但不是仓库 ⇒ 仍是 NoRemote，got {outcome:?}"
        );
    }

    #[test]
    fn identity_survives_the_checkout_being_deleted() {
        // 🔴 **这条就是 project_identity 表存在的全部理由。** 实测有个项目留着 16 万条
        // 历史事件，却因为 checkout 已删而没有任何东西能说出它属于哪个仓库。
        let root = scratch("survives");
        let proj = root.join("Proj");
        std::fs::create_dir_all(&proj).unwrap();
        seed_repo(&proj, Some("git@github.com:o/Proj.git"));

        let store = TotalStore::open_in_memory().unwrap();
        ingest(&store, &proj, 0);
        let root_str = proj.to_string_lossy().into_owned();
        assert_eq!(
            store.project_identity(&root_str),
            Some("git:github.com/o/proj".to_string())
        );

        // checkout 消失 —— 现算的那条路（identity::canonical_repo_id）从此答不出来。
        std::fs::remove_dir_all(&proj).unwrap();
        assert_eq!(
            crate::identity::find_git_root(&proj),
            crate::identity::GitRoot::StartMissing,
            "前提：磁盘上真的没了 —— 而这一格现在有自己的名字（不再与「不是仓库」同格）"
        );
        assert_eq!(
            store.project_identity(&root_str),
            Some("git:github.com/o/proj".to_string()),
            "记下来的身份必须活过 checkout 的删除"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_reused_path_keeps_both_identities_instead_of_silently_overwriting() {
        // 🔴 latest-wins 会把前一个仓的整段历史划到后一个仓名下。这里两行都在，
        // 默认查询给最新的那个，历史查得到。
        let root = scratch("reused");
        let proj = root.join("Proj");
        std::fs::create_dir_all(&proj).unwrap();
        seed_repo(&proj, Some("git@github.com:o/first.git"));

        let store = TotalStore::open_in_memory().unwrap();
        ingest(&store, &proj, 0);

        // 路径被另一个仓复用：删掉重新 clone 别的东西。
        std::fs::remove_dir_all(&proj).unwrap();
        std::fs::create_dir_all(&proj).unwrap();
        seed_repo(&proj, Some("git@github.com:o/second.git"));
        // 缓存按进程记，换个 store 模拟「下次启动」——upsert 幂等，历史累积。
        store.identity_seen.lock().unwrap().clear();
        ingest(&store, &proj, 1);

        let root_str = proj.to_string_lossy().into_owned();
        let hist = store.project_identity_history(&root_str);
        let ids: Vec<&str> = hist.iter().map(|(c, _, _)| c.as_str()).collect();
        assert!(
            ids.contains(&"git:github.com/o/first") && ids.contains(&"git:github.com/o/second"),
            "两个身份都得留着，不能被覆盖：{ids:?}"
        );
        assert_eq!(
            store.project_identity(&root_str),
            Some("git:github.com/o/second".to_string()),
            "默认查询取 last_seen 最新的那条"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_repo_without_a_remote_writes_no_row_at_all() {
        // `path:` 身份不跨 checkout 稳定，记下来会让「查得到身份」变成一句不能信的话。
        let root = scratch("no-remote");
        let proj = root.join("Proj");
        std::fs::create_dir_all(&proj).unwrap();
        seed_repo(&proj, None);

        let store = TotalStore::open_in_memory().unwrap();
        ingest(&store, &proj, 0);
        assert_eq!(
            store.project_identity(&proj.to_string_lossy()),
            None,
            "没有 remote 时不得写 path: 兜底行"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn an_unreadable_project_root_never_blocks_ingestion() {
        // 🔴 身份是**加法**能力。`wsl:<distro>:/abs` 这类规范形不是本机可打开的路径
        // （UNC 回环那一族同理）—— 记不下身份，但事件必须照常入库。
        let store = TotalStore::open_in_memory().unwrap();
        let mut ev = event_in(std::path::Path::new("/nonexistent"), 0);
        ev.project_root = Some("wsl:Ubuntu-22.04:/home/u/Proj".to_string());
        let stats = store
            .apply_projection(FileProjectionBatch {
                source: SourceKey::from_event(&ev),
                parser_revision: None,
                mode: Projection::Append,
                token: None,
                events: vec![ev],
            })
            .expect("身份记不下来时，摄取必须照常成功");
        assert!(stats.appended > 0, "事件没进库：{stats:?}");
        assert_eq!(
            store.project_identity("wsl:Ubuntu-22.04:/home/u/Proj"),
            None
        );
    }

    #[test]
    fn the_same_project_is_only_probed_once_per_process() {
        // 省的是文件 IO：一个大仓的几百个会话文件不该把同一个 .git/config 读几百遍。
        let root = scratch("cached");
        let proj = root.join("Proj");
        std::fs::create_dir_all(&proj).unwrap();
        seed_repo(&proj, Some("git@github.com:o/Proj.git"));

        let store = TotalStore::open_in_memory().unwrap();
        ingest(&store, &proj, 0);
        // 把 .git 拿掉：若第二次仍去读盘，缓存就没生效。
        std::fs::remove_dir_all(proj.join(".git")).unwrap();
        ingest(&store, &proj, 1);

        assert_eq!(
            store.project_identity(&proj.to_string_lossy()),
            Some("git:github.com/o/proj".to_string()),
            "第二次不该再问盘，身份应保持"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    // ── 项目根注册表（ADR-050 决定 3 / 3.5）──────────────────────────────
    use crate::attribution::{attribute, RootSource};

    #[test]
    fn a_mnt_root_still_attributes_after_a_round_trip() {
        // 🔴 这条钉的是一个**架构断言的后果**，不是断言本身。
        // `register_project_root` 算行键时不做 `/mnt` 收敛（传空表），理由是
        // 「归属不读那个键 —— 它由 `root_path` 在读出时重算」。那句话是对周边结构的
        // 断言，而结构一变它不会报错（本仓判例：安全性注释会悄悄失效）。
        // 所以不靠它，直接钉后果：一条 `/mnt` 形式的根落库、读回来，仍与宿主形式收敛。
        let st = TotalStore::open_in_memory().unwrap();
        let mounts = vec![("/mnt/c".to_string(), r"C:\".to_string())];

        st.register_project_root("/mnt/c/w/QuotaBar", RootSource::Git);
        let reg = st.project_root_registry(&mounts);

        // 两种形式的路径都归到同一个根。
        assert_eq!(
            attribute(Some(r"C:\w\QuotaBar\src"), &reg).root(),
            Some("/mnt/c/w/QuotaBar"),
            "宿主形式的路径要认出这个根"
        );
        assert_eq!(
            attribute(Some("/mnt/c/w/QuotaBar/src"), &reg).root(),
            Some("/mnt/c/w/QuotaBar")
        );
        // 且它们是**同一个**根，不是两条。
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn roots_report_converges_a_mnt_root_with_its_host_form() {
        // 🔴 **`roots` 出口的收敛。** 存储层故意不收敛（键只用于本表去重，见
        // `register_project_root`），并把责任交给读出的那一刻。而本出口从前直接吐
        // 存储行 ⇒ 收敛从未发生 ⇒ 同一个仓以两条根发出去，消费方看到两个项目。
        //
        // 实机（2026-08-26）：TumeFlow 的作用域列表里同一个仓出现两行，
        // 而 `snapshots` 出口给的却是能打开的 UNC 形 —— **同一个 svault 的两个出口
        // 对同一个项目给了不同答案**。
        let st = TotalStore::open_in_memory().unwrap();
        let mounts = vec![("/mnt/c".to_string(), r"C:\".to_string())];
        st.register_project_root("/mnt/c/w/proj", RootSource::Git);
        st.register_project_root(r"C:\w\proj", RootSource::Git);

        // 存储层照旧是两行 —— 那是设计，别在这里改它。
        let (raw, _) = st.project_roots_report(&Vec::new()).unwrap();
        assert_eq!(raw.len(), 2, "空表下照实分开（成对断言的另一半在下一条）");

        let (rows, _) = st.project_roots_report(&mounts).unwrap();
        assert_eq!(rows.len(), 1, "有挂载表就该收敛成一行");
        let row = &rows[0];
        // 代表取**宿主打得开**的那个：消费方拿它去 open()。
        assert_eq!(row.root_path, r"C:\w\proj");
        // 另一种写法进 aliases —— 「同一个项目的其它写法」正是这个字段的语义，
        // 而消费方手上的路径未必是代表那种形式。
        assert!(
            row.aliases.iter().any(|a| a == "/mnt/c/w/proj"),
            "另一种写法要留在 aliases 里：{:?}",
            row.aliases
        );
    }

    #[test]
    fn roots_report_refuses_to_merge_conflicting_identities() {
        // 🔴 **合错了没有任何东西会报错** —— 所以宁可多一行，不可错并。
        // 两条根算出同一个挂载键、却带着不同的 `canonical_id`，说明有个前提错了
        // （挂载表过期？两个仓恰好挂在同一处？）。合并会把两个项目的记忆混在一起。
        let st = TotalStore::open_in_memory().unwrap();
        let mounts = vec![("/mnt/c".to_string(), r"C:\".to_string())];
        st.register_project_root("/mnt/c/w/proj", RootSource::Git);
        st.register_project_root(r"C:\w\proj", RootSource::Git);
        {
            // 直接插身份行 —— 与 `a_differently_spelled_identity_row_still_matches`
            // 同一种造法（生产路径要跑真扫描，这条测的不是那个）。
            let conn = st.conn.lock().unwrap();
            for (root, cid) in [
                ("/mnt/c/w/proj", "git:example.com/a"),
                ("c:/w/proj", "git:example.com/b"),
            ] {
                conn.execute(
                    "INSERT INTO project_identity                        (project_root, canonical_id, first_seen_ms, last_seen_ms)                      VALUES (?1, ?2, 1, 2)",
                    rusqlite::params![root, cid],
                )
                .unwrap();
            }
        }

        let (rows, _) = st.project_roots_report(&mounts).unwrap();
        assert_eq!(rows.len(), 2, "身份冲突时整组不动");
    }

    #[test]
    fn identical_identities_do_merge() {
        // 🔴 **上一条的对照，缺了它上一条几乎不设防。**
        //
        // 变异验证时发现的：把收敛整个去掉，`roots_report_refuses_to_merge_*`
        // **照样绿** —— 它断言「两行」，而没有收敛时本来就是两行。
        // 那条测试因此分不出「因为身份冲突所以没合并」与「根本没有合并这回事」。
        //
        // 这一条把原因钉死：**同样的两条根、身份相同 ⇒ 必须合并**。
        // 两条一起才说明「不合并」是那个 `Err(group)` 分支干的。
        let st = TotalStore::open_in_memory().unwrap();
        let mounts = vec![("/mnt/c".to_string(), r"C:\".to_string())];
        st.register_project_root("/mnt/c/w/proj", RootSource::Git);
        st.register_project_root(r"C:\w\proj", RootSource::Git);
        {
            let conn = st.conn.lock().unwrap();
            for root in ["/mnt/c/w/proj", "c:/w/proj"] {
                conn.execute(
                    "INSERT INTO project_identity                        (project_root, canonical_id, first_seen_ms, last_seen_ms)                      VALUES (?1, 'git:example.com/same', 1, 2)",
                    rusqlite::params![root],
                )
                .unwrap();
            }
        }

        let (rows, _) = st.project_roots_report(&mounts).unwrap();
        assert_eq!(rows.len(), 1, "身份相同就该合并 —— 否则上一条测的不是冲突");
        assert_eq!(
            rows[0].canonical_id.as_deref(),
            Some("git:example.com/same")
        );
    }

    #[test]
    fn without_a_mount_table_the_two_forms_stay_apart() {
        // 收敛不能凭空发生：没读到挂载表就不该猜 `/mnt/c` 是哪个盘。
        let st = TotalStore::open_in_memory().unwrap();
        st.register_project_root("/mnt/c/w/QuotaBar", RootSource::Git);
        let reg = st.project_root_registry(&Vec::new());
        assert_eq!(
            attribute(Some(r"C:\w\QuotaBar\src"), &reg).root(),
            None,
            r"空表下不该把 C:\ 认成 /mnt/c"
        );
    }

    /// 🔴 **修订号数的是「长出过几个根」，不是「注册表被写过几次」**（ADR-051 I7）。
    ///
    /// 注册是幂等的：每轮发现都会把已知的根重登记一遍。若按写入次数计，
    /// 修订号会随刷新次数疯长，把全库 `ProjectionToken` 每分钟作废一次 ——
    /// 那等于没有幂等。
    #[test]
    fn the_attribution_revision_counts_new_roots_not_writes() {
        let st = TotalStore::open_in_memory().unwrap();
        assert_eq!(st.attribution_revision(), 0, "空注册表是 0");

        st.register_project_root("/w/A", RootSource::Git);
        let after_first = st.attribution_revision();
        assert_eq!(after_first, 1);

        // 同一个根重登记十次 —— 修订号不能动。
        for _ in 0..10 {
            st.register_project_root("/w/A", RootSource::Git);
        }
        assert_eq!(
            st.attribution_revision(),
            after_first,
            "幂等重登记不得推高修订号，否则 token 每轮作废"
        );

        // 换个大小写/尾斜杠仍是同一个根（比较键归一），也不能动。
        st.register_project_root("/w/A/", RootSource::Marker);
        assert_eq!(
            st.attribution_revision(),
            after_first,
            "同一个键就是同一个根"
        );

        // 真的新根才 +1。
        st.register_project_root("/w/B", RootSource::Git);
        assert_eq!(st.attribution_revision(), after_first + 1);

        // 重登记不能丢掉 last_seen / source 的更新 —— 拆成两条语句时最容易漏这一半。
        let reg = st.project_root_registry(&Vec::new());
        assert_eq!(reg.len(), 2);
        assert_eq!(
            reg.roots().find(|(p, _)| *p == "/w/A/").map(|(_, s)| s),
            Some(RootSource::Marker),
            "重登记要更新 source 与原始路径"
        );
    }

    #[test]
    fn registry_round_trips_through_the_store() {
        let st = TotalStore::open_in_memory().unwrap();
        st.register_project_root("/w/QuotaBar", RootSource::Git);
        st.register_project_root("/w/QuotaBar/third_party/TumeFlow", RootSource::Git);
        let reg = st.project_root_registry(&Vec::new());
        assert_eq!(reg.len(), 2);
        assert_eq!(
            attribute(Some("/w/QuotaBar/src-tauri/src"), &reg).root(),
            Some("/w/QuotaBar")
        );
        assert_eq!(
            attribute(Some("/w/QuotaBar/third_party/TumeFlow/x.py"), &reg).root(),
            Some("/w/QuotaBar/third_party/TumeFlow")
        );
    }

    #[test]
    fn registry_stores_the_original_form_and_matches_case_insensitively() {
        // 🔴 归一化只用于比较；查出来的必须是原始形式，否则 `project_root` 列里会
        // 出现一堆小写正斜杠路径，而那不是任何一个系统里真实存在的写法。
        let st = TotalStore::open_in_memory().unwrap();
        st.register_project_root(r"C:\Users\u\QuotaBar", RootSource::Git);
        let reg = st.project_root_registry(&Vec::new());
        let a = attribute(Some(r"c:\users\u\quotabar\src-tauri"), &reg);
        assert_eq!(a.root(), Some(r"C:\Users\u\QuotaBar"));
    }

    // ── 对外报告面：`project_roots_report`（#40 步 1）────────────────────

    /// 🔴 **「读到了、没有根」与「没读成」必须是两种结果。**
    ///
    /// 这个命令存在的全部理由，是让走 CLI 的消费方不必自己再发现一遍项目。
    /// 若读失败也返回空列表，消费方会读作「这台机器上没有项目」，然后心安理得地
    /// 回退到它自己那套发现 —— 正好把要消除的第二份实现请回来。
    #[test]
    fn an_empty_registry_and_an_unreadable_one_are_different_answers() {
        let st = TotalStore::open_in_memory().unwrap();

        // ① 读到了，确实没有根 ⇒ Ok(空)。
        let (roots, rev) = st
            .project_roots_report(&Vec::new())
            .expect("空注册表要能读出来");
        assert!(roots.is_empty());
        assert_eq!(rev, 0, "还没注册过根，修订号是初始值 0");

        // ② 读不成 ⇒ Err。打真实失败路径：把表拆掉，让 prepare 真的失败。
        st.conn
            .lock()
            .unwrap()
            .execute("DROP TABLE project_root_registry", [])
            .unwrap();
        let err = st
            .project_roots_report(&Vec::new())
            .expect_err("读不到注册表必须报错，不能返回空列表冒充「没有项目」");
        assert!(
            err.contains("project_root_registry"),
            "错误要说出是哪张表读不到：{err}"
        );
    }

    /// 一行的五个字段如实返回，且 `root_key` 与 `root_path` **都给**。
    ///
    /// 只给原始形式会逼每个消费方自己再归一化一遍 —— 同一条规则长出第二份实现的入口。
    #[test]
    fn a_reported_root_carries_both_the_key_and_the_original_form() {
        let st = TotalStore::open_in_memory().unwrap();
        st.register_project_root(r"C:\Users\u\Proj", RootSource::Git);

        let (roots, _) = st.project_roots_report(&Vec::new()).unwrap();
        assert_eq!(roots.len(), 1);
        let r = &roots[0];
        assert_eq!(r.root_path, r"C:\Users\u\Proj", "原始形式原样返回");
        assert_ne!(
            r.root_key, r.root_path,
            "比较键是归一化过的，不该等于原始形式"
        );
        assert_eq!(r.root_key, "c:/users/u/proj", "小写、正斜杠、无尾斜杠");
        assert_eq!(r.root_source, "git");
        assert!(r.first_seen_ms > 0 && r.last_seen_ms > 0, "时间戳要落库");
    }

    /// 🔴 **清单与修订号必须同代。**
    ///
    /// `attribution_revision` 是消费方的缓存失效锚。若它与清单来自两次读，中间的
    /// 锁间隙足够一次注册挤进来，消费方就会拿到「新清单 + 旧修订号」（或反过来）——
    /// 而**两种都会让它认为缓存仍然有效**。
    #[test]
    fn the_report_and_its_revision_come_from_one_read() {
        let st = TotalStore::open_in_memory().unwrap();
        st.register_project_root("/w/A", RootSource::Git);
        let (roots1, rev1) = st.project_roots_report(&Vec::new()).unwrap();
        assert_eq!(roots1.len(), 1);
        assert_eq!(rev1, 1, "一个新根 ⇒ 修订号 1");

        st.register_project_root("/w/B", RootSource::Git);
        let (roots2, rev2) = st.project_roots_report(&Vec::new()).unwrap();
        assert_eq!(roots2.len(), 2, "清单跟上了");
        assert_eq!(rev2, 2, "修订号也跟上了 —— 两者必然同代");
    }

    /// 🔴 **等价写法要给出来，否则消费方会把同一个项目当成两个。**
    ///
    /// 一个 Windows 上的消费方枚举出 `\\wsl.localhost\<distro>\…`，注册表里存的却是
    /// `wsl:<distro>:/…` —— 用 `==` 一比就是两个项目。实测后果：同一个项目在记忆库里
    /// 存成两个身份，各持一半记忆且互相看不见。
    #[test]
    fn a_wsl_root_carries_the_form_a_windows_consumer_can_open() {
        let st = TotalStore::open_in_memory().unwrap();
        st.register_project_root("wsl:Ubuntu-22.04:/home/u/proj", RootSource::Git);

        let (roots, _) = st.project_roots_report(&Vec::new()).unwrap();
        assert_eq!(
            roots[0].aliases,
            vec![r"\\wsl.localhost\Ubuntu-22.04\home\u\proj"],
            "规范形要带上 Windows 侧能打开的 UNC 形"
        );

        // 反过来也要成立 —— 归属发生在哪一侧决定了注册表存的是哪一种。
        let st2 = TotalStore::open_in_memory().unwrap();
        st2.register_project_root(r"\\wsl.localhost\Ubuntu-22.04\home\u\proj", RootSource::Git);
        let (roots2, _) = st2.project_roots_report(&Vec::new()).unwrap();
        assert_eq!(roots2[0].aliases, vec!["wsl:Ubuntu-22.04:/home/u/proj"]);
    }

    /// 🔴 **`/mnt/<drive>/…` 的换算要挂载表，这个进程没有 ⇒ 不给别名，而不是猜一个。**
    ///
    /// 按盘符猜出 `C:\…` 在 `automount.root` 被改过的机器上会把两个不相干的项目并成
    /// 一个。空别名是诚实的「我算不出来」，与本仓「没问成不能长得像这里是空的」互为
    /// 表里：那条讲不许把失败说成空，这条讲不许把不知道说成知道。
    #[test]
    fn a_mnt_root_gets_no_guessed_alias() {
        let st = TotalStore::open_in_memory().unwrap();
        st.register_project_root("/mnt/c/work/proj", RootSource::Git);
        let (roots, _) = st.project_roots_report(&Vec::new()).unwrap();
        assert!(
            roots[0].aliases.is_empty(),
            "没有挂载表就不该猜出一个 Windows 形式：{:?}",
            roots[0].aliases
        );
    }

    /// 纯 Windows / 纯 Linux 路径没有第二种写法 —— 空表，不是把自己复制一份。
    #[test]
    fn a_plain_path_has_no_aliases() {
        let st = TotalStore::open_in_memory().unwrap();
        st.register_project_root(r"D:\work\proj", RootSource::Git);
        let (roots, _) = st.project_roots_report(&Vec::new()).unwrap();
        assert!(roots[0].aliases.is_empty());
        assert!(
            !roots[0].aliases.contains(&roots[0].root_path),
            "别名里不该含 root_path 自身 —— 消费方会把它当成第二个身份"
        );
    }

    /// 🔴 **跨系统身份要带出来 —— 别名收敛不了「两份 checkout」。**
    ///
    /// Windows 一份、WSL 一份，路径毫不相干，`aliases` 永远认不出它们是同一个项目；
    /// 认得出的是 git origin。消费方要把同一个项目聚成一组，两样都得有。
    #[test]
    fn two_checkouts_of_one_repo_share_a_canonical_id() {
        let st = TotalStore::open_in_memory().unwrap();
        st.register_project_root(r"D:\work\proj", RootSource::Git);
        st.register_project_root("/home/u/proj", RootSource::Git);
        {
            let conn = st.conn.lock().unwrap();
            for root in [r"D:\work\proj", "/home/u/proj"] {
                conn.execute(
                    "INSERT INTO project_identity \
                       (project_root, canonical_id, first_seen_ms, last_seen_ms) \
                     VALUES (?1, 'git:example.com/o/r', 1, 2)",
                    rusqlite::params![root],
                )
                .unwrap();
            }
        }

        let (roots, _) = st.project_roots_report(&Vec::new()).unwrap();
        let ids: Vec<_> = roots.iter().map(|r| r.canonical_id.as_deref()).collect();
        assert_eq!(
            ids,
            vec![Some("git:example.com/o/r"), Some("git:example.com/o/r")],
            "两份 checkout 要报出同一个身份，消费方才聚得起来"
        );
    }

    /// 🔴 **身份按归一化键匹配，不按字面相等。**
    ///
    /// 两张表的路径来自不同时刻的归属，写法可能不同（大小写、斜杠方向）。字面比会让
    /// 一个明明记着身份的项目报成「说不出身份」—— 一个**看得见的功能缺失**
    /// （两份 checkout 不再聚成一组），却不会有任何东西报错。
    #[test]
    fn a_differently_spelled_identity_row_still_matches() {
        let st = TotalStore::open_in_memory().unwrap();
        st.register_project_root(r"D:\Work\Proj", RootSource::Git);
        {
            let conn = st.conn.lock().unwrap();
            // 身份行用小写正斜杠写法 —— 与注册表那条字面不等，但归一化后同键。
            conn.execute(
                "INSERT INTO project_identity \
                   (project_root, canonical_id, first_seen_ms, last_seen_ms) \
                 VALUES ('d:/work/proj/', 'git:example.com/o/r', 1, 2)",
                [],
            )
            .unwrap();
        }

        let (roots, _) = st.project_roots_report(&Vec::new()).unwrap();
        assert_eq!(
            roots[0].canonical_id.as_deref(),
            Some("git:example.com/o/r"),
            "写法差异不该让身份丢掉"
        );
    }

    /// 🔴 **本任务（#56）的判据本身：三种「没有身份」在报告里必须看得出区别。**
    ///
    /// | 根 | `canonical_id` | 判决 | 下游 |
    /// | --- | --- | --- | --- |
    /// | 有 origin 的仓 | `git:…` | `resolved` | 用它 |
    /// | 没有 `.git` 的目录 | `null` | `no_identity` | **接受**，别再算 |
    /// | 桥够不着的根 | `null` | `unresolved` | **重试**，且**绝不据此删东西** |
    /// | 登记了但没扫过 | `null` | `not_probed` | **等** |
    ///
    /// 后三行从前**一模一样**（三个 `null`）。实测本机 20 个根里，第二类 1 个、
    /// 第三类 3 个 —— 而 #41/#42 两次误诊正是把它们读成了同一件事。
    ///
    /// ⚠️ **断言写成「四个值互不相同」而不是逐个比字面量**：后者在有人把
    /// `no_identity` 改名时会红，而那不是缺陷；前者钉的是**能不能分辨**，
    /// 那才是这条护栏存在的理由。逐个的字面量由下面 `as_str` 那条钉。
    ///
    /// ⚠️ `wsl:no-such-distro:/…` 在**两个平台上都**走不通（Windows 上
    /// `WSL_E_DISTRO_NOT_FOUND`，非 Windows 上 `stub_on_non_windows!` 直接 `Err`）
    /// —— 本仓「只在一个平台上编译的代码等于没有编译过」那条的测试侧对应物。
    #[test]
    fn the_three_ways_to_have_no_identity_are_told_apart() {
        let dir = scratch("verdicts");
        let with_origin = dir.join("WithOrigin");
        let plain = dir.join("PlainDir");
        std::fs::create_dir_all(&plain).unwrap();
        seed_repo(&with_origin, Some("git@github.com:o/r.git"));
        let unreachable = "wsl:no-such-distro-56:/home/u/p";

        let st = TotalStore::open_in_memory().unwrap();
        for p in [&with_origin, &plain] {
            st.register_project_root(&p.to_string_lossy(), RootSource::Git);
        }
        st.register_project_root(unreachable, RootSource::Git);
        let sweep = st.sweep_registered_root_identities(None, &Vec::new(), Deadline::unbounded());
        assert_eq!(
            (sweep.recorded, sweep.no_remote, sweep.unresolved),
            (1, 1, 1),
            "前提：三个根各走一条路 —— 否则下面的断言在测别的东西"
        );

        // 第四种：登记在扫描**之后**，所以这一轮没问过它。
        st.register_project_root("/w/never-swept-56", RootSource::Git);

        let (roots, _) = st.project_roots_report(&Vec::new()).unwrap();
        let verdict_of = |needle: &str| {
            roots
                .iter()
                .find(|r| r.root_path.contains(needle))
                .unwrap_or_else(|| panic!("报告里没有 {needle}"))
                .identity_verdict
                .clone()
        };
        let resolved = verdict_of("WithOrigin");
        let no_identity = verdict_of("PlainDir");
        let unresolved_v = verdict_of("no-such-distro-56");
        let not_probed = verdict_of("never-swept-56");

        assert_eq!(resolved, IdentityVerdict::Resolved);
        assert_eq!(not_probed, IdentityVerdict::NotProbed);
        assert!(
            matches!(no_identity, IdentityVerdict::NoIdentity { .. }),
            "确认没有身份 —— 实得 {no_identity:?}"
        );
        assert!(
            matches!(unresolved_v, IdentityVerdict::Unresolved { .. }),
            "没问成 —— 实得 {unresolved_v:?}"
        );

        // 判据的正面表述：四个**都不相同**。
        let wire = [&resolved, &no_identity, &unresolved_v, &not_probed]
            .map(|v| v.as_str())
            .to_vec();
        let mut uniq = wire.clone();
        uniq.sort_unstable();
        uniq.dedup();
        assert_eq!(
            uniq.len(),
            4,
            "四种情况必须给出四个不同的说法，实得 {wire:?}"
        );

        // 而 `canonical_id` 对后三种**一律是 `null`** —— 这正是从前唯一的信息量。
        for needle in ["PlainDir", "no-such-distro-56", "never-swept-56"] {
            let r = roots.iter().find(|r| r.root_path.contains(needle)).unwrap();
            assert_eq!(r.canonical_id, None, "{needle} 本来就不该有身份");
        }

        // 「没问成」必须带上原话 —— 「没问成」和「没问成：桥不通」不是一回事。
        assert!(
            unresolved_v.why().is_some_and(|w| !w.is_empty()),
            "没问成要说得出为什么"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// 判决的线上拼写 —— `project_identity_probe.outcome` 与 `svault roots`
    /// **共用这一份**。改动它就是改协议，消费方看见一个不认识的值不会报错、
    /// 只会静默走 else 分支。
    #[test]
    fn the_verdict_wire_values_are_pinned() {
        assert_eq!(IdentityVerdict::NotProbed.as_str(), "not_probed");
        assert_eq!(IdentityVerdict::Resolved.as_str(), "resolved");
        assert_eq!(
            IdentityVerdict::NoIdentity { why: "x".into() }.as_str(),
            "no_identity"
        );
        assert_eq!(
            IdentityVerdict::Unresolved { why: "x".into() }.as_str(),
            "unresolved"
        );
        // 🔴 未来版本写下的新值读作「没问成」，不是「还没扫到」——
        // 后者是**编造**（我们明明看见有人探测过），前者至少方向对（重试、别删）。
        assert!(matches!(
            IdentityVerdict::from_row("something_new", None),
            IdentityVerdict::Unresolved { .. }
        ));
    }

    /// 🔴 **「有身份」与「本轮问到了」是两个事实，报告要能同时说。**
    ///
    /// `project_identity` 的行活过 checkout 被删（那是它存在的全部理由），
    /// 而判决说的是**最后一次探测**。把两者绑成一个值，就会出现
    /// 「桥今天不通 ⇒ 连历史身份也说不出来」或者反过来
    /// 「历史身份还在 ⇒ 报告说今天问到了」—— 后者更坏，它是**编造**。
    #[test]
    fn a_known_identity_and_a_failed_probe_are_both_reported() {
        let st = TotalStore::open_in_memory().unwrap();
        st.register_project_root(r"D:\Work\Proj", RootSource::Git);
        {
            let conn = st.conn.lock().unwrap();
            conn.execute(
                "INSERT INTO project_identity \
                   (project_root, canonical_id, first_seen_ms, last_seen_ms) \
                 VALUES ('d:/work/proj', 'git:example.com/o/r', 1, 2)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO project_identity_probe \
                   (project_root, outcome, detail, last_probe_ms) \
                 VALUES ('d:/work/proj', 'unresolved', 'bridge timed out', 3)",
                [],
            )
            .unwrap();
        }

        let (roots, _) = st.project_roots_report(&Vec::new()).unwrap();
        assert_eq!(
            roots[0].canonical_id.as_deref(),
            Some("git:example.com/o/r"),
            "上一次问到的身份不该被这一次的失败抹掉"
        );
        assert_eq!(
            roots[0].identity_verdict,
            IdentityVerdict::Unresolved {
                why: "bridge timed out".to_string()
            },
            "而这一次确实没问成，报告要说出来"
        );
    }

    /// 🔴 **判决没落库，就不配保留「问过了」** —— 与
    /// [`a_failed_identity_insert_is_retried_on_the_next_pass`] 同一条纪律的另一半。
    ///
    /// `identity_seen` 是**先记后算**的，所以每一个中途退出的分支都必须撤回。
    /// 新增的判决写入是**第四个**这样的出口；漏掉它的后果与前三个一样：
    /// 一次瞬时故障让这个根在本进程内**再也不被问起**，而报告永远说「还没扫到」。
    ///
    /// 用 trigger 精确阻断判决表的写入 —— 身份行照常落库，正是那个真实形状。
    ///
    /// [`a_failed_identity_insert_is_retried_on_the_next_pass`]: #method.a_failed_identity_insert_is_retried_on_the_next_pass
    #[test]
    fn a_failed_verdict_write_is_retried_on_the_next_pass() {
        let dir = scratch("verdict-write-fails");
        seed_repo(&dir, Some("git@github.com:o/vw.git"));
        let root_str = dir.to_string_lossy().into_owned();

        let st = TotalStore::open_in_memory().unwrap();
        st.register_project_root(&root_str, RootSource::Git);
        st.conn
            .lock()
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER block_verdict BEFORE INSERT ON project_identity_probe
                 BEGIN SELECT RAISE(ABORT, 'blocked'); END;",
            )
            .unwrap();

        let first = st.sweep_registered_root_identities(None, &Vec::new(), Deadline::unbounded());
        assert_eq!(
            (first.recorded, first.unresolved),
            (0, 1),
            "判决写不进去 ⇒ 这一轮不算数，要报成「没问成」"
        );
        let (roots, _) = st.project_roots_report(&Vec::new()).unwrap();
        assert_eq!(
            roots[0].identity_verdict,
            IdentityVerdict::NotProbed,
            "判决表里什么都没有 ⇒ 报告只能说「还没扫到」"
        );

        st.conn
            .lock()
            .unwrap()
            .execute_batch("DROP TRIGGER block_verdict;")
            .unwrap();

        // 🔴 第二轮：`identity_seen` 若没撤回，这里永远补不上。
        let second = st.sweep_registered_root_identities(None, &Vec::new(), Deadline::unbounded());
        assert_eq!(
            second.already_probed, 0,
            "缓存没撤回 —— 这个根在本进程内再也不会被问起"
        );
        let (roots, _) = st.project_roots_report(&Vec::new()).unwrap();
        assert_eq!(roots[0].identity_verdict, IdentityVerdict::Resolved);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// 没记过身份 ⇒ `None`。**不是空串、不是拿 `root_path` 兜底** —— 一个不跨 checkout
    /// 稳定的兜底 id 会让「查得到身份」变成一句不能信的话（同 `record_project_identity`
    /// 的约束 1）。
    #[test]
    fn a_root_without_a_recorded_identity_says_so() {
        let st = TotalStore::open_in_memory().unwrap();
        st.register_project_root("/home/u/proj", RootSource::Git);
        let (roots, _) = st.project_roots_report(&Vec::new()).unwrap();
        assert_eq!(roots[0].canonical_id, None);
    }

    /// 🔴 **不认识的来源标签原样透传，不改写。**
    ///
    /// 与 [`TotalStore::project_root_registry`] 的 `unwrap_or(Scan)` 是**有意的**
    /// 分野：那里在算归属，一个不认识的标签不该让整个根消失，所以取个保守默认；
    /// 这里在**报告事实**，把未来版本写下的 `configured_v2` 报成 `scan` 就是在
    /// 编造。消费方看见一个自己不认识的来源，好过看见一个伪造的熟面孔。
    #[test]
    fn an_unknown_root_source_is_reported_verbatim() {
        let st = TotalStore::open_in_memory().unwrap();
        st.conn
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO project_root_registry \
                   (root_key, root_path, root_source, first_seen_ms, last_seen_ms) \
                 VALUES ('/w/x', '/w/X', 'from_the_future', 1, 2)",
                [],
            )
            .unwrap();

        let (roots, _) = st.project_roots_report(&Vec::new()).unwrap();
        assert_eq!(roots.len(), 1);
        assert_eq!(
            roots[0].root_source, "from_the_future",
            "报告面不得把不认识的来源改写成一个认识的"
        );
    }

    #[test]
    fn registering_the_same_root_twice_does_not_duplicate_it() {
        let st = TotalStore::open_in_memory().unwrap();
        st.register_project_root("/w/P", RootSource::Marker);
        st.register_project_root("/w/P/", RootSource::Git); // 尾斜杠 = 同一个根
        assert_eq!(st.project_root_count(), 1);
        let reg = st.project_root_registry(&Vec::new());
        match attribute(Some("/w/P/x"), &reg) {
            crate::attribution::Attribution::Root { source, .. } => {
                assert_eq!(source, RootSource::Git, "后来者应覆盖来源")
            }
            other => panic!("expected Root, got {other:?}"),
        }
    }

    #[test]
    fn a_root_without_a_git_remote_is_still_a_root() {
        // 🔴 这条正是「为什么不与 project_identity 合表」：那张表刻意**不写** `path:`
        // 兜底行（身份要跨 checkout 稳定），而归属恰恰需要这些没有 remote 的根。
        let st = TotalStore::open_in_memory().unwrap();
        st.register_project_root("/w/no-remote-project", RootSource::Marker);
        assert_eq!(
            st.project_identity("/w/no-remote-project"),
            None,
            "身份表不该有它"
        );
        assert_eq!(
            attribute(
                Some("/w/no-remote-project/src"),
                &st.project_root_registry(&Vec::new())
            )
            .root(),
            Some("/w/no-remote-project"),
            "但归属必须认得它"
        );
    }

    #[test]
    fn an_empty_registry_attributes_nothing_rather_than_falling_back() {
        // 发现整个没跑过时，归属该一致地说不出来 —— 而不是退回「用 cwd 当根」。
        let st = TotalStore::open_in_memory().unwrap();
        let reg = st.project_root_registry(&Vec::new());
        assert!(reg.is_empty());
        assert!(!attribute(Some("/w/anything"), &reg).is_attributed());
    }

    #[test]
    fn an_unknown_root_source_label_does_not_drop_the_root() {
        // 未来版本写进一个本版不认识的来源标签时，那个根**仍然要参与归属** ——
        // 少一个根会让它名下所有事件静默变成 Unattributed。
        let st = TotalStore::open_in_memory().unwrap();
        {
            let conn = st.conn.lock().unwrap();
            conn.execute(
                "INSERT INTO project_root_registry
                     (root_key, root_path, root_source, first_seen_ms, last_seen_ms)
                 VALUES ('/w/future', '/w/future', 'some_future_source', 1, 1)",
                [],
            )
            .unwrap();
        }
        assert_eq!(
            attribute(Some("/w/future/x"), &st.project_root_registry(&Vec::new())).root(),
            Some("/w/future")
        );
    }

    /// 🔴 **只读打开不许留下任何痕迹。**
    ///
    /// `open` 会 `create_dir_all` + 让 SQLite 建文件 + 必要时 `create_os_key()`，
    /// 所以一个「我只想读一下」的调用方用错入口，就会**凭空给用户造一个空库和
    /// 一把 OS 密钥，而且不报错**。QuotaBar 为此在调用点自己先探一次 ——
    /// 一条本该由 API 消化掉的知识外泄到了消费者身上。
    ///
    /// 判据是**正向的**：既要 `Absent`，也要**目录和文件都还不存在**。只断言
    /// 返回值等于没测到副作用 —— 而副作用才是这个 API 存在的理由。
    #[test]
    fn open_existing_never_creates_anything() {
        let dir = std::env::temp_dir().join(format!(
            "sv-open-existing-{}-{}",
            std::process::id(),
            line!()
        ));
        let path = dir.join("nested").join("total_store.db");
        assert!(!dir.exists(), "前置：这个目录必须还不存在");

        assert!(matches!(TotalStore::open_existing(&path), Probed::Absent));

        assert!(!path.exists(), "只读打开不许建文件");
        assert!(
            !dir.exists(),
            "只读打开不许建目录（`open` 会 create_dir_all）"
        );
    }

    /// 🔴 **两个写者同时写同一个落盘的总库 —— 都要成功。**
    ///
    /// 这是 QuotaBar task #44 的前置：让 TumeFlow 也扫会话之后，总库就有**两个**
    /// 写者（宿主的常驻扫描 + 引擎的按需同步）。
    ///
    /// ⚠️ **缺的从来不是「锁」** —— WAL 已经保证同一时刻只有一个写者。缺的是两样：
    ///
    /// | | 没有它会怎样 |
    /// | --- | --- |
    /// | `busy_timeout`（默认 **0**）| 第二个写者**立刻**拿到 `SQLITE_BUSY`，而不是稍等 |
    /// | `BEGIN IMMEDIATE`（默认 DEFERRED）| 两个写者各拿读锁再升级 ⇒ 死锁，**而 `busy_timeout` 管不了这一种** |
    ///
    /// 所以两条必须同时在。本测试对**任一条**缺失都会红 —— 变异验证过。
    ///
    /// ⚠️ 用两个**线程 + 两个独立连接**（不是同一个 `TotalStore`）：同一个实例
    /// 内部有 `Mutex<Connection>`，那把锁会替 SQLite 挡掉一切争用，
    /// **于是测的是那把 Mutex，不是并发写**。判据要打真正的那条路。
    #[test]
    fn two_independent_writers_on_one_store_both_succeed() {
        let dir = std::env::temp_dir().join(format!(
            "sv-concurrent-writers-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("total_store.db");
        // 两个连接必须用**同一把**密钥 —— `open()` 会各建各的 OS key。
        let key = StoreKey::from_bytes([7u8; 32]);
        drop(TotalStore::open_with_key(&path, key).unwrap());

        // 🔴 **必须制造真正的争用** —— 第一版是 2 线程 × 8 次微小写入，它们天然
        // 错开，于是把 `busy_timeout` 和 `IMMEDIATE` **各去掉一条**测试都照样绿
        // （实测）。一条不会红的护栏比没有护栏更糟。
        //
        // 三样一起才够：① 屏障让所有线程同一刻起跑；② 线程数 > 2；
        // ③ 每次写**一批**（写锁被按住的时间足够长到重叠）。
        const WRITERS: u64 = 4;
        const ROUNDS: u64 = 25;
        const BATCH: u64 = 40;
        let gate = std::sync::Arc::new(std::sync::Barrier::new(WRITERS as usize));

        let mut handles = Vec::new();
        for w in 0..WRITERS {
            let p = path.clone();
            let gate = gate.clone();
            handles.push(std::thread::spawn(move || {
                let store = TotalStore::open_with_key(&p, StoreKey::from_bytes([7u8; 32]))
                    .map_err(|e| format!("writer {w} open: {e}"))?;
                gate.wait();
                for r in 0..ROUNDS {
                    let evs: Vec<_> = (0..BATCH)
                        .map(|i| {
                            let mut ev = super::tests::mk_event(
                                w * 1_000_000 + r * 1_000 + i,
                                &format!("s{w}"),
                                Some("x"),
                            );
                            ev.source_path = format!("/w/f{w}.jsonl");
                            ev
                        })
                        .collect();
                    store
                        .append_events(&evs, Projection::Append)
                        .map_err(|e| format!("writer {w} round {r}: {e}"))?;
                }
                Ok::<(), String>(())
            }));
        }
        for h in handles {
            h.join().expect("writer panicked").expect("并发写不该失败");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 🔴 **`gc` 在事务里「先读后写」—— 那才是 DEFERRED 的危险所在。**
    ///
    /// 上一条测的是 `append_events`，它进事务后**第一条就是写** —— DEFERRED 在
    /// 那一刻等价于 IMMEDIATE，所以把 `IMMEDIATE` 变异掉它照样绿（实测）。
    /// **测试打错了函数，不是护栏没用。**
    ///
    /// `gc_superseded_projections` 不一样（`store.rs` 里 `tx.prepare(CANDIDATES)` 与
    /// 逐候选的 `tx.query_row`，写在 `if !dry_run` 里）：BEGIN → SELECT 拿读快照 →
    /// 别的写者提交 → 本事务再写 ⇒ SQLite 返回 `SQLITE_BUSY_SNAPSHOT`，
    /// **而 `busy_timeout` 对这一种无能为力**（快照已经旧了，等下去也不会变新）。
    /// `BEGIN IMMEDIATE` 一开始就拿写锁，根本不会出现「快照过期」。
    #[test]
    fn a_read_then_write_transaction_survives_a_concurrent_committer() {
        let dir = std::env::temp_dir().join(format!(
            "sv-gc-vs-writer-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("total_store.db");
        let key = || StoreKey::from_bytes([9u8; 32]);

        // 🔴 **两个连接都先开好，再种数据。** 先种再重开会在 `open_with_key` 里
        // 撞 `QueryReturnedNoRows`（裸 SQL 绕过了正常写入口，重开时的一致性查询
        // 落空）—— 那是 fixture 的问题，不是被测行为，别让它冒充失败。
        let gc_store = TotalStore::open_with_key(&path, key()).unwrap();
        let writer_store = TotalStore::open_with_key(&path, key()).unwrap();

        // 造一批「被取代的投影」，让 gc 的读阶段有活干（每个候选一次 COUNT 查询）。
        {
            let conn = gc_store.conn.lock().unwrap();
            for i in 0..300 {
                let p = format!("/p/rolled{i}.jsonl");
                conn.execute_batch(&format!(
                    r#"
                    INSERT INTO projections (source_type, source_location, source_path,
                        source_revision, projection_revision, parser_revision, origin, created_at)
                    VALUES ('claude_code','local','{p}',1,0,1,'append',0);
                    INSERT INTO raw_events (ingested_at, schema_version, source_type,
                        source_location, source_path, source_session_id, seq, source_revision,
                        projection_revision, aad_version, event_type, occurred_at,
                        occurred_at_unix_ms, project_root, event_json)
                    VALUES (0,1,'claude_code','local','{p}','s',0,1,0,1,
                            'message',NULL,NULL,NULL,'sv2:head');

                    INSERT INTO projections (source_type, source_location, source_path,
                        source_revision, projection_revision, parser_revision, origin, created_at)
                    VALUES ('claude_code','local','{p}',0,7,1,'reparse',0);
                    INSERT INTO raw_events (ingested_at, schema_version, source_type,
                        source_location, source_path, source_session_id, seq, source_revision,
                        projection_revision, aad_version, event_type, occurred_at,
                        occurred_at_unix_ms, project_root, event_json)
                    VALUES (0,1,'claude_code','local','{p}','s',0,0,7,1,
                            'message',NULL,NULL,NULL,'sv2:superseded');
                    "#
                ))
                .unwrap();
            }
        }

        // 另一条线程在 gc 读阶段持续提交 —— 这正是让 DEFERRED 的读快照过期的那件事。
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let committer = {
            let stop = stop.clone();
            std::thread::spawn(move || {
                let store = writer_store;
                let mut n = 0u64;
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let mut ev = super::tests::mk_event(n, "noise", Some("x"));
                    ev.source_path = "/w/noise.jsonl".to_string();
                    // 失败不算数：本测试判的是 **gc 那一侧**能不能活下来。
                    let _ = store.append_events(&[ev], Projection::Append);
                    n += 1;
                }
            })
        };

        let got = gc_store.gc_superseded_projections(false);
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        committer.join().unwrap();

        let stats = got.expect("先读后写的事务不该被并发提交挤掉");
        assert!(
            stats.projections > 0,
            "前提：确实有候选，否则事务里根本不写"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 反面：库真的在，就要拿到它 —— 一个恒 `Absent` 的实现同样能通过上一条。
    #[test]
    fn open_existing_returns_the_store_when_it_is_there() {
        let dir = std::env::temp_dir().join(format!(
            "sv-open-existing-ok-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("total_store.db");
        // 先用写入口建出来 —— 这正是 `open` 该做而 `open_existing` 不该做的事。
        drop(TotalStore::open(&path).unwrap());
        assert!(path.exists());

        assert!(matches!(TotalStore::open_existing(&path), Probed::Found(_)));
        std::fs::remove_dir_all(&dir).ok();
    }
}
