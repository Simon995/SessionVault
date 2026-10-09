# 总库同步到 Linux 主机：前提、决定与方案

> **状态（2026-10-09）**：方案定稿、未动代码。目标主机未定，复制本身等主机定了再接；
> 不依赖主机的三件（只读打开、Linux 密钥来源、SV 发布 svault）先做，用 WSL 当 Linux 验证。
> 数字与「现在如何」停在当天，动手前按文中方法重新量。

## 需求（消费方提，2026-10-09）

个人 AI 的核心放在一台 Linux 主机上，总库要同步过去；读者是一个只读的检索消费方
（`pull` 建索引、`changes` 跟替换、`sessions-read` 核原文）。消费方给的判据：

1. Linux 上 `sessions-read` 读到的某个会话与 Windows 总库**逐事件一致（含 `seq`）**；
2. Windows 侧继续写入后，**N 分钟内** Linux 侧读得到新事件。

本仓补一条：3. Windows 上 `svault erase` 删掉的内容，N 分钟内 Linux 副本里也查不到。

## 实测前提（2026-10-09）

| 事实 | 怎么核的 | 对方案的含义 |
| --- | --- | --- |
| 主密钥在 OS 密钥链（`keyring`，账号 `total-store-master-v1`），数据密钥包裹后存库内 `data_keys` | 读 `src/store_crypto.rs` | 只拷库文件不带主密钥，一条都读不出 |
| 库里已有加密行而密钥链没有主密钥时报 `MissingKey`，不会新建一把错钥 | 读 `TotalStore::open`（`store.rs`「`None if encrypted_store`」分支） | 拷库不会悄悄毁掉库，但也打不开 |
| `keyring` 在 Linux 上默认走 Secret Service；无桌面 Ubuntu 里建空库失败：`OS keychain: Platform secure storage failure: no secret service provider or dbus session found` | WSL 里编 Linux 版 svault，`HOME` 指空目录跑 `sync-snapshots --store /tmp/…` | 无桌面 Linux 上正式版 svault 连库都建不出 |
| 每次打开总库都跑 `migrate()`，包括 `open_existing` 这种只读打开 | 读 `open` → `from_conn` → `migrate()` | 比写入方新的只读 svault 一打开就可能迁移共享库 |
| `pull` 每行是 `offset` + 完整事件，事件本身不带投影修订号；`changes` 发投影替换记录 | 读 `Out::Pulled` / `Out::ProjectionReplaced` | 复制要用 `--projection current` 加 `changes` 按来源替换 |
| `erase` 只有库内 `tombstone()`，没有对外出口 | grep `src/bin/svault.rs` | Windows 上的删除同步不到副本，要补一个出口 |
| 本仓没有任何发布物（无 GitHub Release、无 CI） | `gh release list`、`.github/` | 消费方拿 svault 只有「内嵌在别的产品里」一条路 |

外部输入（会过期）：Claude Code 默认清理 30 天以前的会话记录（`cleanupPeriodDays`），
早期会话在本机常常只剩总库这一份 —— Linux 副本也因此兼作备份，不能靠重扫原始文件重建。

## 决定（2026-10-09 人拍板）

1. **svault 由本仓发布和更新**（Windows / Linux 两个版本），消费方只读发布物的路径，不各自内嵌。
2. **无桌面 Linux 的主密钥用受保护的密钥文件**；Windows 照旧用 OS 密钥链。
3. **增量复制**：Linux 副本是一个独立总库，用自己的主密钥重新加密；主密钥不离开 Windows。
4. **先做不依赖主机的部分**，复制本身等主机定了再接。

## 方案

### 一、只读打开不迁移（已做，2026-10-09）

> 实现：`TotalStore::open_read_only` / `open_read_only_with_key`；CLI 的 7 个只读子命令改用它。
> 「应有的表结构」取自本程序自己的 `migrate()`（内存库跑一遍再比），不另抄一份。
> 守护：`store.rs` 三条单元测试 + `tests/cli_read_only_it.rs`（真实进程，7 个子命令逐个验）。

- 新增只读打开入口：不建库、不建密钥、不跑 `migrate()`；库的表结构与本程序不一致时**明确报错**，
  分开报「库比我新」与「库比我旧」，不静默。
- 只读子命令（`pull` / `changes` / `sessions-read` / `sessions-recent` / `snapshots` / `roots` /
  `attribute` / `session-origin` 等）改用它；写入类（`scan-all --write-store` / `sync-snapshots` / `erase` / `gc`）不变。
- 判据：比库新的 svault 跑只读命令后，库的表结构与 `store_meta` 逐字节不变；比库旧的明确报错。

### 二、Linux 主密钥来源（已做，2026-10-09）

- `SVAULT_KEY_FILE=<路径>` 设了就从该文件取主密钥，不碰 OS 密钥链；没设时行为不变。
- 首次建库在该路径生成密钥文件，建出来就是 `0600`；对组或其他用户开放了权限就拒绝；
  已有加密数据而文件缺失时报 `MissingKey`，不重新生成。内容格式与现有 `StoreKey` 编码一致。
- 验证：无桌面 Ubuntu（WSL）里端到端建库、只读打开、`644` 被拒、丢失不重生成都按预期；
  不设该变量时仍是 Secret Service 那条报错（对照）。
- 部署建议（主机定了再落）：用 systemd 的加密凭据把密钥文件交给服务，不明文落盘。

### 三、本仓发布 svault（脚本已做，2026-10-09；渠道人定为本机脚本 + GitHub Release）

- `scripts/release.py <tag>`：本机编 Windows 版、WSL 里编 Linux 版；`--version` 必须正好是当前提交、
  不带 `-dirty`；编译时把家目录映射成 `~`，编完逐字节扫描，二进制里嵌着本机家目录就拒绝发布
  （反向对照：未映射的构建里有上千处，会被拦下）。默认试运行；`--publish` 只认 origin/main 上干净的提交，
  打标签、发 GitHub Release（附 `SHA256SUMS` 与 `Cargo.lock`）、把 Windows 版装到 `%LOCALAPPDATA%\svault\bin`。
- 消费方用配置的显式路径，启动时正向断言版本与所需子命令；本仓合并影响 CLI 形状的改动后按规则 4 通知。
- Linux 主机上的安装位置等主机定了再定。

### 四、增量复制（主机定了再做）

- 副本侧维护「源 offset 水位」：`pull --since <水位> --projection current` 拉新事件，保留源 `seq` 与来源键；
  `changes --since-seq` 收到替换时按来源重取当前代；新增一个墓碑出口传播 `erase`。
- 项目注册与身份表（`roots` 的内容）也随之同步：它们依赖 Windows / WSL 的路径与挂载表，Linux 上算不出来。
- 第一版只读副本，单一写入方（Windows）；Linux 本机的会话要不要也进总库，另议。
- 未决：传输方向（Linux 拉 Windows 需要 Windows 侧 SSH 服务；或 Windows 推送）、目标主机、N 取多少。
