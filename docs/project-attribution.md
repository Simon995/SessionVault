# ADR-050：项目归属分成「发现」与「归属」两层 —— I/O 能力不该决定语义

> 状态：**实现中**（2026-08-12）——
> **步 1 已落地**（`attribution.rs` 归属纯函数 + `project_root_registry` 表，
> 17 条测试 / 7 个变异精确变红）；**步 2 已完成**；**步 3 代码已实现并在真库副本上跑通，
> 但尚未在真库执行**（不可逆，等人确认），见下方「落地步骤」。
> 跨仓：SessionVault（解析器 + 注册表）· QuotaBar（发现的一个来源）· TumeFlow（消费）
> 前置：ADR-032 P1/P2（`identity.rs` + `project_identity` 表）已落地
> 触发：`PARSER_REVISION` 3 → 4，全库重投影（ADR-044 决定 6 的第③触发条件）

## 🔴 为什么这套逻辑归 SessionVault

**不是因为「多个仓库都依赖它」** —— 实测 TumeChat 完全不碰 SessionVault
（`package.json` 无依赖、源码零引用），它只经 TumeFlow 的 daemon 拿记忆。

真正的理由是：**`project_root` 是 `RawEvent` 的字段，只由 `parser.rs` 写 ——
摄取只在 SessionVault 发生**。归属逻辑必须与产生这个字段的地方同处，而下游
（TumeFlow 的 `episode.project_root`、QuotaBar 的 `usage_facts`）**只消费结果**。

这个区分是承重的：按「多仓共享」的理由，会得出「做成一个公共库让各方调用」的设计
—— 那恰恰会把同一套规则复制到多个进程里各自演化，正是本 ADR 根因二要消除的东西。
正确的形态是**下游根本没有这个逻辑**。

## 症状

实测全库 991,621 条事件、365 个会话（2026-08-12）：

```
111 个不同的 project_root
  按「已知项目根最长前缀」归一后 → 43 个（收敛 61%）
  43 个里还有大量同一个仓的不同路径形式（下面第二层）

365 个会话
  单根                334（91.5%）
  跨子目录（同一仓）     30
  跨仓库                1 ← sid 是 "snapshot"，Class-B 快照的合成 id，不是真会话
```

一个会话 11 个 `project_root`，而那 11 个全是 `EyeVLM/…` 的子目录
（`docs/model_report`、`experimental_model/segmentation_oct/outputs/phase2`…）。
**那不是「跨了 11 个项目」，是同一个项目被记成了 11 个。**

按 `source` 分（用真实的 `resolve_project_root` 对全部 111 个跑一遍）：

| source | root 数 | 事件数 | 含义 |
| --- | --- | --- | --- |
| `wsl_cwd` | 68 | 707,386（71.4%） | 被守卫挡在 `find_upward` 门外，**从没做过项目根解析** |
| `git` | 5 | 151,016 | 命中 `.git` |
| `cwd` | 37 | 123,146 | 走了 `find_upward` 但一路没命中（路径在本机不存在） |
| `marker:Cargo.toml` | 1 | 8,692 | 命中构建 marker，**停在了子构建单元上** |

## 🔴 根因一：I/O 能力决定了语义

`resolve_project_root` 用「**我此刻能不能在本机 stat 这个路径**」来决定「**要不要回答
归属问题**」：

```rust
let is_unstattable_wsl = split_canonical_wsl(cwd).is_some()
    || canonical_wsl_unc(cwd).is_some()
    || (host == Windows && is_bare_linux_path(cwd));
if is_unstattable_wsl {
    return ProjectRoot { path: Some(cwd), source: "wsl_cwd" };  // ← 用 cwd 冒充答案
}
```

⚠️ **那个守卫本身是对的**：在 Windows 上把 `/home/simon/…` 当本地路径去 walk，会去错盘、
甚至命中无关仓库（`project_root.rs` 的测试注释里记着两次真踩过）。问题不在守卫，
**在守卫之后做了什么**。

「这个 cwd 属于哪个项目」是一个**语义问题** —— 它的答案不该依赖于我此刻有没有能力读盘。
而现在：读不了 ⇒ 不回答 ⇒ **把 cwd 当答案交出去**。

`source: "wsl_cwd"` 这个标签看起来像在诚实交代「我知道这是个 WSL cwd」，但它产出的
`path` **被下游当作 project_root 使用**（`RawEvent.project_root` → `episode.project_root`
→ `MemoryRecord.project_path` → 记忆的 scope 与检索维度）。

> 这与本仓那条**「降级要降到说不出来，不是降到另一个答案」**完全同形。
> 同族：`Offline` 而不是「Healthy 但没数」；分母为 0 返回 null 不返回 0；
> `ANALYTICS_NEEDS_DB` 而不是回退到第二条枚举路径。

`find_upward` 那一半是同一个形状的另一面：「哪个目录是项目根」用「**最近一个有构建
marker 的目录**」来回答 —— 而 marker 只说明「这里有个构建单元」，不说明「这里是项目
根」。**拿一个可观测的代理量冒充要测的量。**

