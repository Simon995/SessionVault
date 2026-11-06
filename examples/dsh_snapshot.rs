//! 只读核验显式给出的 DSH v4 物理快照，不发现来源、不写总库。
//! cargo run --example dsh_snapshot --features dsh-experimental -- --path <absolute-file> --compression zstd --positions
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, ValueEnum};
use session_vault::dsh_snapshot::{capture, Compression};

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
    let (report, code) = match capture(&args.path, encoding) {
        Ok(snapshot) => (snapshot.report(args.positions), ExitCode::SUCCESS),
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
