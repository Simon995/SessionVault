# SessionVault — 共享摄取内核与 RawEvent 总库

状态：初始设计
最后更新：2026-06-13

> **外部引用约定**：本文出现的 `DECISIONS.md`（ADR-0xx）、`SYSTEM_DESIGN.md`、`INTEGRATION.md`、`AGENT_MEMORY_POSITION.md` 均为**其它仓库的跨仓库文档**，不在 SessionVault 仓内：
> - `DECISIONS.md` / `SYSTEM_DESIGN.md` / `INTEGRATION.md` → [TumeFlow `docs/`](https://github.com/Simon995/TumeFlow/tree/main/docs)（ADR-024/025/026 与本内核直接相关；其摘要见本仓 [README](../README.md) 与 [LOGGING.md](LOGGING.md)）。
> - `SESSION_MEMORY_ARCHITECTURE.md` / `AGENT_MEMORY_POSITION.md` / `LOGGING.md`（QuotaBar 侧）→ [QuotaBar `docs/`](https://github.com/Simon995/QuotaBar/tree/main/docs)。
>
> 本仓内的文档为：本文 + [rawevent-reconciliation.md](rawevent-reconciliation.md) + [LOGGING.md](LOGGING.md)。内核独立建仓后，关键 ADR 摘要将固化进本仓，外部仅留链接。

## 1. 定位

**SessionVault** 是一个**独立仓库的 Rust crate**（共享摄取内核 + 不可变 RawEvent 总库），把"发现各 Agent 的本地数据 → 增量扫描 → 归一化为 `RawEvent` → 落入永不删的总库"这件最容易踩坑的事**只实现一次**。QuotaBar 与 TumeFlow 都消费它，避免两处实现悄悄分叉、各踩一遍 Codex 累计 token / `safe_offset` / WSL / 路径发现等坑。

- **中立命名 SessionVault**（crate `session-vault`、CLI `svault`），不属于任何一个消费者，两个产品平等依赖。
- **声明式来源目录**（§3）是单一事实源：要扫哪些 provider、每个平台扫哪些文件夹/文件，全部集中在内核里维护一份。
- **provider 可扩展**（§4）：现在是 Codex / Claude Code，后续 Cursor / Gemini 等只增描述符 + 解析器，不改消费者。
- **路径可配置**（§5）：内置默认路径之外，允许前端/用户填入自定义路径覆盖或补充。
- **无头（headless）**：内核不带 GUI，但产出标准化**扫描报告**（§10），由宿主（QuotaBar / TumeFlow）渲染界面。
- 提供 **Rust lib** + 一个吐 **NDJSON 的 CLI**；可选 **PyO3** wheel。
- 公开 API = 来源目录 + `RawEvent` 契约 + 无状态游标 + 扫描报告。破坏它 = major 版本。
- **黄金 fixture 语料**（§11）是它的一致性测试，也是"坑只有一处"的物理保证。

决策见 `DECISIONS.md` ADR-018、ADR-019。本文件是内核的设计契约；内核独立建仓后，本文件迁入该仓库，TumeFlow 仅保留引用。

## 2. 边界：内核做什么、不做什么

| 内核负责（唯一一份） | 内核不做（消费者各自负责） |
|---|---|
| 维护来源目录（provider × 平台 × 路径） | 自带 GUI（仅产出扫描报告，宿主渲染） |
| 发现来源（Win / macOS / Linux / WSL；环境变量与用户配置覆盖） | 脱敏 / 敏感信息处理 |
| 增量字节读、`safe_offset`、坏行 / 半截行处理 | 写任何数据库 |
| Codex 累计 token parser state | 候选 / 记忆 / Dream（TumeFlow） |
| WSL `wsl.exe` 桥接（只回传 `safe_offset` 后新字节） | `usage_facts` / 成本 / ROI 投影（QuotaBar） |
| 项目根解析（`.git` / marker / `cwd` + confidence） | 游标持久化（消费者各存各的库） |
| 归一化为 `RawEvent`、产出结构化扫描报告 | LLM 调用、联网 |

内核是**无状态纯函数式**的：不持有数据库、不落盘游标、不发网络、不弹界面。详细来源发现与探测规则见 `SYSTEM_DESIGN.md` §11.2–§11.5（那是本内核的来源契约参考）。

## 3. 来源目录（Source Catalog）

这是内核的核心资产之一：**要扫什么、在哪扫，集中声明一份**，消费者不各自硬编码路径。下表是内置默认；实际路径以"环境变量 > 用户配置 > 内置候选"的顺序探测（§5），存在才扫。所有路径中 `~` / `%USERPROFILE%` 按当前平台展开。

#### 来源的两个正交属性（架构保险，见 ADR-025）

数据源审计（结论已并入本文，原始报告已移除）揭示一个关键点：**不能把所有来源都默认当成"追加写的 JSONL、按字节增量读"**——一旦后面接 rules / hooks / agents / 任务状态 / SQLite，就会被迫改核心层。为此来源目录给每个 artifact 标两个正交属性：

- **`source_mode`（形态）**：决定怎么读、用什么游标——
  - `append_log`：只增长的 JSONL（会话、history 的常态），按字节 `safe_offset` 增量读。
  - `snapshot_file`：会被整体重写/覆盖的文本（CLAUDE.md / AGENTS.md / rules / settings / hooks / 自定义 commands、agents、skills），按内容指纹（`hash` / `mtime+size`）判变，变了产"快照变更事件"。**内容存哪里需定型**：`config_snapshot` 事件仅带新旧 `hash` 不足以回溯"改成了什么"——要做行为归因，须把（脱敏后的）配置正文存进 `RawEvent.content`（`full` 档）或单独的 `artifact_store`/`snapshot_blob`（按 `content_hash` 寻址、去重）。骨架暂只发 hash 变更事件，正文存储留作 snapshot 解析器实装时的设计点。
  - `sqlite_store`：SQLite 状态库（Codex `CODEX_SQLITE_HOME`、Cursor `state.vscdb`），用表游标（`rowid` / `wal_lsn` / `content_hash`），**不可按字节读**。
  - `opaque_family`：官方未公开稳定结构的状态族（Claude background/supervisor state、Codex plugin bundle），只登记为"保留来源族"先不写死 glob。
- **两层契约**：§3 区分**来源族**（稳定层：session log / snapshot 指令 / rules / hooks / skill root / plugin root / sqlite state root / derived 任务根）与**已验证实现**（易变层：具体子目录 glob，如 Codex `sessions/YYYY/MM/DD/rollout-*.jsonl`）。子目录漂移只动易变层，**不拖公开契约升 major**。

下面 §3.2 / §3.3 的"主表"是 `append_log` 主数据源；其后的"控制面/行为塑形补充表"多为 `snapshot_file` / `sqlite_store` / `opaque_family`。

### 3.1 平台数据根参考

| 平台 | 家目录 | 应用数据根（供新 provider 用） |
|---|---|---|
| Windows 原生 | `%USERPROFILE%` | `%APPDATA%`（Roaming）、`%LOCALAPPDATA%` |
| macOS | `~` | `~/Library/Application Support` |
| Linux | `~` | `$XDG_CONFIG_HOME`（默认 `~/.config`） |
| WSL 发行版 | 发行版内 `~` | 经 `wsl.exe` 桥或 `\\wsl$\<distro>\home\<user>\` 访问 |

Windows 上每个 provider 都要同时考虑 **Windows 原生**与**一个或多个 WSL 发行版**两套独立来源（见 `SYSTEM_DESIGN.md` §11.3 / ADR-017），靠 `source_location`（`local` / `wsl:<distro>`）区分，禁止只扫一处或合并两处。

### 3.2 Claude Code（status: stable）

配置根默认 `~/.claude`，可被 `CLAUDE_CONFIG_DIR` 覆盖。

| 类别(kind) | 相对路径 / glob | 含正文 | 说明 |
|---|---|---|---|
| session | 来源族=`projects/` **与 `sessions/`** 两根下**递归任意 `*.jsonl`**（已验证实现，非 `projects/<enc>/<uuid>.jsonl` 单一模式） | 是 | 会话事件流，主数据源。QuotaBar 实证：`.claude/projects` + `.claude/sessions` 两根、`find -name '*.jsonl'` 递归 |
| memory | `projects/<项目>/memory/*.md` | 是 | Auto Memory（v2.1.59+），可被 `autoMemoryDirectory` 改写。注：QuotaBar 现未扫，greenfield |
| history | `history.jsonl` | 是 | 命令历史 |
| instruction | 用户 `<config>/CLAUDE.md`；项目 `./CLAUDE.md`、`./.claude/CLAUDE.md`、`./CLAUDE.local.md`、`./.claude/rules/*.md`；托管 policy（平台相关） | 是 | 只读不改写；须解析 `@import`（≤4 跳）与 rules 的 `paths` 作用域 |
| config | `<config>/settings.json` 等 | 否 | 仅用于解析 `autoMemoryDirectory` / `cleanupPeriodDays`，不作事件 |

平台配置根：Windows `%USERPROFILE%\.claude\`；macOS / Linux `~/.claude/`；WSL 发行版内 `~/.claude/`。

**控制面 / 行为塑形来源（补充，多为 `snapshot_file` / `opaque_family`）**——它们解释"Agent 为什么按这种工作流做事"，是流程性/偏好记忆的证据；下列相对配置根 `<config>`（默认 `~/.claude`，受 `CLAUDE_CONFIG_DIR` 覆盖），项目层在 `./.claude/` 下：

| 类别(kind) | 相对路径 / glob | source_mode | 含正文 | 说明 / 置信度 |
|---|---|---|---|---|
| command | `<config>/commands/*.md`；项目 `./.claude/commands/*.md` | snapshot_file | 否 | 自定义 slash 命令=可调用工作流证据。较可信 |
| agent | `<config>/agents/*.md`；项目 `./.claude/agents/*.md` | snapshot_file | 否 | subagent 定义改系统提示/工具/模型，行为塑形。较可信 |
| skill | `<config>/skills/**/SKILL.md`（含插件分发载体） | snapshot_file | 部分 | `CLAUDE_CODE_SYNC_SKILLS=1` 会下载 skills；程序性记忆。较可信 |
| plugin | `<config>/plugins/**`（marketplaces / cache） | opaque_family | 否 | 装了哪些插件/来源；至少读来解析能力面。env `CLAUDE_CODE_PLUGIN_CACHE_DIR` / `_SEED_DIR`。中 |
| task | `<config>/tasks/<task-list-id>/**` | opaque_family | 不定 | 跨 compaction/会话的计划状态；格式未公开。env `CLAUDE_CODE_TASK_LIST_ID`。保留来源族 |
| mcp/config | 项目 `./.mcp.json`；`<config>/settings.json`、项目 `./.claude/settings.json` | snapshot_file | 否 | 解析 MCP / hooks / `autoMemoryDirectory` / worktree 行为，定位真实数据根。中 |
| worktree | 项目 `./.claude/worktrees/**` | opaque_family | 间接 | background session 隔离编辑快照；作 provenance，不默认全文摄取。中 |
| forensic log | `OTEL_LOG_RAW_API_BODIES=file:<dir>` 指定目录 | append_log | 可能含完整正文 | 完整请求/响应落盘；路径全由 env 决定，作"条件来源族"，不写死。高确信存在 |
| bg state | `<config>/` 下未公开 supervisor/background-session state | opaque_family | 可能 | 官方确认持久化在 config dir，文件名未公开；保留来源族，不硬编码 |

> 这些**多数不进 `RawEvent` 正文型事件**，但要作为被扫描、被版本化、可参与路径解析与行为归因的正式来源，否则下游只记住"说了什么"、解释不了"为什么这么做"。

### 3.3 Codex（status: stable）

配置根默认 `~/.codex`，可被 `CODEX_HOME` 覆盖。存在与语义随版本变化，**先探测再按 schema 解析**。

| 类别(kind) | 相对路径 / glob | 含正文 | 说明 |
|---|---|---|---|
| session | 来源族=`sessions/` **与 `archived_sessions/`** 两根下**递归任意 `*.jsonl`**；已观察实现=`sessions/YYYY/MM/DD/rollout-*.jsonl` | 是 | 日期分桶会话轨迹。QuotaBar 实证：扫 `.codex/sessions` + **`.codex/archived_sessions`** 两根、递归 `*.jsonl`（勿漏 archived） |
| history | `history.jsonl` | 是 | 历史/会话转录，受 `history.persistence` / `max_bytes` 控制 |
| index | `state_*.sqlite` | 否 | 若存在仅作可选索引，不作唯一事实 |
| instruction | 全局 `$CODEX_HOME/AGENTS.override.md` 或 `AGENTS.md`（取首个非空）；项目自 Git 根到 cwd 逐级 `AGENTS.override.md` / `AGENTS.md` / `project_doc_fallback_filenames` | 是 | 只读；`.codex/config.toml` 是配置而非指令 |
| config | `$CODEX_HOME/config.toml` | 否 | 仅配置，不作事件 |

平台配置根：Windows `%USERPROFILE%\.codex\`；macOS / Linux `~/.codex/`；WSL 发行版内 `~/.codex/`。

> **session glob 降级**：上表 `sessions/YYYY/MM/DD/rollout-*.jsonl` 应视为**已观察实现**而非公开稳定契约——官方只承诺 `CODEX_HOME` 下有 sessions、不承诺分桶层级。归到"sessions 来源族 + 探测后 schema 解析"，当前 rollout JSONL 作一个 parser 候选。

**控制面 / 状态面来源（补充）**——`<root>`=`CODEX_HOME`（默认 `~/.codex`），项目层在 `./.codex/` 下：

| 类别(kind) | 相对路径 / glob | source_mode | 含正文 | 说明 / 置信度 |
|---|---|---|---|---|
| config（多层） | `<root>/config.toml`、`<root>/<profile>.config.toml`、项目 `./.codex/config.toml`、Unix `/etc/codex/config.toml` | snapshot_file | 否 | 配置层级=项目/profile/用户/system；漏了会丢项目级 MCP/hooks/rules。中 |
| agent | `<root>/agents/*.toml`；项目 `./.codex/agents/*.toml` | snapshot_file | 否 | personal/project custom agents，覆盖模型/sandbox/MCP/skills。中 |
| hooks | `<root>/hooks.json`、项目 `./.codex/hooks.json`，及 config.toml `[hooks]` | snapshot_file | 否 | 行为控制面/自动化 side-effect 证据。低中 |
| rules | `<root>/rules/*.rules`、各 active config 层下 `rules/` | snapshot_file | 否 | 用户授权命令写到 `rules/default.rules`=持续学习出的权限/流程记忆。低中 |
| skill | `~/.agents/skills/**/SKILL.md`、`$REPO/.agents/skills/**`、`$CWD[/..]/.agents/skills/**`、Unix `/etc/codex/skills/**` | snapshot_file | 是 | REPO/USER/ADMIN/SYSTEM 多作用域；程序性记忆一等公民。**当前最大缺口**。中 |
| sqlite state | `CODEX_SQLITE_HOME` 或 `<root>` 下 `**/*.sqlite*` | sqlite_store | 不定 | 官方有 SQLite-backed state 且可单独重定向；提升成"SQLite 状态根"。高 |
| plugin | config.toml `[plugins.*]`；本地 bundle 路径未公开 | opaque_family | 可能 | 插件启停+注入 MCP/hook/skill；先纳配置层，bundle 后补。很高漂移 |
| auth | `<root>/auth.json`（file-based 凭据时） | —（排除） | 否 | **凭据面，明确不收正文、不入 RawEvent**，仅供安全排除规则。低 |

> `history.jsonl` 已在主表，但**不是严格 append-only**：超 `history.max_bytes` 会丢最旧并压缩文件，需配合 §8 的截断/重写检测。

### 3.4 后续 provider（status: planned，路径待验证）

下列 provider 先以**描述符**登记占位，准确路径以探测 + 用户配置为准，验证后再升 `experimental` / `stable`，**不要把候选路径当稳定接口**。

| provider | 候选位置（待验证） | 形态线索 |
|---|---|---|
| Cursor | Windows `%APPDATA%\Cursor\User\...`；macOS `~/Library/Application Support/Cursor/User/...`；Linux `~/.config/Cursor/User/...` | VS Code fork，会话多在 `workspaceStorage/<hash>/state.vscdb`、`globalStorage`（SQLite），非 JSONL |
| Gemini CLI | 候选 `~/.gemini/`（按平台展开） | 待验证文件结构 |
| 通用 JSONL | 用户手动指定路径 | 任意符合通用 schema 的 JSONL，走 `jsonl` 解析器 |

新增 provider 的标准流程见 §4。

## 4. Provider 可扩展性（声明式描述符）

加一个 provider = **加一个描述符 + 一个解析器**，消费者代码与 `RawEvent` 契约不动。描述符形如：

```text
ProviderDescriptor {
  provider_id          # claude_code | codex | cursor | gemini | jsonl | ...
  display_name
  status               # stable | experimental | planned
  config_root {        # 每平台候选根 + 环境变量覆盖
    env_override        # 如 ["CLAUDE_CONFIG_DIR"]
    windows / macos / linux / wsl   # 候选根路径列表，逐个探测
  }
  artifacts [          # 该 provider 下要扫的各类产物
    { kind, glob, parser, content: bool,
      source_mode,      # append_log | snapshot_file | sqlite_store | opaque_family（见 §3）
      cursor_kind,      # byte_offset | fingerprint | sqlite_rowid | none（见 §8）
      family,           # 稳定来源族 id（两层契约的稳定层）
      override_setting? }
    # 例：{ session, "projects/*/*.jsonl",   claude_jsonl, true,  append_log,    byte_offset,  session_log }
    #     { agent,   ".claude/agents/*.md",   markdown,     false, snapshot_file, fingerprint,  snapshot_instruction }
    #     { sqlite,  "**/*.sqlite",           codex_sqlite, false, sqlite_store,  sqlite_rowid, sqlite_state }
  ]
}
```

- **描述符是数据，不是分支逻辑**：发现器对所有 provider 跑同一套"探测根 → 匹配 glob → 绑定解析器"流程；`source_mode` 决定走哪类读取器与游标，发现/报告框架复用。
- **解析器是 trait**：`parse(input, cursor, profile) -> (events, cursor)`，`input` 随 `source_mode` 而异（字节窗口 / 快照全文 / SQLite 句柄）。Claude/Codex/通用 JSONL 各实现 `append_log` 一份；快照指令实现 `snapshot_file` 一份；Cursor / Codex SQLite 实现 `sqlite_store` 一份。新增来源**首选只加描述符 + 复用已有 mode 的解析器**，不动公开契约。
- 新 provider 先入库为 `planned` 描述符（仅占位、不默认扫），补完解析器 + 黄金 fixture（§11）后升级状态。
- provider 状态与 `RawEvent` 的 `parser_version` 绑定，格式漂移时显式失败而非静默错解。

## 5. 路径可配置（内置默认 + 用户覆盖）

内核自带默认目录，但**允许前端/用户填路径**，便于非标准安装、便携目录、额外 WSL 发行版或新 provider 抢先接入。优先级（高→低）：

1. **环境变量覆盖**：`CLAUDE_CONFIG_DIR` / `CODEX_HOME` / `CODEX_SQLITE_HOME` / `CLAUDE_CODE_PLUGIN_CACHE_DIR` / `CLAUDE_CODE_PLUGIN_SEED_DIR` / `CLAUDE_CODE_TASK_LIST_ID` 等（来自描述符 `env_override`）。
2. **用户配置覆盖**：宿主把一份用户配置传给内核，可启用/禁用 provider、增删根路径、登记新 provider 根。
3. **内置候选**：§3 默认路径，逐个探测，存在才扫。

**来源目录不是静态路径表，而是派生路径解析器（derived-path resolver，见 ADR-025）**：有些根要先读一层 config/env 才知道去哪扫（`CODEX_SQLITE_HOME` 决定 SQLite 根、`CLAUDE_CODE_TASK_LIST_ID` 决定任务目录、profile 选择决定 config 层、project trust 决定项目层是否生效）。内核按"读 env/config → 算真实根 → 派生子来源"两段式发现，**把这层逻辑收在内核里**；否则消费者各自塞路径推导，正好违背"坑只有一处"的设计目标。这层是 `discover()`（§9）的职责，仍无状态——config/env 由宿主作入参传入。

用户配置示意（由宿主持有与持久化，内核只读取入参）：

```text
[providers.codex]      enabled=true                                  # 开关
[providers.cursor]     enabled=true  roots=["D:/Cursor/User"]        # 补一个根
[providers.claude_code] roots_add=["\\\\wsl$\\Ubuntu-22.04\\home\\me\\.claude"]  # 额外 WSL 根
[extra_jsonl]          paths=["E:/exports/*.jsonl"]                  # 通用 JSONL 自定义路径
```

- 内核**不**自己存配置（无状态原则）：配置由宿主（QuotaBar 设置页 / TumeFlow 配置）保存，每次调用作为入参传入。
- 这样"前端填一个路径就能扫"对两个产品都成立，且填的逻辑不在内核——内核只认入参，UI 各做各的。
- 安全：用户新增根仍受授权与排除规则约束（`auth.json`、`.env`、私钥、未授权目录默认排除，见 `SYSTEM_DESIGN.md` §11.2 / §12）。

## 6. 两种抽取档位（profile）

| profile | 适用场景 | 是否含正文 | 用途 |
|---|---|---|---|
| `metadata` | 不建总库的独立 / 轻量场景 | **否** | token / model / cwd / 项目 / 时间 |
| `full` | 共享总库 + TumeFlow 物化（默认集成形态） | 是（原始可见正文） | 物化分库时由 TumeFlow **脱敏**后入证据 |

内核在 `metadata` 档**根本不提取正文**。脱敏不在内核里，而在 TumeFlow 物化分库时做。

> **`full` 档的正文持久化是新建能力，非来自 QuotaBar**（见 `rawevent-reconciliation.md` §3）：QuotaBar 现仅按需即时解析正文、落库只存折叠 `search_text`。`full` 档把 `message_from_*_value` / `extract_text` 的即时正文升级为持久 `RawEvent.content` 并入总库，是 P3 总库的主要新建量。

档位是内核的**能力**，不再等于 QuotaBar 的隐私边界：采用共享 RawEvent 总库（§13 / ADR-020）后，总库以 `full` 物化（含正文），QuotaBar 已放开"不读正文"限制、经总库读取正文（见 ADR-021 / QuotaBar `AGENT_MEMORY_POSITION.md` §1.5）。`metadata` 档位保留用于不建总库的场景（例如只装 QuotaBar、或想要不含正文的轻量投影）。描述符中 `content: false` 的 artifact（如 config）在任何档位下都不出正文。

## 7. `RawEvent` 契约

```text
schema_version            # 内核归一化 schema 版本
source_type               # claude_code | codex | cursor | gemini | jsonl
source_location           # transcript 文件存储位置：local | wsl:<distro>
workspace_location        # 项目物理所在：local | wsl:<distro> | wsl（WSL 项目可记在 local transcript 下，与 source_location 不同；QuotaBar 实证有此二分）
source_mode               # append_log | snapshot_file | sqlite_store（事件来自哪类形态，见 §3）。注：opaque_family 只登记、不产 RawEvent，故此处不含它
source_path               # 源文件
source_session_id
seq                       # 文件内单调序号，用于排序 + 去重
occurred_at               # 事件在对话内发生的时间，UTC unix 秒；冲突裁决(latest-wins)的权威时间
time_confidence           # occurred_at 可信度：high | low（缺失/不可靠时 low，交下游处理）；【QuotaBar 无此概念，本契约新增——QuotaBar 对无时间戳事件直接丢弃】
actor                     # user | assistant | tool | system（thinking 不单列 actor，归 assistant，见下 event_type 说明）
event_type                # message | tool_use | tool_result | usage | meta | config_snapshot（snapshot_file 变更时产出）| thinking（思考/推理块）
cwd
project_root              # 解析结果
project_root_source       # git | marker:<file> | cwd | wsl_cwd | missing_cwd（QuotaBar 实证取值；marker 文件∈{Cargo.toml,package.json,pyproject.toml,go.mod,AGENTS.md,CLAUDE.md}）
project_root_confidence   # 0.0–1.0
git_branch
model                     # 适用时
effort                    # Codex
input_tokens
output_tokens
cache_creation_tokens
cache_read_tokens
tool_name
result_status
message_id
request_id
parent_ref                # 父子关系（Claude parentUuid）；内部字段，非稳定外部接口。【QuotaBar 现完全不读 parentUuid，本契约新增——greenfield】
content                   # 仅 full 档：原始可见正文（未脱敏）；metadata 档为空。【QuotaBar 不持久化正文，仅按需即时解析 + 存折叠 search_text；full 档总库存正文是新建能力，见 §6/§13、rawevent-reconciliation.md】
content_hash
raw_reference             # 字节偏移 / 行指针，用于回溯源
parser_version            # 解析器版本，绑定字段语义
```

去重唯一键 = `(source_type, source_location, source_path, source_session_id, seq)`。`content_hash` 仅用于相似重复检测，不作全局唯一约束（见 `SYSTEM_DESIGN.md` §9.4）。`ingested_at`（入总库时间）与总库 `offset` 由**总库层在 append 时附加**，不由解析内核产出（解析内核无状态，见 §13.1）。

> **粒度对账（见 `rawevent-reconciliation.md`）**：QuotaBar 只在 **usage 事件**（Claude `type=assistant` 带 usage；Codex `token_count`）产 fact，**用户提问 / 工具结果 / 纯文本助手消息不成事件**，且正文只在即时 transcript 路径上、不落库。本契约的"逐事件 RawEvent（每条消息一事件、含正文、带 `actor`/`event_type`）"是在 QuotaBar 正文抽取函数（`message_from_*_value` / `extract_text`）之上的**升级**：把即时、含正文的 transcript 解析改成持久事件源。`seq` 语义 = 文件内行位序（增量时接已存最大 `seq`+1）。

> **thinking / reasoning 建模（闭合 `rawevent-reconciliation.md` §3.3 的取舍）**：思考块（Claude `thinking`、Codex `reasoning.summary[].text`）统一记为 `actor=assistant` + `event_type=thinking`——**不另设 thinking actor**（QuotaBar 的 role=thinking 气泡归一到 actor/event_type 二维）。**opaque（明文不可得）**：Codex `encrypted_content` 等无明文场景，仍产 `thinking` 事件但 `content=None`，表示"推理发生过、无正文"，下游据此区分明文思考与加密思考。黄金语料 §11 已含该用例。

## 8. 无状态游标 API

```text
scan(source_ref, cursor_in, profile) -> ScanResult {
  events:     [RawEvent],
  cursor_out: Cursor,
  status:     ok | partial | error,
  error?:     ...
}

Cursor {
  source_path
  cursor_kind               # byte_offset | fingerprint | sqlite_rowid | none（随 source_mode，见 §3）
  # —— append_log（cursor_kind=byte_offset）——
  safe_offset?              # 已安全消费的字节
  size?                     # 上轮文件大小
  mtime?
  # —— snapshot_file（cursor_kind=fingerprint）——
  content_hash?             # 上轮快照内容哈希；变了才产 config_snapshot 事件
  # —— sqlite_store（cursor_kind=sqlite_rowid）——
  last_rowid?               # 或 wal_lsn / 表级 max(ts)
  schema_fingerprint?       # 表结构指纹，漂移即显式失败
  # —— provider 专有 ——
  codex_state? {            # 仅 Codex append_log：累计 token 解析所需（QuotaBar CodexParserState 实证）
    current_model
    current_effort
    current_cwd
    current_session_id
    previous_total { input, cached, output }   # 累计 token 三元组（非标量）；delta = last_token_usage 优先，否则 total − previous_total
  }
}
```

游标是**带类型的联合**：`source_mode` 选定 `cursor_kind`，三类形态各用各的推进量，公开契约一次定全，新增来源不再撑破"只有 byte_offset"的假设。

推进规则（与 QuotaBar 实机验证一致）：

1. **append_log** — 只解析**完整 JSONL 行**；末行半截写入本轮不解析、不推进 `safe_offset`。
2. **append_log 坏 JSON** — 本轮所读尾批中**任一完整行**解析失败 → **冻结整批**：`safe_offset` 保持本轮起点、`status=error`、**本轮不发事件**，下轮重读整段尾（与 QuotaBar 实证一致，见 `rawevent-reconciliation.md` §2："一坏行冻结整批尾"）。append-only 完整行不可变、retry 对永久损坏无效，这是"宁可重读、不静默跳过/错解"的保守取舍；**已知代价**：永久损坏的完整行会让该来源停在原地，将来可在游标加 retry 计数做"毒行"跳过（后续阶段）。（半截尾行属规则 1 的 pending，不在此列。）
3. `(mtime, size)` 未变且游标已到尾（byte_offset 满足 `safe_offset >= size`；fingerprint 满足 `content_hash` 未变）时跳过该文件。
4. **append_log 截断 / 重写 / 压缩检测**（关键）：判据以 **`(mtime, size)` 回退**为准（QuotaBar 实证：缓存 `mtime > 当前` 或 `size <` 缓存即触发 `safe_offset` 归零、全量重建；WSL 侧 `size < known` → Full 重读）。压缩场景如 Codex `history.jsonl` 超 `max_bytes` 丢最旧。重建后用 `(occurred_at, message_id, request_id)` 去重，**不把旧尾当新事件**。（`content_hash` 不用于 append_log 截断判定，仅用于 snapshot_file，见规则 5。）
5. **snapshot_file** — 比 `content_hash`；变了产一条 `config_snapshot` 事件（带新旧哈希），未变跳过。整体重写是常态，不用字节游标。
6. **sqlite_store** — 先校 `schema_fingerprint`（漂移即显式失败、不静默错解），再按 `last_rowid` / `wal_lsn` 增量取；**不可按字节读**。
7. **游标由调用方持久化**：QuotaBar 存自己的 `cache.db`，TumeFlow 存自己的库。内核本身不落盘——不同消费者的读取进度互不干扰，无锁竞争。

## 9. 标准化接口汇总

内核对外是一组稳定调用，CLI（NDJSON）与 lib/PyO3 等价：

| 接口 | 入参 | 出参 | 用途 |
|---|---|---|---|
| `catalog()` | （可选 user_config） | 生效后的 provider 描述符列表 | 宿主据此渲染"将扫哪些 provider/路径"与设置页 |
| `discover(user_config)` | 用户配置 + 平台 | 发现到的来源清单（path、location、kind、provider、是否已授权） | 首次只发现不读，供用户授权 |
| `scan(source_ref, cursor_in, profile)` | 单来源 + 游标 + 档位 | `ScanResult`（事件 + 新游标 + 状态） | 增量摄取主接口 |
| `scan_all(user_config, cursors, profile)` | 全量来源 + 游标表 | 事件流 + 游标表 + **扫描报告**（§10） | 一轮全量增量扫描 |

- CLI 形态（`svault`）：`svault discover`、`svault scan --source ... --cursor ...`、`svault scan-all --profile metadata|full`，事件走 stdout NDJSON，报告走单独 JSON。
- 两个消费者用**同一组接口**，差异只在 `profile` 与各自持久化的游标——这是"代码一份、维护一处"的接口面。

## 10. 扫描报告（Scan Report）与 GUI

内核**无头**，但每轮扫描产出结构化报告，宿主据此渲染"扫了哪些文件、各是什么情况"。报告即 GUI 的数据契约：

```text
ScanReport {
  started_at, finished_at, platform
  providers [
    { provider_id, status: enabled | skipped | error,
      roots_probed [ { path, location, exists, source: builtin|user|env } ],
      sources [
        { source_path, location, kind, provider_id,
          # —— 模式无关（任何 source_mode 都有意义）——
          source_mode, cursor_kind,
          items_examined, events_emitted, items_skipped,
          fingerprint_changed,    # snapshot_file：指纹是否变
          schema_changed,         # sqlite_store：schema 指纹是否漂移
          rollback_detected,
          status: ok|partial|skipped|error, error?, last_event_at,
          # —— append_log 专属（字节口径）——
          bytes_new, pending_tail_bytes } ],
      totals { sources, events, bytes_new } } ]
  warnings [ ... ]   # 路径不存在 / 权限拒绝 / 未授权 / 格式漂移
}
```

**GUI 放在宿主，不放内核**：

- QuotaBar 已是 Tauri 桌面应用，是这个扫描状态界面的**天然落点**——直接把 `ScanReport` 渲染成"provider → 根 → 文件 → 新字节/事件数/状态"的表，警告单列。
- TumeFlow 侧用 CLI 表格或简单 web 视图渲染同一份 `ScanReport` 即可，无需重做采集逻辑。
- 这样"一个简单 GUI 显示扫描了什么、情况如何"的需求由**共享的报告 schema + 各自的渲染**满足，仍是"代码一份"（采集一份、报告结构一份），渲染按产品风格各做各的。

建议宿主界面至少展示：本轮扫描的 provider 与生效根、每个文件读取的新字节/事件数、跳过/出错原因、未授权与格式漂移告警，以及"前端填路径/启停 provider"的设置入口（写回 §5 的用户配置）。

> **`ScanReport`（机器结果）≠ 日志（诊断流水账）**：前者确定性、`return`/序列化给宿主渲染、可进黄金语料断言；后者走 stderr、含 `run_id`/时间等易变量、给人排错。日志规范见 `LOGGING.md`（对齐 QuotaBar `docs/LOGGING.md`：lib 用 `log` 复用宿主 sink、stdout=NDJSON/stderr=日志、正文不进日志；决策见 TumeFlow ADR-026）。

## 11. 黄金 fixture 语料（一致性套件，必须覆盖）

这是整套架构最值钱的资产：所有 tricky case 做成样例输入 + 期望 `RawEvent` / `ScanReport` 输出，消费者升级内核前必须全绿。

- Codex 累计 token 跨多次 append 的 delta 正确性：`last_token_usage` 存在时直接取、缺失时 `total_token_usage − previous_total{input,cached,output}`。
- Codex `cached = min(delta.cached, delta.input)`、`input_tokens = delta.input − cached` 的拆分正确。
- 一文件多 session：Claude `--resume`/fork 把父 `sessionId` 行重放进子文件、Codex 多个 `session_meta` → 按**行级** `session_id` 归属，不串话。
- thinking/reasoning：Claude `thinking` 块、Codex `reasoning.summary[].text` → 按取舍策略产 `thinking` 事件或排除；Codex `encrypted_content` 无明文 → 不产正文（opaque）。
- 末行半截写入 → 本轮不解析、`safe_offset` 不前进。
- 单行坏 JSON（完整行）→ **冻结整批**：`status=error`、`safe_offset` 不前进、本轮不发事件、下轮重读整段尾（见 §8 规则 2）。
- `(mtime, size)` 回退 / 文件被截断 → 归零重建。
- 同 `session_id` 在 `local` 与 `wsl:<distro>` 各一份 → 靠 `source_location` 区分，不互相覆盖。
- Claude `parentUuid` 分支 / 重试 / 编辑 → 父子树，区分采纳与废弃分支。
- Codex 会话中途 `cwd` 变化 → token 与项目归因正确。
- `cwd` 缺失 → `project_root_source=missing_cwd`（QuotaBar 实证取值）；WSL UNC `\\wsl.localhost\<distro>\…` → 规范化为 `wsl:<distro>:/path`。
- WSL 增量三态：`CacheHit`（size==known，零正文）/ `Append`（tail）/ `Full`（首次或截断）→ 由 `(size, known)` 决定，与本地字节游标等价。
- `workspace_location` ≠ `source_location`：WSL 项目记在 local transcript 下时，前者 `wsl:<distro>`、后者 `local`，不混淆。
- 迟到入库：旧 `occurred_at` 事件以大 `offset` 追加 → 时间线按 `occurred_at` 重建，`offset` 不当时间用（latest-wins 不被冒充）。
- `occurred_at` 缺失 / 不可信 → `time_confidence=low`，不默认当"现在"。
- 项目根 `.git` vs marker vs `cwd` 三种来源 + 对应 confidence。
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

| 形态 | 服务谁 | 调用方式 | 优先级 |
|---|---|---|---|
| **CLI `svault`（NDJSON）** | 任何语言（Python / Node / Go / …） | 子进程 + stdout 流 | **P0 主交付** |
| **Rust lib `session-vault`** | QuotaBar（原生） | cargo 依赖，进程内 | **P0**（同一份代码） |
| **PyO3 wheel（pip）** | 仅 Python | `import`，进程内 | 后置，实测需要再上 |

- **CLI 是跨语言主交付，不是 pip**：内核中立，交付也中立。一个 `svault` 二进制服务所有语言；pip 只解决 Python 一种消费者，却要背 `manylinux × macOS × Windows × 多 Python 版本` 的 wheel 构建矩阵。
- **CLI 优先的四个理由**：(1) 一个产物服务所有消费者；(2) 给定二进制 = 给定确定内核版本，和"总库 offset + schema 版本"复现戳天然对齐，钉版最干净；(3) 扫描器 panic 只挂子进程，不波及宿主（PyO3 同进程一崩全崩）；(4) 扫描是 I/O 密集批量摄取、非高频小调用热循环，子进程开销被大块读摊薄，且 NDJSON 流贴合 `scan_all`。
- **pip（PyO3）何时才上**：仅当**实测**出现高频细粒度（逐事件）调用、大正文序列化成热点、或需给第三方干净 `import` API 时，用 `maturin` 把同一份 crate 包成预编译 wheel 传 PyPI——是锦上添花，不替代 CLI。
- **预编译**：各平台 CLI 走 GitHub Release（Windows / macOS / Linux）；TumeFlow 打包对应二进制随 sidecar 一起发。
- **semver**：来源目录、`RawEvent`、`ScanReport`、游标都是公开 API；黄金语料是一致性套件，消费者升级前必须跑。
- **两个消费者的钉版方式不同**（"submodule" 是源码钉版手段，不是调用方式）：
  - **QuotaBar（Rust，进程内 lib）**——钉**源码**。抽取期（P1–P2，co-development）用 **git submodule + cargo `path` 依赖**，submodule 的 commit SHA 即版本，方便在 QuotaBar 构建里直接迭代 crate；契约稳定后转 **cargo git `rev` 锁** 或 **crates.io**。QuotaBar 已用 submodule，顺手。
  - **TumeFlow（Python，子进程 CLI）**——**不** submodule Rust 源码，而是钉 **GitHub Release 的预编译 `svault` 二进制版本**，随 sidecar 打包、运行时子进程调用。仅当将来上 PyO3/pip（ADR-024 后置项）才可能 submodule 源码本地构建 wheel。

## 13. 集成形态：上游 RawEvent 总库 + 下游物化分库

解析内核本身无状态（§8 纯函数）。在它之上，SessionVault 仓库再提供一个**可选的持久化组件**——共享 **RawEvent 总库**（SessionVault 名字里的 "Vault" 即指这个永不删、不可变的库），把两个产品的集成收敛成"上游一个活库、下游各自物化"的形态。决策见 ADR-020 / ADR-021。

### 13.1 总库（活、最新、中立）

- **内容**：只存 `RawEvent` 契约，**append-only 不可变**；不含任何产品的领域表（`usage_facts` / 记忆都不入总库）。
- **归属**：SessionVault 仓库（中立），不归 QuotaBar，也不归 TumeFlow。
- **写者**：谁跑扫描谁写，同一时刻单写者；QuotaBar 常驻，天然当默认写者。开 WAL，读者不挡写。
- **版本**：一直跟随最新内核往前走，方便 QuotaBar 直接读用。
- **正文**：以 `full` 物化（含正文）。QuotaBar 已放开"不读正文"限制（ADR-021），故总库可承载正文供两边使用。
- **保留**：**永不删、不压缩、不过期**——总库是证据最终归宿（与 ADR-016 一致）；正因永不删，下游落后可随时全量重建。**注意此默认与本地隐私控制存在张力，必须配套 §13.6 的隐私/删除机制**，否则"证据归宿"会与"用户对本机数据的掌控权"冲突。
- **时间**：每条记录 `occurred_at`（对话内时间，冲突裁决权威）与 `ingested_at`（入库时间，溯源）；`offset` 仅作同步游标，**不代表**时间先后（迟到入库的旧事件 offset 大但 `occurred_at` 旧）。

### 13.2 TumeFlow 分库（物化、固化、可复现）

- TumeFlow **不直接读活总库**，而是按自己的节奏、钉自己的 `RawEvent` 版本，从总库**增量拉取**并物化成自己的领域分库（Episode / 证据 / 记忆），两次同步之间冻结。
- 同步用一个**指向总库的游标**（拉到哪条 RawEvent），幂等、可断点续。
- 分库盖**复现戳**：`总库 offset + RawEvent schema 版本`，评测据此精确重建。
- 因总库不可变，给定 offset 永远确定性地物化出同一份分库。
- **两条同步路径**：**增量**（日常拉 offset 之后的新事件，轻）/ **全量重建**（落后过多 / 版本升级 / 分库损坏 / 换钉版本时，从 offset 0 重物化）。总库不可变 → 重建恒为确定性结果，是增量的可靠底座。
- **时间敏感**：记忆冲突裁决（latest-wins，如用户后来更新规则以最新为准）按事件 `occurred_at`，**不按入库顺序 / offset**；详见 `DECISIONS.md` ADR-020 时间语义。

### 13.3 为什么同时满足"新"和"稳"

- QuotaBar 要新 → 总库一直最新，直接用。
- TumeFlow 要稳 → 分库两次 sync 之间冻结；TumeFlow 是**主动拉**，总库升到它尚不支持的大版本时只要不同步过该点，就停在上一个固化版本，**暂停同步而非崩**。

### 13.4 独立性退路（铁律）

TumeFlow 运行时不依赖 QuotaBar 是否存在：

- 有总库 → 从总库拉。
- 无总库（用户只装 TumeFlow）→ TumeFlow 自己跑内核扫盘填分库。

两条路最终都得到分库（与 ADR-001 独立性一致）。

### 13.5 固化管理（GUI 四件套）

分库固化的边界条件已定（见 ADR-020）：总库永不删 → 落后则全量重建；不做多版本并存，只维护单一分库。界面上用四件套管理（"记忆"页，宿主渲染，详见 `INTEGRATION.md` §12）：

- **自动后台增量同步**（默认开，跟 Dream 节奏）——保持"新"，无需按钮。
- **"重建记忆库"按钮**（带确认 + 进度）——落后过多 / 版本升级 / 修复时的底座操作。
- **状态行**——显示"已固化至 offset / 时间、落后总库 N 条、上次固化时间"（数据取自同步游标 + `ScanReport`）。
- **"锁定版本"开关**——评测 / 开发期暂停自动同步、冻住分库，满足"钉固定版本"诉求。

### 13.6 隐私 / 删除 / 加密（设计要求，待定型为 ADR）

总库默认 `full`、含正文、永不删——架构上强，但隐私风险也最大。"证据最终归宿"与"用户对本机数据的掌控"必须靠下列机制调和，**否则二者打架**。这些是总库组件的硬要求（解析内核仍无状态、不涉隐私存储）：

- **显式授权**：`discover()` 首次只发现不读；每个来源（尤其 `full` 含正文、forensic log、含凭据目录）须经用户显式授权才纳入摄取。
- **敏感源默认排除**：`auth.json` / `.env` / 私钥 / 凭据面**永不进 RawEvent**（§3.3 已列、§11 有用例）；未授权目录默认不扫。
- **at-rest 加密**：总库正文落盘应支持加密（密钥归用户/OS keychain，内核不持密钥）。
- **导出 / 销毁**：提供"导出整库"与"销毁整库 / 按来源删除"两类操作。**永不删是默认保留策略、不是不可删**——用户主动销毁是一等公民操作（与"逻辑 append-only"不矛盾：销毁是用户对物理存储的控制权）。
- **正文存储档位可配**：除全局 `metadata` / `full`（§6）外，允许**按来源**关正文（只留元数据），满足"想留用量、不想留正文"的折中。

> 落点：这些应固化为一条 ADR（暂记 **ADR-027 隐私与删除**，TumeFlow `DECISIONS.md`），并在 QuotaBar/TumeFlow 的"记忆"页给出对应 GUI（授权清单、排除规则、导出/销毁、加密开关）。**在该 ADR 落定前，总库默认实现应保守：`full` 正文加密 + 敏感源排除 + 可销毁。**

## 14. 非目标

- ❌ 不脱敏（TumeFlow 在物化分库时做）。
- ❌ **解析内核**不写库、不落盘游标、不存配置（消费者负责）；可选的 RawEvent 总库是同仓库内的独立持久化组件，只 append RawEvent、不碰领域表（见 §13）。
- ❌ 不自带 GUI（只产出 `ScanReport`，宿主渲染）。
- ❌ 不调 LLM、不联网。
- ❌ 不做 `usage_facts` / 记忆等下游投影（消费者各自投影）。
- ❌ 不把 `planned` provider 的候选路径当稳定接口（须探测 + 验证）。
- ❌ **解析内核**不持久任何状态——纯函数 `(目录 + 配置 + 游标) → (事件 + 新游标 + 报告)`；持久化是总库组件的事，与解析分层。

## 15. 落地计划（从 QuotaBar 抽取）

SessionVault 不从零写，而是**抽取 QuotaBar 已实机验证的扫描器**；QuotaBar 用绞杀者迁移改用它，不改现有功能。这是当前最高优先级，决策见 ADR-022。

**核心原则：一个扫描器，总库是它的输出。** 不要先用新代码建总库、却让 QuotaBar 继续跑旧扫描器——那会变成同机两个扫描器（正是本项目要消灭的重复）。先让 QuotaBar 改用共享扫描器，总库只是其额外输出。

**待抽取的 QuotaBar 源**（→ `session-vault`）：

- `session_index.rs`（会话索引 / 扫描主体）、`session_transcript.rs`
- `jsonl_cache.rs`（增量 `safe_offset`）
- `providers/claude.rs`、`providers/codex.rs`（Codex 累计 token）
- `wsl/mod.rs`（WSL 桥）、`paths.rs`（路径发现）

**绞杀者迁移五步**（保住成熟功能）：

1. 冻结 QuotaBar 当前输出为**黄金语料**（零运行时改动，纯安全网 + 规格）。
2. **抽取而非重写**，crate 输出 `RawEvent`。
3. QuotaBar 在**现有接口后**调用，`agent_sessions` / `usage_facts` schema 与 UI 不变。
4. **影子并跑**：新 crate 与老扫描器并行，输出 diff 老 `cache.db`，一致才切；切换走 feature flag，老路径留回退。
5. 信任后再把**总库作为输出**打开，TumeFlow 才开始消费；QuotaBar 用 submodule pin 控更新。

**风险点**（切换前必须有 fixture）：Codex 累计 token parser state、WSL 桥、增量 `safe_offset`——对应 QuotaBar `SESSION_MEMORY_ARCHITECTURE.md` §13 回归风险。

**优先级与 P0 硬边界**（防止 P0 被总库/snapshot/sqlite/插件/隐私一起拖大）。**进度（截至 2026-06-14）**：

- **P0（窄而硬）✅ 基本完成**：`RawEvent` v0 字段已定稿（`src/rawevent.rs`）；`append_log` JSONL（Claude/Codex **Local**）+ Codex 累计 token 的黄金语料已落地（**31 单测全绿**：坏行冻结整批、半行 pending、续扫不重不漏、`(mtime,size)` 回退重读、Codex 跨批延续/跨 session 重置、字段映射、profile 正文开关、**宿主感知路径规范化**）。`source_mode` / 多形态游标 / 派生路径 / 两层目录公开 API 已预留；`snapshot_file` / `sqlite_store` / `opaque_family` / derived-path 按设计返回 `planned` / `skipped`。**待补**：WSL 双位置的真实黄金语料（规范化逻辑已单测覆盖，缺端到端 fixture）。
- **P1 ✅ 基本完成**：Local Claude/Codex `append_log` 解析器已实装并过语料（`parser.rs` / `scan.rs` / `discover.rs` / `cursor.rs`，`svault scan-all` 已能吐 `RawEvent` NDJSON 流）。**游标已持久化**：`scan-all --state <file>`（默认 `<data_local_dir>/svault/cursors.json`，`--stateless` 关）跨运行续扫——实测二次扫描 23476→**0** 事件（全 cache-hit），Codex 累计 token 状态/`next_seq` 随游标存活。**路径规范化层已抽取并标准化**：`src/pathnorm.rs` 宿主感知（`HostPlatform`），UNC↔规范形互转、`workspace_location` 已接线产出（修掉了 QuotaBar「裸 `/abs` 一律当 WSL」的 Windows 宿主假设——Linux 原生跑时同路径正确判 `local`）。**WSL 访问桥已通（端到端实测）**：`src/wsl.rs`（移植 QuotaBar `wsl/mod.rs`）——纯逻辑（发行版名/UTF-16LE/`find -print0` 解析、`default_distro` 选择，跨平台单测）+ 实时层（`#[cfg(windows)]` spawn `wsl.exe`：`list_distros`/`list_jsonl_under_home`/`stat`/`read_range`/`read_file_at`，非 Windows 给桩）。`discover()` 枚举 `Wsl(distro)` 来源；扫描经 **`ByteSource` 抽象**（`scan.rs`）让本地（`File`/`Seek`）与 WSL（`wsl.exe stat` + `tail -c +K | head -c N`）**共用同一份**游标/回退/坏行冻结逻辑；`default_distro` 用来源自身发行版把裸 Linux cwd 打成精确 `wsl:<distro>`。**实测（2026-06-14，真实本机数据，metadata profile 无正文）**：48 来源（19 local + 29 WSL）→ 23373 事件，其中 **8102 来自 WSL**（Ubuntu-24.04 codex 7726 + Ubuntu-OpenClaw claude 376），**零 warning/error**。WSL 增量 tail（`read_range` start>0）由 env-gated 实机 IT（`SVAULT_WSL_IT=1`，`/tmp` 一次性文件）验证。**P1 剩余尾巴**：WSL 双位置真实**黄金语料** fixture（逻辑已全验，缺归档式回归 fixture）；snapshot/sqlite/其它 provider 属 P3 后续。
- **P2 ⬜ 未开始**：QuotaBar 影子并跑 → diff `cache.db` 一致才切（feature flag）。
- **P3 ⬜ 未开始**：总库落地 + TumeFlow 消费；此后再按需实装 snapshot/sqlite/derived-path（各自补语料后从 `planned` 升 `stable`）。

> snapshot/sqlite/压缩/派生路径的 §11 用例是**契约预留 + 将来门槛**，不是 P0 的实现门槛——API 形状现在定全（避免后期撑破契约），实现按 provider 逐个补。

> 数据源完备性审计（基于 Claude Code / Codex 官方文档与源码）的缺口与架构风险已并入 §3–§8 并定型为 ADR-025；原始调研报告已移除，结论以本文与 ADR-025 为准。**核验 caveat**：该审计的路径/环境变量/语义为 **2026-06 快照**，会随上游版本漂移；未逐条附可复核 URL + 核验日期，故作**设计依据**可用、但**不作长期稳定契约**——所有候选路径仍走"先探测、后按 schema 解析、`planned`→`stable` 升级"（§3.4 / §4），把"官方确认"的稳定性约束交给探测与黄金语料，而非审计快照。
