#!/bin/bash
# 蓝鲸记忆中枢 — macOS DMG 打包
#
# 设计：**真正能跑的只有那个单二进制**。DMG 里是一个 .app，.app 里是
#   Contents/MacOS/bluewhale-memory   ← 本体（单文件）
#   Contents/Info.plist               ← 让 Finder 认识它
#   Contents/Resources/install.sh     ← 装成 launchd 服务
#
# 不做代码签名/公证（用户明确说不需要认证流程）。代价：首次打开会被
# Gatekeeper 拦一次，右键→打开即可。README 里写清这一条。
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/.." && pwd)
VERSION=$(grep '^version' "$ROOT/Cargo.toml" | head -1 | cut -d'"' -f2)
APP_NAME="蓝鲸记忆中枢"
SLUG="bluewhale-memory"
DIST="$ROOT/dist"
STAGE="$DIST/stage"
APP="$STAGE/$APP_NAME.app"

echo "==> 打包 $APP_NAME $VERSION"

# 1. 构建
echo "==> 构建 release"
(cd "$ROOT" && cargo build --release --locked 2>/dev/null || cargo build --release)
BIN="$ROOT/target/release/$SLUG"
[ -x "$BIN" ] || { echo "找不到二进制：$BIN" >&2; exit 1; }

# 2. 组装 .app
echo "==> 组装 .app"
rm -rf "$STAGE"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN" "$APP/Contents/MacOS/$SLUG"
chmod +x "$APP/Contents/MacOS/$SLUG"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>$APP_NAME</string>
  <key>CFBundleDisplayName</key><string>$APP_NAME</string>
  <key>CFBundleIdentifier</key><string>local.$SLUG</string>
  <key>CFBundleVersion</key><string>$VERSION</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleExecutable</key><string>$SLUG</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>LSUIElement</key><true/>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST

# 3. 安装脚本（装成 launchd 服务，开机自启）
cp "$HERE/install.sh" "$APP/Contents/Resources/install.sh"
chmod +x "$APP/Contents/Resources/install.sh"

# 4. 说明
cp "$HERE/README-DMG.txt" "$STAGE/使用说明.txt" 2>/dev/null || true

# 5. 生成 DMG
DMG="$DIST/$SLUG-$VERSION-macos-$(uname -m).dmg"
echo "==> 生成 DMG"
rm -f "$DMG"
hdiutil create -volname "$APP_NAME" -srcfolder "$STAGE" -ov -format UDZO "$DMG" >/dev/null

echo
echo "完成：$DMG"
echo "大小：$(du -h "$DMG" | cut -f1)"
echo
echo "校验："
shasum -a 256 "$DMG"
