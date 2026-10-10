//! 集成测试造 fixture 要直接碰盘 —— 边界管的是生产行为。
#![allow(clippy::disallowed_methods)]
//! 多库合读（`stores` / `sessions-read --all-stores` / `sessions-recent --all-stores`）的真实进程验收。
//!
//! 两个库代表两台机器（标识不同，同一把钥匙 —— 多机共用一把主密钥）。判据来自 TumeChat
//! （2026-10-10）：同一会话的同一 seq，在哪台机器上读都取回同一条事件。
//!
//! ```text
//! cargo test --features "store acceptance-fixtures" --test cli_union_it
//! ```
#![cfg(all(feature = "store", feature = "acceptance-fixtures", debug_assertions))]

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;
use session_vault::attribution::RootRegistry;
use session_vault::discover::SourceRef;
use session_vault::{
    Profile, Projection, SourceLocation, SourceMode, SourceType, StoreKey, TotalStore,
};

/// 与 `cli_roots_it` 同一把测试密钥（32 个 0 字节）。
const TEST_KEY: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

fn claude_lines(session: &str, texts: &[&str]) -> String {
    texts
        .iter()
        .map(|t| {
            serde_json::json!({
                "type": "user",
                "sessionId": session,
                "message": {"role": "user", "content": t}
            })
            .to_string()
                + "\n"
        })
        .collect()
}

/// 把一个会话文件整份扫进库里，返回 `sessions-read --session` 用的标识。
fn ingest(store: &TotalStore, file: &Path) -> String {
    let source = SourceRef {
        source_type: SourceType::ClaudeCode,
        source_location: SourceLocation::Local,
        source_mode: SourceMode::AppendLog,
        path: file.to_path_buf(),
        project_root: None,
        artifact_kind: None,
    };
    let res = session_vault::scan(
        &source,
        None,
        Profile::Full,
        std::sync::Arc::new(RootRegistry::new()),
    );
    store
        .append_events(&res.events, Projection::Append)
        .unwrap();
    let ev = &res.events[0];
    format!(
        "claude_code/local/{}/{}",
        ev.source_path, ev.source_session_id
    )
}

fn svault(args: &[&str]) -> Vec<Value> {
    let out = Command::new(env!("CARGO_BIN_EXE_svault"))
        .env("SVAULT_ACCEPTANCE_KEY", TEST_KEY)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn of_kind<'a>(lines: &'a [Value], kind: &str) -> Vec<&'a Value> {
    lines.iter().filter(|l| l["kind"] == kind).collect()
}

/// `sessions-read` 取回的事件，只留比得了的部分（seq 与整条事件）。
fn events(lines: &[Value]) -> Vec<(Value, Value)> {
    of_kind(lines, "pulled")
        .into_iter()
        .map(|l| (l["event"]["seq"].clone(), l["event"].clone()))
        .collect()
}

/// 用 `VACUUM INTO` 把库拷成一份副本（与镜像同步得到的一样：整库、带着源库的标识）。
fn copy_store(from: &Path, to: &Path) {
    rusqlite::Connection::open(from)
        .unwrap()
        .execute("VACUUM INTO ?1", [to.to_str().unwrap()])
        .unwrap();
}

