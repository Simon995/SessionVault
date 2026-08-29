//! 单来源增量扫描（§8）。按 `source_mode` 分派；已实现 append_log 与 snapshot_file。
//!
//! append_log 流程：stat 文件 → 比对 `(mtime, size)` 检测回退/截断 → 从 `safe_offset`
//! 读尾部 → `split_complete_jsonl` 切出完整行（半行留下轮）→ 解析 → 推进游标。
//! 移植自 QuotaBar `session_index.rs::split_complete_jsonl`（纯函数，带单测）。

use crate::cursor::{Cursor, CursorKind, ScanResult, ScanStatus};
use crate::discover::SourceRef;
use crate::logging::tag;
use crate::parser::{parse_lines, ParseCtx};
use crate::pathnorm::HostPlatform;
use crate::rawevent::{
    Actor, EventType, RawEvent, SourceLocation, SourceMode, TimeConfidence, SCHEMA_VERSION,
};
use crate::report::SourceReport;
use std::sync::Arc;

use crate::attribution::RootRegistry;
use crate::observation::{
    AppendLogObservation, ParseDiagnostics, ParseQuality, ScanFailure, SourceChange,
    SourceFingerprint,
};
use crate::Profile;

/// scan 主入口：按形态分派。append_log 的字节来源（本地 `File` vs WSL `wsl.exe`）
/// 经 [`ByteSource`] 抽象，游标/回退/坏行冻结逻辑两者**共用同一份**。
pub fn scan_source(
    source: &SourceRef,
    cursor_in: Option<Cursor>,
    profile: Profile,
    roots: Arc<RootRegistry>,
    deadline: crate::deadline::Deadline,
) -> ScanResult {
    match source.source_mode {
        SourceMode::AppendLog => {
            let (obs, report) =
                scan_append_log_observed(source, cursor_in, None, profile, roots, deadline);
            obs.into_scan_result(report)
        }
        SourceMode::SnapshotFile => scan_snapshot_file(source, cursor_in, profile),
        SourceMode::SqliteStore | SourceMode::OpaqueFamily => scan_unimplemented(source, cursor_in),
    }
}

/// append-log 扫描的**权威返回** —— 完整观察（ADR-051 §1）。
///
/// [`scan_source`] 是它的有损投影：`ScanStatus` 三个变体压掉了四种含义，于是消费方
/// 只能靠 `items_skipped` 与 warning 文案前缀猜回来（实测 QuotaBar 的 `svault_bridge`
/// 正是这么做的 —— **一条日志文案的前缀决定要不要写库**）。新消费方走这个入口。
pub fn scan_append_log_observed(
    source: &SourceRef,
    cursor_in: Option<Cursor>,
    prior_fingerprint: Option<SourceFingerprint>,
    profile: Profile,
    roots: Arc<RootRegistry>,
    deadline: crate::deadline::Deadline,
) -> (AppendLogObservation, SourceReport) {
    match &source.source_location {
        SourceLocation::Local => scan_append_log(
            &LocalSource { path: &source.path },
            source,
            cursor_in,
            prior_fingerprint,
            profile,
            roots,
        ),
        SourceLocation::Wsl(distro) => {
            let abs = source.path.to_string_lossy().into_owned();
            scan_append_log(
                &WslSource {
                    distro,
                    abs: &abs,
                    deadline,
                },
                source,
                cursor_in,
                prior_fingerprint,
                profile,
                roots,
            )
        }
    }
}

