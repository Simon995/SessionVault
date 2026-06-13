# SessionVault

> 共享摄取内核（shared ingestion kernel）+ 不可变 RawEvent 总库。
> 把"发现各 Agent 本地数据 → 增量扫描 → 归一化为 `RawEvent` → 落入永不删的总库"
> 这件最容易踩坑的事，**只实现一次**。

状态：初始设计（设计契约已定稿，代码抽取待启动）
最后更新：2026-06-13

---

## 是什么

**SessionVault** 是一个独立仓库的 **Rust crate**（`session-vault`）+ CLI（`svault`），
中立、无头，不属于任何单一消费者。两个产品平等依赖它：

- **[QuotaBar](https://github.com/Simon995/QuotaBar)** —— 配额/用量监控桌面应用，是总库的默认常驻写者。
- **[TumeFlow](https://github.com/Simon995/TumeFlow)**（Time-Uniform Memory Flow，时间一致记忆流）—— 让 Agent 记忆新旧一视同仁的本地优先记忆系统，从总库物化自己的固化分库。

这样避免两处各写一遍扫描器、各踩一遍 Codex 累计 token / `safe_offset` / WSL 桥 / 路径发现等坑。

## 核心设计

| 维度 | 约定 |
|---|---|
| **来源目录** | 声明式单一事实源：要扫哪些 provider、每平台扫哪些路径，集中维护一份（现 Codex / Claude Code，后续 Cursor / Gemini）。 |
| **provider 可扩展** | 加 provider = 加一个描述符 + 一个解析器，消费者代码与 `RawEvent` 契约不动。 |
| **路径可配置** | 优先级：环境变量 > 用户配置 > 内置候选；前端填一个路径就能扫。 |
| **无状态内核** | 纯函数 `(目录 + 配置 + 游标) → (事件 + 新游标 + 报告)`；不写库、不落盘游标、不联网、不弹界面。 |
| **RawEvent 契约** | 归一化事件 schema；去重唯一键 `(source_type, source_location, source_path, source_session_id, seq)`。 |
| **时间语义** | 每条带 `occurred_at`（对话内时间，冲突裁决权威）+ `time_confidence`；latest-wins 只认 `occurred_at`，不认入库顺序 / offset。 |
| **扫描报告** | 内核无头，但每轮产出结构化 `ScanReport`，由宿主（QuotaBar / TumeFlow）渲染 GUI。 |
| **黄金 fixture** | tricky case 做成样例输入 + 期望输出，消费者升级内核前必须全绿——"坑只有一处"的物理保证。 |

## 总库 / 分库

内核之上提供一个可选持久化组件——共享 **RawEvent 总库**（"Vault" 即指这个永不删、不可变的库）：

- **总库（活、最新、中立）**：只存 `RawEvent`，append-only 不可变，**永不删 / 不压缩 / 不过期**；
  以 `full` 物化（含正文）；QuotaBar 常驻当默认写者，开 WAL 读者不挡写。
- **TumeFlow 分库（物化、固化、可复现）**：按自己节奏从总库增量拉取并物化，两次同步之间冻结，
  盖复现戳（`总库 offset + RawEvent schema 版本`）；总库不可变 → 给定 offset 永远确定性重建。

## 标准化接口

CLI（NDJSON）与 lib/PyO3 等价：

| 接口 | 用途 |
|---|---|
| `catalog()` | 生效后的 provider 描述符列表（宿主据此渲染设置页） |
| `discover(user_config)` | 发现来源清单，首次只发现不读，供用户授权 |
| `scan(source_ref, cursor_in, profile)` | 单来源增量摄取主接口 |
| `scan_all(user_config, cursors, profile)` | 一轮全量增量扫描 + 扫描报告 |

CLI：`svault discover` / `svault scan ...` / `svault scan-all --profile metadata|full`。

## 落地路线

SessionVault **不从零写**，而是**抽取 QuotaBar 已实机验证的扫描器**，用绞杀者（strangler-fig）
迁移改用它，不破坏 QuotaBar 现有功能：

- **P0** 冻结黄金语料 + `RawEvent` 定稿
- **P1** crate 抽取过语料（`session_index.rs` / `jsonl_cache.rs` / `providers/{claude,codex}.rs` / `wsl/mod.rs` / `paths.rs`）
- **P2** QuotaBar 影子并跑 → diff `cache.db` 一致才切（feature flag，留回退）
- **P3** 总库作为输出打开，TumeFlow 开始消费

## 文档

- [docs/INGEST_KERNEL.md](docs/INGEST_KERNEL.md) —— 完整设计契约（来源目录、provider 扩展、`RawEvent`、游标、扫描报告、总库/分库、黄金语料、落地计划）。
- [docs/rawevent-reconciliation.md](docs/rawevent-reconciliation.md) —— `RawEvent` 契约 ⇄ QuotaBar 实际扫描器逐字段对账（P0）。
- [docs/LOGGING.md](docs/LOGGING.md) —— 日志规范（对齐 QuotaBar `docs/LOGGING.md`：`log` 复用宿主 sink、stdout=NDJSON/stderr=日志、正文不进日志；TumeFlow ADR-026）。
- 跨仓库决策记录见 [TumeFlow `DECISIONS.md`](https://github.com/Simon995/TumeFlow/blob/main/docs/DECISIONS.md)（ADR-018～ADR-026）。其中与本仓直接相关：**ADR-024**（交付：CLI/lib/PyO3 与钉版）、**ADR-025**（四项架构保险：source_mode / 多形态游标 / 派生路径 / 两层目录）、**ADR-026**（日志：lib 用 `log` 复用宿主 sink、stdout=NDJSON/stderr=日志）。

## 分发与版本

Rust **lib**（QuotaBar 原生 cargo 依赖）+ **CLI**（NDJSON 出 stdout，TumeFlow 子进程消费）+ 可选 **PyO3** wheel。
来源目录、`RawEvent`、`ScanReport`、游标均为公开 API，破坏即 major 版本；早期两消费者用 git submodule pin 到某 commit，契约稳定后再走 registry 正式发版。
