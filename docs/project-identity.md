# 项目身份：从「现算」到「记下来」（ADR-032 的 P2）

> 状态：**已实现**（2026-08-11，store.rs `project_identity` 表 + 5 条验收测试）
> 前置：P1 已落地——`src/identity.rs`（`canonical_repo_id` 等四个纯函数 + 8 条测试）

## P1 解决了什么，没解决什么

P1 把身份**规则**从 QuotaBar 下沉到这里，规则的定义点因此唯一化。
但它**没有**改变身份是怎么来的：仍然是**每次现算**——读磁盘上的 `.git/config`。

于是有一类项目永远算不出身份：

> **checkout 被删除之后，`.git/config` 也没了。**

实测（2026-08-11，QuotaBar 侧）：事件量最大的那个项目**没有别名组**，因为它的 WSL
checkout 已被删除——而总库里还留着它 **161,256 条**历史事件。那些事件都带着
`project_root`，只是没有任何东西能说出它们属于哪个仓库。

⇒ **要让身份活过 checkout 的删除，得在扫描时把它记下来**（那时 `.git` 还在）。

## 挂载点：新表，走 `migrate()`

`tombstones` / `store_meta` 是后加的表，方式是 `migrate()` 里一句
`CREATE TABLE IF NOT EXISTS`——幂等、无版本号、对既有库零风险。身份表照做。

字段全部明文，与 `data_keys` 同级（那张表已经存着 `project_root`）；
加密只作用于 `raw_events.event_json`，本表不涉及那条边界。

```sql
CREATE TABLE IF NOT EXISTS project_identity (
    source_type     TEXT    NOT NULL,
    source_location TEXT    NOT NULL,
    project_root    TEXT    NOT NULL,
    -- `git:<host>/<owner>/<repo>` 或 `path:<git root>`，见 identity.rs
    canonical_id    TEXT    NOT NULL,
    -- 🔴 毫秒，不是秒：last_seen_ms 是**排序键**，而秒级精度下同一秒内观察到的
    -- 两个身份会平局、"取最新" 退化成 "取字母序靠前的"。实现时一条测试当场撞上了它。
    first_seen_ms   INTEGER NOT NULL,
    last_seen_ms    INTEGER NOT NULL,
    PRIMARY KEY (source_type, source_location, project_root, canonical_id)
);
CREATE INDEX IF NOT EXISTS idx_identity_root ON project_identity(project_root);
CREATE INDEX IF NOT EXISTS idx_identity_cid  ON project_identity(canonical_id);
```

## 🔴 主键为什么带 `canonical_id`——不是 latest-wins

最自然的设计是主键只到 `project_root`、身份变了就覆盖（latest-wins）。**那会错，
而且是静默地错。**

身份变更有两种，它们看起来一模一样，后果完全相反：

| 发生了什么                       | 正确处理                     |
| -------------------------------- | ---------------------------- |
| 同一个仓改了 remote / 仓库迁移了 | 新旧身份指同一个项目，可合并 |
| **路径被另一个仓复用**（删了重建、换项目） | 新旧身份是**两个**项目，绝不能合 |

第二种一点也不罕见（`~/workspace/proj` 删掉重新 clone 别的东西）。latest-wins 会把
前一个仓的全部历史事件划到后一个仓名下——**一次路径复用就污染一整段历史**。

区分这两种需要仓库自己的连续性证据（比如首次提交的 hash），而那要 spawn git 或解析
packfile，**代价远超本模块「只读 `.git/config`」的定位**。

所以不区分，**改为不丢信息**：主键带上 `canonical_id` ⇒ 同一个 `project_root` 先后
观察到不同身份就是**两行**，各自带 `first_seen_ms` / `last_seen_ms`。

- **默认消费**：取 `last_seen_ms` 最大的那行 —— 行为等同 latest-wins，调用方不必懂历史
- **需要时可查**：「这个路径的身份变过吗、什么时候」有答案，而不是被覆盖掉

判据是本仓反复用的那条：**降级要降到「说得出来」**。分不清两种变更时，把两条都记下、
让消费方看得见，好过挑一个猜。

## 写入时机与幂等

在 `scan_source` 已经解析出 `project_root` 的地方顺带做一次：`find_git_root` →
`canonical_repo_id` → upsert（命中主键就只更新 `last_seen`）。

三条要求：

1. **算不出身份就什么都不写**，不写 `path:` 兜底行 —— 那种 id 不跨 checkout 稳定，
   记下来只会让「查得到身份」变成一句不能信的话。
2. **每个 (root, 扫描轮次) 至多一次文件 IO**：`.git/config` 的读取按 `git_root` 缓存
   在本轮扫描内，否则一个大仓的几百个会话文件会重复读几百次。
3. **失败不影响扫描**：读不到就跳过并计数，与本模块其余部分一致（身份是**加法**能力，
   它坏了不该让摄取停下）。

## 🔴 P2 解决不了 `/mnt/…` 那一族，别把它算进收益

实测另有 **114,746 条**事件的 `project_root` 是 `/mnt/…` 形式（WSL 里访问 Windows 盘
的 checkout，如 `-mnt-d-mwf-code-EyeVLM`）。它们拿不到身份的原因**不是**「checkout 被
删了」，而是**从 Windows 侧经 UNC 回访 WSL 的 `/mnt/c` 是回环访问、被系统拒绝**。

⇒ 扫描时**同样**读不到 `.git/config` ⇒ P2 记不下任何东西。

这一族的两条出路，都不属于 P2：

- **从 WSL 内部扫描**：那时 `/mnt/c` 是本地路径，`.git` 可读。这是多根扫描的形态问题。
- **路径语义映射**（`\\wsl.localhost\<distro>\mnt\c\…` ≡ `C:\…`）：⚠️ 要先确认
  `wsl.conf` 的 `automount.root` 没被改过 —— **不能猜**，默认值不是保证。

**写在这里是因为「P2 会顺带解决它」是个很自然的误读**，而按上面的分析它不会。

## 验收

1. 扫描一次含 git remote 的项目 ⇒ `project_identity` 有对应行，`canonical_id` 与
   `identity::canonical_repo_id` 一致。
2. **删掉那个 checkout 再扫** ⇒ 行还在、内容不变（这条就是 P2 存在的理由）。
3. 同一 `project_root` 换一个 remote 再扫 ⇒ **两行**，`last_seen_ms` 区分新旧；
   默认查询返回新的那条。
4. 无 remote 的仓 ⇒ **不写任何行**（不写 `path:` 兜底）。
5. `.git/config` 不可读（UNC 回环等）⇒ 不写行、不报错、计数可见。
