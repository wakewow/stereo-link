//! 托盘唤醒主循环的自证探针（GUI 版）。
//!
//! 这个 example 专门验证一件事：**主窗口隐藏到托盘后，eframe 主循环会阻塞等待；
//! 托盘事件（经 `egui::Context::request_repaint()`）能把主循环重新叫醒。**
//!
//! 流程（打印同时写入日志文件）：
//! 1. 启动 eframe 窗口 + 安装托盘图标；
//! 2. 第 20 帧左右执行 `ViewportCommand::Minimized(true)` + `Visible(false)`，
//!    之后**不再请求周期重绘**（模拟正式 GUI 隐藏后的省 CPU 行为）；
//! 3. 后台线程等 1.5 s，再统计 0.5 s 内 `update()` 次数（应为 0 → 证明主循环确实
//!    阻塞等待，而不是忙等空转）；
//! 4. 后台线程用 `simulate_left_click_at(hwnd)` 投递一次托盘左键消息
//!    （`PostMessageW` → wndproc → 推事件 + `request_repaint()`）；
//! 5. 主窗口如果被唤醒，`update()` 里会 poll 到 `Show` 事件 → 日志写 `PASS`，
//!    恢复显示窗口并退出（退出码 0）。超时没醒 → 退出码 1。
//!
//! 用法：
//! ```text
//! cargo run -p stereo-link --example tray_probe -- E:\AI\MiPlay\.scratch\tray-probe-egui.log
//! ```
//! 参数是日志文件路径（默认写到系统临时目录）。图形界面用 ASCII 文案，避免依赖中文字体；
//! 日志文件与 stdout 都是中文/UTF-8。
//!
//! 为什么要 `#[path]` 引 src/tray.rs：本包只有 bin target，example 不能 `use`
//! `crate::tray`（那是 bin 里的私有模块），只能把源文件直接编进 example。

#![cfg(windows)]

#[path = "../src/tray.rs"]
mod tray;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use eframe::egui;

