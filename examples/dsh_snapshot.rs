//! 核验显式给出的 DSH v4 物理快照、会话目录、归一化位置及实验性总库提交，不自动发现来源。
//! cargo run --example dsh_snapshot --features dsh-experimental -- --path <absolute-file> --compression zstd --positions
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{ArgGroup, Parser, ValueEnum};
use serde_json::{json, Value};
use session_vault::deadline::Deadline;
use session_vault::dsh_snapshot::{
    capture, capture_source, discover_dsh_sessions_in_root, evaluate_commit_gate,
    normalize_admitted_snapshot, normalize_source_snapshot, Blocked, Compression,
    DshNormalizeConfig, NativeRuntime, SourceConfig, SourceSnapshot,
};

#[derive(Clone, Copy, ValueEnum)]
enum Encoding {
    None,
    Zstd,
}

#[derive(Parser)]
#[command(group(
    ArgGroup::new("target")
        .required(true)
        .args(["path", "session_dir", "sessions_root"]),
))]
struct Args {
    #[arg(long)]
    path: Option<PathBuf>,
    #[arg(
        long,
        requires_all = ["source_instance", "node", "dsh_package"],
        conflicts_with_all = ["path", "sessions_root"]
    )]
    session_dir: Option<PathBuf>,
    #[arg(long, conflicts_with_all = ["path", "session_dir"])]
    sessions_root: Option<PathBuf>,
    #[arg(long, requires = "session_dir")]
    source_instance: Option<String>,
    #[arg(long, requires = "session_dir")]
    session_id: Option<String>,
    #[arg(long, requires = "session_dir", conflicts_with = "compare_session_dir")]
    follow_stdin: bool,
    #[arg(long, requires = "session_dir")]
    compare_session_dir: Option<PathBuf>,
    #[arg(long, requires = "compare_session_dir")]
    compare_source_instance: Option<String>,
    #[arg(long, value_enum)]
    compression: Encoding,
    /// 输出字节范围、原生 seq，并在 --normalize 时输出归一化跨度与 usage 台账（不输出对话正文）。
    #[arg(long)]
    positions: bool,
    /// 执行 DSH 原生记录到 RawEvent 归一化位置映射。
    #[arg(long, requires = "node")]
    normalize: bool,
    /// 显式启用本机原生 admission；Node 与安装包路径必须成对给出。
    #[arg(long, requires = "dsh_package")]
    node: Option<PathBuf>,
    #[arg(long, requires = "node")]
    dsh_package: Option<PathBuf>,
    #[arg(long, requires = "node")]
    native_timeout_ms: Option<u64>,
    #[cfg(feature = "store")]
    #[arg(long, requires = "state_file")]
    store_db: Option<PathBuf>,
    #[cfg(feature = "store")]
    #[arg(long, requires = "store_db")]
    state_file: Option<PathBuf>,
    #[cfg(feature = "store")]
    #[arg(long, requires = "store_db")]
    bindings_file: Option<PathBuf>,
    #[cfg(feature = "store")]
    #[arg(long, requires = "store_db")]
    force: bool,
    #[cfg(feature = "store")]
    #[arg(long, requires = "store_db")]
    simulate_crash_before_save_state: bool,
}

fn make_deadline(timeout_ms: Option<u64>) -> Deadline {
    timeout_ms
        .map(|ms| Deadline::after(Duration::from_millis(ms)))
        .unwrap_or_else(Deadline::unbounded)
}

fn write_json_line(value: &Value) -> bool {
    let mut stdout = io::stdout().lock();
    serde_json::to_writer(&mut stdout, value).is_ok()
        && writeln!(stdout).is_ok()
        && stdout.flush().is_ok()
}

fn run_file_mode(args: Args, encoding: Compression) -> Result<Value, Blocked> {
    let path = args.path.expect("checked by clap group");
    let deadline = make_deadline(args.native_timeout_ms);
    let snapshot = capture(&path, encoding)?;
    if let (Some(node), Some(package)) = (args.node, args.dsh_package) {
        let admitted = NativeRuntime::new(node, package)?.admit(snapshot, deadline)?;
        let base_report = admitted.report(args.positions);
        if args.normalize {
            let norm_cfg = DshNormalizeConfig::local_full(path.display().to_string());
            let normalized = normalize_admitted_snapshot(&admitted, &norm_cfg)?;
            Ok(normalized.report(base_report, args.positions))
        } else {
            Ok(base_report)
        }
    } else {
        Ok(snapshot.report(args.positions))
    }
}

