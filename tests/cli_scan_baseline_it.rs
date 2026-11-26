//! 用隔离的 CLI、总库和状态文件核验全读的起点及来源基线。
#![cfg(feature = "store")]
#![allow(clippy::disallowed_methods)]

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use serde_json::{json, Value};

static NEXT_RUN: AtomicU64 = AtomicU64::new(0);
#[cfg(windows)]
const AGY_ROOT: &str = "agy data #一";
#[cfg(windows)]
const AGY_ID: &str = "11111111-2222-3333-4444-555555555555";
#[cfg(windows)]
const AGY_OTHER_ID: &str = "22222222-2222-3333-4444-555555555555";

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
        #[cfg(windows)]
        {
            // Rust 在 System32 前搜索启动程序所在目录。让 CLI 从夹具启动，并让
            // wsl.exe 在该处确定失败，不会枚举宿主发行版（也不靠改宿主 PATH）。
            let cli = env!("CARGO_BIN_EXE_svault");
            let local_cli = root.join("svault.exe");
            std::fs::hard_link(cli, &local_cli)
                .or_else(|_| std::fs::copy(cli, &local_cli).map(|_| ()))
                .unwrap();
            std::fs::write(root.join("wsl.exe"), b"not an executable").unwrap();
        }
        Self::at(root)
    }

    fn at(root: PathBuf) -> Self {
        Self {
            source: root.join("source").join("sessions").join("fixture.jsonl"),
            state: root.join("state.json"),
            store: root.join("fixture.db"),
            root,
        }
    }

    fn command(&self) -> Command {
        #[cfg(not(windows))]
        let program = env!("CARGO_BIN_EXE_svault");
        #[cfg(windows)]
        let program = self.root.join("svault.exe");
        self.command_for(program)
    }

    fn command_for(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut command = Command::new(program);
        command.env_remove("SVAULT_ANTIGRAVITY_CLI_DIR");
        #[cfg(windows)]
        command.env("SVAULT_ANTIGRAVITY_CLI_DIR", self.root.join(AGY_ROOT));
        command
            .current_dir(&self.root)
            .env("SVAULT_LOG", "info")
            .env("SVAULT_JSONL_DIR", self.root.join("source"))
            .env_remove("SVAULT_DSH_SESSIONS_DIR")
            .env_remove("SVAULT_DSH_HOME")
            .env_remove("SVAULT_ANTIGRAVITY_SESSION_BINDINGS")
            .env("SVAULT_KEY_FILE", self.root.join("fixture.key"))
            .env(
                "SVAULT_SESSION_BINDINGS",
                self.root.join("empty-bindings.json"),
            )
            .env("CLAUDE_CONFIG_DIR", self.root.join(".claude"))
            .env("CODEX_HOME", self.root.join("empty-codex"))
            .env_remove("SVAULT_ACCEPTANCE_KEY");
        command
    }

    fn output(&self, args: &[&str]) -> std::process::Output {
        self.output_with(self.command(), args)
    }

    fn output_with(&self, mut command: Command, args: &[&str]) -> std::process::Output {
        command
            .args(args)
            .arg("--store")
            .arg(&self.store)
            .output()
            .unwrap()
    }

    fn run(&self, args: &[&str]) -> Vec<Value> {
        let output = self.output(args);
        successful_rows(args, output)
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

    #[cfg(windows)]
    fn antigravity(&self) -> Vec<Value> {
        let rows = self.scan_for("antigravity-cli", false);
        let summary = only_row(&rows, "summary");
        assert_eq!(summary["wrote_store"], false);
        assert_eq!(summary["state_saved"], true);
        assert!(rows_by_kind(&rows, "store_write").is_empty());
        rows
    }

    #[cfg(windows)]
    fn seed_store(&self) -> session_vault::TotalStore {
        use base64::{engine::general_purpose::STANDARD_NO_PAD, Engine};
        use session_vault::{StoreKey, TotalStore};

        let key = [7; 32];
        std::fs::write(self.root.join("fixture.key"), STANDARD_NO_PAD.encode(key)).unwrap();
        TotalStore::open_with_key(&self.store, StoreKey::from_bytes(key)).unwrap()
    }

    fn state(&self) -> Value {
        serde_json::from_slice(&std::fs::read(&self.state).unwrap()).unwrap()
    }

    #[cfg(windows)]
    fn antigravity_cursor_path(&self) -> PathBuf {
        self.root.join("state.json.antigravity.json")
    }

    #[cfg(windows)]
    fn antigravity_cursors(&self) -> Value {
        serde_json::from_slice(&std::fs::read(self.antigravity_cursor_path()).unwrap()).unwrap()
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

// 保留非 Windows 的本机 SSH 转录回归。
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

#[cfg(windows)]
#[test]
fn antigravity_explicit_and_mixed_write_requests_are_rejected_before_creating_a_store() {
    let fixture = Fixture::new();
    std::fs::write(agy_transcript(&fixture, AGY_ID), agy_user(0, "antigravity")).unwrap();
    std::fs::write(&fixture.source, messages(&["ordinary"])).unwrap();
    for only in [
        vec!["--only", "antigravity-cli"],
        vec!["--only", "jsonl", "--only", "antigravity-cli"],
    ] {
        let mut args = vec![
            "scan-all",
            "--write-store",
            "--state",
            fixture.state.to_str().unwrap(),
        ];
        args.extend(only);
        let output = fixture.output(&args);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(1), "{stderr}");
        assert!(
            stderr.contains("antigravity-cli is a read-only source; omit --write-store"),
            "{stderr}"
        );
        assert!(output.stdout.is_empty());
        assert!(!fixture.store.exists());
        assert!(!fixture.root.join("fixture.key").exists());
        assert!(!fixture.state.exists());
    }
}

#[cfg(windows)]
#[test]
fn antigravity_rejects_state_paths_inside_the_source_tree() {
    let fixture = Fixture::new();
    let source = agy_transcript(&fixture, AGY_ID);
    let transcript = agy_user(0, "first");
    std::fs::write(&source, &transcript).unwrap();
    let (_, summary) = agy_register_workspace(&fixture, "project");
    drop(summary);
    let data_root = fixture.root.join(AGY_ROOT);
    let summary_path = data_root.join("conversation_summaries.db");
    let summary_bytes = std::fs::read(&summary_path).unwrap();
    let missing = data_root.join("new").join("state.json");

    // 不依赖路径字符串恰好一样：大小写和 `..` 由 Windows 按同一目录解释。
    let source_alias = source
        .parent()
        .unwrap()
        .join("..")
        .join("LOGS")
        .join("transcript.jsonl");
    for path in [&source_alias, &summary_path, &missing] {
        let result = fixture.output(&[
            "scan-all",
            "--only",
            "antigravity-cli",
            "--state",
            path.to_str().unwrap(),
        ]);
        assert_eq!(std::fs::read(&source).unwrap(), transcript.as_bytes());
        assert_eq!(std::fs::read(&summary_path).unwrap(), summary_bytes);
        assert!(!missing.exists());
        assert_eq!(
            result.status.code(),
            Some(1),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(result.stdout.is_empty());
        assert!(String::from_utf8_lossy(&result.stderr).contains("Antigravity CLI data root"));
    }
    let stateless = fixture.run(&[
        "scan-all",
        "--only",
        "antigravity-cli",
        "--stateless",
        "--profile",
        "full",
        "--state",
        source_alias.to_str().unwrap(),
    ]);
    assert_eq!(event_contents(&stateless), ["first"]);
    assert!(only_row(&stateless, "summary")["state_saved"].is_null());
    assert_eq!(std::fs::read(&source).unwrap(), transcript.as_bytes());
    assert_eq!(std::fs::read(&summary_path).unwrap(), summary_bytes);
    assert_eq!(event_contents(&fixture.antigravity()), ["first"]);
    assert!(fixture.antigravity_cursor_path().is_file());
    assert!(!fixture.state.exists());
}

#[cfg(windows)]
#[test]
fn antigravity_state_save_never_overwrites_a_preexisting_staging_path() {
    let fixture = Fixture::new();
    let source = agy_transcript(&fixture, AGY_ID);
    let transcript = agy_user(0, "first");
    std::fs::write(&source, &transcript).unwrap();
    let old_stage = fixture.antigravity_cursor_path().with_extension("json.tmp");
    std::fs::hard_link(&source, &old_stage).unwrap();

    assert_eq!(event_contents(&fixture.antigravity()), ["first"]);
    assert_eq!(std::fs::read(&source).unwrap(), transcript.as_bytes());
    assert_eq!(std::fs::read(&old_stage).unwrap(), transcript.as_bytes());
    assert!(event_contents(&fixture.antigravity()).is_empty());
}

#[cfg(windows)]
#[test]
fn antigravity_streams_only_the_canonical_transcript_and_detects_rewrites() {
    let fixture = Fixture::new();
    let root = fixture.root.join(AGY_ROOT);
    let source = agy_transcript(&fixture, AGY_ID);
    let full_transcript = source.with_file_name("transcript_full.jsonl");
    let stray_transcript = root.join("transcript.jsonl");
    let decoy = agy_user(0, "never read this");
    std::fs::write(&full_transcript, &decoy).unwrap();
    std::fs::write(&stray_transcript, &decoy).unwrap();
    let non_messages = agy_step(
        1,
        "not a user message",
        "VIEW_FILE",
        "USER_EXPLICIT",
        "DONE",
    ) + &agy_step(2, "unfinished", "PLANNER_RESPONSE", "MODEL", "RUNNING");
    let initial = agy_user(0, "first") + &non_messages;
    std::fs::write(&source, &initial).unwrap();
    let first = fixture.antigravity();
    let event = &only_row(&first, "event")["event"];
    assert_eq!(event["source_type"], "antigravity_cli");
    assert_eq!(event["source_session_id"], AGY_ID);
    assert_eq!(event["content"], "first");
    assert_ne!(event["input_origin"]["sender_kind"], "human_verified");
    assert!(!fixture.store.exists());
    assert!(!fixture.root.join("fixture.key").exists());
    assert!(!root.join("conversation_summaries.db").exists());

    let answer = agy_step(3, "answer", "PLANNER_RESPONSE", "MODEL", "DONE");
    let appended = initial + &answer;
    std::fs::write(&source, &appended).unwrap();
    let incremental = fixture.antigravity();
    assert_eq!(event_contents(&incremental), ["answer"]);
    assert!(only_row(&incremental, "source_report")
        .get("replacement")
        .is_none());
    let before = fixture.antigravity_cursors();
    let mtime = std::fs::metadata(&source).unwrap().modified().unwrap();
    let corrected = agy_user(0, "fixed") + &non_messages + &answer;
    assert_eq!(corrected.len(), appended.len());
    std::fs::write(&source, &corrected).unwrap();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&source)
        .unwrap()
        .set_modified(mtime)
        .unwrap();
    assert_eq!(
        std::fs::metadata(&source).unwrap().modified().unwrap(),
        mtime
    );
    let rewritten = fixture.antigravity();
    assert_eq!(
        only_row(&rewritten, "source_report")["replacement"],
        "rollback_or_rewrite"
    );
    assert_eq!(event_contents(&rewritten), ["fixed", "answer"]);
    assert_ne!(fixture.antigravity_cursors(), before);
    let steady = fixture.antigravity();
    assert!(event_contents(&steady).is_empty());
    assert!(only_row(&steady, "source_report")
        .get("replacement")
        .is_none());
    assert_eq!(std::fs::read(&source).unwrap(), corrected.as_bytes());
    for decoy_path in [full_transcript, stray_transcript] {
        assert_eq!(std::fs::read(decoy_path).unwrap(), decoy.as_bytes());
    }
    assert!(!fixture.store.exists());
}

#[cfg(windows)]
#[test]
fn antigravity_reads_committed_wal_during_a_write_and_after_full_checkpoint() {
    let fixture = Fixture::new();
    let source = agy_transcript(&fixture, AGY_ID);
    let transcript = agy_user(0, "first");
    std::fs::write(&source, &transcript).unwrap();
    assert!(only_row(&fixture.antigravity(), "event")["event"]["project_root"].is_null());

    let (project, summary) = agy_register_workspace(&fixture, "my repo");
    let store_before = std::fs::read(&fixture.store).unwrap();
    let summary_path = fixture
        .root
        .join(AGY_ROOT)
        .join("conversation_summaries.db");
    let summary_before = std::fs::read(&summary_path).unwrap();
    let streamed = fixture.antigravity();
    let event = &only_row(&streamed, "event")["event"];
    assert_eq!(event["project_root"], project.to_str().unwrap());
    assert!(event["cwd"].is_null());
    assert_eq!(
        only_row(&streamed, "source_report")["replacement"],
        "reparse_required"
    );
    assert_eq!(std::fs::read(&summary_path).unwrap(), summary_before);
    assert!(!summary_path.with_extension("db-shm").exists());
    let state_before = fixture.antigravity_cursors();
    let state_bytes = std::fs::read(fixture.antigravity_cursor_path()).unwrap();
    let stateless = fixture.run(&[
        "scan-all",
        "--only",
        "antigravity-cli",
        "--profile",
        "metadata",
        "--stateless",
        "--state",
        fixture.state.to_str().unwrap(),
    ]);
    let event = &only_row(&stateless, "event")["event"];
    assert_eq!(event["project_root"], project.to_str().unwrap());
    assert!(event["content"].is_null());
    assert!(only_row(&stateless, "summary")["state_saved"].is_null());
    assert_eq!(
        std::fs::read(fixture.antigravity_cursor_path()).unwrap(),
        state_bytes
    );

    let original: String = summary
        .query_row(
            "SELECT workspace_uris FROM conversation_summaries",
            [],
            |row| row.get(0),
        )
        .unwrap();
    summary.pragma_update(None, "journal_mode", "WAL").unwrap();
    summary
        .pragma_update(None, "wal_autocheckpoint", 0)
        .unwrap();
    summary
        .execute_batch("UPDATE conversation_summaries SET workspace_uris = '[]'; BEGIN IMMEDIATE")
        .unwrap();
    summary
        .execute(
            "UPDATE conversation_summaries SET workspace_uris = ?1",
            [&original],
        )
        .unwrap();
    let wal = summary_path.with_extension("db-wal");
    let shm = summary_path.with_extension("db-shm");
    let source_bytes = || {
        use std::io::{Read, Seek, SeekFrom};

        // Win32 的 shm 锁字节不能用 ReadFile 跨读。核对锁区外的索引头、read marks 和页映射。
        let mut index = std::fs::File::open(&shm).unwrap();
        let mut bytes = vec![0; 120];
        index.read_exact(&mut bytes).unwrap();
        index.seek(SeekFrom::Start(129)).unwrap();
        index.read_to_end(&mut bytes).unwrap();
        [
            std::fs::read(&summary_path).unwrap(),
            std::fs::read(&wal).unwrap(),
            bytes,
        ]
    };
    let before = source_bytes();
    assert!(!before[1].is_empty());
    let committed = fixture.antigravity();
    assert_eq!(
        only_row(&committed, "source_report")["replacement"],
        "reparse_required"
    );
    assert!(
        only_row(&committed, "event")["event"]["project_root"].is_null(),
        "uncommitted attribution leaked: {committed:?}"
    );
    assert_ne!(fixture.antigravity_cursors(), state_before);
    assert_eq!(source_bytes(), before);
    summary.execute_batch("ROLLBACK").unwrap();

    let (busy, frames, checkpointed): (i32, i32, i32) = summary
        .query_row("PRAGMA wal_checkpoint(FULL)", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .unwrap();
    assert_eq!(busy, 0);
    assert!(frames > 0);
    assert_eq!(frames, checkpointed);
    let before = source_bytes();
    assert!(!before[1].is_empty());
    let appended = transcript + &agy_user(1, "tail");
    std::fs::write(&source, &appended).unwrap();
    let streamed = fixture.antigravity();
    assert_eq!(event_contents(&streamed), ["tail"]);
    assert!(only_row(&streamed, "event")["event"]["project_root"].is_null());
    assert!(only_row(&streamed, "source_report")
        .get("replacement")
        .is_none());
    assert_eq!(source_bytes(), before);

    summary
        .execute(
            "UPDATE conversation_summaries SET workspace_uris = ?1",
            [&original],
        )
        .unwrap();
    let streamed = fixture.antigravity();
    assert_eq!(
        only_row(&streamed, "source_report")["replacement"],
        "reparse_required"
    );
    assert_eq!(event_contents(&streamed), ["first", "tail"]);
    for row in rows_by_kind(&streamed, "event") {
        assert_eq!(row["event"]["project_root"], project.to_str().unwrap());
    }
    summary.close().unwrap();
    assert!(!wal.exists() && !shm.exists());
    let appended = appended + &agy_user(2, "after close");
    std::fs::write(&source, &appended).unwrap();
    let streamed = fixture.antigravity();
    assert_eq!(event_contents(&streamed), ["after close"]);
    assert_eq!(
        only_row(&streamed, "event")["event"]["project_root"],
        project.to_str().unwrap()
    );
    assert!(!wal.exists() && !shm.exists());
    assert_eq!(std::fs::read(&source).unwrap(), appended.as_bytes());
    assert_eq!(std::fs::read(&fixture.store).unwrap(), store_before);
    assert!(fixture.contents("all").is_empty());
}

#[cfg(windows)]
#[test]
fn antigravity_rollback_writer_exclusive_lock_freezes_cursor_until_retry() {
    let fixture = Fixture::new();
    let source = agy_transcript(&fixture, AGY_ID);
    let (project, summary) = agy_register_workspace(&fixture, "project");
    let initial = agy_user(0, "first");
    std::fs::write(&source, &initial).unwrap();
    assert_eq!(
        only_row(&fixture.antigravity(), "event")["event"]["project_root"],
        project.to_str().unwrap()
    );
    let before = fixture.antigravity_cursors();
    let tail = initial + &agy_user(1, "second");
    std::fs::write(&source, &tail).unwrap();
    summary
        .execute_batch("BEGIN EXCLUSIVE; UPDATE conversation_summaries SET workspace_uris = '[]'")
        .unwrap();
    let summary_path = fixture
        .root
        .join(AGY_ROOT)
        .join("conversation_summaries.db");
    let journal_path = summary_path.with_extension("db-journal");
    let journal = std::fs::read(&journal_path).unwrap();
    assert!(!journal.is_empty());
    let held = fixture.antigravity();
    let report = only_row(&held, "source_report");
    assert!(report.get("replacement").is_none());
    assert!(report["report"]["warnings"][0]
        .as_str()
        .unwrap()
        .contains("summary"));
    assert!(event_contents(&held).is_empty());
    assert_eq!(fixture.antigravity_cursors(), before);
    assert_eq!(std::fs::read(&journal_path).unwrap(), journal);
    assert!(!summary_path.with_extension("db-wal").exists());
    assert!(!summary_path.with_extension("db-shm").exists());
    summary.execute_batch("ROLLBACK").unwrap();
    let recovered = fixture.antigravity();
    assert_eq!(event_contents(&recovered), ["second"]);
    assert_eq!(
        only_row(&recovered, "event")["event"]["project_root"],
        project.to_str().unwrap()
    );
    assert!(only_row(&recovered, "source_report")
        .get("replacement")
        .is_none());
    assert_ne!(fixture.antigravity_cursors(), before);
}

#[cfg(windows)]
#[test]
fn antigravity_does_not_create_missing_wal_index_or_read_a_stale_main_file() {
    use rusqlite::config::DbConfig;

    let fixture = Fixture::new();
    let source = agy_transcript(&fixture, AGY_ID);
    let (project, writer) = agy_register_workspace(&fixture, "project");
    let initial = agy_user(0, "first");
    std::fs::write(&source, &initial).unwrap();
    assert_eq!(
        only_row(&fixture.antigravity(), "event")["event"]["project_root"],
        project.to_str().unwrap()
    );
    let state_before = fixture.antigravity_cursors();
    let path = fixture
        .root
        .join(AGY_ROOT)
        .join("conversation_summaries.db");
    let wal = path.with_extension("db-wal");
    let shm = path.with_extension("db-shm");
    writer
        .execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;
         UPDATE conversation_summaries SET workspace_uris = '[]';",
        )
        .unwrap();
    writer
        .set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, true)
        .unwrap();
    writer.close().unwrap();
    let main_before = std::fs::read(&path).unwrap();
    let wal_before = std::fs::read(&wal).unwrap();
    assert!(!wal_before.is_empty());
    let shm_bytes = std::fs::read(&shm).unwrap();
    std::fs::remove_file(&shm).unwrap();
    let tail = initial + &agy_user(1, "second");
    std::fs::write(&source, tail).unwrap();

    let held = fixture.antigravity();
    assert!(event_contents(&held).is_empty());
    assert!(only_row(&held, "source_report")
        .get("replacement")
        .is_none());
    assert_eq!(fixture.antigravity_cursors(), state_before);
    assert!(!shm.exists());
    assert_eq!(std::fs::read(&path).unwrap(), main_before);
    assert_eq!(std::fs::read(&wal).unwrap(), wal_before);

    // The fixture restores the existing index. A read-only connection may recover
    // its own private index while the writer stays closed.
    std::fs::write(&shm, &shm_bytes).unwrap();
    let recovered = fixture.antigravity();
    assert_eq!(
        only_row(&recovered, "source_report")["replacement"],
        "reparse_required"
    );
    assert_eq!(event_contents(&recovered), ["first", "second"]);
    for row in rows_by_kind(&recovered, "event") {
        assert!(row["event"]["project_root"].is_null());
    }
    assert_eq!(std::fs::read(&path).unwrap(), main_before);
    assert_eq!(std::fs::read(&wal).unwrap(), wal_before);
    assert_eq!(std::fs::read(&shm).unwrap(), shm_bytes);
}

