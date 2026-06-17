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

use rusqlite::{params, Connection};
use serde::Serialize;

use crate::rawevent::{EventType, RawEvent, SourceType};

/// 总库错误。
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
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
    pub fn open(path: &Path) -> StoreResult<Self> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let conn = Connection::open(path)?;
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
                dedup_key         TEXT    NOT NULL UNIQUE,
                schema_version    INTEGER NOT NULL,
                source_type       TEXT    NOT NULL,
                source_location   TEXT    NOT NULL,
                source_path       TEXT    NOT NULL,
                source_session_id TEXT    NOT NULL,
                seq               INTEGER NOT NULL,
                event_type        TEXT    NOT NULL,
                occurred_at       TEXT,
                project_root      TEXT,
                event_json        TEXT    NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_raw_events_session ON raw_events(source_session_id);
            CREATE INDEX IF NOT EXISTS idx_raw_events_project ON raw_events(project_root);

            CREATE TABLE IF NOT EXISTS tombstones (
                key           TEXT    PRIMARY KEY,
                tombstoned_at INTEGER NOT NULL
            );
            "#,
        )?;
        Ok(())
    }

    /// 批量追加事件。`INSERT OR IGNORE` 命中 `dedup_key` 唯一约束即跳过——**幂等**：
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
                     (ingested_at, dedup_key, schema_version, source_type, source_location,
                      source_path, source_session_id, seq, event_type, occurred_at,
                      project_root, event_json)
                   VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)"#,
            )?;
            for ev in events {
                let json = serde_json::to_string(ev)?;
                let changed = stmt.execute(params![
                    now,
                    ev.dedup_key(),
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

    /// 读 `offset` 之后的事件（升序、最多 `limit` 条），跳过被墓碑标记的来源。
    /// 这是最小读 API——验证总库可读，亦是 P3-③ TumeFlow `pull --since` 的种子。
    pub fn read_since(&self, after_offset: i64, limit: usize) -> StoreResult<Vec<(i64, RawEvent)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            r#"SELECT r.offset, r.event_json
                 FROM raw_events r
                WHERE r.offset > ?1
                  AND NOT EXISTS (
                      SELECT 1 FROM tombstones t
                       WHERE t.key IN (r.source_session_id, r.source_path, r.project_root)
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
            out.push((offset, serde_json::from_str::<RawEvent>(&json)?));
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

    /// 写一条墓碑（erase 传播脚手架）。`key` 可为 `source_session_id` / `source_path` /
    /// `project_root`；`read_since` 据此跳过。全量 erase（跨分库 + crypto-shred）见 ADR-027，留后续。
    pub fn tombstone(&self, key: &str) -> StoreResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO tombstones (key, tombstoned_at) VALUES (?1, ?2)",
            params![key, now_unix_secs()],
        )?;
        Ok(())
    }
}

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

    #[test]
    fn append_is_idempotent_by_dedup_key() {
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
        store.tombstone("drop").unwrap();
        let visible = store.read_since(0, 100).unwrap();
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].1.source_session_id, "keep");
        // 物理仍在（逻辑 append-only，墓碑只读时跳过）。
        assert_eq!(store.status().unwrap().count, 2);
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
