#!/bin/bash
# SSH(22) / 屏幕共享(5900) IP 白名单
# 只允许 allow.txt 里的来源建立连接，其余来源在 pf 层直接丢包。
#
#   sudo lanlock.sh apply            重新生成规则并加载（改完 allow.txt 后用，或等 launchd 自动触发）
#   sudo lanlock.sh add 1.2.3.4      追加一条 IP/CIDR 并立即生效
#   sudo lanlock.sh remove 1.2.3.4   移除一条
#   sudo lanlock.sh off              清空本锚点规则（恢复全放行），不碰系统其它 pf 规则
#   lanlock.sh list / status         查看（status 里部分项需要 sudo）
set -euo pipefail

PREFIX=${LANLOCK_PREFIX:-/usr/local/etc/lanlock}
ALLOW="$PREFIX/allow.txt"
RULES="$PREFIX/lanlock.pf"
ANCHOR=com.bluewhale.lanlock
PORTS="{ 22, 5900 }"
PF_CONF=/etc/pf.conf
LAN_DEFAULT="192.168.3.0/24"

need_root() { [ "$(id -u)" -eq 0 ] || { echo "需要 root: sudo $0 $*" >&2; exit 1; }; }

case "${1:-apply}" in
  status)
    echo "== 白名单文件 =="; cat "$ALLOW" 2>/dev/null || echo "(不存在)"
    echo; echo "== pf 是否启用 =="; pfctl -s info 2>/dev/null | head -1 || echo "(需要 sudo)"
    echo; echo "== 本锚点生效规则 =="; pfctl -a "$ANCHOR" -sr 2>/dev/null || echo "(需要 sudo)"
    ;;

  list)
    grep -vE '^[[:space:]]*(#|$)' "$ALLOW" 2>/dev/null || true
    ;;

  add)
    need_root "$@"; entry=${2:?用法: lanlock.sh add 1.2.3.4}
    grep -qxF "$entry" "$ALLOW" 2>/dev/null || echo "$entry" >> "$ALLOW"
    "$0" apply >/dev/null; echo "已加入白名单: $entry"
    ;;

  remove)
    need_root "$@"; entry=${2:?用法: lanlock.sh remove 1.2.3.4}
    tmp=$(mktemp); grep -vxF "$entry" "$ALLOW" > "$tmp" || true
    cat "$tmp" > "$ALLOW"; rm -f "$tmp"
    "$0" apply >/dev/null; echo "已移除: $entry"
    ;;

  off)
    need_root "$@"
    pfctl -a "$ANCHOR" -F all 2>/dev/null || true
    echo "已清空 $ANCHOR 规则（22/5900 恢复全放行，直到下次 apply 或重启）"
    ;;

  apply)
    need_root "$@"
    mkdir -p "$PREFIX"
    if [ ! -f "$ALLOW" ]; then
      printf '# SSH(22) / 屏幕共享(5900) 白名单：每行一个 IP 或 CIDR\n%s\n' "$LAN_DEFAULT" > "$ALLOW"
    fi

    tmp=$(mktemp)
    {
      echo "# 由 lanlock.sh 生成于 $(date '+%F %T')，请改 allow.txt 后重新 apply"
      echo "table <lanlock_allow> persist {"
      grep -vE '^[[:space:]]*(#|$)' "$ALLOW" \
        | sed -E 's/[[:space:]]*#.*$//' | tr -d '[:blank:]' | grep -v '^$' | sed 's/^/  /'
      echo "}"
      echo "pass quick on lo0 all"
      echo "pass in quick proto tcp from <lanlock_allow> to any port $PORTS"
      echo "block in log quick proto tcp from any to any port $PORTS"
    } > "$tmp"
    mv "$tmp" "$RULES"
    chmod 644 "$RULES"

    # pf.conf 里没有引用我们的锚点就补上（这台机器的 /etc/pf.conf 目前是空的，所以只需追加一次）
    if ! grep -q "load anchor \"$ANCHOR\"" "$PF_CONF" 2>/dev/null; then
      {
        echo ""
        echo "# lanlock: SSH / 屏幕共享 IP 白名单"
        echo "anchor \"$ANCHOR\""
        echo "load anchor \"$ANCHOR\" from \"$RULES\""
      } >> "$PF_CONF"
      pfctl -f "$PF_CONF" 2>/dev/null || true
    fi

    pfctl -a "$ANCHOR" -f "$RULES"
    # 打开 pf（重复 -E 只是重新发 token，无害）
    pfctl -E >/dev/null 2>&1 || true

    echo "规则已加载:"
    pfctl -a "$ANCHOR" -sr
    ;;

  *)
    echo "未知命令: $1（可用 apply / add / remove / off / list / status）" >&2
    exit 1
    ;;
esac
