//! 真机回环自测：**一边放一段已知正弦，一边回环采集**，核对 peak / RMS / 主频。
//!
//! ```text
//! cargo run -p miplay-audio --bin loopback -- --seconds 5 --out evidence/loopback_capture.wav
//! cargo run -p miplay-audio --bin loopback -- --no-play --expect-silence --seconds 2
//! ```
//!
//! 退出码：`0` 符合预期；`2` 该有声却采到全零（失败）；`3` 静音对照里采到非零；`1` 参数/设备错误。

use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use miplay_audio::{
    estimate_frequency, play_sine, write_wav, AudioCapture, CaptureConfig, DeviceSelector,
    PcmFormat, PlaybackInfo,
};

struct Args {
    seconds: f64,
    freq: f64,
    amp: f32,
    device: DeviceSelector,
    no_play: bool,
    expect_silence: bool,
    out: Option<PathBuf>,
    tail_ms: u64,
    /// 采集开始后等多久再播放（用来验证「回环先开、声音后到」的场景）
    play_delay_ms: u64,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            seconds: 5.0,
            freq: 1_000.0,
            amp: 0.4,
            device: DeviceSelector::Default,
            no_play: false,
            expect_silence: false,
            out: None,
            tail_ms: 300,
            play_delay_ms: 0,
        }
    }
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args::default();
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut need = |name: &str| it.next().ok_or_else(|| format!("{name} 缺少参数值"));
        match arg.as_str() {
            "--seconds" | "-s" => a.seconds = need("--seconds")?.parse().map_err(|e| format!("{e}"))?,
            "--freq" | "-f" => a.freq = need("--freq")?.parse().map_err(|e| format!("{e}"))?,
            "--amp" | "-a" => a.amp = need("--amp")?.parse().map_err(|e| format!("{e}"))?,
            "--name" | "-n" => a.device = DeviceSelector::name_contains(need("--name")?),
            "--id" => a.device = DeviceSelector::id(need("--id")?),
            "--tail-ms" => a.tail_ms = need("--tail-ms")?.parse().map_err(|e| format!("{e}"))?,
            "--play-delay-ms" => {
                a.play_delay_ms = need("--play-delay-ms")?.parse().map_err(|e| format!("{e}"))?
            }
            "--out" | "-o" => a.out = Some(PathBuf::from(need("--out")?)),
            "--no-play" => a.no_play = true,
            "--expect-silence" => a.expect_silence = true,
            "--help" | "-h" => return Err("help".to_string()),
            other => return Err(format!("未知参数：{other}")),
        }
    }
    Ok(a)
}

fn usage() {
    println!(
        "用法：loopback [--seconds N] [--freq HZ] [--amp 0..1] [--name 子串 | --id ID]\n\
         \x20                [--no-play] [--expect-silence] [--play-delay-ms N]\n\
         \x20                [--tail-ms N] [--out FILE.wav]\n\
         默认：--seconds 5 --freq 1000 --amp 0.4（默认 render 端点回环采集）"
    );
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            if e != "help" {
                eprintln!("参数错误：{e}");
            }
            usage();
            std::process::exit(if e == "help" { 0 } else { 1 });
        }
    };

    if let Err(e) = run(&args) {
        eprintln!("失败：{e}");
        std::process::exit(1);
    }
}

