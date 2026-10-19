//! Sky Remote Assistance —— Windows 下对 Linux 的远程 SSH 登录与文件系统可视化操作。
//!
//! 桌面 GUI（eframe/egui）：
//! - 上半部分：真实 SSH 交互式终端（VT100 模拟，支持 vim / top 等全屏程序）
//! - 下半部分：远程文件管理器（浏览 / 上传 / 下载 / 新建 / 删除 / 重命名）

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod kbhook;
mod ssh;

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};

use eframe::egui;
use egui::text::LayoutJob;
use egui::{
    Align2, Color32, Event, FontId, Key, Modifiers, Rect, RichText, Sense, Stroke, StrokeKind,
    TextFormat,
};
use egui_extras::{Column, TableBuilder};
use vt100::Parser as VtParser;

// =====================================================================
// 后台桥接消息（GUI 线程 <-> 后台 tokio 线程）
// =====================================================================

/// 连接请求（token 用于把后端生成的会话 id 匹配回发起它的标签页）。
struct ConnectReq {
    token: u64,
    host: String,
    port: u16,
    user: String,
    password: Option<String>,
    key_path: Option<String>,
}

enum FsMsg {
    Connect(ConnectReq),
    For { id: String, msg: FsInner },
}

enum FsInner {
    Disconnect,
    List(String),
    Mkdir(String),
    Remove(String),
    Rename(String, String),
    Upload { local: PathBuf, remote_dir: String },
    Download { remote: String, local: PathBuf },
    /// 取消当前正在进行的传输（按方向，全局）
    CancelTransfer { up: bool },
    ShellInput(Vec<u8>),
    /// 向远程发送中断：0x03 + SSH SIGINT（docker logs -f 等场景确保可中断）
    SigInt,
    SysInfo,
    Ping,
}

enum BackMsg {
    Connected(Result<(u64, String), (u64, String)>), // Ok(token, id) / Err(token, msg)
    For { id: String, msg: BackInner },
    Log(String),
    Progress { up: bool, done: u64, total: u64 }, // 传输实时进度（已传 / 总字节）
    /// 后台传输开始（用于 UI 展示对应方向的「取消」按钮）
    TransferStart { up: bool },
}

enum BackInner {
    Disconnected,
    List(Result<Vec<ssh::FileEntry>, String>),
    Mkdir(Result<(), String>),
    Remove(Result<(), String>),
    Rename(Result<(), String>),
    Upload(Result<u64, String>),
    Download(Result<u64, String>),
    ShellData(Vec<u8>),
    ShellClosed,
    SysInfo(Result<Vec<(String, String)>, String>),
    Ping(Result<(), String>),
}

// =====================================================================
// 终端状态（VT100 解析器 + 尺寸）
// =====================================================================

struct TermState {
    parser: VtParser,
    dims: (u16, u16),
    /// 用户当前输入行的字节累积（用于检测 `cd` 命令联动文件面板）
    pending_line: Vec<u8>,
    /// 终端当前工作目录（由提示符解析 / cd 命令乐观更新）
    term_cwd: Option<String>,
    /// 上次识别到的提示符文本（防止重复触发）
    last_prompt: Option<String>,
    /// 最近一次提交的命令行（用于「最近会话」展示）
    last_cmd: String,
    /// 回看历史时的滚动偏移（0 = 最新，>0 = 向上回看行数）
    scroll_offset: usize,
    /// 鼠标选择：锚点与当前点 ((row,col),(row,col))；None = 无选择
    sel: Option<((u16, u16), (u16, u16))>,
}

impl TermState {
    fn new(rows: u16, cols: u16) -> Self {
        Self {
            parser: VtParser::new(rows, cols, 2000), // 2000 行历史缓冲，支持回看
            dims: (cols, rows),
            pending_line: Vec::new(),
            term_cwd: None,
            last_prompt: None,
            last_cmd: String::new(),
            scroll_offset: 0,
            sel: None,
        }
    }

    /// 把发给远端的输入字节喂给累积器；若检测到 `cd` 命令，
    /// 返回（乐观计算的）目标绝对路径供文件面板联动。
    fn feed_input(&mut self, bytes: &[u8], user: &str) -> Option<String> {
        let mut nav = None;
        for &b in bytes {
            match b {
                0x0d | 0x0a => {
                    let line = String::from_utf8_lossy(&self.pending_line).into_owned();
                    self.pending_line.clear();
                    let line_trim = line.trim();
                    if !line_trim.is_empty() {
                        self.last_cmd = line_trim.to_string();
                    }
                    if let Some(t) = parse_cd_line(&line, &self.term_cwd, user) {
                        nav = Some(t);
                    }
                }
                0x08 | 0x7f => {
                    self.pending_line.pop();
                }
                // ESC 序列（方向键编辑）、Ctrl+C/D/U/W：无法可靠重建行，放弃追踪
                0x1b | 0x03 | 0x04 | 0x15 | 0x17 => {
                    self.pending_line.clear();
                }
                0x20..=0x7e | 0x80..=0xff => self.pending_line.push(b),
                _ => {}
            }
        }
        nav
    }

    /// 解析终端屏幕最后一行是否为 shell 提示符；若是且与上次不同，
    /// 返回提示符中的路径（供文件面板定位；权威值，可纠正乐观估计）。
    /// `line` 为调用方提取的屏幕最后一行文本。
    fn detect_prompt(&mut self, line: &str) -> Option<String> {
        let trimmed = line.trim_end().to_string();
        if let Some(path) = parse_prompt_path(&trimmed) {
            let changed = self.last_prompt.as_deref() != Some(trimmed.as_str());
            self.last_prompt = Some(trimmed);
            if changed {
                return Some(path);
            }
        }
        None
    }

    /// 把远端输出字节喂给终端解析器。
    fn feed(&mut self, data: &[u8]) {
        self.parser.process(data);
    }
}

// =====================================================================
// 终端 <-> 文件面板联动：cd 命令解析 / 提示符解析 / 路径规范化
// =====================================================================

/// 登录用户的主目录（启发式：root -> /root，其他 -> /home/<user>）。
fn home_path(user: &str) -> String {
    if user.is_empty() || user == "root" {
        "/root".into()
    } else {
        format!("/home/{}", user)
    }
}

/// 把 `~` / `~user` 展开为绝对路径，其余原样返回。
fn expand_tilde(path: &str, user: &str) -> String {
    if path == "~" {
        return home_path(user);
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return format!("{}/{}", home_path(user), rest);
    }
    if let Some(rest) = path.strip_prefix('~') {
        // ~otheruser/xxx
        let rest = rest.trim_start_matches('/');
        return format!("/home/{}", rest);
    }
    path.to_string()
}

/// 按显示宽度截断单行文本（CJK≈2、其余≈1 个半角单位），超出以 … 结尾。
fn truncate_wide(s: &str, max_w: usize) -> String {
    let mut w = 0usize;
    let mut out = String::new();
    for ch in s.chars() {
        let cw = if (ch as u32) > 0x2E80 { 2 } else { 1 };
        if w + cw > max_w {
            out.push('…');
            return out;
        }
        w += cw;
        out.push(ch);
    }
    out
}

/// 把 target（绝对或相对路径）基于 base 规范化：
/// 处理 `.`、`..`、重复 `/`、结尾 `/`；target 支持 `~` 展开。
fn normalize_path(base: &str, target: &str, user: &str) -> String {
    let t = expand_tilde(target, user);
    let full = if t.starts_with('/') {
        t
    } else {
        format!("{}/{}", base.trim_end_matches('/'), t)
    };
    let mut segs: Vec<&str> = Vec::new();
    for part in full.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                segs.pop();
            }
            p => segs.push(p),
        }
    }
    let mut out = String::from("/");
    out.push_str(&segs.join("/"));
    out
}

/// 解析一行输入是否为 `cd` 命令，返回目标绝对路径。
/// base 为终端当前目录（None 时按 "/" 处理）。
fn parse_cd_line(line: &str, term_cwd: &Option<String>, user: &str) -> Option<String> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    // 只取第一个命令（忽略 && / ; / | 之后的内容）
    let cmd = line.split([';', '|', '&']).next()?.trim();
    let mut parts = cmd.split_whitespace();
    let first = parts.next()?;
    if first != "cd" {
        return None;
    }
    let target = parts.next().unwrap_or("~");
    if target == "-" {
        return None; // OLDPWD 未知
    }
    let base = term_cwd.clone().unwrap_or_else(|| "/".into());
    Some(normalize_path(&base, target, user))
}

/// 尝试把一行文本解析为 shell 提示符中的路径。
/// 支持 `user@host:/path$`、`/path#`、`~$` 等常见格式；解析不出返回 None。
fn parse_prompt_path(line: &str) -> Option<String> {
    let line = line.trim_end();
    if line.is_empty() {
        return None;
    }
    let last = line.chars().last()?;
    if !matches!(last, '$' | '#' | '%' | '>') {
        return None;
    }
    let body = line[..line.len() - last.len_utf8()].trim_end();
    let path = if let Some(at) = body.rfind('@') {
        let after = &body[at + 1..];
        let ci = after.rfind(':')?;
        let host = after[..ci].trim();
        let p = after[ci + 1..].trim();
        // zsh 的 "user@host /path %" 形式 host 部分含空格，拒绝
        if host.is_empty() || host.chars().any(|c| c.is_whitespace()) {
            return None;
        }
        if p.is_empty() || p.chars().any(|c| c.is_whitespace()) {
            return None;
        }
        p
    } else {
        body.trim()
    };
    if path.is_empty() || !(path.starts_with('/') || path.starts_with('~')) {
        return None;
    }
    if path.chars().any(|c| c.is_whitespace()) {
        return None;
    }
    Some(path.to_string())
}

// =====================================================================
// 对话框
// =====================================================================

#[derive(Clone, Copy, PartialEq)]
enum DialogKind {
    Mkdir,
    Rename,
    ConfirmDelete,
    NewAccount,
    Groups,      // 分组管理（列表 + 新建 + 重命名 + 删除）
    RenameGroup, // 重命名分组（dialog_old = 旧组名）
    MoveGroup,   // 将账号移动到分组（dialog_move_idx = 账号下标）
}

// =====================================================================
// 连接标签页（每个打开的远端连接一套独立状态）
// =====================================================================

struct ConnTab {
    id: String,              // 后端会话 id（连接成功后填充）
    token: u64,              // 前端发起连接时的匹配 token
    account: SavedAccount,   // 连接参数（名称/IP/端口 + 密码，用于展示与自动重连）
    connecting: bool,
    connected: bool,
    shell_alive: bool,
    status: String,
    login_log: Vec<String>,  // 登录过程日志（未连接时显示在终端区域）
    term: Option<TermState>,
    cwd: String,
    entries: Vec<ssh::FileEntry>,
    selected: Option<String>,
    last_ok_cwd: Option<String>,
    sysinfo: Vec<(String, String)>,
    manual_disconnect: bool,
    reconnect_attempts: u32,
    ping_timer: f64,
    /// 最近一次 ping 发出的时刻（用于计算 RTT 延迟）
    ping_sent_at: Option<std::time::Instant>,
    /// 最近一次 ping 往返延迟（毫秒）
    ping_ms: Option<u64>,
    sysinfo_timer: f64,
    /// 用户是否手动在文件面板打开过路径（此后停止终端路径自动联动）
    user_navigated: bool,
    /// 定时把终端 cwd 同步到文件面板的计时器
    sync_timer: f64,
    /// Tab 补全等待目录列表：待补全的词
    pending_tab_word: Option<String>,
    /// Tab 补全等待目录列表：发起请求时的终端目录
    pending_tab_dir: Option<String>,
    /// 需要把键盘焦点交给终端（连接成功 / 切换标签后置位，渲染时消费）
    focus_term: bool,
}

impl ConnTab {
    fn new(acc: SavedAccount, token: u64) -> Self {
        Self {
            id: String::new(),
            token,
            account: acc,
            connecting: false,
            connected: false,
            shell_alive: false,
            status: "未连接".to_string(),
            login_log: Vec::new(),
            term: None,
            cwd: "/".to_string(),
            entries: Vec::new(),
            selected: None,
            last_ok_cwd: None,
            sysinfo: Vec::new(),
            manual_disconnect: false,
            reconnect_attempts: 0,
            ping_timer: 0.0,
            ping_sent_at: None,
            ping_ms: None,
            sysinfo_timer: 0.0,
            user_navigated: false,
            sync_timer: 0.0,
            pending_tab_word: None,
            pending_tab_dir: None,
            focus_term: false,
        }
    }
}

// =====================================================================
// 应用
// =====================================================================

struct SshApp {
    // 连接表单（新增账号 / 发起连接参数）
    host: String,
    port: String,
    user: String,
    password: String,
    key_path: Option<PathBuf>,
    remember_pwd: bool,
    /// 新增账号对话框中的分组名输入。
    new_group: String,
    // 已存账号
    accounts: Vec<SavedAccount>,
    /// 左侧服务器列表搜索关键字。
    search: String,
    /// 最近会话（快速重连）。
    recent: Vec<RecentSession>,
    // 打开的连接标签页
    sessions: Vec<ConnTab>,
    active: usize,
    next_token: u64,
    // 后台通道
    tx: Sender<FsMsg>,
    rx: Receiver<BackMsg>,
    // 对话框
    dialog: Option<DialogKind>,
    /// 上一帧窗口是否拥有焦点（用于热键注册/注销的状态管理）
    last_focused: bool,
    dialog_input: String,
    dialog_old: String,
    /// 分组维护列表（有序，不含「未分组」）；持久化到 groups.tsv。
    groups: Vec<String>,
    /// 「移动到分组」弹窗的目标账号下标。
    dialog_move_idx: Option<usize>,
    // 传输明细记录（上传 / 下载）
    transfer_log: Vec<TransferRec>,
    show_transfer: bool,
    /// 后台当前正在执行的传输方向（Some(up) = 正在传输，None = 空闲）；用于显示「取消」按钮
    active_transfer: Option<bool>,
    /// 文件操作区域是否显示（可关闭/打开）
    show_files: bool,
    /// 文件面板固定高度（首次按窗口一半计算，之后保持不变，防止高度漂移动画）
    files_h: Option<f32>,
    /// 双击远程文件后待打开的本地临时文件（下载完成后触发系统默认应用）。
    pending_open: Option<PathBuf>,
    /// 双栏文件传输模式（本地 ⇄ 远程对照面板）。
    show_dual: bool,
    /// 本地面板当前目录。
    local_dir: String,
    /// 本地面板目录列表。
    local_entries: Vec<LocalEntry>,
    /// 本地面板选中项（绝对路径）。
    local_selected: Option<String>,
    /// 当前传输任务进度 (done, total)。
    cur_prog: Option<(u64, u64)>,
    /// 当前传输瞬时速率（字节/秒）。
    cur_rate: f64,
    /// 速率计算锚点（上次进度采样）。
    last_prog: Option<(u64, u64, std::time::Instant)>,
}

/// 本地面板的一行条目。
#[derive(Clone)]
struct LocalEntry {
    name: String,
    path: String,
    is_dir: bool,
    size: u64,
    mtime: String,
}

/// 一条传输明细记录。
struct TransferRec {
    time: String,       // 发起时间 HH:MM:SS
    up: bool,           // true=上传 false=下载
    name: String,       // 文件名 / 目录名
    size: Option<u64>,  // 完成时的传输字节数
    ok: bool,
    detail: String,     // 完成 / 失败原因
    progress: Option<(u64, u64)>, // 进行中：已传字节 / 总字节（实时进度条）
    is_dir: bool,       // true=目录（图标区分显示）
}

/// 更新最近一条未完成传输的结果；找不到则兜底追加一条。
fn finish_transfer(log: &mut Vec<TransferRec>, up: bool, size: Option<u64>, ok: bool, detail: String) {
    let mut found = false;
    for r in log.iter_mut().rev() {
        if r.up == up && !r.ok && r.detail == "进行中..." {
            r.size = size;
            r.ok = ok;
            r.detail = detail.clone();
            r.progress = None;
            found = true;
            break;
        }
    }
    if !found {
        log.push(TransferRec {
            time: now_hms(),
            up,
            name: detail.clone(),
            size,
            ok,
            detail: if ok { "完成".to_string() } else { detail },
            progress: None,
            is_dir: false,
        });
    }
    persist_transfers(log);
}

// =====================================================================
// 传输明细持久化：exe 同目录 transfers.log（TSV：time\tup\tname\tsize\tok\tdetail）
// =====================================================================

fn transfers_path() -> std::path::PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("transfers.log")))
        .unwrap_or_else(|| std::path::PathBuf::from("transfers.log"))
}

/// TSV 字段净化：字段内的制表符 / 换行替换为空格，保证单行可解析。
fn sanitize_tsv(s: &str) -> String {
    s.replace('\t', " ").replace('\n', " ").replace('\r', " ")
}

/// 启动时加载历史传输记录；中断的（进行中）记录不保留。
fn load_transfers() -> Vec<TransferRec> {
    load_transfers_from(&transfers_path())
}

fn load_transfers_from(path: &std::path::Path) -> Vec<TransferRec> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in text.lines() {
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() < 6 {
            continue;
        }
        let ok = parts[4] == "1";
        if !ok {
            continue; // 进程退出即中断，进行中记录无意义
        }
        out.push(TransferRec {
            time: parts[0].to_string(),
            up: parts[1] == "1",
            name: parts[2].to_string(),
            size: parts[3].parse::<u64>().ok(),
            ok,
            detail: parts[5].to_string(),
            progress: None,
            is_dir: parts.get(6).map(|v| *v == "1").unwrap_or(false),
        });
    }
    out
}

/// 全量写入传输明细（记录量小，直接整体覆盖）。
fn persist_transfers(log: &[TransferRec]) {
    persist_transfers_to(&transfers_path(), log);
}

fn persist_transfers_to(path: &std::path::Path, log: &[TransferRec]) {
    let mut s = String::new();
    for r in log {
        s.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
            sanitize_tsv(&r.time),
            if r.up { 1 } else { 0 },
            sanitize_tsv(&r.name),
            r.size.map(|n| n.to_string()).unwrap_or_default(),
            if r.ok { 1 } else { 0 },
            sanitize_tsv(&r.detail),
            if r.is_dir { 1 } else { 0 },
        ));
    }
    let _ = std::fs::write(path, s);
}

