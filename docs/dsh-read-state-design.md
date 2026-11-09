# DSH 读取状态与安全推进接缝

> 2026-11-06 更新。基于安装版 `@deepseek-ai/dsh` `0.2.1-rc.5` 的公开类型、原生 backend 源码及本仓生产调用点。受控运行证据见 [核查记录](dsh-ingest-survey.md) §13–§18；独立快照审计、Node 捕获、Rust 字节 / 原生位置、原生 admission 及 Rust 来源选代 / 保守提交门见本文 §6–§10。本文不表示 DSH provider、生产凭据或临时排除已实现；执行顺序只在 [backlog.md](backlog.md) 维护。

## 1. 先保留哪几件事实

一次读取应分别回答：取得了什么前缀、来源的哪个版本得到验证、本次是否完整或为什么未完成。三者不能压成一个成功标志或事件数。

| 事实             | 可用证据                                                                                            | 不能替代它的值                                    |
| ---------------- | --------------------------------------------------------------------------------------------------- | ------------------------------------------------- |
| 来源与生命周期   | 明确配置的来源实例、逻辑来源、实际选中的物理格式代、header 生命周期与继承边界                       | 目录叶子名、同一个 session id、恢复后的逻辑版本号 |
| 已验证前缀       | 同一份读取快照上的事件顺序、完整内容和旧前缀对照，覆盖范围明确                                      | 事件数、末尾 seq、物理文件大小                    |
| 完整性与本次结果 | 该快照的分行 / 帧 / 校验与 admission 结果；变化、损坏、未知代、迁移条件不足、I/O 未知和取消分别保留 | `eventState`、原生 read 没抛错、能构造完整 turn   |

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

| 本次结果                                                     | 首版提交行为                             | 后续仍需的证据                                     |
| ------------------------------------------------------------ | ---------------------------------------- | -------------------------------------------------- |
| 不支持的代、迁移条件不足、损坏、I/O 未知、取消               | 不写新投影，不推进旧凭据；分别报告原因   | 支持策略或重试后取得的新结果                       |
| 合法前缀，物理完整性未知；读取期间来源变化                   | 保留既有投影与凭据，不标已追平           | 与同一快照绑定的完整性及来源版本验证               |
| 修尾期间短于旧已验证前缀                                     | 保留既有投影与凭据，重新观察             | 不能仅凭缩短或错误文案认定永久回退 / 永久损坏      |
| 同一来源 / 生命周期 / 格式代，旧前缀内容保持，新增部分已验证 | 证据齐备后才允许增量提交                 | 原生位置与归一化事件位置的明确映射                 |
| 已确认内容改写、生命周期或格式代变化                         | 使旧推进凭据失效；证据足够时重建目标投影 | 来源版本和解析版本的区分、跨代支持策略及旧读者兼容 |

未来若要先持久化可验证前缀，必须另外表达“前缀已提交”和“来源已完整追平”，再核游标 / 同步目标的推进语义；当前 `CommitPlan` 对可用降级批次的 Advance 不能直接承担这两种水位。

生产持久化凭据至少绑定：来源实例与逻辑来源、会话生命周期、物理格式代、已验证前缀范围与内容证据、解析版本。物理尾部可被修复或重编码，不能把旧物理 offset 当成永久水位。现有幂等 token 解决提交重放，不证明这些来源事实。

## 5. 下一步完成判据

- v4 明文 / zstd 的独立快照验证见 §6，Node 实际捕获函数与受控窗口见 §7，Rust 字节与原生位置核验见 §8，Rust 原生语义 admission 接缝见 §9，Rust 会话目录选代、同句柄文件身份捕获、完整旧前缀对照与观察前保守提交门见 §10。两端的同句柄复查均不证明任意 ABA 已排除。
- 接下来将 `CommitGateDecision` 的已验证候选映射到 `RawEvent` 归一化位置，并接入生产 `AppendLogObservation` / `CommitPlan`；未知完整性不得落成既有 Clean / Degraded 后继续推进。明确快照验证与生产推进之间所需的稳定性条件。
- 同事件数改写、双追加旧前缀、修尾中间态、读取后发布未来代和写库后未保存状态的重放，须经过生产读取 / 写库 / 状态保存路径核验。§7 与 §10 的旧快照保留仅在 reader 内存与提交门决策中成立，尚未验证 DSH 总库投影事务。
- 再决定是否需要公开类型、持久化字段或 schema 变化；改动时按四类消费者及旧读者 / 并存写者逐项验证。当前没有理由先给公开 Cursor 加字段，也不把一次内存模型通过称作生产验收。
- H.1 可信载体、统一临时排除、用量保真、复杂语义与首版支持范围仍是正式适配的独立实施门。

## 6. 独立快照完整性验证

2026-11-06 新增 [check-dsh-snapshot.py](../scripts/check-dsh-snapshot.py) 与 [verify-dsh-snapshot.mjs](../scripts/verify-dsh-snapshot.mjs)。Python 工具使用固定的 `zstandard==0.25.0`，每个文件只读一次，摘要、分行和逐帧检查全部使用这份不可变 bytes；不复制上游私有帧扫描器，也不读取 backend 私有恢复状态。它只判断字节与 JSONL 层，`complete` 不包含 DSH header / 事件语义的接受结论。

