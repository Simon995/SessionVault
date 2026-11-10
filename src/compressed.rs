//! 被写入方就地压缩过的 append-log（Codex 的 `rollout-*.jsonl.zst`）。
//!
//! 🔴 **压缩只换了存放形态，没换来源。** `X.jsonl` 与 `X.jsonl.zst` 是同一个逻辑来源：
//! 发现时报逻辑路径 `X.jsonl`；大小、偏移、指纹都按**解压后的字节**算 —— 与压缩前的原文
//! 逐字节相同，所以游标、上一版指纹、总库里的 `source_path` 在两种形态之间照样接得上。
//! 按物理文件报的话，压缩一次就是「旧来源消失 + 新来源出现」：消费方删掉旧来源的数据，
//! 再把同样的事件以新路径写一遍（2026-10-15 本机 88 个文件被压，QuotaBar 一轮刷新就
//! prune 掉了它们的历史）。
//!
//! Codex 那边怎么做（openai/codex main，`codex-rs/rollout/src/compression.rs`，2026-10-20 查）：
//! 只压 mtime 超过 7 天的冷文件；先写同目录临时文件、不覆盖地原子改名成 `.zst`、再删原文件，
//! 元数据从原文件复制；续聊要追加时反过来，原子地还原出 `.jsonl` 再删 `.zst`。所以两种形态
//! 会短暂并存、内容相同；并存时读 `.jsonl` —— 之后被追加的是它。
//! 本机实测（2026-10-20）：88 个全是单帧、帧头带内容长度、不带校验和；解压后合计 675 MB。
//!
//! 解压用纯 Rust 的 `ruzstd`（不给默认构建加 C 依赖）。多帧、可跳过帧、不带内容长度的帧
//! 都按 RFC 8878 处理，不只认 Codex 今天写的那一种。

use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// 压缩形态在逻辑路径后面加的后缀。
pub const SUFFIX: &str = ".zst";

/// 逻辑路径的压缩形态：`X.jsonl` → `X.jsonl.zst`。
pub fn compressed_path(logical: &Path) -> PathBuf {
    let mut s = logical.as_os_str().to_owned();
    s.push(SUFFIX);
    PathBuf::from(s)
}

/// 压缩形态对应的逻辑路径：`X.jsonl.zst` → `X.jsonl`。不以 `.zst` 结尾 ⇒ `None`；
/// 文件名不是 UTF-8 也是 `None`（Codex 的文件名恒为 ASCII，调用方据此跳过并告警）。
pub fn logical_path(path: &Path) -> Option<PathBuf> {
    let name = path.file_name()?.to_str()?;
    let stem = name.strip_suffix(SUFFIX)?;
    (!stem.is_empty()).then(|| path.with_file_name(stem))
}

const ZSTD_MAGIC: u32 = 0xFD2F_B528;

fn is_skippable(magic: u32) -> bool {
    (0x184D_2A50..=0x184D_2A5F).contains(&magic)
}

fn invalid(what: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what.into())
}

/// 一个数据帧的帧头（RFC 8878 §3.1.1.1），只取本模块要的。`raw` 是连魔数在内的原样字节：
/// 解压时要原样交还给解码器。
struct FrameHeader {
    content_size: Option<u64>,
    checksum: bool,
    window_size: u64,
    dictionary_id: u64,
    raw: Vec<u8>,
}

enum Next {
    /// 流在帧与帧之间结束 —— 完整。
    End,
    /// 可跳过帧，载荷这么多字节。
    Skippable(u32),
    Data(FrameHeader),
}

fn read_exact_into<R: Read + ?Sized>(r: &mut R, raw: &mut Vec<u8>, n: usize) -> io::Result<u64> {
    let start = raw.len();
    raw.resize(start + n, 0);
    r.read_exact(&mut raw[start..])?;
    Ok(raw[start..]
        .iter()
        .rev()
        .fold(0u64, |acc, b| (acc << 8) | u64::from(*b)))
}

