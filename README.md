# 3F-Shell

**基于 Rust 打造的跨平台远程 SSH 客户端** —— 稳定 · 快速 · 资源占用少。

3F-Shell 在桌面窗口内提供**真实交互式 SSH 终端**（可运行 vim / top / htop 等全屏程序）与**远程文件系统可视化操作**，让运维与开发人员以本地 GUI 的体验管理远程 Linux 服务器，支持 **Windows / Linux / macOS** 三大主流平台。

## 为什么选择 Rust

- **纯 Rust 技术栈**：SSH 与 SFTP 基于 `russh` / `russh-sftp` 实现，**无 OpenSSL / libssh2 等 C 依赖**，供应链更可控、二进制更安全。
- **原生 GUI，无浏览器内核**：界面由 `egui / eframe` 原生渲染（终端字节流由 `vt100` 解析），**无 Electron / Chromium 等重运行时**——启动更快，内存与 CPU 开销远小于同类 Web 套壳工具。
- **单文件分发**：编译产物为独立可执行文件，不依赖运行时环境，拷贝即用。
- **异步不阻塞**：SSH 连接、文件传输在后台 `tokio` 运行时执行，界面始终保持流畅响应。

## 核心特性

- **SSH 登录**：密码认证与 OpenSSH 私钥认证；服务器主机密钥自动接受（开发工具定位）。
- **账号本地存储**：连接成功后自动保存账号（主机 / 端口 / 用户，可选记住密码）到本地
  `accounts.tsv`；左侧栏点击已存账号一键连接，支持删除，启动时自动加载。
- **真实交互终端（PTY）**：xterm-256color，支持 vim / top / htop 等全屏程序、256 色、光标
  显示/隐藏、方向键 / Home / End / PageUp / PageDown / Insert / Delete / F1-F12、
  Ctrl / Alt / Shift 组合键、鼠标选中复制、Ctrl+V 粘贴（自动包裹 bracketed-paste 转义），
  以及绕过输入法（IME）拦截的全局 Ctrl+C 热键。
- **远程文件可视化操作**：目录列表（名称 / 大小 / 修改时间 / 权限）、双击进入目录、路径直接
  输入跳转、上传 / 下载（流式，支持大文件）、新建目录、重命名、删除，均为原生文件对话框。
- **终端与文件面板联动**：在终端执行 `cd /路径`（支持相对路径、`..`、`~` 展开）后，文件面板
  自动定位到对应目录；终端提示符中的路径变化（含 vim 内 `:cd` 等）也会自动同步文件面板。
- **会话管理**：多标签页多连接、连接 / 断开，状态栏实时提示；窗口大小变化自动通知远端。
- **连接自动维护**：SSH 底层每 30s keepalive 心跳防静默超时断连；GUI 每 30s 探测连接有效性，
  连接失效时自动使用当前账号重连（最多 3 次，终端区显示重连进度）；手动断开后不再自动重连。
- **远端系统信息**：连接后展示远端主机名 / 系统发行版 / 内核版本 / 架构 / 运行时间，可手动刷新。

## 平台支持

| 平台 | 支持 | 产物 / 打包方式 |
| --- | --- | --- |
| Windows 10/11 x64 | 完整支持 | `sky-remote-assistance.exe` + Inno Setup 安装包（`packaging/installer.iss`） |
| Linux x64 | 完整支持 | 原生二进制 / `.deb` 包（`packaging/linux/`，支持 WSL 内构建） |
| macOS 11+ | 完整支持 | `.app` + `.dmg`（`packaging/mac/build_mac.sh`，无证书自动 ad-hoc 签名） |

## 界面布局

```
┌──────────────────────────────────────────────────────────────┐
│ 3F-Shell 远程终端 + 文件管理                                  │
├──────────┬───────────────────────────────────────────────────┤
│ 已存账号 │ 远程终端 / 登录提示（黑底白字）                    │
│ ＋ 新增  │  · 未连接时显示登录信息与提示                      │
│ 账号列表 │  · 连接成功后为真实 SSH PTY（vim/top/htop）        │
│ ──────── │  …可拖拽调整高度                                  │
│ 状态/断开│                                                    │
│ 系统信息 │                                                    │
├──────────┼───────────────────────────────────────────────────┤
│          │ 远程文件   ↑上级 刷新 +新建目录 ↑上传 ↓下载 重命名 删除 │
│          │ 路径 /                                            │ ← 文件管理器
└──────────┴───────────────────────────────────────────────────┘
```

- **连接 / 断开全部在左侧栏操作**：点击账号直接连接，或「＋ 新增」录入账号。
- **登录信息显示在终端区域**：发起连接、认证中、登录成功、连接失败原因、已断开等提示以黑底白字
  显示在命令行区域；连接成功后自动切换为真实交互式终端。

## 快速开始

### 1. 环境要求

- **Windows 10/11 x64**：Rust 工具链 `stable-x86_64-pc-windows-gnullvm`
  （`rust-toolchain.toml` 已固定，自带 rust-lld 链接器，无需 Visual Studio / MinGW）
- **Linux**：Ubuntu/Debian x64 + Rust stable（构建依赖见 `packaging/linux/build_linux.sh`）
- **macOS 11+**：Rust stable（Apple 开发者证书可选，无证书自动 ad-hoc 签名）

### 2. Windows 构建（推荐）

