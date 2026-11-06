//! 固定仓内脚本通过显式 Node / DSH 安装做原生 admission；正文只经 stdin。
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::json;

use super::{limits, Blocked, PhysicalSnapshot};
use crate::deadline::Deadline;

const POLL: Duration = Duration::from_millis(10);

#[derive(Debug)]
pub struct NativeRuntime {
    node: PathBuf,
    package: PathBuf,
}

#[derive(Debug)]
pub struct AdmittedSnapshot {
    physical: PhysicalSnapshot,
    evidence: Evidence,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Evidence {
    jsonl_sha256: String,
    header_sha256: String,
    native_events: usize,
    inherited_event_count: usize,
    lifecycle_sha256: String,
    native_events_sha256: String,
    dsh_version: String,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Reply {
    NativeAdmitted(Evidence),
    Rejected { reason: String },
    Unavailable { reason: String },
    BudgetExceeded { reason: String },
}

impl NativeRuntime {
    /// 运行时来自显式、可信的本机工具配置；不发现安装、不下载依赖。
    pub fn new(node: impl Into<PathBuf>, package: impl Into<PathBuf>) -> Result<Self, Blocked> {
        let runtime = Self {
            node: node.into(),
            package: package.into(),
        };
        if !runtime.node.is_absolute() || !runtime.package.is_absolute() {
            return Err(Blocked::Unavailable("native_runtime_paths_not_absolute"));
        }
        Ok(runtime)
    }

    /// 消费实际物理核验结果，在其私有 JSONL 上做原生 admission；不重开来源。
    pub fn admit(
        &self,
        snapshot: PhysicalSnapshot,
        deadline: Deadline,
    ) -> Result<AdmittedSnapshot, Blocked> {
        let output = self.run(&snapshot.jsonl.0, deadline)?;
        let evidence = bind_reply(output.status, &output.stdout, &snapshot)?;
        Ok(AdmittedSnapshot {
            physical: snapshot,
            evidence,
        })
    }

    fn run(&self, input: &[u8], deadline: Deadline) -> Result<NodeOutput, Blocked> {
        let limits = limits();
        let budget = deadline
            .budget_for(Duration::from_millis(limits.audit_timeout_ms))
            .ok_or(Blocked::Unavailable("native_deadline_exhausted"))?;
        let end = Instant::now() + budget;
        // 安装约束与语义实现均嵌入，不把编译机路径写进运行时，也不信任外部 JSON 凭据。
        let script = format!(
            "{}\n{}\nawait runNativeAdmission(loadDshInstallation, {}, process.argv.slice(1));",
            include_str!("../../scripts/dsh-installed.mjs"),
            include_str!("../../scripts/dsh-native-admission.mjs"),
            include_str!("../../scripts/dsh-snapshot-limits.json")
        );
        let mut command = Command::new(&self.node);
        command
            .args([
                "--input-type=module",
                "--eval",
                &script,
                "--",
                "--dsh-package",
            ])
            .arg(&self.package)
            .env_remove("NODE_OPTIONS")
            .env_remove("NODE_PATH")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        configure(&mut command);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        if Instant::now() >= end {
            return Err(Blocked::Unavailable("native_deadline_exhausted"));
        }
        let mut child = Guard(
            command
                .spawn()
                .map_err(|_| Blocked::Unavailable("native_spawn_failed"))?,
        );
        let stdin = child
            .0
            .stdin
            .take()
            .ok_or(Blocked::Unavailable("native_stdin_missing"))?;
        let stdout = child
            .0
            .stdout
            .take()
            .ok_or(Blocked::Unavailable("native_stdout_missing"))?;
        let stderr = child
            .0
            .stderr
            .take()
            .ok_or(Blocked::Unavailable("native_stderr_missing"))?;
        let (sender, receiver) = mpsc::channel();
        let writer = sender.clone();
        let bytes = input.to_vec();
        std::thread::spawn(move || {
            let mut stdin = stdin;
            let result = stdin.write_all(&bytes);
            drop(stdin);
            let _ = writer.send(Pipe::Input(result.is_ok()));
        });
        let reader = sender.clone();
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let result = stdout
                .take(limits.audit_output_bytes + 1)
                .read_to_end(&mut bytes);
            let _ = reader.send(Pipe::Output(result.map(|_| bytes)));
        });
        std::thread::spawn(move || {
            let result = std::io::copy(
                &mut stderr.take(limits.audit_output_bytes + 1),
                &mut std::io::sink(),
            );
            let _ = sender.send(Pipe::Error(result));
        });
        let mut status = None;
        let mut stdout = None;
        let mut error_bytes = 0;
        let mut finished = 0;
        loop {
            let failure = match child.0.try_wait() {
                Ok(found) => {
                    status = status.or(found);
                    None
                }
                Err(_) => Some(Blocked::Unavailable("native_wait_failed")),
            };
            if let Some(failure) = failure {
                return stop_with(&mut child.0, &receiver, finished, failure);
            }
            if finished == 3 && status.is_some() {
                break;
            }
            let Some(left) = end.checked_duration_since(Instant::now()) else {
                return stop_with(
                    &mut child.0,
                    &receiver,
                    finished,
                    Blocked::Unavailable("native_timeout"),
                );
            };
            let failure = match receiver.recv_timeout(left.min(POLL)) {
                Ok(message) => {
                    finished += 1;
                    match message {
                        Pipe::Input(true) => None,
                        Pipe::Input(false) => Some(Blocked::Unavailable("native_input_failed")),
                        Pipe::Output(Ok(bytes))
                            if bytes.len() as u64 <= limits.audit_output_bytes =>
                        {
                            stdout = Some(bytes);
                            None
                        }
                        Pipe::Output(Ok(_)) => {
                            Some(Blocked::BudgetExceeded("native_output_budget"))
                        }
                        Pipe::Error(Ok(size)) if size <= limits.audit_output_bytes => {
                            error_bytes = size;
                            None
                        }
                        Pipe::Error(Ok(_)) => Some(Blocked::BudgetExceeded("native_output_budget")),
                        Pipe::Output(Err(_)) | Pipe::Error(Err(_)) => {
                            Some(Blocked::Unavailable("native_pipe_failed"))
                        }
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) if finished == 3 => {
                    std::thread::sleep(left.min(POLL));
                    None
                }
                Err(_) => Some(Blocked::Unavailable("native_pipe_disconnected")),
            };
            if let Some(failure) = failure {
                return stop_with(&mut child.0, &receiver, finished, failure);
            }
        }
        let stdout = stdout.ok_or(Blocked::Unavailable("native_response_missing"))?;
        if stdout.len() as u64 + error_bytes > limits.audit_output_bytes {
            return Err(Blocked::BudgetExceeded("native_output_budget"));
        }
        Ok(NodeOutput {
            status: status.expect("observed exit status"),
            stdout,
        })
    }
}

impl AdmittedSnapshot {
    /// 原生接受仍不证明来源稳定、归一化位置或生产推进。
    pub fn report(&self, include_positions: bool) -> serde_json::Value {
        let mut report = self.physical.report(include_positions);
        report["kind"] = json!("native_admitted_snapshot");
        report["semantic_admission_validated"] = json!(true);
        report["blockers"] = json!(["source_observation_required", "normalization_required"]);
        report["native_admission"] = json!({
            "inherited_event_count": self.evidence.inherited_event_count,
            "lifecycle_sha256": self.evidence.lifecycle_sha256,
            "native_events_sha256": self.evidence.native_events_sha256,
            "dsh_version": self.evidence.dsh_version,
        });
        report
    }
}

fn bind_reply(
    status: ExitStatus,
    bytes: &[u8],
    snapshot: &PhysicalSnapshot,
) -> Result<Evidence, Blocked> {
    let reply = serde_json::from_slice(bytes)
        .map_err(|_| Blocked::Unavailable("native_response_invalid"))?;
    match reply {
        Reply::NativeAdmitted(evidence) if status.success() => {
            if evidence.jsonl_sha256 != snapshot.jsonl_hash
                || evidence.header_sha256 != snapshot.header_hash
                || evidence.native_events != snapshot.positions.len()
                || evidence.inherited_event_count > evidence.native_events
                || !digest_shape(&evidence.lifecycle_sha256)
                || !digest_shape(&evidence.native_events_sha256)
                || evidence.dsh_version.is_empty()
            {
                return Err(Blocked::Unverified("native_binding_mismatch"));
            }
            Ok(evidence)
        }
        Reply::Rejected { reason } if status.code() == Some(2) && !reason.is_empty() => {
            Err(Blocked::Rejected("native_semantic_admission"))
        }
        Reply::Unavailable { reason } if status.code() == Some(1) && !reason.is_empty() => {
            Err(Blocked::Unavailable("native_runtime_unavailable"))
        }
        Reply::BudgetExceeded { reason } if status.code() == Some(2) && !reason.is_empty() => {
            Err(Blocked::BudgetExceeded("native_input_budget"))
        }
        _ => Err(Blocked::Unverified("native_exit_response_mismatch")),
    }
}

fn digest_shape(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

struct NodeOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
}
enum Pipe {
    Input(bool),
    Output(std::io::Result<Vec<u8>>),
    Error(std::io::Result<u64>),
}
struct Guard(Child);
impl Drop for Guard {
    fn drop(&mut self) {
        let _ = self.0.kill();
    }
}

fn configure(command: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    let _ = command;
}

fn stop_with<T>(
    child: &mut Child,
    receiver: &Receiver<Pipe>,
    mut finished: usize,
    failure: Blocked,
) -> Result<T, Blocked> {
    let id = child.id().to_string();
    #[cfg(windows)]
    let mut command = {
        let mut c = Command::new("taskkill");
        c.args(["/PID", &id, "/T", "/F"]);
        c
    };
    #[cfg(unix)]
    let mut command = {
        let mut c = Command::new("kill");
        c.args(["-KILL", "--", &format!("-{id}")]);
        c
    };
    #[cfg(not(any(windows, unix)))]
    let mut command = Command::new("unsupported-process-tree-termination");
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    configure(&mut command);
    let mut killer = command.spawn().ok().map(Guard);
    if killer.is_none() {
        let _ = child.kill();
    }
    let end = Instant::now() + Duration::from_millis(limits().audit_cleanup_timeout_ms);
    loop {
        while receiver.try_recv().is_ok() {
            finished += 1;
        }
        let child_closed = matches!(child.try_wait(), Ok(Some(_)));
        let killer_status = killer.as_mut().and_then(|k| k.0.try_wait().ok().flatten());
        let killer_closed = killer.is_none() || killer_status.is_some();
        if child_closed && killer_closed && finished == 3 {
            return Err(if killer_status.is_some_and(|status| status.success()) {
                failure
            } else {
                Blocked::Unavailable("native_cleanup_unconfirmed")
            });
        }
        if Instant::now() >= end {
            let _ = child.kill();
            return Err(Blocked::Unavailable("native_cleanup_unconfirmed"));
        }
        std::thread::sleep(POLL);
    }
}

#[cfg(all(test, any(windows, unix)))]
mod tests {
    use super::*;
    use crate::dsh_snapshot::{hash, verify, Compression};

