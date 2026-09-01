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
//! 上 stat 不了 WSL 路径；同一个项目被记成 11 个 `project_root`（`VisionApp/docs`、
//! `VisionApp/experimental_model/…`）。
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
    let fold = folds_case(p);
    let needs = p.contains('\\') || p.ends_with('/') || (fold && p.chars().any(char::is_uppercase));
    if !needs {
        return Cow::Borrowed(p);
    }
    let mut s = p.replace('\\', "/");
    if fold {
        s = s.to_lowercase();
    }
    while s.len() > 1 && s.ends_with('/') {
        s.pop();
    }
    Cow::Owned(s)
}

/// 这条路径所在的文件系统**是否**大小写不敏感。
///
/// 🔴 **不能一律小写。** Linux 上 `/home/u/Foo` 与 `/home/u/foo` 是**两个真实目录**，
/// 可以各自是一个仓库；全部折叠会让它们在注册表里撞成一个键，归属跟着归到后写入的
/// 那个 —— 症状是**跨项目的会话与记忆被混在一起**，而且不报任何错。
///
/// 判据按**路径命名空间**，不按当前宿主：一条 `wsl:<distro>:/home/…` 在 Windows 上
/// 被处理时，它描述的仍然是 Linux 文件系统。
///
/// | 形式 | 折叠 |
/// | --- | --- |
/// | `C:\…` / `c:/…`（Windows 盘符） | ✅ |
/// | `/mnt/<drive>/…`（WSL 里挂的 Windows 盘，底下是 NTFS） | ✅ |
/// | `wsl:<distro>:/…`、`//wsl$/…`、裸 `/home/…` | ❌ |
/// | 其余（相对路径等） | ❌ 保守：宁可少收敛，不可错并 |
fn folds_case(p: &str) -> bool {
    // `wsl:<distro>:/path` —— 冒号前是 `wsl` 时那是规范形前缀，不是盘符。
    // 里面的 Linux 路径除非本身是 `/mnt/<drive>`，否则不折叠。
    if let Some(rest) = p.strip_prefix("wsl:").or_else(|| p.strip_prefix("WSL:")) {
        let linux = rest.split_once(':').map_or(rest, |(_, tail)| tail);
        return crate::pathnorm::is_windows_drive_mount(linux);
    }
    // UNC 形式的 WSL 路径同理（`\\wsl$\Ubuntu\home\…`）。
    if crate::pathnorm::canonical_wsl_unc(p).is_some() {
        return false;
    }
    // 单字母 + 冒号 = Windows 盘符。
    let b = p.as_bytes();
    if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        return true;
    }
    crate::pathnorm::is_windows_drive_mount(p)
}

// ── 按路径反查项目身份（ADR-050 的读侧出口）──────────────────────────────────
//
// ## 为什么它必须在这一层，而不是每个消费者各写一份
//
// 注册表按**发现时看到的那种写法**存根：从 Windows 侧发现的 WSL 项目存成
// `wsl:<distro>:/home/…`，而**在那个发行版里跑**的会话记下的 `cwd` 是裸 Linux 路径
// `/home/…`。两者指同一个目录，直接比较却对不上 —— 而且**不报错**，只表现成
// 「这条会话没有项目」。
//
// 🔴 实测（2026-09-01，某台开发机）：281 条 codex 会话里 **31 条**因为这一条认不出
// 归属，其中包括项目搬家**之后**新开的那几条 —— 所以它不是历史债，是**每个在发行版
// 里跑的会话都会中招**的活缺陷。
//
// 消费者自己补这条归一化是能补的，代价是同一条规则出现 N 份：本仓已经为
// 「同一条规则两处实现」付过多次代价（`decode_project_dir` 被删就是其中一次）。
// 所以出口开在这里，与 `roots` 是「项目身份的唯一对外出口」同一个模式。

