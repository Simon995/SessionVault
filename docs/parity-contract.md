# 影子并跑对账契约 —— QuotaBar `usage_facts` ⇄ SessionVault RawEvent (P2)

> 绞杀者迁移（strangler-fig）**第一步**产物（ADR-022 / INGEST_KERNEL §15 五步之 1）：
> **冻结 QuotaBar 当前 `cache.db` 输出为黄金基线**，作为后续「影子并跑 → diff 一致才切」的回归权威。
> **零 QuotaBar 改动**：基线只**读** QuotaBar 自然运行写出的 `cache.db`，不改其任何代码/行为。
>
> 读源（只读）：`D:\code\QuotaBar\src-tauri\src\storage\db.rs`（表结构 + 写入路径
> `replace_agent_file_index`）、`session_index.rs`（`parse_claude_lines` / `parse_codex_lines` /
> `fact_from_entry` / seq 赋值）、`usage/blocks.rs`（`parse_claude_jsonl_entry`）。
> 最后更新：2026-06-14。

---

## 0. 一句话

QuotaBar 把会话 JSONL 落库成 **`usage_facts`（每 usage 事件一行、无正文）** 与 `agent_sessions`（每会话聚合）。
本契约把 SessionVault 的 **`event_type=usage` 子集**投影到 `usage_facts` 同粒度，逐字段比对，
证明「抽取后的解析器复现 QuotaBar 计费用量、字节级一致」。**首次实测：Codex 两个位置 + Claude WSL
字节级零分歧；Claude local 仅差活跃会话文件的实时增长尾（见 §8）。**

---

## 1. 权威基线 = `usage_facts`（+ `agent_session_files` 做字节边界）

| 基线对象 | 用途 | 取自 |
|---|---|---|
| `usage_facts` | **对账主体**：确定性、无正文、每 usage 事件一行、PK 稳定 | `cache.db` |
| `agent_session_files` | 每文件 `(mtime, size, safe_offset, parse_status)`——**界定 QuotaBar 当时实际消费的字节**，用于消解实时增长偏移（§8） | `cache.db` |
| `agent_sessions` | 每会话聚合（含 `title/preview/search_text` 正文片段）；**仅作二次校验**，非主对账 | `cache.db` |

不选 `agent_sessions` 作主体：它是 `usage_facts` 的下游聚合（`summarize_sessions` 按 `session_id` 桶加和），
对账主体应取最细、最确定、无正文的 `usage_facts`。`agent_sessions` 的 token 合计可作派生交叉验证。

`cache.db` 默认路径：`dirs_next::config_dir()/quotabar/cache.db`，Windows 即 `%APPDATA%\quotabar\cache.db`。
schema `user_version=6`。

## 2. 粒度对齐：`usage_facts` ⇄ RawEvent `event_type=usage`

QuotaBar `usage_facts` **只在 usage 事件落行**（Claude `type=assistant` 且带 `message.usage`；
Codex `event_msg/token_count`）。RawEvent 粒度更细（message / tool_use / tool_result / **usage** /
thinking / meta）。**对账只取 RawEvent 的 `event_type=usage` 子集**，其余事件类型是 SessionVault
之上的新建能力（reconciliation §3），不参与 parity。

实测量级：QuotaBar `usage_facts` ≈ 9134 行 ⇄ SessionVault 全事件 ≈ 24689，其 `usage` 子集 ≈ 9137。

## 3. ⚠ `seq` 是不同坐标系——**禁止用作 join 键，改序数对齐**

这是本契约最关键的一条。两边 `seq` 都「文件内单调」，但**计数对象不同**：

| | `seq` 语义 | 取值特征 |
|---|---|---|
| **QuotaBar** `usage_facts.seq` | `base_seq + idx`，`idx` = **文件行号**（`content.lines().enumerate()`，含非 usage 行）。usage fact 稀疏地携带其所在**行号** | 稀疏（只有 usage 行有值），值=行位置 |
| **SessionVault** `RawEvent.seq` | **已发射事件**的累计序（一个 assistant 行可同时发 message+usage，各 +1） | 稠密，跨全事件类型连续 |

同一个 usage 事件，两边 `seq` **值不同**。因此 `(provider, location, source_path, seq)` 这个键
**不能跨系统 join**。对账改用**序数对齐**：

