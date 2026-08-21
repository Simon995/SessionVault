//! 一次扫描提交的规划（ADR-051 §2）—— **纯函数，四个决定一次算出**。
//!
//! # 🔴 它为什么在这个仓（2026-08-21 从 QuotaBar 移入）
//!
//! 它原本在 `QuotaBar/src-tauri/src/scan_plan.rs`，而它的**全部依赖都是本仓的类型**
//! （`observation` + `store::Projection`），一个宿主符号都没有。也就是说它一直是
//! 内核的一部分，只是写在了消费者那边。
//!
//! 逼出这次移动的是**即将出现的第二个写者**：`svault scan-all` 要能把扫到的事件写进
//! 总库（TumeFlow task #44，尚在评审中，**本提交不含它**），而它要回答的是同样的
//! 四个问题。留在 QuotaBar 就意味着 CLI 这侧要照抄一份 —— 而这份东西的每一格
//! 都是实测撞出来的（`degraded_and_empty` 那格是 780 条既有测试抓的，
//! `has_prior` 那格是「没有前代可取代的空代」），照抄必然漂，
//! 而漂开之后**两个写者对同一份字节做出不同的投影决定，且没有任何东西会报错**。
//!
//! ⚠️ 移动**不改变**「内核不做下游投影」（§14）：本模块不执行任何投影，
//! 它只回答「该怎么投」。执行仍在各消费者自己那边，且 `IndexAction` /
//! `SyncTransition` 说的是**消费者自己的物化层**（QuotaBar 的 UI 索引、
//! TumeFlow 的分库），本仓一行都不碰它们。
//!
//! ⚠️ 四个决定**必须一次算出**，别按消费者「只用得上其中两个」拆开 ——
//! 拆开正是本类型存在要防的那件事（见下）。用不上的那两个忽略即可，
//! 忽略不会造出非法组合，而拆开会。
//!
//! # 为什么是一个函数而不是四段 if
//!
//! 一次提交要对四件事各做一个决定：UI 索引怎么写、总库怎么写、游标推不推、同步
//! 目标变不变。它们**不是独立的** —— 比如「索引整代替换」必须配「总库开新代」，
//! 分开决定就会出现「索引换了、总库追加」这种谁都没打算要的组合。
//!
//! 本仓已经栽过一次：修「全读失败别用空投影覆盖旧数据」时用一个 `scan_ok: bool`
//! 表达扫描状态，而真实状态有四种（读完 / 读完但有坏行 / 没读成 / 没轮到）。
//! 于是「好行 + 坏行」那格落进增量分支，旧 facts 与从 seq 0 重建的新 facts 合并、
//! 撞主键、事务回滚 —— **一个坏行让索引此后每轮刷新都失败**。
//!
//! ⇒ [`CommitPlan`] 字段私有、唯一构造入口是 [`CommitPlan::plan`]：非法组合
//! **构造不出来**。
//!
//! # 优先级（观察压过原因）
//!
//! ```text
//! 观测到源变化(RollbackOrRewrite)
//!   > PARSER_STALE | ATTRIBUTION_STALE
//!   > INITIAL | FORCE
//!   > 增量（无 reason）
//! ```
//!
//! 🔴 **第一条是承重的。** 磁盘上那段字节已经不存在了，前一个源版本是它的唯一
//! 副本（SessionVault `store.rs`：`Rollback` 的旧版本**永不自动回收**）。把它当
//! `Reparse` 处理，等于允许把唯一副本当成「可再生的更差解析」回收掉。

use crate::observation::{AppendLogObservation, ParseDiagnostics, ParseQuality, SourceChange};
use crate::store::Projection;

/// 这一轮**为什么**要扫它 —— 由调度器给，**可以同时成立多个**。
///
/// 手写位标志而不是引 `bitflags`：四个位不值得给两个仓各加一个依赖（SessionVault
/// 是公开仓，每加一个依赖都是一笔要还的账）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanReasons(u8);