impl SshApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        // egui 默认字体不含 CJK，按平台加载系统中文字体作为回退，避免中文显示为方块；
        // 同时从 exe 同目录加载 emoji 图标字体（icon_font，带纵向微调），
        // 保证 ⬆/↻/🗑/📂 等按钮图标在 Windows / macOS / Linux 上统一渲染、位置居中
        // （egui 无法渲染彩色位图 emoji 字体，需用内置 emoji-icon-font 副本兜底）。
        let mut fonts = egui::FontDefinitions::default();

        // (a) CJK 中文字体：按平台候选路径，首个存在者生效
        let cjk_candidates: &[&str] = if cfg!(target_os = "macos") {
            &[
                "/System/Library/Fonts/PingFang.ttc",
                "/System/Library/Fonts/STHeiti Light.ttc",
                "/System/Library/Fonts/Hiragino Sans GB.ttc",
            ]
        } else if cfg!(target_os = "linux") {
            &[
                "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
                "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.otf",
                "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
                "/usr/share/fonts/truetype/droid/DroidSansFallbackFull.ttf",
            ]
        } else {
            &["C:\\Windows\\Fonts\\simhei.ttf", "C:\\Windows\\Fonts\\msyh.ttc"]
        };
        let mut cjk_loaded = false;
        for path in cjk_candidates {
            if let Ok(bytes) = std::fs::read(path) {
                fonts.font_data.insert(
                    "cjk".to_owned(),
                    std::sync::Arc::new(egui::FontData::from_owned(bytes)),
                );
                cjk_loaded = true;
                break;
            }
        }

        // (b) 图标字体：独立加载，不依赖 CJK 是否命中（macOS / Linux 同样生效）
        let mut icon_loaded = false;
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                let p = dir.join("emoji-icon-font.ttf");
                if let Ok(icon_bytes) = std::fs::read(&p) {
                    let mut icon_data = egui::FontData::from_owned(icon_bytes);
                    icon_data.tweak.y_offset = 2.0; // 图标整体下移一点
                    fonts.font_data.insert(
                        "icon_font".to_owned(),
                        std::sync::Arc::new(icon_data),
                    );
                    icon_loaded = true;
                }
            }
        }

        // 把 cjk / icon_font 挂到两个字族末尾（默认字形优先命中，中文与图标回退到自定义字体）。
        // 注意：保留 egui 内置 emoji-icon-font（含 ✓ ✖ ⬆ ⬇ ✏ 等标准图标），
        // 仅移除会导致按钮/状态图标渲染为空。
        for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            if let Some(list) = fonts.families.get_mut(&family) {
                if cjk_loaded {
                    list.push("cjk".to_owned());
                }
                if icon_loaded {
                    list.push("icon_font".to_owned());
                }
            }
        }
        cc.egui_ctx.set_fonts(fonts);

        // 全局紧凑样式：缩小间距与控件留白，让终端/文件区域更紧凑
        let mut style = (*cc.egui_ctx.style()).clone();
        style.spacing.item_spacing = egui::vec2(5.0, 2.0);
        style.spacing.button_padding = egui::vec2(5.0, 1.0);
        style.spacing.interact_size.y = 20.0;
        cc.egui_ctx.set_style(style);

        // 全局浅色主题：对齐设计稿"白蓝管理后台"风格（终端区单独保持黑底白字）
        let mut visuals = egui::Visuals::light();
        visuals.panel_fill = Color32::from_rgb(247, 248, 250); // 面板浅灰
        visuals.window_fill = Color32::from_rgb(255, 255, 255); // 弹窗纯白
        visuals.override_text_color = Some(Color32::from_rgb(31, 41, 55)); // 正文深灰蓝
        visuals.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0_f32, Color32::from_rgb(107, 114, 128));
        visuals.widgets.inactive.fg_stroke = egui::Stroke::new(1.0_f32, Color32::from_rgb(55, 65, 81));
        visuals.widgets.hovered.fg_stroke = egui::Stroke::new(1.0_f32, Color32::from_rgb(31, 41, 55));
        visuals.widgets.active.fg_stroke = egui::Stroke::new(1.0_f32, Color32::from_rgb(37, 99, 235));
        visuals.widgets.noninteractive.bg_fill = Color32::from_rgb(243, 244, 246);
        visuals.widgets.inactive.bg_fill = Color32::from_rgb(255, 255, 255);
        visuals.widgets.hovered.bg_fill = Color32::from_rgb(219, 234, 254); // 浅蓝 hover
        visuals.widgets.active.bg_fill = Color32::from_rgb(191, 219, 254);
        visuals.selection.bg_fill = Color32::from_rgb(59, 130, 246);
        visuals.selection.stroke = egui::Stroke::new(1.0_f32, Color32::WHITE);
        cc.egui_ctx.set_visuals(visuals);

        let (fs_tx, fs_rx) = mpsc::channel::<FsMsg>();
        // std::sync::mpsc::channel 本身无界：日志洪峰时读循环发送不阻塞，
        // SSH 窗口持续回收，避免数据通道拥塞导致 Ctrl+C（0x03）排不上队
        let (back_tx, back_rx) = mpsc::channel::<BackMsg>();
        spawn_backend(fs_rx, back_tx);
        let accounts0 = load_accounts();
        Self {
            host: String::new(),
            port: "22".into(),
            user: String::new(),
            password: String::new(),
            key_path: None,
            remember_pwd: true,
            new_group: String::new(),
            accounts: accounts0.clone(),
            search: String::new(),
            recent: load_recent(),
            sessions: Vec::new(),
            active: 0,
            next_token: 1,
            tx: fs_tx,
            rx: back_rx,
            dialog: None,
            last_focused: false,
            dialog_input: String::new(),
            dialog_old: String::new(),
            groups: merged_groups(&accounts0),
            dialog_move_idx: None,
            transfer_log: load_transfers(),
            show_transfer: true,
            active_transfer: None,
            show_files: true,
            files_h: None,
            pending_open: None,
            show_dual: false,
            local_dir: home_dir_str(),
            local_entries: Vec::new(),
            local_selected: None,
            cur_prog: None,
            cur_rate: 0.0,
            last_prog: None,
        }
    }

    fn send(&self, msg: FsMsg) {
        let _ = self.tx.send(msg);
    }

    /// 当前激活标签页索引（空列表时返回 0，调用前需判空）。
    fn active_idx(&self) -> usize {
        if self.sessions.is_empty() {
            0
        } else {
            self.active.min(self.sessions.len() - 1)
        }
    }

    /// 从左侧账号发起连接：填充表单 + 新开标签页。
    fn connect_account(&mut self, acc: &SavedAccount) {
        self.host = acc.host.clone();
        self.port = acc.port.to_string();
        self.user = acc.user.clone();
        self.password = acc.password.clone().unwrap_or_default();
        self.key_path = acc.key.clone().map(PathBuf::from);
        self.connect_with_log(true, true);
    }

    /// 发起连接。new_tab=true 时新开一个标签页；false 时复用当前标签页（自动重连）。
    fn connect_with_log(&mut self, clear_log: bool, new_tab: bool) {
        let host = self.host.trim().to_string();
        if host.is_empty() {
            self.status_set("请填写主机地址");
            return;
        }
        let port: u16 = self.port.trim().parse().unwrap_or(22);
        let user = self.user.trim().to_string();
        let (password, key_path) = if self.key_path.is_some() {
            (
                None,
                self.key_path
                    .as_ref()
                    .map(|p| p.to_string_lossy().into_owned()),
            )
        } else {
            (Some(self.password.clone()), None)
        };
        let token = if new_tab {
            let acc = self.current_account();
            let t = self.next_token;
            self.next_token += 1;
            self.sessions.push(ConnTab::new(acc, t));
            self.active = self.sessions.len() - 1;
            t
        } else {
            self.sessions[self.active_idx()].token
        };
        let ai = self.active_idx();
        {
            let tab = &mut self.sessions[ai];
            tab.connecting = true;
            tab.manual_disconnect = false; // 用户发起连接 -> 恢复自动重连
            tab.status = "正在连接...".into();
            if clear_log {
                tab.login_log.clear();
            }
            tab.login_log.push(format!("正在连接 {host}:{port} ..."));
            tab.login_log.push(format!("用户 {user}，正在进行认证..."));
        }
        self.send(FsMsg::Connect(ConnectReq {
            token,
            host,
            port,
            user,
            password,
            key_path,
        }));
    }

    /// 设置当前标签页的状态栏文本。
    fn status_set(&mut self, s: impl Into<String>) {
        if !self.sessions.is_empty() {
            let i = self.active_idx();
            self.sessions[i].status = s.into();
        }
    }

    /// 关闭（断开）指定标签页并移除。
    fn close_tab(&mut self, idx: usize) {
        if idx >= self.sessions.len() {
            return;
        }
        // 关闭前把最近命令写入「最近会话」并落盘
        {
            let acc = self.sessions[idx].account.clone();
            let cmd = self.sessions[idx]
                .term
                .as_ref()
                .map(|tm| tm.last_cmd.clone())
                .unwrap_or_default();
            upsert_recent(
                &mut self.recent,
                RecentSession {
                    name: acc.name,
                    host: acc.host,
                    port: acc.port,
                    user: acc.user,
                    cmd,
                    when: String::new(),
                },
            );
        }
        let id = self.sessions[idx].id.clone();
        if !id.is_empty() {
            self.send(FsMsg::For {
                id,
                msg: FsInner::Disconnect,
            });
        }
        self.sessions.remove(idx);
        if self.sessions.is_empty() {
            self.active = 0;
        } else {
            self.active = self.active.min(self.sessions.len() - 1);
        }
    }

    /// 当前连接表单对应的账号（内存态，密码始终保留，供自动重连使用）。
    fn current_account(&self) -> SavedAccount {
        let host_raw = self.host.trim().to_string();
        // 支持设计稿单行输入「root@10.0.1.24」：自动拆分用户名与主机地址
        let (user, host) = match host_raw.find('@') {
            Some(i) => {
                let u = host_raw[..i].trim().to_string();
                let h = host_raw[i + 1..].trim().to_string();
                (u, h)
            }
            None => (self.user.trim().to_string(), host_raw),
        };
        let user = if user.is_empty() { "root".to_string() } else { user };
        SavedAccount {
            name: format!("{user}@{host}"),
            host,
            port: self.port.trim().parse().unwrap_or(22),
            user,
            password: if self.key_path.is_some() {
                None
            } else if self.password.is_empty() {
                None
            } else {
                Some(self.password.clone())
            },
            key: self
                .key_path
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
            group: String::new(),
        }
    }

    fn pump(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                BackMsg::Connected(Ok((token, id))) => {
                    // 按 token 匹配发起连接的标签页
                    if let Some(idx) = self.sessions.iter().position(|t| t.token == token) {
                        {
                            let tab = &mut self.sessions[idx];
                            tab.id = id;
                            tab.connecting = false;
                            tab.connected = true;
                            tab.shell_alive = true;
                            tab.term = Some(TermState::new(24, 80));
                            tab.login_log.clear(); // 登录完成，进入真实终端
                            tab.reconnect_attempts = 0; // 重连成功，重置计数
                            tab.status = "已连接，正在加载目录...".into();
                            tab.cwd = "/".into();
                            tab.selected = None;
                            tab.sysinfo.clear();
                            tab.user_navigated = false; // 新连接重新开启终端路径自动联动
                            tab.sync_timer = 0.0;
                            tab.focus_term = true; // 连接成功后键盘焦点交给命令行
                        }
                        // 全局动作：账号自动保存 + 初始目录与系统信息 + 最近会话
                        self.save_current_account();
                        let acc = self.sessions[idx].account.clone();
                        upsert_recent(
                            &mut self.recent,
                            RecentSession {
                                name: acc.name,
                                host: acc.host,
                                port: acc.port,
                                user: acc.user,
                                cmd: String::new(),
                                when: String::new(),
                            },
                        );
                        let id2 = self.sessions[idx].id.clone();
                        self.send(FsMsg::For {
                            id: id2,
                            msg: FsInner::List("/".into()),
                        });
                        let id2 = self.sessions[idx].id.clone();
                        self.send(FsMsg::For {
                            id: id2,
                            msg: FsInner::SysInfo,
                        });
                    }
                }
                BackMsg::Connected(Err((token, e))) => {
                    if let Some(tab) = self.sessions.iter_mut().find(|t| t.token == token) {
                        tab.connecting = false;
                        tab.status = format!("连接失败：{e}");
                        tab.login_log.push(format!("× 连接失败：{e}"));
                    }
                }
                BackMsg::Log(s) => {
                    if !self.sessions.is_empty() {
                        let i = self.active_idx();
                        self.sessions[i].status = s;
                    }
                }
                BackMsg::Progress { up, done, total } => {
                    // 更新最近一条对应方向的进行中记录进度（不落盘，进程退出即中断）
                    for r in self.transfer_log.iter_mut().rev() {
                        if r.up == up && !r.ok && r.detail == "进行中..." {
                            r.progress = Some((done, total));
                            break;
                        }
                    }
                    // 瞬时速率：按 0.1s 采样间隔计算（同任务同总量）
                    let now = std::time::Instant::now();
                    if let Some((ld, lt, lt0)) = self.last_prog {
                        let dt = now.duration_since(lt0).as_secs_f64();
                        if lt == total && dt >= 0.1 {
                            let speed = done.saturating_sub(ld) as f64 / dt;
                            self.cur_rate = speed.max(0.0);
                        }
                    }
                    self.last_prog = Some((done, total, now));
                    self.cur_prog = Some((done, total));
                }
                BackMsg::TransferStart { up } => {
                    self.active_transfer = Some(up);
                    self.cur_prog = None;
                    self.cur_rate = 0.0;
                    self.last_prog = None;
                }
                BackMsg::For { id, msg } => {
                    // Ping 失效 -> 自动重连；其余按会话路由
                    match msg {
                        BackInner::Ping(Err(e)) => self.handle_ping_fail(&id, e),
                        m => self.handle_back(&id, m),
                    }
                }
            }
        }
    }

    /// Ping 探针失效：自动重连（复用本标签页），除非用户手动断开或次数超限。
    fn handle_ping_fail(&mut self, id: &str, e: String) {
        let idx = match self.sessions.iter().position(|t| t.id == id) {
            Some(i) => i,
            None => return,
        };
        if self.sessions[idx].manual_disconnect || self.sessions[idx].reconnect_attempts >= 3 {
            let tab = &mut self.sessions[idx];
            tab.connected = false;
            tab.shell_alive = false;
            tab.term = None;
            tab.login_log.push(format!("× 连接已失效：{e}"));
            tab.login_log.push("已停止自动重连，请手动连接".into());
            tab.status = format!("连接已失效：{e}（已停止自动重连）");
            return;
        }
        self.sessions[idx].reconnect_attempts += 1;
        {
            let tab = &mut self.sessions[idx];
            tab.connected = false;
            tab.shell_alive = false;
            tab.term = None;
            tab.login_log.push(format!("× 连接已失效：{e}"));
            tab.login_log.push(format!(
                "正在自动重连（第 {} 次）...",
                tab.reconnect_attempts
            ));
            tab.status = format!(
                "连接失效，自动重连（第 {} 次）...",
                tab.reconnect_attempts
            );
        }
        // 清理失效会话，然后复用本标签页重连
        let old_id = self.sessions[idx].id.clone();
        self.send(FsMsg::For {
            id: old_id,
            msg: FsInner::Disconnect,
        });
        let acc = self.sessions[idx].account.clone();
        self.host = acc.host;
        self.port = acc.port.to_string();
        self.user = acc.user;
        self.password = acc.password.clone().unwrap_or_default();
        self.key_path = acc.key.map(PathBuf::from);
        self.active = idx;
        self.connect_with_log(false, false); // 保留失效提示日志
    }

    /// 按会话 id 处理各后端响应（tab 状态 + 全局传输记录）。
    fn handle_back(&mut self, id: &str, msg: BackInner) {
        let idx = match self.sessions.iter().position(|t| t.id == id) {
            Some(i) => i,
            None => return,
        };
        match msg {
            BackInner::Disconnected => {
                let tab = &mut self.sessions[idx];
                tab.connected = false;
                tab.shell_alive = false;
                tab.term = None;
                tab.status = "已断开".to_string();
            }
            BackInner::List(Ok(v)) => {
                let tab = &mut self.sessions[idx];
                tab.entries = v;
                tab.last_ok_cwd = Some(tab.cwd.clone());
                tab.status = format!("{} 项", tab.entries.len());
                // 异步 Tab 补全：目录列表到达后，若等待目录与当前终端目录一致则执行补全
                let wait_word = tab.pending_tab_word.clone();
                let wait_dir = tab.pending_tab_dir.clone();
                tab.pending_tab_word = None;
                tab.pending_tab_dir = None;
                if let (Some(w), Some(d)) = (wait_word, wait_dir) {
                    let (term_cwd, line, entries, id) = {
                        let t = &self.sessions[idx];
                        (
                            t.term.as_ref().and_then(|tm| tm.term_cwd.clone()),
                            t.term.as_ref().map(|tm| String::from_utf8_lossy(&tm.pending_line).into_owned()).unwrap_or_default(),
                            t.entries.clone(),
                            t.id.clone(),
                        )
                    };
                    if term_cwd.as_deref() == Some(d.as_str()) && last_word(&line) == w.as_str() {
                        if let Some((out, new_line)) = build_tab_completion(&entries, &line) {
                            if let Some(term) = &mut self.sessions[idx].term {
                                term.pending_line = new_line.into_bytes();
                            }
                            self.send(FsMsg::For {
                                id,
                                msg: FsInner::ShellInput(out),
                            });
                        }
                    }
                }
            }
            BackInner::List(Err(e)) => {
                // 清理挂起的 Tab 补全（列表获取失败）
                self.sessions[idx].pending_tab_word = None;
                self.sessions[idx].pending_tab_dir = None;
                // cd 联动到不存在目录等失败场景：回滚到最近一次成功加载的目录
                let mut reload_path: Option<String> = None;
                {
                    let tab = &mut self.sessions[idx];
                    if let Some(ok) = tab.last_ok_cwd.clone() {
                        if tab.cwd != ok {
                            let msg = format!("读取目录失败：{e}（已回到 {ok}）");
                            tab.cwd = ok;
                            tab.status = msg;
                            reload_path = Some(tab.cwd.clone());
                        } else {
                            tab.status = format!("读取目录失败：{e}");
                        }
                    } else {
                        tab.status = format!("读取目录失败：{e}");
                    }
                }
                if let Some(p) = reload_path {
                    let id2 = self.sessions[idx].id.clone();
                    self.send(FsMsg::For {
                        id: id2,
                        msg: FsInner::List(p),
                    });
                }
            }
            BackInner::Mkdir(Ok(())) => {
                self.sessions[idx].status = "目录已创建".into();
                self.reload_tab(idx);
            }
            BackInner::Mkdir(Err(e)) => {
                self.sessions[idx].status = format!("创建目录失败：{e}");
            }
            BackInner::Remove(Ok(())) => {
                self.sessions[idx].selected = None;
                self.sessions[idx].status = "已删除".into();
                self.reload_tab(idx);
            }
            BackInner::Remove(Err(e)) => {
                self.sessions[idx].status = format!("删除失败：{e}");
            }
            BackInner::Rename(Ok(())) => {
                self.sessions[idx].selected = None;
                self.sessions[idx].status = "已重命名".into();
                self.reload_tab(idx);
            }
            BackInner::Rename(Err(e)) => {
                self.sessions[idx].status = format!("重命名失败：{e}");
            }
            BackInner::Upload(Ok(n)) => {
                self.active_transfer = None;
                self.cur_prog = None;
                self.cur_rate = 0.0;
                self.last_prog = None;
                finish_transfer(&mut self.transfer_log, true, Some(n), true, "完成".to_string());
                self.sessions[idx].status = if n > 0 {
                    format!("上传完成（{}）", fmt_size(n))
                } else {
                    "上传完成".into()
                };
                self.reload_tab(idx);
            }
            BackInner::Upload(Err(e)) => {
                self.active_transfer = None;
                self.cur_prog = None;
                self.cur_rate = 0.0;
                self.last_prog = None;
                finish_transfer(&mut self.transfer_log, true, None, false, e.clone());
                self.sessions[idx].status = format!("上传失败：{e}");
            }
            BackInner::Download(Ok(n)) => {
                self.active_transfer = None;
                self.cur_prog = None;
                self.cur_rate = 0.0;
                self.last_prog = None;
                finish_transfer(&mut self.transfer_log, false, Some(n), true, "完成".to_string());
                self.sessions[idx].status = if n > 0 {
                    format!("下载完成（{}）", fmt_size(n))
                } else {
                    "下载完成".into()
                };
                // 双击远程文件下载：完成后用系统默认应用打开
                if let Some(p) = self.pending_open.take() {
                    let _ = opener::open(&p);
                    self.sessions[idx].status = "已用默认应用打开".into();
                }
            }
            BackInner::Download(Err(e)) => {
                self.active_transfer = None;
                self.cur_prog = None;
                self.cur_rate = 0.0;
                self.last_prog = None;
                finish_transfer(&mut self.transfer_log, false, None, false, e.clone());
                self.sessions[idx].status = format!("下载失败：{e}");
            }
            BackInner::ShellData(data) => {
                if let Some(t) = &mut self.sessions[idx].term {
                    t.feed(&data);
                }
            }
            BackInner::ShellClosed => {
                let tab = &mut self.sessions[idx];
                tab.shell_alive = false;
                if tab.connected {
                    tab.status = "远端 shell 已退出（可重新连接）".into();
                }
            }
            BackInner::SysInfo(Ok(info)) => {
                self.sessions[idx].sysinfo = info;
                self.sessions[idx].status = "已获取系统信息".into();
            }
            BackInner::SysInfo(Err(e)) => {
                self.sessions[idx].status = format!("获取系统信息失败：{e}");
            }
            BackInner::Ping(Ok(())) => {
                // 计算往返延迟（毫秒），供底部状态栏显示
                if let Some(t0) = self.sessions[idx].ping_sent_at.take() {
                    self.sessions[idx].ping_ms = Some(t0.elapsed().as_millis() as u64);
                }
            }
            BackInner::Ping(Err(_)) => {} // 已在 pump 主循环按自动重连处理
        }
    }

    fn reload_tab(&mut self, idx: usize) {
        let p = self.sessions[idx].cwd.clone();
        let id = self.sessions[idx].id.clone();
        self.send(FsMsg::For {
            id,
            msg: FsInner::List(p),
        });
    }

    fn navigate(&mut self, path: String) {
        if self.sessions.is_empty() {
            return;
        }
        let idx = self.active_idx();
        self.sessions[idx].cwd = path.clone();
        self.sessions[idx].selected = None;
        self.sessions[idx].status = "加载中...".into();
        let id = self.sessions[idx].id.clone();
        self.send(FsMsg::For {
            id,
            msg: FsInner::List(path),
        });
    }

    fn reload(&mut self) {
        if self.sessions.is_empty() {
            return;
        }
        let idx = self.active_idx();
        let p = self.sessions[idx].cwd.clone();
        let id = self.sessions[idx].id.clone();
        self.send(FsMsg::For {
            id,
            msg: FsInner::List(p),
        });
    }

    /// 更新最近一条未完成传输的结果；找不到则兜底追加一条。
    fn start_transfer(&mut self, up: bool, name: String, is_dir: bool) {
        self.transfer_log.push(TransferRec {
            time: now_hms(),
            up,
            name,
            size: None,
            ok: false,
            detail: "进行中...".to_string(),
            progress: None,
            is_dir,
        });
        // 只保留最近 200 条
        if self.transfer_log.len() > 200 {
            let keep = self.transfer_log.split_off(self.transfer_log.len() - 200);
            self.transfer_log = keep;
        }
        persist_transfers(&self.transfer_log);
    }

    /// 把当前连接表单保存为一条本地账号（连接成功后调用）。
    /// 文件里是否保存密码由「记住密码」决定。
    fn save_current_account(&mut self) {
        if self.host.trim().is_empty() || self.user.trim().is_empty() {
            return;
        }
        let mut acc = self.current_account();
        if !self.remember_pwd {
            acc.password = None;
        }
        let host = acc.host.clone();
        let user = acc.user.clone();
        upsert_account(&mut self.accounts, acc);
        save_accounts(&self.accounts);
        if !self.sessions.is_empty() {
            let i = self.active_idx();
            self.sessions[i].status = format!("账号 {user}@{host} 已保存到本地");
        }
    }
}

