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
        std::fs::write(
            root.join("empty-bindings.json"),
            r#"{"schemaVersion":1,"bindings":[]}"#,
        )
        .unwrap();
        Self::at(root)
    }

    fn at(root: PathBuf) -> Self {
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
            .env(
                "SVAULT_SESSION_BINDINGS",
                self.root.join("empty-bindings.json"),
            )
            .env("CLAUDE_CONFIG_DIR", self.root.join(".claude"))
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
        self.scan_for("jsonl", true)
    }

    fn scan_for(&self, source_type: &str, write_store: bool) -> Vec<Value> {
        let mut args = vec![
            "scan-all",
            "--only",
            source_type,
            "--profile",
            "full",
            "--state",
            self.state.to_str().unwrap(),
        ];
        if write_store {
            args.push("--write-store");
        }
        self.run(&args)
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

// Windows CLI 会枚举真实 WSL 转录；Linux CLI 只需要这组隔离的本机配置根。
#[cfg(not(windows))]
#[test]
fn unchanged_ssh_transcript_reparses_from_the_old_projection() {
    use session_vault::attribution::RootSource;
    use session_vault::{
        FileProjectionBatch, Projection, RawEvent, SourceKey, StoreKey, TotalStore, PARSER_REVISION,
    };

    // 种旧投影的库调用也会读绑定环境，放在子进程里以免继承真实绑定或修改全局环境。
    const FIXTURE_ENV: &str = "SVAULT_TEST_SSH_REPARSE_FIXTURE";
    let Some(root) = std::env::var_os(FIXTURE_ENV) else {
        let fixture = Fixture::new();
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "unchanged_ssh_transcript_reparses_from_the_old_projection",
                "--nocapture",
            ])
            .env(FIXTURE_ENV, &fixture.root)
            .env(
                "SVAULT_SESSION_BINDINGS",
                fixture.root.join("empty-bindings.json"),
            )
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    };
    let fixture = Fixture::at(PathBuf::from(root));
    let scan = |write_store| fixture.scan_for("claude-code", write_store);
    // 先让真实 CLI 创建独立钥匙和空库。两种转录稍后用同一份 cwd。
    scan(true);
    let key = std::fs::read_to_string(fixture.root.join("fixture.key")).unwrap();
    let store =
        TotalStore::open_with_key(&fixture.store, StoreKey::from_encoded(key.trim()).unwrap())
            .unwrap();
    let project = fixture.root.join("project");
    std::fs::create_dir(&project).unwrap();
    let cwd = project.to_str().unwrap();
    store.register_project_root(cwd, RootSource::Git);

    const ENDPOINT: &str = "11111111-2222-3333-4444-555555555555";
    const OLD_REVISION: u32 = 5;
    let namespace = format!("ssh:{ENDPOINT}");
    let remote_root = format!("{namespace}:{cwd}");
    let remote = fixture
        .root
        .join(format!(".claude/projects/ssh-{ENDPOINT}/remote.jsonl"));
    let local = fixture
        .root
        .join(".claude/projects/local-project/local.jsonl");
    let line = |id: &str, content: &str| {
        json!({"type":"user", "sessionId":id, "cwd":cwd, "message":{"role":"user", "content":content}}).to_string() + "\n"
    };
    let remote_bytes = line("remote", "remote first") + &line("remote", "remote second");
    let local_bytes = line("local", "local control");
    for (path, bytes) in [(&remote, &remote_bytes), (&local, &local_bytes)] {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }
    let remote_path = remote.to_str().unwrap();
    let local_path = local.to_str().unwrap();
    let mtime = std::fs::metadata(&remote).unwrap().modified().unwrap();
    let streamed = scan(false);
    let mut state = fixture.state();
    assert_eq!(state.as_object().unwrap().len(), 2);
    let head = |path| store.current_head("claude_code", "local", path).unwrap();
    for (path, bytes, revision, event_count) in [
        (remote_path, &remote_bytes, OLD_REVISION, 2),
        (local_path, &local_bytes, PARSER_REVISION, 1),
    ] {
        let cursor = &mut state[format!("claude_code|local|{path}")];
        assert_eq!(cursor["safe_offset"], bytes.len());
        assert_eq!(cursor["fingerprint"]["covered_len"], bytes.len());
        cursor["parser_revision"] = json!(revision);
        let events: Vec<RawEvent> = streamed
            .iter()
            .filter(|row| row["kind"] == "event" && row["event"]["source_path"] == path)
            .map(|row| {
                let mut event: RawEvent = serde_json::from_value(row["event"].clone()).unwrap();
                if path == remote_path {
                    event.project_root = Some(cwd.to_owned());
                    event.project_root_source = Some("git".into());
                    event.workspace_location = Some("local".into());
                }
                event
            })
            .collect();
        assert_eq!(events.len(), event_count);
        let stats = store
            .apply_projection(FileProjectionBatch {
                source: SourceKey {
                    source_type: events[0].source_type,
                    source_location: events[0].source_location.clone(),
                    source_path: path.to_owned(),
                },
                parser_revision: Some(revision),
                mode: Projection::Append,
                events,
                token: None,
            })
            .unwrap();
        assert_eq!(stats.appended, event_count as u64);
    }
    let [old_ssh_head, local_head] = [remote_path, local_path].map(head);
    std::fs::write(&fixture.state, serde_json::to_vec(&state).unwrap()).unwrap();

    let current = || -> Vec<Value> {
        fixture
            .run(&["pull", "--projection", "current"])
            .into_iter()
            .filter(|row| row["kind"] == "pulled")
            .map(|row| row["event"].clone())
            .collect()
    };
    let old_events = current();
    assert_eq!(old_events.len(), 3);
    for event in &old_events {
        assert_eq!(event["cwd"], cwd);
        assert_eq!(event["project_root"], cwd);
        assert_eq!(event["project_root_source"], "git");
        assert_eq!(event["workspace_location"], "local");
    }
    let changes = || store.read_projection_changes(0, 100).unwrap();
    let prior_changes = changes();
    let rows = scan(true);
    let writes = rows_by_kind(&rows, "store_write");
    assert_eq!(writes.len(), 2, "{rows:?}");
    let ssh_write = writes
        .iter()
        .find(|row| row["source_path"] == remote_path)
        .unwrap();
    assert_eq!(ssh_write["mode"], "reparse");
    assert_eq!(ssh_write["head_moved"], true);
    assert_eq!(ssh_write["appended"], 2);
    let local_write = writes
        .iter()
        .find(|row| row["source_path"] == local_path)
        .unwrap();
    assert_eq!(local_write["mode"], "append");
    assert_eq!(local_write["head_moved"], false);
    assert_eq!(local_write["appended"], 0);
    let bytes_read = |rows: &[Value], path| {
        rows.iter()
            .find(|row| row["kind"] == "source_report" && row["report"]["source_path"] == path)
            .unwrap()["report"]["bytes_read"]
            .as_u64()
            .unwrap()
    };
    assert_eq!(bytes_read(&rows, remote_path), remote_bytes.len() as u64);
    assert_eq!(bytes_read(&rows, local_path), 0);
    let remote_head = (old_ssh_head.0, old_ssh_head.1 + 1);
    assert_eq!(head(remote_path), remote_head);
    assert_eq!(head(local_path), local_head);
    let updated = fixture.state();
    let remote_key = format!("claude_code|local|{remote_path}");
    assert_eq!(updated[&remote_key]["parser_revision"], PARSER_REVISION);
    assert_eq!(
        updated[&remote_key]["fingerprint"],
        state[&remote_key]["fingerprint"]
    );
    assert_eq!(std::fs::read_to_string(&remote).unwrap(), remote_bytes);
    assert_eq!(
        std::fs::metadata(&remote).unwrap().modified().unwrap(),
        mtime
    );

    let mut expected = old_events;
    for event in expected
        .iter_mut()
        .filter(|event| event["source_path"] == remote_path)
    {
        event["project_root"] = json!(remote_root);
        event["project_root_source"] = json!("unattributed");
        event["workspace_location"] = json!(namespace);
    }
    let sort = |events: &mut Vec<Value>| {
        events.sort_by_key(|event| event["content"].as_str().unwrap().to_owned())
    };
    sort(&mut expected);
    let mut events = current();
    sort(&mut events);
    assert_eq!(events, expected);
    let new_changes = changes();
    assert_eq!(new_changes.len(), prior_changes.len() + 1);
    let change = new_changes.last().unwrap();
    assert_eq!(change.source_path, remote_path);
    assert_eq!(change.reason, "reparse");
    assert_eq!(change.old_source_revision, Some(old_ssh_head.0));
    assert_eq!(change.old_projection_revision, Some(old_ssh_head.1));
    assert_eq!(
        (change.new_source_revision, change.new_projection_revision),
        remote_head
    );

    let again = scan(true);
    let writes = rows_by_kind(&again, "store_write");
    assert_eq!(writes.len(), 2, "{again:?}");
    for write in writes {
        assert_eq!(write["mode"], "append");
        assert_eq!(write["head_moved"], false);
        assert_eq!(write["appended"], 0);
    }
    assert_eq!(bytes_read(&again, remote_path), 0);
    assert_eq!(head(remote_path), remote_head);
    assert_eq!(head(local_path), local_head);
    assert_eq!(changes(), new_changes);
    assert_eq!(fixture.state(), updated);
}

fn rows_by_kind<'a>(rows: &'a [Value], kind: &str) -> Vec<&'a Value> {
    rows.iter().filter(|row| row["kind"] == kind).collect()
}

fn mode(rows: &[Value]) -> &str {
    let writes = rows_by_kind(rows, "store_write");
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