zstd 路线按标准帧依次解码，以 `eof` 和 `unused_data` 确定完整帧及剩余输入，解码器负责校验 checksum。只把完整且已通过校验的帧计入已验证范围；恢复出了全部文本但没有帧尾，仍为 `incomplete`。每帧必须结束于 JSONL 记录边界，第一帧只能含一条 header 记录，UTF-8 使用严格解码。空事件帧可保留既有边界，空 header 帧不能通过。无 checksum 的帧暂报 `unsupported/checksum_absent`，这是本原型的保守范围，不表示上游日志必然损坏；帧参数不足或不合法尚未细分时保留 `unverified`，不武断报永久损坏。读取失败、预算用尽与空输入分别报告，不给未读完的文件补一个全文 hash。

Node 工具在 `target/dsh-snapshot-*` 的全新目录排他写入合成快照，批量调用 Python 审计，然后让原生 persistence 只读打开同一副本，并经过原生 restore / detached Session admission。审计摘要必须匹配该副本，原生读取后再次核字节未变；v4 的物理记录数须覆盖 header 与全部事件，不能以合法短前缀的 restore 成功放行整份快照。合法的 JSON 空白不要求重编码后逐字相同。这里的同副本保证来自受控夹具没有并发 writer；两端 hash 相同本身不能替代活跃来源的原子快照或防 ABA 证据。

2026-11-06，29 组快照 / 原生读取与证据绑定检查全部通过（Windows，DSH `0.2.1-rc.5`、Node `v25.2.1`、`zstandard` `0.25.0`）。核心对照：

| 合成来源                                                       | 原生观察                                | 独立证据与约束                                                                                |
| -------------------------------------------------------------- | --------------------------------------- | --------------------------------------------------------------------------------------------- |
| 完整 v4 明文 / 逐帧 zstd                                       | 均返回 10 事件                          | 字节、JSONL、原生 admission 与记录覆盖同时满足，才在本原型中认该副本已验证                    |
| zstd 最后一个 checksum 字节缺失                                | 仍返回全部 10 事件，restore 可接受      | 标 `incomplete/torn_frame`；最后一个完整帧结束于 1572，当前文件 1661 字节，不能推进到物理 EOF |
| 正文字符串内损坏一个 UTF-8 字节                                | 两种编码仍返回 10 事件，restore 可接受  | 严格 UTF-8 审计拒绝；不能把替换字符解码当作来源原文保真                                       |
| JSONL 完整但包含未知必需事件，或 tool/result 指向未出现的 call | 原生 read 分别报不支持 / 损坏           | 物理完整不能替代格式与语义 admission                                                          |
| 在完整 zstd 日志后追加一个新事件，但缺最后一个 checksum 字节   | 原生 read 与 restore 可接受全部 11 事件 | 仍标 `incomplete`，已验证物理范围只覆盖追加前的完整帧                                         |
| 同事件数正文改写                                               | 新内容也能通过当前副本验证              | 事件内容与物理摘要均改变，旧证据不能借相同事件数复用                                          |
| 保留单独 header 帧，把事件重打包为一帧                         | 仍为相同 10 事件                        | 物理摘要改变，解压后的原始 JSONL 摘要不变；物理 offset 不是长期逻辑位置                       |
| 把旧副本审计结果用于改写后的副本                               | 在原生 read 前阻断                      | 报 `snapshot_mismatch`，不能拼接两次读取的证据                                                |

自测逐一截断第二帧的 45 个非空切点，均不能得到 `complete`，另逐个破坏四个 checksum 字节。完整帧边界上的独立前缀作为单独快照可以完整，这仍不等于来源已追平。附加对照还覆盖坏 JSON、完整帧内未换行记录、header 混帧、下一帧 magic 截断、无 checksum 及未来物理代；具体断言维护在可复跑脚本中。

```sh
uv run scripts/check-dsh-snapshot.py --self-test
node scripts/verify-dsh-snapshot.mjs --dsh-package '<dsh-package.json>'
```

该工具不启动 Host / LLM，不写真实日志或总库，运行物保留。报告明确 `frozen_fixture_snapshots_validated=true`、`live_source_capture_validated=false`、`production_advance_gate_integrated=false` 与 `provider_registered=false`。这是独立诊断路线的运行证据，不是已发布能力，也不是可直接持久化的生产推进凭据。

本节冻结副本核验已提交为 `6b2a0b3`，尚未合并发布；活跃来源捕获原型另见 §7。生产接线仍需已验证字节到原生事件位置的映射、持久化旧前缀证据和阻断状态到扫描 / 提交门的接合。当前未改公开 Cursor、schema 或 SourceType，未把试验性状态塞进 Clean / Degraded。

## 7. 活跃来源的实际捕获原型

2026-11-06 新增 [dsh-snapshot-reader.mjs](../scripts/dsh-snapshot-reader.mjs)、显式只读入口 [read-dsh-snapshot.mjs](../scripts/read-dsh-snapshot.mjs) 与 [verify-dsh-capture.mjs](../scripts/verify-dsh-capture.mjs)。这是使用固定安装版原生 codec 的 Node 实验 reader，不是 Rust provider 或正式 `svault` 子命令。它不调用原生 read 后再另读一份文件拼接证据，而是在真实文件句柄取得的私有 Buffer 上完成物理审计与语义解码。

捕获顺序：

