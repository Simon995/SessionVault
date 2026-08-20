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

    /// 读文件为 UTF-8 文本 —— 三态，**且与 [`Self::probe`] 是同一个事实来源**。
    ///
    /// 🔴 **它在 trait 上，是因为不在 trait 上时同一个缺陷复发了三次。**
    ///
    /// 从前 trait 只有 `probe`，而读取是个直接走 `std::fs` 的自由函数。于是一个拿了
    /// backend 的函数只能用它**探测**、却只能用本机文件系统**读取**：
    ///
    /// | 调用点 | 后果 |
    /// | --- | --- |
    /// | `identity::read_origin_url_with` | 已被发现（「判决链中途不许换事实来源」），处置是**在这一个调用点**补 fail-safe |
    /// | `identity::git_config_path` ×2 | **同一形状，没有那个 fail-safe** |
    /// | `identity::wsl_repo_id` | 干脆自己拼路径读文件 ⇒ **第二份实现，且漏了 `commondir` 那一步** |
    ///
    /// 最后一项的实测后果：每一个 WSL 里的 linked worktree 永远拿不到
    /// `canonical_id`，同一个仓的记忆分裂成两组。
    ///
    /// **根因写对了，处置却是按反例补控制流** —— 于是同一根因在相邻路径反复复发。
    /// 把读取提上 trait 之后，「探测用 A、读取用 B」在类型上表达不出来。
    ///
    /// 语义与 [`read_text`] 一致：`Absent` = 那里没有（且过了命名空间那一关）；
    /// 非法 UTF-8 归 `Unknown`（读到了但看不懂，与「没有」是两件事）。
    fn read_text(&self, path: &Path, deadline: Deadline) -> Probed<String>;
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
///
/// 🔴 **但 `NotFound` 本身不足以定案** —— 见 [`namespace_confirms_absence`]。本函数只做
/// 「这一次系统调用说了什么」的忠实翻译，**不是**最终判决；`LocalBackend` 会对
/// `Absent` 再核一次命名空间。分成两步是因为它们是两个问题：这里是「系统怎么说的」，
/// 那里是「这个说法算不算数」。
pub fn classify(path: &Path, meta: std::io::Result<std::fs::Metadata>) -> Probed<FileKind> {
    match meta {
        Ok(m) if m.is_file() => Probed::Found(FileKind::File),
        Ok(m) if m.is_dir() => Probed::Found(FileKind::Dir),
        Ok(_) => Probed::Found(FileKind::Other),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Probed::Absent,
        Err(e) => Probed::Unknown(ProbeError::new(path, e)),
    }
}

/// 这次探测的**命名空间锚点** —— 「什么东西还在，才轮得到说叶子不在」。
///
/// 🔴 **上一版从路径语法推导锚点，那在 Unix 上是空操作**（三轮评审 P1）。
/// `ancestors().last()` 在 Windows 上给出 `C:\` / `\\server\share\`，够用；
/// 而在 Linux/macOS 上**永远是 `/`**，它永远可达。于是
/// `/mnt/work/.claude` 或 `/Volumes/Work/.codex` 所在的卷被卸载时：
/// 叶子 `NotFound` → `/` 可达 → 报 `Absent` → 该位置不进 `unreachable` →
/// **prune 照常删掉会话与用量投影**。Windows 的目录挂载点、DFS 链接同理 ——
/// 盘符根还在，挂上去的那个卷已经不在了。
///
/// **正解是让调用方给锚点**：只有它知道这次探测属于哪个来源根。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Anchor {
    /// 调用方声明的来源根（配置目录 / 发现根）。**只有它可达时，其下的缺失才算事实。**
    /// 这是 prune 驱动路径**必须**用的那一种。
    Under(std::path::PathBuf),
    /// 调用方拿不出来源根（一次性 CLI、诊断探针、祖先链上溯 —— 后者本来就是在
    /// **找**根，没有更上层可锚）。
    ///
    /// ⚠️ **它只核到路径语法根**，因此在 Unix 上说不出卷卸载，在 Windows 上说不出
    /// 目录挂载点消失。**写出 `unanchored()` 就是接受这一点** —— 让它在调用点可见，
    /// 而不是藏在一个默认值里。
    /// 要彻底覆盖需要**持久化上次成功扫描时的卷/设备身份**，那是另一件事。
    None,
}