## 🔴 根因二：同一个知识分散在三处，互相看不见

「哪些路径是项目根」这个知识现在有三个持有者：

| 谁 | 怎么发现 | 服务谁 |
| --- | --- | --- |
| `project_root.rs::find_upward` | 摄取时现算，本地 stat marker | `RawEvent.project_root` |
| `identity.rs::find_git_root`（ADR-032 P1） | 本地 stat + 读 `.git/config` | `canonical_repo_id` |
| QuotaBar `sources.rs::compute_project_aliases` | 目录扫描 `~/.claude/projects` | 记忆窗口的别名表 |

三者回答的问题略有不同，**结果互不共享**。后果是双向的：

- 摄取时算不出的（WSL 那 71.4%），**别处其实知道** —— QuotaBar 的目录扫描每天都在
  列举这些项目根；
- 别处新发现的项目根，**摄取侧永远不知道** —— 因为摄取只在自己那一次 stat 里找答案。

这是本仓反复批评的「同一规则多份实现」，只是这次它们没有明显冲突，**而是互相看不见**
—— 那更难发现，因为没有任何一处会报错。

## 决定

### 决定 1：拆成「发现」与「归属」两层，它们的性质相反

```
发现 Discovery ── 产出「已知项目根」集合
   慢 · 需要 I/O · 允许失败 · 可增量 · 多来源
        ↓ 注册表
归属 Attribution ── 给定 cwd → 项目根
   快 · 零 I/O · 纯函数 · 必然给出结果（含「说不出来」这一种）
```

**这条是本 ADR 的全部要点。** 之所以现在会错，是因为两件性质相反的事被压在了一次
函数调用里：一个需要 I/O 且可能失败的操作，和一个必须对每条事件都给出答案的映射。
压在一起时，前者的失败就只能由后者去编一个答案。

拆开之后：

- **发现失败** ⇒ 少一个已知根，其余照常归属；
- **归属** ⇒ 纯字符串前缀匹配，WSL 路径不需要 stat 也能归位（这正是现在做不到的 71.4%）；
- **新增一个项目根** ⇒ 下次投影时历史事件**自动归位**，不需要写任何迁移。

### 决定 2：归属规则是「最长匹配的已知项目根」，且**没匹配上要说出来**

```rust
pub enum Attribution {
    /// 归属到一个已知项目根。`source` 记下这个根是怎么发现的。
    Root { path: String, source: RootSource },
    /// 🔴 没有任何已知根覆盖它 —— **说出来，不拿它冒充项目根**。
    Unattributed { path: String },
    /// 连路径都没有（如 TumeChat 的一次纯闲聊）—— 见决定 3.6。
    NoPath,
}
```

> ⚠️ 实现时比原设计多了 `NoPath`：原文把「没有路径」和「有路径但归不到根」压在
> 同一个变体里，而两者的下游动作不同（前者连粗粒度查询都做不了）。

⚠️ **`Unattributed` 仍然携带那个路径**（下游要能按它做粗粒度查询），但它是**另一个变体**：
下游想当 project_root 用就必须 `match`，于是「这是兜底」这件事没法被静默忽略。

> 与账号身份那条同源：`IdentityResolution::Pending` **不携带值**，于是
> `.unwrap_or(account_id)` 式降级**编译不过**。这里不能完全不带值（查询维度要它），
> 但至少让它无法被当成正常答案。

判据：最长前缀。一个 root 是另一个的真后代时，**取更长的那个** —— `QuotaBar/third_party/TumeFlow`
是独立仓（有自己的 `.git`），它比 `QuotaBar` 更准确。

### 决定 3：注册表存在总库里，但是**独立的一张表**

> 🔴 **本条 2026-08-12 实现时订正过。** 原文写的是「与 ADR-032 P2 的
> `project_identity` **合并**」——动手时发现两者的约束**互相冲突**，合表就得在
> 关键的一条上二选一。原文保留在下方 details 里，因为「为什么当初以为能合」
> 本身是这条决定的一半理由。

`project_root_registry`：

```sql
CREATE TABLE IF NOT EXISTS project_root_registry (
    root_key      TEXT PRIMARY KEY,  -- 归一化比较键（attribution::registry_key）
    root_path     TEXT NOT NULL,     -- 原始形式；归属结果返回它
    root_source   TEXT NOT NULL,     -- git / marker / scan / configured
    first_seen_ms INTEGER NOT NULL,
    last_seen_ms  INTEGER NOT NULL
);
```

**为什么不能与 `project_identity` 合表** —— 两者回答不同的问题，而约束冲突：

| 表 | 回答 | 关键约束 |
| --- | --- | --- |
| `project_identity` | 这个项目**在别的系统里叫什么** | 🔴 **不写 `path:` 兜底行** —— 那种 id 不跨 checkout 稳定，记下来会让「查得到身份」变成一句不能信的话 |
| `project_root_registry` | 这个路径**是不是**一个项目根 | 🔴 **必须收没有 remote 的根** —— 「是个根」不要求跨 checkout 稳定 |

