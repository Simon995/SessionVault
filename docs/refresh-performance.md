# 刷新性能：测量边界与已验证结论

查证日期：2026-11-16；各表保留原始实验日期。源码、可复跑工具和本仓实验是本文依据；消费方的安装身份、生产轮次与内部账本由消费方维护。当前任务顺序只维护在 [backlog.md](backlog.md)，本文不另开待办队列。

## 结论与根因

| 问题 | 已核实的机制 | 能得出的结论 |
| --- | --- | --- |
| 重启后扫描报告零新字节，仍有压缩读取成本 | `scan` 在 EOF 早退前请求逻辑长度；WSL `.zst` 解压长度按压缩文件身份缓存，缓存属于进程，重启即为空 | `bytes_read=0` 只描述后续增量读取，不能排除前面的压缩件搬运和求长度 |
| 超过批上限的整件读取重复启动桥 | 旧 `read_whole` 先尝试 `read_many`，超限后再 `stat`、`read_range`；成功路径实际三次桥调用 | `556d1f8` 将元数据与正文合并为一次桥调用，直接消除了两次重复启动 |
| WSL 项目身份探测逐跳串行启动桥 | 旧 `wsl_repo_id` 经 `WslBackend` 每次 `probe` / `read_text` 单独启动 `wsl.exe`（标准仓 3 次、submodule 5 次、worktree 6 次），总库扫描逐根累加 | `probe_git_roots` + `PrefetchedWslBackend` 将单根多跳与同发行版批扫描各合并为 1 次桥调用，判决仍走 `identity.rs` 唯一规则 |
| 日志中的 guard 时间很大 | 调用方可在 `with_session_bindings` 回调中执行投影、本地索引与确认；guard 包围这些工作 | guard 总时间不能当作锁等待，也不能与其包含的 index / projection 再相加 |
| 首轮与稳态差异很大 | 进程缓存、首次身份探测、来源积压、变化来源和并发工作可能不同 | 自然轮次只支持当轮观测；来源数相同也不证明输入、缓存或负载相同 |

生产调用链分别见 [`scan.rs`](../src/scan.rs)、[`probe.rs`](../src/probe.rs)、[`compressed.rs`](../src/compressed.rs)、[`wsl.rs`](../src/wsl.rs) 和 [`store.rs`](../src/store.rs)。本地压缩长度探针直接调用解码函数，未复现 WSL 搬运及进程长度缓存；不能用其单轮结果否定 WSL 冷读成本。

## 四类实验各自测了什么

| 工具 | 输入与计时范围 | 不覆盖的工作 |
| --- | --- | --- |
| `measure_compressed_len` | 只读索引中非归档的本机 Codex 路径，逐项探测并调用生产 `decoded_len`；分别报告查询、求长度和遍历 wall | WSL、真实刷新全链路、OS 冷缓存保证；不可测项计数非零时命令失败 |
| `measure_binding_guard` | 自建隔离磁盘库，经公开 `with_session_bindings`；分别运行 guard-only 与 guard+账本 SQL 复现 | 生产连接争用、历史正文、本地索引方法或真实投影负载；两种模式按固定顺序运行，不是随机 A/B |
| `measure_wsl_read_whole` + Python 对照 | 同一合成文件、同一生产读取 API，逐轮交替新旧二进制先后；发行版枚举在计时外，SHA-256 在读取计时后计算 | 解压、scan、整个应用首轮、ext4 真实来源与 OS 冷缓存保证 |
| `measure_wsl_identity` + Python 对照 | `agent-logs` 下合成 git 夹具，经公开 `repo_id_for_root`（单根）与内存库 `sweep_registered_root_identities`（8 根混合批），逐轮交替新旧二进制并核对判决与二进制哈希 | 真实磁盘总库锁竞争、`drive_mounts` 枚举、ext4 真实项目树、OS 冷缓存保证与整个应用首轮 |

绑定守卫的本仓隔离实验使用 910 个合成来源、每模式三轮：guard-only 含 prior 查询的 wall 为 134.85–160.29 ms，加入账本 SQL 后为 319.63–353.08 ms。`index_ack_ms` 位于 `guard_ms` 内，不能再相加；这些结果既不证明生产没有锁竞争，也不支持删除准入守卫。

