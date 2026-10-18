//! `sync_probe` —— StereoLink 声学自校验真机探针。
//!
//! ```text
//! cargo run -p stereo-audio --example sync_probe -- --list
//! cargo run -p stereo-audio --example sync_probe -- --mic 麦克风 --check --speaker "Xiaomi Sound"
//! cargo run -p stereo-audio --example sync_probe -- --speaker "Xiaomi Sound" --loopback
//! cargo run -p stereo-audio --example sync_probe -- --mic Chu2 --volume
//! cargo run -p stereo-audio --example sync_probe -- --mic Chu2 --volume --set 100 --unmute
//! cargo run -p stereo-audio --example sync_probe -- --mic Chu2 --meter --seconds 15
//! cargo run -p stereo-audio --example sync_probe -- --mic Chu2 --exclusive --rate 48000
//! ```
//!
//! * `--check`：对 `--mic` 指向的 **capture** 端点做两次采集 —— 静音基线、以及
//!   同时在 `--speaker` 上播 1 kHz 满幅音 —— 打印原始 RMS / peak。两次的差异
//!   就是「本机麦克风能否采到真实声音」的判据。
//! * `--loopback`：对照实验，回环采 `--speaker` 这个 **render** 端点，证明数字流
//!   确实送到了设备（与麦克风无关）。
//! * `--volume`：直接问 `IAudioEndpointVolume` 要静音 / 音量 / 硬件支持位，
//!   可加 `--set <0..100>` 把音量拉到指定百分比、`--unmute` 取消静音。
//! * `--meter`：实时电平表，每 200 ms 打一行瞬时 RMS / peak / 累计峰值 / LSB 分布，
//!   同时读 `IAudioMeterInformation::GetPeakValue()`（音频引擎自己的表）做交叉验证。
//! * `--exclusive`：**独占模式**裸 WASAPI 采集（绕过音频引擎与任何重采样路径），
//!   并统计 `AUDCLNT_BUFFERFLAGS_SILENT` 包的占比。
//! * `--rate <hz>`：采集/播放采样率（默认 44100；用 48000 可绕开重采样路径）。

use std::time::{Duration, Instant};

use stereo_audio::sync::{
    calibrate_pair, calibrate_pair_sequential, check_alignment, list_microphones, measure_comb,
    measure_polarity, mic_health, play_alignment_clicks, scan_alignment, CombResult,
    PolarityResult, SyncConfig, SyncProbe,
};
use stereo_audio::{
    build_sine_s16, list_endpoints, play_pcm_s16, AudioCapture, CaptureConfig, CaptureMode,
    DataFlow, DeviceSelector, PcmFormat,
};

use windows::core::{IUnknown, HSTRING};
use windows::Win32::Media::Audio::{
    IAudioCaptureClient, IAudioClient, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator,
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_EXCLUSIVE, AUDCLNT_S_BUFFER_EMPTY,
    WAVE_FORMAT_PCM, WAVEFORMATEX,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, COINIT_MULTITHREADED, CLSCTX_ALL,
};

// `IAudioEndpointVolume` / `IAudioMeterInformation` 在 windows crate 里被
// `Win32_Media_Audio_Endpoints` feature 门控，而本 crate 的 Cargo.toml 没有开这个 feature
// （写作用域也不允许改 Cargo.toml）。这里按 `endpoint_format.rs` 的做法本地声明同名接口：
// IID 与方法顺序严格照 Windows SDK 的 endpointvolume.h / endpointvolume.h(meter)，
// **顺序错了会直接崩，不要调整**。
#[windows::core::interface("5cdf2c82-841e-4546-9722-0cf74078229a")]
unsafe trait IAudioEndpointVolumeLocal: windows::core::IUnknown {
    unsafe fn register_control_change_notify(
        &self,
        pnotify: *mut core::ffi::c_void,
    ) -> windows::core::Result<()>;
    unsafe fn unregister_control_change_notify(
        &self,
        pnotify: *mut core::ffi::c_void,
    ) -> windows::core::Result<()>;
    unsafe fn get_channel_count(&self, pnchannelcount: *mut u32) -> windows::core::Result<()>;
    unsafe fn set_master_volume_level(
        &self,
        fleveldb: f32,
        pguideventcontext: *const windows::core::GUID,
    ) -> windows::core::Result<()>;
    unsafe fn set_master_volume_level_scalar(
        &self,
        flevel: f32,
        pguideventcontext: *const windows::core::GUID,
    ) -> windows::core::Result<()>;
    unsafe fn get_master_volume_level(&self, pfleveldb: *mut f32) -> windows::core::Result<()>;
    unsafe fn get_master_volume_level_scalar(&self, pflevel: *mut f32) -> windows::core::Result<()>;
    unsafe fn set_channel_volume_level(
        &self,
        nchannel: u32,
        fleveldb: f32,
        pguideventcontext: *const windows::core::GUID,
    ) -> windows::core::Result<()>;
    unsafe fn set_channel_volume_level_scalar(
        &self,
        nchannel: u32,
        flevel: f32,
        pguideventcontext: *const windows::core::GUID,
    ) -> windows::core::Result<()>;
    unsafe fn get_channel_volume_level(
        &self,
        nchannel: u32,
        pfleveldb: *mut f32,
    ) -> windows::core::Result<()>;
    unsafe fn get_channel_volume_level_scalar(
        &self,
        nchannel: u32,
        pflevel: *mut f32,
    ) -> windows::core::Result<()>;
    unsafe fn set_mute(
        &self,
        bmute: i32,
        pguideventcontext: *const windows::core::GUID,
    ) -> windows::core::Result<()>;
    unsafe fn get_mute(&self, pbmute: *mut i32) -> windows::core::Result<()>;
    unsafe fn get_volume_step_info(
        &self,
        pnstep: *mut u32,
        pnstepcount: *mut u32,
    ) -> windows::core::Result<()>;
    unsafe fn volume_step_up(
        &self,
        pguideventcontext: *const windows::core::GUID,
    ) -> windows::core::Result<()>;
    unsafe fn volume_step_down(
        &self,
        pguideventcontext: *const windows::core::GUID,
    ) -> windows::core::Result<()>;
    unsafe fn query_hardware_support(
        &self,
        pdwhardwaresupportmask: *mut u32,
    ) -> windows::core::Result<()>;
    unsafe fn get_volume_range(
        &self,
        pflvolumemindb: *mut f32,
        pflvolumemaxdb: *mut f32,
        pflvolumeincrementdb: *mut f32,
    ) -> windows::core::Result<()>;
}

#[windows::core::interface("c02216f6-8c67-4b5b-9d00-d008e73e0064")]
unsafe trait IAudioMeterInformationLocal: windows::core::IUnknown {
    unsafe fn get_peak_value(&self, pfpeak: *mut f32) -> windows::core::Result<()>;
    unsafe fn get_metering_channel_count(&self, pnchannelcount: *mut u32)
    -> windows::core::Result<()>;
    unsafe fn get_channels_peak_values(
        &self,
        u32channelcount: u32,
        afpeakvalues: *mut f32,
    ) -> windows::core::Result<()>;
    unsafe fn query_hardware_support(
        &self,
        pdwhardwaresupportmask: *mut u32,
    ) -> windows::core::Result<()>;
}

const RATE: u32 = 44_100;
const CHANNELS: u16 = 2;

type AnyErr = Box<dyn std::error::Error>;

fn arg_value(args: &[String], key: &str) -> Option<String> {
    let idx = args.iter().position(|a| a == key)?;
    args.get(idx + 1).cloned()
}

fn has_flag(args: &[String], key: &str) -> bool {
    args.iter().any(|a| a == key)
}

