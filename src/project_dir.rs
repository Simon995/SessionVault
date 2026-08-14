//! 把 Claude 的 `projects/<enc>` 目录名解回真实路径 —— **判据的唯一实现**。
//!
//! # 为什么在这一层
//!
//! 这条规则此前有**两份实现**，一份 Rust（QuotaBar `memory/sources.rs`）、
//! 一份 Python（TumeFlow `adapters/base.py`），而 Rust 那份的文档写着
//!
//! > Mirrors TumeFlow `decode_project_dir(enc, base)`
//!
//! 🔴 **「mirrors」这个词本仓栽过**：`version.merge_key_for` 的注释写着
//! 「Mirrors this exactly」，两份漂开时**没有任何东西会报错**（写入按一个 key
//! 落盘、supersede 链按另一个 key 找）。2026-08-14 同一件事在这里发生了 ——
//! Rust 那份修好了 WSL 符号链接，Python 那份没有，实测同一个编码名给出
//! `\\wsl.localhost\…\QuotaBar` 与 `None` 两个答案。
//!
//! 所以规则收口到这里写**一次**：Rust 侧直接调；Python 侧经 `svault
//! decode-project-dir` 子命令（TumeFlow 与 SessionVault 之间本来就只有二进制
//! 接口，没有源码依赖）。
//!
//! # 为什么必须探文件系统
//!
//! Claude 把 `/home/simon/workspace/QuotaBar` 编码成
//! `-home-simon-workspace-QuotaBar`（分隔符 → `-`）。而路径成分本身可能含 `-`
//! （`corneal-staining-grading`），所以**哪一种切分才对只能靠探真实目录**。
//! 贪心取「最长的、真实存在的目录成分」，不是 `replace('-', "/")`。

use std::path::PathBuf;

use crate::deadline::Deadline;
use crate::probe::{self, FileKind, ProbeBackend, Probed};

/// 一次编码目录名解码的结果。
///
/// 🔴 **三态，因为解码要靠探测文件系统消歧。**
///
/// 上一版用 `candidate.is_dir()`：一个停掉的 WSL 发行版让每次探测都失败，于是
/// 解码器要么换个切分（**给出一个看起来合理但错的项目根**），要么返回 `None`
/// （被调用方读作「这个项目已删除」）。
///
/// `Absent` 与 `Unresolvable` 必须分开：前者是**探明白了没有**（项目确实删了），
/// 后者是**没问成**。与 `Probed::Absent` / `Probed::Unknown` 是同一条判据，
/// 只是发生在解码这一层。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodedProject {
    /// 探到了对应的目录。
    Found(String),
    /// 探明白了：磁盘上没有对应目录（项目已删除）。
    Absent,
    /// **没问成** —— 探测本身失败（权限拒绝 / 发行版停了 / UNC 不通）。
    Unresolvable(String),
}

/// `base` 是宿主→根的文件系统前缀（ADR-033）：WSL 发行版经 UNC 到达时是
/// `\\wsl.localhost\<distro>`（于是宿主探真实挂载点、返回宿主能打开的**物理
/// 路径**），本机命名空间则为 `""`。
pub fn decode_project_dir(enc: &str, base: &str) -> DecodedProject {
    decode_project_dir_with(enc, base, probe_backend_for(base).as_ref())
}

/// 按 root 的宿主前缀挑探测后端。
///
/// 🔴 **WSL root 不能只问宿主。** 用 `LocalBackend` 问 `\\wsl.localhost\…` 对大多数
/// 目录确实答得上来，于是这条一直没被发现 —— 直到路上出现一个**符号链接**：
/// 实测 `/home/simon/workspace/QuotaBar -> /mnt/c/Users/user/workspace/QuotaBar`，
/// 宿主沿 9P 跟不进那个挂载点，返回「既不是文件也不是目录」，解码于是报
/// [`DecodedProject::Absent`]＝**「这个项目不存在」**。后果不是少一条路径，而是
/// 调用方的别名分组（`len() >= 2`）让**整组消失** —— 同一个目录的项目记忆分裂成
/// 互不可见的 37 + 24 两半，而界面上一切正常。
///
/// [`probe::WslUncBackend`] 只在那一格上回落到访问桥（理由与代价见它的文档）。
pub fn probe_backend_for(base: &str) -> Box<dyn ProbeBackend> {
    match crate::pathnorm::wsl_distro_of_unc_prefix(base) {
        Some(distro) => Box::new(probe::WslUncBackend::new(distro, base)),
        None => Box::new(probe::LocalBackend::unanchored()),
    }
}