impl eframe::App for SshApp {
    /// 窗口背景：统一浅色（eframe 默认返回半透明近黑，会在左侧栏下方等
    /// 面板未覆盖区域露出黑色隔断）
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        egui::Color32::from_rgb(242, 244, 247).to_normalized_gamma_f32()
    }
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.pump();

        // 焦点管理：3F-Shell 获得焦点时注册全局热键 Ctrl+C（必达），失焦时注销
        // （避免影响其他程序的 Ctrl+C 复制等行为）
        #[cfg(target_os = "windows")]
        {
            let focused = ctx.input(|i| i.focused);
            if focused != self.last_focused {
                self.last_focused = focused;
                if focused {
                    kbhook::register_hotkey();
                } else {
                    kbhook::unregister_hotkey();
                }
            }
        }

        // 自动保活 / 系统信息刷新 / 终端路径定时同步：仅对当前激活标签页推进计时
        let dt = ctx.input(|i| i.stable_dt) as f64;
        if !self.sessions.is_empty() {
            let idx = self.active_idx();
            let (connected, connecting, manual, ping, sys, sync, nav_ok, term_cwd) = {
                let t = &self.sessions[idx];
                (
                    t.connected,
                    t.connecting,
                    t.manual_disconnect,
                    t.ping_timer,
                    t.sysinfo_timer,
                    t.sync_timer,
                    !t.user_navigated,
                    t.term.as_ref().and_then(|tm| tm.term_cwd.clone()),
                )
            };
            if connected && !connecting && !manual {
                if ping + dt >= 30.0 {
                    let id = self.sessions[idx].id.clone();
                    self.send(FsMsg::For { id, msg: FsInner::Ping });
                    self.sessions[idx].ping_timer = 0.0;
                    // 记录发出时刻，收到 Ok 后计算往返延迟
                    self.sessions[idx].ping_sent_at = Some(std::time::Instant::now());
                } else {
                    self.sessions[idx].ping_timer = ping + dt;
                }
                if sys + dt >= 5.0 {
                    let id = self.sessions[idx].id.clone();
                    self.send(FsMsg::For { id, msg: FsInner::SysInfo });
                    self.sessions[idx].sysinfo_timer = 0.0;
                } else {
                    self.sessions[idx].sysinfo_timer = sys + dt;
                }
                // 未手动导航时：每 3 秒把终端当前目录同步到文件面板
                if sync + dt >= 3.0 {
                    self.sessions[idx].sync_timer = 0.0;
                    if nav_ok {
                        if let Some(tc) = term_cwd {
                            let cur = self.sessions[idx].cwd.clone();
                            if tc != cur {
                                self.sessions[idx].cwd = tc;
                                self.sessions[idx].selected = None;
                                self.sessions[idx].status = "已跟随终端目录".into();
                                self.reload_tab(idx);
                            }
                        }
                    }
                } else {
                    self.sessions[idx].sync_timer = sync + dt;
                }
            }
        }
        // 空闲时也保持重绘，保证计时器推进
        ctx.request_repaint_after(std::time::Duration::from_secs(1));

        // 左侧栏：已存账号 + 状态 + 远端系统信息（连接/断开全部在左侧操作）
        // 固定宽度 240px：不随内容/文本长度缩放，也不允许拖拽
        egui::SidePanel::left("left_panel")
            .exact_width(240.0)
            .frame(
                egui::Frame::new()
                    .fill(Color32::from_rgb(242, 244, 247))
                    .inner_margin(egui::Margin::symmetric(6, 2)),
            )
            .show(ctx, |ui| left_panel(ui, self));

        // 文件面板：高度完全由用户手动拖动分隔条控制（无拖动时保持不变）。
        // 默认高度首次按窗口一半固定；之后仅拖动分隔条时更新。
        if self.files_h.is_none() {
            self.files_h = Some((ctx.available_rect().height() * 0.7).clamp(120.0, 900.0));
        }
        if self.show_files {
            egui::TopBottomPanel::bottom("files_panel")
                .exact_height(self.files_h.unwrap_or(360.0))
                .frame(
                    egui::Frame::new()
                        .fill(Color32::from_rgb(255, 255, 255))
                        .inner_margin(egui::Margin::symmetric(6, 2)),
                )
                .show(ctx, |ui| files_panel(ui, self));
        } else {
            // 关闭时底部保留标签条（白纸黑字风格，浅色底 + 黑色字体），点击重新展开
            let tab_frame = egui::Frame::new()
                .fill(Color32::from_rgb(245, 245, 248))
                .inner_margin(egui::Margin::symmetric(6, 2));
            egui::TopBottomPanel::bottom("files_tab")
                .exact_height(26.0)
                .frame(tab_frame)
                .show(ctx, |ui| {
                    ui.visuals_mut().override_text_color = Some(Color32::from_rgb(20, 20, 20));
                    ui.vertical_centered(|ui| {
                        let btn = ui
                            .add(
                                egui::Button::new(
                                    RichText::new("📂 文件操作区域（点击展开）")
                                        .color(Color32::from_rgb(20, 20, 20)),
                                )
                                .small(),
                            )
                            .on_hover_text("点击重新打开文件操作区域");
                        if btn.clicked() {
                            self.show_files = true;
                        }
                    });
                });
        }

        // 底部状态栏：当前连接 / 模式切换 / 编码 / SSH 版本 / 时间
        egui::TopBottomPanel::bottom("status_bar")
            .exact_height(24.0)
            .frame(compact_frame())
            .show(ctx, |ui| status_bar(ui, self));

        // 终端区域吃掉全部剩余空间，避免底部留白。
        // 右侧主操作区：标签栏位于命令行区域顶部，下方为终端内容
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(Color32::from_rgb(255, 255, 255))
                    .inner_margin(egui::Margin::symmetric(6, 2)),
            )
            .show(ctx, |ui| {
                tab_bar(ui, self);
                ui.separator();
                term_panel(ui, self);
            });

        // 模态对话框
        if let Some(kind) = self.dialog {
            let title = match kind {
                DialogKind::Mkdir => "新建目录",
                DialogKind::Rename => "重命名",
                DialogKind::ConfirmDelete => "确认删除",
                DialogKind::NewAccount => "新增账号",
                DialogKind::Groups => "分组管理",
                DialogKind::RenameGroup => "重命名分组",
                DialogKind::MoveGroup => "移动到分组",
            };
            let mut open = true;
            egui::Window::new(title)
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .open(&mut open)
                .show(ctx, |ui| match kind {
                    DialogKind::Mkdir => {
                        ui.label("目录名：");
                        ui.add(egui::TextEdit::singleline(&mut self.dialog_input).desired_width(260.0));
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            if ui.button("创建").clicked() {
                                let name = self.dialog_input.trim().to_string();
                                self.dialog = None;
                                if !name.is_empty() {
                                    if self.sessions.is_empty() {
                                        self.dialog_input = name;
                                    } else {
                                        let idx = self.active_idx();
                                        let p = join_path(&self.sessions[idx].cwd, &name);
                                        let id = self.sessions[idx].id.clone();
                                        self.status_set("创建中...");
                                        self.send(FsMsg::For { id, msg: FsInner::Mkdir(p) });
                                    }
                                }
                            }
                            if ui.button("取消").clicked() {
                                self.dialog = None;
                            }
                        });
                    }
                    DialogKind::Rename => {
                        let old_name = self
                            .dialog_old
                            .rsplit('/')
                            .next()
                            .unwrap_or("")
                            .to_string();
                        ui.label(format!("将「{old_name}」重命名为："));
                        ui.add(egui::TextEdit::singleline(&mut self.dialog_input).desired_width(260.0));
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            if ui.button("确定").clicked() {
                                let new_name = self.dialog_input.trim().to_string();
                                let old = self.dialog_old.clone();
                                self.dialog = None;
                                if !new_name.is_empty() {
                                    if self.sessions.is_empty() {
                                        self.dialog_input = new_name;
                                    } else {
                                        let idx = self.active_idx();
                                        let newp = join_path(&self.sessions[idx].cwd, &new_name);
                                        let id = self.sessions[idx].id.clone();
                                        self.status_set("重命名中...");
                                        self.send(FsMsg::For { id, msg: FsInner::Rename(old, newp) });
                                    }
                                }
                            }
                            if ui.button("取消").clicked() {
                                self.dialog = None;
                            }
                        });
                    }
                    DialogKind::ConfirmDelete => {
                        let name = self.dialog_old.rsplit('/').next().unwrap_or("").to_string();
                        ui.label(format!("确定删除「{name}」？"));
                        ui.label(RichText::new("目录将连同全部内容递归删除，且不可恢复").weak().small());
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            if ui
                                .button(RichText::new("删除").color(Color32::LIGHT_RED))
                                .clicked()
                            {
                                let p = self.dialog_old.clone();
                                self.dialog = None;
                                if !self.sessions.is_empty() {
                                    let idx = self.active_idx();
                                    let id = self.sessions[idx].id.clone();
                                    self.status_set("删除中...");
                                    self.send(FsMsg::For { id, msg: FsInner::Remove(p) });
                                }
                            }
                            if ui.button("取消").clicked() {
                                self.dialog = None;
                            }
                        });
                    }
                    DialogKind::NewAccount => {
                        ui.label("名称（可选，留空自动生成 用户@主机）：");
                        ui.add(egui::TextEdit::singleline(&mut self.dialog_input).desired_width(260.0));
                        ui.add_space(4.0);
                        ui.label("主机（必填）：");
                        ui.add(egui::TextEdit::singleline(&mut self.host).desired_width(260.0).hint_text("192.168.1.100"));
                        ui.label("端口：");
                        ui.add(egui::TextEdit::singleline(&mut self.port).desired_width(120.0));
                        ui.label("用户（必填）：");
                        ui.add(egui::TextEdit::singleline(&mut self.user).desired_width(260.0).hint_text("root"));
                        ui.label("密码（可留空，与密钥二选一）：");
                        ui.add(egui::TextEdit::singleline(&mut self.password).password(true).desired_width(260.0));
                        ui.checkbox(&mut self.remember_pwd, "记住密码（明文保存到本地）");
                        ui.label("分组（可选，如「生产环境」「开发测试」）：");
                        ui.add(egui::TextEdit::singleline(&mut self.new_group).desired_width(260.0).hint_text("未分组"));
                        ui.horizontal(|ui| {
                            if ui.button("选择密钥...").clicked() {
                                if let Some(p) = rfd::FileDialog::new()
                                    .add_filter("私钥", &["pem", "key"])
                                    .pick_file()
                                {
                                    self.key_path = Some(p);
                                }
                            }
                            if let Some(kp) = &self.key_path {
                                let name = kp
                                    .file_name()
                                    .map(|n| n.to_string_lossy().into_owned())
                                    .unwrap_or_default();
                                ui.weak(format!("密钥:{name}"));
                            }
                        });
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            if ui.button("保存").clicked() {
                                let host = self.host.trim().to_string();
                                let user = self.user.trim().to_string();
                                if host.is_empty() || user.is_empty() {
                                    self.status_set("主机和用户不能为空");
                                } else {
                                    let port = self.port.trim().parse().unwrap_or(22);
                                    let name = self.dialog_input.trim();
                                    let name = if name.is_empty() {
                                        format!("{user}@{host}")
                                    } else {
                                        name.to_string()
                                    };
                                    let key = self
                                        .key_path
                                        .as_ref()
                                        .map(|p| p.to_string_lossy().into_owned());
                                    let password = if key.is_some() {
                                        None
                                    } else if self.remember_pwd && !self.password.is_empty() {
                                        Some(self.password.clone())
                                    } else {
                                        None
                                    };
                                    upsert_account(
                                        &mut self.accounts,
                                        SavedAccount {
                                            name,
                                            host,
                                            port,
                                            user,
                                            password,
                                            key,
                                            group: self.new_group.trim().to_string(),
                                        },
                                    );
                                    // 同步登记新分组到分组列表（持久化 groups.tsv）
                                    let ng = self.new_group.trim().to_string();
                                    if !ng.is_empty() && !self.groups.contains(&ng) {
                                        self.groups.push(ng);
                                        save_groups(&self.groups);
                                    }
                                    save_accounts(&self.accounts);
                                    self.dialog = None;
                                    self.status_set("账号已保存到本地，点击左侧账号即可连接");
                                }
                            }
                            if ui.button("取消").clicked() {
                                self.dialog = None;
                            }
                        });
                    }
                    DialogKind::Groups => {
                        // 顶部：输入框 + 添加分组
                        ui.horizontal(|ui| {
                            ui.add(
                                egui::TextEdit::singleline(&mut self.dialog_input)
                                    .desired_width(180.0)
                                    .hint_text("新分组名称"),
                            );
                            if ui.button("添加").clicked() {
                                let name = self.dialog_input.trim().to_string();
                                if !name.is_empty() && !self.groups.contains(&name) {
                                    self.groups.push(name);
                                    save_groups(&self.groups);
                                    self.dialog_input.clear();
                                    self.status_set("分组已创建");
                                } else if !name.is_empty() {
                                    self.status_set("分组已存在");
                                }
                            }
                        });
                        ui.add_space(4.0);
                        ui.separator();
                        if self.groups.is_empty() {
                            ui.weak("暂无分组，输入名称添加");
                        }
                        // 分组列表：每行 [组名] [✎ 重命名] [× 删除]
                        let mut del_group: Option<String> = None;
                        let mut rename_group: Option<String> = None;
                        egui::ScrollArea::vertical()
                            .max_height(220.0)
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                for g in &self.groups.clone() {
                                    let n = self
                                        .accounts
                                        .iter()
                                        .filter(|a| a.group == *g)
                                        .count();
                                    ui.horizontal(|ui| {
                                        ui.strong(RichText::new(g).color(Color32::from_rgb(31, 41, 55)));
                                        ui.weak(format!("{n} 台"));
                                        if ui.small_button("✎").on_hover_text("重命名分组").clicked() {
                                            rename_group = Some(g.clone());
                                        }
                                        if ui
                                            .small_button("×")
                                            .on_hover_text("删除分组（组内服务器移至未分组）")
                                            .clicked()
                                        {
                                            del_group = Some(g.clone());
                                        }
                                    });
                                    ui.separator();
                                }
                            });
                        if let Some(g) = rename_group {
                            self.dialog_old = g;
                            self.dialog_input = self.dialog_old.clone();
                            self.dialog = Some(DialogKind::RenameGroup);
                        }
                        if let Some(g) = del_group {
                            let mut changed = false;
                            for a in self.accounts.iter_mut() {
                                if a.group == g {
                                    a.group.clear();
                                    changed = true;
                                }
                            }
                            self.groups.retain(|x| *x != g);
                            save_groups(&self.groups);
                            if changed {
                                save_accounts(&self.accounts);
                            }
                            self.status_set(format!("分组「{g}」已删除，服务器移至未分组"));
                        }
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            if ui.button("关闭").clicked() {
                                self.dialog = None;
                            }
                        });
                    }
                    DialogKind::RenameGroup => {
                        let old = self.dialog_old.clone();
                        ui.label(format!("将分组「{old}」重命名为："));
                        ui.add(egui::TextEdit::singleline(&mut self.dialog_input).desired_width(220.0));
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            if ui.button("确定").clicked() {
                                let new_name = self.dialog_input.trim().to_string();
                                if new_name.is_empty() {
                                    self.status_set("分组名不能为空");
                                } else if self
                                    .groups
                                    .iter()
                                    .any(|x| *x == new_name && *x != old)
                                {
                                    self.status_set("分组名已存在");
                                } else {
                                    let mut changed = false;
                                    for a in self.accounts.iter_mut() {
                                        if a.group == old {
                                            a.group = new_name.clone();
                                            changed = true;
                                        }
                                    }
                                    if let Some(slot) = self.groups.iter_mut().find(|x| **x == old) {
                                        *slot = new_name.clone();
                                    }
                                    save_groups(&self.groups);
                                    if changed {
                                        save_accounts(&self.accounts);
                                    }
                                    self.dialog = None;
                                    self.status_set("分组已重命名");
                                }
                            }
                            if ui.button("取消").clicked() {
                                self.dialog = None;
                            }
                        });
                    }
                    DialogKind::MoveGroup => {
                        let idx = self.dialog_move_idx;
                        let acc_name = idx
                            .and_then(|i| self.accounts.get(i))
                            .map(|a| format!("{} ({})", a.name, a.host))
                            .unwrap_or_default();
                        ui.label(format!("将「{acc_name}」移动到分组："));
                        ui.add_space(4.0);
                        // 「未分组」+ 全部分组
                        let mut target: Option<String> = None;
                        let cur = idx.and_then(|i| self.accounts.get(i)).map(|a| a.group.clone());
                        if ui
                            .selectable_label(cur.as_deref() == Some(""), "未分组")
                            .clicked()
                        {
                            target = Some(String::new());
                        }
                        for g in &self.groups {
                            if ui.selectable_label(cur.as_deref() == Some(g), g).clicked() {
                                target = Some(g.clone());
                            }
                        }
                        if let Some(t) = target {
                            if let Some(i) = idx {
                                if i < self.accounts.len() {
                                    self.accounts[i].group = t;
                                    save_accounts(&self.accounts);
                                    self.status_set("已移动到分组");
                                }
                            }
                            self.dialog = None;
                        }
                        ui.add_space(6.0);
                        if ui.button("取消").clicked() {
                            self.dialog = None;
                        }
                    }
                });
            if !open {
                self.dialog = None;
            }
        }
    }
}

// =====================================================================
// 顶部标签栏（多连接切换 / 关闭）
// =====================================================================

fn tab_bar(ui: &mut egui::Ui, app: &mut SshApp) {
    ui.horizontal(|ui| {
        ui.add_space(2.0);
        ui.strong(RichText::new("连接").color(Color32::from_rgb(31, 41, 55)));
        ui.separator();
        if app.sessions.is_empty() {
            ui.weak("未打开任何连接：点击左侧账号或「新建」发起连接");
        } else {
            let mut switch: Option<usize> = None;
            let mut close: Option<usize> = None;
            let n = app.sessions.len();
            let tab_h = 24.0;
            let radius = 3.0;
            let font_id = FontId::proportional(13.0);
            // 第一遍：按文本宽度预计算每个标签宽度（状态点 + 文本 + X 按钮 + 内边距）
            // 宽度缩小一半左右：文本区减半，固定元素（状态点/边距/X 区）保持不变
            let mut widths: Vec<f32> = Vec::with_capacity(n);
            for i in 0..n {
                let t = &app.sessions[i];
                let label_full = format!("{} ({})", t.account.name, t.account.host);
                let w_full = ui
                    .fonts(|f| f.layout_no_wrap(label_full, font_id.clone(), Color32::WHITE))
                    .size()
                    .x;
                widths.push(w_full * 0.5 + 12.0 + 9.0 + 5.0);
            }
            for i in 0..n {
                let (rect, resp) = ui.allocate_exact_size(
                    egui::vec2(widths[i], tab_h),
                    Sense::click(),
                );
                let sel = i == app.active_idx();
                let t = &app.sessions[i];
                let dot_c = if t.connecting {
                    Color32::from_rgb(255, 195, 70) // 连接中：黄
                } else if t.connected {
                    Color32::from_rgb(96, 205, 120) // 已连接：绿
                } else {
                    Color32::from_rgb(130, 130, 140) // 未连接：灰
                };
                let (label_c, bg, border) = if sel {
                    (
                        Color32::from_rgb(29, 78, 216),
                        Color32::from_rgb(219, 234, 254),
                        Color32::from_rgb(96, 165, 250),
                    )
                } else if resp.hovered() {
                    (
                        Color32::from_rgb(31, 41, 55),
                        Color32::from_rgb(243, 244, 246),
                        Color32::from_rgb(209, 213, 219),
                    )
                } else {
                    (
                        Color32::from_rgb(107, 114, 128),
                        Color32::from_rgb(255, 255, 255),
                        Color32::from_rgb(229, 231, 235),
                    )
                };
                let painter = ui.painter();
                painter.rect_filled(rect, radius, bg);
                painter.rect_stroke(rect, radius, Stroke::new(1.0_f32, border), StrokeKind::Inside);
                // 状态圆点
                painter.circle_filled(
                    egui::pos2(rect.left() + 6.0, rect.center().y),
                    2.0,
                    dot_c,
                );
                // 标签文本（宽度减半后按可用宽度截断，避免溢出到相邻标签）
                let label_full = format!("{} ({})", t.account.name, t.account.host);
                let w_full = ui
                    .fonts(|f| f.layout_no_wrap(label_full.clone(), font_id.clone(), Color32::WHITE))
                    .size()
                    .x;
                let avail = (w_full * 0.5).max(8.0);
                let max_cw = ((avail / 7.0) as usize).max(4);
                let label = truncate_wide(&label_full, max_cw);
                painter.text(
                    egui::pos2(rect.left() + 12.0, rect.center().y),
                    Align2::LEFT_CENTER,
                    label,
                    font_id.clone(),
                    label_c,
                );
                // X 关闭按钮（独立可点击区域，悬停变红）
                let x_rect = Rect::from_min_max(
                    egui::pos2(rect.right() - 12.0, rect.top() + 1.5),
                    egui::pos2(rect.right() - 3.0, rect.bottom() - 1.5),
                );
                let x_resp = ui.interact(x_rect, ui.id().with(i).with("close"), Sense::click());
                if x_resp.hovered() {
                    painter.rect_filled(x_rect, 2.0, Color32::from_rgb(190, 60, 50));
                }
                painter.text(
                    x_rect.center(),
                    Align2::CENTER_CENTER,
                    "×",
                    FontId::proportional(13.0),
                    if x_resp.hovered() {
                        Color32::WHITE
                    } else {
                        Color32::from_rgb(107, 114, 128)
                    },
                );
                if x_resp.clicked() {
                    close = Some(i);
                }
                if resp.clicked() {
                    switch = Some(i);
                }
                ui.add_space(4.0);
            }
            // 循环外统一处理（先关后切，避免索引错位）
            if let Some(i) = close {
                app.close_tab(i);
            }
            if let Some(i) = switch {
                if i < app.sessions.len() {
                    app.active = i;
                    app.sessions[i].focus_term = true; // 切换到该连接时，键盘焦点交给命令行
                }
            }
            ui.add_space(2.0);
        }
    });
}

// =====================================================================
// 左侧栏（已存账号 + 状态 + 远端系统信息）
// =====================================================================

fn left_panel(ui: &mut egui::Ui, app: &mut SshApp) {
    // ---- 服务器 ----
    ui.horizontal(|ui| {
        ui.strong(RichText::new("服务器").color(Color32::from_rgb(31, 41, 55)));
        if ui.small_button("新建").clicked() {
            app.dialog_input.clear();
            app.dialog = Some(DialogKind::NewAccount);
        }
        if ui
            .small_button("分组")
            .on_hover_text("分组管理：新建 / 重命名 / 删除分组")
            .clicked()
        {
            app.dialog_input.clear();
            app.dialog = Some(DialogKind::Groups);
        }
    });
    ui.add(
        egui::TextEdit::singleline(&mut app.search)
            .hint_text("搜索服务器...")
            .desired_width(f32::INFINITY),
    );
    ui.separator();
    if app.accounts.is_empty() {
        ui.weak("暂无账号，点「新建」添加");
    }
    let kw = app.search.trim().to_lowercase();
    let mut remove_idx: Option<usize> = None;
    let mut to_connect: Option<SavedAccount> = None;
    let mut rename_group: Option<String> = None; // 分组重命名（非空分组）
    let mut del_group: Option<String> = None;    // 分组删除（非空分组）
    // 服务器列表占左侧栏约一半高度，超出部分用滚动条查看（下半区为系统信息）
    let list_h = (ui.available_height() * 0.36).max(60.0);
    egui::ScrollArea::vertical()
        .max_height(list_h)
        .auto_shrink([false, false])
        .show(ui, |ui| {
            // 按分组聚类显示（保留分组首次出现顺序；组内保持原顺序）
            let mut groups: Vec<String> = Vec::new();
            for a in &app.accounts {
                if !groups.contains(&a.group) {
                    groups.push(a.group.clone());
                }
            }
            let mut shown = 0;
            for g in &groups {
                let members: Vec<(usize, &SavedAccount)> = app
                    .accounts
                    .iter()
                    .enumerate()
                    .filter(|(_, a)| &a.group == g)
                    .filter(|(_, a)| {
                        kw.is_empty()
                            || a.name.to_lowercase().contains(&kw)
                            || a.host.to_lowercase().contains(&kw)
                            || a.user.to_lowercase().contains(&kw)
                    })
                    .collect();
                if members.is_empty() {
                    continue;
                }
                let gname = if g.is_empty() { "未分组" } else { g.as_str() };
                if g.is_empty() {
                    ui.weak(RichText::new(format!("› {gname}")).small());
                } else {
                    // 命名分组：右侧提供「重命名 / 删除」维护入口
                    ui.horizontal(|ui| {
                        ui.weak(RichText::new(format!("› {gname}")).small());
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui
                                .small_button("×")
                                .on_hover_text("删除分组（组内服务器移至未分组）")
                                .clicked()
                            {
                                del_group = Some(g.clone());
                            }
                            if ui
                                .small_button("✏")
                                .on_hover_text("重命名分组")
                                .clicked()
                            {
                                rename_group = Some(g.clone());
                            }
                        });
                    });
                }
                for (i, a) in members {
                    shown += 1;
                    let label0 = format!("{}  {}", a.name, a.host);
                    let btn_w = (ui.available_width() - 46.0).max(70.0);
                    let max_cw = ((btn_w / 7.4) as usize).max(6); // 13px 字体半角字符宽约 7.4px
                    let label = truncate_wide(&label0, max_cw);
                    let active = app.sessions.iter().any(|t| {
                        t.account.host == a.host
                            && t.account.port == a.port
                            && t.account.user == a.user
                    });
                    let btn = if active {
                        egui::Button::new(RichText::new(label).strong())
                    } else {
                        egui::Button::new(label)
                    };
                    let mut clicked = false;
                    let mut del = false;
                    ui.horizontal(|ui| {
                        clicked = ui.add_sized([btn_w, 18.0], btn).clicked();
                        del = ui.small_button("×").on_hover_text("删除该账号").clicked();
                        if ui
                            .small_button("→")
                            .on_hover_text("移动到分组")
                            .clicked()
                        {
                            app.dialog_move_idx = Some(i);
                            app.dialog = Some(DialogKind::MoveGroup);
                        }
                    });
                    if clicked {
                        to_connect = Some(a.clone());
                    }
                    if del {
                        remove_idx = Some(i);
                    }
                }
            }
            if shown == 0 && !kw.is_empty() {
                ui.weak("无匹配账号");
            }
        });
    // 循环外统一处理，避免与 app.accounts 的迭代借用冲突
    if let Some(a) = to_connect {
        // 已打开同账号连接 -> 切到对应标签页；否则新开连接
        let existing = app.sessions.iter().position(|t| {
            t.account.host == a.host && t.account.port == a.port && t.account.user == a.user
        });
        match existing {
            Some(i) => {
                app.active = i;
                if !app.sessions.is_empty() {
                    app.sessions[i].status = "已切换到该连接".into();
                }
            }
            None => app.connect_account(&a),
        }
    }
    if let Some(i) = remove_idx {
        app.accounts.remove(i);
        save_accounts(&app.accounts);
        app.status_set("账号已删除");
    }
    if let Some(g) = rename_group {
        app.dialog_old = g;
        app.dialog_input = app.dialog_old.clone();
        app.dialog = Some(DialogKind::RenameGroup);
    }
    if let Some(g) = del_group {
        let mut changed = false;
        for a in app.accounts.iter_mut() {
            if a.group == g {
                a.group.clear();
                changed = true;
            }
        }
        app.groups.retain(|x| *x != g);
        save_groups(&app.groups);
        if changed {
            save_accounts(&app.accounts);
        }
        app.status_set(format!("分组「{g}」已删除，服务器移至未分组"));
    }
    // 系统信息区：与上方服务器列表各占一半固定高度（可独立滚动）
    ui.separator();
    // 系统信息区：占满左侧栏剩余全部高度，内容超出时始终显示滚动条
    let sys_h = ui.available_height();
    egui::ScrollArea::vertical()
        .max_height(sys_h)
        .auto_shrink([false, false])
        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysVisible)
        .show(ui, |ui| {
            system_info_block(ui, app);
        });
}