1. 通过同一原生 service 的 `stat` 观察 header、revision、物化大小与格式状态。`locate(header).path` 只用于定位当前格式的目标目录，不能据此认定实际选中了该代；对目录再用公开文件名 parser 选数字最高的规范代，拒绝冲突编码、非 regular 文件及本原型不支持的代。
2. 对所选路径 `lstat`，打开只读句柄，再用同句柄 `fstat` 核 dev / ino / size / 纳秒 mtime / ctime。字段不可得就阻断；按初始大小有界读取，短读、取消和前后身份变化均阻断。关闭句柄后，再核路径身份、原生 metadata 与目录选代。
3. 将取得的同一 Buffer 经 stdin 传给 Python `--bytes --compression` 审计，不让审计器重开来源。审计返回完整通过校验的帧范围；Node 按这些范围解压原 Buffer，核物理摘要、完整覆盖及原始 JSONL 摘要，再严格解码 UTF-8 / JSON，用原生 codec、restore 与 detached Session 做 admission，核 header / id / 物理代和记录覆盖。
4. 再观察一次来源并核取消，才在 reader 私有 `WeakMap` 中登记 receipt。输入、解码、审计超时与输出预算共用 [dsh-snapshot-limits.json](../scripts/dsh-snapshot-limits.json)，不在多个工具中各留一份阈值。

`capture()` 成功只返回不可变的 `{kind:"verified_snapshot"}` 句柄；字节、事件、绑定与观察证据留在闭包。`describe()` 只导出摘要，`events()` 返回深复制。手工对象、JSON 往返副本、CLI 摘要及其他 reader 的 receipt 都不能被本 reader 当作有效句柄。这里约束的是 API 构造与引用归属，不是在同一 JS 进程中隔离恶意代码。

来源绑定包括调用方明确提供的 `sourceInstance`、规范目录、session id、压缩格式及物理代；生命周期另用原生 header 编码和 inherited cut 计算。`sourceInstance` 是配置标识，尚非已认证主机身份或跨重启生产凭据。`compare()` 依次区分不同来源、生命周期变化、旧前缀缩短、内容改写与前缀保持，内容比较覆盖完整事件及顺序；重新分帧而事件不变仍为前缀保持。`recheck()` 只再观察文件身份、metadata 与选代，不重读正文，也不与后续写库原子绑定。

Windows、Node `v25.2.1`、DSH `0.2.1-rc.5`、`zstandard` `0.25.0` 实跑 36 组全部通过；每种编码 17 组，另有重分帧和输入预算两组：

| 实际路径上的对照                                        | 结果与边界                                                                                                                              |
| ------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------- |
| 稳定读取、只读 CLI、不可伪造引用与导出副本隔离          | 取得完整 10 事件、来源字节未变；伪造引用均拒绝                                                                                          |
| 原生追加、同事件数改写、header 生命周期变化             | 分别为前缀保持、`prefix_rewritten`、`lifecycle_changed`；旧 receipt 不因失败或新读取而被改写                                            |
| 真实句柄读完、返回调用方之前发生追加 / 同长度原地覆盖   | 两种编码均阻断为 `changed`；追加后下一轮可取得完整新快照                                                                                |
| 关闭读取句柄后，另一进程两次替换、最终字节恢复          | 最终摘要相同仍因文件身份变化阻断；只覆盖此受控替换，不能排除 metadata 不可见的任意 ABA                                                  |
| 读取中发布 v5，或有效 receipt 取得后才发布 v5           | 本轮捕获或后续 recheck 均报不支持，未静默回退到 v4                                                                                      |
| 断尾、未知必需事件、缺 metadata、注入 EIO、取消、超预算 | 分别保留阻断结果，不构造空成功；EIO 通过公开 open 注入，非实机磁盘故障                                                                  |
| 原生修尾已 truncate、恢复记录未写入                     | JSONL 中间快照 10 事件、前缀保持；zstd 为 9 事件、`prefix_shrank`。完成恢复与追加后均为 16 事件，旧前缀保持；中间快照也没有生产推进许可 |
| zstd 事件重新分帧                                       | 物理摘要改变，JSONL 摘要及事件前缀保持                                                                                                  |

验证复用 [io-harness.mjs](../tests/fixtures/dsh-v4/io-harness.mjs) 的公共 I/O 屏障与独立进程回执，每个窗口断言实际命中；操作只落本轮 `target/dsh-capture-*` 全新合成目录。保留运行物，不读真日志或写总库，不启动 Host / LLM。相同 helper 提取后，原生并发 21 组与冻结快照 29 组回归均通过。

```sh
node scripts/verify-dsh-capture.mjs --dsh-package '<dsh-package.json>'
node scripts/read-dsh-snapshot.mjs --dsh-package '<dsh-package.json>' --root '<absolute-existing-root>' --id '<session-id>' --compression zstd --source-instance '<configured-source>'
```

只读入口要求根已存在、为绝对目录且非符号链接，不创建来源根；成功仅输出摘要（退出 0），阻断输出状态（退出 2），基础设施 / 参数异常为退出 1。入口不输出正文，不连接总库。

审计取消另做了真实 `uv → Python` 进程链对照：Windows 只 kill uv 时，合成 Python 仍运行到三秒后的自然结束；因此不再使用 spawn 的 `signal` 自动先杀 wrapper。取消、审计超时和输出超限均走统一终止路径，Windows 对本次子进程 PID 用 `taskkill /T /F`，POSIX 启动独立进程组后终止该组；不会按进程名处理其他运行物。清理另有共享配置中的截止时间，未确认结束时返回 `unavailable/auditor_cleanup_unconfirmed`，不被取消状态掩盖。新增两编码反例从 Python 的 ready 屏障触发取消，并断言它未走到自然结束标记，随后重新捕获成功。WSL 的 Node `v12.22.9` 只验证了进程组终止原语；该环境没有 uv，未运行完整 reader，不将其算作 WSL 捕获验收。

