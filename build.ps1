# 构建脚本：编译 release 版本，并复制运行所需 DLL 到输出目录
$ErrorActionPreference = "Stop"

$cargo = "C:\Users\Administrator\.cargo\bin\cargo.exe"
$rustc = "C:\Users\Administrator\.cargo\bin\rustc.exe"
if (-not (Test-Path $cargo)) { $cargo = "cargo" }
if (-not (Test-Path $rustc)) { $rustc = "rustc" }

Write-Host "[1/3] cargo build --release ..."
& $cargo build --release
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

Write-Host "[2/3] 复制运行依赖 libunwind.dll ..."
$sysroot = & $rustc --print sysroot
$dll = Join-Path $sysroot "lib\rustlib\x86_64-pc-windows-gnullvm\bin\libunwind.dll"
if (Test-Path $dll) {
    Copy-Item $dll "target\release\" -Force
    Write-Host "已复制 libunwind.dll"
} else {
    Write-Host "警告：未找到 libunwind.dll（$dll）" -ForegroundColor Yellow
}

Write-Host "[3/3] 复制 Mesa 软件渲染 DLL（无 GPU / 远程桌面环境需要）..."
if (Test-Path "mesa_x64\x64") {
    Copy-Item "mesa_x64\x64\*.dll" "target\release\" -Force
    Write-Host "已复制 Mesa 软件渲染 DLL（opengl32.dll 等）"
} else {
    Write-Host "未找到 mesa_x64\x64，跳过（有独立显卡的机器无需 Mesa）" -ForegroundColor Yellow
}

Write-Host ""
Write-Host "构建完成：target\release\sky-remote-assistance.exe" -ForegroundColor Green
Write-Host "运行：双击该 exe 即可打开桌面 GUI（无需端口 / 浏览器）" -ForegroundColor Green
