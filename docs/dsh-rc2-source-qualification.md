# DSH rc.2 来源侧隔离验证

查证日期：2026-11-13。仅本机 Windows、Node `v25.2.1`、无模型合成来源；不打开真库，不启动 Host/UI，不安装或发布。

## 接线与固定证据

基线为 `74e9ca648f6d81db0694e281c45dd509d225634c`，候选适配与后续审查修复位于分支 `fix/dsh-rc2-qualification`；本记录描述源码与隔离验证，不代表已推送、发布或安装。

共享 `scripts/dsh-installed.mjs` 保留默认 `0.2.2-rc.1`，继续接受 `0.2.1-rc.5` / `0.2.2-rc.1`，显式增加 `0.2.2-rc.2`。安装入口先取 `realpath`，再按真实 package.json 建立模块解析锚点，以兼容 pnpm 目录链接。原生 admission 和各检查报告使用 `installation.version`，不以默认版本常量替代实测。

本轮显式安装的 manifest 实测为 `@deepseek-ai/dsh@0.2.2-rc.2`，要求 Node `>=24.9.0`；真实安装锚点已核，个人绝对路径不写入公开记录。版本、源码和安装字节分别记录，不能相互替代。

| 证据 | 实测值 |
| --- | --- |
| 官方源码候选提交 | `ddac96760b6edf8fa0fe17cbdcf0b512f4c12b2c` |
| 候选 manifest 文件 SHA-256 | `7467026981c612f9e5d675a63174d39dcad1fcb7ff98019b3caac6500277b695` |
| 实际安装包文件树 | 541 个文件；摘要 `704b7ff5bea961b2bc5ca4efbea3e38fbd147b12351bd53cfbca842fcfab85bb` |
| `dsh-session` 入口 SHA-256 | `d9911b5e1585bcb142d03ce46eb2990b3fc7c254de5a76d3ffb124bd2469216c` |
| `dsh-session-format-v3-to-v4` 入口 SHA-256 | `da0b0c2e1460316e659f48f4222730962046b3634d1fb634683bb7243a7bba80` |
| `dsh-session-persistence-jsonl` 入口 SHA-256 | `69efef5f84dddf947032bc1425067878fa610416c8885acde2ded9b01230c9b7` |
| `dsh-token-meter` 入口 SHA-256 | `0f27f128e12aaecdf360de1f2c974d5d759bc1002c5a3de560322669d200cb9a` |

安装包文件树的取数方法由 `scripts/verify-dsh-rc2-qualification.mjs` 的 `treeDigest` 固定：递归按名称排序，拒绝包内链接，以 `[相对路径, 文件 SHA-256]` 列表的 JSON 字节计算 SHA-256。所列模块入口另取解析后文件的 SHA-256。这不是整个依赖闭包的摘要，也不证明 npm 安装字节可以从所列源码提交可重现构建。

## 最终嵌入 loader 的 binary

本轮仅使用 worktree 的 `target` 作为 `CARGO_TARGET_DIR`，从修改后的源码新建 `target/debug/examples/dsh_snapshot.exe`，没有复用已安装 svault 或复制其他构建缓存。`src/dsh_snapshot/native.rs` 的 `include_str!` 嵌入共享 loader、admission 和 limits；该 Rust 文件本身未改。

构建命令为 `cargo build --example dsh_snapshot --features store,dsh-experimental`，自身 rc=0。binary 为 7,016,960 字节，SHA-256 为 `b504d9c87cb927b5685f2f5b5059e80390217ad6e4e3cd294907c63b3e21b536`；首轮验证结束时再次核对，字节未变。

| 嵌入来源 | SHA-256 |
| --- | --- |
| `scripts/dsh-installed.mjs` | `46866892c52379f1423cad2ddb80f247995c71c60bb1e8b92998be93227cc6a0` |
| `scripts/dsh-native-admission.mjs` | `c301f612b7223266eacf92c534b8bdf9d7ff21bf36b6b82dc0ef39d116946c01` |
| `scripts/dsh-snapshot-limits.json` | `fe4921c393f0ab2b7f719ba884572b7e2e28c4b315d11273744ce66a96570e6e` |
| `src/dsh_snapshot/native.rs` | `5035d76c87c41411a655744778a57ef23c785be9aec3fbadde0edd8ac1d74ca1` |

## 合成兼容与持久化

复用既有 codec、Rust 捕获/归一化、提交规划和 TotalStore 路径，没有增加另一套来源解析器。所有合成检查均显式传入候选 package.json 与本轮新建 binary，所有成功报告均返回实际 `0.2.2-rc.2`。下表为适配首轮结果；审查后的完整复跑另列于后文。

