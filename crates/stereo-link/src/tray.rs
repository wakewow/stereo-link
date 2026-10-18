//! Windows 系统托盘图标：自建隐藏窗口 + 独立消息泵线程 + `Shell_NotifyIconW`。
//!
//! 为什么不用 `tray-icon` 之类的 crate：那类库通常要求「创建它的线程同时跑 winit/tao
//! 事件循环」。本项目把窗口最小化到托盘时，winit 窗口是隐藏的，事件循环不一定还在正常
//! 转发托盘消息。直接调 Win32 反而最简单也最可控：
//!
//! * [`Tray::install`] 起一个自己的线程，在**这个线程**里 `RegisterClassW` +
//!   `CreateWindowExW`（隐藏窗口）+ `Shell_NotifyIconW(NIM_ADD)`，然后 `GetMessageW` 泵消息；
//! * 鼠标/菜单/Explorer 重启（`TaskbarCreated`）都在这个线程里处理；
//! * 主线程只通过 `mpsc` 收事件（[`Tray::poll`]），并通过 `PostMessageW` 让托盘线程改
//!   tooltip / 删图标，完全不需要主线程有消息循环。
//!
//! **唤醒主循环**：窗口隐藏到托盘后，eframe 会阻塞等待下一次重绘请求，不再跑 `update()`。
//! 所以托盘线程每推一个事件进 `mpsc` 之后都会调用 [`egui::Context::request_repaint`]，
//! 并额外向 winit 主窗口 `PostMessageW(WM_PAINT)`——只调 `request_repaint` 不够：它的重绘
//! 请求最终会走 winit 的 `RedrawWindow(..., RDW_INTERNALPAINT)`，而隐藏窗口不会产生
//! `WM_PAINT`，主循环永远醒不过来（详见 [`wake_hidden_main_window`] 的注释）。
//!
//! 图标用 `main.rs::app_icon()` 生成的 RGBA（`egui::IconData` 的 `rgba` + `width`），
//! 这里用 GDI 的 32bpp DIB 转成 `HICON`。

#![cfg(windows)]

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, Sender, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{anyhow, Result};
use eframe::egui;
use windows::core::{w, BOOL, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateBitmap, CreateDIBSection, DeleteObject, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS,
    HGDIOBJ,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY,
    NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIconIndirect, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyIcon,
    DestroyMenu, DestroyWindow, DispatchMessageW, EnumWindows, GetClassNameW, GetCursorPos,
    GetMessageW, GetWindowThreadProcessId, IsWindow, PostMessageW, RegisterClassW,
    RegisterWindowMessageW, SetForegroundWindow, TrackPopupMenu, TranslateMessage, ICONINFO,
    MF_SEPARATOR, MF_STRING, MSG, TPM_LEFTALIGN, TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_APP,
    WM_LBUTTONDBLCLK, WM_LBUTTONUP, WM_PAINT, WM_RBUTTONUP, WNDCLASSW, WS_EX_TOOLWINDOW, WS_POPUP,
};

/// 托盘里发生的事。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayEvent {
    /// 用户左键点了托盘图标（或菜单里选了「显示主界面」）→ 调用方应把主窗口显示出来。
    Show,
    /// 用户右键菜单里选了「退出」→ 调用方应停止路由并关闭窗口。
    Quit,
}

/// 托盘图标句柄。Drop 时自动移除图标。
pub struct Tray {
    /// 托盘线程里那个隐藏窗口的 HWND（以 isize 保存，避免跨线程 `Send` 问题）。
    hwnd: isize,
    /// 托盘线程发来的事件。
    events: Receiver<TrayEvent>,
    /// 待应用的 tooltip 文本（写在这里，然后 PostMessage 叫托盘线程去 NIM_MODIFY）。
    pending_tip: Arc<Mutex<Option<String>>>,
    /// 托盘线程句柄（remove 时 join）。
    thread: Mutex<Option<JoinHandle<()>>>,
    /// 是否已经移除过（幂等）。
    removed: AtomicBool,
}