fn main() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_writer(std::io::stderr)
        .try_init();
    let log_path = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("stereo-link-tray-probe.log"));
    let _ = std::fs::write(&log_path, "");
    logln(
        &log_path,
        &format!("[probe] 日志文件：{}", log_path.display()),
    );
    let no_hide = std::env::var_os("TRAY_PROBE_NO_HIDE").is_some();
    let minimize = std::env::var_os("TRAY_PROBE_MINIMIZE").is_some();
    logln(
        &log_path,
        if no_hide {
            "[probe] 模式：不隐藏窗口（只验证托盘回调 → request_repaint 路径）"
        } else if minimize {
            "[probe] 模式：最小化窗口（Minimized(true)，模拟正式 GUI 的托盘方式）后验证托盘事件唤醒主循环"
        } else {
            "[probe] 模式：隐藏窗口（Visible(false)）后验证托盘事件唤醒主循环"
        },
    );

    let hidden = Arc::new(AtomicBool::new(false));
    let frames = Arc::new(AtomicU64::new(0));
    let hwnd_slot: Arc<Mutex<Option<isize>>> = Arc::new(Mutex::new(None));
    let pass = Arc::new(AtomicBool::new(false));

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("StereoLink 托盘唤醒探针")
            .with_inner_size([760.0, 320.0]),
        ..Default::default()
    };

    let log_for_app = log_path.clone();
    let hidden_for_app = Arc::clone(&hidden);
    let frames_for_app = Arc::clone(&frames);
    let hwnd_for_app = Arc::clone(&hwnd_slot);
    let pass_for_app = Arc::clone(&pass);
    let log_for_thread = log_path.clone();
    let hidden_for_thread = Arc::clone(&hidden);
    let frames_for_thread = Arc::clone(&frames);
    let hwnd_for_thread = Arc::clone(&hwnd_slot);
    let minimize_for_app = minimize;

    let result = eframe::run_native(
        "StereoLink 托盘唤醒探针",
        options,
        Box::new(move |cc| {
            let (rgba, size) = probe_icon();
            let tray = match tray::Tray::install(
                "StereoLink 托盘唤醒探针（隐藏后点我唤回）",
                &rgba,
                size,
                cc.egui_ctx.clone(),
            ) {
                Ok(t) => {
                    *hwnd_for_app.lock().expect("hwnd slot") = Some(t.hwnd());
                    logln(&log_for_app, "[probe] 托盘图标已安装");
                    Some(t)
                }
                Err(e) => {
                    logln(&log_for_app, &format!("[probe] 托盘安装失败：{e:#}"));
                    None
                }
            };

            // 后台线程：等窗口隐藏后，检查主循环是否真的停住，再投递托盘点击把它叫醒。
            std::thread::Builder::new()
                .name("tray-probe-waker".to_string())
                .spawn(move || {
                    while !hidden_for_thread.load(Ordering::SeqCst) {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    logln(&log_for_thread, "[probe] 窗口已隐藏，等 1.5 s 后检查主循环是否停滞");
                    std::thread::sleep(Duration::from_millis(1500));
                    let before = frames_for_thread.load(Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(500));
                    let after = frames_for_thread.load(Ordering::SeqCst);
                    logln(
                        &log_for_thread,
                        &format!(
                            "[probe] 隐藏/最小化后 0.5 s 内 update() 次数 = {}（0 = 主循环已阻塞，非忙等）",
                            after.saturating_sub(before)
                        ),
                    );
                    let hwnd = *hwnd_for_thread.lock().expect("hwnd slot");
                    if let Some(hwnd) = hwnd {
                        logln(
                            &log_for_thread,
                            "[probe] 投递托盘左键消息：PostMessage(WM_TRAY_CALLBACK) → wndproc → request_repaint()",
                        );
                        let posted = tray::simulate_left_click_at(hwnd);
                        logln(&log_for_thread, &format!("[probe] PostMessage 返回：{posted}"));
                        // 3 s 后如果还没被唤醒，再补一发（排查「第一次事件丢了」的情况）。
                        std::thread::sleep(Duration::from_millis(3000));
                        let again = frames_for_thread.load(Ordering::SeqCst);
                        logln(
                            &log_for_thread,
                            &format!(
                                "[probe] 点击后 3 s：update() 次数从 {} 变成 {}",
                                after, again
                            ),
                        );
                        if again == after {
                            let retry = tray::simulate_left_click_at(hwnd);
                            logln(
                                &log_for_thread,
                                &format!("[probe] 主循环仍未动，补投一次点击：{retry}"),
                            );
                        }
                    } else {
                        logln(&log_for_thread, "[probe] 托盘未安装，无法投递事件");
                    }
                })
                .ok();

            Ok(Box::new(Probe {
                tray,
                log_path: log_for_app,
                frames: frames_for_app,
                hidden: hidden_for_app,
                pass: pass_for_app,
                wake_frame: None,
                phase: 0,
                frame: 0,
                event_count: 0,
                no_hide,
                minimize: minimize_for_app,
            }))
        }),
    );

    if let Err(e) = &result {
        logln(&log_path, &format!("[probe] run_native 返回错误：{e}"));
    }
    let ok = pass.load(Ordering::SeqCst);
    logln(
        &log_path,
        if ok {
            "[probe] 结论：PASS —— 隐藏/最小化后，托盘事件成功唤醒主循环"
        } else {
            "[probe] 结论：FAIL —— 隐藏/最小化后主循环没有被托盘事件唤醒"
        },
    );
    std::process::exit(if ok && result.is_ok() { 0 } else { 1 });
}

struct Probe {
    tray: Option<tray::Tray>,
    log_path: PathBuf,
    frames: Arc<AtomicU64>,
    hidden: Arc<AtomicBool>,
    pass: Arc<AtomicBool>,
    wake_frame: Option<u64>,
    /// 0 = 正常显示等待隐藏；1 = 已隐藏、等待托盘事件；2 = 已唤醒、准备退出。
    phase: u8,
    frame: u64,
    event_count: u32,
    /// 自检模式：不隐藏窗口，只验证「托盘回调 → request_repaint」这条链路。
    no_hide: bool,
    /// 最小化模式：不 `Visible(false)`，改为 `Minimized(true)`（正式 GUI 的托盘方式）。
    minimize: bool,
}

