# 扫描与投影的状态模型 —— 让同类错误无法表达

> 状态：**已落地**（`observation.rs` / `scan_plan.rs` / `deadline.rs` / `probe.rs` /
> `token.rs`；`scan_plan` 于 2026-08-21 从 QuotaBar 搬入本仓，13 条测试随文件迁移）
>
> 来源：本文是 QuotaBar `docs/ADR-051` 的**本仓这一半**，2026-08-27 拆出。
> 拆分线由那份文档自己的头部给出：
> **本仓（扫描事实 + 幂等 token + 规划器）· QuotaBar（同步状态 + 调用方义务）**。
> 判据内容一字未改，只是**执行者换了仓** —— 代码在这里，判据就该在这里。
>
> ⚠️ **不带 ADR 编号是有意的。** 本仓 `docs/` 用主题命名；而 QuotaBar 那个编号
> 与 TumeFlow 的 ADR-051 同号异义，沿用它等于把编号歧义扩散到第三个仓。

## 为什么需要这一份

四轮评审的诊断，照抄在这里，因为它是本文存在的全部理由：

> **根因分析经常写对了，落地时仍按具体反例补控制流和正则，
> 缺少统一类型和状态机来让同类错误无法表达。**

🔴 **设计的前两稿自己也在犯同一个错，各一次。**

第 1 稿用一个 `ProjectionAction::Replace` 压掉 `Rollback` 与 `Reparse` —— 而
`store.rs` 那个枚举的注释**恰恰在讲这件事**（「用 bool 表达时第二种只能伪装成
第一种，`rollback` 这个词会开始撒谎」）。

第 2 稿改用**互斥的** `ScanIntent`，而真实原因可以**同时成立**（回退 + 解析器过期 +
归属标脏 + 用户 force）—— 互斥枚举表达不了组合，于是「解析器过期 + 文件回退」会被
归成 `Reparse`，而磁盘上那段字节**已经没了**，本该走 `Rollback`。

**两稿都是「换了个更丰富的类型，但非法状态仍可表达」。** 判据不是「类型够不够多」，
是「**这个错误现在还能不能被构造出来**」。

## 本仓负责的四条不变式

| #                  | 不变式                                                                                                  | 它挡住的错误                                      |
| ------------------ | ------------------------------------------------------------------------------------------------------- | ------------------------------------------------- |
| **I1**             | **提交计划 = 扫描观察 × 原因集合 × 目标物化层**；原因是**集合**不是枚举，观察由扫描器给、原因由调度器给 | 组合原因被压成一个词；`Rollback` 被当成 `Reparse` |
| **I2**             | **「更近的位置不确定」时不得给出更远的答案**                                                            | 子项目被归到父仓                                  |
| **I4**             | **deadline 是调用链参数**，不是调用前看一眼的时钟                                                       | 在途调用越过整轮预算                              |
| **I7**（本仓这半） | **开新代的操作携带稳定 token 且幂等**                                                                   | 崩溃重复 `Rollback` 每次留一代不可回收            |

> 另外三条（**I3** 逐来源双确认 · **I5** 归属五态 · **I6** 的 `verify-agents-md`
> 那一半）由 QuotaBar 持有 —— 那些表和查询在它的 SQLite 里，不在本仓。

## 1. 扫描观察（限定 append-log）

### 适用范围

`scan_source` 还处理 `SnapshotFile` 与预留的 `SqliteStore` / `OpaqueFamily`，
而下面四态**围绕 JSONL 增量语义设计**，因此限定在 transcript 路径。

快照路径保持现状（它的失败态是「无效 UTF-8」「未实现的 source mode」，**不得**塞进
`Unavailable` —— 那会让「内容无效」伪装成「读不到」）。

### 🔴 第 1 稿的事实错误（保留记录）

第 1 稿说 `Partial` 混了三种含义。实测（`scan.rs`）是**两种**，第三种在 `Error` 里：
半行 pending（正常成功）与 one_shot 坏行（降级全读）都是 `Partial`；
**增量坏行是 `Error`**（整批丢弃、冻结起点）；真的没读成也是 `Error`。
⇒ `RejectedIncremental` 从 **`Error`** 拆，且 `Error` 自己也扛两件事。

