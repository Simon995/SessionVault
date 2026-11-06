# DSH 来源核查与适配判据

> 2026-11-06 更新。第一轮安装源码与本机日志结构已交叉核对；静态 profile、原生样本、恢复边界、用量保真、原生 backend、受控并发、快照审计、Node 捕获及 Rust 字节 / 原生位置核验见 §8–§16。**正式 provider、生产推进、临时排除与可信来源接口仍待交付**。任务状态与先后只在 [backlog.md](backlog.md) H 节维护，职责见 [INGEST_KERNEL.md](INGEST_KERNEL.md) §2.2。

## 1. 证据范围

- **安装源码**：本机 `@deepseek-ai/dsh` 及下列相关包，已核版本均为 `0.2.1-rc.5`。这是安装包快照，不声称 npm 最新版或上游 `main` 当前仍相同。包元数据指向 [deepseek-ai/deepseek-harness](https://github.com/deepseek-ai/deepseek-harness)，查证日期为 2026-11-05。
- **实读范围**：显式指定 `<DSH_HOME>/sessions` 下的本地日志，使用 [inspect-dsh.py](../scripts/inspect-dsh.py)。原生 restore、codec 与 reducer 用于完全合成的内存夹具（§9–§10）；原生 backend、独立审计与实际捕获只对本仓 `target` 新建目录内的合成文件做读写 / 修尾（§12–§15）。未对真实日志调用恢复 API，未启动 Host / LLM、读取凭据、写真实会话日志或总库。
- **未覆盖**：WSL / 其他机器来源、当前活动 Host 的启动环境及额外 `--patch`、真实会话恢复、复杂迁移、无约束并发压力和崩溃持久性。受控双进程窗口见 §13；本机两个 profile 的静态配置见 §8，这仍不证明覆盖全部活动来源。
- **初始探针限制**：`inspect-dsh.py` 只统计物理 JSONL、白名单字段形状并输出脱敏结构投影，不解码完整 DSH 事件语义、attempt stream 或有效 surface。它的 `complete` 仅表示流读取未中断，`valid_jsonl` 不代表通过 DSH schema 校验，也没有独立验证压缩帧完整性。§14–§15 后加独立审计与原生 admission，不回填成初始真实日志探针已验证。

## 2. 本机观察

以下数字来自 2026-11-05 本轮探针，包含两代日志，**不是去重后的会话事件数或 RawEvent 数**：

| 项                                        | 实测                   |
| ----------------------------------------- | ---------------------- |
| 会话目录                                  | 6                      |
| 物理日志                                  | 10                     |
| `session.jsonl.zstd`，header version 0    | 4                      |
| `session.v4.jsonl.zstd`，header version 4 | 6                      |
| 同目录两代并存                            | 4                      |
| 按最高规范代选择                          | 6 个目录均选 v4 / zstd |
| 解压后读到的字节                          | 8,975,978              |
| 物理 JSON 对象 / 行                       | 6,572 / 6,572          |
| 预算触顶、遍历错误、读取期间文件变化      | 本轮均未观察到         |

v4 每份日志只有 header 不带 `seq/time`，其余顶层对象均有这两个数字字段。v0 中存在 `seq0/time0` 的 packed rows，不能把两代混在一起数，再推断 v4 也支持顶层 packed rows。

### 脱敏结构片段

下面来自本轮一份 v4 日志的 header 和首个 `user/message`，由探针按字段白名单投影后摘录；身份、路径、正文与非白名单来源值已替换，数字保留。**这是不连续的结构片段，不是可回放的完整会话，也不是原生格式验证 fixture**；两个 `redacted` 不能当作真实 id 相同的证据。

```json
{"type":"session","version":4,"id":"redacted","createdAt":1788845978087,"cwd":"/work/repo","isSeeded":false,"delegationDepth":0,"agentPreset":"redacted"}
{"type":"user/message","seq":16,"time":1788846494690,"data":{"content":[{"type":"text","text":"redacted"}],"source":{"kind":"redacted"},"role":"user","id":"redacted"},"surfaceOp":"append"}
```

结构样本只证明存在这些字段，不能证明 `role=user` 来自人、`source.kind` 可信、输入有执行授权或会话不是临时会话。尚未核对并存两代的完整消息等价性，也未按真实 id 做跨文件合并。

## 3. 来源与代选择

以下为安装版源码结论；相对文件与行号见 §7。

1. `resolveDshHome` 按显式参数、非空白 `DSH_HOME`、`~/.dsh` 取根；base composition 默认把 backend root 设为 home 下的 `sessions`。backend 本身要求 root 入参，profile / home patch / `--patch` 可覆盖。因此不能只硬编码 `~/.dsh` 并声称支持全部配置。[S1]
2. 布局为 `<root>/<projectKey(cwd)>/<encodeSegment(sessionId)>/<generation filename>`。`projectKey` 有损；项目归属用 header 的原始 `cwd` 走本仓注册表，不反解目录名。session id 的路径编码也不能简化成取叶子目录或假定 UUID。[S2]
3. `session.jsonl[.zstd]` 是 v0；v1–v4 使用 `.vN`。只认规范的小写十进制名称；`.v0`、前导零、临时文件不是已提交代。**同目录选数值最高规范代，不按 mtime，不合并各代。** 遇到未知新版拒绝，不能回退旧版；文件名与 header 版本须一致。[S2]
4. backend 检查编码一致性；明文 / 压缩混放不能默认选一份碰运气。默认 zstd 是带 checksum 的串接帧：header 单独一帧，后续每次持久批次一帧，不是只有一帧的压缩文档。[S3]
5. 当前格式链为 0→1→2→3→4，迁移会展开、插入或重排事件，不是只改 header。v4 顶层每事件一行；旧版 packed row 和 v4 `data.stream` 内嵌 packed stream 是两层不同编码。[S4]
6. 原生日志是事实源；surface 与 projection cache 为派生数据。`surfaceOp=replace` 可让旧节点退出有效视图，但旧事件仍在日志。不能把 cache 当唯一来源；也不调用可能写回 cache 的 `coldSnapshot()` 充当只读探针。[S5]

探针中的代选择只是可复测的诊断实现，不是 `catalog` / `discover` / `scan` 已支持该 provider。正式来源需确定逻辑路径与来源版本如何跨代衔接，避免代切换被当成第二份会话。

## 4. 最小字段映射与读取状态

下表是实施依据，**不预定新增公开枚举、字段或 schema**：

| 来源证据                                                                                   | 本仓需要保留或判定的语义           | 约束                                                                                                 |
| ------------------------------------------------------------------------------------------ | ---------------------------------- | ---------------------------------------------------------------------------------------------------- |
| root、所选代、header version                                                               | 来源实例、逻辑来源与格式代         | 未知代不回退，物理压缩形态不是另一份事实                                                             |
| `id,createdAt,cwd`                                                                         | 会话生命周期与项目归属             | 单个 id 不证明生命周期；cwd 不从目录反解                                                             |
| `type,seq,time,data`                                                                       | 事件类型、原生顺序、原生时间与内容 | `seq` 不等于行号，跨代迁移可能改变；已核 event / stream 为 epoch 毫秒，不保证墙钟单调，见 §9         |
| `user/message.data`；system / developer / assistant 消息与 `tool/result` 的 `data.message` | 消息、角色、来源与内容             | 保留未知 producer / form，不能把 user 角色当人类证明；`tool/result` 不是 `tool/message`              |
| assistant attempt stream / usage                                                           | 文本、思考、工具及用量             | 已用原生 codec 与合成 fixture 核对主要字段，见 §9；cache 未知与精确 total 的表示仍须在正式适配前解决 |
| `parentSession,origin,delegationDepth,isSeeded`、`session/end-seed`                        | 父子关系、继承边界与 own suffix    | 有 parent 不等于继承历史；不能直接占用父事件槽位 `parent_ref`                                        |
| `subagent/descriptor`、父 `subagent/catalog`、`runId`                                      | 子会话与一次执行的不同身份         | external catalog 不保证本地有子日志；展示投影不能替代恢复判据                                        |
| `sourceEventSeqs,surfaceOp`                                                                | 重放引用与有效视图                 | 范围编码须解开；日志全史与当前有效消息分开                                                           |

事件位置不能只靠 `(session_id, seq)` 跨格式升级复用；正式方案至少考虑来源实例、会话生命周期与格式代。是否还能提供跨代稳定映射，须用迁移样例验证，不能由一次并存观察推断。[S4][S6]

读取状态至少区分：完整、可恢复前缀、读取期间变化、损坏、未知格式、迁移条件不足、I/O 未知与取消。源码显示明文缺换行的尾行不提交；坏记录后若出现 `turn/end` 会报损坏；压缩尾帧修复还可能缩短物理文件并重写恢复前缀。不能照搬「坏行跳过继续」或「物理压缩 offset 永远只增」的假设。[S3] §13 的受控反例进一步说明：本次取得合法前缀与物理来源完整是两条判断，原生 `eventState` 不承担后者。

## 5. H.1 可信输入契约草案

上层复核结论：当前还没有可供本仓验证的正式持久化临时属性、可信真实发送方或任务到宿主 / 子 CLI 的执行绑定载体；实验插件、合成探针不算产品接口。这里只保留能力缺口，不记录消费方内部工作编号。

源码中存在原生父子元数据、可扩展 `message.source.kind` / `form` 和连接层 peer / principal，但它们不是同一个证明层。`user/message` 还可能由注入或 goal continuation 产生；已核 header / event / message schema 没有通用 temporary 标记，不能把缺失当 false，也不能往严格 header 中直接加字段。[S7]

建议把下面三层分别验收，实际载体待双方验证：

| 层             | 最小完成判据                                                                                             | 缺失 / 冲突时                                            |
| -------------- | -------------------------------------------------------------------------------------------------------- | -------------------------------------------------------- |
| 原生日志事实   | 保留原生来源、父子关系与继承边界，并指出来源字段由谁写                                                   | 报未知，不把它提升为可信人类或执行授权                   |
| 控制面绑定     | 可把一次启动 / 执行绑定到具体来源实例与会话生命周期，区分临时 / 持久，并传给子 CLI；正式入口有可复现样例 | 隔离模式拒入；既有普通来源兼容策略另定，不全局猜成非临时 |
| 摄取与记忆消费 | CLI / Rust 写者统一在入库前排除，报告计数；缓存与记忆引擎遵守同一临时范围；旧写者不能绕过                | 显式阻断或协调版本，不只改单个 parser                    |

载体可评估原生正式扩展或单独的控制面绑定记录，但本轮不选定文件名、签名方式或公开 DTO。无论哪种载体，都必须说明信任边界、绑定真实性、生命周期复用、并发写入、属性继承与冲突处理；正文标签和自填 `principal` 不能自证。授权裁定仍归上层控制层。

## 6. 执行过的检查与后续实施门

**首轮结构扫描已执行**：探针自检覆盖未知字段 / 值与正文不泄露、脱敏结构投影、坏 JSON / UTF-8 / 非对象 / 未换行尾部可观察、预算截断、Zstd 多帧、checksum 错、规范代选择、前导零 / 临时名排除、未来代不回退、编码冲突；随后跑本机显式根的结构扫描。另经 CLI 验证缺失根与非目录根分别报告 `absent` / `not_directory`，均退出 2；Ruff 格式与检查、ty 类型检查、公开仓闸、指令预算闸及补丁格式检查通过。ty 显式使用含脚本依赖的解释器及依赖搜索目录。自检走与扫描相同的函数，首轮未启动原生恢复；后续合成原生核验见 §9–§10。未知字段被脱敏并不等于已理解其含义。

**正式适配仍须落到生产路径的反例集**（部分已有下文的合成核查证据，不能代替生产接入验收）：

- 文件名与 header 版本不符、同 id 跨目录、同 id 新生命周期、projectKey 碰撞、实际 profile root 覆写。
- v0 packed row 展开与跨代引用重映射；v4 内嵌 stream 与最终消息 / usage 不重复；未知 required / ignorable 事件分开。
- seq 缺口、无换行完整 JSON、坏行后无 / 有 `turn/end`；Zstd 尾帧截断但已有完整行、完整帧末尾半行、header 错放帧、并发 append / 修尾 / 新代发布。
- fork 继承父消息和 descriptor、parent 存在但未继承、external catalog 无本地子日志、descriptor 多份 / 未知版本。
- user-role 的人工、注入、agent relay、settlement 与未知来源；可信属性缺失 / 冲突、多写者及记忆缓存不绕过临时范围。
- CLI / Rust 构造与穷举 / 逐字段读取 / `SourceMode` 过滤兼容；旧版读者与并存写者协调。

建议首个隔离实现以已核 v4 为范围，旧代与未来代显式报不支持，先不在 Rust 中复制整条迁移链；该建议不是上线兼容承诺。§8–§15 已补静态 profile 根、合法合成 fixture、消息 / 工具 / usage、重试 / 继承、codec 坏尾、原生 backend 读取 / 修尾、双进程受控窗口、独立快照审计与 Node 同缓冲区捕获原型。正式实现仍缺生产读取状态与安全推进位置、活动启动配置、复杂迁移 / 有效视图样本、§11 用量保真草案的实现与兼容验证，以及 H.1 的可信载体；一般并发压力、崩溃及租约争夺仍未验。正式摄取、临时排除与来源保真按 H.7 一起验证交付，不能因核查脚本退出 0 就启用自动入库。

## 7. 安装源码索引

以下路径均相对已安装的 `@deepseek-ai/` 包目录，版本均为 `0.2.1-rc.5`；查证日期 **2026-11-05**。这些是源码读到的行为，未经本轮运行时复现的恢复 / 迁移分支已在上文标明。

| 索引 | 包相对文件与行                                                                                                                                                                                                                                                                                                                                                      | 支撑内容                                                                   |
| ---- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------- |
| S1   | 导出 `resolveDshHome` 的 home 路径包，入口 `lib/index.js:70,83,99`；`dsh-base/cordis.patch.yml:138`；`dsh-session-persistence-jsonl/lib/index.js:2434`；`dsh/lib/profile-boot-C14i-8IN.js:223`                                                                                                                                                                      | home / backend root、composition 与覆盖顺序                                |
| S2   | `dsh-session-persistence-jsonl/lib/index.js:758,848,871,907,919,2673,3159,3410,3443,3467,3526`                                                                                                                                                                                                                                                                      | 规范名、路径编码、最高代、版本 / 编码 / id 检查                            |
| S3   | `dsh-session-persistence-jsonl/lib/index.js:102,187,223,1024,1070,1086,1286,1298,1377,1641,2290,2298,2980,3268,3294`                                                                                                                                                                                                                                                | append、串接帧、尾部恢复、稳定读取与修尾                                   |
| S4   | `dsh-session-format-catalog/lib/index.js:39`；`dsh-session-format-v0-to-v1/lib/index.js:1595,1610,1661,1700,1741`；`dsh-session-format-v1-to-v2/lib/index.js:163,243,263,296,303`；`dsh-session-format-v3-to-v4/lib/index.js:1044,1065,1092,1452,1513`；`dsh-session-persistence-jsonl/lib/index.js:945`                                                            | 格式链、packed 位置、事件 envelope、迁移重排                               |
| S5   | `dsh-session/lib/types/types.d.ts:251,447`；`dsh-session/lib/index.js:1654`；`dsh-session-projection/lib/index.js:263`；`dsh-session-projection-cache/lib/index.js:34,272`                                                                                                                                                                                          | 事实源、surface、可丢弃 cache 与生命周期                                   |
| S6   | `dsh-session/lib/types/types.d.ts:408,490`；`dsh-session/lib/types/fork.d.ts:6`；`dsh-subagent/lib/index.js:332,697,1659,2158,2720`；`dsh-subagent/lib/types/descriptor.d.ts:44`；`dsh-subagent/lib/types/catalog.d.ts:19`                                                                                                                                          | seq、父子 / seed、descriptor、catalog 与恢复差别                           |
| S7   | `dsh-session-persistence-jsonl/lib/index.js:782,844`；`dsh-llm/lib/types/message.d.ts:30,95,123,134,166`；`dsh-session/lib/types/types.d.ts:289`；`dsh-subagent/lib/types/continuation-messages.d.ts:11`；`dsh-typert-protocol/lib/types/types.d.ts:352`；`dsh-api-remote-access/lib/types/types.d.ts:12,43`；`dsh-api-session-controller/lib/types/types.d.ts:386` | header、来源与角色、peer / principal、临时属性边界                         |
| S8   | `dsh-app-boot/lib/index.js:61,311,421,529,995,1015,1065,1084,1127,3942,3961`；`dsh/lib/profile-boot-C14i-8IN.js:236`；`dsh-base/cordis.patch.yml:138`                                                                                                                                                                                                               | 静态 patch 组合、兼容检查、读取与可能改写 profile 的入口区别               |
| S9   | `dsh-session/lib/types/types.d.ts:289`；`dsh-session/lib/index.js:1194,1378,1528`；`dsh-llm/lib/types/types.d.ts:45,413`；`dsh-agent-loop/lib/index.js:395,681,1093`                                                                                                                                                                                                | 消息 payload、工具标识、时间与 settlement 分支                             |
| S10  | `dsh-llm/lib/types/assistant-stream.js:31,135,378`；`dsh-llm/lib/types/assembler.js:33,121,155`；`dsh-session-format-v3-to-v4/lib/index.js:314,459,982,1057`；`dsh-session-persistence-jsonl/lib/index.js:1838`                                                                                                                                                     | stream 编解码、装配规则、原生恢复与校验边界                                |
| S11  | `dsh-token-meter/lib/types/usage-projection.js:60,91`；`dsh-token-meter/lib/types/turn-usage.js:21,115`；`dsh-llm-deepseek/lib/index.js:1832,2017`                                                                                                                                                                                                                  | 用量样本选择、重试折叠、完整性与 cache 拆分                                |
| S12  | `dsh-agent-loop/lib/index.js:1217,1226`；`dsh-session/lib/types/types.d.ts:218`；`dsh-llm-retry/lib/types/types.d.ts:13`；`dsh-session-format-v3-to-v4/lib/index.js:676`                                                                                                                                                                                            | 请求头 writer、完整 retry payload 与原生配对                               |
| S13  | `dsh-session-format-v1-to-v2/lib/index.js:219,263`；`dsh-session-format-v3-to-v4/lib/index.js:982,1065,1107`；`dsh-session/lib/index.js:1399`                                                                                                                                                                                                                       | framing 恢复、v4 admission、继承 cut 与 marker                             |
| S14  | `dsh-session-persistence-jsonl/lib/index.js:63,103,2488,2644,2839,2980,3626`；`dsh-session-persistence/lib/types/handle.d.ts:52`；`dsh-session-format-v2-to-v3/lib/index.js:533`                                                                                                                                                                                    | 原生 backend 挂载、只读句柄、历史代准备、帧扫描与修尾读取                  |
| S15  | `dsh-session-persistence/lib/types/handle.d.ts:19,38`；`dsh-session-persistence-jsonl/lib/index.js:63,228,1632,1652,2826,2938`                                                                                                                                                                                                                                      | 公开读取结果与并发前缀承诺、长度防倒退、稳定读单次重试、缓存身份及修尾顺序 |

## 8. 后续核查：静态 profile 的日志根

2026-11-05，用 [inspect-dsh-roots.mjs](../scripts/inspect-dsh-roots.mjs) 显式指定安装包与 home，只读检查本机两个已存在的 profile。工具使用安装版 `composeEntries`、bundle 路径解析与 patch 读取函数，按 bundle → profile → home → 显式 overlay 的顺序组合；不调用可能初始化、清理或改写 profile 的 `loadProfile` / `loadProfileDirectory`，不读取凭据或 settings 文件，也不启动 Host。[S8]

本轮两个 profile 分别声明 3 / 2 个 bundle，均未跳过 bundle、未产生 patch 告警，每份组合均只有一个 JSONL backend，最终 root 都仍是 `dshHomePath('sessions')` 表达式。观测的 17 份文件或缺失状态在检查前后相同；没有输出实际路径、profile 名或其他配置内容。

这证明**当前静态组合仍指向显式 home 下的 sessions**，没有执行 `!!js`，也没有观测活动 Host 的环境或额外启动参数。不能据此推出所有 Host 都使用这份 home；动态 root 表达式、相对根、disabled / 替换 backend、组合告警均单列为未完成状态。

已运行原生 patch 组合反例：默认表达式、后层根覆写、整份 config 替换后缺 root、显式禁用、未知表达式、父 group 动态禁用 / 明确禁用及 backend 替换；检查配置未被原生组合函数原地改变。Node 语法、自检与本机静态检查均退出 0。

## 9. 后续核查：消息、stream 与用量

2026-11-05，新增 [complete-turn.json](../tests/fixtures/dsh-v4/complete-turn.json) 与 [verify-dsh-v4.mjs](../scripts/verify-dsh-v4.mjs)。样本完全合成，包含 10 个事件、五种消息角色和一次完整工具调用关系；已通过安装版 v4 restore、detached `Session.create`、物理 codec 往返及内嵌 stream 往返。它不是上游 Host 实际运行生成的会话，也没有验证真实 provider 的输出。该批（`7625bce`）Node 核验为 12 组，退出 0；后续扩展到 20 组，见 §10。

### 消息与工具

| 事件                | 消息位置 / 关键字段                                   | 摄取约束                                                                                              |
| ------------------- | ----------------------------------------------------- | ----------------------------------------------------------------------------------------------------- |
| `user/message`      | `data` 就是消息对象                                   | `role=user` 仍不能证明人类来源                                                                        |
| `system/message`    | `data.{turn,step,message}`                            | 消息 source 为 `system-prompt`                                                                        |
| `developer/message` | `data.{turn,step,message,headerSeq?}`                 | producer 自有 source；tool-addition 引用历史 request/header，不内联工具定义                           |
| `assistant/message` | `data.{turn,step,message,stream,usage?,interrupted?}` | 已提交正文取 message.content；stream 是同一次生成的过程，不能再拼成第二份正文                         |
| `assistant/attempt` | `data.{turn,step,stream}`                             | 保留失败 / 取消 attempt 的过程，不伪造一条已提交消息                                                  |
| `tool/call`         | `data.{turn,step,callId,name,arguments}`              | arguments 是原始 JSON 字符串；call 记录不证明执行成功                                                 |
| `tool/result`       | `data.{turn,step,message,error?,meta?}`               | `message.toolCallId = message.source.callId`，对应 assistant 的 tool-call.id；结果有自己的 message id |

合成样本核对了 text、reasoning、tool-call 三类块；工具 id、参数与结果的原生关系错误会被拒绝。image / file / attachment、tool-addition / tool-removal、错误修复结果及 external 子会话仍须另补样本，不能把本轮内容子集当作全部合法 content。[S9][S10]

结构探针同步修正了工具事件白名单：采样实际 `tool/result`，加入 `tool/call`、`assistant/attempt` 与 camelCase 用量字段。第一轮结构探针不解码 DSH 语义的限制仍成立；字段白名单与脱敏输出不承担原生 admission。

### stream 的事实与去重

`text-chunks` / `reasoning-chunks` / `tool-call-chunks` 按 `time0` 与相邻 `dt` 展开，另有单个 `{type:"chunk",time,chunk}`。本轮实跑确认负 `dt` 合法，不能强加单调时间。event.time 为宿主写事件的 epoch 毫秒，stream 时间是宿主收到 chunk 的时间，不是 provider 生成时间。[S9][S10]

原生 assembler 按 index 首次出现顺序装配；首次 block-end 的完整块覆盖累积 delta，此后的 delta / re-close 不再改变该块。取消前缀只保留非空 text / reasoning，max-tokens 也去掉工具调用。本轮已运行这些分支。stream 中多条 usage 由最后一条覆盖，不能逐条相加；message 与 stream 也不能各记一次用量。

**原生 restore 不是完整内容校验器。** 本轮构造的嵌套 `text:123`、负 inputTokens 可通过 native restore + Session.create；raw chunk 的未知 type 可通过 stream 展开，但不能用于装配。本仓验证脚本把 native restore、stream / 字段子集及用量 reducer 分开检查，分别断言这些缺口；它仍不是可供生产摄取的完整 validator。[S10]

### 用量映射与不能压掉的未知

| DSH 字段            | 现有 TokenUsage 的候选字段 | 必须保留的限制                                                  |
| ------------------- | -------------------------- | --------------------------------------------------------------- |
| `inputTokens`       | `input`                    | 与 cache 分桶，不再次扣 cache；已核 DeepSeek adapter 的实际赋值 |
| `outputTokens`      | `output`                   | 不再把 reasoning 加一次                                         |
| `cacheWriteTokens?` | `cache_creation`           | 缺失不等于 0                                                    |
| `cacheReadTokens?`  | `cache_read`               | 缺失不等于 0                                                    |
| `reasoningTokens?`  | 无独立字段                 | 四项映射会丢失拆分                                              |
| `totalTokens?`      | 无独立字段                 | 四项相加不能保证重建原始精确 total                              |

已跑反例：`inputTokens=10, outputTokens=5, totalTokens=20`，cache 均未报告，严格 reducer 仍能保留精确总量 20 与未知 cache；若缺失 cache 填 0，四项之和只有 15。**正式适配前必须确定如何保留原始 usage / 未知状态及精确 total；不先修改公开类型或 schema。** 缺 usage、缺 turn/end、负值或矛盾 total 应报告不可得，不能生成 0 用量。[S11]

两个原生 reducer 的承诺也要分开：会话投影取外层 message.usage，缺失时取 stream 最后 usage，同一 turn/step 替换槽位，遇匹配 retry-started 后才另加；严格 `deriveTurnTokenUsage` 还要求完整 turn / step / attempt 边界，失败 attempt 也可计入。`7625bce` 的重试检查仅是 reducer 单元样本，后续已替换为通过完整 v4 admission 的 14 事件派生夹具，总量同为 37，见 §10。共享 `complete-turn.json` 的单次总量仍为 20。[S11]

### 复跑与剩余范围

两个 Node 工具共用 [dsh-installed.mjs](../scripts/dsh-installed.mjs) 的安装版本约束。明确传入自己要核的安装包与 home；不通过全局 CLI 启动 profile，不生成上游配置，不输出配置或会话原文。公开仓扫描闸补入 `.mjs`，使新增脚本也进入身份检查；该闸脚本的 Ruff 检查在本轮修改前后均有相同的 14 条存量诊断，未扩展清理，不能报成全量 Ruff 通过。

```sh
node scripts/inspect-dsh-roots.mjs --dsh-package '<dsh-package.json>' --home '<DSH_HOME>'
node scripts/verify-dsh-v4.mjs --dsh-package '<dsh-package.json>'
```

字段与合成样本准备已取得进展；后续完整重试、seed 前缀及 codec 恢复反例见 §10，用量保真方案草案见 §11。原生 backend 的合成文件读取 / 修尾见 §12，受控双进程窗口与读取状态反例见 §13；真实失败 / 重试、附件、compaction / 有效视图、一般并发压力及旧版读者 / 多写者协调仍未验收。活动启动配置、H.1 可信载体与这些实现门仍须完成，运行时适配尚未启用。

## 10. 后续核查：完整重试、继承与恢复边界

2026-11-05，扩展 [verify-dsh-v4.mjs](../scripts/verify-dsh-v4.mjs)，新增 [scenarios.mjs](../tests/fixtures/dsh-v4/scenarios.mjs) 和 [usage-cases.json](../tests/fixtures/dsh-v4/usage-cases.json)。场景从共享逻辑 fixture 派生，全部在内存中经过安装版原生函数验证；仍未运行 Host / LLM 或操作真实日志 / 总库。实跑 20 组检查，其中一组遍历 13 个用量向量，退出 0。

### 完整失败重试

派生样本有 14 个事件，包含 `request/header`、带失败 finish / usage 的 `assistant/attempt`、完整 `llm/retry` 与对应 `llm/retry-started`，随后成功消息及工具 call/result。请求头来自实际 writer 的形状，retry 补齐 provider、retryId、policyKey、mode、retry / maxRetries、delayMs 与 failure。样本通过 native v4 restore、detached Session、物理 codec 往返和 stream / 字段子集检查。[S12]

- 严格单 turn 总量为 37：uncached input 17、output 10、cache read 4、cache write 6；reasoning 2 已包含在 output 内。
- 失败 attempt 未提供模型路由，聚合结果也不返回完整 routes；不能拿成功消息的路由补给失败 attempt。
- 原生关系校验拒绝 provider 不匹配、retry 跳号、retry-started 无对应 retryId 或改变坐标。单独从 reducer 输入移除 retry-started 时，用量不可得；该删减序列不作为合法原生日志。
- 会话投影仍保留同槽覆盖的对照：没有 retry-started 时替换，匹配后另计一次。完整原生恢复和 reducer 语义分别断言。

### 继承前缀与本会话新增部分

派生 child 有 17 个事件：原 10 个事件逐个保留，cut=10 处为当前 child 拥有的 `session/end-seed {inherited:true}`，其后为新增 turn。marker 自己不属于 inherited prefix。原前缀用量 20 与本会话新增 turn 的用量 5 分别核算，不能把整份 child 当作一个 turn，也不能把父历史重新计作 child 工作。消息 id 保留在未改写的前缀中。[S6][S13]

再派生一层时，祖先 marker 留在继承前缀，**最后一个 inherited marker** 决定当前 cut。错误 cut、缺 marker、unseeded 却带 inherited marker 均被原生恢复拒绝。只有 `parentSession` 而 `isSeeded=false` 的样本通过，cut 仍为 0；父子关系本身不证明继承历史。上述边界通过 restore / Session 与 codec 往返，但未验证原生 `buildForkSeed` 的复杂 fork 修复、descriptor 或 compaction 有效视图。

### codec 恢复与完整性

| 合成反例                                                  | 实跑结果                                                 | 正式读取必须保留的判断                                       |
| --------------------------------------------------------- | -------------------------------------------------------- | ------------------------------------------------------------ |
| 最后一行 envelope 损坏，后面没有有效 turn/end             | strict 拒绝；recoverable 只返回有效前缀，finish 正常     | 对照输入行与产出；不能仅凭 finish 未抛错报完整               |
| 完整 turn 后再接坏行与无 end 的后缀                       | recoverable 返回原完整 turn，用量仍可得 20               | 有完整 turn / 用量不证明整个输入已读完，不推进到损坏后缀末尾 |
| 坏行或 seq 缺口之后还有有效 turn/end                      | recoverable 拒绝                                         | 不能跳过中间损坏继续入库                                     |
| 尾部 request/header 含已退役 header.system                | v4 admission 在 recoverable framing 前直接拒绝           | 不是所有错误都进入可恢复尾部逻辑                             |
| 未知 required 事件、tool/call 与 assistant 声明的参数不符 | codec 可解码；restore 才拒绝，unknown ignorable 则可保留 | strict codec 也不替代事件词汇与关系校验                      |

本节的离线脚本调用公开 codec 和 restore，没有经过 JSONL 字节分行或 DSH backend 的帧扫描 / 稳定读 / 修尾入口；后续原生 backend 合成文件验证见 §12。codec 的 recoverable 行为是恢复证据的一层，不能当作整个 backend 验收。[S13]

### Node zstd 原语的实测边界

本机 Node `v25.2.1`，合成数据使用带 checksum 的独立帧再串接。逐帧解压后字节等于原始 JSONL；改坏完整末帧的 checksum 会被拒绝。另实测：

- `zstdDecompressSync` 对整串帧只返回首帧；脚本显式报告 `first_frame_only`，不能把同步解压成功当成读完整个串接日志。
- 末帧少一个 checksum 字节时，默认同步解压仍返回完整 payload；`ZSTD_e_flush` 也能返回 payload。完整行已经出现，不证明帧已完整提交。
- 对完整但 checksum 错误的单帧，默认及 flush 两种同步调用均拒绝。

这些是当前 Node 同步 API 的实测，不泛化为所有版本或异步入口的行为。脚本允许其他 Node 返回完整串接内容或拒绝残帧，但每次都报告观测值。已知帧边界由夹具的压缩调用提供，未复制私有 scanner；该离线工具的 `backend_recovery_validated=false` 只限定本节范围。DSH backend 的结构扫描与逐帧流程已另用合成文件验证，见 §12。[S3]

## 11. 用量保真方案草案与存储边界

2026-11-05，核过本仓 [RawEvent / TokenUsage](../src/rawevent.rs)、[parser 的 profile 判据](../src/parser.rs) 与 [总库写入 / 读取](../src/store.rs)。`TokenUsage` 只有四个必填 `u64`，`RawEvent` 无通用原始记录扩展字段；`append_events` / `FileProjectionBatch` 接收 `RawEvent`，写入时把它序列化成加密 `event_json`，读取时反序列化回 `RawEvent`。**这一写读路径不会自动保留没有进入 DTO 的源字段**。`metadata` profile 不物化正文，因此把源 usage 放进 `content` 不能满足统一保真要求。

### 建议保留的独立事实

建议正式实现优先评估事件上的独立计量证据字段，旧四桶作为证据完整时的兼容投影；本轮只形成契约草案，不新增公开字段或 schema。证据至少同时包含：

| 事实         | 草案要求                                                                                                                                   |
| ------------ | ------------------------------------------------------------------------------------------------------------------------------------------ |
| 原生计量样本 | 保存原生 usage 对象、来自外层 message 还是 stream、所在事件与 stream 样本位置；保留被覆盖样本供核对，合计只用 settlement 的选定样本        |
| 逐字段值     | input / output / cache read / cache write / reasoning 各自保留已知与未知；报告的 0 仍是已知 0，缺字段不变成 0，也不自行求未知 cache 的拆分 |
| 精确 total   | 保留显式报告的 total，或在所有分桶已知时标记为推导值；reasoning 不重复加，原始矛盾值保留但不进入可用合计                                   |
| 可用状态     | 区分完全缺报告、已有合法但不足以求精确总量的报告、值非法 / 矛盾、生命周期不完整、已有精确总量；精确 total 可以与未知 cache 同时成立        |
| 计量归属     | 绑定来源实例 / 格式代 / 事件与 attempt；继承前缀不计作本会话新增。缺 route、父子元数据与执行绑定的未知分别保留                             |

原生严格 reducer 对缺失、非法和生命周期不完整均返回 `undefined`，不能单靠它的返回值推断失败原因。正式适配需要在规范化前保留字段观察和生命周期诊断；不能最终只输出 `usage=None`，使原始精确 total 或不可得原因消失。13 个公开向量已覆盖完整四桶、推导 total、精确 total + 全部 / 单个未知 cache、明确零、缺报告、缺 total、矛盾 total、reasoning 超界、负数、不安全整数与加法溢出；它们验证原生 reducer 的边界，不是已实现的本仓状态类型。

### 实现与兼容验收门

1. metadata / full 均可保留计量证据，JSON 序列化、加密入库、只读解码往返不丢逐字段未知、精确 total、原始报告及诊断。
2. 旧四桶仅在四项均可证明时提供；精确 total 与未知分桶不能靠填 0 进入旧统计。全量 / 增量 / 重扫 / 重试 / seed 均无重复合计。
3. 若给公开 `RawEvent` 加字段，Rust 字面量构造会受影响，必须核消费方构造 / 穷举与逐字段读取。Serde 对缺字段的默认兼容不等于旧读者理解新证据，更不解决新增 SourceType 值域的兼容。
4. 先实测旧版 reader 和并存 writer，再确定事件版本、schema / DTO 的实际形状与发布协调；本轮未测，因此不宣称可以零迁移上线，也不默认需要重建数据库表。
5. 和 H.1 可信属性及临时排除共同验收后才启用正式摄取；本轮合成证据不补足上层执行绑定。

## 12. 后续核查：原生 backend 的合成文件验证

2026-11-05，新增 [verify-dsh-backend.mjs](../scripts/verify-dsh-backend.mjs)，使用已安装 `0.2.1-rc.5` 的公开 plugin，在最小 Cordis Context 中挂载真实 JSONL persistence service。没有加载 profile / Host / LLM；没有复制私有帧扫描器或构造替代 backend。运行环境为 Windows、Node `v25.2.1`，19 组检查退出 0。[S14]

每次运行先核本仓 `target` 是真实目录、解析后仍位于项目内，再用 `mkdtemp` 建新目录。只向其中写完全合成的日志，路径由原生 `locate()` 得到并再次核范围；不接受真实 session root 参数。只读操作前后对照目录与文件列表、mtime / ctime、文件大小和 SHA-256；每组结束释放句柄与 Context。运行目录保留在 `target/dsh-backend-*`，报告给出本次相对路径，脚本不做递归清理。

### 已实跑的服务路径

| 合成场景                                                    | 实跑结果                                                                                             |
| ----------------------------------------------------------- | ---------------------------------------------------------------------------------------------------- |
| 完整明文、zstd 多帧                                         | 原生 open/read 返回完整事件；切片、末尾空读与元数据出口符合预期                                      |
| read 句柄 append / flush                                    | 均抛 SessionReadOnlyError，只读检查前后文件未变                                                      |
| 明文完整 JSON 但没有末尾换行                                | 最后一条不返回；尾部只有不完整 JSON 时也保留此前有效事件                                             |
| 坏 JSON 行之后仍有有效 turn/end                             | SessionPersistenceCorruptionError，不跳过后继续                                                      |
| zstd 最后一帧缺一个 checksum 字节                           | 读回已恢复的完整记录，文件字节不变；该样本能读回全部事件，仍存在物理坏尾                             |
| 完整帧 checksum 错误、完整帧内缺换行、header 与事件挤在首帧 | 均抛 SessionPersistenceCorruptionError                                                               |
| 完整重试、seed child                                        | 原生 backend 返回共享派生事件，child 的 inherited cut 与父前缀长度相等                               |
| v4 与不可读 v3 并存，v3 的 mtime 更晚                       | 选 v4；不按 mtime 选代，也不要求读完较低代                                                           |
| 只有合法 v3 纯文本 turn                                     | stat 报 migration-required；只读 open/read 在内存中转成当前逻辑事件，文件列表与哈希不变，没有发布 v4 |
| v3 读句柄已打开，再原子发布等价 v4                          | 原句柄能切换到新代，stat 变为 current；这是可控顺序交错，不是并发窗口压力验证                        |
| 已有 v4 读句柄，随后出现 v5                                 | 原句柄后续 read 和新 open 均抛 SessionFormatUnsupportedError，不回退 v4                              |
| 明文 / zstd 读句柄已读完，夹具写入者再追加完整 turn         | 原句柄读到新增部分；按旧逻辑事件 offset 读取只返回新增事件                                           |
| 已观察完整前缀后，文件被较短合法前缀原子替换                | 原句柄拒绝倒退，不静默缩短已观察历史                                                                 |
| 明文 / zstd 坏尾，随后经原生 write 句柄追加                 | open/read 尚不修尾；首次 append 修复并持久化，再读事件无丢失、无重复                                 |

历史样本单独构造两代都支持的纯文本 turn，由原生 v3 codec 编码；没有把包含 v4 developer / tool-role 的共享样本仅改 header 后冒充合法 v3。v4 发布和“另一写者追加”由夹具写入者控制；原生读者始终使用 read 句柄。两个修尾样本则显式打开 write 句柄，只修改项目内合成日志，核明文最终字节、zstd 已完成帧的保留前缀及重新读取的完整事件。

### 对正式适配的约束

`SessionHandle.read()` 的公开结果只有事件与事件所有权状态，修尾位置等诊断没有随它返回。**能读出完整 turn 甚至全部逻辑事件，也不证明物理来源无坏尾。** 正式摄取仍需独立保留读取状态和可安全推进的位置，不能仅拿原生 read 的事件数或没有异常当完整性证明。原生 writer 会截断 / 重编码尾帧，物理 offset 不能作为永久只增的事实。[S14]

本节脚本验证原生 backend 在合成文件上的读取、受控代切换和写路径修尾，报告的 `concurrent_races_validated=false` 只描述该脚本；新增双进程受控窗口另见 §13。崩溃持久性、租约争夺、0→4 全链迁移、复杂 fork / compaction、活动配置根、WSL 或真实会话修复仍未验证；原有 `verify-dsh-v4.mjs` 的 `backend_recovery_validated=false` 仍只描述该离线 codec 工具，不代表本节没有调用 backend。

```sh
node scripts/verify-dsh-backend.mjs --dsh-package '<dsh-package.json>'
```

## 13. 后续核查：并发窗口与读取状态反例

2026-11-05，新增 [verify-dsh-concurrency.mjs](../scripts/verify-dsh-concurrency.mjs) 和独立进程夹具 [persistence-peer.mjs](../tests/fixtures/dsh-v4/persistence-peer.mjs)。安装版仍为 `0.2.1-rc.5`，Windows、Node `v25.2.1`；21 组检查退出 0。它们验证指定的跨进程窗口与读取结果边界，尚未实现生产读取状态或安全游标。[S15]

### 方法与隔离范围

两个独立 Node 进程分别挂载原生 Cordis backend，正向断言 PID 不同，通过 IPC 请求与回执安排合成日志操作。父进程临时包装公开 `node:fs/promises.readFile` / `truncate`，并用 `syncBuiltinESMExports()` 同步 ESM binding：在真实 I/O 已完成、backend 尚未取得结果时，让另一进程完成指定追加、替换、发布或读取，再继续原生调用。每个窗口断言预定 I/O 已命中及实际次数，避免未触发也算通过；`finally` 恢复方法，超时集中定义为 15 秒，不靠 sleep 猜时序。为使已打开句柄重新进入真实读取，必要时先以同字节替换刷新文件身份并核身份变化。

没有改安装源码、访问私有 backend 状态或复制私有 scanner。父子进程均把操作限制在本仓 `target` 下本次新建运行目录，拒绝链接 / 越界路径，不接受真实 session root。每组释放句柄与 Context，结束时等待自己启动的子进程退出；保留 `target/dsh-concurrency-*` 合成运行物，不递归清理。没有启动 Host / LLM，也没有读取真实日志、凭据或写总库。替换 / 发布由夹具控制，其中未来代用排他硬链接发布；它们不证明任意外部改写都满足原生 append 契约。

### 已实跑的 21 组

前十行分别跑明文 / zstd，共 20 组；历史代发布另 1 组。每次比较完整事件内容与顺序，续读比较精确后缀，不只比较数量。

| 受控场景                                             | 实跑结果与限制                                                                                                                                     |
| ---------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------- |
| 首次 readFile 完成后只追加下一记录 / 帧的 2 字节     | 原生重读一次，仅返回原 10 事件；文件仍有残尾，reader 未修复。补齐后同句柄得到 16 事件，旧逻辑 offset 只返回新增 6 条                               |
| 两次 readFile 后分别追加一个完整 turn                | 两次读取均与追加重叠，本次返回 16 事件时磁盘已存在 22 事件；随后续读精确补齐 6 条，全读与 22 事件样本一致                                          |
| 另一进程持原生 write 句柄 append + flush             | 父进程既有 read 句柄随后读到完整 16 事件及精确新增后缀；该场景未争夺第二份 write 租约                                                              |
| 首次 readFile 后发布损坏内容                         | 明文为坏 JSON 后仍有 turn/end，zstd 为完整帧 checksum 错；第二次读后均报 SessionPersistenceCorruptionError。恢复文件后同句柄可读，失败未变成空成功 |
| 公开 readFile 屏障注入 EIO                           | 保留原 `code=EIO`；恢复方法后读取正常、文件未变。这是受控故障注入，不是实测磁盘或权限故障                                                          |
| 已观察前缀后替换成同事件数的不同正文                 | 两种编码均读回改写后的 10 事件；明文物理字节数也相同，zstd 物理字节数不同。事件数防倒退不证明旧内容未变                                            |
| 原生 writer 完成 truncate、尚未写回恢复记录          | 明文读者仍取得原前缀；zstd 既有读者因低于已观察前缀而拒绝。writer 完成恢复及追加后，同读者取得完整 16 事件                                         |
| v4 的 readFile 已返回，另一进程才发布 v5             | 执行中的读取返回已选 v4 合法前缀；下一次 read 和新 open 均拒绝未知代，没有回退到 v4                                                                |
| readFile 后追加并取消当前读取                        | 保留 AbortError，不返回空成功；下一次未取消的 read 读到完整 16 事件                                                                                |
| 完整文件与带坏尾文件分别读取                         | 明文 / zstd 的公开结果均完全相等，键只有 `eventState` / `events`；对照确认物理字节不同且 reader 未修尾                                             |
| open 已读到 v3 字节后，另一进程发布含新增 turn 的 v4 | 随后的 handle.read 使用 v4，得到 12 事件，v3 字节未变，stat 为 current。v3 用原生 codec 编码的合法纯文本 turn，不以改 v4 header 冒充               |

### 读取结果不能替代的证据

原生 `read()` 在并发写入时只承诺合法连续前缀；`eventState=shared-frozen` 描述事件所有权，不表示物理完整。稳定读比较文件身份，变化时重试一次；若再次变化，则按第二次读之前的大小返回字节前缀。该分支依赖 append 保持旧前缀的前提。两次追加得到 16 / 22 的实测符合这个边界，不能把“本次没有异常”解释成已追到物理 EOF。[S15]

既有句柄的长度检查能阻止事件数倒退，没有逐事件证明旧前缀内容相同。同事件数改写反例说明：只持久化事件数或 `seq`，下一轮可能漏掉已消费内容的变化。zstd 修尾反例又说明：在已知 writer 修复的窗口内，暂时读不到旧前缀不等于永久损坏；生产路径不能只凭错误字符串把所有缩短归因为修尾，也不能用空结果覆盖已归档前缀。这些是适配约束，不据此定性上游 append 契约有缺陷。

正式读取方案需要分别表达本次是否取得可用前缀、来源是否变化 / 完整以及失败原因，至少守住：

| 判据                                         | 读取状态 / 推进要求                                                                        |
| -------------------------------------------- | ------------------------------------------------------------------------------------------ |
| 已验证的物理快照完整，或仅恢复出前缀         | 分别报告；当前原生公开结果无法区分，若没有独立证据则完整性保留未知，不能借事件齐全补成完整 |
| 读取期间追加 / 修尾 / 代发布                 | 保留本次选代与来源版本证据；合法旧前缀可与“仍有变化”同时成立，不能冒充已追平               |
| 旧前缀缩短或内容改写                         | 使旧推进凭据失效，重新核对 / 建立投影；不能只凭相同事件数沿用，也不能把未读成当成空会话    |
| 损坏、未知格式、迁移条件不足、I/O 未知、取消 | 保留各自状态与已有已验证前缀；是否重试另定，失败不自动推进，也不产生空成功                 |

安全推进凭据至少绑定来源实例、会话生命周期、格式代、实际已验证前缀及其版本证据；内容改写、代变化和尾部重编码时须验证旧凭据是否仍成立。物理 EOF、旧压缩 offset 或逻辑事件数都不能单独充当该凭据。如何取得独立完整性证据、验证旧前缀及把状态接入 `scan` / 投影替换，仍是下一步实现设计；本轮不新增公开枚举、DTO 或 schema。

报告明确 `controlled_concurrent_windows_validated=true`、`public_physical_completeness_available=false`、`general_stress_validated=false` 与 `crash_durability_validated=false`。无约束压力、崩溃持久性、租约争夺、WSL、0→4 全链迁移、复杂 fork / descriptor / compaction 仍未验；正式 provider、可信临时属性和统一排除仍未交付。

下一阶段的生产接缝与旧消费基线修复见 [读取状态设计](dsh-read-state-design.md)。上游公开 `stat.revision` 已有变化观察能力，但只允许在同 service 实例 / session id 内比较；不能当成跨重启持久化游标或来源完整性证明。现有扫描的全读意图与旧来源基线也要分开保留，不能因解析器刷新丢掉缩短 / 时间回退的证据。

```sh
node scripts/verify-dsh-concurrency.mjs --dsh-package '<dsh-package.json>'
```

## 14. 后续核查：独立快照完整性

2026-11-06，新增 Python 逐帧 / checksum / 严格 UTF-8 审计及原生 persistence / restore 对照，29 组检查通过；审计器自测另覆盖第二帧的 45 个非空截断点及四个 checksum 字节破坏。DSH 安装版仍由脚本核为 `0.2.1-rc.5`，Node 实跑 `v25.2.1`，独立解码依赖固定 `zstandard==0.25.0`。实现、命令和完整边界见 [读取状态设计 §6](dsh-read-state-design.md#6-独立快照完整性验证)。

关键结果：最后一个 checksum 字节缺失时，原生 read 仍可返回全部 10 事件；追加新事件但缺 checksum 时甚至可取得 11 事件，独立审计均阻断。正文坏 UTF-8 可被原生替换后接受，严格审计拒绝。反向对照确认物理完整不等于语义可接受，而重新分帧可以只改变物理摘要、保留相同 JSONL 与事件。旧副本的完整性结果不能配给改写后的副本。

本节只证明新建、无并发 writer 的合成副本可核，不证明从活跃来源捕获快照的原子性；两端 hash 相同也不能防 ABA。该脚本报告保留 `live_source_capture_validated=false`、`production_advance_gate_integrated=false` 与 `provider_registered=false`，已提交为 `6b2a0b3`，尚未合并发布。后续实际捕获原型见 §15；它不改变本脚本的范围，也没有完成 H.4。

## 15. 后续实现：同缓冲区捕获与旧前缀核对

2026-11-06，新增 [dsh-snapshot-reader.mjs](../scripts/dsh-snapshot-reader.mjs)、显式只读 [read-dsh-snapshot.mjs](../scripts/read-dsh-snapshot.mjs) 与 [verify-dsh-capture.mjs](../scripts/verify-dsh-capture.mjs)。同一真实文件句柄取得的私有 Buffer 同时用于独立物理审计和原生 codec / restore / detached Session admission；Python 新增 stdin 字节入口与已验证帧范围，Node 不重开来源拼接证据。输入 / 解码 / 审计资源上限归同一份 JSON 配置。完整流程、状态与命令见 [读取状态设计 §7](dsh-read-state-design.md#7-活跃来源的实际捕获原型)。

选代依据重新核过安装源码（查证日期 2026-11-06）：`dsh-session-persistence-jsonl/lib/index.js:2450` 的 `locate` 是当前格式目标的诊断路径；`:2570` 的 `stat` 提供同 service 内 revision / header 观察；`:3411,3444,3468` 是目录最高规范代、跨项目目录歧义及 header 绑定检查。公开规范文件名 parser 见 `dsh-session-format/lib/types/filename.d.ts`。捕获入口据此另列目录选择实际物理代，并在真实句柄读取前后、审计后重查文件身份、metadata 与选代；不拿逻辑版本号代替物理代。

Windows、Node `v25.2.1`、DSH `0.2.1-rc.5`、`zstandard` `0.25.0` 上 36 组捕获检查全部通过：两种编码各 17 组，另加重分帧及输入预算。包括稳定读取与只读 CLI、receipt 归属、导出副本隔离、原生追加、同事件数改写、生命周期变化、断尾、读中追加 / 原地覆盖、读后两次路径替换的受控 ABA、未来代发布 / 后续复查、读取取消及运行中审计进程树取消、EIO 注入、语义拒绝与 metadata 缺失。每个受控 I/O 窗口均断言真实命中，复用 [io-harness.mjs](../tests/fixtures/dsh-v4/io-harness.mjs) 及独立进程；原生并发 21 组和冻结快照 29 组回归通过。

原生修尾在 truncate 已完成、恢复记录未写时，明文中间快照仍有 10 事件，zstd 只有 9 事件并返回 `prefix_shrank`；修复完成并追加后均为 16 事件，旧前缀保持。这里对照的是完整内容及顺序，不能凭事件数复用旧证据；失败后仍可读取旧内存快照，也不等于已接入总库事务保留。重新分帧只改变物理摘要时，JSONL 摘要与事件前缀仍保持。

receipt 仅可由该 reader 的实际验证路径登记，外部对象 / JSON / 另一 reader 的句柄不能冒充；内部 Buffer 不导出，事件导出为深复制。来源绑定含调用方明确配置的实例、规范目录、session id、压缩与物理代，生命周期含 header 和继承边界。配置实例尚非可信主机身份，receipt 也不是跨重启持久化凭据。

报告为 `controlled_capture_windows_validated=true`、`same_buffer_validation_and_decode=true`；一般压力、任意 ABA 排除、生产提交门及 provider 仍为 false。成功摘要显式保留 `atomic_source_snapshot_proven=false` 和 `production_advance_permit=false`，后续 `matches_observation` 仅代表再一轮观察。整个原型不改 Rust 公开 API、Cursor、SourceType 或 schema，不连接真库，也不提供正式 `svault` 命令。本节实现已提交为 `86c34aa`，尚未合并发布。后续 Rust 字节 / 原生位置核验见 §16；生产读取 / 提交门、归一化位置映射与持久化重放仍未完成。

## 16. 后续实现：Rust 字节与原生记录位置

2026-11-06，新增 `dsh-experimental` feature、[dsh_snapshot.rs](../src/dsh_snapshot.rs)、只读 [example](../examples/dsh_snapshot.rs) 和 [verify-dsh-rust-snapshot.mjs](../scripts/verify-dsh-rust-snapshot.mjs)。通过 `probe` 一次打开、取得有界字节后，在同一 Vec 上复用 `compressed.rs` 唯一帧头 / 解码器，核 checksum、分行、UTF-8 与最小 v4 envelope。私有构造的成功结果只导出摘要及可选记录位置，不注册 provider，不产生 RawEvent、观察或推进凭据。

Windows 与 WSL Linux 二进制分别通过 136 份合成快照及四组附加对照；独立 Python 帧审计、Node 解压与原生 codec / restore / Session admission 用于交叉检查。覆盖最后一帧的 89 个非空截断点、四个 checksum 字节破坏、输入 / 解码 / 窗口预算、字典与无 checksum、坏 JSON / UTF-8、混合 header 帧、重复 / 缺失 seq、重分帧与空帧位置。重分帧后原生记录的 JSONL 字节范围不变，物理帧位置改变。原生 seq、记录索引、UTF-8 字节位置和物理帧位置分别输出；它们没有被当成 RawEvent.seq 或 Cursor 的位置。

`9caf18b` 提交时，Windows 432 个库测试与所需功能组合 / 零警告 clippy 通过；Linux experimental 组合 256 个库测试与 store 编译通过，Linux 全库 store 测试当时另有一项既有钥匙链准备失败（后续本批已在 `store.rs` 修复并全绿，详见 §17 与 [读取状态设计 §8–§9](dsh-read-state-design.md#8-rust-同缓冲区物理核验与原生记录位置)）。现有 Rust 消费方临时指向本仓源码的编译也通过，未改其 manifest、lock 或子模块指针。

本节记录 `9caf18b` 的 Rust 物理与原生记录位置隔离验收，尚未合并发布；该版语义 admission、来源 / 生命周期 / 选代与旧前缀核对尚未移植，归一化位置和生产写库门也未接入。未知必需事件或断裂 call/result 能通过 Rust 物理检查、同时被原生 admission 拒绝，对拍明确保留这一差别。所有成功摘要的 `production_advance_permit`、`semantic_admission_validated`、`source_observation_validated` 与 `normalized_positions_available` 均为 false；没有完成 H.4 或完整 WSL DSH 捕获。

## 17. 后续实现：Rust 原生 admission 与继承边界绑定

2026-11-06，Rust `PhysicalSnapshot` 保留实际核验后的私有 JSONL，经 stdin 交给显式本机 Node / 安装版 DSH，复用严格 codec、restore 与 detached Session。共享脚本内嵌于实验模块，不重开来源、不导出正文；响应摘要、原 header、事件覆盖和退出状态匹配后才构造私有 `AdmittedSnapshot`。支持版本仍由安装约束核为 `0.2.1-rc.5`，Node 两平台实际 `v25.2.1`；详见 [读取状态设计 §9](dsh-read-state-design.md#9-rust-原生-admission-接缝)。这是显式可信本地运行时的接缝，不认证恶意 SDK。

同时修正 §15 生命周期描述未落实的部分：原来只 hash 编码后的 header，但 v4 `encodeHeader` 不写入 inherited cut。现在共享摘要函数显式纳入 cut；两份原生合法、header 完全相同、旧事件前缀保留的 artifact，cut 为 10 / 17、事件数为 17 / 24，实际捕获判为 `lifecycle_changed`。捕获矩阵由此前 36 组增加到 38 组并通过。

Windows / WSL Linux 各通过 140 份原生模式快照和五组附加对照，包括上述 seeded 样本、原生语义拒绝、同缓冲区摘要、全部字节 / 帧位置与物理失败；Windows 纯物理模式同一矩阵回归通过。每个平台另过 17 组异常响应、错绑、资源预算与进程收尾检查，超时后本次父子 PID 均已消失；ready 用随机标记，不跨系统比较时间。Windows 库测试 435 passed，Linux experimental 库测试 259 passed；修复 `open_existing_returns_the_store_when_it_is_there` 改用显式测试文件钥匙后，Linux 全库 `store`（415 passed）与 `store,dsh-experimental`（421 passed）均 0 失败通过，相关 feature 编译与零警告 clippy 通过。

原生模式成功将 `semantic_admission_validated` 置为 true，其他来源观察、归一化、原子来源证明和生产推进标志仍为 false；无参数的物理模式保留原语义。本节已提交为 `495bd46`，尚未合并发布；后续 Rust 来源选代、同句柄身份捕获与保守提交门见 §18。

## 18. 后续实现：Rust 来源选代、同句柄捕获与保守提交门

2026-11-06，在 [probe.rs](../src/probe.rs)、[source.rs](../src/dsh_snapshot/source.rs)、[native.rs](../src/dsh_snapshot/native.rs) 与 [examples/dsh_snapshot.rs](../examples/dsh_snapshot.rs) 中实现 Rust 会话目录最高规范代选择、同句柄文件身份（`FileStamp`）读取与前后复查、逐事件语义摘要（`event_sha256s`）绑定、`SourceSnapshot::compare` 生命周期 / 旧前缀对照及观察前保守提交门 `evaluate_commit_gate`；详见 [读取状态设计 §10](dsh-read-state-design.md#10-rust-来源选代同句柄身份旧前缀对照与保守提交门)。

所有 `std::fs` 操作仍收在 `probe.rs` 内：`read_pinned_regular_file` 对目标路径依次核打开前 `lstat`、同句柄 `file.metadata()`、有界读取、读后同句柄 `file.metadata()` 及关闭句柄后 `lstat`；`capture_source` 在原生 admission 前后两次重查目录选代与文件身份。`evaluate_commit_gate` 将修尾中间态（zstd `prefix_shrank`）与任何捕获阻断统一归为 `HoldPriorProjection`，阻止缩短或未完成读取覆盖旧投影；只有旧前缀完整保持才进入 `NoOpUnchanged` 或 `IncrementalAppendCandidate`，生命周期变化或旧前缀改写则标 `invalidates_prior_credential=true` 并进入 `RebuildProjectionCandidate`。

Windows 与 WSL Linux 实跑 [verify-dsh-rust-source-capture.mjs](../scripts/verify-dsh-rust-source-capture.mjs) 各 35 组检查全部通过，两平台 19 组 `verify-dsh-native-admission.mjs` 与 140 份快照 + 5 组对照的 `verify-dsh-rust-snapshot.mjs`（物理 + 原生模式）回归全部通过；Windows `store,dsh-experimental` 库测试 439 passed，WSL Linux `store,dsh-experimental` 库测试 425 passed，两平台零警告 clippy 通过。

成功摘要将 `source_observation_validated` 与 `semantic_admission_validated` 置为 true，同时保留 `normalized_positions_available=false`、`atomic_source_snapshot_proven=false`、`production_advance_permit=false`。本批源码尚未提交，尚未接入 `RawEvent` 归一化位置、`AppendLogObservation` / `CommitPlan`、总库写库事务与状态文件重放。