/// 🔴 **`Absent` 要以「命名空间根可达」为前提**（三轮评审 P1-1）。
///
/// Windows 上一个**未挂载的盘符**返回的是 `ERROR_PATH_NOT_FOUND`（raw 3），
/// Rust 映射成 `ErrorKind::NotFound` —— **与「盘符在、文件确实没有」逐位相同**
/// （后者也是 raw 3）。本机实测：
///
/// | 路径 | kind | raw |
/// | --- | --- | --- |
/// | `C:\不存在\x.json`（盘符在） | `NotFound` | 3 |
/// | **未挂载 `Z:\home\u`** | `NotFound` | **3** |
/// | **死 UNC 主机** | `NotFound` | 53 |
/// | 死 WSL distro UNC | `Uncategorized` | 64 ✅ 本来就落 `Unknown` |
///
/// ⚠️ `try_exists()` 在未挂载盘符上返回 `Ok(false)` —— **它同样答错**，所以「用
/// `try_exists` 就对了」这条旧说法在这一格上不成立。
///
/// 后果不是崩溃：`CLAUDE_CONFIG_DIR` / `CODEX_HOME` 落在临时断开的盘符或网络位置时，
/// 发现阶段会报「问过了，那里什么都没有」⇒ 调用方不把该位置记为不可达 ⇒ **prune 照常
/// 执行**，删掉既有的 `agent_session_files` / `agent_sessions` / `usage_facts` 投影。
/// 那正是这套三态类型存在的理由（ADR-050 那次「WSL 变慢删掉 369 个文件」同形）。
///
/// **判据是一条原则，不是一张错误码表**：叶子的缺失只有在**命名空间本身可达**时才算
/// 事实。根用 `ancestors().last()` 取，粒度天然正确 ——
/// `C:\` / `\\server\share\` / `\\wsl.localhost\<distro>\`。
/// raw 53（`ERROR_BAD_NETPATH`）这类被它自然覆盖，不必逐个枚举。
///
/// 代价：每个 `Absent` 多一次 `metadata(root)`。根的元数据被 OS 缓存，且只在
/// **否定**结论上付费 —— 而否定结论正是会驱动删除的那一种。
#[allow(clippy::disallowed_methods)] // ← 唯一允许点之一（见 clippy.toml）
fn namespace_confirms_absence(path: &Path, anchor: &Anchor) -> Probed<FileKind> {
    // 锚点由调用方给：只有它可达，其下的缺失才算事实。这一支能看见卷卸载 ——
    // 语法根那一支看不见（Unix 上永远是 `/`）。
    // 🔴 **锚点只用来接住「真的问不成」，不再拿它推断卷在不在**（五轮评审 P1）。
    //
    // 上一版按「根不见了就再看一层父目录」分「没装」与「掉盘」，**两个方向都错**：
    //
    // - Unix 卷卸载后**挂载点目录仍然存在**：`/mnt/work` 照常 `metadata` 得到，
    //   只有 `.claude` 没了 ⇒ 判成 `Absent` ⇒ local 不进 `unreachable` ⇒ **照样 prune**。
    //   而「卸载后根与父目录一起消失」是我造的测试形状，不是真实形状。
    // - 反方向：自定义根 `/home/u/.config/claude` 而 `.config` 尚未建立 ⇒ 父目录不在
    //   ⇒ 判成 `Unknown` ⇒ **永久阻止 local prune**。
    //
    // **路径语法里没有「这个卷还在不在」这个信息**，一层、两层、走到 `/` 都一样 ——
    // 换个层数只是换一组反例。要真答出来必须持久化上次成功访问的卷/设备身份
    // （Unix `st_dev` / Windows volume serial），那需要一处存储，是独立一步（task #15）。
    //
    // 所以这里**不再猜**：锚点只在给出**明确的不可达信号**时才升级为 `Unknown`
    // —— 权限拒绝、IO 错误、UNC 不通这些是真的「问不成」；而 `NotFound` 与
    // 「卷卸载」在这一层不可区分，**归 `Absent`**（与本函数之前的语法根判定一致）。
    if let Anchor::Under(root) = anchor {
        if let Err(e) = std::fs::metadata(root) {
            if e.kind() != std::io::ErrorKind::NotFound {
                return Probed::Unknown(ProbeError::new(
                    path,
                    format!("source root {} is not reachable: {e}", root.display()),
                ));
            }
        }
        // 落到下面的语法根判定 —— Windows 未挂载盘符 / 死 UNC 仍然会被它接住。
    }

    let Some(root) = path.ancestors().last() else {
        return Probed::Absent;
    };
    // 相对路径的根是空串：命名空间就是进程的当前目录，进程活着它就可达。
    if root.as_os_str().is_empty() {
        return Probed::Absent;
    }
    // 路径本身就是根（`Z:\`）：没有更上层可核，就说不出「它确实不存在」。
    if root == path {
        return Probed::Unknown(ProbeError::new(
            path,
            "namespace root itself is not reachable, cannot tell whether it exists",
        ));
    }
    match std::fs::metadata(root) {
        Ok(_) => Probed::Absent,
        Err(e) => Probed::Unknown(ProbeError::new(
            path,
            format!("namespace root {} is not reachable: {e}", root.display()),
        )),
    }
}

/// 本机文件系统。**全仓唯一为「存在性」调 `std::fs` 的地方之一**（另一个是
/// [`WslBackend`]，它经访问桥）。
///
/// 🔴 **没有 `Default`、没有单元构造** —— 每个调用点必须写出
/// [`LocalBackend::rooted_at`] 还是 [`LocalBackend::unanchored`]。
/// 上一版是个单元结构体，于是「用哪种命名空间判据」这个决定**没有人做过**，
/// 默认落到了在 Unix 上等于没有的那一种（三轮评审 P1）。
/// 同 `Probed` 不给 `is_found()`：**让类型提出问题，而不是让下一个人记得问**。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalBackend {
    anchor: Anchor,
}

impl LocalBackend {
    /// 锚定到一个来源根 —— **prune 驱动路径必须用这个**。
    ///
    /// 只有 `root` 可达时，其下的 `NotFound` 才被判为 `Absent`；`root` 探不到就是
    /// `Unknown`（卷卸载 / 挂载点消失 / 网络位置断开都落在这一支）。
    pub fn rooted_at(root: impl Into<std::path::PathBuf>) -> Self {
        Self {
            anchor: Anchor::Under(root.into()),
        }
    }

    /// 没有来源根可给。⚠️ **只核到路径语法根**，在 Unix 上说不出卷卸载 —— 见
    /// [`Anchor::None`]。写出它就是接受这一点。
    pub fn unanchored() -> Self {
        Self {
            anchor: Anchor::None,
        }
    }
}

