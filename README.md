# SessionVault

> 共享摄取内核（shared ingestion kernel）+ 加密 RawEvent 总库。
> 把"发现各 Agent 本地数据 → 增量扫描 → 归一化为 `RawEvent` → 追加保存到总库"
> 这件最容易踩坑的事，**只实现一次**；用户主动擦除受墓碑与防复活规则保护。

**现状（2026-11-11 核对）**：Claude Code / Codex 本地与 WSL 摄取、状态快照、总库正文加密、用户擦除、项目归属及多机副本读取已实现。最新正式版 `v2026.11.11.1`（`b9339c3`）包含控制面绑定与双层临时排除、DSH v4 复杂语义和细粒度用量证据、会话有效视图标记、元数据窄查询，以及准入、游标和完整投影的一致性修复。显式配置的 DSH 来源复用 `Jsonl` + `AppendLog` 接入常规 `catalog` / `discover` / `scan-all`，未配置时不扩大发现范围。Windows / Linux 发布资产已回下载核验，本机独立 CLI 已安装并核对版本与哈希。

**接下来**：新版 CLI 已发布，本机登记写者已完成安装与运行身份核验；真实来源接入仍单独验收。消费方须完整持久化游标并保留证据字段，正式启用新证据前确认所有整代重写入口均使用新版。旧版 CLI 能结构读取新事件，但不会输出未知的 `usage_evidence`；旧版整代重写会丢失证据，因此接入前需升级所有可能重写事件的写者。配置方式见下文，证据与兼容边界见 [读取状态设计](docs/dsh-read-state-design.md) §14–§15；任务顺序只维护在 [待做清单](docs/backlog.md)。

历史抽取 / 迁移记录保留在下文，均按当时日期理解。消费方的迁移收尾、发布与实际接入由各仓维护，本仓不据历史记录推断它们的当前进度。

---

## 是什么

**SessionVault** 是一个独立仓库的 **Rust crate**（`session-vault`）+ CLI（`svault`），
中立、无头，不属于任何单一消费者。主要消费者包括：

