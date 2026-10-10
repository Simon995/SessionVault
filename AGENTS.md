<!-- 本仓的 agent 指令。体量 ≤200 行、≤24 KB，由 scripts/check-agents-budget.py 守（pre-push 会跑）。
     判例、历史、会变的数字放 docs/，这里只留一行指针；2026-10-09 瘦身前的全文见 docs/agents-full-2026-10-09.md。 -->
# AGENTS.md

SessionVault 是多个编程 agent 会话的摄取内核与加密总库（Rust crate `session-vault` + CLI `svault`）。
这里只写动手前必须知道、否则会出事的事；它们几乎都来自一个事实：本仓有多个互不知情的消费者。

## 1. 命令与门

- 克隆后先执行 `git config core.hooksPath .githooks`：pre-push 在推送时跑下面两道闸，不过就拒绝推送；没配的克隆不受保护。
- 提交前：`cargo fmt --all`、`cargo test --lib --features store`。feature 组合逐个编：`cargo check --bins`（默认）与 `--features store` 都要过。
- clippy：`cargo clippy --all-targets --all-features -- -D warnings` 保持 0 条。
- 公开仓闸：`uv run python scripts/check-public-safe.py`（扫整个工作区和 `origin/main..HEAD` 的 commit message，先自检再扫）。
- 体量闸：`uv run python scripts/check-agents-budget.py`（本文件 ≤200 行、≤24 KB、单行 ≤320 字符；`CLAUDE.md` 只能是 `@AGENTS.md` 导入壳）。
- 发布 svault：`uv run python scripts/release.py <tag>` 试运行，加 `--publish` 才打标签、发 GitHub Release、装到 `%LOCALAPPDATA%\svault\bin`；二进制里嵌着本机家目录就拒绝发布。
- 变异验证用 `scripts/mutate.py`：变异要打在生产实际走的路径上，脚本自己备份、自己还原并核字节一致。
- 「没问成」在 `src/` 里的规模现查，不写进文档：`grep -rc 没问成 src/ --include=*.rs | awk -F: '{s+=$2} END {print s}'`。

### 提交与分支（不依赖全局指令：只读本文件的环境也要照做）

- 代码、脚本、hook 改动走独立分支，做完请人 review，人同意后才合并推送；纯文档改动可直接提交 main。
- 提交消息用 Conventional Commits：`<type>(<scope>): <subject>`，主题中文、不超过 50 字；正文分 What / Why / Impact / Validation。
- 作者用 GitHub noreply 邮箱，不用工作邮箱；按文件名逐个暂存，不用 `git add -A` / `git add .`；不用 `--no-verify` 跳 hook。

## 2. 公开仓

- 本仓公开。不得出现真实用户名、内部主机名、个人路径、真实项目名、邮箱：散文与注释写 `<user>` / `<distro>` / `<repo>`，测试 fixture 用中性字面量（`dev` / `me` / `u`）。数字保留：脱的是身份，不是证据。
- 别的仓的内部状态（他们的计数、工单号、缺陷清单）不是本仓的证据。引用前问「这个数是我在本仓量的吗」，不是就略去或泛化；闸抓不到这一类。
- 自动生成的托管块（记忆蒸馏一类写进 `CLAUDE.md` / `AGENTS.md` 的块）一律被闸拦下，由人决定删留；匹配字面量只写在 `scripts/check-public-safe.py` 里。
- 已知缺口：闸不查 git 历史里已删文件的路径。故意不补，理由与「何时必须补」见存档「git 历史里的文件路径」那一节。

## 3. 消费者与版本

| 消费者 | 怎么用本仓 | 它拿到的版本由什么决定 |
| --- | --- | --- |
| QuotaBar | Rust 库编进去（`features = ["store"]`）；也是总库会话正文的写入者 | 它的子模块指针 |
| TumeFlow | 子模块 + 把 `svault` 内嵌进 PyInstaller onefile | 它上一次重新冻结的时刻 |
| TumeChat | 只读 CLI（`pull` / `changes` / `sessions-read`） | 本仓发布的那份 svault（2026-10-09 定） |

- 本仓的测试只证明本仓自洽。动 `pub` 项前问「另一个消费者编不编得过」，能编就去那个仓编一次，编不了就明说没验。
- 优先只加不删、不改签名。给公开结构体加字段：字面量构造会编不过，逐字段读取会安静少报一格；收紧 `#[cfg(feature)]` 等于删。
- 不替消费方 bump 它们的子模块指针。
- 运行时会有不止一份 svault 同时打开同一个总库（`<data_local_dir>/svault/total_store.db`），子模块指针拦不住。写入类打开都跑 `migrate()`；只读子命令走 `TotalStore::open_read_only`（不建库、不迁移，库缺表列时报 `SchemaBehind`），新的只读子命令也必须走它。
- 改 schema 前必须回答「落后一版的消费者读不读得动」：核心表做过整表重建，「旧版还读得动」是纪律，不是构造。
- 判断一份 svault 是哪一版看 `svault --version` 括号里的提交（`build.rs` 写入，`-dirty` = 构建时有未提交改动，`unknown` = 构建时没有 git）；`0.0.0` 本身不带信息。消费方应正向断言版本与子命令，不靠「没报错」。
- 新增消费者只有两种姿势：复用已有的一份 svault，或自带一份并同时给出版本协调方案。场景推演见存档「新增一个消费者」那一节。