本机求长度探针的 `source_stat_ms` 是成功压缩来源的逻辑 stat 总时间，包含路径探测、打开压缩件和 `decoded_len_ms`，两者不能相加；它不是纯元数据探测耗时，也不包含被跳过的普通来源与不可测来源时间。`wall_ms` 覆盖全部来源遍历，`query_ms` 单独计时。原始记录包含路径哈希和逐项时间，仅本机保留。该探针执行时未清 OS 缓存，结果不称为“磁盘冷读”；物理文件在测量期间变化时，本次逐项读取也不构成跨文件一致快照。

## 单桥读取的最终受控对照

旧臂为 `7c3506f` 的冻结源码，新臂为最终 `556d1f8` 生产读取实现，均为 release、相同锁文件与工具链。文件位于 Windows 文件系统，由 WSL 经挂载路径读取；每尺寸每臂六次，共 48 个新进程样本，未清 OS 缓存。全部命令返回码、长度、SHA-256 和 mtime 一致；原始记录为本机 `review-release-ab.json`。

| 文件大小 | 旧中位耗时 | 新中位耗时 | 耗时降低 | 旧 / 新桥启动尝试数 |
| --- | ---: | ---: | ---: | ---: |
| 0 B | 309.78735 ms | 336.08340 ms | -8.49% | 1 / 1 |
| 4 KiB | 298.49425 ms | 287.96165 ms | 3.53% | 1 / 1 |
| 8 MiB + 1 B | 1085.66835 ms | 412.22865 ms | 62.03% | 3 / 1 |
| 128 MiB | 3450.32465 ms | 2752.95125 ms | 20.21% | 3 / 1 |

大件三桥变一桥符合实现预期；空文件本次中位值变慢，不宣称普遍提速。早一候选的 68.35% / 25.02% 不对应最终实现，只保留在历史记录，不能替换本表。以上百分比均不能外推成整个自然刷新首轮收益。

复跑时分别准备 `7c3506f` 与 `556d1f8` 的隔离源码快照。旧臂 `7c3506f` 尚无 `measure_wsl_read_whole` example，须将 `556d1f8` 的 `examples/measure_wsl_read_whole.rs` 原样补入旧快照，仅补测量入口、不替换旧臂 `src/`。两臂使用同一份 `Cargo.lock` 和同一工具链，分别执行 `cargo build --locked --release --example measure_wsl_read_whole`，保留两份二进制，再从当前仓库运行：

```bash
uv run python scripts/measure-wsl-whole-read.py --help
```

命令要求显式的新旧二进制、新夹具目录及发行版内的同一目录；路径应经真实 `wslpath` 核对，不按盘符猜。Git Bash 传入 `/mnt/...` 时需 `MSYS_NO_PATHCONV=1`。夹具拒绝复用已有目录，工具保留合成文件供核验。

## 读取协议与失败边界

元数据、文件系统类型和正文来自同一个已打开 fd，并再次确认该 fd 是普通文件。普通非零长度文件使用 `F` 头，只读取声明长度，短读或多读均失败；procfs / sysfs 和零长度文件使用 `Z` 头读到 EOF。虚拟文件的 stat 长度可能为零或大于实际正文，不能套用普通文件的长度断言。确认缺席或非普通文件返回 `None`；探测未知、读取失败和协议损坏返回 `Err`。

回归覆盖任意二进制、空文件、超批上限、缺席、到期不启动、procfs / sysfs、读取失败及打开后设备替换。曾有一次实机对拍在旧 `read_file_at` 调用上退出 -1；同代码随后重跑通过，瞬时失败根因未核，不能声称该底层问题已被本次优化修复。短 stdin 脚本的同步写入也没有独立写超时；本次不扩大为“所有桥路径均有完整绝对 deadline 保证”。

## 自然刷新验收口径

