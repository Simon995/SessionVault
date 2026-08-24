//! 项目的**规范身份**——一个跨系统、跨 checkout 稳定的 id（ADR-032）。
//!
//! ## 为什么身份必须与路径分开
//!
//! 同一个仓库在这台机器上可能有好几份 checkout：Windows 一份、WSL 一份、外接盘一份。
//! 它们的**路径互不相同**，而用户心里那是**一个项目** —— 在 WSL 里学到的东西，切到
//! Windows 那份接着干时理应还在。所以「哪些路径是同一个项目」不能靠路径字符串判断，
//! 得靠仓库自己的身份：**git origin remote**。
//!
//! [`resolve_project_root`](crate::project_root::resolve_project_root) 回答的是
//! 「这次会话在哪个**目录**」；本模块回答的是「那个目录属于哪个**项目**」。两者都需要，
//! 但它们是不同的问题，混在一起会让「同一个项目的两份 checkout」永远合不到一起。
//!
//! ## 只读 `.git/config`，从不 spawn git
//!
//! 一次 `git remote get-url` 是一个进程；扫描时每个项目都要问一次，而 SessionVault
//! 的调用方里有 GUI 的 setup 路径。读那个 ini 文件是纯文件 IO，跨平台、可测、
//! 在没装 git 的机器上照样工作 —— 而**存在 `.git/config` 却没装 git** 是常见的
//! （容器、精简镜像、别人拷来的目录）。
//!
//! ## 🔴 拿不到 origin 时退回路径，且**这件事要看得出来**
//!
//! 没有 remote 的本地仓库（`git init` 之后没 add remote）是完全正常的状态。那时
//! 身份退回 `path:<git root>` —— 它**不跨 checkout 稳定**（同一个仓的另一份拷贝会得到
//! 另一个 id），但那不是缺陷，是「这个仓库确实没有可跨机器识别的身份」的忠实反映。
//! 前缀 `git:` / `path:` 让消费方一眼看得出自己拿到的是哪一种，而不是拿到一个
//! 看起来一样、稳定性却完全不同的串。
//!
//! ## 边界：本模块**只认磁盘上现在有什么**
//!
//! checkout 被删除之后，`.git/config` 也没了 ⇒ 这里再也算不出它的 `git:` 身份。
//! 实测（2026-08-11，QuotaBar 侧）：事件量最大的那个项目**没有别名组**，正是因为它的
//! WSL checkout 已被删除，而总库里还留着它 16 万条历史事件。
//!
//! ⇒ **要让身份活过 checkout 的删除，得在扫描时把它记下来**（那时 `.git` 还在），
//! 而不是每次现算。那是本模块之后的一步，不在这里。

use std::path::{Path, PathBuf};

use crate::deadline::Deadline;
use crate::probe::{FileKind, ProbeBackend, Probed};

/// 一次 git 根查找的结果 —— **三态**。
///
/// 🔴 从前是 `Option<PathBuf>`，而两处 `.exists()` 把「没问成」折叠进了 `None`。
/// 后果不是崩溃，是**静默**：`store.rs` 的 `record_identity_for_root` 拿到 `None` 就
/// `return`，且它**先记后算**（`identity_seen` 在计算前就插了 key）—— 于是一次
/// 权限错误 / UNC 不通让这个项目在本进程生命周期内**永远**算不出 `git:` 身份，
/// 没有别名组，跨 checkout 的 Class-A 证据在 project 作用域里蒸发。
/// 而界面上什么都不会说。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitRoot {
    Found(PathBuf),
    /// 探明白了：起点已不在磁盘上，或整条链上没有 `.git`。**这是事实。**
    Absent,
    /// **没问成** —— 本轮这个答案不作数，别据此写「这个项目没有身份」。
    Unknown(crate::probe::ProbeError),
}

/// 从 `start` 向上找最近的含 `.git` 的目录。
///
/// **起点不存在时返回 [`GitRoot::Absent`]**，而不是继续向上走 —— 一个已被删除的
/// checkout 的父目录可能恰好是另一个仓库，那样会把它的身份安到一个不相干的项目上。
///
/// 🔴 **一层没问成就停在那里**（ADR-051 §5 规则 ③，与 `discovery::probe_local_with`
/// 同一条）：继续上溯会把一个**错误的归属**说成成功 —— `/w/proj/sub` 的 `.git` 读不到、
/// 于是上溯到 `/w/proj` 命中，可 `sub` 很可能本来就有 `.git`。报 `Unknown` 走重试，
/// 错误归属只会安静地留在库里。
pub fn find_git_root(start: &Path) -> GitRoot {
    // ⚠️ 本机 stat 本来就不宣称硬超时（见 `LocalBackend::probe`），所以这个便捷
    // 包装用 `unbounded` 是**忠实**的，不是省事。走桥的调用方必须自己给上限。
    find_git_root_with(
        start,
        &crate::probe::LocalBackend::unanchored(),
        Deadline::unbounded(),
    )
}

/// [`find_git_root`] 的可测形态 —— **backend 注入**（「探测失败」在本机造不出来）。
pub fn find_git_root_with(start: &Path, backend: &dyn ProbeBackend, d: Deadline) -> GitRoot {
    match backend.probe(start, d) {
        Probed::Found(_) => {}
        Probed::Absent => return GitRoot::Absent,
        Probed::Unknown(e) => return GitRoot::Unknown(e),
    }
    let mut cur = Some(start);
    while let Some(dir) = cur {
        match backend.probe(&dir.join(".git"), d) {
            // `.git` 是**文件**时同样成立（子模块 / worktree）。
            Probed::Found(_) => return GitRoot::Found(dir.to_path_buf()),
            Probed::Absent => {}
            Probed::Unknown(e) => return GitRoot::Unknown(e),
        }
        cur = dir.parent();
    }
    GitRoot::Absent
}