/// [`decode_project_dir`] 的可测形态 —— **backend 注入**。
///
/// 🔴 拆出来是因为「更长的候选没问成 ⇒ 不许要更短的」这条**在本机造不出来**：
/// 同一次解码里所有候选共享一个命名空间根，而探测失败恰恰是**整个根**不可达才
/// 发生的 —— 没法让长候选失败而兄弟候选成功。判定作参数，逻辑才可单测。
pub fn decode_project_dir_with(
    enc: &str,
    base: &str,
    backend: &dyn ProbeBackend,
) -> DecodedProject {
    let bytes = enc.as_bytes();
    let (root, encoded_path) = if !base.is_empty() {
        (PathBuf::from(base), enc.trim_start_matches('-'))
    } else if cfg!(windows)
        && bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b'-'
        && bytes[2] == b'-'
    {
        (PathBuf::from(format!("{}:\\", bytes[0] as char)), &enc[3..])
    } else {
        (
            PathBuf::from(std::path::MAIN_SEPARATOR_STR),
            enc.trim_start_matches('-'),
        )
    };
    let parts: Vec<&str> = encoded_path.split('-').collect();
    let mut path = root.clone();
    let mut i = 0;
    // 🔴 记住有没有探测**失败过**。没匹配上时，「确实没有」与「没问成」的区别
    // 全靠它 —— 少了它，一个停掉的发行版就变成「这些项目都删了」。
    let mut probe_failed: Option<String> = None;
    while i < parts.len() {
        let mut matched = false;
        for j in (i + 1..=parts.len()).rev() {
            // longest component first
            let component = parts[i..j].join("-");
            let candidate = path.join(&component);
            match backend.probe(&candidate, Deadline::unbounded()) {
                Probed::Found(FileKind::Dir) => {
                    // 🔴 **有更长的候选没问成，就不能要这个更短的**。
                    //
                    // 循环是最长优先，所以此刻 `probe_failed` 非空 ⇒ 失败发生在一个
                    // **优先级更高**的候选上。接受更短的那个不是「保守多报」，是
                    // **给出一个错误的成功答案**：期望 `C:\foo-bar\baz`，探
                    // `C:\foo-bar` 暂时失败，而机器上恰好有 `C:\foo\bar\baz` ⇒ 返回
                    // 后者 ⇒ 这个项目的记忆按**另一个项目**的 git 身份归组。
                    //
                    // 与 ADR-051 §5 规则③（一层没问成就停在那里）是同一条。
                    if let Some(why) = probe_failed {
                        return DecodedProject::Unresolvable(why);
                    }
                    path = candidate;
                    i = j;
                    matched = true;
                    break;
                }
                // 存在但不是目录，或确认不存在 —— 都是事实，换下一种切分。
                Probed::Found(_) | Probed::Absent => {}
                Probed::Unknown(e) => {
                    probe_failed.get_or_insert_with(|| e.to_string());
                }
            }
        }
        if !matched {
            return match probe_failed {
                Some(why) => DecodedProject::Unresolvable(why),
                None => DecodedProject::Absent,
            };
        }
    }
    if path == root {
        match probe_failed {
            Some(why) => DecodedProject::Unresolvable(why),
            None => DecodedProject::Absent,
        }
    } else {
        DecodedProject::Found(path.to_string_lossy().into_owned())
    }
}

/// 一条真实路径 → Claude 的 `projects/<enc>` 目录名。**无歧义的那个方向。**
///
/// 🔴 **这才是消费方该用的方向。** 解码要探盘消歧（`-` 既可能是分隔符也可能是
/// 名字的一部分），而编码是纯字符串变换 —— 把问题倒过来之后，「哪个目录属于
/// 哪个项目」不再需要任何探测：拿已知的项目根编码一次，直接去看那个目录在不在。
///
/// Claude 编码的是**会话当时看到的 cwd**：WSL 会话是发行版内的
/// `/home/u/proj`，本机会话是 `C:\Users\u\proj`（盘符的 `:` 也换成 `-`）。
/// 所以调用方要拿**对应命名空间的那个写法**来编码，不能拿 UNC 或规范形。
pub fn encode_project_dir(path: &str) -> String {
    path.chars()
        .map(|c| {
            if matches!(c, '/' | '\\' | ':') {
                '-'
            } else {
                c
            }
        })
        .collect()
}

