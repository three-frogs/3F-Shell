//! 系统级键盘钩子（WH_KEYBOARD_LL）。
//!
//! 目的：绕过输入法（IME）在 OS 层的按键拦截，可靠捕获 Ctrl+C。
//! 中文输入法打开时（微软拼音等）会把 Ctrl+C 吞掉，egui 收不到按键事件，
//! 导致「docker logs -f 时 Ctrl+C 无法中断」。本模块在系统层先于输入法
//! 拿到按键，检测到 Ctrl+C（且本程序为前台窗口）时置位原子标志，
//! UI 线程每帧消费该标志发送 SSH 中断信号。
//!
//! 注意：钩子只“标记”事件、不吞掉事件（始终 CallNextHookEx 放行），
//! 不影响其他程序与系统语义。

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, RegisterHotKey, UnregisterHotKey};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, PeekMessageW, PostThreadMessageW, SetWindowsHookExW,
    TranslateMessage, UnhookWindowsHookEx, HHOOK, KBDLLHOOKSTRUCT, MSG, PM_REMOVE, WM_KEYDOWN,
    WM_QUIT, WM_SYSKEYDOWN, WH_KEYBOARD_LL,
};

// 虚拟键码（不依赖常量名，直接数值最稳）
const VK_CONTROL: i32 = 0x11;
const VK_SHIFT: i32 = 0x10;
const VK_MENU: i32 = 0x12;
const VK_C: u32 = 0x43;

static CTRL_C_FLAG: AtomicBool = AtomicBool::new(false);
static EXIT_FLAG: AtomicBool = AtomicBool::new(false);
static HOOK_THREAD_ID: AtomicU32 = AtomicU32::new(0);
// 钩子句柄仅在钩子线程内读写（回调也在该线程执行），static mut 无数据竞争
static mut HOOK: HHOOK = std::ptr::null_mut();

// 全局热键：Ctrl+C（0x43 = 'C'，MOD_CONTROL = 0x0002）
const HOTKEY_ID: i32 = 0x3F00;
const WM_HOTKEY: u32 = 0x0312;

/// 钩子回调：检测 Ctrl+C（无 Shift/Alt）时置位标志。
/// 不做前台窗口判断：用户可能在点击其他窗口后直接按 Ctrl+C（焦点丢失场景），
/// 钩子只标记事件、始终放行（CallNextHookEx），不影响其他程序。
/// 回调整体用 catch_unwind 包裹，防止任何意外 panic 杀死钩子线程。
unsafe extern "system" fn ll_kb_proc(code: i32, wparam: usize, lparam: isize) -> isize {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if code >= 0 {
            let msg = wparam as u32;
            if msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN {
                let kb = &*(lparam as *const KBDLLHOOKSTRUCT);
                if kb.vkCode == VK_C {
                    let ctrl = (GetAsyncKeyState(VK_CONTROL) as u16) & 0x8000 != 0;
                    let shift = (GetAsyncKeyState(VK_SHIFT) as u16) & 0x8000 != 0;
                    let alt = (GetAsyncKeyState(VK_MENU) as u16) & 0x8000 != 0;
                    if ctrl && !shift && !alt {
                        CTRL_C_FLAG.store(true, Ordering::SeqCst);
                    }
                }
            }
        }
    }));
    CallNextHookEx(unsafe { HOOK }, code, wparam, lparam)
}

/// 启动钩子线程（进程生命周期内调用一次）。
/// 返回 SetWindowsHookExW 是否真正挂载成功（同步等待线程内的挂载结果）。
pub fn start() -> bool {
    let (tx, rx) = std::sync::mpsc::channel::<bool>();
    let spawned = std::thread::Builder::new()
        .name("kb-ctrl-c-hook".into())
        .spawn(move || unsafe {
            let hmod = GetModuleHandleW(std::ptr::null());
            let hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(ll_kb_proc), hmod, 0);
            if hook.is_null() {
                let err = GetLastError();
                eprintln!("[kbhook] SetWindowsHookExW 失败，错误码 {err}");
                let _ = tx.send(false);
                return;
            }
            HOOK = hook;
            HOOK_THREAD_ID.store(GetCurrentThreadId(), Ordering::SeqCst);
            let _ = tx.send(true);
            // 消息循环：改用 PeekMessageW 轮询（GetMessageW 在某些环境下会因
            // 返回 0/-1 提前退出导致钩子失效；轮询永不退出，仅由 EXIT_FLAG 控制）。
            // WM_HOTKEY（RegisterHotKey 热键）与 LL 钩子事件都在此分发。
            let mut msg: MSG = std::mem::zeroed();
            loop {
                if EXIT_FLAG.load(Ordering::SeqCst) {
                    break;
                }
                while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) > 0 {
                    // 全局热键 Ctrl+C：系统消息必然到达（不依赖 LL 钩子投递）
                    if msg.message == WM_HOTKEY && msg.wParam as i32 == HOTKEY_ID {
                        // Shift 按下（Ctrl+Shift+C 复制意图）时不触发中断，避免误中断远程任务
                        let shift = (GetAsyncKeyState(VK_SHIFT) as u16) & 0x8000 != 0;
                        if !shift {
                            CTRL_C_FLAG.store(true, Ordering::SeqCst);
                        }
                    }
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
                std::thread::sleep(std::time::Duration::from_millis(8));
            }
            UnhookWindowsHookEx(hook);
        });
    match spawned {
        Ok(_) => rx
            .recv_timeout(std::time::Duration::from_secs(3))
            .unwrap_or(false),
        Err(_) => false,
    }
}

/// UI 每帧调用：消费一次 Ctrl+C（原子取走，只返回一次 true）。
pub fn ctrl_c_pressed() -> bool {
    CTRL_C_FLAG.swap(false, Ordering::SeqCst)
}

/// 注册全局热键 Ctrl+C（仅当 3F-Shell 拥有焦点时调用，由 UI 每帧管理）。
/// 注册后系统级拦截 Ctrl+C，WM_HOTKEY 必然到达钩子线程的消息循环，
/// 不依赖 LL 钩子的事件投递（本机 LL 钩子普通键投递被环境吞掉）。
/// 3F-Shell 失焦时必须调用 unregister_hotkey，避免影响其他程序的 Ctrl+C。
pub fn register_hotkey() {
    unsafe {
        UnregisterHotKey(std::ptr::null_mut(), HOTKEY_ID);
        RegisterHotKey(std::ptr::null_mut(), HOTKEY_ID, 0x0002 /*MOD_CONTROL*/, 0x43);
    }
}

/// 注销全局热键 Ctrl+C（3F-Shell 失焦时调用，恢复其他程序的复制等行为）。
pub fn unregister_hotkey() {
    unsafe {
        UnregisterHotKey(std::ptr::null_mut(), HOTKEY_ID);
    }
}

/// 退出钩子线程（程序退出时调用）。
pub fn stop() {
    EXIT_FLAG.store(true, Ordering::SeqCst);
    unregister_hotkey();
    let tid = HOOK_THREAD_ID.load(Ordering::SeqCst);
    if tid != 0 {
        unsafe {
            PostThreadMessageW(tid, WM_QUIT, 0, 0);
        }
    }
}