报告明确 `controlled_capture_windows_validated=true` 与 `same_buffer_validation_and_decode=true`，同时保留 `general_stress_validated=false`、`arbitrary_aba_excluded=false`、`production_advance_gate_integrated=false`、`provider_registered=false`。每份成功摘要也明确 `atomic_source_snapshot_proven=false` 与 `production_advance_permit=false`：前后多次观察不能保证期间没有任意 ABA，`matches_observation` 更不能证明未来提交时来源仍稳定。本节捕获与进程树取消修复已提交为 `86c34aa`，尚未合并发布；不把 reader 内存 receipt 序列化为生产游标。后续 Rust 字节与原生位置核验见 §8，生产提交与持久化重放仍待接线。

## 8. Rust 同缓冲区物理核验与原生记录位置

2026-11-06 新增显式 feature `dsh-experimental`、[dsh_snapshot.rs](../src/dsh_snapshot.rs) 与只读 [example](../examples/dsh_snapshot.rs)。默认构建不开放该模块。入口要求显式绝对文件路径与压缩格式，不发现来源、不创建来源目录，也不注册正式 provider 或 `svault` 子命令。

实际路径为 `probe::with_reader → 有界 Vec → compressed::checked_frames → 严格 UTF-8 / JSONL → 最小 v4 envelope 与位置表`。只打开一次，输入预算加一字节区分完整读取和超限；摘要与验证来自同一份字节。zstd 复用 `compressed.rs` 原有唯一帧头解析与解码器，在分配解码窗口前核窗口 / 声明长度预算，逐帧核内容长度和 checksum。第一帧只含 header，每帧结束于记录边界，允许空事件帧。输入 / 解码预算仍只取共享 `dsh-snapshot-limits.json`，没有新增依赖。

结果 `PhysicalSnapshot` 的字段私有，只有实际核验路径能构造；`Blocked` 用穷举 enum 保留 unavailable、incomplete、unverified、unsupported、budget_exceeded、invalid 与 rejected。坏尾与 checksum 解码失败尚未全部精确分类，统一保留未核结论时返回 `unverified`，不会猜成永久损坏或空会话。读取未完成、来源打不开与超预算均不输出全文摘要或成功位置表。本节初版（`9caf18b`）核验完成只保留摘要与位置；后续 §9 为原生 admission 保留私有已验证 JSONL。两阶段均不导出原始 header / 正文，也未提供摄取重放内容。

| 输出位置 / 摘要                                    | 含义与限制                                                                                            |
| -------------------------------------------------- | ----------------------------------------------------------------------------------------------------- |
| `record_index`、`native_seq`                       | header 占第 0 条记录，事件原生 seq 从 0 连续；不是 `RawEvent.seq` 或 `Cursor.next_seq`                |
| `jsonl_start/end`                                  | 解压后 UTF-8 字节范围，包含该行末尾 LF；CRLF 的 CR 也计入，不按字符数算                               |
| `frame_index`、`frame_spans`                       | 对应物理帧与该帧解压后的字节范围；明文的事件 frame_index 为 null，空帧不占事件序号                    |
| `record_fingerprint`                               | 复用现有 `EventKey::fingerprint_of`，对 trim 后原记录截断摘要；没有构造 EventKey、RawEvent 或生产身份 |
| `snapshot_sha256`、`jsonl_sha256`、`header_sha256` | 物理文件、原始 JSONL 与含 LF 的原 header 行摘要；header 摘要没有规范化，不是已验证生命周期凭据        |

[verify-dsh-rust-snapshot.mjs](../scripts/verify-dsh-rust-snapshot.mjs) 用原生 v4 codec 从共享 artifact 生成全新合成文件，加入中文正文与 JSON 空白；Python `zstandard==0.25.0` 独立审计同副本，Rust example 实际打开核验，再与 Node 按审计帧范围解码所得的每条位置、摘要和原生 admission 对拍。上游 v4 物理编码出处为安装版 `dsh-session-format-v3-to-v4/lib/index.js:1057–1099`，查证日期 2026-11-06。未知必需事件与断裂 call/result 可以通过本轮 Rust 物理核验、同时被原生 admission 拒绝；脚本明确断言成功摘要的 `semantic_admission_validated=false`，不把最小 envelope 检查称作语义接入。

Windows 实跑 136 份快照，另有重分帧位置保持、默认省略位置表、缺路径不建目录及目录不能冒充空会话四组对照。最后一帧逐一截断 89 个非空切点，全部阻断；四个 checksum 字节分别破坏也全部阻断。无 checksum、字典、超大窗口、声明 / 流式解码超预算、跨帧半行、混合 header 帧、坏 UTF-8、重复 / 缺失序号与未来 header 均覆盖。重分帧后原始 JSONL 摘要和原生记录字节位置相同，物理摘要与 frame_index 改变。

```sh
CARGO_TARGET_DIR=target/dsh-read-state cargo build --example dsh_snapshot --features dsh-experimental
node scripts/verify-dsh-rust-snapshot.mjs --dsh-package '<dsh-package.json>' --rust-reader '<absolute-example-binary>'
cargo run --example dsh_snapshot --features dsh-experimental -- --path '<absolute-snapshot>' --compression zstd --positions
```

WSL 另用 Linux 工具链编译 example，并由 Windows Node / Python 生成、审计新副本后通过 `wsl.exe --exec` 调用该二进制，136 份快照及上述四组对照也全部通过。`--wsl-reader` 与 `--rust-reader` 二选一；脚本只对自身新建的夹具根调用 `wslpath`，不换算消费方会话路径。Git Bash 调用时禁用参数路径改写：

```sh
MSYS_NO_PATHCONV=1 node scripts/verify-dsh-rust-snapshot.mjs --dsh-package '<dsh-package.json>' --wsl-reader '<absolute-linux-example-binary>'
```