/// 定位真正的 `config` 文件。
///
/// `.git` 可能是**目录**，也可能是**文件** —— linked worktree 与 submodule 的
/// `.git` 是一行 `gitdir: <path>`。
///
/// 🔴 上一版固定读 `git_root/.git/config`，而 [`find_git_root`] **明确认可** `.git`
/// 文件（子模块 / worktree 的注释就写在那里）。两处对同一个概念的理解不一致，
/// 后果是确定性的、不是偶发的：**每一个 linked worktree 都永远退回 `path:` 身份**，
/// 而 `store::record_identity_for_root` 只写 `git:` 前缀的行 ⇒ 这些项目**从来**没有过
/// 跨 checkout 身份。这正是本模块存在的理由，却在它自己的读取路径上失效了。
///
/// linked worktree 的 config 在 **commondir**（`<gitdir>/commondir` 指向主仓的
/// `.git`）；submodule 的 config 就在 `<gitdir>/config`。两种都覆盖。
/// 解一个 git 内部引用（`.git` 文件的 `gitdir:`、`<gitdir>/commondir`）：
/// 绝对就原样用，相对就挂到 `base` 下。
///
/// 🔴 **不能用 `Path::is_absolute()`** —— 那是**宿主平台**的语义，而本模块处理的
/// 路径可能属于**别的命名空间**（经访问桥寻址的发行版内部路径）。
/// Windows 上 `/home/u/x`.is_absolute() 是 **false**，于是
/// `base.join("/home/u/x")` 会**拼接**成 `base\/home/u/x` 而不是替换 ——
/// 而 git 为 linked worktree 写进 `.git` 的正是**绝对**路径
/// （`gitdir: /home/u/repo/.git/worktrees/wt`）。
///
/// ⚠️ **`..` 不在这里解**：`commondir` 几乎总是 `../..`，而**真实的 FS 与发行版都在
/// OS 层解它** —— 本机 `metadata` 解，`bash`/`stat` 也解。在这里自己解一遍，
/// 就是第二份 path 语义实现（还会在符号链接上给出与 OS 不同的答案）。
fn join_git_ref(base: &Path, raw: &str) -> PathBuf {
    let p = Path::new(raw);
    // POSIX 绝对（`/…`）与宿主绝对（`C:\…` / UNC）都算 —— 判据是「它自带根」，
    // 不是「当前平台认不认它」。
    if p.is_absolute() || raw.starts_with('/') {
        p.to_path_buf()
    } else {
        base.join(p)
    }
}

fn git_config_path(git_root: &Path, backend: &dyn ProbeBackend, d: Deadline) -> Probed<PathBuf> {
    let dot_git = git_root.join(".git");
    match backend.probe(&dot_git, d) {
        Probed::Found(FileKind::Dir) => Probed::Found(dot_git.join("config")),
        Probed::Found(_) => {
            // `.git` 文件：`gitdir: <path>`（可相对 git_root）。
            let text = match backend.read_text(&dot_git, d) {
                Probed::Found(t) => t,
                Probed::Absent => return Probed::Absent,
                Probed::Unknown(e) => return Probed::Unknown(e),
            };
            let Some(rel) = text
                .lines()
                .find_map(|l| l.trim().strip_prefix("gitdir:"))
                .map(str::trim)
            else {
                // 是文件但不是 gitdir 指针 —— 探明白了，这里没有可读的 git 配置。
                return Probed::Absent;
            };
            let gitdir = join_git_ref(git_root, rel);
            // linked worktree：真正的 config 在 commondir 里。
            let commondir = gitdir.join("commondir");
            match backend.probe(&commondir, d) {
                Probed::Found(_) => match backend.read_text(&commondir, d) {
                    Probed::Found(c) => {
                        let c = c.trim();
                        let common = join_git_ref(&gitdir, c);
                        Probed::Found(common.join("config"))
                    }
                    Probed::Absent => Probed::Absent,
                    Probed::Unknown(e) => Probed::Unknown(e),
                },
                // 没有 commondir ⇒ submodule 形态，config 就在 gitdir 里。
                Probed::Absent => Probed::Found(gitdir.join("config")),
                Probed::Unknown(e) => Probed::Unknown(e),
            }
        }
        Probed::Absent => Probed::Absent,
        Probed::Unknown(e) => Probed::Unknown(e),
    }
}

/// 读 origin url —— **三态**。
///
/// 🔴 上一版是 `Option<String>` + `.ok()?`，把「这个仓确实没配 origin」与
/// 「config 这一刻读不了（权限 / UNC 不通 / 句柄耗尽）」压成同一个 `None`。
/// 而 `store::record_identity_for_root` **先记后算**：拿到 `path:` 就直接 return，
/// 不撤销 `identity_seen` ⇒ 一次瞬时故障让该项目在**整个进程生命周期**内拿不到
/// 稳定身份。这是 [`find_git_root`] 那次三态化只做了一半 —— stat 阶段分开了，
/// 紧接着的读取阶段又合上了。
pub fn read_origin_url(git_root: &Path) -> Probed<String> {
    read_origin_url_with(
        git_root,
        &crate::probe::LocalBackend::unanchored(),
        Deadline::unbounded(),
    )
}

/// [`read_origin_url`] 的可测形态 —— backend 注入。
pub fn read_origin_url_with(
    git_root: &Path,
    backend: &dyn ProbeBackend,
    d: Deadline,
) -> Probed<String> {
    let config = match git_config_path(git_root, backend, d) {
        Probed::Found(p) => p,
        Probed::Absent => return Probed::Absent,
        Probed::Unknown(e) => return Probed::Unknown(e),
    };
    read_origin_from_config(&config, backend, d)
}

