# Windows Antigravity CLI 来源

## 首版边界

- 仅 Windows `store` 构建支持。将 `SVAULT_ANTIGRAVITY_CLI_DIR` 显式设为本机盘符下的绝对目录；不设时不扫描。单独指定 `scan-all --only antigravity-cli` 不枚举其他转录来源。只取 `brain/<UUID>/.system_generated/logs/transcript.jsonl`，不读取 Hook 指向的第二份 `transcript_full.jsonl`，不读 App、独立 IDE、WSL 实例或更老的二进制转录。
- `source_type=antigravity_cli`，`source_location=local`，`source_mode=append_log`。从目录取会话 ID，只输出 `USER_INPUT / USER_EXPLICIT / DONE` 与 `PLANNER_RESPONSE / MODEL / DONE` 中 `content` 非空的消息，采用原始 `created_at` 和可用的助手 `model`。`--profile full` 带正文，`metadata` 不带正文。普通 user 消息不证明是人类直接输入。其他步骤、推理、工具参数和未校准的 token 数不输出。
- 不启动客户端、不装 Hook、不写原始转录或摘要。`scan-all` 不带 `--write-store` 时输出 NDJSON；默认可读写扫描游标，`--stateless` 则从头扫，不读取或保存游标。配置 Antigravity CLI 数据根后，状态文件必须位于数据根之外；按解析后的 Windows 路径检查，在打开总库或输出事件前拒绝冲突路径。保存时独占创建同目录暂存文件，不覆盖既有暂存路径。
- 首版只输出扫描流。`--write-store` 遇到显式 `--only` 中的 `antigravity-cli`（包括混选）就在打开总库前以退出码 `1` 拒绝。常规写库扫描在枚举前排除该来源。Rust 的总库通用投影入口（如 `TotalStore::apply_projection`）也拒绝该来源，即使事件批为空。

## 归属与会话绑定

有 `--store` 时使用指定的已有总库，否则使用默认路径。只读 `scan-all`（含 `--stateless`）从已有总库加载项目根注册表。扫描 Antigravity 时还读取该库生效的共享会话绑定（合并 `store_meta['session_bindings_json']` 与共享文件配置）。确认没有总库时按空项目根注册表扫描，仍加载共享文件配置。

共享 `session_bindings.json`、`SVAULT_SESSION_BINDINGS` 指定的文件及库内键沿用旧写者支持的 v1 格式。共有的 `temporaryRoots` 和未指定 backend 的 `pendingSources` 也约束 Antigravity。含 `backend: "antigravity_cli"` 的绑定记录或未决条目须放进另一份 v1 JSON 文件，并把 `SVAULT_ANTIGRAVITY_SESSION_BINDINGS` 指向它。该文件仅供 Antigravity 扫描读取，每轮与共享规则合并一次。未设置该变量时只用共享规则。显式指定的专属文件缺失、不可读、格式无效或包含其他 backend 时，Antigravity 报 `unknown_hold`，原有来源继续按共享规则处理。误将新 backend 放入共享清单会触发整份共享清单的故障保护。

Rust 自动加载绑定的 append-log 扫描入口也读取专属文件。显式传入绑定的调用方可用 `SessionBindingRegistry::for_antigravity_scan()` 在共享注册表上生成本轮快照并传给 Antigravity 来源。快照仅用于扫描，没有可持久化的共享 JSON。

归属证据是同一数据根的 `conversation_summaries.db` 中对应 `conversation_id` 的 `workspace_uris`。解析 Windows 盘符和 UNC `file:` URI 后用项目根注册表判定，只有所有工作区都能归到同一已注册根才填项目归属。确认缺摘要库、缺摘要行、URI 列表为空、包含不支持的 URI、空注册表或没有唯一登记根时，允许输出未知归属。不用 `project_id`、路径叶子名、Tool Cwd 或最近会话缓存猜项目；`cwd` 保留空，工作区不是命令执行 cwd。

