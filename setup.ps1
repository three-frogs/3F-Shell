# 一次性环境准备脚本：
#   1. 安装 gnullvm 工具链（自带 rust-lld 链接器，无需 Visual Studio / MinGW）
#   2. 安装 GNU 工具链（仅用于提供 Windows 系统导入库文件）
#   3. 把 GNU 工具链中 gnullvm 缺失的系统库（bcrypt/advapi32 等）补齐到 gnullvm
$ErrorActionPreference = "Stop"

$rustup = "$env:USERPROFILE\.cargo\bin\rustup.exe"

Write-Host "[1/3] 检查 Rust 工具链..."
if (-not (Test-Path $rustup)) {
    Write-Host "未检测到 rustup，请先安装：https://rustup.rs（或运行 rustup-init.exe）" -ForegroundColor Red
    exit 1
}

Write-Host "[2/3] 安装工具链（首次会下载，请耐心等待）..."
& $rustup toolchain install stable-x86_64-pc-windows-gnullvm
& $rustup toolchain install stable-x86_64-pc-windows-gnu

Write-Host "[3/3] 补齐 gnullvm 缺失的 Windows 系统导入库..."
$gnuLib = "$env:USERPROFILE\.rustup\toolchains\stable-x86_64-pc-windows-gnu\lib\rustlib\x86_64-pc-windows-gnu\lib\self-contained"
$gnullvmLib = "$env:USERPROFILE\.rustup\toolchains\stable-x86_64-pc-windows-gnullvm\lib\rustlib\x86_64-pc-windows-gnullvm\lib\self-contained"

if (-not (Test-Path $gnuLib)) { Write-Host "GNU 工具链目录异常：$gnuLib" -ForegroundColor Red; exit 1 }
if (-not (Test-Path $gnullvmLib)) { Write-Host "gnullvm 工具链目录异常：$gnullvmLib" -ForegroundColor Red; exit 1 }

$count = 0
Get-ChildItem $gnuLib -Filter "*.a" | ForEach-Object {
    if (-not (Test-Path (Join-Path $gnullvmLib $_.Name))) {
        Copy-Item $_.FullName $gnullvmLib
        $count++
    }
}
Write-Host "已补齐 $count 个系统库文件" -ForegroundColor Green
Write-Host "环境准备完成，接下来运行 build.ps1 构建。" -ForegroundColor Green
