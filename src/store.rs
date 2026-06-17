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
//! - 永不删/不压缩/不过期是默认保留策略（ADR-016）；用户主动 erase 经 `tombstones` 传播
//!   （本版仅建表 + 读时跳过的脚手架，全量 crypto-shred 见 ADR-027，留后续）。
//! - **MVP 明文**：`content` 明文落 `event_json`（与 QuotaBar `cache.db` 的 `search_text` 同等
//!   本地姿态）；at-rest 加密留作后续独立分支。

use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

use crate::rawevent::{EventType, RawEvent, SourceLocation, SourceType};

/// 总库错误。
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("serde: {0}")]
    Serde(#[from] serde_json::Error),
}

pub type StoreResult<T> = std::result::Result<T, StoreError>;

/// 一次 `append_events` 的结果。`skipped_dup` = 命中 `dedup_key` 唯一约束被忽略的条数
/// （force 全量重扫时旧事件全走这里 → 幂等）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct AppendStats {
    pub appended: u64,
    pub skipped_dup: u64,
    pub max_offset: i64,
}

/// 总库状态（宿主渲染 / 验证用）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct StoreStatus {
    pub count: u64,
    pub max_offset: i64,
    pub last_ingested_at: Option<i64>,
}

/// 不可变 RawEvent 总库句柄。`.clone()` 不提供——单写者持有（ADR-020：同一时刻单写者）；
/// 读者经只读连接或 WAL 并发读，不与写竞争。
pub struct TotalStore {
    conn: Mutex<Connection>,
}