/// 系统信息区（CPU / 内存 / 硬盘柱状图 + 预警阈值），显示在右侧传输队列面板顶部。
fn system_info_block(ui: &mut egui::Ui, app: &mut SshApp) {
    ui.strong(RichText::new("系统信息").color(Color32::from_rgb(31, 41, 55)));
    ui.separator();
    let (connected, sysinfo) = app
        .sessions
        .get(app.active_idx())
        .map(|t| (t.connected, t.sysinfo.clone()))
        .unwrap_or((false, Vec::new()));
    if !connected {
        ui.weak("未连接");
        return;
    }
    if sysinfo.is_empty() {
        ui.weak("获取中...");
        return;
    }
    for (k, v) in &sysinfo {
        match k.as_str() {
            // CPU / 内存 / 硬盘：柱状图 + 详细文本 + 超过预警阈值提示
            "CPU" | "内存" | "硬盘" => {
                let pct = metric_pct(v);
                ui.horizontal(|ui| {
                    ui.weak(format!("{k}:"));
                    if let Some(pct) = pct {
                        // 预警阈值对齐设计稿：使用率达到 40% 即提示（设计稿 002：黄色预警）
                        let warn = pct >= 40.0;
                        let warn_color = Color32::from_rgb(217, 119, 6);
                        ui.add(
                            egui::ProgressBar::new(pct / 100.0)
                                .desired_width(128.0)
                                .fill(if warn {
                                    warn_color
                                } else {
                                    usage_color(pct)
                                })
                                .text(format!("{pct:.0}%")),
                        );
                    }
                });
                // 预警提示独立一行，保证完整显示（左侧栏固定 240px，行内放不下整句）
                if let Some(pct) = pct {
                    if pct >= 40.0 {
                        ui.colored_label(Color32::from_rgb(217, 119, 6), "⚠ 已超过预警阈值 40%");
                    }
                }
                ui.weak(v.clone());
            }
            _ => {
                ui.horizontal(|ui| {
                    ui.weak(format!("{k}:"));
                    ui.label(v);
                });
            }
        }
    }
    if ui.button("刷新系统信息").clicked() {
        let idx = app.active_idx();
        let id = app.sessions[idx].id.clone();
        app.send(FsMsg::For {
            id,
            msg: FsInner::SysInfo,
        });
        app.status_set("刷新系统信息中...");
    }
}

/// 从指标文本中提取百分比数值：
/// CPU "使用率 35.2% / 空闲 64.8%" -> 35.2
/// 内存 "已用 1023M / 共 15.1G（7%）" / 硬盘 "...（22%）" -> 7 / 22
fn metric_pct(v: &str) -> Option<f32> {
    if let Some(idx) = v.find("使用率") {
        let rest = &v[idx + "使用率".len()..];
        let num = rest.split('%').next().unwrap_or("").trim();
        if let Ok(f) = num.parse::<f32>() {
            return Some(f);
        }
    }
    for open in ['（', '('] {
        if let Some(oi) = v.rfind(open) {
            let rest = &v[oi + open.len_utf8()..];
            let num = rest.split('%').next().unwrap_or("").trim();
            if let Ok(f) = num.parse::<f32>() {
                return Some(f);
            }
        }
    }
    None
}

/// 按使用率分色：<60% 绿、60~85% 橙、>85% 红
fn usage_color(pct: f32) -> Color32 {
    if pct > 85.0 {
        Color32::from_rgb(220, 60, 60)
    } else if pct > 60.0 {
        Color32::from_rgb(235, 160, 40)
    } else {
        Color32::from_rgb(70, 190, 100)
    }
}

/// 当前时间 HH:MM:SS。
/// 底部状态栏：当前连接 / 文件传输模式切换 / SSH 版本 / 编码 / 时间。
fn status_bar(ui: &mut egui::Ui, app: &mut SshApp) {
    ui.horizontal(|ui| {
        // 左侧：当前连接状态
        let conn = app.sessions.get(app.active_idx()).map(|t| {
            if t.connected {
                // name 可能已含 user@host（快速连接自动拆分），状态栏用 user@host 拼接避免重复
                let u = if t.account.user.is_empty() {
                    "root"
                } else {
                    t.account.user.as_str()
                };
                format!("已连接 {}@{}", u, t.account.host)
            } else if t.connecting {
                format!("连接中... {}", t.account.name)
            } else {
                format!("未连接 {}", t.account.name)
            }
        });
        match conn {
            Some(s) => {
                ui.colored_label(Color32::from_rgb(120, 200, 120), s);
                // 对齐设计稿：连接状态下显示 ping 往返延迟
                if let Some(ms) = app
                    .sessions
                    .get(app.active_idx())
                    .and_then(|t| t.ping_ms)
                {
                    ui.weak(format!("延迟 {ms}ms"));
                }
            }
            None => {
                ui.colored_label(Color32::GRAY, "未连接");
            }
        }
        ui.separator();
        // 文件传输模式切换（对齐设计稿：状态栏展示模式）
        let mode_btn = if app.show_files {
            "📂 文件传输:开"
        } else {
            "📂 文件传输:关"
        };
        if ui
            .add(egui::Button::new(mode_btn).small())
            .on_hover_text("显示 / 隐藏文件操作区域")
            .clicked()
        {
            app.show_files = !app.show_files;
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.weak(now_hms());
            ui.separator();
            ui.weak("UTF-8");
            ui.separator();
            ui.weak("SSH-2.0");
        });
    });
}

fn now_hms() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

// =====================================================================
// 终端面板（上半部分）
// =====================================================================

/// 无连接时的中央快速连接面板（对齐设计稿：快速连接表单 + 最近会话）。
fn quick_connect_panel(ui: &mut egui::Ui, app: &mut SshApp) {
    let avail = ui.available_size();
    let (rect, _) = ui.allocate_exact_size(avail, egui::Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 0.0, Color32::from_rgb(255, 255, 255));
    let inner = rect.shrink2(egui::vec2(28.0, 18.0));
    let mut cui = ui.new_child(egui::UiBuilder::new().max_rect(inner));

    cui.add_space(4.0);
    cui.heading(
        RichText::new("连接到服务器")
            .size(22.0)
            .color(Color32::from_rgb(31, 41, 55)),
    );
    cui.add_space(4.0);
    cui.weak("输入主机地址快速连接，或从左侧选择已保存的服务器");
    cui.add_space(12.0);
    cui.strong(RichText::new("快速连接").size(14.0).color(Color32::from_rgb(31, 41, 55)));
    cui.add_space(4.0);

    // 设计稿 002：单行「用户名@主机地址」+ 端口 + 连接按钮
    cui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut app.host)
                .desired_width(240.0)
                .hint_text("用户名@主机地址，如 root@10.0.1.24"),
        );
        ui.add(
            egui::TextEdit::singleline(&mut app.port)
                .desired_width(56.0)
                .hint_text("端口"),
        );
        if ui.button(RichText::new("连接").strong()).clicked() {
            let acc = app.current_account();
            app.connect_account(&acc);
        }
    });
    cui.add_space(4.0);
    // 密码（可选）与密钥导入；对齐设计稿「支持SSH密钥/密码登录，密钥可从本机一键导入」
    cui.horizontal(|ui| {
        ui.label("密码");
        ui.add(
            egui::TextEdit::singleline(&mut app.password)
                .password(true)
                .desired_width(150.0)
                .hint_text("可选，留空用密钥/免密"),
        );
        if ui.button("导入密钥...").clicked() {
            if let Some(p) = rfd::FileDialog::new()
                .add_filter("私钥", &["pem", "key"])
                .pick_file()
            {
                app.key_path = Some(p);
            }
        }
        if let Some(kp) = &app.key_path {
            let kn = kp
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            ui.weak(format!("密钥: {kn}"));
        }
    });
    cui.add_space(4.0);
    cui.weak("支持 SSH 密钥 / 密码登录，密钥可从本机一键导入");
    cui.add_space(16.0);
    cui.strong(RichText::new("最近会话").size(14.0).color(Color32::from_rgb(31, 41, 55)));
    cui.add_space(4.0);
    if app.recent.is_empty() {
        cui.weak("暂无最近会话，连接一次后自动记录");
    } else {
        let mut to_reconnect: Option<SavedAccount> = None;
        let list_h = (rect.height() - 330.0).max(60.0);
        egui::ScrollArea::vertical()
            .max_height(list_h)
            .auto_shrink([false, false])
            .show(&mut cui, |ui| {
                let recent = app.recent.clone();
                for r in &recent {
                    let when = recent_display(&r.when);
                    let title = if r.host.is_empty() {
                        format!("{} · {}", r.name, r.user)
                    } else {
                        format!("{} · {}", r.name, r.host)
                    };
                    let sub = if r.cmd.is_empty() {
                        format!("{}@{}:{}", r.user, r.host, r.port)
                    } else {
                        r.cmd.clone()
                    };
                    let mut clicked = false;
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            if ui
                                .add_sized(
                                    [250.0, 16.0],
                                    egui::Button::new(
                                        RichText::new(truncate_wide(&title, 26)).size(12.5),
                                    )
                                    .frame(false),
                                )
                                .clicked()
                            {
                                clicked = true;
                            }
                            ui.add_sized(
                                [250.0, 15.0],
                                egui::Button::new(
                                    RichText::new(truncate_wide(&sub, 32)).size(11.5).weak(),
                                )
                                .frame(false),
                            );
                        });
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::TOP),
                            |ui| {
                                ui.label(
                                    RichText::new(when)
                                        .color(Color32::from_rgb(120, 130, 145))
                                        .size(11.5),
                                );
                            },
                        );
                    });
                    if clicked {
                        to_reconnect = Some(SavedAccount {
                            name: r.name.clone(),
                            host: r.host.clone(),
                            port: r.port,
                            user: r.user.clone(),
                            password: None,
                            key: None,
                            group: String::new(),
                        });
                    }
                    ui.separator();
                }
            });
        if let Some(acc) = to_reconnect {
            // 最近会话只记 host/user：优先用已存账号补全密码/密钥，否则直接尝试连接
            let saved = app
                .accounts
                .iter()
                .find(|a| a.host == acc.host && a.port == acc.port && a.user == acc.user)
                .cloned();
            match saved {
                Some(s) => app.connect_account(&s),
                None => app.connect_account(&acc),
            }
        }
    }
}

fn term_panel(ui: &mut egui::Ui, app: &mut SshApp) {
    let (connected, shell_alive) = app
        .sessions
        .get(app.active_idx())
        .map(|t| (t.connected, t.shell_alive))
        .unwrap_or((false, false));
    ui.horizontal(|ui| {
        ui.strong(RichText::new("远程终端").color(Color32::from_rgb(31, 41, 55)));
        if connected && shell_alive {
            ui.colored_label(Color32::from_rgb(22, 101, 52), "● 已连接");
        } else if connected {
            ui.colored_label(Color32::from_rgb(180, 83, 9), "● shell 已退出");
        } else {
            ui.colored_label(Color32::from_rgb(107, 114, 128), "未连接");
        }
        // 连接时提供断开按钮（设计稿布局调整后从左侧栏移至终端标题栏）
        if connected {
            if ui
                .add(egui::Button::new(RichText::new("断开连接").color(Color32::from_rgb(190, 40, 40))).small())
                .on_hover_text("关闭当前连接")
                .clicked()
            {
                let idx = app.active_idx();
                app.close_tab(idx);
            }
        }
        // 右上角：文件面板开关（传输队列开关已移至文件区域右上侧）
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let file_btn = if app.show_files { "📂 文件:开" } else { "📂 文件:关" };
            let fbtn = ui
                .add(egui::Button::new(file_btn).small())
                .on_hover_text("显示 / 隐藏文件操作区域");
            if fbtn.clicked() {
                app.show_files = !app.show_files;
            }
        });
    });
    ui.separator();
    if app.sessions.is_empty() {
        // 无任何连接：终端区域显示快速连接面板（快速连接表单 + 最近会话）
        quick_connect_panel(ui, app);
        return;
    }
    let idx = app.active_idx();
    let sid = app.sessions[idx].id.clone();
    let user = app.sessions[idx].account.user.clone();
    let want_focus = app.sessions[idx].focus_term;
    if let Some(term) = &mut app.sessions[idx].term {
        let tx = &app.tx;
        let (nav, out_bytes, _) = render_terminal(ui, term, &user, want_focus);
        // 键盘字节转发：Tab 键走本地补全，其余照常发送远端
        if let Some(b) = out_bytes {
            if let Some(tpos) = b.iter().position(|&x| x == 0x09) {
                let before = &b[..tpos];
                if !before.is_empty() {
                    let _ = tx.send(FsMsg::For {
                        id: sid.clone(),
                        msg: FsInner::ShellInput(before.to_vec()),
                    });
                }
                handle_tab(app, idx);
            } else {
                let _ = tx.send(FsMsg::For {
                    id: sid.clone(),
                    msg: FsInner::ShellInput(b),
                });
            }
        }
        // 终端联动：文件面板定位到对应目录（提示符解析出的路径权威，展开并规范化）
        // 仅当用户未手动在文件面板打开过路径时自动跟随
        if let Some(target) = nav {
            let target = normalize_path(&target, ".", &user);
            let cur_cwd = app.sessions[idx].cwd.clone();
            let follow = app.sessions[idx].connected
                && !app.sessions[idx].user_navigated
                && cur_cwd != target;
            if follow {
                app.navigate(target);
            }
        }
    } else {
        // 未连接 / 登录过程中：终端区域显示登录信息与提示（黑底白字）
        let avail = ui.available_size();
        let (rect, _) = ui.allocate_exact_size(avail, egui::Sense::hover());
        let painter = ui.painter();
        painter.rect_filled(rect, 0.0, Color32::BLACK);
        let font_id = FontId::monospace(13.0);
        let mut y = rect.top() + 8.0;
        let last = app.sessions[idx].login_log.len().saturating_sub(60);
        for line in app.sessions[idx].login_log.iter().skip(last) {
            let galley = painter.layout_no_wrap(line.clone(), font_id.clone(), Color32::WHITE);
            painter.galley(egui::pos2(rect.left() + 8.0, y), galley, Color32::WHITE);
            y += 16.0;
            if y > rect.bottom() - 8.0 {
                break;
            }
        }
    }
    // 全局 Ctrl+C：连接中任意焦点位置按下均作为通用中断信号发送
    // （不依赖终端焦点 / 回看状态 / 输入法，docker logs -f 等场景必达）
    // 双通道：① egui 事件（输入法未拦截时）② 系统级键盘钩子（输入法 OS 层拦截时兜底）
    if app.dialog.is_none() && app.sessions[idx].term.is_some() {
        let mut ctrl_c = ui.input(|i| {
            i.events.iter().any(|e| {
                matches!(e, egui::Event::Key { key: egui::Key::C, pressed: true, modifiers, .. }
                    if modifiers.ctrl && !modifiers.shift && !modifiers.alt)
            })
        });
        #[cfg(target_os = "windows")]
        {
            if kbhook::ctrl_c_pressed() {
                ctrl_c = true;
            }
        }
        if ctrl_c {
            let _ = app.tx.send(FsMsg::For {
                id: app.sessions[idx].id.clone(),
                msg: FsInner::SigInt,
            });
        }
    }
    // 焦点需求已消费（无论终端是否就绪）
    app.sessions[idx].focus_term = false;
}

/// 传输队列面板内容（右侧常驻）：标题行（含 ✕ 关闭）+ 实时队列概览 + 可滚动明细列表。
fn transfer_panel_content(ui: &mut egui::Ui, app: &mut SshApp) {
    let black = Color32::from_rgb(31, 41, 55);
    let gray = Color32::from_rgb(107, 114, 128);
    ui.set_min_width(240.0);
    ui.horizontal(|ui| {
        ui.strong(RichText::new("传输队列").color(black));
        // 任务计数（对齐设计稿「2个任务」）
        let n = app.transfer_log.len();
        ui.label(RichText::new(format!("{n} 个任务")).color(gray).size(11.5));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .add(egui::Button::new("X").small())
                .on_hover_text("关闭传输队列面板")
                .clicked()
            {
                app.show_transfer = false;
            }
            if ui.button("清空").clicked() {
                app.transfer_log.clear();
                persist_transfers(&app.transfer_log);
            }
        });
    });
    ui.separator();
    // 实时传输队列概览：当前任务进度 + 瞬时速率 + 全部暂停
    if let Some((done, total)) = app.cur_prog {
        if total > 0 && app.active_transfer.is_some() {
            let up = app.active_transfer == Some(true);
            let pct = (done.min(total) as f32 / total as f32).clamp(0.0, 1.0);
            let dir_icon = if up { "↑" } else { "↓" };
            ui.horizontal(|ui| {
                ui.strong(RichText::new(dir_icon).color(black));
                if app.cur_rate > 0.0 {
                    ui.label(
                        RichText::new(format!("{} / s", fmt_size(app.cur_rate as u64)))
                            .color(gray),
                    );
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add(egui::Button::new(RichText::new("⏸ 全部暂停").color(black)).small())
                        .on_hover_text("取消当前传输任务")
                        .clicked()
                    {
                        let sid = app.sessions.get(app.active_idx()).map(|t| t.id.clone());
                        if let Some(sid) = sid {
                            app.send(FsMsg::For {
                                id: sid,
                                msg: FsInner::CancelTransfer { up },
                            });
                        }
                    }
                });
            });
            ui.add(
                egui::ProgressBar::new(pct)
                    .desired_width(ui.available_width() - 8.0)
                    .fill(Color32::from_rgb(40, 120, 220))
                    .text(format!(
                        "{:.0}%  {} / {}",
                        pct * 100.0,
                        fmt_size(done),
                        fmt_size(total)
                    )),
            );
            ui.separator();
        }
    }
    if app.transfer_log.is_empty() {
        ui.label(RichText::new("暂无传输记录").color(gray));
        return;
    }
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .max_height((ui.available_height() - 30.0).max(120.0))
        .show(ui, |ui| {
            for r in &app.transfer_log {
                let (arrow, color) = if r.up {
                    // 用内置字体箭头（emoji ⬆/⬇ 在部分平台默认字体缺失不渲染）
                    ("↑", Color32::from_rgb(30, 80, 160))
                } else {
                    ("↓", Color32::from_rgb(20, 110, 50))
                };
                let status_color = if r.ok {
                    Color32::from_rgb(20, 120, 60)
                } else {
                    Color32::from_rgb(190, 40, 40)
                };
                let size_txt = r.size.map(fmt_size).unwrap_or_else(|| "—".to_string());
                // 对齐设计稿：条目一行 = 方向 + 图标 + 文件名 + 右侧状态（百分比 / 已完成）
                ui.horizontal(|ui| {
                    ui.label(RichText::new(arrow).color(color));
                    // 文件/文件夹矢量图标区分显示（与文件区域风格一致，浅底深色版）
                    let (icon_rect, _) =
                        ui.allocate_exact_size(egui::vec2(15.0, 13.0), egui::Sense::hover());
                    paint_file_icon(ui.painter(), icon_rect, r.is_dir, false, false, true);
                    // 文件名固定宽度截断（剩余宽度留给右侧状态，避免换行/重叠）
                    let name = truncate_wide(&r.name, 12);
                    let name_resp = ui.add_sized(
                        [115.0, 16.0],
                        egui::Label::new(RichText::new(name).color(black)).truncate(),
                    );
                    name_resp.on_hover_text(format!(
                        "{}\n{} · {}",
                        r.name,
                        size_txt,
                        if r.time.is_empty() { "—" } else { &r.time }
                    ));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        // 状态：进行中显示实时百分比，完成显示「已完成 + 自绘绿色对勾图标」
                        // （✓ U+2713 不在任何已加载字体字族中，用 painter 矢量绘制保证跨平台渲染）
                        if r.ok {
                            let (rect, _) =
                                ui.allocate_exact_size(egui::vec2(16.0, 16.0), egui::Sense::hover());
                            let c = rect.center();
                            let painter = ui.painter();
                            painter.circle_filled(
                                egui::pos2(c.x, c.y),
                                7.0,
                                Color32::from_rgb(34, 197, 94),
                            );
                            painter.line_segment(
                                [
                                    egui::pos2(c.x - 3.4, c.y),
                                    egui::pos2(c.x - 0.8, c.y + 2.6),
                                ],
                                egui::Stroke::new(1.8_f32, Color32::WHITE),
                            );
                            painter.line_segment(
                                [
                                    egui::pos2(c.x - 0.8, c.y + 2.6),
                                    egui::pos2(c.x + 3.6, c.y - 2.6),
                                ],
                                egui::Stroke::new(1.8_f32, Color32::WHITE),
                            );
                            ui.label(RichText::new("已完成").color(status_color));
                        } else {
                            let status_txt = if r.detail == "进行中..." {
                                match r.progress {
                                    Some((done, total)) if total > 0 => format!(
                                        "{:.0}%",
                                        done.min(total) as f32 * 100.0 / total as f32
                                    ),
                                    _ => "进行中...".to_string(),
                                }
                            } else {
                                r.detail.clone()
                            };
                            ui.label(RichText::new(status_txt).color(status_color));
                        }
                    });
                });
                // 进行中：实时进度条（已传 / 总字节）
                if !r.ok && r.detail == "进行中..." {
                    if let Some((done, total)) = r.progress {
                        if total > 0 {
                            let pct = (done.min(total) as f32 / total as f32).clamp(0.0, 1.0);
                            ui.add(
                                egui::ProgressBar::new(pct)
                                    .desired_width(ui.available_width() - 24.0)
                                    .fill(Color32::from_rgb(40, 120, 220))
                                    .text(format!(
                                        "{:.0}%  {} / {}",
                                        pct * 100.0,
                                        fmt_size(done),
                                        fmt_size(total)
                                    )),
                            );
                        }
                    }
                    // 取消按钮：仅当前正在传输的该方向记录显示
                    if app.active_transfer == Some(r.up) {
                        let sid = app.sessions.get(app.active_idx()).map(|t| t.id.clone());
                        if let Some(sid) = sid {
                            if ui
                                .add(egui::Button::new(RichText::new("取消").color(black)).small())
                                .on_hover_text("取消本次传输")
                                .clicked()
                            {
                                app.send(FsMsg::For {
                                    id: sid,
                                    msg: FsInner::CancelTransfer { up: r.up },
                                });
                            }
                        }
                    }
                }
            }
        });
    ui.add_space(2.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new(format!("共 {} 条", app.transfer_log.len())).color(gray));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.weak("完成后自动停留 10 秒");
        });
    });
}