> 在每个 `(provider, location, source_path, session_id)` 桶内，两边各按自身 `seq` 升序排列 usage 事件，
> **第 N 条对第 N 条**比较。两边都保文件序、同序产 usage（同源行、同过滤逻辑），故公共前缀逐条对应。

序数对齐的前提是「无中途漏发」：若一侧在中段漏/多发一条，其后全部错位 → 表现为级联字段不匹配，
diff 工具据此报「该桶错位」而非逐条误判。实测数据无中途分歧（§8），错位只可能出现在活跃文件尾部增长。

## 4. 字段映射（`usage_facts` 列 ⇄ RawEvent 字段）

| `usage_facts` 列 | RawEvent（usage）字段 | 对齐处理 |
|---|---|---|
| `provider` | `source_type` | **命名归一**：`claude_code → claude`、`codex → codex` |
| `location` | `source_location.as_key()` | 同形：`local` / `wsl:<distro>` ✓ |
| `source_path` | `source_path` | 见 §5 路径形态（join 前需归一，WSL 尤其） |
| `session_id` | `source_session_id` | 桶键，**must-match** |
| `seq` | `seq` | **不比**（§3，仅各自排序用） |
| `ts_unix` | `occurred_at`（ISO 串）→ iso8601→unix | **advisory**（解析后比；见 §6） |
| `model` | `model` | **must-match** |
| `effort` | `effort` | **must-match**（Codex；Claude 两边恒 None） |
| `cwd` | `cwd` | **advisory**（规范化形态可能不同，§6） |
| `project_root` | `project_root` | **advisory**（同上，且可能反映 SV 的宿主感知修正） |
| `input_tokens` | `usage.input` | **must-match**（计费核心） |
| `output_tokens` | `usage.output` | **must-match** |
| `cache_creation_tokens` | `usage.cache_creation` | **must-match**（Codex 两边恒 0） |
| `cache_read_tokens` | `usage.cache_read` | **must-match** |
| `message_id` | `message_id` | **must-match**（Claude；Codex 两边 None） |
| `request_id` | `request_id` | **must-match**（同上） |

## 5. 路径形态（join 前归一）

`source_path` 是 join 维度之一，形态须先归一：
- **local**：QuotaBar 与 SessionVault 同为宿主原生路径（Windows `C:\Users\…`）✓。
- **wsl:<distro>**：SessionVault 记发行版内 Linux 路径（`/home/<user>/.codex/…`）。QuotaBar WSL 侧
  `source_path` 形态需在建 diff 工具时核对（可能为同一 Linux 路径或经 `wsl.exe` 桥发现的形式）；
  若不同，工具按 `(provider, location, basename + 路径尾段)` 归一后再 join，或断言两边等价。
  **实测 WSL 两个 combo 计数精确相等**（codex Ubuntu-24.04=3366、claude OpenClaw=136），说明发现到的
  文件集合一致；字段级 join 的路径归一在工具落地时锁定。

## 6. 等价定义：must-match vs advisory

- **must-match（任一不等即判 parity 失败）**：四段 token、`model`、`effort`、`message_id`、`request_id`、`session_id`。
  这是计费与归属的硬契约——绞杀者迁移「一致才切」的红线。
- **advisory（记录差异、不直接判失败，单列复核）**：
  - `ts_unix` ⇄ `occurred_at`：QuotaBar 落 unix 秒，SessionVault v0 存原始 ISO 串。比对需 iso8601→unix；
    精度（秒/毫秒/RFC3339 变体）差异归 advisory。
  - `cwd` / `project_root`：**两边规范化路径不同源**——QuotaBar 走 `normalize_cwd_for_location`（内建
    「裸 `/abs` 一律当 WSL」的 Windows 宿主假设），SessionVault 走 `pathnorm` **宿主感知**归一
    （已修掉该假设，见 ADR / `pathnorm` 模块）。因此 `project_root` 形态差异**可能是 SV 的刻意改进而非回归**，
    必须人工复核方向，不计入 must-match。

## 7. 已知允许的分歧（intentional divergences，不算 parity 失败）

1. **无时间戳 usage**：QuotaBar `parse_claude_jsonl_entry`（`usage/blocks.rs:329`）与 `parse_codex_lines`
   （`session_index.rs:825`）都**以 `?`/`else continue` 丢弃无可解析时间戳的 usage**；SessionVault 保留并标
   `time_confidence=low`（设计意图，parser.rs 头注）。→ SessionVault usage 可能 **多出**这批，归「SV-only，允许」。
   *（本机实测：9137 条 usage 全 `time_confidence=high`，此项当前无实例。）*
