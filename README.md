# SessionVault

> 共享摄取内核（shared ingestion kernel）+ 不可变 RawEvent 总库。
> 把"发现各 Agent 本地数据 → 增量扫描 → 归一化为 `RawEvent` → 落入永不删的总库"
> 这件最容易踩坑的事，**只实现一次**。

状态：摄取核心已落地（P0 ✅ / P1 ✅ —— 本地 + WSL 双位置摄取、游标持久化端到端实测）；
P2 绞杀者迁移 **step 1 ✅**（冻结 QuotaBar 黄金基线 + `parity` diff 工具，**首测 9134 条 usage
must-match=0**）；总库（P3）待启动。
最后更新：2026-06-14

---

## 是什么

**SessionVault** 是一个独立仓库的 **Rust crate**（`session-vault`）+ CLI（`svault`），
中立、无头，不属于任何单一消费者。两个产品平等依赖它：

- **[QuotaBar](https://github.com/Simon995/QuotaBar)** —— 配额/用量监控桌面应用，是总库的默认常驻写者。
- **[TumeFlow](https://github.com/Simon995/TumeFlow)**（Time-Uniform Memory Flow，时间一致记忆流）—— 让 Agent 记忆新旧一视同仁的本地优先记忆系统，从总库物化自己的固化分库。

这样避免两处各写一遍扫描器、各踩一遍 Codex 累计 token / `safe_offset` / WSL 桥 / 路径发现等坑。

## 现状（已实现）

| 模块 | 职责 | 状态 |
|---|---|---|
| `catalog` | 声明式来源目录（provider × 子目录 × 形态） | ✅ Claude / Codex |
| `discover` | 发现本地 + WSL 来源清单（不读内容） | ✅ 本地 + WSL |
| `parser` | 行级 JSONL → `RawEvent`（Claude / Codex 字段映射、Codex 累计 token delta） | ✅ |
| `pathnorm` | 宿主感知路径规范化（`HostPlatform`、UNC↔规范形、`workspace_location`） | ✅ |
| `wsl` | WSL 访问桥（`wsl.exe` 枚举/`find`/`stat`/`tail`，UTF-16LE 解码） | ✅ 实机实测 |
| `scan` | 增量扫描（`ByteSource` 抽象，本地/WSL 共用游标·回退·坏行冻结） | ✅ append_log |
| `cursor` | 多形态游标（字节偏移 + Codex 状态 + `next_seq`） | ✅ append_log |
| `svault` CLI | `discover` / `scan-all`（NDJSON 出 stdout），跨运行游标持久化 | ✅ |
| `parity` 工具 | P2 影子并跑 diff：QuotaBar `usage_facts` ⇄ RawEvent(usage)（`required-features=["parity"]`） | ✅ 首测 must-match=0 |
| 总库（持久化输出库） | append-only RawEvent 库 | ⬜ 规划（P3） |
| snapshot_file / sqlite_store / 其它 provider | 契约预留 | ⬜ `planned` |

**实机实测（2026-06-14，真实本机数据）**：48 来源（19 local + 29 WSL）→ 23373 事件
（8102 来自 WSL）；二次扫描借持久化游标 23476 → **0** 事件（全 cache-hit），增量端到端闭环。

## 核心设计

| 维度 | 约定 |
|---|---|
| **来源目录** | 声明式单一事实源：要扫哪些 provider、每平台扫哪些路径，集中维护一份（现 Codex / Claude Code，后续 Cursor / Gemini）。 |
| **provider 可扩展** | 加 provider = 加一个描述符 + 一个解析器，消费者代码与 `RawEvent` 契约不动。 |
| **路径可配置** | 优先级：环境变量 > 用户配置 > 内置候选；前端填一个路径就能扫。 |
| **宿主感知路径** | `pathnorm` 把「裸 Unix 绝对路径如何归属」按宿主平台分叉：Windows 宿主默认 WSL、Unix 原生判 local——修掉了 QuotaBar 内建的「裸 `/abs` 一律当 WSL」假设。Unix 路径语义在 Linux 原生与 WSL 内部共用同一套函数。 |
| **无状态内核** | lib 是纯函数 `(目录 + 配置 + 游标) → (事件 + 新游标 + 报告)`：不写库、不落盘、不联网、不弹界面。游标**持久化由宿主负责**——`svault` CLI 提供一个默认实现（`--state` 状态文件）。 |
| **RawEvent 契约** | 归一化事件 schema；去重唯一键 `(source_type, source_location, source_path, source_session_id, seq)`。 |
| **时间语义** | 每条带 `occurred_at`（对话内时间，冲突裁决权威）+ `time_confidence`；latest-wins 只认 `occurred_at`，不认入库顺序 / offset。 |
| **扫描报告** | 内核无头，但每轮产出结构化 `SourceReport`，由宿主（QuotaBar / TumeFlow）渲染 GUI。 |
| **黄金 fixture** | tricky case 做成样例输入 + 期望输出，消费者升级内核前必须全绿——"坑只有一处"的物理保证。 |

## 总库 / 分库（规划，P3）

内核之上将提供一个可选持久化组件——共享 **RawEvent 总库**（"Vault" 即指这个永不删、不可变的库）。
**当前尚未落地**：`svault scan-all` 现把 `RawEvent` 以 NDJSON 吐到 stdout，由消费者接走。

- **总库（活、最新、中立）**：只存 `RawEvent`，append-only 不可变，**默认永不删 / 不压缩 / 不过期**
  （用户主动删除是一等操作，见 TumeFlow ADR-027）；以 `full` 物化（含正文）；QuotaBar 常驻当默认写者。
- **TumeFlow 分库（物化、固化、可复现）**：按自己节奏从总库增量拉取并物化，两次同步之间冻结，
  盖复现戳（`总库 offset + RawEvent schema 版本`）；总库不可变 → 给定 offset 永远确定性重建。

## 标准化接口

CLI（NDJSON）与 lib 等价（PyO3 wheel 后置）：

| lib 接口 | 用途 |
|---|---|
| `catalog()` | 生效后的 provider 描述符列表（宿主据此渲染设置页） |
| `discover()` | 发现来源清单（本地 + WSL），首次只发现不读内容 |
| `scan(source_ref, cursor_in, profile)` | 单来源增量摄取主接口（无状态：游标进、游标出） |

CLI：

- `svault discover` —— 列出来源（NDJSON，不读内容）。
- `svault scan-all --profile metadata|full [--state <file> | --stateless]` —— 一轮增量扫描，
  逐条吐 `RawEvent` + 每来源 `SourceReport` + 汇总。游标默认持久化到
  `<data_local_dir>/svault/cursors.json`（`--state` 覆盖、`--stateless` 关），跨运行真增量。
  退出码：`0` 成功 / `1` 发现失败 / `2` **游标保存失败**（`summary.state_saved=false`，
  本轮增量游标未推进，下游应重试或预期重复）。

## 落地路线（绞杀者迁移）

SessionVault **不从零写**，而是**抽取 QuotaBar 已实机验证的扫描器**，用绞杀者（strangler-fig）
迁移改用它，不破坏 QuotaBar 现有功能：

- **P0 ✅** 冻结黄金语料 + `RawEvent` 定稿
- **P1 ✅** crate 抽取过语料：本地 + WSL × Claude + Codex 发现/解析/归一化、宿主感知路径规范化、
  WSL 访问桥、跨运行游标持久化——全部实机实测（`session_index.rs` / `jsonl_cache.rs` /
  `providers/{claude,codex}.rs` / `wsl/mod.rs` / `paths.rs` 对应能力已抽取）
- **P2 🟡 进行中** 绞杀者 **step 1 ✅**：冻结 QuotaBar `cache.db` 黄金基线 + 对账契约
  （[docs/parity-contract.md](docs/parity-contract.md)）+ `parity` diff 工具，**首测 9134 条 usage
  must-match=0**（计费字段字节级一致，含 Codex 累计 token）。**待办** step 2–4：QuotaBar 改用共享扫描器 →
  影子并跑 diff `cache.db` 一致才切（feature flag，留回退）
- **P3 ⬜** 总库作为输出打开，TumeFlow 开始消费

## 构建

Windows 上用 **GNU 工具链**（`stable-x86_64-pc-windows-gnu`，**MSRV 1.85**——
依赖 `clap_builder` 用 edition 2024）。GNU target 编 `windows-sys` 需 `dlltool.exe`
（WinLibs 提供）在 PATH 上。

```sh
cargo build         # lib + svault
cargo test          # 单测（WSL 实机 IT 需 SVAULT_WSL_IT=1，默认跳过）
```

## 文档

- [docs/INGEST_KERNEL.md](docs/INGEST_KERNEL.md) —— 完整设计契约（来源目录、provider 扩展、`RawEvent`、游标、扫描报告、总库/分库、黄金语料、落地计划与进度）。
- [docs/rawevent-reconciliation.md](docs/rawevent-reconciliation.md) —— `RawEvent` 契约 ⇄ QuotaBar 实际扫描器逐字段对账（P0）。
- [docs/parity-contract.md](docs/parity-contract.md) —— 影子并跑对账契约（P2 step 1）：QuotaBar `usage_facts` ⇄ SessionVault RawEvent(usage) 的序数对齐、must-match/advisory 等价定义、字节边界、首测结果。`parity` 工具：`cargo run --features parity --bin parity -- …`。
- [docs/LOGGING.md](docs/LOGGING.md) —— 日志规范（对齐 QuotaBar `docs/LOGGING.md`：`log` 复用宿主 sink、stdout=NDJSON/stderr=日志、正文不进日志；TumeFlow ADR-026）。
- 跨仓库决策记录见 [TumeFlow `DECISIONS.md`](https://github.com/Simon995/TumeFlow/blob/main/docs/DECISIONS.md)（ADR-018～ADR-028）。其中与本仓直接相关：**ADR-024**（交付：CLI/lib/PyO3 与钉版）、**ADR-025**（四项架构保险：source_mode / 多形态游标 / 派生路径 / 两层目录）、**ADR-026**（日志）、**ADR-027**（隐私与删除：逻辑 append-only、物理可销毁、删除跨库传播）。

## 分发与版本

Rust **lib**（QuotaBar 原生 cargo 依赖）+ **CLI**（NDJSON 出 stdout，TumeFlow 子进程消费）+ 可选 **PyO3** wheel。
来源目录、`RawEvent`、`SourceReport`、游标均为公开 API，破坏即 major 版本；早期两消费者用 git submodule pin 到某 commit，契约稳定后再走 registry 正式发版。