这是 Linux Rust 读 Windows 盘上合成文件的验收，不是完整 Linux DSH backend / Node 捕获验收。`9caf18b` 提交时：Windows `cargo test --lib --features store,dsh-experimental` 为 432 passed，默认 / store bin、experimental all-target 编译与 all-feature clippy 均通过；Linux `cargo test --lib --features dsh-experimental` 为 256 passed，store + experimental 的 bin 编译通过，而 Linux 全库 store 测试当时为 416 passed / 1 failed（`open_existing_returns_the_store_when_it_is_there` 在建库准备阶段默认访问系统钥匙链，因无桌面 Linux 缺少 `org.freedesktop.secrets` 服务失败；后续本批已在 [store.rs](../src/store.rs) 改用显式测试钥匙文件修复并全绿，见 §9）。最后只调整静态错误值的 `ok_or` 写法，两平台对应模块测试与最终 example 对拍再过。

现有 Rust 消费方另外用 Cargo 临时 `paths` 覆盖指向本仓当前开发源码，先由 metadata 正向核依赖实际落点，再跑 `cargo check --locked --lib`，结果通过；未改消费方 manifest、lock 或子模块指针。这里验证其既有 store 用法，不表示它启用了 experimental feature。CLI、Rust 既有构造 / 穷举及逐字段读取、`discover` 的 SourceMode 过滤没有新增值域或字段；没有改 Cursor、RawEvent、schema 或正式 CLI。

只读入口成功输出 `physically_validated`、退出 0；阻断输出类型与原因、退出 2；路径参数或输出失败为非零。`--positions` 才输出逐记录位置。本节初版（`9caf18b`）尚无 Node §7 的同句柄身份、选代、来源实例 / 生命周期与旧前缀核对，也没有 codec / restore admission、归一化位置或写库门；后续原生 admission 接缝见 §9。物理模式输出固定保留 `source_observation_validated=false`、`normalized_positions_available=false`、`atomic_source_snapshot_proven=false`、`production_advance_permit=false`。对拍只使用没有并发 writer 的新建副本，不能据此前后摘要相同推断来源原子性。下一步仍是这些证据与生产观察 / 提交门的接合，而不是把物理成功包装成 Clean / Degraded。

## 9. Rust 原生 admission 接缝

2026-11-06，在 §8 物理核验后新增 [NativeRuntime](../src/dsh_snapshot/native.rs) 和共享 [dsh-native-admission.mjs](../scripts/dsh-native-admission.mjs)。`PhysicalSnapshot` 现在保留实际核验过的私有 JSONL；`Debug` 仅显示其长度。调用 `NativeRuntime::admit` 消费该快照，正文只经 stdin 交给显式 Node / DSH 安装。子进程没有来源文件路径，不能重开来源混用第二次读取的证据。原生严格 codec、restore 与 detached Session 仍复用安装版实现，Rust 不重写事件 schema。

安装约束、共享 admission 脚本与限额配置通过 `include_str!` 嵌入，不依赖仓库脚本的运行时位置，不嵌入编译机家目录。Node 和 package.json 必须是显式绝对路径，支持版仍为 `0.2.1-rc.5`；移除继承的 `NODE_OPTIONS` / `NODE_PATH`，不自动发现或安装工具。这里依赖可信的本地运行时配置：不是对恶意 Node / SDK 的认证或隔离。

成功响应必须同时满足退出 0、JSONL / 原 header 摘要相符、事件数覆盖完整位置表、继承边界不越界、两个原生摘要形状正确及版本非空，才能构造字段私有的 `AdmittedSnapshot`。输出摘要不能反序列化为该凭据。错误 JSON、缺字段、错绑快照 / header / 覆盖范围、错误退出状态均保留阻断结果。成功报告为 `native_admitted_snapshot`、`semantic_admission_validated=true`；未给原生参数时仍走原有 `physically_validated` 模式。

执行预算取调用方 Deadline 剩余值与共享单次上限的较小值，spawn 前再检查；stdout、stderr 单流及总量均有上限。超时、管道失败或单流超限触发本次 PID 的 Windows 进程树 / Unix 独立进程组终止。终止命令成功、子进程退出且三条管道结束才返回原失败分类；收尾无法确认时改报 `unavailable/native_cleanup_unconfirmed`，不以 timeout 掩盖。日志和返回值不携带正文或原始 stderr。

同轮修正 Node 捕获的生命周期摘要：之前只 hash `codec.encodeHeader(header, inheritedEventCount)`，但安装版 v4 编码器只验证 cut，不把它写进物理 header，因此没有实际绑定继承边界。现在唯一出口 `nativeLifecycleHash` 同时 hash 编码后的 header 和 cut，Node 捕获与 Rust 原生 helper 共用。实际合法反例的逻辑 / 编码 / 物理 header 全相同，旧事件前缀完整保留，cut 为 10 / 17、事件数为 17 / 24；两编码的 capture / compare 都报 `lifecycle_changed`。捕获矩阵由 §7 当时的 36 组增至 38 组并全部通过。

本轮 Windows / WSL Linux 的原生模式各通过 140 份快照和五组附加对照：在 §8 的 136 份基础上加入两编码各两份 seeded 样本，核实同 header 不同 cut 得到不同原生生命周期摘要。未知必需事件、断裂 call/result 两编码共四份在 Rust 实际入口被原生 admission 拒绝；纯物理模式仍接受它们且 semantic 标志为 false，Windows 同一矩阵回归通过。独立 Python 审计、逐记录位置及帧位置比较继续保留。

