# Claude Remote SSH 转录的项目归属

本文记录 `.claude/projects/ssh-<uuid>/...` 布局的摄取契约。源码交付、独立 CLI 版本和真实 Desktop 来源验收见 [待做清单](backlog.md)。

## 来源位置与工作区位置

`SourceLocation` 表示转录文件的物理存放位置，保持 `Local` / `Wsl(distro)` 两个变体。不能因为文件存在本机，就把其远端 cwd 当作本机或默认 WSL 发行版的路径。

Claude 来源的完整 `source_path` 中出现 `.claude/projects/ssh-<uuid>/...` 目录段时，用该 UUID 识别 SSH endpoint；接受正斜杠、反斜杠及会话下的 `subagents` 子目录。UUID 使用标准连字符格式并收敛为小写。普通 `ssh-*` 项目目录、其他来源类型以及位于其他位置的同名目录不会触发该规则。

有 cwd 的事件使用以下归属：

| 字段 | 值 |
| --- | --- |
| `cwd` | 原始远端路径，保留证据 |
| `project_root` | `ssh:<endpoint>:<cwd>`；Linux 绝对路径形如 `ssh:<endpoint>:/abs` |
| `project_root_source` | `unattributed` |
| `workspace_location` | `ssh:<endpoint>` |
| `source_location` | 转录文件原来的 local / WSL 位置 |

SSH namespace 优先于宿主、默认发行版、盘符挂载规则与本地根注册表。因此远端 `/mnt/c/...` 也不会被换算成本机盘符。相同远端 cwd 在不同 endpoint 下是不同路径；endpoint 不是 git 身份，不能用远端目录名或同名本地仓库推导 git 身份。缺少 cwd 的事件仍按现有规则保留未知，不编造远端项目根。

## 探测边界

- `pathnorm::reach_of` 返回 `RootReach::Remote { namespace }`，不进入本机或 WSL 分支。
- `discovery::probe_path` 返回 `Probe::Remote { namespace }`；批量发现单独增加 `DiscoveryReport.remote`，不计入 `without_root` 或 `failed`。
- 消费方应将 Remote 作为已知缺少访问桥的状态，不能按 WSL 瞬时故障定时发 warning 并重试，也不能解释成探过后没有项目根。
- `identity::repo_id_for_root` 对已知远端返回缺少身份访问桥的错误。注册根身份扫描根据 `reach_of` 在进入探测前分流，计入 `IdentitySweep.skipped_remote`，不访问文件或记录探测判决。真实探测失败仍计入 `unresolved`，下一轮重试。
- `roots` 对 SSH 命名空间输出 `host_path: null`。格式错误的 `ssh:` 输入在 `reach_of` 返回 `Unknown`，不会降级成本机路径。当前不提供 SSH 文件访问、远端 git 探测或自动合并 endpoint 的能力。

## 存量数据

`PARSER_REVISION` 提升到 6。扫描与 `TotalStore::recompute_attribution` 从同一来源类型、源路径与原始 cwd 得到 namespace，并走同一归属逻辑。

已记录旧 revision、源文件仍可读且调度器正确传入 `PARSER_STALE` 的来源，全文读取后以新投影替换旧投影；不能仅把读取游标归零再按 `Append` 去重。仅调用库的扫描 API 不会替宿主补齐这一调度职责。未记录旧 revision、源文件消失或本轮预算未覆盖时，不声称 revision bump 已完成修复。

源已消失、但事件仍保留原始 cwd 与 source_path 时，可以预览再执行 `recompute_attribution`；它只重算归属，不把旧事件标成完成了 revision 6 的全部提取。无 cwd 的旧归属按既有保护规则保留。旧的错误根注册记录与历史身份不在本次解析修复中自动删除，消费方需要核实其他本地来源是否仍引用它们，再单独订正其索引。

本次不改 schema，不操作真实总库、索引或凭据，不执行消费方升级或存量订正。

## 四类消费影响

| 消费形状 | 影响 |
| --- | --- |
| CLI | `project_root` 可出现 SSH namespace，`workspace_location` 增加 `ssh:<endpoint>` 值。显式登记的 SSH 根在 `roots` 的 `host_path` 为 `null` |
| Rust 构造与穷举 match | `RootReach`、`discovery::Probe` 增加 `Remote`。`DiscoveryReport` 增加 `remote`，`IdentitySweep` 增加 `skipped_remote`。穷举及字面量构造需适配 |
| Rust 逐字段读取 | 发现与身份扫描各有独立的远端计数。读取汇总时纳入新字段，处理 workspace 值时纳入 `ssh:<endpoint>` |
| discover / SourceMode 过滤 | 来源描述符、物理 SourceLocation 和 `SourceMode::AppendLog` 不变；远端隔离不等于来源不再被发现 |

消费方自行更新子模块、重新构建或冻结，再核实际运行版本。源码交付、消费方编译通过、存量投影修正与正式运行验收分别确认。

## 交付前复审

1. **身份扫描。** 同一进程的两个扫描轮次都把显式登记的 SSH 根计为 `skipped_remote`，探测判决保持 `not_probed`。本机给 `.git/config` 写入无效 UTF-8，首轮报 `unresolved`，恢复配置后次轮写入 git 身份。修复前两个根都计为 `unresolved`。
2. **调度与投影。** 隔离 `scan-all` 夹具以 revision 5 写入 SSH 旧投影，状态保留 EOF 游标和全文指纹。转录字节与 mtime 未变时，实际 CLI 调度全文读并执行 `reparse`，源版本保持、投影版本加一，变更流新增一条 `reparse`。SSH 事件保留原始 cwd，同一 cwd 的普通 Claude 来源保持本机归属。再扫时两份来源均追加零条，头和变更流保持。
3. **兼容与交接。** 对 QuotaBar 实际调用点的穷举模式做了隔离编译，确认新增的 `Probe::Remote` 与 `IdentitySweep.skipped_remote` 需要适配。片段补齐枚举分支和统计字段后编译通过。完整应用构建和运行验收由消费方继续。

## 本仓验证（2026-11-26）

`cargo test --lib --features store,dsh-experimental` 在 Windows 通过 511 项，在 WSL Linux 通过 496 项。Linux 的 `cargo test --features store --test cli_scan_baseline_it` 通过 6 项。CLI 用例在 Linux 构建运行，避免 Windows CLI 枚举宿主的真实 WSL 转录。

两端的默认与 `store` feature 均通过 `cargo check --bins`，`cargo clippy --all-targets --all-features -- -D warnings` 也通过。格式、公开仓及指令体量门通过。

将源码副本的 stale 判据从 `rev != current` 改为 `rev > current` 后，旧版 SSH 来源走成 `append`，新增 CLI 用例失败，同组其余五项通过。变异结束后核对了源码字节。

合成夹具还覆盖来源路径分隔符、subagent、同名本机与 WSL 根、远端 `/mnt/c`、SSH 根的宿主路径、实际 append-log 扫描和存量归属重算。真实 Desktop 来源及消费方完整应用构建仍待验收。
