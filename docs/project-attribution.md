# ADR-050：项目归属分成「发现」与「归属」两层 —— I/O 能力不该决定语义

> 状态：**设计**（2026-08-12）
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
    /// 归属到一个已知项目根。`registry_source` 记下这个根是怎么发现的。
    Root { path: String, registry_source: RootSource },
    /// 🔴 没有任何已知根覆盖它 —— **说出来，不拿 cwd 冒充**。
    Unattributed { cwd: String },
}
```

⚠️ **`Unattributed` 仍然携带 cwd**（下游要能按它做粗粒度查询），但它是**另一个变体**：
下游想当 project_root 用就必须 `match`，于是「这是兜底」这件事没法被静默忽略。

> 与账号身份那条同源：`IdentityResolution::Pending` **不携带值**，于是
> `.unwrap_or(account_id)` 式降级**编译不过**。这里不能完全不带值（查询维度要它），
> 但至少让它无法被当成正常答案。

判据：最长前缀。一个 root 是另一个的真后代时，**取更长的那个** —— `QuotaBar/third_party/TumeFlow`
是独立仓（有自己的 `.git`），它比 `QuotaBar` 更准确。

### 决定 3：注册表存在总库里，与 ADR-032 P2 的 `project_identity` 合并

P2 那张表已经在存 `(source_type, source_location, project_root, canonical_id)`。
本 ADR 扩展它：

- 现在只记「**算得出 `canonical_id`** 的」（有 origin remote）—— 改为**也记没有 remote
  的项目根**（`canonical_id` 为 `path:<root>`，P2 原本刻意不写的那种）；
- 加一列 `root_source`：`git` / `marker:<file>` / `scan` / `configured`，
  让「这个根是怎么来的」可查。

⚠️ P2 当初「不写 `path:` 兜底行」的理由是「那种 id 不跨 checkout 稳定，记下来只会让
『查得到身份』变成一句不能信的话」—— **那条理由针对的是身份（identity），不是归属
（attribution）**。归属只需要知道「这是一个项目根」，不需要它跨 checkout 稳定。
两者现在分开了，所以那条约束不再适用于本表的归属用途。

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

## 验收

1. 全库 111 个 `project_root` 重投影后收敛到 **≤43 个**（前缀归一的实测下界），
   且 `wsl_cwd` 那 68 个不再原样落地；
2. 跨根会话从 31 个降到接近 0（那 30 个「跨子目录」应当归到同一个根）；
3. 🔴 **`Unattributed` 的数量要报出来** —— 它是「发现覆盖不到哪里」的度量，
   而不是一个该被藏起来的失败；
4. 重投影后 `scripts/probe-currency-changefeed.py` 仍通过，会话记忆**没有**被一次性标红；
5. 真库副本上试跑并计时（ADR-044 那次实测「10 分钟跑不完」，四层根因）；
6. `examples/project_root_scope.rs` 前后各跑一次，比对 source 分布。
