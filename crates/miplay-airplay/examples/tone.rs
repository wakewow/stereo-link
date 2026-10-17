//! PoC：把一段正弦实时推给小米音箱（默认 `192.168.1.21:7000`）。
//!
//! ```text
//! cargo run -p miplay-airplay --example tone -- [HOST[:PORT]] [SECONDS] [选项]
//!
//!   --codec alac|lpcm   音频编码（默认 alac，与 pyatv 在 L16A 上实测成功的路径一致）
//!   --latency-ms N      声明延迟（默认 250，即设备窗口下限；窗口 250~2000）
//!   --freq HZ           正弦频率（默认 1000）
//!   --amplitude F       幅度 0~1（默认 0.12，刻意压小；不改音箱音量）
//!   --volume F          可选：同时把音箱音量设成 0~1（默认不发音量命令，避免留下状态）
//!   --chunk-ms N        喂数据的块大小（默认 10 ms，模拟实时采集）
//! ```
//!
//! 输出的是**客观证据**：RTSP 步骤与状态码、设备给的端口、设备自报的渲染延迟、发出的包数/字节数、
//! 以及延迟预算。人耳确认与真实麦克风测量不在这里做（本机麦克风全静默，见 `research/lan/lan-paths.md`）。

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use miplay_airplay::{
    AirPlaySender, Codec, PcmFormat, SenderConfig, DEVICE_LATENCY_MIN_MS, DEVICE_LATENCY_MAX_MS,
    TARGET_TOTAL_MS,
};

const SAMPLE_RATE: f64 = 44_100.0;
const CHANNELS: u8 = 2;

#[derive(Debug, Clone)]
struct Opts {
    target: SocketAddr,
    seconds: u64,
    codec: Codec,
    latency_ms: u32,
    freq: f64,
    amplitude: f64,
    volume: Option<f32>,
    chunk_ms: u64,
}

