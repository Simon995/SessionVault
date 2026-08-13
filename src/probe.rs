//! 存在性探测的**唯一原语**（ADR-051 §5 / §8）。
//!
//! # 为什么要有这个模块
//!
//! 「这条路径上有没有东西」这个问题，本仓在五轮评审里答错了**七次**，每次的修法都是
//! 在出事的那个调用点把两态改成三态。修法本身是对的，可它是**逐点**的 —— 而判据
//!
//! > `NotFound` 是**事实**（那里确实没有），其余每一种 IO 错误都是**「没问成」**
//!
//! 在此之前**没有唯一实现**：`discovery.rs` / `discover.rs` / `memory/sources.rs` 各自
//! 手写过一遍那个 `match`，`identity.rs` / `store.rs` / `project_root.rs` 则各自手写了
//! **折叠掉错误**的那一版。于是每加一处探测，作者都要重新推导一次这条规则，
//! 而实测大约一半推导错。
//!
//! 机械闸挡不住这个，因为它是**滞后**指标：只拦已经咬过人的拼写（`exists()` →
//! 补 `is_dir()` → 补 `is_file()` → 补 `.ok().flatten()`…），且只在已经咬过人的文件里。
//! 判据有无穷多种语法形状，而闸每次只学会上一种。
//!
//! ⇒ 规则收口到这里写**一次**（[`classify`]），调用点只做**决定**，不做分类。
//! 闸随之从「禁止这几种拼写」改成「除 backend 外不得直接问文件系统」——
//! 那是个**边界**判据，对未来新增的写法同样成立。
//!
//! # 三态之外没有第四种表达
//!
//! [`Probed`] **故意不提供** `is_found() -> bool` / `unwrap_or(false)` 之类的便利方法。
//! 那正是七次事故的形状：一个 `bool` 里装不下三种答案，于是「没问成」被迫挤进
//! 「没有」。调用方必须 `match` 到底，把「问不到时算什么」**写出来** ——
//! 这与 `auth/identity.rs::IdentityResolution::Pending` 不携带任何值是同一招：
//! **让编译器持守不变式，而不是让下一个人记得**。
//!
//! # 边界：本模块只回答「有没有、是什么」
//!
//! 读内容、列目录、算指纹都不在这里 —— 那些有各自的错误语义。本模块的产物是
//! [`Probed<FileKind>`]，调用方据此决定，不再二次 stat。
//!
//! ⚠️ **祖先链上溯的 WSL 支不走这个 trait**：`wsl::find_project_root` 把整条链放进
//! 一次 `wsl.exe` 调用（逐级从 Windows 侧 stat 要 N 次跨 VM 往返，每次 0.1–0.3s）。
//! 那是个真实的性能约束，不是遗漏 —— 写在这里，而不是假装抽象是统一的。

use std::path::Path;

use crate::deadline::Deadline;

/// 一次存在性探测的结果 —— **三态**。
///
/// 🔴 没有 `is_found()`、没有 `unwrap_or`、没有 `Into<bool>`。见模块文档。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probed<T> {
    /// 探到了，附带调用方需要的元信息。
    ///
    /// 🔴 **携带 `T` 而不是只说「在」**：只返回 `Present` 会让调用方紧接着自己
    /// 再 `.is_file()` 一次 —— 那次调用又在这个模块之外，绕回原问题（ADR-051 §5）。
    Found(T),
    /// 探明白了：那里确实没有。**这是事实**，调用方可以据此删数据、写负缓存。
    Absent,
    /// **没问成** —— 权限拒绝、句柄耗尽、发行版停了、UNC 不通、瞬时 IO 错误。
    ///
    /// 🔴 调用方**不得**把它当作 [`Probed::Absent`]。它意味着「本轮这个答案不作数」：
    /// 不能据此 prune、不能写负缓存、该走短退避下轮重试。
    Unknown(ProbeError),
}

impl<T> Probed<T> {
    /// 换掉 `Found` 里的值，三态结构不变。
    ///
    /// 只提供不会折叠状态的组合子 —— 任何「把三态压成两态」的便利方法都是
    /// 本模块要防的那件事。
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> Probed<U> {
        match self {
            Probed::Found(v) => Probed::Found(f(v)),
            Probed::Absent => Probed::Absent,
            Probed::Unknown(e) => Probed::Unknown(e),
        }
    }
}