/// 从**已定位的** config 文件读 origin —— [`read_origin_url_with`] 的后半段。
///
/// 🔴 **拆出来是因为 `Probed<String>` 的 `Absent` 装了两个意思**：
/// 「这条链上没有可用的 `.git`」与「读到了 config、里面没有 origin」。
/// 本机唯一的消费者（`canonical_repo_id_with`）把两者都映射成 `path:`，所以不在乎；
/// 而 [`wsl_repo_id`] 要靠这个区别决定 `repo_root` 是 `Some` 还是 `None`。
///
/// 让它自己再写一遍下面这段（尤其那个 fail-safe）就是第二份实现 —— 那正是本轮
/// 要消除的东西。
fn read_origin_from_config(
    config: &Path,
    backend: &dyn ProbeBackend,
    d: Deadline,
) -> Probed<String> {
    // 先探再读：`NotFound` 是「没有 config」（事实），其余是「没问成」。
    match backend.probe(config, d) {
        Probed::Found(_) => {}
        Probed::Absent => return Probed::Absent,
        Probed::Unknown(e) => return Probed::Unknown(e),
    }
    // 🔴 **判决链中途不许换事实来源**（五轮评审 P2）。
    //
    // 上一行刚用 `backend` 确认 config **在**，从前这里却用全局 `probe::read_text`
    // 去读 —— 两个来源不一致时（注入后端说远端可达而本机读不到、或文件正好在两步
    // 之间消失），读到的 `NotFound` 会被降级成 `Absent` ⇒ `canonical_repo_id_with`
    // 给出**终态** `path:`，而 `identity_seen` 从此不再重试。
    //
    // ✅ **2026-08-20 起读取也走 `backend`**（`read_text` 已上 `ProbeBackend`）。
    // ⚠️ **下面那个 fail-safe 保留**，它现在防的是另一件事：同一个 backend
    // 「刚说在、紧接着读不到」。那**仍然不是「这个仓没配 origin」** —— 是「没问成」。
    //
    // 🔴 从前的处置是**只在这一个调用点**补这个 fail-safe，而根因（trait 只抽象了
    // 探测）没动 —— 于是它在 `git_config_path` 复发两次、在 `wsl_repo_id` 变成
    // 第二份实现。**修类型，不修反例。**
    let text = match backend.read_text(config, d) {
        Probed::Found(t) => t,
        Probed::Absent => {
            return Probed::Unknown(crate::probe::ProbeError::new(
                config,
                "config was confirmed present by the probe backend but could not be read \
                 (source of truth switched mid-decision)",
            ))
        }
        Probed::Unknown(e) => return Probed::Unknown(e),
    };
    match parse_origin_url(&text) {
        Some(url) => Probed::Found(url),
        // 读到了、里面没有 origin —— **事实**，该退回 `path:` 身份。
        None => Probed::Absent,
    }
}

/// 手写的极小 ini 扫描：只认小节头与 `url =`，够用且无依赖。**大小写不敏感地**
/// 匹配小节头 —— git 自己接受 `[REMOTE "origin"]`。
fn parse_origin_url(text: &str) -> Option<String> {
    let mut in_origin = false;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            in_origin = t.eq_ignore_ascii_case("[remote \"origin\"]");
            continue;
        }
        if !in_origin {
            continue;
        }
        if let Some(rest) = t.strip_prefix("url") {
            if let Some(v) = rest.trim_start().strip_prefix('=') {
                let v = v.trim();
                if !v.is_empty() {
                    return Some(v.to_string());
                }
            }
        }
    }
    None
}

/// 把同一个仓库的各种 remote 写法收敛成一个串。
///
/// 🔴 **判据是「同一个仓的不同写法必须collapse 成同一个串，不同的仓永不相同」。**
/// 同一个远端可以被写成至少四种样子，而它们指的是同一个仓库：
///
/// ```text
/// https://github.com/o/r.git
/// git@github.com:o/r.git          ← scp-like，冒号不是端口
/// ssh://git@github.com/o/r
/// https://github.com/O/R/         ← 大小写 + 末尾斜杠
/// ```
///
/// 收敛掉：scheme、用户名、末尾 `/`、末尾 `.git`、大小写。**不**收敛主机名 ——
/// `github.com/o/r` 与 `gitlab.com/o/r` 是两个仓库。
pub fn normalize_remote(url: &str) -> Option<String> {
    let u = url.trim();
    if u.is_empty() {
        return None;
    }
    // scp-like (`git@host:owner/repo`) vs `scheme://[user@]host/path`.
    let body = if let Some(rest) = u.strip_prefix("git@") {
        rest.replacen(':', "/", 1)
    } else {
        let after_scheme = u.rsplit("://").next().unwrap_or(u);
        match after_scheme.split_once('@') {
            Some((_, host_path)) => host_path.to_string(),
            None => after_scheme.to_string(),
        }
    };
    let body = body.trim_end_matches('/').trim_end_matches(".git");
    if body.is_empty() {
        None
    } else {
        Some(body.to_lowercase())
    }
}

