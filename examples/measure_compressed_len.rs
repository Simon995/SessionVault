//! Read-only timing of local compressed Codex sources already present in a consumer index.
//! Usage: cargo run --release --features store --example measure_compressed_len -- --index <cache.db>
//! Reports anonymous source hashes; never discovers, parses, projects, or prints source content.

use clap::Parser;
use rusqlite::{Connection, OpenFlags};
use serde::Serialize;
use session_vault::deadline::Deadline;
use session_vault::probe::{LocalBackend, Probed};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::time::Instant;

#[derive(Parser)]
struct Args {
    /// Existing consumer index; opened with SQLITE_OPEN_READ_ONLY.
    #[arg(long)]
    index: PathBuf,
    /// Number of slowest compressed sources included in the output.
    #[arg(long, default_value_t = 10)]
    top: usize,
}

#[derive(Serialize)]
struct Measurement {
    source_hash: String,
    compressed_bytes: u64,
    decoded_bytes: u64,
    decoded_len_ms: f64,
    // Inclusive logical-source stat time: metadata probes plus decoded_len_ms.
    source_stat_ms: f64,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let query_started = Instant::now();
    let paths = {
        let conn = Connection::open_with_flags(&args.index, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let mut stmt = conn.prepare(
            "SELECT f.path FROM agent_session_files f \
             WHERE f.provider = 'codex' AND f.location = 'local' \
               AND NOT EXISTS (SELECT 1 FROM archived_sources a \
                 WHERE a.provider = f.provider AND a.location = f.location AND a.path = f.path) \
             ORDER BY f.path",
        )?;
        let paths = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        paths
    };
    let query_ms = query_started.elapsed().as_secs_f64() * 1000.0;
    let backend = LocalBackend::unanchored();
    let deadline = Deadline::unbounded();
    let started = Instant::now();
    let mut measurements = Vec::new();
    let mut plain_sources = 0usize;
    let mut unavailable_sources = 0usize;
    for path in &paths {
        let source_started = Instant::now();
        let logical = PathBuf::from(path);
        // A foreign or relative path must not silently resolve against this checkout.
        if !logical.is_absolute() {
            unavailable_sources += 1;
            continue;
        }
        match backend.stat(&logical, deadline) {
            Probed::Found(_) => {
                plain_sources += 1;
                continue;
            }
            Probed::Absent => {}
            Probed::Unknown(_) => {
                unavailable_sources += 1;
                continue;
            }
        }
        let compressed = session_vault::compressed::compressed_path(&logical);
        let Probed::Found(facts) = backend.stat(&compressed, deadline) else {
            unavailable_sources += 1;
            continue;
        };
        let len_started = Instant::now();
        let Probed::Found(decoded_bytes) =
            session_vault::probe::with_reader(&compressed, |reader| {
                session_vault::compressed::decoded_len(reader)
            })
        else {
            unavailable_sources += 1;
            continue;
        };
        let decoded_len_ms = len_started.elapsed().as_secs_f64() * 1000.0;
        let source_stat_ms = source_started.elapsed().as_secs_f64() * 1000.0;
        measurements.push(Measurement {
            source_hash: format!("{:x}", Sha256::digest(path.as_bytes()))[..12].to_string(),
            compressed_bytes: facts.len,
            decoded_bytes,
            decoded_len_ms,
            source_stat_ms,
        });
    }
    let wall_ms = started.elapsed().as_secs_f64() * 1000.0;
    let decoded_len_ms: f64 = measurements.iter().map(|m| m.decoded_len_ms).sum();
    let source_stat_ms: f64 = measurements.iter().map(|m| m.source_stat_ms).sum();
    let compressed_bytes: u64 = measurements.iter().map(|m| m.compressed_bytes).sum();
    let decoded_bytes: u64 = measurements.iter().map(|m| m.decoded_bytes).sum();
    measurements.sort_by(|a, b| b.decoded_len_ms.total_cmp(&a.decoded_len_ms));
    let report = serde_json::json!({
        "scope": "indexed_non_archived_codex_local_single_pass",
        "profile": if cfg!(debug_assertions) { "debug" } else { "release" },
        "indexed_sources": paths.len(),
        "plain_sources": plain_sources,
        "compressed_sources": measurements.len(),
        "unavailable_sources": unavailable_sources,
        "query_ms": query_ms,
        "wall_ms": wall_ms,
        "decoded_len_ms": decoded_len_ms,
        "source_stat_ms": source_stat_ms,
        "compressed_bytes": compressed_bytes,
        "decoded_bytes": decoded_bytes,
        "slowest": &measurements[..args.top.min(measurements.len())],
    });
    println!("{}", serde_json::to_string_pretty(&report)?);
    if unavailable_sources != 0 {
        return Err(
            "some indexed sources could not be measured; inspect unavailable_sources".into(),
        );
    }
    Ok(())
}