// 自定义消息：托盘回调、换 tooltip、退出线程。
const WM_TRAY_CALLBACK: u32 = WM_APP + 1;
const WM_SET_TIP: u32 = WM_APP + 2;
const WM_QUIT_TRAY: u32 = WM_APP + 3;

/// 隐藏窗口的类名。
const WINDOW_CLASS: PCWSTR = w!("StereoLinkTrayHiddenWindow");
/// 托盘图标 ID（同一窗口里可以放多个图标，这里只用 1 个）。
const TRAY_ID: u32 = 1;
/// 右键菜单项 ID。
const MENU_SHOW: usize = 1;
const MENU_QUIT: usize = 2;

/// 托盘线程里 wndproc 需要的数据。
struct TrayCtx {
    taskbar_created: u32,
    hicon: windows::Win32::UI::WindowsAndMessaging::HICON,
    tip: Arc<Mutex<Option<String>>>,
    events: Sender<TrayEvent>,
    /// egui 上下文：每推一个事件就 `request_repaint()`，把阻塞等待的 eframe 主循环叫醒。
    ctx: egui::Context,
}

impl TrayCtx {
    /// 把事件交给主线程，并唤醒 egui 主循环。
    ///
    /// **所有**事件的投递都必须走这里（左键、菜单、将来新增的事件），否则窗口隐藏到
    /// 托盘后主循环可能还在等下一条重绘请求，事件会一直躺在 channel 里没人处理。
    fn emit(&self, event: TrayEvent) {
        let _ = self.events.send(event);
        self.ctx.request_repaint();
        // SAFETY: 只做窗口枚举 + PostMessage，不跨线程访问 Rust 状态。
        unsafe { wake_hidden_main_window() };
    }
}

/// 主窗口 HWND 缓存（0 = 尚未找到）。放在原子变量里，托盘线程随时可写。
static MAIN_WINDOW: AtomicIsize = AtomicIsize::new(0);

/// winit 在 Windows 上给普通窗口注册的默认类名（`winit::platform_impl::windows::mod`）。
const WINIT_WINDOW_CLASS: &str = "Window Class";

/// `EnumWindows` 回调：找当前进程里类名为 `WINIT_WINDOW_CLASS` 的顶层窗口。
///
/// # Safety
/// `lparam` 必须是 `*mut isize`（调用方传的就是 `&mut found` 的地址）。
unsafe extern "system" fn find_main_window(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let mut win_pid = 0u32;
    unsafe {
        let _ = GetWindowThreadProcessId(hwnd, Some(&mut win_pid));
    }
    if win_pid != unsafe { GetCurrentProcessId() } {
        return BOOL(1); // TRUE = 继续枚举
    }
    let mut buf = [0u16; 128];
    let n = unsafe { GetClassNameW(hwnd, &mut buf) };
    if n > 0 && String::from_utf16_lossy(&buf[..n as usize]) == WINIT_WINDOW_CLASS {
        unsafe { *(lparam.0 as *mut isize) = hwnd.0 as isize };
        return BOOL(0); // FALSE = 停止枚举
    }
    BOOL(1)
}