/// 一个**根**（可能是本机路径，也可能是 `wsl:<distro>:/abs` 规范形）的身份。
///
/// 🔴 **规范形 WSL 根本机 stat 不了，必须经访问桥**（2026-08-14 实测）。
/// 在此之前这里只走本机 FS，于是每一个 WSL 根的 `canonical_id` 恒为 `null` ——
/// 而同一个仓的 Windows checkout 有 `git:…`，**两份 checkout 因此永远不同身份**。
/// 记忆库里的直接后果：QuotaBar 被拆成 37 条 + 24 条，各持一半互相看不见。
///
/// ⚠️ **UNC 换算对这个根走不通**：换成 `\\wsl.localhost\<distro>\…` 之后，
/// `metadata(base)` 返回「既不是文件也不是目录」，底下每一项都是 `os error 267`
/// （连 `read_dir` 也是）。
///
/// 🔴 **我最初把它解释成「进程之间行为不同」，那是错的**（2026-08-14 当天更正）。
/// 真相是这个特定的根是一个**符号链接**：
/// `/home/<user>/workspace/QuotaBar -> /mnt/c/Users/user/workspace/QuotaBar`，
/// 链接指向 WSL 内部的挂载点，宿主沿 9P 跟不进去。**同一台机器上其它 WSL 项目
/// （VisionApp、image-grading…）走 UNC 一直是好的** —— 所以「UNC 对 WSL
/// 不可用」这个结论过宽，照它去设计会白白付掉每次探测一个 `wsl.exe` 的代价
/// （实测一次往返 ≈1.5 秒）。真正成立的判据是**「这条路径归谁管」**：归发行版管的
/// 内容，宿主答不上来时就该问它 —— 见 `probe::WslUncBackend` 的取舍。
///
/// 访问桥不受影响：`wsl.exe` 在发行版**内部**读，链接在那一侧是正常的。
pub fn repo_id_for_root(
    root: &str,
    default_distro: Option<&str>,
    mounts: &crate::pathnorm::DriveMounts,
    deadline: Deadline,
) -> Result<RepoIdentity, crate::probe::ProbeError> {
    // 🔴 **形态分派收口到 `pathnorm::reach_of`**（2026-08-15）。
    //
    // 这里从前只认两种形态（规范形 / 其余），于是**裸 Linux 路径与 `/mnt/<drive>/…`
    // 全落进本机分支** —— 在 Windows 上前者是当前盘的相对路径、后者同理，
    // stat 不到 `.git/config` ⇒ 落 `path:` id ⇒ 被 `store::record_identity_for_root` 丢弃。
    //
    // 而 `discovery::probe_path` 认**四种**形态。同一条「这条路径归谁管」的判据
    // 有两份实现、答不同的问题、于是分别演化 —— 本仓当天已因此栽过四次。
    // 现在形态只判一次，两个调用点各自决定拿到形态后做什么。
    match crate::pathnorm::reach_of(
        root,
        default_distro,
        mounts,
        crate::pathnorm::HostPlatform::current(),
    ) {
        crate::pathnorm::RootReach::Wsl { distro, linux } => {
            wsl_repo_id(&distro, &linux, root, deadline)
        }
        // 🔴 **`Unknown` 是「没问成」，不是「没有身份」。** 报 `Err` 让调用方撤回
        // 「问过了」缓存、下一轮重试；落 `path:` 会把一个暂时的不确定变成终态。
        crate::pathnorm::RootReach::Unknown(why) => {
            Err(crate::probe::ProbeError::new(Path::new(root), why))
        }
        crate::pathnorm::RootReach::Local(host_path) => {
            match find_git_root(Path::new(&host_path)) {
                GitRoot::Found(p) => Ok(RepoIdentity {
                    id: canonical_repo_id(&p)?,
                    repo_root: Some(p.to_string_lossy().into_owned()),
                }),
                GitRoot::Absent => Ok(RepoIdentity {
                    // ⚠️ `path:` 用**原样的 root**，不是换算后的宿主形式 ——
                    // 它是给人看的标识，要能对回注册表里那一行。
                    id: format!("path:{root}"),
                    repo_root: None,
                }),
                GitRoot::Unknown(e) => Err(e),
            }
        }
    }
}

/// [`repo_id_for_root`] 的答案。
///
/// 🔴 **`repo_root` 不能由调用方从 `id` 反推。** 它想知道的是「我给的这条路径
/// 本身就是仓库根吗」（别名分组挑代表要用），而 `path:` 前缀同时盖住「这里没有
/// `.git`」和「有 `.git` 但里面没有 origin」两种情况 —— 反推在后一种上就错了。
/// 判据只有本模块知道，所以由本模块说出来。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoIdentity {
    /// `git:<host>/<path>`（读到了 origin）或 `path:<root>`（读明白了，没有）。
    pub id: String,
    /// 身份所依据的仓库根。`None` = 这条链上没有可用的 `.git`。
    pub repo_root: Option<String>,
}

/// 经访问桥在发行版内部解析身份 —— **与本机走同一套规则、同一份实现**。
///
/// 🔴 **本函数从前是第二份实现，而且漏了一步。** 它自己拼路径、自己调
/// `wsl::read_file_at`，做了「`.git` 是目录 → 是文件则解 `gitdir:`」两步，
/// **没有第三步 `commondir`**。而 linked worktree 的 `<gitdir>/` 里**没有 config**
/// —— 它在 `commondir` 指向的主仓 `.git` 里。后果是确定性的：
/// **每一个 WSL 里的 linked worktree 永远退回 `path:` 身份**，而
/// `store::record_identity_for_root` 只写 `git:` 行 ⇒ 同一个仓的记忆分裂成两组。
///
/// ⚠️ 它当时的注释写着「与本机那条同一套规则，只是换了访问方式」——
/// **那是一句不成立的声明**。现在它成立了，因为规则只剩一份。
///
/// 只向上找**一层**（`<root>/.git`）而不是整条祖先链：调用方给的已经是注册表判定的
/// 项目根，再上溯等于替它重做一遍归属 —— 而归属只有一个权威（ADR-050）。
/// 这条性质由 [`git_config_path`]（它本来就不上溯）天然保证。
fn wsl_repo_id(
    distro: &str,
    linux_path: &str,
    original_root: &str,
    deadline: Deadline,
) -> Result<RepoIdentity, crate::probe::ProbeError> {
    // 发行版内部绝对路径寻址 —— 与调用方手上的 `linux_path` 同一种写法。
    let backend = crate::probe::WslBackend::new(distro);
    wsl_repo_id_with(linux_path, original_root, &backend, deadline)
}