1. 用提交、精确启动标记、PID 与创建时间固定新进程；磁盘 exe 变新不证明运行映像已更换。分别记录 ready 等待、首轮和后续稳态。
2. 同时核 scanned / attempted / unchanged / changed、计划全读数、实际错误、预算耗尽和延后来源。`errors=0` 不等于整轮完整；身份阶段因预算跳过的轮次，也不能证明身份探测已变快。
3. `sv_wsl_spawns` 及 after / identity 窗口均是进程级计数，可包含并发调用；计划 `whole_reads` 也不等于实际整件读数。
4. 保留完整启动窗口内的失败、task failed、预算不足与进程退出。积压追赶、预算受限样本和完整轮分别登记，不能只选成功尾部构造对照。
5. 核双层确认的 desired、index ack、store ack，并单独核缺账和空 desired；只有 debt=0 不足以证明全部活动来源双确认。验收只读日志与元数据，不为改善数字执行 force、重建或清来源。

截至本次整理，单桥优化已在上游 main 推送，公开 API、schema 与来源值域均未改变；正式独立 CLI 仍为 `v2026.11.11.1`（`b9339c3`）。消费方部署与自然轮验收单独记录。自然样本存在积压、预算及读取失败差异，尚不能给出固定的整轮因果提速百分比，也不能把首个成功轮当作整个验收窗口通过。

本次另核既有 `read_range`、`run_bash_stdin` 和 `wait_with_deadline` 与单桥优化前源码逐函数一致。`read_range` 的 `exited Some(-1)` 是直接输出子进程状态码，不是本模块把超时映射成 -1；本模块等待超时会单独返回 `wsl.exe timed out after ...`。这只定位到非零进程退出，底层失败原因仍未核；“该函数没改”也不能排除共享资源等间接影响。后续先收失败调用的阶段、耗时、退出及重试恢复证据，不以猜测调大 timeout 或跳过确认。

## 调用级诊断与合成 EOF 复核

源码诊断 `74e9ca6` 已合入并推送，使用既有 `log` facade，默认不输出 debug 日志。CLI 可用 `SVAULT_LOG=sv-scan=debug,sv-wsl=debug`；库形态由宿主设置同名 target 的过滤，CLI 环境变量不会自动为库宿主装配过滤。真实首轮前必须核新构建正在运行，且目标日志实际到达所核 sink；应用自己的 debug 设置、子模块更新或依赖测试通过，都不能代替这个条件。启用契约只在 [日志规范](LOGGING.md) 维护，消费方运行状态回各仓记录。

- `sv-scan` 的 `compressed length/read/decode` 分开记录长度缓存命中、未复用、压缩件读取和本机解压；只记录匿名来源短哈希、大小与进程内 `compressed_call` 调用号；同一来源并发调用按调用号关联。`cache=not_reused` 不细分无缓存、身份变化和缓存锁不可用，`identity_available=false` 也不能解释成文件缺席。`read_ms` 包含 `take_whole` 的锁等待和可能的整批读取，不是纯传输耗时；从批内直接取回已读内容时可能很短。`elapsed_through_decode_ms` 包含读取及解码，不能与 `decode_ms` 相加。
- `sv-wsl` 的 `whole batch` 输出整批 requested / fetched / absent_or_non_regular / failed / bytes / elapsed。`result=ok` 只指整批协议调用返回成功，须另看 fetched 是否齐全。`absent_or_non_regular` 合并确认缺席与非普通文件；`failed` 包含逐条读取失败、坏记录和协议缺答。`wsl::parse_read_many` 已将缺答补成错误，上层不能再用 HashMap 缺键另数；旧 `unanswered` 字段恒为零，已移除。未取回的来源仍走原有回退。批时间与触发它的来源 `read_ms` 重叠，不能逐来源求和后再加整批。
- `sv-wsl` 的 `identity batch`、`identity operation completed` 与 `identity completed` 记录按发行版批预取摘要及每根后端 probe / read_text 的调用形状、结果与耗时；根只以 SHA256 短标识关联，不输出配置、remote 或原始根。后端调用次数是 `ProbeBackend` trait 调用数，不等于进程创建尝试：命中预取快照的调用在内存完成。`sweep_registered_root_identities` 触发某发行版首次批预取时，`identity batch` 在进入首个根的 `wsl_repo_id_with` 前执行，其 `elapsed_ms` 与各根 `total_inclusive_us` 不重叠；单根 `repo_id_for_root` 无外层批规划，惰性预取计入首个 `probe` 调用。`probe_nested_us` / `read_text_nested_us` 属于 `total_inclusive_us`，诊断输出开销也可能进入总时长；该入口不含上游注册表查询、锁等待和持久化写入。短哈希仅供诊断，不是项目身份。