fn read_next<R: Read + ?Sized>(r: &mut R) -> io::Result<Next> {
    let mut magic = [0u8; 4];
    let mut got = 0;
    while got < magic.len() {
        match r.read(&mut magic[got..]) {
            Ok(0) if got == 0 => return Ok(Next::End),
            Ok(0) => return Err(invalid("zstd stream ends inside a frame magic number")),
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    let magic_num = u32::from_le_bytes(magic);
    let mut raw = magic.to_vec();
    if is_skippable(magic_num) {
        let len = read_exact_into(r, &mut raw, 4)?;
        return Ok(Next::Skippable(len as u32));
    }
    if magic_num != ZSTD_MAGIC {
        return Err(invalid(format!(
            "not a zstd frame (magic {magic_num:#010x})"
        )));
    }
    let descriptor = read_exact_into(r, &mut raw, 1)? as u8;
    let fcs_flag = descriptor >> 6;
    let single_segment = descriptor & 0x20 != 0;
    if descriptor & 0x08 != 0 {
        return Err(invalid("zstd frame header has the reserved bit set"));
    }
    let checksum = descriptor & 0x04 != 0;
    let window = if !single_segment {
        let descriptor = read_exact_into(r, &mut raw, 1)? as u8;
        let base = 1u64 << (10 + (descriptor >> 3));
        Some(base + (base / 8) * u64::from(descriptor & 7))
    } else {
        None
    };
    let dict_bytes = [0, 1, 2, 4][usize::from(descriptor & 0x03)];
    let dictionary_id = read_exact_into(r, &mut raw, dict_bytes)?;
    let content_size = match (fcs_flag, single_segment) {
        (0, false) => None,
        (0, true) => Some(read_exact_into(r, &mut raw, 1)?),
        (1, _) => Some(read_exact_into(r, &mut raw, 2)? + 256),
        (2, _) => Some(read_exact_into(r, &mut raw, 4)?),
        _ => Some(read_exact_into(r, &mut raw, 8)?),
    };
    Ok(Next::Data(FrameHeader {
        content_size,
        checksum,
        window_size: window
            .or(content_size)
            .ok_or_else(|| invalid("zstd window size is unavailable"))?,
        dictionary_id,
        raw,
    }))
}

/// 往前跳 `n` 字节，不许跳过文件尾（`seek` 越过文件尾不报错，截断的文件会被当成完整的）。
fn skip<R: Seek + ?Sized>(r: &mut R, n: u64, len: u64) -> io::Result<()> {
    let at = r.stream_position()?;
    if at.checked_add(n).is_none_or(|end| end > len) {
        return Err(invalid("zstd stream is truncated"));
    }
    r.seek(SeekFrom::Current(n as i64))?;
    Ok(())
}

/// 解一个数据帧（帧头已读出），解出的字节依次交给 `sink`；`sink` 返回 `false` 就停。
/// 返回 `true` = 整帧解完。解完的帧要对得上帧头里的内容长度与校验和 —— 对不上是坏文件，
/// 不是「少了一点」。
fn decode_frame<R: Read + ?Sized>(
    r: &mut R,
    header: &FrameHeader,
    sink: &mut dyn FnMut(&[u8]) -> bool,
) -> io::Result<bool> {
    let source = io::Cursor::new(&header.raw).chain(r);
    let mut decoder = ruzstd::decoding::StreamingDecoder::new(source)
        .map_err(|e| invalid(format!("zstd frame header: {e}")))?;
    let mut buf = vec![0u8; 64 * 1024];
    let mut produced = 0u64;
    loop {
        let n = decoder.read(&mut buf)?;
        if n == 0 {
            break;
        }
        produced += n as u64;
        if !sink(&buf[..n]) {
            return Ok(false);
        }
    }
    if let Some(expected) = header.content_size {
        if produced != expected {
            return Err(invalid(format!(
                "zstd frame decoded to {produced} bytes, header says {expected}"
            )));
        }
    }
    if header.checksum {
        let frame = &decoder.decoder;
        if frame.get_checksum_from_data() != frame.get_calculated_checksum() {
            return Err(invalid("zstd frame checksum mismatch"));
        }
    }
    Ok(true)
}

#[cfg(feature = "dsh-experimental")]
#[derive(Debug)]
pub(crate) struct CheckedFrame {
    pub physical_start: usize,
    pub physical_end: usize,
    pub decoded: Vec<u8>,
}

#[cfg(feature = "dsh-experimental")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FrameCheckFailure {
    Incomplete,
    Unverified,
    ChecksumAbsent,
    DictionaryRequired,
    UnsupportedFrame,
    BudgetExceeded,
}

/// 完整验证每一帧后才交出字节与边界；沿用上面的唯一帧头/解码实现。
#[cfg(feature = "dsh-experimental")]
pub(crate) fn checked_frames(
    bytes: &[u8],
    decoded_limit: u64,
) -> Result<Vec<CheckedFrame>, FrameCheckFailure> {
    use FrameCheckFailure as F;
    let mut source = io::Cursor::new(bytes);
    let mut frames = Vec::new();
    let mut produced = 0u64;
    while source.position() < bytes.len() as u64 {
        let start = source.position() as usize;
        let remaining = &bytes[start..];
        let magic = ZSTD_MAGIC.to_le_bytes();
        if remaining.len() < magic.len() && magic.starts_with(remaining) {
            return Err(F::Incomplete);
        }
        let header = match read_next(&mut source).map_err(|e| {
            if e.kind() == io::ErrorKind::UnexpectedEof {
                F::Incomplete
            } else {
                F::Unverified
            }
        })? {
            Next::Data(header) => header,
            Next::Skippable(_) => return Err(F::UnsupportedFrame),
            Next::End => break,
        };
        if !header.checksum {
            return Err(F::ChecksumAbsent);
        }
        if header.dictionary_id != 0 {
            return Err(F::DictionaryRequired);
        }
        let left = decoded_limit - produced;
        if header.window_size > decoded_limit || header.content_size.is_some_and(|n| n > left) {
            return Err(F::BudgetExceeded);
        }
        let mut decoded = Vec::new();
        let mut exceeded = false;
        let finished = decode_frame(&mut source, &header, &mut |chunk| {
            if chunk.len() as u64 > left - decoded.len() as u64 {
                exceeded = true;
                return false;
            }
            decoded.extend_from_slice(chunk);
            true
        })
        .map_err(|_| F::Unverified)?;
        if exceeded || !finished {
            return Err(F::BudgetExceeded);
        }
        produced += decoded.len() as u64;
        frames.push(CheckedFrame {
            physical_start: start,
            physical_end: source.position() as usize,
            decoded,
        });
    }
    Ok(frames)
}

/// 逐帧解压整个流，解出的字节依次交给 `sink`，`sink` 返回 `false` 就停。
fn decode_stream<R: Read>(mut r: R, mut sink: impl FnMut(&[u8]) -> bool) -> io::Result<()> {
    loop {
        match read_next(&mut r)? {
            Next::End => return Ok(()),
            Next::Skippable(n) => {
                let skipped = io::copy(&mut (&mut r).take(u64::from(n)), &mut io::sink())?;
                if skipped != u64::from(n) {
                    return Err(invalid("zstd stream is truncated"));
                }
            }
            Next::Data(header) => {
                if !decode_frame(&mut r, &header, &mut sink)? {
                    return Ok(());
                }
            }
        }
    }
}

/// 解压后的总长度 —— 逻辑来源的「文件大小」。
///
/// 每轮扫描都要问一次（没变化的来源只 stat 不读），所以**不解压**：逐帧读帧头，带内容长度的
/// 帧按块头跳到帧尾（Codex 写的都带）；不带的帧才解压计数。截断、魔数不对 ⇒ `Err`。
pub fn decoded_len<R: Read + Seek + ?Sized>(r: &mut R) -> io::Result<u64> {
    let len = r.seek(SeekFrom::End(0))?;
    r.seek(SeekFrom::Start(0))?;
    let mut total = 0u64;
    loop {
        let frame = match read_next(r)? {
            Next::End => return Ok(total),
            Next::Skippable(n) => {
                skip(r, u64::from(n), len)?;
                continue;
            }
            Next::Data(header) => header,
        };
        let size = match frame.content_size {
            Some(size) => {
                skip_blocks(r, len)?;
                if frame.checksum {
                    skip(r, 4, len)?;
                }
                size
            }
            None => {
                let mut counted = 0u64;
                decode_frame(r, &frame, &mut |b| {
                    counted += b.len() as u64;
                    true
                })?;
                counted
            }
        };
        total = total
            .checked_add(size)
            .ok_or_else(|| invalid("zstd content sizes overflow"))?;
    }
}

/// 按块头跳过一个帧的全部块（RFC 8878 §3.1.1.2）。
fn skip_blocks<R: Read + Seek + ?Sized>(r: &mut R, len: u64) -> io::Result<()> {
    loop {
        let mut h = [0u8; 3];
        r.read_exact(&mut h)?;
        let h = u32::from(h[0]) | u32::from(h[1]) << 8 | u32::from(h[2]) << 16;
        let last = h & 1 == 1;
        let size = u64::from(h >> 3);
        let body = match (h >> 1) & 3 {
            0 | 2 => size, // Raw / Compressed：块里就是这么多字节
            1 => 1,        // RLE：一个字节，重复 size 次
            _ => return Err(invalid("zstd block has the reserved type")),
        };
        skip(r, body, len)?;
        if last {
            return Ok(());
        }
    }
}

/// 解压出 `[start, end)`。从头解（zstd 不能从中间开始），`start` 之前的丢掉，够了就停。
/// 解出来不足 `end` ⇒ `Err` —— 与读普通文件时的短读同一个语义：按 `stat` 的大小推进游标
/// 却少了字节，会跳过数据。
pub fn decode_range<R: Read>(r: R, start: u64, end: u64) -> io::Result<Vec<u8>> {
    if end <= start {
        return Ok(Vec::new());
    }
    let want = end - start;
    let mut out = Vec::with_capacity(want.min(64 * 1024 * 1024) as usize);
    let mut pos = 0u64;
    decode_stream(r, |b| {
        let (b_start, b_end) = (pos, pos + b.len() as u64);
        pos = b_end;
        let (lo, hi) = (start.max(b_start), end.min(b_end));
        if lo < hi {
            out.extend_from_slice(&b[(lo - b_start) as usize..(hi - b_start) as usize]);
        }
        pos < end
    })?;
    if (out.len() as u64) < want {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            format!("decoded content ends at {pos}, wanted up to {end}"),
        ));
    }
    Ok(out)
}

