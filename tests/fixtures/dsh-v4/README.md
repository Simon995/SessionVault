# DSH v4 合成夹具

目标安装版为 `@deepseek-ai/dsh` `0.2.1-rc.5`；查证与运行日期：2026-11-05。所有样本完全合成，不含真实日志片段、身份或配置。

- `complete-turn.json`：10 事件的逻辑 artifact，含五种消息角色、text / reasoning / tool-call、call/result 与完整 turn/step 边界。stream 有两次 usage 报告，外层 message 保留最后一份；正确单次用量为 20。
- `scenarios.mjs`：从共享 artifact 在内存中派生完整失败重试（14 事件、用量 37）、继承前缀与新增 turn（child 共 17 事件，父用量 20、新增用量 5）及再继承。该模块只是夹具生成器，按共享样本的事件关系构造，不是通用 DSH 日志改写器。
- `usage-cases.json`：13 个用量向量，含精确 / 推导 total、未知 cache 与已知零、缺失 / 非法 / 溢出；`expected:null` 表示原生严格 reducer 返回 `undefined`，没有承诺已实现本仓诊断分类器。
- `persistence-peer.mjs`：并发核验脚本 fork 的独立进程，只接受本次 `target` 运行目录内的合成文件操作和原生 persistence 句柄调用。通过 IPC 回执安排窗口，不是 Host 或正式来源服务。

运行 [verify-dsh-v4.mjs](../../../scripts/verify-dsh-v4.mjs) 执行 20 组检查。物理 JSONL header 由原生 codec 生成，逻辑 header 不含 `type:"session"`；strict / recoverable codec、原生 restore、detached Session、stream 装配与严格用量 reducer 分层校验。完整重试已替换之前只给 reducer 部分字段的单元样本。

```sh
node scripts/verify-dsh-v4.mjs --dsh-package '<dsh-package.json>'
```

该离线脚本的 zstd 检查只涉及 Node 同步原语和已知边界的合成帧，并报告当前 Node 版本、串接读取及截断 checksum 的行为。解压有输出或 codec finish 未抛错都不能单独证明输入完整；其 `backend_recovery_validated` 明确为 false。

另运行 [verify-dsh-backend.mjs](../../../scripts/verify-dsh-backend.mjs) 执行 19 组真实 backend 检查。它用最小 Cordis Context 挂载安装版 persistence plugin，复用这里的样本，在本仓 `target` 的全新目录内生成明文 / zstd 日志，验证只读不改文件、原生坏尾恢复、v3 内存迁移、受控 v4 发布、未来代拒绝、追加与缩短，以及明文 / zstd 写路径修尾。脚本保留合成日志目录，报告本次相对路径；不加载 Host / LLM，不写真实日志或总库。

```sh
node scripts/verify-dsh-backend.mjs --dsh-package '<dsh-package.json>'
```

另运行 [verify-dsh-concurrency.mjs](../../../scripts/verify-dsh-concurrency.mjs) 执行 21 组受控双进程 / 读取状态反例。父子进程各自挂载原生 backend，借公开 Node I/O 完成屏障与 IPC 回执控制追加、修尾和代发布窗口；每个窗口断言实际命中，随后恢复方法。覆盖合法旧前缀与精确续读、半条 / 半帧、原生 flush 可见性、损坏、EIO 注入、取消、同事件数内容改写，以及完整 / 坏尾文件的公开读取结果不可区分。EIO 是合成故障注入；这些检查不证明真实磁盘故障或一般压力下的行为。

```sh
node scripts/verify-dsh-concurrency.mjs --dsh-package '<dsh-package.json>'
```

该脚本仅操作 `target/dsh-concurrency-*` 的全新合成目录，保留运行物，等待自己启动的子进程退出；不改安装源码或私有 backend 状态。报告区分受控窗口已验与公开物理完整性证据不可得；没有交付生产读取状态或安全推进位置。`eventState` 表示事件所有权，不是来源完整性；事件数防倒退不能证明内容未改。

当前未覆盖附件、原生 `buildForkSeed` 复杂修复、descriptor、compaction、0→4 全链迁移、无约束并发压力、租约争夺、崩溃持久性、WSL 或真实 provider。19 组 backend 脚本仍报告 `concurrent_races_validated=false`；21 组并发脚本另报 `controlled_concurrent_windows_validated=true`，两者分别限定自身范围。字段映射、实测边界、源码出处与待实施门见 [DSH 核查记录](../../../docs/dsh-ingest-survey.md) §9–§13。