### 目标形态

```rust
/// 扫描器**观察到了什么** —— 事实，不含「为什么扫」，也不含「该怎么办」。
pub struct AppendLogObservation {
    /// 🔴 **源变化是观察，不是意图。** 由扫描器 stat 后检出
    /// （`size < safe_offset || mtime 倒退`），调用方事先不知道。
    pub source_change: SourceChange,
    pub quality: ParseQuality,
    pub events: Vec<RawEvent>,
    pub cursor: Cursor,
    /// 全读时计算，用于识别**同尺寸原地重写**。
    pub source_fingerprint: Option<SourceFingerprint>,
}

pub enum SourceChange { Appended, RollbackOrRewrite, ReparseRequired }

pub enum ParseQuality {
    /// 完整行全部有效。半截尾行属于这里（`deferred_tail_bytes > 0` 是常态，不是降级）。
    Clean { deferred_tail_bytes: u64 },
    /// 有坏行，好行已保留（全读）。**降级，不是失败。**
    Degraded(ParseDiagnostics),
    /// 字节读到了，但**我们主动拒绝这一批**（增量遇坏行：保留事件又冻游标会让下轮
    /// 重复追加，所以整批丢弃 + 冻结起点）。这是设计决定，不是故障。
    RejectedPoisonLine(ParseDiagnostics),
    /// 没读成，手上没有可信事件。
    Unavailable(ScanFailure),
}
```

> 🔴 **`ForceVerify` 不能预先宣称「字节未变」。** 回退检测只看 `size` 与 `mtime`，
> **同尺寸原地重写检不出来**（`Cursor.content_hash` 只服务快照的指纹游标，
> append-log 不算）。所以强制全读**必须计算源指纹**并与上次比对；否则调用方的 UI
> 索引会 `ReplaceFile`，而总库走 `Append` 把新 seq 当重复丢弃 ——
> **两层从此不一致，且没有任何东西会说出来。**

⚠️ **`Skipped` 不在这里**：扫描器根本没被调用，它没有立场报告这件事。
那属于调度层（QuotaBar 侧）的「未尝试」。

### 控制面过滤与游标连续性（2026-11-10）

强制全读或欠账重试可以重置解析起点，但不能丢弃旧游标的已消费范围与 mtime 基线；原生前缀验证仍需同时使用旧 `safe_offset` 和可信旧指纹的覆盖长度。增量追加可能已消费到旧全读指纹之外，当前大小短于该已消费范围或 `SourceFingerprint::covered_len()` 均须识别为 `RollbackOrRewrite`，包括文件被清空的早退路径。append-log 新指纹只覆盖原始字节中的完整行，不覆盖尚未消费的半行尾巴；删除半行不应触发回退。指纹和游标共用原始字节边界，在有损 UTF-8 解码前确定，避免坏字节扩展后误算偏移。旧版已存指纹若包含半行，首次遇到该范围被截短时仍可能保守保留旧代，成功全读后更新基线。只靠游标偏移会把重试误报成追加，进而保留已不在源中的旧尾部事件。

过滤前的原生事件序号与过滤后保留条数是两个维度。混合临时与持久会话的文件逐事件判断准入，不能用首条事件代表整份文件；`next_seq` 按过滤前的解析事件数推进，保留被排除事件留下的序号空档。整批临时排除时，保留本批起点、序号与旧解析状态；不能回到文件头却沿用增量序号，否则会重发之前已保存的事件。

Codex 的 `session_meta` 可以单独占一批且不产事件。已知父关系保存于 `CodexState.session_parents`，随游标序列化，下一批按会话 ID 恢复；同一批或跨批切走、切回同一会话时仍保留其已知父关系，不能只存最后一个会话的父 ID。`PARSER_REVISION = 5` 使旧投影重新提取来源与父血缘。旧状态缺少该默认空映射仍可读取；Rust 字面量构造与手写状态映射需要消费方适配。