impl ScanReasons {
    /// 没有任何理由 —— 例行增量。
    pub const NONE: Self = Self(0);
    /// 新文件 / 首次回填。
    pub const INITIAL: Self = Self(1 << 0);
    /// 用户显式刷新。
    pub const FORCE: Self = Self(1 << 1);
    /// `PARSER_REVISION` 提升 ⇒ 同一份字节要用新解析器重出。
    pub const PARSER_STALE: Self = Self(1 << 2);
    /// 注册表长出覆盖它的新根 ⇒ 同一份字节要重算归属。
    pub const ATTRIBUTION_STALE: Self = Self(1 << 3);
    /// 两个物化层里至少有一层还没确认目标版本（ADR-051 §3 的欠账）。
    ///
    /// 🔴 **它必须是一个 reason，不能只是调用点的一个 `if`。** 我第一次接欠账时
    /// 把它直接加进了 `was_full`，而 planner 不知道 —— 于是索引走增量（旧 facts +
    /// 从 seq 0 重建的新 facts 合并）当场撞主键。**两个真相源，一个加了条件另一个
    /// 没有**，正是这个类型要消灭的那件事。测试立刻抓住了。
    pub const SYNC_DEBT: Self = Self(1 << 4);

    pub fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// 需要**重出整代**的理由（同一份字节、不同产物）。
    ///
    /// `pub` 是给调用方决定「这一轮从哪里开始读」用的：`Reparse` 要取代**一整代**，
    /// 拿增量的尾巴去取代会把当前投影换成只剩尾巴的那一份。调用方因此必须在扫描
    /// **之前**就知道该不该全读 —— 而那个判断必须与 planner 用的是**同一个谓词**，
    /// 否则就是「两个真相源，一个加了条件另一个没有」（见 `SYNC_DEBT` 的注释，
    /// 那正是它记下的缺陷）。
    pub fn wants_reparse(self) -> bool {
        self.0 & (Self::PARSER_STALE.0 | Self::ATTRIBUTION_STALE.0) != 0
    }

    /// 需要**整代替换索引**、但总库内容不变的理由。
    ///
    /// `pub` 的理由同 [`Self::wants_reparse`]。
    pub fn wants_full_read(self) -> bool {
        self.0 & (Self::INITIAL.0 | Self::FORCE.0 | Self::SYNC_DEBT.0) != 0
    }
}

impl std::ops::BitOr for ScanReasons {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl std::ops::BitOrAssign for ScanReasons {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

/// UI 索引这一轮怎么写。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexAction {
    /// 整代替换这个文件的 facts（先删后插，同一事务）。
    ReplaceFile,
    /// 增量追加。
    AppendFacts,
    /// 一个字都不动。
    Preserve,
}

/// 总库这一轮怎么写。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreAction {
    /// 带投影模式提交。
    Project(Projection),
    /// 一个字都不动（当前投影仍然是对的）。
    Preserve,
}

/// 游标推不推。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorAction {
    Advance,
    /// 冻在原处 —— 下轮从同一个偏移重读。
    Freeze,
}

/// 同步目标变不变（§3）。token 由调用方按 `desired` 造，本层只决定「要不要推进」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncTransition {
    /// 推进到新目标版本：先落 `desired`，提交后两层各自 ack。
    Advance,
    /// 目标不变 —— 旧投影仍然是当前的。
    Hold,
}

/// 这份投影的解析质量 —— **与同步无关**（§7 三维正交）。
///
/// `Degraded` 与 `RejectedPoisonLine` 都**不是欠账**：前者已同步只是质量降级，
/// 后者旧投影仍是当前的。但两者都**必须可见** —— 界面/日志要说得出「这个来源
/// 卡在某个偏移」，否则一个永久坏行会安静地让某个文件停在原地。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QualityState {
    Clean,
    Degraded(ParseDiagnostics),
    RejectedPoisonLine(ParseDiagnostics),
    /// 没读成 —— 新鲜度未知，两层仍一致。
    Unknown,
}

