//! 扫描器**观察到了什么**（ADR-051 §1）—— 事实，不含「为什么扫」，也不含「该怎么办」。
//!
//! # 它替换掉的东西
//!
//! `ScanStatus { Ok, Partial, Error }` 三个变体扛着四种含义：
//!
//! | 实际情况 | 旧变体 | 该怎么办 |
//! | --- | --- | --- |
//! | 完整行全部有效，尾部半行留下轮 | `Partial` | 正常成功，推进游标 |
//! | 一次性全读遇坏行，好行已保留 | `Partial` | 降级，但数据可用 |
//! | 增量遇坏行，整批主动丢弃 | `Error` | 冻结起点，下轮重读 |
//! | 没读成（stat/read 失败） | `Error` | 跳过本文件，别写库 |
//!
//! 前两个共用 `Partial`、后两个共用 `Error`，于是**消费方只能靠别的东西猜回来**。
//! 实测 QuotaBar 的 `svault_bridge` 正是这么做的：`items_skipped > 0` 分开前两者，
//! `warnings.first().starts_with("stat failed")` 分开后两者 —— **靠一条日志文案的前缀
//! 决定要不要写库**。那行文案改一个字，判断就静默失效。
//!
//! 🔴 **信息在类型里丢了，就会在别处被字符串重建。** 这是本模块存在的全部理由。
//!
//! # 边界：只服务 append-log
//!
//! 快照路径（`SourceMode::SnapshotFile`）**不用**这套。它的失败态是「无效 UTF-8」
//! 与「未实现的 source mode」—— 塞进 [`ParseQuality::Unavailable`] 会让「内容无效」
//! 伪装成「读不到」，而那两件事的处置完全不同（前者重试无用，后者下轮可能就好了）。

use crate::cursor::{Cursor, ScanStatus};
use crate::rawevent::RawEvent;

/// 一次 append-log 扫描的完整观察。
#[derive(Debug, Clone)]
pub struct AppendLogObservation {
    /// 🔴 **源变化是观察，不是意图。** 由扫描器 stat 之后检出（size 变小或 mtime 倒退），
    /// 调用方事先并不知道 —— 所以它不能是入参。
    pub source_change: SourceChange,
    pub quality: ParseQuality,
    pub events: Vec<RawEvent>,
    pub cursor: Cursor,
    /// 全读时算出的源内容指纹。
    ///
    /// 🔴 **它存在是为了识别「同尺寸原地重写」。** 回退检测只看 `size` 与 `mtime`，
    /// 一次保留大小与时间戳的原地重写**检不出来**：UI 索引若因别的原因走了全读，
    /// 会把它当 `ReplaceFile`，而总库走 `Append` 把新 seq 当重复丢弃 ——
    /// **两层从此不一致，且没有任何东西会说出来。**
    ///
    /// `None` = 这一轮是增量，没读全文，**算不出**指纹。不是「内容没变」。
    pub source_fingerprint: Option<SourceFingerprint>,
}

/// 源文件相对上次游标发生了什么。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceChange {
    /// 只增长（或没动）—— 增量可以接着上次的位置读。
    Appended,
    /// 变短、时间戳倒退，或指纹与上次不符 ⇒ 之前读到的字节已经不作数。
    RollbackOrRewrite,
}

/// 这一批**字节**读得怎么样。
///
/// ⚠️ 四个变体互斥且完备，覆盖上表四行。加变体时先问：它是不是真的与这四个都不同 ——
/// 这个类型的价值就在于消费方可以穷举匹配，多一个含糊的变体就退回到「猜」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseQuality {
    /// 完整行全部有效。**半截尾行属于这里** —— `deferred_tail_bytes > 0` 是 append-log
    /// 的常态（写入方正写到一半），不是降级。
    Clean { deferred_tail_bytes: u64 },
    /// 有坏行，好行已保留（一次性全读）。**降级，不是失败** —— 数据可用。
    ///
    /// 只在全读时可能出现：没有下一轮、不存在重复发，丢弃只会平白少数据。
    Degraded(ParseDiagnostics),
    /// 字节读到了，但**我们主动拒绝这一批**（增量遇坏行）。
    ///
    /// 保留事件又冻结游标会让下轮把同一批好行再发一遍，所以整批丢弃 + 冻结起点。
    /// **这是设计决定，不是故障** —— 与 `Unavailable` 分开，正因为处置不同：
    /// 这里游标要冻在原处，那里本文件整个跳过。
    RejectedPoisonLine(ParseDiagnostics),
    /// 没读成，手上没有可信事件。
    Unavailable(ScanFailure),
}

/// 坏行的证据。**计数与首条文案都要有** —— 只给计数，日志里说不出是哪一行坏了；
/// 只给文案，说不出坏了多少。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseDiagnostics {
    pub skipped_lines: u64,
    pub first_warning: Option<String>,
}

