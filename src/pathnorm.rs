//! 路径规范化 —— 宿主感知（host-aware）、三层分离的唯一权威。
//!
//! # 三层分离（务必别搅在一起）
//! 1. **规范化（本模块）**：纯字符串/路径语义，无 I/O、无系统调用、可跨平台单测。
//!    Unix 路径语义在 Linux/macOS 原生与 WSL 发行版内部**完全一致**，因此同一套函数
//!    共用，**不为 WSL 单独复制一份**。
//! 2. **访问桥（未实现，Windows 专属）**：Windows 宿主经 `wsl.exe` / `\\wsl$\` 实际
//!    读取发行版内文件、枚举发行版。Linux 原生宿主不需要此桥。default_distro 等运行期
//!    事实由该桥注入到本模块的纯函数里——本模块自己**不**枚举发行版。
//! 3. **location 标记**：`local` vs `wsl:<distro>`，由 [`workspace_location`] 产出，
//!    写入 `RawEvent.workspace_location`。
//!
//! # 规范形
//! WSL 路径的规范形是 `wsl:<distro>:/abs/path`（`distro` 后单冒号，再接以 `/` 开头的
//! Linux 绝对路径）。三种形态的关系：
//!
//! ```text
//! UNC 形            \\wsl$\Ubuntu\home\me  ──canonical_wsl_unc──▶  wsl:Ubuntu:/home/me
//! 规范形            wsl:Ubuntu:/home/me    ──split_canonical_wsl─▶  ("Ubuntu", "/home/me")
//! ```
//!
//! # 与 QuotaBar 的差异（这次标准化的关键）
//! QuotaBar 的 `normalize_cwd_for_location` / `workspace_location`（`session_index.rs`）
//! 内建 **「裸 `/abs` ⇒ WSL」的 Windows 宿主假设**：它默认软件跑在 Windows 上，因此
//! 把 `/home/me/proj` 当 WSL 路径。若软件在 **Linux 原生**跑，同样的 `/home/me/proj`
//! 是**本机**路径，盲抽会把 Linux 原生路径误标成 `wsl`。本模块把这条隐含假设**显式化**
//! 为 [`HostPlatform`] 参数，并把分散在 QuotaBar 多处的 `split_canonical_wsl_cwd`
//! （同名异义：一处解析 UNC、一处解析规范形）收敛成命名互不混淆的两个函数。

use crate::rawevent::SourceLocation;

/// 宿主平台 —— 决定「裸 Unix 绝对路径」的归属。
///
/// 这是 QuotaBar 没显式化、却隐含在代码里的维度。Linux 原生与 Windows+WSL 对同一个
/// `/home/me/proj` 的解读**相反**：原生宿主上它是本机路径，Windows 宿主上它八成来自
/// WSL 发行版内部。必须由调用方（而非 `cfg!`）明确告知，单测才能两种宿主都覆盖。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostPlatform {
    /// Windows 宿主：裸 Unix 绝对路径通常来自 WSL 发行版内部。
    Windows,
    /// Unix 宿主（Linux/macOS 原生）：裸 Unix 绝对路径就是本机路径。
    Unix,
}

impl HostPlatform {
    /// 当前编译目标的宿主平台（运行期默认值；单测请显式传 `Windows`/`Unix`）。
    pub const fn current() -> Self {
        if cfg!(windows) {
            HostPlatform::Windows
        } else {
            HostPlatform::Unix
        }
    }
}

/// UNC 形 WSL 路径 → 规范形 `wsl:<distro>:/abs`；非 UNC 形返回 `None`。
///
/// 接受 `\\wsl$\<distro>\..`、`//wsl$/..`、`\\wsl.localhost\<distro>\..`、
/// `//wsl.localhost/..`（反斜杠先归一为正斜杠）。
///
/// 注意与 [`split_canonical_wsl`] 区分：本函数吃 **UNC**，那个吃**规范形**。
/// 二者曾在 `project_root.rs` 被同名 `split_canonical_wsl_cwd` 混淆，是这次收敛的对象。
pub fn canonical_wsl_unc(path: &str) -> Option<String> {
    let normalized = path.replace('\\', "/");
    let rest = normalized
        .strip_prefix("//wsl.localhost/")
        .or_else(|| normalized.strip_prefix("//wsl$/"))?;
    let (distro, linux_path) = rest.split_once('/')?;
    if distro.is_empty() || linux_path.is_empty() {
        return None;
    }
    Some(format!("wsl:{distro}:/{linux_path}"))
}

