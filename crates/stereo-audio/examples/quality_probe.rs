//! 数字音质探针：用真实引擎把已知信号从「采集源」送到「目标端点」，再回环采集目标端点，
//! 量化引擎数字路径对音频做了什么（增益/失真/削顶/掉块/反相/高频衰减）。
//!
//! 目的：把「引擎数字路径的锅」和「蓝牙编解码 / 声学 / 喇叭的锅」分开——
//! 目标是虚拟设备（Steam Streaming Microphone / 立体声链虚拟声卡）时，回环拿到的
//! 就是引擎输出的数字原样，不含蓝牙和喇叭。
//!
//! 用法：
//! ```text
//! quality_probe --source <采集源 spec> --target <目标端点 spec>
//!     [--channel left|right|both] [--signal sine|sweep|noise]
//!     [--freq 1000] [--seconds 3] [--gain 1.0] [--queue-ms 40]
//!     [--render-buffer-ms 40]
//! ```
//!
//! * `--source` / `--target`：端点友好名子串或完整 `{GUID}` 端点 id；
//! * `--channel`：引擎的声道选择（left/right/both），默认 both；
//! * `--signal`：sine（单频）/ sweep（100 Hz–10 kHz 指数扫频）/ noise（白噪声）；
//! * `--freq`：sine 的频率（Hz），默认 1000；
//! * `--seconds`：信号长度（秒），默认 3；
//! * `--gain`：引擎路由增益（0–8），默认 1.0；>1 用于故意制造削顶；
//! * `--queue-ms` / `--render-buffer-ms`：引擎抖动队列与 render 缓冲（默认 40 / 40）；
//! * `--save <目录>`：把「目标端点回环录制」与「送进采集源的播放信号」各存一个 WAV，便于离线复核。
//!
//! 输出：人类可读报告 + 最后一行 `SUMMARY key=value ...`（机器可解析）。
//!
//! 说明：本探针只读端点信息 + 播放/回环采集，**不修改**默认输出、端点格式、音量。

use std::f64::consts::PI;
use std::path::Path;
use std::time::{Duration, Instant};

use stereo_audio::{
    play_pcm_s16, resolve_endpoint, selector_for, AudioCapture, CaptureConfig, CaptureMode,
    DeviceSelector, EndpointInfo, PcmFormat, PlaybackInfo, RouteChannel, RouteConfig,
    RouteEngineConfig, RouteEngineHandle, RouteEngineStats,
};

/// 引擎对外契约：44100 Hz / 2 ch / 16 bit（一帧 4 字节）。
const SR: u32 = 44_100;
/// 一帧字节数（44100/2ch/s16）。
const FRAME_BYTES: usize = 4;
/// 播放信号前后的静音预填（ms）：给引擎开流/建队列留时间，也方便找起点。
const PREFILL_MS: u32 = 1_000;
/// 播放线程启动后多久开始真正播放（ms）：等引擎的采集线程/路由 sink 就绪。
const PLAY_START_DELAY_MS: u32 = 400;
/// 10 ms 一块（44100 Hz）。
const BLOCK_FRAMES: usize = 441;
/// 互相关搜索时对信号做的抽取倍数（44100/8 = 5512 Hz 等效采样）。
const CORR_DECIMATE: usize = 8;
/// 互相关最多搜索多长的延迟（ms）。
const CORR_MAX_LAG_MS: f64 = 2_000.0;
/// 播放信号幅度（相对满刻度）：留 6 dB 余量，避免默认就削顶。
const SIGNAL_AMPLITUDE: f64 = 0.5;

fn main() {
    let code = match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("Error: {e}");
            2
        }
    };
    std::process::exit(code);
}

// ---------------------------------------------------------------------------
// 参数
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Args {
    source: String,
    target: String,
    channel: RouteChannel,
    signal: String,
    freq: f64,
    seconds: f64,
    gain: f32,
    queue_ms: u32,
    render_buffer_ms: u32,
    /// 采集源端点的音量（dB，来自 IAudioEndpointVolume；只在回环是「后置音量」时参与归因）。
    src_vol_db: f64,
    /// 目标端点的音量（dB）；回环采集是后置音量，所以通常要扣掉它。
    tgt_vol_db: f64,
    save_dir: Option<String>,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            source: String::new(),
            target: String::new(),
            channel: RouteChannel::Both,
            signal: "sine".to_owned(),
            freq: 1_000.0,
            seconds: 3.0,
            gain: 1.0,
            queue_ms: 40,
            render_buffer_ms: 40,
            src_vol_db: 0.0,
            tgt_vol_db: 0.0,
            save_dir: None,
        }
    }
}

fn usage() -> &'static str {
    "用法: quality_probe --source <采集源 spec> --target <目标端点 spec> \
     [--channel left|right|both] [--signal sine|sweep|noise] [--freq 1000] \
     [--seconds 3] [--gain 1.0] [--queue-ms 40] [--render-buffer-ms 40] [--save <目录>]\n\
     spec 可以是端点友好名子串（大小写不敏感）或完整端点 id（{0.0.0.00000000}.{GUID}）。\n\
     --save 会把「目标端点回环录制」和「送进采集源的播放信号」各存一个 WAV，便于离线复核。"
}