fn format_source_report(
    snapshot: &SourceSnapshot,
    include_positions: bool,
    normalize: bool,
) -> Result<Value, Blocked> {
    let base_report = snapshot.report(include_positions);
    if normalize {
        let norm_cfg = DshNormalizeConfig::local_full(snapshot.session_dir().display().to_string());
        let normalized = normalize_source_snapshot(snapshot, &norm_cfg)?;
        Ok(normalized.report(base_report, include_positions))
    } else {
        Ok(base_report)
    }
}

fn step_report(
    prior: &SourceSnapshot,
    prior_report: &Value,
    recheck: Value,
    next: Result<SourceSnapshot, Blocked>,
    include_positions: bool,
    normalize: bool,
) -> Value {
    let gate = evaluate_commit_gate(Some(prior), next.as_ref());
    match next {
        Ok(current) => {
            let relation = prior.compare(&current);
            let current_report = format_source_report(&current, include_positions, normalize)
                .unwrap_or_else(|b| serde_json::to_value(b).unwrap());
            json!({
                "kind": "source_step_evaluated",
                "prior_recheck": recheck,
                "relation": relation.kind_str(),
                "relation_detail": relation,
                "commit_gate": gate,
                "current": current_report,
                "prior_native_events": prior.native_events(),
                "prior_snapshot_sha256": prior_report["snapshot_sha256"],
                "production_advance_permit": false,
            })
        }
        Err(blocked) => json!({
            "kind": "source_step_blocked",
            "prior_recheck": recheck,
            "blocked": blocked,
            "commit_gate": gate,
            "prior_native_events": prior.native_events(),
            "prior_snapshot_sha256": prior_report["snapshot_sha256"],
            "production_advance_permit": false,
        }),
    }
}

