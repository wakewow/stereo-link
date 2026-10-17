//! 诊断探针：量化「小米音箱拉 HTTP WAV 流」时自身的预缓冲深度。
//!
//! 原理：本探针起一个只发**静音 PCM**（全 0）的 WAV 流，然后用 MiNA 云 API 让音箱
//! 来拉这个 URL。音箱听不见任何声音（全 0），但它会按自己的内部缓冲策略**预读**数据：
//!
//! * 若它拉取速率 ≈ 176400 B/s（44.1k/2ch/16bit 实时速率）→ 边播边拉，缓冲浅；
//! * 若它先以远高于实时的速率猛拉一段，再降到实时速率 → 那一段就是它的预缓冲。
//!
//! 用 `累计已发字节 - 实时速率 × 已播时间` 的峰值估算缓冲深度（秒）：
//! `buffer_bytes = max over samples (sent_bytes - BYTES_PER_SECOND * t)`
//!
//! 用法：
//! ```text
//! cargo run -p miplay-server --example probe_speaker_buffer -- <did|device_id> [port] [seconds]
//! ```
//! 注意：会让目标音箱切到这个静音流（安静，不响），结束后调云 API stop；
//! 若 hub 引擎正在投放该设备，请先让它停掉（否则引擎会自动重发抢占）。

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use miplay_core::{MiToken, MinaClient};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::{sleep, timeout};

const SAMPLE_RATE: u32 = 44_100;
const CHANNELS: u16 = 2;
const BYTES_PER_SECOND: f64 = 176_400.0; // 44100 * 2ch * 2B

/// 44 字节标准 WAV 头。数据长度写 0xFFFFFFFF（流式，长度未知）。
fn wav_header() -> [u8; 44] {
    let mut h = [0u8; 44];
    h[0..4].copy_from_slice(b"RIFF");
    h[4..8].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
    h[8..12].copy_from_slice(b"WAVE");
    h[12..16].copy_from_slice(b"fmt ");
    h[16..20].copy_from_slice(&16u32.to_le_bytes());
    h[20..22].copy_from_slice(&1u16.to_le_bytes()); // PCM
    h[22..24].copy_from_slice(&CHANNELS.to_le_bytes());
    h[24..28].copy_from_slice(&SAMPLE_RATE.to_le_bytes());
    h[28..32].copy_from_slice(&(SAMPLE_RATE * CHANNELS as u32 * 2).to_le_bytes());
    h[32..34].copy_from_slice(&(CHANNELS * 2).to_le_bytes());
    h[34..36].copy_from_slice(&16u16.to_le_bytes());
    h[36..40].copy_from_slice(b"data");
    h[40..44].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
    h
}

struct Stats {
    request_line: String,
    t_accept: f64,
    samples: Vec<(f64, u64)>,
    total: u64,
    duration: f64,
}