/// 解压全文。
pub fn decode_all<R: Read>(r: R) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    decode_stream(r, |b| {
        out.extend_from_slice(b);
        true
    })?;
    Ok(out)
}

/// 从任意字节流 `R` 按块切分 UTF-8 行并依次调用 `on_line`（不含行尾 `\n` / `\r\n`）；
/// `on_line` 返回 `false` 时立即停止读取。
fn feed_lines(
    mut stream: impl FnMut(&mut dyn FnMut(&[u8]) -> bool) -> io::Result<()>,
    mut on_line: impl FnMut(&str) -> bool,
) -> io::Result<()> {
    let mut pending = Vec::new();
    let mut stopped = false;
    let mut utf8_err: Option<io::Error> = None;

    stream(&mut |chunk| {
        pending.extend_from_slice(chunk);
        let mut start = 0usize;
        while let Some(rel) = pending[start..].iter().position(|&b| b == b'\n') {
            let end = start + rel;
            let mut line_bytes = &pending[start..end];
            if line_bytes.last() == Some(&b'\r') {
                line_bytes = &line_bytes[..line_bytes.len() - 1];
            }
            match std::str::from_utf8(line_bytes) {
                Ok(line) => {
                    if !on_line(line) {
                        stopped = true;
                        return false;
                    }
                }
                Err(e) => {
                    utf8_err = Some(invalid(format!("invalid utf-8 in line: {e}")));
                    return false;
                }
            }
            start = end + 1;
        }
        if start > 0 {
            pending.drain(..start);
        }
        true
    })?;

    if let Some(err) = utf8_err {
        return Err(err);
    }
    if !stopped && !pending.is_empty() {
        if pending.last() == Some(&b'\r') {
            pending.pop();
        }
        let line = std::str::from_utf8(&pending)
            .map_err(|e| invalid(format!("invalid utf-8 in trailing line: {e}")))?;
        let _ = on_line(line);
    }
    Ok(())
}

