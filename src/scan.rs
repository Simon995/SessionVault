//! 单来源增量扫描（§8）。按 `source_mode` 分派；append_log 是骨架唯一实装路径。
//!
//! append_log 流程：stat 文件 → 比对 `(mtime, size)` 检测回退/截断 → 从 `safe_offset`
//! 读尾部 → `split_complete_jsonl` 切出完整行（半行留下轮）→ 解析 → 推进游标。
//! 移植自 QuotaBar `session_index.rs::split_complete_jsonl`（纯函数，带单测）。

use std::io::{Read, Seek, SeekFrom};

use crate::cursor::{Cursor, CursorKind, ScanResult, ScanStatus};
use crate::discover::SourceRef;
use crate::logging::tag;
use crate::parser::{parse_lines, ParseCtx};
use crate::pathnorm::HostPlatform;
use crate::rawevent::{SourceLocation, SourceMode};
use crate::report::SourceReport;
use crate::Profile;

/// scan 主入口：按形态分派。append_log 的字节来源（本地 `File` vs WSL `wsl.exe`）
/// 经 [`ByteSource`] 抽象，游标/回退/坏行冻结逻辑两者**共用同一份**。
pub fn scan_source(source: &SourceRef, cursor_in: Option<Cursor>, profile: Profile) -> ScanResult {
    match source.source_mode {
        SourceMode::AppendLog => match &source.source_location {
            SourceLocation::Local => {
                scan_append_log(&LocalSource { path: &source.path }, source, cursor_in, profile)
            }
            SourceLocation::Wsl(distro) => {
                let abs = source.path.to_string_lossy().into_owned();
                scan_append_log(&WslSource { distro, abs: &abs }, source, cursor_in, profile)
            }
        },
        // 其余形态骨架未实装：返回 NoCursor，事件空。
        SourceMode::SnapshotFile | SourceMode::SqliteStore | SourceMode::OpaqueFamily => {
            let mut report = SourceReport {
                source_path: source.path.display().to_string(),
                source_mode: Some(source.source_mode),
                cursor_kind: Some(CursorKind::NoCursor),
                ..Default::default()
            };
            report
                .warnings
                .push(format!("source_mode {:?} not implemented", source.source_mode));
            ScanResult {
                status: ScanStatus::Ok,
                events: Vec::new(),
                cursor_out: Cursor {
                    kind: CursorKind::NoCursor,
                    ..Cursor::new_byte_offset()
                },
                report,
            }
        }
    }
}

/// append_log 的字节来源抽象：把「stat 取 (size,mtime)」与「读 `[start,end)` 字节」
/// 从扫描逻辑里剥出来，本地（`File`/`Seek`）与 WSL（`wsl.exe`）各实现一份，
/// 游标/回退/坏行冻结逻辑则在 [`scan_append_log`] 里**共用同一份**。
trait ByteSource {
    /// `(size, mtime_secs)`；失败返回人类可读错误串。
    fn stat(&self) -> Result<(u64, Option<i64>), String>;
    /// 读字节区间 `[start, end)`。
    fn read_range(&self, start: u64, end: u64) -> Result<Vec<u8>, String>;
}

/// 本机文件字节来源。
struct LocalSource<'a> {
    path: &'a std::path::Path,
}

impl ByteSource for LocalSource<'_> {
    fn stat(&self) -> Result<(u64, Option<i64>), String> {
        let meta = std::fs::metadata(self.path).map_err(|e| e.to_string())?;
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64);
        Ok((meta.len(), mtime))
    }

    fn read_range(&self, start: u64, end: u64) -> Result<Vec<u8>, String> {
        read_range(self.path, start, end).map_err(|e| e.to_string())
    }
}

/// WSL 发行版内文件字节来源（经 `wsl.exe`）。`abs` 是发行版内 Linux 绝对路径。
struct WslSource<'a> {
    distro: &'a str,
    abs: &'a str,
}