/// 叫醒隐藏的 eframe 主循环：向 winit 主窗口投递一条 `WM_PAINT`。
///
/// 为什么不只靠 `request_repaint()`：eframe 的 repaint 请求最终落到 winit 的
/// `Window::request_redraw()`，而 winit 在 Windows 上把它实现成
/// `RedrawWindow(hwnd, null, 0, RDW_INTERNALPAINT)`。窗口被 `ShowWindow(SW_HIDE)` 隐藏后
/// 系统不会再产生 `WM_PAINT`，winit 就不会派发 `RedrawRequested`，eframe 的 `update()`
/// 也就永远不会被调用。手动 `PostMessageW(WM_PAINT)` 会走 winit 的 `WM_PAINT` 分支，
/// 把 `RedrawRequested` 派发出去，从而让主循环跑一帧 `update()`。
///
/// 主窗口靠 `EnumWindows` + 进程 ID + 类名定位；首次找到后缓存 HWND，之后每条事件
/// 只多一次 `IsWindow` + `PostMessage`，开销可以忽略。
///
/// 除托盘事件外，`app::VolumeWaker` 在「收进托盘期间检测到系统音量变化」时也会调用它，
/// 否则最小化/隐藏状态下 `update()` 不跑，音量跟随会失灵。
///
/// # Safety
/// 只调用 Win32 窗口 API；`MAIN_WINDOW` 里存的 HWND 用 `IsWindow` 校验后才使用。
pub(crate) unsafe fn wake_hidden_main_window() {
    let mut hwnd = MAIN_WINDOW.load(Ordering::Relaxed);
    if hwnd == 0 || !unsafe { IsWindow(Some(HWND(hwnd as *mut core::ffi::c_void))) }.as_bool() {
        let mut found: isize = 0;
        unsafe {
            let _ = EnumWindows(
                Some(find_main_window),
                LPARAM((&mut found as *mut isize) as isize),
            );
        }
        if found == 0 {
            return;
        }
        MAIN_WINDOW.store(found, Ordering::Relaxed);
        hwnd = found;
    }
    unsafe {
        let _ = PostMessageW(
            Some(HWND(hwnd as *mut core::ffi::c_void)),
            WM_PAINT,
            WPARAM(0),
            LPARAM(0),
        );
    }
}

thread_local! {
    /// 托盘线程专属状态（wndproc 是 `extern "system"` 普通函数，拿不到闭包，只能这样存）。
    static CTX: RefCell<Option<TrayCtx>> = const { RefCell::new(None) };
}

impl Tray {
    /// 安装托盘图标。
    ///
    /// * `tooltip`：鼠标悬停时显示的文本（太长会被截断到 127 个宽字符）；
    /// * `rgba`：`egui::IconData::rgba`，每像素 RGBA、非预乘；
    /// * `size`：图标边长（正方形），例如 64；
    /// * `ctx`：egui 上下文（`&egui::Context` 的 clone）。托盘线程推事件后会
    ///   `ctx.request_repaint()` 唤醒主循环——窗口隐藏到托盘、主循环阻塞等待时也能被叫醒。
    ///
    /// 可以在任意线程调用：本函数内部会起一个专属线程跑消息泵。
    pub fn install(tooltip: &str, rgba: &[u8], size: u32, ctx: egui::Context) -> Result<Self> {
        if size == 0 {
            return Err(anyhow!("托盘图标尺寸不能为 0"));
        }
        if rgba.len() != size as usize * size as usize * 4 {
            return Err(anyhow!(
                "托盘图标 RGBA 长度不对：尺寸 {size}×{size} 需要 {} 字节，实际 {} 字节",
                size as usize * size as usize * 4,
                rgba.len()
            ));
        }

        let tip = Arc::new(Mutex::new(Some(tooltip.to_string())));
        let tip_for_thread = Arc::clone(&tip);
        let (events_tx, events_rx) = std::sync::mpsc::channel();
        let (ready_tx, ready_rx) = sync_channel::<Result<isize>>(1);
        let rgba = rgba.to_vec();

        let thread = std::thread::Builder::new()
            .name("stereo-link-tray".to_string())
            .spawn(move || tray_thread(rgba, size, tip_for_thread, events_tx, ctx, ready_tx))
            .map_err(|e| anyhow!("创建托盘线程失败：{e}"))?;

        let hwnd = match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(hwnd)) => hwnd,
            Ok(Err(e)) => return Err(anyhow!("托盘初始化失败：{e}")),
            Err(_) => return Err(anyhow!("托盘初始化超时（5 秒内没有回应）")),
        };

        Ok(Self {
            hwnd,
            events: events_rx,
            pending_tip: tip,
            thread: Mutex::new(Some(thread)),
            removed: AtomicBool::new(false),
        })
    }

    /// 非阻塞取一个事件（每帧调用一次即可）。
    pub fn poll(&self) -> Option<TrayEvent> {
        self.events.try_recv().ok()
    }

    /// 换 tooltip 文本（下次鼠标悬停时生效）。
    pub fn set_tooltip(&self, text: &str) {
        if self.removed.load(Ordering::SeqCst) {
            return;
        }
        if let Ok(mut pending) = self.pending_tip.lock() {
            *pending = Some(text.to_string());
        }
        self.post(WM_SET_TIP);
    }

    /// 移除托盘图标（幂等；Drop 时也会调）。
    pub fn remove(&self) {
        if self.removed.swap(true, Ordering::SeqCst) {
            return;
        }
        self.post(WM_QUIT_TRAY);
        // 等托盘线程真正删掉图标并退出，避免留下「僵尸图标」。
        if let Ok(mut guard) = self.thread.lock() {
            if let Some(handle) = guard.take() {
                let _ = handle.join();
            }
        }
    }

    fn post(&self, msg: u32) {
        let hwnd = HWND(self.hwnd as *mut core::ffi::c_void);
        // SAFETY: hwnd 由托盘线程创建、在 Tray 存活期间有效；PostMessage 只是投递消息。
        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                Some(hwnd),
                msg,
                WPARAM(0),
                LPARAM(0),
            );
        }
    }

    /// 测试/自证用：托盘隐藏窗口的 HWND（`isize` 形式，跨线程可用）。
    #[doc(hidden)]
    pub fn hwnd(&self) -> isize {
        self.hwnd
    }
}