原文以为「那条约束针对身份、不针对归属，所以合表后放宽即可」。**但一张表只能有
一套写入规则**：放宽之后 `project_identity` 的查询就会读到它当初刻意排除的那些行，
而那正是 P2 明确不要的。测试 `a_root_without_a_git_remote_is_still_a_root` 钉住了
这个区分。

⚠️ **注册表不带 `source_type` / `source_location`**：一个路径是不是项目根，与
「谁在什么位置扫到它」无关。带上它们会让同一个根按发现者分裂成多行，而归属时又得
决定听谁的 —— 那是凭空造出来的分歧，与决定 3.5「结果与执行者无关」直接冲突。

<details><summary>原文（保留见证：为什么当初以为能合）</summary>

> P2 那张表已经在存 `(source_type, source_location, project_root, canonical_id)`。
> 本 ADR 扩展它：现在只记「算得出 `canonical_id` 的」（有 origin remote）—— 改为
> 也记没有 remote 的项目根（`canonical_id` 为 `path:<root>`）；加一列 `root_source`。
>
> ⚠️ P2 当初「不写 `path:` 兜底行」的理由是「那种 id 不跨 checkout 稳定」—— 那条
> 理由针对的是身份（identity），不是归属（attribution）。归属只需要知道「这是一个
> 项目根」，不需要它跨 checkout 稳定。

**漏掉的一步**：约束是写在**表**上的，不是写在**读法**上的。即使「归属不需要稳定
身份」成立，放宽写入规则也会同时改变身份查询看到的东西。

</details>

### 🔴 决定 3.5：注册表是**共享状态**，不是各进程各自的缓存

这条是 2026-08-12 复核架构归属时补的，它堵的是**决定 4 的一个副作用**。

MASTER_PLAN「摄取归谁」已经拍板：**QuotaBar 与 TumeFlow 各自摄取，谁也不依赖谁在跑**
（前者为配额/成本分析扫，后者为蒸馏读，都是各自本职）。而两者拿 SessionVault 的方式
**不同**：

| 进程 | 怎么用 SessionVault | 发现能力 |
| --- | --- | --- |
| QuotaBar | Rust crate 硬依赖（编译进去） | 本地 stat + **WSL 访问桥** + 目录扫描 |
| TumeFlow | spawn `svault` 二进制 | 只有 `svault` 自己能做的那些 |
| TumeChat | **完全不碰**（只经 TumeFlow daemon） | — |

⚠️ **于是同一个 cwd，在两个进程里可能归属到不同的项目根** —— 因为它们能发现的根不一样。
那会让 `project_root` 变成一个**取决于谁跑的**字段，而它本该是事件的客观属性。

⇒ **注册表必须存在总库里、被两个进程共享**（决定 3 已经这么定了，这里把理由补上）。
于是能力不对称从「各算各的」变成「**谁先发现谁写进去，之后大家用同一份**」：

- QuotaBar 有 WSL 访问桥 ⇒ 它发现 WSL 里的项目根并写进注册表；
- TumeFlow 独立跑时读到同一张表 ⇒ 归属结果与 QuotaBar 一致；
- 谁都没发现过的 ⇒ 两边都是 `Unattributed`，**一致地说不出来**。

**不变式**：*同一个 cwd 在任何进程、任何时刻，归属结果只取决于注册表的内容，
不取决于谁在跑。* 这是归属层必须是**纯函数**的真正理由 —— 不只是为了可测，
是为了让结果与执行者无关。

🔴 **推论：发现必须写盘，不能只在内存里。** 一个只在本进程内存里生效的「已发现根」
会让另一个进程看不见它，那正是本 ADR 根因二（同一知识分散多处、互相看不见）的重演，
只是这次分散在**进程**之间而不是模块之间。

### 🔴 决定 3.6：归属的输入是**一个路径**，而「路径从哪来」是来源相关的

这条是 2026-08-12 讨论 TumeChat 定位时补的 —— 它堵的是一个**将来才会炸**的缺口。

现在归属的输入是 `cwd`，而 `cwd` 隐含一个前提：**这个来源的会话发生在某个目录里**。
Claude Code / Codex 满足它（人在项目里敲命令）。**TumeChat 不满足** ——
用户在聊天框里提问时，「人在哪个目录」这个概念根本不存在。

而 TumeChat 的会话**是要被摄取的**（TumeChat `ADR-001 §10`「记忆是双向的」：
它的聊天记录应当和 Claude Code / Codex 的会话一样被 SessionVault 摄取、被 TumeFlow
蒸馏，再经 `recall.context_pack` 注入回来）。落地缺口是 `SourceType::TumeChat`
（QuotaBar 待办 #7）。

⇒ 所以本 ADR 的模型要分清两件事：

```
归属（本 ADR 管）      路径 → 项目根        对所有来源相同
路径从哪来（来源管）    Claude Code: cwd
                      Codex:       cwd
                      TumeChat:    用户显式选的上下文 / 被引用的文件 / 无
```

