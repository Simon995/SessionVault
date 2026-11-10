# 待做清单

> 最后整理：2026-11-09。这里维护当前任务、依赖和执行顺序；带日期、划线及提交号的条目是历史交付记录，不算当前队列。
> 做完一项就划掉并写上提交号；顺序以本页为准，A–H 是稳定分类，不表示执行先后。
> 标记：**[人]** 要人做或人拍板；**[本仓]** 本仓动手；**[等]** 等外部条件。文档登记、源码完成、正式发布和消费方实际接入分别核验。

## 当前状态与下一步

截至 2026-11-09，最新正式发布与本机安装已更新为 `v2026.11.09.1`（`ccfbdca`；核对 GitHub Release、标签及 `svault --version`），包含 `reclaim-rollbacks`（`ab72363`）、CLI 全读旧来源基线保留修复（`368b730`）以及 **H.1 / H.2 / H.3** 的控制面绑定输入契约（`SessionBindingRegistry`、三态 `IngestAdmissionVerdict`）、扫描与 `TotalStore::apply_projection_if` 双层统一临时会话排除（含 `<parent>/subagents/agent-*.jsonl` 与 `parentSession` 血缘继承、未知/未决/冲突 fail-closed `UnknownHold`）和真实发送方与同侪来源保真（`RawEvent::input_origin`、`SenderKind`、`InputTrustLevel`，`a151997`）。H.4 已在 `dsh-experimental` 门控下完成结构探针、静态配置根、合法消息 / 重试 / seed 样本、离线与原生 backend 检查、双进程受控窗口、独立快照审计、Node 同缓冲区捕获（`86c34aa`）、Rust 字节 / 原生记录位置核验（`9caf18b`）、Rust 原生语义 admission 与无桌面 Linux `store` 测试钥匙修复（`495bd46`）、Rust 会话目录选代与保守提交门（`faed841`），以及 `RawEvent` 归一化位置映射与实验性 `TotalStore` + `DshSavedState` 提交/崩溃重放幂等验证（`9d64cce`），证据见 [DSH 核查记录](dsh-ingest-survey.md) §8–§19 与 [读取状态设计](dsh-read-state-design.md) §6–§11。本轮只写项目内合成日志与测试临时库，未操作真库。

| 顺序 / 条件                    | 对应任务                            | 下一步具体产物与完成判据                                                                                                                                                                                                                                                                                                                                                                                                                                                                                           |
| ------------------------------ | ----------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| ~~H.1 / H.2 / H.3 源码与发版~~ | ✅ 已发布 `v2026.11.09.1`（`a151997` / `ccfbdca`） | 已在 [`src/session_binding.rs`](../src/session_binding.rs)、[`src/rawevent.rs`](../src/rawevent.rs)、[`src/scan.rs`](../src/scan.rs) 与 [`src/store.rs`](../src/store.rs) 落地并随 `v2026.11.09.1` 正式发布；TumeChat 可直接用已发布 CLI，QuotaBar / TumeFlow 可按需升级子模块指针                                                                                                                                                                                                                                |
| 下一批优先，可独立推进         | H.4：复杂语义与正式 provider 注册   | 已完成归一化位置映射与实验性 `TotalStore` + `DshSavedState` 提交接线（`9d64cce`）；**人 11-09 定**：首版正式 provider 仅支持 DSH 会话日志格式第 4 代（`session.v4.jsonl[.zstd]`，header `version: 4`；注：指日志文件 schema 版本，与对话所用大模型无关），仅存 `v0..v3` 或未来 `v5+` 显式报 `UnsupportedGeneration` 且不回退旧代，不在 Rust 内重复维护 0→4 历史迁移链。接下来补 fork / descriptor、compaction / 有效视图样本，并将 DSH 接入正式 `SourceType` / `discover` / `scan` |
| 可并行设计，改公开类型前核兼容 | H.4 / H.7：用量保真实现             | 按核查记录 §11 保留原生报告、逐字段未知、精确 total、诊断及 attempt / 继承归属；metadata / full 与加密存储往返不丢事实。旧四桶不填零冒充已知，核旧 reader / 多 writer 和四类消费者                                                                                                                                                                                        |
| 相关功能接入时                 | H.5 / H.6、G.4：血缘与跨主机证据    | 父子会话控制面与日志血缘已在 H.1/H.2（`a151997`）接入临时排除；后续按任务追溯、原生转写或远程回源的实际需求补齐重放防重和跨机证据 / 删除状态；本地只读与墓碑出口已可复用，远程认证与编排归上层                                                                                                                                                                            |
| 外部条件或专项开工后           | A / D、E、F、G.1–G.3、C 的其余项    | 副本传输等组网条件；有效视图 / 分页核查、统一项目检索标识、长空格字面量排查及性能优化按各节推进。回收命令须单独发布，真库执行仍按人定“先不删”                                                                                                                                                                                                                             |

