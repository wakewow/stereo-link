//! 端到端**声学**延迟探针。
//!
//! 思路：同时采集两路声音——
//!   ① `扬声器 (小米妙播设备)` 的 WASAPI 回环 = 我们"送进链路"的原始音频（源）；
//!   ② 麦克风 = 音箱真正从空气里发出来的声音（终点）。
//! 播放一段已知的 1.2 kHz click 序列，量出两路里 click 起点的**时间差**：
//!   回环 click 起点 → 麦克风 click 起点 = 网络 + 音箱内部缓冲 + 声传播（约 3~10 ms）。
//!
//! 这一步把"延迟到底耗在哪"钉死：如果回环→麦克风只有 0.2 s，而人耳感觉 0.6~1.5 s，
//! 差额就来自播放器/浏览器自己的音频缓冲；如果这里就是 1 s 以上，则瓶颈在链路+音箱。
//!
//! 用法：
//! ```text
//! cargo run -p miplay-server --example probe_acoustic
//! ```
//! 要求：引擎正在投放（音箱已连上），房间里相对安静，麦克风没被静音。

use anyhow::{anyhow, Result};
use miplay_audio::{
    list_endpoints, play_pcm_s16, play_sine, AudioCapture, CaptureConfig, CaptureMode, DataFlow,
    DeviceSelector, PcmFormat,
};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const SR: u32 = 44_100;
const CH: u16 = 2;
const VAD_HINT: &str = "小米妙播";
const CLICK_HZ: f64 = 1_200.0;
const LEAD_S: f64 = 0.35;
const CLICK_S: f64 = 0.09;
const GAP_S: f64 = 0.80;
const CLICKS: usize = 5;

