//! 一次性探针：对给定的 project root 现算身份，**不碰总库**。
//!
//! `svault roots` 读的是**已存的**注册表（身份在扫描时写入），所以它看不出解析
//! 规则的改动 —— 要验规则本身就得现算一次。
//!
//! 🔴 **路径从文件读，不从 argv 拿。** Git Bash 会把 `/home/…` / `/mnt/…` 这类
//! POSIX 形参数改写成 `C:/Program Files/Git/home/…` —— 实测第一版就是这么把三个
//! 真实的根量成了不存在的路径。同「Git Bash 传参进 wsl.exe 会被篡改」那条判例。
//!
//! 用法：
//! - `cargo run --example identity_probe --features store -- <每行一个根的文件>`
//!   —— 现算规则，**不碰总库**。
//! - `cargo run --example identity_probe --features store -- --store <总库副本>`
//!   —— **端到端**：跑一轮身份扫描，再按 `svault roots` 的口径打印判决直方图。
//!   那正是 task #56 的判据（三种「没有身份」看不看得出区别）。
//!
//! 🔴 **`--store` 必须给副本，不能给真库** —— 它会写（身份行 + 判决行）。
//! 探针自己拦不住这件事：一个路径是不是副本，只有拿它的人知道。
use session_vault::deadline::Deadline;
use session_vault::probe::{LocalBackend, ProbeBackend, Probed};
use std::time::Duration;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--store") {
        let Some(path) = args.get(2) else {
            eprintln!("用法: identity_probe --store <总库副本路径>");
            std::process::exit(2);
        };
        probe_through_the_store(path);
        return;
    }
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

/// 端到端：扫一轮 → 按报告的口径逐根打印判决。
///
/// 运行期事实（`default_distro` / `drive_mounts`）**照 QuotaBar 那三行取**
/// —— 少了它们，裸 Linux 路径与 `/mnt/<drive>/…` 会因为别的原因判成「没问成」，
/// 那时量到的数字说不了本轮改动的事。
fn probe_through_the_store(path: &str) {
    let d = Deadline::after(Duration::from_secs(300));
    let store = match session_vault::store::TotalStore::open(std::path::Path::new(path)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("打不开总库副本 {path}: {e}");
            std::process::exit(2);
        }
    };
    let distro = session_vault::wsl::list_distros(d)
        .ok()
        .and_then(|ds| session_vault::wsl::default_distro(&ds));
    let mounts = match distro.as_deref() {
        Some(x) => session_vault::wsl::drive_mounts(x, d).unwrap_or_default(),
        None => Vec::new(),
    };
    let sweep = store.sweep_registered_root_identities(distro.as_deref(), &mounts, d);
    println!("sweep: {sweep:?}");

    // 🔴 报告失败**当场退出**，不渲染成一个说得出口的数字（诊断探针的纪律）。
    let (roots, _) = match store.project_roots_report(&Vec::new()) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("project_roots_report failed: {e}");
            std::process::exit(2);
        }
    };
    let mut hist = std::collections::BTreeMap::<&str, usize>::new();
    for r in &roots {
        *hist.entry(r.identity_verdict.as_str()).or_default() += 1;
        println!(
            "{}\t{}\t{}\t{}",
            r.identity_verdict.as_str(),
            r.canonical_id.as_deref().unwrap_or("-"),
            r.identity_verdict.why().unwrap_or("-"),
            r.root_path,
        );
    }
    println!("---- {} roots: {hist:?}", roots.len());
}
