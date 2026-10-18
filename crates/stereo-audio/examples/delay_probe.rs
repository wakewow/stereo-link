//! 用「数字回环」验证链路的额外延迟真的生效 —— **不需要麦克风**。
//!
//! 链路：点击串 → 源端点（虚拟声卡）→ 路由引擎回环采集 → 按 `delay_ms` 延迟
//! → 目标端点 → 再回环采集目标端点，量出点击的到达时间。
//!
//! 跑几档延迟（默认 0 / 120 / 250 ms），到达时间的差值应当 ≈ 设定延迟。
//! 这个测试完全在数字域里，不依赖音箱、不依赖麦克风，也不需要人耳。
//!
//! ```text
//! cargo run -p stereo-audio --example delay_probe
//! cargo run -p stereo-audio --example delay_probe -- --source 立体声链虚拟声卡 --dest "Steam Streaming Microphone" --delay 250
//! ```

use std::sync::mpsc;
use std::time::{Duration, Instant};

use stereo_audio::{
    build_sine_s16, play_pcm_s16, selector_for, AudioCapture, CaptureConfig, CaptureMode,
    PcmFormat, RouteChannel, RouteConfig, RouteEngineConfig, RouteEngineHandle,
};

const RATE: u32 = 44_100;
const FRAME_BYTES: usize = 4;