impl Probe {
    const HIDE_AFTER_FRAMES: u64 = 20;
}

impl eframe::App for Probe {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.frame += 1;
        self.frames.store(self.frame, Ordering::SeqCst);

        // 1) 轮询托盘事件（poll 语义不变：没有事件返回 None）。
        if let Some(tray) = self.tray.as_ref() {
            while let Some(event) = tray.poll() {
                self.event_count += 1;
                logln(&self.log_path, &format!("[probe] 收到托盘事件：{event:?}"));
                match event {
                    tray::TrayEvent::Show => {
                        tray.set_tooltip(&format!(
                            "StereoLink 托盘唤醒探针（收到 {} 次 Show）",
                            self.event_count
                        ));
                        if self.no_hide && self.phase == 0 {
                            logln(
                                &self.log_path,
                                &format!(
                                    "[probe] PASS-CALLBACK：可见状态下收到 Show（第 {} 帧），托盘回调与唤醒链路正常",
                                    self.frame
                                ),
                            );
                            self.pass.store(true, Ordering::SeqCst);
                            self.wake_frame = Some(self.frame);
                            self.phase = 2;
                        } else if self.phase == 1 {
                            logln(
                                &self.log_path,
                                &format!(
                                    "[probe] PASS：隐藏/最小化状态下收到 Show，第 {} 帧的 update() 被托盘事件唤醒",
                                    self.frame
                                ),
                            );
                            self.pass.store(true, Ordering::SeqCst);
                            self.wake_frame = Some(self.frame);
                            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                            self.phase = 2;
                        }
                    }
                    tray::TrayEvent::Quit => {
                        logln(&self.log_path, "[probe] 收到 Quit，关闭窗口");
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                }
            }
        }

        // 2) 自证状态机。
        match self.phase {
            0 => {
                if self.frame >= Self::HIDE_AFTER_FRAMES {
                    if self.no_hide {
                        logln(
                            &self.log_path,
                            &format!(
                                "[probe] 第 {} 帧：自检模式不隐藏窗口，等待托盘点击（心跳保持 100 ms）",
                                self.frame
                            ),
                        );
                        self.hidden.store(true, Ordering::SeqCst);
                        ctx.request_repaint_after(Duration::from_millis(100));
                    } else if self.minimize {
                        logln(
                            &self.log_path,
                            &format!(
                                "[probe] 第 {} 帧：最小化窗口（Minimized(true)），之后停止周期重绘",
                                self.frame
                            ),
                        );
                        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                        self.hidden.store(true, Ordering::SeqCst);
                        self.phase = 1;
                        // 关键：这一帧之后再没有 request_repaint_after，主循环进入阻塞等待。
                    } else {
                        logln(
                            &self.log_path,
                            &format!(
                                "[probe] 第 {} 帧：隐藏窗口（Visible(false)），之后停止周期重绘",
                                self.frame
                            ),
                        );
                        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                        self.hidden.store(true, Ordering::SeqCst);
                        self.phase = 1;
                        // 关键：这一帧之后再没有 request_repaint_after，主循环进入阻塞等待。
                    }
                } else {
                    ctx.request_repaint_after(Duration::from_millis(100));
                }
            }
            1 => {
                // 隐藏等待唤醒：故意不请求任何重绘（模拟正式 GUI 隐藏后的省 CPU 行为）。
            }
            2 => {
                if self
                    .wake_frame
                    .is_some_and(|f| self.frame >= f.saturating_add(50))
                {
                    logln(&self.log_path, "[probe] 窗口已恢复显示，探针退出");
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    self.phase = 3;
                } else {
                    ctx.request_repaint_after(Duration::from_millis(100));
                }
            }
            _ => {}
        }

        // 3) 界面（ASCII 文案，避免依赖中文字体）。
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("StereoLink tray wake probe");
            ui.label(format!("phase: {}", self.phase));
            ui.label(format!("update frames: {}", self.frame));
            ui.label(format!("tray events: {}", self.event_count));
            if self.pass.load(Ordering::SeqCst) {
                ui.colored_label(
                    egui::Color32::from_rgb(90, 220, 130),
                    "WAKE: PASS (main loop woken by tray event)",
                );
            } else {
                ui.label("waiting: window hides, then a simulated tray click must wake the loop");
            }
            ui.label(format!("log: {}", self.log_path.display()));
        });
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if let Some(tray) = self.tray.take() {
            tray.remove();
            logln(&self.log_path, "[probe] 托盘图标已移除");
        }
    }
}