#[cfg(windows)]
fn agy_transcript(fixture: &Fixture, id: &str) -> PathBuf {
    let logs = fixture
        .root
        .join(AGY_ROOT)
        .join("brain")
        .join(id)
        .join(".system_generated")
        .join("logs");
    std::fs::create_dir_all(&logs).unwrap();
    logs.join("transcript.jsonl")
}

#[cfg(windows)]
fn agy_step(index: u64, content: &str, kind: &str, actor: &str, status: &str) -> String {
    json!({"step_index": index, "type": kind, "source": actor, "status": status,
        "created_at": "2026-11-26T00:00:00Z", "content": content})
    .to_string()
        + "\n"
}

#[cfg(windows)]
fn agy_user(index: u64, text: &str) -> String {
    agy_step(index, text, "USER_INPUT", "USER_EXPLICIT", "DONE")
}

#[cfg(windows)]
fn agy_register_workspace(
    fixture: &Fixture,
    project_name: &str,
) -> (PathBuf, rusqlite::Connection) {
    use session_vault::attribution::RootSource;

    let project = fixture.root.join(project_name);
    std::fs::create_dir(&project).unwrap();
    let store = fixture.seed_store();
    store.register_project_root(project.to_str().unwrap(), RootSource::Git);
    drop(store);
    let summary = rusqlite::Connection::open(
        fixture
            .root
            .join(AGY_ROOT)
            .join("conversation_summaries.db"),
    )
    .unwrap();
    summary.execute_batch("CREATE TABLE conversation_summaries(conversation_id TEXT PRIMARY KEY, workspace_uris TEXT)").unwrap();
    let workspace = format!(
        "file:///{}",
        project
            .to_str()
            .unwrap()
            .replace('\\', "/")
            .replace(' ', "%20")
    );
    summary
        .execute(
            "INSERT INTO conversation_summaries VALUES (?1, ?2)",
            [AGY_ID, &json!([workspace]).to_string()],
        )
        .unwrap();
    (project, summary)
}