/// 流式解压并逐行交给 `on_line`（不含行尾 `\n` / `\r\n`）；`on_line` 返回 `false` 即刻停止解压。
///
/// 早停时不继续解压当前帧剩余块或后续帧（专为 `session-origin` 在大文件首行命中 `session_meta`
/// 即停设计，G.3）。若在停止前遇到流损坏或非 UTF-8 字节，则返回 `Err`。
pub fn decode_lines<R: Read>(r: R, on_line: impl FnMut(&str) -> bool) -> io::Result<()> {
    let mut r = r;
    feed_lines(|sink| decode_stream(&mut r, |b| sink(b)), on_line)
}

/// 流式读取未压缩 UTF-8 文本流并逐行交给 `on_line`；`on_line` 返回 `false` 即刻停止读取。
fn read_plain_lines<R: Read + ?Sized>(
    r: &mut R,
    on_line: impl FnMut(&str) -> bool,
) -> io::Result<()> {
    feed_lines(
        |sink| {
            let mut buf = [0u8; 64 * 1024];
            loop {
                let n = match r.read(&mut buf) {
                    Ok(n) => n,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e),
                };
                if n == 0 || !sink(&buf[..n]) {
                    return Ok(());
                }
            }
        },
        on_line,
    )
}

/// 读一个**本机**逻辑来源的全文：`path` 在就读它，不在才读压缩形态。
/// 三态与 [`crate::probe::read_bytes`] 相同：两种形态都不在才是 `Absent`。
pub fn read_local(path: &Path) -> crate::probe::Probed<Vec<u8>> {
    use crate::probe::Probed;
    match crate::probe::read_bytes(path, None) {
        Probed::Absent => crate::probe::with_reader(&compressed_path(path), |r| decode_all(r)),
        other => other,
    }
}