impl ProbeBackend for LocalBackend {
    /// ⚠️ **本地同步 stat 不宣称硬超时** —— 一次卡在断开网络盘上的 `metadata` 没有
    /// 办法从外面打断（`deadline.rs` 已把这条写清楚）。预算耗尽时**不发起**这次
    /// 调用，报 `Unknown`：那是诚实的「本轮没问成」，而不是假装有硬超时。
    #[allow(clippy::disallowed_methods)] // ← 唯一允许点之一（见 clippy.toml）
    fn probe(&self, path: &Path, deadline: Deadline) -> Probed<FileKind> {
        if deadline.expired() {
            return Probed::Unknown(ProbeError::new(path, "round budget exhausted before probe"));
        }
        match classify(path, std::fs::metadata(path)) {
            // 🔴 系统说「没有」还不算数 —— 未挂载的盘符也这么说。见
            // [`namespace_confirms_absence`]。
            Probed::Absent => namespace_confirms_absence(path, &self.anchor),
            verdict => verdict,
        }
    }

    /// ⚠️ **anchor 跟着 backend 走** —— 这正是它本来就该在的地方。自由函数
    /// [`read_text`] 要调用方**再传一次** `anchor_root`，而调用方手上已经有一个
    /// 带 anchor 的 backend 了：两处各说一次，就有两处可以说得不一样。
    #[allow(clippy::disallowed_methods)] // ← 唯一允许点之一（见 clippy.toml）
    fn read_text(&self, path: &Path, deadline: Deadline) -> Probed<String> {
        if deadline.expired() {
            return Probed::Unknown(ProbeError::new(path, "round budget exhausted before read"));
        }
        match std::fs::read_to_string(path) {
            Ok(t) => Probed::Found(t),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                namespace_confirms_absence(path, &self.anchor).map(|_| String::new())
            }
            Err(e) => Probed::Unknown(ProbeError::new(path, e)),
        }
    }
}

/// 一个文件的大小与修改时间。
///
/// 🔴 **它和存在性走同一个出口，不是另开一条路。** 从前 `scan.rs` 直接
/// `std::fs::metadata(...)?` 取这两个数 —— 那次调用本身没有折叠任何东西（错误往上抛），
/// 但它的存在**逼着边界闸留一个 carve-out**（「带 `?` 的 metadata 放行」），
/// 而那个 carve-out 正是 `std::fs::metadata(p).is_ok()` 能溜过去的原因
/// （三轮评审 P2-2）。**闸上每一个例外，都是一条以后会被走的路。**
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileFacts {
    pub len: u64,
    /// UNIX 秒。取不到 mtime 不是错误 —— 有文件系统就是不提供。
    pub modified_unix: Option<i64>,
}

impl LocalBackend {
    /// 本机文件的大小与 mtime。三态与 [`ProbeBackend::probe`] 一致。
    #[allow(clippy::disallowed_methods)] // ← 唯一允许点之一（见 clippy.toml）
    pub fn stat(&self, path: &Path, deadline: Deadline) -> Probed<FileFacts> {
        if deadline.expired() {
            return Probed::Unknown(ProbeError::new(path, "round budget exhausted before stat"));
        }
        match std::fs::metadata(path) {
            Ok(m) => Probed::Found(FileFacts {
                len: m.len(),
                modified_unix: m
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64),
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // 同 `probe`：`NotFound` 还要过命名空间这一关。
                match namespace_confirms_absence(path, &self.anchor) {
                    Probed::Absent => Probed::Absent,
                    Probed::Unknown(e) => Probed::Unknown(e),
                    Probed::Found(_) => Probed::Absent,
                }
            }
            Err(e) => Probed::Unknown(ProbeError::new(path, e)),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 文件系统访问的其余部分 —— **边界要么是模块要么不是**（四轮评审 P2）
// ─────────────────────────────────────────────────────────────────────────────
//
// 上一版只禁存在性 API，于是 `File::open(p).is_ok()` / `read_dir(p).is_ok()` /
// `fs::read(p).is_ok()` 照样把「没问成」折叠成「不存在」—— 而 `scan.rs` 里**当时
// 就已经有**一处 `File::open`，不是假想。
//
// 现在整个 `std::fs` 只许出现在本文件。下面按**折叠风险**分两组：
//
// - **观测**（读内容、列目录）：失败必须能与「空/没有」分开，所以返回 `Probed`。
// - **变更**（建目录、写、改名、改权限）：失败本来就会响亮地报出来，**透传 `Result`**，
//   不假装它有三态。
//
// ⚠️ 为什么变更也要收进来：留「只有观测才禁」这个例外，下一个人就得自己判断
// 「我这个算观测还是变更」—— 而那正是例外的成本，它把判断权还回了每一个调用点。
// 本轮四条 findings 里有两条就是从上一个例外长出来的。

/// 读文件全部字节 —— 三态。
///
/// 🔴 读失败不是「空文件」。`Absent` 才是「那里没有」，且同样要过命名空间那一关。
#[allow(clippy::disallowed_methods)]
pub fn read_bytes(path: &Path, anchor_root: Option<&Path>) -> Probed<Vec<u8>> {
    match std::fs::read(path) {
        Ok(v) => Probed::Found(v),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            namespace_confirms_absence(path, &anchor(anchor_root)).map(|_| Vec::new())
        }
        Err(e) => Probed::Unknown(ProbeError::new(path, e)),
    }
}

/// 读文件为 UTF-8 文本 —— 三态。非法 UTF-8 归 `Unknown`：它是「读到了但看不懂」，
/// 与「那里没有」是两件事。
#[allow(clippy::disallowed_methods)]
pub fn read_text(path: &Path, anchor_root: Option<&Path>) -> Probed<String> {
    match std::fs::read_to_string(path) {
        Ok(s) => Probed::Found(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            namespace_confirms_absence(path, &anchor(anchor_root)).map(|_| String::new())
        }
        Err(e) => Probed::Unknown(ProbeError::new(path, e)),
    }
}

/// 一个目录项 —— **不透明**，边界外拿不到 `std::fs::DirEntry`。
///
/// 🔴 **这是「模块面清单」能不能成立的关键**（五轮评审 P2）。上一版返回
/// `Vec<io::Result<DirEntry>>`，于是 `DirEntry` 逃出边界，边界外可以
/// `entry.metadata()` / `entry.file_type()` —— 而它们又是新的 def-path，
/// 清单永远补不完。**只要原始类型会外泄，有限清单就实现不了「整个模块面」。**
/// （生产代码里就有一处：`discover.rs` 当时正在调 `DirEntry::file_type()`。）
///
/// 所以这里把需要的事实**在边界内**取好再交出去：名字、路径、以及类型的三态判定。
#[derive(Debug, Clone)]
pub struct EntryFacts {
    pub file_name: std::ffi::OsString,
    pub path: std::path::PathBuf,
    /// 这一项是什么 —— `file_type()` 自己也会失败，那同样是「没问成」。
    pub kind: Probed<FileKind>,
}

/// 列目录 —— 三态。
///
/// 🔴 **逐条目的错误也保留**：`read_dir` 成功之后每个条目仍可能失败，
/// 而 `.flatten()` 会把它们静默丢掉（`memory/sources.rs` 曾经就是）。
/// 返回 `Vec<Result<EntryFacts, ProbeError>>` 让调用方**看得见**每一条。
#[allow(clippy::disallowed_methods)]
pub fn read_dir_entries(
    path: &Path,
    anchor_root: Option<&Path>,
) -> Probed<Vec<Result<EntryFacts, ProbeError>>> {
    match std::fs::read_dir(path) {
        Ok(it) => Probed::Found(
            it.map(|e| match e {
                Ok(entry) => Ok(EntryFacts {
                    file_name: entry.file_name(),
                    path: entry.path(),
                    kind: match entry.file_type() {
                        Ok(t) if t.is_file() => Probed::Found(FileKind::File),
                        Ok(t) if t.is_dir() => Probed::Found(FileKind::Dir),
                        Ok(_) => Probed::Found(FileKind::Other),
                        Err(err) => Probed::Unknown(ProbeError::new(&entry.path(), err)),
                    },
                }),
                Err(err) => Err(ProbeError::new(path, err)),
            })
            .collect(),
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            namespace_confirms_absence(path, &anchor(anchor_root)).map(|_| Vec::new())
        }
        Err(e) => Probed::Unknown(ProbeError::new(path, e)),
    }
}

/// 读文件的 `[start, end)` 字节区间 —— 三态。
///
/// 🔴 **不再交出 `File`**（五轮评审 P2）。上一版是 `open_read -> Probed<File>`，
/// 而 `File` 逃出边界之后 `f.metadata()` / `f.set_permissions()` 又是新的 def-path。
/// seek + read 整个放在边界内，边界外只拿到字节。
#[allow(clippy::disallowed_methods)]
pub fn read_range(path: &Path, start: u64, end: u64) -> Probed<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Probed::Absent,
        Err(e) => return Probed::Unknown(ProbeError::new(path, e)),
    };
    let mut go = || -> std::io::Result<Vec<u8>> {
        f.seek(SeekFrom::Start(start))?;
        let mut buf = vec![0u8; (end - start) as usize];
        f.read_exact(&mut buf)?;
        Ok(buf)
    };
    match go() {
        Ok(buf) => Probed::Found(buf),
        Err(e) => Probed::Unknown(ProbeError::new(path, e)),
    }
}