[verify-dsh-native-admission.mjs](../scripts/verify-dsh-native-admission.mjs) 另在每个平台通过 17 组失败 / 收尾检查：模块加载边界上的合成 SDK 制造错配响应、无效 JSON、字段缺失、退出状态不符、单流 / 总输出超限、缺 Node / 安装、版本不符及已过期预算；真实成功由上面的安装版矩阵负责。超时夹具先写本次随机 ready 标记和父子 PID，子进程不继承输出管道；返回后逐个核 PID。两平台父子进程均为 absent，没有把“管道排空”当作树已停止。ready 使用随机标记，避免跨 Windows / WSL 比较时钟。

Windows `store,dsh-experimental` 库测试 435 passed，Linux `dsh-experimental` 库测试 259 passed；同时将 [store.rs](../src/store.rs) 中 `open_existing_returns_the_store_when_it_is_there` 的建库准备与 `open_existing` 调用改为显式测试文件钥匙（`open_with_source` / `open_existing_from`，不改生产默认钥匙来源），Linux 全库 `--features store`（415 passed, 0 failed）与 `--features store,dsh-experimental`（421 passed, 0 failed）均全部通过；默认 / store 相关 bin 与 all-feature clippy 两平台通过。新类型仅在 experimental feature 下添加；既有 CLI、Rust 构造 / 穷举、逐字段读取及 SourceMode 过滤均无值域变化。

```sh
node scripts/verify-dsh-rust-snapshot.mjs --dsh-package '<dsh-package.json>' --rust-reader '<absolute-example-binary>' --native-node '<absolute-node>'
node scripts/verify-dsh-native-admission.mjs --dsh-package '<dsh-package.json>' --rust-reader '<absolute-example-binary>' --native-node '<absolute-node>'
cargo run --example dsh_snapshot --features dsh-experimental -- --path '<absolute-snapshot>' --compression zstd --node '<absolute-node>' --dsh-package '<absolute-package.json>' --native-timeout-ms 3000
```

WSL 模式仍用 `--wsl-reader`；`--native-node` 必须是 Linux 可执行路径，快照矩阵另须显式 `--native-dsh-package '<absolute-linux-package.json>'`，不会猜测 Windows 安装路径。此次在项目 `target` 内安装 Linux Node `v25.2.1`，没有修改全局工具链；夹具与 Python 审计在 Windows，Rust 和其原生 Node 在 Linux。此组合不等于完整 WSL DSH 来源捕获验收。

本节（`495bd46`）时尚未将来源句柄身份、实例 / 选代、生命周期对照及旧前缀核对接入 Rust；后续 Rust 来源选代、同句柄身份捕获与保守提交门见 §10。

## 10. Rust 来源选代、同句柄身份、旧前缀对照与保守提交门

2026-11-06，在 [probe.rs](../src/probe.rs)、[source.rs](../src/dsh_snapshot/source.rs)、[native.rs](../src/dsh_snapshot/native.rs)、[dsh-native-admission.mjs](../scripts/dsh-native-admission.mjs) 与 [examples/dsh_snapshot.rs](../examples/dsh_snapshot.rs) 中补齐 Rust 会话目录选代、同句柄文件身份捕获、旧前缀与生命周期对照及观察前保守提交门，并新增 [verify-dsh-rust-source-capture.mjs](../scripts/verify-dsh-rust-source-capture.mjs)。

所有 `std::fs` 观察仍严格收在 `probe.rs` 边界内，不向外泄露 `File`、`Metadata` 或 `DirEntry`：

1. `probe.rs` 新增不透明 `FileStamp`（Unix 记录 `dev, ino, size, mtime, mtime_nsec, ctime, ctime_nsec`，缺少正 `dev/ino` 时拒绝；Windows 记录 `file_attributes, creation_time, last_write_time, file_size`，任一时间戳为 0 时拒绝）、非跟随路径探测 `lstat_entry` 与同句柄读取 `read_pinned_regular_file`。
2. `read_pinned_regular_file` 顺序核对：打开前 `lstat` 为普通文件 $\to$ 只读打开句柄 $\to$ 同句柄 `file.metadata()` 与打开前 `FileStamp` 一致 $\to$ 校验字节上限 $\to$ `take(len + 1)` 有界读取（区分短读与读中变长）$\to$ 读完后同句柄 `file.metadata()` 再次一致 $\to$ 关闭句柄后再 `lstat` 核对路径身份未变。
3. `source.rs` 对会话目录先 `lstat_entry` 确认是真实目录（非符号链接），再遍历全部目录项（不吞任何条目错误），用 `parse_canonical_log_filename` 识别 `session.jsonl[.zstd]`（v0）与 `session.v<1..>.jsonl[.zstd]`（v1+，拒绝 `.v0`、`.v04`、`.tmp` 等非规范名），拒绝同目录编码后缀冲突（`encoding_conflict`）与非普通文件（`artifact_kind`），按最高规范代选代（不看 `mtime`）；最高代 `!= 4`（含 v0–v3 待迁移或 v5+ 未来代）返回 `Unsupported("physical_generation")`。
4. `capture_source` 在 `read_pinned_regular_file` 取得字节与初始 `FileStamp` 后，在同缓冲区物理核验及原生 admission 前后两次复查目录最高代与目标文件 `FileStamp`，并校验可选预期 `session_id`；全部通过才构造字段私有的 `SourceSnapshot`。
5. `dsh-native-admission.mjs` 与 `native.rs` 新增绑定恢复后的 `session_id` 与逐事件语义摘要数组 `event_sha256s`（长度须等于物理记录位置数且每项为 64 位小写 hex）；[verify-dsh-native-admission.mjs](../scripts/verify-dsh-native-admission.mjs) 同步新增 `wrong_session_id` 与 `wrong_event_hashes`，由 17 组扩展为 19 组。

