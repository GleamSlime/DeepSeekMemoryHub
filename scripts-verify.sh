#!/bin/bash
# 一键校验：开发与打包前都必须跑这个。
# 任何一步失败都退出非零——CI 与人工都只看退出码。
set -euo pipefail

ROOT=$(cd "$(dirname "$0")" && pwd)
cd "$ROOT"

green() { printf '\033[32m%s\033[0m\n' "$*"; }
red()   { printf '\033[31m%s\033[0m\n' "$*"; }
step()  { printf '\n\033[1m==> %s\033[0m\n' "$*"; }

fail=0
run() {
  local name="$1"; shift
  if "$@" >/tmp/bw-step.log 2>&1; then
    green "  ✓ $name"
  else
    red "  ✗ $name"; tail -30 /tmp/bw-step.log; fail=1
  fi
}

step "1/5 格式检查"
run "cargo fmt --check" cargo fmt --all -- --check

step "2/5 静态检查（clippy，警告视为错误）"
run "cargo clippy" cargo clippy --all-targets -- -D warnings

step "3/5 单元测试"
run "cargo test --lib" cargo test --lib

step "4/5 集成测试"
run "cargo test --test http_api" cargo test --test http_api
# admin_api 之前漏在脚本外 —— 面板接口改动不会被这脚本拦住。
run "cargo test --test admin_api" cargo test --test admin_api

step "5/5 release 构建"
run "cargo build --release" cargo build --release

if [ "$fail" -ne 0 ]; then
  red "\n校验未通过"
  exit 1
fi
green "\n全部通过"