总库的绑定读取、三态判断与投影保存处于同一个 `BEGIN IMMEDIATE` 事务中。读取 `store_meta` 失败返回带 `RegistryUnreadable` 的注册表，不能压成空规则。消费方若另存本地索引，也须把 `TotalStore::load_session_bindings()` 的有效规则显式传入扫描；总库未就绪时无法确认只存在于元数据中的规则，应明确报告未就绪并冻结整轮持久化，不能把仅有文件规则当成完整快照。总库末端拒绝写入不能撤销消费方提前保存的正文或用量。

`SessionBindingRegistry::fingerprint()` 标识合并后的全部有效规则与故障，忽略 JSON 排版；`to_canonical_json()` 是回写原文，不能代替它。调用方逐来源持久化绑定身份，变化时使用 `ScanReasons::BINDINGS_CHANGED` 驱动完整重解析，并经 `ProjectionToken::with_bindings` 将绑定作为独立身份分量。规则 A→B→A 会恢复同一语义身份，完整目标提交仍须核对当前内容，不能把旧 token 当作这次已完成；缺少可信旧目标哈希时保守保留旧代。

`TotalStore::with_session_bindings(expected, action)` 在 `BEGIN IMMEDIATE` 后重读并校验扫描快照；变化或未知在回调前拒绝。回调用私有构造的 `BindingProjection` 句柄投影和提交本地索引，数据库中的绑定写者在整段期间被串行化。文件规则以事务入口读到的快照为准，SQLite 不锁外部文件写者；整轮快照本身也不等于提交栅栏。回调不得重入 `TotalStore` 的加锁方法。跨库提交仍不是一个原子事务：先持久化 desired，各层分别确认，总库 ack 必须等方法成功返回，失败保留欠账以恢复。普通空增量的提前确认也必须处于同一栅栏内。

绑定路径比较复用 `attribution::registry_key`；发行版内绝对路径先按来源声明的 `SourceLocation::Wsl` 补齐命名空间，与 UNC / `wsl:<distro>:` 写法收敛，保留 Linux 大小写。绑定注册表尚不持有运行期挂载事实；涉及 Windows 盘符与 Linux / WSL 命名空间的交叉比较，或 `/mnt/<盘>` 的别名比较时，显式返回 `UnknownHold`，不能把空挂载表当成已证明不匹配。控制面应提供可直接核对的同命名空间路径；本修复没有承诺跨盘符挂载的完整别名识别。显式非字符串 `state` 属于非法记录，只有缺失字段继续兼容默认 `Bound`。

`TotalStore::apply_projection` 保留操作重放契约：相同 token 返回那次操作的历史头，不能因当前头变化就重建旧目标。新增公开的 `apply_full_projection_target` 复用已有完整目标逻辑，只有当前目标、头与逐事件内容均一致时才短路；完整回退可正确处理已提交前缀 → 追加尾部 → 再回到该前缀及空目标。调用方只在成功观察且已确认从头读取完整事件时选择它，不能传入增量尾部；`Reparse` 缺少可核旧目标哈希时会保守转为保留旧代的 `Rollback`。

完整读取不一定观察为回退：增量追加不会刷新旧全读指纹，欠账重试又可丢弃游标；来源回到指纹覆盖的历史前缀时，观察仍可能为 `Appended`，而总库还留有未撤销的尾部。调用方在成功观察、`ReplaceFile` 且需恢复完整目标时，仍须显式选择完整目标 API；不能只以 `Rollback` 模式作为入口。完整目标为空也须提交，不能被空 `Append` 的 no-op 分支提前确认。整个来源改为临时排除时，已有当前投影的显式完整空目标同样切换当前头并保留旧代；首次临时空来源不建空代，重复提交不反复开代。API 选择与空批旁路共用同一个完整目标判据，普通增量尾仍走原操作 API。

以上回归均使用合成来源、隔离 SQLite 与显式测试钥匙，不证明运行中旧写者已完成升级，也不代替旧写者重写证据的兼容验收。

## 2. 原因是集合，规划是纯函数（I1）