/// 一个项目根在 Claude 侧**可能**的目录名（每种写法各一个，去重保序）。
///
/// 给多个而不是猜一个：注册表存的写法未必是会话当时用的那个。规范形
/// `wsl:<d>:/home/u/p` 对应的是**发行版内路径** `/home/u/p`，而 UNC 写法对应的
/// 是宿主视角 —— 两者编码出来完全不同，只有会话真正用过的那个才会有目录。
/// 消费方逐个看哪个存在即可：那是**存在性检查**，不是从歧义串反推切分。
pub fn claude_project_dirs(root_path: &str, aliases: &[String]) -> Vec<String> {
    let mut forms: Vec<String> = Vec::new();
    let mut push = |s: String| {
        if !s.is_empty() && !forms.contains(&s) {
            forms.push(s);
        }
    };
    // 规范形要拆出**发行版内路径** —— 那才是会话里的 cwd。
    if let Some((_, linux)) = crate::pathnorm::split_canonical_wsl(root_path) {
        push(encode_project_dir(linux));
    } else {
        push(encode_project_dir(root_path));
    }
    for a in aliases {
        if let Some((_, linux)) = crate::pathnorm::split_canonical_wsl(a) {
            push(encode_project_dir(linux));
        } else {
            push(encode_project_dir(a));
        }
    }
    forms
}

/// 这个根**宿主能打开**的那个写法。
///
/// 🔴 规范形 `wsl:<d>:/…` 与发行版内路径都**打不开** —— 前者是标识符，后者在
/// Windows 上会被当成当前盘的相对路径。宿主要读这个项目的 `CLAUDE.md`，只能用
/// UNC 那个写法（它在 `aliases` 里，由 `alias_forms_of` 产出）。
///
/// `None` = 没有任何一个宿主可用的写法。**这是诚实的「我给不出」**，不是拿规范形
/// 冒充路径 —— 拿它去 `open()` 必然失败，而失败点会离这里很远。
///
/// ⚠️ **「宿主能不能打开」是平台相关的**：`/home/u/p` 在 Linux 宿主上就是本机路径，
/// 在 Windows 宿主上却会被当成当前盘的相对路径（`C:\home\u\p`）—— 那不是打不开，
/// 是**打开了错的东西**，比报错更坏。所以裸 Linux 路径在 Windows 上必须报 `None`。
pub fn host_openable_form(
    root_path: &str,
    aliases: &[String],
    host: crate::pathnorm::HostPlatform,
) -> Option<String> {
    let usable = |p: &str| -> bool {
        // 规范形是标识符，永远不是路径。
        if crate::pathnorm::split_canonical_wsl(p).is_some() {
            return false;
        }
        // 裸 Linux 路径只在 Unix 宿主上是本机路径。
        if crate::pathnorm::is_bare_linux_path(p) {
            return host == crate::pathnorm::HostPlatform::Unix;
        }
        true
    };
    if usable(root_path) {
        return Some(root_path.to_string());
    }
    aliases.iter().find(|a| usable(a)).cloned()
}

