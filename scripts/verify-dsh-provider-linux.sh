#!/usr/bin/env bash
# Linux 全套 DSH 验收；只用隔离构建目录、合成来源与文件钥匙，不更换安装版。
# 用法：bash scripts/verify-dsh-provider-linux.sh <package.json> [released-svault]
set -e
if [ "$#" -lt 1 ] || [ "$#" -gt 2 ]; then
    printf '%s\n' 'usage: verify-dsh-provider-linux.sh <package.json> [released-svault]' >&2
    exit 1
fi
package=$1
export PATH="$HOME/.cargo/bin:$PATH"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$HOME/svault-probe-target}"
node_binary=${SVAULT_DSH_NODE:-node}
set -eu
root=$(git rev-parse --show-toplevel)
cd "$root"
# 发布构建会覆盖构建缓存；旧读者必须取固定发布产物，不能指回同一个构建出口。
legacy_tag=v2026.11.09.1
old_binary=${2:-$root/target/dist/$legacy_tag/svault-$legacy_tag-x86_64-linux}
if [ ! -f "$package" ] || [ ! -f "$old_binary" ]; then
    printf '%s\n' 'DSH package or released binary is not a file; use Linux absolute paths' >&2
    exit 1
fi

# 每条命令分别保留输出和自己的退出码，失败时立即退出，不用管道判断。
run_check() {
    local name=$1
    shift
    local rc=0
    "$@" > "target/final-linux-${name}.log" 2>&1 || rc=$?
    printf '%s\n' "$rc" > "target/final-linux-${name}.rc"
    if [ "$rc" -ne 0 ]; then
        printf 'failed: %s (rc=%s)\n' "$name" "$rc" >&2
        exit "$rc"
    fi
}

run_check lib cargo test --lib --features store,dsh-experimental
run_check cli cargo test --bin svault --features store
run_check integration cargo test --features store,acceptance-fixtures \
    --test cli_scan_baseline_it --test cli_union_it --test cli_read_only_it
run_check default cargo check --bins
run_check store cargo check --bins --features store
run_check clippy cargo clippy --all-targets --all-features -- -D warnings
run_check build cargo build --bin svault --example dsh_snapshot --features store,dsh-experimental
run_check normalize "$node_binary" scripts/verify-dsh-rust-normalize-store.mjs \
    --binary "$CARGO_TARGET_DIR/debug/examples/dsh_snapshot" --dsh-package "$package"

rc=0
"$node_binary" scripts/verify-dsh-provider-compat.mjs \
    --binary "$CARGO_TARGET_DIR/debug/svault" --old-binary "$old_binary" \
    --example-binary "$CARGO_TARGET_DIR/debug/examples/dsh_snapshot" \
    --dsh-package "$package" > target/final-linux-compat.log 2>&1 || rc=$?
printf '%s\n' "$rc" > target/final-linux-compat.rc
case "$rc" in
    0) printf '%s\n' 'supported contracts passed; no legacy evidence loss observed' ;;
    2) printf '%s\n' 'supported contracts passed; limited compatibility: legacy evidence loss remains' ;;
    *) printf 'failed: compatibility (rc=%s)\n' "$rc" >&2; exit "$rc" ;;
esac