/// 注册表里一个根的**身份视图** —— [`identify_path`] 的输入。
///
/// 刻意只带三个字段：这一层不需要知道存储层 `ProjectRootRow` 的其余部分，
/// 少一个依赖就少一处「`store` feature 关掉时编不过」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootIdentity {
    /// 注册表里的原始写法（`C:\…` / `wsl:<distro>:/…` / UNC / 裸 Linux）。
    pub root: String,
    /// 同一个根的其它**等价写法**（`ProjectRootRow::aliases`）。
    ///
    /// 🔴 **必须参与匹配。** 注册表按发现时看到的那种形式存根：UNC 形式登记的根，
    /// 它的规范形只在 `aliases` 里。只拿 `root` 去比，一条发行版内的裸路径查询
    /// 就永远撞不上那个 UNC 根 —— 而结果是 `unknown`，**看起来像「库里没有」**。
    pub aliases: Vec<String>,
    /// 规范身份（`git:…`）。`None` = 这个根问不出身份，**为什么**看 `identity_verdict`。
    pub canonical_id: Option<String>,
    /// 身份探测结论的线上拼写（`not_probed` / `resolved` / `no_identity` / `unresolved`）。
    pub identity_verdict: String,
}

/// [`RootIdentity::identity_verdict`] 里**「确认没有 remote」**那一个取值。
///
/// 🔴 提成常量是为了让「只有它才配叫 `NoIdentity`」这件事有一处可查的依据 ——
/// 其余取值（`not_probed` / `unresolved`）是「**没问成**」，处置是等或重试。
pub const VERDICT_NO_IDENTITY: &str = "no_identity";

/// 一条路径的身份判决 —— **五态，每种「说不出来」有自己的名字**。
///
/// 🔴 压成 `Option<String>` 就又造一个「没问成长得像没有」：调用方分不出
/// 「这个目录不属于任何已知项目」「属于一个没有 remote 的仓库」「身份这一轮没问成」
/// 「两个根都盖得住它」这四件事，而它们的下一步完全不同（找 / 接受 / 重试 / 别猜）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathIdentity {
    /// 认出来了。`matched_form` = **查询**的哪种写法撞上的（原样 / 展开出的规范形 /
    /// 裸路径），不是根那一侧的别名 —— 排查「为什么这条认出来了那条没有」第一个要问它。
    Resolved {
        root: String,
        canonical_id: String,
        matched_form: String,
    },
    /// 找到根了，且**确认**那个仓库没有可跨 checkout 的身份（没有 origin remote）。
    /// 这是事实，不是失败 —— 本地仓库就是这样，**接受**。
    NoIdentity { root: String, matched_form: String },
    /// 找到根了，但它的身份**这一轮没问成**（`not_probed` 还没扫到 / `unresolved`
    /// 探测失败：访问桥不通、权限拒绝）。
    ///
    /// 🔴 **它与 `NoIdentity` 不是同一件事**，而它们此前被压成同一个判决 ——
    /// 消费方会按「确认没有，接受」处理一个其实该**重试**的根，
    /// 于是一次权限错误变成一条永久的错误结论。`verdict` 带上是哪一种。
    IdentityUnavailable {
        root: String,
        matched_form: String,
        verdict: String,
    },
    /// 同样具体的两个根盖住它，而它们身份不同 —— **不猜**。
    Ambiguous { candidates: Vec<(String, String)> },
    /// 没有任何已知根盖住它。`tried` 是试过的写法，让「没试到」与「试了没有」分得开。
    Unknown { tried: Vec<String> },
}

/// 一个比较键的**路径深度** —— 剥掉命名空间前缀之后的段数。
///
/// 🔴 **不能用字符串长度当「具体性」。** `wsl:<distro>:/home/u/proj` 与
/// `/home/u/proj` 指向同一深度的路径，但前者多了 `wsl:<distro>:` 那一截；
/// 按长度排序会让它**无条件胜出**，于是两个身份不同的同深度根不会进歧义分支，
/// 而是被静默解析成其中一个 —— 跨项目错误归属，且不报错。
fn namespace_free_depth(key: &str) -> usize {
    let bare = strip_namespace(key);
    bare.split('/').filter(|s| !s.is_empty()).count()
}

/// 剥掉 `wsl:<distro>:` / `//wsl.localhost/<distro>` / `//wsl$/<distro>` 前缀。
/// 剥不掉就原样返回（Windows 盘符路径、裸 Linux 路径本来就没有命名空间前缀）。
fn strip_namespace(key: &str) -> &str {
    if let Some(rest) = key.strip_prefix("wsl:") {
        if let Some((_, tail)) = rest.split_once(':') {
            return tail;
        }
    }
    for prefix in ["//wsl.localhost/", "//wsl$/"] {
        if let Some(rest) = key.strip_prefix(prefix) {
            return rest.split_once('/').map_or("", |(_, tail)| tail);
        }
    }
    key
}

