//! 集成测试造 fixture 要直接碰盘 —— 边界管的是生产行为。
#![allow(clippy::disallowed_methods)]
//! `svault key-export` / `key-import` 的真实进程验收。
//!
//! 子进程一律显式设 `SVAULT_KEY_FILE`：不设就会去碰跑测试那台机器的 OS 密钥链。
//!
//! ```text
//! cargo test --features store --test cli_key_it
//! ```
#![cfg(feature = "store")]

use std::path::Path;
use std::process::{Command, Output};

use session_vault::discover::SourceRef;
use session_vault::{SourceLocation, SourceMode, SourceType, StoreKey, TotalStore};

/// 32 个 0 字节 / 32 个 1 字节。
const KEY_A: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
const KEY_B: &str = "AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE";

fn svault(key_file: &Path, args: &[&str]) -> Output {
    let out = Command::new(env!("CARGO_BIN_EXE_svault"))
        .env("SVAULT_KEY_FILE", key_file)
        .env_remove("SVAULT_ACCEPTANCE_KEY")
        .args(args)
        .output()
        .unwrap();
    for stream in [&out.stdout, &out.stderr] {
        let text = String::from_utf8_lossy(stream);
        assert!(
            !text.contains(KEY_A) && !text.contains(KEY_B),
            "钥匙出现在了输出里"
        );
    }
    out
}

fn line(out: &Output) -> serde_json::Value {
    serde_json::from_slice(&out.stdout).unwrap()
}

/// 钥匙文件夹具：Unix 上 svault 拒绝对组或其他用户可读的钥匙文件，夹具得和真的一样是 0600。
fn write_key(path: &Path, key: &str) {
    std::fs::write(path, key).unwrap();
    #[cfg(unix)]
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o600)).unwrap();
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap().trim().to_string()
}

#[test]
fn a_key_travels_by_file_and_only_where_it_belongs() {
    let dir = std::env::temp_dir().join(format!("svault-it-key-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // 一个用 KEY_A 加密、里面有数据的库 —— 空库谁的钥匙都「打得开」，验不出东西。
    let (db, memory) = (dir.join("total_store.db"), dir.join("memory.md"));
    std::fs::write(&memory, "# m\n").unwrap();
    TotalStore::open_with_key(&db, StoreKey::from_encoded(KEY_A).unwrap())
        .unwrap()
        .sync_snapshots(&[SourceRef {
            source_type: SourceType::ClaudeCode,
            source_location: SourceLocation::Local,
            source_mode: SourceMode::SnapshotFile,
            path: memory,
            project_root: None,
            artifact_kind: Some("memory".into()),
        }])
        .unwrap();
    let (key_a, key_b) = (dir.join("a.key"), dir.join("b.key"));
    write_key(&key_a, KEY_A);
    write_key(&key_b, KEY_B);
    let db_arg = db.to_str().unwrap();

    // 导出：写进新文件、内容就是本机那把；同一路径再导出被拒、不覆盖。
    let exported = dir.join("exported.key");
    let to = exported.to_str().unwrap();
    let out = svault(&key_a, &["key-export", "--to", to]);
    assert!(out.status.success());
    assert_eq!(line(&out)["kind"], "key_exported");
    assert_eq!(read(&exported), KEY_A);
    assert!(!svault(&key_b, &["key-export", "--to", to]).status.success());
    assert_eq!(read(&exported), KEY_A, "第二次导出覆盖了文件");

    // 新机器：装上、并用库验过；再导一次什么都不做。
    let fresh = dir.join("fresh.key");
    let out = svault(&fresh, &["key-import", "--from", to, "--store", db_arg]);
    assert!(out.status.success());
    assert_eq!(line(&out)["outcome"], "installed");
    assert_eq!(line(&out)["verified_against_store"], true);
    assert_eq!(read(&fresh), KEY_A);
    let out = svault(&fresh, &["key-import", "--from", to, "--store", db_arg]);
    assert_eq!(line(&out)["outcome"], "already_present");
    assert_eq!(line(&out)["verified_against_store"], false, "没验却报验过");

    // 本机已有另一把：拒绝，原来那把不动。
    let b = key_b.to_str().unwrap();
    assert!(
        !svault(&fresh, &["key-import", "--from", b, "--store", db_arg])
            .status
            .success()
    );
    assert_eq!(read(&fresh), KEY_A);

    // 打不开库的钥匙：不装。显式给的库不在：拒绝。
    let empty = dir.join("empty.key");
    assert!(
        !svault(&empty, &["key-import", "--from", b, "--store", db_arg])
            .status
            .success()
    );
    assert!(!empty.exists(), "打不开库的钥匙被装上了");
    let missing = dir.join("missing.db");
    let args = [
        "key-import",
        "--from",
        to,
        "--store",
        missing.to_str().unwrap(),
    ];
    assert!(!svault(&empty, &args).status.success());
    assert!(!empty.exists());

    // 换钥：本机（fresh，钥匙 A）把库换到 B 下；旧钥匙备份在旁边；再跑一次什么都不改。
    let out = svault(
        &fresh,
        &["key-import", "--from", b, "--store", db_arg, "--rekey"],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let rekeyed = line(&out);
    assert_eq!(rekeyed["kind"], "key_rekeyed");
    assert_eq!(rekeyed["outcome"], "rekeyed");
    assert!(rekeyed["rewrapped"].as_u64().unwrap() >= 1);
    let backup = rekeyed["previous_key_backup"].as_str().unwrap();
    let backup = Path::new(backup.strip_prefix("file:").unwrap());
    assert_eq!(read(backup), KEY_A, "旧钥匙没备份对");
    assert_eq!(read(&fresh), KEY_B, "本机钥匙没换");
    let out = svault(
        &fresh,
        &["key-import", "--from", b, "--store", db_arg, "--rekey"],
    );
    assert_eq!(line(&out)["outcome"], "already_present");
    assert_eq!(line(&out)["rewrapped"], 0);

    std::fs::remove_dir_all(&dir).unwrap();
}