fn arg(args: &[String], key: &str) -> Option<String> {
    args.iter()
        .position(|a| a == key)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

/// 3 声 8ms 的 1kHz 点击，间隔 60ms —— 便于回环里一眼看出起始沿。
fn click_train() -> Vec<u8> {
    let format = PcmFormat::new(RATE, 2, 16);
    let burst = build_sine_s16(1_000.0, 0.008, 0.8, format);
    let gap = vec![0u8; (RATE as f64 * 0.06) as usize * FRAME_BYTES];
    let mut pcm = Vec::new();
    for _ in 0..3 {
        pcm.extend_from_slice(&burst);
        pcm.extend_from_slice(&gap);
    }
    pcm.extend_from_slice(&vec![0u8; (RATE as f64 * 0.3) as usize * FRAME_BYTES]);
    pcm
}

fn peak_s16(bytes: &[u8]) -> f32 {
    let mut peak = 0.0_f32;
    for pair in bytes.as_chunks::<2>().0 {
        let v = i16::from_le_bytes(*pair) as f32 / 32_768.0;
        if v.abs() > peak {
            peak = v.abs();
        }
    }
    peak
}

/// 采集某个端点的回环。`spec` 既可以是名字子串，也可以是 `{...}` 形状的端点 id
/// （与引擎的采集源解析保持一致）。
fn loopback(spec: &str) -> anyhow::Result<AudioCapture> {
    let mut config = CaptureConfig::default()
        .with_mode(CaptureMode::Loopback)
        .with_samplerate(RATE)
        .with_channels(2);
    config.device = selector_for(spec);
    config.buffer_ms = 20;
    Ok(AudioCapture::new(config)?)
}

/// 跑一档延迟，返回点击到达时间（秒，相对开始播放）。
fn run_once(source: &str, dest: &str, delay_ms: u32) -> anyhow::Result<Option<f64>> {
    let config = RouteEngineConfig {
        source_endpoint: source.to_owned(),
        routes: vec![RouteConfig {
            delay_ms,
            ..RouteConfig::new(dest, RouteChannel::Both)
        }],
        ..Default::default()
    };
    config.validate()?;
    let mut engine = RouteEngineHandle::start(config)?;
    // 等采集/渲染线程把流拉起来。
    std::thread::sleep(Duration::from_millis(800));

    // 同时采源端点（引擎的输入）与目标端点（引擎的输出），好把「引擎引入的延迟」
    // 从「点击本身什么时候发生」里分离出来。
    let mut source_capture = loopback(source)?;
    source_capture.start()?;
    let mut capture = loopback(dest)?;
    capture.start()?;
    std::thread::sleep(Duration::from_millis(200));

    let pcm = click_train();
    let source_needle = source.to_owned();
    let (tx, rx) = mpsc::channel();
    let t0 = Instant::now();
    let player = std::thread::spawn(move || {
        let info = play_pcm_s16(
            &selector_for(&source_needle),
            PcmFormat::new(RATE, 2, 16),
            &pcm,
        );
        let _ = tx.send(info.map(|i| i.describe()).map_err(|e| e.to_string()));
    });

    let mut buf = vec![0u8; 16 * 1024];
    let mut source_buf = vec![0u8; 16 * 1024];
    // (读到的时刻, 峰值)：读超时压到 10ms，时间分辨率就够分辨 120/250ms 的差异。
    let mut peaks: Vec<(f64, f32)> = Vec::new();
    let mut source_peak = 0.0_f32;
    let window = Duration::from_millis(2_600);
    while t0.elapsed() < window {
        let at = t0.elapsed().as_secs_f64();
        let n = capture.read(&mut buf, Duration::from_millis(10))?;
        if n > 0 {
            peaks.push((at, peak_s16(&buf[..n])));
        }
        let n = source_capture.read(&mut source_buf, Duration::from_millis(10))?;
        if n > 0 {
            let p = peak_s16(&source_buf[..n]);
            if p > source_peak {
                source_peak = p;
            }
        }
    }
    let max_peak = peaks.iter().map(|(_, p)| *p).fold(0.0_f32, f32::max);
    // 端点音量会压低回环电平，所以用「相对自己峰值的一半」判起始沿，而不是绝对阈值。
    let threshold = (max_peak * 0.5).max(0.001);
    let onset = peaks.iter().find(|(_, p)| *p > threshold).map(|(t, _)| *t);
    println!("  回环峰值：源 {source_peak:.4} · 目标 {max_peak:.4}（判沿阈值 {threshold:.4}）");
    source_capture.stop();
    capture.stop();
    let played = rx.recv_timeout(Duration::from_secs(5));
    let _ = player.join();
    match played {
        Ok(Ok(info)) => println!("  播放：{info}"),
        Ok(Err(e)) => println!("  播放失败：{e}"),
        Err(_) => println!("  播放线程没有回报（超时）"),
    }
    // 诊断：引擎到底有没有把数据写到目标端点。
    let stats = engine.stats();
    println!(
        "  采集源：{}（{}）· 采集帧 {}",
        stats.source_device,
        if stats.running { "运行中" } else { "已停" },
        stats.captured_frames
    );
    if let Some(e) = &stats.source_error {
        println!("  采集错误：{e}");
    }
    for route in &stats.routes {
        println!(
            "  路由「{}」{:?}：已发 {} 字节 · 欠载 {} · 排队 {:.0} ms · 错误 {:?} · 路径：{}",
            route.endpoint,
            route.channel,
            route.sent_bytes,
            route.underruns,
            route.queued_ms,
            route.error,
            route.format_path.as_deref().unwrap_or("<未开流>")
        );
    }
    println!("  采集端：{}", capture.device_name().unwrap_or("<未知>"));
    engine.stop();
    Ok(onset)
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let source = arg(&args, "--source").unwrap_or_else(|| "立体声链虚拟声卡".to_owned());
    let dest = arg(&args, "--dest").unwrap_or_else(|| "Steam Streaming Microphone".to_owned());
    let delays: Vec<u32> = match arg(&args, "--delay") {
        Some(v) => vec![v.parse()?],
        None => vec![0, 120, 250],
    };

    println!("源端点（回环采集）：{source}");
    println!("目标端点（写 + 回环采集）：{dest}");
    println!("延迟档位：{delays:?} ms\n");

    let mut results: Vec<(u32, Option<f64>)> = Vec::new();
    for delay in delays {
        println!("—— delay_ms = {delay} ——");
        let onset = run_once(&source, &dest, delay)?;
        match onset {
            Some(t) => println!("  到达时间：{:.1} ms\n", t * 1000.0),
            None => println!("  没抓到点击（目标端点没出声？）\n"),
        }
        results.push((delay, onset));
        std::thread::sleep(Duration::from_millis(300));
    }

    println!("==== 结论 ====");
    let base = results.first().and_then(|(_, o)| *o);
    for (delay, onset) in &results {
        match (base, onset) {
            (Some(_), Some(t)) if *delay == 0 => {
                println!("delay {delay:>4} ms → {:.1} ms（基准）", t * 1000.0);
            }
            (Some(b), Some(t)) => {
                let delta = (t - b) * 1000.0;
                let expect = *delay as f64;
                let verdict = if (delta - expect).abs() < 25.0 {
                    "✅ 与设定值一致"
                } else {
                    "❌ 与设定值不符"
                };
                println!(
                    "delay {delay:>4} ms → {:.1} ms（比基准晚 {:.1} ms，期望 {:.0} ms）{verdict}",
                    t * 1000.0,
                    delta,
                    expect
                );
            }
            _ => println!("delay {delay:>4} ms → 没测到"),
        }
    }
    Ok(())
}
