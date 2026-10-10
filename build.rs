//! 把构建时的 git 提交写进 `svault --version`。
//!
//! `Cargo.toml` 的版本号从不 bump，单靠它分不出一个落后一个月的构建。
//! 🔴 提交号必须跟着 HEAD 与源码刷新 —— 过期的提交号比 `0.0.0` 更糟。

use std::process::Command;

/// 参与构建的路径：`-dirty` 只看它们，重跑也只盯它们。
const BUILD_INPUTS: [&str; 4] = ["src", "Cargo.toml", "Cargo.lock", "build.rs"];

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn main() {
    let mut status = vec!["status", "--porcelain", "--"];
    status.extend(BUILD_INPUTS);
    let commit = match (git(&["rev-parse", "--short=12", "HEAD"]), git(&status)) {
        (Some(sha), Some(changes)) if changes.is_empty() => sha,
        (Some(sha), _) => format!("{sha}-dirty"),
        (None, _) => "unknown".to_string(),
    };
    println!("cargo:rustc-env=SVAULT_BUILD_COMMIT={commit}");

    // 发布号由 `scripts/release.py` 经 `SVAULT_RELEASE_TAG` 传进来（`v2026.10.10.1` → `2026.10.10.1`）。
    // 从源码自己编的（开发克隆、消费方的子模块）没有它，记成 `dev`：它不是哪一次发布，是哪一个
    // 构建只看括号里的提交。
    println!("cargo:rerun-if-env-changed=SVAULT_RELEASE_TAG");
    let release = match std::env::var("SVAULT_RELEASE_TAG") {
        Ok(tag) if !tag.trim().is_empty() => {
            let tag = tag.trim().trim_start_matches('v').to_string();
            assert!(
                tag.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-'),
                "SVAULT_RELEASE_TAG 只能含字母、数字、点和横线：{tag:?}"
            );
            tag
        }
        _ => "dev".to_string(),
    };
    println!("cargo:rustc-env=SVAULT_RELEASE={release}");

    for input in BUILD_INPUTS {
        println!("cargo:rerun-if-changed={input}");
    }
    // HEAD 本身，以及它指向的分支引用（分离 HEAD 时没有后者）。子模块的 git 目录
    // 不在 `.git/` 下，所以路径问 git 要，不自己拼。引用被 gc 打包后文件不存在，
    // cargo 会每次重跑 —— 慢一点，但不会过期。
    let head_ref = git(&["symbolic-ref", "-q", "HEAD"]);
    for name in ["HEAD"].into_iter().chain(head_ref.as_deref()) {
        if let Some(path) = git(&["rev-parse", "--git-path", name]) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
}
