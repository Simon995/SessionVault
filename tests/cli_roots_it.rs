//! 集成测试造 fixture 要直接碰盘 —— 边界管的是生产行为。
#![allow(clippy::disallowed_methods)]

//! `svault roots` 的真实进程验收（#40 步 1）。
//!
//! # 为什么单元测试不够
//!
//! `store::project_roots_report` 的单元测试钉的是**读**：读到了什么、读不到时报不报错。
//! 它们证明不了 CLI 那一层 —— 子命令有没有接进 `main` 的分发、`for` 循环有没有真的
//! 把行发出去、summary 的计数与实际发出的行数是不是同一个来源。
//!
//! 🔴 **这三样恰恰是会静默坏掉的那种**。一个只发 summary、不发行的实现，输出看起来
//! 完全正常（`{"kind":"roots_summary","roots":94,…}`），消费方读到的却是「有 94 个根，
//! 但我一行都没看到」—— 而它多半会把这读成「拿不到明细，那就自己发现吧」，正好把
//! 这个命令要消除的第二份实现请回来。
//!
//! 本仓已经栽过同形的一次：装机的 `svault.exe` 落后一个月、缺 5 个子命令，两个调用点
//! 都 `catch` 后静默降级，界面全程正常，持续一个多月（见 QuotaBar AGENTS.md
//! 「外部二进制的契约在打包时验」）。**判据是跑它、问它会什么，不是 `Test-Path`。**
//!
//! # 跑法
//!
//! ```text
//! cargo test --features "store acceptance-fixtures" --test cli_roots_it
//! ```
//!
//! `SVAULT_ACCEPTANCE_KEY` 只在 `acceptance-fixtures` + `debug_assertions` 下被读取
//! （见 `bin/svault.rs::open_total_store`），所以这个测试也只在那个组合下编译 ——
//! 正式发布的二进制里既没有这个环境变量口子，也没有这条测试。

#![cfg(all(feature = "store", feature = "acceptance-fixtures", debug_assertions))]

use std::process::Command;

use session_vault::attribution::RootSource;
use session_vault::{StoreKey, TotalStore};

/// 43 个 `A` = 32 个 0 字节（base64 no-pad）。**这不是密钥模式的示范**，只是让
/// 建库的一端与读库的一端拿到同一把测试密钥；两端都走 `from_encoded`，于是不必
/// 把 `StoreKey::encode`（私有）暴露出来。
const TEST_KEY: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

/// 一个本测试专属的空目录。
///
/// 刻意不引 `tempfile` —— 本仓的 `[dev-dependencies]` 是空的，为一条测试给一个
/// **公开仓**添依赖是笔要还的账。名字带测试名 + pid 以免并行跑时互相踩，
/// 开头清一次保证可重复。
fn scratch(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("svault-it-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn run_roots(store: &std::path::Path) -> (i32, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_svault"))
        .env("SVAULT_ACCEPTANCE_KEY", TEST_KEY)
        .args(["roots", "--store"])
        .arg(store)
        .output()
        .expect("spawn svault");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

fn rows(stdout: &str) -> Vec<serde_json::Value> {
    stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str::<serde_json::Value>(l).expect("每一行都要是合法 JSON"))
        .collect()
}