fn parse_args() -> Result<Option<Args>, String> {
    let mut args = Args::default();
    let mut it = std::env::args().skip(1);
    let mut have_source = false;
    let mut have_target = false;
    while let Some(arg) = it.next() {
        let value = |it: &mut std::iter::Skip<std::env::Args>, name: &str| -> Result<String, String> {
            it.next()
                .ok_or_else(|| format!("{name} 后面缺少值（用 --help 看用法）"))
        };
        match arg.as_str() {
            "-h" | "--help" => {
                println!("{}", usage());
                return Ok(None);
            }
            "--source" => {
                args.source = value(&mut it, "--source")?;
                have_source = true;
            }
            "--target" => {
                args.target = value(&mut it, "--target")?;
                have_target = true;
            }
            "--channel" => {
                let v = value(&mut it, "--channel")?;
                args.channel = RouteChannel::parse(&v)
                    .ok_or_else(|| format!("--channel 只接受 left/right/both，收到 {v}"))?;
            }
            "--signal" => {
                let v = value(&mut it, "--signal")?;
                let v = v.to_ascii_lowercase();
                if !matches!(v.as_str(), "sine" | "sweep" | "noise") {
                    return Err(format!("--signal 只接受 sine/sweep/noise，收到 {v}"));
                }
                args.signal = v;
            }
            "--freq" => args.freq = parse_f64(&value(&mut it, "--freq")?, "--freq")?,
            "--seconds" => {
                let v = parse_f64(&value(&mut it, "--seconds")?, "--seconds")?;
                if !(0.2..=30.0).contains(&v) {
                    return Err(format!("--seconds={v} 不在 0.2..=30"));
                }
                args.seconds = v;
            }
            "--gain" => {
                let v = parse_f64(&value(&mut it, "--gain")?, "--gain")? as f32;
                if !(0.0..=8.0).contains(&v) {
                    return Err(format!("--gain={v} 不在 0..=8"));
                }
                args.gain = v;
            }
            "--queue-ms" => args.queue_ms = parse_u32(&value(&mut it, "--queue-ms")?, "--queue-ms")?,
            "--render-buffer-ms" => {
                args.render_buffer_ms =
                    parse_u32(&value(&mut it, "--render-buffer-ms")?, "--render-buffer-ms")?
            }
            "--save" => {
                args.save_dir = Some(value(&mut it, "--save")?);
            }
            "--src-vol-db" => {
                args.src_vol_db =
                    parse_f64(&value(&mut it, "--src-vol-db")?, "--src-vol-db")?
            }
            "--tgt-vol-db" => {
                args.tgt_vol_db =
                    parse_f64(&value(&mut it, "--tgt-vol-db")?, "--tgt-vol-db")?
            }
            other => return Err(format!("未知参数 {other}（用 --help 看用法）")),
        }
    }
    if !have_source || !have_target {
        return Err(format!("--source 和 --target 都是必填。\n{}", usage()));
    }
    Ok(Some(args))
}

fn parse_f64(v: &str, name: &str) -> Result<f64, String> {
    v.parse::<f64>()
        .map_err(|_| format!("{name} 不是合法数字：{v}"))
}

fn parse_u32(v: &str, name: &str) -> Result<u32, String> {
    v.parse::<u32>()
        .map_err(|_| format!("{name} 不是合法整数：{v}"))
}

// ---------------------------------------------------------------------------
// 主流程
// ---------------------------------------------------------------------------

fn run() -> Result<i32, String> {
    let args = match parse_args()? {
        Some(a) => a,
        None => return Ok(0),
    };
    println!("== StereoLink 数字音质探针 ==");
    println!("参数: {}", args_summary(&args));

    // 1. 解析端点（按 spec → 友好名子串或 id）
    let source_sel = selector_for(&args.source);
    let target_sel = selector_for(&args.target);
    let source_ep = resolve_endpoint(&source_sel, CaptureMode::Loopback)
        .map_err(|e| format!("解析采集源 \"{}\" 失败: {e}", args.source))?;
    let target_ep = resolve_endpoint(&target_sel, CaptureMode::Loopback)
        .map_err(|e| format!("解析目标端点 \"{}\" 失败: {e}", args.target))?;
    println!("采集源: {}  [{}]", source_ep.name, source_ep.id);
    println!("目标  : {}  [{}]", target_ep.name, target_ep.id);

    // 2. 造信号：16 bit 立体声交错 PCM（含 PREFILL_MS 静音）+ 纯信号参考（f32 单声道）
    let (pcm_i16, reference) = build_signal(&args);
    let signal_frames = (args.seconds * SR as f64) as usize;
    println!(
        "信号: {} / {} Hz / {:.2} s / 幅度 {:.2}（playback 44.1k 2ch s16，含 {} ms 预填静音）",
        args.signal, args.freq, args.seconds, SIGNAL_AMPLITUDE, PREFILL_MS
    );
    println!(
        "播放帧数: {}（其中信号 {} 帧 = {:.2} s）",
        pcm_i16.len() / 2,
        signal_frames,
        signal_frames as f64 / SR as f64
    );

    // 3. 起真实引擎：采集源 → 目标端点（单条路由）
    let mut route = RouteConfig::new(target_ep.name.clone(), args.channel);
    route.gain = args.gain;
    let engine_cfg = RouteEngineConfig {
        source_endpoint: source_ep.name.clone(),
        routes: vec![route],
        capture_gain: 1.0,
        render_buffer_ms: args.render_buffer_ms,
        queue_ms: args.queue_ms,
        rate: None,
        capture_buffer_ms: None,
    };
    println!("引擎配置: {}", engine_cfg.describe());
    let engine = RouteEngineHandle::start(engine_cfg)
        .map_err(|e| format!("启动路由引擎失败: {e}"))?;
    std::thread::sleep(Duration::from_millis(300));

    // 4. 对目标端点做 WASAPI 回环采集（后台线程持续读，主线程累积）
    let cap_cfg = CaptureConfig {
        device: selector_for(&target_ep.name),
        samplerate: SR,
        channels: 2,
        mode: CaptureMode::Loopback,
        ring_seconds: 2.0,
        buffer_ms: 20,
    };
    let mut capture =
        AudioCapture::new(cap_cfg).map_err(|e| format!("创建目标回环采集失败: {e}"))?;
    capture
        .start()
        .map_err(|e| format!("启动目标回环采集失败: {e}"))?;
    if let Some(n) = capture.negotiated_format() {
        println!("目标回环采集: {}", n.describe());
    }
    let cap_start = Instant::now();

    // 5. 播放线程：把已知信号播进采集源
    let pcm_bytes = pcm_i16_to_bytes(&pcm_i16);
    let play_dev: DeviceSelector = selector_for(&source_ep.name);
    let play_pcm = pcm_bytes.clone();
    let play_handle = std::thread::spawn(move || -> (Duration, Duration, Result<PlaybackInfo, String>) {
        std::thread::sleep(Duration::from_millis(PLAY_START_DELAY_MS as u64));
        let call_at = Instant::now();
        let started = Instant::now();
        let res = play_pcm_s16(&play_dev, PcmFormat::new(SR, 2, 16), &play_pcm)
            .map_err(|e| e.to_string());
        (call_at.duration_since(cap_start), started.elapsed(), res)
    });

    // 6. 主线程收数据（读完整个播放窗口 + 余量）
    let wanted_ms = (PREFILL_MS as f64 + PLAY_START_DELAY_MS as f64 + args.seconds * 1_000.0) * 1.0
        + 900.0;
    let capture_deadline = Duration::from_millis(wanted_ms.max(1_500.0) as u64);
    let mut captured: Vec<u8> = Vec::new();
    let mut capture_error: Option<String> = None;
    let mut buf = vec![0u8; SR as usize / 10 * FRAME_BYTES]; // 100 ms
    while cap_start.elapsed() < capture_deadline {
        match capture.read(&mut buf, Duration::from_millis(100)) {
            Ok(0) => {}
            Ok(n) => captured.extend_from_slice(&buf[..n]),
            Err(e) => {
                capture_error = Some(e.to_string());
                break;
            }
        }
    }
    capture.stop();

    let (play_call_offset, play_elapsed, play_res) = play_handle
        .join()
        .map_err(|_| "播放线程 panic".to_string())?;
    let play_info = play_res.map_err(|e| format!("播放失败: {e}"))?;
    let stats = engine.stats();
    drop(engine);

    println!("播放: {}", play_info.describe());
    println!(
        "播放调用时刻(相对采集开始): {:.1} ms；播放耗时: {:.0} ms",
        play_call_offset.as_secs_f64() * 1_000.0,
        play_elapsed.as_secs_f64() * 1_000.0
    );
    println!("引擎统计: {}", stats.describe());
    if let Some(err) = &stats.source_error {
        println!("!! 引擎采集故障: {err}");
    }
    if let Some(route_stats) = stats.routes.first() {
        if let Some(err) = &route_stats.error {
            println!("!! 引擎目标路由失败: {err}");
        }
    }
    if let Some(err) = &capture_error {
        println!("!! 目标回环采集错误: {err}");
    }

    // 7. 分析
    if captured.len() < FRAME_BYTES * SR as usize / 2 {
        return Err(format!(
            "采集数据太少：{} 字节（{:.2} s），无法分析",
            captured.len(),
            captured.len() as f64 / FRAME_BYTES as f64 / SR as f64
        ));
    }
    let frames = captured.len() / FRAME_BYTES;
    let recorded = bytes_to_mono(&captured);
    let recorded_l = bytes_to_channel(&captured, 0);
    let recorded_r = bytes_to_channel(&captured, 1);

    if let Some(dir) = &args.save_dir {
        let tag = sanitize_tag(&target_ep.name);
        save_wavs(dir, &tag, &captured, &pcm_i16);
    }

    let report = analyze(
        &args,
        &recorded,
        &recorded_l,
        &recorded_r,
        &reference,
        frames,
        play_call_offset,
        capture_error.is_some(),
        &stats,
        &source_ep,
        &target_ep,
        &play_info,
    );
    report.print(&args, &source_ep, &target_ep);
    Ok(if report.verdict_is_clean() { 0 } else { 1 })
}