`SourceSnapshot::compare` 与 `evaluate_commit_gate` 将 §4 的保守提交表编码为类型安全的 `SnapshotRelation` 与 `CommitGateDecision`：

| 观察与比较结果                                              | `SnapshotRelation` | `CommitGateDecision`                                                                                  |
| ----------------------------------------------------------- | ------------------ | ----------------------------------------------------------------------------------------------------- |
| 无旧基线且本次捕获阻断                                      | —                  | `RejectInitial { blocked }`                                                                           |
| 无旧基线且本次捕获与原生 admission 通过                     | —                  | `InitialProjectionCandidate { verified_events, inherited_event_count }`                               |
| 存在旧基线，本次捕获阻断（断尾、语义拒绝、并发变化、发 v5） | —                  | `HoldPriorProjection { prior_events, reason: CaptureBlocked { blocked } }`                            |
| 不同来源实例、目录、session id、编码或物理代                | `DifferentSource`  | `RejectDifferentSource`                                                                               |
| 同一来源下旧事件前缀完整且无新增（含 zstd 重分帧）          | `PrefixPreserved`  | `NoOpUnchanged { verified_events }`                                                                   |
| 同一来源下旧事件前缀完整且尾部新增已验证事件                | `PrefixPreserved`  | `IncrementalAppendCandidate { prior_events, verified_events, appended_events }`                       |
| 当前已验证事件数少于旧前缀（如 zstd 修尾 truncate 中间态）  | `PrefixShrank`     | `HoldPriorProjection { prior_events, reason: PrefixShrank { current_events, shared_prefix_intact } }` |
| 同一生命周期下旧前缀内某条事件内容改写                      | `PrefixRewritten`  | `RebuildProjectionCandidate { invalidates_prior_credential: true, reason: PrefixRewritten { .. } }`   |
| 同一来源下 header 编码或 `inheritedEventCount` 变化         | `LifecycleChanged` | `RebuildProjectionCandidate { invalidates_prior_credential: true, reason: LifecycleChanged }`         |

Windows 与 WSL Linux 实跑 [verify-dsh-rust-source-capture.mjs](../scripts/verify-dsh-rust-source-capture.mjs) 各 35 组全部通过（两种编码各 17 组，加 zstd 重分帧 1 组）：覆盖选代忽略非规范名与旧代 v0、只读 `recheck`、原生 `open("write") + append + flush` 增量追加、同事件数改写、header 生命周期变化、同物理 header 下 `inheritedEventCount` 变化、原生修尾 `truncate` 中间态（明文 10 事件 `no_op_unchanged`；zstd 9 事件 `prefix_shrank` 触发 `hold_prior_projection`，修复并追加后均为 16 事件 `incremental_append_candidate`）、断尾 / 语义拒绝 / 发布 v5 保持旧投影、Rust 读取后进入原生 admission 期间发生追加 / 同长原地覆盖 / 两次原子替换恢复原字节 ABA / 发布 v5 的真实拦截、同目录编码冲突、session id 不符、空或仅 v0 目录，以及跨目录 `different_source` 拒绝。两平台 19 组 `verify-dsh-native-admission.mjs` 与 140 份快照 + 5 组对照的 `verify-dsh-rust-snapshot.mjs`（物理 + 原生模式）回归全部通过。Windows `store,dsh-experimental` 库测试 439 passed，WSL Linux `store,dsh-experimental` 库测试 425 passed，两平台零警告 clippy 通过。

```sh
node scripts/verify-dsh-rust-source-capture.mjs --dsh-package '<dsh-package.json>' --rust-reader '<absolute-example-binary>' --native-node '<absolute-node>'
MSYS_NO_PATHCONV=1 node scripts/verify-dsh-rust-source-capture.mjs --dsh-package '<dsh-package.json>' --wsl-reader '<absolute-linux-example-binary>' --native-node '<absolute-linux-node>' --native-dsh-package '<absolute-linux-package.json>'
```

成功报告显式保留 `source_observation_validated=true`、`semantic_admission_validated=true`，同时在未开启 `--normalize` 时保持 `normalized_positions_available=false`、`atomic_source_snapshot_proven=false`、`production_advance_permit=false`。后续归一化位置映射、持久化凭据 `DshSavedState` 及实验性总库提交接线见 §11。

## 11. Rust 归一化位置映射与实验性总库提交接线

2026-11-06，新增 [normalize.rs](../src/dsh_snapshot/normalize.rs)、[verify-dsh-rust-normalize-store.mjs](../scripts/verify-dsh-rust-normalize-store.mjs)，并扩展 [source.rs](../src/dsh_snapshot/source.rs) 与 [examples/dsh_snapshot.rs](../examples/dsh_snapshot.rs)，在 `dsh-experimental`（及可选 `store`）门控下完成原生记录位置到 `RawEvent` 归一化位置映射、跨进程持久化读取凭据 `DshSavedState`，以及 `CommitGateDecision` 到 `AppendLogObservation` / `CommitPlan` / `TotalStore::apply_projection` 的保守接线：