fn anchor(root: Option<&Path>) -> Anchor {
    match root {
        Some(r) => Anchor::Under(r.to_path_buf()),
        None => Anchor::None,
    }
}

/// 建目录（含父级）。**变更操作，透传 `Result`** —— 见本节开头。
#[allow(clippy::disallowed_methods)]
pub fn create_dir_all(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path)
}

/// 写文件。**变更操作，透传 `Result`**。
#[allow(clippy::disallowed_methods)]
pub fn write_bytes(path: &Path, contents: impl AsRef<[u8]>) -> std::io::Result<()> {
    std::fs::write(path, contents)
}

/// 改名。**变更操作，透传 `Result`**。
#[allow(clippy::disallowed_methods)]
pub fn rename(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::rename(from, to)
}

/// 改权限。**变更操作，透传 `Result`**。
#[allow(clippy::disallowed_methods)]
pub fn set_permissions(path: &Path, perm: std::fs::Permissions) -> std::io::Result<()> {
    std::fs::set_permissions(path, perm)
}

/// 某个 WSL 发行版内的路径，经访问桥探测。
///
/// 🔴 **为什么要它，而不是让 `LocalBackend` 去问 `\\wsl.localhost\…`**：宿主侧
/// 那条 UNC 路对**大多数**目录确实答得上来，于是这个后端看起来可有可无 ——
/// 直到路上出现一个**符号链接**。实测
/// `/home/simon/workspace/QuotaBar -> /mnt/c/Users/user/workspace/QuotaBar`：
/// 链接的目标是 WSL 内部的挂载点，宿主沿 9P 跟不进去，`metadata` 返回「既不是
/// 文件也不是目录」⇒ 调用方（`decode_project_dir`）读成**「这个项目不存在」**。
/// 后果是同一个目录的项目记忆分裂成互不可见的两半，而界面上一切正常。
///
/// 判据因此不是「UNC 通不通」，是**「这条路径归谁管」** —— 归发行版管的，就该问它。
#[derive(Debug, Clone)]
pub struct WslBackend {
    pub distro: String,
    /// 宿主侧的 UNC 前缀（`\\wsl.localhost\<distro>`）。非空时 [`probe`] 收到的
    /// 路径按它剥掉、`\` 换 `/`，还原成发行版内部的绝对路径再问桥。
    ///
    /// 空 = 调用方**已经**用发行版内部写法寻址（`discover.rs` 就是）。
    ///
    /// [`probe`]: ProbeBackend::probe
    prefix: String,
}