fn run(args: &Args) -> Result<(), Box<dyn std::error::Error>> {
    println!("=== miplay-audio 真机回环自测 ===");

    let cfg = CaptureConfig {
        device: args.device.clone(),
        samplerate: 44_100,
        channels: 2,
        mode: miplay_audio::CaptureMode::Loopback,
        ring_seconds: 5.0,
        buffer_ms: 200,
    };

    let mut capture = AudioCapture::new(cfg)?;
    capture.start()?;
    let negotiated = capture
        .negotiated_format()
        .ok_or("start() 之后没有协商格式")?
        .clone();
    println!("采集：{}", negotiated.describe());

    // 先让采集线程跑起来，再开始放声音，避免漏掉起播的头部
    std::thread::sleep(Duration::from_millis(200));

    let (tx, rx) = mpsc::channel::<Result<PlaybackInfo, String>>();
    let play_selector = args.device.clone();
    let (freq, seconds, amp, no_play, delay_ms) =
        (args.freq, args.seconds, args.amp, args.no_play, args.play_delay_ms);
    if !no_play {
        std::thread::spawn(move || {
            if delay_ms > 0 {
                std::thread::sleep(Duration::from_millis(delay_ms));
            }
            let r = play_sine(&play_selector, freq, seconds, amp).map_err(|e| e.to_string());
            let _ = tx.send(r);
        });
    } else {
        let _ = tx.send(Err("--no-play：本进程不播放（跨进程场景：等其它程序出声）".to_string()));
    }

    let format = negotiated.output;
    let mut pcm: Vec<u8> = Vec::new();
    let hard_deadline = Instant::now() + Duration::from_secs_f64(args.seconds + 5.0);
    let mut play_reported = false;
    let mut play_done_at: Option<Instant> = None;
    let mut play_ok = false;
    let loop_started_at = Instant::now();
    println!("采集循环开始：{}（epoch 毫秒）", now_ms());

    loop {
        let chunk = capture.read_vec(8_192, Duration::from_millis(200))?;
        pcm.extend_from_slice(&chunk);

        if !play_reported {
            if let Ok(res) = rx.try_recv() {
                play_reported = true;
                match res {
                    Ok(info) => {
                        println!("播放：{}", info.describe());
                        play_ok = true;
                    }
                    Err(msg) => println!("播放：{msg}"),
                }
                play_done_at = Some(Instant::now());
            }
        }

        match play_done_at {
            Some(t) => {
                let tail = Duration::from_millis(args.tail_ms);
                if Instant::now().duration_since(t) >= tail || Instant::now() >= hard_deadline {
                    break;
                }
            }
            None => {
                if Instant::now() >= hard_deadline {
                    eprintln!("⚠ 播放线程一直没有返回，按超时结束");
                    break;
                }
            }
        }
    }

    let mut empty_reads = 0u32;
    if no_play {
        // 不播放：按 args.seconds 采集（也是「等别的程序出声」的跨进程场景）
        let want = (format.bytes_per_second() as f64 * args.seconds) as usize;
        let deadline = Instant::now() + Duration::from_secs_f64(args.seconds + 1.5);
        while pcm.len() < want && Instant::now() < deadline {
            let chunk = capture.read_vec(8_192, Duration::from_millis(200))?;
            if chunk.is_empty() {
                empty_reads += 1;
                // 已经采到过数据、之后连续 3 次空读 → 说明流空了，收工；
                // 一次都没采到就等到 deadline（别的进程可能过一会儿才开始放）。
                if !pcm.is_empty() && empty_reads >= 3 {
                    break;
                }
                continue;
            }
            empty_reads = 0;
            pcm.extend_from_slice(&chunk);
        }
    }
    capture.stop();
    println!(
        "采集循环结束：{}（跑了 {:.3} 秒，空读 {} 次，epoch 毫秒）",
        now_ms(),
        loop_started_at.elapsed().as_secs_f64(),
        empty_reads
    );

    // ---- 分析 ----
    let stats = analyse(&pcm, &format);
    println!("采样：{} 字节（{:.3} 秒，{} 帧）", pcm.len(), stats.seconds, stats.frames);
    println!(
        "左声道：peak={} ({:.4} FS) 整段 RMS={:.2} ({:.4} FS)",
        stats.peak_l, stats.peak_l as f64 / 32_768.0, stats.rms_l, stats.rms_l / 32_768.0
    );
    println!(
        "右声道：peak={} ({:.4} FS) 整段 RMS={:.2} ({:.4} FS)",
        stats.peak_r, stats.peak_r as f64 / 32_768.0, stats.rms_r, stats.rms_r / 32_768.0
    );
    println!(
        "有声区间：{:.3} 秒 / {} 帧（阈值 = peak/64，用来把首尾静音排除掉）",
        stats.active_seconds, stats.active_frames
    );
    println!(
        "        左声道 RMS={:.2} ({:.4} FS)；正弦理论 RMS={:.2}（peak/√2）；实测/理论={:.4}",
        stats.rms_active_l,
        stats.rms_active_l / 32_768.0,
        stats.peak_l as f64 / std::f64::consts::SQRT_2,
        stats.rms_active_l / (stats.peak_l as f64 / std::f64::consts::SQRT_2).max(1e-9)
    );
    let expected_peak = (args.amp as f64 * 32_767.0).round();
    if no_play {
        println!("        期望 peak：不适用（--no-play 时峰值由外部播放决定）");
    } else {
        println!(
            "        期望 peak（--amp {:.3} × 32767）= {:.0}，实测 = {}（比值 {:.4}）",
            args.amp,
            expected_peak,
            stats.peak_max(),
            stats.peak_max() as f64 / expected_peak.max(1e-9)
        );
    }
    match stats.freq_hz {
        Some(f) => println!("主频：{f:.1} Hz"),
        None => println!("主频：无法估计（信号太弱或太短）"),
    }
    println!(
        "声道关系：左右最大差 = {} LSB（{}）",
        stats.max_lr_delta,
        if stats.max_lr_delta == 0 {
            "完全一致"
        } else if stats.max_lr_delta <= 8 {
            "基本相同（量化/重采样噪声级，≤8 LSB ≈ 0.02% FS）"
        } else {
            "不同（左右确实是两路信号）"
        }
    );

    if let Some(path) = &args.out {
        if let Some(dir) = path.parent() {
            if !dir.as_os_str().is_empty() {
                std::fs::create_dir_all(dir)?;
            }
        }
        write_wav(path, format.channels, format.samplerate, 16, &pcm)?;
        println!("已写出 WAV：{}", path.display());
    }

    // ---- 判定 ----
    let silent = stats.peak_l == 0 && stats.peak_r == 0;
    if args.expect_silence {
        if pcm.is_empty() {
            println!(
                "结论：静音对照通过——回环端点没投递任何数据包（空读 {empty_reads} 次）。"
            );
            println!("     注意：这只是「本机当前没有程序在出声」，不等于采到了 N 秒数字静音。");
            return Ok(());
        }
        if silent {
            println!(
                "结论：静音对照符合预期——采到 {:.3} 秒全零 PCM（peak=0）✅",
                stats.seconds
            );
            return Ok(());
        }
        eprintln!("结论：静音对照里采到了非零数据（peak={}），说明有别的程序在出声", stats.peak_max());
        std::process::exit(3);
    }
    if silent {
        eprintln!("结论：采集到全零 —— 失败（没听到系统声音）");
        std::process::exit(2);
    }
    if !play_ok && !no_play {
        eprintln!("结论：采集到非零数据，但播放侧报错，不能确认是本程序播的音");
        std::process::exit(1);
    }
    let peak_ratio = stats.peak_max() as f64 / expected_peak.max(1e-9);
    if !no_play && !(0.85..=1.15).contains(&peak_ratio) {
        eprintln!("注意：实测峰值与播放幅度不符（比值 {peak_ratio:.4}），端点可能有音量/增强处理");
    }
    if let Some(f) = stats.freq_hz {
        if (f - args.freq).abs() > args.freq * 0.05 {
            eprintln!("注意：主频 {f:.1} Hz 与播放的 {:.1} Hz 相差超过 5%", args.freq);
        }
    }
    println!("结论：采集到真实音频 ✅");
    Ok(())
}

