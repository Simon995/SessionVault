//! 项目归属：给定一个路径，它属于哪个项目根（ADR-050）。
//!
//! ## 这一层为什么存在
//!
//! 从前 `project_root::resolve_project_root` 把两件**性质相反**的事压在一次调用里：
//!
//! | | 需要 I/O | 允许失败 | 必须对每条事件有答案 |
//! | --- | --- | --- | --- |
//! | **发现**：哪些路径是项目根 | ✅ | ✅ | ❌ |
//! | **归属**：这个路径属于哪个根 | ❌ | ❌ | ✅ |
//!
//! 压在一起时，前者的失败只能由后者去**编一个答案** —— 而它编的那个答案是 `cwd`。
//! 实测后果：71.4% 的事件（`source = "wsl_cwd"`）从没做过项目根解析，因为 Windows
//! 上 stat 不了 WSL 路径；同一个项目被记成 11 个 `project_root`（`EyeVLM/docs`、
//! `EyeVLM/experimental_model/…`）。
//!
//! 本模块只做**归属**：纯函数、零 I/O、必然给出结果（含「说不出来」那一种）。
//! 发现在别处，产物是一份注册表喂给这里。
//!
//! ## 🔴 输入是「一个路径」，不是「一个 cwd」
//!
//! `cwd` 隐含「这个来源的会话发生在某个目录里」—— Claude Code / Codex 满足，
//! **TumeChat 不满足**（聊天框里没有「人在哪个目录」）。而 TumeChat 的会话是要被
//! 摄取的（TumeChat `ADR-001 §10`：记忆是双向的）。
//!
//! 所以签名吃 `Option<&str>` 路径，「路径从哪来」由各来源自己决定。一字之差，
//! 但它决定了新增 `SourceType` 时要不要回来改这一层 —— 不该改。
//!
//! ## 🔴 结果与执行者无关
//!
//! QuotaBar 与 TumeFlow **各自摄取**（MASTER_PLAN「摄取归谁」），而它们能发现的
//! 项目根不一样（前者有 WSL 访问桥）。若各算各的，同一个路径在两个进程里会归到
//! 不同的根 —— `project_root` 就成了「取决于谁跑」的字段，而它本该是事件的客观属性。
//!
//! 所以归属**只看注册表**，不看当前进程有什么能力。不变式：
//!
//! > 同一个路径在任何进程、任何时刻，归属结果只取决于注册表的内容。

use std::borrow::Cow;

use crate::pathnorm::{mnt_to_windows, DriveMounts};

/// 一个已知项目根是**怎么发现的**。写进注册表供排查，不参与归属判定。
///
/// 归属只关心「这是个根」，不关心它怎么来的 —— 但排查时「为什么这个目录被当成
/// 项目根」是第一个要问的问题，而答案必须查得到。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RootSource {
    /// 目录下有 `.git`（含子模块那种 `.git` 文件）。
    Git,
    /// 目录下有构建 marker（`Cargo.toml` / `package.json` / …）且**没有** `.git`。
    Marker,
    /// 由宿主的目录扫描给出（QuotaBar 的 `enumerate_source_roots` 等）。
    Scan,
    /// 用户显式配置。
    Configured,
}

impl RootSource {
    pub fn as_str(self) -> &'static str {
        match self {
            RootSource::Git => "git",
            RootSource::Marker => "marker",
            RootSource::Scan => "scan",
            RootSource::Configured => "configured",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "git" => RootSource::Git,
            "marker" => RootSource::Marker,
            "scan" => RootSource::Scan,
            "configured" => RootSource::Configured,
            _ => return None,
        })
    }
}

/// 归属结果。
///
/// 🔴 **`Unattributed` 是一个独立变体，不是「path 为空」。** 下游想把它当
/// `project_root` 用就必须 `match`，于是「这是兜底」这件事没法被静默忽略。
///
/// 它**仍然携带那个路径**（查询维度要它），但携带的位置不同 —— 与账号身份那条
/// 同源：`IdentityResolution::Pending` 不携带值，于是 `.unwrap_or(id)` 式降级
/// 编译不过。这里不能完全不带（下游要做粗粒度查询），但至少让它无法被当成正常答案。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Attribution {
    /// 归属到一个已知项目根。
    Root { path: String, source: RootSource },
    /// 没有任何已知根覆盖它 —— **说出来，不拿它冒充项目根**。
    Unattributed { path: String },
    /// 连路径都没有（如 TumeChat 的一次纯闲聊）。
    NoPath,
}

