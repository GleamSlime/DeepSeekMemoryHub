#!/bin/bash
# 把蓝鲸记忆中枢装成 launchd 服务（开机自启、崩溃重启）。
#
# 为什么用 launchd 而不是 cron/手动 nohup：
#   - 开机自启、退出后自动拉起
#   - 日志有固定去处
#   - macOS 上 launchd 是唯一的正规做法
set -euo pipefail

APP_DIR="$(cd "$(dirname "$0")/../.." && pwd)"
BIN="$APP_DIR/Contents/MacOS/bluewhale-memory"
LABEL="local.bluewhale-memory"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
DATA_DIR="${BLUEWHALE_DATA_DIR:-$HOME/.bluewhale-memory}"
PORT="${BLUEWHALE_PORT:-25590}"
TOKEN="${BLUEWHALE_TOKEN:-}"

if [ ! -x "$BIN" ]; then
  echo "找不到二进制：$BIN" >&2
  exit 1
fi

if [ -z "$TOKEN" ]; then
  echo "⚠ 没有设置 BLUEWHALE_TOKEN。"
  echo "  服务将**只能本机访问**（这是刻意的安全设计）。"
  echo "  要让内网访问，请：BLUEWHALE_TOKEN=<至少16字符> $0"
  echo
fi

mkdir -p "$DATA_DIR" "$HOME/Library/LaunchAgents"

# token 不写进 plist：plist 在 ~/Library/LaunchAgents 里，权限比环境变量宽。
# 写进一个只有本用户可读的 env 文件，plist 引用它。
ENV_FILE="$DATA_DIR/env"
umask 077
cat > "$ENV_FILE" <<ENV
BLUEWHALE_DATA_DIR=$DATA_DIR
BLUEWHALE_PORT=$PORT
BLUEWHALE_BIND=lan
BLUEWHALE_TOKEN=$TOKEN
ENV
chmod 600 "$ENV_FILE"

cat > "$PLIST" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>$LABEL</string>
  <key>ProgramArguments</key>
  <array>
    <string>/bin/sh</string>
    <string>-c</string>
    <string>set -a; . "$ENV_FILE"; set +a; exec "$BIN" serve</string>
  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key><string>$DATA_DIR/service.log</string>
  <key>StandardErrorPath</key><string>$DATA_DIR/service.err.log</string>
  <key>ProcessType</key><string>Background</string>
</dict>
</plist>
PLIST

launchctl unload "$PLIST" 2>/dev/null || true
launchctl load "$PLIST"

sleep 1
echo "已安装并启动：$LABEL"
echo "  数据目录  $DATA_DIR"
echo "  环境文件  $ENV_FILE（含 token，权限 600）"
echo "  日志      $DATA_DIR/service.log"
echo "  健康检查  curl http://127.0.0.1:$PORT/v1/health"
echo
echo "卸载：launchctl unload $PLIST && rm $PLIST"