/// UNC 形 WSL **前缀** → 发行版名（`\\wsl.localhost\Ubuntu-22.04` → `Ubuntu-22.04`）。
///
/// 与 [`canonical_wsl_unc`] 的区别是**尾巴可以为空**：那个吃的是一条完整路径
/// （`\\wsl.localhost\<distro>\home\u\x`，尾巴为空时返回 `None`），这个吃的是
/// ADR-033 的 `fs_prefix` —— 它本身就止于发行版名。分成两个函数而不是放宽那一个：
/// 「这是不是一条 WSL 路径」与「这个前缀属于哪个发行版」是两个问题，让前者对空尾巴
/// 说 `Some` 会把 `\\wsl.localhost\Ubuntu` 报成一条合法路径。
pub fn wsl_distro_of_unc_prefix(prefix: &str) -> Option<String> {
    let normalized = prefix.replace('\\', "/");
    let rest = normalized
        .strip_prefix("//wsl.localhost/")
        .or_else(|| normalized.strip_prefix("//wsl$/"))?;
    let distro = rest.split('/').next()?;
    (!distro.is_empty()).then(|| distro.to_string())
}

/// 解析规范形 `wsl:<distro>:/abs` → `(distro, linux_path)`；非规范形返回 `None`。
///
/// `linux_path` 保证以 `/` 开头。本函数**不**吃 UNC（那是 [`canonical_wsl_unc`] 的活）。
pub fn split_canonical_wsl(path: &str) -> Option<(&str, &str)> {
    let rest = path.strip_prefix("wsl:")?;
    let (distro, linux_path) = rest.split_once(':')?;
    if distro.is_empty() || !linux_path.starts_with('/') {
        return None;
    }
    Some((distro, linux_path))
}

/// 规范形 `wsl:<distro>:/abs` → Windows 侧能打开的 UNC 形；非规范形返回 `None`。
///
/// [`canonical_wsl_unc`] 的反向。**两个方向都必须住在这里**：一个 Windows 上的消费者
/// 拿到 `wsl:Ubuntu:/home/u/proj` 是打不开的，它需要 `\\wsl.localhost\Ubuntu\home\u\proj`；
/// 若让它自己拼，同一条规则就有了第二份实现 —— 而两份都活着的时候，没有任何东西会
/// 说出它们不一致（本仓判例：同一个项目在记忆库里存成两个身份，各持一半互不可见）。
///
/// 用 `wsl.localhost` 而非老的 `wsl$`：后者在新版 Windows 上仍可用但已不是官方写法，
/// 而**产生**一律用一种形式、**接受**两种都认（见 [`canonical_wsl_unc`]）—— 宽进严出。
pub fn canonical_wsl_to_unc(path: &str) -> Option<String> {
    let (distro, linux_path) = split_canonical_wsl(path)?;
    let windows_tail = linux_path.trim_start_matches('/').replace('/', "\\");
    Some(format!("\\\\wsl.localhost\\{distro}\\{windows_tail}"))
}

#[cfg(test)]
// 测试要造 fixture（建目录、写文件、再核一遍），允许直接碰盘 —— 文件系统边界
// 管的是**生产行为**，而 `#[cfg(test)]` 不在生产路径上。
#[allow(clippy::disallowed_methods)]
mod unc_round_trip_tests {
    use super::*;