impl QualityState {
    /// 对外报这一格用的稳定短名。
    ///
    /// 🔴 **这四个串是持久化的**（QuotaBar 写进 `record_sync_outcome` 的 `quality`
    /// 列，svault CLI 吐进 NDJSON）。换个 `Debug` derive 或改一个字，就会悄悄改掉
    /// 历史记录的含义、并让按旧值筛选的查询静默漏掉数据。
    ///
    /// 之所以在这里而不是各消费者各写一个 `match`：两个消费者报同一件事，
    /// 而两张手写映射表**漂开时没有任何东西会报错**。
    pub fn key(&self) -> &'static str {
        match self {
            Self::Clean => "clean",
            Self::Degraded(_) => "degraded",
            Self::RejectedPoisonLine(_) => "poison_line",
            Self::Unknown => "unknown",
        }
    }

    /// 坏行的首条文案（`Degraded` / `RejectedPoisonLine` 才有）。
    ///
    /// 与 [`Self::key`] 配套：只给短名说不出「坏在哪一行」，只给文案说不出「多少条」
    /// —— `ParseDiagnostics` 那条「计数与首条文案都要有」在报告面同样成立。
    pub fn detail(&self) -> Option<&str> {
        match self {
            Self::Degraded(d) | Self::RejectedPoisonLine(d) => d.first_warning.as_deref(),
            Self::Clean | Self::Unknown => None,
        }
    }
}

/// 一次提交的四个决定 + 质量。**字段私有：非法组合构造不出来。**
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitPlan {
    index: IndexAction,
    store: StoreAction,
    cursor: CursorAction,
    sync: SyncTransition,
    quality: QualityState,
}

impl CommitPlan {
    /// 唯一构造入口（I1 的落地形式）。
    ///
    /// `has_prior` = 总库里已经有这个来源的投影。**它决定 `Rollback` 是否有意义**：
    /// 第一次见到一个文件时即使观察报了源变化（比如游标存在而投影不存在，两层
    /// 本来就不一致），也没有旧代可回退 —— 那时是 `Append`，不是 `Rollback`。
    pub fn plan(obs: &AppendLogObservation, reasons: ScanReasons, has_prior: bool) -> Self {
        // ── 先处理「这一批不能用」的两格：它们与原因无关 ──────────────
        //
        // 🔴 `Unavailable` 与 `RejectedPoisonLine` 都是全 Preserve + Freeze，但
        // **质量不同**：前者「没读成，新鲜度未知」，后者「读到了，我们主动拒绝」。
        // 压成同一个 quality 就等于把「网络/权限问题」与「数据里有坏行」说成一回事，
        // 而用户能做的事完全不同（等一等 vs 去看那一行）。
        // 🔴 **降级到一个好行都没有 = 这个文件我们完全读不懂，不是「它变空了」。**
        //
        // `Degraded` 的语义是「有坏行，**好行已保留**」—— 整代替换的前提是它确实产出
        // 了事件。一个文件从「两条好行」变成「全是坏行」时，整代替换就成了**用空的
        // 替换非空的**：一次解析失败当场抹掉真实数据，而这不会报任何错。
        //
        // ⚠️ 与「新解析器合法地产出零事件」（`Clean` + 空批）必须分开：那时整代替换
        // 是对的。分辨它们的是 quality，不是批的长度 —— 这正是本仓那条
        // 「『跑完了但产出为空』与『I/O 没跑完』在批的长度上一模一样」的第三种形态。
        //
        // 这一格是 780 条既有测试抓出来的：我照 ADR 的决策表写，而决策表默认
        // `Degraded` 总有好行。
        let degraded_and_empty =
            matches!(obs.quality, ParseQuality::Degraded(_)) && obs.events.is_empty();
        if degraded_and_empty {
            if let ParseQuality::Degraded(d) = &obs.quality {
                return Self {
                    index: IndexAction::Preserve,
                    store: StoreAction::Preserve,
                    cursor: CursorAction::Freeze,
                    sync: SyncTransition::Hold,
                    quality: QualityState::Degraded(d.clone()),
                };
            }
        }

        match &obs.quality {
            ParseQuality::Unavailable(_) => {
                return Self {
                    index: IndexAction::Preserve,
                    store: StoreAction::Preserve,
                    cursor: CursorAction::Freeze,
                    sync: SyncTransition::Hold,
                    quality: QualityState::Unknown,
                };
            }
            ParseQuality::RejectedPoisonLine(d) => {
                return Self {
                    index: IndexAction::Preserve,
                    store: StoreAction::Preserve,
                    cursor: CursorAction::Freeze,
                    sync: SyncTransition::Hold,
                    quality: QualityState::RejectedPoisonLine(d.clone()),
                };
            }
            _ => {}
        }

        let quality = match &obs.quality {
            ParseQuality::Degraded(d) => QualityState::Degraded(d.clone()),
            _ => QualityState::Clean,
        };

        // ── 观察压过原因 ────────────────────────────────────────────────
        //
        // 🔴 **`Rollback` 与 `Reparse` 由观察决定，不由原因决定。**
        // 磁盘上那段字节已经不存在了，而 `Rollback` 的旧版本按设计永不自动回收 ——
        // 它是那段内容的唯一副本。当成 `Reparse` 处理等于允许把唯一副本当作
        // 「可再生的更差解析」回收掉。
        let rewritten = obs.source_change == SourceChange::RollbackOrRewrite;
        let projection = if rewritten && has_prior {
            Projection::Rollback
        } else if reasons.wants_reparse() && has_prior {
            // 同一份字节、更好的解析器/更全的注册表 ⇒ 取代被超越的那一代。
            Projection::Reparse
        } else {
            // 首次、强制刷新、例行增量都是追加：总库按 seq 去重，重放天然幂等。
            Projection::Append
        };

        // 🔴 **`Degraded` 必须整代替换，绝不与旧投影合并。**
        // 两批都从 seq 0 起，合并必然撞 `(provider, location, source_path, seq)` 主键
        // —— 事务回滚，此后每轮刷新都失败。
        let full_read = rewritten
            || reasons.wants_reparse()
            || reasons.wants_full_read()
            || matches!(quality, QualityState::Degraded(_));

        Self {
            index: if full_read {
                IndexAction::ReplaceFile
            } else {
                IndexAction::AppendFacts
            },
            store: StoreAction::Project(projection),
            cursor: CursorAction::Advance,
            sync: SyncTransition::Advance,
            quality,
        }
    }

