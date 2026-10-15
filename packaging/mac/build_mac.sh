#!/usr/bin/env bash
# ============================================================================
# 在 macOS 上构建「3F-Shell」安装包（.app + .dmg）
#
# 前置条件（在 Mac / GitHub Actions macOS runner 上）：
#   1. 安装 Rust：  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
#   2. 本项目源码（含 vendor/ 目录）
#   3. （可选）Apple 开发者证书用于正式签名；无证书则自动 ad-hoc 签名，
#      本机可直接运行，分发给他人首次打开需在「系统设置-隐私与安全性」放行。
#
# 用法： 在项目根目录执行  bash packaging/mac/build_mac.sh
# 产物：  dist/mac/3F-Shell-1.0.0.dmg
# ============================================================================
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOT"

APP="3F-Shell"
APP_DISPLAY="3F-Shell"
VERSION="1.0.0"
BUNDLE_ID="com.3fshell.app"
STAGE="$ROOT/dist/mac"
APPDIR="$STAGE/$APP.app"
DMG="$STAGE/$APP-$VERSION.dmg"

echo "==> [1/5] cargo build --release（macOS 原生编译）"
cargo build --release

echo "==> [2/5] 组装 $APP.app"
rm -rf "$STAGE"
mkdir -p "$APPDIR/Contents/MacOS" "$APPDIR/Contents/Resources"
cp "$ROOT/target/release/sky-remote-assistance" "$APPDIR/Contents/MacOS/$APP"

# 从 1024 源图生成 AppIcon.icns（sips + iconutil）
if [ -f "$ROOT/packaging/icon-1024.png" ]; then
    ICONSET="$STAGE/AppIcon.iconset"
    mkdir -p "$ICONSET"
    for s in 16 32 128 256 512; do
        sips -z "$s" "$s" "$ROOT/packaging/icon-1024.png" --out "$ICONSET/icon_${s}x${s}.png" >/dev/null 2>&1
        d=$((s * 2))
        sips -z "$d" "$d" "$ROOT/packaging/icon-1024.png" --out "$ICONSET/icon_${s}x${s}@2x.png" >/dev/null 2>&1
    done
    iconutil -c icns "$ICONSET" -o "$APPDIR/Contents/Resources/AppIcon.icns"
    rm -rf "$ICONSET"
    echo "    图标 AppIcon.icns 已生成"
fi

cat > "$APPDIR/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>$APP</string>
    <key>CFBundleDisplayName</key><string>$APP_DISPLAY</string>
    <key>CFBundleIdentifier</key><string>$BUNDLE_ID</string>
    <key>CFBundleVersion</key><string>$VERSION</string>
    <key>CFBundleShortVersionString</key><string>$VERSION</string>
    <key>CFBundleExecutable</key><string>$APP</string>
    <key>CFBundleIconFile</key><string>AppIcon</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
    <key>LSMinimumSystemVersion</key><string>11.0</string>
    <key>NSHighResolutionCapable</key><true/>
    <key>NSPrincipalClass</key><string>NSApplication</string>
</dict>
</plist>
PLIST

echo "==> [3/5] 签名（无开发者证书时使用 ad-hoc 签名）"
codesign --force --deep -s - "$APPDIR" || echo "（提示：codesign 失败不影响本机构建产物，可手动处理）"

echo "==> [4/5] 生成 dmg 安装包"
hdiutil create -volname "$APP-$VERSION" -srcfolder "$APPDIR" -ov -format UDZO "$DMG"

echo "==> [5/5] 完成"
echo "构建完成：$DMG"
echo "说明：正式对外分发建议使用 Apple 开发者证书签名（替换 -s - 为证书名）并做公证（notarytool）。"
