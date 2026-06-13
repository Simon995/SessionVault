//! 行级解析（§7）。把完整 JSONL 行解析校验为 `RawEvent`。
//!
//! 骨架阶段：只做 JSON 行合法性校验 + 计数，`RawEvent` 字段映射留 TODO
//! （待 P1 从 QuotaBar `parse_claude_lines` / `parse_codex_lines` 抽取过语料）。

use crate::cursor::CodexState;
use crate::rawevent::{RawEvent, SourceType};

/// 解析产物：本批事件 + 更新后的 Codex 状态 + 告警。
#[derive(Debug, Clone, Default)]
pub struct ParseOut {
    pub events: Vec<RawEvent>,
    pub codex_state: Option<CodexState>,
    /// 跳过的坏行数。
    pub skipped: u64,
    pub warnings: Vec<String>,
}

/// 解析一批完整行。`base_seq` 是本批首行在文件内的序号。
///
/// TODO(P1)：按 `source_type` 分派到 claude / codex 字段映射，发出 `RawEvent`。
/// 当前仅校验每行是否合法 JSON 并计数，保证骨架可编译可跑空转。
pub fn parse_lines(
    source_type: SourceType,
    lines: &[&str],
    _base_seq: u64,
    codex_state: Option<CodexState>,
) -> ParseOut {
    let mut out = ParseOut {
        codex_state,
        ..Default::default()
    };
    for line in lines {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<serde_json::Value>(trimmed) {
            Ok(_v) => {
                // TODO(P1): map -> RawEvent（claude/codex 分派）。
                let _ = source_type;
            }
            Err(e) => {
                out.skipped += 1;
                out.warnings.push(format!("bad json line: {e}"));
            }
        }
    }
    out
}
