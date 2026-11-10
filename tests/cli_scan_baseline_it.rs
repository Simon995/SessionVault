//! 全读的读取起点与旧来源基线不能混用；用真实 CLI 写库及状态文件核验。
#![cfg(feature = "store")]
#![allow(clippy::disallowed_methods)]

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use serde_json::{json, Value};

static NEXT_RUN: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    root: PathBuf,
    source: PathBuf,
    state: PathBuf,
    store: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        // 跟随构建目录；WSL 在 Linux 文件系统上生成钥匙，才能验证真实的 0600 权限。
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let target = std::env::var_os("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| manifest.join("target"));
        let target = if target.is_absolute() {
            target
        } else {
            manifest.join(target)
        };
        let info = std::fs::symlink_metadata(&target).unwrap();
        assert!(info.is_dir() && !info.file_type().is_symlink());
        let root = loop {
            let candidate = target.join(format!(
                "cli-scan-baseline-{}-{}",
                std::process::id(),
                NEXT_RUN.fetch_add(1, Ordering::Relaxed)
            ));
            match std::fs::create_dir(&candidate) {
                Ok(()) => break candidate,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create fixture directory: {error}"),
            }
        };
        std::fs::create_dir(root.join("source")).unwrap();
        std::fs::create_dir(root.join("source/sessions")).unwrap();
        Self {
            source: root.join("source/sessions/fixture.jsonl"),
            state: root.join("state.json"),
            store: root.join("fixture.db"),
            root,
        }
    }

    fn run(&self, args: &[&str]) -> Vec<Value> {
        let output = Command::new(env!("CARGO_BIN_EXE_svault"))
            .env("SVAULT_JSONL_DIR", self.root.join("source"))
            .env_remove("SVAULT_DSH_SESSIONS_DIR")
            .env_remove("SVAULT_DSH_HOME")
            .env("SVAULT_KEY_FILE", self.root.join("fixture.key"))
            .env("CLAUDE_CONFIG_DIR", self.root.join("empty-claude"))
            .env("CODEX_HOME", self.root.join("empty-codex"))
            .env_remove("SVAULT_ACCEPTANCE_KEY")
            .args(args)
            .arg("--store")
            .arg(&self.store)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn scan(&self) -> Vec<Value> {
        self.run(&[
            "scan-all",
            "--only",
            "jsonl",
            "--write-store",
            "--profile",
            "full",
            "--state",
            self.state.to_str().unwrap(),
        ])
    }

    fn state(&self) -> Value {
        serde_json::from_slice(&std::fs::read(&self.state).unwrap()).unwrap()
    }

    fn modified_at(&self, time: SystemTime) {
        std::fs::OpenOptions::new()
            .write(true)
            .open(&self.source)
            .unwrap()
            .set_modified(time)
            .unwrap();
    }

    fn stale_state(&self) -> Vec<u8> {
        let mut state = self.state();
        let entries = state.as_object_mut().unwrap();
        assert_eq!(entries.len(), 1);
        for previous in entries.values_mut() {
            previous["parser_revision"] = json!(session_vault::PARSER_REVISION.wrapping_add(1));
        }
        let bytes = serde_json::to_vec(&state).unwrap();
        std::fs::write(&self.state, &bytes).unwrap();
        bytes
    }

    fn contents(&self, projection: &str) -> Vec<String> {
        self.run(&["pull", "--projection", projection])
            .into_iter()
            .filter(|row| row["kind"] == "pulled")
            .map(|row| row["event"]["content"].as_str().unwrap().to_owned())
            .collect()
    }
}

fn messages(texts: &[&str]) -> String {
    texts
        .iter()
        .map(|text| {
            json!({"role":"user", "session_id":"fixture", "content":text}).to_string() + "\n"
        })
        .collect()
}

fn mode(rows: &[Value]) -> &str {
    let writes: Vec<_> = rows
        .iter()
        .filter(|row| row["kind"] == "store_write")
        .collect();
    assert_eq!(writes.len(), 1);
    writes[0]["mode"].as_str().unwrap()
}

