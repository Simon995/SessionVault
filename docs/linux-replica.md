# 总库在多台机器之间同步：前提、决定与方案

> **状态（2026-10-10）**：不依赖主机的三件已做完 —— 只读打开（`fe89ce9`）、Linux 密钥来源
> （`7b474d4`）、本仓发布 svault（脚本 `83ec9df`，首个发布物 `v2026.10.09`）。10-10 需求扩成多台机器
> 都写都读，方案改为「每台只写自己的库、经中转整库镜像、读时合并」（第四部分），先验证 `sqlite3_rsync`。
> 数字与「现在如何」停在当天，动手前按文中方法重新量。

## 需求（消费方提，2026-10-09）

个人 AI 的核心放在一台 Linux 主机上，总库要同步过去；读者是一个只读的检索消费方
（`pull` 建索引、`changes` 跟替换、`sessions-read` 核原文）。消费方给的判据：

1. Linux 上 `sessions-read` 读到的某个会话与 Windows 总库**逐事件一致（含 `seq`）**；
2. Windows 侧继续写入后，**N 分钟内** Linux 侧读得到新事件。

本仓补一条：3. Windows 上 `svault erase` 删掉的内容，N 分钟内 Linux 副本里也查不到。

**2026-10-10 扩充**：机器不止一台 —— 两台 Windows、一台 Linux，不在同一网络；**每台都会产生会话，
每台都要读到全部会话**。机器之间经用户自有的一台服务器中转。以后可能有其他用户各自同步自己的设备；
用户之间不合并。

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

2026-10-10 人拍板（需求扩成多机之后）：

5. **传输改为整库镜像**：用 SQLite 官方的 `sqlite3_rsync` 经 SSH 与中转服务器同步，取代决定 3 的
   「逐事件增量复制、副本另用主密钥」。主密钥因此要放到用户的每台设备上（中转服务器上没有）。
6. **多台机器都写，汇总到一起**：每台只写自己的库，别人的库是只读副本，读时合并（第四部分）。

不用同步盘（Dropbox 一类）的理由：同步数据库文件 —— 有写事务时复制库文件会得到一半旧一半新的坏库，
`-wal` 也必须与库文件一起复制，而同步盘逐个文件上传（sqlite.org「How To Corrupt An SQLite Database
File」§1.2，2026-10-10 查证）；同步原始会话文件 —— 会话工具按期清理旧记录，删除会同步过去，
而早期会话往往只剩总库这一份；原始文件还是明文。

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

### 四、多机：每台只写自己的库，经中转整库镜像，读时合并（2026-10-10 定，未动手）

冲突靠构造避免：一条会话只产生在一台机器上，只有「两台写同一个库」才会冲突，所以不让两台写同一个库。

- **布局**：中转服务器上每台机器一个库（`<目录>/<机器标识>.db`）。每台把自己的库推上去，把别的机器的
  拉到本机的副本目录。`sqlite3_rsync` 一次同步一个库；同步期间副本只读、可以照常查询；源库可以同时在写。
- **密钥**：每个用户一把主密钥，他的所有设备共用；新设备导入一次。中转服务器上没有主密钥，看不到
  `event_json`（正文密文），但看得到明文列：`raw_events` 的 `source_path` / `project_root` / 会话 ID / 时间，
  `project_identity` 的 `canonical_id`，注册表里的根路径。
- **合并**：同一个项目按 `canonical_id`（git 远端）归并，没有远端的按机器分开；同一会话 ID 出现在两个库里时去重。
- **删除**：`erase` 的删除记录写在执行者自己的库里，各端读时都遵守；会话所属的机器下次运行时删掉原文。
- **要做**：① 库里记下属于哪台机器（只加不删）；② 主密钥导出 / 导入（直接写成受保护的文件，不上屏）；
  ③ 读命令把本机库与副本合起来读，输出带机器标识，游标按机器分（TumeChat 要跟着改）；④ 跨库删除；
  ⑤ 同步脚本与各机定时任务（中转服务器只需 SSH 账号与 `sqlite3_rsync`）。
- **先验证**（不过就重新考虑整个方案）：`sqlite3_rsync` 对本库（WAL、GB 级、同步时源库在写）可用；
  Windows / Linux 两端都有可用的二进制且能互通。
- **以后多用户**：数据组织不变，只换传输 —— 整库镜像让中转看得到元数据，不适合替别人托管；
  产品化时改为设备端把新增内容加密成小包、放到只存文件的存储上，中转自始至终只见密文。
- 未决：中转上的目录与账号、N 取多少、各机定时任务的形式。