```rust
bitflags! {
    /// 这一轮**为什么**要扫它 —— 由调度器给，**可以同时成立多个**。
    pub struct ScanReasons: u8 {
        const INITIAL           = 1 << 0; // 新文件 / 首次回填
        const FORCE             = 1 << 1; // 用户显式刷新
        const PARSER_STALE      = 1 << 2; // PARSER_REVISION 提升
        const ATTRIBUTION_STALE = 1 << 3; // 注册表长出覆盖它的新根
        const SYNC_DEBT         = 1 << 4; // 某个物化层尚未确认目标
        const BINDINGS_CHANGED  = 1 << 5; // 有效绑定规则变化
    }
}
```

**规划优先级（观察压过原因）：**

```
观测到源变化(RollbackOrRewrite)
  > ReparseRequired | PARSER_STALE | ATTRIBUTION_STALE | BINDINGS_CHANGED
  > INITIAL | FORCE | SYNC_DEBT
  > 增量（无 reason）
```

第一条是承重的：**磁盘上那段字节已经不存在了**，前一个源版本是它的唯一副本
（`store.rs`：`Rollback` 的旧版本**永不自动回收**）。把它当 `Reparse` 处理，
等于允许把唯一副本当成「可再生的更差解析」回收掉。

```rust
/// 一次提交要对四件事各做一个决定 —— 纯函数一次算出，**字段私有**。
pub struct CommitPlan { /* 私有 */ }

impl CommitPlan {
    /// 唯一构造入口。非法组合**构造不出来**（I1 的落地形式）。
    pub fn plan(obs: &AppendLogObservation, reasons: ScanReasons, has_prior: bool) -> Self;
    pub fn index(&self) -> IndexAction;      // ReplaceFile | AppendFacts | Preserve
    pub fn store(&self) -> StoreAction;      // Projection::{Append,Rollback,Reparse} | NoOp | Preserve
    pub fn cursor(&self) -> CursorAction;    // Advance | Freeze
    pub fn sync(&self) -> SyncTransition;    // 消费方（QuotaBar）的同步账本用
    pub fn quality(&self) -> QualityState;   // 见 §4
}
```

🔴 **守它的是字段私有，不是机械闸。** 跨模块自己拼一个字面量根本编译不过，
那比任何正则都强；模块内只有 `plan()` 一处，逻辑由组合矩阵守着。

> ⚠️ 曾给它写过一条正则闸，**误报四处** —— 全是**类型位置**（`struct X {` /
> `impl X {` / `-> Self {`）而非字面量。加排除规则能让它绿，但那时守住的是
> **历史拼写**而不是语义。**编译器已经守住的事，不要再用正则守一遍。**

### 决策表（观察 × 原因）

| 观察 \ 原因          | 无（增量）                                       | `INITIAL`/`FORCE`                                       | `PARSER`/`ATTRIBUTION_STALE`                          | 任意 + 观测到源变化                                    |
| -------------------- | ------------------------------------------------ | ------------------------------------------------------- | ----------------------------------------------------- | ------------------------------------------------------ |
| `Clean`              | index `AppendFacts` · store `Append` · `Advance` | index `ReplaceFile` · store `Append`（幂等）· `Advance` | index `ReplaceFile` · store **`Reparse`** · `Advance` | index `ReplaceFile` · store **`Rollback`** · `Advance` |
| `Degraded`           | _（增量不产生）_                                 | 同上 + `QualityState::Degraded`                         | 同上 + `Degraded`                                     | 同上 + `Degraded`                                      |
| `RejectedPoisonLine` | 全 `Preserve` · `Freeze`                         | —                                                       | —                                                     | —                                                      |
| `Unavailable`        | 全 `Preserve` · `Freeze`                         | 全 `Preserve` · `Freeze`                                | 全 `Preserve` · `Freeze`                              | 全 `Preserve` · `Freeze`                               |

**两条承重规则：**

1. **`Degraded` 整代替换、绝不与旧投影合并** —— 两批都从 seq 0 起，合并必然撞
   `(provider, location, source_path, seq)` 主键。
2. **`Rollback` 与 `Reparse` 由观察决定，不由原因决定** —— 见上。

## 3. deadline 是参数（I4）

