# 待做清单历史快照（2026-11-11）

> 2026-11-12 整理前的原文，保留历史实测、交付和决定，不再维护。下文的“当前”“下一步”均以原整理日期理解；现行顺序与状态只看 [backlog.md](backlog.md)。

> 最后整理：2026-11-11。这里维护当前任务、依赖和执行顺序；带日期、划线及提交号的条目是历史交付记录，不算当前队列。
> 做完一项就划掉并写上提交号；顺序以本页为准，A–H 是稳定分类，不表示执行先后。
> 标记：**[人]** 要人做或人拍板；**[本仓]** 本仓动手；**[等]** 等外部条件。文档登记、源码完成、正式发布和消费方实际接入分别核验。

## 当前状态与下一步

截至 2026-11-11，最新正式发布与本机安装为 `v2026.11.11.1`（`b9339c3`）。已亲核 GitHub Release、回下载 Windows / Linux 产物及依赖锁的 SHA-256，并核装机 `svault --version` 与二进制哈希一致。源码合入、正式发布、运行写者升级与消费方实际接入分别记账，不能相互替代。

本次 `v2026.11.11.1` 已包含：H.4 DSH v4 复杂语义（`dc682ff`）、细粒度用量证据（`498b7a3`）、G.1–G.3 有效视图与分页核查 / 压缩件回源早停（`d7d3315`）、显式根常规来源接线与 `stores` / `pull` 窄查询（`29beb8d`），以及准入 / 游标一致性补修（`2e60192`）。证据与边界见 [DSH 核查记录](dsh-ingest-survey.md) §8–§19、[读取状态设计](dsh-read-state-design.md) §6–§15。

有效绑定指纹、独立投影身份与受控提交栅栏（`42da63e`）、完整临时空目标补修（`7441ee6`）已合入并推送 `main`。后者覆盖已有当前投影清空、旧代保留、首次临时空源与重复提交；上游最终验证 Windows `store` 库 493 / 扩展库 494、Linux 扩展库 479 条及双平台严格 Clippy 均通过。消费方状态与证据保真适配已验；本轮另修总库未就绪冻结、全读基线及损坏状态拒绝，Windows 完整门与 Linux 编译、相关回归及探测门均通过；消费方源码交付以其仓库记录为准。本机独立 CLI、Rust 常驻写者及内嵌 CLI 写者均已升级并实核新构建，旧只读连接保留。桌面正常后台刷新通过，未手工重建、重算或回收总库。