async fn serve(listener: TcpListener, seconds: f64, base: Instant) -> Result<Stats> {
    let (mut sock, peer) = timeout(Duration::from_secs(20), listener.accept())
        .await
        .context("20s 内音箱没来拉流")??;
    let t_accept = base.elapsed().as_secs_f64();
    println!("[探针] 音箱接入: {peer} (t={t_accept:.3}s)");

    // 读 HTTP 请求头
    let mut req = Vec::new();
    let mut buf = [0u8; 1024];
    let mut request_line = String::new();
    while req.len() < 8192 {
        let n = timeout(Duration::from_secs(5), sock.read(&mut buf))
            .await
            .context("读请求头超时")??
            .max(0);
        if n == 0 {
            break;
        }
        req.extend_from_slice(&buf[..n]);
        if req.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    if let Some(line) = String::from_utf8_lossy(&req).lines().next() {
        request_line = line.trim().to_string();
    }
    println!("[探针] 请求行: {request_line}");

    sock.write_all(
        b"HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nCache-Control: no-cache\r\n\
          Connection: close\r\nTransfer-Encoding: chunked\r\n\r\n",
    )
    .await?;

    let hdr = wav_header();
    let chunk_fmt = |len: usize, out: &mut Vec<u8>| {
        out.clear();
        out.extend_from_slice(format!("{len:X}\r\n").as_bytes());
        out.extend_from_slice(&hdr);
        out.extend_from_slice(b"\r\n");
    };
    let mut framed = Vec::with_capacity(64);
    chunk_fmt(44, &mut framed);
    sock.write_all(&framed).await?;

    // 静音 PCM：每次 16 KiB = 4096 帧 = 92.9 ms
    let zeros = vec![0u8; 16 * 1024];
    let mut samples: Vec<(f64, u64)> = Vec::new();
    let mut total: u64 = 0;
    let deadline = base.elapsed().as_secs_f64() + seconds;
    samples.push((base.elapsed().as_secs_f64(), 0));
    loop {
        let elapsed = base.elapsed().as_secs_f64();
        if elapsed >= deadline {
            break;
        }
        let mut out = Vec::with_capacity(zeros.len() + 16);
        out.extend_from_slice(format!("{:X}\r\n", zeros.len()).as_bytes());
        out.extend_from_slice(&zeros);
        out.extend_from_slice(b"\r\n");
        match timeout(Duration::from_secs(5), sock.write_all(&out)).await {
            Ok(Ok(())) => {
                total += zeros.len() as u64;
                samples.push((base.elapsed().as_secs_f64(), total));
            }
            Ok(Err(e)) => {
                println!("[探针] 写入失败（音箱断开）: {e}");
                break;
            }
            Err(_) => {
                println!("[探针] 写入阻塞 >5s：音箱不再读（缓冲满且未继续播放？）");
                break;
            }
        }
    }
    let _ = sock.write_all(b"0\r\n\r\n").await;
    let _ = sock.shutdown().await;
    let duration = base.elapsed().as_secs_f64() - t_accept;
    println!("[探针] 传输结束: 共 {total} B ({:.2} s 音频), 连接存活 {duration:.2}s", total as f64 / BYTES_PER_SECOND);
    Ok(Stats { request_line, t_accept, samples, total, duration })
}

fn analyze(stats: &Stats, t_play: f64) {
    println!("\n===== 分析 =====");
    println!("首字节延迟（play_on 返回 → 音箱首次 GET）: {:.0} ms", (stats.t_accept - t_play) * 1000.0);

    // 0.5s 网格采样
    println!("\n时间(s)  累计发送(B)  瞬时速率(B/s)  相对实时倍率");
    let mut last = (0.0f64, 0u64);
    let mut t = 0.5f64;
    while t <= stats.duration + 0.001 {
        // 找最接近 t 的样本
        let pos = stats.samples.iter().position(|(s, _)| *s >= t);
        let Some(idx) = pos else { break };
        let (ts, bytes) = stats.samples[idx];
        let dt = ts - last.0;
        let db = bytes as f64 - last.1 as f64;
        let rate = if dt > 0.0 { db / dt } else { 0.0 };
        println!("{:>6.2}  {:>12}  {:>12.0}  {:>8.2}x", ts, bytes, rate, rate / BYTES_PER_SECOND);
        last = (ts, bytes);
        t += 0.5;
    }

    // 缓冲深度 = max(sent - realtime*t)，只在播放已开始的区间内统计
    let mut max_buffer = 0.0f64;
    let mut max_at = 0.0f64;
    for (ts, bytes) in &stats.samples {
        if *ts < t_play {
            continue;
        }
        let played = BYTES_PER_SECOND * (ts - t_play);
        let buffered = *bytes as f64 - played;
        if buffered > max_buffer {
            max_buffer = buffered;
            max_at = *ts - t_play;
        }
    }
    let tail_rate = {
        let n = stats.samples.len();
        if n >= 4 {
            let (t1, b1) = stats.samples[n - 3];
            let (t2, b2) = stats.samples[n - 1];
            if t2 > t1 { (b2 - b1) as f64 / (t2 - t1) } else { 0.0 }
        } else {
            0.0
        }
    };
    println!("\n峰值预缓冲字节: {:.0} B（在播放开始后 {:.2}s 达到）", max_buffer, max_at);
    println!("= 音箱侧预缓冲约 {:.2} s（含 TCP/OS 缓冲误差，通常 <0.2s）", max_buffer / BYTES_PER_SECOND);
    println!("尾部拉取速率: {:.0} B/s = {:.2}x 实时", tail_rate, tail_rate / BYTES_PER_SECOND);
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let key = args.next().unwrap_or_else(|| "931892755".to_string());
    let port: u16 = args.next().and_then(|s| s.parse().ok()).unwrap_or(8932);
    let seconds: f64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(12.0);

    let token = MiToken::load_default().context("加载小米 token 失败")?;
    let client = MinaClient::new(token)?;
    let devices = client.device_list().await?;
    let dev = devices
        .iter()
        .find(|d| d.did == key || d.device_id == key)
        .with_context(|| format!("设备列表里找不到 {key}"))?;
    println!(
        "目标设备: {} (did={} device_id={}) online={}",
        dev.label(),
        dev.did,
        dev.device_id,
        dev.online()
    );

    let ip = {
        let s = std::net::UdpSocket::bind("0.0.0.0:0")?;
        s.connect("8.8.8.8:80")?;
        s.local_addr()?.ip()
    };
    let url = format!("http://{ip}:{port}/probe.wav");
    println!("探针 URL: {url}");

    let base = Instant::now();
    let listener = TcpListener::bind(("0.0.0.0", port)).await?;
    println!("[探针] 监听 {ip}:{port}，等待音箱来拉流……");
    let srv = tokio::spawn(serve(listener, seconds, base));

    sleep(Duration::from_millis(200)).await;
    let t_play = base.elapsed().as_secs_f64();
    client
        .play_on(dev, &url)
        .await
        .context("云 API play_on 失败")?;
    println!("[探针] play_on 已下发 (t={t_play:.3}s)");

    let stats = srv.await??;
    analyze(&stats, t_play);

    client
        .stop(&dev.device_id)
        .await
        .context("云 API stop 失败")?;
    println!("\n[探针] 已调云 API stop，目标音箱退出诊断流。");
    Ok(())
}