- **[QuotaBar](https://github.com/Simon995/QuotaBar)** —— 配额/用量监控桌面应用，是总库的默认常驻写者。
- **[TumeFlow](https://github.com/Simon995/TumeFlow)**（记忆流，Memory Flow）—— 本地优先的 Agent 记忆系统：当前认知持续向前演化，历史变化始终可追溯（`supersede ≠ delete`）。从总库物化自己的固化分库。

此外，只读 CLI 消费者可使用已发布的原文读取、路径归属和擦除记录出口。消费方自行维护领域索引、记忆和界面；本仓统一扫描与来源语义，避免重复实现 Codex 累计 token / `safe_offset` / WSL 桥 / 路径发现。

## 现状（已实现）

| 模块                            | 职责                                                                                                                                                                                                                                                                                                                                                                                        | 状态                                                                |
| ------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------- |
| `catalog`                       | 声明式来源目录（provider × 子目录 × 形态）                                                                                                                                                                                                                                                                                                                                                  | ✅ Claude / Codex / 显式配置的 DSH v4（`v2026.11.11.1`）            |
| `discover`                      | 发现本地 + WSL 来源清单（不读内容）                                                                                                                                                                                                                                                                                                                                                         | ✅ 本地 + WSL                                                       |
| `parser`                        | 行级 JSONL → `RawEvent`（Claude / Codex 字段映射、Codex 累计 token delta）                                                                                                                                                                                                                                                                                                                  | ✅                                                                  |
| `pathnorm`                      | 宿主感知路径规范化（`HostPlatform`、UNC↔规范形、`workspace_location`）                                                                                                                                                                                                                                                                                                                      | ✅                                                                  |
| `wsl`                           | WSL 访问桥（`wsl.exe` 枚举/`find`/`stat`/`tail`，UTF-16LE 解码）                                                                                                                                                                                                                                                                                                                            | ✅ 实机实测                                                         |
| `scan`                          | 增量扫描：append_log 字节游标；snapshot_file SHA-256 指纹                                                                                                                                                                                                                                                                                                                                   | ✅ append_log + snapshot_file                                       |
| `compressed`                    | Codex 就地压缩的 `*.jsonl.zst`：按逻辑路径 `*.jsonl` 发现，偏移按解压后的字节算（纯 Rust 解压）                                                                                                                                                                                                                                                                                             | ✅ 2026-10-20 本机 88 个实测                                        |
| `session_binding`               | 控制面会话绑定（`SessionBindingRegistry`）、三态准入（`PersistentAllowed` / `TemporaryExcluded` / `UnknownHold`）、扫描与总库双层临时会话排除、子会话血缘传递及 `RawEvent::input_origin` 同侪信封分类                                                                                                                                                                                       | ✅ 已发布（`v2026.11.09.1` / `a151997`）                            |
| `dsh_snapshot` / `dsh_provider` | DSH v4 物理与原生核验、选代、旧前缀与生命周期、复杂语义、细粒度用量及正式扫描接线                                                                                                                                                                                                                                                                                                           | ✅ `v2026.11.11.1`；扫描要求 `store` 与显式配置，物理快照实验门不变 |
| `cursor`                        | 多形态游标（字节偏移 / fingerprint + Codex 状态 + `next_seq`）                                                                                                                                                                                                                                                                                                                              | ✅                                                                  |
| `svault` CLI                    | 子命令（NDJSON 出 stdout）：`discover` / `scan-all` / `pull` / `sessions-recent` / `sessions-read` / `session-origin` / `snapshots` / `sync-snapshots` / `roots` / `attribute` / `memory-roots` / `store-path` / `store-info` / `stores` / `erasures` / `adopt-erasures` / `key-export` / `key-import` / `changes` / `gc` / `erase` / `reattribute` / `reclaim-rollbacks`；跨运行游标持久化 | ✅ 已发布（`v2026.11.11.1`，发布记录见待做清单 B 节）               |
| `parity` 工具                   | P2 影子并跑 diff：QuotaBar `usage_facts` ⇄ RawEvent(usage)（`required-features=["parity"]`）                                                                                                                                                                                                                                                                                                | ✅ 首测 must-match=0                                                |
| 总库（持久化输出库）            | 逻辑追加、正文加密、投影读取、墓碑擦除与防复活                                                                                                                                                                                                                                                                                                                                              | ✅ `store` feature                                                  |
| snapshot_file                   | Claude/Codex memory、rules、项目 instruction 状态快照                                                                                                                                                                                                                                                                                                                                       | ✅ experimental（Windows/WSL + 最新快照查询）                       |
| sqlite_store / 其它 provider    | 契约预留                                                                                                                                                                                                                                                                                                                                                                                    | ⬜ `planned`                                                        |

**实机实测（2026-06-14，真实本机数据）**：48 来源（19 local + 29 WSL）→ 23373 事件
（8102 来自 WSL）；二次扫描借持久化游标 23476 → **0** 事件（全 cache-hit），增量端到端闭环。

## 核心设计

| 维度                | 约定                                                                                                                                                                                                        |
| ------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **来源目录**        | 声明式单一事实源：要扫哪些 provider、每平台扫哪些路径，集中维护一份（现 Codex / Claude Code，后续 Cursor / Gemini）。                                                                                       |
| **provider 可扩展** | 扩展描述符与解析器；涉及公开来源类型、字段或值域时，必须核消费方构造、穷举及逐字段读取的兼容性。                                                                                                            |
| **路径可配置**      | 现有配置根环境变量与内置候选；任意宿主 `user_config` / 新增 provider 根仍是设计目标，不能视为现成接口。                                                                                                     |
| **宿主感知路径**    | `pathnorm` 把「裸 Unix 绝对路径如何归属」按宿主平台分叉：Windows 宿主默认 WSL、Unix 原生判 local——修掉了 QuotaBar 内建的「裸 `/abs` 一律当 WSL」假设。Unix 路径语义在 Linux 原生与 WSL 内部共用同一套函数。 |
| **无状态内核**      | 扫描 / 解析层读来源与游标，返回事件、新游标和报告；不写总库、不持久化游标、不联网。游标由宿主持久化，CLI 提供 `--state`；同仓可选 `store` 组件另管总库。                                                    |
| **RawEvent 契约**   | 归一化事件 schema；`seq` 为解析序，跨解析器升级的稳定事件引用使用 `event_key`。当前 DTO 与总库存储身份分别见内核设计 §7 和扫描状态模型。                                                                    |
| **时间语义**        | 每条带 `occurred_at`（对话内时间，冲突裁决权威）+ `time_confidence`；latest-wins 只认 `occurred_at`，不认入库顺序 / offset。                                                                                |
| **扫描报告**        | 内核无头，但每轮产出结构化 `SourceReport`，由宿主（QuotaBar / TumeFlow）渲染 GUI。                                                                                                                          |
| **黄金 fixture**    | tricky case 做成样例输入 + 期望输出，消费者升级内核前必须全绿——"坑只有一处"的物理保证。                                                                                                                     |

## 总库 / 分库

扫描内核之上提供可选持久化组件 **RawEvent 总库**（`store` feature）：保存加密的归一化事件、投影与墓碑，不承担下游领域模型。正文加密、旧库迁移、用户擦除及删除防复活已实现，消费方通过 `pull` / `changes` / `sessions-read` 等出口维护自己的索引或记忆。

`svault scan-all` 默认输出 NDJSON 事件与报告；带 `--write-store` 才写总库，此时以写入报告替代逐条事件输出。原始总库留在来源主机的使用方式，与复制整库作为只读副本的方式分别按契约接入，后者不是本地读取的前置。

- **总库（活、最新、中立）**：保存 `RawEvent` 与投影、墓碑等存储元数据，默认追加保留，不自动过期；用户主动擦除与受判据保护的旧代回收是显式操作。以 `full` 物化正文，当前未做入库前压缩；QuotaBar 常驻当默认写者。
- **TumeFlow 分库（物化、固化、可复现）**：按自己节奏从总库增量拉取并物化，两次同步之间冻结，
  盖复现戳（`总库 offset + RawEvent schema 版本`）；重建仍须遵守当前投影与墓碑，不能仅凭 offset 假定已擦除的正文永远可恢复。

## 标准化接口

Rust lib 与 CLI 复用摄取实现，接口粒度不同；PyO3 wheel 尚未交付：

| lib 接口                                          | 用途                                                                                                                                                                 |
| ------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `catalog()`                                       | 生效后的 provider 描述符列表（宿主据此渲染设置页）                                                                                                                   |
| `discover()`                                      | 发现来源清单（本地 + WSL），首次只发现不读内容                                                                                                                       |
| `discover_transcripts()` / `discover_snapshots()` | 分离会话与状态制品，防止 snapshot 污染会话投影                                                                                                                       |
| `discover_project_snapshots()`                    | 在宿主已确认的项目根内发现 CLAUDE.md / AGENTS.md；身份由宿主提供                                                                                                     |
| `scan(source_ref, cursor_in, profile, roots)`     | 单来源增量摄取主接口（无状态：游标进、游标出）。`roots` 是已知项目根注册表 —— 归属的唯一输入，**空表 = 一个根都不知道 ⇒ 每条路径 `Unattributed`**，不是退回 cwd 兜底 |
| `TotalStore::sync_snapshots()`                    | 快照增量扫描并加密写入总库                                                                                                                                           |
| `TotalStore::read_active_latest_snapshots()`      | 每个来源的当前最新快照，删除文件不再返回                                                                                                                             |

CLI：

- `svault discover` —— 列出来源（NDJSON，不读内容）。
- `svault scan-all --profile metadata|full [--state <file> | --stateless]` —— 一轮增量扫描，
  逐条吐 `RawEvent` + 每来源 `SourceReport` + 汇总。游标默认持久化到
  `<data_local_dir>/svault/cursors.json`（`--state` 覆盖、`--stateless` 关），跨运行真增量。
  退出码：`0` 成功 / `1` 发现失败 / `2` **游标保存失败**（`summary.state_saved=false`，
  本轮增量游标未推进，下游应重试或预期重复）。
- `svault snapshots [--store <path>]` —— 输出每个 snapshot source 的当前最新版本；
  供 TumeFlow Class-B 主路径消费，不要求从总库头扫描历史。不在列表里的最新快照
  各出一行 `snapshot_excluded`，带原因：`source_gone`（确认不在）/ `decode_failed`
  （没读成，**不是**删了）。**判「已删」要凭 `source_gone`，不能凭「不在列表里」** ——
  本仓从没采到的来源也不在列表里。

## DSH 来源配置（当前开发分支）

使用带 `store` feature 的构建，并在运行进程中显式配置：

| 环境变量                  | 要求                                                         |
| ------------------------- | ------------------------------------------------------------ |
| `SVAULT_DSH_SESSIONS_DIR` | 本机绝对会话根，直接包含各会话目录；优先于 `SVAULT_DSH_HOME` |
| `SVAULT_DSH_HOME`         | 可选替代，读取其 `sessions` 子目录                           |
| `SVAULT_DSH_NODE`         | 可信本机 Node 可执行文件的绝对路径                           |
| `SVAULT_DSH_PACKAGE`      | 可信本机 `@deepseek-ai/dsh/package.json` 的绝对路径          |

未配置根时不发现 DSH。配置后沿用 `svault discover` 与 `svault scan-all`，来源类型仍为 `jsonl`、模式仍为 `append_log`，`source_path` 是稳定的会话目录。仅摄取最高规范格式代为 v4 的会话；旧代、未来代、冲突编码、坏尾、缺少运行时或核验失败会报告并冻结旧状态与投影。`--only jsonl` 同时包括显式配置的通用 JSONL 来源，不是 DSH 独占过滤。Windows 进程只接本机配置根；发行版内扫描需在 Linux 进程内配置对应根与运行时。

接入写库前需协调所有写者升级：旧版整代重写会丢掉 `usage_evidence`，包含旧 CLI `reattribute --apply`，以及消费方旧版归属重算路径。Rust 消费方还需持久化 `Cursor.dsh_state`，适配 `SourceChange::ReparseRequired` 的完整重投影语义；DSH 写库应使用 `dsh_provider::scan_source` 与 `apply_dsh_planned_projection`，以完整目标核对总库、避免只读游标领先时漏写。只读取原四桶的消费方不会自动获得新的细粒度用量汇总。完整证据与可复跑命令见 [读取状态设计 §14–§15](docs/dsh-read-state-design.md#14-常规来源接线与持久化凭据)。

## 历史落地路线（绞杀者迁移）

以下保留 2026-06 至 2026-07 的抽取与迁移记录；P0–P3 是当时的阶段划分，soak 与消费方接入状态不代表今天的进度。当前本仓任务以 [待做清单](docs/backlog.md) 为准，消费方收尾回各仓维护。

SessionVault 起步于抽取 QuotaBar 已实机验证的扫描器，通过绞杀者（strangler-fig）迁移接入：

- **P0 ✅** 冻结黄金语料 + `RawEvent` 定稿
- **P1 ✅** crate 抽取过语料：本地 + WSL × Claude + Codex 发现/解析/归一化、宿主感知路径规范化、
  WSL 访问桥、跨运行游标持久化——全部实机实测（`session_index.rs` / `jsonl_cache.rs` /
  `providers/{claude,codex}.rs` / `wsl/mod.rs` / `paths.rs` 对应能力已抽取）
- **P2 🟡 step 1-4 ✅，切换完成、soak 中** 绞杀者迁移：冻结 QuotaBar `cache.db` 黄金基线 + 对账契约
  （[docs/parity-contract.md](docs/parity-contract.md)）+ `parity` diff 工具（**首测 9134 条 usage
  must-match=0**，计费字段字节级一致，含 Codex 累计 token）→ QuotaBar 已改用共享扫描器、影子并跑
  diff `cache.db` 一致后切换（feature flag，留回退）。**待办**：soak 收尾删旧路径
- **P3 ✅ ADR-027 C5 技术安全门已落地** 总库 `TotalStore`（QuotaBar 默认写者）使用
  AES-256-GCM `sv2` 信封；OS keychain 主密钥只包裹按 `(source_type, location, path,
project_root)` 随机生成的数据密钥。旧明文/`sv1` 库事务迁移并清理空闲页；事件身份与数据密钥
  分组均作为 AAD 防调换。`svault erase` 同事务写无正文墓碑、物理删除并回收孤立数据密钥，
  后续 `append_events` 按墓碑拒绝重扫复活。合成隔离跨进程 E2E 10/10 PASS；正式构建不包含
  fixture 密钥入口。TumeFlow 删除传播与 QuotaBar 双重确认已同步落地。

## 归属修正与旧代回收

`svault reattribute`：把当前投影里「同一个目录的旧写法」的 `project_root`（如旧注册表留下的 UNC 写法）改成注册表现在的写法，受影响的来源各写一代新投影（不读源文件），消费方经 `changes` 收到替换。只改写法、不改归属；注册表里同一个目录有不止一种写法时不改、单列报出。默认只预览，`--apply` 才写（先退出 QuotaBar / TumeFlow）。

`TotalStore::recompute_attribution`（库接口，没有 CLI）：调用方点名一批来源，按每条事件记下的 `cwd` 与当前注册表重算归属三项（`project_root` / `project_root_source` / `workspace_location`）。用的是解析器同一个函数，结果就是拿这份注册表重扫会写下的；有改动的来源各写一代新投影，经 `changes` 可见。给源文件已不在、重扫不了的来源用，还在的来源照旧重扫。

`svault reclaim-rollbacks`（源码已合并，尚未进入 `v2026.10.29.1` 正式版）：回收回退（源文件变短、被原地改写）留下的旧代里，**每一条**都能在当前代找到内容相同事件的那些；部分覆盖的整代留着（它可能是已消失内容的唯一副本，`gc` 也不碰）。每份旧代一行、最后一行汇总。默认只预览，`--apply` 才删（先备份、先退出 QuotaBar / TumeFlow）；删完要 `VACUUM` 才缩文件。判据与实测见 `docs/storage-growth.md` 第 5 项。

## 构建

Windows 上用 **GNU 工具链**（`stable-x86_64-pc-windows-gnu`，**MSRV 1.87**——
解压 Codex 压缩会话用的 `ruzstd` 0.8.3 要求它，见 `Cargo.toml`）。GNU target 编 `windows-sys` 需 `dlltool.exe`
（WinLibs 提供）在 PATH 上。

```sh
cargo build         # lib + svault
cargo test          # 单测（WSL 实机 IT 需 SVAULT_WSL_IT=1，默认跳过）
```

## 主密钥从哪来

默认在 OS 密钥链（Windows 凭据管理器 / macOS 钥匙串 / Linux Secret Service）。无桌面的 Linux
没有 Secret Service，这时设 `SVAULT_KEY_FILE=<路径>`：首次建库在该路径生成密钥文件（权限 `0600`），
之后从它读；对组或其他用户开放了权限的文件会被拒绝，文件丢了不会重新生成（报 `MissingKey`）。
多台设备共用一把主密钥（多机同步，见 [docs/linux-replica.md](docs/linux-replica.md)）。在已有钥匙的机器上
`svault key-export --to <文件>` 把它写进一个新建的受保护文件（不上屏）；把文件安全地带到新设备后，
`svault key-import --from <文件>` 装进那里的密钥链（设了 `SVAULT_KEY_FILE` 就新建那个文件），然后删掉
导出的文件。新设备上已有**另一把**钥匙时导入会被拒绝；本机总库存在时先确认这把钥匙打得开它。

那台设备已经有自己的库和钥匙时，用 `svault key-import --from <文件> --rekey` 把它的库换到共用钥匙下：
**先退出 QuotaBar 与 TumeFlow**（它们手里是旧钥匙；Windows 上库被别的进程开着会拒绝）。旧钥匙先备份
（密钥链里同一服务下名为 `<原条目>.previous-<毫秒>` 的条目，或密钥文件旁的 `.previous-<毫秒>` 文件，
输出的 `previous_key_backup` 写明在哪），数据密钥在一个事务里改包，最后才换本机的钥匙；正文不动。
中途断了重跑即可；换钥后若有写入方没退出、留下旧钥匙包着的数据密钥，重跑同一条命令会用备份修好
（`outcome = "healed"`）。确认库读写正常后，备份可以删掉。

## 文档

- [docs/backlog.md](docs/backlog.md) —— 当前任务顺序、消费影响及源码 / 发布状态；下一步从这里看。
- [docs/INGEST_KERNEL.md](docs/INGEST_KERNEL.md) —— 完整设计契约（来源目录、provider 扩展、`RawEvent`、游标、扫描报告、总库/分库、黄金语料、落地计划与进度）。
- [docs/rawevent-reconciliation.md](docs/rawevent-reconciliation.md) —— `RawEvent` 契约 ⇄ QuotaBar 实际扫描器逐字段对账（P0）。
- [docs/parity-contract.md](docs/parity-contract.md) —— 影子并跑对账契约（P2 step 1）：QuotaBar `usage_facts` ⇄ SessionVault RawEvent(usage) 的序数对齐、must-match/advisory 等价定义、字节边界、首测结果。`parity` 工具：`cargo run --features parity --bin parity -- …`。
- [docs/LOGGING.md](docs/LOGGING.md) —— 日志规范（对齐 QuotaBar `docs/LOGGING.md`：`log` 复用宿主 sink、stdout=NDJSON/stderr=日志、正文不进日志；TumeFlow ADR-026）。
- [docs/scan-state-model.md](docs/scan-state-model.md) —— 扫描与投影的状态模型：`AppendLogObservation` 四态、`ScanReasons` 组合原因 × `CommitPlan` 纯函数规划、`Deadline` 作为调用链参数、`Probed` 三态探测、幂等 `ProjectionToken`。
- [docs/project-identity.md](docs/project-identity.md) —— 项目身份从「现算」到「记下来」（`project_identity` 表）。
- [docs/project-attribution.md](docs/project-attribution.md) —— 会话归属到项目根的判据与解析链。
- [docs/dsh-ingest-survey.md](docs/dsh-ingest-survey.md)、[docs/dsh-read-state-design.md](docs/dsh-read-state-design.md) —— DSH v4 宿主来源核查记录、读取状态与实验核验接缝。
- 跨仓库决策记录见 [TumeFlow `DECISIONS.md`](https://github.com/Simon995/TumeFlow/blob/main/docs/DECISIONS.md)（ADR-018～ADR-028）。其中与本仓直接相关：**ADR-024**（交付：CLI/lib/PyO3 与钉版）、**ADR-025**（四项架构保险：source_mode / 多形态游标 / 派生路径 / 两层目录）、**ADR-026**（日志）、**ADR-027**（隐私与删除：逻辑 append-only、物理可销毁、删除跨库传播）。

## 分发与版本

已交付 Rust **lib**（原生 cargo 依赖）与 **CLI**（NDJSON 出 stdout，跨语言子进程消费）；**PyO3** wheel 后置，尚未交付。

**CLI 二进制由本仓发布**（2026-10-09 起）：`uv run python scripts/release.py <tag>` 在本机编
Windows 版、在 WSL 里编 Linux 版，逐项验证后试运行结束；加 `--publish` 才打标签、发 GitHub
Release（附 `SHA256SUMS` 与构建用的 `Cargo.lock`），并把 Windows 版装到 `%LOCALAPPDATA%\svault\bin\svault.exe`。
消费方读发布物的显式路径，并正向断言 `svault --version` 括号里的提交与所需子命令。
来源目录、`RawEvent`、`SourceReport`、游标均为公开 API，破坏即 major 版本；早期两消费者用 git submodule pin 到某 commit，契约稳定后再走 registry 正式发版。