/// 其余形态骨架未实装：返回 NoCursor，事件空。
///
/// ⚠️ **不塞进 `ParseQuality::Unavailable`**：那表示「读不成」，而这里是「这种来源
/// 我们还不会读」—— 重试一万次也不会变，与一次瞬时 IO 失败的处置完全不同。
fn scan_unimplemented(source: &SourceRef, _cursor_in: Option<Cursor>) -> ScanResult {
    let mut report = SourceReport {
        source_path: source.path.display().to_string(),
        source_mode: Some(source.source_mode),
        cursor_kind: Some(CursorKind::NoCursor),
        ..Default::default()
    };
    report.warnings.push(format!(
        "source_mode {:?} not implemented",
        source.source_mode
    ));
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

/// 快照源文件的 mtime（Unix 秒），**问不到返回 `None`**。
///
/// 🔴 三条路径各有各的失败方式，而它们**都不是**「这个文件没有 mtime」：
///
/// | 路径 | 问法 | 问不到时 |
/// | --- | --- | --- |
/// | 本机 | `probe::stat`（经边界闸，不直接 `std::fs::metadata`） | `Absent` / `Unknown` → `None` |
/// | WSL（Windows 构建） | `wsl::stat` —— 它跑 `stat -c %Y` | 桥出错 / 文件不在 → `None` |
/// | WSL（非 Windows 构建） | 那座桥编译进来的是桩 | 恒 `Err` → `None` |
///
/// ⚠️ 这里**刻意不 warn**：快照扫描每轮对每个文件都跑一次，而 WSL 侧一次
/// `stat` 要拉一个 shell。把「拿不到」升成告警会在正常机器上刷屏
/// （本仓那条「一个满屏误报的扫描器等于一个关掉的扫描器」）。
/// 拿不到的后果是下游少一维排序信号，由 `modified_at: None` **如实表达**。
pub(crate) fn snapshot_mtime(source: &SourceRef) -> Option<i64> {
    match &source.source_location {
        SourceLocation::Local => match crate::probe::LocalBackend::unanchored()
            .stat(&source.path, crate::deadline::Deadline::unbounded())
        {
            crate::probe::Probed::Found(f) => f.modified_unix,
            _ => None,
        },
        SourceLocation::Wsl(distro) => crate::wsl::stat(
            distro,
            &source.path.to_string_lossy(),
            crate::deadline::Deadline::unbounded(),
        )
        .ok()
        .flatten()
        .map(|(_size, mtime)| mtime),
    }
}

fn scan_snapshot_file(
    source: &SourceRef,
    cursor_in: Option<Cursor>,
    profile: Profile,
) -> ScanResult {
    use sha2::{Digest, Sha256};

    let mut cursor = cursor_in.unwrap_or_else(Cursor::new_fingerprint);
    cursor.kind = CursorKind::Fingerprint;
    let mut report = SourceReport {
        source_path: source.path.display().to_string(),
        source_mode: Some(SourceMode::SnapshotFile),
        cursor_kind: Some(CursorKind::Fingerprint),
        ..Default::default()
    };
    let read = match &source.source_location {
        SourceLocation::Local => match crate::probe::read_bytes(&source.path, None) {
            crate::probe::Probed::Found(v) => Ok(v),
            crate::probe::Probed::Absent => Err("snapshot file disappeared".to_string()),
            crate::probe::Probed::Unknown(e) => Err(e.to_string()),
        },
        SourceLocation::Wsl(distro) => crate::wsl::read_file_at(
            distro,
            &source.path.to_string_lossy(),
            crate::deadline::Deadline::unbounded(),
        )
        .and_then(|v| v.ok_or_else(|| "snapshot file disappeared".to_string()))
        .map(String::into_bytes),
    };
    let bytes = match read {
        Ok(v) => v,
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
    report.bytes_read = bytes.len() as u64;
    report.items_examined = 1;
    let hash = format!("sha256:{:x}", Sha256::digest(&bytes));
    if cursor.content_hash.as_deref() == Some(&hash) {
        report.fingerprint_changed = false;
        return ScanResult {
            status: ScanStatus::Ok,
            events: Vec::new(),
            cursor_out: cursor,
            report,
        };
    }
    let content = match String::from_utf8(bytes) {
        Ok(v) => v,
        Err(e) => {
            report
                .warnings
                .push(format!("snapshot is not valid UTF-8: {e}"));
            return ScanResult {
                status: ScanStatus::Error,
                events: Vec::new(),
                cursor_out: cursor,
                report,
            };
        }
    };
    let observed_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .to_string();
    // 源文件的 mtime —— 见 `RawEvent::modified_at`：它与 `observed_at` 互补，
    // 一个分得开「扫描批次之间」，另一个分得开「批次之内的存量文件」。
    //
    // 🔴 **问不到就是 `None`，不折进任何一个值。** 三条路径各自可能失败
    // （本机 stat 报 Unknown / WSL 桥不可用 / 非 Windows 构建没有那座桥），
    // 而「没问出来」与「这个文件没有 mtime」在下游的处置不同。
    let modified_at = snapshot_mtime(source).map(|s| s.to_string());
    let event = RawEvent {
        schema_version: SCHEMA_VERSION,
        source_type: source.source_type,
        source_location: source.source_location.clone(),
        source_path: report.source_path.clone(),
        source_session_id: "snapshot".to_string(),
        seq: cursor.next_seq,
        // 快照类事件没有 `EventKey`：它不是「某条源记录内的第几个槽位」，整个文件才是
        // 一条记录，且它已经由 `content_hash` 标识。硬套一个键只会造出一个没人能解析
        // 的坐标。见 `rawevent::EventKey`。
        event_key: None,
        source_mode: SourceMode::SnapshotFile,
        cwd: None,
        project_root: source.project_root.clone(),
        project_root_source: source
            .project_root
            .as_ref()
            .map(|_| "host_identity".to_string()),
        workspace_location: source
            .project_root
            .as_ref()
            .map(|_| source.source_location.as_key()),
        event_type: EventType::ConfigSnapshot,
        actor: Some(Actor::System),
        occurred_at: None,
        time_confidence: TimeConfidence::Low,
        model: None,
        effort: None,
        usage: None,
        content: matches!(profile, Profile::Full).then_some(content),
        parent_ref: None,
        content_hash: Some(hash.clone()),
        artifact_kind: source.artifact_kind.clone(),
        observed_at: Some(observed_at),
        modified_at,
        message_id: None,
        request_id: None,
    };
    cursor.content_hash = Some(hash);
    cursor.next_seq += 1;
    report.fingerprint_changed = true;
    report.events_emitted = 1;
    log::info!(
        target: tag::SNAPSHOT,
        "snapshot changed: path={} kind={} seq={}",
        report.source_path,
        source.artifact_kind.as_deref().unwrap_or("unknown"),
        event.seq
    );
    ScanResult {
        status: ScanStatus::Ok,
        events: vec![event],
        cursor_out: cursor,
        report,
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
        // 经 `probe` 而不是直接 `std::fs::metadata` —— 见 `FileFacts` 的注释：
        // 这里从前是边界闸唯一的 carve-out，而那个 carve-out 让
        // `std::fs::metadata(p).is_ok()` 能溜过去（三轮评审 P2-2）。
        match crate::probe::LocalBackend::unanchored()
            .stat(self.path, crate::deadline::Deadline::unbounded())
        {
            crate::probe::Probed::Found(f) => Ok((f.len, f.modified_unix)),
            crate::probe::Probed::Absent => Err(format!("{} not found", self.path.display())),
            crate::probe::Probed::Unknown(e) => Err(e.to_string()),
        }
    }

    fn read_range(&self, start: u64, end: u64) -> Result<Vec<u8>, String> {
        read_range(self.path, start, end).map_err(|e| e.to_string())
    }
}

/// WSL 发行版内文件字节来源（经 `wsl.exe`）。`abs` 是发行版内 Linux 绝对路径。
struct WslSource<'a> {
    distro: &'a str,
    abs: &'a str,
    /// 🔴 **持有整轮 deadline**，不各自新建 —— `ByteSource` 的 `stat`/`read_range`
    /// 没有参数位，而「每次调用重新拿 60 秒」正是这次要修的（ADR-051 §4）。
    deadline: crate::deadline::Deadline,
}

impl ByteSource for WslSource<'_> {
    fn stat(&self) -> Result<(u64, Option<i64>), String> {
        match crate::wsl::stat(self.distro, self.abs, self.deadline)? {
            Some((size, mtime)) => Ok((size, Some(mtime))),
            None => Err(format!("wsl file missing: {}:{}", self.distro, self.abs)),
        }
    }

    fn read_range(&self, start: u64, end: u64) -> Result<Vec<u8>, String> {
        crate::wsl::read_range(self.distro, self.abs, start, end, self.deadline)
    }
}