**归属函数的签名因此是 `attribute(path, registry)`，不是 `attribute(cwd, registry)`。**
一个字之差，但它决定了新增来源时要不要改这一层 —— 不该改。

⚠️ **`None` 是合法输入**：TumeChat 的一次纯闲聊没有任何项目路径可言，那时归属结果是
`Unattributed`，**而不是硬塞一个**。与本 ADR 的主线一致：说不出来就说不出来。

> 这条也解释了为什么归属层必须只吃「一个路径」而不吃「一个会话」：会话的形状随来源
> 变化，而路径不会。把来源特有的知识挡在归属层之外，新增 `SourceType` 才不会每次都
> 回来改这里。

### 决定 4：发现的来源是多个，且**每一个都可以缺席**

| 来源 | 能发现什么 | 缺席时 |
| --- | --- | --- |
| 本地 `.git` / 构建 marker 上溯 | Windows 本机路径 | 那些路径退回 `Unattributed` |
| **WSL 访问桥 stat**（`wsl::stat` 已存在） | WSL 内的项目根 —— **现在完全空白的那 71.4%** | 同上 |
| QuotaBar 目录扫描（`enumerate_source_roots`） | 已知会话目录对应的项目根 | 同上 |
| 用户显式配置 | 前三者都发现不了的 | 同上 |

🔴 **一个来源坏了，只该少发现一些根，不该让整套归属失效。** 这与「TumeFlow 缺席只
降级记忆窗口、不影响 QuotaBar 核心」是同一条不变式。

### 决定 5：`.git` 优先于构建 marker（原 P3），但只在**发现**这一层

> 🔴 **实现时把「优先」的确切含义定死了（2026-08-12）**：是「**整条链找最近的
> `.git`；一路没有才回退最近的构建 marker**」，**不是**原 P3 设想的「一路走到
> 文件系统根找 `.git`」。
>
> 差别是承重的：前者路径上任何更近的 `.git` 都会先命中，所以 `EyeVLM/docs` 会停在
> `EyeVLM`，永远轮不到 `~`；后者才有「命中 dotfiles 仓」的风险。
>
> 唯一还能走到 home 的情况是「这条链上一个 `.git` 都没有」，所以再加一条：
> **home 目录本身永远不算项目根**（但 home **之下**的真项目照常认 —— 排除的是
> home 本身，不是整棵子树）。
>
> ⚠️ **这条规则一度写在代码里但测不到**：其余测试都用 `temp_dir` 下的路径，home
> 根本不在那条链上，于是「删掉 home 排除」这个变异 8 条测试全绿。修法不是多写一条
> 测试，是把 home 变成**显式参数**（`probe_local_with_home`）—— 本机 home 是不是
> git 仓完全看用户，测试不该依赖那个。与 `currency.anchor_fn` / `HostPlatform`
> 的注入同源：**平台事实作参数，逻辑才可单测**。

原 P3（#34）是改 `find_upward` 让 `.git` 优先。本 ADR 把它并进来，**但位置变了**：
它是**发现**规则的一部分（「什么算一个项目根」），不再是摄取时的现算逻辑。

⚠️ 原 P3 独立做时收益只有 **0.88%**（全库只有一个 `marker:Cargo.toml` 的 root），
不值得为它单独提版。合进本 ADR 就没有额外成本 —— 反正要提版。

🔴 **原 P3 的休眠风险在这里被解决掉了**：「一路走到根找 `.git` 会命中 `~/.git`
这类 dotfiles 仓」。在发现层，一个候选根要**被注册**才算数，而注册可以带判据
（比如要求它在已知的工作区根之下，或有 origin remote）；而在原来的摄取时现算里，
`find_upward` 走到哪儿算哪儿，没有任何东西能挡住 home 目录。

### 决定 6：`PARSER_REVISION` 3 → 4，且**必须与 change-feed 消费方同批**

改归属规则 ⇒ 重投影 ⇒ ADR-044 决定 6 的第③触发条件。
✅ **消费方已于 2026-08-12 接通**（`eval/currency.py` 的两段判定），所以这次提版
不会把全部会话记忆一次性标红 —— 那正是接它的意义。

⚠️ 提版前必须复核：`scripts/probe-currency-changefeed.py` 仍然通过。

### 🔴 决定 7：同一个目录的多种路径形式，收敛在**注册表的比较键**上

