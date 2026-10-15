#!/bin/bash
# 3F-Shell Linux 产物验证脚本
set -e
cd /root/sra_build
echo "== ldd missing deps =="
MISSING=$(ldd target/release/sky-remote-assistance 2>/dev/null | grep -c "not found" || true)
echo "missing_count=$MISSING"
ldd target/release/sky-remote-assistance 2>/dev/null | grep "not found" || true
echo "== xvfb smoke test =="
if command -v xvfb-run >/dev/null 2>&1 || command -v Xvfb >/dev/null 2>&1; then
    timeout 15 xvfb-run -a ./target/release/sky-remote-assistance >/tmp/xvfb.log 2>&1 &
    XPID=$!
    sleep 8
    if kill -0 $XPID 2>/dev/null; then
        echo "smoke_ok: process alive after 8s under Xvfb"
        kill $XPID 2>/dev/null || true
    else
        echo "smoke_fail: process exited early"
        tail -5 /tmp/xvfb.log || true
    fi
else
    echo "no_xvfb: install with: apt-get install -y xvfb"
fi
echo "== artifacts =="
ls -la /root/pkg