impl Attribution {
    /// 归属到的项目根；`Unattributed` / `NoPath` 一律 `None`。
    ///
    /// 🔴 **这个方法故意不为 `Unattributed` 返回它携带的路径。** 想要那个路径的
    /// 调用方必须显式 `match` —— 否则「兜底」又会经由一个便利方法悄悄变回「答案」，
    /// 而那正是本模块要消除的东西。
    pub fn root(&self) -> Option<&str> {
        match self {
            Attribution::Root { path, .. } => Some(path.as_str()),
            _ => None,
        }
    }

    /// 供**存储与查询**用的路径：归属到根就用根，否则用原路径。
    ///
    /// ⚠️ 与 [`root`](Self::root) 的区别是承重的：这个方法承认「我用的可能是兜底」，
    /// 所以它只该出现在「总得往 `project_root` 列里写点什么」的地方，
    /// 而判断逻辑一律用 `root()`。
    pub fn storage_path(&self) -> Option<&str> {
        match self {
            Attribution::Root { path, .. } | Attribution::Unattributed { path } => {
                Some(path.as_str())
            }
            Attribution::NoPath => None,
        }
    }

    pub fn is_attributed(&self) -> bool {
        matches!(self, Attribution::Root { .. })
    }
}

/// 已知项目根的集合 —— **发现的产物，归属的唯一输入**。
///
/// 有意做得很薄：它不知道怎么发现根，也不知道谁在用它。加进来的每一条都是
/// 「某个来源在某个时刻认为这是个项目根」。
#[derive(Debug, Clone, Default)]
pub struct RootRegistry {
    /// `(归一化路径, 原始路径, 来源)`，按归一化路径排序，便于最长前缀匹配。
    roots: Vec<(String, String, RootSource)>,
    /// WSL 挂载表 —— 让 `/mnt/c/X` 与 `C:\X` 认成同一个根。空表 = 不收敛。
    ///
    /// 🔴 **住在注册表里，而不是当参数传给 `attribute`**：写入键与查询键必须由
    /// **同一份**表算出来，否则一个已登记的根会查不到，而那**不报错** ——
    /// 只表现成「归属突然失效」。放进结构体，两侧就没有拿不同表的机会。
    mounts: DriveMounts,
}

impl RootRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 带挂载表的注册表 —— `/mnt/<drive>/…` 与它对应的 `<drive>:\…` 收敛成一个根。
    ///
    /// 🔴 为什么收敛在**这一层**：`/mnt/c/X` 与 `C:\X` 在本机是同一个目录，
    /// 不是两个项目。曾经的做法是在发现侧把探到的 Windows 根转回 `/mnt` 形式，
    /// 那让 `/mnt` 那一族**自成一个根** —— 干跑实测 `/mnt/c/…/QuotaBar` 与
    /// `C:\…\QuotaBar` 是两条，且前者 `canonical_repo_id` 读不到 `.git/config`
    /// （Windows 上 stat 不了 `/mnt/…`），只能落 `path:` id，于是身份层也分家。
    /// 根因是**注册表按比较键判覆盖，而同一目录在本机有多种形式** ——
    /// 所以形式收敛属于比较规则，不是某个发现分支的补丁。
    pub fn with_mounts(mounts: DriveMounts) -> Self {
        Self {
            mounts,
            ..Self::default()
        }
    }

    /// 本注册表的比较键 —— **归一化规则的唯一出口**（见 [`registry_key`]）。
    pub fn key(&self, path: &str) -> String {
        registry_key(path, &self.mounts)
    }

    /// 从 `(路径, 来源)` 构建。重复路径按**后来者覆盖**，与注册表写入语义一致。
    ///
    /// 不叫 `from_iter` —— 那个名字会与 `std::iter::FromIterator` 混淆，而这里
    /// **不是**那个语义（它带覆盖规则，不是纯收集）。
    pub fn from_roots<I, S>(items: I) -> Self
    where
        I: IntoIterator<Item = (S, RootSource)>,
        S: AsRef<str>,
    {
        let mut reg = Self::new();
        for (p, src) in items {
            reg.insert(p.as_ref(), src);
        }
        reg
    }

    pub fn insert(&mut self, path: &str, source: RootSource) {
        let path = path.trim();
        if path.is_empty() {
            return;
        }
        let key = self.key(path);
        match self
            .roots
            .binary_search_by(|(k, _, _)| k.as_str().cmp(key.as_str()))
        {
            Ok(i) => self.roots[i] = (key, path.to_string(), source),
            Err(i) => self.roots.insert(i, (key, path.to_string(), source)),
        }
    }

    pub fn len(&self) -> usize {
        self.roots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.roots.is_empty()
    }

    /// 已知根的原始路径，按归一化序。
    pub fn roots(&self) -> impl Iterator<Item = (&str, RootSource)> {
        self.roots.iter().map(|(_, p, s)| (p.as_str(), *s))
    }
}