impl TotalStore {
    /// 打开（或新建）磁盘总库，WAL 模式，建表幂等。父目录自动创建。
    ///
    /// **隐私（明文 MVP）**：总库 `event_json` 含会话正文。unix 下把父目录设 `0700`、库文件设
    /// `0600`——共享机器上其它账户不可读（WAL/SHM 在 `0700` 目录内同受保护）。Windows 下
    /// `%LOCALAPPDATA%` 已是按用户隔离，依赖其 ACL。at-rest 加密见 ADR-027（后续）。
    pub fn open(path: &Path) -> StoreResult<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
            restrict_permissions(parent, 0o700);
        }
        let conn = Connection::open(path)?;
        restrict_permissions(path, 0o600);
        Self::from_conn(conn)
    }

    /// 内存库（测试用）。
    pub fn open_in_memory() -> StoreResult<Self> {
        Self::from_conn(Connection::open_in_memory()?)
    }

    fn from_conn(conn: Connection) -> StoreResult<Self> {
        // WAL 让读不挡写（QuotaBar 常驻写、未来 TumeFlow 并发读）。
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        let store = Self {
            conn: Mutex::new(conn),
        };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> StoreResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS raw_events (
                offset            INTEGER PRIMARY KEY AUTOINCREMENT,
                ingested_at       INTEGER NOT NULL,
                schema_version    INTEGER NOT NULL,
                source_type       TEXT    NOT NULL,
                source_location   TEXT    NOT NULL,
                source_path       TEXT    NOT NULL,
                source_session_id TEXT    NOT NULL,
                seq               INTEGER NOT NULL,
                event_type        TEXT    NOT NULL,
                occurred_at       TEXT,
                project_root      TEXT,
                event_json        TEXT    NOT NULL,
                -- 去重唯一键 = 五列复合（§7）。**不**拼成单个 `dedup_key` 串——可变字段里的
                -- 分隔符会歧义碰撞（`/a|b`+`c` 撞 `/a`+`b|c`），UNIQUE 会误判重复静默丢事件。
                UNIQUE (source_type, source_location, source_path, source_session_id, seq)
            );
            CREATE INDEX IF NOT EXISTS idx_raw_events_session ON raw_events(source_session_id);
            CREATE INDEX IF NOT EXISTS idx_raw_events_project ON raw_events(project_root);

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
        Ok(())
    }

    /// 批量追加事件。`INSERT OR IGNORE` 命中五列复合唯一约束即跳过——**幂等**：
    /// force 全量重扫时旧事件全 skip、增量只落新尾。单事务批量插。
    pub fn append_events(&self, events: &[RawEvent]) -> StoreResult<AppendStats> {
        let now = now_unix_secs();
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let mut appended = 0u64;
        let mut skipped_dup = 0u64;
        {
            let mut stmt = tx.prepare(
                r#"INSERT OR IGNORE INTO raw_events
                     (ingested_at, schema_version, source_type, source_location,
                      source_path, source_session_id, seq, event_type, occurred_at,
                      project_root, event_json)
                   VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)"#,
            )?;
            for ev in events {
                let json = serde_json::to_string(ev)?;
                let changed = stmt.execute(params![
                    now,
                    ev.schema_version,
                    source_type_key(ev.source_type),
                    ev.source_location.as_key(),
                    ev.source_path,
                    ev.source_session_id,
                    ev.seq as i64,
                    event_type_key(ev.event_type),
                    ev.occurred_at,
                    ev.project_root,
                    json,
                ])?;
                if changed == 1 {
                    appended += 1;
                } else {
                    skipped_dup += 1;
                }
            }
        }
        tx.commit()?;
        let max_offset = max_offset_on(&conn)?;
        Ok(AppendStats {
            appended,
            skipped_dup,
            max_offset,
        })
    }

    /// 读 `offset` 之后的事件（升序、最多 `limit` 条），跳过被**按作用域**墓碑标记的来源。
    /// 这是最小读 API——验证总库可读，亦是 P3-③ TumeFlow `pull --since` 的种子。
    ///
    /// **韧性**：单行 `event_json` 反序列化失败（损坏 / 未来不兼容 `schema_version`）只 **skip+warn**，
    /// 不让整批 `read_since` 失败。跨版本升级 DTO（把旧 `schema_version` 行 up-convert 到当前）是
    /// 首次破坏性 schema 升级前的前置工作（届时按 `schema_version` 分派解析），当前 v1 单版本不需要。
    pub fn read_since(&self, after_offset: i64, limit: usize) -> StoreResult<Vec<(i64, RawEvent)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            r#"SELECT r.offset, r.event_json
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
        let rows = stmt.query_map(params![after_offset, limit as i64], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (offset, json) = row?;
            match serde_json::from_str::<RawEvent>(&json) {
                Ok(ev) => out.push((offset, ev)),
                Err(e) => log::warn!(
                    target: crate::logging::tag::SQLITE,
                    "raw_events offset={offset} skipped (deserialize failed, likely schema drift): {e}"
                ),
            }
        }
        Ok(out)
    }

    /// 读单个 (file, session) 的全部事件（按 `seq` 升序 = 文件内事件顺序）。作用域**四列精确**：
    /// 一张会话卡 = 一个 `(source_type, source_location, source_path, session_id)` 对——session_id
    /// 可跨文件 replay（Claude `--resume`），故必须连 `source_path` 一起 scope，不能只按 session_id
    /// 串话。供 QuotaBar transcript 从总库重建（不再重读 JSONL）。墓碑此处**不过滤**：transcript 是
    /// 宿主对自己已索引会话的展示，erase 语义作用于下游 pull（`read_since`），不该让某条墓碑令一张
    /// 仍在列表里的卡片打不开。反序列化失败的行 skip+warn（同 `read_since` 韧性口径）。
    pub fn read_session(
        &self,
        source_type: SourceType,
        source_location: &SourceLocation,
        source_path: &str,
        session_id: &str,
    ) -> StoreResult<Vec<RawEvent>> {
        let conn = self.conn.lock().unwrap();
        // 按 `seq`（文件内单调序号 = 气泡顺序）升序，**非** `offset`（append 顺序，乱序重扫时会偏）；
        // `offset` 作次序稳定 tiebreak。
        let mut stmt = conn.prepare(
            r#"SELECT offset, event_json
                 FROM raw_events
                WHERE source_type = ?1
                  AND source_location = ?2
                  AND source_path = ?3
                  AND source_session_id = ?4
                ORDER BY seq ASC, offset ASC"#,
        )?;
        let rows = stmt.query_map(
            params![
                source_type_key(source_type),
                source_location.as_key(),
                source_path,
                session_id,
            ],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )?;
        let mut out = Vec::new();
        for row in rows {
            let (offset, json) = row?;
            match serde_json::from_str::<RawEvent>(&json) {
                Ok(ev) => out.push(ev),
                Err(e) => log::warn!(
                    target: crate::logging::tag::SQLITE,
                    "raw_events offset={offset} skipped (deserialize failed, likely schema drift): {e}"
                ),
            }
        }
        Ok(out)
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
        Ok(StoreStatus {
            count,
            max_offset,
            last_ingested_at,
        })
    }

    /// 写一条**带作用域**墓碑（erase 传播脚手架）。`scope ∈ {session, source_path, project_root}`，
    /// `key` 是该维度下的值；`read_since` 按 scope 精确匹配跳过（避免跨维度误伤）。
    /// 全量 erase（跨分库 + crypto-shred）见 ADR-027，留后续。
    pub fn tombstone(&self, scope: TombstoneScope, key: &str) -> StoreResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO tombstones (scope, key, tombstoned_at) VALUES (?1, ?2, ?3)",
            params![scope.as_str(), key, now_unix_secs()],
        )?;
        Ok(())
    }

    /// 回填标志（写者侧 catch-up 用，见 QuotaBar `refresh_index`）：宿主据此判断总库是否已与
    /// 索引一致。新建库默认 `false` → 宿主触发一次 force 全量回填；任一 append 失败时宿主 `set` 回
    /// `false`，下轮再 force 重发（dedup 幂等补回丢失批）。
    pub fn is_backfilled(&self) -> StoreResult<bool> {
        let conn = self.conn.lock().unwrap();
        let v: Option<String> = conn
            .query_row(
                "SELECT v FROM store_meta WHERE k = 'backfilled'",
                [],
                |r| r.get(0),
            )
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
        store.append_events(&[a1, a0c, b0, t0]).unwrap();

        let got = store
            .read_session(
                SourceType::ClaudeCode,
                &SourceLocation::Local,
                "/a.jsonl",
                "s",
            )
            .unwrap();
        // 只 A 文件的 session s 两条，按 seq 升序。
        assert_eq!(got.len(), 2, "只取 (A, s)，不串 (B, s) / (A, t)");
        assert_eq!(got[0].content.as_deref(), Some("first"));
        assert_eq!(got[1].content.as_deref(), Some("second"));
        assert!(got[0].seq < got[1].seq, "按 seq 升序");

        // 跨文件 replay 的同名 session 各自独立。
        let from_b = store
            .read_session(
                SourceType::ClaudeCode,
                &SourceLocation::Local,
                "/b.jsonl",
                "s",
            )
            .unwrap();
        assert_eq!(from_b.len(), 1);
    }

    #[test]
    fn append_is_idempotent_by_identity() {
        let store = TotalStore::open_in_memory().unwrap();
        let batch = vec![
            mk_event(0, "s1", Some("hello")),
            mk_event(1, "s1", Some("world")),
        ];
        let first = store.append_events(&batch).unwrap();
        assert_eq!(first.appended, 2);
        assert_eq!(first.skipped_dup, 0);

        // 重放同批（force 重扫场景）→ 全部 dedup，count 不变。
        let again = store.append_events(&batch).unwrap();
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
        let stats = store.append_events(&[a, b]).unwrap();
        assert_eq!(stats.appended, 2, "含 `|` 的两条不同身份必须都入库（不碰撞）");
        assert_eq!(store.status().unwrap().count, 2);
    }

    #[test]
    fn offset_is_monotonic_and_read_since_paginates() {
        let store = TotalStore::open_in_memory().unwrap();
        for seq in 0..5u64 {
            store
                .append_events(&[mk_event(seq, "s1", Some(&format!("m{seq}")))])
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
            .append_events(&[mk_event(0, "keep", None), mk_event(0, "drop", None)])
            .unwrap();
        assert_eq!(store.read_since(0, 100).unwrap().len(), 2);
        store.tombstone(TombstoneScope::Session, "drop").unwrap();
        let visible = store.read_since(0, 100).unwrap();
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].1.source_session_id, "keep");
        // 物理仍在（逻辑 append-only，墓碑只读时跳过）。
        assert_eq!(store.status().unwrap().count, 2);
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
        store.append_events(&[by_session, by_project]).unwrap();

        // 只墓碑 project_root=/work → 只隐藏 by_project，不碰 session 名为 /work 的那条。
        store
            .tombstone(TombstoneScope::ProjectRoot, "/work")
            .unwrap();
        let visible = store.read_since(0, 100).unwrap();
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].1.source_session_id, "/work", "session 维度不应被 project 墓碑误伤");
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
            .append_events(&[mk_event(0, "s", None), mk_event(1, "s", None)])
            .unwrap();
        let st = store.status().unwrap();
        assert_eq!(st.count, 2);
        assert_eq!(st.max_offset, stats.max_offset);
        assert!(st.last_ingested_at.is_some());
    }
}
