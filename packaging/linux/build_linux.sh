#!/bin/bash
# 3F-Shell Linux 构建脚本（在 WSL/Ubuntu 内执行）
# 前置：apt-get install -y build-essential pkg-config curl ca-certificates \
#   libxkbcommon-dev libxcb1-dev libxcb-render0-dev libxcb-shape0-dev \
#   libxcb-xfixes0-dev libxcb-randr0-dev libxcb-icccm4-dev libxcb-composite0-dev \
#   libxcb-cursor-dev libxcb-keysyms1-dev libxcb-shm0-dev libgl1-mesa-dev \
#   libegl1-mesa-dev libgtk-3-dev libwayland-dev libfontconfig1-dev
set -e

export RUSTUP_DIST_SERVER=https://rsproxy.cn
export RUSTUP_UPDATE_ROOT=https://rsproxy.cn/rustup
export PATH=/root/.cargo/bin:$PATH

# 1. 安装 rustup（如未安装）
if ! command -v rustc >/dev/null 2>&1; then
    echo "== installing rustup =="
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable --no-modify-path
fi
rustc --version
cargo --version

# 2. 构建（源码需已复制到当前目录，含 src/vendor/.cargo）
cd "$(dirname "$0")"
cargo build --release
echo "== built =="
ls -la target/release/sky-remote-assistance
file target/release/sky-remote-assistance 2>/dev/null || true