/// 把一个路径归属到它所属的项目根。**纯函数、零 I/O。**
///
/// 规则：**最长匹配的已知根**。一个根是另一个的真后代时取更长的那个 ——
/// `QuotaBar/third_party/TumeFlow` 有自己的 `.git`，它比 `QuotaBar` 更准确。
///
/// 匹配是**路径段级**的，不是字符串前缀：`/a/bc` 不能被 `/a/b` 匹上。
/// 大小写不敏感、分隔符归一（Windows 与 WSL 形式在同一份注册表里共存）。
///
/// ⚠️ **注册表为空时一律 `Unattributed`**，不是「随便挑一个」也不是 panic。
/// 发现整个失效（svault 缺席、扫描没跑）时，归属该一致地说不出来 ——
/// 而不是退回「用 cwd 当根」那个老答案。
pub fn attribute(path: Option<&str>, registry: &RootRegistry) -> Attribution {
    let Some(raw) = path.map(str::trim).filter(|p| !p.is_empty()) else {
        return Attribution::NoPath;
    };
    let key = registry.key(raw);
    // 从最长往回找：注册表按归一化路径排序，比 key 大的不可能是它的前缀。
    let key_ref: &str = &key;
    let upper = registry
        .roots
        .partition_point(|(k, _, _)| k.as_str() <= key_ref);
    for (k, original, source) in registry.roots[..upper].iter().rev() {
        if covers(k, key_ref) {
            return Attribution::Root {
                path: original.clone(),
                source: *source,
            };
        }
    }
    Attribution::Unattributed {
        path: raw.to_string(),
    }
}

/// 注册表存储用的比较键 —— **归一化规则的唯一出口**。
///
/// 🔴 存储侧必须用它，不能自己写一遍 `to_lowercase().replace('\', "/")`。
/// 两份归一化规则各自演化时，写进去的键和查出来的键会对不上，而那**不会报错** ——
/// 只会让一个已登记的根查不到，表现成「归属突然失效」。本仓已有判例：
/// 规范形拼串曾散在多处，收口到 `pathnorm::canonical_wsl_unc` 一处。
///
/// `mounts` 非空时先把 `/mnt/<drive>/…` 换算成宿主形式 —— **同一个目录只能有
/// 一个键**。读写两侧必须传同一份表，所以正常路径都经 [`RootRegistry::key`]，
/// 它持有那份表；本函数直接调用只用于存储层算行键。
pub fn registry_key(path: &str, mounts: &DriveMounts) -> String {
    let p = path.trim();
    match mnt_to_windows(p, mounts) {
        Some(host) => normalize(&host).into_owned(),
        None => normalize(p).into_owned(),
    }
}

/// `root` 是否覆盖 `path`（相等，或 `path` 在它之下）。**按路径段比，不按字符**。
fn covers(root: &str, path: &str) -> bool {
    if path == root {
        return true;
    }
    path.len() > root.len() && path.starts_with(root) && path.as_bytes()[root.len()] == b'/'
}