impl ByteSource for WslSource<'_> {
    fn stat(&self) -> Result<(u64, Option<i64>), String> {
        match crate::wsl::stat(self.distro, self.abs)? {
            Some((size, mtime)) => Ok((size, Some(mtime))),
            None => Err(format!("wsl file missing: {}:{}", self.distro, self.abs)),
        }
    }

    fn read_range(&self, start: u64, end: u64) -> Result<Vec<u8>, String> {
        crate::wsl::read_range(self.distro, self.abs, start, end)
    }
}

/// 追加型日志增量扫描（字节来源经 [`ByteSource`] 抽象，本地/WSL 共用此函数）。
fn scan_append_log<S: ByteSource>(
    src: &S,
    source: &SourceRef,
    cursor_in: Option<Cursor>,
    profile: Profile,
) -> ScanResult {
    let mut report = SourceReport {
        source_path: source.path.display().to_string(),
        source_mode: Some(SourceMode::AppendLog),
        cursor_kind: Some(CursorKind::ByteOffset),
        ..Default::default()
    };

    let mut cursor = cursor_in.unwrap_or_else(Cursor::new_byte_offset);

    let (size, mtime) = match src.stat() {
        Ok(v) => v,
        Err(e) => {
            report.warnings.push(format!("stat failed: {e}"));
            return ScanResult {
                status: ScanStatus::Error,
                events: Vec::new(),
                cursor_out: cursor,
                report,
            };
        }
    };

    // 回退/截断检测：size 变小，或 mtime 倒退 → 从头重读。
    let rollback = size < cursor.safe_offset
        || matches!((mtime, cursor.mtime), (Some(now), Some(prev)) if now < prev);
    let mut start = cursor.safe_offset;
    if rollback {
        log::warn!(
            target: tag::CURSOR,
            "rollback detected: path={} prev_offset={} new_size={}",
            report.source_path, cursor.safe_offset, size
        );
        report.rollback_detected = true;
        start = 0;
        cursor = Cursor::new_byte_offset();
    }

    if start >= size {
        // 无新增。
        cursor.size = size;
        cursor.mtime = mtime;
        return ScanResult {
            status: ScanStatus::Ok,
            events: Vec::new(),
            cursor_out: cursor,
            report,
        };
    }

    // 读 [start, size) 尾部。
    let tail = match src.read_range(start, size) {
        Ok(b) => b,
        Err(e) => {
            report.warnings.push(format!("read failed: {e}"));
            return ScanResult {
                status: ScanStatus::Error,
                events: Vec::new(),
                cursor_out: cursor,
                report,
            };
        }
    };
    report.bytes_read = tail.len() as u64;

    let text = String::from_utf8_lossy(&tail);
    let (complete, pending) = split_complete_jsonl(&text);
    report.pending_tail_bytes = pending as u64;

    let lines: Vec<&str> = complete.lines().collect();
    // default_distro：WSL 来源用其自身发行版（权威），把 distro 未知的裸 Linux cwd 打成
    // 精确 wsl:<distro>（见 parser）；本地来源为 None（裸 Linux cwd 记在 local transcript
    // 下的边角由 host/CLI 决定是否注入 wsl::default_distro）。
    let default_distro = match &source.source_location {
        SourceLocation::Wsl(distro) => Some(distro.clone()),
        SourceLocation::Local => None,
    };
    let ctx = ParseCtx {
        source_type: source.source_type,
        source_location: source.source_location.clone(),
        source_path: report.source_path.clone(),
        profile,
        host: HostPlatform::current(),
        default_distro,
    };
    let base_seq = cursor.next_seq;
    let codex_state_before = cursor.codex_state.clone();
    let parsed = parse_lines(&ctx, &lines, base_seq, cursor.codex_state.take());

    cursor.kind = CursorKind::ByteOffset;
    cursor.size = size;
    cursor.mtime = mtime;
    report.items_examined = lines.len() as u64;
    report.items_skipped = parsed.skipped;
    report.warnings.extend(parsed.warnings);

    // P1：坏 JSON 行 → **冻结整批尾**（对齐 QuotaBar 实证，见 rawevent-reconciliation §2 / §8 规则 2）。
    // append-only 的完整行不可变，坏了就永远坏；保守契约选「保持起点 offset + status=error +
    // 本轮不发事件 + 下轮重读整段尾」，宁可重读也不静默跳过/错解。
    // （已知取舍：永久损坏的完整行会让该来源停在原地——这是从 QuotaBar 继承的行为，
    //   将来可在游标加 retry 计数做「毒行」跳过，属后续阶段。）
    if parsed.skipped > 0 {
        cursor.safe_offset = start; // 不前进
        cursor.codex_state = codex_state_before; // 不吃进坏批的状态推进
        report.events_emitted = 0;
        log::warn!(
            target: tag::SCAN,
            "append_log batch frozen (bad json): path={} skipped={} kept_offset={}",
            report.source_path, parsed.skipped, start
        );
        return ScanResult {
            status: ScanStatus::Error,
            events: Vec::new(),
            cursor_out: cursor,
            report,
        };
    }

    // 全部好行：推进 safe_offset 到「完整行」边界（size - pending），半行留下轮。
    cursor.safe_offset = size - pending as u64;
    cursor.next_seq = base_seq + parsed.events.len() as u64;
    cursor.codex_state = parsed.codex_state;
    report.events_emitted = parsed.events.len() as u64;

    let status = if pending > 0 {
        ScanStatus::Partial
    } else {
        ScanStatus::Ok
    };

    log::info!(
        target: tag::SCAN,
        "append_log done: path={} events={} examined={} bytes={} pending={} rollback={}",
        report.source_path, report.events_emitted, report.items_examined, report.bytes_read, pending, report.rollback_detected
    );

    ScanResult {
        status,
        events: parsed.events,
        cursor_out: cursor,
        report,
    }
}

