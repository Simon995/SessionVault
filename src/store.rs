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

use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use serde::Serialize;

use crate::cursor::{Cursor, ScanStatus};
use crate::discover::SourceRef;
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
    fn origin_key(self) -> &'static str {
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
    project_root        TEXT,
    event_json          TEXT    NOT NULL,
    UNIQUE (source_type, source_location, source_path, source_session_id, seq,
            source_revision, projection_revision)
);
CREATE INDEX IF NOT EXISTS idx_raw_events_session ON raw_events(source_session_id);
CREATE INDEX IF NOT EXISTS idx_raw_events_project ON raw_events(project_root);
"#;

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

/// 每个源文件的当前头：哪个源版本、以及该源版本的哪份投影是**当前**的。
///
/// 🔴 它取代的是 `read_session` 里那个 `MAX(generation)` 相关子查询。区别不是性能，是
/// **可表达性**：`MAX` 只能表达「编号最大的那一代」，表达不了「这份投影被替换成了空」
/// ——而一个新解析器合法地对某文件产出零事件时，正需要表达它（ADR-044 决定 2 / G2）。
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
}

impl TotalStore {
    /// 打开（或新建）磁盘总库，WAL 模式，建表幂等。父目录自动创建。
    ///
    /// 密钥只从 OS keychain 读取。全新库或既有明文库可创建首把密钥；若检测到已有密文但
    /// keychain 中没有对应密钥则硬失败，绝不生成新钥匙覆盖并造成静默数据丢失。
    pub fn open(path: &Path) -> StoreResult<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
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

    /// 使用宿主提供的密钥打开数据库。适用于测试和不由默认 OS keychain 管理密钥的嵌入方。
    pub fn open_with_key(path: &Path, key: StoreKey) -> StoreResult<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
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

    fn from_conn(conn: Connection, key: StoreKey) -> StoreResult<Self> {
        // WAL 让读不挡写（QuotaBar 常驻写、未来 TumeFlow 并发读）。
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "secure_delete", "ON")?;
        let store = Self {
            conn: Mutex::new(conn),
            cipher: StoreCipher::new(key),
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
        let conn = self.conn.lock().unwrap();
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
            "#,
        )?;

