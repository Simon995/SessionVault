# DSH 来源核查与适配判据

> 2026-11-05 第一轮只读核查。安装包源码与本机日志结构已交叉核对；**不是正式 provider、临时排除或可信来源接口的交付**。任务状态与先后只在 [backlog.md](backlog.md) H 节维护，职责见 [INGEST_KERNEL.md](INGEST_KERNEL.md) §2.2。

## 1. 证据范围

- **安装源码**：本机 `@deepseek-ai/dsh` 及下列相关包，已核版本均为 `0.2.1-rc.5`。这是安装包快照，不声称 npm 最新版或上游 `main` 当前仍相同。包元数据指向 [deepseek-ai/deepseek-harness](https://github.com/deepseek-ai/deepseek-harness)，查证日期为 2026-11-05。
- **实读范围**：显式指定 `<DSH_HOME>/sessions` 下的本地日志，使用 [inspect-dsh.py](../scripts/inspect-dsh.py)。未启动 Host / LLM，未读取凭据配置、回放或调用上游恢复 API，未写上游日志或总库。
- **未覆盖**：其他 profile 覆写的 root、WSL / 其他机器来源、当前活动 Host 最终采用的配置、原生恢复与迁移的运行时验收。目录存在只能证明这里有日志，不证明它覆盖全部活动来源。
- **工具限制**：探针只统计物理 JSONL、白名单字段形状并输出脱敏结构投影，不解码完整 DSH 事件语义、attempt stream 或有效 surface。`complete` 仅表示本轮流读取未中断；`valid_jsonl` 不代表通过 DSH schema 校验。压缩帧完整性没有独立验证，不能用本轮退出码证明 torn-frame 恢复正确。

## 2. 本机观察

以下数字来自 2026-11-05 本轮探针，包含两代日志，**不是去重后的会话事件数或 RawEvent 数**：

| 项 | 实测 |
| --- | --- |
| 会话目录 | 6 |
| 物理日志 | 10 |
| `session.jsonl.zstd`，header version 0 | 4 |
| `session.v4.jsonl.zstd`，header version 4 | 6 |
| 同目录两代并存 | 4 |
| 按最高规范代选择 | 6 个目录均选 v4 / zstd |
| 解压后读到的字节 | 8,975,978 |
| 物理 JSON 对象 / 行 | 6,572 / 6,572 |
| 预算触顶、遍历错误、读取期间文件变化 | 本轮均未观察到 |

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

| 来源证据 | 本仓需要保留或判定的语义 | 约束 |
| --- | --- | --- |
| root、所选代、header version | 来源实例、逻辑来源与格式代 | 未知代不回退，物理压缩形态不是另一份事实 |
| `id,createdAt,cwd` | 会话生命周期与项目归属 | 单个 id 不证明生命周期；cwd 不从目录反解 |
| `type,seq,time,data` | 事件类型、原生顺序、原生时间与内容 | `seq` 不等于行号，跨代迁移可能改变；时间单位仍须逐事件类型核验后映射 |
| `user/message.data`；其余 message 事件的 `data.message` | 消息、角色、来源与内容 | 保留未知 producer / form，不能把 user 角色当人类证明 |
| assistant attempt stream / usage | 文本、思考、工具及用量 | 须展开对应 codec，并防止 stream 与最终 message 重复计量；本轮未完成字段级映射 |
| `parentSession,origin,delegationDepth,isSeeded`、`session/end-seed` | 父子关系、继承边界与 own suffix | 有 parent 不等于继承历史；不能直接占用父事件槽位 `parent_ref` |
| `subagent/descriptor`、父 `subagent/catalog`、`runId` | 子会话与一次执行的不同身份 | external catalog 不保证本地有子日志；展示投影不能替代恢复判据 |
| `sourceEventSeqs,surfaceOp` | 重放引用与有效视图 | 范围编码须解开；日志全史与当前有效消息分开 |

事件位置不能只靠 `(session_id, seq)` 跨格式升级复用；正式方案至少考虑来源实例、会话生命周期与格式代。是否还能提供跨代稳定映射，须用迁移样例验证，不能由一次并存观察推断。[S4][S6]

读取状态至少区分：完整、可恢复前缀、损坏、未知格式、迁移条件不足、I/O 未知。源码显示明文缺换行的尾行不提交；坏记录后若出现 `turn/end` 会报损坏；压缩尾帧修复还可能缩短物理文件并重写恢复前缀。不能照搬「坏行跳过继续」或「物理压缩 offset 永远只增」的假设。[S3]

## 5. H.1 可信输入契约草案

上层复核结论：当前还没有可供本仓验证的正式持久化临时属性、可信真实发送方或任务到宿主 / 子 CLI 的执行绑定载体；实验插件、合成探针不算产品接口。这里只保留能力缺口，不记录消费方内部工作编号。

源码中存在原生父子元数据、可扩展 `message.source.kind` / `form` 和连接层 peer / principal，但它们不是同一个证明层。`user/message` 还可能由注入或 goal continuation 产生；已核 header / event / message schema 没有通用 temporary 标记，不能把缺失当 false，也不能往严格 header 中直接加字段。[S7]

建议把下面三层分别验收，实际载体待双方验证：

| 层 | 最小完成判据 | 缺失 / 冲突时 |
| --- | --- | --- |
| 原生日志事实 | 保留原生来源、父子关系与继承边界，并指出来源字段由谁写 | 报未知，不把它提升为可信人类或执行授权 |
| 控制面绑定 | 可把一次启动 / 执行绑定到具体来源实例与会话生命周期，区分临时 / 持久，并传给子 CLI；正式入口有可复现样例 | 隔离模式拒入；既有普通来源兼容策略另定，不全局猜成非临时 |
| 摄取与记忆消费 | CLI / Rust 写者统一在入库前排除，报告计数；缓存与记忆引擎遵守同一临时范围；旧写者不能绕过 | 显式阻断或协调版本，不只改单个 parser |

载体可评估原生正式扩展或单独的控制面绑定记录，但本轮不选定文件名、签名方式或公开 DTO。无论哪种载体，都必须说明信任边界、绑定真实性、生命周期复用、并发写入、属性继承与冲突处理；正文标签和自填 `principal` 不能自证。授权裁定仍归上层控制层。

## 6. 执行过的检查与后续实施门

**本轮已执行**：探针自检覆盖未知字段 / 值与正文不泄露、脱敏结构投影、坏 JSON / UTF-8 / 非对象 / 未换行尾部可观察、预算截断、Zstd 多帧、checksum 错、规范代选择、前导零 / 临时名排除、未来代不回退、编码冲突；随后跑本机显式根的结构扫描。另经 CLI 验证缺失根与非目录根分别报告 `absent` / `not_directory`，均退出 2；Ruff 格式与检查、ty 类型检查、公开仓闸、指令预算闸及补丁格式检查通过。ty 显式使用含脚本依赖的解释器及依赖搜索目录。自检走与扫描相同的函数，未启动原生恢复。未知字段被脱敏并不等于已理解其含义。

**正式适配仍须执行的反例集**：

- 文件名与 header 版本不符、同 id 跨目录、同 id 新生命周期、projectKey 碰撞、实际 profile root 覆写。
- v0 packed row 展开与跨代引用重映射；v4 内嵌 stream 与最终消息 / usage 不重复；未知 required / ignorable 事件分开。
- seq 缺口、无换行完整 JSON、坏行后无 / 有 `turn/end`；Zstd 尾帧截断但已有完整行、完整帧末尾半行、header 错放帧、并发 append / 修尾 / 新代发布。
- fork 继承父消息和 descriptor、parent 存在但未继承、external catalog 无本地子日志、descriptor 多份 / 未知版本。
- user-role 的人工、注入、agent relay、settlement 与未知来源；可信属性缺失 / 冲突、多写者及记忆缓存不绕过临时范围。
- CLI / Rust 构造与穷举 / 逐字段读取 / `SourceMode` 过滤兼容；旧版读者与并存写者协调。

建议首个隔离实现以已核 v4 为范围，旧代与未来代显式报不支持，先不在 Rust 中复制整条迁移链；该建议不是上线兼容承诺。开工前补真实配置根确认、合法完整的合成 fixture、文本 / 思考 / 工具 / usage 字段对账，以及 H.1 的可信载体。正式摄取、临时排除与来源保真按 H.7 一起验证交付，不能因结构探针退出 0 就启用自动入库。

## 7. 安装源码索引

以下路径均相对已安装的 `@deepseek-ai/` 包目录，版本均为 `0.2.1-rc.5`；查证日期 **2026-11-05**。这些是源码读到的行为，未经本轮运行时复现的恢复 / 迁移分支已在上文标明。

| 索引 | 包相对文件与行 | 支撑内容 |
| --- | --- | --- |
| S1 | 导出 `resolveDshHome` 的 home 路径包，入口 `lib/index.js:70,83,99`；`dsh-base/cordis.patch.yml:138`；`dsh-session-persistence-jsonl/lib/index.js:2434`；`dsh/lib/profile-boot-C14i-8IN.js:223` | home / backend root、composition 与覆盖顺序 |
| S2 | `dsh-session-persistence-jsonl/lib/index.js:758,848,871,907,919,2673,3159,3410,3443,3467,3526` | 规范名、路径编码、最高代、版本 / 编码 / id 检查 |
| S3 | `dsh-session-persistence-jsonl/lib/index.js:102,187,223,1024,1070,1086,1286,1298,1377,1641,2290,2298,2980,3268,3294` | append、串接帧、尾部恢复、稳定读取与修尾 |
| S4 | `dsh-session-format-catalog/lib/index.js:39`；`dsh-session-format-v0-to-v1/lib/index.js:1595,1610,1661,1700,1741`；`dsh-session-format-v1-to-v2/lib/index.js:163,243,263,296,303`；`dsh-session-format-v3-to-v4/lib/index.js:1044,1065,1092,1452,1513`；`dsh-session-persistence-jsonl/lib/index.js:945` | 格式链、packed 位置、事件 envelope、迁移重排 |
| S5 | `dsh-session/lib/types/types.d.ts:251,447`；`dsh-session/lib/index.js:1654`；`dsh-session-projection/lib/index.js:263`；`dsh-session-projection-cache/lib/index.js:34,272` | 事实源、surface、可丢弃 cache 与生命周期 |
| S6 | `dsh-session/lib/types/types.d.ts:408,490`；`dsh-session/lib/types/fork.d.ts:6`；`dsh-subagent/lib/index.js:332,697,1659,2158,2720`；`dsh-subagent/lib/types/descriptor.d.ts:44`；`dsh-subagent/lib/types/catalog.d.ts:19` | seq、父子 / seed、descriptor、catalog 与恢复差别 |
| S7 | `dsh-session-persistence-jsonl/lib/index.js:782,844`；`dsh-llm/lib/types/message.d.ts:30,95,123,134,166`；`dsh-session/lib/types/types.d.ts:289`；`dsh-subagent/lib/types/continuation-messages.d.ts:11`；`dsh-typert-protocol/lib/types/types.d.ts:352`；`dsh-api-remote-access/lib/types/types.d.ts:12,43`；`dsh-api-session-controller/lib/types/types.d.ts:386` | header、来源与角色、peer / principal、临时属性边界 |
