//! 集成测试造 fixture 要直接碰盘 —— 边界管的是生产行为。
#![allow(clippy::disallowed_methods)]
//! 只读子命令不迁移共享库：真实进程验收。
//!
//! 单元测试钉的是 `TotalStore::open_read_only`；这里钉的是 CLI 那一层 —— 每个只读子命令
//! 真的走它。改回读写打开的那个子命令会在打开时跑 `migrate()`，把库缺的表建回来，
//! 也就是替写入方迁移了别人正在写的库；这种回退没有任何别的东西会报错。
//!
//! ```text
//! cargo test --features "store acceptance-fixtures" --test cli_read_only_it
//! ```
#![cfg(all(feature = "store", feature = "acceptance-fixtures", debug_assertions))]

use std::process::Command;

use rusqlite::Connection;
use session_vault::{StoreKey, TotalStore};

/// 与 `cli_roots_it` 同一把测试密钥（32 个 0 字节）。
const TEST_KEY: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

fn has_table(db: &std::path::Path, name: &str) -> bool {
    Connection::open(db)
        .unwrap()
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE name = ?1",
            [name],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
        > 0
}

#[test]
fn every_read_subcommand_leaves_an_older_store_unmigrated() {
    let dir = std::env::temp_dir().join(format!("svault-it-ro-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("total_store.db");
    drop(TotalStore::open_with_key(&db, StoreKey::from_encoded(TEST_KEY).unwrap()).unwrap());
    // 让库「比本程序旧」：少一张 migrate() 会建的表。
    Connection::open(&db)
        .unwrap()
        .execute_batch("DROP TABLE projection_log;")
        .unwrap();

    let store = db.to_str().unwrap();
    let reads: [&[&str]; 8] = [
        &["store-info"],
        &["pull", "--since", "0"],
        &["changes", "--since-seq", "0"],
        &["sessions-recent"],
        &["sessions-read", "--session", "codex/local/p/s"],
        &["snapshots"],
        &["roots"],
        &["attribute", "--path", "/w/p"],
    ];
    for args in reads {
        let out = Command::new(env!("CARGO_BIN_EXE_svault"))
            .env("SVAULT_ACCEPTANCE_KEY", TEST_KEY)
            .args(args)
            .args(["--store", store])
            .output()
            .unwrap();
        assert!(!out.status.success(), "{}: 库缺表却照常成功", args[0]);
        assert!(
            !has_table(&db, "projection_log"),
            "{}: 打开时迁移了库（把缺的表建回来了）",
            args[0]
        );
    }
    // 多库的读命令把打不开的库报成 unavailable、照常退出 0 —— 但同样不许迁移它。
    let multi: [&[&str]; 3] = [
        &["stores"],
        &[
            "sessions-read",
            "--all-stores",
            "--session",
            "codex/local/p/s",
        ],
        &["sessions-recent", "--all-stores"],
    ];
    let no_replicas = dir.join("no-replicas");
    for args in multi {
        let out = Command::new(env!("CARGO_BIN_EXE_svault"))
            .env("SVAULT_ACCEPTANCE_KEY", TEST_KEY)
            .args(args)
            .args([
                "--store",
                store,
                "--replicas",
                no_replicas.to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("\"store_unavailable\"") || stdout.contains("\"stores_unavailable\":1"),
            "{}: 打不开的库没有报出来：{stdout}",
            args[0]
        );
        assert!(
            !has_table(&db, "projection_log"),
            "{}: 打开时迁移了库",
            args[0]
        );
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

/// `store-info` 报出的就是库里记的那个标识（不是现场生成的一个）。
#[test]
fn store_info_reports_the_id_the_store_minted() {
    let dir = std::env::temp_dir().join(format!("svault-it-id-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("total_store.db");
    let minted = TotalStore::open_with_key(&db, StoreKey::from_encoded(TEST_KEY).unwrap())
        .unwrap()
        .store_id()
        .unwrap()
        .unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_svault"))
        .env("SVAULT_ACCEPTANCE_KEY", TEST_KEY)
        .args(["store-info", "--store", db.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(out.status.success());
    let line: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(line["kind"], "store_info");
    assert_eq!(line["store_id"], minted.as_str());
    std::fs::remove_dir_all(&dir).unwrap();
}
