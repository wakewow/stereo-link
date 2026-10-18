//! StereoLink 立体声链 —— 桌面版入口。
//!
//! 一个 exe 里跑两件事：
//! * UI 线程（eframe/egui，OpenGL 渲染）；
//! * 路由引擎线程（WASAPI 回环采集 + 把左右声道分别推给两台蓝牙音箱）。
//!
//! 用户不用开控制台、不用记命令：窗口里点「开始路由」就行。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod audio_default;
mod config;
mod fonts;
mod logbuf;

use eframe::egui;

fn main() -> eframe::Result<()> {
    // 1) 先接日志：正式版没有控制台，日志面板是唯一诊断出口。
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
            .with_icon(std::sync::Arc::new(app_icon())),
        ..Default::default()
    };

    eframe::run_native(
        "StereoLink 立体声链",
        options,
        Box::new(move |cc| Ok(Box::new(app::StereoLinkApp::new(cc, log)))),
    )
}

/// 程序图标：深色圆角底 + 青/紫两道声道条（64×64，纯代码画，不依赖图片资源）。
fn app_icon() -> egui::IconData {
    const N: usize = 64;
    const SS: usize = 3; // 每像素 3×3 超采样，边缘抗锯齿
    let mut rgba = vec![0u8; N * N * 4];

    // 单个采样点的颜色（None = 透明背景）
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

        // 左声道：青色竖条（低）
        if (16.0..=24.0).contains(&px) && (30.0..=46.0).contains(&py) {
            return Some([56.0, 189.0, 248.0]);
        }
        // 右声道：紫色竖条（高）
        if (30.0..=38.0).contains(&px) && (20.0..=46.0).contains(&py) {
            return Some([167.0, 139.0, 250.0]);
        }
        // 中间连接线：把两路连起来（"Link"）
        if (24.0..=30.0).contains(&px) && (36.0..=39.0).contains(&py) {
            return Some([120.0, 200.0, 240.0]);
        }

        // 背景：上浅下深的石墨渐变
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

    egui::IconData {
        rgba,
        width: N as u32,
        height: N as u32,
    }
}
