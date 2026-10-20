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
    if !single_segment {
        read_exact_into(r, &mut raw, 1)?; // Window_Descriptor
    }
    let dict_bytes = [0, 1, 2, 4][usize::from(descriptor & 0x03)];
    read_exact_into(r, &mut raw, dict_bytes)?;
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

/// 读一个**本机**逻辑来源的全文：`path` 在就读它，不在才读压缩形态。
/// 三态与 [`crate::probe::read_bytes`] 相同：两种形态都不在才是 `Absent`。
pub fn read_local(path: &Path) -> crate::probe::Probed<Vec<u8>> {
    use crate::probe::Probed;
    match crate::probe::read_bytes(path, None) {
        Probed::Absent => crate::probe::with_reader(&compressed_path(path), |r| decode_all(r)),
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