| 顺序 / 条件                            | 对应任务                                           | 下一步具体产物与完成判据                                                                                                                                                                                                                                                                                                                                                                                                                    |
| -------------------------------------- | -------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| ~~H.1 / H.2 / H.3 源码与发版~~         | ✅ 已发布 `v2026.11.09.1`（`a151997` / `ccfbdca`） | 已在 [`src/session_binding.rs`](../src/session_binding.rs)、[`src/rawevent.rs`](../src/rawevent.rs)、[`src/scan.rs`](../src/scan.rs) 与 [`src/store.rs`](../src/store.rs) 落地并随 `v2026.11.09.1` 正式发布；TumeChat 可直接用已发布 CLI，QuotaBar / TumeFlow 可按需升级子模块指针                                                                                                                                                          |
| ~~G.1 / G.2 / G.3 核查与实现~~         | ✅ 已合并 `main`（`d7d3315`）                      | 已完成 Claude `isCompactSummary` / `compact_boundary` 与 Codex `compacted` / `paginated` / `history_base` 的本机与 WSL 实测；在 `classify_user_input_origin` 最前端识别压缩摘要并导出 `is_compaction_summary` / `effective_view_slice`；`read_codex_origin` 改为流式逐行早停                                                                                                                                                                |
| ~~H.4 正式来源接线~~                   | ✅ 已合并 `main`（`29beb8d`）                      | 显式配置根下的 DSH v4 已接到 `catalog` / `discover` / `scan-all`，复用 `Jsonl` + `AppendLog`，保存 `Cursor.dsh_state` 并核丰富前后旧前缀摘要；通过 `ReparseRequired` 保留公共扫描的重投影观察，完整目标提交覆盖读取领先入库与 A→B→A 回退；未配置根不增加来源。复杂语义与细粒度用量已在 `main`（`dc682ff` / `498b7a3`）。双平台库测试、CLI 回归及各 61 项支持契约检查已通过；旧写者限制保留。已随 `v2026.11.11.1` 发布，真实来源接入仍另验。 |
| ~~H.1 / H.2 准入与游标一致性源码补修~~ | ✅ 已合入 `main`（`2e60192`）                      | 修复混合会话误排除、过滤后序号复用、Codex 父关系跨空批丢失、WSL 路径别名、事务前读取绑定、数据库读取失败被当成空规则及全读重试丢失截断观察；非法显式 `state` 进入 `UnknownHold`。`PARSER_REVISION` 升至 5，`CodexState` 新增默认空的 `session_parents` 映射；双平台验收通过，已随 `v2026.11.11.1` 发布，消费方实际接线仍另验。状态与调用方义务见 [扫描状态模型](scan-state-model.md#控制面过滤与游标连续性2026-11-10)。                     |
| ~~H.1 / H.2 绑定身份与完整临时空目标~~ | ✅ 已合入 `main`（`42da63e` / `7441ee6`）          | 有效规则独立参与投影身份；总库写事务入口复核整轮快照，应用失败毒化整批。已有当前投影的完整临时空目标切头清空并保留历史；首次临时空源不建代，重复完整目标不开新代。消费方需显式采用受控提交 API，运行写者升级另验。                                                                                                                                                                                                                          |
| ~~本机运行写者升级~~                   | ✅ H.7：已装机核验（`v2026.11.11.1` / `b9339c3`）  | 本机独立 CLI、Rust 常驻写者与内嵌 CLI 已重建安装，分别经版本 / 哈希、实际启动标记及真实健康响应核对新构建；定时入口无旧 CLI 覆盖，旧只读 MCP 连接保留。旧版整代重写仍会删除未知 `usage_evidence`，升级不抹去这条有限兼容限制；真实来源接入另验，详见 [读取状态设计 §15](dsh-read-state-design.md#15-h7-兼容验收与升级边界)。                                                                                                                |
| 相关功能接入时                         | H.5 / H.6、G.4：血缘与跨主机证据                   | 父子会话控制面与日志血缘已在 H.1/H.2（`a151997`）接入临时排除；后续按任务追溯、原生转写或远程回源的实际需求补齐重放防重和跨机证据 / 删除状态；本地只读与墓碑出口已可复用，远程认证与编排归上层                                                                                                                                                                                                                                              |
| 外部条件或专项开工后                   | A / D、E、F、C 的其余项                            | 副本传输等组网条件；统一项目检索标识、旧事件归属复核、长空格字面量排查及存储体积优化按各节推进。真库回退旧代回收仍按人定“先不删”                                                                                                                                                                                                                                                                                                            |

2026-11-11 一致性补修已合入 `main`（`2e60192`），后随 `v2026.11.11.1` 发布并替换本机 CLI。以下保留源码合并时的验收记录：Windows `store` 库 482 条、全 feature 库 483 条，Linux 库 468 条；双平台 CLI 各 28 条、扫描基线各 5 条、只读及多库合读各 2 条，以及默认 / `store` 编译、严格 clippy 通过；Windows doctest 2 条通过、1 条示例忽略。归一化各 25 项，正式来源矩阵各 61 项支持契约通过，仍以退出码 2 保留旧写者字段损失限制。验收另修复指纹覆盖未消费半行及有损解码导致字节偏移错误，完整行边界由一个函数共用。此前 2026-11-10 的 `WslService = StopPending` 阻塞已解除，本轮只读确认服务为 `Running`，未执行恢复操作。`apply_full_projection_target` 已作为加法 API 公开；消费方编译、完整回退目标接线和运行写者升级分别验收，不能用本仓通过代替。

DSH v4 复杂语义与会话根目录批量发现/扫描提交（`discover_dsh_sessions_in_root` / `scan_dsh_sessions_root_to_store`）已合入 `main`（`dc682ff`）并在 Windows / WSL 双平台通过 24 项端到端检查。一般高并发压力、租约争夺和完整 WSL DSH 捕获仍未验，不能把指定受控窗口通过视为一般并发验收。

## 哪些影响消费方使用

| 项目                           | 当前状态与影响边界                                                                                                                                                                                                        |
| ------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| H.1 / H.2 临时契约与排除       | 已随 `v2026.11.09.1`（`a151997` / `ccfbdca`）正式发布，覆盖 CLI 与 `TotalStore::apply_projection_if` 多写者及子会话血缘；QuotaBar / TumeFlow 需各自 bump 子模块指针                                                       |
| H.1 / H.3 真实来源与 peer      | 已随 `v2026.11.09.1`（`a151997` / `ccfbdca`）正式发布，`RawEvent::input_origin` 区分 `HumanVerified` / `PeerSession` / `CoordinatorDispatch` / `Unknown`                                                                  |
| H.4 正式宿主来源               | 复杂语义与用量证据已在 `main`（`dc682ff` / `498b7a3`）；常规显式根接线（`29beb8d`）已随 `v2026.11.11.1` 发布。                                                                                                            |
| H.7 写者与字段兼容             | 本机登记写者已完成升级与运行身份核验。旧版普通追加可共存、整代重写不保真的限制仍保留；消费方需保存完整 Cursor、执行完整重投影并保真转存证据，来源联合验收仍独立进行。                                                     |
| G.1 / G.2 / G.3 有效视图与分页 | 已合入 `main`（`d7d3315`）：压缩摘要识别为 `SubagentOrRuntime`（`is_compaction_summary` / `effective_view_slice`），Codex `paginated` 分段已核不重计 token，`read_codex_origin` 已流式早停；已随 `v2026.11.11.1` 正式发布 |
| H.5 / G.4 血缘与重放           | 父子会话血缘已接入临时会话继承（`a151997`）；跨工具原生转写重放防重与完整任务树追溯待消费方接入时补齐，普通历史读取不以之为前置                                                                                           |
| H.6 来源主机证据 / 删除状态    | 跨设备回源、离线状态及下游删除闭环需上层接合；本仓已有本地只读与墓碑出口，不等于删除能力整体未实现                                                                                                                        |

已发布的 `sessions-recent` / `sessions-read`、`stores`、`pull` / `changes`（逐库）、`roots` / `attribute` 和 `erasures` 可供消费方先做合成接入准备；本轮已向消费方核对版本、源码与边界，没有新增读取接口需求。`current` 是当前解析投影，不是 G.1 的有效上下文窗口；`pull`、`changes` 和会话续读各有自己的游标，`erasures` 没有增量游标；`store_id` 是库标识，不是可信主机身份。`session-origin` 仅查 Codex 记录的仓库远端。

原文索引 / 检索、授权、记忆保存同步及产品界面的实现分别属于消费方与记忆引擎，不移成本仓任务。整库副本同步（A / D）、未来统一检索标识（C）、压缩件早停（G.3）与体积回收（E）均不是本地历史读取的统一前置。

## 对 TumeChat 开发的影响（2026-11-11 核对）

依据：当日核对 TumeChat 的 `docs/PRODUCT.md`、`docs/CROSS_REPO_CONTRACTS.md` 当前增量与维护会话回复；任务执行序只由其 `docs/BACKLOG.md` 维护。本节只记本仓依赖与跨仓边界，不复制消费方内部工单、测试计数或预算。

| 分类             | 仍缺什么                                                                   | 完成判据与责任边界                                                                                                                                                                           |
| ---------------- | -------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 可立即并行       | 宿主控制面到绑定注册表的适配与隔离验收                                     | 已发布 H.1–H.3 可用；消费方在项目隔离目录生成绑定清单，验证临时会话双层零入库、未决冻结与真实来源回读。无需等待所有待办。                                                                    |
| 有界续读待复验   | 生产时限内的元数据复验与新版身份下的续读提案                               | `v2026.11.11.1` 已发布安装并包含窄查询修复。消费方在原生产时限内重核元数据，再按其既定授权流程重出有界续读提案；不能沿用旧二进制批准或以合成测试替代真源验收。只读续读无需等待全部写者升级。 |
| ~~本机写者升级~~ | 已完成，不再阻塞本机接入准备                                               | 已核独立 CLI、Rust 常驻写者与内嵌 CLI 的本机运行版本和写入入口；旧只读连接保留。下一步是实际根、可信绑定与真实来源的受控联合验收，不能把升级完成等同于来源接入完成。                         |
| 对应来源待验     | DSH 正式新会话入库与临时继承                                               | 来源描述符已注册，当前契约仅显式配置根时发现；满足写者兼容条件后，宿主传实际根与可信绑定，用发布 CLI 端到端核验。默认自动发现若另有需求，按规则 2 提出，不预设已经实现。                     |
| 对应功能缺契约   | 任务到宿主 / 子 CLI 的父子关系、官方桌面来源、跨工具转写重放               | H.5 / G.4 与消费方用实际样本确定证据载体；`parent_ref` 是父事件槽位，不能冒充父子会话协议。官方桌面来源尚未专项核验；原生转写选定后才触发重放跳过。跨 `source_path` 的分页串联也未实现。     |
| 记忆闭环待交付   | 正式事项写入 / 修订 / 删除确认、稳定身份、逐事项旧来源抑制、授权主机间同步 | 归记忆引擎与产品控制面；只读状态链基础不等于写侧与同步完成。各方确认版本 / 因果、水位、目标确认、删除 / 撤权和防复活契约后分别实现、装机并联合验收。SessionVault 不另造记忆权威。            |
| 产品侧待完成     | 可信费用核对、真实执行主路径、持久任务授权、跨设备通道、个人空间与双端闭环 | 归消费方自身执行序；不等待本仓整库传输、项目标识或体积回收，也不将这些未完成项误报成本仓代码阻塞。                                                                                           |

## A. 多机同步（个人用，决定见 [linux-replica.md](linux-replica.md)）

以下为原有整库副本方案。新增「原始总库留来源、记忆就地处理」的本地证据出口与上层远程编排边界见 H.6 及 [INGEST_KERNEL.md](INGEST_KERNEL.md) §2.2；不把整库同步当作该模式的交付前置，也不据此把本节任务标成已撤销。

1. **[人]** 公司电脑、家里 Linux、家里 Windows 装 Tailscale、同一账号登录；家里 Linux 开 SSH、常开机。
   装好后给出家里 Linux 在组网里的地址或机器名。
2. **[本仓]** 实测公司 → 家里能否直连（不经中继）与上下行速度；据此定首次推送怎么做。
3. **[本仓]** `svault sync` 子命令：推 → 中转上核对 → 拉 → 本机核对 → `adopt-erasures`；每步重试、加锁；
   Windows 上 `--ssh` 指 Git 自带的 ssh。中转只是一个 SSH 目标，换成家里 Linux 不改代码。
4. **[本仓]** 发布物带上 `sqlite3_rsync`（Windows 版、按官方编译参数从源码编的 Linux 版）。
5. **[人]** 家里两台：装 svault；`key-import --rekey` 换到共用主密钥（**先退出 QuotaBar / TumeFlow**）。
6. **[本仓 + 人]** 各机定时任务（Windows 任务计划 / systemd timer），同步间隔 N。
7. 境外测试中转（专用账号、`relay/`、`sqlite3_rsync` / `sqlite3`）：人 10-10 定**留到家里 Linux 跑通再说**；
   同步子命令先拿它做端到端测试（只用小测试库）。

## B. 发版

以下是发布记录，不代表当前仍待实现：

- ~~发 `v2026.10.10.1`~~ ✅ 10-10 已发（`eaee7ab`）：含合并读取、跨库删除、UNC 写法归属、`open_existing` 只读、
  按项目根删覆盖各种写法；已通知 TumeChat，对方核过哈希、版本与新子命令。
- ~~发 `v2026.10.10.2`~~ ✅ 10-10（`2b250f9`）：含 `reattribute`、盘符写法优先序、发现阶段批量；`--version` 改为
  `<发布号> (<提交>)`，从源码自己编的显示 `dev (<提交>)`（人 10-10 定，原来恒为 `0.0.0`）。已通知三方。
- ~~发 `v2026.10.29.1`~~ ✅ 10-29（`cba8525`）：含 Codex 就地压缩的会话、`recompute_attribution`、WSL 压缩件按身份缓存、
  `rollback_coverage`，以及 `gc` 不再删回退之前的源版本（v2026.10.10.2 会删，这是这次发版的主因）。CLI 子命令与输出
  格式不变。已装到本机 `%LOCALAPPDATA%\svault\bin`，已通知三方。
- ~~发 `v2026.11.09.1`~~ ✅ 11-09（`ccfbdca`）：含 `reclaim-rollbacks`（`ab72363`）、CLI 全读时保留旧来源已消费范围与 mtime 基线（`368b730`），以及 H.1 / H.2 / H.3 控制面会话绑定（`SessionBindingRegistry`、三态 `IngestAdmissionVerdict`）、扫描与 `TotalStore::apply_projection_if` 双层临时会话排除、子会话血缘传递与 `RawEvent::input_origin` 真实来源 / 同侪信封保真（`a151997`）。已装到本机 `%LOCALAPPDATA%\svault\bin`。

- ~~发 `v2026.11.11.1`~~ ✅ 11-11（`b9339c3`）：包含 H.4 常规显式根与复杂语义、细粒度用量证据、G.1–G.3 有效视图与压缩件回源、元数据窄查询，以及准入、游标、绑定快照栅栏和完整临时空目标修复。双平台构建无本机路径泄漏，公开资产回下载校验通过，已安装本机独立 CLI；装机 Windows 二进制 SHA-256 为 `e07b3ec3bfe5a77a77079e311597b757a5ef89d16253bd9574701f72146f275b`。发布说明保留旧整代重写器字段损失限制，运行写者与真实来源验收分别记账。

## C. 消费方提的需求（规则 2）

- ~~QuotaBar【3】发现阶段批量~~ ✅ 10-10 合并（`c337c6d`）：`wsl.exe` 5 → 2（列发行版 1 + 每个发行版 1），输出逐字节不变；
  已通知 QuotaBar / TumeFlow。判据「≤ 发行版数」差的那 1 次是列发行版：要省得由调用方传入发行版列表，
  而列表一旧会碰到它按发现结果删存量的路 —— 留给 QuotaBar 按规则 2 决定要不要提。
- ~~855 条 UNC 写法事件~~ ✅ 10-10：`svault reattribute` 合并（`dc32d4f`），人退出 QuotaBar / TumeFlow 后对真库执行：
  271 个来源各写一代新投影，836 条改成规范形（`workspace_location` 从被错算的 `local` 改对），旧代删除；
  执行前用 SQLite 备份接口留了一份快照。剩 19 条属于下一项那个有两种登记写法的目录。已通知三个消费方。
- ~~盘符写法统一成 `C:\`~~ ✅ 10-10：代码 `2b472e3`（同一目录几种登记写法由 `attribution::spelling_order` 定，
  归属与 `roots` 共用、与行序无关）；QuotaBar v0.8.14 与 TumeFlow 换上新版后，人退出两个程序，先备份再
  `reattribute --apply`：106 个来源、111,799 条，那个目录的 `/mnt/c` 写法 94,550 → 0、UNC 19 → 0、盘符 136,092；
  全部解密核过。旧代里还有 12,914 条 `/mnt/c` 写法（非当前代，按设计不回收）。已通知三方，请 QuotaBar 核它自己的索引。
- **[本仓，待复核]** 10-10 留下的旧事件位置修正：当时估计约 1,462 条来自 WSL、本来就写成盘符的旧事件，
  `workspace_location` 仍为 `wsl:<distro>`，在 6 条上核到应为 `local`，其余未逐条解密；这是历史快照，本轮未查真库。
  **11-05 订正处理路径**：现有 `TotalStore::recompute_attribution` 已能重算归属三项，不再预设给 `reattribute` 加代码。
  先由写入方确认受影响来源是否仍有残留：源还在则重扫，确认消失的来源再评估复用重算接口；真库改写另按授权、备份与停机协调执行。
- ~~两份真库备份的去留~~ ✅ 10-10 人定删除，已删（两次改写都已逐条解密核对）。
- **[待议，并入项目标识]** QuotaBar 10-10 重建索引后的两条写法观察（界面上都没坏，它按身份 / 目录折叠了）：
  ① 没归属、cwd 在 `/mnt/<盘>` 下的 WSL 会话（125 个、18 个目录），`project_root` 原样是 `/mnt/<盘>/…`：优先序只管
  「注册表里登记了几种写法」，没登记的路径按原样存。改成 `X:\…` 要在解析时用挂载表换算，而挂载表取不取得到随
  WSL 在不在跑而变 —— 同一路径会时而两种写法。② 本机 Codex 会话的 cwd 是 WSL 路径时 `project_root` 是裸
  `/home/…`（当前投影 53,406 条）：扫描器只给 WSL 来源传发行版（`scan.rs`），本机来源一律 `None`，注释说的
  「由宿主 / CLI 注入默认发行版」没接上；注册表里有两条最早登记的裸 `/home/…` 根接住了它们。只有一个用户
  发行版时注入不算猜，但改的是写入约定。两条都是「同一目录不同写法」，读路径上的项目标识要能归并它们。
- ~~强制重读逐文件起 `wsl.exe`~~ ✅ 10-12 合并（`5ec7e2b`）：QuotaBar 按规则 2 提出，判据双方定为「重建一轮
  `wsl.exe` 次数 ≤ Σ每个发行版 ⌈小文件（≤ 8 MiB）合计 / 32 MiB⌉ + 大文件数」（我最初给的「≤ 发行版数 + 大文件数」
  会逼出一次约 500 MB 的读取，已改正）。`StatPrefetch::reading_whole` 按调用方顺序分批、用到时才读、交出即删。
  实机夹具：逐个 10 次 → 分批 4 次，结果逐项一致。QuotaBar v0.8.15 真机重建（10-12）：上限 ⌈372.7/32⌉ + 38 = 50
  （另每轮固有约 6 次）；第 1 轮 45 次、第 2 轮 18 次，判据成立；整次重建约 10 分钟 → 2 分 31 秒（主要因它改成连跑多轮）。
- ~~Codex 就地压缩的会话~~ ✅ 10-20 合并（`6189b16`），10-21 QuotaBar v0.8.17 装机验收：88 个按逻辑路径认回、首轮无报错，
  有用量的会话按「月 × provider × 位置」数，索引与总库逐行相等。`rollout-*.jsonl.zst`（10-20 QuotaBar 按规则 2
  提出）：10-15 本机 88 个会话被压，发现阶段不认 → QuotaBar 当成消失、prune 掉了它们的索引（总库没受影响）。做法：按逻辑路径
  `X.jsonl` 发现，偏移按解压后的字节算（`compressed` 模块，纯 Rust 解压）。本机核过：88 个逐字节与 libzstd 一致；发现结果只多出
  这 88 个逻辑路径；扫出的事件逐来源等于总库当前投影（88 个来源共 89,812 条，`source_path` 串逐字相同）。
  每轮 stat 本机 88 个压缩件合计 113 ms（不解压，按帧头）。
- ~~源文件已不在的来源按当前注册表重算归属~~ ✅ 10-21 合并（`cbffc71`），同日 QuotaBar v0.8.18 装机验收：它只点名源文件已不在的
  归档来源、空注册表不调；真库首轮排队的来源全部改写、无跳过，再跑不重复写。四种跳过原因真库上都没触发过，只有单测覆盖。
  10-21 QuotaBar 按规则 2 提出 ——
  它把发现确认消失的来源改为保留、从总库投影，而这些来源重扫不了，归属停在入库时那份（`reattribute` 只改写法）。
  做法：新库接口 `TotalStore::recompute_attribution`，调用方点名来源（不探源文件），按事件记下的 `cwd` 与当前注册表
  重算归属三项（同解析器），有改动的来源整代 Reparse、经 change-feed 可见。验收判据由 QuotaBar 给（四条，见提交）。
- ~~WSL 里的压缩件每轮整份读~~ ✅ 10-29 合并（`d92442f`），同日 QuotaBar v0.8.19 装机复测通过：稳态每轮 `wsl.exe` 中位数
  回到压缩前的水平（10-20 合并时记下的代价）：10-28 起 WSL 里的 Codex 也开始压缩，10-29 实测
  248 个、788 MiB，其中 202 个帧头不写解压后的长度，只能整份解压才知道。一轮没有新内容的扫描起 281 次 `wsl.exe`、约 55 s。
  改法与原计划「按发行版分批读」不同（分批读每轮照样搬 788 MiB）：预取时顺带问 `.zst` 的身份（大小、mtime、ctime），
  进程内按身份缓存解压后的长度；没命中的（首轮、Codex 改写过的）按发行版分批整份读。同一进程连跑三轮实测
  （对照为改前的 main）：稳态每轮 281 次、55 s → 2 次、0.4 s；第一轮 284 次、66 s → 56 次、30 s；事件数相同。
  QuotaBar 的复测判据：启动后第 2 轮起连续 12 轮，`sv_wsl_spawns` 中位数回到个位数。
- **[本仓，非首版前置]** TumeChat「项目标识」（10-10 按规则 2 提出）。已有 `roots` / `attribute` 的根归属与 `canonical_id` 基础，待交付的是供检索收窄的完整消费契约与验收，不是从零新增身份系统。
  消费方不按字符串比 `project_root`、不自己换算路径。完成判据：① 同一目录的各种写法（UNC、规范形、有挂载表时的
  盘符与 `/mnt`）拿到相同标识，另一个发行版下的同名路径标识不同；② 两个库里 git 远端相同的项目标识相同，
  没有远端的按机器分开；③ 用户说的项目名或任一写法的路径能对到这个标识上，形状本仓定；④ 不加新参数时
  单库的既有输出不变。

- ~~`stores` 与单库 `pull` 只读元数据查询~~ ✅ 11-10 已合并 `main`（`29beb8d`）：原路径调用完整 `status()`，将无用的入库时间统计带入逐行聚合；改用窄索引计数与主键末端水位，输出契约及旧库只读不迁移保持不变。根因、合成 SQL 实测、回归与生产复测边界见 [store-metadata.md](store-metadata.md)。未操作真库，未替换安装版。

## D. 产品化（决定 9：iroh）

- **[本仓]** 个人同步跑通之后：用 iroh 把点对点同步做进 svault，只替换传输层；新设备拿钥匙
  （同步密码 / 设备配对 / 恢复码）一起做。设计在 [linux-replica.md](linux-replica.md)「以后给别的用户」。

## E. 存储体积

- **[人说了再做]** 见 [storage-growth.md](storage-growth.md)：`history.jsonl` 改增量、旧代审计清理、入库前压缩。
- ~~`svault gc` 会删回退留下的旧代~~ ✅ 10-29 合并（`6413981`）：候选少了「与当前头同一个源版本」，真库 279 份、
  237,541 条唯一副本全在候选里；修后候选 0。修复已在 `v2026.10.29.1` 发布，见 B 节；旧版 v2026.10.10.2 仍带旧条件，换到已含修复的版本前别拿它跑 `gc --apply`。
- **[人]** 回收被当前代完整覆盖的回退旧代（storage-growth 第 5 项，人 10-29 定三步）：第 1 步「量」✅（`6413981`，
  `TotalStore::rollback_coverage`；按内容完整覆盖 521 份、667.8 MiB）；第 2 步「手动回收」✅ 代码与发布（`ab72363` / `v2026.11.09.1`）：
  `svault reclaim-rollbacks`（默认预览，`--apply` 才删），真库只读预览与第 1 步一致。真库人 10-29 定**先不删**；
  要删时先备份、先跟 QuotaBar 说好（它退出、重启都要人同意，并先备份凭据与索引），删完 `VACUUM`。第 3 步以后再说。

## F. 测试与工具

- ~~`open_existing_returns_the_store_when_it_is_there` 曾在无桌面 Linux 上失败于准备步骤。~~ ✅
  2026-11-06 在 [store.rs](../src/store.rs) 改为显式测试文件钥匙（`open_with_source` / `open_existing_from`），不改生产钥匙来源默认值；WSL Linux 实跑 `--features store` 与 `--features store,dsh-experimental` 全绿（`495bd46`，已随 `v2026.11.09.1` 发布）。
- **[本仓，小，非交付前置]** 清理字符串字面量里误折叠留下的长空格：先查候选、逐条确认，只改确属意外的日志 / 断言 / SQL 文本，不把正常缩进算作缺陷。候选数量不固化；`grep -rnE '"[^"]*[^ ] {8,}[^ ][^"]*"' src/` 可列出。
- 本仓没有 CI：Linux 专属分支（非 Windows 的桩、Unix 文件权限）只能在 WSL 里用 Linux 工具链手动跑。

## G. 会话有效视图与跨工具续接（人说了再做；边界与外部对标见 [INGEST_KERNEL.md](INGEST_KERNEL.md) §2.1）

1. ~~**识别上下文压缩边界（有效上下文 `VIEW` 切点）**~~ ✅ 11-10 完成：
   - **本机与 WSL 源文件只读核查（2026-11-10）**：
     - Claude Code：本机 335 个 `.jsonl` 中含 182 条 `type = "system", subtype = "compact_boundary"` 与 182 条配对的 `type = "user", isCompactSummary = true, isVisibleInTranscriptOnly = true`（`message.role = "user"`）；WSL 152 个 `.jsonl` 中含 126 对 `compact_boundary` 与 `isCompactSummary = true`（`type = "summary"` 均为 0 条）。此前 `parse_claude` 将 `isCompactSummary = true` 当作普通 `Actor::User` 消息且 `input_origin.basis = "log_role_only"`。
     - Codex：本机 105 个文件（7 个 `.jsonl` + 98 个 `.jsonl.zst`）含 221 条 `type = "compacted"`（均带 `payload.replacement_history`）；WSL 263 个文件（9 个 `.jsonl` + 254 个 `.jsonl.zst`）含 310 条 `type = "compacted"`（237 条仅 `replacement_history`、68 条同时带非空 `payload.message` 与 `replacement_history`、5 条仅 `payload.message`）。Codex 不把压缩摘要另写成 `response_item (role = "user")`。
   - **解析层区分与 `VIEW` 切点实现**：在 [`src/session_binding.rs`](../src/session_binding.rs) 的 `classify_user_input_origin` 最前端（优先于文本同侪信封嗅探，防止摘要复述的历史 `<cross-session-message>` / `[xs ...]` 冲掉压缩标记）识别 `claude_compact_summary`（`isCompactSummary: true`）、`claude_compact_boundary`、`codex_compacted` 及 DSH `dsh_compaction_checkpoint` / `dsh_surface_replace`，统一归为 `SenderKind::SubagentOrRuntime` + `InputTrustLevel::PeerUntrusted`（即使会话具人类控制面绑定也绝不升格为 `HumanVerified`）；并在 [`src/rawevent.rs`](../src/rawevent.rs) 提供 `InputOrigin::is_compaction_summary`、`RawEvent::is_compaction_summary`、`effective_view_start_index` 与 `effective_view_slice`。
2. ~~**核查 Codex 分页续写段（`session_meta.payload.history_base` / `history_mode: "paginated"`）**~~ ✅ 11-10 只读核查完成：
   - 本机 105 个 Codex 文件中 104 次 `session_meta` 带 `history_mode: "paginated", history_base: None`，存在 1 对同线程分段文件（`<thread>.jsonl.zst` 21 行与 `<thread>_<segment>.jsonl.zst` 61 行，`session_meta.payload.id` 相同）；
   - WSL 263 个 Codex 文件中 266 次 `session_meta` 带 `history_mode: "paginated"`，其中 **2 次带非空 `history_base`**：① 跨线程分叉（`history_base: { thread_id: "<parent_thread>", end_ordinal_exclusive: 513, end_byte_offset: 2229863 }`）；② 同线程大文件自动续写段（首个文件达 22,093 行 / 207,668,567 字节后切出 `<thread>_<segment>.jsonl`，`session_meta.payload.id` 保持原 `<thread>` 且带 `history_base: { thread_id: "<thread>", end_ordinal_exclusive: 22093, end_byte_offset: 207668567 }`）。另观察到 3 个 `thread_source: "subagent"` 文件在第 0 行写子代理 `session_meta`、第 1 行写父会话 `session_meta`。
   - **结论**：`paginated` 续写段与分叉文件只追加切点后的新记录、不重放前段历史，单文件 `parse_codex` 不会重复计费或重复摄取；但因总库会话桶以 `(source_type, source_location, source_path, source_session_id)` 为键，同线程 `<thread>` 与 `<thread>_<segment>` 会作为 `session_id` 相同、`source_path` 不同的两段独立来源入库。是否需要在读取层按 `session_id` + `history_base` 跨段串联，留待消费方按规则 2 提出。
3. ~~**`session-origin` 读 `.jsonl.zst` 流式早停**~~ ✅ 11-10 完成：在 [`src/compressed.rs`](../src/compressed.rs) 新增 `decode_lines`、`scan_lines_local` 与 `scan_lines_wsl`，并将 [`src/session_origin.rs`](../src/session_origin.rs) 的 `read_codex_origin` 改为流式逐行读取/解压，命中目标 `session_id` 的 `session_meta` 即刻停止，不再整份解压后续块或后续帧。
4. **[等消费方按规则 2 提]** **跨工具续接（Claude Code ⇄ Codex Handoff / Resume）的重放血缘跳过**：本仓保持「只读不写回」边界（交接单拼装或原生会话文件转写归上层编排方，不进本仓，也不接入 `agit` 运行时）。若上层经实测对比「软交接单 vs 原生格式转写」后选定往 `~/.claude` / `~/.codex` 写入转写文件走原生 `--resume`，本仓配合识别重放前缀的血缘标记、扫描时跳过重放段，避免重复计 token 用量与重复入记忆。与 H.5 共用血缘契约，不重复实现。

## H. 宿主与子 CLI 来源适配（职责与判据见 [INGEST_KERNEL.md](INGEST_KERNEL.md) §2.2）

2026-11-04 按用户要求登记，并与消费方确认职责方向。11-05 用户授权开工，H.4 首轮只读证据与 H.1 草案见 [dsh-ingest-survey.md](dsh-ingest-survey.md)；11-09 完成 H.1 / H.2 / H.3 并随 `v2026.11.09.1`（`a151997` / `ccfbdca`）正式发布。宿主摄取独立于 Worker 选型，替代此前「整批适配等执行后端选定」的安排；子 CLI 接缝随所选后端验证。正式实现遵守 `AGENTS.md` §4 规则 2 的脱敏实测与可执行完成判据。

1. ~~**明确临时属性、真实来源与血缘的输入契约**~~ ✅ 11-09 合并并随 `v2026.11.09.1` 发布（`a151997` / `ccfbdca`）：在 [`src/session_binding.rs`](../src/session_binding.rs) 落地 `SessionBindingRegistry`、`SessionBindingRecord`、`BindingState` 与三态 `IngestAdmissionVerdict`（`PersistentAllowed` / `TemporaryExcluded` / `UnknownHold`）；明确区分长期写入/摄取策略（`temporary`）与召回读取策略（`read_memory`），二者正交（`temporary: false, read_memory: false` 仍正常摄取）；绑定缺失必填字段、处于 `pending` 未决、存在冲突或清单损坏时显式进入 `UnknownHold`（fail-closed），既有未绑定普通历史会话默认以 `verified_human_input: false` 兼容摄取。
2. ~~**统一临时会话摄取排除**~~ ✅ 11-09 合并并随 `v2026.11.09.1` 发布（`a151997` / `ccfbdca`）：在扫描入口（`scan_append_log_with_bindings`）与总库写入口（`TotalStore::apply_projection_if`）双层强制执行同一准入判据，在任何 `Message` / `Thinking` / `ToolUse` / `ToolResult` / `Usage` 入库前阻断临时会话，并在 `SourceReport`（`admission_status` / `events_excluded_temporary` / `admission_detail`）与 `ScanReport`（`total_excluded_temporary`）可观察地报告排除与未知搁置状态；覆盖全量、增量（保持 `safe_offset = 0` 防首行元信息丢失）、重扫、子会话血缘继承（`<parent>/subagents/agent-*.jsonl` 与 `parentSession`）以及通过 `store_meta['session_bindings_json']` 共享策略的多写者场景。
3. ~~**保留真实发送方与 peer 来源**~~ ✅ 11-09 合并并随 `v2026.11.09.1` 发布（`a151997` / `ccfbdca`）：在 [`src/rawevent.rs`](../src/rawevent.rs) 新增 `RawEvent::input_origin: Option<InputOrigin>`（含 `SenderKind`、`InputTrustLevel` 及 `sender_session` / `sender_address` / `xs_message_id` / `assignment_id` / `handoff_id`）；自动识别跨会话同侪信封（`<cross-session-message ...>`、`[xs id=...]`）与残缺信封，确保同侪消息即使在源日志中为 `role=user` 或会话具有人类绑定，也不会被标成 `HumanVerified`。
4. ~~**DSH 宿主正式摄取与复用来源**~~ ✅ 11-10 已合并 `main`（正式接线 `29beb8d`，前序提交见下文）：核查实际根、压缩后缀与 schema、稳定会话 / 事件身份、项目归属、增量、损坏恢复，以及旧版 / 新版文件并存时的唯一逻辑来源。上层复用 DSH 日志时只摄取一次；若另有独立宿主记录，再按来源契约评估，不预设第二份重复 provider。既有 Claude Code / Codex 解析可复用，但宿主启动子 CLI 后的实际落盘与配置根接线需另验。进展：11-06 至 11-09 已在 `dsh-experimental` 门控下完成物理核验、原生 admission、会话目录选代、同句柄捕获、旧前缀与生命周期对照、保守提交门（`faed841`），以及 `RawEvent` 归一化位置映射与实验性 `TotalStore` + `DshSavedState` 提交/崩溃重放幂等验证（`9d64cce`，见 [dsh-read-state-design.md](dsh-read-state-design.md) §6–§11）；**人 11-09 定**：首版正式 provider 仅支持 DSH 会话日志格式第 4 代（`session.v4.jsonl[.zstd]`），仅存 `v0..v3` 或未来 `v5+` 显式报 `UnsupportedGeneration` 且不回退旧代；**11-10 已合并 `main`（`dc682ff`）**：完成表面折叠（`surfaceOp: "replace"`）、压缩记录（`compaction/*`）、子代理（`subagent/descriptor` / `subagent/catalog` / 无 seed 子代理）、多层 seed fork、DSH v4 `source.kind` 到 `InputOrigin` 分类，以及会话根目录批量发现与总库扫描提交（`discover_dsh_sessions_in_root` / `scan_dsh_sessions_root_to_store`，见 [dsh-read-state-design.md](dsh-read-state-design.md) §12）；**11-10 已合并 `main` 的细粒度用量保真（`498b7a3`，后随 `v2026.11.11.1` 发布）**：落地 [dsh-ingest-survey.md](dsh-ingest-survey.md) §11 五项事实（`UsageEvidence`：原生计量样本与流式 chunk 覆盖历史、逐字段 `Option<u64>`、精确 total 来源 `Reported` / `DerivedFromCompleteBuckets`、全部 13 个向量的可用状态与诊断、计量归属）及 `TotalStore` 持久化汇总（`RawEvent::usage_evidence` + `summarize_session_usage_evidence`，见 [dsh-read-state-design.md](dsh-read-state-design.md) §13）；**11-10 正式接线已合并 `main`（`29beb8d`）**：`catalog::builtin_descriptors()` 增加独立 DSH 描述符，沿用 `Jsonl` / `AppendLog`，仅显式 `SVAULT_DSH_SESSIONS_DIR` 或 `SVAULT_DSH_HOME` 时发现；扫描要求 `store` 与显式可信运行时。`Cursor.dsh_state` 保存生命周期与旧前缀，归一化摘要变化会重投影，坏尾 / 未决准入 / 缺运行时 / 未知最高代均冻结。详见 [读取状态设计 §14](dsh-read-state-design.md#14-常规来源接线与持久化凭据)，已随 `v2026.11.11.1` 发布；本机写者升级已验，宿主真实接入仍单独验收。
5. **[本仓 + 等消费方]** **父子会话与重放血缘**：上层提供任务 / 执行到宿主及子 CLI 的真实绑定，本仓保留可核关系与未知状态，区分父事件、父子会话和重放前缀。`parent_ref` 现为空且语义为父事件，不直接挪用为会话关系；重放防重承接 G.4。
6. **[本仓 + 等消费方]** **来源主机就地证据与删除状态**：复用本地只读命令、稳定引用、墓碑、防复活及现有删除消费出口；按实测补齐区分不可读、找不到、版本变化与已擦除的必要能力。远程认证、请求路由、来源主机离线等待与回执归上层；原生源日志及下游索引 / 缓存 / 派生记忆的清理由各自所有者承接。不把 `--all-stores` 当远程回源，也不借 `session-origin` 绕过墓碑；不默认搬运原始总库。
7. **[本仓]** **兼容验证与发布通知**：任何新 provider、来源字段、值域或 schema 变化均须核旧版读者与多个写者的兼容性；分别验证 CLI、Rust 构造 / 穷举、逐字段读取、`discover` 按 `SourceMode` 过滤四类消费者。合并后主动通知各消费方，源码完成、已发布与真实接入验收分开记录；各方自行升级其子模块或运行时。进展：11-10 用量保真已合入 `main`（`498b7a3`）；正式接线与本仓有限兼容验收已合并 `main`（`29beb8d`），不改 CLI 参数、`EventType` / `SourceMode` 值域或 SQLite 表列。隔离实测旧版 `pull` 结构可读但不输出新证据，旧版普通 JSONL 追加不改 DSH 投影；旧版 `reattribute --apply` 整代重写会丢失全部新证据。消费方另核旧自动 `recompute_attribution` 路径有同类源码风险，未实跑证明丢失。旧 CLI 共用游标文件会抹掉 `dsh_state`，已补无凭据而库有前代时的保守重建，双平台完整矩阵已核恢复与旧代保留。此前桌面消费方构造 `Cursor` 缺 `dsh_state` 的编译阻塞已通过版本化状态持久化适配消除，完整状态与旧累计字段往返、证据字段保留及自动归属重算已在其仓验收；本轮 review 边界补修已通过最终验收并合入消费方主线，桌面写者已安装且正常后台刷新通过。TumeChat 的历史协议投影与记忆引擎的来源投影均已保留 `usage_evidence` 与 `input_origin`，不再把这两处字段丢弃列为当前缺口；记忆引擎 S3 读侧与新版内嵌 `svault` 写者均已完成本地装机，真实健康响应已核目标提交和 dense 可用状态，定时入口无旧二进制覆盖。详见 [读取状态设计 §15](dsh-read-state-design.md#15-h7-兼容验收与升级边界)。本仓 review、发布与本机登记写者升级均已完成；实际来源接入仍未完成。下一步由宿主按显式根和可信绑定做受控端到端验收，消费方自行维护源码远端交付、产品接入与发布状态。