/// 测试/自证用：向托盘隐藏窗口投递一次「左键点击」消息。
///
/// 走的是和真实鼠标点击完全相同的路径：`WM_TRAY_CALLBACK` → wndproc →
/// `TrayCtx::emit(Show)`（推 channel + `request_repaint`）。主窗口隐藏后调用它，
/// 可以验证 eframe 主循环确实会被托盘事件唤醒。返回 `PostMessageW` 是否成功。
#[doc(hidden)]
pub fn simulate_left_click_at(hwnd: isize) -> bool {
    let hwnd = HWND(hwnd as *mut core::ffi::c_void);
    // SAFETY: hwnd 由托盘线程创建；PostMessage 只投递消息，不做其它操作。
    unsafe {
        windows::Win32::UI::WindowsAndMessaging::PostMessageW(
            Some(hwnd),
            WM_TRAY_CALLBACK,
            WPARAM(TRAY_ID as usize),
            LPARAM(WM_LBUTTONUP as isize),
        )
        .is_ok()
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        self.remove();
    }
}

/// 托盘线程主体：建窗口、加图标、泵消息。
fn tray_thread(
    rgba: Vec<u8>,
    size: u32,
    tip: Arc<Mutex<Option<String>>>,
    events: Sender<TrayEvent>,
    ctx: egui::Context,
    ready: SyncSender<Result<isize>>,
) {
    match unsafe { setup_and_pump(rgba, size, tip, events, ctx, &ready) } {
        Ok(()) => {}
        Err(e) => {
            // 还没通知 install 成功（或已经通知过）都发一条错误；install 只取第一条。
            let _ = ready.send(Err(e));
        }
    }
}