本机上 `C:\Users\u\P`、`/mnt/c/Users/u/P` 是**同一个目录**（`/mnt/c → C:\`，由
`wsl::drive_mounts` 读 `mount` 实测）。它们必须归到同一个根。

收敛点是 `RootRegistry`：它持有挂载表，`insert` 与 `attribute` 都经
`registry.key()` 算键。**发现侧不做任何形式转换** —— 探到什么形式就登记什么形式。

为什么不放发现侧（那是第一版的写法，跑通了、干跑也绿）：

- 它让 `/mnt` 那一族**自成一个根**（实测 `C:\…\QuotaBar` 153,413 条与
  `/mnt/c/…/QuotaBar` 2,912 条并列），而后者在 Windows 上 stat 不到 `.git/config`，
  `canonical_repo_id` 只能落 `path:` id ⇒ **身份层也分家**。
- 更根本的：「两个路径是不是同一个根」是**归属层的判据**。放在发现侧等于给每一种
  未来的路径形式各打一次补丁。

> **一条规则该放哪一层，看它约束的是谁。**

⚠️ 挂载表**住在注册表里**，不是当参数传给 `attribute()`：写入键与查询键必须由
同一份表算出，否则已登记的根会查不到 —— 而那**不报错**，只表现成「归属突然失效」。

⚠️ 与本 ADR 的「登记什么形式，就只认什么形式」**不矛盾**：那条约束的是
**WSL 里的路径**（`/home/u/P` 与 `wsl:D:/home/u/P` 指向同一份文件，但 Windows 侧
根本 stat 不到它们，没有共同的宿主形式可归）。`/mnt/…` 有，所以它归得了。
判据是**「本机能不能把两者解析成同一个目录」**，不是「看起来像不像」。

## 不做什么

- **不改 `project_path` 为列表**（原 #20 的 (B) 方向）。支持它的证据是「51% 跨根」，
  而重测证明真正跨仓库的会话是 **0 个** —— 那 51% 按会话算只有 8.5%，且几乎全是
  「同一个仓被记成多个」。本 ADR 修的正是后者。多归属存储现在没有数据要求它。
- **不引入 LLM 推断 `related_projects`**（原 (A)）。同上：它要救的场景（在 A 仓会话里
  改 B 仓）实测只有 TumeChat 一个实例，而那有零成本的正解（在 TumeChat 目录下开会话）。
- **不在消费侧归一**。省掉重投影，但每个消费点都要记得归一 ⇒ 会漏。本仓已有判例：
  `strip_deployment_verdict` **在产出侧剥，不在消费侧过滤** —— 后者要在每个未来的
  读点重写一遍。

## 已知不解决

🔴 **TumeChat 的工作记不进 TumeChat。** 实测（本会话 7,509 行）：提到 TumeChat 路径
的行 336，而 **cwd 记成 TumeChat 的行 0**；同一会话里 `third_party/TumeFlow`
（QuotaBar 的**子目录**）被记了 673 行 —— 两者都是 `cd … && cmd` 进去的。

⇒ **Claude Code 只记录落在项目根子树内的 cwd。** TumeChat 是 QuotaBar 的**同级**目录，
所以在 QuotaBar 会话里改它，cwd 永远不会变成 TumeChat。

本 ADR 修的是「cwd → 项目根」的映射，而这里 **cwd 本身就没记对** —— 不在本 ADR 的
射程内。唯一有效的办法是**在 TumeChat 目录下单独开会话**（那时它有自己的
`~/.claude/projects/` 目录）。原 #20 提的「主题切到哪个仓就 cd 过去」对同级仓库
**结构性无效**，已实测否定。

## 落地步骤

**前两步不改变任何现有行为** —— `parser.rs` 仍走老的 `resolve_project_root`，
所以可以随时停在任何一步而不留下半拉状态。

| 步 | 内容 | 状态 |
| --- | --- | --- |
| 1 | `attribution.rs`（归属纯函数 + `RootRegistry`）+ `project_root_registry` 表与读写 API | ✅ 2026-08-12 |
| 2 | 发现的多个来源接上（本地 `.git`/marker、**WSL 访问桥**、`/mnt/<drive>` 换算），写进注册表 | ✅ 2026-08-12 |
| 3 | `PARSER_REVISION` 3→4，`parser.rs` 改用 `attribute()`；全库重投影 | 🟡 代码已实现 + 副本实测；**真库未执行，等人确认** |

### 步 2 实现记录（2026-08-12）

- `src/discovery.rs`：`Probe` 三态 / `DiscoveryReport` / `probe_local` / `probe_wsl` /
  `probe_path`（按路径形式分派）/ `discover`。8 条测试。
- `src/wsl.rs::find_project_root`：**一次 `wsl.exe` 调用走完整条祖先链**。
  逐级从 Windows 侧 stat 要 N 次跨 VM 往返（每次约 0.1–0.3s），而全库有 68 个 WSL
  形式的 root —— 把 `while` 写进脚本，一次调用解决一条链。
- **6 个变异精确变红**（删掉第一遍 `.git` 扫 / 不排除 home / 排除整棵 home 子树 /
  探测失败当成「没有根」/ report 不去重 / …）。

🔴 **`Probe::None` 与 `Probe::Failed` 必须分开**：前者是「问过了，没有」，后者是
「没问成」。混成一个会让「WSL 关机」看起来像「这些路径都不属于任何项目」——
而归属会据此把它们静默记成 `Unattributed`，掩盖掉一个可修的配置问题。

⚠️ **两处自陈（都是变异验证抓出来的）**：

1. 头两个变异**锚点命中却全绿** —— 那是**真的假护栏**，不是脚本没匹配上。
   变异 A 改的是第二遍扫的 filter，而第一遍先跑并命中 `.git` ⇒ 那行代码在测试里
   走不到；变异 B 改的 home 排除，而测试路径根本不经过 home。
   **判据：变异「应用了」只是第一关，还要问它改的那行在被测路径上跑不跑得到。**
2. 修法不是补测试，是**改代码让规则可测**（home 显式注入）。补一条依赖「本机 home
   恰好是 git 仓」的测试，等于把护栏建在一个环境事实上。

### 🔴 干跑实测（2026-08-12，`examples/attribution_dryrun.rs`）

**发现 + 归属跑一遍真实数据，一个字节都不写。** 步 3 不可逆，所以在那之前必须先
回答「新规则会把全库变成什么样」。

```
总库现状：111 个 project_root，992,460 条事件
发现：72 条探到根 · 39 条确认无根 · 0 条探测失败 —— 12.2s
归属：111 → 18 个项目根（收敛 84%）
      Unattributed：43 条路径 / 约 12% 事件
