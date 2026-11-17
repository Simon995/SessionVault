//! Measure production WSL project-identity probing on caller-owned synthetic roots.
//! Supports both single-root `identity::repo_id_for_root` and in-memory
//! `TotalStore::sweep_registered_root_identities`. No real store, keychain, or
//! non-fixture directory is accessed.
use clap::{Parser, ValueEnum};
use serde::Serialize;
use session_vault::{
    attribution::RootSource,
    deadline::Deadline,
    identity,
    probe::{lstat_entry, LocalBackend, ProbeBackend, Probed, RealEntryKind},
    wsl, TotalStore,
};
use std::{
    path::{Component, Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, ValueEnum)]
enum Mode {
    Single,
    Sweep,
}

#[derive(Parser)]
struct Args {
    #[arg(long, default_value = "-")]
    distro: String,
    /// Host fixture directory strictly below this checkout's Git agent-logs.
    #[arg(long)]
    fixture_dir: PathBuf,
    /// File under `fixture_dir` listing canonical `wsl:<distro>:<abs-path>` roots (one per line).
    #[arg(long)]
    roots_file: PathBuf,
    #[arg(long, value_enum)]
    mode: Mode,
}

#[derive(Serialize)]
struct SingleRow {
    root: String,
    id: String,
    repo_root: Option<String>,
    checkout_missing: bool,
}

#[derive(Serialize)]
struct SweepSummary {
    registered: usize,
    recorded: usize,
    already_probed: usize,
    no_remote: usize,
    checkout_missing: usize,
    unresolved: usize,
    skipped_out_of_budget: usize,
    unreadable: Option<String>,
}

#[derive(Serialize)]
struct ReportRow {
    root_path: String,
    canonical_id: Option<String>,
    verdict: String,
    detail: Option<String>,
}

fn resolve_distro(arg: &str) -> Result<String, String> {
    if arg != "-" {
        return Ok(arg.to_string());
    }
    let distros = wsl::list_distros(Deadline::after(Duration::from_secs(15)))?;
    wsl::default_distro(&distros).ok_or_else(|| "need one user distro or explicit --distro".into())
}

fn validate_strict_descendant(
    base: &Path,
    candidate: &Path,
    label: &str,
) -> Result<PathBuf, String> {
    let rel = candidate
        .strip_prefix(base)
        .map_err(|_| format!("{label} must be below {}", base.display()))?;
    if rel.as_os_str().is_empty()
        || rel
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(format!(
            "{label} must be a strict descendant without traversal"
        ));
    }
    Ok(rel.to_path_buf())
}

fn validate_fixture_and_load_roots(args: &Args, distro: &str) -> Result<Vec<String>, String> {
    let git = Command::new("git")
        .args([
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "agent-logs",
        ])
        .output()
        .map_err(|e| format!("cannot locate fixture boundary: {e}"))?;
    if !git.status.success() {
        return Err("cannot locate this checkout's Git agent-logs".into());
    }
    let log_root = PathBuf::from(
        String::from_utf8(git.stdout)
            .map_err(|e| e.to_string())?
            .trim(),
    );
    let rel_fixture = validate_strict_descendant(&log_root, &args.fixture_dir, "fixture_dir")?;
    let mut cur = log_root;
    for part in std::iter::once(None).chain(rel_fixture.components().map(Some)) {
        if let Some(part) = part {
            cur.push(part.as_os_str());
        }
        if !matches!(
            lstat_entry(&cur, None),
            Probed::Found(RealEntryKind::Directory)
        ) {
            return Err("fixture directory must exist without links".into());
        }
    }
    let rel_roots = validate_strict_descendant(&cur, &args.roots_file, "roots_file")?;
    if rel_roots.components().count() != 1
        || !matches!(
            lstat_entry(&args.roots_file, Some(&cur)),
            Probed::Found(RealEntryKind::RegularFile(_))
        )
    {
        return Err("roots_file must be a direct regular file inside fixture_dir".into());
    }

    let mut child = Command::new("wsl.exe")
        .args(["-d", distro, "--exec", "wslpath", "-a", "-u"])
        .arg(&cur)
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
    let mounted = String::from_utf8(converted.stdout)
        .map_err(|e| e.to_string())?
        .trim()
        .trim_end_matches('/')
        .to_string();
    if !mounted.starts_with('/') || mounted.is_empty() {
        return Err("converted WSL fixture path is not absolute".into());
    }

    let raw = match LocalBackend::unanchored().read_text(&args.roots_file, Deadline::unbounded()) {
        Probed::Found(t) => t,
        Probed::Absent => return Err("roots_file disappeared".into()),
        Probed::Unknown(e) => return Err(format!("cannot read roots_file: {e}")),
    };
    let expected_prefix = format!("wsl:{distro}:{mounted}/");
    let mut roots = Vec::new();
    for line in raw.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let Some(suffix) = line.strip_prefix(&expected_prefix) else {
            return Err(format!("root outside guarded fixture prefix: {line}"));
        };
        if suffix.is_empty()
            || suffix
                .split('/')
                .any(|seg| seg.is_empty() || seg == "." || seg == "..")
        {
            return Err(format!("invalid relative fixture root segment: {line}"));
        }
        roots.push(line.to_string());
    }
    if roots.is_empty() {
        return Err("roots_file must contain at least one root".into());
    }
    if matches!(args.mode, Mode::Single) && roots.len() != 1 {
        return Err("single mode requires exactly one root".into());
    }
    Ok(roots)
}