/// [`wsl_repo_id`] 的可测形态 —— **后端注入**。
///
/// 🔴 **没有它，「两份实现收成一份」就只是一句声明**：本机那条 worktree 测试红不红，
/// 说不出 WSL 那条走的是不是同一段代码。而本仓判例反复说的正是这个 ——
/// 「纯函数的测试钉的是映射，永远说不了输入的来路」。
///
/// 用注入后端还有一个现实理由：真实 WSL 里造一个 linked worktree 才能测，
/// 那就把一条**语义**测试绑在了机器状态上。
fn wsl_repo_id_with(
    linux_path: &str,
    original_root: &str,
    backend: &dyn ProbeBackend,
    deadline: Deadline,
) -> Result<RepoIdentity, crate::probe::ProbeError> {
    let base = linux_path.trim_end_matches('/');
    let root = Path::new(base);

    // 🔴 **两步分开，是因为它们回答两个问题**：`git_config_path` 的 `Absent`
    // 是「这条链上没有可用的 `.git`」（⇒ `repo_root: None`），而
    // `read_origin_from_config` 的 `Absent` 是「读到了、里面没有 origin」
    // （⇒ `repo_root: Some`，根确实在这里）。压成一个三态就说不出这个区别。
    let config = match git_config_path(root, backend, deadline) {
        Probed::Found(p) => p,
        // 探明白了：这个根下没有可用的 `.git` —— 事实，不是没问成。
        Probed::Absent => {
            return Ok(RepoIdentity {
                id: format!("path:{original_root}"),
                repo_root: None,
            })
        }
        Probed::Unknown(e) => return Err(e),
    };

    // 走到这里 `.git` 就在这一层 —— 无论有没有 origin，`repo_root` 都是 `Some`。
    // 「有没有身份」和「根在哪」是两件事。
    let has_root = Some(original_root.to_string());
    match read_origin_from_config(&config, backend, deadline) {
        Probed::Found(url) => Ok(RepoIdentity {
            // 规范化不出来（空串 / 畸形）也是**读到了** —— 与本机那条对齐。
            id: match normalize_remote(&url) {
                Some(norm) => format!("git:{norm}"),
                None => format!("path:{original_root}"),
            },
            repo_root: has_root,
        }),
        Probed::Absent => Ok(RepoIdentity {
            id: format!("path:{original_root}"),
            repo_root: has_root,
        }),
        Probed::Unknown(e) => Err(e),
    }
}

/// 一个 git 仓库根的规范身份：`git:<host>/<owner>/<repo>`，**确认**拿不到 remote 时
/// `path:<git root>`。前缀是契约的一部分 —— 见模块文档「拿不到 origin 时」。
///
/// 🔴 返回 `Result` 而不是 `String`：「这个仓确实没有 origin」（⇒ `path:` 身份，是
/// 忠实描述）与「这一刻读不到 config」是两件事，而 `path:` 身份会被
/// `store::record_identity_for_root` 丢弃 ⇒ 后者被当成前者时，一次瞬时故障就变成
/// 「这个项目没有跨 checkout 身份」，且因为**先记后算**再也不会重试。
pub fn canonical_repo_id(git_root: &Path) -> Result<String, crate::probe::ProbeError> {
    canonical_repo_id_with(
        git_root,
        &crate::probe::LocalBackend::unanchored(),
        Deadline::unbounded(),
    )
}

/// [`canonical_repo_id`] 的可测形态 —— backend 注入。
pub fn canonical_repo_id_with(
    git_root: &Path,
    backend: &dyn ProbeBackend,
    d: Deadline,
) -> Result<String, crate::probe::ProbeError> {
    match read_origin_url_with(git_root, backend, d) {
        Probed::Found(url) => match normalize_remote(&url) {
            Some(norm) => Ok(format!("git:{norm}")),
            // url 在那儿但规范化不出来（空串 / 畸形）—— 读到了，是事实。
            None => Ok(format!("path:{}", git_root.to_string_lossy())),
        },
        Probed::Absent => Ok(format!("path:{}", git_root.to_string_lossy())),
        Probed::Unknown(e) => Err(e),
    }
}