```

判据（ADR「验收」）：✅ 收敛到 ≤43（实际 18）· ✅ `Unattributed` 数量可报 ·
✅ 失败与「无根」分开计数（failed=0 / no_root=39）。

⚠️ **不写盘是判据的一部分**，不只是谨慎：一次会改状态的「干跑」跑第二遍时输入已经
变了，量到的数字和第一遍不同 —— 而那正是要拿来做决策的数。

#### 干跑当场抓出一个真 bug：结果的形式必须跟随输入

第一版把 WSL 探测结果**无条件**转成规范形（`wsl:<distro>:/home/u/P`），于是注册表
里只有规范形的根。而归属是**纯字符串匹配** —— 裸 Linux 形式的 `project_root`
匹配不上任何根：

```
修复前   /home/simon/workspace/EyeVLM              106,814 条 → Unattributed
         wsl:Ubuntu-22.04:/home/simon/…/EyeVLM     344,217 条 → 归到了
```

**同一个项目的两种形式，一种归到一种没有。** 修法：`probe_wsl` 的结果形式跟随
输入形式（规范形/UNC 输入拿规范形，裸 Linux 输入拿裸形式）。

> **登记什么形式，就只认什么形式** —— 这是归属层「零 I/O、纯字符串」的直接推论，
> 而它在单元测试里看不出来（测试只用一种形式）。**只有对着真实数据的多种形式跑
> 一遍才暴露。**

#### 已知剩余：`/mnt/…` 那一族（43 条 Unattributed 的全部）

```
26,572  /mnt/d/mwf/code/corneal-staining-grading
22,256  /mnt/d/mwf/code/fbut-video-classifier
 …
```

`/mnt/<drive>/…` 不是裸 Linux 路径（`is_windows_drive_mount` 为真），所以走
`probe_local`；而在 Windows 上 `Path::new("/mnt/d/…")` 被当成当前盘根的相对路径
⇒ 不存在 ⇒ 报 `Probe::None`。

**解法**：把 `/mnt/<drive>/…` 映射到 `<drive>:\…` 再本机探测。⚠️ **不能直接猜**
—— 挂载点与盘符的对应关系是**运行期事实**，不是常量。

> 🔴 **判据订正（2026-08-12 实测）：读 `mount`，不读 `wsl.conf`。**
>
> 本条原写「经访问桥读一次 `wsl.conf` 确认 `automount.root` 没被改过」。实测本机
> `/etc/wsl.conf` **根本没有 `[automount]` 段**（只有 `[boot] systemd=true`）——
> 那时得知道默认值才能推，而「配置没写 = 默认值」这个推断还有两个漏洞：
> 配置改了但没 `wsl --shutdown` 重启（意图 ≠ 生效），以及**并非所有 `/mnt/x` 都是
> Windows 盘**（`/mnt/data` 可能是普通 Linux 挂载）。
>
> `mount` 直接给出运行期真相：
>
> ```
> C:\ on /mnt/c type 9p (rw,noatime,aname=drvfs;path=C:\;uid=1000;…)
> D:\ on /mnt/d type 9p (…;path=D:\;…)
> ```
>
> 它比原设想**更强**：即使用户把 `automount.root` 改成 `/win/`、甚至把 D 盘挂到
> `/data`，这张表照样对；而 `/mnt/data` 这类非 drvfs 挂载不会被误当成盘。
>
> **一般化的教训**：要问「实际挂在哪」时，读**运行期状态**（`mount`）而不是
> **配置意图**（`wsl.conf`）。两者在「改了没重启」「配置缺省」这两种情况下会分岔，
> 而分岔时配置那一侧是错的。同族：本仓那条「装机验证要锚在启动标记上，
> 不能用 exe 时间戳」。

读不到 `mount` ⇒ **不映射**，那些路径照旧 `Unattributed`（诚实地说不出来）。

#### 落地：收敛在**注册表的比较键**，不在发现侧（决定 7）

实现时先写成「发现侧把探到的 Windows 根**转回** `/mnt/…` 形式」——理由是本 ADR
自己那条「登记什么形式，就只认什么形式」。它跑通了，干跑也绿，**但错了一层**：

```
修复前   C:\Users\user\workspace\QuotaBar       153,413 条  ← git:github.com/…
         /mnt/c/Users/user/workspace/QuotaBar     2,912 条  ← path:/mnt/c/…
