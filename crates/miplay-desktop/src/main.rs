//! 小米妙播投放面板 —— 桌面版入口。
//!
//! 一个 exe 里同时跑：
//! * UI 线程（eframe/egui，OpenGL 渲染）
//! * 引擎线程（tokio + WASAPI 回环采集 + axum 分发 + 小米云下发）
//!
//! 所以用户不用开控制台、不用记命令，窗口里点「开始投放」就行。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod config;
mod fonts;
mod logbuf;

use eframe::egui;

fn main() -> eframe::Result<()> {
    // 1) 先接日志：GUI 正式版没有控制台，日志面板是唯一诊断出口。
    let log = logbuf::LogBuffer::new(1000);
    logbuf::install(log.clone());

    // 2) 崩溃也要留痕（否则窗口一闪就没了，什么都不知道）。
    std::panic::set_hook(Box::new(|info| {
        tracing::error!("程序崩溃：{info}");
    }));

    tracing::info!(
        "小米妙播投放面板启动，版本 {}",
        env!("CARGO_PKG_VERSION")
    );

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("小米妙播投放面板")
            .with_inner_size([1360.0, 880.0])
            .with_min_inner_size([1100.0, 640.0])
            .with_app_id("miplay-desktop")
            .with_icon(std::sync::Arc::new(app_icon())),
        ..Default::default()
    };

    eframe::run_native(
        "小米妙播投放面板",
        options,
        Box::new(move |cc| Ok(Box::new(app::DesktopApp::new(cc, log)))),
    )
}

/// 程序图标：圆角蓝底 + 白色喇叭 + 两道声波（64×64，纯代码画，不依赖图片资源）。
fn app_icon() -> egui::IconData {
    const N: usize = 64;
    const SS: usize = 3; // 每像素 3×3 超采样，边缘抗锯齿
    let mut rgba = vec![0u8; N * N * 4];

    // 单个采样点的颜色（None = 透明背景）
    let sample = |px: f32, py: f32| -> Option<[f32; 3]> {
        // 圆角方块（中心 32,32，半宽 28，圆角 10）
        let (cx, cy, half, radius) = (32.0f32, 32.0f32, 28.0f32, 10.0f32);
        let qx = (px - cx).abs() - (half - radius);
        let qy = (py - cy).abs() - (half - radius);
        let ax = qx.max(0.0);
        let ay = qy.max(0.0);
        let sd = (ax * ax + ay * ay).sqrt() + qx.max(qy).min(0.0) - radius;
        if sd > 0.0 {
            return None;
        }

        // 喇叭：方形底座 + 向右张开的圆锥
        let cone_half = 8.0 + 0.9 * (px - 25.0).max(0.0);
        let in_box = (14.0..=25.0).contains(&px) && (24.0..=40.0).contains(&py);
        let in_cone = (25.0..=45.0).contains(&px) && (py - cy).abs() <= cone_half;
        if in_box || in_cone {
            return Some([250.0, 250.0, 252.0]);
        }

        // 两道声波弧（以喇叭口右侧为圆心，只画右半边的弧段）
        let (wx, wy) = (30.0f32, 32.0f32);
        let r = ((px - wx).powi(2) + (py - wy).powi(2)).sqrt();
        let ang = (py - wy).atan2(px - wx).abs();
        let in_arc = |need: f32| (r - need).abs() <= 1.6 && ang < 0.72 && px > wx;
        if in_arc(21.0) || in_arc(28.0) {
            return Some([255.0, 255.0, 255.0]);
        }

        // 背景：上浅下深的蓝色渐变
        let t = (py / N as f32).clamp(0.0, 1.0);
        Some([
            30.0 + (13.0 - 30.0) * t,
            136.0 + (71.0 - 136.0) * t,
            229.0 + (161.0 - 229.0) * t,
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

    egui::IconData {
        rgba,
        width: N as u32,
        height: N as u32,
    }
}