impl Default for Opts {
    fn default() -> Self {
        Self {
            target: "192.168.1.21:7000".parse().expect("默认地址"),
            seconds: 5,
            codec: Codec::Alac,
            latency_ms: DEVICE_LATENCY_MIN_MS,
            freq: 1000.0,
            amplitude: 0.12,
            volume: None,
            chunk_ms: 10,
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let opts = parse_args(std::env::args().skip(1))?;
    println!("== miplay-airplay PoC ==");
    println!(
        "目标 {} · {} 秒 · {} · 声明延迟 {} ms · {} Hz · 幅度 {:.2} · 块 {} ms",
        opts.target,
        opts.seconds,
        opts.codec.as_str(),
        opts.latency_ms,
        opts.freq,
        opts.amplitude,
        opts.chunk_ms
    );

    let config = SenderConfig::default()
        .with_latency_ms(opts.latency_ms)
        .with_codec(opts.codec);
    config.validate()?;

    let pcm = PcmFormat::S16LE_44100_STEREO;
    let chunk_frames = (pcm.sample_rate as u64 * opts.chunk_ms / 1000) as usize;

    println!("\n-- 1. 建流（HAP transient 配对 -> SETUP -> RECORD）--");
    let connect_started = Instant::now();
    let mut sender = AirPlaySender::connect(opts.target, config).await?;
    println!("建流耗时 {:?}，状态 {:?}", connect_started.elapsed(), sender.status());

    let info = sender.session_info().clone();
    println!("设备给的端口：data={:?} control={:?}", info.data_port, info.control_port);
    println!(
        "流 id={:?} audio_buffer_size={:?} 设备自报渲染延迟={:?} ms",
        info.stream_id, info.audio_buffer_size, info.arrival_to_render_latency_ms
    );
    println!("RTSP 步骤与状态码：");
    for step in &info.steps {
        println!("  {step}");
    }

    if let Some(v) = opts.volume {
        sender.set_volume(v).await?;
        println!("已把音量设为 {v:.2}");
    }
    let budget = sender.latency_budget();
    println!("延迟预算：{}", budget.summary(true));

    println!("\n-- 2. 实时喂 {} 秒（每 {} ms 一块，模拟采集端）--", opts.seconds, opts.chunk_ms);
    let mut tone = Tone::new(opts.freq, opts.amplitude);
    let feed_started = Instant::now();
    let chunks = opts.seconds * 1000 / opts.chunk_ms;
    for index in 1..=chunks {
        let bytes = tone.chunk_s16le(chunk_frames, CHANNELS);
        sender.send_pcm(&bytes).await?;
        if index % (1000 / opts.chunk_ms.max(1)) == 0 {
            let stats = sender.stats();
            println!(
                "  t={:>4.1}s 包={:<6} 排队={:>5.1} ms 欠载={:<3} 丢字节={}",
                feed_started.elapsed().as_secs_f64(),
                stats.packets_sent,
                pcm.ms_for_bytes(stats.queued_bytes as usize),
                stats.underrun_packets,
                stats.dropped_pcm_bytes
            );
        }
        // 按实时速率喂：这正是产品里 WASAPI 环回采集的形态
        let target = Duration::from_micros(index * opts.chunk_ms * 1000);
        let elapsed = feed_started.elapsed();
        if target > elapsed {
            tokio::time::sleep(target - elapsed).await;
        }
    }
    let feed_elapsed = feed_started.elapsed();

    // 让最后几个包也发出去（否则统计里会少几包）
    tokio::time::sleep(Duration::from_millis(60)).await;
    let stats = sender.stats();

    println!("\n-- 3. 收尾（TEARDOWN）--");
    let stop_started = Instant::now();
    sender.stop().await?;
    println!("收尾耗时 {:?}", stop_started.elapsed());

    println!("\n== 客观证据汇总 ==");
    println!(
        "喂入 {:.2} 秒，实发 {} 包 / {} 帧 / {} 字节，同步包 {}，欠载 {}，丢弃 {} 字节",
        feed_elapsed.as_secs_f64(),
        stats.packets_sent,
        stats.frames_sent,
        stats.bytes_sent,
        stats.syncs_sent,
        stats.underrun_packets,
        stats.dropped_pcm_bytes
    );
    let audio_seconds = stats.frames_sent as f64 / SAMPLE_RATE;
    println!(
        "实发音频时长 {audio_seconds:.2} 秒（与喂入时长的差 = 建流前排队的量）"
    );
    println!(
        "平均发包率 {:.1} 包/秒（理论 {:.1}）",
        stats.packets_sent as f64 / feed_elapsed.as_secs_f64(),
        SAMPLE_RATE / 352.0
    );
    println!("最终延迟预算：{}", budget.summary(true));
    if budget.estimated_total_ms(true) <= TARGET_TOTAL_MS {
        println!(
            "结论：估计 {:.0} ms ≤ 目标 {:.0} ms（其中 {} ms 是设备自报值）",
            budget.estimated_total_ms(true),
            TARGET_TOTAL_MS,
            if budget.device_render_ms.is_some() {
                "含"
            } else {
                "不含"
            }
        );
    } else {
        println!(
            "结论：估计 {:.0} ms 超出目标 {:.0} ms，差 {:+.0} ms",
            budget.estimated_total_ms(true),
            TARGET_TOTAL_MS,
            budget.gap_to_target_ms(true)
        );
    }
    for term in budget.unknown_terms() {
        println!("  未实测项：{term}");
    }
    Ok(())
}

fn parse_args(args: impl Iterator<Item = String>) -> Result<Opts, Box<dyn std::error::Error>> {
    let mut opts = Opts::default();
    let mut positional = 0usize;
    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--codec" => {
                let value = args.next().ok_or("--codec 需要参数")?;
                opts.codec = match value.as_str() {
                    "alac" => Codec::Alac,
                    "lpcm" | "l16" => Codec::Lpcm,
                    other => return Err(format!("未知编码 {other}（alac|lpcm）").into()),
                };
            }
            "--latency-ms" => {
                let value: u32 = args.next().ok_or("--latency-ms 需要参数")?.parse()?;
                if !(DEVICE_LATENCY_MIN_MS..=DEVICE_LATENCY_MAX_MS).contains(&value) {
                    return Err(format!(
                        "延迟必须在 {DEVICE_LATENCY_MIN_MS}~{DEVICE_LATENCY_MAX_MS} ms"
                    )
                    .into());
                }
                opts.latency_ms = value;
            }
            "--freq" => opts.freq = args.next().ok_or("--freq 需要参数")?.parse()?,
            "--amplitude" => opts.amplitude = args.next().ok_or("--amplitude 需要参数")?.parse()?,
            "--volume" => {
                let value: f32 = args.next().ok_or("--volume 需要参数")?.parse()?;
                if !(0.0..=1.0).contains(&value) {
                    return Err("--volume 必须在 0~1".into());
                }
                opts.volume = Some(value);
            }
            "--chunk-ms" => {
                let value: u64 = args.next().ok_or("--chunk-ms 需要参数")?.parse()?;
                if !(1..=1000).contains(&value) {
                    return Err("--chunk-ms 必须在 1~1000".into());
                }
                opts.chunk_ms = value;
            }
            "-h" | "--help" => {
                println!("{}", include_str!("tone_usage.txt"));
                std::process::exit(0);
            }
            other => match positional {
                0 => {
                    opts.target = normalize_target(other)?;
                    positional += 1;
                }
                1 => {
                    opts.seconds = other.parse()?;
                    if !(1..=30).contains(&opts.seconds) {
                        return Err("时长必须在 1~30 秒（测试音要短）".into());
                    }
                    positional += 1;
                }
                _ => return Err(format!("多余的参数 {other}").into()),
            },
        }
    }
    Ok(opts)
}

