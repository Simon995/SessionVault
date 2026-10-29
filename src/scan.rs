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
        SourceMode::SnapshotFile => scan_snapshot_file(source, cursor_in, profile, None),
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
    scan_append_log_prefetched(
        source,
        cursor_in,
        prior_fingerprint,
        profile,
        roots,
        deadline,
        &StatPrefetch::default(),
    )
}

/// 一轮开始时按发行版批量问到的 WSL 文件 `(size, mtime)`，喂给 [`scan_append_log_prefetched`]。
///
/// 没变化的文件只需要 stat。逐个问时每个文件一次 `wsl.exe`（约 150 ms 固定开销，QuotaBar
/// 实测一轮 354 个约 50 s）；预取时每个发行版一次，没变化的文件不再起进程。
///
/// 答案会过期：问完之后文件又追加了，这一轮按旧大小读、多出的留到下一轮（与逐个问时
/// 晚一点问等价）；问完之后被截断，`read_range` 短读即报错 —— 这一轮记读失败、游标不动，
/// 下一轮重新问时认出回退。不会读出错位的字节。
#[derive(Debug, Default)]
pub struct StatPrefetch {
    answers: std::collections::HashMap<(String, String), crate::wsl::StatAnswer>,
    /// 原文件确认不在的来源：它的压缩形态（`<路径>.zst`）的身份。键里是逻辑路径。
    compressed: std::collections::HashMap<(String, String), crate::wsl::IdentityAnswer>,
    /// 整份读的批：[`Self::reading_whole`] 规划的原文件，与解压后长度不在缓存里的压缩件。
    /// 批在第一次用到时才读（`&self` 上读，所以要锁）。
    whole: std::sync::Mutex<WholeReads>,
}

/// 整份读的批与已读到的内容。
#[derive(Debug, Default)]
struct WholeReads {
    /// 每批：一个发行版、按调用方给的顺序装、`stat` 到的大小合计 ≤ `READ_MANY_MAX_TOTAL`。
    batches: Vec<(String, Vec<String>)>,
    batch_of: std::collections::HashMap<(String, String), usize>,
    /// 已读的批（读过就不再读，没读成也一样 —— 那批的来源退回逐个读）。
    fetched_batches: std::collections::HashSet<usize>,
    /// 读到、还没交出去的内容。交出去就删，同时占着的内存约一批。
    contents: std::collections::HashMap<(String, String), crate::wsl::FileRead>,
    deadline: Option<crate::deadline::Deadline>,
}

impl WholeReads {
    fn add(&mut self, batches: Vec<(String, Vec<String>)>) {
        for (distro, paths) in batches {
            let i = self.batches.len();
            for p in &paths {
                self.batch_of.insert((distro.clone(), p.clone()), i);
            }
            self.batches.push((distro, paths));
        }
    }
}

/// 分批：按给的顺序，同一发行版的接着往当前批里装，装不下（合计会超过 `READ_MANY_MAX_TOTAL`）
/// 就另起一批。大小不在 `(0, READ_MANY_MAX_EACH]` 里的不收，照旧逐个读。
///
/// 按调用方的顺序装而不是按大小排：调用方按这个顺序逐个扫，一批读回来很快就交完，
/// 不会把几百 MB 同时留在内存里（QuotaBar 是托盘程序）。
fn plan_batches(
    items: impl IntoIterator<Item = (String, String, u64)>,
) -> Vec<(String, Vec<String>)> {
    let mut batches: Vec<(String, Vec<String>, u64)> = Vec::new();
    let mut open: std::collections::HashMap<String, usize> = Default::default();
    for (distro, path, size) in items {
        if size == 0 || size > crate::wsl::READ_MANY_MAX_EACH {
            continue;
        }
        let fits = open
            .get(&distro)
            .is_some_and(|&i| batches[i].2 + size <= crate::wsl::READ_MANY_MAX_TOTAL);
        if !fits {
            open.insert(distro.clone(), batches.len());
            batches.push((distro.clone(), Vec::new(), 0));
        }
        let i = open[&distro];
        batches[i].1.push(path);
        batches[i].2 += size;
    }
    batches
        .into_iter()
        .map(|(distro, paths, _)| (distro, paths))
        .collect()
}

/// WSL 上的 append-log 来源，按 `sources` 的顺序：`(发行版, 路径)`。
fn wsl_append_logs(sources: &[SourceRef]) -> impl Iterator<Item = (&str, String)> {
    sources
        .iter()
        .filter_map(|s| match (s.source_mode, &s.source_location) {
            (SourceMode::AppendLog, SourceLocation::Wsl(distro)) => {
                Some((distro.as_str(), s.path.to_string_lossy().into_owned()))
            }
            _ => None,
        })
}

/// [`StatPrefetch::reading_whole`] 的分批：只收 `stat` 到了的原文件。
fn plan_whole_batches(
    sources: &[SourceRef],
    answers: &std::collections::HashMap<(String, String), crate::wsl::StatAnswer>,
) -> Vec<(String, Vec<String>)> {
    plan_batches(wsl_append_logs(sources).filter_map(|(distro, path)| {
        match answers.get(&(distro.to_string(), path.clone())) {
            Some(Ok(Some((size, _)))) => Some((distro.to_string(), path, *size)),
            _ => None,
        }
    }))
}

/// 这一轮要整份读的压缩件：身份问到了，而解压后的长度不在缓存里（没算过，或身份变了）。分批。
fn plan_compressed_reads(
    sources: &[SourceRef],
    compressed: &std::collections::HashMap<(String, String), crate::wsl::IdentityAnswer>,
) -> Vec<(String, Vec<String>)> {
    plan_batches(wsl_append_logs(sources).filter_map(|(distro, path)| {
        let Some(Ok(Some(id))) = compressed.get(&(distro.to_string(), path.clone())) else {
            return None;
        };
        let zst = format!("{path}{}", crate::compressed::SUFFIX);
        cached_compressed_len(&(distro.to_string(), zst.clone()), *id)
            .is_none()
            .then(|| (distro.to_string(), zst, id.size))
    }))
}

/// 压缩件解压后的长度，键是 `(发行版, .zst 路径)`，以文件身份为准（[`crate::wsl::FileIdentity`]）。
///
/// WSL 里的压缩件多数帧头不写解压后的长度（2026-10-29 实测 248 个里 202 个），要知道长度只能整份读回来
/// 解一遍；不缓存时每轮每个压缩件一次 `wsl.exe`、搬全部字节（实测一轮 282 次、788 MiB、约 1 分钟）。
/// 压缩件写成后不再变，变了身份必变 —— 身份没变就用上次算的。进程内有效：常驻的宿主只在启动后
/// 第一轮、以及 Codex 改写过的文件上读。
type CompressedLengths =
    std::collections::HashMap<(String, String), (crate::wsl::FileIdentity, u64)>;

fn compressed_lengths() -> &'static std::sync::Mutex<CompressedLengths> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<CompressedLengths>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(Default::default)
}

fn cached_compressed_len(key: &(String, String), id: crate::wsl::FileIdentity) -> Option<u64> {
    let cache = compressed_lengths().lock().ok()?;
    cache
        .get(key)
        .and_then(|(known, len)| (*known == id).then_some(*len))
}

impl StatPrefetch {
    /// 给这批来源里 WSL 上的 append-log 来源预取，每个发行版问一次。本机来源不预取（本来就便宜）。
    ///
    /// 某个发行版整批没问成，就不给它留答案：那些来源照旧逐个问，与没有预取时一样。
    ///
    /// 原文件确认不在的（可能被就地压缩了），再一次问完它们压缩形态的身份（有就每个发行版多一次
    /// `wsl.exe`）：身份与上次算长度时相同的，不再读它（[`compressed_lengths`]）；不同或没算过的，
    /// 按发行版分批整份读，用到时才读。
    pub fn for_sources(sources: &[SourceRef], deadline: crate::deadline::Deadline) -> Self {
        let mut by_distro: std::collections::BTreeMap<&str, Vec<String>> = Default::default();
        for (distro, path) in wsl_append_logs(sources) {
            by_distro.entry(distro).or_default().push(path);
        }
        let mut answers = std::collections::HashMap::new();
        let mut compressed = std::collections::HashMap::new();
        for (distro, paths) in by_distro {
            let batch = match crate::wsl::stat_many(distro, &paths, deadline) {
                Ok(batch) => batch,
                Err(e) => {
                    log::warn!(
                        target: tag::WSL,
                        "batch stat failed for {distro} ({} sources fall back to one wsl.exe each): {e}",
                        paths.len()
                    );
                    continue;
                }
            };
            let zst: Vec<String> = paths
                .iter()
                .filter(|p| matches!(batch.get(*p), Some(Ok(None))))
                .map(|p| format!("{p}{}", crate::compressed::SUFFIX))
                .collect();
            answers.extend(
                batch
                    .into_iter()
                    .map(|(path, answer)| ((distro.to_string(), path), answer)),
            );
            match crate::wsl::identify_many(distro, &zst, deadline) {
                Ok(ids) => compressed.extend(ids.into_iter().map(|(zst, answer)| {
                    let logical = zst
                        .strip_suffix(crate::compressed::SUFFIX)
                        .unwrap_or(&zst)
                        .to_string();
                    ((distro.to_string(), logical), answer)
                })),
                Err(e) => log::warn!(
                    target: tag::WSL,
                    "batch identify failed for {distro} ({} compressed candidates are read whole one by one): {e}",
                    zst.len()
                ),
            }
        }
        let mut whole = WholeReads {
            deadline: Some(deadline),
            ..Default::default()
        };
        whole.add(plan_compressed_reads(sources, &compressed));
        Self {
            answers,
            compressed,
            whole: std::sync::Mutex::new(whole),
        }
    }