```

`/mnt` 那一族**自成一个根**。而它在 Windows 上 stat 不到 `.git/config`，
`identity::canonical_repo_id` 只能落 `path:` id ⇒ **身份层跟着分家**，
同一个目录变成两个项目 —— 正是 ADR-032 P1/P2 要消灭的东西，被我从另一头造了回来。

根因不在那个分支，在**注册表按比较键判覆盖，而同一个目录在本机有多种形式**。
所以形式收敛属于**比较规则**：`RootRegistry::with_mounts(mounts)` 持有挂载表，
`insert` 与 `attribute` 都走 `registry.key()`。发现侧于是变简单了 ——
`probe_mnt_with` 探到什么返回什么，round-trip 整段删掉。

> **判据：一条规则要放在哪一层，看它约束的是谁。**「`/mnt/c/X` 与 `C:\X` 是同一个
> 目录」约束的是**两个路径能不能算同一个根**，那是归属层的判据，不是某条探测路径的
> 后处理。在发现侧修等于给每个未来的路径形式各打一次补丁。

挂载表**住在注册表里而不是当参数传给 `attribute`**：写入键与查询键必须由同一份表
算出，否则一个已登记的根会查不到 —— 而那**不报错**，只表现成「归属突然失效」。

⚠️ **存储层的 `root_key` 不做这个收敛**（`register_project_root` 传空表）：它只用于
本表去重，归属是拿 `root_path` 在读出时重算键的。这句话是**对周边结构的断言**，
而结构一变它不会报错 —— 所以不靠它，钉后果：
`a_mnt_root_still_attributes_after_a_round_trip` 让一条 `/mnt` 形式的根落库、读回来，
断言它仍与宿主形式收敛到同一个根。

#### 步 2 完成后的干跑

```
总库现状：111 个 project_root，993,038 条事件
挂载表：/mnt/c→C:\  /mnt/d→D:\  /mnt/e→E:\  /mnt/f→F:\
发现：75 条探到根 · 36 条确认无根 · 0 条探测失败 —— 12.5s
归属：111 → 19 个项目根（收敛 83%）
      Unattributed：40 条路径 / 120,434 条事件（12.1%）
      事件守恒：872,604 + 120,434 = 993,038 ✅
```

**剩下的 40 条不是缺陷**：其中 8 条指向的目录在磁盘上**已经不存在**了（那棵子树
整个搬过家），还有一条存在但既无 `.git` 也无构建 marker。两种都是**真的说不出来**，
`Probe::None` 是正确答案 —— 而不是找个最近的祖先凑一个。

🔴 **守恒断言是这一轮补的，因为它抓住了我一个错误推论。** 步 2 前后各根的数字有
小幅出入，我据此「解释」成更具体的根抢走了子路径 —— 而真实原因主要是**总库是活的**
（两轮之间 992,802 → 993,038，本会话自己的事件在写入）。跨轮比各根数字本就不成立；
守恒是**轮内**不变量，只有它能把「挪」和「丢」分开。

### 步 3 实现记录（2026-08-12，代码已实现，真库未执行）

**改了什么**

| 位置 | 改动 |
| --- | --- |
| `parser.rs::resolve_cached` | `resolve_project_root`（会 I/O、失败就拿 cwd 顶上）→ `attribute()`（纯函数、对着注册表最长匹配） |
| `parser.rs::ParseCtx` | 新增 `roots: Arc<RootRegistry>`，**无 `Option`、无回退分支** —— 空注册表就是「一个根都不知道」 |
| `parser.rs::project_root_of` | `Attribution` → `ProjectRoot`：`Unattributed` 的 `source` 记成 `unattributed`，`path` 走 `storage_path()` |
| `PARSER_REVISION` | 3 → 4（决定 6） |
| `lib.rs` | `host_drive_mounts()` + `project_root_registry(store, mounts)` —— **两个 client 的唯一入口** |
| `store.rs` | `distinct_project_roots()`（发现的候选清单） |
| QuotaBar `session_index.rs` | `discover_project_roots()`：扫描**之前**先发现，只探还归不上的候选 |

🔴 **`project_root_source` 上必须留下痕迹。** `Unattributed` 的 `path` 仍是那条路径（粗粒度
查询要它），但 `source` 记 `unattributed` —— 否则「归到了一个根」与「没归到、拿原路径顶上」
在库里长得**一模一样**，而那正是本 ADR 要消灭的东西（旧的 `cwd` / `wsl_cwd` 就是这样）。

🔴 **发现只探还归不上的候选。** 第一轮要探全库历史取值（111 条 / 12.5s，WSL 那些每条一次
`wsl.exe` 往返）；之后绝大多数已被某个根覆盖 ⇒ 候选集塌到近乎空 ⇒ 代价回到零。
没有这条过滤，**每次刷新都要付一次 12 秒**。

**变异验证**（三条各自精确变红）：`attribute` 换成恒 `Unattributed` 的桩 /
归不到根却谎称 `cwd` / `Unattributed` 丢掉路径。第一条是关键 —— 没有它，
「空注册表 ⇒ 说不出来」那两条测试在整个步 3 被架空的情况下照样全绿。

#### 🔴 真库副本实测：干跑的 83% 是乐观数字

`examples/reprojection_timing.rs`（在 QuotaBar 仓）在两个库的一致快照上跑完整步 3，
真库全程只读：

```
重投影   227.5s · 646 个文件 · 总库 3146 → 3349 MB（+202 MB，上界）
当代（查询实际看到的那一代）
  project_root  111 → 94 个（−15%）
  改了归属       7,605 条事件
  归到已知根 87.6% · 说不出来 12.4%