/// 一条路径的等价写法 —— 裸 Linux ⇄ `wsl:<distro>:/…`。
///
/// 🔴 **裸 Linux 路径本身不含发行版**，所以展开需要调用方给出候选发行版；
/// 给不出就只有原样一种写法，而那正是今天对不上的原因。展开出多个候选时，
/// 判决可能是 [`PathIdentity::Ambiguous`] —— 那是**正确**的答案，不是缺陷。
pub fn path_forms(path: &str, distros: &[String]) -> Vec<String> {
    let p = path.trim();
    let mut out = vec![p.to_string()];
    let push = |out: &mut Vec<String>, s: String| {
        if !out.contains(&s) {
            out.push(s);
        }
    };
    // `wsl:<distro>:/p` → 裸 `/p`（在那个发行版里跑的会话会这么记）。
    if let Some(rest) = p.strip_prefix("wsl:").or_else(|| p.strip_prefix("WSL:")) {
        if let Some((_, tail)) = rest.split_once(':') {
            push(&mut out, tail.to_string());
        }
    }
    // UNC → 规范形 → 裸；`canonical_wsl_unc` 是规范形拼串的唯一出口。
    if let Some(canon) = crate::pathnorm::canonical_wsl_unc(p) {
        push(&mut out, canon.clone());
        if let Some((_, tail)) = canon.trim_start_matches("wsl:").split_once(':') {
            push(&mut out, tail.to_string());
        }
    }
    // 裸 Linux 绝对路径 → 每个候选发行版的规范形。
    // ⚠️ `/mnt/<drive>/…` 不在此列：那是挂进来的 Windows 盘，由 `registry_key`
    //    的挂载表换算，再套发行版前缀会把同一个目录变成两个键。
    if p.starts_with('/') && !crate::pathnorm::is_windows_drive_mount(p) {
        for d in distros {
            push(&mut out, format!("wsl:{d}:{p}"));
        }
    }
    out
}

/// 这条路径属于哪个项目 —— **纯函数、零 I/O**，与 [`attribute`] 同一条纪律。
///
/// 嵌套的根按**最具体**的那个算（`…/QuotaBar` 与 `…/QuotaBar/third_party/X` 同时
/// 盖住时取后者）—— 否则每个子模块都会被判成歧义。只有**同样具体**的两个根身份不同
/// 才是真歧义。
pub fn identify_path(
    path: &str,
    distros: &[String],
    roots: &[RootIdentity],
    mounts: &DriveMounts,
) -> PathIdentity {
    let forms = path_forms(path, distros);
    // (根, 撞上它的那种写法, **剥掉命名空间之后**的路径深度)
    let mut hits: Vec<(&RootIdentity, &str, usize)> = Vec::new();
    for form in &forms {
        let key = registry_key(form, mounts);
        for r in roots {
            if hits.iter().any(|(h, _, _)| h.root == r.root) {
                continue;
            }
            // 🔴 `root` 与它的**所有别名**都参与匹配：UNC 形式登记的根，
            // 它的规范形只在 `aliases` 里。只比 `root` 就永远撞不上。
            let matched = std::iter::once(&r.root)
                .chain(r.aliases.iter())
                .map(|form| registry_key(form, mounts))
                .find(|rk| covers(rk, &key));
            if let Some(rk) = matched {
                hits.push((r, form.as_str(), namespace_free_depth(&rk)));
            }
        }
    }
    if hits.is_empty() {
        return PathIdentity::Unknown { tried: forms };
    }
    hits.sort_by_key(|(_, _, depth)| std::cmp::Reverse(*depth));
    let (top, form, top_depth) = hits[0];
    // 同样具体、身份不同 ⇒ 歧义。身份相同的多条写法不是歧义（同一个仓库）。
    let tied = hits
        .iter()
        .any(|(r, _, depth)| *depth == top_depth && r.canonical_id != top.canonical_id);
    if tied {
        let mut candidates: Vec<(String, String)> = hits
            .iter()
            .filter(|(_, _, depth)| *depth == top_depth)
            .map(|(r, _, _)| {
                (
                    r.root.clone(),
                    r.canonical_id.clone().unwrap_or_else(|| "-".to_string()),
                )
            })
            .collect();
        candidates.sort();
        candidates.dedup();
        return PathIdentity::Ambiguous { candidates };
    }
    match &top.canonical_id {
        Some(id) => PathIdentity::Resolved {
            root: top.root.clone(),
            canonical_id: id.clone(),
            matched_form: form.to_string(),
        },
        // 🔴 **只有确认没有 remote 才叫 `NoIdentity`。** `not_probed` / `unresolved`
        // 是「没问成」，处置是等或重试 —— 把它们说成「确认没有」，等于让一次
        // 权限错误变成一条永久的错误结论。
        None if top.identity_verdict == VERDICT_NO_IDENTITY => PathIdentity::NoIdentity {
            root: top.root.clone(),
            matched_form: form.to_string(),
        },
        None => PathIdentity::IdentityUnavailable {
            root: top.root.clone(),
            matched_form: form.to_string(),
            verdict: top.identity_verdict.clone(),
        },
    }
}

