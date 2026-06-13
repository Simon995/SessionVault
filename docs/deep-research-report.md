# SessionVault 数据源完备性审计与缺口分析

## 项目目标与 RawEvent 入库准则

从附件看，**SessionVault** 的职责不是“再做一个聊天记录查看器”，而是把各类本地 Agent 留下的原始证据统一发现、增量扫描、归一化成中立的 `RawEvent`，再落入一个**永不删除、append-only、不可变**的共享总库，让上层的 **TumeFlow** 和 **QuotaBar** 共用同一条摄取链路，避免重复踩路径发现、WSL、偏移游标、格式漂移之类的坑。它本身是无状态的，不存游标、不做 GUI、不联网；公开 API 的核心就是**来源目录 + RawEvent 契约 + 无状态游标 + 扫描报告**。

“什么样的数据值得进 `RawEvent`”，附件给出的标准也很清楚：这是一层**证据优先**、为**时间敏感的长期记忆**服务的中立事件层，而不是简单“搜历史”。总库默认面向 `full` 档位，允许保存原始正文，供 TumeFlow 在下游做脱敏、证据绑定和物化；冲突裁决不看入库顺序，而看事件真实发生时间 `occurred_at`，因为记忆系统需要重建**随时间演化的事实状态**，而不是谁后落库谁赢。DECISIONS 明确要求“每条长期记忆必须绑定可回溯证据”，README 也把“证据优先、时间一致、可复现”作为核心原则。 

## 当前已覆盖的数据源基线

以下是 `INGEST_KERNEL.md` §3 里**当前已经声明覆盖**的来源目录，我将把它当作本轮审计的基线。

| provider    | kind        | 当前已覆盖路径 / glob                                        | 含正文 | 备注                                                   |
| :---------- | :---------- | :----------------------------------------------------------- | :----- | :----------------------------------------------------- |
| Claude Code | session     | `projects/<编码项目路径>/<session-uuid>.jsonl`               | 是     | 主会话事件流                                           |
| Claude Code | memory      | `projects/<项目>/memory/*.md`                                | 是     | Auto Memory，文档里已注明 `autoMemoryDirectory` 可改写 |
| Claude Code | history     | `history.jsonl`                                              | 是     | 命令历史                                               |
| Claude Code | instruction | 用户 `<config>/CLAUDE.md`；项目 `./CLAUDE.md`、`./.claude/CLAUDE.md`、`./CLAUDE.local.md`、`./.claude/rules/*.md`；托管 policy | 是     | 已要求解析 `@import` 与 rules `paths`                  |
| Claude Code | config      | `<config>/settings.json` 等                                  | 否     | 仅用于解析路径与清理策略                               |
| Codex       | session     | `sessions/YYYY/MM/DD/rollout-*.jsonl`                        | 是     | 日期分桶会话轨迹                                       |
| Codex       | history     | `history.jsonl`                                              | 是     | 受 `history.persistence` / `max_bytes` 控制            |
| Codex       | index       | `state_*.sqlite`                                             | 否     | 仅作可选索引                                           |
| Codex       | instruction | 全局 `$CODEX_HOME/AGENTS.override.md` 或 `AGENTS.md`；项目自 Git 根到 cwd 逐级 `AGENTS.override.md` / `AGENTS.md` / fallback filenames | 是     | 已覆盖 AGENTS 体系                                     |
| Codex       | config      | `$CODEX_HOME/config.toml`                                    | 否     | 仅配置，不作事件                                       |

文档同时把默认配置根写成：Claude Code 为 `~/.claude`（可由 `CLAUDE_CONFIG_DIR` 覆盖），Codex 为 `~/.codex`（可由 `CODEX_HOME` 覆盖）；Windows/WSL/macOS/Linux 目前都以这个“home 下隐藏目录”的模式展开。 

## Claude Code 缺口分析

先给结论：**Claude Code 现在的来源目录已经覆盖了“最主要的对话与记忆正文”，但还没有覆盖大量会直接塑造 Agent 行为的本地控制面**，尤其是 **commands / agents / skills / plugins / tasks / hooks 相关旁路状态**。如果 SessionVault 的目标是“上层记忆系统可重建 Agent 为什么会那样做”，这些缺口里有几项必须前置纳入来源目录。官方文档确认 Claude Code 的配置根默认是 `~/.claude`，可由 `CLAUDE_CONFIG_DIR` 覆盖；官方还确认项目与用户层存在 `CLAUDE.md`、skills、plugins、agent view 状态、task list、worktrees 等多条本地状态面。