2. **非 usage 事件**：SessionVault 另发 message/tool_use/tool_result/thinking/meta，QuotaBar 无对应——
   对账前已 filter 到 usage 子集，不参与。
3. **路径规范化**：见 §6 advisory。

## 8. 实时增长偏移与字节边界（消解「活跃会话」假分歧）

`cache.db` 是某一时刻快照；`svault scan-all` 在**之后**读盘，活跃会话文件可能已追加新行 →
SessionVault 多出「快照后新增的 usage」，**这是采样时差、非解析分歧**。

消解办法（diff 工具据 `agent_session_files` 实现）：
- 每文件取 QuotaBar 记录的 `size`，与当前盘上 `size` 比：
  - `qb.size == disk.size` 且计数差 ≠ 0 → **真分歧**，报错。
  - `qb.size < disk.size`（文件已增长，append-only）→ SessionVault 超出 QuotaBar 计数的**尾部** usage
    判为「快照后增长（informational）」，不计 parity 失败；公共前缀仍逐条 must-match。

**首次实测（2026-06-14，`parity` 工具对冻结基线 9134 usage_facts）**：

| combo | qb | sv | prefix | **must** | adv | growth | qb_extra |
|---|---|---|---|---|---|---|---|
| claude\|local | 4237 | 4278 | 4237 | **0** | 0 | 41 | 0 |
| claude\|wsl:Ubuntu-OpenClaw | 136 | 136 | 136 | **0** | 136 | 0 | 0 |
| codex\|local | 1395 | 1395 | 1395 | **0** | 2 | 0 | 0 |
| codex\|wsl:Ubuntu-24.04 | 3366 | 3366 | 3366 | **0** | 3369 | 0 | 0 |
| **合计** | 9134 | 9175 | 9134 | **0** | 3507 | 41 | 0 |

- **must-match = 0**：9134 条对齐 fact 上，四段 token / model / effort / message_id / request_id / session_id
  **逐条字节级一致**——含最难的 Codex 累计 token delta（两个 location 全中）。计费红线全绿，**PASS**。
- **growth = 41**：全部集中在唯一增长文件 `90a29ebd…jsonl`（本对话 transcript，仍在追加），按 §8 字节边界
  正确归类为「快照后增长」，非分歧。`qb_extra=0`（无 SessionVault 漏发）。

> **退出码契约（2026-06-15 加固，`exit_code()` 纯函数 + 单测）**：工具非 0 退出的红线 =
> `must_mismatch > 0` **或** `qb_extra > 0`（SessionVault 漏发 QB 有的 usage；整文件/桶缺失、
> 乃至**空 SV 输出**都会全进 `qb_extra` → 不再被自动化误当绿灯）**或** `sv_extra_unknown > 0`
> （非增长尾、未归类的 SV 多发，疑似重复/过度提取）。`growth` / advisory 只报不判败。
> 传了 `--report` 但序列化/写入失败 → 退出码 **2**（区别于 parity 红线的 1），避免调用方误以为报告已生成。
- **advisory = 3507**：**全是 `cwd` 字段**，且几乎全在 WSL（local 近 0）。已逐例核实为**设计差异非回归**：
  - QuotaBar：`cwd = wsl:Ubuntu-OpenClaw:/mnt/openclaw/workspace/QuotaBar`（`normalize_cwd_for_location` 规范化）。
  - SessionVault：`cwd = /mnt/openclaw/workspace/QuotaBar`（**原始 provenance**，rawevent.rs 明确「原始 cwd」）。
  - `project_root`（两边都规范化的派生值）**基本一致**（codex 3369 ≈ 3366 cwd + 3 个 project_root 边角；
    claude OpenClaw 136 = 全 cwd）。即：SessionVault 把「原始 cwd（证据）」与「规范化 project_root（派生）」
    分开存，QuotaBar 把 cwd 就地规范化。这是有意的，不计 parity 失败。

## 9. 冻结产物与隐私