#[cfg(test)]
// 测试要造 fixture（建目录、写文件、再核一遍），允许直接碰盘 —— 文件系统边界
// 管的是**生产行为**，而 `#[cfg(test)]` 不在生产路径上。
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use crate::probe::ProbeError;
    use std::path::Path;

    fn scratch(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "sv-project-dir-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&base).expect("scratch");
        base
    }

    /// 照 Claude 的编码规则把一条真实路径变回目录名（分隔符与盘符冒号都换成 `-`）。
    fn enc_of(path: &Path) -> String {
        path.to_string_lossy()
            .chars()
            .map(|c| {
                if matches!(c, '/' | '\\' | ':') {
                    '-'
                } else {
                    c
                }
            })
            .collect()
    }

    #[test]
    fn decode_round_trips_a_real_path_with_dashes() {
        // 目录名自身含 `-` —— 考的是贪心最长成分重建，不是天真的 dash→slash 替换。
        let root = scratch("decode");
        let project = root.join("work").join("my-cool-repo");
        std::fs::create_dir_all(&project).unwrap();

        assert_eq!(
            decode_project_dir(&enc_of(&project), ""),
            DecodedProject::Found(project.to_string_lossy().into_owned())
        );

        // 路径已不存在 → `Absent`（项目被删了）。**探明白了没有**，不是没问成。
        let gone = root.join("work").join("ghost");
        assert_eq!(
            decode_project_dir(&enc_of(&gone), ""),
            DecodedProject::Absent
        );

        std::fs::remove_dir_all(&root).ok();
    }

    /// 🔴 **更长的候选没问成时，不许退而求其次要更短的。**
    ///
    /// 上一版遇 `Unknown` 只记进 `probe_failed` 就继续找更短候选，更短的存在就
    /// `Found` —— 那**不是「保守多报」，是给出一个错误的成功答案**：
    /// 期望 `…/foo-bar/baz`，探 `foo-bar` 暂时失败，而磁盘上恰好有 `…/foo/bar/baz`
    /// ⇒ 返回后者 ⇒ 这个项目的记忆按**另一个项目**的 git 身份归组。
    ///
    /// ⚠️ 两端都断言：探测**全部成功**时必须解出 `foo/bar/baz`（证明这条路径真的
    /// 走得通、`Unresolvable` 不是因为布局造不出匹配），而只要 `foo-bar` 那一格
    /// 没问成就必须 `Unresolvable`。少了前一条，一个恒 `Unresolvable` 的实现照样绿。
    #[test]
    fn a_failed_longer_candidate_never_settles_for_a_shorter_one() {
        let root = scratch("decode-shorter");
        // 磁盘上**只有** foo/bar/baz，没有 foo-bar —— 正是那个陷阱布局。
        let decoy = root.join("foo").join("bar").join("baz");
        std::fs::create_dir_all(&decoy).unwrap();
        let enc = enc_of(&root.join("foo-bar").join("baz"));

        struct Fixed<F>(F);
        impl<F: Fn(&Path) -> Probed<FileKind>> ProbeBackend for Fixed<F> {
            fn probe(&self, p: &Path, _d: Deadline) -> Probed<FileKind> {
                (self.0)(p)
            }
        }

        assert_eq!(
            decode_project_dir_with(&enc, "", &probe::LocalBackend::unanchored()),
            DecodedProject::Found(decoy.to_string_lossy().into_owned()),
            "前提：探测全成功时这份布局解得出更短的那条 —— 否则下面那条断言是空的"
        );

        let flaky = Fixed(|p: &Path| {
            if p.file_name().is_some_and(|n| n == "foo-bar") {
                Probed::Unknown(ProbeError::new(p, "drive temporarily disconnected"))
            } else {
                probe::LocalBackend::unanchored().probe(p, Deadline::unbounded())
            }
        });
        match decode_project_dir_with(&enc, "", &flaky) {
            DecodedProject::Unresolvable(_) => {}
            other => panic!("更长候选没问成却接受了更短的 ⇒ 跨项目错误归属，得到 {other:?}"),
        }

        std::fs::remove_dir_all(&root).ok();
    }

    /// ADR-033：带 `base` 前缀（代表 WSL 的 UNC 挂载）时探 base+path、返回物理路径；
    /// 不带前缀的裸 enc 在本机命名空间里找不到 → `Absent`。
    #[test]
    fn decode_resolves_an_in_root_path_under_the_fs_prefix() {
        let base = scratch("fsprefix");
        let proj = base.join("home").join("simon").join("my-proj-with-dash");
        std::fs::create_dir_all(&proj).unwrap();
        let enc = "-home-simon-my-proj-with-dash";

        assert_eq!(
            decode_project_dir(enc, &base.to_string_lossy()),
            DecodedProject::Found(proj.to_string_lossy().into_owned())
        );
        assert_eq!(decode_project_dir(enc, ""), DecodedProject::Absent);

        std::fs::remove_dir_all(&base).ok();
    }

    /// 🔴 **WSL root 的解码必须能问到发行版，不能只问宿主。**
    ///
    /// 判据是**错误来自哪条路** —— 「报了 Unknown」本身分不开两种实现：一个
    /// 不存在的 UNC 主机，本机探测也报 Unknown。所以断言访问桥自己的前缀
    /// （`wsl stat_kind`），那是本机那条路产不出来的。
    ///
    /// 用不存在的发行版驱动真实路径（约 0.1s 失败），不伪造错误。
    #[test]
    #[cfg(windows)]
    fn a_wsl_root_decodes_through_the_access_bridge() {
        let prefix = r"\\wsl.localhost\NoSuchDistro_quotabar_xyz";
        let backend = probe_backend_for(prefix);
        match backend.probe(
            Path::new(&format!(r"{prefix}\home\u\p")),
            Deadline::after(std::time::Duration::from_secs(20)),
        ) {
            Probed::Unknown(e) => assert!(
                e.to_string().contains("wsl stat_kind"),
                "错误应带访问桥自己的前缀；只走本机 FS 时这里是一个 OS 错误。实际：{e}"
            ),
            other => panic!("不存在的发行版必须报「没问成」，实际：{other:?}"),
        }
    }

    /// 反向：本机 root **不能**被误判成 WSL 而去 spawn `wsl.exe`。
    #[test]
    fn a_local_root_stays_on_the_local_fs() {
        let backend = probe_backend_for("");
        let dir = scratch("local-root-backend");
        assert_eq!(
            backend.probe(&dir, Deadline::unbounded()),
            Probed::Found(FileKind::Dir)
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}

#[cfg(test)]
mod encode_tests {
    use super::*;
    use crate::pathnorm::HostPlatform;

    /// 编码是**纯字符串变换** —— 这正是「把问题倒过来」之后不再需要探盘的理由。
    #[test]
    fn encoding_needs_no_probing() {
        assert_eq!(
            encode_project_dir("/home/simon/workspace/QuotaBar"),
            "-home-simon-workspace-QuotaBar"
        );
        // 盘符的 `:` 也换掉 —— 与 Claude 实际创建的目录名一致（`C--Users-…`）。
        assert_eq!(
            encode_project_dir(r"C:\Users\user\workspace\QuotaBar"),
            "C--Users-user-workspace-QuotaBar"
        );
    }

    /// 🔴 **规范形要拆出发行版内路径再编码。**
    ///
    /// Claude 编码的是会话当时看到的 cwd —— WSL 会话看到的是 `/home/u/p`，不是
    /// `wsl:U:/home/u/p`，更不是 UNC。直接编码规范形会得到 `wsl-U--home-u-p`，
    /// 那个目录**永远不存在**，于是这个项目的记忆一条也找不到。
    #[test]
    fn a_canonical_wsl_root_encodes_its_in_distro_path() {
        let dirs = claude_project_dirs(
            "wsl:Ubuntu-22.04:/home/simon/workspace/QuotaBar",
            &[r"\wsl.localhost\Ubuntu-22.04\home\simon\workspace\QuotaBar".to_string()],
        );
        assert!(
            dirs.contains(&"-home-simon-workspace-QuotaBar".to_string()),
            "会话真正用过的那个写法必须在候选里，实际：{dirs:?}"
        );
        assert!(
            !dirs.iter().any(|d| d.starts_with("wsl-")),
            "规范形不能被直接编码 —— 那个目录永远不存在：{dirs:?}"
        );
    }

    /// 🔴 **裸 Linux 路径在 Windows 宿主上不是「打不开」，是「打开错的东西」。**
    ///
    /// `/home/u/p` 会被解释成当前盘的相对路径（`C:\home\u\p`）。报 `None` 是诚实的；
    /// 把它当路径交出去，失败点会离这里很远，而且可能悄悄命中一个不相干的目录。
    #[test]
    fn a_bare_linux_path_is_not_host_openable_on_windows() {
        assert_eq!(
            host_openable_form("/home/u/p", &[], HostPlatform::Windows),
            None
        );
        assert_eq!(
            host_openable_form("/home/u/p", &[], HostPlatform::Unix),
            Some("/home/u/p".to_string())
        );
    }

    /// 规范形永远不是路径；UNC 那个别名才是宿主能打开的。
    #[test]
    fn a_canonical_root_hands_back_its_unc_alias() {
        let unc = r"\wsl.localhost\Ubuntu-22.04\home\u\p".to_string();
        assert_eq!(
            host_openable_form(
                "wsl:Ubuntu-22.04:/home/u/p",
                &[unc.clone()],
                HostPlatform::Windows
            ),
            Some(unc)
        );
        // 没有 UNC 别名时**不许**拿规范形冒充。
        assert_eq!(
            host_openable_form("wsl:Ubuntu-22.04:/home/u/p", &[], HostPlatform::Windows),
            None
        );
    }

    /// 本机路径两边都直接可用。
    #[test]
    fn a_local_root_is_its_own_host_form() {
        for host in [HostPlatform::Windows, HostPlatform::Unix] {
            assert_eq!(
                host_openable_form(r"C:\Users\u\p", &[], host),
                Some(r"C:\Users\u\p".to_string())
            );
        }
    }
}