/// 真正的 Win32 初始化 + 消息循环。返回时图标已被删除。
unsafe fn setup_and_pump(
    rgba: Vec<u8>,
    size: u32,
    tip: Arc<Mutex<Option<String>>>,
    events: Sender<TrayEvent>,
    ctx: egui::Context,
    ready: &SyncSender<Result<isize>>,
) -> Result<()> {
    // 1) 注册隐藏窗口类。
    let hinstance =
        unsafe { GetModuleHandleW(None) }.map_err(|e| anyhow!("GetModuleHandleW 失败：{e}"))?;
    let hinstance = HINSTANCE(hinstance.0);

    let wndclass = WNDCLASSW {
        lpfnWndProc: Some(tray_wndproc),
        hInstance: hinstance,
        lpszClassName: WINDOW_CLASS,
        ..Default::default()
    };
    let atom = unsafe { RegisterClassW(&wndclass) };
    if atom == 0 {
        return Err(anyhow!("RegisterClassW 失败"));
    }

    // 2) 建隐藏窗口（不能用 HWND_MESSAGE：消息窗口收不到 TaskbarCreated 广播）。
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW,
            WINDOW_CLASS,
            w!("StereoLink 托盘窗口"),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(hinstance),
            None,
        )
    }
    .map_err(|e| anyhow!("CreateWindowExW 失败：{e}"))?;

    // 3) RGBA → HICON。
    let hicon =
        unsafe { hicon_from_rgba(&rgba, size) }.map_err(|e| anyhow!("生成托盘图标失败：{e}"))?;

    // 4) 注册 TaskbarCreated（Explorer 重启后要重新添加图标）。
    let taskbar_created = unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) };
    if taskbar_created == 0 {
        // 不致命：拿不到消息只是 Explorer 重启后图标可能丢。
        tracing::warn!(
            "RegisterWindowMessageW(TaskbarCreated) 失败，Explorer 重启后托盘图标可能消失"
        );
    }

    // 5) 把上下文交给 wndproc。
    CTX.with(|slot| {
        *slot.borrow_mut() = Some(TrayCtx {
            taskbar_created,
            hicon,
            tip: Arc::clone(&tip),
            events: events.clone(),
            ctx: ctx.clone(),
        });
    });

    // 6) 添加图标；失败要报错而不是静默（任务书硬性要求）。
    let initial_tip = tip.lock().ok().and_then(|t| t.clone()).unwrap_or_default();
    let nid = unsafe { make_nid(hwnd, hicon, &initial_tip) };
    if !unsafe { Shell_NotifyIconW(NIM_ADD, &nid) }.as_bool() {
        unsafe {
            let _ = DestroyIcon(hicon);
        }
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
        CTX.with(|ctx| *ctx.borrow_mut() = None);
        return Err(anyhow!(
            "Shell_NotifyIconW(NIM_ADD) 失败（通知区域可能被系统策略禁用）"
        ));
    }
    tracing::info!("托盘图标已添加（hwnd={:?}，尺寸 {size}×{size}）", hwnd.0);

    // 7) 通知 install 成功。
    let _ = ready.send(Ok(hwnd.0 as isize));

    // 8) 消息泵。
    let mut msg = MSG::default();
    loop {
        let ret = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if ret.0 == 0 || ret.0 == -1 {
            break; // WM_QUIT 或错误
        }
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    // 9) 清理：删图标、毁图标句柄、销毁窗口。
    let nid = unsafe { make_nid(hwnd, hicon, "") };
    unsafe {
        let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
    }
    unsafe {
        let _ = DestroyIcon(hicon);
    }
    unsafe {
        let _ = DestroyWindow(hwnd);
    }
    CTX.with(|ctx| *ctx.borrow_mut() = None);
    tracing::info!("托盘图标已移除，托盘线程退出");
    Ok(())
}

