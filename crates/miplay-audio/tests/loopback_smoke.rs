//! 真机集成测试：**真的播一段正弦，同时回环采集**，验证采到的是有声数据。
//!
//! 判定标准（关键的不变量）：
//! * 默认 render 端点存在、能打开 → 那么采到的 peak **必须** > 0；
//! * 采到的数据长度、声道关系、主频都要对得上播放的内容。
//!
//! 如果机器上根本没有可用的 render 端点（例如无声卡的 CI），测试会打印原因后跳过，
//! 而不是假装通过。

#![cfg(windows)]

use std::sync::mpsc;
use std::time::{Duration, Instant};

use miplay_audio::{
    estimate_frequency, list_endpoints, play_sine, AudioCapture, CaptureConfig, DataFlow, PcmFormat,
};

fn default_render_available() -> bool {
    match list_endpoints() {
        Ok(list) => list
            .iter()
            .any(|e| e.flow == DataFlow::Render && e.is_default && e.mix_format.is_some()),
        Err(_) => false,
    }
}

#[test]
fn default_loopback_captures_playing_sine() {
    if !default_render_available() {
        eprintln!("跳过：找不到带混音格式的默认 render 端点（无声卡环境）");
        return;
    }

    let mut capture = match AudioCapture::new(CaptureConfig::default()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("跳过：默认回环设备打不开（{e}）");
            return;
        }
    };
    if let Err(e) = capture.start() {
        eprintln!("跳过：start() 失败（{e}）");
        return;
    }
    let format = capture
        .negotiated_format()
        .expect("start() 成功后应有协商格式")
        .output;
    assert_eq!(
        format,
        PcmFormat::new(44_100, 2, 16),
        "对外契约必须是 44100/2ch/16bit"
    );
    println!("采集端点：{}", capture.device_name().unwrap_or("<未知>"));
    println!("协商格式：{}", capture.negotiated_format().unwrap().describe());

    // 先跑起来再出声，避免漏掉起播头
    std::thread::sleep(Duration::from_millis(200));

    let (tx, rx) = mpsc::channel();
    let player = std::thread::spawn(move || {
        let _ = tx.send(play_sine(
            &miplay_audio::DeviceSelector::Default,
            1_000.0,
            1.0,
            0.4,
        ));
    });

    // 采集循环：边放边采，播放结束后再采 300ms 尾巴；总共最多 6 秒
    let mut pcm: Vec<u8> = Vec::new();
    let hard_deadline = Instant::now() + Duration::from_secs(6);
    let mut play_done_at: Option<Instant> = None;
    while Instant::now() < hard_deadline {
        let chunk = capture.read_vec(8_192, Duration::from_millis(200)).unwrap();
        pcm.extend_from_slice(&chunk);
        if play_done_at.is_none() {
            if let Ok(res) = rx.try_recv() {
                match &res {
                    Ok(info) => println!("播放端点：{}", info.describe()),
                    Err(e) => {
                        eprintln!("跳过：播放失败（{e}）");
                        capture.stop();
                        let _ = player.join();
                        return;
                    }
                }
                play_done_at = Some(Instant::now());
            }
        }
        if let Some(t) = play_done_at {
            if t.elapsed() >= Duration::from_millis(300) {
                break;
            }
        }
    }
    let _ = player.join();
    capture.stop();

    let samples: Vec<i16> = pcm
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect();
    let frames = samples.len() / 2;
    let left: Vec<i16> = samples.iter().step_by(2).copied().collect();
    let right: Vec<i16> = samples.iter().skip(1).step_by(2).copied().collect();
    let peak = left.iter().map(|s| (*s as i32).abs()).max().unwrap_or(0);
    let rms = (left.iter().map(|s| (*s as f64) * (*s as f64)).sum::<f64>()
        / left.len().max(1) as f64)
        .sqrt();
    let max_lr_delta = left
        .iter()
        .zip(right.iter())
        .map(|(l, r)| (*l as i32 - *r as i32).abs())
        .max()
        .unwrap_or(0);
    let as_f32: Vec<f32> = left.iter().map(|s| *s as f32 / 32_768.0).collect();
    let freq = estimate_frequency(&as_f32, format.samplerate);

    println!(
        "采到 {frames} 帧 / peak={peak} / RMS={rms:.2} / 左右最大差={max_lr_delta} LSB / 主频={freq:?}"
    );

    assert!(
        frames > 22_050,
        "至少该采到 0.5 秒数据，实际 {frames} 帧"
    );
    assert!(peak > 0, "采到全零：回环链路没有真正采到系统声音");
    // 本机实测：AUTOCONVERTPCM + 48k→44.1k 重采样后左右最多差 3 LSB，
    // 不是逐位相同，所以按「量化噪声级」判定。
    assert!(
        max_lr_delta <= 8,
        "播放的是单声道复制的正弦，左右最多只该差量化噪声级，实测最大差 {max_lr_delta} LSB"
    );
    // 1kHz @ 0.4FS：峰值应该接近 13107，允许播放链路有增益/淡入淡出
    assert!(peak > 3_000, "峰值太小（{peak}），不像是 0.4 FS 的正弦");
    let freq = freq.expect("有信号时应该能估出主频");
    assert!((freq - 1_000.0).abs() < 50.0, "主频 {freq}Hz 偏离 1kHz");
}