#[test]
fn union_reads_pick_the_same_copy_on_every_machine() {
    let dir = std::env::temp_dir().join(format!("svault-it-union-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let key = || StoreKey::from_encoded(TEST_KEY).unwrap();
    let (db_a, db_b) = (dir.join("a.db"), dir.join("b.db"));
    let (only_a, only_b, both) = (
        dir.join("s1.jsonl"),
        dir.join("s2.jsonl"),
        dir.join("s3.jsonl"),
    );

    // 机器 A：s1、s3（2 条）。机器 B：s2、s3（同一个文件续写到 4 条 —— 拷过去之后接着用）。
    std::fs::write(&only_a, claude_lines("s1", &["a1", "a2", "a3"])).unwrap();
    std::fs::write(&only_b, claude_lines("s2", &["b1", "b2"])).unwrap();
    std::fs::write(&both, claude_lines("s3", &["c1", "c2"])).unwrap();
    let store_a = TotalStore::open_with_key(&db_a, key()).unwrap();
    ingest(&store_a, &only_a);
    let s3 = ingest(&store_a, &both);
    let id_a = store_a.store_id().unwrap().unwrap();
    drop(store_a);
    std::fs::write(&both, claude_lines("s3", &["c1", "c2", "c3", "c4"])).unwrap();
    let store_b = TotalStore::open_with_key(&db_b, key()).unwrap();
    let s2 = ingest(&store_b, &only_b);
    assert_eq!(ingest(&store_b, &both), s3);
    let id_b = store_b.store_id().unwrap().unwrap();
    drop(store_b);
    assert_ne!(id_a, id_b);

    // 机器 A 的视角：本机 a.db，副本目录里是 B 的镜像，外加一个坏掉的副本。
    let replicas_on_a = dir.join("replicas-on-a");
    std::fs::create_dir_all(&replicas_on_a).unwrap();
    copy_store(&db_b, &replicas_on_a.join(format!("{id_b}.db")));
    std::fs::write(replicas_on_a.join("junk.db"), b"not a database").unwrap();
    // 机器 B 的视角：本机 b.db，副本目录里是 A 的镜像。
    let replicas_on_b = dir.join("replicas-on-b");
    std::fs::create_dir_all(&replicas_on_b).unwrap();
    copy_store(&db_a, &replicas_on_b.join(format!("{id_a}.db")));
    let view = |db: &PathBuf, replicas: &PathBuf, extra: &[&str]| {
        let (db, replicas) = (db.to_str().unwrap(), replicas.to_str().unwrap());
        let mut args = extra.to_vec();
        args.extend(["--store", db, "--replicas", replicas]);
        svault(&args)
    };

    // stores：本机库、B 的镜像、打不开的那一个都在，打不开的不当作没有。
    let listed = view(&db_a, &replicas_on_a, &["stores"]);
    let ids: Vec<_> = of_kind(&listed, "store")
        .iter()
        .map(|l| l["store_id"].clone())
        .collect();
    assert_eq!(
        ids,
        vec![Value::from(id_a.as_str()), Value::from(id_b.as_str())]
    );
    assert_eq!(of_kind(&listed, "store_unavailable").len(), 1);
    assert_eq!(of_kind(&listed, "stores_summary")[0]["unavailable"], 1);
    // 副本目录列不出来（这里给的是个文件）≠ 没有副本：报错退出。
    let not_a_dir = Command::new(env!("CARGO_BIN_EXE_svault"))
        .env("SVAULT_ACCEPTANCE_KEY", TEST_KEY)
        .args(["stores", "--store", db_a.to_str().unwrap(), "--replicas"])
        .arg(&only_a)
        .output()
        .unwrap();
    assert!(!not_a_dir.status.success(), "列不出副本却当作没有副本");

    let read = |db: &PathBuf, replicas: &PathBuf, spec: &str| {
        view(
            db,
            replicas,
            &["sessions-read", "--all-stores", "--session", spec],
        )
    };
    let cursor = |lines: &[Value]| of_kind(lines, "session_cursor")[0].clone();

    // 判据 1：只在副本里的会话，在 A 上读到的就是在 B 本机读到的。
    let on_a = read(&db_a, &replicas_on_a, &s2);
    let on_b_alone = svault(&[
        "sessions-read",
        "--session",
        &s2,
        "--store",
        db_b.to_str().unwrap(),
    ]);
    assert_eq!(events(&on_a), events(&on_b_alone));
    assert_eq!(events(&on_a).len(), 2);
    assert_eq!(cursor(&on_a)["store_id"], id_b.as_str());
    assert_eq!(cursor(&on_a)["found_in"], 1);
    assert_eq!(
        of_kind(&on_a, "sessions_read_summary")[0]["stores_unavailable"],
        1
    );

    // 判据 2：两个库都有的会话，两台机器挑出同一份（更全的 B 那份）、同样的事件。
    let s3_on_a = read(&db_a, &replicas_on_a, &s3);
    let s3_on_b = read(&db_b, &replicas_on_b, &s3);
    assert_eq!(events(&s3_on_a).len(), 4);
    assert_eq!(events(&s3_on_a), events(&s3_on_b));
    for c in [cursor(&s3_on_a), cursor(&s3_on_b)] {
        assert_eq!(c["store_id"], id_b.as_str());
        assert_eq!(c["found_in"], 2);
    }

    // 哪个库里都没有：照样一行游标，说清「找过、没有」。
    let missing = read(
        &db_a,
        &replicas_on_a,
        "claude_code/local/nowhere.jsonl/none",
    );
    assert_eq!(cursor(&missing)["found_in"], 0);
    assert!(cursor(&missing)["store_id"].is_null());
    assert!(events(&missing).is_empty());

    // 不加 --all-stores：只读本机库，输出不带新键。
    let alone = svault(&[
        "sessions-read",
        "--session",
        &s2,
        "--store",
        db_a.to_str().unwrap(),
    ]);
    assert!(events(&alone).is_empty());
    assert!(cursor(&alone).get("found_in").is_none());

    // sessions-recent：三个会话各一行，两库都有的那个取 B 那份。
    let recent = view(&db_a, &replicas_on_a, &["sessions-recent", "--all-stores"]);
    let rows = of_kind(&recent, "recent_session");
    assert_eq!(rows.len(), 3);
    let s3_row = rows.iter().find(|r| r["session_id"] == "s3").unwrap();
    assert_eq!(s3_row["store_id"], id_b.as_str());
    assert_eq!(s3_row["events"], 4);
    assert!(rows
        .iter()
        .any(|r| r["session_id"] == "s1" && r["store_id"] == id_a.as_str()));

    // pull 的摘要带着读的是哪个库。
    let pulled = svault(&["pull", "--store", db_a.to_str().unwrap()]);
    assert_eq!(
        of_kind(&pulled, "pull_summary")[0]["store_id"],
        id_a.as_str()
    );

    std::fs::remove_dir_all(&dir).unwrap();
}