fn main() -> Result<(), String> {
    let args = Args::parse();
    if std::env::var_os("SVAULT_LOG").is_some() || std::env::var_os("RUST_LOG").is_some() {
        env_logger::Builder::from_env(env_logger::Env::default().filter_or("SVAULT_LOG", "info"))
            .format_timestamp(None)
            .init();
    }
    let distro = resolve_distro(&args.distro)?;
    let roots = validate_fixture_and_load_roots(&args, &distro)?;
    let mounts = Vec::new();
    let deadline = Deadline::after(Duration::from_secs(60));

    match args.mode {
        Mode::Single => {
            let root = &roots[0];
            let before = wsl::spawn_count();
            let started = Instant::now();
            let got = identity::repo_id_for_root(root, Some(&distro), &mounts, deadline)
                .map_err(|e| e.to_string())?;
            let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
            let spawns = wsl::spawn_count() - before;
            println!(
                "{}",
                serde_json::json!({
                    "mode": "single",
                    "elapsed_ms": elapsed_ms,
                    "spawn_attempts": spawns,
                    "result": SingleRow {
                        root: root.clone(),
                        id: got.id,
                        repo_root: got.repo_root,
                        checkout_missing: got.checkout_missing,
                    },
                })
            );
        }
        Mode::Sweep => {
            let store = TotalStore::open_in_memory().map_err(|e| e.to_string())?;
            for root in &roots {
                store.register_project_root(root, RootSource::Scan);
            }
            let before = wsl::spawn_count();
            let started = Instant::now();
            let sweep = store.sweep_registered_root_identities(Some(&distro), &mounts, deadline);
            let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
            let spawns = wsl::spawn_count() - before;
            let (rows, _) = store.project_roots_report(&mounts)?;
            let report: Vec<ReportRow> = rows
                .into_iter()
                .map(|r| ReportRow {
                    root_path: r.root_path,
                    canonical_id: r.canonical_id,
                    verdict: r.identity_verdict.as_str().to_string(),
                    detail: r.identity_verdict.why().map(str::to_string),
                })
                .collect();
            println!(
                "{}",
                serde_json::json!({
                    "mode": "sweep",
                    "elapsed_ms": elapsed_ms,
                    "spawn_attempts": spawns,
                    "sweep": SweepSummary {
                        registered: sweep.registered,
                        recorded: sweep.recorded,
                        already_probed: sweep.already_probed,
                        no_remote: sweep.no_remote,
                        checkout_missing: sweep.checkout_missing,
                        unresolved: sweep.unresolved,
                        skipped_out_of_budget: sweep.skipped_out_of_budget,
                        unreadable: sweep.unreadable,
                    },
                    "report": report,
                })
            );
        }
    }
    Ok(())
}