struct Stats {
    frames: u64,
    seconds: f64,
    peak_l: i32,
    peak_r: i32,
    rms_l: f64,
    rms_r: f64,
    /// 有声区间（|x| > peak/64）的帧数与秒数
    active_frames: u64,
    active_seconds: f64,
    rms_active_l: f64,
    freq_hz: Option<f64>,
    /// 左右声道逐帧最大差（LSB）
    max_lr_delta: i32,
}

impl Stats {
    fn peak_max(&self) -> i32 {
        self.peak_l.max(self.peak_r)
    }
}

/// 只支持 s16le 交错（也正是采集链路对外的契约）。
fn analyse(pcm: &[u8], format: &PcmFormat) -> Stats {
    let channels = format.channels.max(1) as usize;
    let samples: Vec<i16> = pcm
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect();
    let frames = samples.len() / channels;
    let mut peak_l = 0i32;
    let mut peak_r = 0i32;
    let mut sum_l = 0f64;
    let mut sum_r = 0f64;
    let mut max_delta = 0i32;
    let mut left_mono: Vec<f32> = Vec::with_capacity(frames);
    for f in 0..frames {
        let base = f * channels;
        let l = samples[base] as i32;
        let r = if channels > 1 { samples[base + 1] as i32 } else { l };
        max_delta = max_delta.max((l - r).abs());
        peak_l = peak_l.max(l.abs());
        peak_r = peak_r.max(r.abs());
        sum_l += (l as f64) * (l as f64);
        sum_r += (r as f64) * (r as f64);
        left_mono.push(l as f32 / 32_768.0);
    }
    let n = frames.max(1) as f64;

    // 第二遍：只看有声区间，排除首尾的静音填充（否则 RMS 会被拉低）
    let threshold = (peak_l.max(peak_r) / 64) as i16;
    let mut active = 0u64;
    let mut sum_active = 0f64;
    for f in 0..frames {
        let base = f * channels;
        let l = samples[base];
        let r = if channels > 1 { samples[base + 1] } else { l };
        if l.abs() > threshold || r.abs() > threshold {
            active += 1;
            sum_active += (l as f64) * (l as f64);
        }
    }
    let active_n = active.max(1) as f64;

    Stats {
        frames: frames as u64,
        seconds: frames as f64 / format.samplerate as f64,
        peak_l,
        peak_r,
        rms_l: (sum_l / n).sqrt(),
        rms_r: (sum_r / n).sqrt(),
        active_frames: active,
        active_seconds: active as f64 / format.samplerate as f64,
        rms_active_l: (sum_active / active_n).sqrt(),
        freq_hz: estimate_frequency(&left_mono, format.samplerate),
        max_lr_delta: max_delta,
    }
}