impl WslBackend {
    /// 调用方用发行版内部的绝对路径寻址（`/home/u/x`）。
    pub fn new(distro: impl Into<String>) -> Self {
        Self {
            distro: distro.into(),
            prefix: String::new(),
        }
    }

    /// 调用方用宿主的 UNC 写法寻址（`\\wsl.localhost\<distro>\home\u\x`）。
    ///
    /// 同一条路径的两种写法 —— `pathnorm` 早就把这件事建模了，所以翻译放在这里，
    /// 调用方一行不用改。
    pub fn under_host_prefix(distro: impl Into<String>, prefix: impl Into<String>) -> Self {
        Self {
            distro: distro.into(),
            prefix: prefix.into(),
        }
    }

    /// 宿主写法 → 发行版内部绝对路径。`None` = 这条路径**不在**声明的前缀下，
    /// 本后端答不了它。
    fn to_linux(&self, path: &Path) -> Option<String> {
        let s = path.to_string_lossy();
        // 🔴 **无论走哪一支都要归一分隔符。** 调用方用 `Path::join` 拼路径，而它在
        // Windows 上产出 `\` —— `/home/u/x` join `.git` 得到 `/home/u/x\.git`，
        // 原样送进发行版就是一个不存在的文件名。本后端的**输出契约是 Linux 路径**，
        // 归一是它自己的责任，不该让每个调用方记得先换。
        // ⚠️ 代价：Linux 文件名里合法的 `\` 会被误换。实践中本仓的路径全部来自
        // git / agent 目录，不含反斜杠文件名；而不归一则整条路根本用不了。
        if self.prefix.is_empty() {
            return Some(s.replace('\\', "/"));
        }
        // Windows 路径大小写不敏感，而前缀来自配置、路径来自拼接 —— 两者大小写
        // 不一致是可预期的，不该因此答不出来。
        //
        // 前缀是 ASCII（`\\wsl.localhost\<distro>`），所以匹配成立时 `prefix.len()`
        // 必然落在字符边界上，切片不会 panic。
        if !s
            .to_ascii_lowercase()
            .starts_with(&self.prefix.to_ascii_lowercase())
        {
            return None;
        }
        let linux = s[self.prefix.len()..].replace('\\', "/");
        Some(if linux.starts_with('/') {
            linux
        } else {
            format!("/{linux}")
        })
    }
}

impl ProbeBackend for WslBackend {
    /// `wsl::stat_kind` 已经是三态的（`Ok(Some)` / `Ok(None)` / `Err`），这里只做翻译。
    fn probe(&self, path: &Path, deadline: Deadline) -> Probed<FileKind> {
        // 🔴 **答不了 ≠ 这里没有。** 前缀对不上说明调用方拿错了命名空间；
        // 报 `Absent` 会让它据此删数据 —— 那正是本类型存在的理由的反面。
        let Some(linux_path) = self.to_linux(path) else {
            return Probed::Unknown(ProbeError::new(
                path,
                format!(
                    "not under this backend's declared WSL prefix {:?} (distro {})",
                    self.prefix, self.distro
                ),
            ));
        };
        match crate::wsl::stat_kind(&self.distro, &linux_path, deadline) {
            Ok(Some(crate::wsl::PathKind::Dir)) => Probed::Found(FileKind::Dir),
            Ok(Some(crate::wsl::PathKind::File)) => Probed::Found(FileKind::File),
            Ok(Some(crate::wsl::PathKind::Other)) => Probed::Found(FileKind::Other),
            Ok(None) => Probed::Absent,
            Err(e) => Probed::Unknown(ProbeError::new(path, e)),
        }
    }

    /// 经访问桥在发行版**内部**读 —— `wsl::read_file_at` 本来就是三态
    /// （`Ok(Some)` / `Ok(None)` / `Err`），这里只做翻译。
    ///
    /// ⚠️ **没有 `namespace_confirms_absence` 那一步，也不需要**：桥的
    /// `Ok(None)` 已经是发行版自己说的「没有」，而发行版能应答本身就证明了
    /// 命名空间可达 —— 桥不通时它给的是 `Err`。
    fn read_text(&self, path: &Path, deadline: Deadline) -> Probed<String> {
        let Some(linux_path) = self.to_linux(path) else {
            return Probed::Unknown(ProbeError::new(
                path,
                format!(
                    "not under this backend's declared WSL prefix {:?} (distro {})",
                    self.prefix, self.distro
                ),
            ));
        };
        match crate::wsl::read_file_at(&self.distro, &linux_path, deadline) {
            Ok(Some(t)) => Probed::Found(t),
            Ok(None) => Probed::Absent,
            Err(e) => Probed::Unknown(ProbeError::new(path, e)),
        }
    }
}

/// 宿主 UNC 探测，**只在一个答案上**回落到发行版权威。
///
/// 🔴 **兜底只针对 [`FileKind::Other`]。** 宿主对发行版内的路径不是万能的，但也
/// 不是全无用处：只要父目录可遍历，`Dir` / `File` / `Absent` 都是事实。唯一不可信的
/// 是「有东西，但既不是文件也不是目录」—— 它几乎总是一个**宿主跟不进去的符号链接**，
/// 而链接的另一头完全可能是目录。实测
/// `/home/simon/workspace/QuotaBar -> /mnt/c/Users/user/workspace/QuotaBar`：宿主沿
/// 9P 跟不进那个挂载点，`metadata` 返回既非文件也非目录，`decode_project_dir` 于是
/// 读成**「这个项目不存在」**，同一个目录的项目记忆分裂成互不可见的 37 + 24 两半。
///
/// 🔴 **为什么不干脆全走桥**：每次探测都要起一个 `wsl.exe`，实测一次往返 ≈1.5 秒，
/// 而 `decode_project_dir` 对一条四段路径要问 10 个候选 —— 全走桥是「7 个项目
/// 30 秒的界面卡顿」，兜底是「**每个符号链接一次**」。这个取舍成立的前提正是上一段
/// 那句话：宿主给的另外三种答案是事实，不需要复核。前提若变（比如将来要探
/// 宿主根本挂不上的发行版），这里就该整体换成 [`WslBackend`]。
pub struct WslUncBackend {
    host: Box<dyn ProbeBackend>,
    authority: Box<dyn ProbeBackend>,
}

