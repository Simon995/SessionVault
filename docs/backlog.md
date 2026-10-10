# 待做清单

> 最后整理：2026-10-10。这里只放「还没做的」与「做到哪」，判据与背景在各项指向的文档里。
> 做完一项就从这里划掉并写上提交号；改了先后次序在这里改，别在多处各写一份。
> 标记：**[人]** 要人做或人拍板；**[本仓]** 本仓动手；**[等]** 等外部条件。

## A. 多机同步（个人用，决定见 [linux-replica.md](linux-replica.md)）

1. **[人]** 公司电脑、家里 Linux、家里 Windows 装 Tailscale、同一账号登录；家里 Linux 开 SSH、常开机。
   装好后给出家里 Linux 在组网里的地址或机器名。
2. **[本仓]** 实测公司 → 家里能否直连（不经中继）与上下行速度；据此定首次推送怎么做。
3. **[本仓]** `svault sync` 子命令：推 → 中转上核对 → 拉 → 本机核对 → `adopt-erasures`；每步重试、加锁；
   Windows 上 `--ssh` 指 Git 自带的 ssh。中转只是一个 SSH 目标，换成家里 Linux 不改代码。
4. **[本仓]** 发布物带上 `sqlite3_rsync`（Windows 版、按官方编译参数从源码编的 Linux 版）。
5. **[人]** 家里两台：装 svault；`key-import --rekey` 换到共用主密钥（**先退出 QuotaBar / TumeFlow**）。
6. **[本仓 + 人]** 各机定时任务（Windows 任务计划 / systemd timer），同步间隔 N。
7. **[人]** 境外测试中转（专用账号、`relay/`、`sqlite3_rsync` / `sqlite3`）的去留。

## B. 发版

- **[人拍板]** `v2026.10.10`（`6cd6bd1`）之后合并的都还没进发布物：合并读取、跨库删除、UNC 写法归属、
  `open_existing` 只读、按项目根删覆盖各种写法。TumeChat 接合并读取与删除要用新版。

## C. 消费方提的需求（规则 2）

- **[可选]** QuotaBar【3】：`discover_transcripts_reported` 一个发行版起 5 次 `wsl.exe`，判据是 ≤ 发行版数、
  结果逐字节不变；收益小（每次约 0.8 s）。
- **[人拍板]** 总库里 855 条事件的 `project_root` 还是 UNC 写法（7 个值）：归属与删除已认得出它们，
  字段值要等来源重投影才更新。要不要做一次定向重投影。
- **[等]** TumeChat 要按项目收窄、但不自己换算路径（它的 ADR-015）：等它按规则 2 提「项目标识」的需求。

## D. 产品化（决定 9：iroh）

- **[本仓]** 个人同步跑通之后：用 iroh 把点对点同步做进 svault，只替换传输层；新设备拿钥匙
  （同步密码 / 设备配对 / 恢复码）一起做。设计在 [linux-replica.md](linux-replica.md)「以后给别的用户」。

## E. 存储体积

- **[人说了再做]** 见 [storage-growth.md](storage-growth.md)：`gc` + `VACUUM`、`history.jsonl` 改增量、
  旧代审计清理、入库前压缩。

## F. 测试与工具

- **[本仓]** `open_existing_returns_the_store_when_it_is_there` 在无桌面 Linux 上失败于准备步骤
  （`TotalStore::open` 要系统钥匙链）—— 改成钥匙来源可指定，让它在 Linux 上也跑得了。
- 本仓没有 CI：Linux 专属分支（非 Windows 的桩、Unix 文件权限）只能在 WSL 里用 Linux 工具链手动跑。