    #[test]
    fn a_bare_unc_prefix_yields_its_distro() {
        assert_eq!(
            wsl_distro_of_unc_prefix(r"\\wsl.localhost\Ubuntu-22.04").as_deref(),
            Some("Ubuntu-22.04")
        );
        // 老写法与正斜杠都认（宽进严出）。
        assert_eq!(
            wsl_distro_of_unc_prefix(r"\\wsl$\Debian").as_deref(),
            Some("Debian")
        );
        assert_eq!(
            wsl_distro_of_unc_prefix("//wsl.localhost/Ubuntu").as_deref(),
            Some("Ubuntu")
        );
        // 带尾巴也认 —— 调用方给的是 `fs_prefix`，但多给一截不该翻脸。
        assert_eq!(
            wsl_distro_of_unc_prefix(r"\\wsl.localhost\Ubuntu\home\u").as_deref(),
            Some("Ubuntu")
        );
    }

    /// 🔴 **非 WSL 的东西一律 `None`** —— 它决定 `probe_backend_for` 走不走访问桥，
    /// 误报会让每个本机项目去 spawn 一个 `wsl.exe`。
    #[test]
    fn anything_that_is_not_a_wsl_prefix_is_declined() {
        for s in [
            "",
            r"C:\Users\u",
            r"\\server\share",
            r"\\wsl.localhost\",
            "wsl:Ubuntu:/home/u", // 规范形不是 UNC —— 那是 split_canonical_wsl 的活
        ] {
            assert_eq!(wsl_distro_of_unc_prefix(s), None, "误认了 {s:?}");
        }
    }

    /// 🔴 **两个方向必须真的互为逆**，否则「同一个根的两种写法」会在某一侧多出一个
    /// 身份 —— 而那正是这对函数存在的理由。
    #[test]
    fn the_two_directions_round_trip() {
        let canonical = "wsl:Ubuntu-22.04:/home/u/proj";
        let unc = canonical_wsl_to_unc(canonical).unwrap();
        assert_eq!(unc, r"\\wsl.localhost\Ubuntu-22.04\home\u\proj");
        assert_eq!(canonical_wsl_unc(&unc).as_deref(), Some(canonical));
    }

    /// 产生只用 `wsl.localhost`，接受两种 —— 宽进严出。
    #[test]
    fn the_legacy_unc_form_is_accepted_but_never_produced() {
        let legacy = r"\\wsl$\Ubuntu\home\u\proj";
        let canonical = canonical_wsl_unc(legacy).unwrap();
        assert_eq!(canonical, "wsl:Ubuntu:/home/u/proj");
        assert!(
            canonical_wsl_to_unc(&canonical)
                .unwrap()
                .contains("wsl.localhost"),
            "产生的一律是新写法"
        );
    }

    /// 不是 WSL 的路径没有第二种写法 —— **返回 `None`，不是返回它自己**。
    /// 后者会让调用方把「没有别名」与「别名恰好等于自己」混起来。
    #[test]
    fn a_non_wsl_path_has_no_second_form() {
        assert_eq!(canonical_wsl_to_unc(r"C:\work\proj"), None);
        assert_eq!(canonical_wsl_to_unc("/home/u/proj"), None);
        assert_eq!(canonical_wsl_to_unc("wsl:Ubuntu:relative"), None);
    }
}

/// `/mnt/<drive>/…`：WSL 里挂载的 Windows 盘。工程物理在 Windows → 应判为 `local`。
///
/// 仅匹配单个盘符字母后接 `/` 或路径结束（`/mnt/c`、`/mnt/c/...`），避免把
/// `/mnt/data` 这种普通 Linux 挂载点误判为 Windows 盘。
pub fn is_windows_drive_mount(path: &str) -> bool {
    let Some(rest) = path.strip_prefix("/mnt/") else {
        return false;
    };
    let bytes = rest.as_bytes();
    // 用 `map_or(true, ..)`（1.0 起）而非 `is_none_or`（1.82 才稳定）：等价且更保守，
    // 不给 MSRV 添约束。
    bytes.first().is_some_and(u8::is_ascii_alphabetic) && bytes.get(1).map_or(true, |b| *b == b'/')
}