/// 同时打印到 stdout 和日志文件，保证无控制台时也有证据。
fn logln(path: &Path, line: &str) {
    println!("{line}");
    use std::io::Write as _;
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{line}");
    }
}

/// 与 `main.rs::app_icon()` 相同的 64×64 图标（深色圆角底 + 青/紫两道声道条）。
/// 复制一份是因为 example 访问不到 bin 里的私有函数；正式 GUI 里直接用 `app_icon()`。
fn probe_icon() -> (Vec<u8>, u32) {
    const N: usize = 64;
    const SS: usize = 3; // 每像素 3×3 超采样，边缘抗锯齿
    let mut rgba = vec![0u8; N * N * 4];

    let sample = |px: f32, py: f32| -> Option<[f32; 3]> {
        // 圆角方块（中心 32,32，半宽 28，圆角 12）
        let (cx, cy, half, radius) = (32.0f32, 32.0f32, 28.0f32, 12.0f32);
        let qx = (px - cx).abs() - (half - radius);
        let qy = (py - cy).abs() - (half - radius);
        let ax = qx.max(0.0);
        let ay = qy.max(0.0);
        let sd = (ax * ax + ay * ay).sqrt() + qx.max(qy).min(0.0) - radius;
        if sd > 0.0 {
            return None;
        }
        if (16.0..=24.0).contains(&px) && (30.0..=46.0).contains(&py) {
            return Some([56.0, 189.0, 248.0]); // 左声道：青
        }
        if (30.0..=38.0).contains(&px) && (20.0..=46.0).contains(&py) {
            return Some([167.0, 139.0, 250.0]); // 右声道：紫
        }
        if (24.0..=30.0).contains(&px) && (36.0..=39.0).contains(&py) {
            return Some([120.0, 200.0, 240.0]); // 连接线
        }
        let t = (py / N as f32).clamp(0.0, 1.0);
        Some([
            30.0 + (11.0 - 30.0) * t,
            34.0 + (13.0 - 34.0) * t,
            43.0 + (17.0 - 43.0) * t,
        ])
    };

    for y in 0..N {
        for x in 0..N {
            let mut acc = [0.0f32; 3];
            let mut hits = 0.0f32;
            for sy in 0..SS {
                for sx in 0..SS {
                    let px = x as f32 + (sx as f32 + 0.5) / SS as f32;
                    let py = y as f32 + (sy as f32 + 0.5) / SS as f32;
                    if let Some(c) = sample(px, py) {
                        acc[0] += c[0];
                        acc[1] += c[1];
                        acc[2] += c[2];
                        hits += 1.0;
                    }
                }
            }
            let total = (SS * SS) as f32;
            let idx = (y * N + x) * 4;
            if hits > 0.0 {
                rgba[idx] = (acc[0] / hits).round().clamp(0.0, 255.0) as u8;
                rgba[idx + 1] = (acc[1] / hits).round().clamp(0.0, 255.0) as u8;
                rgba[idx + 2] = (acc[2] / hits).round().clamp(0.0, 255.0) as u8;
                rgba[idx + 3] = (255.0 * hits / total).round() as u8;
            }
        }
    }

    (rgba, N as u32)
}