冻结于 `D:\code\SessionVault\baseline\`（**gitignore，不入库**）：

| 文件 | 内容 |
|---|---|
| `cache.snapshot.db`（+ `-wal`/`-shm`） | QuotaBar `cache.db` 原始不可变拷贝 |
| `quotabar_usage_facts.ndjson` | `usage_facts` 全量，按 `(provider,location,source_path,seq)` 排序，逐行一 JSON 对象（确定性） |
| `quotabar_agent_session_files.ndjson` | 每文件 `(mtime,size,safe_offset,parse_status)`，做 §8 字节边界 |

**隐私**：`usage_facts` 无正文，但 `cwd`/`project_root`/`source_path` 暴露目录结构与用户名；
`agent_sessions`（在 `.db` 内）还含 `title/preview/search_text` 正文片段。故**基线一律本地、不进 git**。
可分享的只有本契约里的**无正文聚合指标**（行数 / 计数 / token 合计 / 形态差异分类）。
此与 INGEST_KERNEL §13.6 / TumeFlow ADR-027 隐私边界一致。

> **注**：本机黄金基线 ≠ P0 的合成黄金 fixture（§11，已入库、可分享、跑单测）。P2 的「黄金语料」本质是
> 机器本地真实数据，受隐私/体积约束不可入库；其回归价值靠「契约（入库）+ 本地基线（不入库）+ diff 工具
> （入库）」三件套保证，而非把真实数据塞进仓库。

## 10. 工具与流程

- **冻结脚本**（一次性）：拷 `cache.db` + `sqlite3` 导出 `usage_facts` / `agent_session_files` 为确定性 NDJSON。
- **diff 工具** `parity`（SessionVault 内、`required-features=["parity"]` 门控，不拖累默认构建与发布 lib）：
  读基线 NDJSON + `svault scan-all` 的 NDJSON → 投影 usage 子集 → §3 序数对齐 + §8 字节边界 → 结构化 parity 报告
  （per-combo 计数差、must-match 不符清单、advisory 差异、增长尾归类）。**纯 Rust、不引 rusqlite**
  （QuotaBar 侧经 `sqlite3` 预导出为 NDJSON）。
- **两种 diff 机制**（互补）：
  - **离线 `parity` 工具**（上条，SessionVault 内）：对**冻结基线** NDJSON 做严格回归，确定性、可归档。
  - **在线影子 runner**（QuotaBar 内，step 2）：在 app 进程里对**实时** `cache.db` 做轻量 diff，常驻闸门。
  两者都按 §3 序数对齐、§6 must-match/advisory、§8 字节边界。

- **流程**：
  - **step 1 ✅**：本契约 + 冻结基线 + `parity` diff 工具 + 首次 parity 报告，首测 **must-match=0**（§8）。
    `parity` 工具单测随 `cargo test --features parity` 跑。
  - **step 2 🟡（QuotaBar `p2/svault-shadow` 分支，feature `svault_shadow` 默认关）**：SessionVault 经
    git submodule + 可选依赖接进 QuotaBar，建**影子接入**——
    - `svault_bridge::project_events`（**seam**）：`&[RawEvent]` 流 → QuotaBar 领域投影（`usage_facts` 走
      适配器 `raw_event_to_usage_fact`；`agent_sessions` **复用 QuotaBar `summarize_sessions`**，hints 从
      RawEvent 构造）。事件源抽象成 `&[RawEvent]`：现喂进程内 `scan(full)`，step 5 改喂总库读出的 RawEvent，
      **投影零改动**（INGEST_KERNEL §15 step3 的正确实现）。
    - `svault_bridge::run_shadow_diff(&Db)`：`discover()+scan(full)` → `project_events` → diff `cache.db`
      两表 + 逐源/逐会话 drill-down 日志。**绿灯看后端算好的 `facts_parity_ok`**
      （= `facts_must_mismatch==0 && qb_extra==0 && sv_extra_unknown==0`；§1：`agent_sessions` 仅二次
      校验；§6：时间戳 advisory）——`qb_extra`（SV 漏发尾部 fact）与
      `sv_extra_unknown`（文件未变长却多发）均**判红**；`sv_growth_tail`（按文件 size 判为快照后增长）
      与 `session_diff`（会话 started/last 时间戳）走 advisory，不翻红灯。**只读不写**。
      （计数缺口纳入绿灯 = review P2 修复，对齐 `parity` 工具 `exit_code` 语义。）
      - **`scan_error_files` 排除（2026-06-16 加固，捕获假阳性教训）**：`run_shadow_diff` 对 `scan` 返
        `ScanStatus::Error`（真 I/O 级失败——**坏行不再走这里**，一次性全扫坏行已走 `Partial` 保留好行，见
        INGEST_KERNEL §8 规则 2）的文件**移出 parity 闸**，计入 advisory `scan_error_files`，**不**把它的缺席
        灌进 `qb_extra` 翻假红。**根因**：旧版 `scan` 对坏行一律返 `Error`+空事件，一个真实大文件（6.8 万行、
        第 15177 行坏）→ SV 空输出 → 3 万条好行全进 `qb_extra` → 假红 `qb_extra=30319`（误判 svault_index
        写库"多发"，实为 SV 重扫"漏发"）。**两层修**：① `scan` one-shot 保留好行返 `Partial`（根治，让坏行
        文件两侧 facts 对齐）；② 此处 `scan_error_files` 排除兜底真正的 I/O 失败。修后实测 `sv=qb=106931`、
        0 回退、绿。**给 TumeFlow 的契约提示**：上面"空 SV 输出全进 `qb_extra` 不被误当绿灯"只针对**语义漏发**；
        真 I/O 失败必须单列排除，否则一个坏文件就把整轮对账误判成红。
    - **debug 触发（一键可跑）**：后端 Tauri 命令 `svault_shadow_diff`（`#[cfg(feature = "svault_shadow")]`）
      + 前端 **QuotaBar Settings「SessionVault 影子对账」dev 区块**的「跑影子 diff」按钮。前端按钮再受
      `import.meta.env.DEV` 门控（Vite 生产构建整段剔除，连命令名字符串都不进发版包——已 `vite build` 验证）。
      跑法：`cargo tauri dev --features svault_shadow`（或 `pnpm tauri:dev` 带该 feature）→ 打开 Settings →
      点按钮 → 界面显示 `ShadowReport`，**`facts_parity_ok` 即绿灯**；计数缺口（qb_extra/sv_extra_unknown）判红，session/增长走 advisory 行；
      drill-down 明细在日志 `target=svault-shadow`（logger 须放行该 target,见 main.rs `level_for`）。
      （注：QuotaBar 未开 `withGlobalTauri`，devtools 里没有 `window.__TAURI__`，故**不能**靠 devtools `invoke`，
      必须走前端按钮这个入口。）
    - **首次在线影子实测（2026-06-14，真实本机数据）**：50 源 → `facts_must_mismatch=0`（**绿灯**，
      sv_facts=9706/qb_facts=9704，+2 是本会话 transcript 的实时增长、cache 慢半拍，§8 informational）；
      `session_diff=10` 全是会话 started/last 的**秒级~分钟级时间戳漂移**（如 Codex started +3~15s、
      WSL last −1~−777s），均 `mm=0`、facts 数一致——属 §6 advisory：sv 与 QuotaBar 原生解析器对
      `session_meta`/尾部 `token_count` 等非正文事件的取时口径略不同,不影响任何 token/usage。
    - **边界自检**（与规范一致）：内核只产事件（§14）、`agent_sessions` 是 QuotaBar 领域投影（§15 step3）、
      总库只存 RawEvent 不碰领域表（§13.1）——三条边界不越界。
  - **step 3 ✅（QuotaBar `p2/svault-shadow`，feature `svault_index`；step 4 已翻默认）**：真实索引**写库路径**
    走 seam（增量保真版）。`session_index::parse_source_file` 的解析步经 `dispatch_parse` 切换——
    开 `svault_index` 时调 `svault_bridge::parse_complete`：把 QuotaBar 已切好的「完整行」交给
    `session_vault::parse_lines`，复用 `project_facts_hints` 产 facts+hints；**字节游标 `safe_offset`、
    `base_seq`、坏行冻结、与现有 facts 合并、summarize 等后处理全部复用 QuotaBar 原逻辑**（只换解析）。
    Codex 累计 token 经 `CodexParserState ⇄ CodexState` 桥接（两结构字段同构），`parser_state_json`
    格式不变、flag 可随意开关；未知 provider / 解析不可用**回落原生**（旧路径永远是安全网）。
    正确性依据：facts 投影已被 step 2 影子实测 `mm=0`；状态桥接字段同构；只换解析、增量脚手架原样。
    四种 feature 组合（default / shadow / index / both）均编过。
  - **step 4 🟡 切换完成、soak 中（QuotaBar `p2/svault-shadow`）**：
    - **已翻默认**：`Cargo.toml` `default = ["svault_index"]` —— svault 写库路径成为正式路径；
      `parse_native` 仍作 `dispatch_parse` 的出错回退**保留**（最终绞杀前的安全网）。
    - **构建链补齐**：`release.yml` build job 补 SessionVault submodule init（PAT secret + `shell:bash`
      跨平台），否则默认拉 path dep 会让所有平台发布构建挂；`ci.yml` 测三配置——默认(svault_index)/
      原生(`--no-default-features`)/诊断(index+shadow)，且 `SUBMODULE_TOKEN` 缺失时优雅降级（只跑原生，
      fork PR / Dependabot 不会红）。**CI 全绿**。
    - **证据台账**（svault==原生）：① 翻默认**前**影子实测 svault-vs-原生 cache.db `mm=0`（§step2，2026-06-14）；
      ② L1 vs 冻结原生基线 `mm=0`；③ L2 分块==整解 18106 事件；④ L3 桥接 index_tests CI 绿。
    - **soak 待办（→ 触发最终绞杀的闸门）**：唯一未被离线覆盖的是**写库路径端到端**（真实增量循环里
      `replace_agent_file_index` + `safe_offset` 推进 + `parser_state_json` 跨重启续传）。
      验法：原生构建（`--no-default-features`）与 svault 默认构建各索引同一份真实数据 →
      逐字段 diff 两份 `cache.db` 的 `usage_facts`（GUI，本机 Tauri exe 受限，需手动跑）。
      ⚠ 翻默认**后**影子 diff 已退化为 svault-vs-svault（cache.db 已是 svault 写的），**不再**是
      svault-vs-原生的有效闸门——soak 须用上面的「原生构建 vs svault 构建」对比，别再依赖影子。
    - **绿了之后（最终绞杀，不可逆）**：删 `parse_native` 与 `not(svault_index)` 的 `dispatch_parse`
      分支 + `ci.yml` 的原生回退步骤，完成绞杀。

