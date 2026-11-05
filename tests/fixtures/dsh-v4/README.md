# DSH v4 合成夹具

目标安装版为 `@deepseek-ai/dsh` `0.2.1-rc.5`；查证与运行日期：2026-11-05。所有样本完全合成，不含真实日志片段、身份或配置。

- `complete-turn.json`：10 事件的逻辑 artifact，含五种消息角色、text / reasoning / tool-call、call/result 与完整 turn/step 边界。stream 有两次 usage 报告，外层 message 保留最后一份；正确单次用量为 20。
- `scenarios.mjs`：从共享 artifact 在内存中派生完整失败重试（14 事件、用量 37）、继承前缀与新增 turn（child 共 17 事件，父用量 20、新增用量 5）及再继承。该模块只是夹具生成器，按共享样本的事件关系构造，不是通用 DSH 日志改写器。
- `usage-cases.json`：13 个用量向量，含精确 / 推导 total、未知 cache 与已知零、缺失 / 非法 / 溢出；`expected:null` 表示原生严格 reducer 返回 `undefined`，没有承诺已实现本仓诊断分类器。

运行 [verify-dsh-v4.mjs](../../../scripts/verify-dsh-v4.mjs) 执行 20 组检查。物理 JSONL header 由原生 codec 生成，逻辑 header 不含 `type:"session"`；strict / recoverable codec、原生 restore、detached Session、stream 装配与严格用量 reducer 分层校验。完整重试已替换之前只给 reducer 部分字段的单元样本。

```sh
node scripts/verify-dsh-v4.mjs --dsh-package '<dsh-package.json>'
```

该离线脚本的 zstd 检查只涉及 Node 同步原语和已知边界的合成帧，并报告当前 Node 版本、串接读取及截断 checksum 的行为。解压有输出或 codec finish 未抛错都不能单独证明输入完整；其 `backend_recovery_validated` 明确为 false。

另运行 [verify-dsh-backend.mjs](../../../scripts/verify-dsh-backend.mjs) 执行 19 组真实 backend 检查。它用最小 Cordis Context 挂载安装版 persistence plugin，复用这里的样本，在本仓 `target` 的全新目录内生成明文 / zstd 日志，验证只读不改文件、原生坏尾恢复、v3 内存迁移、受控 v4 发布、未来代拒绝、追加与缩短，以及明文 / zstd 写路径修尾。脚本保留合成日志目录，报告本次相对路径；不加载 Host / LLM，不写真实日志或总库。

```sh
node scripts/verify-dsh-backend.mjs --dsh-package '<dsh-package.json>'
```

当前未覆盖附件、原生 `buildForkSeed` 复杂修复、descriptor、compaction、0→4 全链迁移、并发竞争窗口、崩溃持久性或真实 provider。顺序交错追加 / 代切换不等于并发压力验收。字段映射、实测边界、源码出处与待实施门见 [DSH 核查记录](../../../docs/dsh-ingest-survey.md) §9–§12。
