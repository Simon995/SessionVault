# SessionVault — 共享摄取内核与 RawEvent 总库

> **本仓当前任务、顺序与发布记录统一见 [backlog.md](backlog.md)**；消费方自己的接入与产品进度回各仓维护。本文保存职责与数据契约，§15 保留历史抽取记录，不再作为当前排期。

现状（2026-11-10 对照源码整理）：Claude Code / Codex 会话摄取、状态快照、总库正文加密、墓碑擦除、防复活、项目归属及多机只读副本能力已实现；控制面会话绑定、扫描与总库双层临时会话排除、真实发送方与同侪来源保真（H.1 / H.2 / H.3）已随 `v2026.11.09.1`（`a151997` / `ccfbdca`）正式发布；DSH v4 复杂语义与会话根目录批量发现/扫描提交（`dc682ff`）、上下文压缩边界识别与有效上下文 `VIEW` 切点、Codex 分页续写核查及 `session-origin` 流式早停（G.1 / G.2 / G.3，`d7d3315`）已合入 `main`。参见 §2.1、§2.2 及待做清单 B / G / H。

本文包含现行接口、设计候选与历史记录；标为候选 / 预留的内容不能视为已实现能力。最后整理：2026-11-10。

> **外部引用约定**：本文出现的 `DECISIONS.md`（ADR-0xx）、`SYSTEM_DESIGN.md`、`INTEGRATION.md`、`AGENT_MEMORY_POSITION.md` 均为**其它仓库的跨仓库文档**，不在 SessionVault 仓内：
>
> - `DECISIONS.md` / `SYSTEM_DESIGN.md` / `INTEGRATION.md` → [TumeFlow `docs/`](https://github.com/Simon995/TumeFlow/tree/main/docs)（ADR-024/025/026 与本内核直接相关；其摘要见本仓 [README](../README.md) 与 [LOGGING.md](LOGGING.md)）。
> - `SESSION_MEMORY_ARCHITECTURE.md` / `AGENT_MEMORY_POSITION.md` / `LOGGING.md`（QuotaBar 侧）→ [QuotaBar `docs/`](https://github.com/Simon995/QuotaBar/tree/main/docs)。
>
> 本仓文档导航见 [README](../README.md#文档)；字段对账见 [rawevent-reconciliation.md](rawevent-reconciliation.md)，日志见 [LOGGING.md](LOGGING.md)，当前扫描 / 投影状态见 [scan-state-model.md](scan-state-model.md)。外仓文档是决策背景，不替代本仓现行源码与契约。

## 1. 定位

**SessionVault** 是一个**独立仓库的 Rust crate**（共享摄取内核 + 加密 RawEvent 总库），把"发现各 Agent 的本地数据 → 增量扫描 → 归一化为 `RawEvent` → 追加保存到总库"这件最容易踩坑的事**只实现一次**。总库默认保留，用户擦除与旧代回收受显式判据约束；各消费方复用同一份来源语义，避免 Codex 累计 token / `safe_offset` / WSL / 路径发现逻辑分叉。

- **中立命名 SessionVault**（crate `session-vault`、CLI `svault`），不属于任何一个消费者；消费形状包括 Rust 库、内嵌 CLI 与读取独立发布的 CLI。
- **声明式来源目录**（§3）是单一事实源：要扫哪些 provider、每个平台扫哪些文件夹/文件，全部集中在内核里维护一份。
- **provider 可扩展**（§4）：优先复用描述符与解析器框架；增加公开来源类型、字段或值域必须验证消费者兼容，不能承诺消费者零改动。
- **路径可配置**（§5）：已有配置根环境变量覆盖；任意宿主 `user_config`、派生配置根与前端设置接线仍是设计目标。
- **无头（headless）**：内核不带 GUI，但产出标准化**扫描报告**（§10），由宿主（QuotaBar / TumeFlow）渲染界面。
- 已提供 **Rust lib** + 一个吐 **NDJSON 的 CLI**；**PyO3** wheel 后置，尚未交付。
- 公开 API = 来源目录 + `RawEvent` 契约 + 无状态游标 + 扫描报告。破坏它 = major 版本。
- **黄金 fixture 语料**（§11）是它的一致性测试，也是"坑只有一处"的物理保证。

决策见 `DECISIONS.md` ADR-018、ADR-019。本文件是内核的设计契约；内核独立建仓后，本文件迁入该仓库，TumeFlow 仅保留引用。

## 2. 边界：内核做什么、不做什么

| 内核负责（唯一一份）                                | 内核不做（消费者各自负责）                                            |
| --------------------------------------------------- | --------------------------------------------------------------------- |
| 维护来源目录（provider × 平台 × 路径）              | 自带 GUI（仅产出扫描报告，宿主渲染）                                  |
| 发现来源（本地 / WSL；配置根环境变量覆盖）          | 脱敏 / 敏感信息处理                                                   |
| 增量字节读、`safe_offset`、坏行 / 半截行处理        | 下游产品领域数据库（总库另属同仓 `store` 组件）                       |
| Codex 累计 token parser state                       | 候选 / 记忆 / Dream（TumeFlow）                                       |
| WSL `wsl.exe` 桥接（只回传 `safe_offset` 后新字节） | `usage_facts` / 成本 / ROI 投影（QuotaBar）                           |
| 按已知项目根注册表归属；无法判定时保留未知          | 游标持久化（消费者各存各的库；`svault` CLI 自带默认实现 `--state`）   |
| 归一化为 `RawEvent`、产出结构化扫描报告             | LLM 调用、联网                                                        |
| —                                                   | 反向写回或原地改写各 Agent 原生会话目录（续接 / `resume` 物化归上层） |

上述边界指**扫描 / 解析层**：读取来源与调用方给出的游标、注册表，返回事件、新游标及报告；不持有总库、不持久化游标、不发网络、不弹界面、不改动上游源文件。同仓 `TotalStore`（`store` feature）另管持久化，CLI 负责编排。来源探测与项目归属以本仓源码及 [project-attribution.md](project-attribution.md) 为准；外仓 `SYSTEM_DESIGN.md` 的旧章节仅作设计背景。

### 2.1 外部参考：AgentGit（向后存证据 vs 向前续会话）

> 📌 **外部输入，会过期**：核查 [Einsia/agent-git](https://github.com/Einsia/agent-git) `main` 分支源码（`v0.2.29`，MIT，2026-10-30 查证 `Cargo.toml`、`docs/02_session_store.md`、`src/adapter/`、`src/domain/install/`、`src/commands/resume.rs`），非二手文章转述。

本仓与 AgentGit 都处理 Claude Code / Codex 会话历史，但目标正交：本仓管**「过去发生过什么」**（全自动后台入库、本地端到端加密、多机只读副本、物理擦除传播、Windows ⇄ WSL 路径与项目身份收敛）；AgentGit 管**「怎么从某一步继续跑」**（手动快照、会话分叉、跨运行时恢复）。对照结论：

| 维度                             | AgentGit 的做法（源码核实）                                                                                                                                                                                                                                                          | 本仓取舍与边界                                                                                                                                                                                                                                                                                                                          |
| -------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **运行时与 Hub 依赖**            | 本地每事件拆成单文件存 Git 对象库；`~/.agit/store/` 只存软链不自动兜底上游 30 天清理；本地 `commit` 强制先登录闭源 Hub，服务端持钥（非端到端加密）且只追加不可擦除；源码零 WSL 路径支持；读 Codex `.jsonl.zst` 时 `codex_rollout::restore` 会在原目录解压并**删掉原 `.zst`**         | **不安装、不接入其 CLI 或 Hub**。不在真实会话目录运行它，以免原地删 `.zst` 干扰扫描现场或破坏本仓的端到端加密与 `erase` 物理删除契约                                                                                                                                                                                                    |
| **跨工具恢复与 `RawEvent` 体积** | 核心 IR 极简（不含工具入参、`call_id` 与 `Thinking`），同格式恢复走原始行闭集改写，跨格式恢复（`resume --as`）靠 `enrich.rs` 按行号**回读源 JSONL** 配对入参与输出，并用 `close_open_calls` 补齐末尾悬空工具调用；跨格式损失判定只是 `a.format() != b.format()` 布尔值加固定三行提示 | **不为此扩大 `RawEvent` 存原始 JSONL 行**。单靠总库现有的瘦 `RawEvent`（工具入参只留 `[Tool: <名>]`）不能反向生成原生 `--resume` 文件；实时接力场景源文件仍在盘上，上层编排方若选原生转写路线，按本仓给出的 `(source_location, source_path)` 与路径换算回读源文件、参考其 MIT `src/adapter/` 转换即可；软交接单路线则直接读总库有效视图 |
| **压缩边界与有效视图（借鉴 ①）** | `LOG`（审计全史）与 `VIEW`（有效上下文，默认自最近一次压缩边界起算）分离；`EventKind` 区分无损过滤（Codex `compacted`）与有损合成摘要（Claude `isCompactSummary: true`，禁止喂给 `merge`、提首条意图时跳过）                                                                         | **已落地（`d7d3315`，见 [backlog.md](backlog.md) G.1）**：在 `classify_user_input_origin` 最前端标出压缩边界与合成摘要（`claude_compact_summary` / `claude_compact_boundary` / `codex_compacted` / `dsh_compaction_checkpoint` / `dsh_surface_replace`，归为 `SubagentOrRuntime`），并提供 `is_compaction_summary` 与 `effective_view_slice` 帮下游切出当前有效窗口，防止记忆层把模型二手摘要当用户原话重复蒸馏 |
| **重放血缘防重（借鉴 ②）**       | 恢复/分叉在 `link` 与 `meta.json` 记血缘基线                                                                                                                                                                                                                                         | **上层做原生转写时的硬前提**（见 [backlog.md](backlog.md) G.4）：往原生目录写重放文件必须带血缘标记，让本仓扫描跳过重放段，防重复计 token 与重复入记忆                                                                                                                                                                                  |
| **Codex 上游格式线索（借鉴 ③）** | `codex_rollout.rs` 处理 `<thread>_<segment>` 分页续写段（沿 `session_meta.payload.history_base` 拼接）与 `.jsonl.zst` 流式首行早停；`codex_index.rs` 只读查 `state_<N>.sqlite`                                                                                                       | **已核查并落地（`d7d3315`，见 [backlog.md](backlog.md) G.2、G.3）**：已核本机与 WSL 的分段 rollout（只追加切点后新记录，不重复计 token），并将 `read_codex_origin` 改为流式逐行早停                                                                                                                                                   |

### 2.2 宿主与子 CLI 来源适配：职责与控制面绑定契约

> **2026-11-04 登记，2026-11-09 落地控制面绑定、临时会话排除与真实来源保真（H.1 / H.2 / H.3），2026-11-10 合入 DSH v4 复杂语义与会话根批量扫描（H.4，`dc682ff`）及有效视图切点（G.1–G.3，`d7d3315`）**。11-05 至 11-10 已完成只读格式核查、受控双进程窗口、独立快照审计、Node 同缓冲区捕获、Rust `dsh-experimental` 物理核验与原生语义 admission 接缝、会话目录选代、保守提交门、`RawEvent` 归一化位置映射、表面折叠/压缩/子代理/多层 fork 及会话根目录批量发现与扫描提交（`discover_dsh_sessions_in_root` / `scan_dsh_sessions_root_to_store`），并在 [`src/session_binding.rs`](../src/session_binding.rs) 与 [`src/rawevent.rs`](../src/rawevent.rs) 实现控制面准入契约、真实来源字段与压缩边界切点，详见 [dsh-ingest-survey.md](dsh-ingest-survey.md) 与 [dsh-read-state-design.md](dsh-read-state-design.md)。DSH 默认 `default_providers()` 注册待宿主固定线上会话根后开启，任务与顺序记在 [backlog.md](backlog.md) H 节，遵守 `AGENTS.md` §4 规则 2。

宿主对话摄取与执行后端选型分开：宿主已选用 DSH 时，其会话来源可以独立核查，不必等 Worker 选型结束。上层若复用 DSH 原生日志，只摄取同一份事实来源；只有上层另有独立会话记录时才评估新增 provider，避免宿主导出与原始日志重复计量、重复进入记忆。当前通用 JSONL 的实验入口不等于专用接入已交付。

| 事项                   | 本仓承接                                                                                                                                                                   | 上层编排方 / 记忆引擎承接                                                                    |
| ---------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------- |
| 正式会话来源           | 发现、解压、格式解析、稳定来源身份、项目归属、增量与损坏恢复；核清旧版 / 新版并存时的逻辑来源与去重                                                                        | 提供实际配置根、脱敏样本及启动绑定；决定是否产生独立宿主记录，不另写路径换算或原生格式解析器 |
| 临时会话               | `SessionBindingRegistry` 统一三态准入（`PersistentAllowed` / `TemporaryExcluded` / `UnknownHold`），扫描与 `TotalStore::apply_projection_if` 执行同一判据；区分 `temporary` 与 `read_memory` | 启动宿主与子 CLI 时传递控制面绑定；记忆引擎与消费缓存遵守临时范围，不把临时输出蒸馏进长期记忆 |
| 真实发送方与 peer 来源 | `RawEvent::input_origin`（`SenderKind` + `InputTrustLevel` + `sender_session` / `sender_address` / `xs_message_id` / `assignment_id` / `handoff_id`）保留可核来源与未知状态 | 产生真实发送方 / 接收方绑定；执行授权由控制层判定，peer 输入与历史检索结果不能升级成人类授权 |
| 父子与重放血缘         | 区分父事件、父子会话（目录 `<parent>/subagents/agent-*.jsonl` 及显式 `parentSession` / 控制面 `parent_session_id`）与重放前缀关系；无法确认时显式 `ParentLineageUnknown`     | 记录任务到宿主 / 子 CLI 的执行绑定、派单与接力；若做原生转写则提供重放基线                   |
| 删除与可回源状态       | 复用本地只读读取、稳定引用、墓碑与防复活；明确不可用、已擦除及缺证据的差别                                                                                                 | 消费删除记录并清理索引、缓存与派生记忆；跨主机认证、请求路由、离线等待与删除回执编排归上层   |

临时属性与来源身份来自控制面绑定（`TotalStore` 的 `store_meta['session_bindings_json']`、与库同目录的 `session_bindings.json` 或 `SVAULT_SESSION_BINDINGS`）及可核同侪信封元数据，不靠自由文本自称。排除在持久化任何 `Message` / `Thinking` / `ToolUse` / `ToolResult` / `Usage` 事件前生效，覆盖全量、增量、重扫、子会话血缘传递及多写者；绑定缺失必填字段、处于 `pending` 未决、存在冲突或清单损坏时显式进入 `UnknownHold`（fail-closed，扫描返回 `Unavailable` 冻住游标，写库返回 `StoreError::BindingUncertain`），绝不静默当成 `temporary: false`。既有未登记普通历史会话默认以 `verified_human_input: false`、`InputTrustLevel::Unknown` 保持兼容摄取。`scan-all --only` 按来源类型过滤、`--stateless` 控制游标、`metadata` 抑制正文，均不替代 `SessionBindingRegistry` 的端到端排除。

日志 `role=user` / `Actor::User` 不是人类亲自输入或授权的证明：`RawEvent::input_origin` 将用户角色消息细分为 `HumanVerified`、`PeerSession`（如 `<cross-session-message ...>`、`[xs id=...]`）、`CoordinatorDispatch`、`SubagentOrRuntime` 与 `Unknown`，即使会话绑定了人类发起者，混入该会话的同侪跨会话消息仍保持 `PeerSession` / `PeerEnvelope` 或 `IncompleteEnvelope`，绝不升格为 `HumanVerified`。`parent_ref` 保持为父事件引用槽位，父子会话血缘由 `ParseOut::session_parents` 与 `SessionBindingRegistry` 独立承载。

**原始总库留在来源主机、记忆就地处理**的需求不以复制整库为前提。现有 [多机副本模型](linux-replica.md) 仍是另一条已记录方案；它的 `--all-stores` 不是远程回源 RPC。新模式下，本仓提供来源主机上的本地证据与状态出口，上层承担远程编排；具体传输与身份绑定尚待契约确认。`erase` 不删除上游原生日志，也不会自动清理下游缓存；`session-origin` 只读 Codex 记录的仓库远端，不查墓碑，不能据它绕过擦除限制回读正文。总库不可读、会话找不到、证据版本变化、已擦除与来源主机离线，不得合并成一个空结果或“可恢复”。

## 3. 来源目录（Source Catalog）

这是内核的核心资产之一：**要扫什么、在哪扫，集中声明一份**，消费者不各自硬编码路径。已实现的内置描述符以 [`src/catalog.rs`](../src/catalog.rs) 为准，本地配置根按 §5 的环境变量覆盖；来源探测区分存在、不在与无法判定。所有路径中 `~` / `%USERPROFILE%` 按当前平台展开。

**2026-11-05 实现对照**：以下旧来源表还保留了设计候选，并非每一行均已摄取；未列入当前描述符的候选不能据此启用。

| 已实现来源                                                                                                                                             | 形态与状态                                                                                            |
| ------------------------------------------------------------------------------------------------------------------------------------------------------ | ----------------------------------------------------------------------------------------------------- |
| Claude `projects/**/*.jsonl`、`sessions/**/*.jsonl`                                                                                                    | `append_log`，`stable`                                                                                |
| Claude 全局 `CLAUDE.md`、`projects/**/memory/*.md`、`skills/**/*.md`、`tasks/**/*.json`、`plans/*.md`、`history.jsonl`                                 | `snapshot_file`，`experimental`；history 目前仍是整份指纹快照，增量化见 [backlog.md](backlog.md) E 节 |
| Codex `sessions/**/*.jsonl`、`archived_sessions/**/*.jsonl`                                                                                            | `append_log`，前者 `stable`、后者 `experimental`；都支持逻辑路径对应 `.zst`                           |
| Codex 全局 `AGENTS.md`、`memories/*.md`、`memories/rollout_summaries/*.md`、`memories/extensions/ad_hoc/notes/*.md`、`rules/*.rules`、`skills/**/*.md` | `snapshot_file`，`experimental`                                                                       |
| 宿主已确认项目根内的指令文件                                                                                                                           | `discover_project_snapshots` 单独发现，不能把全部项目层候选都算作已实现                               |
| 通用 JSONL `sessions/**/*.jsonl`                                                                                                                       | `append_log`，`experimental`；仅显式设置 `SVAULT_JSONL_DIR` 时发现                                    |

`sqlite_store` / `opaque_family`、任意派生配置根与其他 provider 仍是预留设计；当前描述符没有 Cursor / Gemini 的可扫描实现。

#### 来源的两个正交属性（架构保险，见 ADR-025）

数据源审计（结论已并入本文，原始报告已移除）揭示一个关键点：**不能把所有来源都默认当成"追加写的 JSONL、按字节增量读"**——一旦后面接 rules / hooks / agents / 任务状态 / SQLite，就会被迫改核心层。为此来源目录给每个 artifact 标两个正交属性：

- **`source_mode`（形态）**：决定怎么读、用什么游标——
  - `append_log`：只增长的 JSONL（会话、history 的常态），按字节 `safe_offset` 增量读。
  - `snapshot_file`：会被整体重写/覆盖的文本（CLAUDE.md / AGENTS.md / rules / memory 等），按 SHA-256 内容指纹判变，变了产 `config_snapshot`。**已落地选择**：`full` 档把正文写入 `RawEvent.content`，随事件一起进入 AES-256-GCM 加密总库；`content_hash` 用于版本和增量，`observed_at` 只表示扫描观测时间，不伪造真实生效时间。后续体积证明需要时再评估按 hash 去重的 `snapshot_blob`，当前不提前增加第二存储契约。

    🔴 **2026-08-29 新增 `modified_at`（源文件 mtime，Unix 秒串）。** 这**不是**推翻
    「mtime 不冒充语义时间」那条 —— 它仍然**不**写进 `occurred_at`，而是自成一个字段，
    由消费者显式选择怎么用。理由是消费者侧量出来的：`observed_at` 是**扫描时刻**，
    206 个快照只有 110 个不同取值，**60% 与别人同秒**（首次上线时一次收进来的存量
    文件全挤在一起，最大一堆 34 个）。⇒ 两个信号**互补**：`observed_at` 分得开
    「扫描批次之间」，`modified_at` 分得开「批次之内的存量文件」。
    ⚠️ 之所以不并进 `occurred_at`：`recent_sessions` 按 `occurred_at_unix_ms` 排序且
    **不过滤 `config_snapshot`**，而下游正是靠那个判「用户最近有没有活动」——
    快照文件本身就是下游写的，那会让它把自己的写入读成新活动。
    ⚠️ 问不到（stat 失败 / WSL 桥不可用 / 非 Windows 构建）一律 `None`，
    **不折进 0，也不折进 `observed_at`**。

  - `sqlite_store`：SQLite 状态库（Codex `CODEX_SQLITE_HOME`、Cursor `state.vscdb`），用表游标（`rowid` / `wal_lsn` / `content_hash`），**不可按字节读**。
  - `opaque_family`：官方未公开稳定结构的状态族（Claude background/supervisor state、Codex plugin bundle），只登记为"保留来源族"先不写死 glob。

- **两层契约**：§3 区分**来源族**（稳定层：session log / snapshot 指令 / rules / hooks / skill root / plugin root / sqlite state root / derived 任务根）与**已验证实现**（易变层：具体子目录 glob，如 Codex `sessions/YYYY/MM/DD/rollout-*.jsonl`）。子目录漂移只动易变层，**不拖公开契约升 major**。

下面 §3.2 / §3.3 的"主表"是 `append_log` 主数据源；其后的"控制面/行为塑形补充表"多为 `snapshot_file` / `sqlite_store` / `opaque_family`。

### 3.1 平台数据根参考

| 平台         | 家目录          | 应用数据根（供新 provider 用）                        |
| ------------ | --------------- | ----------------------------------------------------- |
| Windows 原生 | `%USERPROFILE%` | `%APPDATA%`（Roaming）、`%LOCALAPPDATA%`              |
| macOS        | `~`             | `~/Library/Application Support`                       |
| Linux        | `~`             | `$XDG_CONFIG_HOME`（默认 `~/.config`）                |
| WSL 发行版   | 发行版内 `~`    | 经 `wsl.exe` 桥或 `\\wsl$\<distro>\home\<user>\` 访问 |

Windows 上每个 provider 都要同时考虑 **Windows 原生**与**一个或多个 WSL 发行版**两套独立来源（见 `SYSTEM_DESIGN.md` §11.3 / ADR-017），靠 `source_location`（`local` / `wsl:<distro>`）区分，禁止只扫一处或合并两处。

### 3.2 Claude Code（status: stable）

配置根默认 `~/.claude`，可被 `CLAUDE_CONFIG_DIR` 覆盖。

| 类别(kind)  | 相对路径 / glob                                                                                                                            | 含正文 | 说明                                                                                                           |
| ----------- | ------------------------------------------------------------------------------------------------------------------------------------------ | ------ | -------------------------------------------------------------------------------------------------------------- |
| session     | 来源族=`projects/` **与 `sessions/`** 两根下**递归任意 `*.jsonl`**（已验证实现，非 `projects/<enc>/<uuid>.jsonl` 单一模式）                | 是     | 会话事件流，主数据源。QuotaBar 实证：`.claude/projects` + `.claude/sessions` 两根、`find -name '*.jsonl'` 递归 |
| memory      | `projects/<项目>/memory/*.md`                                                                                                              | 是     | 已按 `snapshot_file` / `experimental` 摄取；`autoMemoryDirectory` 改写属于待核派生配置根，不能视为已接线       |
| history     | `history.jsonl`                                                                                                                            | 是     | 当前按 `snapshot_file` / `experimental` 保存整份；尚未按行增量解析                                             |
| instruction | 用户 `<config>/CLAUDE.md`；项目 `./CLAUDE.md`、`./.claude/CLAUDE.md`、`./CLAUDE.local.md`、`./.claude/rules/*.md`；托管 policy（平台相关） | 是     | 只读不改写；须解析 `@import`（≤4 跳）与 rules 的 `paths` 作用域                                                |
| config      | `<config>/settings.json` 等                                                                                                                | 否     | 仅用于解析 `autoMemoryDirectory` / `cleanupPeriodDays`，不作事件                                               |

平台配置根：Windows `%USERPROFILE%\.claude\`；macOS / Linux `~/.claude/`；WSL 发行版内 `~/.claude/`。

**控制面 / 行为塑形来源（补充，多为 `snapshot_file` / `opaque_family`）**——它们解释"Agent 为什么按这种工作流做事"，是流程性/偏好记忆的证据；下列相对配置根 `<config>`（默认 `~/.claude`，受 `CLAUDE_CONFIG_DIR` 覆盖），项目层在 `./.claude/` 下：

| 类别(kind)   | 相对路径 / glob                                                              | source_mode   | 含正文         | 说明 / 置信度                                                                                   |
| ------------ | ---------------------------------------------------------------------------- | ------------- | -------------- | ----------------------------------------------------------------------------------------------- |
| command      | `<config>/commands/*.md`；项目 `./.claude/commands/*.md`                     | snapshot_file | 否             | 自定义 slash 命令=可调用工作流证据。较可信                                                      |
| agent        | `<config>/agents/*.md`；项目 `./.claude/agents/*.md`                         | snapshot_file | 否             | subagent 定义改系统提示/工具/模型，行为塑形。较可信                                             |
| skill        | `<config>/skills/**/SKILL.md`（含插件分发载体）                              | snapshot_file | 部分           | `CLAUDE_CODE_SYNC_SKILLS=1` 会下载 skills；程序性记忆。较可信                                   |
| plugin       | `<config>/plugins/**`（marketplaces / cache）                                | opaque_family | 否             | 装了哪些插件/来源；至少读来解析能力面。env `CLAUDE_CODE_PLUGIN_CACHE_DIR` / `_SEED_DIR`。中     |
| task         | `<config>/tasks/**/*.json`                                                   | snapshot_file | 是             | 已按 `experimental` 摄取，只收 JSON，排除锁 / 游标文件；`CLAUDE_CODE_TASK_LIST_ID` 派生根未接线 |
| mcp/config   | 项目 `./.mcp.json`；`<config>/settings.json`、项目 `./.claude/settings.json` | snapshot_file | 否             | 解析 MCP / hooks / `autoMemoryDirectory` / worktree 行为，定位真实数据根。中                    |
| worktree     | 项目 `./.claude/worktrees/**`                                                | opaque_family | 间接           | background session 隔离编辑快照；作 provenance，不默认全文摄取。中                              |
| forensic log | `OTEL_LOG_RAW_API_BODIES=file:<dir>` 指定目录                                | append_log    | 可能含完整正文 | 完整请求/响应落盘；路径全由 env 决定，作"条件来源族"，不写死。高确信存在                        |
| bg state     | `<config>/` 下未公开 supervisor/background-session state                     | opaque_family | 可能           | 官方确认持久化在 config dir，文件名未公开；保留来源族，不硬编码                                 |

> 本表是控制面候选与已实现来源的混合记录，当前覆盖以上方实现对照为准。已接入的快照在 `full` 档可产 `RawEvent.content`；未实现来源的发现、配置解析与版本化不能据此宣称完成。

### 3.3 Codex（status: stable）

配置根默认 `~/.codex`，可被 `CODEX_HOME` 覆盖。存在与语义随版本变化，**先探测再按 schema 解析**。

| 类别(kind)  | 相对路径 / glob                                                                                                                                                    | 含正文 | 说明                                                                                                                         |
| ----------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ------ | ---------------------------------------------------------------------------------------------------------------------------- |
| session     | 来源族=`sessions/` **与 `archived_sessions/`** 两根下**递归任意 `*.jsonl`**；已观察实现=`sessions/YYYY/MM/DD/rollout-*.jsonl`                                      | 是     | 日期分桶会话轨迹。QuotaBar 实证：扫 `.codex/sessions` + **`.codex/archived_sessions`** 两根、递归 `*.jsonl`（勿漏 archived） |
| history     | `history.jsonl`                                                                                                                                                    | 是     | 历史/会话转录，受 `history.persistence` / `max_bytes` 控制                                                                   |
| index       | `state_*.sqlite`                                                                                                                                                   | 否     | 若存在仅作可选索引，不作唯一事实                                                                                             |
| instruction | 全局 `$CODEX_HOME/AGENTS.override.md` 或 `AGENTS.md`（取首个非空）；项目自 Git 根到 cwd 逐级 `AGENTS.override.md` / `AGENTS.md` / `project_doc_fallback_filenames` | 是     | 只读；`.codex/config.toml` 是配置而非指令                                                                                    |
| config      | `$CODEX_HOME/config.toml`                                                                                                                                          | 否     | 仅配置，不作事件                                                                                                             |

平台配置根：Windows `%USERPROFILE%\.codex\`；macOS / Linux `~/.codex/`；WSL 发行版内 `~/.codex/`。

> **就地压缩的会话**（2026-10-20 加）：新版 Codex 把 mtime 超过 7 天的 rollout 就地压成 `rollout-*.jsonl.zst`，续聊要追加时再还原成 `.jsonl`（出处与实测见 `src/compressed.rs` 开头）。两处会话目录都认它，且它与 `X.jsonl` 是**同一个逻辑来源**：发现报逻辑路径 `X.jsonl`（两种形态并存只报一次），`source_path`、字节偏移、大小、指纹都按解压后的字节算，与原文逐字节相同，游标在两种形态之间照样接得上。读时 `X.jsonl` 在就读它，不在才读 `.zst`。所以 append-log 来源的 `path` 不保证磁盘上就有这个文件 —— 读它走本仓（`scan` / `session-origin`），别在消费方直接开。

> **session glob 降级**：上表 `sessions/YYYY/MM/DD/rollout-*.jsonl` 应视为**已观察实现**而非公开稳定契约——官方只承诺 `CODEX_HOME` 下有 sessions、不承诺分桶层级。归到"sessions 来源族 + 探测后 schema 解析"，当前 rollout JSONL 作一个 parser 候选。

**控制面 / 状态面来源（补充）**——`<root>`=`CODEX_HOME`（默认 `~/.codex`），项目层在 `./.codex/` 下：

| 类别(kind)     | 相对路径 / glob                                                                                                       | source_mode   | 含正文 | 说明 / 置信度                                                             |
| -------------- | --------------------------------------------------------------------------------------------------------------------- | ------------- | ------ | ------------------------------------------------------------------------- |
| config（多层） | `<root>/config.toml`、`<root>/<profile>.config.toml`、项目 `./.codex/config.toml`、Unix `/etc/codex/config.toml`      | snapshot_file | 否     | 配置层级=项目/profile/用户/system；漏了会丢项目级 MCP/hooks/rules。中     |
| agent          | `<root>/agents/*.toml`；项目 `./.codex/agents/*.toml`                                                                 | snapshot_file | 否     | personal/project custom agents，覆盖模型/sandbox/MCP/skills。中           |
| hooks          | `<root>/hooks.json`、项目 `./.codex/hooks.json`，及 config.toml `[hooks]`                                             | snapshot_file | 否     | 行为控制面/自动化 side-effect 证据。低中                                  |
| rules          | `<root>/rules/*.rules`、各 active config 层下 `rules/`                                                                | snapshot_file | 否     | 用户授权命令写到 `rules/default.rules`=持续学习出的权限/流程记忆。低中    |
| skill          | `~/.agents/skills/**/SKILL.md`、`$REPO/.agents/skills/**`、`$CWD[/..]/.agents/skills/**`、Unix `/etc/codex/skills/**` | snapshot_file | 是     | REPO/USER/ADMIN/SYSTEM 多作用域；程序性记忆一等公民。**当前最大缺口**。中 |
| sqlite state   | `CODEX_SQLITE_HOME` 或 `<root>` 下 `**/*.sqlite*`                                                                     | sqlite_store  | 不定   | 官方有 SQLite-backed state 且可单独重定向；提升成"SQLite 状态根"。高      |
| plugin         | config.toml `[plugins.*]`；本地 bundle 路径未公开                                                                     | opaque_family | 可能   | 插件启停+注入 MCP/hook/skill；先纳配置层，bundle 后补。很高漂移           |
| auth           | `<root>/auth.json`（file-based 凭据时）                                                                               | —（排除）     | 否     | **凭据面，明确不收正文、不入 RawEvent**，仅供安全排除规则。低             |

> `history.jsonl` 已在主表，但**不是严格 append-only**：超 `history.max_bytes` 会丢最旧并压缩文件，需配合 §8 的截断/重写检测。

### 3.4 后续 provider（status: planned，路径待验证）

下列 provider 先以**描述符**登记占位，准确路径以探测 + 用户配置为准，验证后再升 `experimental` / `stable`，**不要把候选路径当稳定接口**。

| provider       | 候选位置（待验证）                                                                                                            | 形态线索                                                                                          |
| -------------- | ----------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------- |
| Cursor         | Windows `%APPDATA%\Cursor\User\...`；macOS `~/Library/Application Support/Cursor/User/...`；Linux `~/.config/Cursor/User/...` | VS Code fork，会话多在 `workspaceStorage/<hash>/state.vscdb`、`globalStorage`（SQLite），非 JSONL |
| Gemini CLI     | 候选 `~/.gemini/`（按平台展开）                                                                                               | 待验证文件结构                                                                                    |
| ~~通用 JSONL~~ | ——                                                                                                                            | **已升 `experimental`，见 §3.5**                                                                  |

新增 provider 的标准流程见 §4。

### 3.5 通用 JSONL（status: experimental）

**把「外部投喂的对话」变成一个来源。** 用途是让本内核之外产生的对话走**与任何
agent 会话逐字相同**的摄取路径 —— 同一套物化、去重、游标与归因。

⚠️ **本节刻意不举某个消费者当用途。** 一个 provider 的正当性不该依赖谁在用它：
写进内核文档的消费者名字，会在那个消费者消失之后仍然留在这里，声称一个不再成立
的理由。判据是「这批数据是不是对话」，不是「谁要用」。

|               |                                                     |
| ------------- | --------------------------------------------------- |
| `source_type` | `jsonl`                                             |
| 根            | **只认 `$SVAULT_JSONL_DIR`，没有默认值**            |
| 产物          | `sessions/**/*.jsonl`，`append_log` + `byte_offset` |

🔴 **没有默认根是刻意的，而且是它能进内置清单的全部理由。** 环境变量没设 ⇒
`config_dir: None` ⇒ 发现器跳过并记 `no_config_dir` ⇒ **既有消费者的行为一个字节
都不变**。由 `catalog::tests::the_generic_jsonl_provider_is_absent_without_its_env_var`
钉着。

**行契约**（一行一条对话消息；缺字段就跳过该行，**不猜**）：

```jsonc
{"role": "user"|"assistant"|"system"|"tool",   // 认不出 ⇒ 跳过，不落到默认 actor
 "content": "…",                               // 缺 ⇒ 跳过
 "timestamp": "2026-08-24T10:00:00Z",          // 可选，**ISO8601 字符串**
 "session_id": "…",                            // 可选，缺省取文件名
 "cwd": "…"}                                   // 可选，参与项目归属
```

⚠️ **输入 `timestamp` 只收字符串，不收数字**（unix 毫秒）。`RawEvent.occurred_at` 保留该串；缺失或类型不符时为 `None`、`time_confidence=low`，不拿「现在」冒充。字符串的时间解析是另一层：本仓已有 `occurred_at_unix_ms` 归一到 UTC 毫秒，无法解析则返回 `None`；不能把输入是字符串等同于已核时间有效。

🔴 **不要把外部对话伪装成 Claude / Codex 的形状塞进它们的根。** 那样零改动就能跑，
但事件会带着错误的 `source_type` —— **对来源撒谎**，而 provenance 是下游一切判决的
地基。这个 provider 存在就是为了让那条捷径没有必要。

## 4. Provider 可扩展性（声明式描述符）

新增来源优先复用描述符与读取框架；若要增加公开 `SourceType`、字段或值域，仍须按 §2.2 与 [backlog.md](backlog.md) H.7 验证多类消费者。当前数据结构摘要如下，完整字段类型见 [`src/catalog.rs`](../src/catalog.rs)：

```text
ProviderDescriptor {
  source_type
  name
  config_dir           # 已解析的本地配置根；通用 JSONL 未设环境变量时为 None
  artifacts: [Artifact {
    subdir, glob
    source_mode
    status             # stable | experimental | planned，属于 artifact
    recursive
    kind               # 快照类别；append_log 为 None
    may_be_compressed  # 是否允许逻辑路径对应 <path>.zst
  }]
}
```

- **描述符集中声明读取范围**：发现器复用根探测、文件匹配与来源报告；配置根和 WSL 发现行为以当前实现为准，不因表中列了候选环境变量就视为已支持。
- **解析按来源类型与形态分派**：当前已有 Claude / Codex / 通用 JSONL 的 append-log 解析及 snapshot-file 读取；并非已交付任意 parser trait / SQLite 插件接口。
- 新 provider 的流程是先登记候选、补解析与黄金 fixture、再核状态和兼容；`planned` 是占位而非已有可扫描实现。
- `RawEvent` 当前没有 `parser_version` 字段。解析器升级的稳定引用、投影替换与游标处理按 [`scan-state-model.md`](scan-state-model.md) 核验，不能假定 schema 版本就等于解析器版本。

## 5. 路径可配置（内置默认 + 用户覆盖）

**现行能力**：本地 Claude 根由 `CLAUDE_CONFIG_DIR` 覆盖默认 `~/.claude`，Codex 根由 `CODEX_HOME` 覆盖默认 `~/.codex`；通用 JSONL 只认 `SVAULT_JSONL_DIR`，没有默认根。当前 `catalog()` / `discover()` 不接收宿主 `user_config` 入参。Windows 的 WSL 来源另行发现，本地根覆盖不能当作 WSL 隔离，见 §13.1.1。

**仍待实现的设计目标**：任意宿主配置可启停 provider、增删根路径；需要解析 config/env 的来源走「读配置 → 算真实根 → 派生子来源」。例如 SQLite、插件、任务列表或 profile 的派生根，不因 §3 列了环境变量就视为已接入。路径解析仍应归本仓，消费者不另写同一套推导。

以下只表达配置需求，**不是现成配置文件格式**；宿主持有和持久化配置，新增公开入参前另定兼容契约：

```text
[providers.codex]       enabled=true
[providers.cursor]      enabled=true  roots=["D:/Cursor/User"]
[providers.claude_code]  roots_add=["<extra-config-root>"]
```

授权 UI、启停设置和按来源正文策略由上层控制面接合；当前来源白名单不能替代尚未交付的任意目录授权清单或临时会话排除。

## 6. 两种抽取档位（profile）

| profile    | 适用场景                                 | 是否含正文         | 用途                                   |
| ---------- | ---------------------------------------- | ------------------ | -------------------------------------- |
| `metadata` | 不建总库的独立 / 轻量场景                | **否**             | token / model / cwd / 项目 / 时间      |
| `full`     | 共享总库 + TumeFlow 物化（默认集成形态） | 是（原始可见正文） | 物化分库时由 TumeFlow **脱敏**后入证据 |

内核在 `metadata` 档**根本不提取正文**。脱敏不在内核里，而在 TumeFlow 物化分库时做。

> **`full` 档的正文持久化是新建能力，非来自 QuotaBar**（见 `rawevent-reconciliation.md` §3）：QuotaBar 现仅按需即时解析正文、落库只存折叠 `search_text`。`full` 档把 `message_from_*_value` / `extract_text` 的即时正文升级为持久 `RawEvent.content` 并入总库，是 P3 总库的主要新建量。

档位是内核的**能力**，不再等于 QuotaBar 的隐私边界：采用共享 RawEvent 总库（§13 / ADR-020）后，总库以 `full` 物化（含正文），QuotaBar 已放开"不读正文"限制、经总库读取正文（见 ADR-021 / QuotaBar `AGENT_MEMORY_POSITION.md` §1.5）。`metadata` 档位保留用于不建总库的场景（例如只装 QuotaBar、或想要不含正文的轻量投影）。描述符中 `content: false` 的 artifact（如 config）在任何档位下都不出正文。

## 7. `RawEvent` 契约

以下为 2026-11-05 对照 [`src/rawevent.rs`](../src/rawevent.rs) 的字段摘要，完整类型与值域以该文件为准：

```text
schema_version            # RawEvent schema 版本；不等于解析器版本
source_type               # provider，含已实现与预留枚举值
source_location           # SourceLocation；as_key() 为 local / wsl:<distro>
source_path               # 来源逻辑路径；压缩件磁盘上可能只有 <path>.zst
source_session_id
seq                       # 本次解析的产出序号，增量批次延续；不作跨解析器升级身份
event_key                 # Option<EventKey>：version / record_fingerprint / slot_ordinal
source_mode               # 来源形态；sqlite_store 仍预留，opaque_family 不产事件
cwd                       # 原始会话 cwd
project_root              # 归属结果；未归属时也可能保留原始路径
project_root_source       # 归属判定依据；须据此区分已归属与未归属
workspace_location        # 项目物理位置，与 transcript 的 source_location 分开
event_type                # message / tool_use / tool_result / usage / meta / config_snapshot / thinking
actor                     # Option<Actor>，user / assistant / tool / system；不是授权证明
input_origin              # Option<InputOrigin>：sender_kind / trust_level / sender_session / sender_address / xs_message_id / assignment_id / handoff_id
occurred_at               # Option<String>，保留来源原始时间戳
time_confidence           # high / low；现行主要按有无 occurred_at 标记，不替代时间解析
model
effort
usage                     # Option<TokenUsage>：input / output / cache_creation / cache_read
content                   # full 档提取的可见正文；metadata 档为 None
parent_ref                # 父事件引用预留槽位，现有解析器为 None；会话血缘另见 §2.2
content_hash              # 快照 SHA-256，用于变更与版本证据
artifact_kind             # 快照类别，对应 catalog::ArtifactKind；append_log 为 None
observed_at               # 快照观测时刻，Unix 秒字符串，不冒充 occurred_at
modified_at               # 快照 mtime，Unix 秒字符串；问不出为 None
message_id                # Claude 来源适用时
request_id                # Claude 来源适用时
```

`RawEvent::dedup_key()` 仍返回 `(source_type, source_location, source_path, source_session_id, seq)` 的字符串，但 `seq` 会随解析器产出组成改变；**跨解析器升级的稳定事件引用使用来源定位、事件类型与 `event_key`**。键缺失表示 v1 遗留或非 append-log 快照，不能补猜成稳定身份。源记录被改写时指纹也会变化，不承诺跨任意上游改写仍是同一条记录。

`content_hash` 是快照内容证据，不是所有事件的全局唯一键；总库还区分来源版本与投影版本，不能拿 DTO 的旧去重串代替存储身份。投影替换与回退见 [scan-state-model.md](scan-state-model.md)。`ingested_at` 与总库 `offset` 由总库层附加，不由解析内核产出。

> **粒度对账（见 `rawevent-reconciliation.md`）**：QuotaBar 只在 **usage 事件**（Claude `type=assistant` 带 usage；Codex `token_count`）产 fact，**用户提问 / 工具结果 / 纯文本助手消息不成事件**，且正文只在即时 transcript 路径上、不落库。本契约把即时正文解析升级为按事件类型持久化的 `RawEvent`；一条源 JSONL 记录可产多个事件。`seq` 是产出序而非物理行号，增量从游标 `next_seq` 延续；稳定引用另用 `event_key`。

> **thinking / reasoning 建模（闭合 `rawevent-reconciliation.md` §3.3 的取舍）**：思考块（Claude `thinking`、Codex `reasoning.summary[].text`）统一记为 `actor=assistant` + `event_type=thinking`——**不另设 thinking actor**（QuotaBar 的 role=thinking 气泡归一到 actor/event_type 二维）。**opaque（明文不可得）**：Codex `encrypted_content` 等无明文场景，仍产 `thinking` 事件但 `content=None`，表示"推理发生过、无正文"，下游据此区分明文思考与加密思考。黄金语料 §11 已含该用例。

### 7.1 时间保真度与两类几何（time fidelity）

时间是记忆系统的命根子——冲突裁决（latest-wins）、可复现评测、**行为归因**都靠它，且**不止对话与 memory 需要时间**：规则 / 配置 / 技能 / 计划 / 用量等控制面与状态数据同样要时间（解释"agent 当时为什么这么做"）。但**不同来源能提供的时间精度不同**，内核必须**诚实标注、不伪造精度**。记忆层完整时间模型见 TumeFlow **ADR-029**（本节是其在内核侧的落点；三类时钟与 latest-wins 基础见 ADR-020）。

**两类时间几何**：

- **点事件（point event）**：`append_log` 的 message / usage / tool_use / thinking——单点 `occurred_at` + `time_confidence`。
- **状态制品（stateful artifact）**：`snapshot_file` / `sqlite_store` 的 instruction / rules / config / skill / memory / task——语义上是**有效期区间**而非点。内核只产**变更点**（`config_snapshot` 事件，带 `content_hash` + 检测到变更的时间）；把变更点序列折叠成 `[valid_from, valid_until)` 区间、做 supersedes 与 as-of 重建，是 **TumeFlow 物化层**的职责（ADR-029），内核不算区间。

**保真度按 `source_mode` 分档**：

| source_mode     | 时间信号                                                                | 保真度                                                               |
| --------------- | ----------------------------------------------------------------------- | -------------------------------------------------------------------- |
| `append_log`    | 每行 `occurred_at`（Claude 顶层 `timestamp` / Codex `value.timestamp`） | 高（精确点）                                                         |
| `sqlite_store`  | 行级时间戳（有则用）+ `rowid`/`wal_lsn` 顺序                            | 中                                                                   |
| `snapshot_file` | `content_hash` 判变，`observed_at` 记观测、`modified_at` 另记 mtime     | 低；真实有效期由下游推导，RawEvent 没有 `valid_from_confidence` 字段 |
| `opaque_family` | 仅 `observed_at`（何时见到）                                            | 仅 provenance，不进语义时间                                          |

**两条铁律**：

- **文件 mtime 永不作语义时间**：它只用于增量 / 回退检测（§8）；copy / sync / `touch` / WSL 9P 都会搅乱甚至令其倒退，拿来当事件时间必错。
- **缺失 / 不可信不补猜**：点事件缺时间为 `None` / `low`，快照不伪造 `occurred_at`；下游推导有效期时另标不确定性。DTO 保留原始时间串，本仓已有 `occurred_at_unix_ms` 归一为 **UTC 毫秒**供总库排序 / 查询使用，不能按原始字符串字典序比较；无法解析返回 `None`，不能改用入库时刻冒充。

## 8. 无状态游标 API

当前 `ScanResult` / `Cursor` 字段摘要如下，完整类型见 [`src/cursor.rs`](../src/cursor.rs)；写入总库时还须使用扫描观察与提交规划，不能只凭压缩后的三态状态决定，见 [scan-state-model.md](scan-state-model.md)。

```text
ScanResult {
  events: [RawEvent]
  cursor_out: Cursor
  status: ok | partial | error
  report: SourceReport
}

Cursor {
  kind                     # byte_offset | fingerprint | sqlite_rowid | no_cursor
  safe_offset              # u64，已安全消费的字节
  size                     # u64，上轮逻辑文件大小
  mtime                    # Option<i64>
  content_hash             # Option<String>，快照指纹
  last_rowid               # Option<i64>，SQLite 预留
  schema_fingerprint       # Option<String>，SQLite 预留
  codex_state? {
    current_model, current_effort, current_cwd, current_session_id
    previous_total { input, cached, output }
  }
  next_seq                 # 下批事件产出序号；回退重读归零
}
```

游标当前是带 `kind` 的结构体，各形态读取相应字段；SQLite 的字段预留不表示对应读取器已实现。来源路径由调用方在游标外关联。

推进规则（与 QuotaBar 实机验证一致）：

1. **append_log** — 只解析**完整 JSONL 行**；末行半截写入本轮不解析、不推进 `safe_offset`。
2. **append_log 坏 JSON** — 本轮所读尾批中**任一完整行**解析失败,按**有无传入游标**(`cursor_in`)分两路：
   - **增量扫描**（`cursor_in=Some`，常态轮询）→ **冻结整批**：`safe_offset` 保持本轮起点、`status=error`、**本轮不发事件**，下轮重读整段尾（与 QuotaBar 实证一致，见 `rawevent-reconciliation.md` §2："一坏行冻结整批尾"）。append-only 完整行不可变、retry 对永久损坏无效，这是"宁可重读、不静默跳过/错解"的保守取舍。
   - **一次性全扫**（`cursor_in=None`，影子对账 / 总库首扫）→ **保留好行事件**：没有"下一轮"可重读，丢弃只会平白少数据（一行坏 = 整文件 N 条全丢，比 native `parse_lines` 还差），故跳过坏行、保留前后所有好行，`status=partial`（与"半行 pending"同档：带事件、非 Ok），供上层按需降级而非整文件作废；`safe_offset` 推进到完整行边界（游标推进对一次性调用无意义，调用方丢弃 `cursor_out`）。
     **已知代价**（仅增量路径）：永久损坏的完整行会让该来源停在原地，将来可在游标加 retry 计数做"毒行"跳过（后续阶段；QuotaBar `ROADMAP §E.5.5` 已记此 backlog）。（半截尾行属规则 1 的 pending，不在此列。）
3. `(mtime, size)` 未变且游标已到尾（byte_offset 满足 `safe_offset >= size`；fingerprint 满足 `content_hash` 未变）时跳过该文件。
4. **append_log 截断 / 重写 / 压缩检测**（关键）：判据以 **`(mtime, size)` 回退**为准（QuotaBar 实证：缓存 `mtime > 当前` 或 `size <` 缓存即触发 `safe_offset` 归零、全量重建；WSL 侧 `size < known` → Full 重读）。压缩场景如 Codex `history.jsonl` 超 `max_bytes` 丢最旧。总库按 `Rollback` / `Reparse` 等提交计划维护来源版本与当前投影，不能只按时间 / 消息 id 合并。写入观察路径还可携带源指纹识别原地改写，见 [scan-state-model.md](scan-state-model.md)；通用 `scan()` 与带指纹观察路径的能力不可混为一谈。
5. **snapshot_file** — 比 `content_hash`；变了产一条 `config_snapshot` 事件（带新旧哈希），未变跳过。整体重写是常态，不用字节游标。
6. **sqlite_store（预留设计）** — 将来先校 schema、再按表级游标增量取，不按字节读；当前不能视为已有可调用读取路径。
7. **游标由调用方持久化**：QuotaBar 存自己的 `cache.db`，TumeFlow 存自己的库。内核本身不落盘——不同消费者的读取进度互不干扰，无锁竞争。

## 9. 标准化接口汇总

当前 Rust 库与 CLI 复用摄取实现，接口粒度不同；PyO3 尚未交付。以下对应 [`src/lib.rs`](../src/lib.rs)：

| 库接口                                            | 入参 / 结果                                                                   | 用途                                                     |
| ------------------------------------------------- | ----------------------------------------------------------------------------- | -------------------------------------------------------- |
| `catalog()`                                       | 无入参，返回 `Vec<ProviderDescriptor>`                                        | 当前内置描述符；配置根读取环境变量，尚未接 `user_config` |
| `discover()` / `discover_local()`                 | 无入参，返回 `Result<Vec<SourceRef>>`                                         | 全来源发现 / 仅本地发现，不读正文                        |
| `discover_transcripts()` / `discover_snapshots()` | 无入参，返回相应来源清单                                                      | 分开会话与状态制品                                       |
| `discover_transcripts_reported(deadline)`         | 返回 `DiscoveryOutcome`                                                       | 显式报告哪些位置没问成；依发现结果清存量的调用方须用它   |
| `discover_project_snapshots(roots, deadline)`     | 已确认的项目根与预算，返回 `ProjectSnapshotOutcome`                           | 项目指令快照发现，保留无法探测的状态                     |
| `scan(source, cursor_in, profile, roots)`         | `&SourceRef`、`Option<Cursor>`、`Profile`、`Arc<RootRegistry>` → `ScanResult` | 单来源扫描；空注册表表示无法归属，不退回 cwd 猜根        |

- 一轮编排由 CLI `svault scan-all --profile metadata|full` 提供；没有同名公开 `scan_all(user_config, ...)` 库函数，也没有 `svault scan --source ...` 子命令。
- `svault discover` 输出来源 NDJSON。`scan-all` 默认将事件、每来源报告与汇总都写到 stdout NDJSON，诊断写 stderr；带 `--write-store` 时不再逐条输出事件，见 §13.1.1。
- 总库读取、快照、项目归属与擦除等 CLI 另见 [README](../README.md) 及对应主题文档；是否已进正式发布以 [backlog.md](backlog.md) B 节为准。

## 10. 扫描报告（Scan Report）与 GUI

内核**无头**，但每轮扫描产出结构化报告，宿主据此渲染"扫了哪些文件、各是什么情况"。报告即 GUI 的数据契约：

当前库结构摘要（[`src/report.rs`](../src/report.rs)）；CLI 外层另包装状态、来源标识与 `kind`，不能把下列结构直接当作每条命令的完整线格式：

```text
SourceReport {
  source_path
  source_mode?, cursor_kind?
  items_examined, events_emitted, items_skipped
  fingerprint_changed, schema_changed, rollback_detected
  bytes_read, pending_tail_bytes
  warnings: [String]
}
ProviderReport {
  provider, sources_scanned, events_emitted, errors
  sources: [SourceReport]
}
ScanReport {
  providers: [ProviderReport]
  total_events, total_errors
}
```

`schema_changed` 等字段保留扩展空间，不表示 SQLite 读取已交付；当前报告没有旧草案中的 `roots_probed`、授权状态或 `started_at` / `finished_at` 字段。

**GUI 放在宿主，不放内核**：

- QuotaBar 已是 Tauri 桌面应用，是这个扫描状态界面的**天然落点**——直接把 `ScanReport` 渲染成"provider → 根 → 文件 → 新字节/事件数/状态"的表，警告单列。
- TumeFlow 侧用 CLI 表格或简单 web 视图渲染同一份 `ScanReport` 即可，无需重做采集逻辑。
- 这样"一个简单 GUI 显示扫描了什么、情况如何"的需求由**共享的报告 schema + 各自的渲染**满足，仍是"代码一份"（采集一份、报告结构一份），渲染按产品风格各做各的。

建议宿主界面展示已可取得的来源、读取字节 / 事件数及告警，无法探测与确认不在须分开。授权清单、任意根设置及 provider 启停仍需 §5 的控制面设计，不能承诺当前报告已经提供这些字段。

> **`ScanReport`（机器结果）≠ 日志（诊断流水账）**：前者确定性、`return`/序列化给宿主渲染、可进黄金语料断言；后者走 stderr、含 `run_id`/时间等易变量、给人排错。日志规范见 `LOGGING.md`（对齐 QuotaBar `docs/LOGGING.md`：lib 用 `log` 复用宿主 sink、stdout=NDJSON/stderr=日志、正文不进日志；决策见 TumeFlow ADR-026）。

## 11. 黄金 fixture 语料（一致性套件，必须覆盖）

以下保留一致性用例目标，并非每一条均已有测试。当前已实现路径升级时须跑相关测试；父子树 / 压缩边界、任意配置根、SQLite 等未交付能力的用例是将来开工门槛，不算本轮已验证结果。过时字段名以 §7–§10 的现行结构为准。

- Codex 累计 token 跨多次 append 的 delta 正确性：`last_token_usage` 存在时直接取、缺失时 `total_token_usage − previous_total{input,cached,output}`。
- Codex `cached = min(delta.cached, delta.input)`、`input_tokens = delta.input − cached` 的拆分正确。
- 一文件多 session：Claude `--resume`/fork 把父 `sessionId` 行重放进子文件、Codex 多个 `session_meta` → 按**行级** `session_id` 归属，不串话。
- thinking/reasoning：Claude `thinking` 块、Codex `reasoning.summary[].text` → 按取舍策略产 `thinking` 事件或排除；Codex `encrypted_content` 无明文 → 不产正文（opaque）。
- 末行半截写入 → 本轮不解析、`safe_offset` 不前进。
- 单行坏 JSON（完整行）→ 按游标分两路（见 §8 规则 2），各一条 fixture：**增量**（`cursor_in=Some`）**冻结整批**（`status=error`、`safe_offset` 不前进、本轮不发事件、下轮重读整段尾）；**一次性全扫**（`cursor_in=None`）**保留好行事件**（`status=partial`、跳过坏行、`safe_offset` 推进到完整行边界）。
- `(mtime, size)` 回退 / 文件被截断 → 归零重建。
- 同 `session_id` 在 `local` 与 `wsl:<distro>` 各一份 → 靠 `source_location` 区分，不互相覆盖。
- Claude `parentUuid` 分支 / 重试 / 编辑 → 父子树，区分采纳与废弃分支。
- Codex 会话中途 `cwd` 变化 → token 与项目归因正确。
- `cwd` 缺失 → `project_root_source=missing_cwd`（QuotaBar 实证取值）；WSL UNC `\\wsl.localhost\<distro>\…` → 规范化为 `wsl:<distro>:/path`。
- WSL 增量三态：`CacheHit`（size==known，零正文）/ `Append`（tail）/ `Full`（首次或截断）→ 由 `(size, known)` 决定，与本地字节游标等价。
- `workspace_location` ≠ `source_location`：WSL 项目记在 local transcript 下时，前者 `wsl:<distro>`、后者 `local`，不混淆。
- 迟到入库：旧 `occurred_at` 事件以大 `offset` 追加 → 时间线按 `occurred_at` 重建，`offset` 不当时间用（latest-wins 不被冒充）。
- `occurred_at` 缺失 / 不可信 → `time_confidence=low`，不默认当"现在"。
- 项目归属按当前注册表判据验证已归属、未归属与无法判定；不再使用旧 `cwd` 兜底或数值 confidence 草案。
- 文件轮转 / 新会话文件出现 → 增量发现。
- 超大文件：只读 `safe_offset` 后新字节（尤其 WSL 9P，避免整文件重读）。
- 来源发现：内置默认 / 用户配置 / 环境变量三种路径来源的优先级与去重。
- 新 provider 描述符（如占位的 Cursor SQLite）→ 框架可发现、`planned` 不默认扫。
- `ScanReport` 字段：路径不存在、权限拒绝、未授权各产出对应 warning。
- **snapshot_file 变更**：CLAUDE.md / AGENTS.md / rules / hooks 整体重写 → 仅当 `content_hash` 变才产一条 `config_snapshot`（带新旧哈希），未变跳过、不产事件。
- **history 压缩/重写**：`history.jsonl` 超 `max_bytes` 丢最旧并缩文件 → 指纹回退检测触发，重算尾部窗口，按 `(occurred_at, content_hash, message_id)` 去重，旧尾不被当新事件。
- **sqlite_store**：SQLite 状态库按 `last_rowid` 增量；`schema_fingerprint` 漂移 → 显式失败而非静默错解；**不按字节读**。
- **derived-path**：先读 `CODEX_SQLITE_HOME` / `CLAUDE_CODE_TASK_LIST_ID` / profile 选择 → 算出真实根 → 派生子来源；env 缺省时回退默认根。
- **opaque_family 占位**：`planned` 的保留来源族（Claude bg state / Codex plugin bundle）框架可登记、默认不扫、不写死 glob。
- **auth 排除**：Codex `auth.json` / 私钥永不进 `RawEvent`，仅命中安全排除规则。

## 12. 分发与版本

**独立仓库 SessionVault**（crate `session-vault`，CLI `svault`）。交付分三层，**按优先级**排，决策见 `DECISIONS.md` ADR-024：

| 形态                         | 服务谁                             | 调用方式           | 优先级               |
| ---------------------------- | ---------------------------------- | ------------------ | -------------------- |
| **CLI `svault`（NDJSON）**   | 任何语言（Python / Node / Go / …） | 子进程 + stdout 流 | **P0 主交付**        |
| **Rust lib `session-vault`** | QuotaBar（原生）                   | cargo 依赖，进程内 | **P0**（同一份代码） |
| **PyO3 wheel（pip）**        | 仅 Python                          | `import`，进程内   | 后置，实测需要再上   |

- **CLI 是跨语言主交付，不是 pip**：内核中立，交付也中立。一个 `svault` 二进制服务所有语言；pip 只解决 Python 一种消费者，却要背 `manylinux × macOS × Windows × 多 Python 版本` 的 wheel 构建矩阵。
- **CLI 优先的四个理由**：(1) 一个产物服务所有消费者；(2) 给定二进制 = 给定确定内核版本，和"总库 offset + schema 版本"复现戳天然对齐，钉版最干净；(3) 扫描器 panic 只挂子进程，不波及宿主（PyO3 同进程一崩全崩）；(4) 扫描是 I/O 密集批量摄取、非高频小调用热循环，子进程开销被大块读摊薄，且 NDJSON 流贴合 `scan_all`。
- **pip（PyO3）何时才上**：仅当**实测**出现高频细粒度（逐事件）调用、大正文序列化成热点、或需给第三方干净 `import` API 时，用 `maturin` 把同一份 crate 包成预编译 wheel 传 PyPI——是锦上添花，不替代 CLI。
- **预编译**：当前发布脚本构建 Windows / Linux CLI，见 [README 分发与版本](../README.md#分发与版本)；macOS 不在现行发布脚本覆盖范围。内嵌与独立发布的运行时分别核实际提交及所需子命令。
- **semver**：来源目录、`RawEvent`、`ScanReport`、游标都是公开 API；黄金语料是一致性套件，消费者升级前必须跑。
- **消费方钉版形状不同**（2026-11-05 按本仓 `AGENTS.md` 的消费者契约整理；具体已接入提交由各仓核验）：
  - **Rust 进程内库**：由消费方子模块指针决定编入的源码版本；当前库消费者启用 `store`。
  - **内嵌 CLI**：从消费方钉住的子模块构建，并冻结进分发包；只重打包、不更新指针，仍是旧版。TumeFlow 属此形状，旧文「不使用子模块、只钉发布物」不再成立。
  - **独立发布 CLI**：TumeChat 按本仓发布的 `svault` 消费只读出口，2026-10-09 起约定。
  - 本仓不替消费方更新指针；源码合并后主动通知，正式发布与消费方实际验收另记。

## 13. 集成形态：上游 RawEvent 总库 + 下游物化分库

解析内核本身不持久化状态（§8）。在它之上，本仓提供**可选持久化组件** `TotalStore`（`store` feature）：保存加密 RawEvent、来源投影与墓碑等元数据，下游各自物化领域视图。默认追加保留，用户擦除与旧代回收按显式判据执行；多机副本与来源主机就地证据两种使用方式的边界见 §2.2。历史决策背景见 ADR-020 / ADR-021。

### 13.1 总库（活、最新、中立）

- **内容**：存 RawEvent 与来源版本 / 投影 / 墓碑等存储元数据；不含产品领域表（`usage_facts` / 记忆都不入总库）。事件默认追加保存，当前投影替换、用户擦除及旧代回收须遵守各自契约。
- **归属**：SessionVault 仓库（中立），不归 QuotaBar，也不归 TumeFlow。
- **写者**：谁跑扫描谁写，同一时刻单写者；QuotaBar 常驻，天然当默认写者。开 WAL，读者不挡写。

  🔴 **「同一时刻单写者」由 SQLite 保证，不需要我们再加一把锁**（2026-08-20 落地）。
  多写者场景（QuotaBar 常驻扫描 + TumeFlow 按需同步）缺的从来不是**锁**，是两样：

  | 缺什么                                      | 后果                                                                                                             |
  | ------------------------------------------- | ---------------------------------------------------------------------------------------------------------------- |
  | `busy_timeout`（默认 **0**）                | 第二个写者**立刻**拿到 `SQLITE_BUSY`，而不是稍等一下 ⇒ 随机失败，失败点离原因很远                                |
  | `BEGIN IMMEDIATE`（rusqlite 默认 DEFERRED） | 事务里**先读后写**时读快照会过期 ⇒ `SQLITE_BUSY`，**而 `busy_timeout` 对这一种无能为力**（等下去快照也不会变新） |

  第二条不是理论：`gc_superseded_projections` 正是这个形状（先 `SELECT` 出候选、
  再在 `if !dry_run` 里写）。写事务规则收在 `store.rs::write_tx()`，具体调用点随源码核查，不固化历史数量。

  ⚠️ **不要在这上面再套一把文件锁。** 那会与 SQLite 自己的锁各管一半，而两套锁的
  边界不重合时**没有任何东西会报错**。**这条对 `scan-all --write-store` 同样成立。**

  🔴 **订正（2026-08-21）**：上一版这里还写着「也别把锁加在 `svault scan-all` 上 ——
  **它根本不写总库**」。`--write-store`（TumeFlow task #44）落地之后**那个理由过期了**
  —— 而结论没变，只是它现在靠的是上面那条（锁归 SQLite），不再靠「它不写库」。
  两者当时得出同一个答案，所以理由作废时没有任何东西会失败。
  **凡是「这样做是安全的，因为 X」，都要在 X 变了的时候回来重读一遍。**

  判据由两条测试各自钉住，**变异互不重叠**：去掉 `busy_timeout` 只红
  `two_independent_writers…`；退回 `DEFERRED` 只红 `a_read_then_write…`。
  那正是「两者缺一不可」该有的证据形状。

  写入入口包括 `scan-all --write-store`（Class-A，见 §13.1.1）、`sync-snapshots`（Class-B）、`erase`、归属重算与直连库的宿主等；不能把历史列举当成完整写者白名单。

#### 13.1.1 Class-A 的写入口：`svault scan-all --write-store`

没有这个口的时候，把 `RawEvent` 写进总库**只能经 `TotalStore` 这个 Rust 库 API**，
于是只有能直链 crate 的消费者做得到 —— 实际上只有 QuotaBar。后果不是「不方便」：
它把「证据什么时候刷新」绑在「哪个宿主在跑」上，只装 TumeFlow + TumeChat 的机器上
**总库没人写 ⇒ Class-A 恒为 0，而界面上什么都不说**。这与 `sync-snapshots`
（Class-B 的写入口）是同一件事的另一半。

```bash
svault scan-all --write-store [--store <path>] [--state <path>] [--only <type>]...
```

| 事              | 怎么定的                                                                                                                                                                                                                                                                       |
| --------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| profile         | 默认 `full`（§13.1「总库以 full 物化」）。**显式 `--profile metadata` 直接拒绝**：去重键 `(type, location, path, session_id, seq)` **不含正文**，metadata 事件的 seq 与 full 逐一相同 ⇒ 先写进去就把之后的正文永久挡在 `INSERT OR IGNORE` 外面，而总库 append-only、**不可逆** |
| 投影模式        | 交给 `scan_plan::CommitPlan`，**不在 CLI 里重算**（见下）                                                                                                                                                                                                                      |
| 事件流          | 带 `--write-store` 时**不再逐条吐 `event`**（一次全量是几个 GB 的 stdout，而写库的调用方本来就要从库里读）。观测走每来源一条 `store_write` / `store_held` / `store_write_failed` + 收尾 `summary`                                                                              |
| 只写 append-log | 快照来源**不由这条路写** —— Class-B 归 `sync-snapshots`（另一套枚举 + 另一套变更检测）。在这里再写一遍就是第二条 Class-B 写路径。摘要里报 `snapshot_sources`，因为「这轮写了 0 条快照」不该被读成「本机没有快照来源」                                                          |
| 退出码          | `0` 正常；`1` 起步就没跑成（参数/发现/开库）；`2` 游标没落盘；`3` 有来源没写进库（本轮**不完整**）。`held_sources` **不进退出码** —— 那不是失败，是计划里的一格                                                                                                                |
| 范围            | `--only <source-type>`（可重复，取值同 `SourceType`）。**不给 = 扫全部**，既有行为逐字不变。过滤发生在**发现之后**（`retain_only`），因为只有那一处看得见全部发现路径的产物                                                                                                    |

🔴 **`--only` 存在是因为「靠环境变量做隔离」不成立。** 调用方想把一轮扫描限制在
自己的数据上时，最自然的做法是把各 provider 的根环境变量指到别处
（`CLAUDE_CONFIG_DIR` / `CODEX_HOME`）。那**只覆盖宿主本机的根**，而 **WSL 发现是
第三条路**，不受它们管。实测（2026-08-24，消费者侧）：两个变量都设对了，扫描照样
把宿主真实的 WSL 会话（上千条事件）读进了本该只装评测数据的库。

**一个只挡住两条路、而实际有三条的护栏，与没有护栏长得一模一样。** 所以过滤放在
**发现之后**：那一处 WSL 来源与本机来源没有区别，隔离因此是**结构性**的，
不依赖调用方枚举得全。过滤命中数走 `log::info`（`--only … kept N of M sources`）——
一个静默过滤的扫描，「隔离生效」与「这台机器上什么都没有」长得一模一样。

🔴 **它走 `scan_append_log_observed`，不走 `scan_source`。** 后者是有损投影
（`ScanStatus` 三个变体压掉了 `ParseQuality` 的四种含义），而写库要用的恰好是被压掉的
那些：「文件消失」要整个跳过、「主动拒绝坏行」要冻游标、「降级到一个好行都没剩」
不能整代替换。`scan_source` 还硬编码 `prior_fingerprint = None`，于是**同尺寸原地重写
从来检不出来** —— CLI 把 `(hash, covered_len)` 存进自己的状态文件再传回去，那条路才通。

- **版本**：一直跟随最新内核往前走，方便 QuotaBar 直接读用。
- **正文**：以 `full` 物化（含正文）。QuotaBar 已放开"不读正文"限制（ADR-021），故总库可承载正文供两边使用。
- **保留**：默认追加、不过期，当前未做入库前压缩；用户主动擦除与受保护的旧代回收是例外。下游重建须遵守当前投影和墓碑，不承诺已擦除正文可按旧 offset 恢复；体积事项见 [storage-growth.md](storage-growth.md)。
- **时间**：每条记录 `occurred_at`（对话内时间，冲突裁决权威）与 `ingested_at`（入库时间，溯源）；`offset` 仅作同步游标，**不代表**时间先后（迟到入库的旧事件 offset 大但 `occurred_at` 旧）。

### 13.2 TumeFlow 分库（物化、固化、可复现）

- TumeFlow **不直接读活总库**，而是按自己的节奏、钉自己的 `RawEvent` 版本，从总库**增量拉取**并物化成自己的领域分库（Episode / 证据 / 记忆），两次同步之间冻结。
- 同步用一个**指向总库的游标**（拉到哪条 RawEvent），幂等、可断点续。
- 历史设计的**复现戳**为 `总库 offset + RawEvent schema 版本`；多库还须区分 `store_id`，事件引用须考虑稳定身份与投影版本。
- **offset 不冻结内容视图**：重解析会换当前投影，擦除会删除正文；给定旧 offset 不能保证永远重建同一份内容，也不能绕过墓碑恢复已删数据。
- **两条同步路径**：日常增量与全量重建。按 offset 拉取事件外，下游还要消费投影变更及擦除记录；重建按当时可读的投影与删除状态执行，不能只重放旧事件流。
- **时间敏感**：记忆冲突裁决（latest-wins，如用户后来更新规则以最新为准）按事件 `occurred_at`，**不按入库顺序 / offset**；详见 `DECISIONS.md` ADR-020 时间语义。

#### 13.2.1 具体命令：`svault pull --since`（P3-③，已落地生产侧）

增量拉取的**跨语言入口**是 `svault pull`（`store` feature 门控）。它通过分页读接口输出带 `offset` 的事件流，**只读打开**总库，不建库、不迁移；游标由调用方持久化。每个库单独维护进度，不能把它当成 `--all-stores` 合读或跨主机 RPC。

```bash
svault pull [--since <offset>] [--limit <N>] [--store <path>] [--projection all|current]
```

| 参数      | 默认                                     | 含义                                                                               |
| --------- | ---------------------------------------- | ---------------------------------------------------------------------------------- |
| `--since` | `0`                                      | 只拉 offset **严格大于**此值的事件；首次全量同步传 `0`，之后传上轮 `last_offset`。 |
| `--limit` | `0`（不限）                              | 本轮最多吐多少条，把大回填切成有界批次；`0` = 一次拉到追平总库尾。                 |
| `--store` | `<data_local_dir>/svault/total_store.db` | 总库路径，**与 QuotaBar 写者同址**（宿主集成时通常显式传）。                       |

`--projection` 默认 `all`，保留既有事件流语义；`current` 只读当前投影。只拉当前事件仍不能替代 `changes` 的来源替换通知与 `erasures` 的删除消费。

**stdout NDJSON 示意**（每行一对象，按 `kind` 分流；摘要另带 `store_id` 等当前字段，以命令输出契约为准）：

```jsonc
// 每条事件——offset 是消费者持久化的游标 token
{"kind":"pulled","offset":42,"event":{ /* 整条 RawEvent */ }}
// 收尾摘要——消费者据 last_offset 推进游标、据 caught_up 决定是否再拉
{"kind":"pull_summary","since":10,"last_offset":42,"events":5,
 "store_max_offset":42,"caught_up":true}
```

- **`offset` 单调递增**；被墓碑覆盖的事件不出流，擦除记录另由 `erasures` 消费。分页进度包含已扫描但解码失败的行，不能把一个全坏行窗口的空事件误判成追平。
- **`caught_up`**：`true` = 已读尽 `since` 之后的可读事件；`false` = 仅因 `--limit` 截断（还有），消费者应带新 `last_offset` 再拉一轮。
- **退出码**：`0` 正常（含「无新事件」）；`1` 定位/打开/读取失败（库不存在 = 宿主还没扫过一轮）。
- **坏行容忍**：单条 `event_json` 反序列化失败（schema drift）在 `read_since` 内 skip+warn，不中断整条增量同步（与 `read_session` 的「显式暴露 skipped」策略相反——pull 是流式同步、不能因一行断流）。

> **消费侧（P3-③）🟡**：拉取环已通——TumeFlow `sources/svault.py` 子进程调本命令、解析 NDJSON 流、持久化游标（端到端实测，含 CJK 正文 + WSL location + 增量）。**下游物化进分库（RawEvent→Episode）在 TumeFlow 侧、已落地**（`tumeflow/materialize/episode.py` + 候选提取，见 TumeFlow ROADMAP Phase 2）——非 SessionVault 待办。生产侧契约已由本节 + bin 单测（`pull_ndjson_wire_shape_is_stable` 等）锁定。

### 13.3 为什么同时满足"新"和"稳"

- 常驻写者推进来源投影，读者使用自己支持的读取契约；持有旧副本、来源暂不可达或本轮写入失败时不能宣称数据必然最新。
- 下游可暂停主动同步以冻结自己的领域视图，但这不保证旧读者能打开任意新版库。schema / 值域变化仍须验证旧读者与并存写者兼容，缺表列的只读路径显式报 `SchemaBehind`，不能静默当空库。

### 13.4 独立性退路（铁律）

TumeFlow 运行时不依赖 QuotaBar 是否存在：

- 有总库 → 从总库拉。
- 无总库（用户只装 TumeFlow）→ TumeFlow 自己跑内核扫盘填分库。

两条路最终都得到分库（与 ADR-001 独立性一致）。

### 13.5 固化管理（GUI 四件套）

本节保留 ADR-020 的下游界面设计建议，当前产品进度由消费方维护。全量重建须遵守当前投影与擦除状态，不能以「总库永不删」为前提；同步、重建、状态与冻结入口由宿主渲染（历史设计见 `INTEGRATION.md` §12）：

- **自动后台增量同步**（默认开，跟 Dream 节奏）——保持"新"，无需按钮。
- **"重建记忆库"按钮**（带确认 + 进度）——落后过多 / 版本升级 / 修复时的底座操作。
- **状态行**——显示"已固化至 offset / 时间、落后总库 N 条、上次固化时间"（数据取自同步游标 + `ScanReport`）。
- **"锁定版本"开关**——评测 / 开发期暂停自动同步、冻住分库，满足"钉固定版本"诉求。

### 13.6 隐私 / 删除 / 加密（ADR-027 C5 技术安全门已落地）

总库默认以 `full` 保存正文，默认保留与主动擦除须同时成立。当前实现与后续产品目标分开：

- **已实现的正文加密**：`sv2` 分组数据密钥、旧库迁移及缺钥匙硬失败，详见 §15 的历史实现记录和 [README](../README.md#主密钥从哪来)。明文索引列仍包含路径、会话 id 等元数据；不把正文加密说成全部元数据均加密。
- **已实现的用户擦除**：`svault erase` 支持 session / source-path / project-root 作用域，同事务写无正文墓碑、物理删除事件并回收孤立数据密钥；后续写入检查墓碑防复活。`erasures` / `adopt-erasures` 提供跨库删除消费，不自动删除上游日志或下游缓存。
- **来源范围**：当前按已声明的 artifact 发现，不能把配置根中的任意文件当正文摄取；来源范围白名单不是任意目录授权系统，也不保证正文中的秘密已被识别或脱敏。
- **尚待产品化的控制面**：任意来源授权清单、整库导出 / 销毁流程及按来源正文开关仍是设计目标，不能据旧草案视为 CLI 已有能力。`scan-all --write-store` 当前明确拒绝 `metadata`，不能拿它模拟临时排除。
- **新增宿主 / 临时模式**：先补 §2.2 的可信属性、排除与多写者兼容契约；执行授权和记忆筛选归上层。

> 历史决策背景见外仓 ADR-027；旧文中的产品发布限制是当时状态，不能据此阻断本仓当前交付。本仓已正式发布 `svault`，具体发布记录见 [backlog.md](backlog.md) B 节；消费方的独立发布约束由各仓维护。

## 14. 非目标

- ❌ 不脱敏（TumeFlow 在物化分库时做）。
- ❌ **解析内核**不写库、不落盘游标、不存配置（消费者负责）；可选的 RawEvent 总库是同仓库内的独立持久化组件，只 append RawEvent、不碰领域表（见 §13）。
- ❌ 不自带 GUI（只产出 `ScanReport`，宿主渲染）。
- ❌ 不调 LLM、不联网。
- ❌ 不做 `usage_facts` / 记忆等下游投影（消费者各自投影）。
- ❌ 不把 `planned` provider 的候选路径当稳定接口（须探测 + 验证）。
- ❌ **解析内核**不持久任何状态——纯函数 `(目录 + 配置 + 游标) → (事件 + 新游标 + 报告)`；持久化是总库组件的事，与解析分层。

## 15. 历史落地计划（从 QuotaBar 抽取）

以下保留抽取阶段及后续按日期补记的记录；P0–P3 状态、消费方分支、soak / CI / 接入结论均按当时理解，本轮未复测，不作为今天的交付清单。本仓当前任务以 [backlog.md](backlog.md) 为准，跨仓进度回各仓维护。

SessionVault 起步于**抽取 QuotaBar 已实机验证的扫描器**，以绞杀者迁移保持原有功能；这是当时的最高优先级，历史决策见 ADR-022。

**核心原则：一个扫描器，总库是它的输出。** 不要先用新代码建总库、却让 QuotaBar 继续跑旧扫描器——那会变成同机两个扫描器（正是本项目要消灭的重复）。先让 QuotaBar 改用共享扫描器，总库只是其额外输出。

**QuotaBar 源抽取对照**（→ `session-vault`，✅ = 已抽取并实测）：

- ✅ `session_index.rs`（会话索引/扫描主体）→ `scan.rs` / `discover.rs` / `project_root.rs`
- ✅ `jsonl_cache.rs`（增量 `safe_offset`）→ `scan.rs` 游标逻辑 + `cursor.rs`
- ✅ `providers/claude.rs`、`providers/codex.rs`（Codex 累计 token）→ `parser.rs`
- ✅ `wsl/mod.rs`（WSL 桥）→ `wsl.rs`；`paths.rs`（路径发现）→ `pathnorm.rs` + `catalog.rs`
- ⬜ `session_transcript.rs`（正文/线程重建的更深部分）——`full` profile 正文已通，线程/`parent_ref` 重建属 greenfield

**绞杀者迁移五步**（保住成熟功能）：

1. 冻结 QuotaBar 当前输出为**黄金语料**（零运行时改动，纯安全网 + 规格）。**✅ 已完成**：冻结
   `cache.db` 的 `usage_facts`（9134 行）为基线 + 写 [parity-contract.md](parity-contract.md) 对账契约 +
   建 `parity` diff 工具，首测 9134 条对齐 fact **must-match=0**（计费字段字节级一致，含 Codex 累计 token）。
2. **抽取而非重写**，crate 输出 `RawEvent`。
3. QuotaBar 在**现有接口后**调用，`agent_sessions` / `usage_facts` schema 与 UI 不变。
4. **影子并跑**：新 crate 与老扫描器并行，输出 diff 老 `cache.db`，一致才切；切换走 feature flag，老路径留回退。
5. 信任后再把**总库作为输出**打开，TumeFlow 才开始消费；QuotaBar 用 submodule pin 控更新。

**风险点**（切换前必须有 fixture）：Codex 累计 token parser state、WSL 桥、增量 `safe_offset`——对应 QuotaBar `SESSION_MEMORY_ARCHITECTURE.md` §13 回归风险。

**优先级与 P0 硬边界**（防止 P0 被总库/snapshot/sqlite/插件/隐私一起拖大）。**进度（截至 2026-06-14）**：

- **P0（窄而硬）✅ 基本完成**：`RawEvent` v1 字段已定稿；`append_log` JSONL 与 Codex 累计 token 黄金语料已落地。`snapshot_file` 于 2026-07-13 从预留升级为 experimental：新增 `content_hash/artifact_kind/observed_at`，mtime 不冒充语义时间（2026-08-29 补 `modified_at`：mtime **另立一个字段**，仍不进 `occurred_at` —— 见上文 `snapshot_file` 那条）。`sqlite_store` / `opaque_family` / derived-path 仍按设计返回 `planned` / `skipped`。
- **P1 ✅ 基本完成**：Local Claude/Codex `append_log` 解析器已实装并过语料（`parser.rs` / `scan.rs` / `discover.rs` / `cursor.rs`，`svault scan-all` 已能吐 `RawEvent` NDJSON 流）。**游标已持久化**：`scan-all --state <file>`（默认 `<data_local_dir>/svault/cursors.json`，`--stateless` 关）跨运行续扫——实测二次扫描 23476→**0** 事件（全 cache-hit），Codex 累计 token 状态/`next_seq` 随游标存活。**路径规范化层已抽取并标准化**：`src/pathnorm.rs` 宿主感知（`HostPlatform`），UNC↔规范形互转、`workspace_location` 已接线产出（修掉了 QuotaBar「裸 `/abs` 一律当 WSL」的 Windows 宿主假设——Linux 原生跑时同路径正确判 `local`）。**WSL 访问桥已通（端到端实测）**：`src/wsl.rs`（移植 QuotaBar `wsl/mod.rs`）——纯逻辑（发行版名/UTF-16LE/`find -print0` 解析、`default_distro` 选择，跨平台单测）+ 实时层（`#[cfg(windows)]` spawn `wsl.exe`：`list_distros`/`list_jsonl_under_home`/`stat`/`read_range`/`read_file_at`，非 Windows 给桩）。`discover()` 枚举 `Wsl(distro)` 来源；扫描经 **`ByteSource` 抽象**（`scan.rs`）让本地（`File`/`Seek`）与 WSL（`wsl.exe stat` + `tail -c +K | head -c N`）**共用同一份**游标/回退/坏行冻结逻辑；`default_distro` 用来源自身发行版把裸 Linux cwd 打成精确 `wsl:<distro>`。**实测（2026-06-14，真实本机数据，metadata profile 无正文）**：48 来源（19 local + 29 WSL）→ 23373 事件，其中 **8102 来自 WSL**（Ubuntu-24.04 codex 7726 + Ubuntu-OpenClaw claude 376），**零 warning/error**。WSL 增量 tail（`read_range` start>0）由 env-gated 实机 IT（`SVAULT_WSL_IT=1`，`/tmp` 一次性文件）验证。**P1 剩余尾巴**：WSL 双位置真实**黄金语料** fixture（逻辑已全验，缺归档式回归 fixture）；snapshot/sqlite/其它 provider 属 P3 后续。
- **P2 🟡 进行中**：
  - **step 1 ✅**：冻结 QuotaBar `cache.db` 黄金基线、写对账契约（[parity-contract.md](parity-contract.md)）、
    建 `parity` diff 工具，**首测 9134 条 usage must-match=0**（advisory 全是 cwd 原始 vs 规范化的设计差异；
    活跃文件增长尾按字节边界归类）。
  - **step 2 🟡（QuotaBar `p2/svault-shadow` 分支，feature `svault_shadow` 默认关）**：SessionVault 经
    submodule + 可选依赖接进 QuotaBar；建 **seam**（`project_events`：`&[RawEvent]` 流 → `usage_facts` 适配器 +
    复用 QuotaBar `summarize_sessions` 产 `agent_sessions`，事件源抽象、总库落地后只换源）+ **影子 runner**
    （`run_shadow_diff`：`scan(full)` → 投影 → diff `cache.db` 两表 + 逐源/逐会话 drill-down 日志，只读不写）
    - **一键 debug 触发**（后端命令 `svault_shadow_diff` + QuotaBar Settings 的 dev 按钮「跑影子 diff」，按钮受
      `import.meta.env.DEV` 门控、生产构建剔除）。**首次在线影子实测（50 源真实数据）：`facts_must_mismatch=0`
      绿灯**（usage 主对账通过；§1/§6：绿灯只看 usage_facts，`session_diff=10` 的会话首尾时间戳漂移属 advisory、
      +2 facts 是本会话 transcript 实时增长）。GNU+MSVC 双工具链编过、默认构建零影响、`vite build` 验证 dev 码不进发版包。
  - **step 3 ✅（feature `svault_index`；step 4 已翻默认）**：真实索引**写库路径**走 seam（增量保真版）——
    `parse_source_file` 的解析步经 `dispatch_parse` 切到 `svault_bridge::parse_complete`（复用
    `session_vault::parse_lines` + `CodexParserState⇄CodexState` 桥接），字节游标/`safe_offset`/`base_seq`/
    坏行冻结/合并/summarize 全复用 QuotaBar 原逻辑，只换解析；出错回落原生。default/shadow/index/both
    四组合均编过；L3 桥接 index_tests CI 绿。
  - **step 4 🟡 切换完成、soak 中**：`Cargo.toml` `default = ["svault_index"]`（svault 写库成正式路径，
    `parse_native` 留作出错回退）；`release.yml` 补 submodule init、`ci.yml` 测三配置（默认/原生/诊断）
    且缺 `SUBMODULE_TOKEN` 时降级，**CI 全绿**。证据台账：翻默认前影子 svault-vs-原生 `mm=0` + L1/L2/L3。
    **soak 闸门**：写库路径端到端 = 原生构建 vs svault 默认构建各重索引、逐字段 diff `cache.db`（手动 GUI，
    Tauri exe 受限）。⚠ 翻默认后影子 diff 已退化为 svault-vs-svault，**不能**再当闸门。过闸 → **最终绞杀**
    （删 `parse_native` + `not(svault_index)` 分支 + ci.yml 原生回退步骤）。
- **P3 🟡 进行中**：
  - **step① IO 委托 ✅**（QuotaBar 侧）：QuotaBar `refresh_index` 改调 `discover()`/`scan()`，删自管 read/游标，`scan()` 成其唯一 IO+解析路径。
  - **P3-② 总库写入侧 ✅ C5 技术门完成**：`src/store.rs`（`store` feature）落地 `TotalStore`。`event_json` 仅以 AES-256-GCM `sv2` 信封落盘；OS keychain 主密钥包裹按来源类型/位置/路径/项目分组的随机数据密钥，事件身份与密钥分组均进 AAD；旧明文/`sv1` 库事务迁移后 `secure_delete + WAL truncate + VACUUM`；错误/缺失密钥硬失败。`svault erase` 写无正文墓碑、物理删除并回收孤立数据密钥，`append_events` 在写入前检查墓碑以阻止原始文件重扫复活。合成隔离跨进程 E2E 10/10 PASS。
  - **P3-③ TumeFlow 消费 🟡**：**生产侧 ✅** —— `svault pull --since <offset> [--limit N] [--store <path>]` 子命令（`src/bin/svault.rs`，`store` feature 门控）把 `read_since` 暴露成带 `offset` 的 NDJSON 流 + `pull_summary`（`last_offset`/`caught_up`），契约见 §13.2.1、线外形由 bin 单测锁定（`pull_ndjson_wire_shape_is_stable` / `pull_since_filters…` / `pull_limit_caps…`）。**消费侧拉取环 ✅** —— TumeFlow `sources/svault.py` 子进程调它、解析流、持久化游标（端到端实测，含 CJK 正文 + WSL location + 增量）；**下游物化分库（RawEvent→Episode）已在 TumeFlow 落地**（`materialize/episode.py` + 候选提取 + Dream，见 TumeFlow ROADMAP Phase 2-3）——非 SessionVault 待办。**这是解锁 TumeFlow 整条主线（摄取→Episode→Dream）的关键路径，现已贯通**。
  - **P3-④ 🟡**：Class-B `snapshot_file` ✅——Claude/Codex memory、Codex rules、项目 CLAUDE.md/AGENTS.md 经 SHA-256 fingerprint 变更事件进入加密总库；QuotaBar 只触发同步并提供项目身份；`svault snapshots` / `read_active_latest_snapshots` 给 TumeFlow 当前态，源文件删除后不再返回，WSL 暂不可达保守保留。TumeFlow 语义解析仍归自身，adapter 仅作无总库回退（ADR-034）。transcript-from-RawEvent 与 sqlite/derived-path 仍待按需实现。

> snapshot/sqlite/压缩/派生路径的 §11 用例是**契约预留 + 将来门槛**，不是 P0 的实现门槛——API 形状现在定全（避免后期撑破契约），实现按 provider 逐个补。

> 数据源完备性审计（基于 Claude Code / Codex 官方文档与源码）的缺口与架构风险已并入 §3–§8 并定型为 ADR-025；原始调研报告已移除，结论以本文与 ADR-025 为准。**核验 caveat**：该审计的路径/环境变量/语义为 **2026-06 快照**，会随上游版本漂移；未逐条附可复核 URL + 核验日期，故作**设计依据**可用、但**不作长期稳定契约**——所有候选路径仍走"先探测、后按 schema 解析、`planned`→`stable` 升级"（§3.4 / §4），把"官方确认"的稳定性约束交给探测与黄金语料，而非审计快照。