#[cfg(test)]
// 测试要造 fixture（建目录、写文件、再核一遍），允许直接碰盘 —— 文件系统边界
// 管的是**生产行为**，而 `#[cfg(test)]` 不在生产路径上。
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sv-identity-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn seed_repo(root: &Path, origin: Option<&str>) {
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let body = match origin {
            Some(url) => format!("[core]\n\tbare = false\n[remote \"origin\"]\n\turl = {url}\n"),
            None => "[core]\n\tbare = false\n".to_string(),
        };
        std::fs::write(root.join(".git").join("config"), body).unwrap();
    }

    #[test]
    fn the_same_repo_written_four_ways_collapses_to_one_id() {
        // 🔴 这是本模块存在的理由：同一个仓的不同 remote 写法**必须**同 id，
        // 否则同一个项目的两份 checkout 会被当成两个项目 —— 而那正是 ADR-032 要防的。
        let want = Some("github.com/o/r".to_string());
        assert_eq!(normalize_remote("https://github.com/o/r.git"), want);
        assert_eq!(normalize_remote("git@github.com:o/r.git"), want);
        assert_eq!(normalize_remote("ssh://git@github.com/o/r"), want);
        assert_eq!(normalize_remote("https://github.com/O/R/"), want);
    }

    #[test]
    fn different_hosts_are_different_repos() {
        // 收敛不能过头：主机名是身份的一部分。
        assert_ne!(
            normalize_remote("https://github.com/o/r"),
            normalize_remote("https://gitlab.com/o/r")
        );
    }

    #[test]
    fn blank_and_empty_remotes_decline_rather_than_inventing_an_id() {
        assert_eq!(normalize_remote("   "), None);
        assert_eq!(normalize_remote(""), None);
        // 只有 scheme、没有主体 —— 造一个 `git:` 身份出来比说不知道更糟。
        assert_eq!(normalize_remote("https://"), None);
    }

    #[test]
    fn a_repo_without_a_remote_falls_back_to_path_and_says_so() {
        // `git init` 之后没 add remote 是正常状态。身份退回 path 前缀 ——
        // 它不跨 checkout 稳定，而前缀让消费方看得出这一点。
        let root = scratch("no-remote");
        seed_repo(&root, None);
        let id = canonical_repo_id(&root).expect("config 读得到");
        assert!(id.starts_with("path:"), "{id}");
        assert!(
            !id.starts_with("git:"),
            "没有 remote 时不得伪造 git 身份：{id}"
        );
    }

    #[test]
    fn origin_is_read_case_insensitively_from_the_section_header() {
        let root = scratch("upper-section");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(
            root.join(".git").join("config"),
            "[REMOTE \"origin\"]\n\turl = git@github.com:o/r.git\n",
        )
        .unwrap();
        assert_eq!(canonical_repo_id(&root).unwrap(), "git:github.com/o/r");
    }

    #[test]
    fn a_non_origin_remote_is_not_mistaken_for_origin() {
        // 只有 origin 算数：upstream / fork 指向别的仓，拿它当身份会把两个项目合并。
        let root = scratch("upstream-only");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(
            root.join(".git").join("config"),
            "[remote \"upstream\"]\n\turl = git@github.com:someone/else.git\n",
        )
        .unwrap();
        assert_eq!(
            read_origin_url(&root),
            Probed::Absent,
            "读到了、里面没有 origin —— 是事实不是没问成"
        );
        assert!(canonical_repo_id(&root).unwrap().starts_with("path:"));
    }

    #[test]
    fn find_git_root_declines_on_a_path_that_no_longer_exists() {
        // 🔴 已删除的 checkout 的父目录可能恰好是另一个仓库 —— 继续向上走会把它的
        // 身份安到一个不相干的项目上。实测就有这种形状（QuotaBar 侧，2026-08-11）。
        let root = scratch("deleted-child");
        seed_repo(&root, Some("git@github.com:o/outer.git"));
        let gone = root.join("was-a-checkout");
        assert_eq!(
            find_git_root(&gone),
            GitRoot::Absent,
            "路径不存在时必须拒绝，不能上溯到 {root:?}"
        );
    }

    #[test]
    fn find_git_root_walks_up_from_a_subdirectory() {
        let root = scratch("nested");
        seed_repo(&root, Some("git@github.com:o/r.git"));
        let sub = root.join("src").join("deep");
        std::fs::create_dir_all(&sub).unwrap();
        assert_eq!(find_git_root(&sub), GitRoot::Found(root.clone()));
        let GitRoot::Found(found) = find_git_root(&sub) else {
            panic!("应当找到 git 根");
        };
        assert_eq!(canonical_repo_id(&found).unwrap(), "git:github.com/o/r");
    }

    /// 🔴 **linked worktree 的 `.git` 是文件，config 在 commondir**（三轮评审 P2-1）。
    ///
    /// `find_git_root` 一直**明确认可** `.git` 文件（注释里点名了子模块 / worktree），
    /// 而 `read_origin_url` 固定读 `<root>/.git/config` —— 对 worktree 那不是目录。
    /// 后果是**确定性的**：每一个 linked worktree 永远退回 `path:` 身份，而
    /// `record_identity_for_root` 只写 `git:` 行 ⇒ 这些项目**从来**没有过跨 checkout 身份。
    /// 两处对「`.git` 是什么」的理解不一致，而只有一处写了注释。
    #[test]
    fn a_linked_worktree_resolves_the_shared_config() {
        let root = scratch("worktree");
        // 主仓：真正的 config 在这里
        let main_git = root.join("main").join(".git");
        std::fs::create_dir_all(main_git.join("worktrees").join("wt")).unwrap();
        std::fs::write(
            main_git.join("config"),
            "[remote \"origin\"]\n\turl = git@github.com:o/r.git\n",
        )
        .unwrap();
        // worktree 的 gitdir：commondir 指回主仓的 .git
        std::fs::write(
            main_git.join("worktrees").join("wt").join("commondir"),
            "../..\n",
        )
        .unwrap();
        // worktree 本体：`.git` 是**文件**
        let wt = root.join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(
            wt.join(".git"),
            format!(
                "gitdir: {}\n",
                main_git.join("worktrees").join("wt").display()
            ),
        )
        .unwrap();

        assert_eq!(
            canonical_repo_id(&wt).unwrap(),
            "git:github.com/o/r",
            "linked worktree 必须解到共享 config，否则它永远拿不到 git: 身份"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// 🔴 **config 读不了 ≠ 这个仓没配 origin**（三轮评审 P2-1）。
    ///
    /// 上一版 `.ok()?` 把两者压成 `None` ⇒ 退回 `path:` ⇒ 被 store 丢弃，
    /// 而 `identity_seen` 已经先记过 ⇒ 本进程再也不重试。
    ///
    /// ⚠️ 反向那条（读到了、里面确实没有 origin ⇒ `Absent` ⇒ `path:`）由
    /// `a_repo_without_a_remote_falls_back_to_path_and_says_so` 与
    /// `a_non_origin_remote_is_not_mistaken_for_origin` 钉着 —— 少了它们，
    /// 一个恒 `Unknown` 的实现照样让本测试通过。
    #[test]
    fn an_unreadable_config_is_unknown_not_a_missing_origin() {
        struct ConfigDenied;
        impl ProbeBackend for ConfigDenied {
            fn probe(&self, p: &Path, _d: Deadline) -> Probed<FileKind> {
                if p.file_name().is_some_and(|n| n == "config") {
                    Probed::Unknown(crate::probe::ProbeError::new(p, "permission denied"))
                } else if p.file_name().is_some_and(|n| n == ".git") {
                    Probed::Found(FileKind::Dir)
                } else {
                    Probed::Absent
                }
            }
            /// 本 fixture **只答探测**。读到这里说明测试的形状变了 —— 见 `ProbeBackend::read_text`。
            fn read_text(&self, p: &Path, _d: Deadline) -> Probed<String> {
                panic!("{p:?}: this fixture only answers probes; a read here means the test changed shape")
            }
        }
        let root = Path::new("/w/proj");
        assert!(matches!(
            read_origin_url_with(root, &ConfigDenied, Deadline::unbounded()),
            Probed::Unknown(_)
        ));
        assert!(
            canonical_repo_id_with(root, &ConfigDenied, Deadline::unbounded()).is_err(),
            "读不了 config 却给出了一个 path: 身份 —— store 会丢弃它且永不重试"
        );
    }

    /// 🔴 **规范形 WSL 根不再走本机 FS**（2026-08-14）。
    ///
    /// 这条钉的是**分派**：本机路径仍走 FS（下面那半），而 `wsl:<distro>:/abs`
    /// 必须转到访问桥 —— 因为它作为字面量在 Windows 上 `metadata` 会得到
    /// `InvalidFilename`（实测 raw 123），于是老代码对**每一个** WSL 根都返回
    /// `Unknown` ⇒ `canonical_id` 恒为 `null` ⇒ 同一个仓的两份 checkout 永远
    /// 不同身份（记忆库里 37 条 + 24 条各持一半）。
    ///
    /// ⚠️ 用**不存在的发行版**驱动（约 0.1s 失败，不必等超时），与
    /// `unreachable_client()` 走死代理、`WSL_E_DISTRO_NOT_FOUND` 同一个手法：
    /// 走真实路径，不伪造错误。
    ///
    /// 🔴 **判据必须能把两条路分开，而这一点我第一版写错了。** 第一版断言
    /// `msg.contains("wsl")` —— 而**被探的路径本身就含 `wsl:`**，于是取消分派、
    /// 全走本机 FS 的变异**照样全绿**。观测量选错，护栏就是装饰。
    /// 现在断言桥自己的操作名前缀，那是本机那条路产不出来的。
    ///
    /// ⚠️ **2026-08-20 改过一次字面量，而那不是护栏失灵，恰恰是它在工作。**
    /// `wsl_repo_id` 收口到 `git_config_path` 之后，入桥的第一站从
    /// `read_file_at`（`wsl read …`）变成了 `stat_kind`（`wsl stat_kind …`）——
    /// **判据没变（走没走桥），变的是从哪个门进去。** 所以两个前缀都认。
    /// 🔴 仍然**不许**放宽成 `contains("wsl")`：被探的路径本身就含 `wsl:`，
    /// 那样写的话「取消分派、全走本机 FS」的变异照样全绿（本测试第一版的原错）。
    #[test]
    #[cfg(windows)]
    fn a_canonical_wsl_root_goes_through_the_bridge_not_the_local_fs() {
        let err = repo_id_for_root(
            "wsl:NoSuchDistro_quotabar_xyz:/home/u/proj",
            None,
            &Vec::new(),
            Deadline::after(std::time::Duration::from_secs(20)),
        )
        .expect_err("不存在的发行版必须报「没问成」，不能给出一个身份");
        let msg = err.to_string();
        assert!(
            msg.contains("wsl stat_kind") || msg.contains("wsl read"),
            "错误应带访问桥自己的操作名；走本机 FS 时这里会是 InvalidFilename。实际：{msg}"
        );
    }

    /// 🔴 **WSL 侧的 linked worktree —— 层 2 的判据。**
    ///
    /// 本机那条（`a_linked_worktree_resolves_the_shared_config`）钉的是
    /// `git_config_path` 的三步规则；**这一条钉的是 WSL 那条路走的是同一段代码**。
    ///
    /// 从前 `wsl_repo_id` 自己拼路径读文件，只做了「目录 → `gitdir:`」两步、
    /// **没有 `commondir`** ⇒ 每一个 WSL 里的 linked worktree 永远退回 `path:` 身份，
    /// 而 `store::record_identity_for_root` 只写 `git:` 行 ⇒ 同一个仓的记忆分裂成两组。
    /// 实测：本机 20 个项目根里有 1 个 `canonical_id` 为空，就是它。
    ///
    /// ⚠️ **变异判据**：删掉 `git_config_path` 里的 `commondir` 那一步，
    /// **本条与本机那条必须同时红**。只有一侧红 = 收口没做成，仍是两份实现。
    #[test]
    fn a_wsl_linked_worktree_resolves_the_shared_config_through_the_same_rule() {
        // 站在访问桥的位置：路径 → (类型, 内容)。
        // ⚠️ 查表前归一分隔符 —— 调用方用 `Path::join`，它在 Windows 上产出 `\`，
        // 而真实的 `WslBackend::to_linux` 也正是在这一步把它换回 `/`。
        struct FakeDistro(std::collections::HashMap<String, (FileKind, Option<String>)>);
        impl FakeDistro {
            /// 归一分隔符 **并解 `..`** —— 真实的 FS 与发行版都在 OS 层做这两件事
            /// （`commondir` 几乎总是 `../..`）。不做的话这个替身就不忠实，
            /// 会把一个**能跑通**的生产路径判成失败。
            fn key(p: &Path) -> String {
                let flat = p.to_string_lossy().replace('\\', "/");
                let mut out: Vec<&str> = Vec::new();
                for seg in flat.split('/') {
                    match seg {
                        ".." => {
                            out.pop();
                        }
                        "." => {}
                        _ => out.push(seg),
                    }
                }
                out.join("/")
            }
        }
        impl ProbeBackend for FakeDistro {
            fn probe(&self, p: &Path, _d: Deadline) -> Probed<FileKind> {
                match self.0.get(&Self::key(p)) {
                    Some((k, _)) => Probed::Found(*k),
                    None => Probed::Absent,
                }
            }
            fn read_text(&self, p: &Path, _d: Deadline) -> Probed<String> {
                match self.0.get(&Self::key(p)) {
                    Some((_, Some(t))) => Probed::Found(t.clone()),
                    // 存在但不是可读文本（目录）—— 与真实桥一致：不是「没有」。
                    Some((_, None)) => Probed::Unknown(crate::probe::ProbeError::new(
                        p,
                        "fixture: not a readable file",
                    )),
                    None => Probed::Absent,
                }
            }
        }

        let mut fs = std::collections::HashMap::new();
        // worktree 本体：`.git` 是**文件**，内容是 gitdir 指针（相对写法，git 常见）
        fs.insert(
            "/home/u/repo/.claude/worktrees/wt/.git".to_string(),
            (
                FileKind::File,
                Some("gitdir: /home/u/repo/.git/worktrees/wt\n".to_string()),
            ),
        );
        // 🔴 worktree 的 gitdir 里**没有 config** —— 这正是旧实现踩空的地方
        fs.insert(
            "/home/u/repo/.git/worktrees/wt/commondir".to_string(),
            (FileKind::File, Some("../..\n".to_string())),
        );
        // 主仓的 config：真正的 origin 在这里
        fs.insert(
            "/home/u/repo/.git/config".to_string(),
            (
                FileKind::File,
                Some("[remote \"origin\"]\n\turl = git@example.com:o/r.git\n".to_string()),
            ),
        );

        let got = wsl_repo_id_with(
            "/home/u/repo/.claude/worktrees/wt",
            "wsl:D:/home/u/repo/.claude/worktrees/wt",
            &FakeDistro(fs),
            Deadline::unbounded(),
        )
        .expect("布局完整时不该报「没问成」");

        assert_eq!(
            got.id, "git:example.com/o/r",
            "WSL 里的 worktree 必须解到**主仓**的身份 —— 那是它与主仓同属一个仓的唯一依据"
        );
        assert!(
            got.repo_root.is_some(),
            "`.git` 确实在这一层 —— 「有没有身份」和「根在哪」是两件事"
        );
    }

    /// 反向：本机路径**不能**被误判成 WSL 规范形而走桥。
    ///
    /// 少了这一条，一个「无脑全走桥」的实现照样让上面那条通过 —— 而那会让每个
    /// 本机项目都去 spawn 一次 `wsl.exe`。
    #[test]
    fn a_local_path_still_resolves_on_the_local_fs() {
        let root = scratch("local-still-local");
        seed_repo(&root, Some("git@github.com:o/r.git"));
        let got = repo_id_for_root(
            &root.to_string_lossy(),
            None,
            &Vec::new(),
            Deadline::unbounded(),
        )
        .unwrap();
        assert_eq!(got.id, "git:github.com/o/r");
        // 🔴 `repo_root` 也要钉：它是别名分组挑代表的依据，而调用方**不能**从 `id`
        // 反推（`path:` 同时盖住「没有 .git」和「有 .git 但没 origin」）。
        assert_eq!(got.repo_root.as_deref(), Some(&*root.to_string_lossy()));
        std::fs::remove_dir_all(&root).ok();
    }

    /// 🔴 **探测失败不是「这个项目没有 git 根」。**
    ///
    /// 两条边都钉：起点探不动、以及链上某一层探不动 —— 从前两处都是 `.exists()`，
    /// 一次权限拒绝会让调用方（`store::record_identity_for_root`）当成 `Absent` 静默
    /// 放弃，而它**先记后算**，于是这个项目在本进程里再也不会被重试。
    ///
    /// ⚠️ 反向那条（真的没有 ⇒ `Absent`）由上面两条测试钉着 —— 少了它，一个
    /// 恒 `Unknown` 的实现照样能让本测试通过。
    #[test]
    fn a_probe_failure_is_unknown_not_absent() {
        struct Failing;
        impl ProbeBackend for Failing {
            fn probe(&self, p: &Path, _d: Deadline) -> Probed<crate::probe::FileKind> {
                Probed::Unknown(crate::probe::ProbeError::new(p, "permission denied"))
            }
            /// 本 fixture **只答探测**。读到这里说明测试的形状变了 —— 见 `ProbeBackend::read_text`。
            fn read_text(&self, p: &Path, _d: Deadline) -> Probed<String> {
                panic!("{p:?}: this fixture only answers probes; a read here means the test changed shape")
            }
        }
        assert!(matches!(
            find_git_root_with(Path::new("/w/proj/sub"), &Failing, Deadline::unbounded()),
            GitRoot::Unknown(_)
        ));

        // 起点探得到、`.git` 探不动 —— 这条链上**可能**有根，只是没问成。
        // 从前它会一路上溯，把子仓库的会话记到父仓库名下（ADR-051 §5 规则 ③）。
        struct StartOkThenFailing;
        impl ProbeBackend for StartOkThenFailing {
            fn probe(&self, p: &Path, _d: Deadline) -> Probed<crate::probe::FileKind> {
                if p.file_name().is_some_and(|n| n == ".git") {
                    Probed::Unknown(crate::probe::ProbeError::new(p, "handle exhausted"))
                } else {
                    Probed::Found(crate::probe::FileKind::Dir)
                }
            }
            /// 本 fixture **只答探测**。读到这里说明测试的形状变了 —— 见 `ProbeBackend::read_text`。
            fn read_text(&self, p: &Path, _d: Deadline) -> Probed<String> {
                panic!("{p:?}: this fixture only answers probes; a read here means the test changed shape")
            }
        }
        assert!(matches!(
            find_git_root_with(
                Path::new("/w/proj/sub"),
                &StartOkThenFailing,
                Deadline::unbounded()
            ),
            GitRoot::Unknown(_)
        ));
    }
}