impl WslUncBackend {
    /// `prefix` 是 ADR-033 的 `fs_prefix`（`\\wsl.localhost\<distro>`）。
    pub fn new(distro: impl Into<String>, prefix: impl Into<String>) -> Self {
        Self {
            host: Box::new(LocalBackend::unanchored()),
            authority: Box::new(WslBackend::under_host_prefix(distro, prefix)),
        }
    }

    /// 两侧都注入 —— **组合逻辑本身要可测**。
    ///
    /// 🔴 只测一个「宿主答案够不够用」的纯谓词是**假护栏**：把 `probe` 的调用点改回
    /// 无条件信任宿主，那种测试照样全绿（本仓判例：断言 `transport_error` 返回什么，
    /// 而调用点改回裸 `format!` 依然通过）。判据必须打在真正跑的那条路上。
    pub fn with_backends(host: Box<dyn ProbeBackend>, authority: Box<dyn ProbeBackend>) -> Self {
        Self { host, authority }
    }
}

impl ProbeBackend for WslUncBackend {
    fn probe(&self, path: &Path, deadline: Deadline) -> Probed<FileKind> {
        match self.host.probe(path, deadline) {
            // 符号链接**本身**：宿主看到「有东西，但既不是文件也不是目录」。
            Probed::Found(FileKind::Other) => self.authority.probe(path, deadline),
            // 符号链接**下面**的路径：宿主给 `ERROR_DIRECTORY`（raw 267）⇒ `Unknown`。
            // 🔴 这一支是补上去的 —— 只兜 `Other` 时，解码修好了，紧接着
            // `find_git_root` 探 `<项目>/.git` 照样报「探不动」。**同一个符号链接，
            // 两种症状**：踩在它身上是 `Other`，穿过它是 `Unknown`。
            //
            // 兜 `Unknown` 不违反上面「另外三种答案是事实」那句话：`Unknown` 本来就
            // 不是答案。代价也可控 —— 宿主对发行版内的路径极少答不出来，答不出来时
            // 本就该问权威。
            Probed::Unknown(_) => self.authority.probe(path, deadline),
            other => other,
        }
    }

    /// **与 [`Self::probe`] 同构**：宿主答不上来（`Unknown`）才回落权威。
    ///
    /// ⚠️ 读取比探测少一支 —— 没有 `FileKind::Other` 的对应物（读一个符号链接，
    /// 宿主要么读到内容、要么给错误）。**保持两个方法的回落条件一致**，
    /// 否则「探测说在、读取说不在」会重新造出这个类型要消除的那种不一致。
    fn read_text(&self, path: &Path, deadline: Deadline) -> Probed<String> {
        match self.host.read_text(path, deadline) {
            Probed::Unknown(_) => self.authority.read_text(path, deadline),
            other => other,
        }
    }
}