```rust
pub struct Deadline(Instant);
impl Deadline {
    pub fn remaining(&self) -> Option<Duration>;
    /// 单次调用的实际上限 —— **剩余预算与该调用自身上限的较小者**。
    pub fn budget_for(&self, cap: Duration) -> Option<Duration>;
}
```

保证范围：**WSL 子进程覆盖全生命周期**（spawn / 关 stdin / 读输出 / 超时 kill /
`wait` 回收）；**所有子调用接收同一个绝对 deadline**，不各自重新拿 60 秒；
⚠️ **本地同步 FS 调用只能在读块之间协作检查**，不宣称硬超时 —— 写进文档而不是假装有；
**测试注入时钟**，不依赖真实 `sleep`。

🔴 **这条栽过四次，前三次是「有没有上限」，第四次是「上限归谁管」。**
三处曾分别拿固定的 `WSL_LIST_TIMEOUT` / `WSL_CALL_TIMEOUT`，与整轮预算无关。
实测量级 —— 整轮 **120 秒**、单次 **60 秒**，**两次调用就吃光全部**，而发现阶段
对每个发行版要调好几次；`expired()` 只在两次调用**之间**被检查，永远追不上。

判据是 `Deadline::budget_for(自身上限)`，耗尽 ⇒ `None` ⇒ **根本不发起**这次调用
（传零上限只会白 spawn 一个立刻被杀的进程）。由 QuotaBar 的
`scripts/verify-agents-md.mjs` 机械守着本仓 `src/wsl.rs` 的每个 `wait_with_deadline`
调用点 —— **人核不住「每一个出站 spawn」**。

> 🔴 **手写替代 `wait_with_output()` 时，先读它到底替你做了什么。** 它的第一件事是
> `drop(self.stdin.take())`；漏掉这一步，`bash` 会一直等 stdin 的 EOF，**每一次调用
> 都撞满超时**。症状极具误导性：日志说「timed out (distro wedged?)」，而同一条命令
> 手工跑只要 0.6 秒。名字（"wait with output"）说不出「顺带关了 stdin」。

## 4. 幂等 token（I7 的本仓这半）

设计第 2 稿把 `operation_id` 列为「待验证」，而测试矩阵又要求「同一操作重复执行
幂等」—— **自相矛盾**。而且当时只算了 `Reparse`（多开一代但会回收），
**漏了 `Rollback`**：

```
① 总库成功 Rollback，推进 source_revision
② 调用方的 UI 索引提交前崩溃
③ UI 仍是旧游标 ⇒ 下轮再次检出 rollback
④ 总库再次推进 source_revision
⑤ Rollback 的旧版本**按设计永不自动回收**（store.rs）
```

每崩一次留一代**不可回收**的源版本。所以最小方案不可推迟：

| 项                                                                                                                                | 状态                    |
| --------------------------------------------------------------------------------------------------------------------------------- | ----------------------- |
| `Rollback` / `Reparse` 携带稳定 `ProjectionToken`（`SourceKey` + 源指纹 + `parser_revision` + `attribution_revision` + 字节范围） | ✅ 已落地（`token.rs`） |
| 本仓对 token 建**唯一约束**，重复应用返回**原来的 head**（不开新代）                                                              | ✅ 已落地（`store.rs`） |
| `Append` 继续靠 seq 去重                                                                                                          | 不变                    |

