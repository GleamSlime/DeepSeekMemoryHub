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

step "1/6 格式检查"
run "cargo fmt --check" cargo fmt --all -- --check

step "2/6 静态检查（clippy，警告视为错误）"
run "cargo clippy" cargo clippy --all-targets -- -D warnings

step "3/6 单元测试"
run "cargo test --lib" cargo test --lib

step "4/6 集成测试"
run "cargo test --test http_api" cargo test --test http_api
# admin_api 之前漏在脚本外 —— 面板接口改动不会被这脚本拦住。
run "cargo test --test admin_api" cargo test --test admin_api

step "5/6 release 构建"
run "cargo build --release"

# ------------------------------------------------------------ 面板
#
# 面板是单文件 + 内联脚本：没有编译期检查，后端测试也覆盖不到。
# 2026-11-01 出过一次事故 —— 导航里加了 `scenes` 项却忘了定义
# `PAGES.scenes`，`PAGES[k].icon` 抛 TypeError，**整个侧栏渲染不出来**，
# 用户看到的是「WEB 页面报错」，而当时 421 条测试全绿。
step "6/6 面板离线判据（每页真渲染 + 模拟点击）"
if command -v node >/dev/null 2>&1; then
  run "node tools/panecheck-all.mjs"
else
  printf '\033[33m  跳过：没装 node\033[0m\n'
fi

if [ "$fail" -ne 0 ]; then
  red "\n校验未通过"
  exit 1
fi
green "\n全部通过"