| 命令入口 / 模式 | 自身 rc | 本轮结果 |
| --- | --- | --- |
| `verify-dsh-v4.mjs` | 0 | 20 项检查、13 个 usage 向量 |
| `verify-dsh-native-admission.mjs` | 0 | 19 项错误响应/预算/收尾检查；超时后本次父子 PID 均 absent |
| `verify-dsh-rust-snapshot.mjs`，物理模式 | 0 | 140 份快照，含截断、校验和、字节/帧位置 |
| 同入口，原生模式 | 0 | 140 份快照；语义 admission、生命周期绑定和位置对拍 |
| `verify-dsh-rust-source-capture.mjs` | 0 | 35 项来源选代、同句柄、旧前缀、改写、并发与保守提交门检查 |
| `verify-dsh-rust-normalize-store.mjs` | 0 | 25 项归一化、游标/状态、重放幂等、usage 与合成 TotalStore 持久化检查 |
| `node --test scripts/dsh-installed.test.mjs` | 0 | 3 项：旧版接受范围/默认值、目录链接解析、沙箱拒绝链接与独占新目录 |
| 全部 `scripts/*.mjs` 的 `node --check` | 0 | 脚本语法通过 |

旧版接受范围验证使用合成 manifest；本轮没有重新验证旧 SDK 的完整运行时矩阵。原生 admission 的故障 SDK 只验证错误处理，真实 rc.2 成功由原生快照与来源捕获矩阵证明。

Node 同步 zstd 原语仍实测为串联帧只返回第一帧，且撕裂校验和尾部可能返回完整 payload；既有断言保留这些结果。物理完整性由 Rust 核验与 Python 独立审计证明，没有据 Node 解压成功宽松放行。

normalize-store 原有系统临时目录与 finally 递归删除已移除，接到现有 `io-harness.mjs` 的受保护目录工具：只在项目 `target` 下创建独占新目录，拒绝链接、不接受复用路径，不自动递归清理。每次 CLI 的 stdout、stderr、rc/signal/error 以及检查状态落盘；成功、失败和命令超时走状态保存分支。本轮成功运行留下 `target/dsh-norm-store-NUQPrb/`，含合成数据库与状态文件。命令超时异常分支本身未单独注入实跑；原生子进程超时及树收尾已由上述故障矩阵验证。

## 源仓门与运行记录

首轮日志目录为 `target/dsh-rc2-qualification-oFB6NV/`，`status.json` 保存各命令自身 rc、时间、参数和 binary 摘要，每条命令有独立 `.stdout.log` / `.stderr.log`。脚本回归日志在 `target/dsh-rc2-qualification-w0N0t0/`。这些目录是本机忽略的证据目录，包含本机参数，不能直接作为公开附件。

| 源仓命令 | 自身 rc |
| --- | --- |
| `cargo fmt --all` | 0 |
| `cargo test --lib --features store,dsh-experimental` | 0；499 passed / 0 failed / 0 ignored |
| `cargo check --bins` | 0 |
| `cargo check --bins --features store` | 0 |
| `cargo clippy --all-targets --all-features -- -D warnings` | 0 |
| `uv run python scripts/check-public-safe.py` | 0 |
| `uv run python scripts/check-agents-budget.py` | 0 |

可复跑入口：

```sh
node scripts/verify-dsh-rc2-qualification.mjs --dsh-package <absolute-package-json> --source-repo <absolute-runtime-source-repo> --candidate-file <absolute-candidate-json>
```

该入口串行执行大构建并逐命令记录真实 rc；`--only` 只运行指定名称的子集，子集通过不能当全套交付通过。所有目录保留，未自动清理。

## 审查修复与完整复跑

2026-11-13 审查后修复六项问题：步骤超时时收尾整个子进程树；最终复核包目录外的依赖入口；记录并绑定跳过构建时实际使用的 binary；预检和最终摘要失败均保存失败终态；pnpm 链接回归实际验证依赖解析；normalize-store 将执行与留证封装在局部函数中，消除可变全局空回调。

修复后的完整记录为 `target/dsh-rc2-qualification-2KyPfu/status.json`：`full_qualification=true`，16 个步骤各自 rc=0，binary、嵌入源码、安装包与依赖入口的结束复核均匹配。该轮包括上述来源、游标、usage 矩阵、499 项库测试、两种 bins 编译、全特性 Clippy 和两道仓库闸；binary 摘要与前表一致。

安装与验收驱动回归由三项增至七项，新增非零退出/启动失败、超时后的父子进程退出、预检失败留证、包外依赖变更或消失时拒绝通过。跳过构建的 Rust 子集另行复核，明确记录 `built_by_this_run=false`，并核 binary 与依赖入口绑定；子集通过仍不替代完整验收。Windows 进程树超时路径通过，Unix 进程组路径通过 WSL 只读加载实际函数单独核验；这不等于 Linux 完整来源验收通过。normalize-store 自身的超时留证分支仍未单独注入。

## 生产 gate 与未验边界

这次完成候选来源接线和合成兼容，不授予生产候选晋升。生产 pending dispose 阻断保持；本轮没有执行真实 Host 的 pending/晚到输入生命周期验证，不能宣称该阻断已解决。没有重试完整 bundle/UI，也未修改 Runtime 内核、消费者指针或部署状态。

物理/来源证据继续断言 `production_advance_permit: false`，没有放宽捕获、语义或提交门。合成持久化通过不等于真实宿主来源的授权、原子快照或生产准入通过。

Linux 完整验收矩阵、远端、真实 Host/UI、真实模型与真库兼容均未验；WSL 仅单独核验新增的 Unix 进程组超时路径，未做跨系统写入。旧 SDK 的完整运行时矩阵仍未复跑。本轮不推送、发布或安装。
