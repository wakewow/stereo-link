//! **低延迟通路实测**：把 Windows 系统声音（WASAPI 回环）**直接**推给支持 AirPlay 2 的音箱。
//!
//! 这条路径完全绕开小米云：本机 → 音箱 `TCP/7000`（RTSP 控制）+ `UDP`（RTP/ALAC 音频），
//! 声明延迟 250 ms（设备窗口下限，`L16A` 实测接受范围 250~2000 ms）。
//!
//! 用途：验证「看视频/打游戏时音频是否跟得上画面」。开着它去播视频，用耳朵判断唇音同步。
//!
//! ```text
//! cargo run -p miplay-airplay --example loopback -- [HOST[:PORT]] [SECONDS]
//! cargo run -p miplay-airplay --example loopback -- 192.168.1.21:7000 120
//! ```
//!
//! 注意：
//! * 需要**入站 UDP** 防火墙放行本程序（AirPlay 时序包），否则卡在 StreamSetup 超时；
//! * 推流会**接管**音箱播放（AirPlay 固有行为），结束后 TEARDOWN 不留会话；
//! * 卧室「立体声组合」整组响应：推 `192.168.1.21`（L16A）即可，左右声道自动分发。

use anyhow::{Context, Result};
use miplay_airplay::{AirPlaySender, Codec, SenderConfig};
use miplay_audio::{AudioCapture, CaptureConfig, CaptureMode, DeviceSelector};
use std::time::{Duration, Instant};

const VAD_HINT: &str = "小米妙播";
const CHUNK_BYTES: usize = 1_764; // 10 ms @ 44100/2ch/s16

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let host = args
        .next()
        .unwrap_or_else(|| "192.168.1.21:7000".to_string());
    let seconds: f64 = args
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(120.0);
    let addr = host
        .parse()
        .with_context(|| format!("目标地址不合法：{host}"))?;

    println!("== AirPlay 低延迟通路实测 ==");
    println!("目标 {host} · {seconds:.0} 秒 · 声明延迟 250 ms · ALAC");

    let cfg = CaptureConfig {
        device: DeviceSelector::name_contains(VAD_HINT),
        samplerate: 44_100,
        channels: 2,
        mode: CaptureMode::Loopback,
        ring_seconds: 2.0,
        buffer_ms: 100,
    };
    let mut cap = AudioCapture::new(cfg)?;
    cap.start()?;
    println!("采集端点：{:?}", cap.device_name());

    let conf = SenderConfig::new("miplay-bridge-lowlat")
        .with_latency_ms(250)
        .with_codec(Codec::Alac);
    let mut tx = AirPlaySender::connect(addr, conf)
        .await
        .context("AirPlay 建流失败")?;
    println!("建流成功，状态 {:?}", tx.status());
    let budget = tx.latency_budget();
    println!("延迟预算：{}", budget.summary(true));
    println!("\n>>> 现在开始推流 {seconds:.0} 秒：去播一段视频，听唇音是否同步。\n");

    let start = Instant::now();
    let mut buf = vec![0u8; CHUNK_BYTES];
    let mut last_report = Instant::now();
    let mut sent = 0usize;
    while start.elapsed() < Duration::from_secs_f64(seconds) {
        let n = cap.read(&mut buf, Duration::from_millis(50))?;
        if n == 0 {
            continue;
        }
        tx.send_pcm(&buf[..n]).await?;
        sent += n;
        if last_report.elapsed() >= Duration::from_secs(2) {
            let st = tx.stats();
            println!(
                "  t={:.1}s 包={} 帧={} 欠载={} 排队={}ms 丢字节={}",
                start.elapsed().as_secs_f64(),
                st.packets_sent,
                st.frames_sent,
                st.underrun_packets,
                st.queued_bytes / 176,
                st.dropped_pcm_bytes
            );
            last_report = Instant::now();
        }
    }

    println!("\n收尾：TEARDOWN …");
    tx.stop().await?;
    cap.stop();
    println!(
        "结束。推入 {} 字节 ≈ {:.1} 秒音频",
        sent,
        sent as f64 / 176_400.0
    );
    Ok(())
}
