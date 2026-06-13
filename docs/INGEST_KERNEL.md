# SessionVault — 共享摄取内核与 RawEvent 总库

状态：初始设计
最后更新：2026-06-13

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
| session | `projects/<编码项目路径>/<session-uuid>.jsonl` | 是 | 会话事件流，主数据源 |
| memory | `projects/<项目>/memory/*.md` | 是 | Auto Memory（v2.1.59+），可被 `autoMemoryDirectory` 改写 |
| history | `history.jsonl` | 是 | 命令历史 |
| instruction | 用户 `<config>/CLAUDE.md`；项目 `./CLAUDE.md`、`./.claude/CLAUDE.md`、`./CLAUDE.local.md`、`./.claude/rules/*.md`；托管 policy（平台相关） | 是 | 只读不改写；须解析 `@import`（≤4 跳）与 rules 的 `paths` 作用域 |
| config | `<config>/settings.json` 等 | 否 | 仅用于解析 `autoMemoryDirectory` / `cleanupPeriodDays`，不作事件 |

平台配置根：Windows `%USERPROFILE%\.claude\`；macOS / Linux `~/.claude/`；WSL 发行版内 `~/.claude/`。

### 3.3 Codex（status: stable）

配置根默认 `~/.codex`，可被 `CODEX_HOME` 覆盖。存在与语义随版本变化，**先探测再按 schema 解析**。

| 类别(kind) | 相对路径 / glob | 含正文 | 说明 |
|---|---|---|---|
| session | `sessions/YYYY/MM/DD/rollout-*.jsonl` | 是 | 日期分桶会话轨迹，按本机结构探测 |
| history | `history.jsonl` | 是 | 历史/会话转录，受 `history.persistence` / `max_bytes` 控制 |
| index | `state_*.sqlite` | 否 | 若存在仅作可选索引，不作唯一事实 |
| instruction | 全局 `$CODEX_HOME/AGENTS.override.md` 或 `AGENTS.md`（取首个非空）；项目自 Git 根到 cwd 逐级 `AGENTS.override.md` / `AGENTS.md` / `project_doc_fallback_filenames` | 是 | 只读；`.codex/config.toml` 是配置而非指令 |
| config | `$CODEX_HOME/config.toml` | 否 | 仅配置，不作事件 |

平台配置根：Windows `%USERPROFILE%\.codex\`；macOS / Linux `~/.codex/`；WSL 发行版内 `~/.codex/`。

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
    { kind, glob, parser, content: bool, override_setting? }
    # 例：{ session, "projects/*/*.jsonl", claude_jsonl, true }
    #     { memory,  "projects/*/memory/*.md", markdown, true, autoMemoryDirectory }
  ]
}
```

- **描述符是数据，不是分支逻辑**：发现器对所有 provider 跑同一套"探测根 → 匹配 glob → 绑定解析器"流程。
- **解析器是 trait**：`parse(bytes, cursor, profile) -> (events, cursor)`。Claude/Codex/通用 JSONL 各实现一份；Cursor 这类 SQLite 来源实现一个读 `state.vscdb` 的解析器即可，发现/游标/报告框架复用。
- 新 provider 先入库为 `planned` 描述符（仅占位、不默认扫），补完解析器 + 黄金 fixture（§11）后升级状态。
- provider 状态与 `RawEvent` 的 `parser_version` 绑定，格式漂移时显式失败而非静默错解。

## 5. 路径可配置（内置默认 + 用户覆盖）

内核自带默认目录，但**允许前端/用户填路径**，便于非标准安装、便携目录、额外 WSL 发行版或新 provider 抢先接入。优先级（高→低）：

1. **环境变量覆盖**：`CLAUDE_CONFIG_DIR` / `CODEX_HOME` 等（来自描述符 `env_override`）。
2. **用户配置覆盖**：宿主把一份用户配置传给内核，可启用/禁用 provider、增删根路径、登记新 provider 根。
3. **内置候选**：§3 默认路径，逐个探测，存在才扫。

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