/// 托盘窗口的消息处理。
unsafe extern "system" fn tray_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // 注意：这里不能长时间持锁，避免和主线程的 set_tooltip 互相等。
    match msg {
        WM_TRAY_CALLBACK => {
            // 没调用 NIM_SETVERSION，走老式回调：lParam 低 16 位是鼠标消息。
            let mouse_msg = (lparam.0 as u32) & 0xFFFF;
            tracing::debug!("托盘回调：mouse_msg=0x{mouse_msg:04X}");
            match mouse_msg {
                WM_LBUTTONUP | WM_LBUTTONDBLCLK => {
                    let _ = wparam;
                    CTX.with(|ctx| {
                        if let Some(ctx) = ctx.borrow().as_ref() {
                            ctx.emit(TrayEvent::Show);
                        }
                    });
                }
                WM_RBUTTONUP => unsafe { show_context_menu(hwnd) },
                _ => {}
            }
            LRESULT(0)
        }
        WM_SET_TIP => {
            // 主线程把新 tooltip 放进 Arc<Mutex>，这里取出来应用。
            let new_tip = CTX.with(|ctx| {
                ctx.borrow()
                    .as_ref()
                    .and_then(|ctx| ctx.tip.lock().ok().and_then(|mut t| t.take()))
            });
            if let Some(text) = new_tip {
                let (hicon, mut nid) = CTX.with(|ctx| {
                    let borrowed = ctx.borrow();
                    let ctx = borrowed.as_ref().expect("托盘上下文已初始化");
                    (ctx.hicon, unsafe { make_nid(hwnd, ctx.hicon, &text) })
                });
                nid.uFlags = NIF_TIP;
                nid.hIcon = hicon;
                unsafe {
                    let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
                }
            }
            LRESULT(0)
        }
        WM_QUIT_TRAY => {
            let nid = CTX.with(|ctx| {
                let borrowed = ctx.borrow();
                let ctx = borrowed.as_ref().expect("托盘上下文已初始化");
                unsafe { make_nid(hwnd, ctx.hicon, "") }
            });
            unsafe {
                let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
            }
            unsafe {
                let _ = DestroyWindow(hwnd);
            } // 触发 WM_DESTROY → PostQuitMessage
            LRESULT(0)
        }
        _ => {
            // Explorer 重启后广播的 TaskbarCreated：重新添加图标。
            let is_taskbar_created = CTX.with(|ctx| {
                ctx.borrow()
                    .as_ref()
                    .map(|ctx| ctx.taskbar_created != 0 && msg == ctx.taskbar_created)
                    .unwrap_or(false)
            });
            if is_taskbar_created {
                let (hicon, nid) = CTX.with(|ctx| {
                    let borrowed = ctx.borrow();
                    let ctx = borrowed.as_ref().expect("托盘上下文已初始化");
                    let tip = ctx
                        .tip
                        .lock()
                        .ok()
                        .and_then(|t| t.clone())
                        .unwrap_or_default();
                    (ctx.hicon, unsafe { make_nid(hwnd, ctx.hicon, &tip) })
                });
                let ok = unsafe { Shell_NotifyIconW(NIM_ADD, &nid) }.as_bool();
                tracing::info!("Explorer 重启（TaskbarCreated），重新添加托盘图标：{ok}");
                let _ = hicon;
                return LRESULT(0);
            }

            if msg == 0x0010 {
                // WM_CLOSE：直接销毁窗口（正常不会走到，兜底）。
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
                return LRESULT(0);
            }
            if msg == 0x0002 {
                // WM_DESTROY
                unsafe { windows::Win32::UI::WindowsAndMessaging::PostQuitMessage(0) };
                return LRESULT(0);
            }
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
    }
}