/// WSL 里 Windows 盘的挂载表 —— **发现的一项运行期事实**（`(挂载点, Windows 路径)`）。
///
/// 由 [`crate::wsl::drive_mounts`] 读 `mount` 得到，见那里的「为什么不读 `wsl.conf`」。
/// 空表 = 不做 `/mnt/…` 映射（那些路径照旧「说不出来」），**不是**退回按盘符猜。
pub type DriveMounts = Vec<(String, String)>;

/// 把 WSL 里的 `/mnt/<x>/…` 换算成 Windows 路径。**纯函数**，挂载表由外面给。
///
/// 🔴 **表为空或没匹配 ⇒ `None`，不猜。** 「`/mnt/<单字母>` 就是盘符」这个猜法
/// 在三种情况下是错的：`automount.root` 被改过、配置改了没重启、以及
/// `/mnt/data` 这类普通 Linux 挂载。而猜错的后果不是「少归一个」——
/// 是把事件归到一个**别的项目**（甚至一个不存在的盘）名下。
///
/// 住在 `pathnorm` 而不是 `discovery`，是因为它有**两个**用户：发现侧要拿它挑
/// 「本机 stat 哪条路径」，归属侧要拿它把 `/mnt/c/X` 与 `C:\X` 认成同一个根。
/// 谁先要它就放在谁那儿，另一个就得反向依赖 —— 本仓已有判例（`TRANSIENT_ERR`
/// 曾住在某一个 provider 里，于是四个 provider 里三个漏掉了它）。
pub fn mnt_to_windows(path: &str, mounts: &DriveMounts) -> Option<String> {
    let p = path.replace('\\', "/");
    for (mount_point, win_root) in mounts {
        let mp = mount_point.trim_end_matches('/');
        if mp.is_empty() {
            continue;
        }
        let rest = if p == mp {
            ""
        } else if let Some(r) = p.strip_prefix(&format!("{mp}/")) {
            r
        } else {
            continue;
        };
        let root = win_root.trim_end_matches(['\\', '/']);
        return Some(if rest.is_empty() {
            format!("{root}\\")
        } else {
            format!("{root}\\{}", rest.replace('/', "\\"))
        });
    }
    None
}

/// 一条根路径**该问谁** —— 形态分派的唯一实现。
///
/// 🔴 收成一处的理由与 `HostProbe`（「该问谁」）同源：这套判据此前有**两份**，
/// 而它们答的是不同的问题、于是分别演化：
/// - `discovery::probe_path`（「项目根在哪」）认四种形态：规范形 / UNC / 裸 Linux /
///   `/mnt/<drive>`；
/// - `identity::repo_id_for_root`（「这个仓的身份是什么」）只认**两种**（规范形 /
///   本机），**裸 Linux 与 `/mnt/…` 全落进本机分支**。
///
/// 后果实测（2026-08-15）：20 个注册根里 3 个拿不到 `canonical_id`，而它们正是这两族
/// —— 在 Windows 上 `/home/simon/…` 是当前盘的相对路径、`/mnt/c/…` 同理，
/// stat 不到 `.git/config` ⇒ 落 `path:` id ⇒ 被 `record_identity_for_root` 丢弃。
/// 而同一个目录的规范形那行**有**身份 ⇒ 同一份记忆按写法落进不同的桶。
///
/// **不合并两个调用点**（它们答不同的问题），只把「形态」这一步收成一处。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RootReach {
    /// 宿主自己就能 stat（本机路径，或 `/mnt/<drive>/…` 换算后的宿主形式）。
    Local(String),
    /// 归发行版管，要经访问桥。
    Wsl { distro: String, linux: String },
    /// 知道它在别处，但**问不出该问谁**（裸 Linux 而 distro 不明、
    /// `/mnt/…` 而挂载表拿不到）。
    ///
    /// 🔴 **这不是「本机路径」**。落回本机会把 `/home/u/x` 当成当前盘的相对路径 ——
    /// 要么探不到（被按「确认无根」缓存 24 小时），要么误中真实存在的 `\home\u\x`。
    Unknown(String),
}

