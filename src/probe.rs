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
    if let Anchor::Under(root) = anchor {
        match std::fs::metadata(root) {
            // 根在 ⇒ 叶子的缺失是事实。
            Ok(_) if root != path => return Probed::Absent,
            Ok(_) => {}
            // 🔴 **根自己不见了，还不足以定案**（四轮评审 P1）。
            //
            // 上一版在这里直接报 `Unknown`，而 `claude_config_dir()` /
            // `codex_config_dir()` **目录不存在也返回 `Some(~/.claude)`** ——
            // 于是「只装了 Codex、从来没有 `~/.claude`」这个**完全正常的配置**
            // 被判成 local 不可达，QuotaBar 因此**永不 prune local**，
            // 已删除的 Codex 会话 / 用量 / 成本永久残留。
            // 方向与它要修的那个 bug **正好相反**：修的是误删，造出的是永不删。
            //
            // 分开这两种状态**不需要持久化状态**，只要再上溯一层：
            // - 父目录在（`~` 在、`~/.claude` 没有）⇒ 那个 CLI 没装，**是事实**；
            // - 父目录也不见了（`/mnt/work` 整个没了）⇒ 命名空间掉了，**没问成**。
            //
            // 只上溯**一层**，不是一路走到 `/` —— 走到 `/` 又会回到「Unix 上恒可达」
            // 那个洞里。一层就够分开这两种，且可解释。
            Err(_) => {
                let Some(parent) = root.parent().filter(|p| !p.as_os_str().is_empty()) else {
                    return Probed::Unknown(ProbeError::new(
                        path,
                        "source root is gone and has no parent to check against",
                    ));
                };
                return match std::fs::metadata(parent) {
                    Ok(_) => Probed::Absent,
                    Err(e) => Probed::Unknown(ProbeError::new(
                        path,
                        format!(
                            "source root {} and its parent are both unreachable: {e}",
                            root.display()
                        ),
                    )),
                };
            }
        }
        // 锚点自己就是被探的那条路径，且它在 —— 交回上面的常规判定。
        return Probed::Absent;
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

/// 列目录 —— 三态。
///
/// 🔴 **逐条目的错误也保留**：`read_dir` 成功之后每个 `DirEntry` 仍可能失败，
/// 而 `.flatten()` 会把它们静默丢掉（`memory/sources.rs` 曾经就是）。
/// 返回 `Vec<io::Result<DirEntry>>` 让调用方**看得见**每一条。
#[allow(clippy::disallowed_methods)]
pub fn read_dir_entries(
    path: &Path,
    anchor_root: Option<&Path>,
) -> Probed<Vec<std::io::Result<std::fs::DirEntry>>> {
    match std::fs::read_dir(path) {
        Ok(it) => Probed::Found(it.collect()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            namespace_confirms_absence(path, &anchor(anchor_root)).map(|_| Vec::new())
        }
        Err(e) => Probed::Unknown(ProbeError::new(path, e)),
    }
}

/// 打开文件用于随机读（`scan.rs` 的 ranged read）—— 三态。
#[allow(clippy::disallowed_methods)]
pub fn open_read(path: &Path, anchor_root: Option<&Path>) -> Probed<std::fs::File> {
    match std::fs::File::open(path) {
        Ok(f) => Probed::Found(f),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            match namespace_confirms_absence(path, &anchor(anchor_root)) {
                Probed::Absent => Probed::Absent,
                Probed::Unknown(e) => Probed::Unknown(e),
                Probed::Found(_) => Probed::Absent,
            }
        }
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

        // 🔴 **根不见了、但父目录在 ⇒ 事实**（四轮评审 P1）。
        // 「只装了 Codex，从来没有 `~/.claude`」是完全正常的配置；把它判成不可达，
        // QuotaBar 就**永不 prune local**，已删除的会话/用量/成本永久残留。
        let never_installed = tmp.join("gone");
        assert_eq!(
            LocalBackend::rooted_at(&never_installed).probe(&leaf, d),
            Probed::Absent,
            "根不存在但父目录在 = 那个 CLI 没装，是事实 —— 判成不可达会让 prune 永久停摆"
        );

        // 根与父目录**都**不见了（= 卷被卸载的签名）⇒ 说不出叶子在不在。
        let unmounted = tmp.join("vanished-volume").join("dot-claude");
        assert!(
            matches!(
                LocalBackend::rooted_at(&unmounted).probe(&unmounted.join("projects"), d),
                Probed::Unknown(_)
            ),
            "来源根与父目录都不可达却报了「确认不存在」—— prune 会据此删数据"
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