为避免表格横向过宽，下面统一记号如下：
**Claude 用户配置根 `CROOT`** = Windows 原生 `%USERPROFILE%\.claude\`；macOS `~/.claude/`；Linux `~/.claude/`；WSL 为发行版内 `~/.claude/`。这是对官方 `~/.claude` / `CLAUDE_CONFIG_DIR` 的平台展开；其中“Windows `%USERPROFILE%` 形式”是按 home 语义推导，属于**较可信**而非官方逐字写明。若设置 `CLAUDE_CONFIG_DIR`，以下所有 `CROOT` 都替换为该目录。

| provider    | kind                                 |    是否含对话正文 | 四平台路径 / glob                                            | 环境变量                                                     | 格式                               | 为什么有价值                                                 | 置信度 / 出处                                                | 版本漂移风险 |
| :---------- | :----------------------------------- | ----------------: | :----------------------------------------------------------- | :----------------------------------------------------------- | :--------------------------------- | :----------------------------------------------------------- | :----------------------------------------------------------- | :----------- |
| Claude Code | instruction                          |                否 | Windows `%USERPROFILE%\.claude\commands\*.md`；macOS/Linux `~/.claude/commands/*.md`；WSL `~/.claude/commands/*.md`；项目 `./.claude/commands/*.md` | `CLAUDE_CONFIG_DIR`                                          | Markdown                           | **高价值**。自定义 slash 命令会直接改变用户可调用工作流，是“为什么这次会话调用了特定流程”的强证据；对记忆系统尤其是“偏好/流程性记忆”重要。 | **较可信**。官方 commands 文档与 plugin 迁移文档明确提到 `.claude/commands/`。 | 中           |
| Claude Code | instruction                          |                否 | Windows `%USERPROFILE%\.claude\agents\*.md`；macOS/Linux `~/.claude/agents/*.md`；WSL `~/.claude/agents/*.md`；项目 `./.claude/agents/*.md` | `CLAUDE_CONFIG_DIR`                                          | Markdown + YAML frontmatter        | **高价值**。自定义 subagent 定义直接改变系统提示、工具限制、模型选择，属于“行为 shaping 证据”，对长期记忆比单次对话更稳定。 | **较可信**。官方 subagents / plugins 文档确认用户与项目层 custom agents 存在，并可被 plugin 覆盖。 | 中           |
| Claude Code | instruction / memory / plugin bundle |        部分含正文 | 用户技能目录：`CROOT/skills/**`；项目插件若以 skills-dir 方式存在则在对应插件根下 `skills/**/SKILL.md`、`agents/**`、`commands/**`、`hooks/hooks.json`、`.mcp.json`、`.lsp.json`、`monitors/monitors.json`、`settings.json` | `CLAUDE_CONFIG_DIR`，以及 `CLAUDE_CODE_SYNC_SKILLS`          | Markdown / JSON                    | **必须补**。官方明确支持把插件放进 `~/.claude/skills/<plugin>/` 自动加载；且 `CLAUDE_CODE_SYNC_SKILLS=1` 会把 claude.ai 上启用的 skills 下载到 `~/.claude/skills/`。这意味着 skills 目录不只是“技能提示词”，还是**插件分发载体**。 | 高                                                           |              |
| Claude Code | config / plugin state                |                否 | 默认 `CROOT/plugins/**`                                      | `CLAUDE_CODE_PLUGIN_CACHE_DIR`、`CLAUDE_CODE_PLUGIN_SEED_DIR` | Git checkout / cache / JSON / 其他 | **应纳入来源目录，至少作为解析辅助源**。官方写明 plugins root 默认在 `~/.claude/plugins`，marketplaces 和 plugin cache 都在其下；它决定安装了哪些插件、从哪个市场来、哪些能力会进入会话。即便不直接转成 `RawEvent`，也至少要读来解析“安装能力面”。 | 中高                                                         |              |
| Claude Code | state                                |            不确定 | 默认 `CROOT/tasks/<task-list-id>/**`                         | `CLAUDE_CODE_TASK_LIST_ID`                                   | **未公开**                         | **高价值但需要描述符预留**。官方确认共享任务列表会使用 `~/.claude/tasks/` 下的命名目录；任务会跨 compaction 持久化，并可跨会话共享。对于记忆系统，这属于“计划/待办状态”的直接本地证据。文件格式未公开。 | 高                                                           |              |
| Claude Code | state / filesystem artifact          |              间接 | 项目根 `./.claude/worktrees/**`                              | `worktree.bgIsolation`                                       | Git worktree + 普通文件            | **建议纳入但默认不全文摄取**。官方确认 background sessions 在编辑前会迁入 `./.claude/worktrees/`；这对“检查点/隔离编辑快照/并行 agent 结果”很重要，但它更像**派生工作目录**，不是天然日志。更适合作为 snapshot / provenance 源，而不是整目录全文入库。 | 中                                                           |              |
| Claude Code | config                               |                否 | 项目根 `./.mcp.json`；以及 `CROOT/settings.json` / 项目 `.claude/settings.json` | `CLAUDE_CONFIG_DIR`                                          | JSON                               | **必须补**。这类文件虽然不应默认入 `RawEvent` 正文，但必须读来解析 MCP、hooks、生效路径、`autoMemoryDirectory`、worktree 行为等；否则会错过真实数据根。官方 debug 文档明确把 `.mcp.json` 视为项目配置面的一部分。 | 低中                                                         |              |
| Claude Code | telemetry / forensic log             |    可能含完整正文 | 任意目录 `<dir>/**`，当 `OTEL_LOG_RAW_API_BODIES=file:<dir>` 时启用 | `OTEL_LOG_RAW_API_BODIES`、`OTEL_LOG_USER_PROMPTS`、`OTEL_LOG_TOOL_CONTENT` 等 | JSON / log                         | **低优先但不可忽视**。官方明确说可把完整请求/响应 JSON 直接写到磁盘，且 bodies 包含整个会话历史。这对取证极有价值，但因为路径完全由环境变量决定，不适合当静态默认来源；更适合后续做“条件来源族”。 | 高                                                           |              |
| Claude Code | opaque session state                 | 可能含正文 / 状态 | `CROOT/**` 下的未公开 supervisor / background-session state  | `CLAUDE_CONFIG_DIR`                                          | 未公开，可能 JSON / SQLite / 其他  | **重要缺口，但目前不能硬编码**。官方确认 agent view 的 background session state 持久化在 Claude Code config directory 下，并跨重启/睡眠保留；但没有公开具体文件名。建议把它作为“保留来源族”，先不纳入固定路径表。 | 很高                                                         |              |

对 Claude 的审计结论是：**最该补的不是再多找一类 transcript，而是补“行为塑形文件”**。也就是 `commands / agents / skills / plugins / tasks / .mcp.json / settings` 这一整组。它们很多不一定进入 `RawEvent` 的“正文型事件”，但至少要成为**被扫描、被版本化、可参与解释路径解析和行为归因**的正式来源族。否则 TumeFlow 只能记住“说了什么”，却解释不了“为什么模型会按这种工作流做事”。

## Codex 缺口分析

Codex 的情况比 Claude 更敏感：**OpenAI 官方文档已经明确把 `CODEX_HOME` 描述为承载 config、auth、logs、sessions、skills 以及 standalone package metadata 的总根目录**，同时又单独引入了 `CODEX_SQLITE_HOME` 用于 SQLite-backed state。换句话说，当前来源目录虽然已覆盖 `history.jsonl`、AGENTS、`config.toml` 和一个假定的 `sessions/YYYY/MM/DD/rollout-*.jsonl`，但**还没有把 Codex 的控制面与状态面完整建模出来**。而且，其中一部分已经不再是 append-only JSONL 语义。

统一记号如下：
**Codex 用户根 `XROOT`** = Windows 原生 `%USERPROFILE%\.codex\`；macOS `~/.codex/`；Linux `~/.codex/`；WSL 为发行版内 `~/.codex/`。若设置 `CODEX_HOME`，则统一替换为该目录。
**Codex SQLite 根 `XSQL`** = `CODEX_SQLITE_HOME` 指向的目录；若未设置，则默认跟随 `CODEX_HOME`。Windows `%USERPROFILE%` / WSL `~` 的写法同样是对官方 `~/.codex` / `CODEX_HOME` 的平台展开，因此在“绝对 Windows 形式”上属于**较可信推导**。

| provider | kind                                      | 是否含对话正文 | 四平台路径 / glob                                            | 环境变量                             | 格式                 | 为什么有价值                                                 | 置信度 / 出处 | 版本漂移风险 |
| :------- | :---------------------------------------- | -------------: | :----------------------------------------------------------- | :----------------------------------- | :------------------- | :----------------------------------------------------------- | :------------ | :----------- |
| Codex    | config                                    |             否 | `XROOT/config.toml` 之外，还应覆盖项目 `.codex/config.toml`、profile `XROOT/<profile>.config.toml`；Unix 还应覆盖 `/etc/codex/config.toml` | `CODEX_HOME`，`--profile`            | TOML                 | **必须补**。官方明确有配置层级：项目层、profile 层、用户层、Unix system 层。你现在只收用户 `config.toml`，这会漏掉项目级 MCP、hooks、rules、fallback filenames、profiles。 | 中            |              |
| Codex    | instruction / agent definition            |             否 | Windows `%USERPROFILE%\.codex\agents\*.toml`；macOS/Linux `~/.codex/agents/*.toml`；WSL `~/.codex/agents/*.toml`；项目 `./.codex/agents/*.toml` | `CODEX_HOME`                         | TOML                 | **必须补**。官方明确支持 personal agents 和 project-scoped custom agents，文件本身可以覆盖模型、sandbox、MCP、skills.config 等，实质上是“agent persona + capability bundle”。 | 中            |              |
| Codex    | hooks                                     |             否 | Windows `%USERPROFILE%\.codex\hooks.json`；macOS/Linux `~/.codex/hooks.json`；WSL `~/.codex/hooks.json`；项目 `./.codex/hooks.json`；同时旁读同层 `config.toml` 里的 `[hooks]` | `CODEX_HOME`                         | JSON / TOML          | **必须补**。这是 Codex 行为控制面；官方明确支持 `hooks.json` 与 inline hooks，且 project-local hooks 受 trust 控制。你若不收，会漏掉大量自动化 side-effect 证据。 | 低中          |              |
| Codex    | rules                                     |             否 | Windows `%USERPROFILE%\.codex\rules\*.rules`；macOS/Linux `~/.codex/rules/*.rules`；WSL `~/.codex/rules/*.rules`；项目 `./.codex/rules/*.rules` | `CODEX_HOME`                         | Rules DSL 文本       | **必须补**。官方确认 `rules/` 会在每个 active config layer 下被扫描，且 UI 中用户允许命令时会直接写到 `~/.codex/rules/default.rules`。这已经不是“配置”，而是**持续学习出来的权限/流程记忆**。 | 低中          |              |
| Codex    | skills                                    |             是 | 用户 `%USERPROFILE%\.agents\skills\**\SKILL.md` / `~/.agents/skills/**/SKILL.md`；仓库 `$REPO_ROOT/.agents/skills/**/SKILL.md`；父目录 `$CWD/../.agents/skills/**/SKILL.md`；当前目录 `$CWD/.agents/skills/**/SKILL.md`；Unix 管理员 `/etc/codex/skills/**/SKILL.md` | 无单独 env；受 repo root 发现影响    | Markdown             | **必须补**。这是你当前来源目录里最大的 Codex 缺口。官方把 skills 明确分成 REPO / USER / ADMIN / SYSTEM 多个作用域，而且它们直接决定 Codex 的作业套路。对记忆系统而言，这是“程序性记忆”的一等公民。 | 中            |              |
| Codex    | state / transcript                        |             是 | `XROOT/history.jsonl`                                        | `CODEX_HOME`                         | JSONL                | 已覆盖，但要改扫描语义。官方明确 `history.max_bytes` 超限后会丢弃最旧记录并压缩文件，因此它**不是严格 append-only**。 | 中            |              |
| Codex    | session / local state family              |     可能含正文 | `XROOT/sessions/**`（官方只确认有 sessions，不公开完整层级）；你当前的 `sessions/YYYY/MM/DD/rollout-*.jsonl` 应视为“已观察结构”，而非公开稳定承诺 | `CODEX_HOME`                         | JSONL / 未公开       | **高优先保留，但应降级置信度**。官方文档只说 `CODEX_HOME` 下有 sessions；没有对你当前 catalog 中的分桶层级作稳定承诺。因此这个来源家族要保留，但 exact glob 需要按“探测后解析”而不是把公开契约写死。 | 很高          |              |
| Codex    | SQLite-backed state                       |         不确定 | `XSQL/**/*.sqlite*` 或 `XROOT/**/*.sqlite*`                  | `CODEX_SQLITE_HOME` 或 `sqlite_home` | SQLite               | **写代码前必须在架构上预留**。官方明确存在 SQLite-backed state，并允许单独重定向目录；你当前基线只写了 `state_*.sqlite`，范围过窄、证据不足。至少要把它提升成“SQLite 状态根”这一来源族。 | 高            |              |
| Codex    | MCP config                                |             否 | 用户 `XROOT/config.toml`；项目 `./.codex/config.toml`；部分插件走 `[plugins."<id>".mcp_servers.*]` | `CODEX_HOME`                         | TOML                 | 基线虽有 `config.toml`，但**尚未把其作为 MCP 解析源完整建模**。官方明确 MCP 配置就在 config.toml，CLI 和 IDE 共享；插件还能注入 MCP server，并由用户 config 控制 on/off 与 tool policy。 | 中            |              |
| Codex    | plugin state / plugin-controlled behavior | 可能含指导正文 | `XROOT/config.toml` 中 `[plugins.*]`；本地插件 bundle/cache 路径官方未公开 | `CODEX_HOME`                         | TOML + 未公开 bundle | **应占位，但先不要硬编码 bundle 路径**。官方公开了插件启停配置和 plugin-provided MCP/hook/skill 行为，但没有公开一个稳定的本地 bundle 路径契约。适合先把“插件配置层”纳入，bundle 目录后补。 | 很高          |              |
| Codex    | auth                                      |             否 | `XROOT/auth.json`（仅当使用 file-based credential storage）  | `CODEX_HOME`                         | JSON                 | **建议明确“不收正文、不默认读取”**。官方确认它可能存在，但它是凭据面，不是记忆证据；最多只用来做安全排除规则，不应进 RawEvent。 | 低            |              |

Codex 这里最关键的审计结论有两条。第一，**skills / hooks / rules / agents / profile config / SQLite state** 都是目前来源目录的明显缺口，而且它们中的前四项对“记忆系统”不是边角料，而是核心上下文。第二，**你当前把 Codex session 精确绑到 `sessions/YYYY/MM/DD/rollout-\*.jsonl` 的做法，官方公开证据不够强**；更稳妥的做法是把它改成“`CODEX_HOME` 下 sessions 家族 + schema 探测”，把当前观察到的 rollout JSONL 作为一个 parser 候选，而不是作为不会变的公共契约。

## 形态会冲击 SessionVault 假设的地方

你特别问的“架构风险”，我建议分成四类看。

**第一类是可覆盖型文本配置**，例如 Codex 的 `AGENTS.md` / `AGENTS.override.md` / fallback files、Claude 的 `CLAUDE.md` / rules、两边的 `config.toml` / `settings.json` / `hooks.json` / `rules/*.rules` / custom agents / skills。这些文件**不是 append-only 日志**，经常是整体重写或覆盖，因此“无状态游标 + 按字节增量读文件”的主假设不够。要容纳它们，最小改动不是改 `RawEvent`，而是给来源目录新增**source mode = snapshot**，游标改成“内容哈希 / mtime+size / inode fingerprint”，解析器在变化时输出“配置快照变更事件”而不是“字节追加事件”。这属于**要动来源目录与游标类型，但不一定要动 RawEvent 主契约**。

**第二类是会被压缩或裁剪的 JSONL**。最典型的是 Codex `history.jsonl`：官方明确说超过 `history.max_bytes` 后会丢弃最旧记录并压缩文件，因此你不能把它当成永远只增长的 append log。这里如果继续只靠 `safe_offset`，一旦文件重写，你就会错过新旧边界，甚至把旧文件尾当成新事件。要容纳它，只需要新增**truncation / rewrite detection**：例如“文件指纹变了就重算尾部窗口，并用 event-id / occurred_at / hash 去重”。这类改动主要在**游标层**，来源目录与 RawEvent 可以不动。

**第三类是 SQLite 或其它未公开内部状态库**。Codex 已公开有 `CODEX_SQLITE_HOME` 指向的 SQLite-backed state；Claude 的 background supervisor/session state 也确定持久化在 config dir 下，但具体文件名未公开。这里你的“按字节增量读文件”假设基本不成立。若这些库只作为**索引 / 加速辅助**，那么加一个解析器就够了，`RawEvent` 仍可保持不变；但如果你要把它们作为**一等证据源**，那公开游标契约大概率要从“byte offset”扩成“per-source cursor”，允许 `sqlite_rowid / wal_lsn / content_hash / byte_offset` 等多种游标形态共存。换言之，**这类会逼你改公开契约**，应在写代码前就预留。

**第四类是路径不固定、需要先解析配置/环境变量才能知道去哪扫的来源**。Claude 的 `CLAUDE_CONFIG_DIR`、`CLAUDE_CODE_PLUGIN_CACHE_DIR`、`CLAUDE_CODE_PLUGIN_SEED_DIR`、`CLAUDE_CODE_TASK_LIST_ID`，以及 Codex 的 `CODEX_HOME`、`CODEX_SQLITE_HOME`、profile 选择、project trust，都属于这一类。它们不会强迫你改 RawEvent，但会要求来源目录不再只是“静态路径表”，而要支持**derived path descriptor**：先读取一层 config/env，算出真实根，再派生子来源。这属于**来源目录解析层需要前置升级**，否则后续会被迫在 consumers 里塞路径逻辑，正好违背你的设计目标。

把它压缩成一句话：
**“再加几个描述符和解析器”就够的，是 append-log JSONL、普通全量快照文本、以及需要少量跨文件 join 的控制文件；真正会逼你改公开契约的，是 SQLite/opaque state，以及把全部来源统一假定成 byte-offset-log 这件事本身。**

## 写代码前必须补的点与可后置的点

我建议把优先级分成两层。

**写代码前必须补进来源目录 / 架构预留的**，有六项。
其一，给来源目录增加**source mode** 概念，至少区分 `append_log`、`snapshot_file`、`sqlite_store`、`opaque_family`，否则后面一接 rules / hooks / agents / tasks / sqlite 就会动核心层。其二，Codex 必须前置补上 **hooks.json / rules/\*.rules / .codex/agents/\*.toml / .agents/skills/** 这四类。其三，Claude 必须前置补上 **commands / agents / skills / plugins-root / tasks / .mcp.json**。其四，Codex 要把**project config、profile config、Unix system config**纳入“解析辅助来源”，哪怕默认不产出正文事件。其五，要把 `CODEX_SQLITE_HOME` 与 Claude/Codex 的各种 config-dir override 变成正式的**derived-path resolver**。其六，Codex session family 的 public wording 要改成“探测 + schema 解析”，不要把 `sessions/YYYY/MM/DD/rollout-*.jsonl` 写成牢不可破的稳定契约。

**可以先占位、以后再说的**，也有几项。
Claude 的 OTEL `file:<dir>` 原样请求/响应落盘、`/heapdump` 生成的快照文件、background supervisor 的未公开内部状态文件、Codex 未公开的 plugin bundle/cache 目录，都值得在来源目录中加“保留来源族”说明，但不建议现在就把具体 glob 写死。它们有价值，却要么路径完全由运行时决定，要么官方没有稳定承诺，要么更接近运维/取证而非核心长期记忆。把这些放进 v1 的强契约，后面反而更容易因为版本漂移而被迫大改。

如果你要据此直接更新 `INGEST_KERNEL.md` 的来源目录，我的建议是把 §3 从“provider × relative path”升级成两层：
一层写**来源族**（session log / snapshot instruction / rules / hooks / skill root / plugin root / sqlite state root / derived task root）；
一层写**当前已验证实现**（例如 Claude `history.jsonl`、Codex `history.jsonl`、Codex observed rollout JSONL parser）。
这样你既能在 v1 先把架构钉稳，又不会因为某个 provider 改了具体子目录，就把整个公开契约拖成 major 变更。 

## 开放问题与本轮限制

本轮能**已核验**的，主要来自官方文档与 OpenAI/Anthropic 官方仓库文档页；其中 Codex 的 config / env / rules / hooks / skills / subagents / AGENTS.md 这条线证据相对完整，Claude 的 skills / plugins / tasks / agent-view / env vars 也较扎实。

仍然**没有公开稳定核验到 exact path** 的，主要有三类：Claude background supervisor/session state 的具体文件名；Codex plugin bundle 的本地安装目录；以及你目录里当前写死的 Codex `sessions/YYYY/MM/DD/rollout-*.jsonl` 是否仍是官方愿意承诺的稳定路径。我在报告里已经把这三类都降成了“较可信 / 待继续源码核验 / 只应保留来源族，不宜写死契约”，建议你在 `INGEST_KERNEL.md` 中也同步标注。