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
    let codex_state = cursor.codex_state.take();
    let parsed = parse_lines(source.source_type, &lines, base_seq, codex_state);

    report.events_emitted = parsed.events.len() as u64;
    report.warnings.extend(parsed.warnings);

    // 推进游标：safe_offset 只推进到「完整行」边界（size - pending）。
    cursor.kind = CursorKind::ByteOffset;
    cursor.safe_offset = size - pending as u64;
    cursor.size = size;
    cursor.mtime = mtime;
    cursor.codex_state = parsed.codex_state;

    let status = if pending > 0 {
        ScanStatus::Partial
    } else {
        ScanStatus::Ok
    };

    log::info!(
        target: tag::SCAN,
        "append_log done: path={} events={} bytes={} pending={} rollback={}",
        report.source_path, report.events_emitted, report.bytes_read, pending, report.rollback_detected
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
    use super::split_complete_jsonl;

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