/// 判定一条根路径该问谁。`host` 是宿主平台（Unix 上裸绝对路径就是本机路径）。
pub fn reach_of(
    path: &str,
    default_distro: Option<&str>,
    mounts: &DriveMounts,
    host: HostPlatform,
) -> RootReach {
    let p = path.trim();
    if let Some((distro, linux)) = split_canonical_wsl(p) {
        return RootReach::Wsl {
            distro: distro.to_string(),
            linux: linux.to_string(),
        };
    }
    if let Some(canonical) = canonical_wsl_unc(p) {
        if let Some((distro, linux)) = split_canonical_wsl(&canonical) {
            return RootReach::Wsl {
                distro: distro.to_string(),
                linux: linux.to_string(),
            };
        }
    }
    if host == HostPlatform::Windows && is_bare_linux_path(p) {
        return match default_distro {
            Some(d) => RootReach::Wsl {
                distro: d.to_string(),
                linux: p.to_string(),
            },
            None => RootReach::Unknown(format!("bare linux path with no known distro: {p}")),
        };
    }
    if host == HostPlatform::Windows && is_windows_drive_mount(p) {
        // 换算走**实测的挂载表**，不按「`/mnt/<单字母>` 就是盘符」猜 —— 那在
        // `automount.root` 改过、配置改了没重启、以及 `/mnt/data` 这类普通挂载
        // 三种情况下都是错的，而猜错会把身份安到别的项目上。
        return match mnt_to_windows(p, mounts) {
            Some(win) => RootReach::Local(win),
            None => RootReach::Unknown(format!(
                "no drive mount covers {p} (mount table unavailable or this is a plain Linux mount)"
            )),
        };
    }
    RootReach::Local(p.to_string())
}

/// 裸 Linux 绝对路径（`/home`、`/root`…），且不是挂载的 Windows 盘。
///
/// 「归属」由 [`HostPlatform`] 决定，本函数只判「形状」，不判归属。
pub fn is_bare_linux_path(path: &str) -> bool {
    path.starts_with('/') && !is_windows_drive_mount(path)
}

/// 把对话记录里的原始 cwd 归一到规范形（宿主感知）。返回 `None` 表示无 cwd。
///
/// `default_distro` 是**访问桥注入**的运行期事实（Windows 上「唯一用户发行版」时才有值），
/// 本模块自身不枚举发行版——纯函数、可单测。规则按序：
/// 1. 空白 → `None`。
/// 2. UNC 形 → 规范形（[`canonical_wsl_unc`]）。
/// 3. 已是规范形 `wsl:..:/..` → 原样。
/// 4. 裸 Linux 绝对路径 **且 Windows 宿主** 且有 `default_distro` → 打标 `wsl:<distro>:<raw>`。
///    Unix 宿主下**跳过**：裸绝对路径就是本机路径，不该被打 WSL 标。
/// 5. 其余（Windows 盘符路径、`/mnt/..`、Unix 宿主的裸绝对路径、distro 不明的裸路径）→ 原样。
pub fn normalize_cwd(
    raw: Option<&str>,
    host: HostPlatform,
    default_distro: Option<&str>,
) -> Option<String> {
    let raw = raw?.trim();
    if raw.is_empty() {
        return None;
    }
    if let Some(canonical) = canonical_wsl_unc(raw) {
        return Some(canonical);
    }
    if split_canonical_wsl(raw).is_some() {
        return Some(raw.to_string());
    }
    if host == HostPlatform::Windows && is_bare_linux_path(raw) {
        if let Some(distro) = default_distro {
            return Some(format!("wsl:{distro}:{raw}"));
        }
    }
    Some(raw.to_string())
}