/// 渲染终端；返回 (联动目标路径, 待转发的键盘字节, Ctrl+C 中断标志)。Tab 键由外层做本地补全。
fn render_terminal(
    ui: &mut egui::Ui,
    term: &mut TermState,
    user: &str,
    want_focus: bool,
) -> (Option<String>, Option<Vec<u8>>, bool) {
    let mut nav: Option<String> = None;
    let mut out_bytes: Option<Vec<u8>> = None;
    let avail = ui.available_size();
    let (rect, resp) = ui.allocate_exact_size(avail, egui::Sense::click_and_drag());
    if want_focus {
        // 连接成功 / 切换标签后，把键盘焦点直接交给终端（无需点击）
        resp.request_focus();
    }
    if resp.clicked() {
        // 点击终端正文回到最新 + 清除选择；点击右侧滚动条条带保持回看位置
        if let Some(pos) = resp.interact_pointer_pos() {
            if pos.x < rect.right() - 12.0 {
                term.scroll_offset = 0;
                term.sel = None;
            }
        }
        resp.request_focus();
    }
    let has_focus = resp.has_focus();
    if has_focus {
        // 锁住终端焦点：Tab / 方向键 / Esc 全部留给终端输入，
        // 不触发 egui 的焦点遍历（否则按 Tab 会跳到界面上的按钮）
        ui.ctx().memory_mut(|mem| {
            mem.set_focus_lock_filter(
                resp.id,
                egui::EventFilter {
                    tab: true,
                    horizontal_arrows: true,
                    vertical_arrows: true,
                    escape: true,
                    ..Default::default()
                },
            );
        });
    }

    // 终端固定黑底白字
    let bg = Color32::BLACK;
    let painter = ui.painter();
    painter.rect_filled(rect, 0.0, bg);

    let font_id = FontId::monospace(13.0);
    let (cell_w, row_h) = ui.fonts(|f| (f.glyph_width(&font_id, 'M'), f.row_height(&font_id)));
    if cell_w <= 0.0 || row_h <= 0.0 {
        return (None, None, false);
    }

    // 按可用区域计算行列并 resize
    let cols = ((rect.width() / cell_w).floor() as u16).max(2);
    let rows = ((rect.height() / row_h).floor() as u16).max(2);
    if (cols, rows) != term.dims {
        term.dims = (cols, rows);
        term.parser.set_size(rows, cols);
    }
    // 应用回看偏移；set_scrollback 内部会 clamp 到实际历史行数，回读生效值修正
    term.parser.set_scrollback(term.scroll_offset);
    let screen = term.parser.screen();
    let applied = screen.scrollback();
    if applied != term.scroll_offset {
        term.scroll_offset = applied;
    }
    let (srows, scols) = screen.size();
    let cursor = screen.cursor_position(); // (row, col)
    let clip = painter.with_clip_rect(rect);

    // 鼠标拖选（cell 尺寸已知后）：按下记录锚点，拖动实时更新范围，松开保留选择
    if resp.drag_started() || resp.dragged() {
        if let Some(pos) = resp.interact_pointer_pos() {
            let cell = pos_to_cell(pos, rect, cell_w, row_h, srows, scols);
            match term.sel {
                Some((anchor, _)) => term.sel = Some((anchor, cell)),
                None => term.sel = Some((cell, cell)),
            }
        }
    }

    for r in 0..srows {
        let mut job = LayoutJob::default();
        let mut seg = String::new();
        let mut seg_fmt: Option<TextFormat> = None;

        for c in 0..scols {
            let cell = screen.cell(r, c);
            let (text, fg, bgc, underline, italic) = match cell {
                Some(cell) => {
                    let mut fg = vt_color(cell.fgcolor());
                    let mut bgc = if matches!(cell.bgcolor(), vt100::Color::Default) {
                        Color32::BLACK
                    } else {
                        vt_color(cell.bgcolor())
                    };
                    if cell.inverse() {
                        std::mem::swap(&mut fg, &mut bgc);
                    }
                    let text = if cell.contents().is_empty() {
                        String::from(" ")
                    } else {
                        cell.contents()
                    };
                    (text, fg, bgc, cell.underline(), cell.italic())
                }
                None => (
                    String::from(" "),
                    Color32::WHITE,
                    Color32::TRANSPARENT,
                    false,
                    false,
                ),
            };

            // 光标：反色显示（回看历史时不显示，避免错位）
            let is_cursor = (r, c) == cursor && term.scroll_offset == 0;
            // 鼠标选中范围：蓝底白字高亮
            let is_sel = term.sel.map_or(false, |((ar, ac), (cr, cc))| {
                let (r0, r1) = (ar.min(cr), ar.max(cr));
                if r < r0 || r > r1 {
                    return false;
                }
                if r0 == r1 {
                    let (c0, c1) = (ac.min(cc), ac.max(cc));
                    c >= c0 && c <= c1
                } else if r == r0 {
                    c >= ac.min(scols - 1)
                } else if r == r1 {
                    c <= cc.min(scols - 1)
                } else {
                    true
                }
            });
            let mut fmt = TextFormat {
                font_id: font_id.clone(),
                color: fg,
                background: bgc,
                ..Default::default()
            };
            if italic {
                fmt.italics = true;
            }
            if underline {
                fmt.underline = egui::Stroke::new(1.0_f32, fg);
            }
            if is_cursor {
                fmt.background = Color32::from_rgb(0xf0, 0xf0, 0xf0);
                fmt.color = Color32::from_rgb(0x10, 0x10, 0x10);
            }
            if is_sel && !is_cursor {
                fmt.background = Color32::from_rgb(0x2a, 0x5a, 0xaa);
                fmt.color = Color32::WHITE;
            }

            if seg_fmt.as_ref() == Some(&fmt) {
                seg.push_str(&text);
            } else {
                if !seg.is_empty() || seg_fmt.is_some() {
                    job.append(&seg, 0.0, seg_fmt.clone().unwrap_or_default());
                }
                seg = text;
                seg_fmt = Some(fmt);
            }
        }
        if !seg.is_empty() || seg_fmt.is_some() {
            job.append(&seg, 0.0, seg_fmt.clone().unwrap_or_default());
        }

        let galley = ui.fonts(|f| f.layout_job(job));
        let pos = egui::pos2(rect.left(), rect.top() + r as f32 * row_h);
        clip.galley(pos, galley, Color32::WHITE);
    }

    // 右键菜单：复制选中文本 / 清除选择（只收集意图，screen 借用结束后统一处理）
    let mut menu_copy = false;
    let mut menu_clear = false;
    resp.context_menu(|ui| {
        let has_sel = term.sel.is_some();
        if has_sel && ui.button("复制选中文本").clicked() {
            menu_copy = true;
            ui.close_menu();
        }
        if has_sel && ui.button("清除选择").clicked() {
            menu_clear = true;
            ui.close_menu();
        }
        if !has_sel {
            ui.weak("（无选中文本）");
        }
    });

    // 先收集键盘字节与最后一行文本，结束对 screen 的借用，再做 &mut 状态更新
    let (bytes, copied, ctrl_c) = if has_focus {
        if term.scroll_offset > 0 {
            // 回看历史时：仅在真实按键时回到最新；本帧不读取输入（屏幕已偏移，避免错位转发）
            if ui.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Key { .. }))) {
                term.scroll_offset = 0;
            }
            (None, false, false)
        } else {
            let (b, c, cc) = term_input_bytes(ui, screen, term.sel);
            (Some(b), c, cc)
        }
    } else {
        (None, false, false)
    };
    let prompt_line = last_row_text(screen);
    if copied {
        // Ctrl+C（有选择时）/ Ctrl+Shift+C 已复制文本，清除选择
        term.sel = None;
    }
    if menu_copy {
        if let Some(t) = term.sel.and_then(|s| selected_text(&term.parser.screen(), Some(s))) {
            ui.ctx().copy_text(t);
        }
        term.sel = None;
    }
    if menu_clear {
        term.sel = None;
    }

    // 滚轮回看历史（仅终端区域 hover 时；向上滚 = 看历史）
    if resp.hovered() {
        let delta = ui.input(|i| i.raw_scroll_delta.y);
        if delta != 0.0 {
            let step: i64 = if delta > 0.0 { 3 } else { -3 };
            term.scroll_offset = ((term.scroll_offset as i64) + step).max(0) as usize;
        }
    }

    // 键盘输入：feed_input 累积输入行（cd 联动检测），字节返回给外层（Tab 由外层本地补全）
    if let Some(bytes) = bytes {
        if !bytes.is_empty() {
            // 有真实输入时确保回到最新（冗余保护）
            if term.scroll_offset > 0 {
                term.scroll_offset = 0;
            }
            // 输入行中检测到 cd 命令 -> 乐观联动文件面板
            if let Some(t) = term.feed_input(&bytes, user) {
                term.term_cwd = Some(t.clone());
                nav = Some(t);
            }
            out_bytes = Some(bytes);
        }
    } else {
        // 焦点提示 / 回看历史提示（画在终端底部）
        let hint = if term.scroll_offset > 0 {
            "↑ 回看历史中，输入任意键回到最新"
        } else {
            "点击此处获得键盘输入"
        };
        let painter = ui.painter();
        let galley = painter.layout_no_wrap(
            hint.to_string(),
            FontId::monospace(12.0),
            Color32::from_rgba_unmultiplied(255, 255, 255, 120),
        );
        let pos = egui::pos2(
            rect.right() - galley.size().x - 8.0,
            rect.bottom() - galley.size().y - 6.0,
        );
        painter.galley(pos, galley, Color32::WHITE);
    }

    // 提示符解析：远端已切换目录时（权威值）纠正文件面板位置；回看历史时跳过
    if term.scroll_offset == 0 {
        if let Some(t) = term.detect_prompt(&prompt_line) {
            term.term_cwd = Some(expand_tilde(&t, user));
            nav = Some(t);
        }
    }

    // 右侧滚动条（回看指示 + 点击跳转）
    draw_term_scrollbar(ui, rect, term);

    (nav, out_bytes, ctrl_c)
}

/// 矢量绘制文件/文件夹小图标（替代 emoji，黑色主题下更清晰美观）。
fn paint_file_icon(
    painter: &egui::Painter,
    rect: egui::Rect,
    is_dir: bool,
    is_symlink: bool,
    selected: bool,
    light_bg: bool,
) {
    if is_dir {
        // 文件夹：标签 + 主体（暖金色，选中时提亮；浅色背景用深金保证对比度）
        let tab = egui::Rect::from_min_max(
            egui::pos2(rect.left(), rect.top() + 1.0),
            egui::pos2(rect.left() + rect.width() * 0.58, rect.top() + 3.5),
        );
        let body = egui::Rect::from_min_max(
            egui::pos2(rect.left(), rect.top() + 3.0),
            egui::pos2(rect.right() - 1.0, rect.bottom() - 1.0),
        );
        let (fill, tab_c, hi_c) = if light_bg {
            (
                Color32::from_rgb(196, 148, 52),
                Color32::from_rgb(172, 124, 38),
                Color32::from_rgb(220, 178, 96),
            )
        } else if selected {
            (
                Color32::from_rgb(252, 205, 100),
                Color32::from_rgb(235, 180, 70),
                Color32::from_rgb(255, 228, 145),
            )
        } else {
            (
                Color32::from_rgb(235, 185, 80),
                Color32::from_rgb(210, 160, 55),
                Color32::from_rgb(250, 215, 130),
            )
        };
        painter.rect_filled(tab, 1.5, tab_c);
        painter.rect_filled(body, 2.0, fill);
        let hi = egui::Rect::from_min_max(
            egui::pos2(body.left() + 2.0, body.top() + 2.0),
            egui::pos2(body.right() - 2.0, body.top() + 4.5),
        );
        painter.rect_filled(hi, 1.5, hi_c);
    } else if is_symlink {
        // 符号链接：蓝色圆环 + 白色箭头
        let c = rect.center();
        let r = (rect.width().min(rect.height()) * 0.40).max(3.5);
        let ring = if selected {
            Color32::from_rgb(120, 190, 255)
        } else {
            Color32::from_rgb(95, 165, 245)
        };
        painter.circle_stroke(c, r, egui::Stroke::new(1.5_f32, ring));
        // 内部小箭头
        let a0 = egui::pos2(c.x - 2.5, c.y + 2.5);
        let a1 = egui::pos2(c.x + 2.5, c.y - 2.5);
        painter.line_segment([a0, a1], egui::Stroke::new(1.2_f32, ring));
        painter.line_segment(
            [a1, egui::pos2(a1.x - 3.0, a1.y)],
            egui::Stroke::new(1.2_f32, ring),
        );
    } else {
        // 文件：圆角矩形 + 右下折角 + 文字线（浅色背景用深色，保证白底可见）
        let body = egui::Rect::from_min_max(
            egui::pos2(rect.left(), rect.top() + 1.0),
            egui::pos2(rect.right() - 1.0, rect.bottom() - 1.0),
        );
        let fill = if light_bg {
            Color32::from_rgb(96, 112, 138)
        } else if selected {
            Color32::from_rgb(240, 248, 255)
        } else {
            Color32::from_rgb(214, 224, 238)
        };
        painter.rect_filled(body, 2.0, fill);
        let tri = vec![
            egui::pos2(body.right() - 5.0, body.top()),
            egui::pos2(body.right(), body.top() + 5.0),
            egui::pos2(body.right() - 5.0, body.top() + 5.0),
        ];
        let (fold_c, line_c) = if light_bg {
            (
                Color32::from_rgb(52, 62, 80),
                Color32::from_rgb(70, 84, 106),
            )
        } else {
            (
                Color32::from_rgb(120, 135, 155),
                Color32::from_rgb(150, 165, 185),
            )
        };
        painter.add(egui::Shape::convex_polygon(
            tri,
            fold_c,
            egui::Stroke::NONE,
        ));
        painter.line_segment(
            [
                egui::pos2(body.left() + 2.0, body.top() + 5.5),
                egui::pos2(body.right() - 6.0, body.top() + 5.5),
            ],
            egui::Stroke::new(1.0_f32, line_c),
        );
        painter.line_segment(
            [
                egui::pos2(body.left() + 2.0, body.top() + 8.0),
                egui::pos2(body.right() - 6.0, body.top() + 8.0),
            ],
            egui::Stroke::new(1.0_f32, line_c),
        );
    }
}

/// 取终端屏幕最后一行的纯文本（用于提示符解析）。
fn last_row_text(screen: &vt100::Screen) -> String {
    let (rows, cols) = screen.size();
    let mut line = String::new();
    for c in 0..cols {
        if let Some(cell) = screen.cell(rows.saturating_sub(1), c) {
            line.push_str(&cell.contents());
        }
    }
    line
}

/// 鼠标坐标 -> 终端字符格 (row, col)，越界自动 clamp。
fn pos_to_cell(
    pos: egui::Pos2,
    rect: egui::Rect,
    cell_w: f32,
    row_h: f32,
    srows: u16,
    scols: u16,
) -> (u16, u16) {
    let col = ((pos.x - rect.left()) / cell_w.max(1.0)).floor() as i32;
    let row = ((pos.y - rect.top()) / row_h.max(1.0)).floor() as i32;
    let row = row.clamp(0, srows.saturating_sub(1) as i32) as u16;
    let col = col.clamp(0, scols.saturating_sub(1) as i32) as u16;
    (row, col)
}

/// 提取选中矩形范围内的文本（行间以换行拼接，行尾空格去除）。
fn selected_text(
    screen: &vt100::Screen,
    sel: Option<((u16, u16), (u16, u16))>,
) -> Option<String> {
    let ((ar, ac), (cr, cc)) = sel?;
    let (srows, scols) = screen.size();
    let (r0, r1) = (ar.min(cr).min(srows.saturating_sub(1)), ar.max(cr).min(srows.saturating_sub(1)));
    let mut lines = Vec::new();
    for r in r0..=r1 {
        let (c0, c1) = if r0 == r1 {
            (ac.min(cc).min(scols.saturating_sub(1)), ac.max(cc).min(scols.saturating_sub(1)))
        } else if r == r0 {
            (ac.min(scols.saturating_sub(1)), scols.saturating_sub(1))
        } else if r == r1 {
            (0, cc.min(scols.saturating_sub(1)))
        } else {
            (0, scols.saturating_sub(1))
        };
        let mut s = String::new();
        for c in c0..=c1 {
            if let Some(cell) = screen.cell(r, c) {
                s.push_str(&cell.contents());
            } else {
                s.push(' ');
            }
        }
        lines.push(s.trim_end_matches(' ').to_string());
    }
    if lines.iter().all(|l| l.is_empty()) {
        None
    } else {
        Some(lines.join("\n"))
    }
}

/// 终端右侧垂直滚动条：指示回看位置，支持点击跳转（与 VtParser 历史容量一致）。
fn draw_term_scrollbar(ui: &mut egui::Ui, rect: egui::Rect, term: &mut TermState) {
    const MAX_SB: usize = 2000; // VtParser::new 的 scrollback 容量
    let sb_w = 8.0;
    let sb_rect = egui::Rect::from_min_max(
        egui::pos2(rect.right() - sb_w - 1.0, rect.top() + 2.0),
        egui::pos2(rect.right() - 1.0, rect.bottom() - 2.0),
    );
    let painter = ui.painter();
    painter.rect_filled(sb_rect, 2.0, Color32::from_gray(30));

    let vis_rows = term.dims.0 as usize;
    let total = vis_rows + MAX_SB;
    let max_h = sb_rect.height();
    // 窗口/终端高度不足时 max < min 会触发 f32::clamp panic：先判定再 clamp
    let slider_h = if max_h >= 16.0 {
        (sb_rect.height() * (vis_rows as f32 / total.max(1) as f32)).clamp(16.0, max_h)
    } else {
        max_h
    };
    let frac = (term.scroll_offset as f32 / MAX_SB as f32).clamp(0.0, 1.0);
    let slider_top = sb_rect.bottom() - slider_h - (sb_rect.height() - slider_h) * frac;
    let slider_color = if term.scroll_offset > 0 {
        Color32::from_rgb(150, 185, 255)
    } else {
        Color32::from_gray(115)
    };
    painter.rect_filled(
        egui::Rect::from_min_max(
            egui::pos2(sb_rect.left(), slider_top),
            egui::pos2(sb_rect.right(), slider_top + slider_h),
        ),
        2.0,
        slider_color,
    );

    // 点击滚动条跳转：点击位置越高 -> 回看越多
    let sb_resp = ui.interact(sb_rect, ui.id().with("term_scrollbar"), egui::Sense::click());
    if sb_resp.clicked() {
        if let Some(pointer) = sb_resp.interact_pointer_pos() {
            let frac2 = 1.0 - (pointer.y - sb_rect.top()) / sb_rect.height().max(1.0);
            term.scroll_offset = (frac2 * MAX_SB as f32) as usize;
        }
    }
}

/// 取输入行最后一个词（空格分隔）。
fn last_word(line: &str) -> &str {
    match line.rfind(' ') {
        Some(i) => &line[i + 1..],
        None => line,
    }
}

/// 两个字符串的最长公共前缀。
fn common_prefix(a: &str, b: &str) -> String {
    let mut s = String::new();
    for (x, y) in a.chars().zip(b.chars()) {
        if x == y {
            s.push(x);
        } else {
            break;
        }
    }
    s
}

/// 基于目录列表对当前输入行做 Tab 补全。
/// 返回 (替换字节序列：退格整行 + 补全后的新行, 新行文本)；None = 不补全（交给远端 shell）。
fn build_tab_completion(entries: &[ssh::FileEntry], line: &str) -> Option<(Vec<u8>, String)> {
    let word = last_word(line);
    if word.is_empty() || word.starts_with('/') || word.contains('/') || word.starts_with("~/") {
        return None; // 空词 / 绝对路径 / 子目录路径交给远端 shell 补全
    }
    let matches: Vec<&ssh::FileEntry> = entries.iter().filter(|e| e.name.starts_with(word)).collect();
    if matches.is_empty() {
        return None;
    }
    let completion = if matches.len() == 1 {
        let m = matches[0];
        if m.is_dir {
            format!("{}/", m.name)
        } else {
            m.name.clone()
        }
    } else {
        // 多个匹配：补全最长公共前缀；无额外公共部分则不补全
        let mut pfx = matches[0].name.clone();
        for m in &matches[1..] {
            pfx = common_prefix(&pfx, &m.name);
            if pfx.is_empty() {
                break;
            }
        }
        if pfx.len() <= word.len() {
            return None;
        }
        pfx
    };
    let new_line = match line.rfind(' ') {
        Some(i) => format!("{}{}", &line[..=i], completion),
        None => completion,
    };
    // 退格删除整行（按字符数），再写入补全后的行
    let mut out = Vec::with_capacity(line.chars().count() + new_line.len());
    for _ in 0..line.chars().count() {
        out.push(0x08);
    }
    out.extend_from_slice(new_line.as_bytes());
    Some((out, new_line))
}

/// Tab 键本地补全入口（term_panel 内调用，拥有 app 上下文）。
fn handle_tab(app: &mut SshApp, idx: usize) {
    let line = app.sessions[idx]
        .term
        .as_ref()
        .map(|t| String::from_utf8_lossy(&t.pending_line).into_owned())
        .unwrap_or_default();
    let word = last_word(&line);
    // 行首 / 绝对路径 / 子目录路径：交给远端 shell 补全
    if line.is_empty() || word.is_empty() || word.starts_with('/') || word.contains('/') || word.starts_with("~/") {
        send_tab(app, idx);
        return;
    }
    let term_cwd = app.sessions[idx].term.as_ref().and_then(|t| t.term_cwd.clone());
    let cwd = app.sessions[idx].cwd.clone();
    let list_ok = term_cwd.is_some()
        && term_cwd.as_deref() == Some(cwd.as_str())
        && !app.sessions[idx].entries.is_empty();
    if list_ok {
        // 文件面板目录与终端目录一致：直接用现有列表同步补全
        let entries = app.sessions[idx].entries.clone();
        let line2 = line.clone();
        let id = app.sessions[idx].id.clone();
        if let Some((out, new_line)) = build_tab_completion(&entries, &line2) {
            if let Some(term) = &mut app.sessions[idx].term {
                term.pending_line = new_line.into_bytes();
            }
            app.send(FsMsg::For {
                id,
                msg: FsInner::ShellInput(out),
            });
        } else {
            send_tab(app, idx);
        }
    } else if let Some(dir) = term_cwd {
        // 目录列表不可用：请求远端目录列表，收到后异步补全
        app.sessions[idx].pending_tab_word = Some(word.to_string());
        app.sessions[idx].pending_tab_dir = Some(dir.clone());
        let id = app.sessions[idx].id.clone();
        app.send(FsMsg::For { id, msg: FsInner::List(dir) });
    } else {
        send_tab(app, idx);
    }
}

/// 把 Tab 键转发给远端 shell（远端 readline 自行补全/列候选）。
fn send_tab(app: &mut SshApp, idx: usize) {
    if app.sessions[idx].connected {
        let id = app.sessions[idx].id.clone();
        app.send(FsMsg::For {
            id,
            msg: FsInner::ShellInput(vec![0x09]),
        });
    }
}

/// 把终端颜色映射为 egui 颜色（终端区域固定黑底白字，默认前景为纯白）。
fn vt_color(c: vt100::Color) -> Color32 {
    match c {
        vt100::Color::Default => Color32::WHITE,
        vt100::Color::Idx(i) => ansi_256(i),
        vt100::Color::Rgb(r, g, b) => Color32::from_rgb(r, g, b),
    }
}

