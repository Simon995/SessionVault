//! 增量保真离线验证（env-gated 实机）。
//!
//! 证明 `parse_lines`「分块带状态续解」与「整文件一次解」**逐字段一致**——这正是 QuotaBar
//! step 3（`svault_index` 写库路径）增量正确性的根据：QuotaBar 复用同一个 `parse_lines`，
//! 跨增量批次靠 `CodexParserState ⇄ CodexState` 桥接续传累计 token。若续传口径错（如 P1 修过的
//! 「新 session_meta 未重置 previous_total」类 bug），分块结果的 usage delta 就会与整解不符。
//!
//! 与 `parity` 工具互补：`parity` 验「SessionVault 全量 scan facts == QuotaBar 原生 cache facts」；
//! 本测验「SessionVault 分块解 == 整解」。两者合起来 ⇒ 分块（=step 3 增量）facts == 原生 facts。
//!
//! 默认跳过——需置 `SVAULT_PARITY_IT=1` 且本机有真实 Claude/Codex 本地会话才跑：
//! ```text
//! SVAULT_PARITY_IT=1 cargo test --test incremental_parity_it -- --nocapture
//! ```

use session_vault::catalog::Profile;
use session_vault::parser::{parse_lines, ParseCtx};
use session_vault::rawevent::{RawEvent, SourceLocation, SourceType};
use session_vault::{discover, HostPlatform};

/// 整文件一次解 vs 在给定行边界分两块、第二块带第一块状态续解——拼接后应与整解逐字段一致。
fn assert_chunk_equals_whole(ctx: &ParseCtx, lines: &[&str], split: usize, whole: &[RawEvent]) {
    let (a, b) = lines.split_at(split);
    let p1 = parse_lines(ctx, a, 0, None);
    // 续解：base_seq 接第一块事件数（镜像 scan_append_log 的 next_seq 推进），带入第一块累计状态。
    let p2 = parse_lines(ctx, b, p1.events.len() as u64, p1.codex_state.clone());

    let mut chunked = p1.events.clone();
    chunked.extend(p2.events.clone());

    assert_eq!(
        chunked.len(),
        whole.len(),
        "事件数不一致 file={} split={}: 分块 {} vs 整解 {}",
        ctx.source_path,
        split,
        chunked.len(),
        whole.len()
    );

    for (i, (c, w)) in chunked.iter().zip(whole.iter()).enumerate() {
        // RawEvent 无 PartialEq —— 用 JSON 序列化做全字段比对（含 usage/model/seq/session/ids/时间）。
        let cj = serde_json::to_string(c).unwrap();
        let wj = serde_json::to_string(w).unwrap();
        assert_eq!(
            cj, wj,
            "第 {i} 个事件分块≠整解 file={} split={}\n  分块: {cj}\n  整解: {wj}",
            ctx.source_path, split
        );
    }
}

#[test]
fn incremental_parse_equals_whole_on_real_local_data() {
    if std::env::var("SVAULT_PARITY_IT").is_err() {
        eprintln!("skip incremental_parity_it (set SVAULT_PARITY_IT=1 to run on real local data)");
        return;
    }

    let sources = discover().expect("discover sources");
    let mut files_checked = 0usize;
    let mut events_checked = 0usize;
    let mut codex_files = 0usize;

    for s in sources {
        // 只测本地（WSL 读取要走 wsl.exe 桥，与「解析增量保真」正交；解析逻辑 location 无关）。
        if !matches!(s.source_location, SourceLocation::Local) {
            continue;
        }
        if !matches!(s.source_type, SourceType::ClaudeCode | SourceType::Codex) {
            continue;
        }
        let bytes = match std::fs::read(&s.path) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let text = String::from_utf8_lossy(&bytes);
        let lines: Vec<&str> = text.lines().collect();
        if lines.len() < 2 {
            continue;
        }

        let ctx = ParseCtx {
            source_type: s.source_type,
            source_location: s.source_location.clone(),
            source_path: s.path.display().to_string(),
            profile: Profile::Full,
            host: HostPlatform::current(),
            default_distro: None,
        };

        let whole = parse_lines(&ctx, &lines, 0, None);

        // 多个分割点压边界：开头、三等分点、中点、末尾——覆盖 session_meta/turn_context/token_count
        // 各种跨块情形（Codex 累计 token 的续传最容易在这些边界出错）。
        let n = lines.len();
        let mut splits = vec![1, n / 3, n / 2, (2 * n) / 3, n - 1];
        splits.sort_unstable();
        splits.dedup();
        for split in splits {
            if split == 0 || split >= n {
                continue;
            }
            assert_chunk_equals_whole(&ctx, &lines, split, &whole.events);
        }

        files_checked += 1;
        events_checked += whole.events.len();
        if matches!(s.source_type, SourceType::Codex) {
            codex_files += 1;
        }
    }

    eprintln!(
        "incremental parity OK: files={files_checked} (codex={codex_files}) events={events_checked}"
    );
    assert!(
        files_checked > 0,
        "未扫到任何本地 Claude/Codex 文件——无法验证（确认本机有真实会话）"
    );
    assert!(
        codex_files > 0,
        "未扫到 Codex 本地文件——Codex 才有累计 token 续传，是本测的重点"
    );
}
