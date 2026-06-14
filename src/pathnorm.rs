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
            workspace_location("/home/me/proj", &SourceLocation::Local, HostPlatform::Windows),
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
            assert_eq!(workspace_location(r"C:\Users\me\proj", &local, host), "local");
        }
    }
}
