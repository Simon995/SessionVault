//! Synthetic disk-fixture baseline for the public binding guard and consumer ledger SQL.
//! Creates only a NEW scratch directory, uses a synthetic key, and never opens a real store.
//! Ledger statements mirror the consumer's current SQL, not its Db methods or lock contention.

use clap::Parser;
use rusqlite::{params, Connection};
use serde::Serialize;
use session_vault::deadline::Deadline;
use session_vault::probe::{self, LocalBackend, ProbeBackend, Probed};
use session_vault::rawevent::{SourceLocation, SourceType};
use session_vault::{FileProjectionBatch, Projection, SourceKey, StoreKey, TotalStore};
use std::path::PathBuf;
use std::time::Instant;

#[derive(Parser)]
struct Args {
    /// New scratch directory; refuses any existing or uncertain path.
    #[arg(long)]
    scratch: PathBuf,
    #[arg(long, default_value_t = 1000)]
    sources: usize,
    #[arg(long, default_value_t = 3)]
    rounds: usize,
}

#[derive(Default, Serialize)]
struct Round {
    mode: &'static str,
    round: usize,
    sources: usize,
    wall_ms: f64,
    prior_projection_ms: f64,
    desired_ms: f64,
    guard_ms: f64,
    // Nested in guard_ms; do not add these two fields.
    index_ack_ms: f64,
    store_ack_ms: f64,
}

fn ms(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.0
}

fn ack(conn: &Connection, path: &str, column: &str, now: i64) -> rusqlite::Result<()> {
    let affected = conn.execute(
        &format!(
            "UPDATE source_sync_state SET {column} = ?4, updated_at = ?5 \
                  WHERE provider = ?1 AND location = ?2 AND source_path = ?3 \
                    AND desired_token = ?4"
        ),
        params!["claude", "local", path, "fixture-token", now],
    )?;
    if affected != 1 {
        return Err(rusqlite::Error::StatementChangedRows(affected));
    }
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    if args.sources == 0 || args.rounds == 0 {
        return Err("sources and rounds must be nonzero".into());
    }
    if std::env::var_os("SVAULT_SESSION_BINDINGS").is_some() {
        return Err("unset SVAULT_SESSION_BINDINGS for this isolated fixture".into());
    }
    match LocalBackend::unanchored().probe(&args.scratch, Deadline::unbounded()) {
        Probed::Absent => {}
        Probed::Found(_) | Probed::Unknown(_) => {
            return Err("scratch directory must be new and confirmed absent".into());
        }
    }
    probe::create_dir_all(&args.scratch)?;
    // An active synthetic rule pins file lookup to the fixture instead of global defaults.
    probe::write_bytes(
        &args.scratch.join("session_bindings.json"),
        br#"{"schemaVersion":1,"temporaryRoots":["/__fixture_excluded__"],"bindings":[]}"#,
    )?;
    let store = TotalStore::open_with_key(
        &args.scratch.join("store.db"),
        StoreKey::from_bytes([42; 32]),
    )?;
    let bindings = store.load_session_bindings();
    bindings.fingerprint()?;
    let ledger = Connection::open(args.scratch.join("ledger.db"))?;
    ledger.pragma_update(None, "journal_mode", "WAL")?;
    ledger.pragma_update(None, "synchronous", "NORMAL")?;
    ledger.execute_batch(
        "CREATE TABLE source_sync_state (
             provider TEXT NOT NULL, location TEXT NOT NULL, source_path TEXT NOT NULL,
             desired_token TEXT, store_ack_token TEXT, index_ack_token TEXT,
             attempt TEXT NOT NULL, attempt_detail TEXT, quality TEXT NOT NULL,
             quality_detail TEXT, updated_at INTEGER NOT NULL,
             PRIMARY KEY (provider, location, source_path));",
    )?;
    let keys: Vec<_> = (0..args.sources)
        .map(|i| SourceKey {
            source_type: SourceType::ClaudeCode,
            source_location: SourceLocation::Local,
            source_path: format!("/__fixture_sources__/{i}.jsonl"),
        })
        .collect();
    for source in &keys {
        store.apply_projection(FileProjectionBatch {
            source: source.clone(),
            mode: Projection::Append,
            parser_revision: Some(session_vault::PARSER_REVISION),
            token: None,
            events: Vec::new(),
        })?;
        ledger.execute(
            "INSERT INTO source_sync_state VALUES \
             ('claude', 'local', ?1, 'fixture-token', 'fixture-token', 'fixture-token', \
              'pending', NULL, 'clean', NULL, 0)",
            params![source.source_path],
        )?;
    }
    let mut rounds = Vec::new();
    for with_ledger in [false, true] {
        for round in 1..=args.rounds {
            // Force changed timestamps, as in separate production rounds, despite fast replay.
            let now = round as i64;
            let mut timing = Round {
                mode: if with_ledger {
                    "guard_and_ledger"
                } else {
                    "guard_only"
                },
                round,
                sources: keys.len(),
                ..Default::default()
            };
            let started = Instant::now();
            for source in &keys {
                let at = Instant::now();
                if !store.has_projection(source)? {
                    return Err("fixture source has no current projection".into());
                }
                timing.prior_projection_ms += ms(at);
                if with_ledger {
                    let at = Instant::now();
                    ledger.execute(
                        "INSERT INTO source_sync_state \
                         (provider, location, source_path, desired_token, attempt, quality, updated_at) \
                         VALUES (?1, ?2, ?3, ?4, 'pending', 'clean', ?5) \
                         ON CONFLICT(provider, location, source_path) DO UPDATE SET \
                         desired_token = ?4, attempt = 'pending', updated_at = ?5",
                        params!["claude", "local", source.source_path, "fixture-token", now],
                    )?;
                    timing.desired_ms += ms(at);
                }
                let at = Instant::now();
                store.with_session_bindings(&bindings, |_| {
                    if with_ledger {
                        let ack_started = Instant::now();
                        ack(&ledger, &source.source_path, "index_ack_token", now)?;
                        timing.index_ack_ms += ms(ack_started);
                    }
                    Ok(())
                })?;
                timing.guard_ms += ms(at);
                if with_ledger {
                    let at = Instant::now();
                    ack(&ledger, &source.source_path, "store_ack_token", now)?;
                    timing.store_ack_ms += ms(at);
                }
            }
            timing.wall_ms = ms(started);
            rounds.push(timing);
        }
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "scope": "synthetic_disk_fixture_no_production_contention",
            "profile": if cfg!(debug_assertions) { "debug" } else { "release" },
            "ledger": "current_consumer_sql_reproduction_not_Db_methods",
            "guard": "public_TotalStore_with_session_bindings",
            "rounds": rounds,
        }))?
    );
    Ok(())
}