#[test]
fn a_streaming_dsh_failure_keeps_the_entire_previous_source_state() {
    let fixture = Fixture::new();
    let sessions = fixture.root.join("dsh");
    let session = sessions.join("s0");
    std::fs::create_dir(&sessions).unwrap();
    std::fs::create_dir(&session).unwrap();
    let mut cursor = session_vault::Cursor::new_byte_offset();
    cursor.next_seq = 17;
    cursor.safe_offset = 7;
    cursor.size = 11;
    let key = format!("jsonl|local|{}", session.display());
    let mut state = json!({ (key): cursor });
    state.as_object_mut().unwrap().values_mut().next().unwrap()["parser_revision"] = json!(1);
    std::fs::write(&fixture.state, serde_json::to_vec(&state).unwrap()).unwrap();
    let bindings = fixture.root.join("empty-bindings.json");
    std::fs::write(&bindings, r#"{"schemaVersion":1,"bindings":[]}"#).unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_svault"))
        .env("SVAULT_JSONL_DIR", "")
        .env("SVAULT_DSH_SESSIONS_DIR", &sessions)
        .env_remove("SVAULT_DSH_HOME")
        .env_remove("SVAULT_DSH_NODE")
        .env_remove("SVAULT_DSH_PACKAGE")
        .env("SVAULT_SESSION_BINDINGS", &bindings)
        .env("CLAUDE_CONFIG_DIR", fixture.root.join("empty-claude"))
        .env("CODEX_HOME", fixture.root.join("empty-codex"))
        .args(["scan-all", "--only", "jsonl", "--state"])
        .arg(&fixture.state)
        .arg("--store")
        .arg(&fixture.store)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(
        String::from_utf8_lossy(&result.stdout).contains("native_runtime_not_configured"),
        "{}",
        String::from_utf8_lossy(&result.stdout)
    );
    assert_eq!(fixture.state(), state);
}

#[test]
fn a_shorter_source_during_parser_refresh_retains_its_old_source_version() {
    let fixture = Fixture::new();
    let old_text = "old ".repeat(100);
    let old = messages(&[&old_text]);
    std::fs::write(&fixture.source, &old).unwrap();
    assert_eq!(mode(&fixture.scan()), "append");
    let stale = fixture.stale_state();

    let replacement = messages(&["new-a", "new-b"]);
    assert!(replacement.len() < old.len());
    std::fs::write(&fixture.source, &replacement).unwrap();
    assert_eq!(mode(&fixture.scan()), "rollback");
    assert_eq!(fixture.contents("current"), ["new-a", "new-b"]);
    assert_eq!(
        fixture.contents("all"),
        [old_text.clone(), "new-a".into(), "new-b".into()]
    );

    let saved = fixture.state();
    let cursor = saved.as_object().unwrap().values().next().unwrap();
    assert_eq!(cursor["safe_offset"], replacement.len() as u64);
    assert_eq!(cursor["parser_revision"], session_vault::PARSER_REVISION);

    // 模拟总库已提交、状态尚未保存；重放不能再开一代或重复事件。
    std::fs::write(&fixture.state, stale).unwrap();
    fixture.scan();
    assert_eq!(fixture.contents("current"), ["new-a", "new-b"]);
    assert_eq!(
        fixture.contents("all"),
        [old_text, "new-a".into(), "new-b".into()]
    );
}

#[test]
fn an_append_during_parser_refresh_remains_a_reparse() {
    let fixture = Fixture::new();
    std::fs::write(&fixture.source, messages(&["first"])).unwrap();
    fixture.scan();
    fixture.stale_state();
    std::fs::write(&fixture.source, messages(&["first", "second"])).unwrap();
    assert_eq!(mode(&fixture.scan()), "reparse");
    assert_eq!(fixture.contents("current"), ["first", "second"]);
    assert_eq!(fixture.contents("all"), ["first", "second"]);
}

#[test]
fn a_rewound_mtime_during_parser_refresh_is_a_rollback_even_if_the_prefix_matches() {
    let fixture = Fixture::new();
    std::fs::write(&fixture.source, messages(&["first"])).unwrap();
    fixture.modified_at(SystemTime::UNIX_EPOCH + Duration::from_secs(2));
    fixture.scan();
    fixture.stale_state();
    std::fs::write(&fixture.source, messages(&["first", "second"])).unwrap();
    fixture.modified_at(SystemTime::UNIX_EPOCH);
    assert_eq!(mode(&fixture.scan()), "rollback");
    assert_eq!(fixture.contents("current"), ["first", "second"]);
    assert_eq!(fixture.contents("all"), ["first", "first", "second"]);
}

#[test]
fn removing_only_an_unconsumed_tail_during_refresh_is_not_a_rollback() {
    let fixture = Fixture::new();
    let complete = messages(&["first"]);
    std::fs::write(&fixture.source, format!("{complete}{{\"role\":")).unwrap();
    fixture.scan();
    let saved = fixture.state();
    let cursor = saved.as_object().unwrap().values().next().unwrap();
    assert_eq!(cursor["safe_offset"], complete.len() as u64);
    assert!(cursor["size"].as_u64().unwrap() > complete.len() as u64);
    fixture.stale_state();
    std::fs::write(&fixture.source, complete).unwrap();
    assert_eq!(mode(&fixture.scan()), "reparse");
    assert_eq!(fixture.contents("all"), ["first"]);
}
