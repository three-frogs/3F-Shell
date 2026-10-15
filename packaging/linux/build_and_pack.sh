#!/bin/bash
# 3F-Shell Linux 构建 + 打包脚本（WSL/Ubuntu 内执行）
# 前置：构建依赖已通过 apt 安装；源码在 /root/sra_build（含 src/vendor/.cargo/packaging）
set -e
cd /root/sra_build
export PATH=/root/.cargo/bin:$PATH
export RUSTUP_DIST_SERVER=https://rsproxy.cn

echo "== cargo build --release =="
cargo build --release
BIN=target/release/sky-remote-assistance
ls -la $BIN
file $BIN 2>/dev/null || true

VER=1.0.0
echo "== packaging =="
rm -rf /root/pkg
mkdir -p /root/pkg/3f-shell_${VER}_amd64/DEBIAN
mkdir -p /root/pkg/3f-shell_${VER}_amd64/usr/bin
mkdir -p /root/pkg/3f-shell_${VER}_amd64/usr/share/applications
mkdir -p /root/pkg/3f-shell_${VER}_amd64/usr/share/icons/hicolor/256x256/apps

cp $BIN /root/pkg/3f-shell_${VER}_amd64/usr/bin/3f-shell
if [ -f packaging/icon-256.png ]; then
  cp packaging/icon-256.png /root/pkg/3f-shell_${VER}_amd64/usr/share/icons/hicolor/256x256/apps/3f-shell.png
fi

cat > /root/pkg/3f-shell_${VER}_amd64/DEBIAN/control <<'CTRL'
Package: 3f-shell
Version: 1.0.0
Section: net
Priority: optional
Architecture: amd64
Depends: libgtk-3-0 (>= 3.22), libxkbcommon0, libgl1, libxcb1, libwayland-client0, libfontconfig1
Maintainer: Eric Lam <ericlam423@users.noreply.github.com>
Description: 3F-Shell - Linux remote SSH terminal and file manager GUI
 Interactive SSH terminal and SFTP file browser for Linux servers.
CTRL

cat > /root/pkg/3f-shell_${VER}_amd64/usr/share/applications/3f-shell.desktop <<'DCT'
[Desktop Entry]
Name=3F-Shell
Comment=Remote SSH terminal and file manager
Exec=/usr/bin/3f-shell
Icon=3f-shell
Terminal=false
Type=Application
Categories=Network;Utility;
StartupNotify=true
DCT

dpkg-deb --build --root-owner-group /root/pkg/3f-shell_${VER}_amd64 /root/pkg/3f-shell_${VER}_amd64.deb
echo "== deb done =="

mkdir -p /root/pkg/3f-shell
cp $BIN /root/pkg/3f-shell/3f-shell
if [ -f packaging/icon-256.png ]; then
  cp packaging/icon-256.png /root/pkg/3f-shell/
fi
cd /root/pkg
tar -czf 3f-shell-linux-x86_64-${VER}.tar.gz 3f-shell
echo "== all done =="
ls -la /root/pkg