```powershell
# 安装工具链并补齐系统库（仅首次需要）
.\setup.ps1

# 编译 release 并复制运行依赖 DLL（libunwind + Mesa 软件渲染）
.\build.ps1
```

### 3. Linux 构建

```bash
# 在 WSL / Ubuntu 内执行（脚本内含依赖安装与 cargo build）
bash packaging/linux/build_linux.sh
# 或打包 .deb： bash packaging/linux/rebuild_deb.sh
```

### 4. macOS 构建

```bash
# 在项目根目录执行，产物：dist/mac/3F-Shell-1.0.0.dmg
bash packaging/mac/build_mac.sh
```

### 5. 运行

- **Windows**：双击 `target\release\sky-remote-assistance.exe` 即可（`build.ps1` 已自动复制
  `libunwind.dll` 与 `mesa_x64\x64\*.dll` 软件渲染依赖；直接拷贝 release 目录时请保留这些 DLL）。
- **Linux**：运行 `target/release/sky-remote-assistance`（或安装 .deb 后从应用菜单启动）。
- **macOS**：打开 `3F-Shell.app` / 安装 `3F-Shell-1.0.0.dmg`。

## 使用说明

1. **连接**：左侧栏「已存账号」点击账号即直接连接；没有账号时点「＋ 新增」在弹窗中录入
   主机 / 端口 / 用户 / 密码（或私钥）保存后，再点击列表账号连接。勾选「记住密码」会连密码一并
   保存（明文，详见注意事项）；不勾选则只保存主机 / 端口 / 用户。
2. **登录提示**：发起连接后，终端区域以黑底白字显示连接进度 / 认证结果 / 失败原因；连接成功后
   自动切换为真实交互式终端。
3. **终端操作**：点击终端区域获得焦点后即可输入命令；终端**输入无回显**（由远端回显，与真实 SSH
   一致），支持 vim / top / htop 等全屏程序；窗口拖拽改变大小时远端终端尺寸自动同步。
4. **系统信息**：连接后在**左侧栏「系统信息」**查看远端主机名 / 发行版 / 内核 / 架构 / 运行时间。
5. **文件操作**：文件列表单击目录进入、双击文件选中；顶部按钮可返回上级 / 刷新 / 新建目录 / 上传 /
   下载 / 重命名 / 删除；也可直接在路径框输入如 `/home/user` 后回车跳转。
6. **上传 / 下载**：点击后弹出系统原生文件对话框选择本地文件；下载的本地路径同样由对话框指定。
7. **删除 / 重命名**：先双击选中文件行，再点对应按钮；删除会弹出确认窗口。

## 无 GPU / 远程桌面环境的说明

egui 渲染需要 OpenGL 2.0+。**无独立显卡或通过远程桌面（RDP）连接时**，系统只提供 OpenGL 1.1，
程序会提示 `egui_glow requires opengl 2.0+` 无法启动。此时需将本项目 `mesa_x64\x64\` 下的
**Mesa3D（llvmpipe）软件渲染 DLL**（`opengl32.dll` 及其依赖 `libgallium_wgl.dll` /
`libGLESv2.dll` / `libEGL.dll` 等）放置到 exe 同目录（`build.ps1` 会自动完成）。
有独立显卡的机器可直接运行，无需 Mesa。

## 注意事项

- **主机密钥不做指纹校验**：仅建议在可信内网环境使用，请勿暴露到公网。
- **「记住密码」为明文存储**：`accounts.tsv` 与 exe 同目录，密码以明文 Tab 分隔保存，仅建议在
  个人本机勾选；请勿拷贝该文件给他人或上传到不可信位置。
- 会话保存在进程内存中，程序退出后连接自动关闭。
- 删除操作不可恢复，请谨慎使用。
- 私钥认证请使用 OpenSSH 格式（`ssh-keygen` 生成的 `id_rsa` / `id_ed25519` 等）。

## 项目结构

```
3F-Shell/                     # 项目根
├── Cargo.toml                # eframe/egui 0.31 + vt100 0.6 + russh 0.46 + russh-sftp 2
├── rust-toolchain.toml       # 固定 gnullvm 工具链
├── .cargo/config.toml        # crates 镜像 + rust-lld 链接器配置
├── build.rs                  # 空 shlwapi 库 + 直接链接系统 DLL（绕过 gnullvm 缺库）
├── setup.ps1                 # Windows 工具链一键安装 + 补齐系统导入库
├── build.ps1                 # Windows release 构建 + 复制运行依赖 DLL
├── vendor/
│   └── enumset_derive/       # egui 依赖 enumset 的本地补丁（syn 兼容修复）
├── mesa_x64/x64/             # Mesa3D 软件渲染 DLL（无 GPU / RDP 环境使用）
├── packaging/
│   ├── installer.iss         # Windows 安装包（Inno Setup）
│   ├── linux/                # Linux 构建与 .deb 打包脚本
│   ├── mac/                  # macOS .app / .dmg 构建脚本
│   └── icon-1024.png 等      # 应用图标素材
└── src/
    ├── main.rs               # GUI 主程序（连接栏 + vt100 终端 + 文件管理器 + 后台 tokio 桥）
    ├── ssh.rs                # SSH/SFTP 连接管理、PTY shell 通道、文件操作
    └── kbhook.rs             # Windows 键盘钩子 + Ctrl+C 全局热键
```