`--only` 仅选 Antigravity CLI 时跳过 WSL 发现和挂载探测，重复指定同一来源也走定向路径。匹配使用已登记的路径，包含能直接规范化匹配的 WSL UNC 工作区。需要实际挂载表才能收敛的盘符与 `/mnt/<盘>` 写法在定向扫描中各自匹配，无法匹配时归属为空。混选其他来源时沿用常规发现和挂载探测。`scan-all` 的 `wsl.exe spawn attempts` 日志覆盖整轮，包含启动失败。

每次查询摘要由 SQLite 取得一份已提交快照。Windows 上保留 SQLite 原生锁和 WAL 协议，只打开已有文件，禁止创建、扩展或改写摘要库及其 `-wal`、`-shm`、rollback journal，也不在关闭时做 checkpoint。长驻写者有未提交事务、未 checkpoint 的已提交 WAL，或 `FULL` checkpoint 后 WAL 仍有字节时，都可读取 SQLite 能取得的已提交快照。写者正常退出并清理旁文件后，若摘要库仍标记为 WAL 模式，只有 Windows 已阻止主文件被写入、删除或替换，且确认没有 WAL 和 rollback journal 时，才按不可变文件读取。每次摘要查询都独立取快照，转录随后单独读取。

遇到独占锁、WAL 所需的共享索引缺失、SQLite 无法取得读快照或摘要损坏时，本轮报告问题，保留该来源的游标，暂停事件和替换信号，以便下轮重试。无法打开已有总库或读取项目根注册表时，也暂停该来源输出并保留游标，摘要带 `roots_unavailable`。