// ---------------------------------------------------------------------------
// 信号生成
// ---------------------------------------------------------------------------

fn args_summary(args: &Args) -> String {
    format!(
        "source=\"{}\" target=\"{}\" channel={} signal={} freq={} Hz seconds={} gain={} queue={} ms render_buffer={} ms",
        args.source,
        args.target,
        args.channel.as_str(),
        args.signal,
        args.freq,
        args.seconds,
        args.gain,
        args.queue_ms,
        args.render_buffer_ms
    )
}

/// 生成 16 bit 立体声交错 PCM（左=右=同一信号，前面 PREFILL_MS 静音），
/// 同时返回信号段（不含预填静音）的 f32 单声道参考（= 播放内容本身）。
fn build_signal(args: &Args) -> (Vec<i16>, Vec<f32>) {
    let sr = SR as f64;
    let prefill_frames = (PREFILL_MS as f64 * sr / 1_000.0) as usize;
    let signal_frames = (args.seconds * sr) as usize;
    let tail_frames = (0.2 * sr) as usize;
    let total_frames = prefill_frames + signal_frames + tail_frames;
    let mut pcm = vec![0i16; total_frames * 2];
    let mut reference = Vec::with_capacity(signal_frames);

    let mut rng: u32 = 0x1234_5678;
    let mut phase = 0.0_f64;
    let amp = SIGNAL_AMPLITUDE;
    for i in 0..signal_frames {
        let t = i as f64 / sr;
        let v = match args.signal.as_str() {
            "noise" => {
                // xorshift32 白噪声，归一化到 -1..1
                rng ^= rng << 13;
                rng ^= rng >> 17;
                rng ^= rng << 5;
                (rng as f64 / u32::MAX as f64) * 2.0 - 1.0
            }
            "sweep" => {
                // 100 Hz → 10 kHz 指数扫频
                let f = 100.0 * 100.0_f64.powf(t / args.seconds);
                phase += 2.0 * PI * f / sr;
                phase.sin()
            }
            _ => (2.0 * PI * args.freq * t).sin(),
        };
        let s = (v * amp * 32_767.0).round().clamp(-32_767.0, 32_767.0) as i16;
        let idx = (prefill_frames + i) * 2;
        pcm[idx] = s;
        pcm[idx + 1] = s;
        reference.push(s as f32 / 32_768.0);
    }
    (pcm, reference)
}