档位是内核的**能力**，不再等于 QuotaBar 的隐私边界：采用共享 RawEvent 总库（§13 / ADR-020）后，总库以 `full` 物化（含正文），QuotaBar 已放开"不读正文"限制、经总库读取正文（见 ADR-021 / QuotaBar `AGENT_MEMORY_POSITION.md` §1.5）。`metadata` 档位保留用于不建总库的场景（例如只装 QuotaBar、或想要不含正文的轻量投影）。描述符中 `content: false` 的 artifact（如 config）在任何档位下都不出正文。

## 7. `RawEvent` 契约

```text
schema_version            # 内核归一化 schema 版本
source_type               # claude_code | codex | cursor | gemini | jsonl
source_location           # local | wsl:<distro>
source_path               # 源文件
source_session_id
seq                       # 文件内单调序号，用于排序 + 去重
occurred_at               # 事件在对话内发生的时间，UTC unix 秒；冲突裁决(latest-wins)的权威时间
time_confidence           # occurred_at 可信度：high | low（缺失/不可靠时 low，交下游处理）
actor                     # user | assistant | tool | system
event_type                # message | tool_use | tool_result | usage | meta
cwd
project_root              # 解析结果
project_root_source       # git | marker | cwd | none
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
parent_ref                # 父子关系（当前版本观察到 parentUuid）；内部字段，非稳定外部接口
content                   # 仅 full 档：原始可见正文（未脱敏）；metadata 档为空
content_hash
raw_reference             # 字节偏移 / 行指针，用于回溯源
parser_version            # 解析器版本，绑定字段语义
```

去重唯一键 = `(source_type, source_location, source_path, source_session_id, seq)`。`content_hash` 仅用于相似重复检测，不作全局唯一约束（见 `SYSTEM_DESIGN.md` §9.4）。`ingested_at`（入总库时间）与总库 `offset` 由**总库层在 append 时附加**，不由解析内核产出（解析内核无状态，见 §13.1）。

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
  safe_offset               # 已安全消费的字节
  size                      # 上轮文件大小
  mtime
  codex_state? {            # 仅 Codex：累计 token append 解析所需
    current_model
    current_effort
    current_cwd
    current_session_id
    previous_total
  }
}
```

推进规则（与 QuotaBar 实机验证一致）：

1. 只解析**完整 JSONL 行**；末行半截写入本轮不解析、不推进 `safe_offset`。
2. 单行坏 JSON 跳过该行、保留同文件其余好行、`status=error`，但**不推进** `safe_offset`，等待下轮重试。
3. `(mtime, size)` 未变且 `safe_offset >= size` 时跳过该文件。
4. metadata 回退（文件被截断 / 重写）→ `safe_offset` 归零，下轮从头重建。
5. **游标由调用方持久化**：QuotaBar 存自己的 `cache.db`，TumeFlow 存自己的库。内核本身不落盘——不同消费者的读取进度互不干扰，无锁竞争。

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
          bytes_total, bytes_new, events_emitted,
          cursor_advanced, status: ok|partial|skipped|error, error?,
          last_event_at } ],
      totals { sources, events, bytes_new } } ]
  warnings [ ... ]   # 路径不存在 / 权限拒绝 / 未授权 / 格式漂移
}
```

**GUI 放在宿主，不放内核**：

- QuotaBar 已是 Tauri 桌面应用，是这个扫描状态界面的**天然落点**——直接把 `ScanReport` 渲染成"provider → 根 → 文件 → 新字节/事件数/状态"的表，警告单列。
- TumeFlow 侧用 CLI 表格或简单 web 视图渲染同一份 `ScanReport` 即可，无需重做采集逻辑。
- 这样"一个简单 GUI 显示扫描了什么、情况如何"的需求由**共享的报告 schema + 各自的渲染**满足，仍是"代码一份"（采集一份、报告结构一份），渲染按产品风格各做各的。