2026-11-10 开发分支的 DSH 接线补充：`SourceChange::ReparseRequired` 表示既有归一化事件的语义或控制面丰富已改变，携带在公共扫描观察中；已有投影时选择 Reparse，索引整代替换。DSH 写库提交完整已核目标，防止只读流推进游标后漏写后缀。其专用目标 token 只在目标仍为当前最后一次提交时短路，历史目标 A→B→A 会重新生效；通用操作 token 仍按上表返回原来的 head。实现与兼容边界见 [DSH 读取状态设计 §14–§15](dsh-read-state-design.md#14-常规来源接线与持久化凭据)。

### 质量与同步是两根轴

```rust
/// 这份投影的解析质量 —— **与同步无关**。
pub enum QualityState { Clean, Degraded(ParseDiagnostics), RejectedPoisonLine(ParseDiagnostics) }
```

对应关系写死：

- `Degraded` ⇒ **已同步、质量降级** —— 不是欠账；
- `RejectedPoisonLine` ⇒ **已同步（旧投影仍是当前的）、质量为「毒行卡住」** ——
  它**必须可见**（界面/日志能说出「这个来源卡在某个偏移」），但不是同步欠账；
- `Unavailable` ⇒ **两层仍一致，新鲜度未知** —— 由调用方记「本轮没够着」，
  不是同步欠账。

> ⚠️ **设计里的第三根轴 `AttemptState` 至今没有落地为一个类型**（两个仓都搜不到）。
> 「本轮尝试结果」今天散在调用方的日志与 `owes_sync` 推导里。写在这里而不是假装
> 它在 —— **把没被遵守的纪律写成既成事实，比不写更糟**。

## 5. 存在性探测（I2）

```rust
/// 携带调用方需要的元信息 —— 只返回 `Present` 会让调用方再调一次 `.is_file()`，绕回原问题。
pub enum Probed<T> { Found(T), Absent, Unknown(ProbeError) }
```

🔴 **故意不提供 `is_found() -> bool` / `unwrap_or(false)`**：一个 `bool` 装不下三种
答案，于是「没问成」被迫挤进「没有」—— 那正是这一类事故的形状（**在两个仓里前后
栽过八次**）。调用方必须 `match` 到底，把「问不到时算什么」写出来。

祖先链上溯用**同一个纯决策引擎**，本地与 WSL 只实现不同 backend。同一目录层：
① 检查 `.git` 与全部 marker；② **任一正向命中即可确认当前层**；
③ **无正向命中且任一探测 `Unknown` ⇒ `Failed`，不继续向父目录找**；
④ 全部明确 `Absent` 才继续父目录。

### 🔴 `NotFound` 本身不足以定案

Windows 上一个**未挂载的盘符**返回 `ERROR_PATH_NOT_FOUND`（raw 3）⇒
`ErrorKind::NotFound`，与「盘符在、文件确实没有」**逐位相同**（后者也是 raw 3）；
死 UNC 主机是 raw 53，同样落 `NotFound`。

⚠️ **`try_exists()` 在未挂载盘符上返回 `Ok(false)`** —— 曾写着「用 `try_exists()`，
只有 `NotFound` 才是没有」，那句话在这一格上是错的。

判据是一条**原则**：叶子的缺失只有在**命名空间根可达**时才算事实
（`ancestors().last()` 天然给出 `C:\` / `\\server\share\` / `\\wsl.localhost\<distro>\`）。
不枚举错误码 —— raw 53 被它自然覆盖。

### 边界由 clippy 守，不由正则守

`clippy.toml` 列出被禁的 def-path（`std::fs::metadata` / `symlink_metadata`、
`Path::{exists,try_exists,is_file,is_dir,metadata,symlink_metadata}`）。
允许点**写在代码里看得见**：本仓只有 `probe.rs` 那三行 `#[allow]`。

🔴 **这条边界用正则守过两版，两版都被绕过。** 第二版被评审用六种**合法且可编译**的
写法穿过：`p.metadata()`（`Path` 的方法，整行不含 `fs::`）、`Path::exists(p)`（UFCS）、
`use std::fs as disk;` + `disk::metadata(…)`（别名）、`use std::fs::{self, metadata};`

- 裸 `metadata(…)`（重导出）。**正则看拼写，而拼写是无穷的；clippy 在名字解析之后
  按 def-path 匹配，六种全中。**

🔴 **三条踩过的坑：**

1. **例外会被走。** 曾为 `scan.rs` 取 size/mtime 留了「带 `?` 的 metadata 放行」，
   于是 `std::fs::metadata(p).is_ok()` 整条溜过去。正解不是把例外描述得更精确，是
   **把需要例外的调用搬进边界模块**（现为 `LocalBackend::stat`）。
2. **「禁存在性 API」仍是枚举语法。** 只禁 8 个存在性 def-path 时，
   `File::open(p).is_ok()` / `read_dir(p).is_ok()` 照样折叠 —— 而 `scan.rs` 里**当时
   就有**一处 `File::open`。禁整个模块面才终结这一类；`std::fs` 的 API 是**封闭集合**，
   枚举它等于禁掉它，与枚举无穷的语法形状是两回事。
3. **cfg / feature 门后的代码不在默认闸的覆盖里。** clippy 只编译当前平台分支；
   `--features acceptance-fixtures` 后面那段在默认组合下根本不编译 —— 有一轮就有
   一处改坏了而 Windows 全绿。判据是**每个会被编译的组合都过一遍**。

⚠️ **仍未闭合、明说**：`LocalBackend::unanchored()` 只核路径语法根，在 Unix 上
（`ancestors().last()` 恒为 `/`）说不出卷卸载。锚定形态（`rooted_at`）能说出来，
已覆盖 prune 驱动路径；彻底覆盖要持久化上次成功扫描时的卷/设备身份。

## 6. 发现失败必须报出来

🔴 **因为调用方会据发现结果删数据。** `discover_wsl` 的两级失败（`wsl -l -q` /
每个 `find`）曾被静默吞掉，只是返回一个更短的来源列表；而调用方的 prune 把
「这个 location 本轮零文件」当成「用户把它清空了」—— **实测一次 WSL 变慢删掉
369 个文件的会话与派生事实**。

现为 `DiscoveryOutcome { sources, unreachable }`，没问成的位置**整个**不 prune
（不能只跳过兜底条目：一个发行版可能一族 `find` 成功、另一族失败，那时 `kept` 里
已有它的**部分**路径）。

**本地遍历同样算**：`read_dir` / 目录项 / `file_type` 的错误也是「没问成」。
⚠️ `NotFound` **不算** —— 目录不存在是事实，把它报成不可达会让每个没装某 CLI 的
用户永久带着一个「不可达」位置、prune 全被禁掉。

**换算不出来也是「没问成」**：`/mnt/<drive>/…` 在挂载表为空时**不能**落回本机探测
（那在 Windows 上是当前盘根的相对路径，要么探不到、要么误中真实存在的 `\mnt\…`），
必须报失败走短退避档 —— 报「无」会被按「确认无根」走长退避。

## 7. 测试矩阵与性质测试

```
原因集合（含组合：回退+解析器过期、force+归属标脏 …）
  × 扫描观察（Clean / Degraded / RejectedPoisonLine / Unavailable）
  × 目标物化层（UI 索引 / 总库 / 游标 / 同步状态）
  × 提交失败点（总库失败 / 索引失败 / 两步之间崩溃）
```

**性质测试**（跨格不变式，比逐格断言更重要）：

- 同一 token 重复应用**幂等**，且不新增代；
- **不同 seq 命名空间永不合并**；
- **同尺寸原地重写**被识别为 `RollbackOrRewrite`（指纹护栏）；
- 只有**完整枚举且可达**的位置才允许 prune。

> 另外三条（未收到持久化确认不得清除欠账 · 只有所有预期来源进入成功终态才能宣告
> 完成 · 总库提交后 UI 提交前崩溃后两边收敛）跨两个仓，由 QuotaBar 驱动。

## 8. 调用方（QuotaBar）这一侧的义务

写在这里只是为了让本仓的读者知道**边界在哪**，正文在 QuotaBar 的 `docs/ADR-051`：

| 义务                                                                                      | 为什么在那边                                                |
| ----------------------------------------------------------------------------------------- | ----------------------------------------------------------- |
| **transcript 扫描只走 `scan_append_log_observed`**，不走 `session_vault::scan` 的有损投影 | `ScanStatus` 三个变体压掉了四种含义，曾靠日志文案前缀分回来 |
| **逐来源双确认**（`desired` / `store_ack` / `index_ack`）                                 | 表在 QuotaBar 的 SQLite 里                                  |
| **归属五态**                                                                              | `count_attribution_states` 等查询在 QuotaBar                |
| `Deadline` 每个出站调用的机械闸                                                           | 闸脚本在 QuotaBar，扫的是本仓的 `src/wsl.rs`                |