fn ansi_256(i: u8) -> Color32 {
    // xterm 256 色：16 基础色 + 6x6x6 立方体 + 24 级灰度
    const BASE: [[u8; 3]; 16] = [
        [0x00, 0x00, 0x00], // Black
        [0xcd, 0x31, 0x31], // Red
        [0x0d, 0xb2, 0x5a], // Green
        [0xcd, 0xcd, 0x00], // Yellow
        [0x34, 0x65, 0xa4], // Blue
        [0xcd, 0x53, 0xcd], // Magenta
        [0x00, 0xcd, 0xcd], // Cyan
        [0xe5, 0xe5, 0xe5], // White
        [0x66, 0x66, 0x66], // BrightBlack
        [0xff, 0x55, 0x55], // BrightRed
        [0x5f, 0xff, 0x5f], // BrightGreen
        [0xff, 0xff, 0x55], // BrightYellow
        [0x55, 0x55, 0xff], // BrightBlue
        [0xff, 0x55, 0xff], // BrightMagenta
        [0x55, 0xff, 0xff], // BrightCyan
        [0xff, 0xff, 0xff], // BrightWhite
    ];
    match i {
        0..=15 => {
            let c = BASE[i as usize];
            Color32::from_rgb(c[0], c[1], c[2])
        }
        16..=231 => {
            let i = i - 16;
            let r = (i / 36) % 6;
            let g = (i / 6) % 6;
            let b = i % 6;
            let v = |x: u8| if x == 0 { 0 } else { 55 + x * 40 };
            Color32::from_rgb(v(r), v(g), v(b))
        }
        _ => {
            let v = 8 + (i - 232) * 10;
            Color32::from_rgb(v, v, v)
        }
    }
}

/// 收集终端区域内的键盘 / 粘贴事件，翻译为终端字节流。
/// 输入法组合期间（IME preedit 非空）丢弃本帧文本事件，避免半成品拼音/候选
/// 被当作普通输入喂给远端；提交帧的文本事件正常上屏。
/// 返回 (字节流, 是否已复制并清除选择)。
fn term_input_bytes(
    ui: &egui::Ui,
    screen: &vt100::Screen,
    sel: Option<((u16, u16), (u16, u16))>,
) -> (Vec<u8>, bool, bool) {
    let events: Vec<Event> = ui.input(|i| i.events.clone());
    let ime_active = events.iter().any(|e| {
        matches!(
            e,
            Event::Ime(egui::ImeEvent::Preedit(s)) if !s.is_empty()
        )
    });
    let mut out = Vec::new();
    for ev in events {
        match ev {
            Event::Ime(_) => {}
            Event::Text(t) => {
                if !ime_active && !t.is_empty() {
                    out.extend_from_slice(t.as_bytes());
                }
            }
            Event::Key {
                key,
                pressed: true,
                modifiers,
                ..
            } => {
                // Ctrl+C 统一由 update 层全局捕获处理（任意焦点位置均作为中断信号），
                // 此处不再产生字节、不拦截复制；Ctrl+Shift+C 仍复制选中文本
                if key == Key::C && modifiers.ctrl && !modifiers.alt {
                    if modifiers.shift {
                        let copied = if let Some(t) = selected_text(screen, sel) {
                            ui.ctx().copy_text(t);
                            true
                        } else {
                            false
                        };
                        return (out, copied, false);
                    }
                    return (out, false, false);
                }
                if let Some(b) = term_key_bytes(key, modifiers, screen) {
                    out.extend_from_slice(&b);
                }
            }
            Event::Paste(text) => {
                if screen.bracketed_paste() {
                    out.extend_from_slice(b"\x1b[200~");
                }
                out.extend_from_slice(text.as_bytes());
                if screen.bracketed_paste() {
                    out.extend_from_slice(b"\x1b[201~");
                }
            }
            _ => {}
        }
    }
    (out, false, false)
}

/// 特殊按键 -> 终端字节序列（可打印字符由 Event::Text 处理，避免重复）。
fn term_key_bytes(key: Key, mods: Modifiers, screen: &vt100::Screen) -> Option<Vec<u8>> {
    use Key::*;

    // Ctrl + 字母 / 符号（常见控制键）
    if mods.ctrl && !mods.alt {
        let v = key as u8 - Key::A as u8 + 1;
        if (A..=Z).contains(&key) {
            return Some(vec![v]);
        }
        match key {
            Space => return Some(vec![0x00]),
            OpenBracket => return Some(vec![0x1b]), // Ctrl+[ = ESC
            Backslash => return Some(vec![0x1c]),
            CloseBracket => return Some(vec![0x1d]),
            Slash => return Some(vec![0x1f]),
            _ => {}
        }
    }

    // Alt + 字符 -> ESC + 字符
    if mods.alt && !mods.ctrl {
        if let Some(c) = key_char(key) {
            let c = if mods.shift {
                c.to_ascii_uppercase()
            } else {
                c
            };
            let mut v = vec![0x1b];
            v.push(c as u8);
            return Some(v);
        }
    }

    let app_cursor = screen.application_cursor();
    Some(match key {
        Enter => vec![b'\r'],
        Backspace => vec![0x7f],
        Escape => vec![0x1b],
        Tab => {
            if mods.shift {
                vec![0x1b, b'[', b'Z']
            } else {
                vec![b'\t']
            }
        }
        ArrowUp => arrow_bytes(app_cursor, b'A'),
        ArrowDown => arrow_bytes(app_cursor, b'B'),
        ArrowRight => arrow_bytes(app_cursor, b'C'),
        ArrowLeft => arrow_bytes(app_cursor, b'D'),
        Home => arrow_bytes(app_cursor, b'H'),
        End => arrow_bytes(app_cursor, b'F'),
        Delete => vec![0x1b, b'[', b'3', b'~'],
        PageUp => vec![0x1b, b'[', b'5', b'~'],
        PageDown => vec![0x1b, b'[', b'6', b'~'],
        Insert => vec![0x1b, b'[', b'2', b'~'],
        F1 => vec![0x1b, b'O', b'P'],
        F2 => vec![0x1b, b'O', b'Q'],
        F3 => vec![0x1b, b'O', b'R'],
        F4 => vec![0x1b, b'O', b'S'],
        F5 => vec![0x1b, b'[', b'1', b'5', b'~'],
        F6 => vec![0x1b, b'[', b'1', b'7', b'~'],
        F7 => vec![0x1b, b'[', b'1', b'8', b'~'],
        F8 => vec![0x1b, b'[', b'1', b'9', b'~'],
        F9 => vec![0x1b, b'[', b'2', b'0', b'~'],
        F10 => vec![0x1b, b'[', b'2', b'1', b'~'],
        F11 => vec![0x1b, b'[', b'2', b'3', b'~'],
        F12 => vec![0x1b, b'[', b'2', b'4', b'~'],
        _ => return None,
    })
}

fn arrow_bytes(app_cursor: bool, c: u8) -> Vec<u8> {
    if app_cursor {
        vec![0x1b, b'O', c]
    } else {
        vec![0x1b, b'[', c]
    }
}

fn key_char(key: Key) -> Option<char> {
    use Key::*;
    // egui Key 枚举判别值按声明顺序排列且连续（A..=Z、Num0..=Num9）
    let v = key as u8;
    if (Key::A as u8..=Key::Z as u8).contains(&v) {
        return Some((b'a' + (v - Key::A as u8)) as char);
    }
    if (Key::Num0 as u8..=Key::Num9 as u8).contains(&v) {
        return Some((b'0' + (v - Key::Num0 as u8)) as char);
    }
    Some(match key {
        Space => ' ',
        Minus => '-',
        Plus => '+',
        Slash => '/',
        Period => '.',
        Comma => ',',
        Semicolon => ';',
        Quote => '\'',
        Backslash => '\\',
        Backtick => '`',
        Equals => '=',
        OpenBracket => '[',
        CloseBracket => ']',
        _ => return None,
    })
}

// =====================================================================
// 文件面板（下半部分）
// =====================================================================

/// 紧凑面板边框：缩小四周内边距，减少留白。
fn compact_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(Color32::from_rgb(247, 248, 250))
        .inner_margin(egui::Margin::symmetric(6, 2))
}

fn files_panel(ui: &mut egui::Ui, app: &mut SshApp) {
    let idx = app.active_idx();
    let connected = app.sessions.get(idx).map(|t| t.connected).unwrap_or(false);
    // 顶部拖拽分隔条：只有手动拖动时才改变面板高度，其余时间高度固定
    let handle_h = 5.0;
    let (handle_rect, handle_resp) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), handle_h), egui::Sense::drag());
    ui.painter().rect_filled(handle_rect, 0.0, Color32::from_rgb(228, 231, 235));
    ui.painter().rect_filled(
        egui::Rect::from_min_max(
            egui::pos2(handle_rect.center().x - 30.0, handle_rect.top() + 1.5),
            egui::pos2(handle_rect.center().x + 30.0, handle_rect.bottom() - 1.5),
        ),
        1.5,
        Color32::from_rgb(148, 163, 184),
    );
    if handle_resp.dragged() {
        let delta = handle_resp.drag_delta().y;
        let cur = app.files_h.unwrap_or(360.0);
        app.files_h = Some((cur - delta).clamp(120.0, 900.0));
    }
    if handle_resp.hovered() || handle_resp.dragged() {
        ui.output_mut(|o| o.cursor_icon = egui::CursorIcon::ResizeVertical);
    }
    // 文件区顶部单行：远程文件 + 基础操作 | 分隔线 | 路径 …… 右侧：传输队列开关 / 上传 / 关闭
    ui.horizontal(|ui| {
        ui.strong(RichText::new("远程文件").color(Color32::from_rgb(31, 41, 55)));
        // 基础操作
        if ui
            .add_enabled(connected, egui::Button::new("↑ 上级"))
            .clicked()
        {
            if let Some(p) = parent_of(&app.sessions[idx].cwd) {
                app.sessions[idx].user_navigated = true; // 手动打开上级，停止自动联动
                app.navigate(p);
            }
        }
        if ui
            .add_enabled(connected, egui::Button::new("🔄 刷新"))
            .clicked()
        {
            app.reload();
        }
        if ui
            .add_enabled(connected, egui::Button::new("+ 新建目录"))
            .clicked()
        {
            app.dialog = Some(DialogKind::Mkdir);
            app.dialog_input.clear();
        }
        let has_sel = app.sessions.get(idx).map(|t| t.selected.is_some()).unwrap_or(false);
        if ui
            .add_enabled(connected && has_sel, egui::Button::new("↓ 下载"))
            .clicked()
        {
            if let Some(remote) = app.sessions[idx].selected.clone() {
                let id = app.sessions[idx].id.clone();
                let default_name = remote.rsplit('/').next().unwrap_or("file").to_string();
                let sel_is_dir = app
                    .sessions
                    .get(idx)
                    .map(|t| t.entries.iter().any(|e| e.path == remote && e.is_dir))
                    .unwrap_or(false);
                if let Some(path) = rfd::FileDialog::new().set_file_name(&default_name).save_file() {
                    app.start_transfer(false, default_name, sel_is_dir);
                    app.status_set("下载中...");
                    app.send(FsMsg::For {
                        id,
                        msg: FsInner::Download { remote, local: path },
                    });
                }
            }
        }
        if ui
            .add_enabled(connected && has_sel, egui::Button::new("✏ 重命名"))
            .clicked()
        {
            if let Some(sel) = app.sessions[idx].selected.clone() {
                app.dialog = Some(DialogKind::Rename);
                app.dialog_old = sel.clone();
                app.dialog_input = sel
                    .rsplit('/')
                    .next()
                    .unwrap_or("")
                    .to_string();
            }
        }
        if ui
            .add_enabled(connected && has_sel, egui::Button::new("🗑 删除"))
            .clicked()
        {
            if let Some(sel) = app.sessions[idx].selected.clone() {
                app.dialog_old = sel;
                app.dialog = Some(DialogKind::ConfirmDelete);
            }
        }
        ui.separator();
        // 路径（唯一路径展示处，可编辑，回车导航）
        if connected {
            let resp = ui.add(
                egui::TextEdit::singleline(&mut app.sessions[idx].cwd)
                    .desired_width((ui.available_width() - 300.0).max(120.0)),
            );
            if resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                app.sessions[idx].user_navigated = true; // 手动输入路径，停止自动联动
                let p = app.sessions[idx].cwd.clone();
                app.navigate(p);
            }
        } else {
            ui.weak("未连接");
        }
        // 右侧（从右到左：关闭最右、上传、传输队列开关）
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .add(egui::Button::new("× 关闭").small())
                .on_hover_text("关闭文件操作区域（关闭后底部保留标签条）")
                .clicked()
            {
                app.show_files = false;
            }
            // 上传主按钮（设计稿主视觉：蓝底白字「上传」）
            if ui
                .add_enabled(
                    connected,
                    egui::Button::new(RichText::new("上传").color(Color32::WHITE))
                        .fill(Color32::from_rgb(59, 130, 246)),
                )
                .clicked()
            {
                if let Some(path) = rfd::FileDialog::new().pick_file() {
                    let dir = app.sessions[idx].cwd.clone();
                    let id = app.sessions[idx].id.clone();
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "文件".to_string());
                    app.start_transfer(true, name, false);
                    app.status_set("上传中...");
                    app.send(FsMsg::For {
                        id,
                        msg: FsInner::Upload {
                            local: path,
                            remote_dir: dir,
                        },
                    });
                }
            }
            // 传输队列开关：带任务计数（对齐设计稿「传输队列 N 个任务」）
            let n = app.transfer_log.len();
            if ui
                .add(egui::Button::new(format!("📋 传输队列 {n} 个任务")).small())
                .on_hover_text("显示 / 隐藏右侧传输队列（高度跟随文件区域）")
                .clicked()
            {
                app.show_transfer = !app.show_transfer;
            }
        });
    });
    ui.separator();

    // 主体水平分栏：左侧文件内容 | 右侧传输队列（高度跟随文件区域）。
    // 先固定记录文件区剩余高度，表格 / 双栏面板都使用该确定高度（避免嵌套布局取到收缩值）
    let body_h = ui.available_height();
    ui.horizontal(|ui| {
        // 强制主体撑满文件区剩余高度，分隔线因此始终贯穿全高（不依赖内容高度）
        ui.set_min_height(body_h);
        // 左列：路径 + 文件内容
        ui.vertical(|ui| {
            ui.set_width(ui.available_width() - 252.0);
            if !connected {
                ui.centered_and_justified(|ui| {
                    ui.label(RichText::new("连接远程主机后，此处显示远程文件").color(Color32::GRAY).weak());
                });
            } else {
                let mut actions: Vec<FilesAction> = Vec::new();
    // 拖放上传：从资源管理器拖入窗口任意位置放开即上传到当前目录
    // （文件/文件夹均支持，文件夹递归上传）
    if connected {
        let dropped: Vec<egui::DroppedFile> = ui.input(|i| i.raw.dropped_files.clone());
        for d in dropped {
            if let Some(p) = d.path {
                if p.exists() {
                    actions.push(FilesAction::UploadLocal(p));
                } else {
                    app.status_set(format!("拖放路径不存在：{}", p.display()));
                }
            }
        }
        // 拖拽悬停中（指针在文件面板内）：高亮边框 + 提示（明确支持文件夹递归上传）
        let dragging_in = ui.input(|i| !i.raw.hovered_files.is_empty());
        if dragging_in && ui.rect_contains_pointer(ui.max_rect()) {
            let painter = ui.painter();
            painter.rect_stroke(
                ui.max_rect().shrink(2.0),
                4.0,
                egui::Stroke::new(2.0_f32, Color32::from_rgb(80, 160, 255)),
                egui::StrokeKind::Inside,
            );
            painter.text(
                ui.max_rect().center() - egui::vec2(0.0, 24.0),
                egui::Align2::CENTER_CENTER,
                "松开以上传（支持文件 / 文件夹递归上传）",
                FontId::proportional(14.0),
                Color32::from_rgb(80, 160, 255),
            );
        }
    }
    if app.show_dual {
        // ===== 双栏传输模式：本地面板 | 传输按钮 | 远程列表（对齐设计稿：本地在左、远程在右） =====
        let list_h = (body_h - 36.0).max(60.0);
        ui.horizontal(|ui| {
            // 左列：本地面板
            ui.vertical(|ui| {
                ui.set_width((ui.available_width() * 0.40).max(190.0));
                ui.strong(RichText::new("本地").color(Color32::from_rgb(31, 41, 55)));
                render_local_table(ui, app, list_h);
            });
            // 中列：传输按钮（本地选中 → 上传；远程选中 → 下载）
            ui.vertical_centered(|ui| {
                ui.set_width(74.0);
                ui.add_space(70.0);
                let local_sel = app.local_selected.clone();
                if ui
                    .add_enabled(
                        connected && local_sel.is_some(),
                        egui::Button::new("→ 上传").min_size(egui::vec2(70.0, 26.0)),
                    )
                    .clicked()
                {
                    if let Some(lp) = local_sel {
                        let dir = app.sessions[idx].cwd.clone();
                        let id = app.sessions[idx].id.clone();
                        let path = PathBuf::from(&lp);
                        let name = path
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| "文件".to_string());
                        let is_dir = path.is_dir();
                        app.start_transfer(true, name, is_dir);
                        app.status_set("上传中...");
                        app.send(FsMsg::For {
                            id,
                            msg: FsInner::Upload {
                                local: path,
                                remote_dir: dir,
                            },
                        });
                    }
                }
                ui.add_space(12.0);
                let rsel = app.sessions.get(idx).and_then(|t| t.selected.clone());
                if ui
                    .add_enabled(
                        connected && rsel.is_some(),
                        egui::Button::new("← 下载").min_size(egui::vec2(70.0, 26.0)),
                    )
                    .clicked()
                {
                    if let Some(remote) = rsel {
                        let id = app.sessions[idx].id.clone();
                        let default_name = remote.rsplit('/').next().unwrap_or("file").to_string();
                        let sel_is_dir = app
                            .sessions
                            .get(idx)
                            .map(|t| t.entries.iter().any(|e| e.path == remote && e.is_dir))
                            .unwrap_or(false);
                        let local = PathBuf::from(&app.local_dir).join(&default_name);
                        app.start_transfer(false, default_name, sel_is_dir);
                        app.status_set("下载中...");
                        app.send(FsMsg::For {
                            id,
                            msg: FsInner::Download { remote, local },
                        });
                    }
                }
            });
            // 右列：远程文件（标题对齐设计稿：会话名 : 远程路径）
            ui.vertical(|ui| {
                ui.set_width(ui.available_width());
                let sess_txt = app
                    .sessions
                    .get(idx)
                    .map(|t| format!("{} : {}", t.account.name, t.cwd))
                    .unwrap_or_else(|| "远程".to_string());
                ui.strong(RichText::new(sess_txt).color(Color32::from_rgb(31, 41, 55)));
                render_remote_table(ui, app, idx, &mut actions, list_h);
            });
        });
        ui.add_space(2.0);
        ui.weak("选中文件后点击中间箭头，或直接拖拽到对侧面板传输");
    } else {
        // 单栏模式：远程文件表格
        let hint_h = ui.text_style_height(&egui::TextStyle::Small) + 4.0;
        render_remote_table(
            ui,
            app,
            idx,
            &mut actions,
            (body_h - hint_h).max(40.0),
        );
        ui.add_space(2.0);
        ui.weak("双击用默认编辑器打开 · 支持拖拽文件到此处直接上传");
    }
    for a in actions {
        match a {
            FilesAction::Navigate(p) => {
                app.sessions[idx].user_navigated = true; // 双击进入目录，停止自动联动
                app.navigate(p);
            }
            FilesAction::Select(p) => app.sessions[idx].selected = Some(p),
            // 拖放上传：文件/文件夹均自动识别，上传到当前目录
            FilesAction::UploadLocal(p) => {
                if app.sessions[idx].connected {
                    let dir = app.sessions[idx].cwd.clone();
                    let id = app.sessions[idx].id.clone();
                    let name = p
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "文件".to_string());
                    app.start_transfer(true, name, p.is_dir());
                    app.status_set("上传中...");
                    app.send(FsMsg::For {
                        id,
                        msg: FsInner::Upload {
                            local: p,
                            remote_dir: dir,
                        },
                    });
                }
            }
            // 右键下载：文件走保存对话框，文件夹走目录选择（递归下载）
            FilesAction::DownloadSel(p) => {
                if !app.sessions[idx].connected {
                    continue;
                }
                let is_dir = app
                    .sessions
                    .get(idx)
                    .map(|t| t.entries.iter().any(|e| e.path == p && e.is_dir))
                    .unwrap_or(false);
                let default_name = p.rsplit('/').next().unwrap_or("file").to_string();
                if is_dir {
                    if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                        let id = app.sessions[idx].id.clone();
                        app.start_transfer(false, default_name, true);
                        app.status_set("下载中...");
                        app.send(FsMsg::For {
                            id,
                            msg: FsInner::Download { remote: p, local: dir },
                        });
                    }
                } else if let Some(path) = rfd::FileDialog::new()
                    .set_file_name(&default_name)
                    .save_file()
                {
                    let id = app.sessions[idx].id.clone();
                    app.start_transfer(false, default_name, false);
                    app.status_set("下载中...");
                    app.send(FsMsg::For {
                        id,
                        msg: FsInner::Download { remote: p, local: path },
                    });
                }
            }
            // 双击远程文件：下载到临时目录，完成后用系统默认应用打开
            FilesAction::OpenRemote(p) => {
                if !app.sessions[idx].connected {
                    continue;
                }
                let default_name = p.rsplit('/').next().unwrap_or("file").to_string();
                let tmp_dir = std::env::temp_dir().join("3f-shell-open");
                let _ = std::fs::create_dir_all(&tmp_dir);
                let local = tmp_dir.join(format!("{}_{}", std::process::id(), default_name));
                let id = app.sessions[idx].id.clone();
                app.pending_open = Some(local.clone());
                app.start_transfer(false, default_name, false);
                app.status_set("下载中，完成后自动打开...");
                app.send(FsMsg::For {
                    id,
                    msg: FsInner::Download {
                        remote: p,
                        local,
                    },
                });
            }
        }
    }
        }
    });
        // 右列：传输队列（高度跟随文件区域）
        if app.show_transfer {
            ui.add_space(8.0);
            ui.vertical(|ui| {
                ui.set_width(240.0);
                transfer_panel_content(ui, app);
            });
        }
    });

    // 分隔线：文件操作区与传输队列之间，固定贯穿文件区全高绘制（不依赖内容高度）
    if app.show_transfer {
        let rect = ui.max_rect();
        let sep_x = rect.right() - 248.0; // 队列列（240 宽）左侧固定位置
        let sep_y0 = rect.top() + 62.0; // 工具栏 / 路径行下方
        ui.painter().vline(
            sep_x,
            egui::Rangef::new(sep_y0, rect.bottom()),
            egui::Stroke::new(1.0_f32, Color32::from_rgb(148, 163, 184)),
        );
    }
}