## 11. 离线 parity 验证流程（不依赖 GUI、可重复、逐字段）

step 3 的增量正确性拆成三层独立验证，**两层在本机即可跑**（无 Tauri），第三层留 CI：

- **L1 · facts 层（`parity` 工具，本机可跑）**：冻结 QuotaBar `cache.db` 的 `usage_facts` 基线 NDJSON
  ⇄ `svault scan-all` 输出 → 序数对齐比 must-match（§3/§6）。证明 **SessionVault 全量解析 facts ==
  QuotaBar 原生 cache facts**。用法见 §10。首测 must-match=0。
- **L2 · 增量保真层（`tests/incremental_parity_it.rs`，本机可跑）**：对真实本地 Claude/Codex 文件，
  `parse_lines`「整文件一次解」vs「在多个行边界分块、第二块带第一块 `CodexState` 续解」→ 拼接后
  **逐字段（JSON 序列化）比对**。证明 **分块解 == 整解**（尤其 Codex 累计 token 跨块续传）。
  跑法：`SVAULT_PARITY_IT=1 cargo test --test incremental_parity_it -- --nocapture`。
  实测（2026-06-14，真实本机）：**20 本地文件（codex 12）、18106 事件，全部一致**。
- **L3 · QuotaBar 桥接层（`svault_bridge::index_tests`，CI 跑）**：`parse_complete` 分块带状态续解
  vs 整解，专测 `CodexParserState ⇄ CodexState` 双向桥接的累计 token 续传。跑法：
  `cargo test --features svault_index`（本机 Tauri 测试 exe `0xC0000139` 起不来，留 CI/MSVC）。

**合起来的论证**：L1 给「SV 解析 facts == 原生 facts」；L2 给「SV 分块解 == 整解」；L3 给「QB 桥接
不改变这一点」。三者 ⇒ **step 3 增量写库 facts == 原生 facts**。剩下的端到端（**原生构建 vs svault
默认构建**各重索引、直接逐字段 diff 两份 `cache.db`）是 step 4 soak 的闸门（见 §10 step 4），需手动
GUI（本机 Tauri exe 受限）；翻默认后**不可**再用影子 diff 充当此闸门（已退化为 svault-vs-svault）。