/// 工程物理位置标记：`"local"` 或 `"wsl:<distro>"`（distro 不明时为泛 `"wsl"`）。
///
/// 写入 `RawEvent.workspace_location`（`Option<String>`），故返回 `String`，与 QuotaBar
/// 同名函数的取值集对齐。`project_root` 应已规范化（理想是 [`normalize_cwd`] 的产物）。
/// `transcript_location` 是 transcript 文件本身的物理位置，用于补全裸路径的 distro。
///
/// 判定（按序）：
/// 1. 规范形 `wsl:<distro>:/..` → `wsl:<distro>`。
/// 2. `/mnt/<drive>/..`（挂载的 Windows 盘）→ `local`。
/// 3. 裸 Unix 绝对路径 → 按 `host` 分叉：
///    - Unix 宿主 → `local`（**这正是 QuotaBar 漏掉的分支**）。
///    - Windows 宿主 → WSL；distro 优先取 transcript 的，否则泛 `wsl`。
/// 4. 其余（Windows 盘符路径等）→ `local`。
pub fn workspace_location(
    project_root: &str,
    transcript_location: &SourceLocation,
    host: HostPlatform,
) -> String {
    if let Some((distro, _)) = split_canonical_wsl(project_root) {
        return format!("wsl:{distro}");
    }
    if is_windows_drive_mount(project_root) {
        return "local".to_string();
    }
    if project_root.starts_with('/') {
        return match host {
            HostPlatform::Unix => "local".to_string(),
            HostPlatform::Windows => match transcript_location {
                SourceLocation::Wsl(distro) => format!("wsl:{distro}"),
                SourceLocation::Local => "wsl".to_string(),
            },
        };
    }
    "local".to_string()
}

