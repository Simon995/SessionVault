//! Measure the production WSL whole-file bridge on a caller-owned synthetic file.
//! Usage: measure_wsl_read_whole <distro|-> <absolute-wsl-path>
//! `-` requires exactly one user distro. No file is created or changed.
use session_vault::{deadline::Deadline, wsl};
use sha2::{Digest, Sha256};
use std::time::{Duration, Instant};

fn main() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() != 2 || !args[1].starts_with('/') {
        return Err("expected <distro|-> <absolute-wsl-path>".into());
    }
    let distro = if args[0] == "-" {
        let distros = wsl::list_distros(Deadline::after(Duration::from_secs(10)))?;
        wsl::default_distro(&distros).ok_or("need exactly one user distro or an explicit name")?
    } else {
        args[0].clone()
    };
    let before = wsl::spawn_count();
    let started = Instant::now();
    let read = wsl::read_whole(&distro, &args[1], Deadline::after(Duration::from_secs(60)))?;
    let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
    let spawns = wsl::spawn_count() - before;
    let (bytes, mtime, digest) = match read {
        Some(read) => (
            Some(read.bytes.len()),
            Some(read.mtime),
            Some(format!("{:x}", Sha256::digest(&read.bytes))),
        ),
        None => (None, None, None),
    };
    println!(
        "{}",
        serde_json::json!({"elapsed_ms": elapsed_ms, "spawn_attempts": spawns,
            "bytes": bytes, "mtime": mtime, "sha256": digest})
    );
    Ok(())
}