## 4. 跨会话分工（本节是正文，消费者仓只放指针）

先问：这件事能不能不改本仓就做完？先拆一刀，再提需求。

1. 代码改动在本仓的开发克隆里做：消费者仓的子模块 checkout 有 hook 会拒，且只有这里跑得了全套测试与公开仓闸。
2. 需求由消费方提，带实测和一个可执行的完成判据，不带结论。
3. 各消费方自己 bump 自己的子模块指针。
4. 本仓合并后主动通知每个消费方，不等它们自己发现。
5. 通知里分开报四类消费形状：CLI / Rust 构造与穷举 match / Rust 逐字段读取 / `discover` 按 `SourceMode` 过滤；值域变化与新描述符的 `source_mode` 也要报。

判例（同一批提交对三类消费方三个答案、规则 4 补的缺口）见存档「跨会话分工」。

## 5. 消费方手上的路径：换算归本仓（2026-09-29 定）

本节是正文，消费者仓只放指针。WSL 写法的路径（`/mnt/<盘>/…`、`/home/…`）被 Windows 进程当成本机路径用，会读成当前盘的相对路径；配上「不在就建」就会静默建出幽灵目录。消费方手上的路径（会话 cwd、记忆里的项目路径）：

1. 不当本机路径直接开：用 `svault attribute` 认，打开用命中的根在 `svault roots` 里的 `host_path`。
2. 认不出就不写、报出来；`unknown`（含 `mounts_needed`）是「这一轮说不出」，不是「不是项目」，两者分开报。
3. 不在消费方另写换算：不按盘符猜，不拿叶子目录名当「同一个项目」的证据；本仓的换算有挂载表、有三态（`pathnorm::reach_of` 的 `Unknown` ≠ 本机）。
4. 路径来自别人的数据时，不许「不在就建」（`mkdir -p` / `exist_ok=True`）。

目录已搬走或删掉的会话，身份用 `svault session-origin`：回源读会话记下的远端，与 `roots` 的 `canonical_id` 走同一个出口（`identity::git_id_of_remote`），只有 codex 会话记远端。

CLI 没有「任意路径 → 宿主写法」的出口，缺什么按规则 2 提，别在消费方补。

## 6. 写代码时的判据

- 没做成的动作和做成了但结果为空，输出一样。每写一个「空 / false / 0 / None」先问它属于哪一边；分不出就加变体、字段或计数（如 `Probed` 的 `Unknown` ≠ `Absent`）；故意压掉时写明压过一个三态。
- 判据能进类型就别只写在注释里，消费者不读注释：例如 `PriorProjection` 只能经 `ask()` / `no_store()` 构造。
- 说「编译器守不住」之前先穷尽换写法（读 → 穷举解构、`bool` → 类型、注释 → 私有字段）；确实不能才降级为机械可核对，并写清为什么。
- `compile_fail` doctest 任何编译错误都算过：配一条必须编得过的对照，并做变异。
- 不给测试留后门（`present_for_test()` 之类）：下一个人会照着测试写生产代码。
- `std::fs` 的观察面由 `clippy.toml` 整体禁掉，存在性与读文件一律走 `probe`。
- 修完一个判据，立刻 grep 全仓问「这个判据还有第几处没换」。

## 7. WSL 与项目身份（改 `probe` / `identity` / `pathnorm` / `discover` 前读）

- `\\wsl.localhost\…` 上宿主的答案只有 `Dir` / `File` / `Absent` 是事实；`Found(Other)` 与 `Unknown` 几乎总是宿主跟不进的符号链接，改用 `probe::WslUncBackend`。判据是「这条路径归谁管」，不是「UNC 通不通」。
- `wsl::stat` 问的是 `[ -f ]`，每个目录都会被它报成不存在；问目录用 `stat_kind`。
- 「该问谁」只有 `pathnorm::reach_of` 一处实现；`RootReach::Unknown` 不是本机。
- 身份是根的属性：`project_identity` 主键不带 `source_type` / `source_location`；身份由注册表驱动，不由事件驱动；别改注册表的多写法，也别给 `path:` 加兜底。
- 细节与判例见 `docs/project-identity.md`，以及存档「宿主答不了发行版内部的事」「身份是根的属性」。

## 8. 文档索引

- 是什么：`README.md`；设计契约：`docs/INGEST_KERNEL.md`；字段对账：`docs/rawevent-reconciliation.md`；扫描 / 投影状态模型：`docs/scan-state-model.md`。
- 项目归属与身份：`docs/project-attribution.md`、`docs/project-identity.md`；日志：`docs/LOGGING.md`；与 QuotaBar 的对拍契约：`docs/parity-contract.md`。
- 总库体积与待优化项（存了什么、哪些可回收，只记录未动手）：`docs/storage-growth.md`。
- 总库在多台机器之间同步（前提、2026-10-09 / 10-10 的决定、方案与未决项）：`docs/linux-replica.md`。
- 存档：本文件 2026-10-09 瘦身前的全文（判例、历史、细节），`docs/agents-full-2026-10-09.md`，不再维护。