#[cfg(windows)]
#[test]
fn antigravity_only_scan_does_not_attempt_wsl_even_with_an_existing_store() {
    use session_vault::attribution::RootSource;

    let fixture = Fixture::new();
    let local = agy_transcript(&fixture, AGY_ID);
    let unc = agy_transcript(&fixture, AGY_OTHER_ID);
    std::fs::write(local, agy_user(0, "local workspace")).unwrap();
    std::fs::write(unc, agy_user(0, "registered UNC workspace")).unwrap();
    std::fs::write(&fixture.source, messages(&["ordinary JSONL"])).unwrap();
    let (project, summary) = agy_register_workspace(&fixture, "project");
    let wsl_root = "wsl:fixture-distro:/home/dev/project";
    let store = fixture.seed_store();
    store.register_project_root(wsl_root, RootSource::Git);
    drop(store);
    summary
        .execute(
            "INSERT INTO conversation_summaries VALUES (?1, ?2)",
            [
                AGY_OTHER_ID,
                r#"["file://wsl.localhost/fixture-distro/home/dev/project/work"]"#,
            ],
        )
        .unwrap();
    summary.close().unwrap();

    let selected = [
        "scan-all",
        "--profile",
        "full",
        "--stateless",
        "--only",
        "antigravity-cli",
    ];
    let check_directed = |args: &[&str], output: std::process::Output, sources| {
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("wsl.exe spawn attempts during scan-all: 0 ("),
            "{stderr}"
        );
        let rows = successful_rows(args, output);
        let summary = only_row(&rows, "summary");
        assert_eq!(summary["sources"], sources);
        assert_eq!(summary["roots_empty"], false);
        assert!(summary["discovery_unreachable"].is_null());
        rows
    };
    let duplicate = [
        "scan-all",
        "--profile",
        "full",
        "--state",
        fixture.state.to_str().unwrap(),
        "--only",
        "antigravity-cli",
        "--only",
        "antigravity-cli",
    ];
    for args in [selected.as_slice(), &duplicate] {
        let rows = check_directed(args, fixture.output(args), 2);
        let events = rows_by_kind(&rows, "event");
        assert_eq!(events.len(), 2);
        for (id, root, workspace) in [
            (AGY_ID, project.to_str().unwrap(), "local"),
            (AGY_OTHER_ID, wsl_root, "wsl:fixture-distro"),
        ] {
            let event = events
                .iter()
                .map(|row| &row["event"])
                .find(|event| event["source_session_id"] == id)
                .unwrap();
            assert_eq!(event["source_type"], "antigravity_cli");
            assert_eq!(event["source_location"], json!({"kind": "local"}));
            assert_eq!(event["project_root"], root);
            assert_eq!(event["workspace_location"], workspace);
        }
    }
    assert_eq!(fixture.antigravity_cursors().as_object().unwrap().len(), 2);
    let mut disabled = fixture.command();
    disabled.env_remove("SVAULT_ANTIGRAVITY_CLI_DIR");
    let rows = check_directed(&selected, fixture.output_with(disabled, &selected), 0);
    assert!(event_contents(&rows).is_empty());

    // Ordinary discovery tries the stub for both append-log and snapshot sources;
    // loading the existing store then tries it for drive mounts.
    let mixed = [selected.as_slice(), &["--only", "jsonl"]].concat();
    let output = fixture.output(&mixed);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("wsl.exe spawn attempts during scan-all: 3 ("),
        "{stderr}"
    );
    assert!(
        stderr.contains("drive mounts: none — cannot list distros"),
        "{stderr}"
    );
    let rows = successful_rows(&mixed, output);
    let summary = only_row(&rows, "summary");
    assert_eq!(summary["sources"], 3);
    assert_eq!(summary["discovery_unreachable"], json!(["wsl:*"]));
    assert!(event_contents(&rows).contains(&"ordinary JSONL"));
}