DSH 格式核查与实验接缝（`dsh-experimental`）已打通到 `TotalStore` + `DshSavedState` 提交与崩溃重放验证（`9d64cce`），控制面绑定、临时会话排除与真实来源保真也已落地（`a151997`）；默认 `discover` / `scan-all` 的正式 DSH provider 注册仍待复杂语义（fork / compaction）与首版支持代拍板后开启。一般高并发压力、租约争夺和完整 WSL DSH 捕获仍未验，不能把指定受控窗口通过视为一般并发验收。

## 哪些影响消费方使用

| 项目                         | 当前状态与影响边界                                                                                                                                                |
| ---------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| H.1 / H.2 临时契约与排除     | 已随 `v2026.11.09.1`（`a151997` / `ccfbdca`）正式发布，覆盖 CLI 与 `TotalStore::apply_projection_if` 多写者及子会话血缘；QuotaBar / TumeFlow 需各自 bump 子模块指针 |
| H.1 / H.3 真实来源与 peer    | 已随 `v2026.11.09.1`（`a151997` / `ccfbdca`）正式发布，`RawEvent::input_origin` 区分 `HumanVerified` / `PeerSession` / `CoordinatorDispatch` / `Unknown`          |
| H.4 正式宿主来源             | `dsh-experimental` 归一化与写库接线已完成（`9d64cce`）；默认 `SourceType` / `discover` 尚未启用，阻断 DSH 宿主会话自动归档，不阻断 Claude / Codex 既有历史读取    |
| H.5 / G.4 血缘与重放         | 父子会话血缘已接入临时会话继承（`a151997`）；跨工具原生转写重放防重与完整任务树追溯待消费方接入时补齐，普通历史读取不以之为前置                                   |
| H.6 来源主机证据 / 删除状态  | 跨设备回源、离线状态及下游删除闭环需上层接合；本仓已有本地只读与墓碑出口，不等于删除能力整体未实现                                                                |
| G.1 / G.2 有效视图与分页核查 | 前者影响自动压缩摘要与用户输入区分，后者是待核格式风险；本轮未测，不能直接断言已有数据丢失                                                                        |

已发布的 `sessions-recent` / `sessions-read`、`stores`、`pull` / `changes`（逐库）、`roots` / `attribute` 和 `erasures` 可供消费方先做合成接入准备；本轮已向消费方核对版本、源码与边界，没有新增读取接口需求。`current` 是当前解析投影，不是 G.1 的有效上下文窗口；`pull`、`changes` 和会话续读各有自己的游标，`erasures` 没有增量游标；`store_id` 是库标识，不是可信主机身份。`session-origin` 仅查 Codex 记录的仓库远端。

原文索引 / 检索、授权、记忆保存同步及产品界面的实现分别属于消费方与记忆引擎，不移成本仓任务。整库副本同步（A / D）、未来统一检索标识（C）、压缩件早停（G.3）与体积回收（E）均不是本地历史读取的统一前置。

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
- **[本仓]** TumeChat「项目标识」（10-10 按规则 2 提出；不急，不挡它的第一版）。用途：检索按项目收窄，
  它不按字符串比 `project_root`、不自己换算路径。完成判据：① 同一目录的各种写法（UNC、规范形、有挂载表时的
  盘符与 `/mnt`）拿到相同标识，另一个发行版下的同名路径标识不同；② 两个库里 git 远端相同的项目标识相同，
  没有远端的按机器分开；③ 用户说的项目名或任一写法的路径能对到这个标识上，形状本仓定；④ 不加新参数时
  单库的既有输出不变。

## D. 产品化（决定 9：iroh）

- **[本仓]** 个人同步跑通之后：用 iroh 把点对点同步做进 svault，只替换传输层；新设备拿钥匙
  （同步密码 / 设备配对 / 恢复码）一起做。设计在 [linux-replica.md](linux-replica.md)「以后给别的用户」。

## E. 存储体积

- **[人说了再做]** 见 [storage-growth.md](storage-growth.md)：`history.jsonl` 改增量、旧代审计清理、入库前压缩。
- ~~`svault gc` 会删回退留下的旧代~~ ✅ 10-29 合并（`6413981`）：候选少了「与当前头同一个源版本」，真库 279 份、
  237,541 条唯一副本全在候选里；修后候选 0。修复已在 `v2026.10.29.1` 发布，见 B 节；旧版 v2026.10.10.2 仍带旧条件，换到已含修复的版本前别拿它跑 `gc --apply`。
