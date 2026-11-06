//! 只读核验显式给出的 DSH v4 物理快照或会话目录，不发现来源、不写总库。
//! cargo run --example dsh_snapshot --features dsh-experimental -- --path <absolute-file> --compression zstd --positions
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{ArgGroup, Parser, ValueEnum};
use serde_json::{json, Value};
use session_vault::deadline::Deadline;
use session_vault::dsh_snapshot::{
    capture, capture_source, evaluate_commit_gate, Blocked, Compression, NativeRuntime,
    SourceConfig,
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
        .args(["path", "session_dir"]),
))]
struct Args {
    #[arg(long)]
    path: Option<PathBuf>,
    #[arg(
        long,
        requires_all = ["source_instance", "node", "dsh_package"],
        conflicts_with = "path"
    )]
    session_dir: Option<PathBuf>,
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
    /// 输出字节范围和原生 seq，不输出正文。
    #[arg(long)]
    positions: bool,
    /// 显式启用本机原生 admission；Node 与安装包路径必须成对给出。
    #[arg(long, requires = "dsh_package")]
    node: Option<PathBuf>,
    #[arg(long, requires = "node")]
    dsh_package: Option<PathBuf>,
    #[arg(long, requires = "node")]
    native_timeout_ms: Option<u64>,
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
        NativeRuntime::new(node, package)?
            .admit(snapshot, deadline)
            .map(|admitted| admitted.report(args.positions))
    } else {
        Ok(snapshot.report(args.positions))
    }
}

fn step_report(
    prior: &session_vault::dsh_snapshot::SourceSnapshot,
    prior_report: &Value,
    recheck: Value,
    next: Result<session_vault::dsh_snapshot::SourceSnapshot, Blocked>,
    include_positions: bool,
) -> Value {
    let gate = evaluate_commit_gate(Some(prior), next.as_ref());
    match next {
        Ok(current) => {
            let relation = prior.compare(&current);
            json!({
                "kind": "source_step_evaluated",
                "prior_recheck": recheck,
                "relation": relation.kind_str(),
                "relation_detail": relation,
                "commit_gate": gate,
                "current": current.report(include_positions),
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

fn main() -> ExitCode {
    let args = Args::parse();
    let encoding = match args.compression {
        Encoding::None => Compression::None,
        Encoding::Zstd => Compression::Zstd,
    };
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

    let session_dir = args.session_dir.expect("checked by clap group");
    let source_instance = args.source_instance.expect("required by session_dir");
    let node = args.node.expect("required by session_dir");
    let package = args.dsh_package.expect("required by session_dir");
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
    let mut prior_report = prior.report(args.positions);
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
        let out = step_report(&prior, &prior_report, recheck, next, args.positions);
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
    let out = step_report(&prior, &prior_report, recheck, next, args.positions);
    if write_json_line(&out) {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