#[cfg(test)]
// 测试要造 fixture（建目录、写文件、再核一遍），允许直接碰盘 —— 存在性边界管的是
// **生产行为**，而 `#[cfg(test)]` 不在生产路径上。允许写在模块上而不是逐个函数：
// 下一条测试不必再想一遍这件事，而生产代码里加一行照样会被 clippy 拦。
#[allow(clippy::disallowed_methods)]
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

        let b = LocalBackend::unanchored();
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
            LocalBackend::unanchored().probe(&file, expired),
            Probed::Unknown(_)
        ));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// 🔴 **未挂载的盘符不是「这里没有」**（三轮评审 P1-1）。
    ///
    /// Windows 上它返回 `ErrorKind::NotFound`（raw 3），与「盘符在、文件确实没有」
    /// **逐位相同**。只按 `io::ErrorKind` 分类的实现无法区分，于是一次盘符掉线会被
    /// 读作「用户把这些会话删了」，prune 随即执行。
    ///
    /// 用**真实的**未挂载盘符驱动 —— 与 `unreachable_client()` 走死代理、WSL 那条走
    /// 不存在的发行版同一个手法：驱动真实路径，不伪造 `io::Error`。
    ///
    /// ⚠️ 两端都断言：未挂载盘符 ⇒ `Unknown`，**且**已挂载盘符下的缺失仍是 `Absent`。
    /// 少了后者，一个「凡 NotFound 都报 Unknown」的实现照样绿 —— 而那会让每个没装
    /// 某 CLI 的用户永久带着假故障、prune 全被禁掉。
    #[test]
    #[cfg(windows)]
    fn an_unmounted_drive_is_unknown_not_absent() {
        let free = ('D'..='Z').find(|d| {
            std::fs::metadata(format!("{d}:\\")).is_err()
                && std::path::Path::new(&format!("{d}:\\")).ancestors().count() == 1
        });
        let Some(drive) = free else {
            eprintln!("跳过：本机没有空闲盘符可用来复现");
            return;
        };
        let b = LocalBackend::unanchored();
        let d = Deadline::unbounded();

        let leaf = std::path::PathBuf::from(format!("{drive}:\\home\\u\\.claude\\projects"));
        assert!(
            matches!(b.probe(&leaf, d), Probed::Unknown(_)),
            "未挂载盘符 {drive}: 下的路径被判成「确认不存在」—— prune 会据此删数据"
        );

        // 反向：盘符在，文件确实没有 ⇒ 仍是事实。
        let missing = std::env::temp_dir().join("sv-probe-definitely-absent-xyz-123");
        let _ = std::fs::remove_file(&missing);
        assert_eq!(
            b.probe(&missing, d),
            Probed::Absent,
            "已挂载盘符下的缺失必须仍是 Absent，否则没装 CLI 的用户永久带着假故障"
        );
    }

    /// 🔴 **锚点不可达 ⇒ `Unknown`，哪怕语法根好好的**（三轮评审 P1）。
    ///
    /// 这条**在任何平台上都确定性可复现**，而上面那条未挂载盘符的只在 Windows 上跑
    /// —— 差别正是这次要修的东西：`ancestors().last()` 在 Unix 上永远是 `/`，
    /// 于是 `/mnt/work/.claude` 所在的卷被卸载时，语法根那一支**看不出任何异常**，
    /// 报 `Absent` ⇒ 该位置不进 `unreachable` ⇒ prune 删掉会话与用量投影。
    ///
    /// 「锚点整个不见了」正是卷卸载的签名：**分不出「用户删了它」还是「它被卸载了」**，
    /// 所以只能说不知道。
    ///
    /// ⚠️ 三端都断言：锚点在 ⇒ 叶子缺失仍是 `Absent`（否则每个没写过 CLAUDE.md 的
    /// 项目都变成假故障）；锚点不在 ⇒ `Unknown`；**同一条路径换成 `unanchored()`
    /// 会得到 `Absent`** —— 最后一条钉的是「这个修复确实改变了行为」，
    /// 少了它，一个 `rooted_at` 与 `unanchored` 行为相同的实现照样绿。
    #[test]
    fn an_unreachable_anchor_is_unknown_even_when_the_syntactic_root_is_fine() {
        let tmp = std::env::temp_dir().join("sv-probe-anchor-test");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let d = Deadline::unbounded();
        let leaf = tmp.join("gone").join(".claude").join("projects");

        // 锚点在（`tmp` 存在）⇒ 叶子确实没有，是事实。
        assert_eq!(
            LocalBackend::rooted_at(&tmp).probe(&leaf, d),
            Probed::Absent,
            "锚点可达时，叶子的缺失必须仍是事实 —— 否则没装 CLI 的用户永久带假故障"
        );

        // 🔴 **根不见了 ⇒ 仍是 `Absent`**（五轮评审 P1）。
        //
        // 「只装了 Codex、从来没有 `~/.claude`」是完全正常的配置；判成不可达会让
        // QuotaBar **永不 prune local**。而「卷卸载」在路径语法层与它不可区分
        // （Unix 卸载后挂载点目录还在），所以这里**不猜** —— 真答案要靠持久化的
        // 卷/设备身份（task #15）。两条都断言，钉住「不猜」这个决定本身。
        let never_installed = tmp.join("gone");
        assert_eq!(
            LocalBackend::rooted_at(&never_installed).probe(&leaf, d),
            Probed::Absent,
            "根不存在 = 那个 CLI 没装（或卷卸载，此层不可区分）—— 判成不可达会让 prune 永久停摆"
        );
        let deep_missing = tmp.join("vanished-volume").join("dot-claude");
        assert_eq!(
            LocalBackend::rooted_at(&deep_missing).probe(&deep_missing.join("projects"), d),
            Probed::Absent,
            "根与父目录都不在，同样不许升级成 Unknown —— 那正是 `.config` 未建立时\
             永久阻断 prune 的形状"
        );

        // 🔴 同一条路径，`unanchored()` 看不出异常 —— 这正是 Unix 上的那个洞，
        // 也是「锚点必须由调用方给」的理由。
        assert_eq!(
            LocalBackend::unanchored().probe(&leaf, d),
            Probed::Absent,
            "前提：语法根那一支确实说不出来 —— 少了这条，rooted_at 与 unanchored \
             行为相同的实现也能让上面两条通过"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// 死掉的 UNC 主机同样不是「这里没有」—— 实测 raw 53（`ERROR_BAD_NETPATH`）
    /// 也被映射成 `NotFound`。这条不枚举错误码，靠的是同一条「根要可达」判据。
    #[test]
    #[cfg(windows)]
    fn a_dead_unc_host_is_unknown_not_absent() {
        let p = std::path::Path::new(r"\\no-such-host-xyz-quotabar\share\f.txt");
        assert!(
            matches!(
                LocalBackend::unanchored().probe(p, Deadline::unbounded()),
                Probed::Unknown(_)
            ),
            "不可达的 UNC 主机被判成「确认不存在」"
        );
    }

    // ── WSL：宿主写法 → 发行版内部路径 ────────────────────────────────────

    const PREFIX: &str = r"\\wsl.localhost\Ubuntu-22.04";

    #[test]
    fn a_host_unc_path_becomes_the_in_distro_absolute_path() {
        let b = WslBackend::under_host_prefix("Ubuntu-22.04", PREFIX);
        assert_eq!(
            b.to_linux(Path::new(&format!(
                r"{PREFIX}\home\simon\workspace\QuotaBar"
            ))),
            Some("/home/simon/workspace/QuotaBar".to_string())
        );
        // 前缀本身 ⇒ 发行版根。
        assert_eq!(b.to_linux(Path::new(PREFIX)), Some("/".to_string()));
    }

    /// Windows 路径大小写不敏感，而前缀来自配置、路径来自拼接。
    #[test]
    fn the_prefix_match_ignores_case() {
        let b = WslBackend::under_host_prefix("Ubuntu-22.04", PREFIX);
        assert_eq!(
            b.to_linux(Path::new(r"\\WSL.LOCALHOST\ubuntu-22.04\home\u")),
            Some("/home/u".to_string())
        );
    }

    /// 🔴 **答不了 ≠ 这里没有。** 前缀对不上说明调用方拿错了命名空间 ——
    /// 报 `Absent` 会让它据此删数据。
    ///
    /// ⚠️ **`Unknown` 本身不足以当判据**（变异时发现）：把「对不上就原样放行」写进
    /// `to_linux`，路径会被原样送进 `wsl.exe`，那边找不到也报 `Unknown` ——
    /// 断言照样绿，而实际白 spawn 了一个进程。所以判据是**错误说的是哪件事**。
    #[test]
    fn a_path_outside_the_prefix_is_unknown_not_absent() {
        let b = WslBackend::under_host_prefix("Ubuntu-22.04", PREFIX);
        assert_eq!(b.to_linux(Path::new(r"C:\Users\u\proj")), None);
        match b.probe(Path::new(r"C:\Users\u\proj"), Deadline::unbounded()) {
            Probed::Unknown(e) => assert!(
                e.to_string()
                    .contains("not under this backend's declared WSL prefix"),
                "必须是「命名空间不对」而不是「问了发行版、它说没有」。实际：{e}"
            ),
            other => panic!("前缀对不上必须报 Unknown，实际：{other:?}"),
        }
    }

    /// 空前缀 = 调用方已经用发行版内部写法寻址（`discover.rs` 就是）。
    #[test]
    fn an_empty_prefix_passes_the_path_through() {
        let b = WslBackend::new("Ubuntu-22.04");
        assert_eq!(
            b.to_linux(Path::new("/home/u/x")),
            Some("/home/u/x".to_string())
        );
    }

    // ── WslUncBackend：只在两种答案上回落到权威 ────────────────────────────

    struct Fixed(Probed<FileKind>);
    impl ProbeBackend for Fixed {
        fn probe(&self, _p: &Path, _d: Deadline) -> Probed<FileKind> {
            self.0.clone()
        }
        /// 本后端只为测 `probe` 的组合逻辑而存在 —— 读取用 [`FixedText`]。
        ///
        /// 🔴 **panic 而不是 `Unknown`**：返回 `Unknown` 会让一条断言「结果是
        /// `Unknown`」的测试在探测那一步被改坏之后照样通过 —— 读取那一步替它
        /// 产出了同一个值。那是假护栏。
        fn read_text(&self, p: &Path, _d: Deadline) -> Probed<String> {
            panic!("{p:?}: Fixed only answers probes; a read here means the test changed shape")
        }
    }

    /// 读取侧的对照后端 —— 与 [`Fixed`] 分开，免得一个桩同时决定两件事。
    struct FixedText(Probed<String>);
    impl ProbeBackend for FixedText {
        fn probe(&self, _p: &Path, _d: Deadline) -> Probed<FileKind> {
            Probed::Found(FileKind::File)
        }
        fn read_text(&self, _p: &Path, _d: Deadline) -> Probed<String> {
            self.0.clone()
        }
    }

    fn composed(host: Probed<FileKind>) -> Probed<FileKind> {
        WslUncBackend::with_backends(
            Box::new(Fixed(host)),
            // 权威答「目录」—— 与宿主给的任何一种答案都不同，所以「问没问权威」
            // 在结果里看得出来。
            Box::new(Fixed(Probed::Found(FileKind::Dir))),
        )
        .probe(
            Path::new(r"\\wsl.localhost\D\home\u\p"),
            Deadline::unbounded(),
        )
    }

    /// 🔴 **符号链接本身**：宿主说「有东西，但既不是文件也不是目录」。
    ///
    /// 实测 `/home/simon/workspace/QuotaBar -> /mnt/c/Users/user/workspace/QuotaBar`，
    /// 宿主沿 9P 跟不进那个挂载点。把这个答案当「不是目录」处理，`decode_project_dir`
    /// 就报「这个项目不存在」，同一个目录的记忆分裂成互不可见的两半。
    #[test]
    fn an_unfollowable_link_asks_the_authority() {
        assert_eq!(
            composed(Probed::Found(FileKind::Other)),
            Probed::Found(FileKind::Dir)
        );
    }

    /// 🔴 **符号链接下面的路径**：宿主给 `ERROR_DIRECTORY`（raw 267）⇒ `Unknown`。
    ///
    /// 同一个链接，两种症状：踩在它身上是 `Other`，穿过它是 `Unknown`。只兜前者时，
    /// 解码刚修好，紧接着 `find_git_root` 探 `<项目>/.git` 照样报「探不动」。
    #[test]
    fn a_failed_host_answer_asks_the_authority() {
        let e = ProbeError::new(Path::new("/p"), "os error 267");
        assert_eq!(composed(Probed::Unknown(e)), Probed::Found(FileKind::Dir));
    }

    /// 🔴 **反向：宿主答得上来的三种，不许再问权威。**
    ///
    /// 少了这一条，「无脑全走桥」的实现照样让上面两条通过 —— 而那是每次探测一个
    /// `wsl.exe`（实测一次往返 ≈1.5 秒），解码一条四段路径要问 10 个候选。
    #[test]
    fn the_host_is_trusted_when_it_can_answer() {
        for host in [
            Probed::Found(FileKind::Dir),
            Probed::Found(FileKind::File),
            Probed::Absent,
        ] {
            let authority_would_say = Probed::Found(FileKind::Dir);
            let got = composed(host.clone());
            assert_eq!(got, host, "宿主答得上来时不该改写它的答案");
            // `Found(Dir)` 那一格上两者恰好相同，说明不了「没问权威」—— 用
            // `File`/`Absent` 那两格承担判据，这里只是把意图写出来。
            let _ = authority_would_say;
        }
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