/// 探测失败的原因 —— 带上是哪条路径，否则日志里只剩一句「stat failed」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeError {
    pub path: String,
    pub message: String,
}

impl ProbeError {
    pub fn new(path: &Path, message: impl std::fmt::Display) -> Self {
        Self {
            path: path.display().to_string(),
            message: message.to_string(),
        }
    }
}

impl std::fmt::Display for ProbeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.path, self.message)
    }
}

/// 探到的是什么。
///
/// 调用方多半只关心其中一种（要文件 / 要目录），但**判据必须由调用方写出来**：
/// 「存在但不是我要的那种」是**事实**，与「没问成」不同。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    File,
    Dir,
    /// 符号链接指向别处、设备文件、FIFO…… 存在，但既不是文件也不是目录。
    Other,
}

/// 一个能回答「这条路径上有没有东西」的地方（本机 / 某个 WSL 发行版）。
///
/// 🔴 **实现者是全仓唯一允许直接问文件系统的地方**，由 `verify-agents-md.mjs`
/// 的边界闸守着。
pub trait ProbeBackend {
    fn probe(&self, path: &Path, deadline: Deadline) -> Probed<FileKind>;
}

/// 🔴 **判据的唯一实现。**
///
/// 在此之前这个 `match` 在仓库里有四份手抄本（`discover.rs` 两处、`discovery.rs`、
/// `memory/sources.rs`），另有五处把它写成了折叠错误的两态版本。
///
/// - `Ok(_)` ⇒ 那里**有东西**，是什么由 [`FileKind`] 说，调用方自己判要不要；
/// - `Err(NotFound)` ⇒ **事实**：确实没有；
/// - 其余 `Err` ⇒ **没问成**。
///
/// ⚠️ `NotFound` 必须留在 `Absent` 这一侧。把它也报成不可达，会让每个没装某 CLI、
/// 没写过 `CLAUDE.md` 的用户永久带着一个假故障，prune 全被禁掉（AGENTS.md 已记）。
pub fn classify(path: &Path, meta: std::io::Result<std::fs::Metadata>) -> Probed<FileKind> {
    match meta {
        Ok(m) if m.is_file() => Probed::Found(FileKind::File),
        Ok(m) if m.is_dir() => Probed::Found(FileKind::Dir),
        Ok(_) => Probed::Found(FileKind::Other),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Probed::Absent,
        Err(e) => Probed::Unknown(ProbeError::new(path, e)),
    }
}

/// 本机文件系统。**全仓唯一为「存在性」调 `std::fs` 的地方之一**（另一个是
/// [`WslBackend`]，它经访问桥）。
#[derive(Debug, Clone, Copy, Default)]
pub struct LocalBackend;

impl ProbeBackend for LocalBackend {
    /// ⚠️ **本地同步 stat 不宣称硬超时** —— 一次卡在断开网络盘上的 `metadata` 没有
    /// 办法从外面打断（`deadline.rs` 已把这条写清楚）。预算耗尽时**不发起**这次
    /// 调用，报 `Unknown`：那是诚实的「本轮没问成」，而不是假装有硬超时。
    fn probe(&self, path: &Path, deadline: Deadline) -> Probed<FileKind> {
        if deadline.expired() {
            return Probed::Unknown(ProbeError::new(path, "round budget exhausted before probe"));
        }
        classify(path, std::fs::metadata(path))
    }
}

/// 某个 WSL 发行版内的路径，经访问桥探测。
#[derive(Debug, Clone)]
pub struct WslBackend {
    pub distro: String,
}

impl WslBackend {
    pub fn new(distro: impl Into<String>) -> Self {
        Self {
            distro: distro.into(),
        }
    }
}

