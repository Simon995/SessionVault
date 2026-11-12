# 总库只读元数据查询

2026-11-10，在本仓合成库验证；源码依据为 [`store.rs`](../src/store.rs) 和 [`svault.rs`](../src/bin/svault.rs)。本记录不包含真库测量，也不将消费方观测到的整条命令延迟归因于单条 SQL。

## 根因与修复

`stores` 只输出每份库的事件数与最大 offset，却调用了完整 `TotalStore::status()`。该接口还求 `MAX(ingested_at)` 和数据钥匙数量；将事件数量、offset 与入库时间放在同一聚合中，会逐行访问 `raw_events`。事件正文虽然不解密，仍与元数据同在较宽的数据表记录中。

单库 `pull` 的摘要也调用完整 `status()`，实际只使用最大 offset。两个调用点都承担了不需要的统计工作。

现在按所需信息选择接口：

| 调用方 | 查询接口 | 契约 |
| --- | --- | --- |
| `stores` | `TotalStore::event_summary()` | `StoreEventSummary { count, max_offset }`，两个标量子查询在同一条 SQL 的快照中执行 |
| 单库 `pull` 摘要 | `TotalStore::max_offset()` | 只查当前事件表的最大 offset，空表为 0 |
| 完整状态消费者 | `TotalStore::status()` | 保留原字段与统计含义 |

轻量摘要允许 SQLite 的 `Count` opcode 使用既有窄索引，并用主键末端求最大 offset。计数仍需访问索引页，不宣称常数时间。没有新增表列或计数缓存，也没有把 `sqlite_sequence` 的历史水位当成当前事件水位；擦除全部事件后，摘要水位仍为 0。

`count` 包括所有保留投影中的事件，与原 `status().count` 相同。`stores` 的 NDJSON 字段、不可用库报告及排序保持原契约。读取入口仍使用 `open_read_only`，不建库、不迁移。

## 可复现证据

运行：

```sh
uv run python scripts/verify-store-metadata.py
cargo test --lib --features store event_summary
cargo test --features store,acceptance-fixtures --test cli_union_it --test cli_read_only_it
```

探针从生产 Rust 常量读取 DDL 和新查询，仅创建独立合成 SQLite 库；正文为占位字符串，不冒充加密总库端到端测试。输出保留各轮原始耗时、查询计划、opcode、结果与 SQLite 版本，位于 `${CARGO_TARGET_DIR:-target}/store-metadata-*/report.json`。

2026-11-10 Windows 实跑：SQLite 3.50.4，4,096 行，每行占位正文 16,384 字节，库文件 69,505,024 字节，每条查询连续执行三次。

| 查询 | 结果 | 查询计划 / opcode | 三次耗时中位数 |
| --- | --- | --- | --- |
| 原完整事件聚合 | count=4096，max_offset=4096，last_ingested_at=4095 | `SCAN raw_events`，逐行聚合 | 4.116 ms |
| 轻量摘要 | count=4096，max_offset=4096 | 覆盖索引的 `Count` + 主键末端查询 | 0.064 ms |
| 单独最大 offset | max_offset=4096 | 主键末端查询 | 0.069 ms |

这是同一合成连接上的连续 SQL 样本，包含缓存与执行顺序影响；它不预测生产冷读延迟、解锁耗时、副本数量或整条 CLI 的耗时。

库回归另直接使用 bundled SQLite 的 `StatementStatus` 比较：旧查询 `FullscanStep > 0`，新摘要 `FullscanStep == 0` 且 `VmStep` 更少；不使用不稳定的耗时阈值。语义回归覆盖空库、Rollback 后保留两代、墓碑物理删除后的计数与水位。CLI 集成回归核对输出字段与计数，并验证旧库只读查询前后未迁移。

上述 2026-11-10 测量阶段未替换已安装 CLI、未访问真库。该修复随后已随 `v2026.11.11.1`（`b9339c3`）正式发布，本机独立 CLI 已升级；是否满足消费方生产时限仍由其按实际库与调用链复测，不能用本表的 SQL 时间替代。当前发布与任务状态只维护在 [backlog.md](backlog.md)。
