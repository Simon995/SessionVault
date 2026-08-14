//! 一轮工作的绝对截止时刻（ADR-051 §4）。
//!
//! # 为什么是参数，不是各处自己看一眼时钟
//!
//! 「加上限」这件事本仓做过三次，每次都只加到**主要那条路径**：
//! 先只改 `run_bash_stdin`，漏 `list_distros`（实测让一次单元测试跑了 10 分钟）；
//! 补上后又漏 `read_file_at`。现在每个出站 spawn 都有上限了 —— 但**上限各自独立**：
//! 三处分别拿 `WSL_LIST_TIMEOUT` / `WSL_CALL_TIMEOUT`，与整轮预算无关。
//!
//! 后果：一轮预算只剩 5 秒时，一次 WSL 调用仍会拿满 60 秒。**整轮 deadline 形同虚设**
//! —— 它只在两次调用**之间**被检查（`deadline.expired()`），管不住任何一次调用本身。
//!
//! ⇒ [`Deadline::budget_for`]：单次调用的实际上限 = **剩余预算与该调用自身上限的
//! 较小者**。预算耗尽时返回 `None`，调用方据此**根本不发起**这次调用。
//!
//! # 保证范围（写清楚，不假装）
//!
//! - ✅ **WSL 子进程覆盖全生命周期**：spawn → 关 stdin → 读输出 → 超时 kill → `wait` 回收。
//! - ✅ **所有子调用接收同一个绝对时刻**，不各自重新起算。
//! - ⚠️ **本地同步 FS 调用只能在读块之间协作检查**，不宣称硬超时。一次
//!   `std::fs::read` 卡在网络盘上没有办法从外面打断 —— 这条写在这里，而不是
//!   假装有。
//! - ✅ **时钟可注入**：`*_at(now)` 变体让测试不依赖真实 `sleep`。

use std::time::{Duration, Instant};

/// 一轮工作的绝对截止时刻。`Copy` —— 传给每个子调用，不是各自新建。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Deadline(Instant);

impl Deadline {
    /// 从现在起 `budget` 之后到期。
    pub fn after(budget: Duration) -> Self {
        Self(Instant::now() + budget)
    }

    /// 指定绝对时刻 —— 测试用，也用于把一个已有的 `Instant` 包起来。
    pub fn at(instant: Instant) -> Self {
        Self(instant)
    }

    /// 一个**实际上不会到期**的 deadline。
    ///
    /// 给那些不属于任何一轮预算的调用方（CLI 一次性命令、凭据读取）。
    /// 🔴 它仍然要求调用方**显式**写出来 —— 「没有 deadline」是一个决定，不该由
    /// 「这个参数可以是 `None`」悄悄表达。
    pub fn unbounded() -> Self {
        Self(Instant::now() + Duration::from_secs(365 * 24 * 3600))
    }

    /// 还剩多少。已过期 ⇒ `None`（**不是** `Duration::ZERO`：零会被当成
    /// 「立即超时」传给下游，而下游多半会把它当成一个合法的极短上限）。
    pub fn remaining(&self) -> Option<Duration> {
        self.remaining_at(Instant::now())
    }

    /// [`Self::remaining`] 的可测形态 —— 时钟作参数。
    pub fn remaining_at(&self, now: Instant) -> Option<Duration> {
        self.0.checked_duration_since(now).filter(|d| !d.is_zero())
    }

    /// 单次调用的实际上限 = **剩余预算与该调用自身上限的较小者**。
    ///
    /// `None` = 预算已耗尽，**这次调用根本不该发起**。
    pub fn budget_for(&self, cap: Duration) -> Option<Duration> {
        self.budget_for_at(cap, Instant::now())
    }

    /// [`Self::budget_for`] 的可测形态。
    pub fn budget_for_at(&self, cap: Duration, now: Instant) -> Option<Duration> {
        self.remaining_at(now).map(|left| left.min(cap))
    }

    pub fn expired(&self) -> bool {
        self.remaining().is_none()
    }
}

#[cfg(test)]
// 测试要造 fixture（建目录、写文件、再核一遍），允许直接碰盘 —— 文件系统边界
// 管的是**生产行为**，而 `#[cfg(test)]` 不在生产路径上。
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    fn t(secs: u64) -> Duration {
        Duration::from_secs(secs)
    }

    /// 🔴 **单次调用拿不到超过剩余预算的时间。** 这正是「整轮 deadline 形同虚设」
    /// 的修法：从前三处 WSL 调用各拿固定 60 秒，与整轮预算无关。
    #[test]
    fn a_single_call_never_outlives_the_round() {
        let start = Instant::now();
        let d = Deadline::at(start + t(5));
        assert_eq!(
            d.budget_for_at(t(60), start),
            Some(t(5)),
            "剩 5 秒时一次调用不能拿 60 秒"
        );
    }

    /// 自身上限更小时用自身的 —— deadline 不是用来**放宽**单次上限的。
    #[test]
    fn a_tighter_per_call_cap_still_wins() {
        let start = Instant::now();
        let d = Deadline::at(start + t(600));
        assert_eq!(d.budget_for_at(t(2), start), Some(t(2)));
    }

    /// 🔴 **耗尽 ⇒ `None`，不是零。**
    ///
    /// 零会被当成一个合法的极短上限传给下游，于是仍然 spawn 一个进程再立刻杀掉 ——
    /// 白付一次进程创建的代价，而且日志里会出现一条看起来像「超时」的失败。
    /// `None` 让调用方**根本不发起**这次调用。
    #[test]
    fn an_exhausted_budget_is_none_not_zero() {
        let start = Instant::now();
        let d = Deadline::at(start);
        assert_eq!(d.budget_for_at(t(60), start), None, "刚好到点就是耗尽");
        assert_eq!(d.budget_for_at(t(60), start + t(1)), None, "过点更是");
        assert_eq!(d.remaining_at(start), None);
    }

    /// 同一个 deadline 传给多个子调用，**每个都按同一个绝对时刻算**，
    /// 不各自重新起算 —— 这是它 `Copy` 而不是每次 `after()` 的理由。
    #[test]
    fn every_child_call_measures_against_the_same_instant() {
        let start = Instant::now();
        let d = Deadline::at(start + t(10));
        assert_eq!(d.budget_for_at(t(60), start), Some(t(10)));
        assert_eq!(d.budget_for_at(t(60), start + t(4)), Some(t(6)), "时间在走");
        assert_eq!(d.budget_for_at(t(60), start + t(9)), Some(t(1)));
    }

    #[test]
    fn unbounded_never_runs_out_in_practice() {
        assert!(Deadline::unbounded().budget_for(t(60)) == Some(t(60)));
        assert!(!Deadline::unbounded().expired());
    }
}
