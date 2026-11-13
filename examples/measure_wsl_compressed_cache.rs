//! Measure EOF scans on a caller-owned synthetic compressed source.
//! A fresh process starts with an empty decoded-length cache; later rounds reuse it.
//! No source, binding file, store, or project registry is written or discovered.
use clap::Parser;
use session_vault::{
    attribution::RootRegistry,
    cursor::Cursor,
    deadline::Deadline,
    discover::SourceRef,
    observation::ParseQuality,
    rawevent::{SourceLocation, SourceMode, SourceType},
    scan::{scan_append_log_with_bindings, StatPrefetch},
    session_binding::SessionBindingRegistry,
    wsl, Profile,
};
use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

#[derive(Parser)]
struct Args {
    distro: String,
    logical_path: String,
    decoded_bytes: u64,
    /// Host directory created by the driver below this checkout's Git agent-logs.
    #[arg(long)]
    fixture_dir: PathBuf,
    #[arg(long, default_value_t = 3)]
    rounds: usize,
}

fn validate_fixture(args: &Args) -> Result<(), String> {
    use session_vault::probe::{lstat_entry, Probed, RealEntryKind};
    use std::{path::Component, process::Command};

    let git = Command::new("git")
        .args([
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "agent-logs",
        ])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .map_err(|e| format!("cannot locate fixture boundary: {e}"))?;
    if !git.status.success() {
        return Err("cannot locate this checkout's Git agent-logs".into());
    }
    let root = PathBuf::from(
        String::from_utf8(git.stdout)
            .map_err(|e| e.to_string())?
            .trim(),
    );
    let relative = args
        .fixture_dir
        .strip_prefix(&root)
        .map_err(|_| "fixture must be below this checkout's Git agent-logs")?;
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err("fixture must be a strict descendant without traversal".into());
    }
    let mut dir = root;
    for component in std::iter::once(None).chain(relative.components().map(Some)) {
        if let Some(component) = component {
            dir.push(component.as_os_str());
        }
        if !matches!(
            lstat_entry(&dir, None),
            Probed::Found(RealEntryKind::Directory)
        ) {
            return Err("fixture directory must exist without links".into());
        }
    }
    let name = format!("synthetic-{}.jsonl", args.decoded_bytes);
    if !matches!(lstat_entry(&dir.join(&name), None), Probed::Absent)
        || !matches!(
            lstat_entry(&dir.join(format!("{name}.zst")), None),
            Probed::Found(RealEntryKind::RegularFile(_))
        )
    {
        return Err("fixture must contain only the regular compressed synthetic source".into());
    }
    // Convert the guarded host directory independently before any source access.
    let mut child = Command::new("wsl.exe")
        .args(["-d", &args.distro, "--exec", "wslpath", "-a", "-u"])
        .arg(&dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("fixture path conversion failed: {e}"))?;
    let deadline = Deadline::after(Duration::from_secs(30));
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if !deadline.expired() => std::thread::sleep(Duration::from_millis(10)),
            result => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "fixture path conversion did not complete: {result:?}"
                ));
            }
        }
    }
    let converted = child.wait_with_output().map_err(|e| e.to_string())?;
    if !converted.status.success() {
        return Err("fixture path conversion failed".into());
    }
    let mounted = String::from_utf8(converted.stdout).map_err(|e| e.to_string())?;
    if args.logical_path != format!("{}/{name}", mounted.trim().trim_end_matches('/')) {
        return Err("source must be the generated file in the guarded fixture".into());
    }
    Ok(())
}

fn main() -> Result<(), String> {
    let args = Args::parse();
    if args.rounds < 2 || args.decoded_bytes == 0 || !args.logical_path.starts_with('/') {
        return Err(
            "need at least two rounds, positive decoded size and absolute synthetic path".into(),
        );
    }
    validate_fixture(&args)?;
    env_logger::Builder::new()
        .parse_filters("sv-scan=debug,sv-wsl=debug")
        .format_timestamp(None)
        .init();
    let source = SourceRef {
        source_type: SourceType::Codex,
        source_location: SourceLocation::Wsl(args.distro),
        source_mode: SourceMode::AppendLog,
        path: PathBuf::from(args.logical_path),
        project_root: None,
        artifact_kind: None,
    };
    let roots = Arc::new(RootRegistry::new());
    let bindings = SessionBindingRegistry::default();
    let mut cursor = Cursor::new_byte_offset();
    cursor.safe_offset = args.decoded_bytes;
    cursor.size = args.decoded_bytes;
    for round in 1..=args.rounds {
        eprintln!("synthetic round begin: round={round}");
        let deadline = Deadline::after(Duration::from_secs(60));
        let before = wsl::spawn_count();
        let started = Instant::now();
        let prefetch = StatPrefetch::for_sources(std::slice::from_ref(&source), deadline);
        let prefetch_ms = started.elapsed().as_secs_f64() * 1000.0;
        let scan_started = Instant::now();
        let (observation, report) = scan_append_log_with_bindings(
            &source,
            Some(cursor),
            None,
            Profile::Metadata,
            roots.clone(),
            deadline,
            &prefetch,
            &bindings,
        );
        let scan_ms = scan_started.elapsed().as_secs_f64() * 1000.0;
        if !matches!(
            observation.quality,
            ParseQuality::Clean {
                deferred_tail_bytes: 0
            }
        ) || report.bytes_read != 0
            || !observation.events.is_empty()
            || observation.cursor.safe_offset != args.decoded_bytes
            || observation.cursor.size != args.decoded_bytes
        {
            return Err(format!(
                "round {round} did not remain at EOF: {:?}; {:?}",
                observation.quality, report.warnings
            ));
        }
        cursor = observation.cursor;
        eprintln!("synthetic round end: round={round}");
        println!(
            "{}",
            serde_json::json!({
                "round": round, "prefetch_ms": prefetch_ms, "scan_ms": scan_ms,
                "wall_ms": started.elapsed().as_secs_f64() * 1000.0,
                "spawn_attempts": wsl::spawn_count() - before,
                "bytes_read": report.bytes_read, "events": observation.events.len(),
                "logical_bytes": cursor.size, "safe_offset": cursor.safe_offset,
            })
        );
    }
    Ok(())
}