#[cfg(feature = "store")]
#[allow(clippy::disallowed_methods)]
fn run_store_mode(
    args: &Args,
    cfg: &SourceConfig,
    runtime: &NativeRuntime,
    session_dir: &std::path::Path,
) -> ExitCode {
    use session_vault::dsh_snapshot::{commit_dsh_source_step, DshSavedState};
    use session_vault::scan_plan::{
        CursorAction, IndexAction, ScanReasons, StoreAction, SyncTransition,
    };
    use session_vault::{Projection, StoreKey, TotalStore};

    let store_path = args.store_db.as_ref().expect("store_db required");
    let state_path = args.state_file.as_ref().expect("state_file required");
    let key = StoreKey::from_bytes([0x5a_u8; 32]);
    let store = match TotalStore::open_with_key(store_path, key) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("failed to open store: {e}");
            return ExitCode::FAILURE;
        }
    };

    if let Some(ref bpath) = args.bindings_file {
        let raw = match std::fs::read_to_string(bpath) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("failed to read bindings_file: {e}");
                return ExitCode::FAILURE;
            }
        };
        let reg = session_vault::SessionBindingRegistry::from_json_str(&raw);
        if let Err(e) = store.set_session_bindings(&reg) {
            eprintln!("failed to set session bindings: {e}");
            return ExitCode::FAILURE;
        }
    }

    let prior_state: Option<DshSavedState> = if state_path.exists() {
        match std::fs::read_to_string(state_path)
            .ok()
            .and_then(|raw| serde_json::from_str::<DshSavedState>(&raw).ok())
        {
            Some(saved) => Some(saved),
            None => {
                eprintln!("failed to parse state_file");
                return ExitCode::FAILURE;
            }
        }
    } else {
        None
    };

    let current = capture_source(cfg, runtime, make_deadline(args.native_timeout_ms));
    let mut reasons = ScanReasons::NONE;
    if args.force {
        reasons |= ScanReasons::FORCE;
    }
    let norm_cfg = DshNormalizeConfig::local_full(session_dir.display().to_string());
    let outcome = match commit_dsh_source_step(
        &store,
        prior_state.as_ref(),
        current.as_ref(),
        reasons,
        &norm_cfg,
    ) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("commit_dsh_source_step error: {e}");
            return ExitCode::FAILURE;
        }
    };

    let mut persisted_state = false;
    if outcome.advanced_saved_state && !args.simulate_crash_before_save_state {
        if let Some(ref next_state) = outcome.next_state {
            let serialized = serde_json::to_string_pretty(next_state).unwrap();
            if std::fs::write(state_path, serialized).is_err() {
                eprintln!("failed to write state_file");
                return ExitCode::FAILURE;
            }
            persisted_state = true;
        }
    }

    let source_path_str = session_dir.display().to_string();
    let head = store
        .current_head("jsonl", "local", &source_path_str)
        .unwrap_or((0, 0));
    let current_page = store.read_current_since_page(0, 10_000).unwrap();
    let all_page = store.read_since_page(0, 10_000).unwrap();

    let index_str = match outcome.planned.plan.index() {
        IndexAction::ReplaceFile => "replace_file",
        IndexAction::AppendFacts => "append_facts",
        IndexAction::Preserve => "preserve",
    };
    let store_str = match outcome.planned.plan.store() {
        StoreAction::Preserve => "preserve",
        StoreAction::Project(Projection::Append) => "project_append",
        StoreAction::Project(Projection::Reparse) => "project_reparse",
        StoreAction::Project(Projection::Rollback) => "project_rollback",
    };
    let cursor_str = match outcome.planned.plan.cursor() {
        CursorAction::Advance => "advance",
        CursorAction::Freeze => "freeze",
    };
    let sync_str = match outcome.planned.plan.sync() {
        SyncTransition::Advance => "advance",
        SyncTransition::Hold => "hold",
    };

    let stats_val = outcome.projection_stats.map(|st| {
        json!({
            "appended": st.appended,
            "skipped_dup": st.skipped_dup,
            "skipped_erased": st.skipped_erased,
            "source_revision": st.source_revision,
            "projection_revision": st.projection_revision,
            "head_moved": st.head_moved,
            "superseded_removed": st.superseded_removed,
        })
    });

    let mut report = json!({
        "kind": "dsh_store_step_committed",
        "relation": outcome.planned.relation.as_ref().map(|r| r.kind_str()),
        "relation_detail": outcome.planned.relation,
        "commit_gate": outcome.planned.gate,
        "admission_status": outcome.planned.admission_status,
        "admission_detail": outcome.planned.admission_detail,
        "events_excluded_temporary": outcome.planned.events_excluded_temporary,
        "plan": {
            "index": index_str,
            "store": store_str,
            "cursor": cursor_str,
            "sync": sync_str,
            "quality": outcome.planned.plan.quality().key(),
        },
        "observed_events_in_batch": outcome.planned.observation.events.len(),
        "projection_stats": stats_val,
        "advanced_saved_state": outcome.advanced_saved_state,
        "persisted_state_file": persisted_state,
        "store_head": {
            "source_revision": head.0,
            "projection_revision": head.1,
        },
        "store_current_events": current_page.events.len(),
        "store_total_events_all_generations": all_page.events.len(),
    });
    if args.positions {
        if let Some(ref norm) = outcome.planned.normalized {
            report["normalized_event_spans"] = json!(norm.spans);
            report["usage_ledger"] = json!(norm.usage_ledger);
            report["subagent_descriptors"] = json!(norm.subagent_descriptors);
            report["subagent_catalog"] = json!(norm.subagent_catalog);
            report["compaction_records"] = json!(norm.compaction_records);
            report["surface_replacements"] = json!(norm.surface_replacements);
            report["active_surface_native_seqs"] = json!(norm.active_surface_native_seqs);
            let user_origins: Vec<Value> = norm
                .events
                .iter()
                .filter_map(|ev| {
                    ev.input_origin.as_ref().map(|orig| {
                        json!({
                            "raw_seq": ev.seq,
                            "sender_kind": orig.sender_kind,
                            "trust_level": orig.trust_level,
                            "basis": orig.basis,
                            "sender_session": orig.sender_session,
                            "assignment_id": orig.assignment_id,
                        })
                    })
                })
                .collect();
            report["observed_user_input_origins"] = json!(user_origins);
        }
    }

    if write_json_line(&report) {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

#[cfg(feature = "store")]
#[allow(clippy::disallowed_methods)]
fn run_root_store_mode(args: &Args, sessions_root: &std::path::Path) -> ExitCode {
    use session_vault::dsh_snapshot::{scan_dsh_sessions_root_to_store, DshSavedState};
    use session_vault::scan_plan::ScanReasons;
    use session_vault::{SourceLocation, StoreKey, TotalStore};
    use std::collections::BTreeMap;

    let (Some(ref node), Some(ref dsh_pkg)) = (&args.node, &args.dsh_package) else {
        eprintln!("--sessions-root with --store-db requires --node and --dsh-package");
        return ExitCode::FAILURE;
    };
    let runtime = match NativeRuntime::new(node.clone(), dsh_pkg.clone()) {
        Ok(rt) => rt,
        Err(reason) => {
            let _ = write_json_line(&serde_json::to_value(reason).unwrap());
            return ExitCode::from(2);
        }
    };

    let store_path = args.store_db.as_ref().expect("store_db required");
    let state_path = args.state_file.as_ref().expect("state_file required");
    let key = StoreKey::from_bytes([0x5a_u8; 32]);
    let store = match TotalStore::open_with_key(store_path, key) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("failed to open store: {e}");
            return ExitCode::FAILURE;
        }
    };

    if let Some(ref bpath) = args.bindings_file {
        let raw = match std::fs::read_to_string(bpath) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("failed to read bindings_file: {e}");
                return ExitCode::FAILURE;
            }
        };
        let reg = session_vault::SessionBindingRegistry::from_json_str(&raw);
        if let Err(e) = store.set_session_bindings(&reg) {
            eprintln!("failed to set session bindings: {e}");
            return ExitCode::FAILURE;
        }
    }

    let mut saved_states: BTreeMap<String, DshSavedState> = if state_path.exists() {
        match std::fs::read_to_string(state_path)
            .ok()
            .and_then(|raw| serde_json::from_str::<BTreeMap<String, DshSavedState>>(&raw).ok())
        {
            Some(map) => map,
            None => {
                eprintln!("failed to parse root state_file");
                return ExitCode::FAILURE;
            }
        }
    } else {
        BTreeMap::new()
    };

    let mut reasons = ScanReasons::NONE;
    if args.force {
        reasons |= ScanReasons::FORCE;
    }
    let inst_prefix = args
        .source_instance
        .as_deref()
        .unwrap_or("dsh-root-instance");
    let outcome = match scan_dsh_sessions_root_to_store(
        &store,
        sessions_root,
        inst_prefix,
        &runtime,
        &mut saved_states,
        reasons,
        SourceLocation::Local,
        0,
        make_deadline(args.native_timeout_ms),
    ) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("scan_dsh_sessions_root_to_store error: {e}");
            return ExitCode::FAILURE;
        }
    };

    if !args.simulate_crash_before_save_state {
        let serialized = serde_json::to_string_pretty(&saved_states).unwrap();
        if std::fs::write(state_path, serialized).is_err() {
            eprintln!("failed to write root state_file");
            return ExitCode::FAILURE;
        }
    }

    let current_page = store.read_current_since_page(0, 10_000).unwrap();
    let entries_json: Vec<Value> = outcome
        .entries
        .iter()
        .map(|e| {
            json!({
                "dir_name": e.dir_name,
                "discovery_status": e.discovery_status,
                "commit_gate": e.commit_outcome.planned.gate,
                "admission_status": e.commit_outcome.planned.admission_status,
                "events_excluded_temporary": e.commit_outcome.planned.events_excluded_temporary,
                "observed_events_in_batch": e.commit_outcome.planned.observation.events.len(),
                "advanced_saved_state": e.commit_outcome.advanced_saved_state,
            })
        })
        .collect();

    let report = json!({
        "kind": "dsh_sessions_root_scanned_and_committed",
        "sessions_root": outcome.sessions_root,
        "total_session_dirs": outcome.total_session_dirs,
        "ready_v4_count": outcome.ready_v4_count,
        "blocked_count": outcome.blocked_count,
        "projected_sessions": outcome.projected_sessions,
        "preserved_sessions": outcome.preserved_sessions,
        "temporary_excluded_sessions": outcome.temporary_excluded_sessions,
        "total_events_appended": outcome.total_events_appended,
        "store_current_events": current_page.events.len(),
        "entries": entries_json,
    });
    if write_json_line(&report) {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn main() -> ExitCode {
    let args = Args::parse();
    let encoding = match args.compression {
        Encoding::None => Compression::None,
        Encoding::Zstd => Compression::Zstd,
    };
    if let Some(ref sessions_root) = args.sessions_root {
        #[cfg(feature = "store")]
        if args.store_db.is_some() {
            return run_root_store_mode(&args, sessions_root);
        }
        let (report, code) = match discover_dsh_sessions_in_root(sessions_root, None) {
            Ok(disc) => (
                json!({
                    "kind": "dsh_sessions_root_discovered",
                    "sessions_root": disc.sessions_root,
                    "ready_v4_count": disc.ready_v4_count,
                    "blocked_count": disc.blocked_count,
                    "entries": disc.entries,
                }),
                ExitCode::SUCCESS,
            ),
            Err(reason) => (
                serde_json::to_value(reason).expect("serializable blocked result"),
                ExitCode::from(2),
            ),
        };
        return if write_json_line(&report) {
            code
        } else {
            ExitCode::FAILURE
        };
    }
    if let Some(ref path) = args.path {
        if !path.is_absolute() {
            eprintln!("path must be absolute");
            return ExitCode::FAILURE;
        }
        let (report, code) = match run_file_mode(args, encoding) {
            Ok(report) => (report, ExitCode::SUCCESS),
            Err(reason) => (
                serde_json::to_value(reason).expect("serializable blocked result"),
                ExitCode::from(2),
            ),
        };
        return if write_json_line(&report) {
            code
        } else {
            ExitCode::FAILURE
        };
    }

    let session_dir = args.session_dir.clone().expect("checked by clap group");
    let source_instance = args
        .source_instance
        .clone()
        .expect("required by session_dir");
    let node = args.node.clone().expect("required by session_dir");
    let package = args.dsh_package.clone().expect("required by session_dir");
    let config = match SourceConfig::new(
        &source_instance,
        &session_dir,
        args.session_id.clone(),
        encoding,
    )
    .and_then(|cfg| NativeRuntime::new(node, package).map(|rt| (cfg, rt)))
    {
        Ok(pair) => pair,
        Err(reason) => {
            let mut value = serde_json::to_value(&reason).expect("serializable blocked result");
            value["commit_gate"] =
                serde_json::to_value(evaluate_commit_gate(None, Err(&reason))).unwrap();
            return if write_json_line(&value) {
                ExitCode::from(2)
            } else {
                ExitCode::FAILURE
            };
        }
    };
    let (cfg, runtime) = config;

    #[cfg(feature = "store")]
    if args.store_db.is_some() {
        return run_store_mode(&args, &cfg, &runtime, &session_dir);
    }

    let first = capture_source(&cfg, &runtime, make_deadline(args.native_timeout_ms));
    let prior = match first {
        Ok(snapshot) => snapshot,
        Err(reason) => {
            let mut value = serde_json::to_value(&reason).expect("serializable blocked result");
            value["commit_gate"] =
                serde_json::to_value(evaluate_commit_gate(None, Err(&reason))).unwrap();
            return if write_json_line(&value) {
                ExitCode::from(2)
            } else {
                ExitCode::FAILURE
            };
        }
    };
    let mut prior_report = match format_source_report(&prior, args.positions, args.normalize) {
        Ok(r) => r,
        Err(reason) => {
            let value = serde_json::to_value(reason).expect("serializable blocked result");
            return if write_json_line(&value) {
                ExitCode::from(2)
            } else {
                ExitCode::FAILURE
            };
        }
    };
    prior_report["commit_gate"] =
        serde_json::to_value(evaluate_commit_gate(None, Ok(&prior))).unwrap();

    if let Some(other_dir) = args.compare_session_dir {
        let other_instance = args.compare_source_instance.unwrap_or(source_instance);
        let next = SourceConfig::new(other_instance, other_dir, args.session_id, encoding)
            .and_then(|other_cfg| {
                capture_source(&other_cfg, &runtime, make_deadline(args.native_timeout_ms))
            });
        let recheck = match prior.recheck() {
            Ok(()) => json!({"kind": "matches_observation"}),
            Err(ref b) => serde_json::to_value(b).unwrap(),
        };
        let out = step_report(
            &prior,
            &prior_report,
            recheck,
            next,
            args.positions,
            args.normalize,
        );
        return if write_json_line(&out) {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        };
    }

    if !write_json_line(&prior_report) {
        return ExitCode::FAILURE;
    }
    if !args.follow_stdin {
        return ExitCode::SUCCESS;
    }
    let mut line = String::new();
    if io::stdin().lock().read_line(&mut line).is_err() {
        return ExitCode::FAILURE;
    }
    let recheck = match prior.recheck() {
        Ok(()) => json!({"kind": "matches_observation"}),
        Err(ref b) => serde_json::to_value(b).unwrap(),
    };
    let next = capture_source(&cfg, &runtime, make_deadline(args.native_timeout_ms));
    let out = step_report(
        &prior,
        &prior_report,
        recheck,
        next,
        args.positions,
        args.normalize,
    );
    if write_json_line(&out) {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