fn main() -> Result<()> {
    println!("=== 端到端声学延迟探针 ===");
    let endpoints = list_endpoints()?;
    let vad = endpoints
        .iter()
        .find(|e| e.flow == DataFlow::Render && e.name.contains(VAD_HINT))
        .ok_or_else(|| anyhow!("找不到虚拟声卡（名字含 {VAD_HINT}）"))?
        .clone();
    println!("[源] 回环端点：{}", vad.describe());

    // ---- 1. 挑一个真能拾到房间声音的麦克风 ----
    let vads = vec![vad.name.clone()];
    println!("\n=== 第 1 步：找能拾音的麦克风（播放 1 kHz 短音，测 RMS）===");
    let mut best: Option<(String, String, f64)> = None;
    for e in endpoints.iter().filter(|e| e.flow == DataFlow::Capture) {
        if e.error.is_some() || e.name.contains("Steam Streaming") {
            continue;
        }
        let rms = match mic_rms(e.id.clone()) {
            Ok(v) => v,
            Err(err) => {
                println!("  [跳过] {}：{err}", e.name);
                continue;
            }
        };
        println!("  [候选] {} : RMS={rms:.0}", e.name);
        if best.as_ref().map(|(_, _, b)| rms > *b).unwrap_or(true) {
            best = Some((e.id.clone(), e.name.clone(), rms));
        }
    }
    let (mic_id, mic_name, mic_rms_level) =
        best.ok_or_else(|| anyhow!("没有可用的采集端点（麦克风全打不开）"))?;
    println!("\n[终点] 选用麦克风：{mic_name}（RMS={mic_rms_level:.0}）");
    if mic_rms_level < 30.0 {
        println!("  ⚠ 拾音很弱：音箱可能不在附近、麦克风静音或增益太低，后面的延迟数会不可靠。");
    }
    let _ = vads;

    // ---- 2. 同时录回环 + 麦克风，播放 click 序列 ----
    println!("\n=== 第 2 步：同时录 回环 + 麦克风，播放 {CLICKS} 个 click ===");
    let mut loop_cap = build_cap(DeviceSelector::name_contains(VAD_HINT), CaptureMode::Loopback)?;
    let mut mic_cap = build_cap(DeviceSelector::id(mic_id.clone()), CaptureMode::Capture)?;
    loop_cap.start()?;
    mic_cap.start()?;
    println!("  loopback: {:?}", loop_cap.device_name());
    println!("  mic     : {:?}", mic_cap.device_name());
    let t0 = Instant::now();
    std::thread::sleep(Duration::from_millis(150));

    let (pcm, click_offsets) = build_click_pattern();
    let (tx, rx) = mpsc::channel::<Instant>();
    let player = {
        let vad_name = vad.name.clone();
        std::thread::spawn(move || -> Result<miplay_audio::PlaybackInfo> {
            let sel = DeviceSelector::name_contains(vad_name);
            let t_play = Instant::now();
            let _ = tx.send(t_play);
            play_pcm_s16(&sel, PcmFormat::new(SR, CH, 16), &pcm).map_err(|e| anyhow!(e))
        })
    };
    let t_play = rx.recv_timeout(Duration::from_secs(5))?;

    let mut lb: Vec<(Instant, Vec<i16>)> = Vec::new();
    let mut mc: Vec<(Instant, Vec<i16>)> = Vec::new();
    let mut buf = vec![0u8; 16_384];
    let total = Duration::from_secs_f64(LEAD_S + CLICKS as f64 * (CLICK_S + GAP_S) + 0.9);
    while t0.elapsed() < total {
        if let Ok(n) = loop_cap.read(&mut buf, Duration::from_millis(2)) {
            if n > 0 {
                lb.push((Instant::now(), to_i16(&buf[..n])));
            }
        }
        if let Ok(n) = mic_cap.read(&mut buf, Duration::from_millis(2)) {
            if n > 0 {
                mc.push((Instant::now(), to_i16(&buf[..n])));
            }
        }
    }
    let info = player.join().map_err(|_| anyhow!("播放线程 panic"))??;
    loop_cap.stop();
    mic_cap.stop();
    println!(
        "  播放完成：{} 帧 / {:.2} s（{}）",
        info.frames,
        info.seconds,
        info.device_name
    );

    // ---- 3. 分析 ----
    println!("\n=== 第 3 步：click 起点对齐 ===");
    let l = analyze("回环", &lb, t0);
    let m = analyze("麦克风", &mc, t0);
    println!(
        "  回环  : 时长 {:.2}s, peak={}, rms={:.0}, 起点 {} 个",
        l.duration,
        l.peak,
        l.rms,
        l.onsets.len()
    );
    println!(
        "  麦克风: 时长 {:.2}s, peak={}, rms={:.0}, 起点 {} 个",
        m.duration,
        m.peak,
        m.rms,
        m.onsets.len()
    );
    if m.onsets.is_empty() || l.onsets.is_empty() {
        println!("\n❌ 至少一路没检测到 click。请确认音箱在放音、麦克风能拾音后重试。");
        return Ok(());
    }
    let t_play_rel = t_play.duration_since(t0).as_secs_f64();
    let mut deltas = Vec::new();
    for (i, &lo) in l.onsets.iter().enumerate().take(CLICKS) {
        let exp = t_play_rel + click_offsets[i];
        let pc = lo - exp;
        // 找离它最近、且在它之后的麦克风起点
        let mic_on = m
            .onsets
            .iter()
            .copied()
            .find(|&mo| mo > lo + 0.05 && mo < lo + 3.0);
        match mic_on {
            Some(mo) => {
                let d = mo - lo;
                deltas.push(d);
                println!(
                    "  click#{i}: 回环 {lo:.3}s（PC 调度 {:+.3}s） → 麦克风 {mo:.3}s  Δ={d:.3}s",
                    pc
                );
            }
            None => println!("  click#{i}: 回环 {lo:.3}s（PC 调度 {pc:+.3}s） → 麦克风未匹配"),
        }
    }
    if !deltas.is_empty() {
        deltas.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let med = deltas[deltas.len() / 2];
        println!(
            "\n>>> 回环→音箱出声（含声传播）：样本 {} 个，最小 {:.3}s，中位 {:.3}s，最大 {:.3}s",
            deltas.len(),
            deltas[0],
            med,
            deltas[deltas.len() - 1]
        );
        println!("    这个数字 ≈ 网络传输 + 音箱内部缓冲；人耳感知的延迟 = 本数字 + 播放器自身缓冲。");
    }
    Ok(())
}

fn build_cap(device: DeviceSelector, mode: CaptureMode) -> Result<AudioCapture> {
    let cfg = CaptureConfig {
        device,
        samplerate: SR,
        channels: CH,
        mode,
        ring_seconds: 2.0,
        buffer_ms: 100,
    };
    Ok(AudioCapture::new(cfg)?)
}

fn to_i16(bytes: &[u8]) -> Vec<i16> {
    bytes
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect()
}

fn mic_rms(id: String) -> Result<f64> {
    let mut cap = build_cap(DeviceSelector::id(id), CaptureMode::Capture)?;
    cap.start()?;
    let tone = std::thread::spawn(|| {
        let _ = play_sine(&DeviceSelector::name_contains(VAD_HINT), 1_000.0, 0.85, 0.8);
    });
    let mut buf = vec![0u8; 8_192];
    let deadline = Instant::now() + Duration::from_millis(900);
    let (mut sum, mut n) = (0f64, 0usize);
    while Instant::now() < deadline {
        match cap.read(&mut buf, Duration::from_millis(5)) {
            Ok(k) if k > 0 => {
                for s in to_i16(&buf[..k]) {
                    let v = s as f64;
                    sum += v * v;
                    n += 1;
                }
            }
            Ok(_) => {}
            Err(e) => {
                cap.stop();
                let _ = tone.join();
                return Err(e.into());
            }
        }
    }
    cap.stop();
    let _ = tone.join();
    Ok(if n == 0 { 0.0 } else { (sum / n as f64).sqrt() })
}

