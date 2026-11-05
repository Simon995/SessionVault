# DSH v4 合成夹具

`complete-turn.json` 是完全合成的逻辑 artifact，不含真实日志片段、身份或配置。目标安装版为 `@deepseek-ai/dsh` `0.2.1-rc.5`；查证与运行日期：2026-11-05。

样本包含五种消息角色、文本 / reasoning / tool-call、一次 call/result 关系与完整 turn/step 边界。assistant 的 stream 有两次 usage 报告，最终 message 也保留最后一份 usage；正确的一次计量为 20，不能累加过程样本或重复计算外层消息。

运行 [verify-dsh-v4.mjs](../../../scripts/verify-dsh-v4.mjs) 可执行原生 v4 restore、detached Session、物理 codec 与 stream 往返，以及缺用量、未知 cache、重试折叠和原生校验缺口的反例。物理 JSONL header 由 codec 生成，逻辑 header 不含 `type:"session"`。

```sh
node scripts/verify-dsh-v4.mjs --dsh-package '<dsh-package.json>'
```

脚本内的重试样本只用于 reducer 单元检查，不能当作可回放的完整原生日志。当前夹具不覆盖附件、seed、compaction、迁移、损坏恢复或真实 provider 行为。详细字段映射、源码出处与尚未完成的实现门见 [DSH 核查记录](../../../docs/dsh-ingest-survey.md) §9。
