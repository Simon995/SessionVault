# 日志规范（SessionVault）

> **对齐 QuotaBar `docs/LOGGING.md`**：消息格式、级别策略、事件命名、隐私红线、反样板**全部照搬**，
> 不另起一套。差异**仅三处**——SessionVault 是 lib + headless CLI 双形态、且 stdout 要吐 NDJSON 数据：
>
> 1. **stdout/stderr 硬隔离**（CLI 特有，QuotaBar 是 GUI 无此问题）；
> 2. **`sv-` module-tag 命名空间** + **CLI 自带 stderr sink**（lib 形态复用宿主 sink）；
> 3. **`run_id` 关联**（一次 `scan_all` 扫多源，需要把整轮串起来）。
>
> 决策见 TumeFlow `DECISIONS.md` ADR-026。本规范**新代码必须遵守**，目标同 QuotaBar：
> "读一份日志就能定位问题"，而不是每次诊断先加日志再复现。

状态：初始规范
最后更新：2026-06-13

---

## 0. 两条不可破的硬约束

1. **stdout 是数据，stderr 是日志——绝对分离。** `svault` 的 `RawEvent` NDJSON 流走 **stdout**，
   **所有日志一律 stderr**。日志混进 stdout = TumeFlow 子进程解析 NDJSON 被污染。配测试守住（§6）。
   → QuotaBar 反样板"`no println!`"对 SessionVault **双重致命**：既不受 `RUST_LOG` 控，又污染数据流。
2. **正文永不进日志。** SessionVault 会读到 `full` 正文，但日志**只许**出现路径、字节数、offset、哈希、
   `session_id`、token 数、时间、行号、错误类型；**禁止** `content` / `search_text` / title / preview /
   任何消息体。配"日志流不含 fixture 正文"测试（§6）。

## 1. 技术选型（与 QuotaBar 同管线）

| 形态 | facade | sink |
|---|---|---|
| **lib `session-vault`**（QuotaBar 进程内调用） | **`log`**（不是 `tracing`） | **不装 sink**——复用宿主的。QuotaBar 是 `log` + `tauri-plugin-log`，SessionVault 用 `log` 即**零桥接**落进 QuotaBar 同一个滚动日志文件 |
| **CLI `svault`**（TumeFlow 子进程 / 独立） | `log` | **自带轻量 stderr sink**（如 `env_logger`/`fern`），文本 layout 对齐 QuotaBar 行格式；另给 `--log-format json` 供机器消费 |

- **lib 绝不调 `init()`/装 sink**——那会抢占宿主日志系统。lib 只发 `log::{warn,info,debug}!` 事件。
- **CLI 才装 sink**，writer 固定 **stderr**；级别由 `--log-level` / `RUST_LOG`（target `session_vault=…`）控。
- 为什么不用 `tracing`：QuotaBar 整条管线是 `log`；用 `tracing` 则 SessionVault 日志进不了 QuotaBar 的文件 sink（要桥接）。一套管线、一个日志文件、一种格式，胜过 `tracing` 的 span。`run_id` 用字段模拟即可（§3）。

## 2. 消息格式（照搬 QuotaBar）

```
[<module-tag>][<scope>] <event>: <key>=<value> <key2>=<value2>
```

落进 QuotaBar sink 后的整行（与 QuotaBar 自身日志统一）：
```
[2026-06-13][19:20:33][session_vault::scan][WARN] [sv-cursor][codex/wsl:Ubuntu-22.04/r#a1b2] rolled back: reason=mtime_regress from=40960 to=0
```

- **`module-tag`**：短、固定、**加 `sv-` 前缀**避免与 QuotaBar 的 `[claude]`/`[codex]` 在同一文件撞。固定表见 §4。
- **`scope`**：操作上下文。来源级操作必须含 `(provider, location, run_id 短码)`，如 `[sv-scan][codex/local/r#a1b2]`；
  全局/启动级 scope 可省。
- **`event`**：名词或过去式动词，描述已发生事件。**禁命令式原形**（同 QuotaBar）。
  - 好：`rolled back`、`bad line skipped`、`scanned`、`bridge failed`、`drift detected`
  - 差：`roll back`、`skip line`、`do scan`
- **`key=value`**：结构化字段，空格分隔。**永远不要把 `content`/正文/凭据当 value**。

## 3. `run_id` 关联（SessionVault 特有）

一次 `scan_all` 生成一个 `run_id`（短码如 `r#a1b2`），**写进每条该轮日志的 scope**，把跨多源、跨 WSL 子进程的日志串成一条线。排错动作：`grep 'r#a1b2'` 看整轮事件序列，再看是哪个来源、哪个事件码出问题。
（这是 QuotaBar 按账号循环时不需要、而我们多源扫描必须的关联手段；不引入 `tracing` span，用字段即可。）

## 4. 固定 module-tag 表

| tag | 范围 |
|---|---|
| `sv-discover` | 来源发现：探测根、派生路径解析、授权/排除 |
| `sv-scan` | 扫描主循环：每来源摘要、坏行、状态 |
| `sv-cursor` | 游标推进/回退/截断重建 |
| `sv-claude` / `sv-codex` | provider 解析器（含 Codex 累计 token state） |
| `sv-wsl` | WSL 桥（`wsl.exe` 调用、framing） |
| `sv-snapshot` | snapshot_file 形态（config/rules/hooks 等变更） |
| `sv-sqlite` | sqlite_store 形态 |
| `sv-cli` | CLI 入口（参数、sink 装配、退出码） |

## 5. 级别策略（照搬 QuotaBar）

