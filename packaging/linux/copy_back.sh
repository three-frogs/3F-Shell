#!/bin/bash
# 重建 deb + 拷贝 Linux 产物回 Windows 项目 dist/
set -e
bash /root/sra_build/rebuild_deb.sh
mkdir -p /mnt/e/dev/work/sky-workspace/sky-remote-assistance/dist
cp /root/pkg/3f-shell_1.0.0_amd64.deb /mnt/e/dev/work/sky-workspace/sky-remote-assistance/dist/
cp /root/pkg/3f-shell-linux-x86_64-1.0.0.tar.gz /mnt/e/dev/work/sky-workspace/sky-remote-assistance/dist/
cp /root/sra_build/Cargo.lock /mnt/e/dev/work/sky-workspace/sky-remote-assistance/Cargo.lock
echo "== dist =="
ls -la /mnt/e/dev/work/sky-workspace/sky-remote-assistance/dist/ | grep -E "3f-shell|tar" || true