/// 归一化用于比较的形式：反斜杠→正斜杠、去尾斜杠、小写。
///
/// 🔴 **只用于比较，不用于存储** —— 注册表同时留着原始路径，归属结果返回的是
/// 原始形式。归一化是这一层的内部细节，不该泄漏到 `project_root` 列里去。
fn normalize(p: &str) -> Cow<'_, str> {
    let needs = p.contains('\\') || p.ends_with('/') || p.chars().any(char::is_uppercase);
    if !needs {
        return Cow::Borrowed(p);
    }
    let mut s = p.replace('\\', "/").to_lowercase();
    while s.len() > 1 && s.ends_with('/') {
        s.pop();
    }
    Cow::Owned(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reg(paths: &[(&str, RootSource)]) -> RootRegistry {
        RootRegistry::from_roots(paths.iter().copied())
    }

    #[test]
    fn attributes_a_subdirectory_to_its_root() {
        let r = reg(&[("/home/u/EyeVLM", RootSource::Git)]);
        let a = attribute(Some("/home/u/EyeVLM/docs/model_report"), &r);
        assert_eq!(a.root(), Some("/home/u/EyeVLM"));
        assert!(a.is_attributed());
    }

    #[test]
    fn takes_the_longest_match_not_the_first() {
        // 🔴 子模块有自己的 .git ⇒ 它比父仓更准确。
        let r = reg(&[
            ("/w/QuotaBar", RootSource::Git),
            ("/w/QuotaBar/third_party/TumeFlow", RootSource::Git),
        ]);
        assert_eq!(
            attribute(Some("/w/QuotaBar/third_party/TumeFlow/src/x.py"), &r).root(),
            Some("/w/QuotaBar/third_party/TumeFlow"),
        );
        assert_eq!(
            attribute(Some("/w/QuotaBar/src-tauri/src"), &r).root(),
            Some("/w/QuotaBar"),
        );
    }

    #[test]
    fn wsl_paths_attribute_without_any_stat() {
        // 这是本 ADR 的要害：71.4% 的事件是这种形式，而它们在本机 stat 不了。
        // 归属是纯字符串的，所以它们照样归位。
        let r = reg(&[(
            "wsl:Ubuntu-22.04:/home/simon/workspace/EyeVLM",
            RootSource::Scan,
        )]);
        assert_eq!(
            attribute(
                Some("wsl:Ubuntu-22.04:/home/simon/workspace/EyeVLM/experimental_model/seg"),
                &r,
            )
            .root(),
            Some("wsl:Ubuntu-22.04:/home/simon/workspace/EyeVLM"),
        );
    }

    #[test]
    fn matching_is_path_segment_wise_not_string_prefix() {
        // `/a/bc` 不该被 `/a/b` 匹上 —— 那是两个不同的目录。
        let r = reg(&[("/a/b", RootSource::Git)]);
        assert!(!attribute(Some("/a/bc"), &r).is_attributed());
        assert!(attribute(Some("/a/b/c"), &r).is_attributed());
    }

    #[test]
    fn separator_and_case_are_normalized_for_comparison_only() {
        let r = reg(&[(r"C:\Users\u\QuotaBar", RootSource::Git)]);
        let a = attribute(Some(r"c:\users\u\quotabar\src-tauri"), &r);
        // 🔴 返回的是注册表里的**原始**形式，不是归一化后的小写串。
        assert_eq!(a.root(), Some(r"C:\Users\u\QuotaBar"));
    }

    #[test]
    fn unknown_path_is_unattributed_not_itself() {
        let r = reg(&[("/w/QuotaBar", RootSource::Git)]);
        let a = attribute(Some("/somewhere/else/deep"), &r);
        assert!(!a.is_attributed());
        // 🔴 `root()` 不给它 —— 想要那个路径必须显式 match。
        assert_eq!(a.root(), None);
        assert_eq!(a.storage_path(), Some("/somewhere/else/deep"));
    }

    #[test]
    fn empty_registry_says_it_cannot_tell_rather_than_guessing() {
        // 发现整个失效时，归属该一致地说不出来 —— 而不是退回「用 cwd 当根」。
        let a = attribute(Some("/w/QuotaBar/src"), &RootRegistry::new());
        assert!(matches!(a, Attribution::Unattributed { .. }));
    }

    #[test]
    fn no_path_is_its_own_variant() {
        // TumeChat 的一次纯闲聊没有任何项目路径可言。
        assert_eq!(attribute(None, &RootRegistry::new()), Attribution::NoPath);
        assert_eq!(
            attribute(Some("   "), &RootRegistry::new()),
            Attribution::NoPath
        );
        assert_eq!(Attribution::NoPath.storage_path(), None);
    }

    #[test]
    fn the_root_itself_attributes_to_itself() {
        let r = reg(&[("/w/QuotaBar", RootSource::Git)]);
        assert_eq!(
            attribute(Some("/w/QuotaBar"), &r).root(),
            Some("/w/QuotaBar")
        );
        // 尾斜杠不该让它变成 Unattributed
        assert_eq!(
            attribute(Some("/w/QuotaBar/"), &r).root(),
            Some("/w/QuotaBar")
        );
    }

    #[test]
    fn registry_insert_is_last_write_wins() {
        let mut r = RootRegistry::new();
        r.insert("/w/P", RootSource::Marker);
        r.insert("/w/P", RootSource::Git);
        assert_eq!(r.len(), 1);
        match attribute(Some("/w/P/x"), &r) {
            Attribution::Root { source, .. } => assert_eq!(source, RootSource::Git),
            other => panic!("expected Root, got {other:?}"),
        }
    }

    #[test]
    fn root_source_round_trips() {
        for s in [
            RootSource::Git,
            RootSource::Marker,
            RootSource::Scan,
            RootSource::Configured,
        ] {
            assert_eq!(RootSource::parse(s.as_str()), Some(s));
        }
        assert_eq!(RootSource::parse("nonsense"), None);
    }
}