#[cfg(test)]
// 测试要造 fixture（建目录、写文件、再核一遍），允许直接碰盘 —— 文件系统边界
// 管的是**生产行为**，而 `#[cfg(test)]` 不在生产路径上。
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    #[test]
    fn unc_to_canonical() {
        assert_eq!(
            canonical_wsl_unc(r"\\wsl$\Ubuntu\home\me\proj").as_deref(),
            Some("wsl:Ubuntu:/home/me/proj")
        );
        assert_eq!(
            canonical_wsl_unc("//wsl.localhost/Debian/srv/app").as_deref(),
            Some("wsl:Debian:/srv/app")
        );
        // 非 UNC 形一律 None。
        assert_eq!(canonical_wsl_unc("C:/Users/me"), None);
        assert_eq!(canonical_wsl_unc("/home/me"), None);
        assert_eq!(canonical_wsl_unc("wsl:Ubuntu:/home/me"), None);
        // distro 或路径缺失。
        assert_eq!(canonical_wsl_unc("//wsl$/Ubuntu"), None);
        assert_eq!(canonical_wsl_unc("//wsl$//home"), None);
    }

    #[test]
    fn parse_canonical_form() {
        assert_eq!(
            split_canonical_wsl("wsl:Ubuntu:/home/me"),
            Some(("Ubuntu", "/home/me"))
        );
        // 不吃 UNC、不吃裸路径。
        assert_eq!(split_canonical_wsl(r"\\wsl$\Ubuntu\home"), None);
        assert_eq!(split_canonical_wsl("/home/me"), None);
        // linux_path 必须以 / 开头。
        assert_eq!(split_canonical_wsl("wsl:Ubuntu:home/me"), None);
        assert_eq!(split_canonical_wsl("wsl::/home"), None);
    }

    #[test]
    fn windows_drive_mount_detection() {
        assert!(is_windows_drive_mount("/mnt/c"));
        assert!(is_windows_drive_mount("/mnt/c/Users/me"));
        assert!(is_windows_drive_mount("/mnt/d/code"));
        // /mnt/data 是普通 Linux 挂载点，不是盘符。
        assert!(!is_windows_drive_mount("/mnt/data"));
        assert!(!is_windows_drive_mount("/home/me"));
        assert!(!is_windows_drive_mount("/mnt/"));
    }

    #[test]
    fn normalize_unc_regardless_of_host() {
        // UNC 与规范形与宿主无关，两种宿主都该归一。
        for host in [HostPlatform::Windows, HostPlatform::Unix] {
            assert_eq!(
                normalize_cwd(Some(r"\\wsl$\Ubuntu\home\me"), host, None).as_deref(),
                Some("wsl:Ubuntu:/home/me")
            );
            assert_eq!(
                normalize_cwd(Some("wsl:Ubuntu:/home/me"), host, None).as_deref(),
                Some("wsl:Ubuntu:/home/me")
            );
        }
    }

    #[test]
    fn normalize_bare_linux_is_host_dependent() {
        // Windows 宿主 + 已知 distro：打标。
        assert_eq!(
            normalize_cwd(Some("/home/me/proj"), HostPlatform::Windows, Some("Ubuntu")).as_deref(),
            Some("wsl:Ubuntu:/home/me/proj")
        );
        // Windows 宿主但 distro 不明：保持裸路径（由 workspace_location 兜底泛 wsl）。
        assert_eq!(
            normalize_cwd(Some("/home/me/proj"), HostPlatform::Windows, None).as_deref(),
            Some("/home/me/proj")
        );
        // Unix 宿主：绝不打 WSL 标，即便注入了 distro。
        assert_eq!(
            normalize_cwd(Some("/home/me/proj"), HostPlatform::Unix, Some("Ubuntu")).as_deref(),
            Some("/home/me/proj")
        );
    }

    #[test]
    fn normalize_empty_and_drive_paths() {
        assert_eq!(normalize_cwd(None, HostPlatform::Unix, None), None);
        assert_eq!(normalize_cwd(Some("   "), HostPlatform::Unix, None), None);
        // Windows 盘符路径原样。
        assert_eq!(
            normalize_cwd(Some(r"C:\Users\me"), HostPlatform::Windows, None).as_deref(),
            Some(r"C:\Users\me")
        );
        // /mnt/c 是挂载盘，不打 WSL 标（即便 Windows 宿主）。
        assert_eq!(
            normalize_cwd(Some("/mnt/c/code"), HostPlatform::Windows, Some("Ubuntu")).as_deref(),
            Some("/mnt/c/code")
        );
    }

    #[test]
    fn workspace_location_canonical_wsl() {
        let local = SourceLocation::Local;
        for host in [HostPlatform::Windows, HostPlatform::Unix] {
            assert_eq!(
                workspace_location("wsl:Ubuntu:/home/me/proj", &local, host),
                "wsl:Ubuntu"
            );
        }
    }

    #[test]
    fn workspace_location_mnt_is_local() {
        let local = SourceLocation::Local;
        for host in [HostPlatform::Windows, HostPlatform::Unix] {
            assert_eq!(workspace_location("/mnt/c/code", &local, host), "local");
        }
    }

    #[test]
    fn workspace_location_bare_linux_splits_by_host() {
        // 这是修掉 QuotaBar 宿主假设的核心断言。
        // Unix 原生宿主：裸 /home → local。
        assert_eq!(
            workspace_location("/home/me/proj", &SourceLocation::Local, HostPlatform::Unix),
            "local"
        );
        // Windows 宿主 + transcript 在本地：distro 不明 → 泛 wsl。
        assert_eq!(
            workspace_location(
                "/home/me/proj",
                &SourceLocation::Local,
                HostPlatform::Windows
            ),
            "wsl"
        );
        // Windows 宿主 + transcript 在某发行版：补全该 distro。
        assert_eq!(
            workspace_location(
                "/home/me/proj",
                &SourceLocation::Wsl("Debian".to_string()),
                HostPlatform::Windows
            ),
            "wsl:Debian"
        );
    }

    #[test]
    fn workspace_location_windows_drive_path_is_local() {
        let local = SourceLocation::Local;
        for host in [HostPlatform::Windows, HostPlatform::Unix] {
            assert_eq!(
                workspace_location(r"C:\Users\me\proj", &local, host),
                "local"
            );
        }
    }

    // ── 形态分派：一条根路径该问谁（2026-08-15）────────────────────────────

    fn mnt_c() -> DriveMounts {
        vec![("/mnt/c".into(), r"C:\".into())]
    }

    /// 🔴 **裸 Linux 与 `/mnt/…` 不是本机路径。**
    ///
    /// 这两族此前在 `identity::repo_id_for_root` 里落进本机分支 —— 在 Windows 上
    /// `/home/u/x` 是**当前盘的相对路径**，stat 不到 `.git/config` ⇒ 落 `path:` id
    /// ⇒ 被 `store::record_identity_for_root` 丢弃。实测 20 个注册根里 3 个因此没有
    /// `canonical_id`，而同一个目录的规范形那行**有** ⇒ 同一份记忆按写法落进不同的桶。
    #[test]
    fn a_bare_linux_root_belongs_to_the_distro_not_to_the_host() {
        let m = mnt_c();
        assert_eq!(
            reach_of("/home/u/proj", Some("Ubuntu"), &m, HostPlatform::Windows),
            RootReach::Wsl {
                distro: "Ubuntu".into(),
                linux: "/home/u/proj".into()
            }
        );
        // Unix 宿主上，裸绝对路径就是本机路径 —— 不该被打 WSL 标。
        assert_eq!(
            reach_of("/home/u/proj", Some("Ubuntu"), &m, HostPlatform::Unix),
            RootReach::Local("/home/u/proj".into())
        );
    }

    /// 🔴 **不知道该问谁 ≠ 本机路径。** 落回本机会把 `/home/u/x` 当成当前盘的相对
    /// 路径：要么探不到（被按「确认无根」缓存 24 小时），要么**误中**真实存在的
    /// `\home\u\x`。必须是第三态。
    #[test]
    fn a_bare_linux_root_with_no_known_distro_is_unknown_not_local() {
        match reach_of("/home/u/proj", None, &mnt_c(), HostPlatform::Windows) {
            RootReach::Unknown(why) => assert!(why.contains("no known distro"), "{why}"),
            other => panic!("distro 不明必须报 Unknown，实际：{other:?}"),
        }
    }

    /// `/mnt/<drive>/…` 换算成宿主形式 —— **零 `wsl.exe`**，本机直接 stat。
    #[test]
    fn a_mounted_windows_drive_is_reached_through_the_host_path() {
        assert_eq!(
            reach_of(
                "/mnt/c/Users/u/proj",
                Some("Ubuntu"),
                &mnt_c(),
                HostPlatform::Windows
            ),
            RootReach::Local(r"C:\Users\u\proj".into())
        );
    }

    /// 🔴 **换算不出来也是「没问成」。** 挂载表拿不到时落回本机同样会误中当前盘上的
    /// `\mnt\…`。
    #[test]
    fn a_mount_no_table_covers_is_unknown_not_local() {
        match reach_of(
            "/mnt/d/x",
            Some("Ubuntu"),
            &Vec::new(),
            HostPlatform::Windows,
        ) {
            RootReach::Unknown(why) => assert!(why.contains("no drive mount"), "{why}"),
            other => panic!("挂载表覆盖不到必须报 Unknown，实际：{other:?}"),
        }
    }

    /// 规范形与 UNC 都归发行版；Windows 盘符路径归本机。
    #[test]
    fn canonical_and_unc_go_to_the_distro_while_drive_paths_stay_local() {
        let m = mnt_c();
        assert_eq!(
            reach_of("wsl:Ubuntu:/home/u/p", None, &m, HostPlatform::Windows),
            RootReach::Wsl {
                distro: "Ubuntu".into(),
                linux: "/home/u/p".into()
            }
        );
        assert_eq!(
            reach_of(
                r"\\wsl.localhost\Debian\home\u\p",
                None,
                &m,
                HostPlatform::Windows
            ),
            RootReach::Wsl {
                distro: "Debian".into(),
                linux: "/home/u/p".into()
            }
        );
        assert_eq!(
            reach_of(r"C:\Users\u\p", None, &m, HostPlatform::Windows),
            RootReach::Local(r"C:\Users\u\p".into())
        );
    }
}