fn db(x: f64) -> f64 {
    if x <= 0.0 {
        f64::NEG_INFINITY
    } else {
        20.0 * x.log10()
    }
}

// ---------------------------------------------------------------- 采样统计

/// 一段交错 s16 立体声的统计量。
struct Stats {
    rms_l: f64,
    peak_l: f64,
    rms_r: f64,
    peak_r: f64,
    bytes: usize,
}

impl Stats {
    fn of(bytes: &[u8]) -> Self {
        let mut sum_l = 0.0f64;
        let mut sum_r = 0.0f64;
        let mut peak_l = 0.0f64;
        let mut peak_r = 0.0f64;
        let mut n = 0usize;
        for frame in bytes.chunks_exact(4) {
            let l = i16::from_le_bytes([frame[0], frame[1]]) as f64 / 32_768.0;
            let r = i16::from_le_bytes([frame[2], frame[3]]) as f64 / 32_768.0;
            sum_l += l * l;
            sum_r += r * r;
            peak_l = peak_l.max(l.abs());
            peak_r = peak_r.max(r.abs());
            n += 1;
        }
        let d = n.max(1) as f64;
        Self {
            rms_l: (sum_l / d).sqrt(),
            peak_l,
            rms_r: (sum_r / d).sqrt(),
            peak_r,
            bytes: bytes.len(),
        }
    }

    fn max_rms(&self) -> f64 {
        self.rms_l.max(self.rms_r)
    }

    fn max_peak(&self) -> f64 {
        self.peak_l.max(self.peak_r)
    }

    fn describe(&self, label: &str) -> String {
        format!(
            "{label}: {} 字节 | L rms={:.6} peak={:.6} ({:.1} dBFS) | R rms={:.6} peak={:.6} ({:.1} dBFS)",
            self.bytes,
            self.rms_l,
            self.peak_l,
            db(self.peak_l),
            self.rms_r,
            self.peak_r,
            db(self.peak_r),
        )
    }
}

/// 交错 s16 流里「样本值分布」的画像：最小 / 最大 / 不同取值个数 / ±2 LSB 内的占比。
///
/// ±1~2 LSB 且不同取值个位数 = 设备在送数字零；取值铺开到几十上百 LSB 才是真实 ADC 噪声。
struct LsbProfile {
    min: i16,
    max: i16,
    distinct: usize,
    frac_tiny: f64,
    samples: usize,
}

impl LsbProfile {
    fn of(bytes: &[u8]) -> Self {
        let mut min = i16::MAX;
        let mut max = i16::MIN;
        let mut distinct: Vec<i16> = Vec::new();
        let mut tiny = 0usize;
        let mut n = 0usize;
        for s in bytes.chunks_exact(2) {
            let v = i16::from_le_bytes([s[0], s[1]]);
            min = min.min(v);
            max = max.max(v);
            if v.abs() <= 2 {
                tiny += 1;
            }
            if distinct.len() < 64 && !distinct.contains(&v) {
                distinct.push(v);
            }
            n += 1;
        }
        if n == 0 {
            return Self {
                min: 0,
                max: 0,
                distinct: 0,
                frac_tiny: 0.0,
                samples: 0,
            };
        }
        Self {
            min,
            max,
            distinct: distinct.len(),
            frac_tiny: tiny as f64 / n as f64,
            samples: n,
        }
    }

    fn describe(&self) -> String {
        format!(
            "min={} max={} 不同取值≤{}{} | |x|≤2LSB 占比={:.4} | 样本数={}",
            self.min,
            self.max,
            self.distinct,
            if self.distinct >= 64 { "+" } else { "" },
            self.frac_tiny,
            self.samples
        )
    }
}

// ---------------------------------------------------------------- COM 小工具

/// 初始化 COM（已初始化过也算成功）。
fn com_init() -> Result<(), AnyErr> {
    let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    if hr.is_ok() || hr.0 == 0x8001_0106u32 as i32 {
        Ok(())
    } else {
        Err(format!("CoInitializeEx 失败：HRESULT 0x{:08X}", hr.0).into())
    }
}

fn enumerator() -> Result<IMMDeviceEnumerator, AnyErr> {
    let en: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None::<&IUnknown>, CLSCTX_ALL) }?;
    Ok(en)
}

/// 按「名字子串」在 `flow` 方向的活动端点里找 `IMMDevice`。
fn device_by_name(name: &str, flow: DataFlow) -> Result<IMMDevice, AnyErr> {
    let eps = list_endpoints()?;
    let ep = eps
        .iter()
        .find(|e| e.flow == flow && e.name.contains(name))
        .ok_or_else(|| format!("找不到端点（方向 {:?}）：{name}", flow))?;
    println!("  解析端点：{} [{}]", ep.name, ep.id);
    let en = enumerator()?;
    Ok(unsafe { en.GetDevice(&HSTRING::from(ep.id.as_str())) }?)
}

/// 打开端点的 `IAudioEndpointVolume`（静音 / 音量 / 硬件支持）。
fn activate_volume(dev: &IMMDevice) -> Result<IAudioEndpointVolumeLocal, AnyErr> {
    Ok(unsafe { dev.Activate::<IAudioEndpointVolumeLocal>(CLSCTX_ALL, None) }?)
}

/// 打开端点的 `IAudioMeterInformation`（音频引擎的峰值表）。
fn activate_meter(dev: &IMMDevice) -> Result<IAudioMeterInformationLocal, AnyErr> {
    Ok(unsafe { dev.Activate::<IAudioMeterInformationLocal>(CLSCTX_ALL, None) }?)
}

// ---------------------------------------------------------------- --volume