建议宿主界面至少展示：本轮扫描的 provider 与生效根、每个文件读取的新字节/事件数、跳过/出错原因、未授权与格式漂移告警，以及"前端填路径/启停 provider"的设置入口（写回 §5 的用户配置）。

## 11. 黄金 fixture 语料（一致性套件，必须覆盖）

这是整套架构最值钱的资产：所有 tricky case 做成样例输入 + 期望 `RawEvent` / `ScanReport` 输出，消费者升级内核前必须全绿。

- Codex 累计 token 跨多次 append 的 delta 正确性（`previous_total`）。
- 末行半截写入 → 本轮不解析、`safe_offset` 不前进。
- 单行坏 JSON → 跳过该行、保留同文件好行、`status=error`、不推进。
- `(mtime, size)` 回退 / 文件被截断 → 归零重建。
- 同 `session_id` 在 `local` 与 `wsl:<distro>` 各一份 → 靠 `source_location` 区分，不互相覆盖。
- Claude `parentUuid` 分支 / 重试 / 编辑 → 父子树，区分采纳与废弃分支。
- Codex 会话中途 `cwd` 变化 → token 与项目归因正确。
- `cwd` 缺失 → `project_root_source=none`。
- 迟到入库：旧 `occurred_at` 事件以大 `offset` 追加 → 时间线按 `occurred_at` 重建，`offset` 不当时间用（latest-wins 不被冒充）。
- `occurred_at` 缺失 / 不可信 → `time_confidence=low`，不默认当"现在"。
- 项目根 `.git` vs marker vs `cwd` 三种来源 + 对应 confidence。
- 文件轮转 / 新会话文件出现 → 增量发现。
- 超大文件：只读 `safe_offset` 后新字节（尤其 WSL 9P，避免整文件重读）。
- 来源发现：内置默认 / 用户配置 / 环境变量三种路径来源的优先级与去重。
- 新 provider 描述符（如占位的 Cursor SQLite）→ 框架可发现、`planned` 不默认扫。
- `ScanReport` 字段：路径不存在、权限拒绝、未授权各产出对应 warning。

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
- **早期减负**：两个消费者用 git submodule pin 到某个 commit（QuotaBar 已用 submodule，顺手）；契约稳定后再走正式 registry（crates.io / PyPI）发版。

## 13. 集成形态：上游 RawEvent 总库 + 下游物化分库

解析内核本身无状态（§8 纯函数）。在它之上，SessionVault 仓库再提供一个**可选的持久化组件**——共享 **RawEvent 总库**（SessionVault 名字里的 "Vault" 即指这个永不删、不可变的库），把两个产品的集成收敛成"上游一个活库、下游各自物化"的形态。决策见 ADR-020 / ADR-021。

### 13.1 总库（活、最新、中立）

- **内容**：只存 `RawEvent` 契约，**append-only 不可变**；不含任何产品的领域表（`usage_facts` / 记忆都不入总库）。
- **归属**：SessionVault 仓库（中立），不归 QuotaBar，也不归 TumeFlow。
- **写者**：谁跑扫描谁写，同一时刻单写者；QuotaBar 常驻，天然当默认写者。开 WAL，读者不挡写。
- **版本**：一直跟随最新内核往前走，方便 QuotaBar 直接读用。
- **正文**：以 `full` 物化（含正文）。QuotaBar 已放开"不读正文"限制（ADR-021），故总库可承载正文供两边使用。
- **保留**：**永不删、不压缩、不过期**——总库是证据最终归宿（与 ADR-016 一致）；正因永不删，下游落后可随时全量重建。
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

**优先级**：P0 黄金语料 + `RawEvent` 定稿 → P1 crate 抽取过语料 → P2 QuotaBar 影子切换 → P3 总库落地 + TumeFlow 消费。