1. **继承前缀抑制与逐记录归一化跨度（`NormalizedDshSnapshot` / `NormalizedEventSpan`）**：
   - 只消费已通过物理核验与原生 admission 的 `AdmittedSnapshot` / `SourceSnapshot`；
   - `native_seq < inherited_event_count` 的父会话历史记录标为 `NativeEventOwnership::InheritedPrefix` 并统计在 `suppressed_inherited_records` 中，不发射为当前 child 会话的 `RawEvent`，避免父历史消息与 token 用量在 child 会话中重复入库与重复计量；
   - `native_seq >= inherited_event_count` 的本会话记录映射为 `RawEvent`：`system/message`（`Actor::System`）、`developer/message`（`actor = None`，不与 `system/message` 混同）、`user/message`（`Actor::User`）、`assistant/message`（依序提取 `Thinking`、`Message`、`Usage`）、`assistant/attempt`（经同 step 决议后的 `Usage`）、`tool/call`（`ToolUse`）与 `tool/result`（`ToolResult`）；
   - 每条发射事件分配独立的 `EventKey { version: EVENT_KEY_VERSION, record_fingerprint, slot_ordinal }`，并在 `NormalizedEventSpan` 中绑定 `raw_seq`、`event_type`、`event_key`、`record_index`、`native_seq`、`jsonl_start..jsonl_end` 与 `frame_index`；毫秒时间戳经纯整数算术 `epoch_ms_to_iso8601_utc` 转为 UTC ISO-8601，与 `rawevent::occurred_at_unix_ms` 精确互逆。
2. **同 `(turn, step)` 用量槽位与四桶诚实性（`DshUsageEntry` / `DshUsageStatus`）**：
   - 同 `(turn, step)` 内若出现成对的 `llm/retry` $\to$ `llm/retry-started`，保留重试前失败 `assistant/attempt` 的 usage；否则同 `(turn, step)` 后一条 usage 覆盖前一条槽位（前一条记为 `SupersededBySameStep`，不发射为 `RawEvent::Usage`）；
   - 失败 `assistant/attempt` 只使用其自身携带的 `model`（缺失则为 `None`），绝不借用同 step 后续成功 `assistant/message` 的 `model`；
   - 仅当 `inputTokens`、`outputTokens`、`cacheReadTokens`、`cacheWriteTokens` 四项均显式存在且与 `totalTokens` 加总一致时发射 `EventType::Usage`（`ExactFourBucket`）；缺失任一 cache 桶或加总不符时记入 `usage_ledger` 为 `PartialOrUnreconciled`，绝不填 `0` 冒充已知四桶。
3. **跨进程持久化凭据 `DshSavedState` 与保守总库提交（`plan_dsh_source_step` / `commit_dsh_source_step`）**：
   - `DshSavedState` 持久化 `source_binding`、`session_id`、`compression`、`physical_generation`、`dsh_version`、`parser_revision`、`attribution_revision`、`inherited_event_count`、`lifecycle_sha256`、`native_events_sha256`、完整旧前缀摘要 `event_sha256s` 与 `normalized_next_seq`，经 `evaluate_saved_commit_gate` 执行跨进程完整旧前缀核对（含两次追加后改写第 0 段旧前缀检测）；
   - `RejectInitial`、`HoldPriorProjection`（含捕获阻断与 zstd 修尾中间态 `PrefixShrank`）、`RejectDifferentSource` 一律映射为 `ParseQuality::Unavailable` + 空事件 + 旧游标，使 `CommitPlan::plan` 即使面对 `FORCE` / `PARSER_STALE` 也稳定产出 `StoreAction::Preserve` 与 `CursorAction::Freeze`，不覆盖库内既有投影、不推进 `DshSavedState`；
   - `IncrementalAppendCandidate` 校验前 `prior_events` 条原生记录产出的归一化事件数是否仍等于 `saved.normalized_next_seq`：一致时只切出 `native_seq >= prior_events` 的新增事件后缀（保留其在全量归一化中的真实 `seq`），不一致时自动升级为 `PARSER_STALE` 全量重读，防止增量 `seq` 断号或撞号；
   - `RebuildProjectionCandidate`（`PrefixRewritten` 或 `LifecycleChanged`）映射为 `SourceChange::RollbackOrRewrite` 全量事件，在库内已有前代时由 `CommitPlan::plan` 产出 `StoreAction::Project(Projection::Rollback)`；`ProjectionToken` 绑定 `(source_key, lifecycle_sha256, native_events_sha256, parser_revision, attribution_revision, native_events)`，保证写库成功但未保存 `DshSavedState` 时的崩溃重放严格幂等、不重复开新代。

Windows 与 WSL Linux 实跑 [verify-dsh-rust-normalize-store.mjs](../scripts/verify-dsh-rust-normalize-store.mjs) 各 16 项检查全部通过：覆盖带配对重试与部分 cache 桶未知的 usage 决议、失败 attempt 不借用后续 model、child 会话抑制 `inherited_event_count` 前缀、真实 `TotalStore` 首次追加、无变化重扫、连续两次增量追加、两次追加后改写第 0 段触发 `Rollback`、写库后未保存 `state_file` 崩溃重放幂等（`head_moved=false`，`source_revision` 不变）、zstd 修尾变短在 `--force` 下仍 `HoldPriorProjection` + `Preserve` + `Freeze`、同 session id 生命周期变化触发 `Rollback` 开第 2 代，以及出现未来代 `session.v5.jsonl.zstd` 时保持旧投影与旧状态文件。Windows `store,dsh-experimental` 库测试增至 442 passed，WSL Linux `store,dsh-experimental` 库测试增至 428 passed。

```sh
node scripts/verify-dsh-rust-normalize-store.mjs --dsh-package '<dsh-package.json>' --binary '<absolute-example-binary>'
```

