//! 只读核验显式给出的 DSH v4 物理快照，不发现来源、不写总库。
//! cargo run --example dsh_snapshot --features dsh-experimental -- --path <absolute-file> --compression zstd --positions
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, ValueEnum};
use session_vault::deadline::Deadline;
use session_vault::dsh_snapshot::{capture, Compression, NativeRuntime};

#[derive(Clone, Copy, ValueEnum)]
enum Encoding {
    None,
    Zstd,
}

#[derive(Parser)]
struct Args {
    #[arg(long)]
    path: PathBuf,
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

fn main() -> ExitCode {
    let args = Args::parse();
    if !args.path.is_absolute() {
        eprintln!("path must be absolute");
        return ExitCode::FAILURE;
    }
    let encoding = match args.compression {
        Encoding::None => Compression::None,
        Encoding::Zstd => Compression::Zstd,
    };
    let deadline = args
        .native_timeout_ms
        .map(|ms| Deadline::after(Duration::from_millis(ms)))
        .unwrap_or_else(Deadline::unbounded);
    let result = capture(&args.path, encoding).and_then(|snapshot| {
        if let (Some(node), Some(package)) = (args.node, args.dsh_package) {
            NativeRuntime::new(node, package)?
                .admit(snapshot, deadline)
                .map(|admitted| admitted.report(args.positions))
        } else {
            Ok(snapshot.report(args.positions))
        }
    });
    let (report, code) = match result {
        Ok(report) => (report, ExitCode::SUCCESS),
        Err(reason) => (
            serde_json::to_value(reason).expect("serializable blocked result"),
            ExitCode::from(2),
        ),
    };
    let mut stdout = io::stdout().lock();
    if serde_json::to_writer(&mut stdout, &report).is_err() || writeln!(stdout).is_err() {
        return ExitCode::FAILURE;
    }
    code
}