/// 远程文件表格（单栏 / 双栏共用）：名称/大小/修改时间/权限 + 单击/双击/右键。
fn render_remote_table(
    ui: &mut egui::Ui,
    app: &mut SshApp,
    idx: usize,
    actions: &mut Vec<FilesAction>,
    avail_h: f32,
) {
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .max_height(avail_h.max(40.0))
        .show(ui, |ui| {
            TableBuilder::new(ui)
                .vscroll(false) // 关闭表格内部滚动，统一由外层 ScrollArea 提供垂直滚动条
                .striped(true)
                .sense(egui::Sense::click()) // 整行可交互：任意列右键都弹出菜单
                .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                .column(Column::auto().at_least(220.0))
                .column(Column::initial(110.0).at_least(70.0))
                .column(Column::initial(150.0))
                .column(Column::remainder())
                .header(22.0, |mut h| {
                    h.col(|ui| {
                        ui.strong(RichText::new("名称").color(Color32::from_rgb(55, 65, 81)));
                    });
                    h.col(|ui| {
                        ui.strong(RichText::new("大小").color(Color32::from_rgb(55, 65, 81)));
                    });
                    h.col(|ui| {
                        ui.strong(RichText::new("修改时间").color(Color32::from_rgb(55, 65, 81)));
                    });
                    h.col(|ui| {
                        ui.strong(RichText::new("权限").color(Color32::from_rgb(55, 65, 81)));
                    });
                })
                .body(|mut body| {
                    for entry in &app.sessions[idx].entries {
                        let path = entry.path.clone();
                        let is_dir = entry.is_dir;
                        body.row(22.0, |mut row| {
                            let selected =
                                app.sessions[idx].selected.as_deref() == Some(path.as_str());
                            let (r1, _) = row.col(|ui| {
                                // hover 联动：指针位于名称列区域时名称文字变蓝（设计稿浅蓝 hover 风格）
                                let cell_rect = ui.max_rect();
                                let hovered_cell = ui.rect_contains_pointer(cell_rect) && !selected;
                                let color = if is_dir {
                                    Color32::from_rgb(29, 78, 216)
                                } else if selected {
                                    Color32::from_rgb(30, 64, 175)
                                } else if hovered_cell {
                                    Color32::from_rgb(37, 99, 235)
                                } else {
                                    ui.visuals().text_color()
                                };
                                // 矢量图标（替代 emoji，黑色主题下更清晰美观）
                                let (icon_rect, _) =
                                    ui.allocate_exact_size(egui::vec2(15.0, 13.0), egui::Sense::hover());
                                paint_file_icon(
                                    ui.painter(),
                                    icon_rect,
                                    is_dir,
                                    entry.is_symlink,
                                    selected,
                                    true,
                                );
                                let hr = ui.horizontal(|ui| {
                                    ui.label(RichText::new(&entry.name).color(color));
                                });
                                // 整行（图标+文本）可点击/双击（右键菜单在行级统一处理）
                                let resp = ui.interact(
                                    hr.response.rect,
                                    ui.id().with(&path),
                                    egui::Sense::click(),
                                );
                                if resp.double_clicked() {
                                    if is_dir {
                                        actions.push(FilesAction::Navigate(path.clone()));
                                    } else {
                                        // 双击文件：下载到临时目录后用默认编辑器打开
                                        actions.push(FilesAction::OpenRemote(path.clone()));
                                    }
                                } else if resp.clicked() {
                                    if is_dir {
                                        actions.push(FilesAction::Navigate(path.clone()));
                                    } else {
                                        actions.push(FilesAction::Select(path.clone()));
                                    }
                                }
                            });
                            let (r2, _) = row.col(|ui| {
                                // hover 联动：大小列文字变蓝
                                let cell_rect = ui.max_rect();
                                let hovered_cell = ui.rect_contains_pointer(cell_rect) && !selected;
                                let hover_color = Color32::from_rgb(37, 99, 235);
                                let normal_color = Color32::from_rgb(107, 114, 128);
                                if is_dir {
                                    ui.label(
                                        RichText::new("—").color(if hovered_cell {
                                            hover_color
                                        } else {
                                            normal_color
                                        }),
                                    );
                                } else {
                                    ui.label(
                                        RichText::new(
                                            entry
                                                .size
                                                .map(fmt_size)
                                                .unwrap_or_else(|| "—".into()),
                                        )
                                        .color(if hovered_cell {
                                            hover_color
                                        } else {
                                            normal_color
                                        }),
                                    );
                                }
                            });
                            let (r3, _) = row.col(|ui| {
                                // hover 联动：修改时间列文字变蓝
                                let cell_rect = ui.max_rect();
                                let hovered_cell = ui.rect_contains_pointer(cell_rect) && !selected;
                                let txt = entry.mtime.clone().unwrap_or_default();
                                ui.label(
                                    RichText::new(txt).color(if hovered_cell {
                                        Color32::from_rgb(37, 99, 235)
                                    } else {
                                        Color32::from_rgb(107, 114, 128)
                                    }),
                                );
                            });
                            row.col(|ui| {
                                // hover 联动：权限列文字变蓝
                                let cell_rect = ui.max_rect();
                                let hovered_cell = ui.rect_contains_pointer(cell_rect) && !selected;
                                let perms = entry.perms.clone().unwrap_or_default();
                                ui.label(
                                    RichText::new(perms).color(if hovered_cell {
                                        Color32::from_rgb(37, 99, 235)
                                    } else {
                                        Color32::from_rgb(107, 114, 128)
                                    }),
                                );
                                // 整行 hover / 选中高亮：前三列 rect union + 当前权限列区域 = 整行
                                let row_full = r1.union(r2).union(r3).union(cell_rect);
                                let hovered = ui.rect_contains_pointer(row_full);
                                if hovered || selected {
                                    let bg = if selected {
                                        Color32::from_rgba_unmultiplied(191, 219, 254, 220)
                                    } else {
                                        Color32::from_rgba_unmultiplied(219, 234, 254, 185)
                                    };
                                    ui.painter().rect_filled(row_full, 4.0, bg);
                                }
                                if hovered {
                                    ui.output_mut(|o| o.cursor_icon = egui::CursorIcon::PointingHand);
                                }
                            });
                            // 整行右键菜单：删除 / 下载（文件与文件夹均支持，文件夹递归处理）
                            let menu_path = path.clone();
                            let menu_is_dir = is_dir;
                            row.response().context_menu(|ui| {
                                if ui.button("↓ 下载").clicked() {
                                    actions.push(FilesAction::DownloadSel(menu_path.clone()));
                                    ui.close_menu();
                                }
                                if ui
                                    .button(RichText::new("🗑 删除").color(Color32::LIGHT_RED))
                                    .clicked()
                                {
                                    app.dialog_old = menu_path.clone();
                                    app.dialog = Some(DialogKind::ConfirmDelete);
                                    ui.close_menu();
                                }
                                if menu_is_dir && ui.button("进入目录").clicked() {
                                    actions.push(FilesAction::Navigate(menu_path));
                                    ui.close_menu();
                                }
                            });
                        });
                    }
                });
        });
}

/// 本地面板：路径行（上级/刷新/输入）+ 文件列表（单击选中、双击进入目录）。
fn render_local_table(ui: &mut egui::Ui, app: &mut SshApp, avail_h: f32) {
    ui.horizontal(|ui| {
        if ui
            .add(egui::Button::new("↑").small())
            .on_hover_text("上级目录")
            .clicked()
        {
            let p = PathBuf::from(&app.local_dir);
            if let Some(parent) = p.parent() {
                app.local_dir = parent.to_string_lossy().into_owned();
                app.local_entries = load_local_entries(&app.local_dir);
                app.local_selected = None;
            }
        }
        if ui
            .add(egui::Button::new("🔄").small())
            .on_hover_text("刷新")
            .clicked()
        {
            app.local_entries = load_local_entries(&app.local_dir);
        }
        let resp =
            ui.add(egui::TextEdit::singleline(&mut app.local_dir).desired_width(ui.available_width()));
        if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            app.local_entries = load_local_entries(&app.local_dir);
            app.local_selected = None;
        }
    });
    let entries = app.local_entries.clone();
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .max_height(avail_h.max(40.0))
        .show(ui, |ui| {
            TableBuilder::new(ui)
                .vscroll(false)
                .striped(true)
                .sense(egui::Sense::click())
                .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                .column(Column::auto().at_least(140.0))
                .column(Column::initial(90.0))
                .column(Column::remainder())
                .header(22.0, |mut h| {
                    h.col(|ui| {
                        ui.strong(RichText::new("名称").color(Color32::from_rgb(55, 65, 81)));
                    });
                    h.col(|ui| {
                        ui.strong(RichText::new("大小").color(Color32::from_rgb(55, 65, 81)));
                    });
                    h.col(|ui| {
                        ui.strong(RichText::new("修改时间").color(Color32::from_rgb(55, 65, 81)));
                    });
                })
                .body(|mut body| {
                    for e in &entries {
                        let path = e.path.clone();
                        let is_dir = e.is_dir;
                        let selected = app.local_selected.as_deref() == Some(path.as_str());
                        body.row(22.0, |mut row| {
                            let (r1, _) = row.col(|ui| {
                                // hover 联动：名称列区域 hover 时文字变蓝（设计稿浅蓝 hover 风格）
                                let cell_rect = ui.max_rect();
                                let hovered_cell = ui.rect_contains_pointer(cell_rect) && !selected;
                                let color = if is_dir {
                                    Color32::from_rgb(29, 78, 216)
                                } else if selected {
                                    Color32::from_rgb(30, 64, 175)
                                } else if hovered_cell {
                                    Color32::from_rgb(37, 99, 235)
                                } else {
                                    ui.visuals().text_color()
                                };
                                let (icon_rect, _) =
                                    ui.allocate_exact_size(egui::vec2(15.0, 13.0), egui::Sense::hover());
                                paint_file_icon(ui.painter(), icon_rect, is_dir, false, selected, true);
                                let hr = ui.horizontal(|ui| {
                                    ui.label(RichText::new(&e.name).color(color));
                                });
                                let resp = ui.interact(
                                    hr.response.rect,
                                    ui.id().with(&path),
                                    egui::Sense::click(),
                                );
                                if resp.double_clicked() && is_dir {
                                    app.local_dir = path.clone();
                                    app.local_entries = load_local_entries(&app.local_dir);
                                    app.local_selected = None;
                                } else if resp.clicked() {
                                    app.local_selected = Some(path.clone());
                                }
                            });
                            let (r2, _) = row.col(|ui| {
                                // hover 联动：大小列文字变蓝
                                let cell_rect = ui.max_rect();
                                let hovered_cell = ui.rect_contains_pointer(cell_rect) && !selected;
                                let hover_color = Color32::from_rgb(37, 99, 235);
                                let normal_color = Color32::from_rgb(107, 114, 128);
                                if is_dir {
                                    ui.label(RichText::new("—").color(if hovered_cell {
                                        hover_color
                                    } else {
                                        normal_color
                                    }));
                                } else {
                                    ui.label(
                                        RichText::new(fmt_size(e.size)).color(if hovered_cell {
                                            hover_color
                                        } else {
                                            normal_color
                                        }),
                                    );
                                }
                            });
                            row.col(|ui| {
                                // hover 联动：修改时间列文字变蓝
                                let cell_rect = ui.max_rect();
                                let hovered_cell = ui.rect_contains_pointer(cell_rect) && !selected;
                                ui.label(
                                    RichText::new(&e.mtime).color(if hovered_cell {
                                        Color32::from_rgb(37, 99, 235)
                                    } else {
                                        Color32::from_rgb(107, 114, 128)
                                    }),
                                );
                                let row_full = r1.union(r2).union(cell_rect);
                                let hovered = ui.rect_contains_pointer(row_full);
                                if hovered || selected {
                                    let bg = if selected {
                                        Color32::from_rgba_unmultiplied(191, 219, 254, 220)
                                    } else {
                                        Color32::from_rgba_unmultiplied(219, 234, 254, 185)
                                    };
                                    ui.painter().rect_filled(row_full, 4.0, bg);
                                }
                            });
                        });
                    }
                });
        });
}

enum FilesAction {
    Navigate(String),
    Select(String),
    UploadLocal(PathBuf),
    DownloadSel(String),
    /// 双击远程文件：下载到临时目录后用系统默认应用打开
    OpenRemote(String),
}

// =====================================================================
// 后台线程：消息驱动的 tokio 运行时 + SSH/SFTP
// =====================================================================

/// 发往远程 shell 通道的命令（经 mpsc 队列由读循环任务独占执行）
enum ShellCmd {
    Data(Vec<u8>),
    SigInt,
}

struct SessionCtx {
    id: String,
    writer: tokio::sync::mpsc::UnboundedSender<ShellCmd>,
    _read_task: tokio::task::JoinHandle<()>,
}

fn spawn_backend(rx: Receiver<FsMsg>, tx: Sender<BackMsg>) {
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Runtime::new() {
            Ok(r) => r,
            Err(e) => {
                let _ = tx.send(BackMsg::Log(format!("启动异步运行时失败：{e}")));
                return;
            }
        };
        let ssh = std::sync::Arc::new(ssh::SshManager::new());
        use std::collections::HashMap;
        let mut sessions: HashMap<String, SessionCtx> = HashMap::new();

        while let Ok(msg) = rx.recv() {
            match msg {
                FsMsg::Connect(req) => {
                    // 允许多个连接同时打开：每个请求建立独立会话
                    let ssh2 = ssh.clone();
                    let tx2 = tx.clone();
                    let mut opened: Option<(
                        String,
                        tokio::sync::mpsc::UnboundedSender<ShellCmd>,
                        tokio::task::JoinHandle<()>,
                    )> = None;
                    rt.block_on(async {
                        match ssh2
                            .connect(req.host, req.port, req.user, req.password, req.key_path)
                            .await
                        {
                            Err(e) => {
                                let _ = tx2.send(BackMsg::Connected(Err((req.token, e))));
                            }
                            Ok(id) => {
                                let _ = tx2.send(BackMsg::Connected(Ok((req.token, id.clone()))));
                                // 打开交互式 shell
                                match ssh2.open_shell(&id, 80, 24).await {
                                    Err(e) => {
                                        let _ = tx2.send(BackMsg::Log(format!("启动交互终端失败：{e}")));
                                    }
                                    Ok(channel) => {
                                        let tx3 = tx2.clone();
                                        let id3 = id.clone();
                                        // 写请求队列；读循环任务独占 channel（锁），同时处理读写与信号
                                        let (wtx, mut wrx) =
                                            tokio::sync::mpsc::unbounded_channel::<ShellCmd>();
                                        let ch = std::sync::Arc::new(tokio::sync::Mutex::new(channel));
                                        let ch_read = ch.clone();
                                        let read_task = rt.spawn(async move {
                                            use tokio::io::AsyncReadExt;
                                            use tokio::io::AsyncWriteExt;
                                            let mut buf = vec![0u8; 16384];
                                            loop {
                                                tokio::select! {
                                                    r = async {
                                                        let mut g = ch_read.lock().await;
                                                        let mut reader = g.make_reader();
                                                        reader.read(&mut buf).await
                                                    } => {
                                                        match r {
                                                            Ok(0) | Err(_) => break,
                                                            Ok(n) => {
                                                                if tx3
                                                                    .send(BackMsg::For {
                                                                        id: id3.clone(),
                                                                        msg: BackInner::ShellData(
                                                                            buf[..n].to_vec(),
                                                                        ),
                                                                    })
                                                                    .is_err()
                                                                {
                                                                    break;
                                                                }
                                                            }
                                                        }
                                                    }
                                                    cmd = wrx.recv() => {
                                                        match cmd {
                                                            Some(ShellCmd::Data(b)) => {
                                                                let g = ch.lock().await;
                                                                let mut w = g.make_writer();
                                                                if w.write_all(&b).await.is_err() {
                                                                    break;
                                                                }
                                                            }
                                                            Some(ShellCmd::SigInt) => {
                                                                let g = ch.lock().await;
                                                                let mut w = g.make_writer();
                                                                // 0x03（PTY 中断字符，ISIG 下即 SIGINT 给前台进程组）；
                                                                // 连发三次抗丢（数据通道此刻可能仍有积压输出）
                                                                let _ = w.write_all(&[0x03]).await;
                                                                let _ = w.write_all(&[0x03]).await;
                                                                let _ = w.write_all(&[0x03]).await;
                                                                drop(w);
                                                                // SSH signal INT 走控制平面（辅助通道，连发两次防 sshd 忽略）
                                                                let _ = g.signal(russh::Sig::INT).await;
                                                                let _ = g.signal(russh::Sig::INT).await;
                                                            }
                                                            None => break,
                                                        }
                                                    }
                                                }
                                            }
                                            let _ = tx3.send(BackMsg::For {
                                                id: id3,
                                                msg: BackInner::ShellClosed,
                                            });
                                        });
                                        opened = Some((id, wtx, read_task));
                                    }
                                }
                            }
                        }
                    });
                    if let Some((id, writer, read_task)) = opened {
                        sessions.insert(
                            id.clone(),
                            SessionCtx {
                                id,
                                writer,
                                _read_task: read_task,
                            },
                        );
                    }
                }
                FsMsg::For { id, msg } => match msg {
                    FsInner::CancelTransfer { up } => {
                        // 置对应方向取消标志；正在传输的循环检测后中止
                        ssh::cancel_transfer(&ssh, up);
                    }
                    FsInner::Disconnect => {
                        if let Some(s) = sessions.remove(&id) {
                            let ssh2 = ssh.clone();
                            rt.block_on(async move {
                                let _ = ssh2.disconnect(&s.id).await;
                            });
                        }
                        let _ = tx.send(BackMsg::For {
                            id,
                            msg: BackInner::Disconnected,
                        });
                    }
                    FsInner::List(path) => {
                        if let Some(s) = sessions.get(&id) {
                            let ssh2 = ssh.clone();
                            let id2 = s.id.clone();
                            let tx2 = tx.clone();
                            rt.block_on(async move {
                                let r = ssh2.list(&id2, &path).await;
                                let _ = tx2.send(BackMsg::For {
                                    id: id2,
                                    msg: BackInner::List(r),
                                });
                            });
                        }
                    }
                    FsInner::Mkdir(p) => {
                        if let Some(s) = sessions.get(&id) {
                            let ssh2 = ssh.clone();
                            let id2 = s.id.clone();
                            let tx2 = tx.clone();
                            rt.block_on(async move {
                                let r = ssh2.mkdir(&id2, &p).await;
                                let _ = tx2.send(BackMsg::For {
                                    id: id2,
                                    msg: BackInner::Mkdir(r),
                                });
                            });
                        }
                    }
                    FsInner::Remove(p) => {
                        if let Some(s) = sessions.get(&id) {
                            let ssh2 = ssh.clone();
                            let id2 = s.id.clone();
                            let tx2 = tx.clone();
                            rt.block_on(async move {
                                // 递归删除：文件直接删，目录先清空子项再删自身
                                let r = ssh2.remove_entry(&id2, &p).await;
                                let _ = tx2.send(BackMsg::For {
                                    id: id2,
                                    msg: BackInner::Remove(r),
                                });
                            });
                        }
                    }
                    FsInner::Rename(old, newp) => {
                        if let Some(s) = sessions.get(&id) {
                            let ssh2 = ssh.clone();
                            let id2 = s.id.clone();
                            let tx2 = tx.clone();
                            rt.block_on(async move {
                                let r = ssh2.rename(&id2, &old, &newp).await;
                                let _ = tx2.send(BackMsg::For {
                                    id: id2,
                                    msg: BackInner::Rename(r),
                                });
                            });
                        }
                    }
                    FsInner::Upload { local, remote_dir } => {
                        if let Some(s) = sessions.get(&id) {
                            let ssh2 = ssh.clone();
                            let id2 = s.id.clone();
                            let tx2 = tx.clone();
                            let local2 = local.clone();
                            let dir = remote_dir.clone();
                            // 异步任务执行传输：不阻塞消息循环，取消消息可被及时处理
                            rt.spawn(async move {
                                // 自动识别本地是文件还是目录：文件直接上传，目录递归上传
                                // 进度回调：实时上报 (已传字节, 文件总字节) 供明细面板进度条
                                let _ = tx2.send(BackMsg::TransferStart { up: true });
                                let txp = tx2.clone();
                                let cb = move |done: u64, total: u64| {
                                    let _ = txp.send(BackMsg::Progress {
                                        up: true,
                                        done,
                                        total,
                                    });
                                };
                                let r = ssh2.upload_entry(&id2, &local2, &dir, &cb).await;
                                ssh::reset_cancel(&ssh2, true);
                                let _ = tx2.send(BackMsg::For {
                                    id: id2,
                                    msg: BackInner::Upload(r),
                                });
                            });
                        }
                    }
                    FsInner::Download { remote, local } => {
                        if let Some(s) = sessions.get(&id) {
                            let ssh2 = ssh.clone();
                            let id2 = s.id.clone();
                            let tx2 = tx.clone();
                            let local2 = local.clone();
                            let remote2 = remote.clone();
                            // 异步任务执行传输：不阻塞消息循环，取消消息可被及时处理
                            rt.spawn(async move {
                                // 自动识别远端是文件还是目录：文件直接下载，目录递归下载
                                // 进度回调：实时上报 (已传字节, 文件总字节) 供明细面板进度条
                                let _ = tx2.send(BackMsg::TransferStart { up: false });
                                let txp = tx2.clone();
                                let cb = move |done: u64, total: u64| {
                                    let _ = txp.send(BackMsg::Progress {
                                        up: false,
                                        done,
                                        total,
                                    });
                                };
                                let r = ssh2.download_entry(&id2, &remote2, &local2, &cb).await;
                                ssh::reset_cancel(&ssh2, false);
                                let _ = tx2.send(BackMsg::For {
                                    id: id2,
                                    msg: BackInner::Download(r),
                                });
                            });
                        }
                    }
                    FsInner::SysInfo => {
                        if let Some(s) = sessions.get(&id) {
                            let ssh2 = ssh.clone();
                            let id2 = s.id.clone();
                            let tx2 = tx.clone();
                            rt.block_on(async move {
                                let r = ssh2.sysinfo(&id2).await;
                                let _ = tx2.send(BackMsg::For {
                                    id: id2,
                                    msg: BackInner::SysInfo(r),
                                });
                            });
                        }
                    }
                    FsInner::Ping => {
                        if let Some(s) = sessions.get(&id) {
                            let ssh2 = ssh.clone();
                            let id2 = s.id.clone();
                            let tx2 = tx.clone();
                            rt.block_on(async move {
                                let r = ssh2.ping(&id2).await;
                                let _ = tx2.send(BackMsg::For {
                                    id: id2,
                                    msg: BackInner::Ping(r),
                                });
                            });
                        }
                    }
                    FsInner::ShellInput(bytes) => {
                        if let Some(s) = sessions.get(&id) {
                            let _ = s.writer.send(ShellCmd::Data(bytes));
                        }
                    }
                    FsInner::SigInt => {
                        if let Some(s) = sessions.get(&id) {
                            let _ = s.writer.send(ShellCmd::SigInt);
                        }
                    }
                },
            }
        }
    });
}

// =====================================================================
// 工具函数
// =====================================================================

fn parent_of(path: &str) -> Option<String> {
    let p = path.trim_end_matches('/');
    if p.is_empty() {
        return None;
    }
    let idx = p.rfind('/')?;
    if idx == 0 {
        Some("/".into())
    } else {
        Some(p[..idx].to_string())
    }
}

fn join_path(dir: &str, name: &str) -> String {
    if dir == "/" {
        format!("/{}", name.trim_start_matches('/'))
    } else {
        format!("{}/{}", dir.trim_end_matches('/'), name.trim_start_matches('/'))
    }
}

fn fmt_size(n: u64) -> String {
    // 大小从字节起算：单位 B / KB / MB / GB / TB（1024 进制）
    const K: f64 = 1024.0;
    if n < K as u64 {
        return format!("{} B", n);
    }
    let units = ["KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut u = 0; // 除以 1024 的次数；u=1 -> KB, u=2 -> MB, u=3 -> GB, u=4 -> TB
    while v >= K && u < units.len() {
        v /= K;
        u += 1;
    }
    format!("{:.1} {}", v, units[u - 1])
}

// =====================================================================
// 本地账号存储（exe 同目录 accounts.tsv，tab 分隔）
// =====================================================================

/// 本地面板默认目录（用户主目录）。
fn home_dir_str() -> String {
    #[cfg(target_os = "windows")]
    {
        std::env::var("USERPROFILE").unwrap_or_else(|_| "C:\\".to_string())
    }
    #[cfg(not(target_os = "windows"))]
    {
        std::env::var("HOME").unwrap_or_else(|_| "/".to_string())
    }
}

/// 读取本地目录列表（目录在前，名称升序）。
fn load_local_entries(dir: &str) -> Vec<LocalEntry> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let meta = e.metadata().ok();
            let is_dir = meta.as_ref().map(|m| m.is_dir()).unwrap_or(false);
            let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
            let mtime = meta
                .and_then(|m| m.modified().ok())
                .map(|t| {
                    let dt: chrono::DateTime<chrono::Local> = t.into();
                    dt.format("%m-%d %H:%M").to_string()
                })
                .unwrap_or_default();
            out.push(LocalEntry {
                name,
                path: e.path().to_string_lossy().into_owned(),
                is_dir,
                size,
                mtime,
            });
        }
    }
    out.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then(a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    out
}