    /// 这些来源这一轮会**从头整份读**（宿主强制重读、首次回填）：其中 WSL 上不超过单文件上限的，
    /// 按发行版分批读（每批一次 `wsl.exe`、合计 ≤ 32 MiB），扫描时直接用读到的字节。
    ///
    /// 逐个读时每个文件一次 `wsl.exe`：QuotaBar 实测（2026-10-10）重建索引一轮 305 个文件起了
    /// 306 次、120 s。批在第一次用到时才读，读完一份交出去一份。只用于从 0 开始的读；大小取本预取
    /// `stat` 到的，没问成的不规划。整批没读成、或读到的比 `stat` 时短（中途被截断），那个来源照旧
    /// 逐个读 —— 截断由逐个读的短读报错处理，与没有预取时一样。
    pub fn reading_whole(
        mut self,
        sources: &[SourceRef],
        deadline: crate::deadline::Deadline,
    ) -> Self {
        let batches = plan_whole_batches(sources, &self.answers);
        let whole = self.whole.get_mut().unwrap_or_else(|e| e.into_inner());
        whole.add(batches);
        whole.deadline = Some(deadline);
        self
    }

    fn get(&self, distro: &str, abs: &str) -> Option<&crate::wsl::StatAnswer> {
        self.answers.get(&(distro.to_string(), abs.to_string()))
    }

    fn get_compressed(&self, distro: &str, abs: &str) -> Option<&crate::wsl::IdentityAnswer> {
        self.compressed.get(&(distro.to_string(), abs.to_string()))
    }

    /// 这个文件整份读到的内容（交出去就不再留）。没规划、批没读成、这一条没读到 ⇒ `None`。
    fn take_whole(&self, distro: &str, abs: &str) -> Option<crate::wsl::FileRead> {
        let mut w = self.whole.lock().ok()?;
        let key = (distro.to_string(), abs.to_string());
        let i = *w.batch_of.get(&key)?;
        if w.fetched_batches.insert(i) {
            let (batch_distro, paths) = w.batches[i].clone();
            let deadline = w.deadline?;
            match crate::wsl::read_many(&batch_distro, &paths, deadline) {
                Ok(answers) => {
                    for (path, answer) in answers {
                        if let Ok(Some(read)) = answer {
                            w.contents.insert((batch_distro.clone(), path), read);
                        }
                    }
                }
                Err(e) => log::warn!(
                    target: tag::WSL,
                    "batch read failed for {batch_distro} ({} sources fall back to one wsl.exe each): {e}",
                    paths.len()
                ),
            }
        }
        w.contents.remove(&key)
    }
}

/// 整份读到的字节里取 `[0, end)`；读到的比 `end` 短（`stat` 之后被截断）⇒ `None`，调用方逐个读。
fn whole_prefix(mut bytes: Vec<u8>, end: u64) -> Option<Vec<u8>> {
    let end = usize::try_from(end).ok()?;
    if bytes.len() < end {
        return None;
    }
    bytes.truncate(end);
    Some(bytes)
}

/// [`scan_append_log_observed`]，但 WSL 来源的 stat 先查 `prefetch`：有答案就不再起 `wsl.exe`。
pub fn scan_append_log_prefetched(
    source: &SourceRef,
    cursor_in: Option<Cursor>,
    prior_fingerprint: Option<SourceFingerprint>,
    profile: Profile,
    roots: Arc<RootRegistry>,
    deadline: crate::deadline::Deadline,
    prefetch: &StatPrefetch,
) -> (AppendLogObservation, SourceReport) {
    match &source.source_location {
        SourceLocation::Local => scan_append_log(
            &LocalSource::new(&source.path),
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
                    prefetched: prefetch.get(distro, &abs),
                    prefetched_compressed: prefetch.get_compressed(distro, &abs),
                    whole: prefetch,
                    stored: Default::default(),
                    compressed: Default::default(),
                    identity: Default::default(),
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
            // 🔴 两支都归 `None`，而它们**不是同一件事** —— 见上表。
            // 合并在这里是安全的，理由只有一条：本函数的返回值只喂一维
            // **排序信号**（`RawEvent.modified_at`），不驱动任何
            // prune / 删除 / 归属判决。写成两支而不是 `_`，是为了让「我们
            // 知道这个区别存在、并且是刻意压掉的」留在代码里：哪天这个值
            // 被拿去做别的判断，改的人第一眼就看见这里压过一个三态。
            crate::probe::Probed::Absent => None,
            crate::probe::Probed::Unknown(_) => None,
        },
        // 同上：`Ok(None)`（文件不在）与 `Err`（桥没答上来 / 非 Windows 构建
        // 的桩）分别写出来。原先这里是 `.ok().flatten()` —— 它把两者压成一个
        // 表达式，于是「没问成」在代码上长得和「这里是空的」一模一样，正是
        // 本仓反复立规矩要防的那个形状（`docs/scan-state-model.md` §5）。
        SourceLocation::Wsl(distro) => match crate::wsl::stat(
            distro,
            &source.path.to_string_lossy(),
            crate::deadline::Deadline::unbounded(),
        ) {
            Ok(Some((_size, mtime))) => Some(mtime),
            Ok(None) => None,
            Err(_) => None,
        },
    }
}

/// 一轮开始时按发行版批量读到的 WSL 快照文件，喂给 [`scan_snapshot_prefetched`]。
///
/// 快照每轮都要整份读来比对内容；逐个读时每个文件一次 `wsl.exe`（本机实测 319 个约 60 s），
/// 批量读时每个发行版一次。只改怎么读：读到的字节、UTF-8 校验、内容比对与逐个读时一样。
#[derive(Debug, Default)]
pub struct SnapshotPrefetch {
    reads: std::collections::HashMap<(String, String), crate::wsl::ReadAnswer>,
}

impl SnapshotPrefetch {
    /// 给这批来源里 WSL 上的快照来源预读，每个发行版一次。整批没读成的发行版不留答案，照旧逐个读。
    pub fn for_sources(sources: &[SourceRef], deadline: crate::deadline::Deadline) -> Self {
        let mut by_distro: std::collections::BTreeMap<&str, Vec<String>> = Default::default();
        for s in sources {
            if let (SourceMode::SnapshotFile, SourceLocation::Wsl(distro)) =
                (s.source_mode, &s.source_location)
            {
                by_distro
                    .entry(distro)
                    .or_default()
                    .push(s.path.to_string_lossy().into_owned());
            }
        }
        let mut reads = std::collections::HashMap::new();
        for (distro, paths) in by_distro {
            match crate::wsl::read_many(distro, &paths, deadline) {
                Ok(batch) => reads.extend(
                    batch
                        .into_iter()
                        .map(|(path, answer)| ((distro.to_string(), path), answer)),
                ),
                Err(e) => log::warn!(
                    target: tag::WSL,
                    "batch read failed for {distro} ({} snapshots fall back to one wsl.exe each): {e}",
                    paths.len()
                ),
            }
        }
        Self { reads }
    }

    fn get(&self, source: &SourceRef) -> Option<&crate::wsl::ReadAnswer> {
        let SourceLocation::Wsl(distro) = &source.source_location else {
            return None;
        };
        self.reads
            .get(&(distro.clone(), source.path.to_string_lossy().into_owned()))
    }
}

/// 快照来源的扫描，但 WSL 文件先查 `prefetch`：批里读到了就不再起 `wsl.exe`。
pub fn scan_snapshot_prefetched(
    source: &SourceRef,
    cursor_in: Option<Cursor>,
    profile: Profile,
    prefetch: &SnapshotPrefetch,
) -> ScanResult {
    scan_snapshot_file(source, cursor_in, profile, prefetch.get(source))
}