- **[人]** 回收被当前代完整覆盖的回退旧代（storage-growth 第 5 项，人 10-29 定三步）：第 1 步「量」✅（`6413981`，
  `TotalStore::rollback_coverage`；按内容完整覆盖 521 份、667.8 MiB）；第 2 步「手动回收」✅ 代码（`ab72363`）：
  `svault reclaim-rollbacks`（源码已有、尚未正式发布；默认预览，`--apply` 才删），真库只读预览与第 1 步一致。真库人 10-29 定**先不删**；
  要删时先备份、先跟 QuotaBar 说好（它退出、重启都要人同意，并先备份凭据与索引），删完 `VACUUM`。第 3 步以后再说。

## F. 测试与工具

- ~~`open_existing_returns_the_store_when_it_is_there` 曾在无桌面 Linux 上失败于准备步骤。~~
  2026-11-06 在 [store.rs](../src/store.rs) 改为显式测试文件钥匙（`open_with_source` / `open_existing_from`），不改生产钥匙来源默认值；WSL Linux 实跑 `--features store`（415 passed, 0 failed）与 `--features store,dsh-experimental`（421 passed, 0 failed）全绿（已提交 `495bd46`，尚未合并发布）。
- **[本仓，小，非交付前置]** 清理字符串字面量里误折叠留下的长空格：先查候选、逐条确认，只改确属意外的日志 / 断言 / SQL 文本，不把正常缩进算作缺陷。候选数量不固化；`grep -rnE '"[^"]*[^ ] {8,}[^ ][^"]*"' src/` 可列出。
- 本仓没有 CI：Linux 专属分支（非 Windows 的桩、Unix 文件权限）只能在 WSL 里用 Linux 工具链手动跑。

## G. 会话有效视图与跨工具续接（人说了再做；边界与外部对标见 [INGEST_KERNEL.md](INGEST_KERNEL.md) §2.1）

1. **[本仓]** **识别上下文压缩边界（有效上下文 `VIEW` 切点）**：先只读核查真库及本机 / WSL 源文件中 Claude Code 合成压缩摘要（`isCompactSummary: true`）与 Codex 压缩记录（`type = "compacted"` / `replacement_history`）的实际形态与数量；随后在解析层把「自动压缩生成的二手摘要 / 压缩边界」与「用户真实输入」区分开。用途：① 供下游切出「最近一次压缩后的当前有效窗口」；② 防止记忆层把模型生成的二手摘要当用户原话重复提炼；③ 防止会话标题预览被压缩长文占据。
2. **[本仓]** **核查 Codex 分页续写段（`session_meta.payload.history_base` / `history_mode: "paginated"`）**：只读核查本机与 WSL 的 Codex 是否已产出 `<thread>_<segment>` 分段 rollout 文件，确认单文件扫描在多段会话下是否会把同一条长会话拆成两截。
3. **[本仓，小]** **`session-origin` 读 `.jsonl.zst` 流式早停**：`read_codex_origin` 目前对压缩件整份解压进内存再扫 `session_meta`；改为流式逐行解压、命中目标 `session_id` 的 `session_meta` 即停（单会话大压缩件只解开头几 KB）。
4. **[等消费方按规则 2 提]** **跨工具续接（Claude Code ⇄ Codex Handoff / Resume）的重放血缘跳过**：本仓保持「只读不写回」边界（交接单拼装或原生会话文件转写归上层编排方，不进本仓，也不接入 `agit` 运行时）。若上层经实测对比「软交接单 vs 原生格式转写」后选定往 `~/.claude` / `~/.codex` 写入转写文件走原生 `--resume`，本仓配合识别重放前缀的血缘标记、扫描时跳过重放段，避免重复计 token 用量与重复入记忆。与 H.5 共用血缘契约，不重复实现。

## H. 宿主与子 CLI 来源适配（职责与判据见 [INGEST_KERNEL.md](INGEST_KERNEL.md) §2.2）

2026-11-04 按用户要求登记，并与消费方确认职责方向。11-05 用户授权开工，H.4 首轮只读证据与 H.1 草案见 [dsh-ingest-survey.md](dsh-ingest-survey.md)；11-09 完成 H.1 / H.2 / H.3 并随 `v2026.11.09.1`（`a151997` / `ccfbdca`）正式发布。宿主摄取独立于 Worker 选型，替代此前「整批适配等执行后端选定」的安排；子 CLI 接缝随所选后端验证。正式实现遵守 `AGENTS.md` §4 规则 2 的脱敏实测与可执行完成判据。