/// 一条已保存的远程账号。
#[derive(Clone)]
struct SavedAccount {
    name: String,
    host: String,
    port: u16,
    user: String,
    password: Option<String>,
    key: Option<String>,
    /// 分组名（如「生产环境」「开发测试」）；空串 = 未分组。
    group: String,
}

/// 最近会话（最近连接的服务器 + 最后一条命令，用于快速重连）。
#[derive(Clone)]
struct RecentSession {
    name: String,
    host: String,
    port: u16,
    user: String,
    cmd: String,
    when: String,
}

/// 账号文件路径：exe 同目录 accounts.tsv。
fn accounts_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("accounts.tsv")
}

/// 分组文件路径：exe 同目录 groups.tsv（每行一个分组名，不含「未分组」）。
fn groups_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("groups.tsv")
}

/// 加载分组列表（跳过空行）。
fn load_groups() -> Vec<String> {
    match std::fs::read_to_string(groups_path()) {
        Ok(text) => text
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// 保存分组列表（每行一个，UTF-8）。
fn save_groups(groups: &[String]) {
    let mut s = String::new();
    for g in groups {
        s.push_str(g);
        s.push('\n');
    }
    let _ = std::fs::write(groups_path(), s);
}

/// 分组列表 = 持久化 groups.tsv ∪ 账号中出现的全部分组（保持出现顺序，兼容旧数据）。
fn merged_groups(accounts: &[SavedAccount]) -> Vec<String> {
    let mut out = load_groups();
    for a in accounts {
        if !a.group.is_empty() && !out.contains(&a.group) {
            out.push(a.group.clone());
        }
    }
    out
}

/// 序列化为 TSV 文本（字段：name\thost\tport\tuser\tpassword\tkey\tgroup）。
fn serialize_accounts(accounts: &[SavedAccount]) -> String {
    let mut s = String::new();
    for a in accounts {
        s.push_str(&a.name);
        s.push('\t');
        s.push_str(&a.host);
        s.push('\t');
        s.push_str(&a.port.to_string());
        s.push('\t');
        s.push_str(&a.user);
        s.push('\t');
        s.push_str(a.password.as_deref().unwrap_or(""));
        s.push('\t');
        s.push_str(a.key.as_deref().unwrap_or(""));
        s.push('\t');
        s.push_str(&a.group);
        s.push('\n');
    }
    s
}

/// 解析 TSV 文本为账号列表（跳过损坏行）。
fn parse_accounts(text: &str) -> Vec<SavedAccount> {
    let mut out = Vec::new();
    for line in text.lines() {
        let mut f = line.split('\t');
        let name = match f.next() {
            Some(n) => n.to_string(),
            None => continue,
        };
        let host = match f.next() {
            Some(h) => h.to_string(),
            None => continue,
        };
        let port: u16 = match f.next() {
            Some(p) => match p.parse() {
                Ok(v) => v,
                Err(_) => continue,
            },
            None => continue,
        };
        let user = match f.next() {
            Some(u) => u.to_string(),
            None => continue,
        };
        if host.is_empty() || user.is_empty() {
            continue;
        }
        let password = f.next().map(|s| s.to_string()).filter(|s| !s.is_empty());
        let key = f.next().map(|s| s.to_string()).filter(|s| !s.is_empty());
        let group = f.next().map(|s| s.to_string()).unwrap_or_default();
        out.push(SavedAccount {
            name: if name.is_empty() {
                format!("{user}@{host}")
            } else {
                name
            },
            host,
            port,
            user,
            password,
            key,
            group,
        });
    }
    out
}

fn load_accounts() -> Vec<SavedAccount> {
    match std::fs::read_to_string(accounts_path()) {
        Ok(text) => parse_accounts(&text),
        Err(_) => Vec::new(),
    }
}

fn save_accounts(accounts: &[SavedAccount]) {
    let _ = std::fs::write(accounts_path(), serialize_accounts(accounts));
}

/// 最近会话文件路径：exe 同目录 recent.tsv。
fn recent_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("recent.tsv")
}

/// 最近会话序列化（字段：name\thost\tport\tuser\tcmd\twhen）。
fn serialize_recent(recent: &[RecentSession]) -> String {
    let mut s = String::new();
    for r in recent {
        s.push_str(&r.name);
        s.push('\t');
        s.push_str(&r.host);
        s.push('\t');
        s.push_str(&r.port.to_string());
        s.push('\t');
        s.push_str(&r.user);
        s.push('\t');
        s.push_str(&r.cmd);
        s.push('\t');
        s.push_str(&r.when);
        s.push('\n');
    }
    s
}

fn parse_recent(text: &str) -> Vec<RecentSession> {
    let mut out = Vec::new();
    for line in text.lines() {
        let mut f = line.split('\t');
        let name = match f.next() {
            Some(n) if !n.is_empty() => n.to_string(),
            _ => continue,
        };
        let host = match f.next() {
            Some(h) if !h.is_empty() => h.to_string(),
            _ => continue,
        };
        let port: u16 = match f.next() {
            Some(p) => match p.parse() {
                Ok(v) => v,
                Err(_) => continue,
            },
            None => continue,
        };
        let user = match f.next() {
            Some(u) if !u.is_empty() => u.to_string(),
            _ => continue,
        };
        let cmd = f.next().map(|s| s.to_string()).unwrap_or_default();
        let when = f.next().map(|s| s.to_string()).unwrap_or_default();
        out.push(RecentSession {
            name,
            host,
            port,
            user,
            cmd,
            when,
        });
    }
    out
}

fn load_recent() -> Vec<RecentSession> {
    match std::fs::read_to_string(recent_path()) {
        Ok(text) => parse_recent(&text),
        Err(_) => Vec::new(),
    }
}

fn save_recent(recent: &[RecentSession]) {
    let _ = std::fs::write(recent_path(), serialize_recent(recent));
}

/// 以 host+port+user 为主键更新最近会话（最新在前，最多保留 12 条）。
fn upsert_recent(recent: &mut Vec<RecentSession>, mut r: RecentSession) {
    recent.retain(|x| !(x.host == r.host && x.port == r.port && x.user == r.user));
    r.when = now_compact();
    recent.insert(0, r);
    recent.truncate(12);
    save_recent(recent);
}

/// 紧凑时间戳（设计稿 002：MM-DD HH:MM）。
fn now_compact() -> String {
    chrono::Local::now().format("%m-%d %H:%M").to_string()
}

/// 最近会话时间显示：今天 / 昨天 + HH:MM，更早显示 MM-DD HH:MM。
/// 兼容旧版 recent.tsv 里的「今天 HH:MM:SS」格式（含中文，不能按字节切）。
fn recent_display(when: &str) -> String {
    if when.starts_with("今天 ") || when.starts_with("昨天 ") {
        return when.to_string();
    }
    if when.len() >= 11 && when.is_char_boundary(5) {
        let (date, time) = when.split_at(5); // "MM-DD" + " HH:MM"
        let today = chrono::Local::now().format("%m-%d").to_string();
        let yesterday = (chrono::Local::now() - chrono::Duration::days(1))
            .format("%m-%d")
            .to_string();
        if date == today {
            format!("今天{time}")
        } else if date == yesterday {
            format!("昨天{time}")
        } else {
            when.to_string()
        }
    } else {
        when.to_string()
    }
}

/// 以 host+port+user 为主键去重插入/覆盖。
fn upsert_account(accounts: &mut Vec<SavedAccount>, acc: SavedAccount) {
    if let Some(i) = accounts
        .iter()
        .position(|a| a.host == acc.host && a.port == acc.port && a.user == acc.user)
    {
        accounts[i] = acc;
    } else {
        accounts.push(acc);
    }
}

// =====================================================================

/// 从 ICO 字节中提取面积最大的 PNG 块（窗口图标使用）。
fn ico_largest_png(ico: &[u8]) -> Option<&[u8]> {
    if ico.len() < 6 {
        return None;
    }
    let count = u16::from_le_bytes([ico[4], ico[5]]) as usize;
    let mut best: Option<(u32, usize, usize)> = None; // (area, offset, size)
    for i in 0..count {
        let e = 6 + i * 16;
        if ico.len() < e + 16 {
            break;
        }
        let w = ico[e] as u32;
        let h = ico[e + 1] as u32;
        let size = u32::from_le_bytes([ico[e + 8], ico[e + 9], ico[e + 10], ico[e + 11]]) as usize;
        let off = u32::from_le_bytes([ico[e + 12], ico[e + 13], ico[e + 14], ico[e + 15]]) as usize;
        if ico.len() < off + size {
            continue;
        }
        let d = &ico[off..off + size];
        if d.starts_with(b"\x89PNG\r\n\x1a\n") {
            let w = if w == 0 { 256 } else { w };
            let h = if h == 0 { 256 } else { h };
            if best.as_ref().map_or(true, |(a, _, _)| w * h > *a) {
                best = Some((w * h, off, size));
            }
        }
    }
    best.map(|(_, off, size)| &ico[off..off + size])
}

fn main() -> eframe::Result<()> {
    // 崩溃诊断：任何 panic 都写入 exe 同目录 crash.log 并弹窗提示位置，
    // 便于在输入法提交等场景下直接定位崩溃行。
    std::panic::set_hook(Box::new(|info| {
        let where_ = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("crash.log")))
            .unwrap_or_else(|| std::path::PathBuf::from("crash.log"));
        let _ = std::fs::write(
            &where_,
            format!(
                "panic: {info}\n\n{}\n",
                std::backtrace::Backtrace::force_capture()
            ),
        );
        let msg = format!(
            "程序发生内部错误，即将退出。\n\n{info}\n\n崩溃详情已写入：\n{}",
            where_.display()
        );
        // Windows 下弹出错误对话框提示崩溃位置；macOS/Linux 仅写 crash.log
        #[cfg(target_os = "windows")]
        {
            unsafe {
                let wide: Vec<u16> = msg
                    .encode_utf16()
                    .chain(std::iter::once(0))
                    .collect();
                let cap: Vec<u16> = "SSH 远程终端 - 错误"
                    .encode_utf16()
                    .chain(std::iter::once(0))
                    .collect();
                extern "system" {
                    fn MessageBoxW(
                        hwnd: *const std::ffi::c_void,
                        text: *const u16,
                        caption: *const u16,
                        u_type: u32,
                    ) -> i32;
                }
                MessageBoxW(std::ptr::null_mut(), wide.as_ptr(), cap.as_ptr(), 0x10);
            }
        }
    }));

    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .init();

    // 窗口图标：从打包的 app.ico 中取出最大 PNG 尺寸作为窗口/任务栏图标
    let app_icon = ico_largest_png(include_bytes!("../packaging/app.ico"))
        .and_then(|png| eframe::icon_data::from_png_bytes(png).ok());
    let options = eframe::NativeOptions {
        // 渲染后端固定为 glow(OpenGL)。无独显/远程桌面环境下，
        // 将 Mesa3D(llvmpipe) 的 opengl32.dll 放置于 exe 同目录即可软件渲染。
        // 注意：不要改回 wgpu —— 无独显时 wgpu 回退到 d3d10warp.dll(WARP)，
        // 输入法提交时会触发 WARP 栈溢出崩溃（BEX64 / c0000409）。
        renderer: eframe::Renderer::Glow,
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1150.0, 780.0])
            .with_min_inner_size([820.0, 560.0])
            .with_drag_and_drop(true) // 支持从资源管理器拖放文件/文件夹上传
            .with_title("3F-Shell")
            .with_icon(app_icon.unwrap_or_default()), // 窗口/任务栏图标（来自 app.ico）
        ..Default::default()
    };

    // 系统级键盘钩子 + 全局热键（Ctrl+C 必达通道）：仅在 Windows 启用
    #[cfg(target_os = "windows")]
    {
        let _ = kbhook::start();
    }

    let result = eframe::run_native(
        "sky-remote-assistance",
        options,
        Box::new(|cc| Ok(Box::new(SshApp::new(cc)))),
    );
    // 事件循环以 Err 返回（初始化失败等）时不会触发 panic hook，这里补记日志
    if let Err(e) = &result {
        let where_ = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("crash.log")))
            .unwrap_or_else(|| std::path::PathBuf::from("crash.log"));
        let _ = std::fs::write(&where_, format!("eframe 事件循环返回错误：{e:?}"));
        result
    } else {
        // 窗口正常关闭后立即退出进程：连接打开时后台线程可能阻塞在
        // SSH 网络 IO（russh 无超时）的 block_on 中，若等它自然退出
        // 会造成关闭卡顿/进程残留，直接强退保证窗口秒关。
        #[cfg(target_os = "windows")]
        kbhook::stop();
        std::process::exit(0);
    }
}

// =====================================================================
// 单元测试：终端 <-> 文件面板联动的路径解析逻辑
// =====================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cd_line_parsing() {
        // 绝对路径
        assert_eq!(parse_cd_line("cd /var/log", &None, "root"), Some(String::from("/var/log")));
        // 多命令截断（&& / ; / |）
        assert_eq!(parse_cd_line("cd /var/log && ls", &None, "root"), Some(String::from("/var/log")));
        assert_eq!(parse_cd_line("cd /tmp; pwd", &None, "root"), Some(String::from("/tmp")));
        // 无参数 -> home
        assert_eq!(parse_cd_line("cd", &None, "root"), Some(String::from("/root")));
        assert_eq!(parse_cd_line("cd", &None, "alice"), Some(String::from("/home/alice")));
        // ~ 展开
        assert_eq!(parse_cd_line("cd ~", &Some(String::from("/root")), "root"), Some(String::from("/root")));
        assert_eq!(parse_cd_line("cd ~/a/b", &None, "alice"), Some(String::from("/home/alice/a/b")));
        // 相对路径 + ..
        assert_eq!(parse_cd_line("cd ..", &Some(String::from("/var/log")), "root"), Some(String::from("/var")));
        assert_eq!(parse_cd_line("cd ./x", &Some(String::from("/var")), "root"), Some(String::from("/var/x")));
        assert_eq!(parse_cd_line("cd x", &Some(String::from("/var")), "root"), Some(String::from("/var/x")));
        // 根目录
        assert_eq!(parse_cd_line("cd /", &None, "root"), Some(String::from("/")));
        // 不支持 / 忽略
        assert_eq!(parse_cd_line("cd -", &Some(String::from("/a")), "root"), None);
        assert_eq!(parse_cd_line("ls", &None, "root"), None);
        assert_eq!(parse_cd_line("cdrom", &None, "root"), None);
        assert_eq!(parse_cd_line("", &None, "root"), None);
    }

    #[test]
    fn normalize_paths() {
        assert_eq!(normalize_path("/a/b", "../c", "root"), String::from("/a/c"));
        assert_eq!(normalize_path("/", "x", "root"), String::from("/x"));
        assert_eq!(normalize_path("/a", "/b/../c", "root"), String::from("/c"));
        assert_eq!(normalize_path("/a", ".", "root"), String::from("/a"));
        assert_eq!(normalize_path("/a/b/", "", "root"), String::from("/a/b"));
        assert_eq!(normalize_path("/a/b/c", "../../x", "root"), String::from("/a/x"));
    }

    #[test]
    fn metric_pct_parsing() {
        // CPU：使用率 35.2%
        assert_eq!(
            metric_pct("使用率 35.2% / 空闲 64.8%"),
            Some(35.2)
        );
        // 内存 / 硬盘：括号内百分数
        assert_eq!(metric_pct("已用 1023M / 共 15.1G（7%）"), Some(7.0));
        assert_eq!(metric_pct("/ 已用 8.2G / 共 40G（22%）"), Some(22.0));
        // 半角括号也兼容
        assert_eq!(metric_pct("/ 已用 1.2G / 共 30G(4%)"), Some(4.0));
        // 异常数据回退
        assert_eq!(metric_pct("(no data)"), None);
        assert_eq!(metric_pct(""), None);
    }

    #[test]
    fn prompt_path_parsing() {
        // Debian/Ubuntu bash 默认提示符
        assert_eq!(parse_prompt_path("root@host:/var/log#"), Some(String::from("/var/log")));
        assert_eq!(parse_prompt_path("user@host:~$"), Some(String::from("~")));
        assert_eq!(parse_prompt_path("user@host:/srv/www $"), Some(String::from("/srv/www")));
        // 简单 PS1
        assert_eq!(parse_prompt_path("/root#"), Some(String::from("/root")));
        assert_eq!(parse_prompt_path("~$"), Some(String::from("~")));
        assert_eq!(parse_prompt_path("/home/user>"), Some(String::from("/home/user")));
        // 不支持的格式（安全跳过）
        assert_eq!(parse_prompt_path("user@host /x %"), None); // zsh 空格分隔
        assert_eq!(parse_prompt_path("hello world"), None);
        assert_eq!(parse_prompt_path(""), None);
        assert_eq!(parse_prompt_path("top - 12:00:00 up 1 day"), None);
        assert_eq!(parse_prompt_path("\"file.txt\" 12L, 45C"), None);
    }

    #[test]
    fn account_serialize_roundtrip() {
        let accounts = vec![
            SavedAccount {
                name: "root@10.0.0.5".into(),
                host: "10.0.0.5".into(),
                port: 22,
                user: "root".into(),
                password: Some("secret".into()),
                key: None,
                group: "生产环境".into(),
            },
            SavedAccount {
                name: "dev@example.com".into(),
                host: "example.com".into(),
                port: 2222,
                user: "dev".into(),
                password: None,
                key: Some("C:\\keys\\id_ed25519".into()),
                group: String::new(),
            },
        ];
        let text = serialize_accounts(&accounts);
        let parsed = parse_accounts(&text);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].host, "10.0.0.5");
        assert_eq!(parsed[0].port, 22);
        assert_eq!(parsed[0].password.as_deref(), Some("secret"));
        assert_eq!(parsed[1].port, 2222);
        assert_eq!(parsed[1].key.as_deref(), Some("C:\\keys\\id_ed25519"));
        assert_eq!(parsed[1].password, None);
    }

    #[test]
    fn account_parse_bad_lines() {
        // 损坏行（端口非法 / 用户为空）跳过；host+user 完整但 name 为空时自动补名
        let text = "root@h\t10.0.0.5\t22\troot\tpwd\t\n\tbad-no-host\t22\tuser\t\t\nx\ty\tnotaport\tu\t\t\nz\th2\t22\t\t\t\nok\th3\t22\tu3\t\t\n";
        let parsed = parse_accounts(text);
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0].host, "10.0.0.5");
        assert_eq!(parsed[1].name, "user@bad-no-host"); // name 空 -> 自动生成
        assert_eq!(parsed[2].name, "ok");
        assert_eq!(parsed[2].host, "h3");
    }

    #[test]
    fn account_upsert_dedupe() {
        let mut accounts = Vec::new();
        upsert_account(
            &mut accounts,
            SavedAccount { name: "a@h".into(), host: "h".into(), port: 22, user: "a".into(), password: Some("p1".into()), key: None, group: String::new() },
        );
        upsert_account(
            &mut accounts,
            SavedAccount { name: "b@h".into(), host: "h".into(), port: 22, user: "b".into(), password: None, key: None, group: String::new() },
        );
        assert_eq!(accounts.len(), 2);
        // 同 host+port+user 覆盖
        upsert_account(
            &mut accounts,
            SavedAccount { name: "a@h".into(), host: "h".into(), port: 22, user: "a".into(), password: Some("p2".into()), key: None, group: String::new() },
        );
        assert_eq!(accounts.len(), 2);
        assert_eq!(accounts[0].password.as_deref(), Some("p2"));
    }

    #[test]
    fn transfer_persist_roundtrip() {
        // 持久化往返：写入 -> 读回一致；进行中记录不保留；字段内 tab/换行被净化
        let path = std::env::temp_dir().join(format!("tlog_test_{}.tsv", std::process::id()));
        let log = vec![
            TransferRec { time: "10:01:02".into(), up: true, name: "a.txt".into(), size: Some(1024), ok: true, detail: "完成".into() },
            TransferRec { time: "10:01:03".into(), up: false, name: "dir b".into(), size: None, ok: false, detail: "进行中...".into() },
            TransferRec { time: "10:01:04".into(), up: false, name: "x\ty\nz".into(), size: Some(0), ok: true, detail: "完成".into() },
        ];
        persist_transfers_to(&path, &log);
        let loaded = load_transfers_from(&path);
        let _ = std::fs::remove_file(&path);
        // 进行中记录被过滤
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].up, true);
        assert_eq!(loaded[0].name, "a.txt");
        assert_eq!(loaded[0].size, Some(1024));
        assert_eq!(loaded[0].ok, true);
        assert_eq!(loaded[0].detail, "完成");
        // 字段内 tab / 换行被替换为空格
        assert_eq!(loaded[1].name, "x y z");
    }

    #[test]
    fn fmt_size_units() {
        // 单位索引防回归：除 2 次 1024 是 MB（units[u-1]），不是 GB
        assert_eq!(fmt_size(500), "500 B");
        assert_eq!(fmt_size(1024), "1.0 KB");
        assert_eq!(fmt_size(11151 * 1024), "10.9 MB"); // 11151 KB 的文件
        assert_eq!(fmt_size(1024 * 1024), "1.0 MB");
        assert_eq!(fmt_size(1024 * 1024 * 1024), "1.0 GB");
        assert_eq!(fmt_size(1024u64.pow(4)), "1.0 TB");
    }

    #[test]
    fn tab_completion_basic() {
        fn mk(name: &str, is_dir: bool) -> ssh::FileEntry {
            ssh::FileEntry {
                name: name.into(),
                path: String::new(),
                is_dir,
                is_symlink: false,
                size: None,
                perms: None,
                mtime: None,
            }
        }
        let entries = vec![
            mk("hello.txt", false),
            mk("hello_dir", true),
            mk("world.rs", false),
        ];
        // 单个匹配 -> 补全文件名
        let (out, new) = build_tab_completion(&entries, "cat hello.tx").unwrap();
        assert_eq!(new, "cat hello.txt");
        assert_eq!(out[0], 0x08); // 退格开头
        // 目录 -> 末尾加 /
        let (_, new) = build_tab_completion(&entries, "cd hello_d").unwrap();
        assert_eq!(new, "cd hello_dir/");
        // 多个匹配 -> 最长公共前缀
        let (_, new) = build_tab_completion(&entries, "ls he").unwrap();
        assert_eq!(new, "ls hello");
        // 无匹配 -> None
        assert!(build_tab_completion(&entries, "ls zzz").is_none());
        // 绝对路径 / 子目录路径 -> None（交给远端 shell）
        assert!(build_tab_completion(&entries, "ls /var/x").is_none());
        assert!(build_tab_completion(&entries, "ls sub/dir").is_none());
        // 空行 -> None
        assert!(build_tab_completion(&entries, "").is_none());
    }
}