fn build_click_pattern() -> (Vec<u8>, Vec<f64>) {
    let total = LEAD_S + CLICKS as f64 * (CLICK_S + GAP_S) + 0.6;
    let frames = (total * SR as f64) as usize;
    let mut pcm = vec![0i16; frames * CH as usize];
    let mut offsets = Vec::with_capacity(CLICKS);
    for i in 0..CLICKS {
        let off = LEAD_S + i as f64 * (CLICK_S + GAP_S);
        offsets.push(off);
        let start = (off * SR as f64) as usize;
        let len = (CLICK_S * SR as f64) as usize;
        for k in 0..len {
            let t = k as f64 / SR as f64;
            let env = ((t / 0.004).min(1.0)).min(((CLICK_S - t) / 0.004).clamp(0.0, 1.0));
            let v = (2.0 * std::f64::consts::PI * CLICK_HZ * t).sin() * 0.85 * env;
            let s = (v * 32767.0) as i16;
            let idx = start + k;
            if idx < frames {
                pcm[idx * 2] = s;
                pcm[idx * 2 + 1] = s;
            }
        }
    }
    (
        pcm.iter().flat_map(|v| v.to_le_bytes()).collect(),
        offsets,
    )
}

struct Analysis {
    duration: f64,
    peak: f32,
    rms: f64,
    onsets: Vec<f64>,
}

fn time_at(bounds: &[(usize, f64)], idx: usize) -> f64 {
    let mut best = (0usize, 0.0);
    for &(start, t) in bounds {
        if start <= idx {
            best = (start, t);
        } else {
            break;
        }
    }
    best.1 + (idx.saturating_sub(best.0)) as f64 / SR as f64
}

/// 用 1.2 kHz 的 Goertzel 能量检测 click（比裸阈值抗音乐干扰）。
fn analyze(_label: &str, chunks: &[(Instant, Vec<i16>)], t0: Instant) -> Analysis {
    let mut mono: Vec<f32> = Vec::new();
    let mut bounds: Vec<(usize, f64)> = Vec::new();
    for (t, samples) in chunks {
        let frames = samples.len() / CH as usize;
        let dur = frames as f64 / SR as f64;
        let start = (t.duration_since(t0).as_secs_f64() - dur).max(0.0);
        bounds.push((mono.len(), start));
        for f in 0..frames {
            mono.push(samples[f * CH as usize] as f32 / 32768.0);
        }
    }
    if mono.is_empty() {
        return Analysis {
            duration: 0.0,
            peak: 0.0,
            rms: 0.0,
            onsets: Vec::new(),
        };
    }
    let peak = mono.iter().fold(0f32, |a, b| a.max(b.abs()));
    let rms = (mono.iter().map(|v| (*v as f64) * (*v as f64)).sum::<f64>()
        / mono.len() as f64)
        .sqrt();
    let win = (0.060 * SR as f64) as usize;
    let hop = (0.005 * SR as f64) as usize;
    let w = 2.0 * std::f64::consts::PI * CLICK_HZ / SR as f64;
    let mut mags: Vec<(f64, f64)> = Vec::new();
    let mut i = win;
    while i <= mono.len() {
        let seg = &mono[i - win..i];
        let mut re = 0f64;
        let mut im = 0f64;
        for (k, v) in seg.iter().enumerate() {
            let ph = w * k as f64;
            re += *v as f64 * ph.cos();
            im -= *v as f64 * ph.sin();
        }
        let mag = (re * re + im * im).sqrt() / win as f64;
        mags.push((time_at(&bounds, i - win / 2), mag));
        i += hop;
    }
    let mmax = mags.iter().map(|m| m.1).fold(0f64, f64::max);
    let thr = (mmax * 0.30).max(0.004);
    let mut onsets = Vec::new();
    let mut last_t = f64::NEG_INFINITY;
    for idx in 1..mags.len().saturating_sub(1) {
        let (t, m) = mags[idx];
        if m > thr && m >= mags[idx - 1].1 && m >= mags[idx + 1].1 && t - last_t > 0.30 {
            onsets.push(t);
            last_t = t;
        }
    }
    Analysis {
        duration: mono.len() as f64 / SR as f64,
        peak,
        rms,
        onsets,
    }
}