/// 右键菜单：显示主界面 / 退出。
unsafe fn show_context_menu(hwnd: HWND) {
    let menu = match unsafe { CreatePopupMenu() } {
        Ok(menu) => menu,
        Err(_) => return,
    };
    // SAFETY: menu 是刚创建的有效菜单句柄；字符串都是静态宽字符串。
    unsafe {
        let _ = AppendMenuW(menu, MF_STRING, MENU_SHOW, w!("显示主界面"));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(menu, MF_STRING, MENU_QUIT, w!("退出"));
    }

    let mut pt = POINT::default();
    let _ = unsafe { GetCursorPos(&mut pt) };

    // 经典坑：不先把窗口设为前台，菜单点外面不会消失。
    let _ = unsafe { SetForegroundWindow(hwnd) };
    let cmd = unsafe {
        TrackPopupMenu(
            menu,
            TPM_LEFTALIGN | TPM_RIGHTBUTTON | TPM_RETURNCMD,
            pt.x,
            pt.y,
            None,
            hwnd,
            None,
        )
    };
    unsafe {
        let _ = DestroyMenu(menu);
    }

    // 让菜单彻底消失（MSDN 建议）。
    unsafe {
        let _ = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
            Some(hwnd),
            0, // WM_NULL
            WPARAM(0),
            LPARAM(0),
        );
    }

    let command = cmd.0;
    if command == 0 {
        return;
    }
    CTX.with(|ctx| {
        if let Some(ctx) = ctx.borrow().as_ref() {
            match command as usize {
                MENU_SHOW => ctx.emit(TrayEvent::Show),
                MENU_QUIT => ctx.emit(TrayEvent::Quit),
                _ => {}
            }
        }
    });
}

/// 组一个 NOTIFYICONDATAW（带图标 + tooltip + 回调消息）。
unsafe fn make_nid(
    hwnd: HWND,
    hicon: windows::Win32::UI::WindowsAndMessaging::HICON,
    tip: &str,
) -> NOTIFYICONDATAW {
    let mut sz_tip = [0u16; 128];
    let wide: Vec<u16> = tip.encode_utf16().take(127).collect();
    sz_tip[..wide.len()].copy_from_slice(&wide);
    NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ID,
        uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
        uCallbackMessage: WM_TRAY_CALLBACK,
        hIcon: hicon,
        szTip: sz_tip,
        ..Default::default()
    }
}

/// 把 RGBA（非预乘）转成 32bpp HICON。
unsafe fn hicon_from_rgba(
    rgba: &[u8],
    size: u32,
) -> Result<windows::Win32::UI::WindowsAndMessaging::HICON> {
    let n = size as i32;
    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: n,
            // 负数 = 从上到下（我们数组就是按行连续的）。
            biHeight: -n,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: windows::Win32::Graphics::Gdi::BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };

    let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
    let color = unsafe { CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0) }
        .map_err(|e| anyhow!("CreateDIBSection 失败：{e}"))?;
    if bits.is_null() {
        unsafe {
            let _ = DeleteObject(HGDIOBJ(color.0));
        }
        return Err(anyhow!("CreateDIBSection 返回了空像素指针"));
    }

    // 写成 BGRA + 预乘 alpha（Windows 图标按预乘 alpha 合成，边缘才不会有白边/黑边）。
    let dst = unsafe { std::slice::from_raw_parts_mut(bits as *mut u8, rgba.len()) };
    let src_pixels = rgba.as_chunks::<4>().0;
    let dst_pixels = dst.as_chunks_mut::<4>().0;
    for (src, out) in src_pixels.iter().zip(dst_pixels.iter_mut()) {
        let (r, g, b, a) = (src[0] as u32, src[1] as u32, src[2] as u32, src[3] as u32);
        out[0] = ((b * a + 127) / 255) as u8;
        out[1] = ((g * a + 127) / 255) as u8;
        out[2] = ((r * a + 127) / 255) as u8;
        out[3] = a as u8;
    }

    let mask = unsafe { CreateBitmap(n, n, 1, 1, None) };
    let info = ICONINFO {
        fIcon: BOOL(1),
        xHotspot: 0,
        yHotspot: 0,
        hbmMask: mask,
        hbmColor: color,
    };
    let hicon = unsafe { CreateIconIndirect(&info) }
        .map_err(|e| anyhow!("CreateIconIndirect 失败：{e}"))?;

    unsafe {
        let _ = DeleteObject(HGDIOBJ(color.0));
        let _ = DeleteObject(HGDIOBJ(mask.0));
    }
    Ok(hicon)
}