        let raw_exists: bool = conn
            .prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name='raw_events'")?
            .query_row([], |_| Ok(true))
            .optional()?
            .unwrap_or(false);
        let column_exists = |name: &str| -> StoreResult<bool> {
            if !raw_exists {
                return Ok(false);
            }
            Ok(conn
                .prepare("SELECT 1 FROM pragma_table_info('raw_events') WHERE name = ?1")?
                .query_row([name], |_| Ok(true))
                .optional()?
                .unwrap_or(false))
        };
        let has_generation = column_exists("generation")?;
        let has_source_revision = column_exists("source_revision")?;

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
            let generation_expr = if has_generation { "generation" } else { "0" };
            conn.execute_batch(&format!(
                r#"
                ALTER TABLE raw_events RENAME TO raw_events_pre_srev;
                {RAW_EVENTS_DDL}
                INSERT INTO raw_events
                    (offset, ingested_at, schema_version, source_type, source_location,
                     source_path, source_session_id, seq, source_revision, projection_revision,
                     aad_version, event_type, occurred_at, project_root, event_json)
                SELECT offset, ingested_at, schema_version, source_type, source_location,
                       source_path, source_session_id, seq, {generation_expr}, 0,
                       1, event_type, occurred_at, project_root, event_json
                  FROM raw_events_pre_srev;
                DROP TABLE raw_events_pre_srev;
                "#
            ))?;
        }
        conn.execute_batch(PROJECTIONS_DDL)?;
        conn.execute_batch(CURRENT_HEAD_DDL)?;
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

            INSERT OR IGNORE INTO current_head
                (source_type, source_location, source_path,
                 source_revision, projection_revision, updated_at)
            SELECT source_type, source_location, source_path,
                   source_revision, projection_revision, COALESCE(MAX(ingested_at), 0)
              FROM raw_events r
             WHERE (r.source_revision, r.projection_revision) = (
                       SELECT r2.source_revision, r2.projection_revision
                         FROM raw_events r2
                        WHERE r2.source_type = r.source_type
                          AND r2.source_location = r.source_location
                          AND r2.source_path = r.source_path
                        ORDER BY r2.source_revision DESC, r2.projection_revision DESC
                        LIMIT 1)
             GROUP BY source_type, source_location, source_path,
                      source_revision, projection_revision;
            "#,
        )?;
        conn.execute_batch(DATA_KEYS_DDL)?;
        drop(conn);
        self.migrate_event_envelopes()?;
        Ok(())
    }

    fn migrate_event_envelopes(&self) -> StoreResult<()> {
        let mut conn = self.conn.lock().unwrap();
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
        let tx = conn.transaction()?;
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
        let head = head_of(&conn, &source_type, &source_location, &source_path)?;
        let (source_revision, projection_revision) = projection.target_revisions(head);
        let tx = conn.transaction()?;
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
                      aad_version, event_type, occurred_at, project_root, event_json)
                   VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)"#,
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
        })
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
    pub fn read_since_page(&self, after_offset: i64, limit: usize) -> StoreResult<ReadPage> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            r#"SELECT r.offset, r.source_type, r.source_location, r.source_path,
                      r.source_session_id, r.seq, r.source_revision, r.projection_revision, r.aad_version,
                      r.project_root, r.event_json
                 FROM raw_events r
                WHERE r.offset > ?1
                  AND NOT EXISTS (
                      SELECT 1 FROM tombstones t
                       WHERE (t.scope = 'session'      AND t.key = r.source_session_id)
                          OR (t.scope = 'source_path'  AND t.key = r.source_path)
                          OR (t.scope = 'project_root' AND t.key = r.project_root)
                  )
                ORDER BY r.offset ASC
                LIMIT ?2"#,
        )?;
        let rows = stmt.query_map(params![after_offset, limit as i64], EncryptedRow::from_sql)?;
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

    /// `read_latest_snapshots` 的当前可见视图：已明确删除的源文件不返回；WSL
    /// 探测暂时失败时保守保留，避免短暂离线被误判成删除。
    pub fn read_active_latest_snapshots(&self) -> StoreResult<Vec<(i64, RawEvent)>> {
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
                SourceLocation::Local => Path::new(&event.source_path).is_file(),
                SourceLocation::Wsl(distro) => existing
                    .get(distro)
                    .and_then(Option::as_ref)
                    .is_none_or(|paths| paths.contains(&event.source_path)),
            })
            .collect())
    }

    fn snapshot_cursor(&self, source: &SourceRef) -> StoreResult<Cursor> {
        let latest = self
            .read_latest_snapshots()?
            .into_iter()
            .map(|(_, event)| event)
            .find(|event| {
                event.source_type == source.source_type
                    && event.source_location == source.source_location
                    && event.source_path == source.path.to_string_lossy()
            });
        let mut cursor = Cursor::new_fingerprint();
        if let Some(event) = latest {
            // 内容未变但宿主补齐/修正了项目身份或 artifact kind 时也要发新版本；
            // 否则早期无身份快照会永久挡住后续规范化元数据。
            if event.project_root == source.project_root
                && event.artifact_kind == source.artifact_kind
            {
                cursor.content_hash = event.content_hash;
            }
            cursor.next_seq = event.seq.saturating_add(1);
        }
        Ok(cursor)
    }

    /// 用 SessionVault 的 snapshot scanner 同步一组已授权来源到总库。宿主只负责
    /// 触发与提供项目身份；发现、读取、指纹、增量和持久化都在本仓完成。
    pub fn sync_snapshots(&self, sources: &[SourceRef]) -> StoreResult<SnapshotSyncStats> {
        let mut stats = SnapshotSyncStats::default();
        for source in sources {
            if source.source_mode != SourceMode::SnapshotFile {
                continue;
            }
            stats.sources += 1;
            let cursor = self.snapshot_cursor(source)?;
            let result = crate::scan::scan_source(source, Some(cursor), Profile::Full);
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
        let tx = conn.transaction()?;
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
    if !path.exists() {
        return Ok(false);
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
    if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)) {
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
mod tests {
    use super::*;
    use crate::rawevent::{Actor, SourceLocation, TimeConfidence, TokenUsage, SCHEMA_VERSION};

    fn mk_event(seq: u64, session: &str, content: Option<&str>) -> RawEvent {
        RawEvent {
            schema_version: SCHEMA_VERSION,
            source_type: SourceType::ClaudeCode,
            source_location: SourceLocation::Local,
            source_path: "/p/file.jsonl".to_string(),
            source_session_id: session.to_string(),
            seq,
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
                (0, 0, "append".to_string()),
                (0, 1, "reparse".to_string()),
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
                events: vec![mk_event(0, "s", Some("v1"))],
            })
            .unwrap();
        assert_eq!(read(&store), 1);

        let stats = store
            .apply_projection(FileProjectionBatch {
                source: source.clone(),
                parser_revision: Some(2),
                mode: Projection::Reparse,
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
                events: vec![mk_event(0, "s", Some("v1"))],
            })
            .unwrap();

        let stats = store
            .apply_projection(FileProjectionBatch {
                source,
                parser_revision: Some(1),
                mode: Projection::Append,
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
            vec![
                (0, Some(1), "append".to_string()),
                (1, Some(2), "reparse".to_string()),
            ]
        );
    }
}