静态调用链上，WSL 普通 `.git` 目录的可读 config 路径经 `ProbeBackend` 产生两次 probe 与一次 read_text，worktree / submodule 随指针和 commondir 增加 trait 操作，缺席或未知会早退；预取上线后，常规单根或同发行版整批的候选路径由一次 `probe_git_roots` 桥调用装入快照，仅超过单文件预取上限（1 MiB）或未覆盖路径才回落单件桥调用。该路径不经过 `read_whole`。每个可写库句柄的身份 seen 从空开始，已存身份不替代新句柄的首次磁盘核验；不能用预填 seen 隐藏 remote 变化。具体判决仍以 `identity.rs` 与 `store.rs` 实现为准。

合成复核入口为 [`measure_wsl_compressed_cache`](../examples/measure_wsl_compressed_cache.rs) 与 [`measure-wsl-compressed-cache.py`](../scripts/measure-wsl-compressed-cache.py)：新进程扫描两种大小的随机合成 `.zst`，游标预置于 EOF，连续三轮验证 `bytes_read=0`、零事件、逻辑长度与游标不变，同时要求首次解压、后两轮命中，以及批读 / 单件两条路径都被覆盖。使用显式空绑定和空项目注册表，不加载真实控制面、索引或总库；合成内容无需具备会话语义，实验不验证 parser。

先在独立 target 构建 release example，再执行脚本：

```bash
CARGO_TARGET_DIR=target/refresh-cost-trace cargo build --release --example measure_wsl_compressed_cache
uv run --with zstandard python scripts/measure-wsl-compressed-cache.py --help
```

必须显式传入二进制、发行版与本 checkout Git `agent-logs` 下尚不存在的夹具目录。脚本经真实 `wslpath` 转换，仅在 Windows 创建合成文件，不写发行版文件系统；日志、逐轮数据与夹具保留。example 同样要求 `--fixture-dir`，拒绝越界、目录链接、非普通压缩件及原文件已存在的夹具，并独立核对 Linux 路径对应的宿主夹具；路径核对发生在逐轮计时与 spawn 计数之前。驱动保存子进程超时的已有输出与失败状态，按逐轮 begin/end 标记断言首轮冷解压、后续每轮命中，不能只数整段日志。三轮是机制验证，未清 OS 缓存，也没有足够重复次数估计稳定性能分布，不能当作生产收益 A/B。

以下为 2026-11-12 审查修复前的实跑记录；当时没有逐轮诊断标记，本次核对保留的 report 与 stderr，审查后的独立复核另列于后文，不替换历史样本：Windows 挂载文件系统、隔离 release 构建、`zstandard 0.25.0` 生成不声明帧内容长度的随机合成数据，每个大小独立新进程，进程内连续三轮。全部六轮保持 EOF、零事件且 `bytes_read=0`；每组首次未复用缓存、实际读取并解压，后两轮均命中。表中预取、scan 与内部读 / 解压分开列出，内层不能与 scan 相加。

| 逻辑大小 / 冷读形态 | 首轮 prefetch | 首轮 read | 首轮 decode | 首轮 scan | 后两轮 scan | 各轮创建尝试 |
| --- | ---: | ---: | ---: | ---: | ---: | --- |
| 4 MiB / batch | 671.917 ms | 2338.756 ms | 2.759 ms | 2341.984 ms | 0.093 / 0.062 ms | 3 / 2 / 2 |
| 12 MiB / single | 658.149 ms | 442.227 ms | 5.951 ms | 449.537 ms | 0.121 / 0.051 ms | 3 / 2 / 2 |

这两条合成冷样本的主要成本位于读取窗口，不能据此断言所有真实压缩件的解压成本都低，也不能凭各一条样本比较批读与单件读取的效率。暖轮 scan 虽很短，预取仍需两次桥，实测为 604.169–1417.679 ms；缓存只省掉前置压缩件读取，没有消除元数据核验。此处每组只有一个来源，不能将该预取成本逐文件乘到真实整批。

首跑在扫描前的路径换算阶段退出 1，未形成扫描样本：默认 shell 消耗了 Windows 路径的反斜杠。同路径对照中，`wsl.exe ... -- wslpath` 返回 1，`--exec wslpath` 返回 0；脚本已固定使用直接执行并保存失败状态与 stderr。原失败日志保留，修正后以新目录完成上述六轮，驱动返回 0。越界夹具负对照返回 2，在创建目录和启动 WSL 前拒绝。