| 级别 | 用途 | SessionVault 触发条件 |
|---|---|---|
| `error` | 不可恢复、用户必须看见 | 根读不了、schema 硬漂移、跨 panic 边界、CLI 致命参数错误 |
| `warn` | **诊断主入口**（`RUST_LOG=warn` 应够） | 坏 JSON 行跳过、游标回退/重建、WSL 桥非 0 退出、未授权路径排除、provider 格式漂移 |
| `info` | 关键生命周期 | run 起止、每来源一行摘要、发现汇总 |
| `debug` | 决策细节 | 游标推进量、env/config 解析、逐根探测、跳过未变文件 |
| `trace` | **仅 fixture 调试**（生产不用，对齐 QuotaBar "trace 不用"精神） | 逐行解析 / 逐事件；**热循环里 level-gated、关闭零分配** |

## 6. 隐私红线 🔴（在 QuotaBar 基础上加严）

- **绝不记**：`content` / 正文 / `search_text` / title / preview / 任何消息体；token / cookie / refresh_token / 私钥。
- **可记**：路径、`session_id`（UUID，等同 QuotaBar 的 account_id 可记）、offset、字节数、`content_hash`、model、token 数、时间、行号、错误类型、`source_mode`、`parser_version`。
- **凭据/`auth.json`/`.credentials.json`**：永不读内容、永不记内容；命中安全排除时只记 `excluded: rule=auth path=…`。
- **两条不变量配测试**：(a) 任意日志行不出现在 stdout；(b) 用一段含已知正文的 fixture 跑扫描，断言**正文串不出现在捕获的日志流里**。

## 7. 必埋点（SessionVault 回归风险点对应）

方法论同 QuotaBar——"哪里曾因日志不足绕弯路就必埋"。对应本内核回归风险（`INGEST_KERNEL.md` §11 / QuotaBar `SESSION_MEMORY_ARCHITECTURE.md` §13）：

### 1. 游标回退 / 截断重建（`sv-cursor`）
```rust
log::warn!("[sv-cursor][{prov}/{loc}/{run}] rolled back: reason={reason} from={old} to=0", reason="mtime_regress");
```
触发：`(mtime,size)` 回退、WSL `size<known`→Full。**必打 warn**，标 reason（`mtime_regress`/`size_shrink`/`truncate`）。

### 2. 坏 JSON 行（`sv-scan`）
```rust
// 增量（cursor_in=Some，常态轮询）：冻结整批
log::warn!("[sv-scan][{prov}/{loc}/{run}] append_log batch frozen (bad json): path={path} skipped={n} kept_offset={k}");
// 一次性全扫（cursor_in=None，影子对账 / 总库首扫）：保留好行
log::warn!("[sv-scan][{prov}/{loc}/{run}] append_log one-shot kept good events despite bad json: path={path} skipped={n} events={m}");
```
两路（见 INGEST_KERNEL §8 规则 2）：**增量** `status=error`、**不推进 safe_offset**、本轮不发事件——这条对应"为什么 events 比预期少"；**一次性全扫** `status=partial`、跳过坏行保留前后好行、推进到完整行边界——这条对应"有坏行但仍出了事件（影子对账不被误判漏发）"。

### 3. WSL 桥失败（`sv-wsl`）
```rust
log::warn!("[sv-wsl][{distro}/{run}] bridge failed: rel={rel} exit={code} stderr={short}");
```
`wsl.exe` 非 0 退出、distro 不可达、framing 解析异常。带 distro + exit_code + **stderr 摘要**（非全文）。

### 4. 格式漂移（`sv-codex`/`sv-sqlite`/…）
```rust
log::warn!("[sv-codex][{run}] drift detected: field={field} parser_version={pv}");
```
字段/ sqlite schema 与 `parser_version` 不符——**显式失败而非静默错解**（高漂移源升 error）。

### 5. 安全排除（`sv-discover`）
```rust
log::info!("[sv-discover][{run}] excluded: rule={rule} path={path}");
```
`auth.json` / 私钥 / 未授权目录被排除时记一条（**info**，只记规则，不记内容）——对应用户"为什么没扫这个"。

### 6. 每来源一行摘要（`sv-scan`，info）
```rust
log::info!("[sv-scan][{prov}/{loc}/{run}] scanned: path={path} bytes_new={b} events={e} cursor_advanced={c} status={s}");
```
与 `ScanReport`（§10）每来源条目对齐——日志给人看流水账，`ScanReport` 给机器看结果。

## 8. 日志 ≠ ScanReport（别重复造）

- **ScanReport**（`INGEST_KERNEL.md` §10）= 机器结果，`return`/序列化给宿主渲染，**确定性**（可进黄金语料断言）。
- **日志** = 诊断流水账 + 异常，走 stderr，含 `run_id`/时间等**易变量**，给人排错。
- 关系：`ScanReport.warnings` 每条**同时**打一条 WARN 日志（同语义）。黄金测试断言 ScanReport（确定），**不**断言原始日志；要测日志只测"事件码序列"（剥掉时间/run_id）。

## 9. 反样板（照搬 QuotaBar + 本仓库追加）

```rust
log::warn!("error: {}", e);                 // 无 tag 无 scope
log::warn!("{}", raw_jsonl_line);           // 🔴 正文/裸文本入日志
log::info!("processing...");                // 无事件名
log::debug!("content = {}", body);          // 🔴 泄正文
println!("...");                            // 🔴 双重致命：不受 RUST_LOG 控 + 污染 stdout NDJSON
log::warn!(target: "foo", "...");           // 不用与模块路径差异巨大的 target
```

**为什么这么严**：排查靠 `grep`。统一的 `sv-` tag + `run_id` scope 让 `grep 'r#a1b2'` 精准串起一整轮扫描；
事件名末尾的 `key=value` 让用户粘贴的日志段直接看出语义，不用回头读源码。与 QuotaBar 同一套语言，两仓库日志可混在一个文件里读。