fn scan_snapshot_file(
    source: &SourceRef,
    cursor_in: Option<Cursor>,
    profile: Profile,
    prefetched: Option<&crate::wsl::ReadAnswer>,
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
    let batch_mtime = match prefetched {
        Some(Ok(Some(file))) => Some(file.mtime),
        _ => None,
    };
    let read = match (&source.source_location, prefetched) {
        (SourceLocation::Local, _) => match crate::probe::read_bytes(&source.path, None) {
            crate::probe::Probed::Found(v) => Ok(v),
            crate::probe::Probed::Absent => Err("snapshot file disappeared".to_string()),
            crate::probe::Probed::Unknown(e) => Err(e.to_string()),
        },
        // 与 `read_file_at` 同一条规矩：不是合法 UTF-8 就是没读成。
        (SourceLocation::Wsl(distro), Some(Ok(Some(file)))) => std::str::from_utf8(&file.bytes)
            .map(|_| file.bytes.clone())
            .map_err(|e| {
                format!(
                    "wsl read {distro}:{} not valid UTF-8: {e}",
                    report.source_path
                )
            }),
        (SourceLocation::Wsl(_), Some(Ok(None))) => Err("snapshot file disappeared".to_string()),
        (SourceLocation::Wsl(distro), Some(Err(_)) | None) => crate::wsl::read_file_at(
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
    let modified_at = batch_mtime
        .or_else(|| snapshot_mtime(source))
        .map(|s| s.to_string());
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

/// 逻辑来源此刻以哪种形态存在（[`crate::compressed`]）。由 `stat` 认出，`read_range` 按它读：
/// 两次调用之间换了形态（正好被压缩或还原），读那一下失败、游标不动，下一轮重认。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Stored {
    #[default]
    Plain,
    /// 只有 `<path>.zst`：大小、区间都按解压后的字节算。
    Compressed,
}

/// 本机文件字节来源。`path` 是逻辑路径：它不在、`<path>.zst` 在，就读压缩形态。
struct LocalSource<'a> {
    path: &'a std::path::Path,
    stored: std::cell::Cell<Stored>,
}

impl<'a> LocalSource<'a> {
    fn new(path: &'a std::path::Path) -> Self {
        Self {
            path,
            stored: Default::default(),
        }
    }
}

impl ByteSource for LocalSource<'_> {
    fn stat(&self) -> Result<(u64, Option<i64>), String> {
        // 经 `probe` 而不是直接 `std::fs::metadata` —— 见 `FileFacts` 的注释：
        // 这里从前是边界闸唯一的 carve-out，而那个 carve-out 让
        // `std::fs::metadata(p).is_ok()` 能溜过去（三轮评审 P2-2）。
        let stat = |p: &std::path::Path| {
            crate::probe::LocalBackend::unanchored().stat(p, crate::deadline::Deadline::unbounded())
        };
        match stat(self.path) {
            crate::probe::Probed::Found(f) => {
                self.stored.set(Stored::Plain);
                return Ok((f.len, f.modified_unix));
            }
            crate::probe::Probed::Absent => {}
            crate::probe::Probed::Unknown(e) => return Err(e.to_string()),
        }
        // 原文件不在：可能被就地压缩了。两种形态都不在才是「不在」。
        let zst = crate::compressed::compressed_path(self.path);
        let mtime = match stat(&zst) {
            crate::probe::Probed::Found(f) => f.modified_unix,
            crate::probe::Probed::Absent => {
                return Err(format!(
                    "{} not found (nor its compressed form)",
                    self.path.display()
                ))
            }
            crate::probe::Probed::Unknown(e) => return Err(e.to_string()),
        };
        match crate::probe::with_reader(&zst, |r| crate::compressed::decoded_len(r)) {
            crate::probe::Probed::Found(len) => {
                self.stored.set(Stored::Compressed);
                Ok((len, mtime))
            }
            crate::probe::Probed::Absent => Err(format!("{} vanished", zst.display())),
            crate::probe::Probed::Unknown(e) => Err(e.to_string()),
        }
    }

    fn read_range(&self, start: u64, end: u64) -> Result<Vec<u8>, String> {
        match self.stored.get() {
            Stored::Plain => read_range(self.path, start, end).map_err(|e| e.to_string()),
            Stored::Compressed => {
                let zst = crate::compressed::compressed_path(self.path);
                match crate::probe::with_reader(&zst, |r| {
                    crate::compressed::decode_range(r, start, end)
                }) {
                    crate::probe::Probed::Found(bytes) => Ok(bytes),
                    crate::probe::Probed::Absent => Err(format!("{} is gone", zst.display())),
                    crate::probe::Probed::Unknown(e) => Err(e.to_string()),
                }
            }
        }
    }
}

/// WSL 发行版内文件字节来源（经 `wsl.exe`）。`abs` 是发行版内 Linux 绝对路径。
struct WslSource<'a> {
    distro: &'a str,
    abs: &'a str,
    /// 🔴 **持有整轮 deadline**，不各自新建 —— `ByteSource` 的 `stat`/`read_range`
    /// 没有参数位，而「每次调用重新拿 60 秒」正是这次要修的（ADR-051 §4）。
    deadline: crate::deadline::Deadline,
    /// 本轮预取到的答案（[`StatPrefetch`]）。批量里这一条没问成（`Err`）就单独再问。
    prefetched: Option<&'a crate::wsl::StatAnswer>,
    /// 本轮预取到的压缩形态（`<路径>.zst`）身份：原文件确认不在时才有。
    prefetched_compressed: Option<&'a crate::wsl::IdentityAnswer>,
    /// 整份读的批（[`StatPrefetch`]）；从 0 开始读原文件、或读压缩件时先查它。
    whole: &'a StatPrefetch,
    stored: std::cell::Cell<Stored>,
    /// 压缩形态的全部字节：`stat` 读回来算了长度的，`read_range` 接着用它，不再起第二次 `wsl.exe`。
    compressed: std::cell::RefCell<Option<Vec<u8>>>,
    /// 长度取自缓存时那个压缩件的身份：`read_range` 补读字节时核它没变。
    identity: std::cell::Cell<Option<crate::wsl::FileIdentity>>,
}

impl WslSource<'_> {
    fn zst(&self) -> String {
        format!("{}{}", self.abs, crate::compressed::SUFFIX)
    }

    /// 原文件不在时问压缩形态，答解压后的长度。
    ///
    /// 预取问到了它的身份、且与上次算长度时相同 ⇒ 用缓存的长度，不读（字节等 `read_range` 真要用时
    /// 再读）。否则整份读回来（预取规划的批，或单独一到两次 `wsl.exe`），在本机算；读到的与预取到的
    /// 身份对得上才记进缓存。预取确认两种形态都不在 ⇒ 直接报不在，不再起进程。
    fn stat_compressed(&self) -> Result<(u64, Option<i64>), String> {
        let zst = self.zst();
        let missing = || {
            format!(
                "wsl file missing: {}:{} (nor its compressed form)",
                self.distro, self.abs
            )
        };
        let id = match self.prefetched_compressed {
            Some(Ok(None)) => return Err(missing()),
            Some(Ok(Some(id))) => Some(*id),
            Some(Err(_)) | None => None,
        };
        let key = (self.distro.to_string(), zst.clone());
        if let Some(id) = id {
            if let Some(len) = cached_compressed_len(&key, id) {
                self.stored.set(Stored::Compressed);
                self.identity.set(Some(id));
                return Ok((len, Some(id.mtime)));
            }
        }
        let read = match self.whole.take_whole(self.distro, &zst) {
            Some(read) => read,
            None => crate::wsl::read_whole(self.distro, &zst, self.deadline)
                .map_err(|e| format!("{}:{zst}: {e}", self.distro))?
                .ok_or_else(missing)?,
        };
        let len = crate::compressed::decoded_len(&mut std::io::Cursor::new(&read.bytes))
            .map_err(|e| format!("{}:{zst}: {e}", self.distro))?;
        if let Some(id) =
            id.filter(|id| id.size == read.bytes.len() as u64 && id.mtime == read.mtime)
        {
            if let Ok(mut cache) = compressed_lengths().lock() {
                cache.insert(key, (id, len));
            }
        }
        self.stored.set(Stored::Compressed);
        *self.compressed.borrow_mut() = Some(read.bytes);
        Ok((len, Some(read.mtime)))
    }
}