/// 追加型日志增量扫描（字节来源经 [`ByteSource`] 抽象，本地/WSL 共用此函数）。
fn scan_append_log<S: ByteSource>(
    src: &S,
    source: &SourceRef,
    cursor_in: Option<Cursor>,
    prior_fingerprint: Option<SourceFingerprint>,
    profile: Profile,
    roots: Arc<RootRegistry>,
) -> (AppendLogObservation, SourceReport) {
    let mut report = SourceReport {
        source_path: source.path.display().to_string(),
        source_mode: Some(SourceMode::AppendLog),
        cursor_kind: Some(CursorKind::ByteOffset),
        ..Default::default()
    };

    // 一次性全扫（`cursor_in=None`，如影子全量对账 / 总库首扫）没有「下一轮重读」——坏行不能
    // 丢好行事件（一行坏 = 整文件 N 条全丢，会比 native/parse_lines 少数据）。增量（`Some`）仍需
    // 冻结+丢弃：保留事件又冻结游标的话下轮会把同一批好行再发一遍 → 事件流重复。
    let one_shot = cursor_in.is_none();
    let mut cursor = cursor_in.unwrap_or_else(Cursor::new_byte_offset);

    let (size, mtime) = match src.stat() {
        Ok(v) => v,
        Err(e) => {
            report.warnings.push(format!("stat failed: {e}"));
            // `Stat` 而非 `Read`：调用方据此**跳过本文件、不写库**。此前两者
            // 同为 `ScanStatus::Error`，消费方靠 warning 文案前缀区分。
            return (
                observe(
                    SourceChange::Appended,
                    ParseQuality::Unavailable(ScanFailure::Stat(e)),
                    cursor,
                    None,
                ),
                report,
            );
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
    // 🔴 **源变化是观察，不是意图** —— 由 stat 结果检出，调用方事先不知道。
    // ⚠️ 这里只认「变短 / mtime 倒退」；**同尺寸原地重写在这一层检不出来**，
    // 要靠下方全读时算出的指纹与上次比对（见 `source_fingerprint`）。
    let mut change = if rollback {
        SourceChange::RollbackOrRewrite
    } else {
        SourceChange::Appended
    };

    if start >= size {
        // 无新增。
        cursor.size = size;
        cursor.mtime = mtime;
        return (
            observe(
                change,
                ParseQuality::Clean {
                    deferred_tail_bytes: 0,
                },
                cursor,
                None,
            ),
            report,
        );
    }

    // 读 [start, size) 尾部。
    let tail = match src.read_range(start, size) {
        Ok(b) => b,
        Err(e) => {
            report.warnings.push(format!("read failed: {e}"));
            // `Read` 而非 `Stat`：文件在，只是读不出来 ⇒ 落一行 error 状态 + 冻游标，
            // 而不是当没发生。两者此前同为 `ScanStatus::Error`。
            return (
                observe(
                    change,
                    ParseQuality::Unavailable(ScanFailure::Read(e)),
                    cursor,
                    None,
                ),
                report,
            );
        }
    };
    report.bytes_read = tail.len() as u64;

    // 全读（`start == 0`）才算得出整份内容的指纹；增量手上只有尾巴，**算不出**。
    // `None` 因此表示「这一轮没读全文」，不是「内容没变」。
    let fingerprint = if start == 0 {
        let fp = SourceFingerprint::of(&tail);
        // 🔴 同尺寸原地重写：size 与 mtime 都可能一字不变，只有内容变了。
        // 不在这里认出来，UI 索引会 `ReplaceFile` 而总库走 `Append` 把新 seq 当重复
        // 丢弃 —— 两层从此不一致，且没有任何东西会说出来。
        // 🔴 **比对用独立的 `prior_fingerprint`，不用 `cursor.content_hash`。**
        //
        // 「从哪里开始读」与「上一版内容是什么」是两件事，而 `cursor_in: None`
        // 同时表达了它们 —— 强制全读时游标被整个丢掉，指纹也就一起没了。
        // 结果：QuotaBar 的生产路径**永远**传不进上一版指纹，同尺寸原地重写
        // 从来没有被识别过，而 SessionVault 这边的性质测试是手工存下指纹再传回的
        // ——**测的是测试自己设计的路径**。
        // 🔴 **比前缀，不比全文**（第二轮评审 P1-B）。
        //
        // 第一版是 `prev != fp`，即拿两个**全文**哈希比 —— 而中间只要发生过
        // 一次正常追加，两者就必然不等，于是纯追加被判成原地重写、走 `Rollback`、
        // 在总库留下一代**永不自动回收**的旧版本。新增的测试当时恰好在追加之后
        // 又做了一次真实重写，因此**没有覆盖「追加后直接强制全读」**。
        if let Some(prev) = prior_fingerprint.as_ref() {
            if prev.prefix_differs_from(&tail) {
                change = SourceChange::RollbackOrRewrite;
                log::warn!(
                    target: tag::CURSOR,
                    "in-place rewrite detected by fingerprint: path={}",
                    report.source_path
                );
            }
        }
        cursor.content_hash = Some(fp.as_str().to_string());
        Some(fp)
    } else {
        None
    };

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
        roots: roots.clone(),
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

    // P1：坏 JSON 行的处理按读取形态分两路（坏行总是 status≠Ok + 留 warning，不静默）：
    //
    // - **增量**（`Some(cursor)`）→ **冻结整批尾**（对齐 QuotaBar 实证，见 rawevent-reconciliation
    //   §2 / §8 规则 2）：保持起点 offset + status=error + 本轮不发事件 + 下轮重读整段尾。
    //   append-only 的完整行不可变，坏了就永远坏；宁可重读也不静默跳过/错解。保留事件又冻结游标
    //   会让下轮把同一批好行再发一遍（事件流重复），所以增量必须丢弃。
    //   （已知取舍：永久损坏行会让增量来源停在原地——继承自 QuotaBar；将来可在游标加 retry 计数
    //     做「毒行」跳过，属后续阶段。）
    // - **一次性全扫**（`cursor_in=None`，影子对账 / 总库首扫）→ **保留好行事件**：没有下一轮、
    //   不存在重复发；丢弃只会平白少数据（一行坏 = 整文件 N 条全丢，比 native/parse_lines 还差）。
    //   游标推进与否对一次性调用无意义（调用方丢弃 cursor_out）。status=Partial 标记「有坏行但已尽量
    //   解析」，与「半行 pending」的 Partial 同语义档位（带事件、非 Ok），供上层按需降级而非整文件作废。
    if parsed.skipped > 0 {
        if one_shot {
            cursor.safe_offset = size - pending as u64;
            cursor.next_seq = base_seq + parsed.events.len() as u64;
            cursor.codex_state = parsed.codex_state;
            report.events_emitted = parsed.events.len() as u64;
            log::warn!(
                target: tag::SCAN,
                "append_log one-shot kept good events despite bad json: path={} skipped={} events={}",
                report.source_path, parsed.skipped, parsed.events.len()
            );
            let diagnostics = ParseDiagnostics {
                skipped_lines: parsed.skipped,
                first_warning: report.warnings.first().cloned(),
            };
            let mut obs = observe(
                change,
                ParseQuality::Degraded(diagnostics),
                cursor,
                fingerprint,
            );
            obs.events = parsed.events;
            return (obs, report);
        }
        cursor.safe_offset = start; // 不前进
        cursor.codex_state = codex_state_before; // 不吃进坏批的状态推进
        report.events_emitted = 0;
        log::warn!(
            target: tag::SCAN,
            "append_log batch frozen (bad json): path={} skipped={} kept_offset={}",
            report.source_path, parsed.skipped, start
        );
        let diagnostics = ParseDiagnostics {
            skipped_lines: parsed.skipped,
            first_warning: report.warnings.first().cloned(),
        };
        // `RejectedPoisonLine` 而非 `Unavailable`：**字节读到了**，是我们主动丢了
        // 这一批。处置也不同 —— 游标冻在原处等下轮重读，而不是跳过本文件。
        return (
            observe(
                change,
                ParseQuality::RejectedPoisonLine(diagnostics),
                cursor,
                fingerprint,
            ),
            report,
        );
    }

    // 全部好行：推进 safe_offset 到「完整行」边界（size - pending），半行留下轮。
    cursor.safe_offset = size - pending as u64;
    cursor.next_seq = base_seq + parsed.events.len() as u64;
    cursor.codex_state = parsed.codex_state;
    report.events_emitted = parsed.events.len() as u64;

    log::info!(
        target: tag::SCAN,
        "append_log done: path={} events={} examined={} bytes={} pending={} rollback={}",
        report.source_path, report.events_emitted, report.items_examined, report.bytes_read, pending, report.rollback_detected
    );

    // 半截尾行是 append-log 的**常态**（写入方正写到一半），属于 `Clean` ——
    // 旧的 `Partial` 把它与「全读遇坏行」归成一档，于是消费方分不出「一切正常」
    // 与「有数据坏了但保住了好行」。
    let mut obs = observe(
        change,
        ParseQuality::Clean {
            deferred_tail_bytes: pending as u64,
        },
        cursor,
        fingerprint,
    );
    obs.events = parsed.events;
    (obs, report)
}

/// 构造一次观察。events 由调用点在需要时补上 —— 大多数失败路径本来就没有事件，
/// 让它们各自写一遍 `events: Vec::new()` 只会多几个可以写错的地方。
fn observe(
    source_change: SourceChange,
    quality: ParseQuality,
    cursor: Cursor,
    source_fingerprint: Option<SourceFingerprint>,
) -> AppendLogObservation {
    AppendLogObservation {
        source_change,
        quality,
        events: Vec::new(),
        cursor,
        source_fingerprint,
    }
}

/// 读文件 `[start, end)` 字节区间。
///
/// seek + read 整个在 `probe::read_range` 里做完 —— **`File` 不再逃出边界**
/// （五轮评审 P2：原始类型外泄时，有限的 def-path 清单实现不了「整个模块面」）。
/// 三态在这里折成 `io::Result` 交给上层，折叠写在**一处**且是在已分好类之后。
fn read_range(path: &std::path::Path, start: u64, end: u64) -> std::io::Result<Vec<u8>> {
    match crate::probe::read_range(path, start, end) {
        crate::probe::Probed::Found(buf) => Ok(buf),
        crate::probe::Probed::Absent => Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "file is gone",
        )),
        crate::probe::Probed::Unknown(e) => Err(std::io::Error::other(e.to_string())),
    }
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
// 测试要造 fixture（建目录、写文件、再核一遍），允许直接碰盘 —— 存在性边界管的是
// **生产行为**，而 `#[cfg(test)]` 不在生产路径上。允许写在模块上而不是逐个函数：
// 下一条测试不必再想一遍这件事，而生产代码里加一行照样会被 clippy 拦。
#[allow(clippy::disallowed_methods)]
mod tests {
    use std::sync::Arc;

    use super::{scan_source, split_complete_jsonl};
    use crate::attribution::RootRegistry;
    use crate::cursor::ScanStatus;

    /// 归属的输入。**空 = 一个根都不知道** ⇒ 每条路径 `Unattributed`。
    /// 本模块的用例测的是游标 / 字节 / 坏行，与归属无关，空表正是它们要的。
    fn no_roots() -> Arc<RootRegistry> {
        Arc::new(RootRegistry::new())
    }
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
            project_root: None,
            artifact_kind: None,
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

    fn temp_snapshot(name: &str, body: &str) -> (PathBuf, SourceRef) {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("svault-snapshot-{name}-{nanos}.md"));
        std::fs::write(&path, body).unwrap();
        let src = SourceRef {
            source_type: SourceType::Codex,
            source_location: SourceLocation::Local,
            source_mode: SourceMode::SnapshotFile,
            path: path.clone(),
            project_root: None,
            artifact_kind: Some("memory".into()),
        };
        (path, src)
    }

    #[test]
    fn snapshot_emits_only_on_hash_change_and_keeps_versions() {
        let (path, src) = temp_snapshot("changed", "# preference\nuse uv\n");
        let first = scan_source(
            &src,
            None,
            Profile::Full,
            no_roots(),
            crate::deadline::Deadline::unbounded(),
        );
        assert_eq!(first.status, ScanStatus::Ok);
        assert_eq!(first.events.len(), 1);
        assert!(first.report.fingerprint_changed);
        assert_eq!(first.events[0].event_type, EventType::ConfigSnapshot);
        assert_eq!(first.events[0].seq, 0);
        assert_eq!(
            first.events[0].content.as_deref(),
            Some("# preference\nuse uv\n")
        );
        assert!(first.events[0]
            .content_hash
            .as_deref()
            .is_some_and(|hash| hash.starts_with("sha256:")));

        let unchanged = scan_source(
            &src,
            Some(first.cursor_out),
            Profile::Full,
            no_roots(),
            crate::deadline::Deadline::unbounded(),
        );
        assert!(unchanged.events.is_empty());
        assert!(!unchanged.report.fingerprint_changed);

        std::fs::write(&path, "# preference\nuse uv only\n").unwrap();
        let changed = scan_source(
            &src,
            Some(unchanged.cursor_out),
            Profile::Full,
            no_roots(),
            crate::deadline::Deadline::unbounded(),
        );
        assert_eq!(changed.events.len(), 1);
        assert_eq!(changed.events[0].seq, 1);
        assert_ne!(changed.events[0].content_hash, first.events[0].content_hash);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn bad_json_one_shot_keeps_good_events() {
        // 一次性全扫（cursor=None，影子对账 / 总库首扫）：两条好行 + 一条坏 JSON →
        // **保留好行事件**、status=Partial、offset 推进。没有下一轮、不会重复发,丢弃只会
        // 平白少数据（一行坏 = 整文件全丢，比 native/parse_lines 还差）。
        let body = format!(
            "{}\n{}\nnot-json-here\n",
            claude_line("s1", "alpha"),
            claude_line("s1", "beta")
        );
        let (path, src) = temp_source("badoneshot", &body);
        let res = scan_source(
            &src,
            None,
            Profile::Full,
            no_roots(),
            crate::deadline::Deadline::unbounded(),
        );
        assert_eq!(res.status, ScanStatus::Partial);
        assert_eq!(res.events.len(), 2, "两条好行事件应保留");
        assert_eq!(res.report.items_skipped, 1);
        assert!(res.cursor_out.safe_offset > 0, "好行 offset 应推进");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn bad_json_incremental_freezes_batch_and_keeps_offset() {
        // 增量（带游标续扫）：坏行整批冻结 —— offset 不前进、status=error、不发事件。
        // 保留事件 + 冻结游标会让下轮把同一批好行再发一遍（事件流重复），故增量必须丢弃。
        let (path, src) = temp_source("badincr", &format!("{}\n", claude_line("s", "alpha")));
        let r1 = scan_source(
            &src,
            None,
            Profile::Full,
            no_roots(),
            crate::deadline::Deadline::unbounded(),
        );
        assert_eq!(r1.status, ScanStatus::Ok);
        let prev_offset = r1.cursor_out.safe_offset;

        append(
            &path,
            &format!("{}\nnot-json-here\n", claude_line("s", "beta")),
        );
        let r2 = scan_source(
            &src,
            Some(r1.cursor_out),
            Profile::Full,
            no_roots(),
            crate::deadline::Deadline::unbounded(),
        );
        assert_eq!(r2.status, ScanStatus::Error);
        assert!(r2.events.is_empty(), "增量坏行批不发事件");
        assert_eq!(
            r2.cursor_out.safe_offset, prev_offset,
            "offset 冻结在上轮完整行边界"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn all_good_lines_advance_offset() {
        // 全好行 → status=ok、offset 推进到完整行边界、无跳过。
        let body = "{\"a\":1}\n{\"b\":2}\n";
        let (path, src) = temp_source("good", body);
        let res = scan_source(
            &src,
            None,
            Profile::Metadata,
            no_roots(),
            crate::deadline::Deadline::unbounded(),
        );
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
            &format!(
                "{}\n{}\n",
                claude_line("s", "alpha"),
                claude_line("s", "beta")
            ),
        );
        let r1 = scan_source(
            &src,
            None,
            Profile::Full,
            no_roots(),
            crate::deadline::Deadline::unbounded(),
        );
        assert_eq!(r1.status, ScanStatus::Ok);
        let n1 = r1.events.len();
        assert_eq!(n1, 2, "两条 user 行 → 两个 message 事件");
        let off1 = r1.cursor_out.safe_offset;

        append(&path, &format!("{}\n", claude_line("s", "gamma")));
        let r2 = scan_source(
            &src,
            Some(r1.cursor_out),
            Profile::Full,
            no_roots(),
            crate::deadline::Deadline::unbounded(),
        );
        assert_eq!(r2.status, ScanStatus::Ok);
        assert_eq!(r2.events.len(), 1, "只出新增那一行");
        assert_eq!(r2.events[0].seq, n1 as u64, "seq 跨批续接（不重不漏）");
        assert_eq!(r2.events[0].content.as_deref(), Some("gamma"));
        assert!(
            r2.events
                .iter()
                .all(|e| e.content.as_deref() != Some("alpha")),
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
        let r1 = scan_source(
            &src,
            None,
            Profile::Full,
            no_roots(),
            crate::deadline::Deadline::unbounded(),
        );
        assert_eq!(r1.status, ScanStatus::Partial);
        assert!(r1.report.pending_tail_bytes > 0, "半行应 pending");
        assert_eq!(r1.events.len(), 1);
        assert_eq!(r1.events[0].content.as_deref(), Some("first"));

        append(&path, &format!("{}\n", &l2[cut..]));
        let r2 = scan_source(
            &src,
            Some(r1.cursor_out),
            Profile::Full,
            no_roots(),
            crate::deadline::Deadline::unbounded(),
        );
        assert_eq!(r2.report.pending_tail_bytes, 0);
        assert_eq!(r2.events.len(), 1);
        assert_eq!(r2.events[0].content.as_deref(), Some("second"));
        assert_eq!(r2.events[0].seq, 1, "seq 续接首批");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn rescan_unchanged_emits_nothing() {
        let (path, src) = temp_source("nochange", &format!("{}\n", claude_line("s", "x")));
        let r1 = scan_source(
            &src,
            None,
            Profile::Full,
            no_roots(),
            crate::deadline::Deadline::unbounded(),
        );
        let off = r1.cursor_out.safe_offset;
        let r2 = scan_source(
            &src,
            Some(r1.cursor_out),
            Profile::Full,
            no_roots(),
            crate::deadline::Deadline::unbounded(),
        );
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
        let r1 = scan_source(
            &src,
            None,
            Profile::Full,
            no_roots(),
            crate::deadline::Deadline::unbounded(),
        );
        assert!(r1.cursor_out.safe_offset > 0);
        assert!(r1.cursor_out.next_seq > 0);

        // 重写为更短内容（截断/重写）→ size 回退。
        std::fs::write(&path, format!("{}\n", claude_line("s", "fresh"))).unwrap();
        let r2 = scan_source(
            &src,
            Some(r1.cursor_out),
            Profile::Full,
            no_roots(),
            crate::deadline::Deadline::unbounded(),
        );
        assert!(r2.report.rollback_detected, "size 变小应触发回退");
        assert_eq!(r2.events.len(), 1);
        assert_eq!(r2.events[0].seq, 0, "回退后 seq 归零重读");
        assert_eq!(r2.events[0].content.as_deref(), Some("fresh"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn codex_cumulative_token_persists_across_scans() {
        // 命门：Codex 累计 token 的 previous_total 必须跨增量批次延续。
        let (path, src) = temp_source_codex(
            "codexincr",
            &format!("{}\n{}\n", codex_meta("cdx"), codex_token(100, 20, 50)),
        );
        let r1 = scan_source(
            &src,
            None,
            Profile::Full,
            no_roots(),
            crate::deadline::Deadline::unbounded(),
        );
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
        let r2 = scan_source(
            &src,
            Some(r1.cursor_out),
            Profile::Full,
            no_roots(),
            crate::deadline::Deadline::unbounded(),
        );
        let u2: Vec<_> = r2
            .events
            .iter()
            .filter(|e| e.event_type == EventType::Usage)
            .collect();
        assert_eq!(u2.len(), 1);
        // 用持久化 previous_total={100,20,50}：delta={50,10,30}→input=40,read=10
        let u = u2[0].usage.unwrap();
        assert_eq!(
            (u.input, u.output, u.cache_read),
            (40, 30, 10),
            "跨批次 delta 正确"
        );
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

    /// ADR-051 §1 的判据 —— 观察四态 + 同尺寸原地重写。
    ///
    /// 嵌在 `tests` 内而非做兄弟模块：`temp_source` / `claude_line` 这些 fixture 是
    /// 私有的，另起一个模块就得把它们逐个 `pub(super)` —— 为测试放宽可见性，是
    /// 让生产代码给测试让路。
    mod observation {
        use super::super::*; // scan 模块本体（`super` 只是 `tests`）
        use super::*; // tests 的 fixture：temp_source / claude_line / append / no_roots
        use crate::observation::{ParseQuality, ScanFailure, SourceChange};

        fn scan_obs(
            src: &SourceRef,
            cursor: Option<Cursor>,
        ) -> (crate::observation::AppendLogObservation, SourceReport) {
            scan_obs_with(src, cursor, None)
        }

        fn scan_obs_with(
            src: &SourceRef,
            cursor: Option<Cursor>,
            prior: Option<crate::observation::SourceFingerprint>,
        ) -> (crate::observation::AppendLogObservation, SourceReport) {
            scan_append_log_observed(
                src,
                cursor,
                prior,
                Profile::Full,
                no_roots(),
                crate::deadline::Deadline::unbounded(),
            )
        }

        /// 🔴 **本步的判据：同尺寸原地重写要被识别出来。**
        ///
        /// 回退检测只看 `size` 与 `mtime`，一次保留大小的原地重写**两样都不变**。
        /// 认不出来的后果：UI 索引走 `ReplaceFile`，总库走 `Append` 把新 seq 当重复
        /// 丢弃 —— 两层从此不一致，而没有任何东西会说出来。
        #[test]
        fn a_same_sized_in_place_rewrite_is_detected() {
            let (path, src) = temp_source("rewrite", &format!("{}\n", claude_line("s1", "aaa")));
            let (first, _) = scan_obs(&src, None);
            assert_eq!(first.source_change, SourceChange::Appended);
            let fp1 = first.source_fingerprint.clone().expect("全读必须算出指纹");

            // 同尺寸原地重写：内容变了，字节数一模一样。
            let rewritten = format!("{}\n", claude_line("s1", "bbb"));
            let before = std::fs::metadata(&path).unwrap().len();
            std::fs::write(&path, &rewritten).unwrap();
            assert_eq!(
                std::fs::metadata(&path).unwrap().len(),
                before,
                "前提：这次重写确实没改变文件大小"
            );

            // 🔴 **走生产路径的形状**：强制全读就是 `cursor_in = None`，
            // 上一版指纹**单独**传进来。
            //
            // ⚠️ 这条测试原先是造一个 `safe_offset = 0` 的游标把指纹「夹带」进去 ——
            // 而生产路径（QuotaBar 的 `scan_one`）force 时传的就是 `None`，游标连同
            // 指纹一起丢掉。于是被证明的性质**在真实运行里从没生效过**：
            // 测的是测试自己设计的那条路径（评审 P1-2）。
            let (second, _) = scan_obs_with(&src, None, Some(fp1.clone()));
            assert_ne!(
                second.source_fingerprint.as_ref().unwrap(),
                &fp1,
                "内容变了，指纹必须变"
            );
            assert_eq!(
                second.source_change,
                SourceChange::RollbackOrRewrite,
                "同尺寸重写要被认出来 —— size/mtime 都指望不上"
            );
            let _ = std::fs::remove_file(&path);
        }

        /// 增量读**算不出**整份指纹 —— `None` 是「没读全文」，不是「内容没变」。
        #[test]
        fn an_incremental_scan_has_no_fingerprint() {
            let (path, src) = temp_source("incr-fp", &format!("{}\n", claude_line("s1", "one")));
            let (first, _) = scan_obs(&src, None);
            assert!(first.source_fingerprint.is_some(), "全读有指纹");

            append(&path, &format!("{}\n", claude_line("s1", "two")));
            let (second, _) = scan_obs(&src, Some(first.cursor.clone()));
            assert!(
                second.source_fingerprint.is_none(),
                "增量只读了尾巴，算不出整份指纹 —— 不得拿尾巴的哈希冒充"
            );
            assert_eq!(second.source_change, SourceChange::Appended);
            let _ = std::fs::remove_file(&path);
        }

        /// 半截尾行是 `Clean`，不是降级 —— 它是 append-log 的常态。
        #[test]
        fn a_half_written_tail_is_clean_with_deferred_bytes() {
            let body = format!("{}\n{{\"type\":\"user\"", claude_line("s1", "done"));
            let (path, src) = temp_source("halftail", &body);
            let (obs, _) = scan_obs(&src, None);
            match obs.quality {
                ParseQuality::Clean {
                    deferred_tail_bytes,
                } => assert!(deferred_tail_bytes > 0, "半行的字节数要报出来"),
                other => panic!("半截尾行必须是 Clean，实际 {other:?}"),
            }
            assert!(obs.events_are_usable() && obs.should_record());
            let _ = std::fs::remove_file(&path);
        }

        /// 全读遇坏行 = **降级**：好行保留，数据可用。
        #[test]
        fn a_one_shot_scan_with_a_bad_line_degrades_but_keeps_events() {
            let body = format!("{}\nnot json at all\n", claude_line("s1", "good"));
            let (path, src) = temp_source("degraded", &body);
            let (obs, _) = scan_obs(&src, None);
            match &obs.quality {
                ParseQuality::Degraded(d) => {
                    assert_eq!(d.skipped_lines, 1);
                    assert!(d.first_warning.is_some(), "要说得出第一条坏在哪");
                }
                other => panic!("全读遇坏行应降级，实际 {other:?}"),
            }
            assert!(!obs.events.is_empty(), "好行必须保留 —— 丢弃只会平白少数据");
            assert!(obs.events_are_usable());
            let _ = std::fs::remove_file(&path);
        }

        /// 🔴 增量遇坏行 = **主动拒绝整批**，与「没读成」是两件事。
        ///
        /// 保留事件又冻结游标会让下轮把同一批好行再发一遍，所以整批丢弃。
        /// 处置也不同：游标冻在原处等下轮重读，而不是跳过本文件。
        #[test]
        fn an_incremental_bad_line_is_rejected_not_unavailable() {
            let (path, src) = temp_source("poison", &format!("{}\n", claude_line("s1", "one")));
            let (first, _) = scan_obs(&src, None);
            append(&path, "garbage\n");
            let (second, _) = scan_obs(&src, Some(first.cursor.clone()));

            assert!(
                matches!(second.quality, ParseQuality::RejectedPoisonLine(_)),
                "增量坏行是主动拒绝，不是 Unavailable：{:?}",
                second.quality
            );
            assert!(second.events.is_empty(), "整批丢弃");
            assert!(!second.events_are_usable());
            assert!(second.should_record(), "要落状态，否则下轮复用旧游标当成功");
            assert_eq!(
                second.cursor.safe_offset, first.cursor.safe_offset,
                "游标冻在原处"
            );
            let _ = std::fs::remove_file(&path);
        }

        /// 🔴 文件不在 ⇒ `Stat`，调用方**跳过本文件、不写库**。
        ///
        /// 与读失败分开，是因为处置相反：写一行 error 会把一次瞬时失败变成持久的坏记录。
        /// 此前两者同为 `ScanStatus::Error`，消费方靠 warning 文案前缀区分。
        #[test]
        fn a_vanished_file_reports_stat_failure_not_read_failure() {
            let (path, src) = temp_source("vanish", &format!("{}\n", claude_line("s1", "x")));
            std::fs::remove_file(&path).unwrap();
            let (obs, _) = scan_obs(&src, None);
            assert!(
                matches!(obs.quality, ParseQuality::Unavailable(ScanFailure::Stat(_))),
                "文件消失是 stat 失败：{:?}",
                obs.quality
            );
            assert!(
                !obs.should_record(),
                "跳过本文件，别把瞬时失败写成持久坏记录"
            );
        }
    }
    // ── 快照带上源文件的 mtime（2026-08-29）────────────────────────────

    #[test]
    fn a_snapshot_event_carries_the_source_file_mtime() {
        // 🔴 判据是**它与 `observed_at` 分得开的东西不同**，不是「有个值」。
        //
        // 消费者侧实测：206 个快照的 `observed_at` 只有 110 个不同取值，
        // **60% 与别人同秒**（首次上线时一次收进来的存量文件全挤在一起）。
        // mtime 是那 60% 唯一能分开的信号。
        let (path, src) = temp_snapshot("mtime", "# hello\n");
        let res = scan_source(
            &src,
            None,
            Profile::Full,
            no_roots(),
            crate::deadline::Deadline::unbounded(),
        );
        assert_eq!(res.status, ScanStatus::Ok);
        assert_eq!(res.events.len(), 1, "内容变了就该发一条快照事件");
        let ev = &res.events[0];

        let mtime = ev
            .modified_at
            .as_deref()
            .expect("本机快照应当带上 mtime")
            .parse::<i64>()
            .expect("mtime 是 Unix 秒字符串");
        let real = std::fs::metadata(&path)
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        assert_eq!(mtime, real, "报出来的 mtime 要等于文件真实的 mtime");

        // **成对的另一半**：它没有顶掉别的时间字段。
        assert!(
            ev.occurred_at.is_none(),
            "文件没有事件时间 —— mtime 不许被写进 occurred_at（§11），\
             否则每个快照都会作为一行「最近会话」参与排序"
        );
        assert!(ev.observed_at.is_some(), "observed_at 仍然要有——两者互补");
        assert_eq!(
            ev.time_confidence,
            crate::rawevent::TimeConfidence::Low,
            "mtime 是弱信号（会被 checkout/复制清掉），置信度不许因此升到 high"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn an_append_log_event_has_no_modified_at() {
        // 反向的一半：**追加日志的单条事件没有「文件写入时间」可言** ——
        // 整个 jsonl 一直在长，它的 mtime 属于最后一行不属于这一行。
        // 少了这条，「给所有事件都填上 mtime」也会全绿。
        let (path, src) = temp_source("nomtime", &format!("{}\n", claude_line("s", "alpha")));
        let res = scan_source(
            &src,
            None,
            Profile::Full,
            no_roots(),
            crate::deadline::Deadline::unbounded(),
        );
        assert_eq!(res.status, ScanStatus::Ok);
        assert!(!res.events.is_empty());
        for ev in &res.events {
            assert!(
                ev.modified_at.is_none(),
                "append_log 的事件不该带 modified_at"
            );
        }
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_snapshot_whose_file_cannot_be_stat_says_none_not_zero() {
        // 🔴 「没问出来」不是「时间是 0」。`snapshot_mtime` 走的是 `probe::stat`，
        // 它对一个不存在的路径答 `Absent` ⇒ 必须落到 `None`。
        //
        // ⚠️ 这条用**直接调 `snapshot_mtime`** 而不是走 `scan_source`：文件不在时
        // 扫描会先在读取那一步失败、根本不发事件，那样这条断言就**测不到它要测的
        // 那一段**（本仓那条「变异要打断生产代码实际走的那条路径」的同族）。
        let src = SourceRef {
            source_type: SourceType::ClaudeCode,
            source_location: SourceLocation::Local,
            source_mode: SourceMode::SnapshotFile,
            path: std::env::temp_dir().join("svault-definitely-not-here-9f3a1c.md"),
            project_root: None,
            artifact_kind: Some("memory".to_string()),
        };
        assert_eq!(super::snapshot_mtime(&src), None);
    }
}
