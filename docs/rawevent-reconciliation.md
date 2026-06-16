# RawEvent 契约 ⇄ QuotaBar 实际扫描器 对账

> P0 产物（ADR-022）。逐字段核对 `INGEST_KERNEL.md` 的设计稿与 QuotaBar 实机代码，
> 暴露设计里的乐观假设，作为 `RawEvent` 定稿与黄金语料的依据。
>
> 读源：`D:\code\QuotaBar\src-tauri\src\` 下 `paths.rs` / `session_index.rs` /
> `session_transcript.rs` / `wsl/mod.rs` / `usage/blocks.rs` / `storage/db.rs`（只读）。
> 最后更新：2026-06-13。

---

## 0. 一句话结论

**QuotaBar 没有"一个 RawEvent"，也不持久化正文。** 它把会话 JSONL 拆成三块落库/产出：

| QuotaBar 产物 | 是什么 | 粒度 | 含正文 | 持久化 |
|---|---|---|---|---|
| `UsageFactRow`（`usage_facts`） | 计费用量事实 | **每个 usage 事件一条**（非每条消息） | 否 | 是 |
| `AgentSessionRow`（`agent_sessions`） | 每会话一行摘要 | 每会话 | 折叠 `search_text` + title/preview | 是 |
| `TranscriptMessage` | 角色化消息气泡 | 每条可见消息 | **是（含正文）** | **否，打开会话时即时解析** |

我设计稿 §7 的 RawEvent（逐事件、带 `content`、带 `actor`/`event_type`/`parent_ref`）**在 QuotaBar 里不存在**：
正文只在 transcript 即时路径上、用完即弃；落库的只有元数据 fact 与折叠搜索文本。
→ **抽取能直接拿到的是"会话 JSONL 的发现 + 增量游标 + Codex 累计 token 状态 + cwd/项目解析 + 正文抽取函数"；
把它们组装成"逐事件、持久化、带正文的 RawEvent 流 + 永不删总库"是这层之上的新建工作。**

---

## 1. 来源发现（paths）⇄ §3

| 维度 | 设计稿 §3 | QuotaBar 实际 | 差异 / 动作 |
|---|---|---|---|
| Claude 根 | `projects/`、`memory/` | `<config>/projects` + **`<config>/sessions`** | **补 `.claude/sessions`**；`memory/` QuotaBar 根本没扫（greenfield） |
| Codex 根 | `sessions/YYYY/MM/DD/rollout-*.jsonl` | `<config>/sessions` + **`<config>/archived_sessions`** | **补 `archived_sessions/`**（重大遗漏） |
| 发现方式 | 写死日期分桶 glob | **递归找根下所有 `*.jsonl`**（`find_jsonl_files` / WSL `find -name '*.jsonl'`） | **证实报告**：rollout 日期层只是文件命名，真正契约是"根下任意 `*.jsonl`"。glob 降级正确 |
| 配置根覆盖 | `CLAUDE_CONFIG_DIR` / `CODEX_HOME` | 同（`local_*_config_dir`） | ✓ 一致；QuotaBar 暂无 profile / `CODEX_SQLITE_HOME` 概念（greenfield） |
| location | `local` / `wsl:<distro>` | 同；WSL 用 `wsl.exe -l -q` 枚举、`is_user_distro` 排除 `docker-desktop*` | ✓；新增账号可在 config 指定 location |
| 控制面来源 | commands/agents/skills/plugins/tasks/hooks/rules/.mcp.json/memory/instruction/history | **一个都没扫**——QuotaBar 只扫会话 transcript JSONL | 全部 greenfield；与"加描述符即可"一致，但要新写 snapshot 解析器 |

**结论**：抽取得到的是**会话 JSONL 扫描器**；§3 里除 session 外的所有来源族都是从零建（架构已为其预留 `source_mode`）。

---

## 2. 增量游标 ⇄ §8

| 设计稿 §8 | QuotaBar 实际（`AgentSessionFileRow` + `parse_source_file`） | 核对 |
|---|---|---|
| `safe_offset` 字节游标 | `safe_offset` / `mtime` / `size` / `parser_state_json` / `parse_status` 持久化 | ✓ 完全一致 |
| 只解析完整行，半截不推进 | `split_complete_jsonl`：无尾换行则回退到最后 `\n` | ✓ |
| 坏行跳过、不推进、待重试 | 坏行计数 + `parse_status=error`，`safe_offset` 保持 `known_offset`（整批不前进） | ✓（增量路径：一坏行冻结**整批**尾，下轮重读整段尾；SV `scan` **一次性全扫**另走 `Partial` 保留好行，见 INGEST_KERNEL §8 规则 2） |
| `(mtime,size)` 回退→归零重建 | 两处：①`row.mtime>file.mtime && row.size<=file.size` → `metadata_rollback_marker`（offset=0、error、安排全rebuild）；②`read_local_*` 仅当 `size<=` 且 `mtime<=` 且 ok 才复用 offset，否则 0 | ✓ 但判据是 **(mtime,size) 回退**，非内容哈希 |
| WSL 增量 | `WslReadMode{CacheHit,Append,Full}`，由 `(size,known)` 决定；`tail -c +k`；截断→Full | ✓ 与 §8 截断检测对应 |
| Codex `codex_state.previous_total` 为标量 | **实为结构体 `CodexRawUsage{input,cached,output}`** | **改 §8/§7**：`previous_total` 是三元组累计 |

**乐观假设暴露**：截断/重写检测在 QuotaBar 里是 **(mtime,size) 回退**，不是内容指纹；§8 我写了 `content_hash` 头部哈希——对 `append_log` 来说 QuotaBar 的 (mtime,size) 已够用且更省，应以它为准，`content_hash` 留给 `snapshot_file`。

---

## 3. RawEvent 字段 ⇄ §7（核心对账）

QuotaBar 把"事件"拆成 `UsageFactRow` + `TranscriptMessage`，没有统一 RawEvent。逐字段：

| §7 RawEvent 字段 | QuotaBar 来源 | 状态 |
|---|---|---|
| `occurred_at` | `ts_unix`（Claude 顶层 `timestamp`；Codex `value.timestamp`），`parse_ts` 收秒/毫秒/RFC3339 | ✓ 有，质量好 |
| `time_confidence` | **无**——Codex 无时间戳的 token 事件直接跳过；Claude hint 记 `None` | **greenfield**（实践中多数行有时间戳） |
| `actor` | `TranscriptMessage.role`（user/assistant/tool/system/**thinking**） | ✓ 但仅在即时 transcript 路径，未落库 |
| `event_type` | 隐含：usage / message / function_call / function_call_output / reasoning(thinking) | **需显式建模**；Codex payload.type 已能区分 |
| `content` | `TranscriptMessage.content`（`extract_text`） | ✓ 有抽取函数，但**即时、不持久** |
| `cwd` / `project_root` / `_source` / `_confidence` | `resolve_project_root` + `normalize_cwd_for_location` | ✓；source 实际值=`git`/`marker:<file>`/`cwd`/`wsl_cwd`/`missing_cwd`（§7 写的 `none` 应改 `missing_cwd`，且 marker 带文件名） |
| `model` / `effort` | `UsageEntry.model/effort`（Codex `turn_context.effort`） | ✓ |
| `input/output/cache_creation/cache_read_tokens` | `UsageFactRow` 同名（Codex 由 `last_token_usage` 或 `total-previous` 求 delta） | ✓ |
| `message_id` / `request_id` | Claude `message.id` / 顶层 `requestId`；**Codex 恒 `None`** | ✓ Claude；Codex 无 |
| `seq` | `base_seq + 行号`（增量时接已存最大 seq+1） | ✓ = 文件内行位序 |
| `parent_ref` | **QuotaBar 完全不读 `parentUuid`** | **greenfield**（§7 "观察到 parentUuid" 不成立于现码） |
| `source_mode` / `content_hash` / `raw_reference` / `parser_version` / `schema_version` / `git_branch` | **无** | **greenfield**（本契约新增） |
| `source_location` | `location` | ✓ |
| —（设计稿缺） | **`workspace_location`**：项目物理所在（WSL 项目可在 local transcript 下）与 transcript 存储位置区分 | **§7 应补**：QuotaBar 有这个有用的二分，我漏了 |

**去重唯一键**：§7 写 `(source_type, source_location, source_path, source_session_id, seq)`；
QuotaBar fact 按 `(provider, location, source_path, session_id, seq)` 落库、Claude 跨文件再用 `(message_id, request_id)`。→ 键一致；`content_hash` 仅相似去重（与设计一致）。

### 三处最关键的乐观假设

1. **正文未持久化**：QuotaBar 只存折叠 `search_text`，**完整正文用完即弃**。"`full` 档总库存正文"是**全新存储**，不是平移。是 P3 总库的主要新建量。
2. **逐消息事件不存在**：facts **只在 usage 事件产生**（Claude `type=assistant` 带 usage；Codex `token_count`）。用户提问、工具结果、纯文本助手消息**都不成 fact**。RawEvent 要"逐消息"，得把 transcript 路径（即时、含正文）升级成持久事件源——这是抽取后的主要改造点。
3. **thinking/reasoning 的取舍**：QuotaBar 故意把 thinking(Claude)/reasoning(Codex) 排除出搜索、仅在 transcript 显示为独立气泡；Codex reasoning 常为 `encrypted_content`（无明文）。RawEvent 必须决定是否收 `thinking`，且明文不可得时按 opaque 处理。

---

## 4. 抽取地图（QuotaBar fn → session-vault 模块）

| QuotaBar 源 | → session-vault | source_mode |
|---|---|---|
| `paths.rs` 常量 + `session_index::{local_provider_roots, wsl_provider_roots, find_jsonl_files}` | `catalog` / `discover`（来源根 + 递归 *.jsonl） | append_log |
| `session_index::{read_local_source_files, read_local_tail, read_wsl_source_files}` | 增量读取层 | append_log |
| `session_index::{split_complete_jsonl, parse_source_file, metadata_rollback_marker}` | append_log 游标引擎 | append_log |
| `session_index::{parse_claude_lines, parse_codex_lines, CodexParserState, CodexRawUsage}` + `usage/blocks::{parse_claude_jsonl_entry, UsageEntry}` | claude / codex 解析器 | append_log |
| `session_transcript::{message_from_*_value, messages_from_*_value, extract_text, extract_*_thinking/reasoning, parse_ts}` | **正文抽取**（升级为持久 RawEvent.content 路径） | append_log |
| `session_index::{resolve_project_root, normalize_cwd_for_location*, canonical_wsl_unc, split_canonical_wsl_cwd, workspace_location, find_upward}` | 共享 项目根/路径 解析 | — |
| `session_index::{extract_claude_session_id, session_id_from_path, looks_like_uuid}` | 会话归属 | — |
| `wsl/mod.rs`（整模块） | WSL 桥（含 framing 协议、distro 枚举） | append_log（+ 将来 snapshot 单文件读已有 `read_file_at`） |
| `storage/db.rs` 行结构 | **不抽取**（消费者侧）；但它们定义了事实上的 RawEvent 字段集 | — |

**纯 greenfield（QuotaBar 无）**：snapshot_file 解析器、sqlite_store、派生路径解析器、统一持久化 RawEvent（含正文）、`parent_ref`、`time_confidence`、`source_mode`/`content_hash`/`parser_version` 戳记、append-only 不可变总库。

---

## 5. 据此应改的设计稿点（待并入）

1. §3：Claude 补 `.claude/sessions`；Codex 补 `archived_sessions/`；明确"递归 *.jsonl"是 session 来源族的真实契约。
2. §7：`project_root_source` 取值改 `git | marker:<file> | cwd | wsl_cwd | missing_cwd`；新增 `workspace_location`；`parent_ref` / `time_confidence` 标注为"QuotaBar 无、本契约新增"。
3. §8：`codex_state.previous_total` 改为 `{input,cached,output}` 结构体；`append_log` 截断检测以 `(mtime,size)` 回退为准（`content_hash` 归 snapshot_file）。
4. §6/§13：明确"正文持久化"是总库新建能力，非来自 QuotaBar。
5. 黄金语料必须含：Codex `last_token_usage` 优先 / `total-previous` 回退、一文件多 session（Claude fork 重放 parentUuid、Codex 多 `session_meta`）、thinking/encrypted reasoning、(mtime,size) 回退重建、WSL Append/Full/CacheHit 三态。

> 这些改动多为"把设计稿往 QuotaBar 实证收敛"，属易变层/字段级，不动 ADR-025 的四项架构保险。