impl ByteSource for WslSource<'_> {
    fn stat(&self) -> Result<(u64, Option<i64>), String> {
        let answer = match self.prefetched {
            Some(Ok(answer)) => *answer,
            Some(Err(_)) | None => crate::wsl::stat(self.distro, self.abs, self.deadline)?,
        };
        match answer {
            Some((size, mtime)) => {
                self.stored.set(Stored::Plain);
                Ok((size, Some(mtime)))
            }
            None => self.stat_compressed(),
        }
    }

    fn read_range(&self, start: u64, end: u64) -> Result<Vec<u8>, String> {
        if self.stored.get() == Stored::Compressed {
            if self.compressed.borrow().is_none() {
                // 长度取自缓存、字节还没读：现在读，且须仍是 stat 时那一份。
                let zst = self.zst();
                let read = crate::wsl::read_whole(self.distro, &zst, self.deadline)
                    .map_err(|e| format!("{}:{zst}: {e}", self.distro))?
                    .ok_or_else(|| format!("{}:{zst} is gone", self.distro))?;
                if let Some(id) = self.identity.get() {
                    if id.size != read.bytes.len() as u64 || id.mtime != read.mtime {
                        return Err(format!(
                            "{}:{zst} changed after it was stat'ed",
                            self.distro
                        ));
                    }
                }
                *self.compressed.borrow_mut() = Some(read.bytes);
            }
            let compressed = self.compressed.borrow();
            let bytes = compressed
                .as_deref()
                .ok_or_else(|| format!("{}:{} was not read", self.distro, self.abs))?;
            return crate::compressed::decode_range(bytes, start, end).map_err(|e| {
                format!(
                    "{}:{}{}: {e}",
                    self.distro,
                    self.abs,
                    crate::compressed::SUFFIX
                )
            });
        }
        if start == 0 {
            if let Some(bytes) = self
                .whole
                .take_whole(self.distro, self.abs)
                .and_then(|read| whole_prefix(read.bytes, end))
            {
                return Ok(bytes);
            }
        }
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
    // 下的边角由 host/CLI 决定是否注入 wsl::default_distro）。按库重算归属用同一个函数。
    let ctx = ParseCtx {
        source_type: source.source_type,
        source_location: source.source_location.clone(),
        source_path: report.source_path.clone(),
        profile,
        host: HostPlatform::current(),
        default_distro: crate::parser::scan_default_distro(&source.source_location),
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

        /// 就地压缩成 `<path>.zst`（带上原文件的 mtime —— Codex 会复制元数据），删原文件。
        fn compress_in_place(path: &Path) -> PathBuf {
            let body = std::fs::read(path).unwrap();
            let mtime = std::fs::metadata(path).unwrap().modified().unwrap();
            let zst = crate::compressed::compressed_path(path);
            let packed = ruzstd::encoding::compress_to_vec(
                body.as_slice(),
                ruzstd::encoding::CompressionLevel::Fastest,
            );
            std::fs::write(&zst, packed).unwrap();
            std::fs::File::options()
                .write(true)
                .open(&zst)
                .unwrap()
                .set_modified(mtime)
                .unwrap();
            std::fs::remove_file(path).unwrap();
            zst
        }

        fn json(events: &[crate::rawevent::RawEvent]) -> serde_json::Value {
            serde_json::to_value(events).unwrap()
        }

        /// 🔴 压缩件与原文件是**同一个来源**（Codex 就地压缩冷会话，2026-10-15 起）。
        ///
        /// 判据（QuotaBar 按规则 2 提的）：换了形态不报「消失」、不重复出事件。所以压缩前扫到
        /// 哪儿，压缩后接着从哪儿 —— 没有新事件、不认成回退或原地重写；从头全读，事件、游标、
        /// 指纹与读原文时一模一样；还原回原文、续写之后只读新增那一段，累计用量接得上。
        #[test]
        fn a_compressed_rollout_is_the_same_source_as_the_plain_one() {
            let body = format!("{}\n{}\n", codex_meta("cdx"), codex_token(100, 20, 50));
            let (path, src) = temp_source_codex("zst", &body);
            let (plain, _) = scan_obs(&src, None);
            assert!(!plain.events.is_empty());
            let fp = plain.source_fingerprint.clone().expect("全读必须算出指纹");

            let zst = compress_in_place(&path);
            let (caught_up, _) = scan_obs_with(&src, Some(plain.cursor.clone()), Some(fp.clone()));
            assert!(
                matches!(caught_up.quality, ParseQuality::Clean { .. }),
                "{:?}",
                caught_up.quality
            );
            assert_eq!(caught_up.source_change, SourceChange::Appended);
            assert!(caught_up.events.is_empty(), "已经读过的不再出事件");
            assert_eq!(caught_up.cursor.safe_offset, plain.cursor.safe_offset);
            assert_eq!(
                caught_up.cursor.size, plain.cursor.size,
                "大小按解压后的字节算"
            );

            let (whole, _) = scan_obs_with(&src, None, Some(fp.clone()));
            assert_eq!(whole.source_change, SourceChange::Appended, "不是原地重写");
            assert_eq!(json(&whole.events), json(&plain.events));
            assert_eq!(whole.cursor.safe_offset, plain.cursor.safe_offset);
            assert_eq!(whole.source_fingerprint, Some(fp));

            // 续聊：还原成原文、删 `.zst`、再追加一行。
            std::fs::write(&path, &body).unwrap();
            std::fs::remove_file(&zst).unwrap();
            append(&path, &format!("{}\n", codex_token(150, 30, 80)));
            let (resumed, _) = scan_obs(&src, Some(whole.cursor.clone()));
            assert_eq!(resumed.events.len(), 1, "只有新增的那一行");
            let usage = resumed.events[0].usage.expect("续写的是一条用量");
            assert_eq!(
                (usage.input, usage.output, usage.cache_read),
                (40, 30, 10),
                "累计用量从压缩前的状态接着减"
            );
            let _ = std::fs::remove_file(&path);
        }

        /// 两种形态并存（压缩或还原的中途）读原文件 —— 之后被追加的是它。
        #[test]
        fn when_both_forms_exist_the_plain_file_is_read() {
            let (path, src) = temp_source_codex("both", &format!("{}\n", codex_meta("cdx")));
            let zst = compress_in_place(&path);
            std::fs::write(
                &path,
                format!("{}\n{}\n", codex_meta("cdx"), codex_token(100, 20, 50)),
            )
            .unwrap();
            let (obs, _) = scan_obs(&src, None);
            assert_eq!(
                obs.cursor.safe_offset,
                std::fs::metadata(&path).unwrap().len()
            );
            assert!(obs.events.iter().any(|e| e.usage.is_some()), "读的是原文件");
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_file(&zst);
        }

        /// 压缩件读不懂（坏了、不是 zstd）不能当成空文件或「没有新内容」：问不出大小就是 `Stat`
        /// 失败，调用方跳过本文件、不写库。
        #[test]
        fn an_unreadable_compressed_file_is_a_stat_failure() {
            let (path, src) = temp_source_codex("badzst", &format!("{}\n", codex_meta("cdx")));
            let zst = crate::compressed::compressed_path(&path);
            std::fs::write(&zst, b"{\"not\":\"zstd\"}\n").unwrap();
            std::fs::remove_file(&path).unwrap();
            let (obs, _) = scan_obs(&src, None);
            assert!(
                matches!(obs.quality, ParseQuality::Unavailable(ScanFailure::Stat(_))),
                "{:?}",
                obs.quality
            );
            let _ = std::fs::remove_file(&zst);
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

    // ── 预取（StatPrefetch）────────────────────────────────────────────────────────
    //
    // 发行版名故意不存在：预取答案被用上时不会去问它；退回逐个问时一定问不成
    // （Windows 上 wsl.exe 报错，别处是桩）—— 所以「有没有退回去问」从结果上看得出来。

    const NO_SUCH_DISTRO: &str = "svault-test-no-such-distro";
    const WSL_PATH: &str = "/home/u/.claude/projects/p/s.jsonl";

    fn wsl_source() -> SourceRef {
        SourceRef {
            source_type: SourceType::ClaudeCode,
            source_location: SourceLocation::Wsl(NO_SUCH_DISTRO.into()),
            source_mode: SourceMode::AppendLog,
            path: PathBuf::from(WSL_PATH),
            project_root: None,
            artifact_kind: None,
        }
    }

    fn caught_up_cursor() -> crate::cursor::Cursor {
        let mut cursor = crate::cursor::Cursor::new_byte_offset();
        cursor.safe_offset = 100;
        cursor.size = 100;
        cursor.mtime = Some(1_700_000_000);
        cursor
    }

    fn prefetch_with(answer: crate::wsl::StatAnswer) -> super::StatPrefetch {
        let mut prefetch = super::StatPrefetch::default();
        prefetch
            .answers
            .insert((NO_SUCH_DISTRO.into(), WSL_PATH.into()), answer);
        prefetch
    }

    fn scan_with(prefetch: &super::StatPrefetch) -> crate::observation::AppendLogObservation {
        super::scan_append_log_prefetched(
            &wsl_source(),
            Some(caught_up_cursor()),
            None,
            Profile::Full,
            no_roots(),
            crate::deadline::Deadline::unbounded(),
            prefetch,
        )
        .0
    }

    /// 没变化的文件：预取答案就够了，不再起 `wsl.exe`；不给预取时要单独问（这里问不成）。
    #[test]
    fn an_unchanged_wsl_file_is_answered_by_the_prefetch_without_a_wsl_call() {
        use crate::observation::{ParseQuality, ScanFailure};
        // 不断言全进程的 `spawn_count`：并行跑的别的测试也会起 wsl.exe。退回逐个问必然
        // 问不成，所以结果是 `Clean` 本身就证明没有退回去问。
        let obs = scan_with(&prefetch_with(Ok(Some((100, 1_700_000_000)))));
        assert!(
            matches!(obs.quality, ParseQuality::Clean { .. }),
            "{:?}",
            obs.quality
        );
        assert!(obs.events.is_empty());

        let obs = scan_with(&super::StatPrefetch::default());
        assert!(
            matches!(obs.quality, ParseQuality::Unavailable(ScanFailure::Stat(_))),
            "对照失效：没有预取时应当单独去问、并且问不成：{:?}",
            obs.quality
        );
    }

    /// 批量确认原文件不在：不再单独问原文件，只问它的压缩形态（可能被就地压缩了）；
    /// 批量里这一条没问成：单独再问。
    #[test]
    fn a_batch_miss_is_final_but_a_batch_failure_falls_back_to_asking_alone() {
        use crate::observation::{ParseQuality, ScanFailure};
        // 这里问什么都问不成（发行版不存在）：报错里带着的路径就是最后问的那一个。
        // 单独再问原文件的话，`wsl::stat` 先失败，走不到压缩形态那一步。
        let obs = scan_with(&prefetch_with(Ok(None)));
        let ParseQuality::Unavailable(ScanFailure::Stat(why)) = &obs.quality else {
            panic!("{:?}", obs.quality);
        };
        assert!(
            why.contains(".jsonl.zst:"),
            "批量已确认原文件不在，应当只问压缩形态：{why}"
        );

        let batch_error = "batch could not answer this one";
        let obs = scan_with(&prefetch_with(Err(batch_error.into())));
        let ParseQuality::Unavailable(ScanFailure::Stat(why)) = &obs.quality else {
            panic!("{:?}", obs.quality);
        };
        assert!(
            !why.contains(batch_error),
            "没有单独再问，直接把批量的错误当了答案：{why}"
        );
    }

    /// 预取问到：原文件不在，压缩形态的身份是 `id`。
    fn prefetch_compressed(path: &str, id: crate::wsl::IdentityAnswer) -> super::StatPrefetch {
        let mut prefetch = super::StatPrefetch::default();
        prefetch
            .answers
            .insert((NO_SUCH_DISTRO.into(), path.into()), Ok(None));
        prefetch
            .compressed
            .insert((NO_SUCH_DISTRO.into(), path.into()), id);
        prefetch
    }

    fn scan_path(
        path: &str,
        cursor: crate::cursor::Cursor,
        prefetch: &super::StatPrefetch,
    ) -> crate::observation::AppendLogObservation {
        let mut source = wsl_source();
        source.path = PathBuf::from(path);
        super::scan_append_log_prefetched(
            &source,
            Some(cursor),
            None,
            Profile::Full,
            no_roots(),
            crate::deadline::Deadline::unbounded(),
            prefetch,
        )
        .0
    }

    fn remember_len(path: &str, id: crate::wsl::FileIdentity, len: u64) {
        super::compressed_lengths()
            .lock()
            .unwrap()
            .insert((NO_SUCH_DISTRO.into(), format!("{path}.zst")), (id, len));
    }

    const ZST_ID: crate::wsl::FileIdentity = crate::wsl::FileIdentity {
        size: 40,
        mtime: 1_700_000_000,
        ctime: 1_700_000_100,
    };

    /// WSL 压缩件解压后的长度按身份缓存：身份没变 ⇒ 不读文件（退回去读必然读不成，所以 `Clean`
    /// 本身就证明没读）；任一项变了 ⇒ 去读。只变 ctime 的也算（Codex 原地改写过，10-29 真机见过）。
    #[test]
    fn a_compressed_wsl_file_whose_identity_is_unchanged_is_not_read() {
        use crate::observation::{ParseQuality, ScanFailure};
        let path = "/home/u/.codex/sessions/cached.jsonl";
        remember_len(path, ZST_ID, 100);
        let obs = scan_path(
            path,
            caught_up_cursor(),
            &prefetch_compressed(path, Ok(Some(ZST_ID))),
        );
        assert!(
            matches!(obs.quality, ParseQuality::Clean { .. }),
            "{:?}",
            obs.quality
        );
        assert_eq!(
            (obs.cursor.size, obs.cursor.mtime),
            (100, Some(ZST_ID.mtime))
        );

        for changed in [
            crate::wsl::FileIdentity {
                ctime: ZST_ID.ctime + 1,
                ..ZST_ID
            },
            crate::wsl::FileIdentity {
                size: ZST_ID.size + 1,
                ..ZST_ID
            },
            crate::wsl::FileIdentity {
                mtime: ZST_ID.mtime + 1,
                ..ZST_ID
            },
        ] {
            let obs = scan_path(
                path,
                caught_up_cursor(),
                &prefetch_compressed(path, Ok(Some(changed))),
            );
            let ParseQuality::Unavailable(ScanFailure::Stat(why)) = &obs.quality else {
                panic!("身份变了却没去读：{changed:?} {:?}", obs.quality);
            };
            assert!(why.contains(".jsonl.zst:"), "{why}");
        }
    }

    /// 预取确认两种形态都不在 ⇒ 直接报不在，不再去读压缩件。
    #[test]
    fn both_forms_confirmed_missing_is_reported_without_reading() {
        use crate::observation::{ParseQuality, ScanFailure};
        let path = "/home/u/.codex/sessions/gone.jsonl";
        let obs = scan_path(
            path,
            caught_up_cursor(),
            &prefetch_compressed(path, Ok(None)),
        );
        let ParseQuality::Unavailable(ScanFailure::Stat(why)) = &obs.quality else {
            panic!("{:?}", obs.quality);
        };
        assert!(why.contains("nor its compressed form"), "{why}");
        assert!(!why.contains(".jsonl.zst:"), "已确认不在还去读了：{why}");
    }

    /// 长度取自缓存、而这一轮要读字节（游标落后）：补读压缩件 —— 这里读不成，报读失败，
    /// 不当成「没有新内容」。
    #[test]
    fn a_cached_length_still_reads_the_bytes_when_they_are_needed() {
        use crate::observation::{ParseQuality, ScanFailure};
        let path = "/home/u/.codex/sessions/behind.jsonl";
        remember_len(path, ZST_ID, 100);
        let mut cursor = caught_up_cursor();
        cursor.safe_offset = 50;
        let obs = scan_path(path, cursor, &prefetch_compressed(path, Ok(Some(ZST_ID))));
        let ParseQuality::Unavailable(ScanFailure::Read(why)) = &obs.quality else {
            panic!("{:?}", obs.quality);
        };
        assert!(why.contains(".jsonl.zst:"), "{why}");
    }

    /// 要整份读的压缩件只有「身份问到了、长度不在缓存里」的那些；按调用方的顺序分批。
    #[test]
    fn only_compressed_files_without_a_cached_length_are_planned_for_reading() {
        let mk = |name: &str| {
            let mut s = wsl_source();
            s.path = PathBuf::from(format!("/home/u/.codex/sessions/plan-{name}.jsonl"));
            s
        };
        let sources = [
            mk("cached"),
            mk("new"),
            mk("gone"),
            mk("unasked"),
            mk("changed"),
        ];
        let path = |s: &SourceRef| s.path.to_string_lossy().into_owned();
        remember_len(&path(&sources[0]), ZST_ID, 100);
        remember_len(&path(&sources[4]), ZST_ID, 100);
        let mut compressed = std::collections::HashMap::new();
        let key = |s: &SourceRef| (NO_SUCH_DISTRO.to_string(), path(s));
        compressed.insert(key(&sources[0]), Ok(Some(ZST_ID)));
        compressed.insert(key(&sources[1]), Ok(Some(ZST_ID)));
        compressed.insert(key(&sources[2]), Ok(None));
        compressed.insert(
            key(&sources[4]),
            Ok(Some(crate::wsl::FileIdentity {
                ctime: ZST_ID.ctime + 1,
                ..ZST_ID
            })),
        );
        assert_eq!(
            super::plan_compressed_reads(&sources, &compressed),
            vec![(
                NO_SUCH_DISTRO.to_string(),
                vec![
                    format!("{}.zst", path(&sources[1])),
                    format!("{}.zst", path(&sources[4])),
                ]
            )]
        );
    }

    /// 实机：QuotaBar 按规则 2 提的三条判据（2026-10-10）。需 Windows + WSL，且置 `SVAULT_WSL_IT=1`。
    ///
    /// 夹具建在 Windows 临时目录，发行版经 `/mnt/<盘>` 读 —— 不往发行版里写，也不从这边删它的文件。
    /// 单独跑（`--test-threads=1`）：数的是全进程的 `wsl.exe` 启动次数。
    #[test]
    #[cfg(windows)]
    fn prefetch_meets_the_wsl_spawn_budget_it() {
        use crate::observation::{ParseQuality, ScanFailure, SourceChange};
        if std::env::var("SVAULT_WSL_IT").is_err() {
            return;
        }
        let unbounded = crate::deadline::Deadline::unbounded;
        let line = |session: &str, text: &str| format!("{}\n", claude_line(session, text));
        let distro = crate::wsl::list_distros(unbounded())
            .unwrap()
            .into_iter()
            .find(|d| crate::wsl::is_user_distro(d))
            .expect("need a user distro");
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("svault-it-prefetch-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        let win = dir.to_string_lossy().into_owned();
        let (drive, rest) = win.split_once(":\\").expect("drive-letter temp dir");
        let mnt = format!("/mnt/{}/{}", drive.to_lowercase(), rest.replace('\\', "/"));

        let files: Vec<PathBuf> = (0..20).map(|i| dir.join(format!("s{i}.jsonl"))).collect();
        for (i, f) in files.iter().enumerate() {
            let body: String = (0..3)
                .map(|n| line(&format!("s{i}"), &format!("m{n}")))
                .collect();
            std::fs::write(f, body).unwrap();
        }
        let sources: Vec<SourceRef> = (0..files.len())
            .map(|i| SourceRef {
                source_type: SourceType::ClaudeCode,
                source_location: SourceLocation::Wsl(distro.clone()),
                source_mode: SourceMode::AppendLog,
                path: PathBuf::from(format!("{mnt}/s{i}.jsonl")),
                project_root: None,
                artifact_kind: None,
            })
            .collect();
        let scan = |i: usize, cursor: &crate::cursor::Cursor, prefetch: &super::StatPrefetch| {
            super::scan_append_log_prefetched(
                &sources[i],
                Some(cursor.clone()),
                None,
                Profile::Full,
                no_roots(),
                unbounded(),
                prefetch,
            )
            .0
        };

        // 第 1 轮：逐个全读，拿到游标。
        let mut cursors: Vec<_> = sources
            .iter()
            .map(|s| {
                let obs = super::scan_append_log_observed(
                    s,
                    None,
                    None,
                    Profile::Full,
                    no_roots(),
                    unbounded(),
                )
                .0;
                assert_eq!(obs.events.len(), 3, "{:?}", obs.quality);
                obs.cursor
            })
            .collect();

        // 判据 1：20 个来源里 2 个有新增，wsl.exe 启动次数 ≤ 发行版数 + 有变化的来源数。
        append(&files[3], &line("s3", "new"));
        append(&files[7], &line("s7", "new"));
        let before = crate::wsl::spawn_count();
        let prefetch = super::StatPrefetch::for_sources(&sources, unbounded());
        let round: Vec<_> = (0..sources.len())
            .map(|i| scan(i, &cursors[i], &prefetch))
            .collect();
        let spawned = crate::wsl::spawn_count() - before;
        assert!(
            spawned <= 1 + 2,
            "20 个来源、2 个有变化，起了 {spawned} 次 wsl.exe"
        );
        for (i, obs) in round.iter().enumerate() {
            let want = if i == 3 || i == 7 { 1 } else { 0 };
            assert_eq!(obs.events.len(), want, "来源 {i}：{:?}", obs.quality);
        }

        // 判据 2：有变化的来源，预取与逐个问的结果逐字节一致。
        let plain = scan(3, &cursors[3], &super::StatPrefetch::default());
        assert_eq!(
            serde_json::to_value(&plain.events).unwrap(),
            serde_json::to_value(&round[3].events).unwrap()
        );
        assert_eq!(plain.cursor.safe_offset, round[3].cursor.safe_offset);
        for (cursor, obs) in cursors.iter_mut().zip(round) {
            *cursor = obs.cursor;
        }

        // 答案过期：预取之后文件被改短。这一轮读失败、游标不动；下一轮重新预取，认出回退、从头读。
        append(&files[5], &line("s5", "grown"));
        let stale = super::StatPrefetch::for_sources(&sources, unbounded());
        std::fs::write(&files[5], line("s5", "rewritten")).unwrap();
        let obs = scan(5, &cursors[5], &stale);
        assert!(
            matches!(obs.quality, ParseQuality::Unavailable(ScanFailure::Read(_))),
            "{:?}",
            obs.quality
        );
        assert!(obs.events.is_empty());
        assert_eq!(
            obs.cursor.safe_offset, cursors[5].safe_offset,
            "过期答案推进了游标"
        );
        let fresh = super::StatPrefetch::for_sources(&sources, unbounded());
        let obs = scan(5, &cursors[5], &fresh);
        assert_eq!(obs.source_change, SourceChange::RollbackOrRewrite);
        assert_eq!(obs.events.len(), 1, "{:?}", obs.quality);

        std::fs::remove_dir_all(dir).unwrap();
    }

    fn wsl_log(distro: &str, path: &str) -> SourceRef {
        SourceRef {
            source_type: SourceType::ClaudeCode,
            source_location: SourceLocation::Wsl(distro.to_string()),
            source_mode: SourceMode::AppendLog,
            path: PathBuf::from(path),
            project_root: None,
            artifact_kind: None,
        }
    }

    /// 整份读的分批：按调用方顺序、同一发行版接着装、合计超 32 MiB 另起一批；只收 WSL 上
    /// `stat` 到了、大小在 `(0, 8 MiB]` 的 append-log。
    #[test]
    fn whole_reads_are_batched_per_distro_in_caller_order() {
        const MIB: u64 = 1024 * 1024;
        let mut answers = std::collections::HashMap::new();
        let mut put = |distro: &str, path: &str, answer: crate::wsl::StatAnswer| {
            answers.insert((distro.to_string(), path.to_string()), answer);
        };
        for (i, path) in ["/a0", "/a1", "/a2", "/a3", "/a4"].iter().enumerate() {
            put("U", path, Ok(Some((7 * MIB, i as i64))));
        }
        put("D", "/d0", Ok(Some((MIB, 0))));
        put("U", "/big", Ok(Some((8 * MIB + 1, 0))));
        put("U", "/empty", Ok(Some((0, 0))));
        put("U", "/gone", Ok(None));
        put("U", "/failed", Err("no answer".to_string()));
        let mut snapshot = wsl_log("U", "/snap");
        snapshot.source_mode = SourceMode::SnapshotFile;
        put("U", "/snap", Ok(Some((MIB, 0))));
        let mut local = wsl_log("U", "/local");
        local.source_location = SourceLocation::Local;
        let sources: Vec<SourceRef> = vec![
            wsl_log("U", "/a0"),
            wsl_log("D", "/d0"),
            wsl_log("U", "/big"),
            wsl_log("U", "/a1"),
            wsl_log("U", "/empty"),
            wsl_log("U", "/a2"),
            wsl_log("U", "/gone"),
            wsl_log("U", "/a3"),
            wsl_log("U", "/failed"),
            snapshot,
            local,
            wsl_log("U", "/a4"),
            wsl_log("U", "/unknown"),
        ];
        let s = |v: &[&str]| v.iter().map(|p| p.to_string()).collect::<Vec<_>>();
        assert_eq!(
            super::plan_whole_batches(&sources, &answers),
            vec![
                ("U".to_string(), s(&["/a0", "/a1", "/a2", "/a3"])),
                ("D".to_string(), s(&["/d0"])),
                ("U".to_string(), s(&["/a4"])),
            ]
        );
    }

    /// 读到的比 `stat` 时短（之后被截断）不能当成整份：交回调用方逐个读，由短读报错。
    #[test]
    fn a_whole_read_shorter_than_the_stat_is_not_used() {
        assert_eq!(
            super::whole_prefix(b"abcdef".to_vec(), 4),
            Some(b"abcd".to_vec())
        );
        assert_eq!(
            super::whole_prefix(b"abcd".to_vec(), 4),
            Some(b"abcd".to_vec())
        );
        assert_eq!(super::whole_prefix(b"abc".to_vec(), 4), None);
    }

    /// 实机：强制重读时整份读分批 —— 结果与逐个读逐字节一致，`wsl.exe` 次数 = 1 次 stat +
    /// 每批 1 次 + 每个超过单文件上限的文件 1 次（QuotaBar 按规则 2 提的判据，2026-10-12）；
    /// 读之后被截断的文件退回逐个读、照旧报读失败。需 Windows + WSL，且置 `SVAULT_WSL_IT=1`，
    /// 单独跑（`--test-threads=1`）。夹具建在 Windows 临时目录，发行版经 `/mnt/<盘>` 读。
    #[test]
    #[cfg(windows)]
    fn whole_reads_are_batched_and_match_one_by_one_it() {
        use crate::observation::{ParseQuality, ScanFailure};
        if std::env::var("SVAULT_WSL_IT").is_err() {
            return;
        }
        let unbounded = crate::deadline::Deadline::unbounded;
        let distro = crate::wsl::list_distros(unbounded())
            .unwrap()
            .into_iter()
            .find(|d| crate::wsl::is_user_distro(d))
            .expect("need a user distro");
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("svault-it-whole-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        let win = dir.to_string_lossy().into_owned();
        let (drive, rest) = win.split_once(":\\").expect("drive-letter temp dir");
        let mnt = format!("/mnt/{}/{}", drive.to_lowercase(), rest.replace('\\', "/"));
        // 每行约 2 KiB；`mib` 给出文件大小（MiB）。
        let filler = "x".repeat(2000);
        let body = |name: &str, mib: f64| -> String {
            let lines = (mib * 1024.0 * 1024.0 / 2100.0) as usize;
            (0..lines.max(3))
                .map(|n| format!("{}\n", claude_line(name, &format!("{n}-{filler}"))))
                .collect()
        };
        // 顺序即扫描顺序：7 MiB × 5（前四个装一批、第五个另起一批）+ 9 MiB 一个 + 小文件三个。
        let plan: [(&str, f64); 9] = [
            ("m0", 7.0),
            ("t0", 0.0),
            ("m1", 7.0),
            ("m2", 7.0),
            ("t1", 0.0),
            ("big", 9.0),
            ("m3", 7.0),
            ("m4", 7.0),
            ("t2", 0.0),
        ];
        for (name, mib) in plan {
            std::fs::write(dir.join(format!("{name}.jsonl")), body(name, mib)).unwrap();
        }
        let sources: Vec<SourceRef> = plan
            .iter()
            .map(|(name, _)| wsl_log(&distro, &format!("{mnt}/{name}.jsonl")))
            .collect();
        let scan_all = |prefetch: &super::StatPrefetch| -> Vec<_> {
            sources
                .iter()
                .map(|s| {
                    super::scan_append_log_prefetched(
                        s,
                        None,
                        None,
                        Profile::Full,
                        no_roots(),
                        unbounded(),
                        prefetch,
                    )
                    .0
                })
                .collect()
        };

        let before = crate::wsl::spawn_count();
        let one_by_one = scan_all(&super::StatPrefetch::for_sources(&sources, unbounded()));
        let spawned_plain = crate::wsl::spawn_count() - before;

        let before = crate::wsl::spawn_count();
        let prefetch = super::StatPrefetch::for_sources(&sources, unbounded())
            .reading_whole(&sources, unbounded());
        let batched = scan_all(&prefetch);
        let spawned_batched = crate::wsl::spawn_count() - before;

        assert_eq!(
            spawned_plain,
            1 + sources.len(),
            "逐个读：1 次 stat + 每个文件 1 次"
        );
        assert_eq!(spawned_batched, 1 + 2 + 1, "1 次 stat + 2 批 + 1 个大文件");
        for (i, (a, b)) in one_by_one.iter().zip(&batched).enumerate() {
            assert!(!a.events.is_empty(), "来源 {i} 没读到事件：{:?}", a.quality);
            assert_eq!(
                serde_json::to_value(&a.events).unwrap(),
                serde_json::to_value(&b.events).unwrap(),
                "来源 {i}"
            );
            assert_eq!(a.cursor.safe_offset, b.cursor.safe_offset, "来源 {i}");
            assert_eq!(
                format!("{:?}", a.quality),
                format!("{:?}", b.quality),
                "来源 {i}"
            );
        }

        // 规划了整份读、实际却是接着读（调用方带了游标）：整份读到的字节不能拿来顶 `[k, size)`。
        let tail = std::slice::from_ref(&sources[8]);
        let cursor = batched[8].cursor.clone();
        let mut appended = std::fs::OpenOptions::new()
            .append(true)
            .open(dir.join("t2.jsonl"))
            .unwrap();
        std::io::Write::write_all(
            &mut appended,
            format!("{}\n", claude_line("t2", "tail")).as_bytes(),
        )
        .unwrap();
        drop(appended);
        let incremental = |prefetch: &super::StatPrefetch| {
            super::scan_append_log_prefetched(
                &sources[8],
                Some(cursor.clone()),
                None,
                Profile::Full,
                no_roots(),
                unbounded(),
                prefetch,
            )
            .0
        };
        let plain = incremental(&super::StatPrefetch::for_sources(tail, unbounded()));
        let planned = incremental(
            &super::StatPrefetch::for_sources(tail, unbounded()).reading_whole(tail, unbounded()),
        );
        assert_eq!(plain.events.len(), 1, "{:?}", plain.quality);
        assert_eq!(
            serde_json::to_value(&plain.events).unwrap(),
            serde_json::to_value(&planned.events).unwrap(),
            "接着读时用了整份读到的字节"
        );

        // 规划之后、读之前文件被截断：读到的比 stat 短 ⇒ 不用它，逐个读短读报错（与没有预取时一样）。
        let short = std::slice::from_ref(&sources[1]);
        let stale =
            super::StatPrefetch::for_sources(short, unbounded()).reading_whole(short, unbounded());
        std::fs::write(
            dir.join("t0.jsonl"),
            body("t0", 0.0).lines().next().unwrap(),
        )
        .unwrap();
        let obs = super::scan_append_log_prefetched(
            &sources[1],
            None,
            None,
            Profile::Full,
            no_roots(),
            unbounded(),
            &stale,
        )
        .0;
        assert!(
            matches!(obs.quality, ParseQuality::Unavailable(ScanFailure::Read(_))),
            "{:?}",
            obs.quality
        );

        std::fs::remove_dir_all(dir).unwrap();
    }

    /// 实机：发行版里只剩压缩件时按逻辑路径读，结果与读原文件逐字节相同。每轮 `wsl.exe` 次数：
    /// 第一次见到它 3（批量 stat、批量问压缩件身份、一批整份读）；身份没变的稳态 2（不读）；
    /// 被改写后 3（重读）；两种形态都不在 2（stat 失败，不去读）。需 Windows + WSL，且置
    /// `SVAULT_WSL_IT=1`，单独跑（`--test-threads=1`）。夹具建在 Windows 临时目录，发行版经 `/mnt/<盘>` 读。
    #[test]
    #[cfg(windows)]
    fn a_compressed_rollout_reads_the_same_over_wsl_it() {
        use crate::observation::{ParseQuality, ScanFailure};
        if std::env::var("SVAULT_WSL_IT").is_err() {
            return;
        }
        let unbounded = crate::deadline::Deadline::unbounded;
        let distro = crate::wsl::list_distros(unbounded())
            .unwrap()
            .into_iter()
            .find(|d| crate::wsl::is_user_distro(d))
            .expect("need a user distro");
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("svault-it-zst-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        let win = dir.to_string_lossy().into_owned();
        let (drive, rest) = win.split_once(":\\").expect("drive-letter temp dir");
        let mnt = format!("/mnt/{}/{}", drive.to_lowercase(), rest.replace('\\', "/"));
        let body: String = (0..300)
            .map(|n| format!("{}\n", claude_line("z", &format!("line {n}"))))
            .collect();
        std::fs::write(dir.join("z.jsonl"), &body).unwrap();
        let src = wsl_log(&distro, &format!("{mnt}/z.jsonl"));
        let sources = std::slice::from_ref(&src);
        let scan = |prefetch: &super::StatPrefetch| {
            super::scan_append_log_prefetched(
                &src,
                None,
                None,
                Profile::Full,
                no_roots(),
                unbounded(),
                prefetch,
            )
            .0
        };
        let plain = scan(&super::StatPrefetch::for_sources(sources, unbounded()));
        assert_eq!(plain.events.len(), 300, "{:?}", plain.quality);

        let packed = ruzstd::encoding::compress_to_vec(
            body.as_bytes(),
            ruzstd::encoding::CompressionLevel::Fastest,
        );
        std::fs::write(dir.join("z.jsonl.zst"), packed).unwrap();
        std::fs::remove_file(dir.join("z.jsonl")).unwrap();
        // 一轮：新的预取 + 从给的游标扫，数这一轮起了几次 wsl.exe。
        let round = |cursor: Option<crate::cursor::Cursor>| {
            let before = crate::wsl::spawn_count();
            let prefetch = super::StatPrefetch::for_sources(sources, unbounded());
            let obs = super::scan_append_log_prefetched(
                &src,
                cursor,
                None,
                Profile::Full,
                no_roots(),
                unbounded(),
                &prefetch,
            )
            .0;
            (obs, crate::wsl::spawn_count() - before)
        };
        let (compressed, spawned) = round(None);
        assert!(
            matches!(compressed.quality, ParseQuality::Clean { .. }),
            "{:?}",
            compressed.quality
        );
        assert_eq!(
            serde_json::to_value(&compressed.events).unwrap(),
            serde_json::to_value(&plain.events).unwrap()
        );
        assert_eq!(compressed.cursor.safe_offset, plain.cursor.safe_offset);
        assert_eq!(compressed.source_fingerprint, plain.source_fingerprint);
        assert_eq!(spawned, 3, "批量 stat + 批量问压缩件身份 + 一批整份读");

        // 稳态：身份没变，长度取自缓存，不读压缩件。
        let (steady, spawned) = round(Some(compressed.cursor.clone()));
        assert!(
            matches!(steady.quality, ParseQuality::Clean { .. }),
            "{:?}",
            steady.quality
        );
        assert!(steady.events.is_empty());
        assert_eq!(steady.cursor.safe_offset, compressed.cursor.safe_offset);
        assert_eq!(spawned, 2, "稳态只有两次批量问");

        // Codex 改写了它（这里多一行）：身份变了，重读，只出新的那一行。
        let longer = format!("{body}{}\n", claude_line("z", "line 300"));
        let repacked = ruzstd::encoding::compress_to_vec(
            longer.as_bytes(),
            ruzstd::encoding::CompressionLevel::Fastest,
        );
        std::fs::write(dir.join("z.jsonl.zst"), repacked).unwrap();
        let (rewritten, spawned) = round(Some(steady.cursor.clone()));
        assert!(
            matches!(rewritten.quality, ParseQuality::Clean { .. }),
            "{:?}",
            rewritten.quality
        );
        assert_eq!(rewritten.events.len(), 1);
        assert_eq!(rewritten.cursor.safe_offset, longer.len() as u64);
        assert_eq!(spawned, 3, "身份变了：两次批量问 + 一批整份读");

        std::fs::remove_file(dir.join("z.jsonl.zst")).unwrap();
        let (gone, spawned) = round(Some(rewritten.cursor.clone()));
        assert!(
            matches!(
                gone.quality,
                ParseQuality::Unavailable(ScanFailure::Stat(_))
            ),
            "{:?}",
            gone.quality
        );
        assert_eq!(spawned, 2, "两种形态都确认不在：不再去读");
        std::fs::remove_dir_all(dir).unwrap();
    }

    // ── 快照预读（SnapshotPrefetch）：同样用不存在的发行版，退回逐个读必然读不成。─────────

    const SNAPSHOT_PATH: &str = "/home/u/.claude/CLAUDE.md";

    fn wsl_snapshot() -> SourceRef {
        SourceRef {
            source_type: SourceType::ClaudeCode,
            source_location: SourceLocation::Wsl(NO_SUCH_DISTRO.into()),
            source_mode: SourceMode::SnapshotFile,
            path: PathBuf::from(SNAPSHOT_PATH),
            project_root: None,
            artifact_kind: Some("instruction".into()),
        }
    }

    fn snapshot_with(
        answer: crate::wsl::ReadAnswer,
        cursor: Option<crate::cursor::Cursor>,
    ) -> crate::cursor::ScanResult {
        let mut prefetch = super::SnapshotPrefetch::default();
        prefetch
            .reads
            .insert((NO_SUCH_DISTRO.into(), SNAPSHOT_PATH.into()), answer);
        super::scan_snapshot_prefetched(&wsl_snapshot(), cursor, Profile::Full, &prefetch)
    }

    fn file(bytes: &[u8]) -> crate::wsl::ReadAnswer {
        Ok(Some(crate::wsl::FileRead {
            bytes: bytes.to_vec(),
            mtime: 1_700_000_000,
        }))
    }

    /// 批里读到了：发一版快照，内容与 mtime 都取自批，不再单独读、单独 stat；
    /// 内容没变时不发。
    #[test]
    fn a_prefetched_snapshot_is_scanned_without_a_wsl_call() {
        let res = snapshot_with(file(b"# rules\n"), None);
        assert_eq!(res.status, ScanStatus::Ok, "{:?}", res.report.warnings);
        assert_eq!(res.events.len(), 1);
        assert_eq!(res.events[0].modified_at.as_deref(), Some("1700000000"));

        let res = snapshot_with(file(b"# rules\n"), Some(res.cursor_out));
        assert_eq!(res.status, ScanStatus::Ok);
        assert!(res.events.is_empty(), "内容没变却又发了一版");

        let control = super::scan_snapshot_prefetched(
            &wsl_snapshot(),
            None,
            Profile::Full,
            &super::SnapshotPrefetch::default(),
        );
        assert_eq!(
            control.status,
            ScanStatus::Error,
            "对照失效：没有预读时应当单独去读、并且读不成"
        );
    }

    /// 批里确认不在：照逐个读时的说法报「消失」；批里没读成：单独再读；不是 UTF-8：与逐个读一样算没读成。
    #[test]
    fn snapshot_batch_answers_keep_the_per_file_semantics() {
        let res = snapshot_with(Ok(None), None);
        assert_eq!(res.status, ScanStatus::Error);
        assert!(
            res.report
                .warnings
                .iter()
                .any(|w| w.contains("disappeared")),
            "{:?}",
            res.report.warnings
        );

        let batch_error = "batch could not read this one";
        let res = snapshot_with(Err(batch_error.into()), None);
        assert_eq!(res.status, ScanStatus::Error);
        assert!(
            !res.report.warnings.iter().any(|w| w.contains(batch_error)),
            "没有单独再读，直接把批量的错误当了答案：{:?}",
            res.report.warnings
        );

        // 逐个读时不是 UTF-8 就读失败 —— 哪怕内容与上一版相同（不走到比对那一步）。
        let not_utf8 = [0xff, 0xfe, b'x'];
        let mut cursor = crate::cursor::Cursor::new_fingerprint();
        cursor.content_hash = Some(format!(
            "sha256:{:x}",
            <sha2::Sha256 as sha2::Digest>::digest(not_utf8)
        ));
        let res = snapshot_with(file(&not_utf8), Some(cursor));
        assert_eq!(
            res.status,
            ScanStatus::Error,
            "批量读放过了不是 UTF-8 的内容"
        );
        assert!(res.events.is_empty());
    }

    /// 实机：快照预读每个发行版一次 `wsl.exe`，结果与逐个读一致；超过单文件上限的照旧单独读。
    /// 需 Windows + WSL，且置 `SVAULT_WSL_IT=1`，单独跑（`--test-threads=1`）。
    #[test]
    #[cfg(windows)]
    fn snapshot_prefetch_reads_a_distro_in_one_call_it() {
        if std::env::var("SVAULT_WSL_IT").is_err() {
            return;
        }
        let unbounded = crate::deadline::Deadline::unbounded;
        let distro = crate::wsl::list_distros(unbounded())
            .unwrap()
            .into_iter()
            .find(|d| crate::wsl::is_user_distro(d))
            .expect("need a user distro");
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("svault-it-snapshots-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        let win = dir.to_string_lossy().into_owned();
        let (drive, rest) = win.split_once(":\\").expect("drive-letter temp dir");
        let mnt = format!("/mnt/{}/{}", drive.to_lowercase(), rest.replace('\\', "/"));

        let mut names: Vec<String> = (0..10).map(|i| format!("m{i}.md")).collect();
        for (i, name) in names.iter().enumerate() {
            std::fs::write(dir.join(name), format!("# memory {i}\n\tline\n")).unwrap();
        }
        let big = "big.md".to_string();
        let line = "x".repeat(1023) + "\n";
        std::fs::write(
            dir.join(&big),
            line.repeat((crate::wsl::READ_MANY_MAX_EACH / 1024 + 1) as usize),
        )
        .unwrap();
        names.push(big);
        let sources: Vec<SourceRef> = names
            .iter()
            .map(|name| SourceRef {
                source_type: SourceType::ClaudeCode,
                source_location: SourceLocation::Wsl(distro.clone()),
                source_mode: SourceMode::SnapshotFile,
                path: PathBuf::from(format!("{mnt}/{name}")),
                project_root: None,
                artifact_kind: Some("memory".into()),
            })
            .collect();
        // `observed_at` 是扫描那一刻的钟，两次扫描必然不同；其余字段都要一致。
        let comparable = |res: &crate::cursor::ScanResult| {
            let mut v = serde_json::to_value(&res.events).unwrap();
            for e in v.as_array_mut().unwrap() {
                e.as_object_mut().unwrap().remove("observed_at");
            }
            (res.status, v, res.cursor_out.content_hash.clone())
        };

        let before = crate::wsl::spawn_count();
        let prefetch = super::SnapshotPrefetch::for_sources(&sources, unbounded());
        let batched: Vec<_> = sources
            .iter()
            .map(|s| super::scan_snapshot_prefetched(s, None, Profile::Full, &prefetch))
            .collect();
        let spawned = crate::wsl::spawn_count() - before;
        // 1 次批量 + 超限那一个单独读 1 次、单独 stat 1 次。
        assert!(spawned <= 3, "11 个快照起了 {spawned} 次 wsl.exe");
        assert!(
            matches!(prefetch.get(&sources[10]), Some(Err(_))),
            "超过单文件上限的文件进了批"
        );

        for (s, res) in sources.iter().zip(&batched) {
            assert_eq!(res.status, ScanStatus::Ok, "{:?}", res.report.warnings);
            let plain = super::scan_source(s, None, Profile::Full, no_roots(), unbounded());
            assert_eq!(comparable(res), comparable(&plain), "{}", s.path.display());
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