    pub fn index(&self) -> IndexAction {
        self.index
    }
    pub fn store(&self) -> StoreAction {
        self.store
    }
    pub fn cursor(&self) -> CursorAction {
        self.cursor
    }
    pub fn sync(&self) -> SyncTransition {
        self.sync
    }
    pub fn quality(&self) -> &QualityState {
        &self.quality
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cursor::Cursor;
    use crate::observation::ScanFailure;

    fn diag() -> ParseDiagnostics {
        ParseDiagnostics {
            skipped_lines: 1,
            first_warning: Some("bad json".into()),
        }
    }

    /// 一次**没有产出事件**的观察。
    ///
    /// ⚠️ 对 `Degraded` 而言这是一个**特殊格子**（「全是坏行」），不是默认情形 ——
    /// 想测「有好行的降级」要用 [`obs_with_events`]。
    fn obs(quality: ParseQuality, change: SourceChange) -> AppendLogObservation {
        AppendLogObservation {
            source_change: change,
            quality,
            events: Vec::new(),
            cursor: Cursor::new_byte_offset(),
            source_fingerprint: None,
        }
    }

    fn one_event() -> crate::rawevent::RawEvent {
        use crate::rawevent::*;
        RawEvent {
            schema_version: SCHEMA_VERSION,
            source_type: SourceType::ClaudeCode,
            source_location: SourceLocation::Local,
            source_path: "/logs/a.jsonl".into(),
            source_session_id: "s1".into(),
            seq: 0,
            source_mode: SourceMode::AppendLog,
            cwd: None,
            project_root: None,
            project_root_source: None,
            workspace_location: None,
            event_type: EventType::Message,
            actor: Some(Actor::User),
            occurred_at: None,
            time_confidence: TimeConfidence::Low,
            model: None,
            effort: None,
            usage: None,
            content: None,
            event_key: None,
            parent_ref: None,
            content_hash: None,
            artifact_kind: None,
            observed_at: None,
            message_id: None,
            request_id: None,
        }
    }

    fn obs_with_events(quality: ParseQuality, change: SourceChange) -> AppendLogObservation {
        let mut o = obs(quality, change);
        o.events = vec![one_event()];
        o
    }

    fn clean() -> ParseQuality {
        ParseQuality::Clean {
            deferred_tail_bytes: 0,
        }
    }

    // ── 决策表逐格（观察 × 原因）──────────────────────────────────────

    #[test]
    fn a_plain_incremental_scan_appends() {
        let p = CommitPlan::plan(
            &obs(clean(), SourceChange::Appended),
            ScanReasons::NONE,
            true,
        );
        assert_eq!(p.index(), IndexAction::AppendFacts);
        assert_eq!(p.store(), StoreAction::Project(Projection::Append));
        assert_eq!(p.cursor(), CursorAction::Advance);
        assert_eq!(p.quality(), &QualityState::Clean);
    }

    /// 强制/首次要整代换索引，但总库仍是 `Append`（按 seq 去重，幂等）。
    #[test]
    fn a_forced_full_read_replaces_the_index_but_still_appends_to_the_store() {
        for r in [ScanReasons::FORCE, ScanReasons::INITIAL] {
            let p = CommitPlan::plan(&obs(clean(), SourceChange::Appended), r, true);
            assert_eq!(p.index(), IndexAction::ReplaceFile, "{r:?}");
            assert_eq!(p.store(), StoreAction::Project(Projection::Append), "{r:?}");
        }
    }

    #[test]
    fn a_stale_parser_or_attribution_reparses() {
        for r in [ScanReasons::PARSER_STALE, ScanReasons::ATTRIBUTION_STALE] {
            let p = CommitPlan::plan(&obs(clean(), SourceChange::Appended), r, true);
            assert_eq!(p.index(), IndexAction::ReplaceFile, "{r:?}");
            assert_eq!(
                p.store(),
                StoreAction::Project(Projection::Reparse),
                "{r:?}"
            );
        }
    }

    /// 🔴 **承重规则：`Rollback` 由观察决定，不由原因决定。**
    ///
    /// 磁盘上那段字节已经没了，而 `Rollback` 的旧版本永不自动回收 —— 它是唯一副本。
    /// 当成 `Reparse` 等于允许把唯一副本当「可再生的更差解析」回收掉。
    #[test]
    fn an_observed_rewrite_beats_every_reason() {
        let every = ScanReasons::INITIAL
            | ScanReasons::FORCE
            | ScanReasons::PARSER_STALE
            | ScanReasons::ATTRIBUTION_STALE;
        for r in [ScanReasons::NONE, ScanReasons::PARSER_STALE, every] {
            let p = CommitPlan::plan(&obs(clean(), SourceChange::RollbackOrRewrite), r, true);
            assert_eq!(
                p.store(),
                StoreAction::Project(Projection::Rollback),
                "观察到源变化时，任何原因都不得把它降成 Reparse/Append（reasons={r:?}）"
            );
            assert_eq!(p.index(), IndexAction::ReplaceFile);
        }
    }

    /// 没有旧代可回退时，源变化也只能是 `Append` —— `Rollback` 无意义。
    #[test]
    fn a_rewrite_without_a_prior_projection_is_just_an_append() {
        let p = CommitPlan::plan(
            &obs(clean(), SourceChange::RollbackOrRewrite),
            ScanReasons::NONE,
            false,
        );
        assert_eq!(p.store(), StoreAction::Project(Projection::Append));
    }

    /// 🔴 **承重规则：`Degraded` 整代替换，绝不与旧投影合并。**
    ///
    /// 两批都从 seq 0 起，合并必然撞主键 —— 事务回滚，此后每轮刷新都失败。
    /// 这正是本仓栽过的那个缺陷（一个坏行让索引永久失败）。
    #[test]
    fn a_degraded_read_always_replaces_never_merges() {
        let p = CommitPlan::plan(
            // **带好行**的降级 —— 这才是 `Degraded` 的正常情形（见下一条测试）。
            &obs_with_events(ParseQuality::Degraded(diag()), SourceChange::Appended),
            ScanReasons::NONE,
            true,
        );
        assert_eq!(
            p.index(),
            IndexAction::ReplaceFile,
            "降级读出的是整代，合并进旧投影必然撞主键"
        );
        assert_eq!(p.quality(), &QualityState::Degraded(diag()));
        assert_eq!(p.cursor(), CursorAction::Advance, "好行可用，游标该推进");
    }

    /// 🔴 **降级到一个好行都没有 ⇒ 保留旧的，不整代替换。**
    ///
    /// 一个文件从「两条好行」变成「全是坏行」时，整代替换就成了**用空的替换非空的**：
    /// 一次解析失败当场抹掉真实数据，而这不会报任何错。
    ///
    /// ⚠️ 这一格是既有的 780 条测试抓出来的 —— 我照 ADR 的决策表写，而决策表默认
    /// `Degraded` 总有好行。质量仍报 `Degraded`（有坏行是事实），只是不许它改数据。
    #[test]
    fn a_degradation_with_no_good_lines_preserves_instead_of_emptying() {
        let p = CommitPlan::plan(
            &obs(ParseQuality::Degraded(diag()), SourceChange::Appended),
            // 连强制全读都不能让它抹掉旧数据。
            ScanReasons::FORCE | ScanReasons::ATTRIBUTION_STALE,
            true,
        );
        assert_eq!(p.index(), IndexAction::Preserve, "别用空的替换非空的");
        assert_eq!(p.store(), StoreAction::Preserve);
        assert_eq!(p.cursor(), CursorAction::Freeze);
        assert_eq!(
            p.quality(),
            &QualityState::Degraded(diag()),
            "有坏行是事实，仍要说出来 —— 只是不许它改数据"
        );
    }

    /// 与上一条成对：**`Clean` 的空批要整代替换**。
    ///
    /// 「新解析器合法地对这个文件产出零事件」与「全是坏行」在批的长度上一模一样，
    /// 分辨它们的是 quality。前者必须替换 —— 否则旧数据永久残留且不再有重试机会。
    #[test]
    fn an_empty_clean_read_still_replaces() {
        let p = CommitPlan::plan(
            &obs(clean(), SourceChange::Appended),
            ScanReasons::PARSER_STALE,
            true,
        );
        assert_eq!(
            p.index(),
            IndexAction::ReplaceFile,
            "合法的零事件要整代替换，否则旧数据永久残留"
        );
        assert_eq!(p.store(), StoreAction::Project(Projection::Reparse));
    }

    /// 增量遇坏行：全 Preserve + Freeze，且**质量可见**。
    #[test]
    fn a_poison_line_freezes_everything_but_stays_visible() {
        let p = CommitPlan::plan(
            &obs(
                ParseQuality::RejectedPoisonLine(diag()),
                SourceChange::Appended,
            ),
            ScanReasons::NONE,
            true,
        );
        assert_eq!(p.index(), IndexAction::Preserve);
        assert_eq!(p.store(), StoreAction::Preserve);
        assert_eq!(p.cursor(), CursorAction::Freeze);
        assert_eq!(p.sync(), SyncTransition::Hold, "旧投影仍是当前的，不是欠账");
        assert_eq!(
            p.quality(),
            &QualityState::RejectedPoisonLine(diag()),
            "必须可见 —— 否则一个永久坏行会安静地让这个文件停在原地"
        );
    }

    /// 🔴 「没读成」与「主动拒绝」都冻结，但**质量必须分开**。
    ///
    /// 压成同一个 quality 等于把「网络/权限问题」与「数据里有坏行」说成一回事，
    /// 而用户能做的事完全不同（等一等 vs 去看那一行）。
    #[test]
    fn unavailable_and_rejected_freeze_alike_but_report_differently() {
        let unavailable = CommitPlan::plan(
            &obs(
                ParseQuality::Unavailable(ScanFailure::Read("io".into())),
                SourceChange::Appended,
            ),
            ScanReasons::FORCE,
            true,
        );
        let rejected = CommitPlan::plan(
            &obs(
                ParseQuality::RejectedPoisonLine(diag()),
                SourceChange::Appended,
            ),
            ScanReasons::NONE,
            true,
        );
        assert_eq!(unavailable.cursor(), rejected.cursor());
        assert_eq!(unavailable.index(), rejected.index());
        assert_ne!(
            unavailable.quality(),
            rejected.quality(),
            "「没读成」与「读到了但我们拒绝」不是一回事"
        );
        assert_eq!(unavailable.quality(), &QualityState::Unknown);
    }

    /// 读不到时，**任何原因都不能让它写库** —— 强制刷新也不行。
    #[test]
    fn no_reason_can_make_an_unavailable_scan_write() {
        let every = ScanReasons::INITIAL
            | ScanReasons::FORCE
            | ScanReasons::PARSER_STALE
            | ScanReasons::ATTRIBUTION_STALE;
        for r in [ScanReasons::NONE, ScanReasons::FORCE, every] {
            let p = CommitPlan::plan(
                &obs(
                    ParseQuality::Unavailable(ScanFailure::Stat("gone".into())),
                    SourceChange::RollbackOrRewrite,
                ),
                r,
                true,
            );
            assert_eq!(p.store(), StoreAction::Preserve, "reasons={r:?}");
            assert_eq!(p.index(), IndexAction::Preserve, "reasons={r:?}");
            assert_eq!(p.cursor(), CursorAction::Freeze, "reasons={r:?}");
        }
    }

    // ── 组合矩阵：每一格都要算得出来，且不变式处处成立 ────────────────

    /// 🔴 **不变式：索引整代替换 ⟺ 总库开新代或全读。**
    ///
    /// 分开决定就会出现「索引换了、总库追加」这种谁都没打算要的组合 —— 而那正是
    /// 主键冲突的形状。这条扫遍全矩阵，不挑格子。
    #[test]
    fn across_the_whole_matrix_the_two_layers_never_disagree() {
        let qualities = [
            clean(),
            ParseQuality::Clean {
                deferred_tail_bytes: 7,
            },
            ParseQuality::Degraded(diag()),
            ParseQuality::RejectedPoisonLine(diag()),
            ParseQuality::Unavailable(ScanFailure::Read("io".into())),
        ];
        let changes = [SourceChange::Appended, SourceChange::RollbackOrRewrite];
        let reason_sets = [
            ScanReasons::NONE,
            ScanReasons::INITIAL,
            ScanReasons::FORCE,
            ScanReasons::PARSER_STALE,
            ScanReasons::ATTRIBUTION_STALE,
            ScanReasons::FORCE | ScanReasons::PARSER_STALE,
            ScanReasons::INITIAL | ScanReasons::ATTRIBUTION_STALE,
        ];
        let mut seen = 0;
        for q in &qualities {
            for c in changes {
                for r in reason_sets {
                    for has_prior in [true, false] {
                        let p = CommitPlan::plan(&obs(q.clone(), c), r, has_prior);
                        seen += 1;
                        match p.index() {
                            IndexAction::Preserve => assert_eq!(
                                p.store(),
                                StoreAction::Preserve,
                                "索引不动时总库也不能动：{q:?}/{c:?}/{r:?}"
                            ),
                            IndexAction::AppendFacts => assert_eq!(
                                p.store(),
                                StoreAction::Project(Projection::Append),
                                "索引增量时总库只能追加：{q:?}/{c:?}/{r:?}"
                            ),
                            IndexAction::ReplaceFile => assert!(
                                matches!(p.store(), StoreAction::Project(_)),
                                "索引整代替换时总库必须写：{q:?}/{c:?}/{r:?}"
                            ),
                        }
                        // 冻游标 ⇔ 不推进同步目标 ⇔ 两层都不动。
                        assert_eq!(
                            p.cursor() == CursorAction::Freeze,
                            p.sync() == SyncTransition::Hold,
                            "冻游标与不推进同步目标必须同进同退：{q:?}/{c:?}/{r:?}"
                        );
                    }
                }
            }
        }
        assert_eq!(seen, 5 * 2 * 7 * 2, "矩阵要跑满");
    }

    #[test]
    fn reasons_compose_and_are_queryable() {
        let r = ScanReasons::FORCE | ScanReasons::PARSER_STALE;
        assert!(r.contains(ScanReasons::FORCE));
        assert!(r.contains(ScanReasons::PARSER_STALE));
        assert!(!r.contains(ScanReasons::INITIAL));
        assert!(ScanReasons::NONE.is_empty());
        assert!(!r.is_empty());
    }
}