```

迁移正是本 ADR 承诺的三类：

```
8692  …\QuotaBar\src-tauri          →  …\QuotaBar     ← 原 P3 的收益
2524  /mnt/c/…/QuotaBar             →  C:\…\QuotaBar  ← 决定 7 的 /mnt 收敛
 952  wsl:…/<proj>/experimental_…  →  wsl:…/<proj>   ┐
 532  wsl:…/<proj>/docs             →  wsl:…/<proj>   ├ 「同一个项目被记成 11 个」
共 20 个旧取值 → 4 个新取值                            ┘
```

**为什么不是干跑说的 111 → 19**：干跑对每个 `project_root` **取值**算归属，而实际重投影
只能覆盖**源文件还在磁盘上**的那些 —— 总库 881 个会话文件现存 **646**，另外 235 个的事件
占 **55%**，没有字节就无法重解析，`project_root` 永远停在旧值。而且被重投影的 40.7 万条里
**98% 本来就归对了**（人多数时间就在仓库根目录工作）。

> **一般化：一个「如果全部重算会怎样」的干跑，回答不了「实际能重算多少」。**
> 两者的差是**不可重算的存量**，而它在干跑里完全不可见。凡是要按干跑结果做不可逆决定的，
> 都要先问一句「这些输入里有多少是这次动不了的」。

⚠️ 历史那 55% 不必靠重投影补：`attribute` 是纯函数，**查询时**重新归属算得起。
那是另一件事，不在步 3 里。

⚠️ **两次量错才拿到上面的数**（都记在探针注释里）：① 第一版数了**全部代**，
其中含从未重投影的旧地层，于是把「55% 不可重投影」读成「归属没生效」；
② 逐行比对的 join 漏了 UNIQUE 索引第 4 列 `source_session_id`，索引只用得上前 3 列前缀、
`seq` 退化成扫描，还拆成四条查询各跑一遍 —— 十分钟没跑完。现改为**一份对照副本 +
单条全前缀查询**（42.9s）。

### 步 1 实现记录（2026-08-12）

- `src/attribution.rs`：`RootSource` / `Attribution` / `RootRegistry` / `attribute()` /
  `registry_key()`。11 条测试。
- `src/store.rs`：`project_root_registry` 表 + `register_project_root()` /
  `project_root_registry()` / `project_root_count()`。6 条测试。
- **7 个变异精确变红**：取最短匹配 · 字符串前缀而非路径段 · `root()` 也返回
  `Unattributed` 的路径 · 空注册表退回老答案 · 存储侧自己写一遍归一化 ·
  未知来源标签丢掉那个根 · 注册表存归一化路径丢掉原始形式。

⚠️ **一处自陈**：头三个变异第一轮全绿，是因为 `perl` 的多行替换没匹配上 ——
**变异根本没应用**。改成「替换前先断言锚点存在」之后，三个全部变红。
同一天在 `project_root_scope.rs` 的探针上刚栽过一次同样的坑（手写 JSON 解析器
一条都没匹配上，探针照常打表头、算出 `NaN%`、退出码 0）。
**判据：变异验证的第一步是证明变异真的发生了。**

## 验收

1. 全库 111 个 `project_root` 重投影后收敛到 **≤43 个**（前缀归一的实测下界），
   且 `wsl_cwd` 那 68 个不再原样落地；
2. 跨根会话从 31 个降到接近 0（那 30 个「跨子目录」应当归到同一个根）；
3. 🔴 **`Unattributed` 的数量要报出来** —— 它是「发现覆盖不到哪里」的度量，
   而不是一个该被藏起来的失败；
4. 重投影后 `scripts/probe-currency-changefeed.py` 仍通过，会话记忆**没有**被一次性标红；
5. 真库副本上试跑并计时（ADR-044 那次实测「10 分钟跑不完」，四层根因）；
6. `examples/project_root_scope.rs` 前后各跑一次，比对 source 分布。