/// 读文件 `[start, end)` 字节区间。
fn read_range(path: &std::path::Path, start: u64, end: u64) -> std::io::Result<Vec<u8>> {
    let mut f = std::fs::File::open(path)?;
    f.seek(SeekFrom::Start(start))?;
    let len = (end - start) as usize;
    let mut buf = vec![0u8; len];
    f.read_exact(&mut buf)?;
    Ok(buf)
}

/// 把一段文本切成「完整行部分」+「尾部半行字节数」。
///
/// 纯函数，移植自 QuotaBar：以最后一个 `\n` 为界，之后的不完整行不消费、
/// 其字节数作为 pending 留待下一轮（保证 `safe_offset` 永远落在完整行边界）。
/// 返回 `(完整行文本含尾随\n, pending_bytes)`。
pub fn split_complete_jsonl(text: &str) -> (&str, usize) {
    match text.rfind('\n') {
        Some(idx) => {
            let boundary = idx + 1; // 含换行
            let complete = &text[..boundary];
            let pending = text.len() - boundary;
            (complete, pending)
        }
        None => ("", text.len()),
    }
}

#[cfg(test)]
mod tests {
    use super::{scan_source, split_complete_jsonl};
    use crate::cursor::ScanStatus;
    use crate::discover::SourceRef;
    use crate::rawevent::{EventType, SourceLocation, SourceMode, SourceType};
    use crate::Profile;
    use std::io::Write;
    use std::path::{Path, PathBuf};