fn pcm_i16_to_bytes(pcm: &[i16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(pcm.len() * 2);
    for s in pcm {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

fn bytes_to_mono(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(FRAME_BYTES)
        .map(|frame| {
            let l = i16::from_le_bytes([frame[0], frame[1]]) as f32;
            let r = i16::from_le_bytes([frame[2], frame[3]]) as f32;
            (l + r) / 2.0 / 32_768.0
        })
        .collect()
}

fn bytes_to_channel(bytes: &[u8], channel: usize) -> Vec<f32> {
    let offset = channel * 2;
    bytes
        .chunks_exact(FRAME_BYTES)
        .map(|frame| {
            i16::from_le_bytes([frame[offset], frame[offset + 1]]) as f32 / 32_768.0
        })
        .collect()
}

// ---------------------------------------------------------------------------
// 分析
// ---------------------------------------------------------------------------

struct Analysis {
    corr: f64,
    onset_ms: f64,
    /// 录制里检测到的有信号区间：(起点 ms, 终点 ms, 峰值 dBFS)
    signal_spans: Vec<(f64, f64, f64)>,
    play_call_offset_ms: f64,
    path_latency_ms: f64,
    rms_l_db: f64,
    rms_r_db: f64,
    rms_db: f64,
    steady_rms_db: f64,
    ref_steady_rms_db: f64,
    ref_rms_db: f64,
    gain_db: f64,
    expected_gain_db: f64,
    src_vol_db: f64,
    tgt_vol_db: f64,
    gain_error_db: f64,
    blocks_min_db: f64,
    blocks_median_db: f64,
    blocks_max_db: f64,
    block_count: usize,
    glitch_blocks: usize,
    dropout_blocks: usize,
    clip_samples: u64,
    max_abs: i32,
    fundamental_hz: f64,
    harmonic_db: Vec<f64>,
    thd_percent: f64,
    thdn_db: f64,
    sweep_ratios_db: Vec<(f64, f64)>, // (期望频率, 录制/参考 幅度比 dB)
    sweep_hf_ratio_db: f64,          // 相对中频的衰减（真正看「高频被削」）
    sweep_hf_abs_db: f64,            // 高频端绝对 录制/参考 比
    frames: usize,
    captured_seconds: f64,
    engine_underruns: u64,
    engine_sent_bytes: u64,
    engine_queued_ms: f64,
    engine_path: String,
    engine_route_error: bool,
    capture_error: bool,
}

impl Analysis {
    fn verdict_is_clean(&self) -> bool {
        let sweep_ok = self.sweep_hf_ratio_db.is_nan() || self.sweep_hf_ratio_db > -1.0;
        let thd_ok = self.thd_percent.is_nan() || self.thd_percent < 2.0;
        // 增益不参与判定：录到的电平 = 信号 × 引擎增益 × 端点音量（回环后置音量），
        // 端点音量不是引擎的错。用 --src-vol-db/--tgt-vol-db 声明后，看「归因误差」这一行即可。
        let level_ok = self.rms_db > -75.0;
        self.corr > 0.99
            && self.glitch_blocks == 0
            && self.dropout_blocks == 0
            && self.clip_samples == 0
            && thd_ok
            && level_ok
            && sweep_ok
            && !self.engine_route_error
            && !self.capture_error
    }

    fn print(&self, args: &Args, source: &EndpointInfo, target: &EndpointInfo) {
        println!("--- 分析（录制 {} 帧 = {:.3} s）---", self.frames, self.captured_seconds);
        println!(
            "信号起点(录制时间轴): {:.1} ms（预填 {} ms + 播放调用偏移 {:.1} ms → 额外管线延迟 ≈ {:.1} ms，含 play_pcm_s16 的 1 s 初始缓冲）",
            self.onset_ms, PREFILL_MS, self.play_call_offset_ms, self.path_latency_ms
        );
        if !self.signal_spans.is_empty() {
            let mut line = String::from("信号区间(10 ms 块 > -50 dBFS): ");
            for (i, (s, e, p)) in self.signal_spans.iter().enumerate() {
                if i > 0 {
                    line.push_str(" | ");
                }
                line.push_str(&format!("{:.2}–{:.2} s 峰值 {:.1} dBFS", s / 1_000.0, e / 1_000.0, p));
            }
            println!("{}", line);
        } else {
            println!("信号区间: 未检测到高于 -50 dBFS 的区间（录制几乎全静音）");
        }
        println!(
            "互相关: {:.6}（{}）",
            self.corr,
            if self.corr >= 0.0 { "正相" } else { "反相/极性反转" }
        );
        println!(
            "电平: 稳态 RMS {:.2} dBFS（参考 {:.2} dBFS；整段 L {:.2} / R {:.2} dBFS），增益 {:+.2} dB，归因误差 {:+.2} dB（|误差| < 1.5 dB 即引擎 0 dB）",
            self.steady_rms_db,
            self.ref_steady_rms_db,
            self.rms_l_db,
            self.rms_r_db,
            self.gain_db,
            self.gain_error_db
        );
        println!(
            "  增益归因: 引擎增益 {:+.2} dB + 采集源音量 {:+.1} dB + 目标音量 {:+.1} dB → 期望 {:+.2} dB\n\
             （回环是后置音量：目标端点音量必然被录进去；采集源是否后置取决于设备——\n\
              自研虚拟声卡实测后置、Realtek 实测前置，所以 --src-vol-db 按实测填，物理输出通常填 0）",
            self.expected_gain_db - self.src_vol_db - self.tgt_vol_db,
            self.src_vol_db,
            self.tgt_vol_db,
            self.expected_gain_db
        );
        println!(
            "10 ms 块 RMS: min {:.2} / 中位 {:.2} / max {:.2} dBFS（{} 块），突变块 {}，零值(掉块)块 {}",
            self.blocks_min_db,
            self.blocks_median_db,
            self.blocks_max_db,
            self.block_count,
            self.glitch_blocks,
            self.dropout_blocks
        );
        println!(
            "数字削顶: {} 个样本（峰值 |s|={}），{}",
            self.clip_samples,
            self.max_abs,
            if self.clip_samples > 0 {
                "削顶会产生「滋啦」/破音"
            } else {
                "无削顶"
            }
        );
        if args.signal != "noise" {
            println!(
                "基波 {:.1} Hz 幅度比: {:+.2} dB，H2 {:+.1} / H3 {:+.1} / H5 {:+.1} dB，THD {:.3} %，THD+N {:.1} dB",
                self.fundamental_hz,
                self.gain_db,
                self.harmonic_db.first().copied().unwrap_or(f64::NAN),
                self.harmonic_db.get(1).copied().unwrap_or(f64::NAN),
                self.harmonic_db.get(2).copied().unwrap_or(f64::NAN),
                self.thd_percent,
                self.thdn_db
            );
        }
        if args.signal == "sweep" {
            let mut line = String::from("扫频响应（录制/参考幅度比, dB）: ");
            for (i, (f, r)) in self.sweep_ratios_db.iter().enumerate() {
                if i > 0 {
                    line.push_str(" | ");
                }
                line.push_str(&format!("{:.0}Hz {:+.1}", f, r));
            }
            println!("{}", line);
            println!(
                "高频端(≈9–10 kHz) 相对中频衰减: {:+.1} dB（绝对 {:+.1} dB；0 dB 附近=高中低频一样，负=高频被削）",
                self.sweep_hf_ratio_db, self.sweep_hf_abs_db
            );
        }
        println!(
            "引擎路由: 已发 {} B | 欠载 {} | 队列 {:.0} ms | 路径 {}",
            self.engine_sent_bytes, self.engine_underruns, self.engine_queued_ms, self.engine_path
        );
        println!(
            "端点: 源 \"{}\" [{}] → 目标 \"{}\" [{}]",
            source.name, source.id, target.name, target.id
        );
        println!("--- 判定: {} ---", if self.verdict_is_clean() { "数字路径干净" } else { "有可疑点（见上）" });
        println!("{}", self.summary_line(args, source, target));
    }

    fn summary_line(&self, args: &Args, source: &EndpointInfo, target: &EndpointInfo) -> String {
        let mut s = format!(
            "SUMMARY source=\"{}\" source_id=\"{}\" target=\"{}\" target_id=\"{}\" channel={} signal={} freq={} seconds={} gain={} frames={} captured_s={:.3} onset_ms={:.1} corr={:.6} sign={} rms_db={:.2} ref_rms_db={:.2} steady_rms_db={:.2} ref_steady_rms_db={:.2} gain_db={:.2} gain_err_db={:.2} src_vol_db={:.1} tgt_vol_db={:.1} expected_gain_db={:.2} blocks_min_db={:.2} blocks_med_db={:.2} blocks_max_db={:.2} block_count={} glitch_blocks={} dropout_blocks={} clip_samples={} max_abs={} thd_pct={:.3} thdn_db={:.1}",
            source.name,
            source.id,
            target.name,
            target.id,
            args.channel.as_str(),
            args.signal,
            args.freq,
            args.seconds,
            args.gain,
            self.frames,
            self.captured_seconds,
            self.onset_ms,
            self.corr,
            if self.corr >= 0.0 { "positive" } else { "negative" },
            self.rms_db,
            self.ref_rms_db,
            self.steady_rms_db,
            self.ref_steady_rms_db,
            self.gain_db,
            self.gain_error_db,
            self.src_vol_db,
            self.tgt_vol_db,
            self.expected_gain_db,
            self.blocks_min_db,
            self.blocks_median_db,
            self.blocks_max_db,
            self.block_count,
            self.glitch_blocks,
            self.dropout_blocks,
            self.clip_samples,
            self.max_abs,
            self.thd_percent,
            self.thdn_db
        );
        if !self.sweep_ratios_db.is_empty() {
            s.push_str(&format!(" hf_ratio_db={:.2} hf_abs_db={:.2}", self.sweep_hf_ratio_db, self.sweep_hf_abs_db));
            let parts: Vec<String> = self
                .sweep_ratios_db
                .iter()
                .map(|(f, r)| format!("{:.0}:{:.1}", f, r))
                .collect();
            s.push_str(&format!(" sweep_db=\"{}\"", parts.join(",")));
        }
        s.push_str(&format!(
            " engine_sent_bytes={} engine_underruns={} engine_queued_ms={:.1} engine_path=\"{}\" engine_error={} capture_error={} verdict={}",
            self.engine_sent_bytes,
            self.engine_underruns,
            self.engine_queued_ms,
            self.engine_path,
            if self.engine_route_error { 1 } else { 0 },
            if self.capture_error { 1 } else { 0 },
            if self.verdict_is_clean() { "CLEAN" } else { "SUSPECT" }
        ));
        s
    }
}

#[allow(clippy::too_many_arguments)]
fn analyze(
    args: &Args,
    recorded: &[f32],
    recorded_l: &[f32],
    recorded_r: &[f32],
    reference: &[f32],
    frames: usize,
    play_call_offset: Duration,
    capture_error: bool,
    stats: &RouteEngineStats,
    _source: &EndpointInfo,
    _target: &EndpointInfo,
    _play_info: &PlaybackInfo,
) -> Analysis {
    // 能量区间：先找「哪里真的有声音」，互相关只在能量区间附近搜索
    //（信号自身可能含 1 s 预填静音 + play_pcm_s16 的初始缓冲，起点常 > 2 s，
    //  远大于旧的固定搜索上限 2 s；全静音窗口的归一化互相关也毫无意义）。
    let signal_spans = energy_spans(recorded, -50.0);
    let longest = signal_spans
        .iter()
        .max_by(|a, b| {
            (a.1 - a.0)
                .partial_cmp(&(b.1 - b.0))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .copied();
    let (lag_min_ms, lag_max_ms) = match longest {
        Some((s, _, _)) => ((s - 300.0).max(0.0), s + 300.0),
        None => (0.0, CORR_MAX_LAG_MS),
    };
    let (corr, best_lag) = cross_correlate(reference, recorded, lag_min_ms, lag_max_ms);
    let onset_from_spans = longest.map(|(s, _, _)| ms_to_sample(s));
    let onset_sample = if corr.abs() >= 0.6 {
        best_lag
    } else {
        onset_from_spans.unwrap_or(best_lag)
    };
    let onset_sample = onset_sample.min(recorded.len().saturating_sub(1));
    let onset_ms = onset_sample as f64 * 1_000.0 / SR as f64;
    let expected_onset = play_call_offset.as_secs_f64() * 1_000.0 + PREFILL_MS as f64;
    let path_latency_ms = onset_ms - expected_onset;

    // 稳态段：信号起点 + 100 ms 开始，最多取 0.5 s，且不越过信号长度
    let signal_samples = reference.len();
    let guard = (SR as f64 * 0.1) as usize;
    let steady_start = (onset_sample + guard).min(recorded.len().saturating_sub(1));
    let steady_len = (SR as usize / 2)
        .min(signal_samples.saturating_sub(guard))
        .min(recorded.len().saturating_sub(steady_start));
    let steady = &recorded[steady_start..steady_start + steady_len];

    // 稳态段 RMS 与「参考里对应的一段」比：绝不能用整段录制 RMS（里面含预填静音，会把增益算低约 2–3 dB）。
    let ref_steady_start = steady_start.saturating_sub(onset_sample).min(reference.len().saturating_sub(1));
    let ref_steady_len = steady_len.min(reference.len().saturating_sub(ref_steady_start));
    let ref_steady = &reference[ref_steady_start..ref_steady_start + ref_steady_len];
    let rec_steady_rms = rms(steady);
    let ref_steady_rms = rms(ref_steady);
    let gain_db = db(rec_steady_rms / ref_steady_rms.max(1e-12));
    // 归因：期望增益 = 引擎增益 + 采集源端点音量(若回环后置) + 目标端点音量(后置)。
    let engine_gain_db = db(args.gain as f64);
    let expected_gain_db = engine_gain_db + args.src_vol_db + args.tgt_vol_db;
    let gain_error_db = gain_db - expected_gain_db;

    // 全段 RMS / 左右电平
    let rec_rms = rms(recorded);
    let rec_rms_l = rms(recorded_l);
    let rec_rms_r = rms(recorded_r);
    let ref_rms = rms(reference);

    // 10 ms 块 RMS
    let block = BLOCK_FRAMES.max(1);
    let blocks: Vec<f64> = steady.chunks(block).map(rms).collect();
    let (blocks_min, blocks_median, blocks_max) = block_stats(&blocks);
    let (glitch_blocks, dropout_blocks) = block_anomalies(&blocks);

    // 数字削顶 / 峰值（左右声道各自检查，取最大绝对值）
    let mut clip_samples = 0u64;
    let mut max_abs = 0i32;
    for &s in recorded_l.iter().chain(recorded_r.iter()) {
        let abs = (s * 32_768.0).abs().round() as i32;
        if abs > max_abs {
            max_abs = abs;
        }
        if abs >= 32_767 {
            clip_samples += 1;
        }
    }

    // 频谱：基波 + 2/3/5 次谐波 + THD/THD+N
    let mut harmonic_db = Vec::new();
    let mut fundamental_hz = args.freq;
    let mut thd_percent = f64::NAN;
    let mut thdn_db = f64::NAN;
    let mut sweep_ratios_db = Vec::new();
    let mut sweep_hf_ratio_db = f64::NAN;
    let mut sweep_hf_abs_db = f64::NAN;
    if steady_len > 0 {
        let sr = SR as f64;
        match args.signal.as_str() {
            "sweep" => {
                // 取 6 个分段，分别比较「期望瞬时频率」上的幅度（录制 vs 参考）
                let segs = 6usize;
                for k in 0..segs {
                    let rel = (k as f64 + 0.1) / segs as f64;
                    let start = (rel * signal_samples as f64) as usize;
                    let len = (signal_samples / segs).min(recorded.len().saturating_sub(onset_sample + start));
                    if len == 0 {
                        continue;
                    }
                    let rec = &recorded[onset_sample + start..onset_sample + start + len];
                    let rf = &reference[start..start + len];
                    let t_mid = (start as f64 + len as f64 / 2.0) / SR as f64;
                    let f = 100.0 * 100.0_f64.powf(t_mid / args.seconds);
                    let ar = goertzel_amp(rec, sr, f);
                    let af = goertzel_amp(rf, sr, f);
                    let ratio = db(ar / af);
                    sweep_ratios_db.push((f, ratio));
                    if k == segs - 1 {
                        sweep_hf_abs_db = ratio;
                    }
                }
                // 高频端更细：信号最后 8%（≈8.5–10 kHz）
                let hf_start = (0.92 * signal_samples as f64) as usize;
                let hf_len = signal_samples.saturating_sub(hf_start)
                    .min(recorded.len().saturating_sub(onset_sample + hf_start));
                if hf_len > 0 {
                    let rec = &recorded[onset_sample + hf_start..onset_sample + hf_start + hf_len];
                    let rf = &reference[hf_start..hf_start + hf_len];
                    let t_mid = (hf_start as f64 + hf_len as f64 / 2.0) / SR as f64;
                    let f = 100.0 * 100.0_f64.powf(t_mid / args.seconds);
                    let ar = goertzel_amp(rec, sr, f);
                    let af = goertzel_amp(rf, sr, f);
                    sweep_hf_abs_db = db(ar / af);
                }
                // 归一化：减去中频段（六段比值的中位数）→ 0 dB 表示平坦，负值表示高频被削
                if sweep_hf_abs_db.is_finite() && !sweep_ratios_db.is_empty() {
                    let mut rs: Vec<f64> = sweep_ratios_db.iter().map(|(_, r)| *r).filter(|r| r.is_finite()).collect();
                    rs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                    if !rs.is_empty() {
                        let flat_ref = rs[rs.len() / 2];
                        sweep_hf_ratio_db = sweep_hf_abs_db - flat_ref;
                    }
                }
            }
            "noise" => {}
            _ => {
                fundamental_hz = args.freq;
                let sr = SR as f64;
                let a1 = goertzel_amp(steady, sr, args.freq);
                let h2 = goertzel_amp(steady, sr, args.freq * 2.0);
                let h3 = goertzel_amp(steady, sr, args.freq * 3.0);
                let h5 = goertzel_amp(steady, sr, args.freq * 5.0);
                harmonic_db.push(db(h2 / a1.max(1e-12)));
                harmonic_db.push(db(h3 / a1.max(1e-12)));
                harmonic_db.push(db(h5 / a1.max(1e-12)));
                let distortion = (h2 * h2 + h3 * h3 + h5 * h5).sqrt();
                thd_percent = 100.0 * distortion / a1.max(1e-12);
                let total = rms(steady);
                let fund_rms = a1 / 2.0_f64.sqrt();
                let noise = (total * total - fund_rms * fund_rms).max(0.0).sqrt();
                thdn_db = db(noise / fund_rms.max(1e-12));
            }
        }
    }

    let route_stats = stats.routes.first();
    let engine_underruns = route_stats.map(|r| r.underruns).unwrap_or(0);
    let engine_sent_bytes = route_stats.map(|r| r.sent_bytes).unwrap_or(0);
    let engine_queued_ms = route_stats.map(|r| r.queued_ms).unwrap_or(0.0);
    let engine_path = route_stats
        .and_then(|r| r.format_path.clone())
        .unwrap_or_default();
    let engine_route_error = route_stats.map(|r| r.error.is_some()).unwrap_or(false);

    Analysis {
        corr,
        onset_ms,
        signal_spans,
        play_call_offset_ms: play_call_offset.as_secs_f64() * 1_000.0,
        path_latency_ms,
        rms_l_db: db(rec_rms_l),
        rms_r_db: db(rec_rms_r),
        rms_db: db(rec_rms),
        steady_rms_db: db(rec_steady_rms),
        ref_steady_rms_db: db(ref_steady_rms),
        ref_rms_db: db(ref_rms),
        gain_db,
        expected_gain_db,
        src_vol_db: args.src_vol_db,
        tgt_vol_db: args.tgt_vol_db,
        gain_error_db,
        blocks_min_db: blocks_min,
        blocks_median_db: blocks_median,
        blocks_max_db: blocks_max,
        block_count: blocks.len(),
        glitch_blocks,
        dropout_blocks,
        clip_samples,
        max_abs,
        fundamental_hz,
        harmonic_db,
        thd_percent,
        thdn_db,
        sweep_ratios_db,
        sweep_hf_ratio_db,
        sweep_hf_abs_db,
        frames,
        captured_seconds: frames as f64 / SR as f64,
        engine_underruns,
        engine_sent_bytes,
        engine_queued_ms,
        engine_path,
        engine_route_error,
        capture_error,
    }
}

// ---------------------------------------------------------------------------
// DSP 小工具（零依赖）
// ---------------------------------------------------------------------------

fn rms(x: &[f32]) -> f64 {
    if x.is_empty() {
        return 0.0;
    }
    let sum: f64 = x.iter().map(|&v| (v as f64) * (v as f64)).sum();
    (sum / x.len() as f64).sqrt()
}

fn db(x: f64) -> f64 {
    20.0 * x.max(1e-12).log10()
}

fn block_stats(blocks: &[f64]) -> (f64, f64, f64) {
    if blocks.is_empty() {
        return (f64::NAN, f64::NAN, f64::NAN);
    }
    let mut sorted: Vec<f64> = blocks.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let min = sorted[0];
    let median = sorted[sorted.len() / 2];
    let max = sorted[sorted.len() - 1];
    (db(min), db(median), db(max))
}

/// 返回（突变块数, 零值块数）。
fn block_anomalies(blocks: &[f64]) -> (usize, usize) {
    let floor = 3.0e-4; // ≈ -70 dBFS：低于这个基本是静音
    let mut glitch = 0;
    let mut dropout = 0;
    for (i, &b) in blocks.iter().enumerate() {
        if b < floor {
            dropout += 1;
        }
        if i > 0 {
            let prev = blocks[i - 1];
            if prev > floor && b > floor {
                let ratio = b / prev;
                if ratio > 2.0 || ratio < 0.5 {
                    glitch += 1;
                }
            }
        }
    }
    (glitch, dropout)
}

/// 检测「有能量」的连续区间：按 10 ms 块算 RMS，超过阈值就归到同一段（间隔 < 50 ms 合并）。
///
/// 返回 (起点 ms, 终点 ms, 峰值 dBFS)，按时间排序。
fn energy_spans(x: &[f32], threshold_db: f64) -> Vec<(f64, f64, f64)> {
    let block = BLOCK_FRAMES.max(1);
    let mut raw: Vec<(usize, usize, f64)> = Vec::new();
    let mut cur: Option<(usize, usize, f64)> = None;
    for (i, chunk) in x.chunks(block).enumerate() {
        let d = db(rms(chunk));
        if d > threshold_db {
            match &mut cur {
                Some((_, end, peak)) => {
                    *end = (i + 1) * block;
                    if d > *peak {
                        *peak = d;
                    }
                }
                None => cur = Some((i * block, (i + 1) * block, d)),
            }
        } else if let Some((s, e, p)) = cur.take() {
            raw.push((s, e.min(x.len()), p));
        }
    }
    if let Some((s, e, p)) = cur.take() {
        raw.push((s, e.min(x.len()), p));
    }

    // 合并间隔小于 50 ms 的相邻段（正弦过零点附近的块可能短暂掉到阈值下）
    let merge_gap = ms_to_sample(50.0);
    let mut spans: Vec<(usize, usize, f64)> = Vec::new();
    for (s, e, p) in raw {
        match spans.last_mut() {
            Some((_, last_end, last_peak)) if s <= *last_end + merge_gap => {
                *last_end = e;
                if p > *last_peak {
                    *last_peak = p;
                }
            }
            _ => spans.push((s, e, p)),
        }
    }
    spans
        .into_iter()
        .map(|(s, e, p)| {
            (
                s as f64 * 1_000.0 / SR as f64,
                e as f64 * 1_000.0 / SR as f64,
                p,
            )
        })
        .collect()
}

fn ms_to_sample(ms: f64) -> usize {
    (ms * SR as f64 / 1_000.0).round() as usize
}

/// 把名字变成安全的文件名片段（只保留 ASCII 字母数字、`-`、`_`）。
fn sanitize_tag(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
            out.push(c);
        } else if c == ' ' {
            out.push('_');
        }
    }
    if out.is_empty() {
        "endpoint".to_string()
    } else {
        out
    }
}

/// 写一个 16 bit PCM WAV（`pcm` 是小端交错 s16 字节）。
fn write_wav_s16(path: &Path, sample_rate: u32, channels: u16, pcm: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let data_len = pcm.len() as u32;
    let block_align = channels.saturating_mul(2);
    let byte_rate = sample_rate.saturating_mul(block_align as u32);
    let mut f = std::fs::File::create(path)?;
    f.write_all(b"RIFF")?;
    f.write_all(&(36u32.saturating_add(data_len)).to_le_bytes())?;
    f.write_all(b"WAVEfmt ")?;
    f.write_all(&16u32.to_le_bytes())?;
    f.write_all(&1u16.to_le_bytes())?;
    f.write_all(&channels.to_le_bytes())?;
    f.write_all(&sample_rate.to_le_bytes())?;
    f.write_all(&byte_rate.to_le_bytes())?;
    f.write_all(&block_align.to_le_bytes())?;
    f.write_all(&16u16.to_le_bytes())?;
    f.write_all(b"data")?;
    f.write_all(&data_len.to_le_bytes())?;
    f.write_all(pcm)?;
    Ok(())
}

/// `--save` 时把「录制（目标端点回环）」与「播放（送进采集源）」都存成 WAV，方便离线复核。
fn save_wavs(dir: &str, tag: &str, recorded_pcm: &[u8], played_pcm: &[i16]) {
    let base = Path::new(dir);
    if let Err(e) = std::fs::create_dir_all(base) {
        println!("!! --save 目录创建失败 {dir}: {e}");
        return;
    }
    let rec_path = base.join(format!("{tag}-recorded.wav"));
    match write_wav_s16(&rec_path, SR, 2, recorded_pcm) {
        Ok(()) => println!("已保存回环录制: {}", rec_path.display()),
        Err(e) => println!("!! 保存回环录制失败: {e}"),
    }
    let played_bytes = pcm_i16_to_bytes(played_pcm);
    let play_path = base.join(format!("{tag}-played.wav"));
    match write_wav_s16(&play_path, SR, 2, &played_bytes) {
        Ok(()) => println!("已保存播放信号: {}", play_path.display()),
        Err(e) => println!("!! 保存播放信号失败: {e}"),
    }
}

/// 抽取（块平均）：降采样到 SR/CORR_DECIMATE，用于互相关搜索。
fn decimate(x: &[f32], factor: usize) -> Vec<f32> {
    x.chunks(factor)
        .map(|c| c.iter().map(|&v| v as f64).sum::<f64>() / c.len() as f64)
        .map(|v| v as f32)
        .collect()
}

/// 归一化互相关：在 `observed` 的 [lag_min_ms, lag_max_ms] 区间里搜索 `reference` 的最佳位置。
///
/// 返回（相关系数, 位置=observed 的起始样本下标，按原始采样率折算）。
/// 只看「有能量」的窗口：观测窗口能量远低于参考窗口时跳过（静音窗口的分母接近 0，
/// 会算出毫无意义的 ±0.x 相关值，这是首版 A 组误报反相的根因）。
fn cross_correlate(
    reference: &[f32],
    observed: &[f32],
    lag_min_ms: f64,
    lag_max_ms: f64,
) -> (f64, usize) {
    let reference_dec = decimate(reference, CORR_DECIMATE);
    let observed_dec = decimate(observed, CORR_DECIMATE);
    let win = reference_dec.len().min((SR as usize / 2) / CORR_DECIMATE);
    let lag_lo = ms_to_sample(lag_min_ms.max(0.0)) / CORR_DECIMATE;
    let lag_hi = ms_to_sample(lag_max_ms.max(lag_min_ms)) / CORR_DECIMATE;
    let mut best = (0.0f64, 0usize);
    if win == 0 || observed_dec.len() < win {
        return (0.0, 0);
    }
    let lag_lo = lag_lo.min(observed_dec.len().saturating_sub(win));
    let lag_hi = lag_hi.min(observed_dec.len().saturating_sub(win));
    let ref_win = &reference_dec[..win];
    let ref_mean: f64 = ref_win.iter().map(|&v| v as f64).sum::<f64>() / win as f64;
    let ref_centered: Vec<f64> = ref_win.iter().map(|&v| v as f64 - ref_mean).collect();
    let ref_energy: f64 = ref_centered.iter().map(|v| v * v).sum();

    for lag in lag_lo..=lag_hi {
        let obs = &observed_dec[lag..lag + win];
        let obs_mean: f64 = obs.iter().map(|&v| v as f64).sum::<f64>() / win as f64;
        let mut dot = 0.0;
        let mut obs_energy = 0.0;
        for (i, &v) in obs.iter().enumerate() {
            let c = v as f64 - obs_mean;
            dot += ref_centered[i] * c;
            obs_energy += c * c;
        }
        let denom = (ref_energy * obs_energy).sqrt();
        if obs_energy < ref_energy * 1e-4 {
            // 观测窗口基本是静音：归一化互相关在这里没有意义（分母接近 0），跳过。
            continue;
        }
        let corr = if denom > 1e-20 { dot / denom } else { 0.0 };
        if corr.abs() > best.0.abs() {
            best = (corr, lag);
        }
    }
    (best.0, best.1 * CORR_DECIMATE)
}

/// 单个频点的 Goertzel 幅度（≈ 该频率正弦的峰值幅度）。
fn goertzel_amp(x: &[f32], sample_rate: f64, freq: f64) -> f64 {
    let n = x.len();
    if n == 0 || freq <= 0.0 || freq >= sample_rate / 2.0 {
        return 0.0;
    }
    let k = freq / sample_rate;
    let w = 2.0 * PI * k;
    let coeff = 2.0 * w.cos();
    let mut s1 = 0.0;
    let mut s2 = 0.0;
    for &v in x {
        let s0 = v as f64 + coeff * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    let power = s1 * s1 + s2 * s2 - coeff * s1 * s2;
    if power <= 0.0 {
        return 0.0;
    }
    2.0 * power.sqrt() / n as f64
}