/// 为什么没读成。
///
/// 🔴 **两个变体必须分开，因为调用方的处置相反。** `Stat` 是「文件在扫描期间消失/
/// 够不着」⇒ 跳过本文件、**不写库**（写一行 error 状态会把一次瞬时失败变成一条
/// 持久的坏记录）；`Read` 是「文件在但读不出来」⇒ 写 error 状态、冻结游标、下轮重读。
///
/// 此前两者都是 `ScanStatus::Error`，消费方靠 `warnings.first().starts_with("stat failed")`
/// 区分 —— 一条日志文案的前缀决定要不要写库。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanFailure {
    /// `stat` 失败：文件消失、权限、WSL 够不着。
    Stat(String),
    /// 读取失败：短读、IO 错误、WSL read failed。
    Read(String),
}

impl ScanFailure {
    pub fn message(&self) -> &str {
        match self {
            ScanFailure::Stat(m) | ScanFailure::Read(m) => m,
        }
    }
}

/// 源内容指纹 —— 不透明，只能比较。
///
/// 认**内容**而非长度/mtime：识别同尺寸原地重写正是它存在的理由，而那两样恰好都不变。
///
/// 沿用快照路径已在用的 `sha256:` 形式（`scan.rs::scan_snapshot_file`）与同一个
/// `sha2` 依赖 —— 一个仓里两种指纹格式，早晚会有人把它们拿去比较。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SourceFingerprint {
    hash: String,
    /// 这个哈希**覆盖到文件的第几个字节** —— 少了它，指纹就说不清自己在描述什么。
    ///
    /// 🔴 第一版只存哈希，于是「全读 N 字节 → 追加到 M 字节 → 再全读」时，
    /// 拿 `hash(M)` 与 `hash(N)` 比，**必然不等**，一次纯追加被判成原地重写 ⇒
    /// `Rollback` ⇒ 总库开一代**按设计永不自动回收**的旧版本。而强制全读并不
    /// 罕见（`force_refresh` / 归属过期 / 欠账）。
    covered_len: u64,
}

impl SourceFingerprint {
    pub fn of(bytes: &[u8]) -> Self {
        use sha2::{Digest, Sha256};
        Self {
            hash: format!("sha256:{:x}", Sha256::digest(bytes)),
            covered_len: bytes.len() as u64,
        }
    }

    /// 这个指纹覆盖到第几个字节。
    pub fn covered_len(&self) -> u64 {
        self.covered_len
    }

    /// 当前全文与上一版指纹比对：**只比它覆盖过的那段前缀**。
    ///
    /// - 前缀一致 ⇒ 只发生过追加（或什么都没变）—— **不是重写**；
    /// - 前缀不一致 ⇒ 那段字节被改过 ⇒ 重写（同尺寸与「重写且变长」都在内）；
    /// - 当前比上一版还短 ⇒ 无法比对，交给 stat 那层的回退判定（它已经认得变短）。
    ///
    /// 比全文哈希强：全文哈希只在尺寸恰好相同时有意义，而这里「重写且变长」
    /// 也认得出。
    pub fn prefix_differs_from(&self, current_full: &[u8]) -> bool {
        let n = self.covered_len as usize;
        if current_full.len() < n {
            return false; // 变短 —— 不由指纹这一层判
        }
        Self::of(&current_full[..n]).hash != self.hash
    }

    pub fn as_str(&self) -> &str {
        &self.hash
    }

    /// 从持久化的 `(哈希, 覆盖长度)` 还原。**两者缺一不可** —— 只存哈希
    /// 就退回第一版那个「纯追加被判成重写」的缺陷。
    pub fn from_stored(hash: impl Into<String>, covered_len: u64) -> Self {
        Self {
            hash: hash.into(),
            covered_len,
        }
    }
}

impl AppendLogObservation {
    /// 这一批事件能不能用。
    ///
    /// `Degraded` 是 `true`：好行已保留，丢掉它们只会平白少数据。
    /// `RejectedPoisonLine` 是 `false`：我们**主动**丢了这一批（`events` 必然为空）。
    pub fn events_are_usable(&self) -> bool {
        matches!(
            self.quality,
            ParseQuality::Clean { .. } | ParseQuality::Degraded(_)
        )
    }

    /// 降级成旧的 [`ScanResult`] —— **move，不 clone**。
    ///
    /// 观察是权威，`ScanResult` 是它的有损投影（status 三态压掉了四种含义）。
    /// 两者绝不并存于同一个值里：events 存两份会让「改了一份忘了另一份」成为可能，
    /// 而那正是本模块要消灭的那类缺陷。
    pub fn into_scan_result(
        self,
        report: crate::report::SourceReport,
    ) -> crate::cursor::ScanResult {
        crate::cursor::ScanResult {
            status: ScanStatus::from(&self.quality),
            events: self.events,
            cursor_out: self.cursor,
            report,
        }
    }

    /// 本文件这一轮该不该写库。
    ///
    /// 🔴 只有 `Unavailable(Stat)` 是「跳过、当没发生」；其余（含读失败与毒行）都要
    /// 落一行状态，否则下轮 `list_agent_file_offsets` 会复用旧游标当成功。
    pub fn should_record(&self) -> bool {
        !matches!(
            self.quality,
            ParseQuality::Unavailable(ScanFailure::Stat(_))
        )
    }
}