### 2026-11-13 审查后复核

当次修复收紧了独立 example 的夹具边界与宿主 / Linux 路径核对，保存两阶段超时和启动失败的已有输出；逐轮 begin/end 验证代替整段计数，并新增 `compressed_call` 分离同源并发日志。2026-11-16 复核发现两处原说明过强：批摘要不能单独区分协议缺答，且旧逐轮计数未核事件顺序与调用关联，订正见后文。生产读取与回退语义不变。

相同环境类别与生成方式，在新夹具目录重新实跑。驱动返回 0、report.complete=true；两组各三轮均逐轮验证：首轮未复用缓存且恰有一次读取和一次解码，后两轮各一次命中且无读取 / 解码。两组全部保持 EOF、零新增字节与零事件。

| 逻辑大小 / 冷读形态 | 首轮 prefetch | 首轮 read | 首轮 decode | 首轮 scan | 后两轮 scan | 各轮创建尝试 |
| --- | ---: | ---: | ---: | ---: | ---: | --- |
| 4 MiB / batch | 366.838 ms | 217.288 ms | 3.461 ms | 221.360 ms | 0.041 / 0.183 ms | 3 / 2 / 2 |
| 12 MiB / single | 417.378 ms | 416.186 ms | 4.465 ms | 421.779 ms | 0.043 / 0.042 ms | 3 / 2 / 2 |

暖轮预取仍为 390.975–419.276 ms。与前日样本的差异不是受控 A/B，不能归因于诊断修复或用于声称提速；本次仅证明修复后完整度断言和两条读取路径成立。

当次 [`test_wsl_compressed_cache.py`](../scripts/test_wsl_compressed_cache.py) 的五项回归通过：正常冷暖序列、相同聚合计数但冷轮错位、部分缺失 / 重复 / 轮外事件、错误读取模式、两个阶段超时与启动失败留证；该轮没有覆盖所有额外事件、调用关联和事件顺序。另对真实量具和驱动分别做越界负对照，均在来源访问前拒绝；驱动在建目录及启动 WSL 前拒绝。验证命令：

```bash
uv run --with zstandard python scripts/test_wsl_compressed_cache.py
```

### 2026-11-16 判据与历史样本复核

复核发现旧逐轮校验只数命中、读取与解码次数，能够接受多余的失败事件、同轮事件调用号不一致或解码先于读取。根因是计数不能表达一条完整调用链。驱动现要求冷轮严格按 `length(cache=not_reused)` → `read(result=found)` → `decode(result=ok)` 排列，三条日志的来源与调用号一致；暖轮只能有一条 `length(result=cache_hit)`。同时核冷暖逻辑长度、跨轮同源与调用号不复用，额外、缺失、错序或错关联均拒绝通过。

八项 Python 回归自身 rc=0，包含正常对照与上述反例；用新校验重新读取 2026-11-13 保留的两组六轮日志仍通过，历史日志和 report 的 SHA-256 前后一致。没有为本次订正重跑 WSL 或替换原始时间表。旧 report 未记录 example 二进制 SHA-256，日志重核也不能补出这项来源绑定。

同日后续 diff 审查补上新实验的 binary 绑定：report 保存运行前 SHA-256，每组调用前后检查同一路径字节，变化则保留 `complete=false` 并拒绝通过；事后摘要一并留证。Python 回归现为九项，自身 rc=0，新增调用前及调用中替换 binary 的反例。未重跑 WSL，旧实验缺失的 binary 摘要仍不可补推。

批摘要另移除恒零的 `unanswered`：底层解析器已经把缺答补为 `Err`，上层再查缺键不可能识别它。缺答保留在 `failed`，确认缺席与非普通文件统一标为 `absent_or_non_regular`，读取及回退语义不变。debug 关闭时不再做批摘要计数和压缩阶段诊断计时；批调用计时仍保留，供失败时 warn 使用。以上是判据与诊断成本订正，没有新的生产性能百分比。

## WSL 身份多步与整批合并的受控对照