/// 流式逐行读一个**本机**逻辑来源：`path` 在就按普通文件流式逐行读，不在才流式解压 `<path>.zst` 逐行读。
/// `on_line` 返回 `false` 时立即停止读取 / 解压（G.3）。
/// 三态与 [`read_local`] 相同：两种形态都不在才是 `Absent`。
pub fn scan_lines_local(
    path: &Path,
    mut on_line: impl FnMut(&str) -> bool,
) -> crate::probe::Probed<()> {
    use crate::probe::Probed;
    match crate::probe::with_reader(path, |r| read_plain_lines(r, &mut on_line)) {
        Probed::Absent => {
            crate::probe::with_reader(&compressed_path(path), |r| decode_lines(r, &mut on_line))
        }
        other => other,
    }
}

/// 读一个 **WSL** 逻辑来源的全文，语义同 [`read_local`]：`Ok(None)` = 两种形态都不在。
/// 原文件照旧一次 `cat`（[`crate::wsl::read_file_at`]，要求 UTF-8）；压缩件按字节读回来再解压。
pub fn read_wsl(
    distro: &str,
    path: &str,
    deadline: crate::deadline::Deadline,
) -> Result<Option<Vec<u8>>, String> {
    match crate::wsl::read_file_at(distro, path, deadline)? {
        Some(text) => Ok(Some(text.into_bytes())),
        None => {
            let zst = format!("{path}{SUFFIX}");
            match crate::wsl::read_whole(distro, &zst, deadline)? {
                Some(read) => decode_all(read.bytes.as_slice())
                    .map(Some)
                    .map_err(|e| format!("{distro}:{zst}: {e}")),
                None => Ok(None),
            }
        }
    }
}