/// `ScanStatus` 由 quality **派生**，不是并列存储的第二个真相。
///
/// 保留它是因为快照路径仍在用；append-log 路径上任何人想读 status，读到的都必然
/// 与 quality 一致 —— 两者不可能漂开。
impl From<&ParseQuality> for ScanStatus {
    fn from(q: &ParseQuality) -> Self {
        match q {
            ParseQuality::Clean {
                deferred_tail_bytes: 0,
            } => ScanStatus::Ok,
            // 半行 pending 沿用旧的 `Partial`：既有消费方（含 QuotaBar 的 `skipped==0`
            // 分支）依赖它表示「正常增量」。
            ParseQuality::Clean { .. } => ScanStatus::Partial,
            ParseQuality::Degraded(_) => ScanStatus::Partial,
            ParseQuality::RejectedPoisonLine(_) | ParseQuality::Unavailable(_) => ScanStatus::Error,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diag() -> ParseDiagnostics {
        ParseDiagnostics {
            skipped_lines: 1,
            first_warning: Some("bad json at line 3".into()),
        }
    }

    /// 🔴 旧的三变体把这四种情况压成两组，于是消费方只能靠 `items_skipped` 与
    /// 一条日志文案的前缀猜回来。这条钉住四者互不相同。
    #[test]
    fn the_four_qualities_are_distinguishable() {
        let all = [
            ParseQuality::Clean {
                deferred_tail_bytes: 0,
            },
            ParseQuality::Degraded(diag()),
            ParseQuality::RejectedPoisonLine(diag()),
            ParseQuality::Unavailable(ScanFailure::Read("io".into())),
        ];
        for (i, a) in all.iter().enumerate() {
            for (j, b) in all.iter().enumerate() {
                if i != j {
                    assert_ne!(a, b, "第 {i} 与第 {j} 种情况必须能分开");
                }
            }
        }
    }

    /// 🔴 「读不成」的两种处置相反 —— 这正是它们必须是两个变体的理由。
    #[test]
    fn a_vanished_file_is_skipped_but_an_unreadable_one_is_recorded() {
        let vanished = obs(ParseQuality::Unavailable(ScanFailure::Stat("gone".into())));
        let unreadable = obs(ParseQuality::Unavailable(ScanFailure::Read("short".into())));
        assert!(
            !vanished.should_record(),
            "文件消失要当没发生 —— 写一行 error 会把瞬时失败变成持久的坏记录"
        );
        assert!(
            unreadable.should_record(),
            "读失败必须落状态，否则下轮会复用旧游标当成功"
        );
    }

    /// 降级保留数据，主动拒绝不保留 —— 两者都不是 `Clean`，但可用性相反。
    #[test]
    fn degraded_keeps_its_events_while_a_rejected_batch_does_not() {
        assert!(obs(ParseQuality::Degraded(diag())).events_are_usable());
        assert!(!obs(ParseQuality::RejectedPoisonLine(diag())).events_are_usable());
    }

    /// 半截尾行是 append-log 的**常态**，不是降级。
    #[test]
    fn a_deferred_tail_is_clean_not_degraded() {
        let o = obs(ParseQuality::Clean {
            deferred_tail_bytes: 42,
        });
        assert!(o.events_are_usable() && o.should_record());
        assert_eq!(ScanStatus::from(&o.quality), ScanStatus::Partial);
    }

    /// `ScanStatus` 是投影，不是第二个真相 —— 四种 quality 都映射得出来。
    #[test]
    fn the_legacy_status_is_derived_from_quality() {
        assert_eq!(
            ScanStatus::from(&ParseQuality::Clean {
                deferred_tail_bytes: 0
            }),
            ScanStatus::Ok
        );
        assert_eq!(
            ScanStatus::from(&ParseQuality::Degraded(diag())),
            ScanStatus::Partial
        );
        assert_eq!(
            ScanStatus::from(&ParseQuality::RejectedPoisonLine(diag())),
            ScanStatus::Error
        );
        assert_eq!(
            ScanStatus::from(&ParseQuality::Unavailable(ScanFailure::Stat("x".into()))),
            ScanStatus::Error
        );
    }

    /// 🔴 指纹要认**内容**，不认长度 —— 识别同尺寸原地重写正是它存在的理由。
    #[test]
    fn the_fingerprint_sees_a_same_sized_rewrite() {
        let a = SourceFingerprint::of(b"{\"a\":1}\n");
        let b = SourceFingerprint::of(b"{\"a\":2}\n");
        assert_eq!(a.as_str().len(), b.as_str().len(), "两份输入同尺寸");
        assert_ne!(a, b, "同尺寸不同内容必须是不同指纹");
        assert_eq!(a, SourceFingerprint::of(b"{\"a\":1}\n"), "同内容同指纹");
    }

    fn obs(quality: ParseQuality) -> AppendLogObservation {
        AppendLogObservation {
            source_change: SourceChange::Appended,
            quality,
            events: Vec::new(),
            cursor: Cursor::new_byte_offset(),
            source_fingerprint: None,
        }
    }
}
