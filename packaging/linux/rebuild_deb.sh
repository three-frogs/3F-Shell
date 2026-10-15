#!/bin/bash
# 重建 .deb：补充 Mesa 软件渲染运行时依赖
set -e
CONTROL=/root/pkg/3f-shell_1.0.0_amd64/DEBIAN/control
sed -i 's/Depends: libgtk-3-0 (>= 3.22), libxkbcommon0, libgl1, libxcb1, libwayland-client0, libfontconfig1/Depends: libgtk-3-0 (>= 3.22), libxkbcommon0, libgl1, libgl1-mesa-dri, libxcb1, libwayland-client0, libfontconfig1/' "$CONTROL"
dpkg-deb --build --root-owner-group /root/pkg/3f-shell_1.0.0_amd64 /root/pkg/3f-shell_1.0.0_amd64.deb
echo "== final artifacts =="
ls -la /root/pkg/*.deb /root/pkg/*.tar.gz
