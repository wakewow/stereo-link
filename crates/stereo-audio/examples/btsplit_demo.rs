//! 蓝牙左右分离真机自检：把一段「左/右/双声道」测试音送到虚拟声卡，
//! 由 [`stereo_audio::BtSplitHandle`] 采下来后分别推给两台蓝牙音箱。
//!
//! 用法（先确认两台音箱已通过蓝牙连到本机）：
//! ```powershell
//! cargo run -p stereo-audio --example btsplit_demo -- <左端点名子串> <右端点名子串>
//! ```
//! 默认左 `Xiaomi Sound-1695`、右 `Xiaomi Sound-5320`，测试音 12 秒：
//! 0–3s 只有左声道 440Hz；3–6s 只有右声道 880Hz；6–9s 两边各一路；9–12s 两边同一 660Hz（听声像是否居中）。

use std::thread;
use std::time::{Duration, Instant};

use stereo_audio::{
    play_pcm_s16, BtSplitConfig, BtSplitHandle, DeviceSelector, PcmFormat,
};

const RATE: u32 = 44_100;

fn tone_pcm(seconds: f64, phase: fn(f64) -> (f32, f32)) -> Vec<u8> {
    let frames = (seconds * RATE as f64) as usize;
    let mut out = Vec::with_capacity(frames * 4);
    for i in 0..frames {
        let t = i as f64 / RATE as f64;
        let (l, r) = phase(t);
        let l = (l.clamp(-1.0, 1.0) * 0.45 * i16::MAX as f32) as i16;
        let r = (r.clamp(-1.0, 1.0) * 0.45 * i16::MAX as f32) as i16;
        out.extend_from_slice(&l.to_le_bytes());
        out.extend_from_slice(&r.to_le_bytes());
    }
    out
}

fn sine(hz: f64, t: f64) -> f32 {
    (2.0 * std::f64::consts::PI * hz * t).sin() as f32
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let left = args
        .first()
        .cloned()
        .unwrap_or_else(|| "Xiaomi Sound-1695".to_owned());
    let right = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| "Xiaomi Sound-5320".to_owned());

    let config = BtSplitConfig {
        left_endpoint: left.clone(),
        right_endpoint: right.clone(),
        ..Default::default()
    };
    config.validate()?;
    println!("== 蓝牙左右分离自检 ==");
    println!("配置: {}", config.describe());
    println!("软件侧缓冲估计: {} ms（不含蓝牙链路本身）", config.software_latency_ms());

    let source = config.source_endpoint.clone();
    let mut split = BtSplitHandle::start(config)?;
    println!("分离输出已启动，稍等 300 ms 让两条链路起流…");
    thread::sleep(Duration::from_millis(300));

    // 12 秒测试音，分段构造。
    let mut pcm = Vec::new();
    pcm.extend(tone_pcm(3.0, |t| (sine(440.0, t), 0.0)));
    pcm.extend(tone_pcm(3.0, |t| (0.0, sine(880.0, t))));
    pcm.extend(tone_pcm(3.0, |t| (sine(440.0, t), sine(880.0, t))));
    pcm.extend(tone_pcm(3.0, |t| (sine(660.0, t), sine(660.0, t))));

    let player = thread::spawn(move || {
        play_pcm_s16(
            &DeviceSelector::name_contains(source),
            PcmFormat::new(RATE, 2, 16),
            &pcm,
        )
    });

    println!("测试音时间表：0–3s 左箱 440Hz | 3–6s 右箱 880Hz | 6–9s 两箱各一路 | 9–12s 两箱同 660Hz（应听成居中）");

    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(14) {
        thread::sleep(Duration::from_secs(1));
        let stats = split.stats();
        println!(
            "[{:4.1}s] {}",
            started.elapsed().as_secs_f64(),
            stats.describe()
        );
        if let Some(err) = &stats.source_error {
            eprintln!("采集故障: {err}");
            break;
        }
        if !stats.running {
            eprintln!("分离输出已停止");
            break;
        }
    }

    match player.join() {
        Ok(Ok(info)) => println!("测试音播放完成: {}", info.describe()),
        Ok(Err(e)) => println!("测试音播放失败: {e}"),
        Err(_) => println!("测试音线程 panic"),
    }
    split.stop();
    println!("== 自检结束 ==");
    Ok(())
}