    fn status(code: u32) -> ExitStatus {
        #[cfg(windows)]
        {
            use std::os::windows::process::ExitStatusExt;
            ExitStatus::from_raw(code)
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            ExitStatus::from_raw((code as i32) << 8)
        }
    }

    fn snapshot(text: &str) -> PhysicalSnapshot {
        let bytes = format!("{{\"type\":\"session\",\"version\":4,\"id\":\"s0\"}}\n{{\"seq\":0,\"type\":\"x\",\"text\":\"{text}\"}}\n");
        verify(bytes.as_bytes(), Compression::None).unwrap()
    }

    fn response(snapshot: &PhysicalSnapshot) -> serde_json::Value {
        json!({
            "kind":"native_admitted", "jsonl_sha256":snapshot.jsonl_hash,
            "header_sha256":snapshot.header_hash, "native_events":snapshot.positions.len(),
            "inherited_event_count":0, "lifecycle_sha256":hash(b"lifecycle"),
            "native_events_sha256":hash(b"events"), "dsh_version":"fixture",
        })
    }

    #[test]
    fn native_reply_must_bind_snapshot_header_and_event_coverage() {
        let snapshot = snapshot("before");
        let baseline = response(&snapshot);
        assert!(bind_reply(
            status(0),
            &serde_json::to_vec(&baseline).unwrap(),
            &snapshot
        )
        .is_ok());
        for (field, wrong) in [
            ("jsonl_sha256", json!(hash(b"other"))),
            ("header_sha256", json!(hash(b"other"))),
            ("native_events", json!(2)),
            ("inherited_event_count", json!(2)),
            ("lifecycle_sha256", json!("short")),
            ("native_events_sha256", json!("short")),
            ("dsh_version", json!("")),
        ] {
            let mut changed = baseline.clone();
            changed[field] = wrong;
            assert_eq!(
                bind_reply(status(0), &serde_json::to_vec(&changed).unwrap(), &snapshot)
                    .unwrap_err(),
                Blocked::Unverified("native_binding_mismatch"),
                "{field}"
            );
        }
        let old = serde_json::to_vec(&baseline).unwrap();
        assert_eq!(
            bind_reply(status(0), &old, &self::snapshot("after")).unwrap_err(),
            Blocked::Unverified("native_binding_mismatch")
        );
        assert_eq!(
            bind_reply(status(1), &old, &snapshot).unwrap_err(),
            Blocked::Unverified("native_exit_response_mismatch")
        );
    }

    #[test]
    fn native_errors_and_invalid_output_never_construct_success() {
        let snapshot = snapshot("内容不得出现在 Debug 中");
        assert!(!format!("{snapshot:?}").contains("内容不得"));
        assert_eq!(
            bind_reply(status(0), b"{}", &snapshot).unwrap_err(),
            Blocked::Unavailable("native_response_invalid")
        );
        let rejected = br#"{"kind":"rejected","reason":"semantic_admission"}"#;
        assert_eq!(
            bind_reply(status(2), rejected, &snapshot).unwrap_err(),
            Blocked::Rejected("native_semantic_admission")
        );
        assert_eq!(
            bind_reply(status(0), rejected, &snapshot).unwrap_err(),
            Blocked::Unverified("native_exit_response_mismatch")
        );
    }

    #[test]
    fn expired_budget_prevents_native_spawn() {
        let root = std::env::current_dir().unwrap();
        let runtime = NativeRuntime::new(
            root.join("target/unused-node"),
            root.join("target/unused-package.json"),
        )
        .unwrap();
        assert_eq!(
            runtime
                .admit(snapshot("before"), Deadline::after(Duration::ZERO))
                .unwrap_err(),
            Blocked::Unavailable("native_deadline_exhausted")
        );
    }
}