    /// 写一个唯一的临时 jsonl 文件，返回其 SourceRef（用完即弃，测试后删）。
    fn temp_source(name: &str, body: &str) -> (PathBuf, SourceRef) {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("svault-test-{name}-{nanos}.jsonl"));
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        let src = SourceRef {
            source_type: SourceType::ClaudeCode,
            source_location: SourceLocation::Local,
            source_mode: SourceMode::AppendLog,
            path: path.clone(),
        };
        (path, src)
    }

    /// 追加写入已存在文件（模拟会话继续写）。
    fn append(path: &Path, data: &str) {
        let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        f.write_all(data.as_bytes()).unwrap();
    }

    /// 一条 Claude user 行（产 1 个 message 事件，正文=text）。
    fn claude_line(session: &str, text: &str) -> String {
        serde_json::json!({
            "type": "user",
            "sessionId": session,
            "message": {"role": "user", "content": text}
        })
        .to_string()
    }

    fn codex_meta(id: &str) -> String {
        serde_json::json!({"type": "session_meta", "payload": {"id": id}}).to_string()
    }

    /// 一条 Codex 累计 token 行（total_token_usage 三段）。
    fn codex_token(input: u64, cached: u64, output: u64) -> String {
        serde_json::json!({
            "type": "event_msg",
            "timestamp": "2026-06-01T10:00:00Z",
            "payload": {"type": "token_count", "info": {
                "total_token_usage": {
                    "input_tokens": input, "cached_input_tokens": cached, "output_tokens": output
                }
            }}
        })
        .to_string()
    }

    fn temp_source_codex(name: &str, body: &str) -> (PathBuf, SourceRef) {
        let (path, mut src) = temp_source(name, body);
        src.source_type = SourceType::Codex;
        (path, src)
    }

    #[test]
    fn bad_json_freezes_batch_and_keeps_offset() {
        // 两条好行 + 一条坏 JSON（均完整行）→ 整批冻结：offset 不前进、status=error、无事件。
        let (path, src) = temp_source(
            "bad",
            "{\"a\":1}\n{\"b\":2}\nnot-json-here\n",
        );
        let res = scan_source(&src, None, Profile::Metadata);
        assert_eq!(res.status, ScanStatus::Error);
        assert_eq!(res.cursor_out.safe_offset, 0, "坏行应保持起点 offset");
        assert!(res.events.is_empty());
        assert_eq!(res.report.items_skipped, 1);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn all_good_lines_advance_offset() {
        // 全好行 → status=ok、offset 推进到完整行边界、无跳过。
        let body = "{\"a\":1}\n{\"b\":2}\n";
        let (path, src) = temp_source("good", body);
        let res = scan_source(&src, None, Profile::Metadata);
        assert_eq!(res.status, ScanStatus::Ok);
        assert_eq!(res.cursor_out.safe_offset, body.len() as u64);
        assert_eq!(res.report.items_skipped, 0);
        assert_eq!(res.report.items_examined, 2);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn incremental_append_no_dup_no_miss() {
        // 两行 → 扫；追加一行 → 带游标续扫：只出新行、seq 续接、不重不漏。
        let (path, src) = temp_source(
            "incr",
            &format!("{}\n{}\n", claude_line("s", "alpha"), claude_line("s", "beta")),
        );
        let r1 = scan_source(&src, None, Profile::Full);
        assert_eq!(r1.status, ScanStatus::Ok);
        let n1 = r1.events.len();
        assert_eq!(n1, 2, "两条 user 行 → 两个 message 事件");
        let off1 = r1.cursor_out.safe_offset;

        append(&path, &format!("{}\n", claude_line("s", "gamma")));
        let r2 = scan_source(&src, Some(r1.cursor_out), Profile::Full);
        assert_eq!(r2.status, ScanStatus::Ok);
        assert_eq!(r2.events.len(), 1, "只出新增那一行");
        assert_eq!(r2.events[0].seq, n1 as u64, "seq 跨批续接（不重不漏）");
        assert_eq!(r2.events[0].content.as_deref(), Some("gamma"));
        assert!(
            r2.events.iter().all(|e| e.content.as_deref() != Some("alpha")),
            "旧行不被重发"
        );
        assert!(r2.cursor_out.safe_offset > off1, "offset 前进");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn pending_half_line_completed_next_scan() {
        // 文件 = 完整行 + 下一行前半（无换行）→ 半行 pending、不解析；
        // 追加后半 + 换行 → 续扫补全该行。
        let l1 = claude_line("s", "first");
        let l2 = claude_line("s", "second");
        let cut = l2.len() / 2;
        let (path, src) = temp_source("pending", &format!("{l1}\n{}", &l2[..cut]));
        let r1 = scan_source(&src, None, Profile::Full);
        assert_eq!(r1.status, ScanStatus::Partial);
        assert!(r1.report.pending_tail_bytes > 0, "半行应 pending");
        assert_eq!(r1.events.len(), 1);
        assert_eq!(r1.events[0].content.as_deref(), Some("first"));

        append(&path, &format!("{}\n", &l2[cut..]));
        let r2 = scan_source(&src, Some(r1.cursor_out), Profile::Full);
        assert_eq!(r2.report.pending_tail_bytes, 0);
        assert_eq!(r2.events.len(), 1);
        assert_eq!(r2.events[0].content.as_deref(), Some("second"));
        assert_eq!(r2.events[0].seq, 1, "seq 续接首批");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn rescan_unchanged_emits_nothing() {
        let (path, src) = temp_source("nochange", &format!("{}\n", claude_line("s", "x")));
        let r1 = scan_source(&src, None, Profile::Full);
        let off = r1.cursor_out.safe_offset;
        let r2 = scan_source(&src, Some(r1.cursor_out), Profile::Full);
        assert_eq!(r2.status, ScanStatus::Ok);
        assert!(r2.events.is_empty(), "未变文件不重发事件");
        assert_eq!(r2.cursor_out.safe_offset, off);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn truncation_triggers_rollback_and_reread() {
        let (path, src) = temp_source(
            "trunc",
            &format!("{}\n{}\n", claude_line("s", "one"), claude_line("s", "two")),
        );
        let r1 = scan_source(&src, None, Profile::Full);
        assert!(r1.cursor_out.safe_offset > 0);
        assert!(r1.cursor_out.next_seq > 0);

        // 重写为更短内容（截断/重写）→ size 回退。
        std::fs::write(&path, format!("{}\n", claude_line("s", "fresh"))).unwrap();
        let r2 = scan_source(&src, Some(r1.cursor_out), Profile::Full);
        assert!(r2.report.rollback_detected, "size 变小应触发回退");
        assert_eq!(r2.events.len(), 1);
        assert_eq!(r2.events[0].seq, 0, "回退后 seq 归零重读");
        assert_eq!(r2.events[0].content.as_deref(), Some("fresh"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn codex_cumulative_token_persists_across_scans() {
        // 命门：Codex 累计 token 的 previous_total 必须跨增量批次延续。
        let (path, src) =
            temp_source_codex("codexincr", &format!("{}\n{}\n", codex_meta("cdx"), codex_token(100, 20, 50)));
        let r1 = scan_source(&src, None, Profile::Full);
        let u1: Vec<_> = r1
            .events
            .iter()
            .filter(|e| e.event_type == EventType::Usage)
            .collect();
        assert_eq!(u1.len(), 1);
        assert_eq!(u1[0].usage.unwrap().input, 80); // 100 - cached(20)
        assert!(r1.cursor_out.codex_state.is_some(), "游标应携带 Codex 状态");

        // 追加第二条累计 token（仅这一行进第二批，session_meta 不重复）。
        append(&path, &format!("{}\n", codex_token(150, 30, 80)));
        let r2 = scan_source(&src, Some(r1.cursor_out), Profile::Full);
        let u2: Vec<_> = r2
            .events
            .iter()
            .filter(|e| e.event_type == EventType::Usage)
            .collect();
        assert_eq!(u2.len(), 1);
        // 用持久化 previous_total={100,20,50}：delta={50,10,30}→input=40,read=10
        let u = u2[0].usage.unwrap();
        assert_eq!((u.input, u.output, u.cache_read), (40, 30, 10), "跨批次 delta 正确");
        assert_eq!(u2[0].source_session_id, "cdx", "session_id 跨批次保留");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn no_newline_is_all_pending() {
        let (c, p) = split_complete_jsonl("{\"a\":1}");
        assert_eq!(c, "");
        assert_eq!(p, 7);
    }

    #[test]
    fn trailing_newline_no_pending() {
        let (c, p) = split_complete_jsonl("a\nb\n");
        assert_eq!(c, "a\nb\n");
        assert_eq!(p, 0);
    }

    #[test]
    fn half_line_is_pending() {
        let (c, p) = split_complete_jsonl("a\nb\nhalf");
        assert_eq!(c, "a\nb\n");
        assert_eq!(p, 4);
    }

    #[test]
    fn empty_input() {
        let (c, p) = split_complete_jsonl("");
        assert_eq!(c, "");
        assert_eq!(p, 0);
    }
}