/// `host` 或 `host:port`；默认端口 7000。
fn normalize_target(value: &str) -> Result<SocketAddr, Box<dyn std::error::Error>> {
    if value.contains(':') {
        Ok(value.parse()?)
    } else {
        Ok(format!("{value}:7000").parse()?)
    }
}

/// 正弦发生器：连续相位，跨块不断音。
#[derive(Debug)]
struct Tone {
    freq: f64,
    amplitude: f64,
    phase: f64,
}

impl Tone {
    fn new(freq: f64, amplitude: f64) -> Self {
        Self {
            freq,
            amplitude,
            phase: 0.0,
        }
    }

    /// 生成 `frames` 帧的 `s16le` 交错立体声。
    fn chunk_s16le(&mut self, frames: usize, channels: u8) -> Vec<u8> {
        let mut out = Vec::with_capacity(frames * usize::from(channels) * 2);
        let step = std::f64::consts::TAU * self.freq / SAMPLE_RATE;
        for _ in 0..frames {
            let value = (self.phase.sin() * self.amplitude * 32_767.0) as i16;
            for _ in 0..channels {
                out.extend_from_slice(&value.to_le_bytes());
            }
            self.phase += step;
            if self.phase > std::f64::consts::TAU {
                self.phase -= std::f64::consts::TAU;
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_target_is_the_measured_speaker() {
        assert_eq!(Opts::default().target.to_string(), "192.168.1.21:7000");
        assert_eq!(Opts::default().latency_ms, 250);
    }

    #[test]
    fn bare_host_gets_the_airplay_port() {
        assert_eq!(
            normalize_target("192.168.1.21").unwrap().to_string(),
            "192.168.1.21:7000"
        );
        assert_eq!(
            normalize_target("192.168.1.21:7001").unwrap().to_string(),
            "192.168.1.21:7001"
        );
    }

    #[test]
    fn parses_positional_and_flags() {
        let opts = parse_args(
            ["192.168.1.21", "3", "--codec", "lpcm", "--latency-ms", "400", "--freq", "440"]
                .iter()
                .map(|s| (*s).to_string()),
        )
        .unwrap();
        assert_eq!(opts.seconds, 3);
        assert_eq!(opts.codec, Codec::Lpcm);
        assert_eq!(opts.latency_ms, 400);
        assert_eq!(opts.freq, 440.0);
    }

    #[test]
    fn rejects_out_of_window_latency_and_long_tests() {
        let args = ["--latency-ms", "2000"].iter().map(|s| (*s).to_string());
        assert!(parse_args(args).is_err(), "2000 不在窗口内");
        let args = ["192.168.1.21", "31"].iter().map(|s| (*s).to_string());
        assert!(parse_args(args).is_err(), "测试音必须短");
    }

    #[test]
    fn tone_is_little_endian_stereo_and_continuous() {
        let mut tone = Tone::new(1000.0, 0.5);
        let bytes = tone.chunk_s16le(4, 2);
        assert_eq!(bytes.len(), 4 * 2 * 2);
        let first = i16::from_le_bytes([bytes[0], bytes[1]]);
        let second = i16::from_le_bytes([bytes[2], bytes[3]]);
        assert_eq!(first, second, "左右声道相同");
        assert_eq!(first, 0, "从相位 0 起，首样本是 0");
        let next = tone.chunk_s16le(1, 2);
        let value = i16::from_le_bytes([next[0], next[1]]);
        assert!(value > 0, "相位连续，第二个样本应当为正，实际 {value}");
    }

    #[test]
    fn chunk_frames_match_the_chunk_length() {
        let chunk_ms = 10u64;
        let frames = (44_100u64 * chunk_ms / 1000) as usize;
        assert_eq!(frames, 441, "10 ms @44.1k");
        assert_eq!(frames * 4, 1764, "字节数");
    }
}