/// 流式逐行读一个 **WSL** 逻辑来源（G.3），语义同 [`scan_lines_local`]：
/// `Ok(false)` = 两种形态都不在；`Ok(true)` = 存在且已完成逐行扫描（含 `on_line` 早停）。
pub fn scan_lines_wsl(
    distro: &str,
    path: &str,
    deadline: crate::deadline::Deadline,
    mut on_line: impl FnMut(&str) -> bool,
) -> Result<bool, String> {
    match crate::wsl::read_file_at(distro, path, deadline)? {
        Some(text) => {
            for line in text.lines() {
                if !on_line(line) {
                    break;
                }
            }
            Ok(true)
        }
        None => {
            let zst = format!("{path}{SUFFIX}");
            match crate::wsl::read_whole(distro, &zst, deadline)? {
                Some(read) => decode_lines(read.bytes.as_slice(), &mut on_line)
                    .map(|()| true)
                    .map_err(|e| format!("{distro}:{zst}: {e}")),
                None => Ok(false),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(data: &[u8]) -> Vec<u8> {
        ruzstd::encoding::compress_to_vec(data, ruzstd::encoding::CompressionLevel::Fastest)
    }

    fn skippable(payload: &[u8]) -> Vec<u8> {
        let mut v = 0x184D_2A53u32.to_le_bytes().to_vec();
        v.extend((payload.len() as u32).to_le_bytes());
        v.extend(payload);
        v
    }

    /// libzstd（与 Codex 同一个库）写的四种形态，由 `scripts/make-zst-fixtures.py` 生成：
    /// 与 Codex 同样的单帧带长度、带校验和、流式写入不带长度、两帧中间夹一个可跳过帧。
    /// 解压结果都必须是同一份原文，长度不解压也得答对。
    #[test]
    fn libzstd_frames_decode_to_the_original() {
        let plain = include_bytes!("../tests/fixtures/zst/rollout.jsonl");
        for (name, bytes) in [
            (
                "like-codex",
                &include_bytes!("../tests/fixtures/zst/like-codex.jsonl.zst")[..],
            ),
            (
                "checksum",
                &include_bytes!("../tests/fixtures/zst/checksum.jsonl.zst")[..],
            ),
            (
                "no-size",
                &include_bytes!("../tests/fixtures/zst/no-size.jsonl.zst")[..],
            ),
            (
                "two-frames",
                &include_bytes!("../tests/fixtures/zst/two-frames.jsonl.zst")[..],
            ),
        ] {
            assert_eq!(decode_all(bytes).unwrap(), plain, "{name}");
            let len = decoded_len(&mut io::Cursor::new(bytes)).unwrap();
            assert_eq!(len, plain.len() as u64, "{name}");
            assert_eq!(
                decode_range(bytes, 100, 300).unwrap(),
                &plain[100..300],
                "{name}"
            );
        }
    }

    #[cfg(feature = "dsh-experimental")]
    #[test]
    fn checked_frames_bind_boundaries_and_refuse_every_torn_suffix() {
        let frame = include_bytes!("../tests/fixtures/zst/checksum.jsonl.zst");
        let plain = include_bytes!("../tests/fixtures/zst/rollout.jsonl");
        let budget = plain.len() as u64 * 2;
        let bytes = [frame.as_slice(), frame.as_slice()].concat();
        let frames = checked_frames(&bytes, budget).unwrap();
        assert_eq!(frames.len(), 2);
        for (index, found) in frames.iter().enumerate() {
            assert_eq!(found.physical_start, index * frame.len());
            assert_eq!(found.physical_end, (index + 1) * frame.len());
            assert_eq!(found.decoded, plain);
        }
        for cut in 1..frame.len() {
            let torn = [frame.as_slice(), &frame[..cut]].concat();
            assert!(checked_frames(&torn, budget).is_err(), "cut {cut}");
        }
        for byte in 1..=4 {
            let mut corrupt = bytes.clone();
            let at = corrupt.len() - byte;
            corrupt[at] ^= 1;
            assert_eq!(
                checked_frames(&corrupt, budget).unwrap_err(),
                FrameCheckFailure::Unverified
            );
        }
        assert_eq!(
            checked_frames(&bytes, plain.len() as u64).unwrap_err(),
            FrameCheckFailure::BudgetExceeded
        );
        assert_eq!(
            checked_frames(
                include_bytes!("../tests/fixtures/zst/like-codex.jsonl.zst"),
                budget
            )
            .unwrap_err(),
            FrameCheckFailure::ChecksumAbsent
        );
    }

    /// 长度是不解压、照帧头答的；帧头说的与解出来的对不上，读全文时必须报错 —— 否则 `stat`
    /// 报的大小与读到的字节差一截，游标按前者推进。
    #[test]
    fn a_frame_whose_header_lies_about_its_size_is_an_error() {
        let plain = include_bytes!("../tests/fixtures/zst/rollout.jsonl");
        let mut bytes = include_bytes!("../tests/fixtures/zst/like-codex.jsonl.zst").to_vec();
        // libzstd 给这份写的是单段帧 + 2 字节内容长度（值 - 256），紧跟在描述符后面。
        assert_eq!(bytes[4], 0x60, "夹具的帧头形状变了，这条用例要跟着改");
        let fcs = u16::from_le_bytes([bytes[5], bytes[6]]) + 1;
        bytes[5..7].copy_from_slice(&fcs.to_le_bytes());
        assert_eq!(
            decoded_len(&mut io::Cursor::new(&bytes)).unwrap(),
            plain.len() as u64 + 1
        );
        assert!(decode_all(bytes.as_slice()).is_err());
    }

    /// 多帧 + 可跳过帧：长度是各帧之和，区间可以跨帧。
    #[test]
    fn frames_and_skippable_frames_concatenate() {
        let (a, b) = (b"{\"a\":1}\n".repeat(50), b"{\"b\":2}\n".repeat(70));
        let mut stream = frame(&a);
        stream.extend(skippable(b"seek table or anything"));
        stream.extend(frame(&b));
        let whole = [a.clone(), b.clone()].concat();
        assert_eq!(decode_all(stream.as_slice()).unwrap(), whole);
        assert_eq!(
            decoded_len(&mut io::Cursor::new(&stream)).unwrap(),
            whole.len() as u64
        );
        let (lo, hi) = (a.len() as u64 - 5, a.len() as u64 + 5);
        assert_eq!(
            decode_range(stream.as_slice(), lo, hi).unwrap(),
            &whole[lo as usize..hi as usize]
        );
    }

    /// 截断的文件不能被当成短一点的完整文件：长度与解压都报错，要的区间超出全文也报错。
    #[test]
    fn a_truncated_stream_is_an_error_not_a_shorter_file() {
        let plain = include_bytes!("../tests/fixtures/zst/rollout.jsonl");
        let full = include_bytes!("../tests/fixtures/zst/like-codex.jsonl.zst");
        let cut = &full[..full.len() - 3];
        assert!(decoded_len(&mut io::Cursor::new(cut)).is_err());
        assert!(decode_all(cut).is_err());
        let past_end = plain.len() as u64 + 1;
        assert!(decode_range(&full[..], 0, past_end).is_err());

        let mut with_cut_skippable = frame(b"x\n");
        with_cut_skippable.extend(&skippable(b"0123456789")[..12]);
        assert!(decoded_len(&mut io::Cursor::new(&with_cut_skippable)).is_err());
        assert!(decode_all(with_cut_skippable.as_slice()).is_err());
    }

    #[test]
    fn not_zstd_is_an_error() {
        let text = b"{\"type\":\"session_meta\"}\n";
        assert!(decoded_len(&mut io::Cursor::new(text)).is_err());
        assert!(decode_all(&text[..]).is_err());
    }

    #[test]
    fn paths_map_both_ways() {
        let logical = Path::new("sessions").join("rollout-x.jsonl");
        let zst = compressed_path(&logical);
        assert_eq!(zst, Path::new("sessions").join("rollout-x.jsonl.zst"));
        assert_eq!(logical_path(&zst), Some(logical.clone()));
        assert_eq!(logical_path(&logical), None);
        assert_eq!(logical_path(Path::new(".zst")), None);
    }
}