impl ProbeBackend for WslBackend {
    /// `wsl::stat` 已经是三态的（`Ok(Some)` / `Ok(None)` / `Err`），这里只做翻译。
    ///
    /// 🔴 **边界：访问桥问的是 `[ -f ]`，只认「普通文件」。** 于是一个**目录**会
    /// 落到 `Ok(None)` ⇒ 本函数报 [`Probed::Absent`]。对今天唯一的 WSL 调用方
    /// （`discover.rs` 找 `CLAUDE.md` / `AGENTS.md`）这是忠实的；但**要问「这里有没有
    /// 目录」的调用方必须先扩访问桥**，不能直接用这个 backend —— 否则拿到的是一个
    /// 看起来权威的错答案，而那比说不出来更糟。
    fn probe(&self, path: &Path, deadline: Deadline) -> Probed<FileKind> {
        let linux_path = path.to_string_lossy();
        match crate::wsl::stat(&self.distro, &linux_path, deadline) {
            Ok(Some(_)) => Probed::Found(FileKind::File),
            Ok(None) => Probed::Absent,
            Err(e) => Probed::Unknown(ProbeError::new(path, e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Error, ErrorKind};

    fn meta_err(kind: ErrorKind) -> std::io::Result<std::fs::Metadata> {
        Err(Error::new(kind, "boom"))
    }

    /// 🔴 **`NotFound` 是事实，其余每一种错误都是「没问成」。**
    ///
    /// 逐个列出真实出现过的错误种类 —— 它们在生产里对应权限拒绝、句柄耗尽、
    /// 停掉的发行版、不通的 UNC。变异（把任何一条并进 `Absent`）当场变红。
    #[test]
    fn only_not_found_is_a_fact() {
        let p = Path::new("/x");
        assert_eq!(classify(p, meta_err(ErrorKind::NotFound)), Probed::Absent);

        for kind in [
            ErrorKind::PermissionDenied,
            ErrorKind::TimedOut,
            ErrorKind::ConnectionRefused,
            ErrorKind::Interrupted,
            ErrorKind::InvalidInput,
            ErrorKind::Other,
        ] {
            assert!(
                matches!(classify(p, meta_err(kind)), Probed::Unknown(_)),
                "{kind:?} 必须是「没问成」，不能折叠成「没有」"
            );
        }
    }

    /// 探到了就要说出**是什么** —— 只说「在」会让调用方再 stat 一次，绕回原问题。
    #[test]
    fn found_carries_the_kind() {
        let tmp = std::env::temp_dir().join("sv-probe-kind-test");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let file = tmp.join("f");
        std::fs::write(&file, b"x").unwrap();

        let b = LocalBackend;
        let d = Deadline::unbounded();
        assert_eq!(b.probe(&file, d), Probed::Found(FileKind::File));
        assert_eq!(b.probe(&tmp, d), Probed::Found(FileKind::Dir));
        assert_eq!(b.probe(&tmp.join("nope"), d), Probed::Absent);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// 🔴 预算耗尽 ⇒ **不发起**这次调用，且报「没问成」而不是「没有」。
    ///
    /// 报 `Absent` 会让一次超时被写进负缓存 / 触发 prune —— 那是 ADR-050 那次
    /// 「WSL 变慢删掉 369 个文件」的形状。
    #[test]
    fn an_exhausted_budget_is_unknown_not_absent() {
        let tmp = std::env::temp_dir().join("sv-probe-budget-test");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let file = tmp.join("f");
        std::fs::write(&file, b"x").unwrap();

        let expired = Deadline::at(std::time::Instant::now());
        // 文件**确实存在**，所以 `Absent` 不可能来自「真的没有」——
        // 这条断言只可能被「预算耗尽被折叠成没有」打破。
        assert!(matches!(
            LocalBackend.probe(&file, expired),
            Probed::Unknown(_)
        ));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// `map` 不许改变三态结构 —— 它是唯一的组合子，折叠状态的便利方法一个都不给。
    #[test]
    fn map_preserves_the_three_states() {
        let e = ProbeError::new(Path::new("/p"), "why");
        assert_eq!(
            Probed::Found(FileKind::File).map(|_| 1),
            Probed::<i32>::Found(1)
        );
        assert_eq!(Probed::<FileKind>::Absent.map(|_| 1), Probed::<i32>::Absent);
        assert_eq!(
            Probed::<FileKind>::Unknown(e.clone()).map(|_| 1),
            Probed::<i32>::Unknown(e)
        );
    }
}