2026-11-16 落地 [`wsl::probe_git_roots`](../src/wsl.rs)、[`probe::PrefetchedWslBackend`](../src/probe.rs) 与 [`identity::IdentityPrefetch`](../src/identity.rs)：在发行版内单次脚本中按打开后的普通文件 fd（`/proc/self/fd/3`）读取 `.git`、`commondir` 与 `config` 候选及根目录状态，以 `<tag>\0<path>\0<payload>\0` 二进制安全帧返回，判决逻辑仍完全复用 `identity.rs` 的唯一实现。`sweep_registered_root_identities` 构造阶段仅按 `reach_of` 对未见过的 WSL 根分桶，预算已耗尽时不发起任何桥进程，排在前面的本机根照旧先执行。

旧臂为 `71c58ff` 隔离源码快照（仅原样补入测量入口 [`measure_wsl_identity.rs`](../examples/measure_wsl_identity.rs) 及对应 `Cargo.toml` example 声明，`src/` 未改），新臂为当前最终合并实现；共用同一份 `Cargo.lock` 与工具链编译 release（`--locked --release --features store`）。合成 git 夹具建在当前 checkout 的 Git `agent-logs` 严格子目录下，经 `wsl.exe --exec wslpath` 在计时外转换并双向校验边界。6 个场景、每场景每臂 6 轮（共 72 个独立新进程样本），奇偶轮交替先后，每次调用前后校验两份二进制 SHA-256 未变，未清 OS 缓存；全部 72 次调用的 `RepoIdentity`、`IdentitySweep` 与 `project_roots_report` 逐字段一致，本机报告为 `wsl-identity-ab-20261116-final.json`（`complete=true`）。

| 场景（模式 / 根数） | 旧中位耗时 | 新中位耗时 | 耗时降低 | 旧 / 新桥启动尝试数 |
| --- | ---: | ---: | ---: | ---: |
| `single_standard_repo`（single / 1） | 654.340 ms | 210.447 ms | 67.84% | 3 / 1 |
| `single_linked_worktree`（single / 1） | 1126.567 ms | 182.332 ms | 83.82% | 6 / 1 |
| `single_submodule`（single / 1） | 929.289 ms | 182.794 ms | 80.33% | 5 / 1 |
| `single_plain_dir`（single / 1） | 315.208 ms | 169.794 ms | 46.13% | 2 / 1 |
| `single_missing_checkout`（single / 1） | 351.792 ms | 169.626 ms | 51.78% | 2 / 1 |
| `sweep_mixed_8_roots`（sweep / 8） | 5017.964 ms | 261.207 ms | 94.79% | 30 / 1 |

8 根混合 sweep 含 3 个标准仓（含无 origin 仓）、2 个相对/绝对路径 linked worktree、1 个 submodule、1 个普通目录与 1 个已删 checkout；6 轮耗时全距为旧臂 4979.263–5320.534 ms、新臂 259.280–285.736 ms（类型别名提取前的同构候选轮 `wsl-identity-ab-20261116-clean.json` 亦保留，8 根中位降低 94.35%）。独立 `diagnostic_smoke`（`SVAULT_LOG=sv-wsl=debug`）验证输出 1 条 `identity batch`（`roots=8 snapshots=19 result=ok`）与 8 条 `identity completed`，无原始路径或 remote URL 泄露。以上百分比描述合成夹具上的身份探测子阶段，不外推为生产整轮刷新百分比。

首次实跑在含单引号、`$`、反引号与中文的合成目录上立即暴露出 `71c58ff` 旧臂既有缺陷并退出 1（原始失败报告 `wsl-identity-ab-20261116.json` 保留，`complete=false`）：旧 `wsl::read_file_at` 唯一仍使用 `wsl.exe -d <distro> -- bash -c <script>`，缺少 `--exec` 时先经发行版默认 shell 二次解析 argv，导致引号错配退出 2（`unexpected EOF while looking for matching '''`）。现已将 `read_file_at` 收口至 `run_bash_stdin`（stdin 单层 `bash` + `cat --`），经 `special_chars_smoke`（单桥 209.844 ms）与 `SVAULT_WSL_IT=1` 实机集成测试双路验证通过。复跑与驱动自检命令：

```bash
uv run python scripts/test_wsl_identity_driver.py
uv run python scripts/measure-wsl-identity.py --help
```