1. ~~**明确临时属性、真实来源与血缘的输入契约**~~ ✅ 11-09 合并并随 `v2026.11.09.1` 发布（`a151997` / `ccfbdca`）：在 [`src/session_binding.rs`](../src/session_binding.rs) 落地 `SessionBindingRegistry`、`SessionBindingRecord`、`BindingState` 与三态 `IngestAdmissionVerdict`（`PersistentAllowed` / `TemporaryExcluded` / `UnknownHold`）；明确区分长期写入/摄取策略（`temporary`）与召回读取策略（`read_memory`），二者正交（`temporary: false, read_memory: false` 仍正常摄取）；绑定缺失必填字段、处于 `pending` 未决、存在冲突或清单损坏时显式进入 `UnknownHold`（fail-closed），既有未绑定普通历史会话默认以 `verified_human_input: false` 兼容摄取。
2. ~~**统一临时会话摄取排除**~~ ✅ 11-09 合并并随 `v2026.11.09.1` 发布（`a151997` / `ccfbdca`）：在扫描入口（`scan_append_log_with_bindings`）与总库写入口（`TotalStore::apply_projection_if`）双层强制执行同一准入判据，在任何 `Message` / `Thinking` / `ToolUse` / `ToolResult` / `Usage` 入库前阻断临时会话，并在 `SourceReport`（`admission_status` / `events_excluded_temporary` / `admission_detail`）与 `ScanReport`（`total_excluded_temporary`）可观察地报告排除与未知搁置状态；覆盖全量、增量（保持 `safe_offset = 0` 防首行元信息丢失）、重扫、子会话血缘继承（`<parent>/subagents/agent-*.jsonl` 与 `parentSession`）以及通过 `store_meta['session_bindings_json']` 共享策略的多写者场景。
3. ~~**保留真实发送方与 peer 来源**~~ ✅ 11-09 合并并随 `v2026.11.09.1` 发布（`a151997` / `ccfbdca`）：在 [`src/rawevent.rs`](../src/rawevent.rs) 新增 `RawEvent::input_origin: Option<InputOrigin>`（含 `SenderKind`、`InputTrustLevel` 及 `sender_session` / `sender_address` / `xs_message_id` / `assignment_id` / `handoff_id`）；自动识别跨会话同侪信封（`<cross-session-message ...>`、`[xs id=...]`）与残缺信封，确保同侪消息即使在源日志中为 `role=user` 或会话具有人类绑定，也不会被标成 `HumanVerified`。
4. **[本仓]** **DSH 宿主正式摄取与复用来源**：核查实际根、压缩后缀与 schema、稳定会话 / 事件身份、项目归属、增量、损坏恢复，以及旧版 / 新版文件并存时的唯一逻辑来源。上层复用 DSH 日志时只摄取一次；若另有独立宿主记录，再按来源契约评估，不预设第二份重复 provider。既有 Claude Code / Codex 解析可复用，但宿主启动子 CLI 后的实际落盘与配置根接线需另验。进展：11-06 至 11-09 已在 `dsh-experimental` 门控下完成物理核验、原生 admission、会话目录选代、同句柄捕获、旧前缀与生命周期对照、保守提交门（`faed841`），以及 `RawEvent` 归一化位置映射与实验性 `TotalStore` + `DshSavedState` 提交/崩溃重放幂等验证（`9d64cce`，见 [dsh-read-state-design.md](dsh-read-state-design.md) §6–§11）；**人 11-09 定**：首版正式 provider 仅支持 DSH 会话日志格式第 4 代（`session.v4.jsonl[.zstd]`），仅存 `v0..v3` 或未来 `v5+` 显式报 `UnsupportedGeneration` 且不回退旧代；接下来推进复杂语义（fork / compaction）与正式 provider 注册。
5. **[本仓 + 等消费方]** **父子会话与重放血缘**：上层提供任务 / 执行到宿主及子 CLI 的真实绑定，本仓保留可核关系与未知状态，区分父事件、父子会话和重放前缀。`parent_ref` 现为空且语义为父事件，不直接挪用为会话关系；重放防重承接 G.4。
6. **[本仓 + 等消费方]** **来源主机就地证据与删除状态**：复用本地只读命令、稳定引用、墓碑、防复活及现有删除消费出口；按实测补齐区分不可读、找不到、版本变化与已擦除的必要能力。远程认证、请求路由、来源主机离线等待与回执归上层；原生源日志及下游索引 / 缓存 / 派生记忆的清理由各自所有者承接。不把 `--all-stores` 当远程回源，也不借 `session-origin` 绕过墓碑；不默认搬运原始总库。
7. **[本仓]** **兼容验证与发布通知**：任何新 provider、来源字段、值域或 schema 变化均须核旧版读者与多个写者的兼容性；分别验证 CLI、Rust 构造 / 穷举、逐字段读取、`discover` 按 `SourceMode` 过滤四类消费者。合并后主动通知各消费方，源码完成、已发布与真实接入验收分开记录；各方自行升级其子模块或运行时。