2026-11-26 核对了 SQLite 的读隔离、只读 WAL、`immutable` 语义，以及 bundled SQLite 3.46.0 的 Win32 `readonly_shm` 实现。 [读隔离](https://sqlite.org/isolation.html) · [只读 WAL](https://sqlite.org/wal.html#read_only_databases) · [URI 参数](https://sqlite.org/uri.html) · [SQLite 3.46.0 Win32 VFS](https://github.com/sqlite/sqlite/blob/version-3.46.0/src/os_win.c)

即使转录是零字节、没有新增字节，或本批没有可输出的消息，也用目录中的 UUID 检查控制面准入。Antigravity UUID 比对忽略十六进制字母大小写，事件仍保留来源的 ID 拼写。未决或绑定读取失败（`unknown_hold`）、临时排除（`temporary_excluded`）均不发事件、不发替换信号、不推进游标；看每来源报告的 `admission_status` 区分。

## 增量、改写与坏尾

- Antigravity 游标保存在基础状态路径追加 `.antigravity.json` 后缀的文件。默认使用 `<data_local_dir>/svault/cursors.json.antigravity.json`。只读扫描和旧写库扫描传同一个 `--state` 时，各自保存自己的文件。定向和混选使用同一份专属游标。首次发现专属文件缺席时可承接开发版曾写进共享文件的 Antigravity 游标，定向扫描只读旧文件。混选的 `state_saved` 表示本轮涉及的状态文件是否都保存成功。
- 每轮读取并核验至多 64 MiB 的转录。旧前缀与归属一致时只解析新增完整行，末尾半行延期；增量遇到坏完整行时整批冻结，不发事件或推进游标。已覆盖的完整行包含无效 UTF-8 时也不发事件或推进游标。
- 已消费的前缀被截短或改写时，从头读并报告 `rollback_or_rewrite`；前缀不变但已消费内容的归属改变时，从头读并报告 `reparse_required`。这是扫描重读源与注册表后报告的变化，不靠总库的 `reattribute` 按 `cwd` 重算。替换读遇到末尾半行或坏完整行时不发替换信号，保留旧游标供下轮重试。准入允许时，清空文件或只剩合法的不输出步骤都可以形成空替换；可输出类型的完成消息缺少字符串 `content` 则算坏行，不是空替换。没有游标的从头读（例如 `--stateless`）遇到坏 JSON 或无效步骤字段时，仍可输出其他完整好行并报告跳过项；这不是一份已确认的完整替换。
- 流式消费方收到 `source_report` 的 `replacement` 时，用该报告前属于该来源的一批事件替换在消费端保留的结果，而不是追加；空替换批也要处理。这里的替换不涉及总库。没有替换信号的零事件批不是清空通知。

## 证据与消费影响

2026-11-26，真实 Windows `agy 1.5.11` 使用隔离目录与本机固定响应的 Gemini 兼容端点：同一进程两轮、再启动新进程按 ID 恢复一轮，转录 2→4→7 行，保留旧字节、文件身份及已完成的步骤；模型请求确实包含先前两轮上下文。Hook 与摘要均给出双工作区，Hook 路径实测为 `transcript_full.jsonl`。这不是运行中、崩溃恢复或其他客户端的只追加保证。本机摘要库工作区 URI 覆盖 18/20 份 CLI 转录；独立 IDE 不在首版范围。外部依据（同日核对）：[官方 Headless](https://antigravity.google/docs/cli/headless/)、[恢复命令](https://antigravity.google/docs/cli/commands/resume)、[Hooks](https://antigravity.google/docs/hooks)、[第三方索引结构参考](https://github.com/NightMean/Antigravity-Conversation-Migrator)。

- **CLI**。`--only antigravity-cli` 的发现与事件行使用来源值 `antigravity_cli`。流式消费方应处理 `replacement`，并检查 `discovery_unreachable` 后再判断缺席。定向发现遇到单个异常会话时继续扫描健康会话，摘要以 `["local"]` 标记枚举不完整。数据根本身无效或无法枚举时，定向命令退出 `1`。`roots_empty` 沿用旧版语义，表示本轮可用根为零。根读取失败时另带 `roots_unavailable`。部分失败时可保留已读取的根，`roots_empty` 为 `false`。部分发现仍可退出 `0`，`discovery_unreachable` 按位置汇总。
- **Rust 构造与穷举**。`SourceType` 新增 `AntigravityCli`；`Cursor` 保持原有公开字段，Antigravity 的前缀与归属双指纹按 `agv1:` 版本化格式写入既有 `Cursor.content_hash`（反序列化自动忽略早期开发版的额外字段，旧裸 `sha256:` 游标触发一次 `reparse_required` 升级）。既有消费方的 `Cursor` 完整字面量与穷举解构不受影响；唯一编译断点是对 `SourceType` 的穷举匹配（例如 QuotaBar `src/svault_bridge.rs:56` 的 `provider_key`），禁用环境变量也无法消除该匹配断点。
- **Rust 逐字段**。应完整持久化扫描游标，其中 `content_hash` 对应同一轮的 `safe_offset`（Antigravity 来源在 `content_hash` 中同时携带 `agv1:` 前缀与归属双指纹，消费方不得改写或截断该字段）。重扫观察按 `SourceChange` 分派，项目归属读取独立字段，`cwd` 保留空。
- **discover / `SourceMode`**。Windows `store` 构建显式配置根后，通用 `discover`、`discover_all_reported` 及定向的 `discover_antigravity_cli_reported` 返回本地 `AppendLog`。已有会话索引用的 `discover_transcripts_reported` 只枚举可投影的转录，`discover_store_sources_reported` 用于总库范围的发现。只选择其他来源的 CLI 扫描也在枚举前排除 Antigravity。写者的 `unreachable` 因此只反映参与枚举的来源。

2026-11-26，`cargo test --features store --lib --bin svault --test cli_scan_baseline_it` 在 Windows 通过 517 项库单测、28 项 CLI 单测和 24 项常规集成测试（含 2 项 `Cursor` 旧字段与 `agv1:` 游标双指纹兼容单测），`cargo clippy --all-targets --all-features -- -D warnings` 及默认/`store` 双构建均通过；在 WSL 通过 501 项库单测、28 项 CLI 单测、7 项集成测试及默认/`store` 双构建。Windows 夹具隔离了 WSL 枚举，实跑常规写库、定向扫描和普通发现。另用当前构建的 `svault` 离线复读上述隔离转录，启用后输出 3 条用户和 3 条助手消息，停用时输出 0 条。原始文件字节一致，目标总库和密钥均未创建。

同日，在独立沙箱副本中将本仓候选源码接入 QuotaBar 复核消费方兼容（未改动 QuotaBar 原仓）。未改 QuotaBar 源码时，全目标编译仅报 1 处错误：`src/svault_bridge.rs:56` `provider_key` 未覆盖 `SourceType::AntigravityCli`，原有 2 处 `Cursor` 字面量与解构零断点。在沙箱内仅补 1 行 `SourceType::AntigravityCli => "antigravity_cli"` 后，`cargo check --all-targets` 通过，`svault_bridge`（19/19）与 `session_index`（100/100）库单测全绿。另在隔离夹具中用 `v2026.11.11.1` 与候选 CLI 做双版本 CLI 对拍：既有 `pull` 输出字节一致，候选游标保存为 `agv1:` 且不含独立旧字段，共享 `--state` 基础路径互不干扰。

同日，Windows 定向扫描夹具使用已有总库，并在测试 CLI 旁放置无效的 `wsl.exe`。修复前，项目根加载尝试启动一次。修复后，单选 Antigravity、重复选 Antigravity、以及未配置 Antigravity 根时的定向扫描均为零次。两份会话仍各自匹配已登记的本机盘符根和 WSL UNC 根。混选 JSONL 时尝试启动三次，分别来自 append-log 发现、快照发现和挂载探测，摘要报 `discovery_unreachable: ["wsl:*"]`，同时输出 JSONL 事件。三次启动尝试均由夹具拦截。

同日，Windows 并发夹具验证了这些交错。库内读事务建立快照后，另一连接提交并尝试 `PASSIVE` checkpoint。原事务持续读到旧值，新事务读到新值。CLI 在写事务未提交时仍取得上一份已提交 WAL 快照，也能在 WAL 仍非空的 `FULL` checkpoint 后继续读取增量。rollback journal 的独占锁阻塞读取时，CLI 保留游标，解锁后补读。WAL 缺共享索引时暂停输出，保留游标，索引恢复后重试成功。夹具核对了读取前后的源文件内容，关闭写者并清理旁文件后，扫描也没有重新创建它们。

同日，另用 Windows 正式发行版 `v2026.11.11.1`（`b9339c359acc`）和当前构建对拍同一个隔离库。专属文件先有效、再损坏，新版分别报告 `temporary_excluded` 和 `unknown_hold`，旧版每轮新增写入一条 JSONL 事件。把 `antigravity_cli` 记录误放进夹具共享文件后，旧版报告 `unknown_hold`，写入 0 条。恢复共享文件后，旧版写入此前暂停的那条事件。新版回归还检查了库内共享清单拒绝该 backend 且保留原值，以及共享临时目录和未决路径继续约束 Antigravity。此实测覆盖旧版 Windows CLI，QuotaBar 未参与对拍。

同日，Windows 发现夹具同时放置正常和异常的 Antigravity 转录。通用发现仍返回正常来源并报告 `local` 枚举不完整。常规写库继续写入 JSONL，写库扫描、仅选 JSONL 的只读扫描和 Rust 会话发现入口均未收到该 `local` 标记，独立的 WSL 失败标记仍保留。项目根全部无法读取的夹具继续写入普通来源，摘要同时带 `roots_empty: true` 和 `roots_unavailable`。注册表确实为空时只带前者。部分读取成功时保留有效根，Antigravity 仍暂停输出。

同日，双版本对拍中，新旧 CLI 传同一 `--state` 基础路径。旧 CLI 继续写入 JSONL 并保存旧文件，Antigravity 专属文件字节一致。新版复扫未变化转录时输出零条事件且无替换信号。定向与混选交替、以及首次从开发版共享文件恢复游标，也有隔离回归。

复跑双版本绑定和游标用例时，把 `SVAULT_TEST_LEGACY_CLI` 指向该 Windows 发行版的绝对路径。测试会校验版本，将其链接或复制到夹具目录，并在夹具内运行。

```powershell
cargo test --features store --test cli_scan_baseline_it antigravity_dedicated_files_preserve_legacy_cli_writes -- --ignored --exact
```
