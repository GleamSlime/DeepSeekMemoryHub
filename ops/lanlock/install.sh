#!/bin/bash
# 安装 lanlock：把脚本放到 /usr/local/etc/lanlock，注册 LaunchDaemon（开机自动加载 + 改 allow.txt 自动生效）
# 用法: sudo ./install.sh
set -euo pipefail

[ "$(id -u)" -eq 0 ] || { echo "请用 sudo 运行: sudo $0" >&2; exit 1; }

SRC="$(cd "$(dirname "$0")" && pwd)"
PREFIX=/usr/local/etc/lanlock
PLIST=/Library/LaunchDaemons/com.bluewhale.lanlock.plist
mkdir -p "$PREFIX" /Library/LaunchDaemons /var/log

install -m 755 "$SRC/lanlock.sh" "$PREFIX/lanlock.sh"
[ -f "$PREFIX/allow.txt" ] || install -m 644 "$SRC/allow.txt" "$PREFIX/allow.txt"

sed "s#__PREFIX__#$PREFIX#g" "$SRC/com.bluewhale.lanlock.plist.tpl" > "$PLIST"
chown root:wheel "$PLIST"
chmod 644 "$PLIST"
plutil -lint "$PLIST"

launchctl bootout system/com.bluewhale.lanlock 2>/dev/null || true
launchctl bootstrap system "$PLIST"

"$PREFIX/lanlock.sh" apply

echo
echo "安装完成。常用命令:"
echo "  sudo $PREFIX/lanlock.sh add 1.2.3.4"
echo "  sudo $PREFIX/lanlock.sh status"
echo "  sudo $PREFIX/lanlock.sh off        # 出问题先执行这个恢复全放行"
