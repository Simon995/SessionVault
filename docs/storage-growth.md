# 总库体积：现状与待优化项

> **状态：只记录，未动手**（2026-10-08）。等人明确说「优化」再做。
> 下面的数是当天的**快照**，会自己漂 —— 动手前按文末「复测」重新量，别照抄。

## 存了什么

| 层 | 内容 |
| --- | --- |
| 明文列 | 来源路径、会话 id、事件类型、时间、项目根 —— 供查询与索引 |
| `event_json` | 整条 `RawEvent`（含正文）**AES-256-GCM 加密后 base64**。🔴 **不压缩**：base64 本身 +⅓，密文也压不动 |

总库按 `Profile::Full` 写，正文入库。解析时**只做抽取，不截断、不脱敏**：

| 会话里的东西 | 入库 |
| --- | --- |
| 用户 / 助手消息、thinking | 全文 |
| 工具返回（文件内容、命令输出） | 全文 |
| 工具调用参数（含 Write/Edit 写入的整份文件） | 只留 `[Tool: <名>]` |
| 图片 / base64 等无文本块 | 丢弃 |
| 每行 JSON 的外壳与重复元数据 | 只取需要的字段 |

## 2026-10-08 快照

- 总库文件 **4.92 GB**；其中 `event_json` 合计 3.2 GB，其余是索引与明文列。
- 最大的单个会话源文件 1.18 GB → 入库 321 MB（≈0.27）。
- 仍在盘上的 445 个会话源文件共 3.6 GB；库里会话部分 3.0 GB（另含 465 个源已删、只剩库内一份的会话）。

| 去处 | 大小 | 说明 |
| --- | --- | --- |
| `message` | 1.3 GB | 63 万条 |
| `usage` | 781 MB | 62 万条纯计量事件，每条 ≈1.3 KB —— **大头是加密 + base64 的固定开销** |
| `tool_result` | 677 MB | 单条最大 110 KB |
| 旧代：`origin = 'reparse'` | 212 MB | `svault gc` 可回收（dry-run：78 份投影 / 89,265 条） |
| 旧代：`origin = 'unknown'` | 765 MB | ADR-044 之前产生，**GC 故意不碰**（可能是已删内容的唯一副本），只能显式审计后清理 |
| `config_snapshot` 中的 `history.jsonl` | 126 MB | 当 `SnapshotFile` 存，**每变一次存一份全文**，文件越长每版越大 |

## 待优化项（按代价从小到大）

1. **`svault gc` + `VACUUM`**（≈0.2 GB，零代码）。⚠️ `gc` 只把页放回空闲表，**文件不缩**，
   要 `VACUUM` 才还给磁盘；而 `VACUUM` 要独占库，两个消费者都开着它 —— 挑空档、先备份。
2. **`history.jsonl` 改增量**（代码改动）：止住增长最快的一块。它只增不改，形状像
   `AppendLog`，当初用 `SnapshotFile` 是权宜（见 AGENTS.md 规则 5 判例）。
3. **`unknown` 旧代的审计清理**（设计先行）：先实测哪些是「当前代已覆盖」、哪些是
   「源已删的唯一副本」，再给人一份可审计的清单。
4. **入库前压缩再加密**（省得最多，估计 >1 GB）。🔴 这是**存储格式变更**：旧版 svault
   解不开新行 ⇒ 先让所有读者支持新格式、再让写者写（AGENTS.md 判据二：两份 svault
   同时开着同一个库，子模块指针拦不住）。

## 复测

```sql
-- 按事件类型
SELECT event_type, count(*), sum(length(event_json)) FROM raw_events GROUP BY 1;

-- 不属于当前代的行，按来由
SELECT coalesce(p.origin, '<无台账>'), count(*), sum(length(r.event_json))
  FROM raw_events r
  LEFT JOIN current_head h USING (source_type, source_location, source_path)
  LEFT JOIN projections  p USING (source_type, source_location, source_path,
                                  source_revision, projection_revision)
 WHERE h.source_path IS NULL
    OR h.source_revision <> r.source_revision
    OR h.projection_revision <> r.projection_revision
 GROUP BY 1;
```

```sh
svault gc --dry-run   # 只统计不删
```

⚠️ 只读打开（`?mode=ro`），别 `cp` 正在被写的库去量 —— 会漏掉 `-wal`。