#[cfg(windows)]
#[test]
fn writer_discovery_excludes_antigravity_before_enumeration() {
    const CHILD_SOURCE: &str = "SVAULT_TEST_WRITER_DISCOVERY_SOURCE";
    if let Some(path) = std::env::var_os(CHILD_SOURCE) {
        let outcome = session_vault::discover_transcripts_reported(
            session_vault::deadline::Deadline::unbounded(),
        )
        .unwrap();
        let paths: Vec<_> = outcome
            .sources
            .into_iter()
            .map(|source| source.path)
            .collect();
        assert_eq!(paths, [PathBuf::from(path)]);
        assert_eq!(outcome.unreachable, ["wsl:*"]);
        return;
    }

    let fixture = Fixture::new();
    let healthy = agy_transcript(&fixture, AGY_ID);
    let malformed = agy_transcript(&fixture, AGY_OTHER_ID);
    std::fs::write(healthy, agy_user(0, "read-only")).unwrap();
    std::fs::create_dir(malformed).unwrap();
    std::fs::write(&fixture.source, messages(&["ordinary"])).unwrap();

    let args = ["discover"];
    let discovery = successful_rows(&args, fixture.command().args(args).output().unwrap());
    assert!(rows_by_kind(&discovery, "source")
        .iter()
        .any(|row| row["source_type"] == "antigravity_cli"));
    assert_eq!(
        only_row(&discovery, "summary")["discovery_unreachable"],
        json!(["local", "wsl:*"])
    );

    let writes = fixture.run(&["scan-all", "--write-store", "--stateless"]);
    let summary = only_row(&writes, "summary");
    assert_eq!(summary["written_events"], 1);
    assert_eq!(summary["discovery_unreachable"], json!(["wsl:*"]));
    let selected = fixture.run(&["scan-all", "--only", "jsonl", "--stateless"]);
    assert_eq!(
        only_row(&selected, "summary")["discovery_unreachable"],
        json!(["wsl:*"])
    );

    let child = fixture.root.join("writer-discovery.exe");
    let test_exe = std::env::current_exe().unwrap();
    std::fs::hard_link(&test_exe, &child)
        .or_else(|_| std::fs::copy(&test_exe, &child).map(|_| ()))
        .unwrap();
    let output = fixture
        .command_for(child)
        .env(CHILD_SOURCE, &fixture.source)
        .args([
            "--exact",
            "writer_discovery_excludes_antigravity_before_enumeration",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
}

#[cfg(windows)]
#[test]
fn mixed_and_directed_antigravity_scans_keep_separate_cursors() {
    let fixture = Fixture::new();
    let source = agy_transcript(&fixture, AGY_ID);
    let initial = agy_user(0, "first");
    std::fs::write(&source, &initial).unwrap();
    std::fs::write(&fixture.source, messages(&["ordinary"])).unwrap();
    let mixed_args = [
        "scan-all",
        "--profile",
        "full",
        "--state",
        fixture.state.to_str().unwrap(),
    ];
    let mixed = fixture.run(&mixed_args);
    assert_eq!(rows_by_kind(&mixed, "event").len(), 2);
    assert_eq!(only_row(&mixed, "summary")["state_saved"], true);
    let shared = fixture.state();
    let shared = shared.as_object().unwrap();
    assert_eq!(shared.len(), 1);
    assert!(shared.keys().all(|key| key.starts_with("jsonl|")));
    let key = format!("antigravity_cli|local|{}", source.display());
    let dedicated = fixture.antigravity_cursors();
    let content_hash = dedicated[&key]["content_hash"].as_str().unwrap();
    assert!(
        content_hash.starts_with("agv1:sha256:") && content_hash.contains(";attr=sha256:"),
        "{content_hash}"
    );
    assert!(dedicated[&key]
        .get("antigravity_attribution_hash")
        .is_none());
    assert_eq!(dedicated.as_object().unwrap().len(), 1);

    let shared_bytes = std::fs::read(&fixture.state).unwrap();
    assert!(event_contents(&fixture.antigravity()).is_empty());
    assert_eq!(std::fs::read(&fixture.state).unwrap(), shared_bytes);
    std::fs::write(&source, initial + &agy_user(1, "tail")).unwrap();
    assert_eq!(event_contents(&fixture.antigravity()), ["tail"]);
    assert!(event_contents(&fixture.run(&mixed_args)).is_empty());
}

#[cfg(windows)]
#[test]
fn antigravity_resumes_from_pre_split_cursor_without_rewriting_shared_file() {
    let fixture = Fixture::new();
    let source = agy_transcript(&fixture, AGY_ID);
    std::fs::write(&source, agy_user(0, "first")).unwrap();
    assert_eq!(event_contents(&fixture.antigravity()), ["first"]);
    let dedicated = fixture.antigravity_cursor_path();
    let initial = std::fs::read(&dedicated).unwrap();
    // 模拟拆分前的开发版：完整 Antigravity 游标仍在共享文件中。
    std::fs::rename(&dedicated, &fixture.state).unwrap();
    let resumed = fixture.antigravity();
    assert!(event_contents(&resumed).is_empty());
    assert!(only_row(&resumed, "source_report")
        .get("replacement")
        .is_none());
    assert_eq!(std::fs::read(&fixture.state).unwrap(), initial);
    assert_eq!(fixture.antigravity_cursors(), fixture.state());

    // 模拟更早开发版（裸 sha256 + 独立 antigravity_attribution_hash 字段）：
    // 反序列化不报错，并按缺少内嵌归属指纹触发一次 reparse_required 升级到 agv1 双指纹。
    let key = format!("antigravity_cli|local|{}", source.display());
    let mut legacy = fixture.antigravity_cursors();
    let encoded = legacy[&key]["content_hash"].as_str().unwrap().to_string();
    let (prefix, attr) = encoded
        .strip_prefix("agv1:")
        .unwrap()
        .split_once(";attr=")
        .unwrap();
    legacy[&key]["content_hash"] = json!(prefix);
    legacy[&key]["antigravity_attribution_hash"] = json!(attr);
    std::fs::write(&dedicated, serde_json::to_vec_pretty(&legacy).unwrap()).unwrap();
    let upgraded = fixture.antigravity();
    assert_eq!(
        only_row(&upgraded, "source_report")["replacement"],
        "reparse_required"
    );
    assert_eq!(event_contents(&upgraded), ["first"]);
    let after = fixture.antigravity_cursors();
    assert_eq!(after[&key]["content_hash"], encoded);
    assert!(after[&key].get("antigravity_attribution_hash").is_none());
    assert!(event_contents(&fixture.antigravity()).is_empty());
}

#[cfg(windows)]
#[test]
fn default_write_scan_skips_antigravity_without_advancing_its_streaming_cursor() {
    let fixture = Fixture::new();
    let source = agy_transcript(&fixture, AGY_ID);
    let initial = agy_user(0, "first");
    std::fs::write(&source, &initial).unwrap();
    assert_eq!(event_contents(&fixture.antigravity()), ["first"]);
    let before = fixture.antigravity_cursors();
    let agy_key = format!("antigravity_cli|local|{}", source.display());
    assert_eq!(before[&agy_key]["safe_offset"], initial.len());
    std::fs::write(&source, initial + &agy_user(1, "tail")).unwrap();
    std::fs::write(&fixture.source, messages(&["ordinary"])).unwrap();
    let selected = fixture.scan();
    assert_eq!(only_row(&selected, "store_write")["appended"], 1);
    assert_eq!(fixture.contents("all"), ["ordinary"]);
    assert_eq!(fixture.antigravity_cursors()[&agy_key], before[&agy_key]);

    std::fs::write(&fixture.source, messages(&["ordinary", "legacy tail"])).unwrap();
    let args = [
        "scan-all",
        "--write-store",
        "--state",
        fixture.state.to_str().unwrap(),
    ];
    let output = fixture.output(&args);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("spawn wsl.exe failed:"), "{stderr}");
    let rows = successful_rows(&args, output);
    let report = only_row(&rows, "source_report");
    assert_eq!(
        report["report"]["source_path"],
        fixture.source.to_str().unwrap()
    );
    let summary = only_row(&rows, "summary");
    assert_eq!(summary["sources"], 1);
    assert_eq!(summary["wrote_store"], true);
    assert_eq!(summary["written_events"], 1);
    assert_eq!(summary["discovery_unreachable"], json!(["wsl:*"]));
    assert_eq!(fixture.contents("all"), ["ordinary", "legacy tail"]);
    assert_eq!(fixture.antigravity_cursors()[&agy_key], before[&agy_key]);
    assert_eq!(event_contents(&fixture.antigravity()), ["tail"]);
}

#[cfg(windows)]
fn agy_binding(temporary: bool, native_id: &str) -> Value {
    json!({
        "schemaVersion": 1,
        "owner": {"namespace": "tumechat.user", "id": "u"},
        "host": {"namespace": "tumechat.host", "id": "h"},
        "session": {"namespace": "tumechat.session", "id": "s"},
        "nativeSource": {"backend": "antigravity_cli", "sourceLocation": "local", "nativeSessionId": native_id},
        "temporary": temporary,
        "readMemory": false,
        "recordedAt": "2026-11-26T00:00:00Z"
    })
}

#[cfg(windows)]
#[test]
fn antigravity_streaming_honors_shared_and_dedicated_bindings_even_without_events() {
    use session_vault::SessionBindingRegistry;

    let fixture = Fixture::new();
    let native_id = "a1234567-b234-c345-d456-e56789012345";
    let upper_id = native_id.to_ascii_uppercase();
    let source = agy_transcript(&fixture, &upper_id);
    let initial = agy_user(0, "previous");
    std::fs::write(&source, &initial).unwrap();
    assert_eq!(event_contents(&fixture.antigravity()), ["previous"]);
    let before = fixture.antigravity_cursors();
    let store = fixture.seed_store();
    let dedicated = fixture.root.join("agy-bindings.json");
    let args = [
        "scan-all",
        "--only",
        "antigravity-cli",
        "--profile",
        "full",
        "--state",
        fixture.state.to_str().unwrap(),
    ];
    let scan = || {
        let mut command = fixture.command();
        command.env("SVAULT_ANTIGRAVITY_SESSION_BINDINGS", &dedicated);
        successful_rows(&args, fixture.output_with(command, &args))
    };
    let set_dedicated = |records| {
        let raw = json!({"schemaVersion": 1, "bindings": records}).to_string();
        std::fs::write(&dedicated, raw).unwrap();
    };
    let set_shared = |raw: Value| {
        store
            .set_session_bindings(&SessionBindingRegistry::from_json_str(&raw.to_string()))
            .unwrap();
    };
    set_dedicated(vec![agy_binding(false, native_id)]);
    set_shared(json!({"schemaVersion": 1, "temporaryRoots": [fixture.root.join(AGY_ROOT)]}));
    assert_eq!(
        only_row(&scan(), "source_report")["report"]["admission_status"],
        "temporary_excluded"
    );
    set_shared(json!({"schemaVersion": 1, "pendingSources": [{"sourcePath": source}]}));
    assert_eq!(
        only_row(&scan(), "source_report")["report"]["admission_status"],
        "unknown_hold"
    );
    let old_shared = store.load_session_bindings().to_canonical_json().unwrap();
    let invalid_shared = [
        agy_binding(true, native_id),
        json!([agy_binding(true, native_id)]),
        json!({"schemaVersion": 1, "bindings": [agy_binding(true, native_id)]}),
        json!({"schemaVersion": 1, "pendingSources": [{"backend": "antigravity_cli", "sourcePath": source}]}),
    ];
    for raw in invalid_shared {
        let registry = SessionBindingRegistry::from_json_str(&raw.to_string());
        let error = store.set_session_bindings(&registry).unwrap_err();
        assert!(error
            .to_string()
            .contains("SVAULT_ANTIGRAVITY_SESSION_BINDINGS"));
        assert_eq!(
            store.load_session_bindings().to_canonical_json(),
            Some(old_shared.clone())
        );
    }
    set_shared(json!({"schemaVersion": 1}));
    let replacement = agy_user(0, "replacement");
    let running = agy_step(0, "unfinished", "PLANNER_RESPONSE", "MODEL", "RUNNING");
    for (records, status) in [
        (vec![agy_binding(true, &upper_id)], "temporary_excluded"),
        (vec![agy_binding(true, native_id)], "temporary_excluded"),
        (
            vec![agy_binding(true, native_id), agy_binding(false, &upper_id)],
            "unknown_hold",
        ),
    ] {
        set_dedicated(records);
        for candidate in [initial.as_str(), "", running.as_str(), replacement.as_str()] {
            std::fs::write(&source, candidate).unwrap();
            let held = scan();
            let report = only_row(&held, "source_report");
            assert_eq!(
                report["report"]["admission_status"], status,
                "{candidate:?}"
            );
            assert!(report.get("replacement").is_none());
            assert!(event_contents(&held).is_empty());
            assert_eq!(fixture.antigravity_cursors(), before);
        }
    }
    set_dedicated(vec![agy_binding(false, native_id)]);
    let allowed = scan();
    let report = only_row(&allowed, "source_report");
    assert_eq!(report["report"]["admission_status"], "persistent_bound");
    assert_eq!(report["replacement"], "rollback_or_rewrite");
    assert_eq!(event_contents(&allowed), ["replacement"]);
    assert_ne!(fixture.antigravity_cursors(), before);
    drop(store);
    assert!(fixture.contents("all").is_empty());
}

#[cfg(windows)]
#[test]
fn antigravity_dedicated_binding_faults_hold_only_antigravity() {
    let fixture = Fixture::new();
    let source = agy_transcript(&fixture, AGY_ID);
    let dedicated = fixture.root.join("agy-bindings.json");
    std::fs::write(&source, agy_user(0, "antigravity")).unwrap();
    std::fs::write(&fixture.source, messages(&["legacy"])).unwrap();
    let args = [
        "scan-all",
        "--only",
        "antigravity-cli",
        "--only",
        "jsonl",
        "--profile",
        "full",
        "--stateless",
    ];
    let mixed = || {
        let mut command = fixture.command();
        command.env("SVAULT_ANTIGRAVITY_SESSION_BINDINGS", &dedicated);
        successful_rows(&args, fixture.output_with(command, &args))
    };
    let mut legacy_binding = agy_binding(true, "fixture");
    legacy_binding["nativeSource"]["backend"] = json!("jsonl");
    let legacy_pending = json!({"schemaVersion": 1, "pendingSources": [{"backend": "jsonl", "sourcePath": fixture.source}]});
    let cases = [
        (None, "explicit session bindings file not found"),
        (Some("{".to_string()), "invalid JSON in session bindings"),
        (
            Some(legacy_binding.to_string()),
            "cannot contain other source types",
        ),
        (
            Some(legacy_pending.to_string()),
            "cannot contain other source types",
        ),
    ];
    for (raw, diagnostic) in cases {
        if let Some(raw) = raw {
            std::fs::write(&dedicated, raw).unwrap();
        }
        let rows = mixed();
        assert_eq!(event_contents(&rows), ["legacy"]);
        let held = rows_by_kind(&rows, "source_report")
            .into_iter()
            .find(|row| row["report"]["source_path"] == source.to_str().unwrap())
            .unwrap();
        assert_eq!(held["report"]["admission_status"], "unknown_hold");
        assert!(
            held["report"]["admission_detail"]
                .as_str()
                .unwrap()
                .contains(diagnostic),
            "{held:?}"
        );
        assert!(held.get("replacement").is_none());
    }
    let pending = json!({"schemaVersion": 1, "pendingSources": [{"backend": "antigravity_cli", "sourcePath": source}]});
    std::fs::write(&dedicated, pending.to_string()).unwrap();
    assert_eq!(event_contents(&mixed()), ["legacy"]);

    std::fs::write(&dedicated, agy_binding(false, AGY_ID).to_string()).unwrap();
    let mut contents = event_contents(&mixed())
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    contents.sort();
    assert_eq!(contents, ["antigravity", "legacy"]);
    assert!(!fixture.store.exists());
}

#[cfg(windows)]
#[test]
#[ignore = "requires SVAULT_TEST_LEGACY_CLI pointing to the Windows v2026.11.11.1 release"]
fn antigravity_dedicated_files_preserve_legacy_cli_writes() {
    let fixture = Fixture::new();
    let old_cli = PathBuf::from(
        std::env::var_os("SVAULT_TEST_LEGACY_CLI").expect("set SVAULT_TEST_LEGACY_CLI"),
    );
    assert!(old_cli.is_absolute());
    let isolated_old_cli = fixture.root.join("legacy-svault.exe");
    std::fs::hard_link(&old_cli, &isolated_old_cli)
        .or_else(|_| std::fs::copy(&old_cli, &isolated_old_cli).map(|_| ()))
        .unwrap();
    let version = fixture
        .command_for(&isolated_old_cli)
        .arg("--version")
        .output()
        .unwrap();
    assert!(version.status.success());
    assert_eq!(
        String::from_utf8(version.stdout).unwrap().trim(),
        "svault 2026.11.11.1 (b9339c359acc)"
    );
    drop(fixture.seed_store());
    let agy = agy_transcript(&fixture, AGY_ID);
    std::fs::write(agy, agy_user(0, "private antigravity")).unwrap();
    let dedicated = fixture.root.join("agy-bindings.json");
    let dedicated_json = agy_binding(true, AGY_ID).to_string();
    std::fs::write(&dedicated, &dedicated_json).unwrap();
    let scan_state = fixture.state.to_str().unwrap();
    let write_args = [
        "scan-all",
        "--only",
        "jsonl",
        "--write-store",
        "--state",
        scan_state,
    ];
    let read_args = [
        "scan-all",
        "--only",
        "antigravity-cli",
        "--profile",
        "full",
        "--stateless",
    ];
    let scan = |program, args: &[&str]| {
        let mut command = fixture.command_for(program);
        command.env("SVAULT_ANTIGRAVITY_SESSION_BINDINGS", &dedicated);
        successful_rows(args, fixture.output_with(command, args))
    };
    let check_new = |status| {
        let rows = scan(fixture.root.join("svault.exe"), &read_args);
        assert_eq!(
            only_row(&rows, "source_report")["report"]["admission_status"],
            status
        );
        assert!(event_contents(&rows).is_empty());
    };
    let old_write = || scan(isolated_old_cli.clone(), &write_args);
    check_new("temporary_excluded");
    std::fs::write(&fixture.source, messages(&["first"])).unwrap();
    assert_eq!(only_row(&old_write(), "summary")["written_events"], 1);
    assert_eq!(fixture.contents("current"), ["first"]);

    std::fs::write(&dedicated, "{").unwrap();
    check_new("unknown_hold");
    std::fs::write(&fixture.source, messages(&["first", "second"])).unwrap();
    assert_eq!(only_row(&old_write(), "summary")["written_events"], 1);
    assert_eq!(fixture.contents("current"), ["first", "second"]);

    // 反对照仅改夹具的旧共享文件。旧版要确实发现新 backend 并暂停，才证明上面测到了隔离。
    let shared = fixture.root.join("empty-bindings.json");
    let original_shared = std::fs::read(&shared).unwrap();
    std::fs::write(&shared, dedicated_json).unwrap();
    let all_messages = ["first", "second", "third"];
    std::fs::write(&fixture.source, messages(&all_messages)).unwrap();
    let blocked = old_write();
    let report = only_row(&blocked, "source_report");
    assert_eq!(report["report"]["admission_status"], "unknown_hold");
    assert!(report["report"]["admission_detail"]
        .as_str()
        .unwrap()
        .contains("antigravity_cli"));
    assert_eq!(only_row(&blocked, "summary")["written_events"], 0);
    assert_eq!(fixture.contents("current"), ["first", "second"]);

    std::fs::write(shared, original_shared).unwrap();
    assert_eq!(only_row(&old_write(), "summary")["written_events"], 1);
    assert_eq!(fixture.contents("current"), all_messages);

    // 新旧扫描显式给同一 --state，专属游标仍须避开旧写者整表重序列化。
    assert_eq!(
        event_contents(&fixture.antigravity()),
        ["private antigravity"]
    );
    let antigravity_state = fixture.antigravity_cursor_path();
    let saved = std::fs::read(&antigravity_state).unwrap();
    let cursors = fixture.antigravity_cursors();
    assert!(cursors.as_object().unwrap().values().all(|cursor| {
        cursor["content_hash"]
            .as_str()
            .is_some_and(|h| h.starts_with("agv1:sha256:") && h.contains(";attr=sha256:"))
            && cursor.get("antigravity_attribution_hash").is_none()
    }));
    std::fs::write(
        &fixture.source,
        messages(&["first", "second", "third", "fourth"]),
    )
    .unwrap();
    let written = old_write();
    assert_eq!(only_row(&written, "summary")["written_events"], 1);
    assert_eq!(only_row(&written, "summary")["state_saved"], true);
    assert!(fixture
        .state()
        .as_object()
        .unwrap()
        .keys()
        .all(|key| key.starts_with("jsonl|")));
    assert_eq!(std::fs::read(&antigravity_state).unwrap(), saved);
    let steady = fixture.antigravity();
    assert!(event_contents(&steady).is_empty());
    assert!(only_row(&steady, "source_report")
        .get("replacement")
        .is_none());
    assert_eq!(
        fixture.contents("current"),
        ["first", "second", "third", "fourth"]
    );
}

#[cfg(windows)]
#[test]
fn antigravity_damaged_tails_do_not_publish_or_advance_the_stream() {
    let fixture = Fixture::new();
    let source = agy_transcript(&fixture, AGY_ID);
    let initial = agy_user(0, "first");
    std::fs::write(&source, &initial).unwrap();
    assert_eq!(event_contents(&fixture.antigravity()), ["first"]);
    let before = fixture.antigravity_cursors();
    let good_tail = agy_user(1, "second");
    let corrected = agy_user(0, "fixed") + &good_tail;
    let bad_append = format!("{initial}{good_tail}not-json\n");
    let bad_replacement = format!("{corrected}not-json\n");
    let invalid_utf8 = [corrected.as_bytes(), b"\xff\n"].concat();
    let mut invalid_content: Value =
        serde_json::from_str(&agy_step(0, "fixed", "PLANNER_RESPONSE", "MODEL", "DONE")).unwrap();
    invalid_content["content"] = json!(["not a string"]);
    let wrong_type = format!("{invalid_content}\n");
    let null_content = {
        invalid_content["content"] = Value::Null;
        format!("{invalid_content}\n")
    };
    invalid_content.as_object_mut().unwrap().remove("content");
    let missing = format!("{invalid_content}\n");
    for (damaged, pending) in [
        (corrected.trim_end().as_bytes(), good_tail.trim_end().len()),
        (bad_append.as_bytes(), 0),
        (bad_replacement.as_bytes(), 0),
        (invalid_utf8.as_slice(), 0),
        (wrong_type.as_bytes(), 0),
        (null_content.as_bytes(), 0),
        (missing.as_bytes(), 0),
    ] {
        std::fs::write(&source, damaged).unwrap();
        let held = fixture.antigravity();
        let report = only_row(&held, "source_report");
        assert!(report.get("replacement").is_none());
        assert_eq!(report["report"]["pending_tail_bytes"], pending);
        assert!(!report["report"]["warnings"].as_array().unwrap().is_empty());
        assert!(event_contents(&held).is_empty());
        assert_eq!(fixture.antigravity_cursors(), before);
        assert_eq!(std::fs::read(&source).unwrap(), damaged);
    }
    std::fs::write(&source, &corrected).unwrap();
    let recovered = fixture.antigravity();
    assert_eq!(
        only_row(&recovered, "source_report")["replacement"],
        "rollback_or_rewrite"
    );
    assert_eq!(event_contents(&recovered), ["fixed", "second"]);
    assert!(!fixture.store.exists());
}

#[cfg(windows)]
#[test]
fn antigravity_zero_bytes_and_empty_events_have_distinct_streaming_baselines() {
    let fixture = Fixture::new();
    let source = agy_transcript(&fixture, AGY_ID);
    let key = format!("antigravity_cli|local|{}", source.display());
    std::fs::write(&source, "").unwrap();
    let initial = fixture.antigravity();
    assert!(event_contents(&initial).is_empty());
    assert!(only_row(&initial, "source_report")
        .get("replacement")
        .is_none());
    let empty_state = fixture.antigravity_cursors();
    assert_eq!(empty_state[&key]["fingerprint"]["covered_len"], 0);
    assert_eq!(empty_state[&key]["safe_offset"], 0);

    let first = agy_user(0, "first");
    std::fs::write(&source, &first).unwrap();
    assert_eq!(event_contents(&fixture.antigravity()), ["first"]);
    let empty = agy_step(0, "", "PLANNER_RESPONSE", "MODEL", "DONE");
    let running = agy_step(0, "unfinished", "PLANNER_RESPONSE", "MODEL", "RUNNING");
    for candidate in [empty.as_str(), running.as_str(), ""] {
        let covered_len = candidate.len();
        std::fs::write(&source, candidate).unwrap();
        let replaced = fixture.antigravity();
        assert_eq!(
            only_row(&replaced, "source_report")["replacement"],
            "rollback_or_rewrite"
        );
        assert!(event_contents(&replaced).is_empty());
        let state = fixture.antigravity_cursors();
        assert_eq!(state[&key]["fingerprint"]["covered_len"], covered_len);
        assert_eq!(state[&key]["safe_offset"], covered_len);
        let steady = fixture.antigravity();
        assert!(only_row(&steady, "source_report")
            .get("replacement")
            .is_none());
        assert!(event_contents(&steady).is_empty());
        assert_eq!(fixture.antigravity_cursors(), state);
    }
    std::fs::write(&source, first).unwrap();
    let refilled = fixture.antigravity();
    assert_eq!(event_contents(&refilled), ["first"]);
    assert_eq!(only_row(&refilled, "event")["event"]["seq"], 0);
    assert!(only_row(&refilled, "source_report")
        .get("replacement")
        .is_none());
    assert!(!fixture.store.exists());
}

#[cfg(windows)]
#[test]
fn partial_root_registry_holds_antigravity_but_preserves_roots_for_legacy_sources() {
    let fixture = Fixture::new();
    let source = agy_transcript(&fixture, AGY_ID);
    let initial = agy_user(0, "first");
    std::fs::write(&source, &initial).unwrap();
    let (project, summary) = agy_register_workspace(&fixture, "project");
    let project_path = project.to_str().unwrap();
    assert_eq!(
        only_row(&fixture.antigravity(), "event")["event"]["project_root"],
        project_path
    );
    let before = fixture.antigravity_cursors();
    let agy_key = format!("antigravity_cli|local|{}", source.display());

    let db = rusqlite::Connection::open(&fixture.store).unwrap();
    db.execute(
        "INSERT INTO project_root_registry VALUES ('unreadable', X'FF', 'git', 1, 1)",
        [],
    )
    .unwrap();
    summary
        .execute(
            "UPDATE conversation_summaries SET workspace_uris = '[]'",
            [],
        )
        .unwrap();
    let new_transcript = initial + &agy_user(1, "tail");
    std::fs::write(&source, &new_transcript).unwrap();
    let cwd = project.join("subdir");
    std::fs::create_dir(&cwd).unwrap();
    let ordinary = json!({"role":"user", "session_id":"ordinary", "content":"legacy", "cwd": cwd.to_str().unwrap()});
    std::fs::write(&fixture.source, format!("{ordinary}\n")).unwrap();
    let mixed = fixture.run(&[
        "scan-all",
        "--only",
        "jsonl",
        "--only",
        "antigravity-cli",
        "--profile",
        "full",
        "--state",
        fixture.state.to_str().unwrap(),
    ]);
    let event = &only_row(&mixed, "event")["event"];
    assert_eq!(event["source_type"], "jsonl");
    assert_eq!(event["content"], "legacy");
    assert_eq!(event["project_root"], project_path);
    assert_eq!(event["project_root_source"], "git");
    let reports = rows_by_kind(&mixed, "source_report");
    assert_eq!(reports.len(), 2, "{mixed:?}");
    let held = reports
        .iter()
        .find(|row| row["report"]["source_path"] == source.to_str().unwrap())
        .unwrap();
    assert!(held.get("replacement").is_none());
    assert!(held["report"]["warnings"][0]
        .as_str()
        .unwrap()
        .contains("project roots unavailable"));
    let status = only_row(&mixed, "summary");
    assert_eq!(status["roots_empty"], false);
    assert!(status["roots_unavailable"].is_string());
    assert_eq!(status["discovery_unreachable"], json!(["wsl:*"]));
    assert_eq!(fixture.antigravity_cursors()[&agy_key], before[&agy_key]);

    db.execute(
        "DELETE FROM project_root_registry WHERE root_key = 'unreadable'",
        [],
    )
    .unwrap();
    let recovered = fixture.antigravity();
    assert_eq!(
        only_row(&recovered, "source_report")["replacement"],
        "reparse_required"
    );
    assert_eq!(event_contents(&recovered), ["first", "tail"]);
    for row in rows_by_kind(&recovered, "event") {
        assert!(row["event"]["project_root"].is_null());
    }
    assert_eq!(std::fs::read(&source).unwrap(), new_transcript.as_bytes());
}

#[cfg(windows)]
#[test]
fn antigravity_discovery_continues_past_a_malformed_sibling() {
    let fixture = Fixture::new();
    let malformed = agy_transcript(&fixture, AGY_ID);
    let healthy = agy_transcript(&fixture, AGY_OTHER_ID);
    let initial = agy_user(0, "healthy");
    std::fs::write(&healthy, &initial).unwrap();
    std::fs::write(&malformed, agy_user(0, "sibling")).unwrap();
    let complete = fixture.antigravity();
    assert_eq!(rows_by_kind(&complete, "event").len(), 2);
    assert!(only_row(&complete, "summary")
        .get("discovery_unreachable")
        .is_none());
    let before = fixture.antigravity_cursors();
    let malformed_key = format!("antigravity_cli|local|{}", malformed.display());
    assert!(before.get(&malformed_key).is_some());

    std::fs::remove_file(&malformed).unwrap();
    std::fs::create_dir(&malformed).unwrap();
    std::fs::write(&healthy, initial + &agy_user(1, "tail")).unwrap();
    let partial = fixture.antigravity();
    let event = &only_row(&partial, "event")["event"];
    assert_eq!(event["source_session_id"], AGY_OTHER_ID);
    assert_eq!(event["content"], "tail");
    assert_eq!(
        only_row(&partial, "summary")["discovery_unreachable"],
        json!(["local"])
    );
    assert_eq!(
        fixture.antigravity_cursors()[&malformed_key],
        before[&malformed_key]
    );
    assert!(event_contents(&fixture.antigravity()).is_empty());

    let args = ["discover"];
    let output = fixture.command().args(args).output().unwrap();
    let discovered = successful_rows(&args, output);
    let source = only_row(&discovered, "source");
    assert_eq!(source["source_type"], "antigravity_cli");
    assert_eq!(source["path"], healthy.to_str().unwrap());
    assert_eq!(
        only_row(&discovered, "summary")["discovery_unreachable"],
        json!(["local", "wsl:*"])
    );
    assert!(!fixture.store.exists());
}

#[test]
fn scan_preserves_the_legacy_empty_roots_warning_when_all_rows_are_unreadable() {
    let fixture = Fixture::new();
    std::fs::write(&fixture.source, messages(&["first"])).unwrap();
    let empty = fixture.scan();
    let empty = only_row(&empty, "summary");
    assert_eq!(empty["roots_empty"], true);
    assert!(empty.get("roots_unavailable").is_none());

    let conn = rusqlite::Connection::open(&fixture.store).unwrap();
    conn.execute(
        "INSERT INTO project_root_registry VALUES ('bad-row', X'FF', 'git', 1, 1)",
        [],
    )
    .unwrap();
    std::fs::write(&fixture.source, messages(&["first", "second"])).unwrap();
    let failed = fixture.scan();
    let failed = only_row(&failed, "summary");
    assert_eq!(failed["written_events"], 1);
    assert_eq!(failed["roots_empty"], true, "{failed}");
    assert!(failed["roots_unavailable"].is_string());
    assert_eq!(fixture.contents("current"), ["first", "second"]);
}

fn successful_rows(args: &[&str], output: std::process::Output) -> Vec<Value> {
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

fn rows_by_kind<'a>(rows: &'a [Value], kind: &str) -> Vec<&'a Value> {
    rows.iter().filter(|row| row["kind"] == kind).collect()
}

fn only_row<'a>(rows: &'a [Value], kind: &str) -> &'a Value {
    let selected = rows_by_kind(rows, kind);
    assert_eq!(selected.len(), 1, "expected one {kind}: {rows:?}");
    selected[0]
}

#[cfg(windows)]
fn event_contents(rows: &[Value]) -> Vec<&str> {
    rows_by_kind(rows, "event")
        .into_iter()
        .map(|row| row["event"]["content"].as_str().unwrap())
        .collect()
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
    let result = fixture
        .command()
        .env("SVAULT_JSONL_DIR", "")
        .env("SVAULT_DSH_SESSIONS_DIR", &sessions)
        .env_remove("SVAULT_DSH_NODE")
        .env_remove("SVAULT_DSH_PACKAGE")
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
