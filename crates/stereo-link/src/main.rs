//! StereoLink 立体声链 —— 窗口版入口（`stereo-link.exe`）。
//!
//! 一个进程里跑两件事：
//! * UI 线程（eframe/egui，OpenGL 渲染）；
//! * 路由引擎线程（WASAPI 回环采集 + 把左右声道分别推给两台音箱）。
//!
//! 用户不用开控制台、不用记命令：窗口里点「开始路由」就行。
//! 命令行在另一个 exe（`stereo-link-cli.exe`）里，两者共用 `stereo_link` 库。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui;
use stereo_link::{app, logbuf};

fn main() -> eframe::Result<()> {
    // 1) 先接日志：正式版没有控制台，日志面板（+ 落盘文件）是诊断出口。
    let log = logbuf::LogBuffer::new(1000);
    logbuf::install(log.clone());

    // 2) 崩溃也要留痕（否则窗口一闪就没了，什么都不知道）。
    std::panic::set_hook(Box::new(|info| {
        tracing::error!("程序崩溃：{info}");
    }));

    tracing::info!(
        "StereoLink 立体声链启动，版本 {}",
        env!("CARGO_PKG_VERSION")
    );

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("StereoLink 立体声链")
            .with_inner_size([1240.0, 820.0])
            .with_min_inner_size([1040.0, 660.0])
            .with_app_id("stereo-link")
            // 托盘应用：不在任务栏留按钮，这样「关闭 → 缩小至托盘」时窗口彻底从任务栏消失
            // （egui 0.31 没有运行期切换任务栏按钮的命令，只能在建窗时定死）。
            .with_taskbar(false)
            .with_icon(std::sync::Arc::new(stereo_link::app_icon())),
        ..Default::default()
    };

    eframe::run_native(
        "StereoLink 立体声链",
        options,
        Box::new(move |cc| Ok(Box::new(app::StereoLinkApp::new(cc, log)))),
    )
}