fn volume_mode(
    mic: &str,
    flow: DataFlow,
    set: Option<f32>,
    unmute: bool,
) -> Result<(), AnyErr> {
    com_init()?;
    let dev = device_by_name(mic, flow)?;
    let vol = activate_volume(&dev)?;
    unsafe {
        let mut ch = 0u32;
        let _ = vol.get_channel_count(&mut ch);
        let mut mute_raw = 0i32;
        let mute_ok = vol.get_mute(&mut mute_raw).is_ok();
        let mut scalar = f32::NAN;
        let _ = vol.get_master_volume_level_scalar(&mut scalar);
        let mut dbv = f32::NAN;
        let _ = vol.get_master_volume_level(&mut dbv);
        let mut hw = 0u32;
        let _ = vol.query_hardware_support(&mut hw);
        println!("-- IAudioEndpointVolume --");
        println!("  通道数        : {ch}");
        println!(
            "  GetMute       : {}  ({})",
            mute_raw != 0,
            if mute_ok { "读取成功" } else { "读取失败" }
        );
        println!("  音量 (标量)   : {scalar:.4}  ({:.1} %)", scalar * 100.0);
        println!("  音量 (dB)     : {dbv:.2}");
        println!(
            "  硬件支持位    : 0x{hw:02X}  (0x1=音量 0x2=静音 0x4=电平表)  → 音量={} 静音={} 电平表={}",
            hw & 0x1 != 0,
            hw & 0x2 != 0,
            hw & 0x4 != 0
        );
        let mut lo = 0.0f32;
        let mut hi = 0.0f32;
        let mut inc = 0.0f32;
        if vol.get_volume_range(&mut lo, &mut hi, &mut inc).is_ok() {
            println!("  音量范围      : {lo:.1} dB .. {hi:.1} dB  步进 {inc:.2} dB");
        }

        if set.is_some() || unmute {
            let null_ctx: *const windows::core::GUID = std::ptr::null();
            if unmute {
                vol.set_mute(0, null_ctx)?;
                println!("  → 已取消静音");
            }
            if let Some(pct) = set {
                let s = (pct / 100.0).clamp(0.0, 1.0);
                vol.set_master_volume_level_scalar(s, null_ctx)?;
                println!("  → 已把音量设为 {:.1} %", s * 100.0);
            }
            let mut m2 = 0i32;
            let _ = vol.get_mute(&mut m2);
            let mut s2 = f32::NAN;
            let _ = vol.get_master_volume_level_scalar(&mut s2);
            println!(
                "  写入后：mute={} 音量={:.4} ({:.1} %)",
                m2 != 0,
                s2,
                s2 * 100.0
            );
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- --meter

fn meter_mode(mic: &str, seconds: f64, rate: u32) -> Result<(), AnyErr> {
    let cfg = CaptureConfig {
        device: DeviceSelector::name_contains(mic),
        samplerate: rate,
        channels: CHANNELS,
        mode: CaptureMode::Capture,
        ring_seconds: 5.0,
        buffer_ms: 100,
    };
    let mut cap = AudioCapture::new(cfg)?;
    cap.start()?;
    if let Some(n) = cap.negotiated_format() {
        println!("  采集端点：{}", n.describe());
    }

    // 音频引擎自己的峰值表（与 ring 里的数据是两条独立路径）。
    let hw_meter = com_init()
        .ok()
        .and_then(|_| device_by_name(mic, DataFlow::Capture).ok())
        .and_then(|d| activate_meter(&d).ok());
    match &hw_meter {
        Some(_) => println!("  已打开 IAudioMeterInformation（引擎峰值表）"),
        None => println!("  IAudioMeterInformation 不可用"),
    }

    println!("-- 实时电平表：{seconds:.0} 秒，每 200 ms 一行（请对着麦克风说话 / 敲击）--");
    let t0 = Instant::now();
    let mut all: Vec<u8> = Vec::new();
    let mut win: Vec<u8> = Vec::new();
    let mut cum_peak = 0.0f64;
    let mut last = Instant::now();
    let mut row = 0usize;

    while t0.elapsed() < Duration::from_secs_f64(seconds) {
        let chunk = cap.read_vec(64 * 1024, Duration::from_millis(50))?;
        if !chunk.is_empty() {
            win.extend_from_slice(&chunk);
            all.extend_from_slice(&chunk);
        }
        if last.elapsed() >= Duration::from_millis(200) {
            last = Instant::now();
            row += 1;
            let s = Stats::of(&win);
            cum_peak = cum_peak.max(s.max_peak());
            let hw = hw_meter
                .as_ref()
                .map(|m| {
                    let mut p = f32::NAN;
                    let _ = unsafe { m.get_peak_value(&mut p) };
                    p
                })
                .unwrap_or(f32::NAN);
            println!(
                "[{:>5.1}s] #{row:<3} 瞬时 rms={:.6} peak={:.6} ({:>7.1} dBFS) | 累计峰值={:.6} ({:.1} dBFS) | 引擎表={:.6} | {}",
                t0.elapsed().as_secs_f64(),
                s.max_rms(),
                s.max_peak(),
                db(s.max_peak()),
                cum_peak,
                db(cum_peak),
                hw,
                LsbProfile::of(&win).describe(),
            );
            win.clear();
        }
    }
    cap.stop();
    if let Some(e) = cap.last_error() {
        println!("  采集线程错误：{e}");
    }
    println!("-- 汇总（全部 {} 字节）--", all.len());
    let all_stats = Stats::of(&all);
    println!("{}", all_stats.describe("整段    "));
    println!("  LSB 画像：{}", LsbProfile::of(&all).describe());
    println!(
        "  判据：整段峰值 {:.1} dBFS —— > -45 dBFS 视为「有信号」，-45..-80 视为「只有本底噪声」，< -80 视为「静默」",
        db(all_stats.max_peak())
    );
    Ok(())
}

// ---------------------------------------------------------------- --exclusive

/// 写一个最小 WAV 文件（PCM 整数，小端）。
fn write_wav(
    path: &str,
    pcm: &[u8],
    rate: u32,
    channels: u16,
    bits: u16,
) -> Result<(), AnyErr> {
    let block = channels as u32 * (bits as u32 / 8);
    let data_len = pcm.len() as u32;
    let mut out = Vec::with_capacity(44 + pcm.len());
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * block).to_le_bytes());
    out.extend_from_slice(&(block as u16).to_le_bytes());
    out.extend_from_slice(&bits.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    out.extend_from_slice(pcm);
    std::fs::write(path, &out)?;
    println!("  已写出 WAV：{path}（{} 字节 PCM）", pcm.len());
    Ok(())
}

/// 独占模式裸 WASAPI 采集：绕过音频引擎、任何 APO 与重采样，拿到设备最原始的数据。
///
/// 传入 `play` 时会在后台线程同时往该 render 端点播 1 kHz 正弦，用来做「正对照」：
/// 只有播音时录音电平明显抬升，才能证明这条采集链路真的连着物理麦克风。
fn exclusive_mode(
    mic: &str,
    rate: u32,
    channels: u16,
    bits: u16,
    seconds: f64,
    live: bool,
    wav: Option<&str>,
    play: Option<(&str, f64, f32)>,
) -> Result<(), AnyErr> {
    com_init()?;
    let dev = device_by_name(mic, DataFlow::Capture)?;
    let block = channels as u32 * (bits as u32 / 8);

    unsafe {
        let client: IAudioClient = dev.Activate(CLSCTX_ALL, None)?;
        let fmt = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_PCM as u16,
            nChannels: channels,
            nSamplesPerSec: rate,
            nAvgBytesPerSec: rate * block,
            nBlockAlign: block as u16,
            wBitsPerSample: bits,
            cbSize: 0,
        };
        let hns = 200 * 10_000i64; // 200 ms
        println!(
            "  请求独占格式：{rate} Hz / {channels} ch / {bits}-bit（块对齐 {block} 字节），缓冲 200 ms"
        );
        if let Err(e) = client.Initialize(AUDCLNT_SHAREMODE_EXCLUSIVE, 0, hns, hns, &fmt, None) {
            println!(
                "  独占 Initialize 失败：HRESULT 0x{:08X} — {}",
                e.code().0,
                e.message()
            );
            println!("  （0x88890008=格式不支持 0x8889000A=设备被占用 0x88890019=缓冲长度未对齐）");
            return Ok(());
        }
        let cap: IAudioCaptureClient = client.GetService()?;
        client.Start()?;
        println!("  独占流已启动，采集 {seconds:.0} 秒…");
        if live {
            println!("-- 实时（独占）电平表，每 200 ms 一行 --");
        }

        let mut all: Vec<u8> = Vec::new();
        let mut win: Vec<u8> = Vec::new();
        let mut packets = 0u64;
        let mut silent_frames = 0u64;
        let mut data_frames = 0u64;
        let mut cum_peak = 0i32;
        let t0 = Instant::now();
        let mut last = Instant::now();
        let deadline = t0 + Duration::from_secs_f64(seconds);
        let mut played = false;
        while Instant::now() < deadline {
            if !played {
                if let Some((sp, freq, amp)) = play {
                    played = true;
                    println!("  正对照：同时往「{sp}」播 {freq:.0} Hz @ {amp:.2}（后台线程）");
                    let sp = sp.to_string();
                    std::thread::spawn(move || {
                        let sel = DeviceSelector::name_contains(sp.clone());
                        let fmt = PcmFormat::new(48_000, 2, 16);
                        let pcm = build_sine_s16(freq, seconds + 1.0, amp, fmt);
                        match play_pcm_s16(&sel, fmt, &pcm) {
                            Ok(info) => println!("  [播放] 完成：{}", info.describe()),
                            Err(e) => println!("  [播放] 失败：{e}"),
                        }
                    });
                }
            }
            let mut pdata: *mut u8 = std::ptr::null_mut();
            let mut frames: u32 = 0;
            let mut flags: u32 = 0;
            match cap.GetBuffer(&mut pdata, &mut frames, &mut flags, None, None) {
                Ok(()) => {
                    packets += 1;
                    let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
                    if silent {
                        silent_frames += frames as u64;
                    } else {
                        data_frames += frames as u64;
                    }
                    if frames > 0 && !pdata.is_null() && !silent {
                        let n = frames as usize * block as usize;
                        let slice = std::slice::from_raw_parts(pdata, n);
                        all.extend_from_slice(slice);
                        if live {
                            win.extend_from_slice(slice);
                        }
                    }
                    let _ = cap.ReleaseBuffer(frames);
                }
                Err(e) => {
                    let c = e.code().0;
                    if c == AUDCLNT_S_BUFFER_EMPTY.0 {
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    println!("  GetBuffer 错误：HRESULT 0x{c:08X} — {}", e.message());
                    break;
                }
            }
            if live && last.elapsed() >= Duration::from_millis(200) {
                last = Instant::now();
                let prof = LsbProfile::of(&win);
                let peak = prof.min.unsigned_abs().max(prof.max.unsigned_abs()) as i32;
                cum_peak = cum_peak.max(peak);
                println!(
                    "[{:>5.1}s] 本窗峰值={peak:>6} LSB ({:>7.2} dBFS) | 累计峰值={cum_peak:>6} LSB ({:.2} dBFS) | {}",
                    t0.elapsed().as_secs_f64(),
                    db(peak as f64 / 32_768.0),
                    db(cum_peak as f64 / 32_768.0),
                    prof.describe(),
                );
                win.clear();
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let _ = client.Stop();
        println!("-- 独占采集结果 --");
        println!(
            "  包数={packets}  有数据帧={data_frames}  SILENT 标志帧={silent_frames}  取回字节={}",
            all.len()
        );
        if let Some(path) = wav {
            write_wav(path, &all, rate, channels, bits)?;
        }
        if all.is_empty() {
            println!("  没有取回任何非 SILENT 数据。");
            return Ok(());
        }
        let prof = LsbProfile::of(&all);
        let peak_lsb = prof.min.unsigned_abs().max(prof.max.unsigned_abs());
        println!("  LSB 画像：{}", prof.describe());
        println!(
            "  原始峰值 = {peak_lsb} LSB  (16-bit 满幅=32768, 即 {:.2} dBFS)",
            db(peak_lsb as f64 / 32_768.0)
        );
        // 整段峰值会被「开流瞬间的直流建立尖峰」污染，所以再分段报 RMS，
        // 让「开头一次性尖峰」和「稳态本底」区分开。
        let s = bits as usize / 8;
        let frames_total = all.len() / block as usize;
        let step = (rate as usize / 10).max(1) * block as usize; // 100 ms
        let mut i = 0usize;
        let mut seg = 0usize;
        println!("  100 ms 分段 RMS（LSB）——尖峰只出现在开头就说明不是声音：");
        while i < all.len() && seg < 60 {
            let end = (i + step).min(all.len());
            let mut acc = 0f64;
            let mut cnt = 0usize;
            let mut pk = 0i32;
            let mut j = i;
            while j + s <= end {
                let v = i16::from_le_bytes([all[j], all[j + 1]]) as i32;
                acc += (v as f64) * (v as f64);
                cnt += 1;
                pk = pk.max(v.abs());
                j += s;
            }
            if cnt == 0 {
                break;
            }
            let rms = (acc / cnt as f64).sqrt();
            print!("    [{:>4.1}s] rms={rms:>8.1} peak={pk:>6}", seg as f64 / 10.0);
            if seg % 5 == 4 {
                println!();
            }
            i = end;
            seg += 1;
        }
        println!();
        println!("  总帧数={frames_total}");
        Ok(())
    }
}

/// 直接轮询端点的 `IAudioMeterInformation`（音频引擎的峰值表，不开任何流）。
fn hwmeter_mode(name: &str, flow: DataFlow, seconds: f64) -> Result<(), AnyErr> {
    com_init()?;
    let dev = device_by_name(name, flow)?;
    let m = activate_meter(&dev)?;
    println!("-- IAudioMeterInformation 轮询（{flow:?}），每 200 ms 一行 --");
    let t0 = Instant::now();
    let mut cum = 0.0f32;
    while t0.elapsed() < Duration::from_secs_f64(seconds) {
        let mut p = f32::NAN;
        let _ = unsafe { m.get_peak_value(&mut p) };
        if p.is_finite() {
            cum = cum.max(p);
        }
        println!(
            "[{:>5.1}s] 瞬时峰值={:.6} ({:>7.2} dBFS) | 累计峰值={:.6} ({:.2} dBFS)",
            t0.elapsed().as_secs_f64(),
            p,
            db(p as f64),
            cum,
            db(cum as f64)
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    Ok(())
}

// ---------------------------------------------------------------- --check / --loopback

/// 采集 `seconds` 秒；若 `play` 为 `Some`，在采集期间并行播 1 kHz 满幅音。
fn capture_window(
    device: &str,
    seconds: f64,
    mode: CaptureMode,
    rate: u32,
    play: Option<(&str, f64, f32)>,
    wav: Option<&str>,
) -> Result<Stats, AnyErr> {
    let cfg = CaptureConfig {
        device: DeviceSelector::name_contains(device),
        samplerate: rate,
        channels: CHANNELS,
        mode,
        ring_seconds: 5.0,
        buffer_ms: 200,
    };
    let mut cap = AudioCapture::new(cfg)?;
    cap.start()?;
    if let Some(n) = cap.negotiated_format() {
        println!("  采集端点：{}", n.describe());
    }

    // 丢弃最初 300ms（流刚起来，可能有端点上电噪声 / 未对齐的旧数据）
    std::thread::sleep(Duration::from_millis(300));
    let _ = cap.read_vec(64 * 1024, Duration::from_millis(200));

    let player = play.map(|(speaker, tone_seconds, amp)| {
        let speaker = speaker.to_string();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            let fmt = PcmFormat::new(rate, CHANNELS, 16);
            let pcm = build_sine_s16(1000.0, tone_seconds, amp, fmt);
            match play_pcm_s16(&DeviceSelector::name_contains(speaker), fmt, &pcm) {
                Ok(info) => println!("  播放端点：{}", info.describe()),
                Err(e) => println!("  播放失败：{e}"),
            }
        })
    });

    let want_bytes = (seconds * rate as f64) as usize * CHANNELS as usize * 2;
    let mut all: Vec<u8> = Vec::with_capacity(want_bytes);
    let deadline = Instant::now() + Duration::from_secs_f64(seconds);
    while all.len() < want_bytes && Instant::now() < deadline {
        let chunk = cap.read_vec(16 * 1024, Duration::from_millis(200))?;
        if chunk.is_empty() && cap.last_error().is_some() {
            break;
        }
        all.extend_from_slice(&chunk);
    }
    cap.stop();
    if let Some(h) = player {
        let _ = h.join();
    }
    if let Some(e) = cap.last_error() {
        println!("  采集线程错误：{e}");
    }
    if let Some(path) = wav {
        write_wav(path, &all, rate, CHANNELS, 16)?;
    }
    Ok(Stats::of(&all))
}

fn check_mode(
    mic: &str,
    speaker: Option<&str>,
    rate: u32,
    wav: Option<&str>,
) -> Result<(), AnyErr> {
    println!("== 麦克风真实性检查：{mic} @ {rate} Hz ==");
    let silent = capture_window(mic, 2.0, CaptureMode::Capture, rate, None, None)?;
    println!("{}", silent.describe("静音基线"));
    let loud = match speaker {
        Some(sp) => {
            let s = capture_window(
                mic,
                4.0,
                CaptureMode::Capture,
                rate,
                Some((sp, 4.0, 0.9)),
                wav,
            )?;
            println!("{}", s.describe("播音中  "));
            Some(s)
        }
        None => None,
    };
    println!("-- 结论 --");
    println!(
        "静音 max_rms={:.8} max_peak={:.8} ({:.1} dBFS) = {:.2} LSB(s16)",
        silent.max_rms(),
        silent.max_peak(),
        db(silent.max_peak()),
        silent.max_peak() * 32_768.0
    );
    if let Some(loud) = loud {
        println!(
            "有声 max_rms={:.8} max_peak={:.8} ({:.1} dBFS) = {:.2} LSB(s16)",
            loud.max_rms(),
            loud.max_peak(),
            db(loud.max_peak()),
            loud.max_peak() * 32_768.0
        );
        println!(
            "RMS 比值（有声/静音）= {:.3}  (>3.0 视为麦克风工作)",
            if silent.max_rms() > 0.0 {
                loud.max_rms() / silent.max_rms()
            } else {
                f64::INFINITY
            }
        );
    }
    Ok(())
}

fn loopback_mode(speaker: &str, rate: u32) -> Result<(), AnyErr> {
    println!("== 回环对照：render={speaker} @ {rate} Hz ==");
    let silent = capture_window(speaker, 2.0, CaptureMode::Loopback, rate, None, None)?;
    println!("{}", silent.describe("静音基线"));
    let loud = capture_window(
        speaker,
        2.0,
        CaptureMode::Loopback,
        rate,
        Some((speaker, 1.5, 0.9)),
        None,
    )?;
    println!("{}", loud.describe("播音中  "));
    println!(
        "回环 RMS 比值 = {:.1}",
        loud.max_rms() / silent.max_rms().max(1e-12)
    );
    Ok(())
}

// ---------------------------------------------------------------- main

/// `--health`：用 `sync::mic_health` 给（一只或全部）采集端点做体检。
///
/// 走的是 `sync::capture_mono`，即**先试独占模式、失败再退回共享模式**——这一点很关键：
/// 本机共享模式在 Chu2 DSP 上是一条 0.48 LSB 的平底，独占模式才拿得到真实数据。
fn health_mode(only: Option<&str>, ms: u32) -> Result<(), AnyErr> {
    if let Some(name) = only {
        let h = mic_health(name, ms)?;
        println!("== 麦克风体检：{name}（{ms} ms）==");
        println!("  {}", h.describe());
        println!("  建议：{}", h.verdict.advice());
        return Ok(());
    }

    println!("== 本机活动采集端点体检（各 {ms} ms）==");
    let cands = list_microphones();
    if cands.is_empty() {
        println!("  （一个活动采集端点都没有）");
        return Ok(());
    }
    for c in &cands {
        print!("  [{}] {} … ", c.kind.as_str(), c.endpoint.name);
        match mic_health(c.name(), ms) {
            Ok(h) => println!("{}", h.describe()),
            Err(e) => println!("调用失败：{e}"),
        }
    }
    Ok(())
}

/// `--calibrate`：真机自动标定（先体检，再测左右两路）。
///
/// 默认走同时法（两路同时播、同一段录音，精度 ≤1 ms）；给 `--sequential` 走依次法对照
/// ——本机实测同一个有线端点自比，依次法会给出 -4.0 ~ +3.0 ms 的假差值。
fn calibrate_mode(
    mic: &str,
    left: &str,
    right: &str,
    cfg: &SyncConfig,
    sequential: bool,
) -> Result<(), AnyErr> {
    println!("== 自动标定（{}）==", if sequential { "依次法" } else { "同时法" });
    println!("  麦克风：{mic}");
    println!("  左路：{left}");
    println!("  右路：{right}");
    println!("  {}", cfg.describe());

    println!("  先给麦克风做体检…");
    let h = mic_health(mic, 1_000)?;
    println!("  {}", h.describe());
    if !h.verdict.is_usable() {
        println!(
            "  麦克风不可用（{}），中止自动标定。请改用 --clicks 手动对齐。",
            h.verdict.as_str()
        );
        return Ok(());
    }

    let t0 = Instant::now();
    let r = if sequential {
        calibrate_pair_sequential(mic, left, right, cfg)?
    } else {
        calibrate_pair(mic, left, right, cfg)?
    };
    println!("  耗时 {:.1}s", t0.elapsed().as_secs_f64());
    println!("  {}", r.describe());
    let (dl, dr) = r.delays();
    println!("  建议延迟：左 {dl} ms / 右 {dr} ms");
    Ok(())
}

/// `--clicks`：手动对齐试听（不需要麦克风）。
fn clicks_mode(left: &str, right: &str, ld: u32, rd: u32, seconds: f64) -> Result<(), AnyErr> {
    println!("== 手动对齐试听 ==");
    println!("  左：{left}  预垫 {ld} ms");
    println!("  右：{right}  预垫 {rd} ms");
    println!("  时长：{seconds:.1}s");
    println!("  （听到「嗒—嗒」两下就是没对齐，调延迟值再试；重合成一下就对齐了）");
    play_alignment_clicks(left, right, ld, rd, seconds)?;
    println!("  播放完成。");
    Ok(())
}

/// `--polarity`：声学诊断两路之间的**相对极性**与**梳状滤波**。
///
/// 会真实从两只箱放出测试音。只给 `--left` 时做单端点测量；`--pair-only` 只做两次单箱测量
/// （快，但拿不到时间差）。
fn polarity_mode(
    mic: &str,
    left: &str,
    right: Option<&str>,
    cfg: &SyncConfig,
    pair_only: bool,
) -> Result<(), AnyErr> {
    println!("== 极性与梳状滤波诊断 ==");
    println!("  麦克风：{mic}");
    println!("  左路：{left}");
    if let Some(r) = right {
        println!("  右路：{r}");
    }
    println!("  {}", cfg.describe());

    println!("  先给麦克风做体检…");
    let h = mic_health(mic, 1_000)?;
    println!("  {}", h.describe());
    if !h.verdict.is_usable() {
        println!(
            "  麦克风不可用（{}），无法做声学诊断。",
            h.verdict.as_str()
        );
        return Ok(());
    }

    let Some(right) = right else {
        println!("  单端点测量（放一次测试音）…");
        let t0 = Instant::now();
        let p = measure_polarity(mic, left, cfg)?;
        println!("  耗时 {:.1}s", t0.elapsed().as_secs_f64());
        println!("  {}", p.describe());
        print_peaks(&p);
        println!(
            "  注意：单端点只能给出**带符号相关**这一个数；要判断「两只箱是否反相」必须用同样的\
             参考信号测另一个端点，比较两次的符号。"
        );
        return Ok(());
    };

    if pair_only {
        println!("  依次测两路的相对极性（只放两次测试音）…");
        let t0 = Instant::now();
        let (l, r) = stereo_audio::sync::measure_pair_polarity(mic, left, right, cfg)?;
        println!("  耗时 {:.1}s", t0.elapsed().as_secs_f64());
        println!("  左路：{}", l.describe());
        print_peaks(&l);
        println!("  右路：{}", r.describe());
        print_peaks(&r);
        println!("  {}", polarity_note(&l, &r));
        return Ok(());
    }

    println!("  完整诊断：左单独 / 右单独 / 两路同时，共三段测试音…");
    let t0 = Instant::now();
    let c: CombResult = measure_comb(mic, left, right, cfg)?;
    println!("  耗时 {:.1}s", t0.elapsed().as_secs_f64());
    println!();
    println!("{}", c.describe());
    println!();
    {
        let mut line = String::new();
        for (ms, v) in &c.both_peaks {
            line.push_str(&format!(" {ms:.3}ms/{v:+.3}"));
        }
        println!("  两路同时播的相关峰（滞后/带符号值）：{line}");
        if c.both_peaks.len() >= 2 {
            let gap = (c.both_peaks[1].0 - c.both_peaks[0].0).abs();
            if gap > 20.0 {
                println!(
                    "  ⚠ 两个峰相距 {gap:.1} ms（>20 ms）→ 两路在时间上**基本不重叠**，\
                     这不是梳状滤波而是回声；此时「合成幅度比」没有意义。"
                );
            }
        }
    }
    println!();
    println!("  逐频点梳状曲线（ratio = 合成幅度 / 单只幅度之和）：");
    println!("     频率Hz      左        右       合成      ratio");
    for p in &c.curve {
        println!(
            "  {:>9.0}  {:8.5}  {:8.5}  {:8.5}   {:6.3}",
            p.freq_hz, p.left, p.right, p.both, p.ratio
        );
    }
    println!();
    println!("  {}", polarity_note(&c.left, &c.right));
    match c.verdict {
        stereo_audio::sync::CombVerdict::Aligned => {
            println!("  建议：不需要改动。若用户仍觉得人声低，问题不在两路相位/时间差。")
        }
        stereo_audio::sync::CombVerdict::Inverted => {
            println!("  建议：把其中一只箱的极性翻转（交换扬声器线序，或在软件里对该路取反）。")
        }
        stereo_audio::sync::CombVerdict::CombFiltered => {
            println!(
                "  建议：用 --calibrate 测出延迟差，再用 --clicks 手动微调到两下重合成一下。"
            )
        }
        stereo_audio::sync::CombVerdict::InvertedAndComb => {
            println!("  建议：先翻转极性，再用 --clicks 对齐时间。")
        }
    }
    Ok(())
}

/// `--align-scan`：左右同时播同一段单频正弦，相对延迟从 `--from` 扫到 `--to`，测合成幅度。
///
/// 直接回答「当前配置里的延迟值对不对」：合成幅度最大的那个延迟才是真正的对齐点，
/// `--current` 给出配置里的值，会算出它比最佳点低多少 dB。
fn align_scan_mode(
    mic: &str,
    left: &str,
    right: &str,
    cfg: &SyncConfig,
    tone_hz: f64,
    from_ms: f64,
    to_ms: f64,
    step_ms: f64,
    current_ms: Option<f64>,
) -> Result<(), AnyErr> {
    println!("== 相对延迟扫描（对齐点搜索）==");
    println!("  麦克风：{mic}");
    println!("  左路：{left}");
    println!("  右路：{right}");
    println!("  {}", cfg.describe());
    println!(
        "  探测单音：{tone_hz} Hz | 范围 {from_ms} → {to_ms} ms | 步进 {step_ms} ms"
    );

    // 单音扫描的合成幅度是 |1 + e^{j2πfΔ}|，以 1/f 为周期摆动；如果步进恰好是音调周期的
    // 整数倍（例如 1 kHz 配 2 ms 步进 = 整整 2 个周期），每个采样点落在同一相位上，
    // 扫出来是一条平线——那不是「没有对齐点」，而是扫描参数自带了混叠。
    let cycles_per_step = tone_hz * step_ms / 1000.0;
    let nearest = cycles_per_step.round();
    if (cycles_per_step - nearest).abs() < 0.02 && nearest >= 1.0 {
        println!(
            "  ⚠ 扫描参数自带混叠：{tone_hz} Hz 的周期是 {:.3} ms，而步进 {step_ms} ms 正好是它的 {nearest:.0} 倍，\n\
             \x20    每个采样点都落在同一相位上 → 扫出来必然是平线，看不到对齐点。\n\
             \x20    请把 --step 改成 1（或任何不是 {:.3} ms 整数倍的步进），或把 --tone-hz 改成与步进不成整数倍关系的频率（例如 997）。",
            1000.0 / tone_hz,
            1000.0 / tone_hz
        );
    }
    println!(
        "  提示：单音只能验证「候选延迟附近有没有峰」，找不到绝对零点；\n\
         \x20    要量两箱真实到达差请用 --align-check（同一次录音里的相关峰间隔）。"
    );

    println!("  先给麦克风做体检…");
    let h = mic_health(mic, 1_000)?;
    println!("  {}", h.describe());
    if !h.verdict.is_usable() {
        println!("  麦克风不可用（{}），无法扫描。", h.verdict.as_str());
        return Ok(());
    }

    println!("  开始扫描（每个点放一段测试音）…");
    let t0 = Instant::now();
    let scan = scan_alignment(mic, left, right, cfg, tone_hz, from_ms, to_ms, step_ms)?;
    println!("  耗时 {:.1}s", t0.elapsed().as_secs_f64());
    println!();
    println!("     延迟ms      tone幅度          RMS       峰值    窗起点ms");
    for p in &scan.points {
        println!(
            "  {:>9.2}  {:>12.6}  {:>11.6}  {:>9.5}  {:>10.1}",
            p.delay_ms, p.tone, p.rms, p.peak_abs, p.tone_at_ms
        );
    }
    println!();
    println!("  {}", scan.describe());

    if let Some(cur) = current_ms {
        let near = scan
            .points
            .iter()
            .min_by(|a, b| {
                (a.delay_ms - cur)
                    .abs()
                    .partial_cmp(&(b.delay_ms - cur).abs())
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .copied()
            .unwrap_or_default();
        let loss_db = if near.tone > 0.0 && scan.best.tone > 0.0 {
            20.0 * (scan.best.tone / near.tone).log10()
        } else {
            0.0
        };
        println!(
            "  当前配置的 {cur:.1} ms（表里最近点 {:.1} ms，幅度 {:.6}）比最佳点低 **{loss_db:.1} dB**",
            near.delay_ms, near.tone
        );
    }

    println!(
        "  注意：单频正弦的合成幅度以 1/f 为周期摆动（1 kHz 即 1 ms），所以只有**扫描范围内**\n\
         \x20 的峰才是可信的对齐点；若整条曲线是平的，说明两路在时间上根本不重叠——\n\
         \x20 先用 --polarity --right 看「两路同时播」的相关峰相距多远。"
    );
    Ok(())
}

/// `--align-check`：两路同时播同一段探测信号（各自可预垫延迟），量相关峰间隔。
///
/// 先用 `--apply-left 0` 量出真实到达差，再把这个差填回去复测：间隔应塌到 0、RMS 抬高。
fn align_check_mode(
    mic: &str,
    left: &str,
    right: &str,
    cfg: &SyncConfig,
    apply_left_ms: i32,
    apply_right_ms: i32,
) -> Result<(), AnyErr> {
    println!("== 两路对齐检查（同一段录音里两个相关峰的间隔）==");
    println!("  麦克风：{mic}");
    println!("  左路：{left}  预垫 {apply_left_ms} ms");
    println!("  右路：{right}  预垫 {apply_right_ms} ms");
    println!("  {}", cfg.describe());
    let h = mic_health(mic, 1_000)?;
    println!("  {}", h.describe());
    if !h.verdict.is_usable() {
        println!("  麦克风不可用（{}），无法测量。", h.verdict.as_str());
        return Ok(());
    }
    let t0 = Instant::now();
    let c = check_alignment(mic, left, right, cfg, apply_left_ms, apply_right_ms)?;
    println!("  耗时 {:.1}s", t0.elapsed().as_secs_f64());
    println!("  左路播放：{}", c.left_playback);
    println!("  右路播放：{}", c.right_playback);
    println!(
        "  起播时刻（相对 Barrier 放行）：左 {} ms / 右 {} ms → 相差 {} ms",
        c.left_start_ms,
        c.right_start_ms,
        (c.right_start_ms - c.left_start_ms).abs()
    );
    println!(
        "  播放占用（建流+播放+排空）：左 {} ms / 右 {} ms",
        c.left_end_ms - c.left_start_ms,
        c.right_end_ms - c.right_start_ms
    );
    if (c.right_start_ms - c.left_start_ms).abs() > 50
        || (c.left_end_ms - c.left_start_ms) > 2 * (c.right_end_ms - c.right_start_ms).max(1)
    {
        println!("  ⚠ 两路没有真正并发起播，本次测量不可信（请重跑）");
    }
    println!("  {}", c.describe());
    if !c.merged {
        println!(
            "  → 两路在麦克风处相隔 {:.1} ms（≈ {:.2} 米声程），**没对齐**：\n\
             \x20    这么大的偏移不是梳状滤波，而是**可听见的回声/双声**（人声会发糊、定位散）。\n\
             \x20    把「左垫」改成 {:.0} ms（或右垫对应值）再测一次，间隔应塌到 0。",
            c.gap_ms,
            c.gap_ms / 1000.0 * 343.0,
            apply_left_ms as f64 + c.gap_ms
        );
    } else {
        println!(
            "  → 两路已重合（间隔 {:.3} ms ≤ {:.1} ms）：这就是当前设置下两路能拿到的最大相干叠加。",
            c.gap_ms,
            stereo_audio::sync::ALIGN_MERGE_MS
        );
    }
    Ok(())
}

/// 打印互相关曲线上的前几个峰——用来判断「峰是不是唯一的」。
///
/// 如果出现**符号相反、幅度相当**的第二个峰，说明录音里有两份反相的探测信号
/// （两只箱其实都在出声、或存在反相强反射），此时最大 `|峰|` 的符号会随机翻转，
/// 极性结论不成立。
fn print_peaks(p: &PolarityResult) {
    if p.peaks.is_empty() {
        return;
    }
    let mut line = String::new();
    for (ms, v) in &p.peaks {
        line.push_str(&format!(" {ms:.3}ms/{v:+.3}"));
    }
    println!("    相关峰（滞后/带符号值）：{line}");
    if p.direct * p.correlation < 0.0 {
        println!(
            "    ⚠ 最大|峰|（{:+.3}）与直达声（{:+.3}）**符号相反** → 最大|峰|量到的是反相强反射；\
             判极性已自动改用直达声",
            p.correlation, p.direct
        );
    }
    if p.peaks.len() >= 2 {
        let (a, b) = (p.peaks[0].1, p.peaks[1].1);
        if a * b < 0.0 && b.abs() >= 0.8 * a.abs() {
            println!(
                "    ⚠ 存在符号相反、幅度相当的第二个峰（{a:+.3} vs {b:+.3}，相隔 {:.3} ms）\
                 → 本次极性结论**不可信**",
                p.peaks[1].0 - p.peaks[0].0
            );
        }
    }
}

/// 由两次单箱测量给出「是否有一只箱反相」的一句话。
///
/// 用 `direct`（直达声）而不是 `correlation`（最大 |峰|）：本机实测两只箱的互相关曲线都是
/// 「直达声 + 其后 ~7 ms 一个更强的反相反射」，取最大 |峰| 会量到反射。
fn polarity_note(l: &PolarityResult, r: &PolarityResult) -> String {
    let (a, b) = (l.direct, r.direct);
    if a.abs() < 0.15 || b.abs() < 0.15 {
        return format!(
            "直达声相关峰太弱（左 {a:+.3} / 右 {b:+.3}），极性不可判——先确认麦克风听得见。"
        );
    }
    if a * b < 0.0 {
        format!("两路**直达声**符号相反（左 {a:+.3} / 右 {b:+.3}）→ **有一只箱极性相反**。")
    } else {
        format!("两路**直达声**同号（左 {a:+.3} / 右 {b:+.3}）→ 两路相对极性一致，没有反相。")
    }
}

fn main() -> Result<(), AnyErr> {
    let args: Vec<String> = std::env::args().collect();

    if has_flag(&args, "--list") || args.len() == 1 {
        for ep in list_endpoints()? {
            println!("{}", ep.describe());
        }
        return Ok(());
    }

    let rate: u32 = arg_value(&args, "--rate")
        .and_then(|v| v.parse().ok())
        .unwrap_or(RATE);
    let seconds: f64 = arg_value(&args, "--seconds")
        .and_then(|v| v.parse().ok())
        .unwrap_or(15.0);

    if has_flag(&args, "--loopback") {
        // 对照实验：回环采 render 端点，证明「数字流确实送到了设备」。
        let speaker = arg_value(&args, "--speaker")
            .or_else(|| arg_value(&args, "--mic"))
            .ok_or("缺少 --speaker <名字子串>")?;
        return loopback_mode(&speaker, rate);
    }

    if has_flag(&args, "--hwmeter") {
        // 直接轮询端点自带的峰值表，不开任何流；用于判断「系统现在到底在不在出声」。
        if let Some(sp) = arg_value(&args, "--speaker") {
            println!("== 端点硬件峰值表（render）：{sp} ==");
            return hwmeter_mode(&sp, DataFlow::Render, seconds);
        }
        let mic = arg_value(&args, "--mic").ok_or("缺少 --mic 或 --speaker <名字子串>")?;
        println!("== 端点硬件峰值表（capture）：{mic} ==");
        return hwmeter_mode(&mic, DataFlow::Capture, seconds);
    }

    if has_flag(&args, "--health") {
        let ms: u32 = arg_value(&args, "--ms")
            .and_then(|v| v.parse().ok())
            .unwrap_or(1_000);
        return health_mode(arg_value(&args, "--mic").as_deref(), ms);
    }

    if has_flag(&args, "--clicks") {
        let left = arg_value(&args, "--left").ok_or("缺少 --left <子串>")?;
        let right = arg_value(&args, "--right").ok_or("缺少 --right <子串>")?;
        let ld: u32 = arg_value(&args, "--left-delay")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let rd: u32 = arg_value(&args, "--right-delay")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        return clicks_mode(&left, &right, ld, rd, seconds.min(30.0));
    }

    if has_flag(&args, "--volume") {
        let set = arg_value(&args, "--set").and_then(|v| v.parse::<f32>().ok());
        let unmute = has_flag(&args, "--unmute");
        if let Some(sp) = arg_value(&args, "--speaker") {
            println!("== 端点音量 / 静音（render）：{sp} ==");
            return volume_mode(&sp, DataFlow::Render, set, unmute);
        }
        let mic = arg_value(&args, "--mic").ok_or("缺少 --mic 或 --speaker <子串>")?;
        println!("== 端点音量 / 静音（capture）：{mic} ==");
        return volume_mode(&mic, DataFlow::Capture, set, unmute);
    }

    if has_flag(&args, "--polarity") {
        let mic = arg_value(&args, "--mic").ok_or("缺少 --mic <子串>")?;
        let left = arg_value(&args, "--left").ok_or("缺少 --left <子串>")?;
        let right = arg_value(&args, "--right");
        let mut cfg = SyncConfig::new(mic.clone(), left.clone());
        if let Some(v) = arg_value(&args, "--probe-ms").and_then(|v| v.parse::<u32>().ok()) {
            cfg = cfg.with_probe_ms(v);
        }
        if let Some(v) = arg_value(&args, "--amplitude").and_then(|v| v.parse::<f32>().ok()) {
            cfg = cfg.with_amplitude(v);
        }
        return polarity_mode(
            &mic,
            &left,
            right.as_deref(),
            &cfg,
            has_flag(&args, "--pair-only"),
        );
    }

    if has_flag(&args, "--align-check") {
        let mic = arg_value(&args, "--mic").ok_or("缺少 --mic <子串>")?;
        let left = arg_value(&args, "--left").ok_or("缺少 --left <子串>")?;
        let right = arg_value(&args, "--right").ok_or("缺少 --right <子串>")?;
        let mut cfg = SyncConfig::new(mic.clone(), left.clone());
        if let Some(v) = arg_value(&args, "--probe-ms").and_then(|v| v.parse::<u32>().ok()) {
            cfg = cfg.with_probe_ms(v);
        }
        if let Some(v) = arg_value(&args, "--amplitude").and_then(|v| v.parse::<f32>().ok()) {
            cfg = cfg.with_amplitude(v);
        }
        let ld = arg_value(&args, "--apply-left")
            .and_then(|v| v.parse::<i32>().ok())
            .unwrap_or(0);
        let rd = arg_value(&args, "--apply-right")
            .and_then(|v| v.parse::<i32>().ok())
            .unwrap_or(0);
        return align_check_mode(&mic, &left, &right, &cfg, ld, rd);
    }

    if has_flag(&args, "--align-scan") {
        let mic = arg_value(&args, "--mic").ok_or("缺少 --mic <子串>")?;
        let left = arg_value(&args, "--left").ok_or("缺少 --left <子串>")?;
        let right = arg_value(&args, "--right").ok_or("缺少 --right <子串>")?;
        let mut cfg = SyncConfig::new(mic.clone(), left.clone());
        if let Some(v) = arg_value(&args, "--probe-ms").and_then(|v| v.parse::<u32>().ok()) {
            cfg = cfg.with_probe_ms(v);
        }
        if let Some(v) = arg_value(&args, "--amplitude").and_then(|v| v.parse::<f32>().ok()) {
            cfg = cfg.with_amplitude(v);
        }
        let tone_hz = arg_value(&args, "--tone-hz")
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(1_000.0);
        let from_ms = arg_value(&args, "--from")
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(-40.0);
        let to_ms = arg_value(&args, "--to")
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(40.0);
        let step_ms = arg_value(&args, "--step")
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(2.0);
        let current_ms = arg_value(&args, "--current").and_then(|v| v.parse::<f64>().ok());
        return align_scan_mode(
            &mic, &left, &right, &cfg, tone_hz, from_ms, to_ms, step_ms, current_ms,
        );
    }

    if has_flag(&args, "--calibrate") {
        let mic = arg_value(&args, "--mic").ok_or("缺少 --mic <子串>")?;
        let left = arg_value(&args, "--left").ok_or("缺少 --left <子串>")?;
        let right = arg_value(&args, "--right").ok_or("缺少 --right <子串>")?;
        let mut cfg = SyncConfig::new(mic.clone(), left.clone());
        if let Some(p) = arg_value(&args, "--probe") {
            match SyncProbe::from_str_opt(&p) {
                Some(k) => cfg = cfg.with_probe(k),
                None => return Err(format!("未知探测信号：{p}（可选 sweep / click / mls）").into()),
            }
        }
        if let Some(v) = arg_value(&args, "--probe-ms").and_then(|v| v.parse::<u32>().ok()) {
            cfg = cfg.with_probe_ms(v);
        }
        if let Some(v) = arg_value(&args, "--amplitude").and_then(|v| v.parse::<f32>().ok()) {
            cfg = cfg.with_amplitude(v);
        }
        return calibrate_mode(&mic, &left, &right, &cfg, has_flag(&args, "--sequential"));
    }

    let mic = arg_value(&args, "--mic").ok_or("缺少 --mic <名字子串>")?;

    if has_flag(&args, "--exclusive") {
        let live = has_flag(&args, "--meter");
        println!(
            "== 独占模式裸采集：{mic}{} ==",
            if live { "（实时电平表）" } else { "" }
        );
        let channels: u16 = arg_value(&args, "--channels")
            .and_then(|v| v.parse().ok())
            .unwrap_or(1);
        let bits: u16 = arg_value(&args, "--bits")
            .and_then(|v| v.parse().ok())
            .unwrap_or(16);
        let ex_rate: u32 = arg_value(&args, "--rate")
            .and_then(|v| v.parse().ok())
            .unwrap_or(48_000);
        let wav = arg_value(&args, "--wav");
        let play_sub = arg_value(&args, "--play");
        let play_freq: f64 = arg_value(&args, "--play-freq")
            .and_then(|v| v.parse().ok())
            .unwrap_or(1000.0);
        let play_amp: f32 = arg_value(&args, "--play-amp")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0.9);
        let play = play_sub.map(|s| (s, play_freq, play_amp));
        return exclusive_mode(
            &mic,
            ex_rate,
            channels,
            bits,
            seconds,
            live,
            wav.as_deref(),
            play.as_ref().map(|(s, f, a)| (s.as_str(), *f, *a)),
        );
    }

    if has_flag(&args, "--meter") {
        println!("== 实时电平表（共享模式）：{mic} @ {rate} Hz ==");
        return meter_mode(&mic, seconds, rate);
    }

    if has_flag(&args, "--check") {
        let speaker = arg_value(&args, "--speaker");
        let wav = arg_value(&args, "--wav");
        return check_mode(&mic, speaker.as_deref(), rate, wav.as_deref());
    }

    println!(
        "用法：\n\
         \x20 --list\n\
         \x20 --health [--mic <子串>] [--ms <n>]            麦克风体检（独占优先，含判定与建议）\n\
         \x20 --calibrate --mic <子串> --left <子串> --right <子串> [--probe sweep|click|mls] [--probe-ms <n>] [--amplitude <0..1>] [--sequential]\n\
         \x20 --polarity --mic <子串> --left <子串> --right <子串> [--probe-ms <n>] [--amplitude <0..1>] [--pair-only]\n\
         \x20 --align-scan --mic <子串> --left <子串> --right <子串> [--tone-hz <n>] [--from <ms>] [--to <ms>] [--step <ms>] [--current <ms>]\n\
         \x20 --align-check --mic <子串> --left <子串> --right <子串> [--apply-left <ms>] [--apply-right <ms>] [--probe-ms <n>]\n\
         \x20 --clicks --left <子串> --right <子串> [--left-delay <ms>] [--right-delay <ms>] [--seconds <n>]\n\
         \x20 --mic <子串> --check [--speaker <子串>] [--rate <hz>] [--wav <路径>]\n\
         \x20 --mic <子串> --volume [--set <0..100>] [--unmute]\n\
         \x20 --mic <子串> --meter [--seconds <n>] [--rate <hz>]\n\
         \x20 --mic <子串> --exclusive [--rate <hz>] [--channels <n>] [--bits <16|24|32>] [--seconds <n>] [--meter] [--wav <路径>] [--play <扬声器子串>] [--play-freq <hz>] [--play-amp <0..1>]\n\
         \x20 --mic <子串> --hwmeter [--seconds <n>]\n\
         \x20 --speaker <子串> --hwmeter [--seconds <n>]\n\
         \x20 --speaker <子串> --loopback [--rate <hz>]"
    );
    Ok(())
}
