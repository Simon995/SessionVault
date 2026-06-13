//! 单来源增量扫描（§8）。按 `source_mode` 分派；append_log 是骨架唯一实装路径。
//!
//! append_log 流程：stat 文件 → 比对 `(mtime, size)` 检测回退/截断 → 从 `safe_offset`
//! 读尾部 → `split_complete_jsonl` 切出完整行（半行留下轮）→ 解析 → 推进游标。
//! 移植自 QuotaBar `session_index.rs::split_complete_jsonl`（纯函数，带单测）。

use std::io::{Read, Seek, SeekFrom};

use crate::cursor::{Cursor, CursorKind, ScanResult, ScanStatus};
use crate::discover::SourceRef;
use crate::logging::tag;
use crate::parser::parse_lines;
use crate::rawevent::SourceMode;
use crate::report::SourceReport;
use crate::Profile;

/// scan 主入口：按形态分派。
pub fn scan_source(source: &SourceRef, cursor_in: Option<Cursor>, profile: Profile) -> ScanResult {
    match source.source_mode {
        SourceMode::AppendLog => scan_append_log(source, cursor_in, profile),
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

/// 追加型日志增量扫描。
fn scan_append_log(source: &SourceRef, cursor_in: Option<Cursor>, _profile: Profile) -> ScanResult {
    let path = &source.path;
    let mut report = SourceReport {
        source_path: path.display().to_string(),
        source_mode: Some(SourceMode::AppendLog),
        cursor_kind: Some(CursorKind::ByteOffset),
        ..Default::default()
    };

    let mut cursor = cursor_in.unwrap_or_else(Cursor::new_byte_offset);

    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
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
    let size = meta.len();
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64);

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
    let tail = match read_range(path, start, size) {
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
    let base_seq = 0; // TODO(P1): 真实文件内行号需累计；骨架占位。
    let codex_state_before = cursor.codex_state.clone();
    let parsed = parse_lines(source.source_type, &lines, base_seq, cursor.codex_state.take());

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
    use crate::rawevent::{SourceLocation, SourceMode, SourceType};
    use crate::Profile;
    use std::io::Write;

    /// 写一个唯一的临时 jsonl 文件，返回其 SourceRef（用完即弃，测试后删）。
    fn temp_source(name: &str, body: &str) -> (std::path::PathBuf, SourceRef) {
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
