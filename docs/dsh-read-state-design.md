# DSH 读取状态与安全推进接缝

> 2026-11-05。基于安装版 `@deepseek-ai/dsh` `0.2.1-rc.5` 的公开类型、原生 backend 源码及本仓生产调用点。受控运行证据见 [核查记录 §13](dsh-ingest-survey.md#13-后续核查并发窗口与读取状态反例)（`dcf1b50`）。本文是实施设计，不表示 DSH provider、生产凭据或临时排除已实现；执行顺序只在 [backlog.md](backlog.md) 维护。

## 1. 先保留哪几件事实

一次读取应分别回答：取得了什么前缀、来源的哪个版本得到验证、本次是否完整或为什么未完成。三者不能压成一个成功标志或事件数。

| 事实 | 可用证据 | 不能替代它的值 |
| --- | --- | --- |
| 来源与生命周期 | 明确配置的来源实例、逻辑来源、实际选中的物理格式代、header 生命周期与继承边界 | 目录叶子名、同一个 session id、恢复后的逻辑版本号 |
| 已验证前缀 | 同一份读取快照上的事件顺序、完整内容和旧前缀对照，覆盖范围明确 | 事件数、末尾 seq、物理文件大小 |
| 完整性与本次结果 | 该快照的分行 / 帧 / 校验与 admission 结果；变化、损坏、未知代、迁移条件不足、I/O 未知和取消分别保留 | `eventState`、原生 read 没抛错、能构造完整 turn |

DSH 原生 `SessionPersistence.stat()` 已有 `revision`，不是没有版本观察口。公开契约只允许在**同一个 service 实例、同一个 session id** 内比较；相等可用于未变化的派生缓存，不承诺跨实例、跨进程或重启后比较。`SessionPersistence.identity` 是进程内的 symbol，不能原样变成持久化来源身份。JSONL 实现的 revision 来自 dev / ino / size / mtime / ctime，是 best-effort 变化观察，不是内容摘要、完整性诊断或可长期复用的事件游标。

因此 `stat → read → stat` 可以补充读取期间的变化线索；两端 revision 相同仍不能证明没有坏尾，也不能把 read 返回的事件与独立读取的文件哈希自动视为同一快照。正式 reader 必须把内容验证、完整性证据与被验证的字节快照绑定，不能各读一遍后拼出一份“已验证”凭据。

上游出处（相对安装版 `@deepseek-ai/`，查证日期 2026-11-05）：`dsh-session-persistence/lib/types/index.d.ts:22,101,140`、`revision.d.ts:1`；`dsh-session-persistence-jsonl/lib/index.js:2331,2570`。这里的 revision 结论来自直接读源码，未另做跨实例运行实验。

## 2. 当前生产模型能做什么

本仓已有 `AppendLogObservation → CommitPlan → FileProjectionBatch` 的生产路径。CLI 写库成功且计划允许后才保存状态；不可读、主动拒绝毒行、降级但零可用事件会保留旧投影、冻结旧状态。Rollback / Reparse 在同一事务写事件和切头，开新代的 token 提供幂等重放。

这些能力可以复用，但不能替 reader 证明来源：

- `ParseQuality::Clean` 表示交给解析器的完整行没有坏 JSON，可以同时有未消费尾行；它不承诺 DSH 的物理帧完整。
- `Degraded` 且有事件允许写入并推进同步目标，不适合作为“只拿到合法前缀，来源完整性未知”的别名。
- 私有 `ByteSource::read_range(start, end)` 的成功契约是精确返回该范围；现有偏移按被请求范围计算。DSH 返回较短合法前缀时，不能伪装成该接口的普通成功。
- 总库接受显式来源的空 Rollback / Reparse，能实际切到空头；保留旧代不等于当前投影不会被不完整结果替换。
- `Cursor.next_seq` 是归一化事件序号，当前 `safe_offset` 是既有来源的逻辑字节位置；它们不能直接装入 DSH 原生事件位置或压缩 offset。

依据：[scan.rs](../src/scan.rs)、[observation.rs](../src/observation.rs)、[scan_plan.rs](../src/scan_plan.rs)、[svault.rs](../src/bin/svault.rs) 的 `scan_one` / `ScanWriter::commit` / `run_scan_all`；总库行为见 [store.rs](../src/store.rs) 的 `apply_projection`。这些是当前实现事实；新 DSH reader 尚未接入这些生产出口。

## 3. 读取范围与历史基线分开

“这次从 0 读”是读取意图；“上次消费了哪些字节”是历史证据。解析器过期、归属过期或同步欠账要求全读，不能因此丢掉旧 safe_offset、mtime 与覆盖长度明确的内容指纹。

修复前，CLI 的解析器刷新会传 `cursor_in=None`，另行保留旧全文指纹；但当当前全文短于旧指纹覆盖长度时，`prefix_differs_from()` 把缩短判断交回 stat 层，而 stat 面对的已经是新游标。真实 CLI 反例已复现：源截短进入 Reparse，而不是 Rollback。若新短文件反而产出更多事件，旧投影可被当作同一份来源的旧解析回收，失去原始来源版本。

本轮修复在 CLI 私有 `scan_one` 中保留旧**已消费范围与 mtime**作为全读的独立对照，并在可用观察进入提交计划前识别旧消费范围缩短或时间倒退。不改公开扫描函数、Cursor 或 schema。不能简单以 `new_size < fingerprint.covered_len` 认回退：指纹还可能覆盖未换行的尾巴，只删除未消费尾巴不应因此开新源版本。读取失败仍冻结，不把新建游标当成旧状态。

对应反例走真实 CLI、隔离来源 / 总库 / 钥匙文件和状态保存路径，不能只手造 `AppendLogObservation`：

1. 旧长文件 1 个事件，来源缩短为 2 个事件，同时制造已记录解析器版本不匹配；必须判 Rollback，当前读只见新事件，all 仍保留旧源事件。
2. 同时发生正常追加与解析器刷新；应为 Reparse，不能误开 Rollback。
3. 只移除未消费半行并刷新；应保持同一源版本。
4. 总库写入成功后恢复旧状态文件再跑；不得重复留代或重复事件。
5. 旧前缀一致且文件变长，但 mtime 倒退；仍应按原有时间回退判据走 Rollback。

可复跑入口为 [cli_scan_baseline_it.rs](../tests/cli_scan_baseline_it.rs)，使用项目 `target` 的新建目录、独立文件钥匙和公开 scan-all / pull 命令，不使用 acceptance 注入或直接修改 SQLite。只改本次合成状态文件中的 parser_revision 来触发生产全读分支；不修改全局配置或真实解析器版本。运行物保留，不递归清理。

```sh
CARGO_TARGET_DIR=target/dsh-read-state cargo test --features store --test cli_scan_baseline_it
```

2026-11-05，Windows 实跑：修复前的三组测试中，缩短反例报 `reparse` 而非 `rollback`，其余两组对照通过；修复后补入 mtime 倒退，共四组全部通过，第一组同时验证写库后恢复旧状态的重放。随后 429 个库测试、28 个 CLI 单元测试、默认 / store 两种 bin 构建及零警告 clippy 均通过。真实 CLI 进程只摄取合成正文、读写项目内测试库，并把 Claude / Codex 本机配置根指向本次沙箱路径；未摄取真实日志或写真实总库。发布 CLI 的 `--only` 在发现之后过滤，仍可能执行宿主 / WSL 发现与挂载探测；本测试不证明发现阶段完全隔离。

2026-11-06 重跑上述提交门全部通过，修复已提交为 `368b730`，尚未合并发布。这项修复只补当前 CLI 的基线接线，不使 DSH 读取完整性自动成立，也不改变既有一次性坏行策略。

## 4. DSH 首次接入采用保守提交条件

建议内部 reader 到观察 / 规划之间先保留独立的读取结果，构造权归实际读取与校验路径；不提供可由调用方随手传入 `complete=true` 的证明。首版只在证据足够覆盖目标投影时放行。

| 本次结果 | 首版提交行为 | 后续仍需的证据 |
| --- | --- | --- |
| 不支持的代、迁移条件不足、损坏、I/O 未知、取消 | 不写新投影，不推进旧凭据；分别报告原因 | 支持策略或重试后取得的新结果 |
| 合法前缀，物理完整性未知；读取期间来源变化 | 保留既有投影与凭据，不标已追平 | 与同一快照绑定的完整性及来源版本验证 |
| 修尾期间短于旧已验证前缀 | 保留既有投影与凭据，重新观察 | 不能仅凭缩短或错误文案认定永久回退 / 永久损坏 |
| 同一来源 / 生命周期 / 格式代，旧前缀内容保持，新增部分已验证 | 证据齐备后才允许增量提交 | 原生位置与归一化事件位置的明确映射 |
| 已确认内容改写、生命周期或格式代变化 | 使旧推进凭据失效；证据足够时重建目标投影 | 来源版本和解析版本的区分、跨代支持策略及旧读者兼容 |

未来若要先持久化可验证前缀，必须另外表达“前缀已提交”和“来源已完整追平”，再核游标 / 同步目标的推进语义；当前 `CommitPlan` 对可用降级批次的 Advance 不能直接承担这两种水位。

生产持久化凭据至少绑定：来源实例与逻辑来源、会话生命周期、物理格式代、已验证前缀范围与内容证据、解析版本。物理尾部可被修复或重编码，不能把旧物理 offset 当成永久水位。现有幂等 token 解决提交重放，不证明这些来源事实。

## 5. 下一步完成判据

- v4 明文 / zstd 的独立快照验证路线见 §6；继续把活跃来源的选代、生命周期与变化观察绑定到取得的快照。已冻结夹具通过不证明真实并发来源的快照捕获安全；原生公开 read 缺少的诊断不能只读取私有状态或复制字段名冒充证据。
- 将实际 reader 的阻断 / 已验证结果接入观察前的提交门；未知完整性不得落成既有 Clean / Degraded 后继续推进。
- 同事件数改写、双追加旧前缀、修尾中间态、读取后发布未来代和写库后未保存状态的重放，须经过实际拟接入路径核验。
- 再决定是否需要公开类型、持久化字段或 schema 变化；改动时按四类消费者及旧读者 / 并存写者逐项验证。当前没有理由先给公开 Cursor 加字段，也不把一次内存模型通过称作生产验收。
- H.1 可信载体、统一临时排除、用量保真、复杂语义与首版支持范围仍是正式适配的独立实施门。

## 6. 独立快照完整性验证

2026-11-06 新增 [check-dsh-snapshot.py](../scripts/check-dsh-snapshot.py) 与 [verify-dsh-snapshot.mjs](../scripts/verify-dsh-snapshot.mjs)。Python 工具使用固定的 `zstandard==0.25.0`，每个文件只读一次，摘要、分行和逐帧检查全部使用这份不可变 bytes；不复制上游私有帧扫描器，也不读取 backend 私有恢复状态。它只判断字节与 JSONL 层，`complete` 不包含 DSH header / 事件语义的接受结论。

zstd 路线按标准帧依次解码，以 `eof` 和 `unused_data` 确定完整帧及剩余输入，解码器负责校验 checksum。只把完整且已通过校验的帧计入已验证范围；恢复出了全部文本但没有帧尾，仍为 `incomplete`。每帧必须结束于 JSONL 记录边界，第一帧只能含一条 header 记录，UTF-8 使用严格解码。空事件帧可保留既有边界，空 header 帧不能通过。无 checksum 的帧暂报 `unsupported/checksum_absent`，这是本原型的保守范围，不表示上游日志必然损坏；帧参数不足或不合法尚未细分时保留 `unverified`，不武断报永久损坏。读取失败、预算用尽与空输入分别报告，不给未读完的文件补一个全文 hash。

Node 工具在 `target/dsh-snapshot-*` 的全新目录排他写入合成快照，批量调用 Python 审计，然后让原生 persistence 只读打开同一副本，并经过原生 restore / detached Session admission。审计摘要必须匹配该副本，原生读取后再次核字节未变；v4 的物理记录数须覆盖 header 与全部事件，不能以合法短前缀的 restore 成功放行整份快照。合法的 JSON 空白不要求重编码后逐字相同。这里的同副本保证来自受控夹具没有并发 writer；两端 hash 相同本身不能替代活跃来源的原子快照或防 ABA 证据。

2026-11-06，29 组快照 / 原生读取与证据绑定检查全部通过（Windows，DSH `0.2.1-rc.5`、Node `v25.2.1`、`zstandard` `0.25.0`）。核心对照：

| 合成来源 | 原生观察 | 独立证据与约束 |
| --- | --- | --- |
| 完整 v4 明文 / 逐帧 zstd | 均返回 10 事件 | 字节、JSONL、原生 admission 与记录覆盖同时满足，才在本原型中认该副本已验证 |
| zstd 最后一个 checksum 字节缺失 | 仍返回全部 10 事件，restore 可接受 | 标 `incomplete/torn_frame`；最后一个完整帧结束于 1572，当前文件 1661 字节，不能推进到物理 EOF |
| 正文字符串内损坏一个 UTF-8 字节 | 两种编码仍返回 10 事件，restore 可接受 | 严格 UTF-8 审计拒绝；不能把替换字符解码当作来源原文保真 |
| JSONL 完整但包含未知必需事件，或 tool/result 指向未出现的 call | 原生 read 分别报不支持 / 损坏 | 物理完整不能替代格式与语义 admission |
| 在完整 zstd 日志后追加一个新事件，但缺最后一个 checksum 字节 | 原生 read 与 restore 可接受全部 11 事件 | 仍标 `incomplete`，已验证物理范围只覆盖追加前的完整帧 |
| 同事件数正文改写 | 新内容也能通过当前副本验证 | 事件内容与物理摘要均改变，旧证据不能借相同事件数复用 |
| 保留单独 header 帧，把事件重打包为一帧 | 仍为相同 10 事件 | 物理摘要改变，解压后的原始 JSONL 摘要不变；物理 offset 不是长期逻辑位置 |
| 把旧副本审计结果用于改写后的副本 | 在原生 read 前阻断 | 报 `snapshot_mismatch`，不能拼接两次读取的证据 |

自测逐一截断第二帧的 45 个非空切点，均不能得到 `complete`，另逐个破坏四个 checksum 字节。完整帧边界上的独立前缀作为单独快照可以完整，这仍不等于来源已追平。附加对照还覆盖坏 JSON、完整帧内未换行记录、header 混帧、下一帧 magic 截断、无 checksum 及未来物理代；具体断言维护在可复跑脚本中。

```sh
uv run scripts/check-dsh-snapshot.py --self-test
node scripts/verify-dsh-snapshot.mjs --dsh-package '<dsh-package.json>'
```

该工具不启动 Host / LLM，不写真实日志或总库，运行物保留。报告明确 `frozen_fixture_snapshots_validated=true`、`live_source_capture_validated=false`、`production_advance_gate_integrated=false` 与 `provider_registered=false`。这是独立诊断路线的运行证据，不是已发布能力，也不是可直接持久化的生产推进凭据。

下一步先在拟接入的实际读取路径取得快照，同时保留所选物理代、header 生命周期、明确来源实例与读取期间变化；再把已验证字节到原生事件位置的映射、旧前缀对照和阻断状态接入扫描 / 提交门。当前未改公开 Cursor、schema 或 SourceType，未把试验性状态塞进 Clean / Degraded。
