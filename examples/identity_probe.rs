//! 一次性探针：对给定的 project root 现算身份，**不碰总库**。
//!
//! `svault roots` 读的是**已存的**注册表（身份在扫描时写入），所以它看不出解析
//! 规则的改动 —— 要验规则本身就得现算一次。
//!
//! 🔴 **路径从文件读，不从 argv 拿。** Git Bash 会把 `/home/…` / `/mnt/…` 这类
//! POSIX 形参数改写成 `C:/Program Files/Git/home/…` —— 实测第一版就是这么把三个
//! 真实的根量成了不存在的路径。同「Git Bash 传参进 wsl.exe 会被篡改」那条判例。
//!
//! 用法：`cargo run --example identity_probe --features store -- <每行一个根的文件>`
use session_vault::deadline::Deadline;
use session_vault::probe::{LocalBackend, ProbeBackend, Probed};
use std::time::Duration;

fn main() {
    let Some(list) = std::env::args().nth(1) else {
        eprintln!("用法: identity_probe <每行一个 project root 的文件>");
        std::process::exit(2);
    };
    // 走模块自己的原语 —— `std::fs::read_to_string` 被存在性边界的闸禁着
    // （clippy.toml），而它**当场抓到了这个 example**。顺带自食其力：用的正是
    // 本轮加到 `ProbeBackend` 上的 `read_text`。
    let host = LocalBackend::unanchored();
    let text = match host.read_text(std::path::Path::new(&list), Deadline::unbounded()) {
        Probed::Found(t) => t,
        // 🔴 诊断探针不许把失败渲染成一个能说出口的值 —— 当场退出。
        Probed::Absent => {
            eprintln!("清单文件不存在: {list}");
            std::process::exit(2);
        }
        Probed::Unknown(e) => {
            eprintln!("读不了清单文件: {e}");
            std::process::exit(2);
        }
    };
    let mounts = Default::default();
    for root in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let d = Deadline::after(Duration::from_secs(30));
        match session_vault::identity::repo_id_for_root(root, None, &mounts, d) {
            Ok(id) => println!("OK\t{}\t{root}", id.id),
            // 🔴 `Err` 是「没问成」，不是「没有身份」—— 两者的下游动作相反。
            Err(e) => println!("UNKNOWN\t{e}\t{root}"),
        }
    }
}