/// 🔴 **每一个根都要真的作为一行发出来，而不是只报个数。**
#[test]
fn every_registered_root_is_actually_emitted_as_a_row() {
    let path = scratch("emitted").join("s.db");
    {
        let store =
            TotalStore::open_with_key(&path, StoreKey::from_encoded(TEST_KEY).unwrap()).unwrap();
        store.register_project_root("/home/u/proj", RootSource::Git);
        store.register_project_root(r"D:\work\code\other", RootSource::Marker);
        store.register_project_root("wsl:Ubuntu-22.04:/home/u/third", RootSource::Configured);
    }

    let (code, stdout) = run_roots(&path);
    assert_eq!(code, 0, "读得到就该成功退出：{stdout}");

    let rows = rows(&stdout);
    let roots: Vec<_> = rows
        .iter()
        .filter(|r| r["kind"] == "project_root")
        .collect();
    assert_eq!(roots.len(), 3, "三个根要发三行，不能只发 summary：{stdout}");

    // summary 的计数与实际发出的行数必须一致 —— 否则消费方拿到的是
    //「说有 N 个，只看见 M 个」，而它无从判断该信哪个。
    let summary = rows
        .iter()
        .find(|r| r["kind"] == "roots_summary")
        .expect("必须有收尾摘要");
    assert_eq!(
        summary["roots"].as_u64().unwrap() as usize,
        roots.len(),
        "summary 的计数不得与实际行数脱节"
    );
    assert_eq!(
        summary["attribution_revision"].as_i64().unwrap(),
        3,
        "三个新根 ⇒ 修订号 3；它是消费方的缓存失效锚，必须在 summary 里"
    );

    for r in &roots {
        for field in [
            "root_key",
            "root_path",
            "root_source",
            "first_seen_ms",
            "last_seen_ms",
            "aliases",
        ] {
            assert!(!r[field].is_null(), "{field} 缺失：{r}");
        }
        // `canonical_id` 可以是 null（说不出身份是诚实答案），但**键必须在** ——
        // 缺键与 null 在消费方那边不是一回事：前者是「这个版本不懂身份」。
        assert!(
            r.get("canonical_id").is_some(),
            "canonical_id 键必须存在（可为 null）：{r}"
        );
    }

    // 🔴 等价写法要跨过进程边界到达消费方 —— 这才是它存在的意义。
    // 一个 Windows 上的消费方枚举出 UNC 形，注册表存的是规范形，`==` 一比就是两个项目。
    let wsl_row = roots
        .iter()
        .find(|r| r["root_path"] == "wsl:Ubuntu-22.04:/home/u/third")
        .expect("规范形那条要在");
    assert_eq!(
        wsl_row["aliases"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect::<Vec<_>>(),
        vec![r"\\wsl.localhost\Ubuntu-22.04\home\u\third"],
        "Windows 侧能打开的那种形式必须发出来"
    );
    // 纯 Windows 路径没有第二种写法：空数组，不是把自己复制一份。
    let win_row = roots
        .iter()
        .find(|r| r["root_path"] == r"D:\work\code\other")
        .unwrap();
    assert_eq!(win_row["aliases"].as_array().unwrap().len(), 0);

    // 两个字段都给，且 `root_path` 是**原始形式**（归一化只用于比较键）。
    //
    // ⚠️ 判据不能写成「每一行 key ≠ path」：一个本来就是小写正斜杠无尾斜杠的
    // POSIX 路径，归一化后与自己相同 —— 那是**正确的**，不是缺陷。正向断言两条
    // 具体的映射，比一个恒不等的直觉强。
    let key_of: std::collections::BTreeMap<&str, &str> = roots
        .iter()
        .map(|r| {
            (
                r["root_path"].as_str().unwrap(),
                r["root_key"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        key_of[r"D:\work\code\other"], "d:/work/code/other",
        "Windows 形式：原始形式原样保留，比较键小写正斜杠 —— 两者都要给，\
         只给后者会逼消费方自己再归一化一遍"
    );
    assert_eq!(
        key_of["/home/u/proj"], "/home/u/proj",
        "已是规范形式时两者相同，这不是缺陷"
    );
    assert_eq!(
        roots
            .iter()
            .find(|r| r["root_path"] == r"D:\work\code\other")
            .unwrap()["root_source"],
        "marker",
        "来源标签如实透传"
    );
}

/// 「读到了、没有根」是成功；空库不是错误。
#[test]
fn an_empty_registry_is_a_successful_empty_answer() {
    let path = scratch("empty").join("s.db");
    drop(TotalStore::open_with_key(&path, StoreKey::from_encoded(TEST_KEY).unwrap()).unwrap());

    let (code, stdout) = run_roots(&path);
    assert_eq!(code, 0);
    let rows = rows(&stdout);
    assert!(
        !rows.iter().any(|r| r["kind"] == "project_root"),
        "没有根就不该有行"
    );
    let summary = rows.iter().find(|r| r["kind"] == "roots_summary").unwrap();
    assert_eq!(summary["roots"].as_u64().unwrap(), 0);
}

/// 🔴 **库不在 ⇒ 非零退出，且不得输出一份「成功的空清单」。**
///
/// 这条与上一条是**一对**：同样是「没有行」，一个必须成功、一个必须失败。消费方
/// 只能靠退出码把它们分开 —— 若两者都退 0，「这台机器上没有项目」与「我没问成」
/// 在它那边长得一模一样。
#[test]
fn a_missing_store_fails_loudly_instead_of_reporting_zero_roots() {
    let (code, stdout) = run_roots(&scratch("missing").join("nope.db"));
    assert_ne!(code, 0, "库不存在必须非零退出");
    assert!(
        !stdout.contains("roots_summary"),
        "失败时不得发出摘要 —— 那会被读成「确实没有根」：{stdout}"
    );
}