#[cfg(test)]
// 测试要造 fixture（建目录、写文件、再核一遍），允许直接碰盘 —— 文件系统边界
// 管的是**生产行为**，而 `#[cfg(test)]` 不在生产路径上。
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    fn reg(paths: &[(&str, RootSource)]) -> RootRegistry {
        RootRegistry::from_roots(paths.iter().copied())
    }

    // ── 大小写：按路径命名空间，不按宿主 ──────────────────────────────
    //
    // 🔴 这一组是评审 [P1] 逼出来的：`normalize` 原先**无条件小写**，于是 Linux 上
    // 两个仅大小写不同的真实仓库在注册表里撞成一个键，归属归到后写入的那个 ——
    // 跨项目的会话与记忆被混在一起，且不报任何错。

    #[test]
    fn two_linux_projects_differing_only_in_case_stay_apart() {
        let r = reg(&[
            ("/home/u/Foo", RootSource::Git),
            ("/home/u/foo", RootSource::Git),
        ]);
        assert_eq!(r.len(), 2, "Linux 上这是两个目录，不能撞成一个键");
        assert_eq!(
            attribute(Some("/home/u/Foo/src"), &r).root(),
            Some("/home/u/Foo")
        );
        assert_eq!(
            attribute(Some("/home/u/foo/src"), &r).root(),
            Some("/home/u/foo")
        );
    }

    #[test]
    fn wsl_canonical_paths_keep_case_too() {
        // 规范形描述的仍是 Linux 文件系统 —— 即便这段代码跑在 Windows 上。
        let r = reg(&[
            ("wsl:Ubuntu:/home/u/Foo", RootSource::Git),
            ("wsl:Ubuntu:/home/u/foo", RootSource::Git),
        ]);
        assert_eq!(r.len(), 2);
        assert_eq!(
            attribute(Some("wsl:Ubuntu:/home/u/foo/deep"), &r).root(),
            Some("wsl:Ubuntu:/home/u/foo")
        );
    }

    #[test]
    fn windows_paths_still_fold_case() {
        // 反向也要钉住：NTFS 大小写不敏感，折叠是**必要**的，不是可选的。
        let r = reg(&[(r"C:\Users\u\Proj", RootSource::Git)]);
        assert_eq!(
            attribute(Some(r"c:\users\u\proj\src"), &r).root(),
            Some(r"C:\Users\u\Proj")
        );
    }

    #[test]
    fn a_windows_drive_mounted_into_wsl_folds_case() {
        // `/mnt/c/…` 底下就是 NTFS —— 虽然写成 Linux 形式，规则跟着**文件系统**走。
        let r = reg(&[("/mnt/c/Users/u/Proj", RootSource::Git)]);
        assert_eq!(
            attribute(Some("/mnt/c/users/u/proj/src"), &r).root(),
            Some("/mnt/c/Users/u/Proj")
        );
        // 而 wsl 规范形里的 /mnt/c 同样折叠。
        let r2 = reg(&[("wsl:Ubuntu:/mnt/d/Work", RootSource::Git)]);
        assert_eq!(
            attribute(Some("wsl:Ubuntu:/mnt/d/work/x"), &r2).root(),
            Some("wsl:Ubuntu:/mnt/d/Work")
        );
    }

    #[test]
    fn attributes_a_subdirectory_to_its_root() {
        let r = reg(&[("/home/u/VisionApp", RootSource::Git)]);
        let a = attribute(Some("/home/u/VisionApp/docs/model_report"), &r);
        assert_eq!(a.root(), Some("/home/u/VisionApp"));
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
            "wsl:Ubuntu-22.04:/home/dev/workspace/VisionApp",
            RootSource::Scan,
        )]);
        assert_eq!(
            attribute(
                Some("wsl:Ubuntu-22.04:/home/dev/workspace/VisionApp/experimental_model/seg"),
                &r,
            )
            .root(),
            Some("wsl:Ubuntu-22.04:/home/dev/workspace/VisionApp"),
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

    // ── identify_path ─────────────────────────────────────────────────────
    //
    // 🔴 路径一律用**脱敏**的假名（`/home/u/ws/…`、`Distro`、`example.com`）——
    // 本仓是公开仓，真实用户名 / 内网主机名 / 项目名不进代码。

    fn root(path: &str, id: Option<&str>) -> RootIdentity {
        RootIdentity {
            root: path.to_string(),
            aliases: Vec::new(),
            canonical_id: id.map(str::to_string),
            identity_verdict: if id.is_some() {
                "resolved"
            } else {
                VERDICT_NO_IDENTITY
            }
            .to_string(),
        }
    }

    /// 身份**没问成**的根（`not_probed` / `unresolved`）—— 与「确认没有」是两回事。
    fn root_unprobed(path: &str, verdict: &str) -> RootIdentity {
        RootIdentity {
            root: path.to_string(),
            aliases: Vec::new(),
            canonical_id: None,
            identity_verdict: verdict.to_string(),
        }
    }

    fn root_with_aliases(path: &str, aliases: &[&str], id: Option<&str>) -> RootIdentity {
        RootIdentity {
            aliases: aliases.iter().map(|s| s.to_string()).collect(),
            ..root(path, id)
        }
    }

    /// 本模块存在的**全部理由**：注册表存带发行版限定的规范形，而在那个发行版里跑的
    /// 会话记的是裸 Linux 路径。这一条红了，出口就没意义了。
    #[test]
    fn a_bare_linux_cwd_resolves_to_the_distro_qualified_root() {
        let roots = [root(
            "wsl:Distro:/home/u/ws/proj",
            Some("git:example.com/o/proj"),
        )];
        let got = identify_path(
            "/home/u/ws/proj",
            &["Distro".to_string()],
            &roots,
            &DriveMounts::new(),
        );
        match got {
            PathIdentity::Resolved {
                root,
                canonical_id,
                matched_form,
            } => {
                assert_eq!(root, "wsl:Distro:/home/u/ws/proj");
                assert_eq!(canonical_id, "git:example.com/o/proj");
                // 撞上它的是**展开出来的**那种写法 —— 排查时要看得见。
                assert_eq!(matched_form, "wsl:Distro:/home/u/ws/proj");
            }
            other => panic!("expected Resolved, got {other:?}"),
        }
    }

    /// 不给发行版就展不开 —— 这正是修复前的行为，留一条测试钉住「为什么需要它」。
    #[test]
    fn without_a_distro_the_same_path_is_honestly_unknown() {
        let roots = [root(
            "wsl:Distro:/home/u/ws/proj",
            Some("git:example.com/o/proj"),
        )];
        let got = identify_path("/home/u/ws/proj", &[], &roots, &DriveMounts::new());
        match got {
            PathIdentity::Unknown { tried } => assert_eq!(tried, vec!["/home/u/ws/proj"]),
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    /// 嵌套的根取**最具体**的那个 —— 否则每个子模块都会被判成歧义。
    #[test]
    fn a_nested_root_wins_over_its_parent() {
        let roots = [
            root(
                "wsl:Distro:/home/u/ws/outer",
                Some("git:example.com/o/outer"),
            ),
            root(
                "wsl:Distro:/home/u/ws/outer/third_party/inner",
                Some("git:example.com/o/inner"),
            ),
        ];
        let got = identify_path(
            "/home/u/ws/outer/third_party/inner/src",
            &["Distro".to_string()],
            &roots,
            &DriveMounts::new(),
        );
        match got {
            PathIdentity::Resolved { canonical_id, .. } => {
                assert_eq!(canonical_id, "git:example.com/o/inner");
            }
            other => panic!("expected Resolved(inner), got {other:?}"),
        }
    }

    /// 同一个仓库以多种写法登记**不是**歧义 —— 身份相同就是同一个项目。
    #[test]
    fn two_spellings_of_the_same_repo_are_not_ambiguous() {
        let roots = [
            root("wsl:Distro:/home/u/ws/proj", Some("git:example.com/o/proj")),
            root("/home/u/ws/proj", Some("git:example.com/o/proj")),
        ];
        let got = identify_path(
            "/home/u/ws/proj/src",
            &["Distro".to_string()],
            &roots,
            &DriveMounts::new(),
        );
        assert!(matches!(got, PathIdentity::Resolved { .. }), "got {got:?}");
    }

    /// 同样具体、身份不同 ⇒ **不猜**。两个发行版里各有一份同名项目就是这一格。
    #[test]
    fn equally_specific_roots_with_different_identities_are_ambiguous() {
        let roots = [
            root("wsl:A:/home/u/ws/proj", Some("git:example.com/o/one")),
            root("wsl:B:/home/u/ws/proj", Some("git:example.com/o/two")),
        ];
        let got = identify_path(
            "/home/u/ws/proj",
            &["A".to_string(), "B".to_string()],
            &roots,
            &DriveMounts::new(),
        );
        match got {
            PathIdentity::Ambiguous { candidates } => assert_eq!(candidates.len(), 2),
            other => panic!("expected Ambiguous, got {other:?}"),
        }
    }

    /// 「找到根了但那个仓库没有 remote」是**事实**，不是失败 —— 它有自己的名字。
    #[test]
    fn a_root_without_a_remote_reports_no_identity_not_unknown() {
        let roots = [root("wsl:Distro:/home/u/ws/local-only", None)];
        let got = identify_path(
            "/home/u/ws/local-only/src",
            &["Distro".to_string()],
            &roots,
            &DriveMounts::new(),
        );
        match got {
            PathIdentity::NoIdentity { root, .. } => {
                assert_eq!(root, "wsl:Distro:/home/u/ws/local-only")
            }
            other => panic!("expected NoIdentity, got {other:?}"),
        }
    }

    /// `/mnt/<drive>/…` **不**套发行版前缀 —— 它是挂进来的 Windows 盘，由挂载表换算。
    #[test]
    fn a_windows_drive_mount_is_not_expanded_with_a_distro_prefix() {
        let forms = path_forms("/mnt/c/ws/proj", &["Distro".to_string()]);
        assert_eq!(forms, vec!["/mnt/c/ws/proj"]);
    }

    /// 规范形能展回裸路径 —— 反方向也要成立，否则注册表存裸形时又对不上。
    #[test]
    fn a_canonical_form_expands_back_to_the_bare_path() {
        let forms = path_forms("wsl:Distro:/home/u/ws/proj", &[]);
        assert!(forms.contains(&"/home/u/ws/proj".to_string()), "{forms:?}");
    }

    // ── 评审 [P1]×3 + [P2] 逼出来的四条 ───────────────────────────────────
    //
    // 🔴 三条都是同一族：**把「没问成」说成了别的东西**。它们能同时存在，是因为
    // 我原来的测试只覆盖「问到了」和「确认没有」两格 —— 而出事的永远是第三格。

    /// [P1] `unresolved`（探测失败）**不是**「确认没有 remote」。
    /// 说成后者，消费方会「接受」一个其实该**重试**的根 ——
    /// 一次权限错误从此变成永久的错误结论。
    #[test]
    fn a_root_whose_identity_probe_failed_is_not_reported_as_confirmed_absent() {
        let roots = [root_unprobed("wsl:Distro:/home/u/ws/proj", "unresolved")];
        let got = identify_path(
            "/home/u/ws/proj",
            &["Distro".to_string()],
            &roots,
            &DriveMounts::new(),
        );
        match got {
            PathIdentity::IdentityUnavailable { verdict, .. } => assert_eq!(verdict, "unresolved"),
            other => panic!("expected IdentityUnavailable, got {other:?}"),
        }
    }

    /// [P1] `not_probed`（还没扫到）同理 —— 处置是**等**，不是接受。
    #[test]
    fn a_root_not_probed_yet_is_not_reported_as_confirmed_absent() {
        let roots = [root_unprobed("wsl:Distro:/home/u/ws/proj", "not_probed")];
        let got = identify_path(
            "/home/u/ws/proj",
            &["Distro".to_string()],
            &roots,
            &DriveMounts::new(),
        );
        assert!(
            matches!(got, PathIdentity::IdentityUnavailable { .. }),
            "got {got:?}"
        );
    }

    /// [P1] 具体性按**剥掉命名空间后的路径深度**，不按字符串长度。
    ///
    /// `wsl:Distro:/home/u/ws/proj` 比 `/home/u/ws/proj` 长 12 个字符，但它们**同深度**。
    /// 按长度排会让前者无条件胜出 ⇒ 两个身份不同的同深度根被静默解析成其中一个。
    #[test]
    fn same_depth_roots_in_different_namespaces_are_ambiguous_not_longest_string() {
        let roots = [
            root("wsl:Distro:/home/u/ws/proj", Some("git:example.com/o/one")),
            root("/home/u/ws/proj", Some("git:example.com/o/two")),
        ];
        let got = identify_path(
            "/home/u/ws/proj",
            &["Distro".to_string()],
            &roots,
            &DriveMounts::new(),
        );
        match got {
            PathIdentity::Ambiguous { candidates } => {
                assert_eq!(candidates.len(), 2, "{candidates:?}")
            }
            other => panic!("expected Ambiguous, got {other:?}"),
        }
    }

    /// 深度真的不同时仍取更深的那个 —— 修 [P1] 不能把「嵌套根」那条弄反。
    #[test]
    fn a_deeper_root_still_wins_across_namespaces() {
        let roots = [
            root("/home/u/ws/outer", Some("git:example.com/o/outer")),
            root(
                "wsl:Distro:/home/u/ws/outer/inner",
                Some("git:example.com/o/inner"),
            ),
        ];
        let got = identify_path(
            "/home/u/ws/outer/inner/src",
            &["Distro".to_string()],
            &roots,
            &DriveMounts::new(),
        );
        match got {
            PathIdentity::Resolved { canonical_id, .. } => {
                assert_eq!(canonical_id, "git:example.com/o/inner")
            }
            other => panic!("expected Resolved(inner), got {other:?}"),
        }
    }

    /// [P1] UNC 形式登记的根：它的规范形只在 `aliases` 里，别名必须参与匹配。
    /// 不参与就恒 `unknown` —— 而 `unknown` 看起来像「库里没有这个根」。
    #[test]
    fn a_unc_root_is_matched_through_its_canonical_alias() {
        let roots = [root_with_aliases(
            "//wsl.localhost/Distro/home/u/ws/proj",
            &["wsl:Distro:/home/u/ws/proj"],
            Some("git:example.com/o/proj"),
        )];
        let got = identify_path(
            "/home/u/ws/proj/src",
            &["Distro".to_string()],
            &roots,
            &DriveMounts::new(),
        );
        match got {
            PathIdentity::Resolved {
                root, matched_form, ..
            } => {
                assert_eq!(root, "//wsl.localhost/Distro/home/u/ws/proj");
                // `matched_form` 是**查询**的哪种写法撞上的（这里是展开出来的规范形），
                // 不是根那一侧的别名 —— 两者都有用，但这个字段只答前者。
                assert_eq!(matched_form, "wsl:Distro:/home/u/ws/proj/src");
            }
            other => panic!("expected Resolved, got {other:?}"),
        }
    }

    /// 剥命名空间只剥前缀，不动路径本身。
    #[test]
    fn namespace_stripping_keeps_the_path_intact() {
        assert_eq!(namespace_free_depth("wsl:Distro:/home/u/ws/proj"), 4);
        assert_eq!(namespace_free_depth("/home/u/ws/proj"), 4);
        assert_eq!(namespace_free_depth("//wsl.localhost/Distro/home/u/ws"), 3);
        assert_eq!(namespace_free_depth("c:/ws/proj"), 3);
    }
}
