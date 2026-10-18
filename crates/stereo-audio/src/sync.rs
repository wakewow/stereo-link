//! 声学自校验（sync）：测出两路音箱的实际到达时间差，并给出补偿量。
//!
//! # 它解决什么问题
//!
//! 两只同款蓝牙音箱左右分开摆时，各自的 A2DP 链路延迟不同，同一声源从两边到达
//! 耳朵的时间就有差（几毫秒到几十毫秒），听感上声像会偏、低频会「糊」。本模块负责：
//!
//! 1. **自动标定**（需要一只可用的麦克风）：往左/右端点各播一次探测信号，同时用
//!    麦克风录，用互相关求出两次的到达时间，差就是 `delta_ms`。
//! 2. **手动对齐**（不需要麦克风）：按给定的延迟值给两路 PCM 预垫静音后同时播放，
//!    用户用耳朵听是否重合成一声，反复调 → 也能标定。
//! 3. **麦克风体检**：告诉 GUI「选中的麦克风有没有信号」，避免用户在数字静默的
//!    端点上白等。
//!
//! # 关键约定
//!
//! * **绝对延迟无意义，两路之差才有意义**。[`SyncResult::left_ms`] /
//!   [`SyncResult::right_ms`] 已经归一化：两者中较小者为 0，另一个是相对更早那
//!   一路的额外延迟。`delta_ms = left_ms - right_ms`。
//! * 采样率固定 [`SYNC_RATE`]（44100 Hz / 2 声道 / s16le 交错），与 crate 对外契约
//!   一致。
//! * 不引入任何新依赖：互相关、抛物线插值、伪随机噪声全部自己实现。
//! * [`play_pcm_s16`] 会先往 WASAPI 缓冲里预填一整块
//!   静音再 `Start()`，所以真实出声时刻比调用时刻晚约 1 秒；录制窗口必须把这段
//!   「前置静音」算进去（见 [`SyncConfig::pre_roll_ms`]）。这段延迟对左右两路是**相同
//!   的固定值**，因此在 `delta_ms` 里自动抵消。
//!
//! # 本机现状（实测，2026-10-18）
//!
//! 本机的 **Chu2 DSP**（USB 复合设备 `VID_31B2&PID_0113` 的 MI_00，走微软通用
//! `USBAudio.sys`；采集端点 `{11111111-2222-3333-4444-555555555555}`）**麦克风是好的**：
//!
//! * **独占模式**（48000 Hz / 1 ch / 16-bit，设备原生格式）：往房间里的蓝牙音箱播
//!   1 kHz 单音，录到的频谱在 **1000.0 Hz** 处是一根尖峰（峰/中位 ≈ **7500**），
//!   稳定段 RMS 从 18 LSB 抬到 67 LSB；
//! * **共享模式**（Windows 音频引擎）：同一条音在录音里**完全不存在**，整段是
//!   0.48 LSB 的平底（-96.7 dBFS），连独占模式那个开流尖峰都没有。
//!
//! 也就是说：**共享模式（音频引擎 / APO / AUTOCONVERTPCM 那条路）在这台机器上采不到
//! 这只麦的真实数据，独占模式能。** 所以本模块的测量路径**默认走独占模式**
//! （见 [`CapturePath`]），共享模式只作兜底。别用 [`mic_health`] 的结果去否定硬件——
//! 它已经会自己试独占模式。
//!
//! 其余采集端点：Realtek 只有 20~22 LSB 开路噪声、Steam/UU 是虚拟设备；所有真麦端点
//! `DeviceState` 都是 4/8（未插入/未启用），**根本不会出现在 [`list_microphones`] 里**。

use crate::convert::s16_to_f32;
use crate::endpoint::{DataFlow, DeviceSelector, EndpointInfo, PcmFormat};
use crate::error::{AudioError, Result};

#[cfg(windows)]
use std::time::{Duration, Instant};

#[cfg(windows)]
use crate::capture::{list_endpoints, AudioCapture, CaptureConfig, CaptureMode};
#[cfg(windows)]
use crate::com::{self, ComGuard, ResolvedDevice};
#[cfg(windows)]
use crate::convert::resample_f32;
#[cfg(windows)]
use crate::playback::play_pcm_s16;
#[cfg(windows)]
use windows::Win32::Media::Audio::{
    IAudioCaptureClient, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_EXCLUSIVE,
    AUDCLNT_S_BUFFER_EMPTY,
};

/// 自校验链路使用的采样率（Hz），与 crate 对外契约一致。
pub const SYNC_RATE: u32 = 44_100;

/// 自校验链路使用的声道数（交错 s16le）。
pub const SYNC_CHANNELS: u16 = 2;

/// 麦克风「有信号」的峰值门限（dBFS）：高于它判定为 [`MicVerdict::Signal`]。
pub const MIC_SIGNAL_DBFS: f64 = -45.0;

/// 麦克风「只有本底噪声」的峰值门限（dBFS）：介于它与 [`MIC_SIGNAL_DBFS`] 之间判定为
/// [`MicVerdict::NoiseFloor`]，低于它判定为 [`MicVerdict::Silent`]。
pub const MIC_NOISE_DBFS: f64 = -80.0;

/// 手动对齐点击串的间隔（ms）。300 ms ≈ 每秒 3.3 下，慢到能听出「双击/镶边」。
pub const ALIGN_CLICK_PERIOD_MS: u32 = 300;

/// 探测信号种类。
///
/// 三种都能用，默认 [`SyncProbe::Sweep`]：扫频在时间轴上没有周期性，互相关的峰唯一，
/// 不会像点击串那样在周期倍数处出现假峰。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SyncProbe {
    /// 短脉冲串：每 25 ms 一个 2 ms 的 Hann 窗 2 kHz 短音，中间是静默。
    /// 瞬态尖锐、听感明显，适合手动试听；但互相关在 25 ms 倍数处有假峰。
    ClickTrain,
    /// 线性扫频 500 → 4000 Hz，整段加 Hann 包络。**自动标定的默认选择**。
    #[default]
    Sweep,
    /// 最大长度序列（10 阶 LFSR，±满幅伪随机方波），自相关接近 δ 函数。
    Mls,
}

impl SyncProbe {
    /// 全部种类，便于 GUI 做下拉列表与穷举测试。
    pub const ALL: [SyncProbe; 3] = [SyncProbe::ClickTrain, SyncProbe::Sweep, SyncProbe::Mls];

    /// 短名（用于命令行参数与日志）。
    pub fn as_str(self) -> &'static str {
        match self {
            SyncProbe::ClickTrain => "clicks",
            SyncProbe::Sweep => "sweep",
            SyncProbe::Mls => "mls",
        }
    }

    /// 从 [`SyncProbe::as_str`] 的名字解析回来。
    pub fn from_str_opt(s: &str) -> Option<Self> {
        let s = s.trim().to_ascii_lowercase();
        SyncProbe::ALL
            .into_iter()
            .find(|k| k.as_str() == s || (s.starts_with("click") && *k == SyncProbe::ClickTrain))
    }
}

impl std::fmt::Display for SyncProbe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 自动标定的配置。
///
/// 用 [`SyncConfig::default`] 或 [`SyncConfig::new`] 起步，再用 `with_*` 逐项覆盖——
/// 这样将来加字段也不会打断调用方。
#[derive(Debug, Clone, PartialEq)]
pub struct SyncConfig {
    /// 麦克风（capture 端点）的**名字子串**或 `{GUID}`。
    pub mic_endpoint: String,
    /// 被测量（render 端点）的名字子串或 `{GUID}`。
    pub probe_endpoint: String,
    /// 探测信号时长（ms）。
    pub probe_ms: u32,
    /// 探测信号幅度（0.0 ~ 1.0，线性）。
    pub amplitude: f32,
    /// 探测信号种类。
    pub probe: SyncProbe,
    /// 录制窗口在探测信号之前的余量（ms），用来覆盖播放器的前置静音与设备启动延迟。
    pub pre_roll_ms: u32,
    /// 录制窗口在探测信号之后的余量（ms），用来覆盖房间余响与蓝牙抖动。
    pub tail_ms: u32,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            mic_endpoint: String::new(),
            probe_endpoint: String::new(),
            probe_ms: 100,
            amplitude: 0.8,
            probe: SyncProbe::Sweep,
            // play_pcm_s16 会先填一整块（约 1 s）静音再 Start，加上设备启动与蓝牙
            // 建链，2 s 余量足够稳。
            pre_roll_ms: 2_000,
            tail_ms: 300,
        }
    }
}

impl SyncConfig {
    /// 指定麦克风与被测端点。
    pub fn new(mic_endpoint: impl Into<String>, probe_endpoint: impl Into<String>) -> Self {
        Self {
            mic_endpoint: mic_endpoint.into(),
            probe_endpoint: probe_endpoint.into(),
            ..Self::default()
        }
    }

    /// 覆盖探测信号时长（ms）。
    pub fn with_probe_ms(mut self, ms: u32) -> Self {
        self.probe_ms = ms;
        self
    }

    /// 覆盖探测信号幅度。
    pub fn with_amplitude(mut self, amplitude: f32) -> Self {
        self.amplitude = amplitude;
        self
    }

    /// 覆盖探测信号种类。
    pub fn with_probe(mut self, probe: SyncProbe) -> Self {
        self.probe = probe;
        self
    }

    /// 覆盖前置余量（ms）。
    pub fn with_pre_roll_ms(mut self, ms: u32) -> Self {
        self.pre_roll_ms = ms;
        self
    }

    /// 覆盖尾部余量（ms）。
    pub fn with_tail_ms(mut self, ms: u32) -> Self {
        self.tail_ms = ms;
        self
    }

    /// 一次测量需要的录制窗口总长（ms）= 前置余量 + 探测时长 + 尾部余量。
    pub fn window_ms(&self) -> u32 {
        self.pre_roll_ms
            .saturating_add(self.probe_ms)
            .saturating_add(self.tail_ms)
    }

    /// 人读一行。
    pub fn describe(&self) -> String {
        format!(
            "mic={} 端点={} 信号={} {}ms amp={:.2} 窗口={}ms（前置{} + 探测{} + 尾部{}）",
            if self.mic_endpoint.is_empty() {
                "<未指定>"
            } else {
                &self.mic_endpoint
            },
            self.probe_endpoint,
            self.probe.as_str(),
            self.probe_ms,
            self.amplitude,
            self.window_ms(),
            self.pre_roll_ms,
            self.probe_ms,
            self.tail_ms,
        )
    }
}

/// 一次互相关的结果。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Correlation {
    /// 使 `observed[lag .. lag+reference.len()]` 最像 `reference` 的整数滞后（采样点）。
    pub lag: usize,
    /// 抛物线插值后的亚采样点滞后。
    pub lag_sub: f64,
    /// 归一化互相关峰值（0.0 ~ 1.0）。
    pub peak: f32,
    /// 峰值 / 峰外均方根（线性比值，越大越干净）。
    pub peak_snr: f32,
    /// 0.0 ~ 1.0 的置信度（峰值与信噪比的组合）。
    pub confidence: f32,
}

impl Correlation {
    /// 亚采样滞后换算成毫秒。
    pub fn lag_ms(&self, rate: u32) -> f64 {
        lag_ms(self.lag_sub, rate)
    }

    /// 人读一行。
    pub fn describe(&self) -> String {
        format!(
            "lag={} ({:.3} 采样) peak={:.3} snr={:.1} 置信度={:.2}",
            self.lag, self.lag_sub, self.peak, self.peak_snr, self.confidence
        )
    }
}

/// 一次双路标定的结果。
///
/// `left_ms` / `right_ms` 是**归一化后的相对延迟**：两者中较小者为 0，另一个是相对
/// 更早那一路的额外延迟；`delta_ms = left_ms - right_ms`。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SyncResult {
    /// 左路相对更早那一路的额外延迟（ms），必有一个为 0。
    pub left_ms: f64,
    /// 右路相对更早那一路的额外延迟（ms），必有一个为 0。
    pub right_ms: f64,
    /// `left_ms - right_ms`：>0 表示左路更晚（该给右路垫延迟）。
    pub delta_ms: f64,
    /// 两次测量中较差的置信度（0.0 ~ 1.0）。
    pub confidence: f32,
    /// 两次测量中较差的峰值信噪比。
    pub peak_snr: f32,
    /// 这次结果是怎么测出来的。
    pub method: SyncMethod,
}

impl SyncResult {
    /// 该给两条路由各设多少毫秒的补偿：`(left_delay_ms, right_delay_ms)`。
    ///
    /// 与自由函数 [`delay_ms_for`] 等价。
    pub fn delays(&self) -> (u32, u32) {
        delay_ms_for(self)
    }

    /// 人读一行。
    pub fn describe(&self) -> String {
        let (l, r) = self.delays();
        format!(
            "左路额外 {:.3} ms / 右路额外 {:.3} ms | delta={:+.3} ms → 建议左垫 {} ms、右垫 {} ms | 置信度={:.2} snr={:.1} | {}",
            self.left_ms, self.right_ms, self.delta_ms, l, r, self.confidence, self.peak_snr,
            self.method.describe()
        )
    }
}

/// 标定用哪种测量方法。
///
/// 精度差别很大，所以必须让调用方知道：同时法把两路放在**同一段录音**里，采集流的起始
/// 相位抖动是共模的、在 delta 里抵消；依次法两次独立采集，抖动直接进 delta（本机实测
/// 同一个有线端点自比，依次法给出 -4.0 ~ +3.0 ms 的假差值）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncMethod {
    /// 同时法：左右两路同时播各自频带的探测信号，从同一段录音里分别相关。
    Simultaneous,
    /// 依次法：先左后右，两次独立采集。**精度只有 ±4 ms 量级**，仅在同时法失败时兜底。
    Sequential,
}

impl SyncMethod {
    /// 稳定的英文标识。
    pub fn as_str(self) -> &'static str {
        match self {
            SyncMethod::Simultaneous => "simultaneous",
            SyncMethod::Sequential => "sequential",
        }
    }

    /// 中文说明。
    pub fn describe(self) -> &'static str {
        match self {
            SyncMethod::Simultaneous => "同时法（同一段录音，精度 ≤1 ms）",
            SyncMethod::Sequential => "依次法（两次独立录音，精度 ±4 ms）",
        }
    }
}

impl std::fmt::Display for SyncMethod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 采样点滞后换算成毫秒。
pub fn lag_ms(lag_samples: f64, rate: u32) -> f64 {
    if rate == 0 {
        0.0
    } else {
        lag_samples * 1000.0 / rate as f64
    }
}

/// 由测量结果算出「两条路由各自该垫多少 ms」。
///
/// 慢（到达更晚）的那路不需要额外延迟，快的那路补上差值。
/// 返回 `(left_delay_ms, right_delay_ms)`，其中必有一个为 0。
pub fn delay_ms_for(result: &SyncResult) -> (u32, u32) {
    let d = result.delta_ms;
    if !d.is_finite() {
        return (0, 0);
    }
    if d >= 0.0 {
        // 左路更晚 → 给右路垫延迟
        (0, d.round().clamp(0.0, u32::MAX as f64) as u32)
    } else {
        ((-d).round().clamp(0.0, u32::MAX as f64) as u32, 0)
    }
}

/// 单路扫频探测信号用的频带（Hz）。
pub const SWEEP_BAND: (f64, f64) = (500.0, 4_000.0);

/// 同时法标定用的**左路**频带（Hz）。
pub const PAIR_BAND_LEFT: (f64, f64) = (600.0, 2_400.0);

/// 同时法标定用的**右路**频带（Hz）。
pub const PAIR_BAND_RIGHT: (f64, f64) = (2_600.0, 4_400.0);

/// 生成一段**指定频带**的升频线性扫频（Hann 包络）。
///
/// `f0` / `f1` 会被夹到 20 Hz ~ 0.45×采样率；`f0 > f1` 时自动交换（所以也能生成降频扫频）。
/// 同时法标定用它给左右两路各生成一段**频带互不重叠**的扫频——频带分开是必须的，两段
/// 信号在时域上近似正交，才能从同一段录音里分别相关出各自的到达时间。
pub fn sweep_signal_banded(ms: u32, amplitude: f32, rate: u32, f0: f64, f1: f64) -> Vec<i16> {
    let rate = rate.max(8_000);
    let ms = ms.clamp(1, 10_000);
    let n = (rate as u64 * ms as u64 / 1000).max(1) as usize;
    let amp = (amplitude.clamp(0.0, 1.0) as f64) * i16::MAX as f64;
    let nyq = rate as f64 * 0.45;
    let (mut lo, mut hi) = (
        if f0.is_finite() {
            f0.clamp(20.0, nyq)
        } else {
            20.0
        },
        if f1.is_finite() {
            f1.clamp(20.0, nyq)
        } else {
            nyq
        },
    );
    if lo > hi {
        std::mem::swap(&mut lo, &mut hi);
    }
    let total = n as f64 / rate as f64;
    let mut out = vec![0i16; n];
    for (i, slot) in out.iter_mut().enumerate() {
        let t = i as f64 / rate as f64;
        // 线性调频的瞬时频率 f(t)=f0+(f1-f0)*t/T → 相位是它的积分
        let phase = std::f64::consts::TAU * (lo * t + (hi - lo) * t * t / (2.0 * total.max(1e-9)));
        let env = 0.5 - 0.5 * (std::f64::consts::TAU * t / total.max(1e-9)).cos();
        *slot = (phase.sin() * env * amp) as i16;
    }
    out
}

/// 同时法标定用的左右探测对：两段频带互不重叠的升频扫频（[`PAIR_BAND_LEFT`] / [`PAIR_BAND_RIGHT`]）。
pub fn probe_pair(probe_ms: u32, amplitude: f32, rate: u32) -> (Vec<i16>, Vec<i16>) {
    (
        sweep_signal_banded(
            probe_ms,
            amplitude,
            rate,
            PAIR_BAND_LEFT.0,
            PAIR_BAND_LEFT.1,
        ),
        sweep_signal_banded(
            probe_ms,
            amplitude,
            rate,
            PAIR_BAND_RIGHT.0,
            PAIR_BAND_RIGHT.1,
        ),
    )
}

/// 生成探测信号（[`SYNC_RATE`] 采样率）。
///
/// * `ms` 会被夹到 1 ~ 10000；
/// * `amplitude` 会被夹到 0.0 ~ 1.0，再映射到 i16 满量程。
pub fn probe_signal(kind: SyncProbe, ms: u32, amplitude: f32) -> Vec<i16> {
    probe_signal_at(kind, ms, amplitude, SYNC_RATE)
}

/// 生成探测信号（指定采样率）。
///
/// 与 [`probe_signal`] 相同，只是采样率可指定——单测里用它来脱离 44100 做数学验证。
pub fn probe_signal_at(kind: SyncProbe, ms: u32, amplitude: f32, rate: u32) -> Vec<i16> {
    let rate = rate.max(8_000);
    let ms = ms.clamp(1, 10_000);
    let n = (rate as u64 * ms as u64 / 1000).max(1) as usize;
    let amp = (amplitude.clamp(0.0, 1.0) as f64) * i16::MAX as f64;
    let mut out = vec![0i16; n];

    match kind {
        SyncProbe::ClickTrain => {
            let period = (rate as usize / 40).max(8); // 25 ms
            let click = (rate as usize / 500).max(4); // 2 ms
            let mut start = 0usize;
            while start + click <= n {
                for i in 0..click {
                    let t = i as f64 / click as f64;
                    let env = 0.5 - 0.5 * (std::f64::consts::TAU * t).cos(); // Hann
                    let s = (std::f64::consts::TAU * 2000.0 * i as f64 / rate as f64).sin();
                    out[start + i] = (s * env * amp) as i16;
                }
                start += period;
            }
        }
        SyncProbe::Sweep => {
            out = sweep_signal_banded(ms, amplitude, rate, SWEEP_BAND.0, SWEEP_BAND.1);
        }
        SyncProbe::Mls => {
            // 10 阶最大长度序列：本原多项式 x^10 + x^7 + 1 → 反馈 = bit9 ^ bit6
            const ORDER: usize = 10;
            const LEN: usize = (1 << ORDER) - 1;
            let chip = (n / LEN).max(1);
            let mut reg: u32 = 0x2A5; // 任意非零种子
            let mut idx = 0usize;
            'fill: for _ in 0..LEN {
                let bit = ((reg >> 9) ^ (reg >> 6)) & 1;
                reg = ((reg << 1) | bit) & ((1 << ORDER) - 1);
                let v = if reg & 1 == 1 { amp } else { -amp };
                for _ in 0..chip {
                    if idx >= n {
                        break 'fill;
                    }
                    out[idx] = v as i16;
                    idx += 1;
                }
            }
            // 探测时长比一个完整序列还长时，把序列首尾相接填满
            let filled = idx;
            if filled > 0 {
                while idx < n {
                    out[idx] = out[idx % filled];
                    idx += 1;
                }
            }
        }
    }
    out
}

/// 把单声道 i16 复制成 2 声道交错 s16le（探测信号两路都放，保证任一只音箱都能听到）。
pub fn interleave_stereo_i16(mono: &[i16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(mono.len() * 4);
    for s in mono {
        out.extend_from_slice(&s.to_le_bytes());
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

/// 给一段交错 s16le PCM **前面垫静音**，实现「这一路晚 `delay_ms` 毫秒出声」。
///
/// 单位按 [`SYNC_RATE`] / [`SYNC_CHANNELS`] 解释；`delay_ms` 为负直接报错（要让某一路
/// 提前，正确做法是给另一路加延迟——两路的**差**才决定同步）。
pub fn apply_delay(pcm: &[u8], delay_ms: i32) -> Result<Vec<u8>> {
    apply_delay_at(pcm, delay_ms, SYNC_RATE, SYNC_CHANNELS)
}

/// [`apply_delay`] 的显式采样率/声道数版本。
pub fn apply_delay_at(pcm: &[u8], delay_ms: i32, rate: u32, channels: u16) -> Result<Vec<u8>> {
    if delay_ms < 0 {
        return Err(AudioError::invalid(format!(
            "延迟不能为负（收到 {delay_ms} ms）；要让某一路提前，请改为给另一路加延迟"
        )));
    }
    let frame_bytes = channels as usize * 2;
    if frame_bytes == 0 || !pcm.len().is_multiple_of(frame_bytes) {
        return Err(AudioError::invalid(format!(
            "PCM 长度 {} 不是帧长 {frame_bytes} 的整数倍",
            pcm.len()
        )));
    }
    let frames = (rate as u64 * delay_ms as u64 / 1000) as usize;
    let mut out = vec![0u8; frames * frame_bytes];
    out.extend_from_slice(pcm);
    Ok(out)
}

/// 交错 s16le 的统计量（麦克风电平表 / 体检都用它）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PcmStats {
    /// 全部采样点的均方根（0.0 ~ 1.0，满量程为 1.0）。
    pub rms: f64,
    /// 全部采样点的绝对值峰值（0.0 ~ 1.0）。
    pub peak: f64,
    /// 采样点个数（i16 个数，不是字节数）。
    pub samples: usize,
}

impl PcmStats {
    /// 峰值换算成 dBFS；全零返回负无穷。
    pub fn peak_dbfs(&self) -> f64 {
        dbfs(self.peak)
    }

    /// 均方根换算成 dBFS；全零返回负无穷。
    pub fn rms_dbfs(&self) -> f64 {
        dbfs(self.rms)
    }

    /// 按峰值门限给出体检结论。
    pub fn verdict(&self) -> MicVerdict {
        classify_mic_level(self.peak_dbfs(), self.samples)
    }

    /// 人读一行。
    pub fn describe(&self) -> String {
        format!(
            "rms={:.8} ({:.1} dBFS) peak={:.8} ({:.1} dBFS, {:.2} LSB) samples={} → {}",
            self.rms,
            self.rms_dbfs(),
            self.peak,
            self.peak_dbfs(),
            self.peak * 32_768.0,
            self.samples,
            self.verdict().as_str()
        )
    }
}

/// 统计一段交错 s16le PCM。
pub fn pcm_stats(bytes: &[u8]) -> PcmStats {
    let mut sum = 0.0f64;
    let mut peak = 0.0f64;
    let mut n = 0usize;
    for ch in bytes.as_chunks::<2>().0 {
        let v = i16::from_le_bytes(*ch) as f64 / 32_768.0;
        sum += v * v;
        peak = peak.max(v.abs());
        n += 1;
    }
    PcmStats {
        rms: if n == 0 { 0.0 } else { (sum / n as f64).sqrt() },
        peak,
        samples: n,
    }
}

/// 把交错 s16le 立体声降成单声道 f32：**取能量更大的那一路**。
///
/// 不做 L/R 平均是有意的——麦克风可能只录到一路，平均会把有用信号砍掉一半幅度，
/// 白白损失 6 dB 信噪比。
pub fn downmix_to_mono(bytes: &[u8]) -> Vec<f32> {
    let frames = bytes.len() / 4;
    let mut left = Vec::with_capacity(frames);
    let mut right = Vec::with_capacity(frames);
    let mut e_left = 0.0f64;
    let mut e_right = 0.0f64;
    for f in bytes.as_chunks::<4>().0 {
        let a = s16_to_f32(i16::from_le_bytes([f[0], f[1]]));
        let b = s16_to_f32(i16::from_le_bytes([f[2], f[3]]));
        e_left += (a as f64) * (a as f64);
        e_right += (b as f64) * (b as f64);
        left.push(a);
        right.push(b);
    }
    if e_right > e_left {
        right
    } else {
        left
    }
}

/// 麦克风体检结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MicVerdict {
    /// 采到了明显高于本底的信号，可以用于自动标定。
    Signal,
    /// 有电平，但只是本底噪声（典型是主板模拟输入的固有噪声），采不到真实声音。
    NoiseFloor,
    /// 数字静默：波形基本是 0 / ±1 LSB 抖动。
    Silent,
    /// 打不开设备，或者打开了但一个采样点都没拿到。
    Unavailable,
}

impl MicVerdict {
    /// 中文短句，可直接展示给用户。
    pub fn as_str(self) -> &'static str {
        match self {
            MicVerdict::Signal => "有信号",
            MicVerdict::NoiseFloor => "只有本底噪声",
            MicVerdict::Silent => "静默",
            MicVerdict::Unavailable => "打不开",
        }
    }

    /// 能不能拿来跑自动标定。
    ///
    /// **只有 [`MicVerdict::Silent`] 和 [`MicVerdict::Unavailable`] 才算不可用。**
    /// 「只有本底噪声」在安静房间里是正常现象——设备确实在采数据，只是没人说话；而标定
    /// 时探测信号是响的，互相关照样能出峰。反过来，把门限卡在环境噪声电平上会让判定在
    /// 门限附近反复横跳（本机实测同一只麦两次量到 -43.3 与 -46.7 dBFS，正好跨过 -45）。
    pub fn is_usable(self) -> bool {
        matches!(self, MicVerdict::Signal | MicVerdict::NoiseFloor)
    }

    /// 给用户看的一句话解释。
    pub fn advice(self) -> &'static str {
        match self {
            MicVerdict::Signal => "麦克风工作正常，可以自动对齐",
            MicVerdict::NoiseFloor => {
                "只采到本底噪声（安静房间正常）：设备在采数据，可以直接试自动对齐；若标定失败，再对着麦克风说话确认电平会变化"
            }
            MicVerdict::Silent => "采到的是数字静默：这个端点没有真实麦克风输入（虚拟设备或被系统置零）",
            MicVerdict::Unavailable => "打不开采集设备：可能被独占、未插入或权限不足",
        }
    }
}

impl std::fmt::Display for MicVerdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 按峰值电平给出体检结论（纯函数，便于单测）。
///
/// * `samples == 0` → [`MicVerdict::Unavailable`]
/// * 峰值 > [`MIC_SIGNAL_DBFS`] → [`MicVerdict::Signal`]
/// * 峰值 > [`MIC_NOISE_DBFS`] → [`MicVerdict::NoiseFloor`]
/// * 否则 → [`MicVerdict::Silent`]
pub fn classify_mic_level(peak_dbfs: f64, samples: usize) -> MicVerdict {
    if samples == 0 {
        return MicVerdict::Unavailable;
    }
    if peak_dbfs > MIC_SIGNAL_DBFS {
        MicVerdict::Signal
    } else if peak_dbfs > MIC_NOISE_DBFS {
        MicVerdict::NoiseFloor
    } else {
        MicVerdict::Silent
    }
}

/// 麦克风体检结果。
#[derive(Debug, Clone, PartialEq)]
pub struct MicHealth {
    /// 均方根（0.0 ~ 1.0）。
    pub rms: f64,
    /// 峰值（0.0 ~ 1.0）。
    pub peak: f64,
    /// 采样点个数（i16 个数）。
    pub samples: usize,
    /// 结论。
    pub verdict: MicVerdict,
    /// 峰值 dBFS，方便 GUI 直接显示。
    pub peak_dbfs: f64,
    /// 采集走的哪条路；设备根本没打开时为 `None`。
    pub path: Option<CapturePath>,
    /// 实际采样率（Hz）；设备没打开时为 0。
    pub rate: u32,
    /// 补充说明（采集线程错误、设备名等），无内容时为空串。
    pub note: String,
}

impl MicHealth {
    /// 打不开设备时用的结果。
    pub fn unavailable(note: impl Into<String>) -> Self {
        Self {
            rms: 0.0,
            peak: 0.0,
            samples: 0,
            verdict: MicVerdict::Unavailable,
            peak_dbfs: f64::NEG_INFINITY,
            path: None,
            rate: 0,
            note: note.into(),
        }
    }

    /// 人读一行。
    pub fn describe(&self) -> String {
        let path = match self.path {
            Some(p) => format!("{} / {} Hz", p.describe(), self.rate),
            None => "未打开".to_string(),
        };
        let base = format!(
            "{} | rms={:.8} peak={:.8} ({:.1} dBFS, {:.2} LSB) samples={} | {}",
            self.verdict.as_str(),
            self.rms,
            self.peak,
            self.peak_dbfs,
            self.peak * 32_768.0,
            self.samples,
            path,
        );
        if self.note.is_empty() {
            base
        } else {
            format!("{base} | {}", self.note)
        }
    }
}

/// 采集端点的类别（按名字推断）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MicKind {
    /// 看起来是真实麦克风（USB 麦、主板麦口、耳机麦）。
    Physical,
    /// 回环设备（立体声混音 / Stereo Mix / What U Hear），**不能**用于声学测量。
    Loopback,
    /// 虚拟音频设备（Steam、UU远程、VB-Cable 之类）。
    Virtual,
    /// 名字里看不出所以然。
    Unknown,
}

impl MicKind {
    /// 中文短句。
    pub fn as_str(self) -> &'static str {
        match self {
            MicKind::Physical => "物理麦克风",
            MicKind::Loopback => "回环设备",
            MicKind::Virtual => "虚拟设备",
            MicKind::Unknown => "未知",
        }
    }
}

impl std::fmt::Display for MicKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 一个候选麦克风：端点信息 + 类别 + 「值不值得试」的提示。
///
/// `likely_usable` 只是**按名字的启发式**，用来给下拉框排序和打标；真正的判据是
/// [`mic_health`]——本机就有 4 个「物理麦克风」名字的端点全是数字静默。
#[derive(Debug, Clone, PartialEq)]
pub struct MicCandidate {
    /// 端点本身。
    pub endpoint: EndpointInfo,
    /// 按名字推断的类别。
    pub kind: MicKind,
    /// 是否值得先试（启发式）。
    pub likely_usable: bool,
}

impl MicCandidate {
    /// 按名字给端点分类。
    pub fn classify(endpoint: EndpointInfo) -> Self {
        let kind = classify_mic_name(&endpoint.name);
        let likely_usable = matches!(kind, MicKind::Physical | MicKind::Unknown);
        Self {
            endpoint,
            kind,
            likely_usable,
        }
    }

    /// 端点名。
    pub fn name(&self) -> &str {
        &self.endpoint.name
    }

    /// 端点 ID（`{GUID}`）。
    pub fn id(&self) -> &str {
        &self.endpoint.id
    }

    /// 人读一行。
    pub fn describe(&self) -> String {
        format!(
            "{} [{}]{} 可能可用={} | {}",
            self.endpoint.name,
            self.kind.as_str(),
            if self.endpoint.is_default {
                " (默认)"
            } else {
                ""
            },
            if self.likely_usable { "是" } else { "否" },
            self.endpoint.describe(),
        )
    }
}

/// 按名字推断采集端点类别（纯函数，便于单测）。
pub fn classify_mic_name(name: &str) -> MicKind {
    let n = name.to_ascii_lowercase();
    let has = |keys: &[&str]| keys.iter().any(|k| n.contains(k));
    if has(&[
        "立体声混音",
        "stereo mix",
        "what u hear",
        "loopback",
        "回环",
        "监视器",
    ]) {
        MicKind::Loopback
    } else if has(&[
        "steam",
        "uu远程",
        "virtual",
        "虚拟",
        "vb-audio",
        "voicemeeter",
        "cable output",
        "nvidia",
        "obs",
        "discord",
    ]) {
        MicKind::Virtual
    } else if has(&[
        "麦克风",
        "microphone",
        "mic",
        "阵列",
        "array",
        "realtek",
        "hands-free",
        "handsfree",
        "耳机",
        "headset",
    ]) {
        MicKind::Physical
    } else {
        MicKind::Unknown
    }
}

/// 把用户输入（名字子串或 `{GUID}`）变成设备选择器。
///
/// `{...}` 形状按 ID 精确匹配，其余按名字子串匹配。
pub fn selector_for(spec: &str) -> DeviceSelector {
    let s = spec.trim();
    if s.starts_with('{') && s.ends_with('}') {
        DeviceSelector::Id(s.to_string())
    } else {
        DeviceSelector::name_contains(s)
    }
}

// ---------------------------------------------------------------------------
// DSP 核心：归一化互相关 + 抛物线插值
// ---------------------------------------------------------------------------

/// 计算归一化互相关曲线：`curve[l]` = `observed[l..l+n]` 与 `reference` 的相关系数。
///
/// 先各自减去整段的均值（去直流），再逐窗口归一化，这样不同响度的窗口可比。
fn ncc_curve(reference: &[f32], observed: &[f32]) -> Vec<f32> {
    let n = reference.len();
    let m = observed.len();
    if n == 0 || m < n {
        return Vec::new();
    }
    let mean = |v: &[f32]| -> f32 {
        if v.is_empty() {
            0.0
        } else {
            v.iter().sum::<f32>() / v.len() as f32
        }
    };
    let ma = mean(reference);
    let mb = mean(observed);
    let a: Vec<f32> = reference.iter().map(|x| x - ma).collect();
    let a_energy: f32 = a.iter().map(|x| x * x).sum();

    let mut curve = Vec::with_capacity(m - n + 1);
    for lag in 0..=(m - n) {
        let window = &observed[lag..lag + n];
        let mut dot = 0.0f32;
        let mut energy = 0.0f32;
        for (i, x) in window.iter().enumerate() {
            let y = x - mb;
            dot += a[i] * y;
            energy += y * y;
        }
        let denom = (a_energy * energy).sqrt();
        curve.push(if denom > 1e-20 { dot / denom } else { 0.0 });
    }
    curve
}

/// 取曲线最大值的位置与数值。
fn argmax(curve: &[f32]) -> (usize, f32) {
    let mut best = 0usize;
    let mut best_v = f32::NEG_INFINITY;
    for (i, v) in curve.iter().enumerate() {
        if *v > best_v {
            best_v = *v;
            best = i;
        }
    }
    if curve.is_empty() {
        (0, 0.0)
    } else {
        (best, best_v)
    }
}

/// 用峰值两侧的三个点做抛物线插值，得到亚采样点峰值位置。
fn parabolic_peak(curve: &[f32], idx: usize) -> f64 {
    if idx == 0 || idx + 1 >= curve.len() {
        return idx as f64;
    }
    let y0 = curve[idx - 1] as f64;
    let y1 = curve[idx] as f64;
    let y2 = curve[idx + 1] as f64;
    let denom = y0 - 2.0 * y1 + y2;
    if denom.abs() < 1e-12 {
        return idx as f64;
    }
    let delta = (0.5 * (y0 - y2) / denom).clamp(-0.5, 0.5);
    idx as f64 + delta
}

/// 峰值与「峰外均方根」的比值（线性）。越干净越大。
fn peak_snr_of(curve: &[f32], idx: usize) -> f32 {
    let mut sum = 0.0f64;
    let mut n = 0usize;
    for (i, v) in curve.iter().enumerate() {
        if i.abs_diff(idx) <= 2 {
            continue;
        }
        sum += (*v as f64) * (*v as f64);
        n += 1;
    }
    if n == 0 {
        return 0.0;
    }
    let rms = (sum / n as f64).sqrt();
    let peak = curve.get(idx).copied().unwrap_or(0.0) as f64;
    if rms <= 1e-12 {
        f32::INFINITY
    } else {
        (peak / rms) as f32
    }
}

/// 由峰值与信噪比合成 0.0 ~ 1.0 的置信度。
///
/// 两个因子都是**门槛式**的，不是乘系数：
/// * `peak` 是归一化互相关值。真实声学路径（蓝牙编解码 + 房间混响）会把波形改得面目
///   全非，本机实测只有 0.29——但峰值位置依然毫不含糊。所以门槛定在 0.25，超过就当作
///   「像」，不再惩罚；
/// * `snr` 是峰值与本底曲线 RMS 的比。它才是「这个峰到底可不可信」的判据：本机实测
///   有线自比 80~87、蓝牙 28~37、而用点击串（周期信号，有歧义峰）只有 4.8。门槛 8。
fn confidence_of(peak: f32, snr: f32) -> f32 {
    let p = (peak as f64 / 0.25).clamp(0.0, 1.0);
    let s = (snr as f64 / 8.0).clamp(0.0, 1.0);
    (p * s) as f32
}

/// 完整互相关：整数滞后、亚采样滞后、峰值、信噪比、置信度一次算齐。
///
/// 语义：把 `reference`（短，探测信号）在 `observed`（长，录音）上滑动，返回
/// `observed[lag .. lag + reference.len()]` 最像 `reference` 的 `lag`。
/// `observed` 比 `reference` 短（或 `reference` 为空）时返回全零结果。
pub fn cross_correlate_detailed(reference: &[f32], observed: &[f32]) -> Correlation {
    let curve = ncc_curve(reference, observed);
    if curve.is_empty() {
        return Correlation {
            lag: 0,
            lag_sub: 0.0,
            peak: 0.0,
            peak_snr: 0.0,
            confidence: 0.0,
        };
    }
    let (lag, peak) = argmax(&curve);
    let lag_sub = parabolic_peak(&curve, lag);
    let peak_snr = peak_snr_of(&curve, lag);
    Correlation {
        lag,
        lag_sub,
        peak,
        peak_snr,
        confidence: confidence_of(peak, peak_snr),
    }
}

/// 互相关，返回 `(整数滞后, 归一化峰值)`。
///
/// 需要亚采样精度时用 [`cross_correlate_sub`]，需要信噪比/置信度时用
/// [`cross_correlate_detailed`]。
pub fn cross_correlate(reference: &[f32], observed: &[f32]) -> (usize, f32) {
    let c = cross_correlate_detailed(reference, observed);
    (c.lag, c.peak)
}

/// 互相关，返回 `(抛物线插值后的亚采样滞后, 归一化峰值)`。
pub fn cross_correlate_sub(reference: &[f32], observed: &[f32]) -> (f64, f32) {
    let c = cross_correlate_detailed(reference, observed);
    (c.lag_sub, c.peak)
}

/// 把 i16 探测信号转成 f32（满量程 1.0）。
pub fn probe_to_f32(probe: &[i16]) -> Vec<f32> {
    probe.iter().map(|&s| s as f32 / 32_768.0).collect()
}

// ---------------------------------------------------------------------------
// 手动对齐：不需要麦克风也能标定
// ---------------------------------------------------------------------------

/// 手动对齐用的点击串：每 [`ALIGN_CLICK_PERIOD_MS`] 一个 2 ms 短音，每 4 下加重一次。
///
/// 左右两路放**完全相同**的串是有意的：对齐时两串重合，用户听到的是「一下」；没对齐
/// 时听到双击/镶边（flam）。拖延迟值直到双击消失即可。
pub fn alignment_clicks(ms: u32, amplitude: f32) -> Vec<i16> {
    let ms = ms.clamp(1, 60_000);
    let n = (SYNC_RATE as u64 * ms as u64 / 1000).max(1) as usize;
    let period = (SYNC_RATE as usize * ALIGN_CLICK_PERIOD_MS as usize / 1000).max(8);
    let click = (SYNC_RATE as usize / 500).max(4); // 2 ms
    let amp = (amplitude.clamp(0.0, 1.0) as f64) * i16::MAX as f64;
    let mut out = vec![0i16; n];
    let mut start = 0usize;
    let mut k = 0usize;
    while start + click <= n {
        let a = if k.is_multiple_of(4) { amp } else { amp * 0.45 };
        for i in 0..click {
            let t = i as f64 / click as f64;
            let env = 0.5 - 0.5 * (std::f64::consts::TAU * t).cos();
            let s = (std::f64::consts::TAU * 2000.0 * i as f64 / SYNC_RATE as f64).sin();
            out[start + i] = (s * env * a) as i16;
        }
        start += period;
        k += 1;
    }
    out
}

fn dbfs(x: f64) -> f64 {
    if x <= 0.0 {
        f64::NEG_INFINITY
    } else {
        20.0 * x.log10()
    }
}

// ---------------------------------------------------------------------------
// 真机路径（需要 Windows / WASAPI）
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// 采集：独占模式优先，共享模式兜底
// ---------------------------------------------------------------------------

/// 音频是从哪条路采回来的。
///
/// 本机实测证明这个区分不是学术问题：同一只麦克风、同一段声音，独占模式能采到，
/// 共享模式采不到（见模块头「本机现状」）。GUI 应当把 [`CapturePath::as_str`] 存进
/// 配置并在界面上显示，用户才知道自己的机器走的是哪条路。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapturePath {
    /// 独占模式：绕过 Windows 音频引擎、任何 APO 与重采样，直接拿设备原生格式的数据。
    Exclusive,
    /// 共享模式：走 Windows 音频引擎（本 crate 的 [`AudioCapture`]）。
    Shared,
}

impl CapturePath {
    /// 稳定的英文标识（GUI 存配置用）。
    pub fn as_str(self) -> &'static str {
        match self {
            CapturePath::Exclusive => "exclusive",
            CapturePath::Shared => "shared",
        }
    }

    /// 中文说明。
    pub fn describe(self) -> &'static str {
        match self {
            CapturePath::Exclusive => "独占模式（绕过音频引擎与 APO，设备原生格式）",
            CapturePath::Shared => "共享模式（Windows 音频引擎）",
        }
    }
}

impl std::fmt::Display for CapturePath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 独占模式按顺序尝试的「采样率 / 声道数」组合（位深固定 16）。
///
/// 顺序是有讲究的：USB 声卡最常见是 48000 Hz 单声道（本机 Chu2 DSP 就**只**接受
/// 48000/1/16，别的组合都返回 `AUDCLNT_E_UNSUPPORTED_FORMAT`），其次是 48000 立体声。
pub const EXCLUSIVE_FORMATS: [(u32, u16); 6] = [
    (48_000, 1),
    (48_000, 2),
    (44_100, 2),
    (44_100, 1),
    (32_000, 1),
    (96_000, 2),
];

/// 独占模式开流后的预热时长（ms）——这段数据**必须丢掉**。
///
/// 实测：本机 Chu2 DSP 独占开流后的前 ~400 ms 是一个巨大的直流建立尖峰
/// （首 100 ms RMS ≈ 14000 LSB ≈ -12 dBFS，随后指数衰减到 ~17 LSB 本底）。
/// 不丢掉它，[`mic_health`] 会把「麦克风没插/采不到」误判成「有信号」，自动标定也会
/// 在错误的位置找峰。
pub const EXCLUSIVE_WARMUP_MS: u32 = 600;

/// 一次单声道采集的结果。
#[derive(Debug, Clone)]
pub struct CapturedAudio {
    /// 单声道样本（-1.0 ~ 1.0），已剔除预热段。
    pub samples: Vec<f32>,
    /// 实际采样率（Hz）——**独占模式下不一定是 [`SYNC_RATE`]**，
    /// 算毫秒必须用它，不能用常量。
    pub rate: u32,
    /// 采集路径。
    pub path: CapturePath,
    /// 设备名。
    pub device_name: String,
    /// 设备 ID。
    pub device_id: String,
    /// 原始 PCM 统计（预热段已剔除）。
    pub stats: PcmStats,
}

impl CapturedAudio {
    /// 这段录音有多长（秒）。
    pub fn seconds(&self) -> f64 {
        if self.rate == 0 {
            0.0
        } else {
            self.samples.len() as f64 / self.rate as f64
        }
    }

    /// 人读一行。
    pub fn describe(&self) -> String {
        format!(
            "{} | {} Hz | {:.2}s | {} | {}",
            self.device_name,
            self.rate,
            self.seconds(),
            self.path.describe(),
            self.stats.describe()
        )
    }
}

/// 把交错 s16le 的 `channels` 声道 PCM 降成单声道 f32：**取能量最大的那一路**。
///
/// 与 [`downmix_to_mono`] 同一个理由：不做平均，避免把只录到一路的信号砍掉 6 dB。
#[cfg(windows)]
fn s16_mono(bytes: &[u8], channels: u16) -> Vec<f32> {
    let ch = channels.max(1) as usize;
    if ch == 1 {
        return bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| s16_to_f32(i16::from_le_bytes(*c)))
            .collect();
    }
    let frames = bytes.len() / (2 * ch);
    let mut best: Vec<f32> = Vec::with_capacity(frames);
    let mut best_energy = -1.0f64;
    for c in 0..ch {
        let mut v = Vec::with_capacity(frames);
        let mut energy = 0.0f64;
        for f in 0..frames {
            let i = f * ch * 2 + c * 2;
            let s = s16_to_f32(i16::from_le_bytes([bytes[i], bytes[i + 1]]));
            energy += (s as f64) * (s as f64);
            v.push(s);
        }
        if energy > best_energy {
            best_energy = energy;
            best = v;
        }
    }
    best
}

/// 用指定格式在独占模式下裸采一段，返回交错 s16le 字节（预热段已剔除）。
#[cfg(windows)]
fn exclusive_capture_s16(
    rd: &ResolvedDevice,
    rate: u32,
    channels: u16,
    total_ms: u32,
) -> Result<Vec<u8>> {
    let client = com::activate_client(&rd.device)?;
    let fmt = com::pcm16_waveformatex(PcmFormat::new(rate, channels, 16));
    let block = channels as usize * 2;
    let hns = com::ms_to_hns(200);

    unsafe {
        client
            .Initialize(AUDCLNT_SHAREMODE_EXCLUSIVE, 0, hns, 0, &fmt, None)
            .map_err(|e| {
                AudioError::Wasapi(
                    e.code().0,
                    format!(
                        "独占 Initialize({rate} Hz/{channels} ch/16 bit) 失败：{}",
                        e.message()
                    ),
                )
            })?;
    }
    let cap: IAudioCaptureClient = unsafe { client.GetService()? };
    unsafe { client.Start()? };

    let warm_frames = (EXCLUSIVE_WARMUP_MS as u64 * rate as u64 / 1000) as usize;
    let want_frames = (total_ms as u64 * rate as u64 / 1000) as usize + warm_frames;
    let mut raw: Vec<u8> = Vec::with_capacity(want_frames * block);
    let mut got_frames = 0usize;
    let deadline = Instant::now()
        + Duration::from_millis(total_ms as u64 + EXCLUSIVE_WARMUP_MS as u64 + 2_000);

    while got_frames < want_frames && Instant::now() < deadline {
        let mut pdata: *mut u8 = std::ptr::null_mut();
        let mut frames: u32 = 0;
        let mut flags: u32 = 0;
        match unsafe { cap.GetBuffer(&mut pdata, &mut frames, &mut flags, None, None) } {
            Ok(()) => {
                let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
                let n = frames as usize * block;
                if frames > 0 {
                    if silent || pdata.is_null() {
                        raw.resize(raw.len() + n, 0);
                    } else {
                        raw.extend_from_slice(unsafe { std::slice::from_raw_parts(pdata, n) });
                    }
                    got_frames += frames as usize;
                }
                let _ = unsafe { cap.ReleaseBuffer(frames) };
            }
            Err(e) => {
                if e.code().0 == AUDCLNT_S_BUFFER_EMPTY.0 {
                    std::thread::sleep(Duration::from_millis(2));
                    continue;
                }
                let _ = unsafe { client.Stop() };
                return Err(AudioError::Wasapi(
                    e.code().0,
                    format!("独占 GetBuffer 失败：{}", e.message()),
                ));
            }
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    let _ = unsafe { client.Stop() };

    let warm_bytes = (warm_frames * block).min(raw.len());
    Ok(raw.split_off(warm_bytes))
}

/// 独占模式采集一段单声道音频（Windows）。
///
/// 会按 [`EXCLUSIVE_FORMATS`] 依次试，用第一个 `Initialize` 成功的组合。全部失败时返回
/// 最后一次的 WASAPI 错误——最常见的是 `0x8889000A`（设备被别的程序独占）与
/// `0x88890008`（格式都不支持）。
#[cfg(windows)]
pub fn capture_mono_exclusive(mic_endpoint: &str, ms: u32) -> Result<CapturedAudio> {
    let _com = ComGuard::new()?;
    let en = com::enumerator()?;
    let rd = com::resolve(&en, &selector_for(mic_endpoint), DataFlow::Capture)?;
    let ms = ms.clamp(20, 10_000);

    let mut last: Option<AudioError> = None;
    for (rate, channels) in EXCLUSIVE_FORMATS {
        match exclusive_capture_s16(&rd, rate, channels, ms) {
            Ok(raw) => {
                return Ok(CapturedAudio {
                    samples: s16_mono(&raw, channels),
                    rate,
                    path: CapturePath::Exclusive,
                    device_name: rd.name.clone(),
                    device_id: rd.id.clone(),
                    stats: pcm_stats(&raw),
                });
            }
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| AudioError::invalid("独占模式没有任何可用格式")))
}

/// 共享模式采集一段单声道音频（Windows），走本 crate 的 [`AudioCapture`]。
#[cfg(windows)]
fn capture_mono_shared(mic_endpoint: &str, ms: u32) -> Result<CapturedAudio> {
    let cfg = CaptureConfig {
        device: selector_for(mic_endpoint),
        samplerate: SYNC_RATE,
        channels: SYNC_CHANNELS,
        mode: CaptureMode::Capture,
        ring_seconds: 5.0,
        buffer_ms: 200,
    };
    cfg.validate()?;
    let mut cap = AudioCapture::new(cfg)?;
    cap.start()?;
    let device_name = cap.device_name().unwrap_or_default().to_string();
    let device_id = cap.device_id().unwrap_or_default().to_string();

    // 丢掉流刚起来的 250 ms（端点上电、ring 里可能残留旧数据）
    std::thread::sleep(Duration::from_millis(250));
    let _ = cap.read_vec(64 * 1024, Duration::from_millis(200));

    let want = (ms as usize * SYNC_RATE as usize / 1000) * SYNC_CHANNELS as usize * 2;
    let mut all: Vec<u8> = Vec::with_capacity(want);
    let deadline = Instant::now() + Duration::from_millis(ms as u64 + 800);
    while all.len() < want && Instant::now() < deadline {
        let chunk = cap.read_vec(16 * 1024, Duration::from_millis(200))?;
        if chunk.is_empty() && cap.last_error().is_some() {
            break;
        }
        all.extend_from_slice(&chunk);
    }
    cap.stop();
    if let Some(e) = cap.last_error() {
        return Err(AudioError::invalid(format!("共享模式采集线程错误：{e}")));
    }
    Ok(CapturedAudio {
        samples: downmix_to_mono(&all),
        rate: SYNC_RATE,
        path: CapturePath::Shared,
        device_name,
        device_id,
        stats: pcm_stats(&all),
    })
}

/// 采集一段单声道音频：**先试独占模式，失败再退回共享模式**（Windows）。
///
/// 这是自动标定与麦克风体检共用的入口。之所以独占优先，是因为本机实测证明共享模式在
/// 某些 USB 声卡上采不到真实数据（见模块头「本机现状」）；共享模式仍然保留，因为对
/// 绝大多数板载/USB 麦克风它是好的，而且独占模式可能被别的程序占用而打不开。
#[cfg(windows)]
pub fn capture_mono(mic_endpoint: &str, ms: u32) -> Result<CapturedAudio> {
    if mic_endpoint.trim().is_empty() {
        return Err(AudioError::invalid("麦克风名字/ID 不能为空"));
    }
    let ms = ms.clamp(20, 10_000);
    match capture_mono_exclusive(mic_endpoint, ms) {
        Ok(a) => Ok(a),
        Err(ex) => capture_mono_shared(mic_endpoint, ms).map_err(|sh| {
            AudioError::invalid(format!("独占模式失败（{ex}）；共享模式也失败（{sh}）"))
        }),
    }
}

/// 列出本机所有**活动**的采集端点，并给出类别与「值不值得先试」的提示。
///
/// 注意 [`list_endpoints`] 只枚举活动端点
/// （`DEVICE_STATE_ACTIVE`），所以列表里的端点 `DeviceState` 都是 1；真麦克风如果没插
/// （state=4/8）**根本不会出现在这个列表里**——这正是「本机没有可用麦克风」最直接的
/// 表现。列表按「可能可用 → 默认端点 → 名字」排序。
#[cfg(windows)]
pub fn list_microphones() -> Vec<MicCandidate> {
    let Ok(all) = list_endpoints() else {
        return Vec::new();
    };
    let mut v: Vec<MicCandidate> = all
        .into_iter()
        .filter(|e| e.flow == DataFlow::Capture)
        .map(MicCandidate::classify)
        .collect();
    v.sort_by(|a, b| {
        b.likely_usable
            .cmp(&a.likely_usable)
            .then(b.endpoint.is_default.cmp(&a.endpoint.is_default))
            .then(a.endpoint.name.cmp(&b.endpoint.name))
    });
    v
}

/// 给一只麦克风做体检：录 `ms` 毫秒，统计 RMS/峰值并给出结论。
///
/// 这是 GUI 在用户选完麦克风后应该立刻调用的东西：提前告诉用户「这只麦没有信号」，
/// 比让他等自动标定失败要好。**注意它走的是 [`capture_mono`]**，也就是先试独占模式、
/// 失败再退回共享模式——本机实测共享模式在 Chu2 DSP 上采不到真实数据，只有独占模式能，
/// 所以不要用共享模式的 `AudioCapture` 结果去否定硬件。
///
/// **打不开设备不会返回 `Err`**：会返回 `Ok(MicHealth { verdict: Unavailable, .. })`，
/// 原因放在 `note` 里，这样 GUI 可以用一条路径统一展示。`Err` 只用于参数非法。
#[cfg(windows)]
pub fn mic_health(mic_endpoint: &str, ms: u32) -> Result<MicHealth> {
    if mic_endpoint.trim().is_empty() {
        return Err(AudioError::invalid("麦克风名字/ID 不能为空"));
    }
    let ms = ms.clamp(50, 10_000);
    match capture_mono(mic_endpoint, ms) {
        Ok(a) => {
            let mut note = format!("{} | {}", a.device_name, a.path.describe());
            if a.path == CapturePath::Shared {
                note.push_str("（本机实测共享模式可能采不到 USB 声卡的真实数据）");
            }
            Ok(MicHealth {
                rms: a.stats.rms,
                peak: a.stats.peak,
                samples: a.stats.samples,
                verdict: a.stats.verdict(),
                peak_dbfs: a.stats.peak_dbfs(),
                path: Some(a.path),
                rate: a.rate,
                note,
            })
        }
        Err(e) => Ok(MicHealth::unavailable(format!("打不开采集设备：{e}"))),
    }
}

/// 测一次「从发起播放到声音到达麦克风」的延迟（ms）。
///
/// 用 [`SyncConfig::mic_endpoint`] 录、[`SyncConfig::probe_endpoint`] 播。返回值里含
/// 约 1 秒的播放器固定前置静音与设备启动延迟，**绝对值没有意义**；要的是两次测量之
/// 差，见 [`calibrate_pair`]。
#[cfg(windows)]
pub fn measure_endpoint_delay(cfg: &SyncConfig) -> Result<f64> {
    Ok(measure_one(cfg)?.0)
}

/// 测一次延迟，同时把互相关的细节与原始录音带出来（内部用）。
///
/// 时序：先起播放线程（它自己会 sleep 100 ms 再调 `play_pcm_s16`，后者还要预填约 1 s
/// 缓冲才真正出声），主线程**同时**用 [`capture_mono`] 录 `cfg.window_ms()` 那么长。
/// 所以窗口必须覆盖 `pre_roll_ms`（默认 2 s），否则声音还没出来就录完了。
///
/// **采样率**：独占模式常见 48000 Hz，而探测信号是在 [`SYNC_RATE`]（44100）生成的。
/// 必须把参考信号重采样到实测采样率再互相关，否则时间轴会错配 48000/44100 ≈ 8.8%
/// （探针在窗口里偏移约 1 s → 误差约 88 ms，直接毁掉 ≤1 ms 的目标）。
#[cfg(windows)]
fn measure_one(cfg: &SyncConfig) -> Result<(f64, Correlation, CapturedAudio)> {
    if cfg.mic_endpoint.trim().is_empty() {
        return Err(AudioError::invalid("SyncConfig.mic_endpoint 不能为空"));
    }
    if cfg.probe_endpoint.trim().is_empty() {
        return Err(AudioError::invalid("SyncConfig.probe_endpoint 不能为空"));
    }
    let probe = probe_signal_at(cfg.probe, cfg.probe_ms, cfg.amplitude, SYNC_RATE);
    if probe.iter().all(|&s| s == 0) {
        return Err(AudioError::invalid(
            "探测信号全零：检查 amplitude 是否被设成了 0",
        ));
    }
    let reference = probe_to_f32(&probe);
    let pcm = interleave_stereo_i16(&probe);
    let format = PcmFormat::new(SYNC_RATE, SYNC_CHANNELS, 16);

    let window_ms = cfg.window_ms();

    // 播放必须在独立线程里：play_pcm_s16 是阻塞的，主线程要同时录音。
    // 它内部自己 ComGuard::new()，所以跨线程调用是安全的。
    let device = cfg.probe_endpoint.clone();
    let player = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        play_pcm_s16(&selector_for(&device), format, &pcm)
    });

    let captured = capture_mono(&cfg.mic_endpoint, window_ms);

    let played = match player.join() {
        Ok(Ok(info)) => Ok(info),
        Ok(Err(e)) => Err(format!("播放失败：{e}")),
        Err(_) => Err("播放线程 panic".to_string()),
    };
    let captured = captured?;

    // 参考信号对齐到实测采样率
    let reference = if captured.rate == SYNC_RATE || captured.rate == 0 {
        reference
    } else {
        resample_f32(&reference, 1, SYNC_RATE, captured.rate)
    };

    let correlation = cross_correlate_detailed(&reference, &captured.samples);
    if correlation.peak <= 0.0 {
        let extra = match &played {
            Ok(info) => format!(
                "（播放端点：{}；采集：{}）",
                info.device_name,
                captured.describe()
            ),
            Err(e) => format!("（{e}；采集：{}）", captured.describe()),
        };
        return Err(AudioError::invalid(format!(
            "互相关找不到峰：麦克风很可能采不到声音{extra}"
        )));
    }
    Ok((
        lag_ms(correlation.lag_sub, captured.rate),
        correlation,
        captured,
    ))
}

/// 自动标定（**依次法**）：先往左端点播探测信号录一次，再往右端点播录一次，互相关求差。
///
/// `cfg` 只提供探测信号形状（[`SyncConfig::probe`] / `probe_ms` / `amplitude`）与窗口
/// 余量；端点由参数给出（会覆盖 `cfg` 里的同名字段）。返回的 `left_ms` / `right_ms`
/// 已归一化，两者中较小者为 0。
///
/// **精度只有 ±4 ms 量级，不要直接用它**——两次独立采集的起始相位抖动会原样进 delta
/// （本机实测：同一个有线端点自比，本该是 0，却给出 -4.0 ~ +3.0 ms）。默认走
/// [`calibrate_pair`]（同时法）；这个函数只在同时法失败时兜底。
#[cfg(windows)]
pub fn calibrate_pair_sequential(
    mic: &str,
    left_endpoint: &str,
    right_endpoint: &str,
    cfg: &SyncConfig,
) -> Result<SyncResult> {
    let mut c = cfg.clone();
    c.mic_endpoint = mic.to_string();

    c.probe_endpoint = left_endpoint.to_string();
    let (left_raw, left_corr, _) = measure_one(&c)?;

    c.probe_endpoint = right_endpoint.to_string();
    let (right_raw, right_corr, _) = measure_one(&c)?;

    let base = left_raw.min(right_raw);
    Ok(SyncResult {
        left_ms: left_raw - base,
        right_ms: right_raw - base,
        delta_ms: left_raw - right_raw,
        confidence: left_corr.confidence.min(right_corr.confidence),
        peak_snr: left_corr.peak_snr.min(right_corr.peak_snr),
        method: SyncMethod::Sequential,
    })
}

/// 自动标定（**同时法，默认路径**）：左右两路**同时**播各自的探测信号，从**同一段录音**
/// 里分别互相关，取两次到达时刻之差。
///
/// 为什么必须同时测：录音流的起始相位每次都不一样（由 WASAPI 缓冲周期决定，本机实测抖动
/// ±4 ms）。依次测时它分别落在两次测量里，delta 直接被污染；同时测时两路共享同一段录音，
/// 它是共模的，在 delta 里自动抵消。这也是唯一能达到 ≤1 ms 精度的做法。
///
/// 两路用的是**频带互不重叠**的扫频（[`PAIR_BAND_LEFT`] / [`PAIR_BAND_RIGHT`]），所以在
/// 时域上近似正交，不需要额外滤波就能各自相关出各自的峰。
///
/// 麦克风采不到声音时返回 `Err`——GUI 应当先用 [`mic_health`] 拦一道。
#[cfg(windows)]
pub fn calibrate_pair_simultaneous(
    mic: &str,
    left_endpoint: &str,
    right_endpoint: &str,
    cfg: &SyncConfig,
) -> Result<SyncResult> {
    if mic.trim().is_empty() {
        return Err(AudioError::invalid("麦克风名字/ID 不能为空"));
    }
    if left_endpoint.trim().is_empty() || right_endpoint.trim().is_empty() {
        return Err(AudioError::invalid("左右端点都不能为空"));
    }

    let (probe_l, probe_r) = probe_pair(cfg.probe_ms, cfg.amplitude, SYNC_RATE);
    if probe_l.iter().all(|&s| s == 0) || probe_r.iter().all(|&s| s == 0) {
        return Err(AudioError::invalid(
            "探测信号全零：检查 amplitude 是否被设成了 0",
        ));
    }
    let ref_l = probe_to_f32(&probe_l);
    let ref_r = probe_to_f32(&probe_r);
    let pcm_l = interleave_stereo_i16(&probe_l);
    let pcm_r = interleave_stereo_i16(&probe_r);
    let format = PcmFormat::new(SYNC_RATE, SYNC_CHANNELS, 16);
    let window_ms = cfg.window_ms();

    // 三方才栅：两个播放线程 + 主线程（采集）同时起跑。两个播放线程睡同样长的时间再调
    // play_pcm_s16，所以它们的软件起播偏差只有线程调度量级（亚毫秒），而设备自己的起播
    // 延迟差正是我们要测的东西。
    let gate = std::sync::Arc::new(std::sync::Barrier::new(3));
    let (gate_l, gate_r) = (gate.clone(), gate.clone());
    let (dev_l, dev_r) = (left_endpoint.to_string(), right_endpoint.to_string());
    let handle_l = std::thread::spawn(move || {
        gate_l.wait();
        std::thread::sleep(Duration::from_millis(50));
        play_pcm_s16(&selector_for(&dev_l), format, &pcm_l)
    });
    let handle_r = std::thread::spawn(move || {
        gate_r.wait();
        std::thread::sleep(Duration::from_millis(50));
        play_pcm_s16(&selector_for(&dev_r), format, &pcm_r)
    });
    gate.wait();

    let captured = capture_mono(mic, window_ms);
    let played_l = join_player(handle_l);
    let played_r = join_player(handle_r);
    let captured = captured?;

    let ref_l = align_reference(ref_l, captured.rate);
    let ref_r = align_reference(ref_r, captured.rate);
    let corr_l = cross_correlate_detailed(&ref_l, &captured.samples);
    let corr_r = cross_correlate_detailed(&ref_r, &captured.samples);
    if corr_l.peak <= 0.0 || corr_r.peak <= 0.0 {
        let extra = match (&played_l, &played_r) {
            (Ok(a), Ok(b)) => format!(
                "（播放端点：{} / {}；采集：{}）",
                a.device_name,
                b.device_name,
                captured.describe()
            ),
            (Err(e), _) | (_, Err(e)) => format!("（{e}；采集：{}）", captured.describe()),
        };
        return Err(AudioError::invalid(format!(
            "互相关找不到峰：麦克风很可能采不到声音{extra}"
        )));
    }

    let rate = captured.rate.max(1) as f64;
    let l_raw = corr_l.lag_sub * 1000.0 / rate;
    let r_raw = corr_r.lag_sub * 1000.0 / rate;
    let base = l_raw.min(r_raw);
    Ok(SyncResult {
        left_ms: l_raw - base,
        right_ms: r_raw - base,
        delta_ms: l_raw - r_raw,
        confidence: corr_l.confidence.min(corr_r.confidence),
        peak_snr: corr_l.peak_snr.min(corr_r.peak_snr),
        method: SyncMethod::Simultaneous,
    })
}

/// 自动标定（默认入口）：先试**同时法**，失败再退回依次法。
///
/// 返回的 [`SyncResult::method`] 会告诉你实际用的是哪一种——依次法的精度只有 ±4 ms 量级，
/// GUI 应当把这一点显示给用户（而不是假装结果一样可信）。
#[cfg(windows)]
pub fn calibrate_pair(
    mic: &str,
    left_endpoint: &str,
    right_endpoint: &str,
    cfg: &SyncConfig,
) -> Result<SyncResult> {
    match calibrate_pair_simultaneous(mic, left_endpoint, right_endpoint, cfg) {
        Ok(r) => Ok(r),
        Err(sim) => {
            calibrate_pair_sequential(mic, left_endpoint, right_endpoint, cfg).map_err(|seq| {
                AudioError::invalid(format!("同时法失败（{sim}）；依次法也失败（{seq}）"))
            })
        }
    }
}

/// 等一个播放线程收尾，把 `Result<PlaybackInfo>` 归一成 `Result<PlaybackInfo, String>`。
#[cfg(windows)]
fn join_player(
    handle: std::thread::JoinHandle<Result<crate::playback::PlaybackInfo>>,
) -> std::result::Result<crate::playback::PlaybackInfo, String> {
    match handle.join() {
        Ok(Ok(info)) => Ok(info),
        Ok(Err(e)) => Err(format!("播放失败：{e}")),
        Err(_) => Err("播放线程 panic".to_string()),
    }
}

/// 把参考信号对齐到实际采集采样率（不等就重采样）。
#[cfg(windows)]
fn align_reference(reference: Vec<f32>, rate: u32) -> Vec<f32> {
    if rate == 0 || rate == SYNC_RATE {
        reference
    } else {
        resample_f32(&reference, 1, SYNC_RATE, rate)
    }
}

// ---------------------------------------------------------------------------
// 极性 / 梳状滤波诊断
//
// 用户说「人声太低、音质有损」时，声学上最可能的两个原因是：
//
// 1. 两只箱**极性相反**——居中人声在两箱之间反相抵消；
// 2. 两只箱**到达时间不同**（毫秒级）——合成响应被梳状滤波挖出等间距的深谷。
//
// 这一节把两者都测出来。所有结论都基于**相对**量：麦克风自己的极性未知，所以「绝对极性」
// 没有意义，只有两路之间的相对极性可判。
// ---------------------------------------------------------------------------

/// 极性 / 梳状诊断用的扫频频带（Hz）。
///
/// 比 [`SWEEP_BAND`] 更靠低端：极性看的是相位，低频段相移小、结论稳；人声能量也主要在
/// 这一段。
pub const POLARITY_BAND: (f64, f64) = (300.0, 4_000.0);

/// 极性诊断探测信号的默认时长（ms）。
pub const POLARITY_PROBE_MS: u32 = 400;

/// 互相关峰值绝对值小于这个数就当作「没测到」，不参与极性判断。
pub const POLARITY_MIN_ABS: f32 = 0.15;

/// 梳状曲线取多少个频点（对数等间隔）。
pub const COMB_POINTS: usize = 32;

/// 梳状曲线每个频点用的分析窗长度（ms）。
///
/// 上限由物理决定：窗内两路的干涉相位必须近似恒定，即 `2π·Δ·μ·W << π`（μ 是扫频速率）；
/// 下限由频率分辨率决定，`1/W` 要明显小于梳状周期 `1/Δ`。
pub const COMB_WINDOW_MS: f64 = 24.0;

/// 合成峰值比低于这个值就算「有可闻抵消」。
pub const COMB_NOTCH_RATIO: f64 = 0.7;

/// 判断「哪一个峰算直达声」时，`|峰|` 至少要达到最大 `|峰|` 的这个比例。
///
/// 实测（一只线麦放桌上、两只蓝牙音箱在房间里）互相关曲线**不是单峰**：
/// 每个端点都有一个 +0.43 的峰和其后 ~7 ms 处一个 −0.57 的反相强反射峰。取「最大 `|峰|`」
/// 会量到反射而不是直达声，符号还会随两次幅度涨落随机翻转（实测某只箱三次 +0.58/+0.58/−0.57）。
/// 所以极性判断必须用**最早的显著峰**（直达声先到）。
pub const POLARITY_DIRECT_FRACTION: f32 = 0.3;

/// 单个端点的极性 / 到达诊断结果。
///
/// **绝对极性无法判定**：麦克风自己的极性未知，所以 [`Self::inverted`] 只表示「这一路相对
/// 参考信号反相」。真正有意义的是两次测量之间 `correlation` 的**符号是否相反**——符号
/// 相反就是两只箱反相（两次测量必须用同一段参考信号，本模块的公开入口都是这样做的）。
///
/// 判断极性请用 [`Self::direct`]（最早的显著峰 = 直达声），不要用 [`Self::correlation`]
/// （最大 `|峰|`，在强反射环境里会被反相反射主导，见 [`POLARITY_DIRECT_FRACTION`]）。
#[derive(Debug, Clone, Default)]
pub struct PolarityResult {
    /// 被测端点（用户给的子串或 ID）。
    pub endpoint: String,
    /// 探测信号在录音里的到达位置（ms）。绝对值含播放器的前置静音，只有**相对值**有意义。
    pub delay_ms: f64,
    /// **带符号**的归一化互相关峰值（最大 `|峰|`）。负 = 录到的波形相对参考信号反相。
    ///
    /// ⚠ 在强反射环境里这个峰可能是**反相的反射**而不是直达声；判极性用 [`Self::direct`]。
    pub correlation: f32,
    /// **直达声**的带符号相关：`peaks` 里最早的、`|值| ≥ POLARITY_DIRECT_FRACTION × 最大|峰|` 的那个。
    ///
    /// 直达声总是先到（因果系统），所以它的符号才是这只箱真正的极性。实测两只箱
    /// 的 `direct` **都是正的** → 两箱同相，没有反相抵消。
    pub direct: f32,
    /// 直达声峰所在的滞后（ms），与 [`Self::delay_ms`] 同一坐标系。
    pub direct_ms: f64,
    /// `direct` 为负且绝对值 ≥ [`POLARITY_MIN_ABS`]。
    pub inverted: bool,
    /// 峰值 / 峰外均方根，越大越可信。
    pub peak_snr: f32,
    /// 参与相关的录音采样数。
    pub samples: usize,
    /// 录音的时域峰值（0~1）。
    pub peak_abs: f32,
    /// 录音的 RMS（0~1）。
    pub rms: f32,
    /// 实际采集采样率（Hz）。
    pub rate: u32,
    /// 互相关曲线上前 4 个互不相邻的 `|峰|`，`(滞后 ms, 带符号相关)`，按滞后升序。
    ///
    /// 用来验证「峰是不是唯一的」：正常情况第一项就是直达声，其余项远小于它。若出现第二项
    /// 符号相反且 `|值|` 与第一项相当，则「最大 `|峰|`」的符号不可信
    /// （见 [`cross_correlate_peaks`] 的说明）。
    pub peaks: Vec<(f64, f32)>,
}

impl PolarityResult {
    /// 人读一行。
    pub fn describe(&self) -> String {
        format!(
            "{} | 直达 {:+.3} @ {:.3} ms{} | 最大|峰| {:+.3} @ {:.3} ms | snr {:.1} | 录音峰值 {:.5} rms {:.6} | {} Hz / {} 采样",
            if self.endpoint.is_empty() {
                "<未测>"
            } else {
                self.endpoint.as_str()
            },
            self.direct,
            self.direct_ms,
            if self.inverted {
                "（相对参考信号反相）"
            } else {
                ""
            },
            self.correlation,
            self.delay_ms,
            self.peak_snr,
            self.peak_abs,
            self.rms,
            self.rate,
            self.samples,
        )
    }
}

/// 从 `peaks` 里挑出**直达声**（最早的显著峰）：`(滞后 ms, 带符号值)`。
///
/// 判据 `|值| ≥ POLARITY_DIRECT_FRACTION × 最大|峰|`；没有满足的峰时退回 `fallback`。
fn direct_arrival(peaks: &[(f64, f32)], fallback: (f64, f32)) -> (f64, f32) {
    let max_abs = peaks.iter().map(|(_, v)| v.abs()).fold(0.0f32, f32::max);
    if max_abs <= 0.0 {
        return fallback;
    }
    peaks
        .iter()
        .find(|(_, v)| v.abs() >= POLARITY_DIRECT_FRACTION * max_abs)
        .copied()
        .unwrap_or(fallback)
}

/// 在互相关曲线上按**绝对值**找峰，返回 `(下标, 带符号的峰值)`。
fn signed_peak(curve: &[f32]) -> (usize, f32) {
    let mut best = 0usize;
    let mut best_v = 0.0f32;
    for (i, v) in curve.iter().enumerate() {
        if v.abs() > best_v.abs() {
            best_v = *v;
            best = i;
        }
    }
    (best, best_v)
}

/// 峰值 / 峰外均方根，取 `|峰值|`（负峰也要给出正的 snr）。
fn peak_snr_abs_of(curve: &[f32], idx: usize) -> f32 {
    let peak = curve.get(idx).copied().unwrap_or(0.0).abs();
    if curve.len() <= 5 {
        return if peak > 0.0 { f32::INFINITY } else { 0.0 };
    }
    let mut sum = 0.0f64;
    let mut n = 0usize;
    for (i, v) in curve.iter().enumerate() {
        if i.abs_diff(idx) > 2 {
            sum += (*v as f64) * (*v as f64);
            n += 1;
        }
    }
    if n == 0 {
        return 0.0;
    }
    let rms = (sum / n as f64).sqrt();
    if rms <= 1e-12 {
        if peak > 0.0 {
            f32::INFINITY
        } else {
            0.0
        }
    } else {
        (peak as f64 / rms) as f32
    }
}

/// 带符号的归一化互相关：按 `|峰|` 找滞后，并**保留峰值的符号**。
///
/// [`cross_correlate_detailed`] 取的是最大正值，所以它永远看不到「反相」这件事；极性诊断
/// 必须用这个版本——峰值显著为负就说明录到的波形相对参考信号翻了 180°。
pub fn cross_correlate_signed(reference: &[f32], observed: &[f32]) -> Correlation {
    let curve = ncc_curve(reference, observed);
    if curve.is_empty() {
        return Correlation {
            lag: 0,
            lag_sub: 0.0,
            peak: 0.0,
            peak_snr: 0.0,
            confidence: 0.0,
        };
    }
    let (lag, peak) = signed_peak(&curve);
    let lag_sub = parabolic_peak(&curve, lag);
    let peak_snr = peak_snr_abs_of(&curve, lag);
    Correlation {
        lag,
        lag_sub,
        peak,
        peak_snr,
        confidence: confidence_of(peak.abs(), peak_snr),
    }
}

/// 取互相关曲线上前 `k` 个**互不相邻**（间隔 ≥ `min_sep` 采样）的 `|峰|`，按滞后升序返回。
fn top_peaks(curve: &[f32], k: usize, min_sep: usize) -> Vec<(usize, f32)> {
    if curve.is_empty() || k == 0 {
        return Vec::new();
    }
    let mut order: Vec<usize> = (0..curve.len()).collect();
    order.sort_by(|&a, &b| {
        curve[b]
            .abs()
            .partial_cmp(&curve[a].abs())
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut out: Vec<(usize, f32)> = Vec::with_capacity(k);
    for i in order {
        if out.len() >= k {
            break;
        }
        if out.iter().any(|(j, _)| i.abs_diff(*j) < min_sep) {
            continue;
        }
        out.push((i, curve[i]));
    }
    out.sort_by_key(|(i, _)| *i);
    out
}

/// 互相关曲线上前 `k` 个互不相邻（间隔 ≥ `min_sep` 采样）的 `|峰|`。
///
/// 返回 `(亚采样滞后采样数, 带符号的归一化相关值)`，按滞后升序。
///
/// 这是判断「极性结论可不可信」的关键诊断：如果曲线里存在**两个符号相反、幅度相当**的峰，
/// 说明录音里有两份反相的探测信号（例如两只箱其实都在出声、或存在反相强反射），
/// 那么「最大 `|峰|`」的符号会随微小变化随机翻转，此时任何极性结论都不成立。
pub fn cross_correlate_peaks(
    reference: &[f32],
    observed: &[f32],
    k: usize,
    min_sep: usize,
) -> Vec<(f64, f32)> {
    let curve = ncc_curve(reference, observed);
    if curve.is_empty() {
        return Vec::new();
    }
    top_peaks(&curve, k, min_sep)
        .into_iter()
        .map(|(i, v)| (parabolic_peak(&curve, i), v))
        .collect()
}

/// 单频点幅度（Goertzel，带 Hann 窗）。
///
/// 自实现而不是做 FFT：整段分析只需要十几个频点，Goertzel 更省也更直接。
fn goertzel_magnitude(segment: &[f32], rate: u32, freq: f64) -> f64 {
    let n = segment.len();
    if n < 8 || rate == 0 || !freq.is_finite() || freq <= 0.0 || freq >= rate as f64 * 0.5 {
        return 0.0;
    }
    let w = std::f64::consts::TAU * freq / rate as f64;
    let coeff = 2.0 * w.cos();
    let (mut s1, mut s2) = (0.0f64, 0.0f64);
    for (i, x) in segment.iter().enumerate() {
        let hann = 0.5 - 0.5 * (std::f64::consts::TAU * i as f64 / (n as f64 - 1.0)).cos();
        let s0 = *x as f64 * hann + coeff * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    let power = s1 * s1 + s2 * s2 - coeff * s1 * s2;
    if power <= 0.0 {
        0.0
    } else {
        2.0 * power.sqrt() / n as f64
    }
}

/// 在录音的「`lag` 之后 `t_sec` 秒」处取一个 `win` 采样的窗，算它在 `freq` 上的幅度。
fn window_magnitude(
    samples: &[f32],
    rate: u32,
    lag: f64,
    t_sec: f64,
    win: usize,
    freq: f64,
) -> f64 {
    if samples.is_empty() || win == 0 {
        return 0.0;
    }
    let center = lag + t_sec * rate as f64;
    let start = center - win as f64 / 2.0;
    if !start.is_finite() || start < 0.0 {
        return 0.0;
    }
    let start = start as usize;
    let end = (start + win).min(samples.len());
    if end <= start + 8 {
        return 0.0;
    }
    goertzel_magnitude(&samples[start..end], rate, freq)
}

/// 极性诊断探测信号：[`POLARITY_BAND`] 带内的升频扫频。
pub fn polarity_probe(probe_ms: u32, amplitude: f32, rate: u32) -> Vec<i16> {
    sweep_signal_banded(probe_ms, amplitude, rate, POLARITY_BAND.0, POLARITY_BAND.1)
}

/// 一次「播放 + 录音 + 带符号互相关」的公共流程（单端点）。
#[cfg(windows)]
fn polarity_capture_one(
    mic: &str,
    endpoint: &str,
    cfg: &SyncConfig,
    probe_ms: u32,
) -> Result<(PolarityResult, CapturedAudio)> {
    if mic.trim().is_empty() {
        return Err(AudioError::invalid("麦克风名字/ID 不能为空"));
    }
    if endpoint.trim().is_empty() {
        return Err(AudioError::invalid("被测端点不能为空"));
    }
    let probe_ms = probe_ms.clamp(100, 2_000);
    let probe = polarity_probe(probe_ms, cfg.amplitude, SYNC_RATE);
    if probe.iter().all(|&s| s == 0) {
        return Err(AudioError::invalid(
            "探测信号全零：检查 amplitude 是否被设成了 0",
        ));
    }
    let reference = probe_to_f32(&probe);
    let pcm = interleave_stereo_i16(&probe);
    let format = PcmFormat::new(SYNC_RATE, SYNC_CHANNELS, 16);
    let window_ms = cfg
        .pre_roll_ms
        .saturating_add(probe_ms)
        .saturating_add(cfg.tail_ms);

    let dev = endpoint.to_string();
    let player = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        play_pcm_s16(&selector_for(&dev), format, &pcm)
    });
    let captured = capture_mono(mic, window_ms);
    let played = join_player(player);
    let captured = captured?;
    let played = played.map_err(AudioError::invalid)?;

    let reference = align_reference(reference, captured.rate);
    // 只算一次互相关曲线，`signed_peak` / `peak_snr` / `top_peaks` 全部从它派生：
    // 曲线是 O(n·m)，重复算会让单次测量从 ~29 s 翻倍。
    let curve = ncc_curve(&reference, &captured.samples);
    if curve.is_empty() {
        return Err(AudioError::invalid(
            "录音太短，做不了互相关（麦克风可能没采到数据）",
        ));
    }
    let (lag, peak) = signed_peak(&curve);
    if peak.abs() < 1e-6 {
        return Err(AudioError::invalid(format!(
            "互相关找不到峰：麦克风很可能采不到声音（播放端点 {}；采集 {}）",
            played.device_name,
            captured.describe()
        )));
    }
    let lag_sub = parabolic_peak(&curve, lag);
    let peak_snr = peak_snr_abs_of(&curve, lag);
    let min_sep = (captured.rate as usize / 1_000).max(2);
    let rate = captured.rate.max(1) as f64;
    let peaks: Vec<(f64, f32)> = top_peaks(&curve, 4, min_sep)
        .into_iter()
        .map(|(i, v)| (parabolic_peak(&curve, i) * 1000.0 / rate, v))
        .collect();
    let peak_abs = captured.samples.iter().fold(0.0f32, |m, x| m.max(x.abs()));
    let (direct_ms, direct) = direct_arrival(&peaks, (lag_ms(lag_sub, captured.rate), peak));
    Ok((
        PolarityResult {
            endpoint: endpoint.to_string(),
            delay_ms: lag_ms(lag_sub, captured.rate),
            correlation: peak,
            direct,
            direct_ms,
            inverted: direct <= -POLARITY_MIN_ABS,
            peak_snr,
            samples: captured.samples.len(),
            peak_abs,
            rms: captured.stats.rms as f32,
            rate: captured.rate,
            peaks,
        },
        captured,
    ))
}

/// 「两路同时播同一段信号」那一次采集的结果（内部用）。
#[cfg(windows)]
struct BothCapture {
    result: PolarityResult,
    captured: CapturedAudio,
    curve: Vec<f32>,
    peaks: Vec<(usize, f32)>,
}

/// 两路**同时**播同一段探测信号，从同一段录音里找两个到达峰。
///
/// 两个峰在同一段录音里 → 时间基准相同 → 它们的间隔就是**精确的**两路到达时间差；两个峰的
/// 符号又分别是两路的相对极性。这是「是否有一只箱反相」最直接的证据。
/// 预检一个 render 端点是否存在，避免「跑满一整轮采集才发现右箱没开机」。
///
/// 两路同时播的测量每次要 30 s 以上，而且失败时只会静默录到**一只箱**、给出看起来
/// 合理的错误结论（有人就踩过：右箱掉线时量到的是左箱自己的桌面反射）。
/// 所以在起播放线程之前先把两个端点都解析一遍，缺哪个立刻报哪个。
#[cfg(windows)]
fn ensure_render_endpoint(name: &str, role: &str) -> Result<()> {
    let _guard = ComGuard::new()?;
    let en = com::enumerator()?;
    com::resolve(&en, &selector_for(name), DataFlow::Render)
        .map(|_| ())
        .map_err(|e| {
            AudioError::DeviceNotFound(format!(
                "{role}端点「{name}」不可用（{e}）——设备是否已开机/重连？"
            ))
        })
}

/// 预检两路 render 端点。
#[cfg(windows)]
fn ensure_pair_endpoints(left: &str, right: &str) -> Result<()> {
    ensure_render_endpoint(left, "左路")?;
    ensure_render_endpoint(right, "右路")
}

#[cfg(windows)]
fn polarity_capture_both(
    mic: &str,
    left: &str,
    right: &str,
    cfg: &SyncConfig,
    probe_ms: u32,
) -> Result<BothCapture> {
    if mic.trim().is_empty() {
        return Err(AudioError::invalid("麦克风名字/ID 不能为空"));
    }
    if left.trim().is_empty() || right.trim().is_empty() {
        return Err(AudioError::invalid("左右端点都不能为空"));
    }
    ensure_pair_endpoints(left, right)?;
    let probe_ms = probe_ms.clamp(100, 2_000);
    let probe = polarity_probe(probe_ms, cfg.amplitude, SYNC_RATE);
    if probe.iter().all(|&s| s == 0) {
        return Err(AudioError::invalid(
            "探测信号全零：检查 amplitude 是否被设成了 0",
        ));
    }
    let reference = probe_to_f32(&probe);
    let pcm = interleave_stereo_i16(&probe);
    let format = PcmFormat::new(SYNC_RATE, SYNC_CHANNELS, 16);
    let window_ms = cfg
        .pre_roll_ms
        .saturating_add(probe_ms)
        .saturating_add(cfg.tail_ms);

    let gate = std::sync::Arc::new(std::sync::Barrier::new(3));
    let (gate_l, gate_r) = (gate.clone(), gate.clone());
    let (dev_l, dev_r) = (left.to_string(), right.to_string());
    let pcm_r = pcm.clone();
    let handle_l = std::thread::spawn(move || {
        gate_l.wait();
        std::thread::sleep(Duration::from_millis(50));
        play_pcm_s16(&selector_for(&dev_l), format, &pcm)
    });
    let handle_r = std::thread::spawn(move || {
        gate_r.wait();
        std::thread::sleep(Duration::from_millis(50));
        play_pcm_s16(&selector_for(&dev_r), format, &pcm_r)
    });
    gate.wait();

    let captured = capture_mono(mic, window_ms);
    let played_l = join_player(handle_l);
    let played_r = join_player(handle_r);
    let captured = captured?;
    let played_l = played_l.map_err(AudioError::invalid)?;
    let _ = played_r;

    let reference = align_reference(reference, captured.rate);
    let curve = ncc_curve(&reference, &captured.samples);
    if curve.is_empty() {
        return Err(AudioError::invalid(format!(
            "互相关曲线为空（播放端点 {}；采集 {}）",
            played_l.device_name,
            captured.describe()
        )));
    }
    let (lag, peak) = signed_peak(&curve);
    if peak.abs() < 1e-6 {
        return Err(AudioError::invalid(format!(
            "互相关找不到峰：麦克风很可能采不到声音（播放端点 {}；采集 {}）",
            played_l.device_name,
            captured.describe()
        )));
    }
    let lag_sub = parabolic_peak(&curve, lag);
    let peak_snr = peak_snr_abs_of(&curve, lag);
    let min_sep = (captured.rate as usize / 1_000).max(2);
    let peaks = top_peaks(&curve, 2, min_sep);
    let rate_f = captured.rate.max(1) as f64;
    let result_peaks: Vec<(f64, f32)> = top_peaks(&curve, 4, min_sep)
        .into_iter()
        .map(|(i, v)| (parabolic_peak(&curve, i) * 1000.0 / rate_f, v))
        .collect();
    let peak_abs = captured.samples.iter().fold(0.0f32, |m, x| m.max(x.abs()));
    let (direct_ms, direct) = direct_arrival(&result_peaks, (lag_ms(lag_sub, captured.rate), peak));
    Ok(BothCapture {
        result: PolarityResult {
            endpoint: format!("{left} + {right}"),
            delay_ms: lag_ms(lag_sub, captured.rate),
            correlation: peak,
            direct,
            direct_ms,
            inverted: direct <= -POLARITY_MIN_ABS,
            peak_snr,
            samples: captured.samples.len(),
            peak_abs,
            rms: captured.stats.rms as f32,
            rate: captured.rate,
            peaks: result_peaks,
        },
        captured,
        curve,
        peaks,
    })
}

/// 测**一个**端点的相对极性（会从这只箱放出一段 300~4000 Hz 的扫频）。
///
/// 返回的 `correlation` 是**带符号**的：把它和另一个端点在同一次会话里的结果比，符号相反
/// 就是两只箱反相。`delay_ms` 只在这两次测量之间**不可比**（两次是独立录音，起始相位不同），
/// 要拿时间差请用 [`measure_comb`]（它把两路放在同一段录音里测）。
///
/// 麦克风采不到声音时返回 `Err`——GUI 应当先用 [`mic_health`] 拦一道。
#[cfg(windows)]
pub fn measure_polarity(mic: &str, endpoint: &str, cfg: &SyncConfig) -> Result<PolarityResult> {
    let probe_ms = cfg.probe_ms.clamp(POLARITY_PROBE_MS, 2_000);
    Ok(polarity_capture_one(mic, endpoint, cfg, probe_ms)?.0)
}

/// 依次测左右两个端点的相对极性，返回 `(左, 右)`。
///
/// 两次用的是**同一段参考信号**，所以 `correlation` 的符号可比：符号相反 → 有一只箱反相。
/// 反过来，两个 `delay_ms` **不可比**（独立录音、起始相位不同），不要拿它们算时间差。
#[cfg(windows)]
pub fn measure_pair_polarity(
    mic: &str,
    left: &str,
    right: &str,
    cfg: &SyncConfig,
) -> Result<(PolarityResult, PolarityResult)> {
    let probe_ms = cfg.probe_ms.clamp(POLARITY_PROBE_MS, 2_000);
    let l = polarity_capture_one(mic, left, cfg, probe_ms)?.0;
    let r = polarity_capture_one(mic, right, cfg, probe_ms)?.0;
    Ok((l, r))
}

/// 梳状曲线上的一个频点。
#[derive(Debug, Clone, Copy, Default)]
pub struct CombPoint {
    /// 分析频率（Hz）。
    pub freq_hz: f64,
    /// 只有左箱在响时该频点的幅度。
    pub left: f64,
    /// 只有右箱在响时该频点的幅度。
    pub right: f64,
    /// 两箱同时响时该频点的幅度。
    pub both: f64,
    /// `both / (left + right)`：1.0 = 完全相加，0.0 = 完全抵消。
    pub ratio: f64,
}

/// 极性 / 梳状诊断的结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CombVerdict {
    /// 两路同相、到达也基本一致：没有可闻抵消。
    Aligned,
    /// 有一路极性相反——居中人声被抵消。
    Inverted,
    /// 同相但到达时间差明显 → 梳状滤波（等间距深谷）。
    CombFiltered,
    /// 既反相又有明显时间差。
    InvertedAndComb,
}

impl CombVerdict {
    /// 短标签。
    pub fn as_str(self) -> &'static str {
        match self {
            CombVerdict::Aligned => "aligned",
            CombVerdict::Inverted => "inverted",
            CombVerdict::CombFiltered => "comb",
            CombVerdict::InvertedAndComb => "inverted+comb",
        }
    }

    /// 给用户看的一句话。
    pub fn describe(self) -> &'static str {
        match self {
            CombVerdict::Aligned => "两路同相且到达一致：没有发现极性抵消或梳状滤波",
            CombVerdict::Inverted => {
                "有一只箱极性相反：居中的人声会被抵消（把这只箱的线序/相位翻转即可）"
            }
            CombVerdict::CombFiltered => "两路到达时间差明显：存在梳状滤波，居中内容被等间距挖空",
            CombVerdict::InvertedAndComb => "既有一只箱反相、又有明显到达时间差：两个问题都要修",
        }
    }
}

/// 极性与梳状滤波的完整诊断结果。
#[derive(Debug, Clone)]
pub struct CombResult {
    /// 左路单独测量。
    pub left: PolarityResult,
    /// 右路单独测量。
    pub right: PolarityResult,
    /// 两路同时播时录音的时域峰值。
    pub both_peak_abs: f32,
    /// 两路同时播时录音的 RMS。
    pub both_rms: f32,
    /// `both_peak_abs / (left.peak_abs + right.peak_abs)`：明显 <1 = 有抵消。
    pub peak_ratio: f32,
    /// 能量口径的同一件事：`both_rms² / (left_rms² + right_rms²)`。
    pub energy_ratio: f32,
    /// 两路到达时间差（ms），从**同一段录音**的两个相关峰得到。
    pub offset_ms: f64,
    /// 「两路同时播」那次录音里互相关曲线的前 4 个峰 `(滞后 ms, 带符号相关)`。
    ///
    /// 这是判断两路到达时间差最直接的证据：两路真的在时间上重叠时会看到相距几毫秒的两个峰；
    /// 相距几百毫秒则说明两路根本没有重叠——听到的是**回声**而不是梳状滤波。
    pub both_peaks: Vec<(f64, f32)>,
    /// 逐频点的梳状曲线。
    pub curve: Vec<CombPoint>,
    /// 有效频点里最小的 `ratio`（谷底）。
    pub min_ratio: f64,
    /// 有效频点里最大的 `ratio`（峰顶）。
    pub max_ratio: f64,
    /// 有效频点的 `ratio` 均值。
    pub mean_ratio: f64,
    /// 谷底所在的频率（Hz）。
    pub worst_freq_hz: f64,
    /// `-20·log10(min_ratio)`，即最深的抵消有多少 dB。
    pub comb_depth_db: f64,
    /// 两次单箱测量的 `correlation` 符号相反且都够大。
    pub polarity_opposed: bool,
    /// 同相假设下模型的加权残差（越小越像）。
    pub fit_inphase: f64,
    /// 反相假设下模型的加权残差（越小越像）。
    pub fit_antiphase: f64,
    /// 结论。
    pub verdict: CombVerdict,
}

impl CombResult {
    /// 人读多行。
    pub fn describe(&self) -> String {
        let mut s = String::new();
        s.push_str(&format!("左路：{}\n", self.left.describe()));
        s.push_str(&format!("右路：{}\n", self.right.describe()));
        s.push_str(&format!(
            "两路同时播：峰值 {:.5} rms {:.6} | 合成峰值比 {:.3} | 能量比 {:.3}\n",
            self.both_peak_abs, self.both_rms, self.peak_ratio, self.energy_ratio
        ));
        s.push_str(&format!("两路到达时间差：{:.3} ms\n", self.offset_ms));
        s.push_str(&format!(
            "梳状曲线：{} 点，谷底 {:.3} @ {:.0} Hz（{:.1} dB），峰顶 {:.3}，均值 {:.3}\n",
            self.curve.len(),
            self.min_ratio,
            self.worst_freq_hz,
            self.comb_depth_db,
            self.max_ratio,
            self.mean_ratio
        ));
        s.push_str(&format!(
            "模型残差：同相 {:.4} / 反相 {:.4} → {}\n",
            self.fit_inphase,
            self.fit_antiphase,
            if self.fit_antiphase < self.fit_inphase {
                "更像反相"
            } else {
                "更像同相"
            }
        ));
        s.push_str(&format!(
            "结论 [{}]：{}",
            self.verdict.as_str(),
            self.verdict.describe()
        ));
        s
    }
}

/// 由三段录音（左单独 / 右单独 / 两路同时）算逐频点梳状曲线。
///
/// 纯函数，不碰设备，所以可以离线单测。`lag_*` 是各自录音里探测信号的到达位置（采样点）。
#[allow(clippy::too_many_arguments)]
fn comb_curve_from(
    cap_l: &[f32],
    rate_l: u32,
    lag_l: f64,
    cap_r: &[f32],
    rate_r: u32,
    lag_r: f64,
    cap_b: &[f32],
    rate_b: u32,
    lag_b: f64,
    probe_ms: u32,
) -> Vec<CombPoint> {
    let total_s = probe_ms as f64 / 1000.0;
    let (f_lo, f_hi) = POLARITY_BAND;
    let span = (f_hi - f_lo).max(1.0);
    let a0 = f_lo + 0.1 * span;
    let a1 = f_hi - 0.1 * span;
    let win_l = (COMB_WINDOW_MS * rate_l.max(1) as f64 / 1000.0).round() as usize;
    let win_r = (COMB_WINDOW_MS * rate_r.max(1) as f64 / 1000.0).round() as usize;
    let win_b = (COMB_WINDOW_MS * rate_b.max(1) as f64 / 1000.0).round() as usize;

    let mut curve: Vec<CombPoint> = Vec::with_capacity(COMB_POINTS);
    for i in 0..COMB_POINTS {
        let frac = (i as f64 + 0.5) / COMB_POINTS as f64;
        let f = a0 * (a1 / a0).powf(frac);
        // 升频线性调频：瞬时频率 f(t) = f0 + (f1-f0)·t/T
        let t = total_s * (f - f_lo) / span;
        let y_l = window_magnitude(cap_l, rate_l, lag_l, t, win_l, f);
        let y_r = window_magnitude(cap_r, rate_r, lag_r, t, win_r, f);
        let y_b = window_magnitude(cap_b, rate_b, lag_b, t, win_b, f);
        let sum = y_l + y_r;
        let ratio = if sum > 1e-9 { y_b / sum } else { 1.0 };
        curve.push(CombPoint {
            freq_hz: f,
            left: y_l,
            right: y_r,
            both: y_b,
            ratio,
        });
    }
    curve
}

/// 极性 + 梳状滤波的完整声学诊断（**会真实放出三段测试音**）。
///
/// 做三次采集：左单独、右单独、两路同时播同一段扫频。然后
///
/// * 用「同时播」那一次录音的两个相关峰得到**精确的**两路到达时间差与各自的相对极性；
/// * 用三次录音在各频点的幅度算出 `ratio(f) = |Y_both| / (|Y_left| + |Y_right|)`：
///   谷底远小于 1 就是抵消，谷底与峰顶交替出现就是梳状滤波；
/// * 用 `|a_L ± a_R·e^{j2πfΔ}| / (a_L + a_R)` 两个假设做加权最小二乘，区分「反相」和
///   「大偏移」——两者都会让合成变小，但频域形状完全不同。
///
/// 探测信号长度用 `cfg.probe_ms` 夹到 600~2000 ms：太短的扫频速率太高，分析窗内干涉相位
/// 会转过去，梳状曲线就糊了。
#[cfg(windows)]
pub fn measure_comb(mic: &str, left: &str, right: &str, cfg: &SyncConfig) -> Result<CombResult> {
    let probe_ms = cfg.probe_ms.clamp(600, 2_000);
    let (pl, cap_l) = polarity_capture_one(mic, left, cfg, probe_ms)?;
    let (pr, cap_r) = polarity_capture_one(mic, right, cfg, probe_ms)?;
    let both = polarity_capture_both(mic, left, right, cfg, probe_ms)?;

    // 到达时间差：同一段录音里前两个互不相邻的 |峰|
    let offset_ms = if both.peaks.len() >= 2 {
        let rate = both.captured.rate.max(1) as f64;
        let a = parabolic_peak(&both.curve, both.peaks[0].0);
        let b = parabolic_peak(&both.curve, both.peaks[1].0);
        (a - b).abs() * 1000.0 / rate
    } else {
        0.0
    };

    // 梳状曲线
    let lag_l = pl.delay_ms * cap_l.rate.max(1) as f64 / 1000.0;
    let lag_r = pr.delay_ms * cap_r.rate.max(1) as f64 / 1000.0;
    let lag_b = both.result.delay_ms * both.captured.rate.max(1) as f64 / 1000.0;
    let curve = comb_curve_from(
        &cap_l.samples,
        cap_l.rate,
        lag_l,
        &cap_r.samples,
        cap_r.rate,
        lag_r,
        &both.captured.samples,
        both.captured.rate,
        lag_b,
        probe_ms,
    );

    // 只在信号够强的频点上统计（弱频点的比值全是噪声）
    let peak_sum = curve
        .iter()
        .map(|p| p.left + p.right)
        .fold(0.0f64, f64::max);
    let floor = peak_sum * 0.15;
    let mut min_ratio = f64::INFINITY;
    let mut max_ratio = 0.0f64;
    let mut sum_ratio = 0.0f64;
    let mut n_ratio = 0usize;
    let mut worst_freq_hz = 0.0f64;
    for p in &curve {
        if p.left + p.right < floor {
            continue;
        }
        if p.ratio < min_ratio {
            min_ratio = p.ratio;
            worst_freq_hz = p.freq_hz;
        }
        if p.ratio > max_ratio {
            max_ratio = p.ratio;
        }
        sum_ratio += p.ratio;
        n_ratio += 1;
    }
    if n_ratio == 0 {
        min_ratio = 1.0;
        max_ratio = 1.0;
        worst_freq_hz = 0.0;
    }
    let mean_ratio = if n_ratio == 0 {
        1.0
    } else {
        sum_ratio / n_ratio as f64
    };
    let comb_depth_db = if min_ratio > 0.0 {
        -20.0 * min_ratio.log10()
    } else {
        60.0
    };

    // 模型拟合：区分「反相」与「大偏移」
    let d_sec = offset_ms / 1000.0;
    let mut fit_in = 0.0f64;
    let mut fit_anti = 0.0f64;
    let mut wsum = 0.0f64;
    for p in &curve {
        let sum = p.left + p.right;
        if sum <= 1e-9 {
            continue;
        }
        let th = std::f64::consts::TAU * p.freq_hz * d_sec;
        let (c, s) = (th.cos(), th.sin());
        let m_in = ((p.left + p.right * c).powi(2) + (p.right * s).powi(2)).sqrt() / sum;
        let m_anti = ((p.left - p.right * c).powi(2) + (p.right * s).powi(2)).sqrt() / sum;
        let w = sum * sum;
        fit_in += w * (p.ratio - m_in).powi(2);
        fit_anti += w * (p.ratio - m_anti).powi(2);
        wsum += w;
    }
    let (fit_inphase, fit_antiphase) = if wsum > 0.0 {
        (fit_in / wsum, fit_anti / wsum)
    } else {
        (f64::NAN, f64::NAN)
    };

    // 极性用**直达声**（`direct`）而不是最大 |峰|：强反射环境里最大 |峰| 可能是反相反射。
    let polarity_opposed = pl.direct * pr.direct < 0.0
        && pl.direct.abs() >= POLARITY_MIN_ABS
        && pr.direct.abs() >= POLARITY_MIN_ABS;

    let notch = min_ratio < COMB_NOTCH_RATIO;
    let has_peak = max_ratio > COMB_NOTCH_RATIO;
    let broad = max_ratio < COMB_NOTCH_RATIO;
    let verdict = match (polarity_opposed, notch && has_peak, broad) {
        (true, true, _) => CombVerdict::InvertedAndComb,
        (true, _, _) => CombVerdict::Inverted,
        (false, _, true) => CombVerdict::Inverted,
        (false, true, false) => CombVerdict::CombFiltered,
        _ => CombVerdict::Aligned,
    };

    let denom_peak = pl.peak_abs + pr.peak_abs;
    let peak_ratio = if denom_peak > 0.0 {
        both.result.peak_abs / denom_peak
    } else {
        0.0
    };
    let denom_energy = pl.rms * pl.rms + pr.rms * pr.rms;
    let energy_ratio = if denom_energy > 0.0 {
        both.result.rms * both.result.rms / denom_energy
    } else {
        0.0
    };

    Ok(CombResult {
        left: pl,
        right: pr,
        both_peak_abs: both.result.peak_abs,
        both_rms: both.result.rms,
        peak_ratio,
        energy_ratio,
        offset_ms,
        both_peaks: both.result.peaks.clone(),
        curve,
        min_ratio,
        max_ratio,
        mean_ratio,
        worst_freq_hz,
        comb_depth_db,
        polarity_opposed,
        fit_inphase,
        fit_antiphase,
        verdict,
    })
}

/// 生成一段单频正弦（单声道 i16），两端各带 10 ms 淡入淡出避免爆音。
///
/// 对齐扫描必须用**窄带**信号：宽带扫频的两份拷贝只要错开超过一个相关宽度（本模块的
/// 600 ms 扫频约 0.27 ms）就不重叠，合成能量与延迟无关，扫不出峰。单频正弦的合成幅度是
/// `|1 + e^{j2πfΔ}|`，随 `Δ` 以 `1/f` 为周期在 0 与 2 倍之间摆动，才能定位对齐点。
pub fn tone_signal(hz: f64, ms: u32, amplitude: f32, rate: u32) -> Vec<i16> {
    let rate = rate.max(8_000);
    let n = ((ms.clamp(1, 10_000) as u64) * rate as u64 / 1_000) as usize;
    let fade = (rate as usize / 100).max(1); // 10 ms
    let amp = (amplitude.clamp(0.0, 1.0) as f64) * i16::MAX as f64;
    (0..n)
        .map(|i| {
            let t = i as f64 / rate as f64;
            let env = if i < fade {
                i as f64 / fade as f64
            } else if n.saturating_sub(i) < fade {
                n.saturating_sub(i) as f64 / fade as f64
            } else {
                1.0
            };
            let v = (std::f64::consts::TAU * hz * t).sin() * amp * env;
            v.clamp(i16::MIN as f64, i16::MAX as f64) as i16
        })
        .collect()
}

/// 在录音里滑窗，返回幅度最大的那个窗的 `(Goertzel 幅度, 窗起点采样)`。
///
/// 窗长取 100 ms：够长到 Goertzel 的分辨率足以把 1 kHz 单音从邻频噪声里挑出来，够短到
/// 不会同时罩住两份拷贝（两路相差几毫秒到几十毫秒时都成立）。
fn best_tone_window(samples: &[f32], rate: u32, freq: f64, win: usize) -> (f64, usize) {
    if win < 8 || samples.len() < win {
        return (goertzel_magnitude(samples, rate, freq), 0);
    }
    let step = (win / 4).max(1);
    let mut best = (0.0f64, 0usize);
    let mut start = 0usize;
    while start + win <= samples.len() {
        let m = goertzel_magnitude(&samples[start..start + win], rate, freq);
        if m > best.0 {
            best = (m, start);
        }
        start += step;
    }
    best
}

/// 一个「相对延迟 → 合成幅度」的扫描点。
#[derive(Debug, Clone, Copy, Default)]
pub struct AlignPoint {
    /// 左路相对右路多垫的静音（ms）。负值 = 右路垫得更多。
    pub delay_ms: f64,
    /// 整段录音的 RMS（0~1）。
    pub rms: f32,
    /// 整段录音的时域峰值（0~1）。
    pub peak_abs: f32,
    /// 探测单音在最响那 100 ms 窗里的幅度（Goertzel，任意单位，只用于横向比较）。
    pub tone: f64,
    /// 那个窗的起点（ms，相对录音开头）。
    pub tone_at_ms: f64,
}

/// 相对延迟扫描的结果。
#[derive(Debug, Clone)]
pub struct AlignScan {
    /// 探测单音频率（Hz）。
    pub tone_hz: f64,
    /// 扫描点，按 `delay_ms` 升序。
    pub points: Vec<AlignPoint>,
    /// `tone` 最大的点 = 两路在麦克风处**叠加最强**的相对延迟。
    pub best: AlignPoint,
    /// `tone` 最小的点 = 抵消最深的相对延迟。
    pub worst: AlignPoint,
    /// `20·log10(best.tone / worst.tone)`：最佳与最差差多少 dB。
    pub span_db: f64,
}

impl AlignScan {
    /// 人读一行。
    pub fn describe(&self) -> String {
        format!(
            "对齐扫描 {:.0} Hz：最佳 {:.1} ms（幅度 {:.6}）↔ 最差 {:.1} ms（幅度 {:.6}）| 落差 {:.1} dB | {} 个点",
            self.tone_hz,
            self.best.delay_ms,
            self.best.tone,
            self.worst.delay_ms,
            self.worst.tone,
            self.span_db,
            self.points.len(),
        )
    }
}

/// 同时播两路之后的一次录音，附带两路各自的播放结果文本。
///
/// 带上播放结果是为了让「只录到一只箱」这种情况能被看见，而不是静默地把结果当真理。
#[cfg(windows)]
struct PairCapture {
    captured: CapturedAudio,
    left_note: String,
    right_note: String,
    /// 左路播放线程实际发起 `play_pcm_s16` 的时刻，相对 Barrier 放行时刻的毫秒数。
    left_start_ms: i128,
    /// 右路同上。
    right_start_ms: i128,
    /// 左路播放线程从发起 `play_pcm_s16` 到它返回（缓冲区放空）的结束时刻，同上是相对 Barrier 放行的毫秒数。
    left_end_ms: i128,
    /// 右路同上。
    right_end_ms: i128,
}

/// 同时往两个端点播同一段单声道 PCM（各自预垫不同静音），并录一次音。
///
/// 用 `Barrier` 保证两个播放线程尽量同时起播——起播相位不受控是本机已知的最大误差源，
/// 但对「相对延迟扫描」没影响：要测的就是两路之间的相对关系。
#[cfg(windows)]
fn play_pair_capture(
    mic: &str,
    left_endpoint: &str,
    right_endpoint: &str,
    mono: &[i16],
    left_delay_ms: i32,
    right_delay_ms: i32,
    capture_ms: u32,
) -> Result<PairCapture> {
    ensure_pair_endpoints(left_endpoint, right_endpoint)?;
    let pcm = interleave_stereo_i16(mono);
    let left = apply_delay(&pcm, left_delay_ms)?;
    let right = apply_delay(&pcm, right_delay_ms)?;
    let format = PcmFormat::new(SYNC_RATE, SYNC_CHANNELS, 16);
    let gate = std::sync::Arc::new(std::sync::Barrier::new(3));
    let t0 = std::time::Instant::now();
    let mut handles = Vec::with_capacity(2);
    for (dev, data) in [
        (left_endpoint.to_string(), left),
        (right_endpoint.to_string(), right),
    ] {
        let g = gate.clone();
        handles.push(std::thread::spawn(move || {
            g.wait();
            std::thread::sleep(Duration::from_millis(50));
            let started = t0.elapsed().as_millis() as i128;
            let res = play_pcm_s16(&selector_for(&dev), format, &data);
            let ended = t0.elapsed().as_millis() as i128;
            (started, ended, res)
        }));
    }
    gate.wait();
    let captured = capture_mono(mic, capture_ms);
    let mut notes = Vec::with_capacity(2);
    let mut starts = Vec::with_capacity(2);
    let mut ends = Vec::with_capacity(2);
    for h in handles {
        match h.join() {
            Ok((started, ended, res)) => {
                starts.push(started);
                ends.push(ended);
                notes.push(match res {
                    Ok(info) => info.describe(),
                    Err(e) => format!("**这一路播放失败**：{e}"),
                });
            }
            Err(_) => {
                starts.push(-1);
                ends.push(-1);
                notes.push("**播放线程 panic**".to_string());
            }
        }
    }
    let mut it = notes.into_iter();
    let mut its = starts.into_iter();
    let mut ite = ends.into_iter();
    Ok(PairCapture {
        captured: captured?,
        left_note: it.next().unwrap_or_default(),
        right_note: it.next().unwrap_or_default(),
        left_start_ms: its.next().unwrap_or(-1),
        right_start_ms: its.next().unwrap_or(-1),
        left_end_ms: ite.next().unwrap_or(-1),
        right_end_ms: ite.next().unwrap_or(-1),
    })
}

/// 对齐扫描：左右**同时**播同一段单频正弦，左路相对右路预垫 `delay_ms`，测麦克风处的合成幅度。
///
/// 这是「当前配置的延迟值对不对」的直接检验：合成幅度最大的那个 `delay_ms` 才是真正的对齐点。
/// 用它对比配置里的值，两者相差多少就是当前设置损失了多少 dB。
///
/// * `tone_hz`：探测频率，夹在 50~15000 Hz。**不要用宽带扫频**（见 [`tone_signal`] 的说明）；
/// * `from_ms` / `to_ms` / `step_ms`：扫描范围与步进，点数上限 400。
///
/// 注意：这只在**两路确实在时间上重叠**时才有意义。若两路的绝对延迟差远大于扫描范围
/// （先用 [`measure_comb`] 的 `both_peaks` 确认），整个扫描会是一条平线。
#[cfg(windows)]
#[allow(clippy::too_many_arguments)]
pub fn scan_alignment(
    mic: &str,
    left_endpoint: &str,
    right_endpoint: &str,
    cfg: &SyncConfig,
    tone_hz: f64,
    from_ms: f64,
    to_ms: f64,
    step_ms: f64,
) -> Result<AlignScan> {
    if !tone_hz.is_finite() || !(50.0..=15_000.0).contains(&tone_hz) {
        return Err(AudioError::invalid(format!(
            "探测单音频率非法：{tone_hz}（要在 50~15000 Hz）"
        )));
    }
    if !step_ms.is_finite() || step_ms <= 0.0 {
        return Err(AudioError::invalid(format!("扫描步进非法：{step_ms}")));
    }
    if !from_ms.is_finite() || !to_ms.is_finite() {
        return Err(AudioError::invalid("扫描范围必须是有限数"));
    }
    let (lo, hi) = if from_ms <= to_ms {
        (from_ms, to_ms)
    } else {
        (to_ms, from_ms)
    };
    let steps = ((hi - lo) / step_ms).round() as usize + 1;
    if steps > 400 {
        return Err(AudioError::invalid(format!(
            "扫描点太多（{steps} 个，上限 400）：把范围收窄或步进调大"
        )));
    }

    let probe_ms = cfg.probe_ms.clamp(200, 2_000);
    let mono = tone_signal(tone_hz, probe_ms, cfg.amplitude, SYNC_RATE);
    let capture_ms = cfg
        .pre_roll_ms
        .saturating_add(probe_ms)
        .saturating_add(cfg.tail_ms);
    let win = (SYNC_RATE as usize / 10).max(8); // 100 ms

    let mut points = Vec::with_capacity(steps);
    for i in 0..steps {
        let d = lo + step_ms * i as f64;
        let (ld, rd) = if d >= 0.0 {
            (d.round() as i32, 0)
        } else {
            (0, (-d).round() as i32)
        };
        let cap = play_pair_capture(
            mic,
            left_endpoint,
            right_endpoint,
            &mono,
            ld,
            rd,
            capture_ms,
        )?
        .captured;
        let rate = cap.rate.max(1);
        let (tone, at) = best_tone_window(&cap.samples, rate, tone_hz, win);
        points.push(AlignPoint {
            delay_ms: d,
            rms: cap.stats.rms as f32,
            peak_abs: cap.samples.iter().fold(0.0f32, |m, x| m.max(x.abs())),
            tone,
            tone_at_ms: at as f64 * 1000.0 / rate as f64,
        });
    }

    let by_tone = |a: &AlignPoint, b: &AlignPoint| {
        a.tone
            .partial_cmp(&b.tone)
            .unwrap_or(std::cmp::Ordering::Equal)
    };
    let best = points.iter().copied().max_by(by_tone).unwrap_or_default();
    let worst = points.iter().copied().min_by(by_tone).unwrap_or_default();
    let span_db = if worst.tone > 0.0 && best.tone > 0.0 {
        20.0 * (best.tone / worst.tone).log10()
    } else {
        0.0
    };
    Ok(AlignScan {
        tone_hz,
        points,
        best,
        worst,
        span_db,
    })
}

/// 两路同时播（各自可预垫延迟）之后，麦克风处录到的对齐检查结果。
///
/// 这是判断「配置里的延迟值对不对」的**权威口径**：同一段录音里两路拷贝的相关峰相隔
/// [`AlignmentCheck::gap_ms`]，接近 0 才算对齐。宽带扫频的对齐峰宽约 `1/带宽`（本机
/// 300–4000 Hz 扫频约 0.27 ms），所以相关峰的位置能给出亚毫秒精度的到达差。
///
/// 注意：单频正弦**测不出**绝对对齐——合成幅度 `|1+e^{j2πfΔ}|` 以 `1/f` 为周期摆动，
/// 所有 Δ = k/f 的点都一样高，所以 `--align-scan` 的单音表只能用来验证「某个候选延迟
/// 附近有没有峰」，不能用来找绝对零点。找零点要用这里的互相关口径。
#[derive(Debug, Clone, Default)]
pub struct AlignmentCheck {
    /// 左路预垫的延迟（ms）。
    pub apply_left_ms: i32,
    /// 右路预垫的延迟（ms）。
    pub apply_right_ms: i32,
    /// 互相关曲线上的前几个峰（滞后 ms / 带符号值），按滞后升序。
    pub peaks: Vec<(f64, f32)>,
    /// 前两个峰之间的间隔（ms）；只有一个峰时为 0。
    pub gap_ms: f64,
    /// 是否已经对齐（间隔 ≤ [`ALIGN_MERGE_MS`]）。
    pub merged: bool,
    /// 录音 RMS。
    pub rms: f32,
    /// 录音峰值绝对值。
    pub peak_abs: f32,
    /// 左路播放结果（失败时是错误文本）——用来发现「只录到一只箱」。
    pub left_playback: String,
    /// 右路播放结果（失败时是错误文本）。
    pub right_playback: String,
    /// 左路线程实际发起播放的时刻（相对 Barrier 放行，ms）——用来发现两路没有同时起播。
    pub left_start_ms: i128,
    /// 右路线程实际发起播放的时刻（相对 Barrier 放行，ms）。
    pub right_start_ms: i128,
    /// 左路线程从发起播放到 `play_pcm_s16` 返回（缓冲区放空）的结束时刻（相对 Barrier 放行，ms）。
    ///
    /// `end - start` 就是这一路「建流 + 播放 + 排空」占用的墙钟时间；两路的 `start` 相差大
    /// 或某一路的 `end - start` 接近另一路的两倍，说明两路没有真正并发起播，测量不可信。
    pub left_end_ms: i128,
    /// 右路同上。
    pub right_end_ms: i128,
}

/// 两路拷贝「算对齐」的判定门限（ms）。扫频的对齐峰宽约 0.27 ms，取 2 ms 留足余量。
pub const ALIGN_MERGE_MS: f64 = 2.0;

impl AlignmentCheck {
    /// 一行摘要。
    pub fn describe(&self) -> String {
        let peaks: Vec<String> = self
            .peaks
            .iter()
            .map(|(ms, v)| format!("{ms:.3}ms/{v:+.3}"))
            .collect();
        format!(
            "左垫 {} ms / 右垫 {} ms | 峰 {} | 间隔 {:.3} ms → {} | rms {:.6} 峰值 {:.5}",
            self.apply_left_ms,
            self.apply_right_ms,
            if peaks.is_empty() {
                "无".to_string()
            } else {
                peaks.join(" ")
            },
            self.gap_ms,
            if self.merged {
                "**已对齐**"
            } else {
                "**没对齐**"
            },
            self.rms,
            self.peak_abs
        )
    }
}

/// 两路同时播同一段探测信号（各自预垫给定延迟），录一次，量两路拷贝的相关峰间隔。
///
/// 用法：先用 0/0 量出真实到达差，再把这个差填进 `apply_left_ms` 复测，间隔应当塌到 0
/// 附近、RMS 也会因两路相干叠加而抬高（相干时 +3 dB，不相干时只 +0 dB 左右）。
///
/// * `apply_left_ms` / `apply_right_ms`：各自预垫的静音长度（必须 ≥ 0）；
/// * 探测信号用 `cfg.probe`，时长 `cfg.probe_ms` 夹到 200 ~ 2000 ms。
#[cfg(windows)]
pub fn check_alignment(
    mic: &str,
    left_endpoint: &str,
    right_endpoint: &str,
    cfg: &SyncConfig,
    apply_left_ms: i32,
    apply_right_ms: i32,
) -> Result<AlignmentCheck> {
    if apply_left_ms < 0 || apply_right_ms < 0 {
        return Err(AudioError::invalid(format!(
            "预垫延迟不能为负：左 {apply_left_ms} / 右 {apply_right_ms}"
        )));
    }
    let probe_ms = cfg.probe_ms.clamp(200, 2_000);
    let mono = probe_signal_at(cfg.probe, probe_ms, cfg.amplitude, SYNC_RATE);
    let capture_ms = cfg
        .pre_roll_ms
        .saturating_add(probe_ms)
        .saturating_add(cfg.tail_ms);
    let pair = play_pair_capture(
        mic,
        left_endpoint,
        right_endpoint,
        &mono,
        apply_left_ms,
        apply_right_ms,
        capture_ms,
    )?;
    let captured = pair.captured;
    let reference = align_reference(probe_to_f32(&mono), captured.rate);
    let curve = ncc_curve(&reference, &captured.samples);
    let min_sep = (captured.rate as usize / 1_000).max(2);
    let rate_f = captured.rate.max(1) as f64;
    let peaks: Vec<(f64, f32)> = top_peaks(&curve, 6, min_sep)
        .into_iter()
        .map(|(i, v)| (parabolic_peak(&curve, i) * 1000.0 / rate_f, v))
        .collect();
    let gap_ms = if peaks.len() >= 2 {
        (peaks[1].0 - peaks[0].0).abs()
    } else {
        0.0
    };
    Ok(AlignmentCheck {
        apply_left_ms,
        apply_right_ms,
        peaks,
        gap_ms,
        merged: gap_ms <= ALIGN_MERGE_MS,
        rms: captured.stats.rms as f32,
        peak_abs: captured.samples.iter().fold(0.0f32, |m, x| m.max(x.abs())),
        left_playback: pair.left_note,
        right_playback: pair.right_note,
        left_start_ms: pair.left_start_ms,
        right_start_ms: pair.right_start_ms,
        left_end_ms: pair.left_end_ms,
        right_end_ms: pair.right_end_ms,
    })
}

/// 手动对齐试听：按给定延迟值给两路预垫静音，然后**同时**往两个端点播点击串。
///
/// 用户听到双击/镶边就说明没对齐，拖动延迟值（通常是先按 [`SyncResult::delays`] 的建议
/// 值填）直到两下重合成一下。不需要麦克风。
///
/// * `left_delay_ms` / `right_delay_ms`：各自要垫多少毫秒（0 表示不垫）；
/// * `seconds`：播放时长，夹到 0.1 ~ 60 秒。
#[cfg(windows)]
pub fn play_alignment_clicks(
    left_endpoint: &str,
    right_endpoint: &str,
    left_delay_ms: u32,
    right_delay_ms: u32,
    seconds: f64,
) -> Result<()> {
    if !seconds.is_finite() || seconds <= 0.0 {
        return Err(AudioError::invalid(format!("播放时长非法：{seconds}")));
    }
    if left_endpoint.trim().is_empty() || right_endpoint.trim().is_empty() {
        return Err(AudioError::invalid("左右端点都不能为空"));
    }
    let seconds = seconds.clamp(0.1, 60.0);
    let ms = (seconds * 1000.0) as u32;
    let clicks = alignment_clicks(ms, 0.9);
    let left = apply_delay(&interleave_stereo_i16(&clicks), left_delay_ms as i32)?;
    let right = apply_delay(&interleave_stereo_i16(&clicks), right_delay_ms as i32)?;
    let format = PcmFormat::new(SYNC_RATE, SYNC_CHANNELS, 16);

    let right_dev = right_endpoint.to_string();
    let handle =
        std::thread::spawn(move || play_pcm_s16(&selector_for(&right_dev), format, &right));
    let left_result = play_pcm_s16(&selector_for(left_endpoint), format, &left);
    let right_result = handle
        .join()
        .map_err(|_| AudioError::invalid("右路播放线程 panic"))?;
    left_result?;
    right_result?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 确定性伪随机（LCG），避免为一个测试引入 `rand` 依赖。
    struct Lcg(u64);

    impl Lcg {
        fn new(seed: u64) -> Self {
            Self(seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1))
        }

        /// 均匀分布在 [-1, 1)。
        fn next_f32(&mut self) -> f32 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let u = (self.0 >> 40) as f32 / (1u64 << 24) as f32; // [0,1)
            u * 2.0 - 1.0
        }
    }

    /// 给信号加上指定 SNR 的高斯-ish（均匀）噪声。
    fn add_noise(signal: &[f32], snr_db: f64, seed: u64) -> Vec<f32> {
        let n = signal.len().max(1) as f64;
        let power: f64 = signal
            .iter()
            .map(|x| (*x as f64) * (*x as f64))
            .sum::<f64>()
            / n;
        let noise_power = power / 10f64.powf(snr_db / 10.0);
        let scale = noise_power.sqrt() as f32;
        let mut rng = Lcg::new(seed);
        signal.iter().map(|x| x + rng.next_f32() * scale).collect()
    }

    /// 整数滞后移位：前面补 `lag` 个零。
    fn shift(signal: &[f32], lag: usize) -> Vec<f32> {
        let mut out = vec![0.0f32; lag + signal.len()];
        out[lag..].copy_from_slice(signal);
        out
    }

    /// 分数滞后移位（线性插值），用来验证亚采样精度。
    fn shift_fractional(signal: &[f32], lag: f64, pad: usize) -> Vec<f32> {
        let n = pad + signal.len() + 2;
        (0..n)
            .map(|j| {
                let pos = j as f64 - lag;
                if pos < 0.0 || pos >= (signal.len() - 1) as f64 {
                    return 0.0;
                }
                let i = pos.floor() as usize;
                let f = (pos - i as f64) as f32;
                signal[i] * (1.0 - f) + signal[i + 1] * f
            })
            .collect()
    }

    fn sweep_ref(ms: u32, amp: f32) -> Vec<f32> {
        probe_to_f32(&probe_signal(SyncProbe::Sweep, ms, amp))
    }

    // ---- 探测信号 ----

    #[test]
    fn probe_signals_have_expected_shape() {
        for kind in SyncProbe::ALL {
            let s = probe_signal(kind, 50, 0.5);
            assert_eq!(s.len(), 50 * SYNC_RATE as usize / 1000, "{kind} 长度不对");
            let peak = s.iter().map(|v| v.unsigned_abs()).max().unwrap();
            assert!(peak > 0, "{kind} 全零");
            assert!(peak <= 32_767, "{kind} 溢出");
        }

        // 点击串必须有大段静默间隔（否则退化成连续音）
        let clicks = probe_signal(SyncProbe::ClickTrain, 100, 1.0);
        let zeros = clicks.iter().filter(|v| **v == 0).count();
        assert!(
            zeros > clicks.len() / 2,
            "点击串静默比例太低：{zeros}/{}",
            clicks.len()
        );

        // 幅度为 0 时应当全零（measure_one 会据此报错）
        assert!(probe_signal(SyncProbe::Sweep, 20, 0.0)
            .iter()
            .all(|v| *v == 0));

        // 非法 ms 会被夹住而不是 panic
        assert!(!probe_signal(SyncProbe::Sweep, 0, 1.0).is_empty());
    }

    #[test]
    fn probe_kind_parses_round_trip() {
        for kind in SyncProbe::ALL {
            assert_eq!(SyncProbe::from_str_opt(kind.as_str()), Some(kind));
        }
        assert_eq!(SyncProbe::from_str_opt("SWEEP"), Some(SyncProbe::Sweep));
        assert_eq!(
            SyncProbe::from_str_opt("clicktrain"),
            Some(SyncProbe::ClickTrain)
        );
        assert_eq!(SyncProbe::from_str_opt("nope"), None);
    }

    // ---- 互相关 ----

    #[test]
    fn recovers_known_lag_of_1000_samples() {
        // 1000 采样 @44100 = 22.676 ms
        let reference = sweep_ref(50, 0.8);
        let observed = add_noise(&shift(&reference, 1000), 20.0, 7);
        let c = cross_correlate_detailed(&reference, &observed);
        assert_eq!(c.lag, 1000, "整数滞后应精确为 1000（{}）", c.describe());
        let ms = c.lag_ms(SYNC_RATE);
        let expect = 1000.0 * 1000.0 / SYNC_RATE as f64;
        assert!(
            (ms - expect).abs() <= 0.1,
            "lag_ms={ms} 期望 {expect}（容差 0.1ms）"
        );
        assert!(c.peak > 0.5, "峰值太低：{}", c.describe());
        assert!(c.confidence > 0.5, "置信度太低：{}", c.describe());
    }

    /// 同时法的核心前提：两段频带互不重叠的扫频混在同一段录音里仍然各自可辨，
    /// 能分别相关出各自的到达时刻。这是「≤1 ms」精度的基础。
    #[test]
    fn simultaneous_pair_separates_two_bands() {
        let (pl, pr) = probe_pair(200, 0.9, SYNC_RATE);
        assert_eq!(pl.len(), pr.len(), "左右探测长度应一致");
        let rl = probe_to_f32(&pl);
        let rr = probe_to_f32(&pr);

        // 1) 两段信号必须近似正交，否则会互相把对方的峰拉偏
        let dot: f64 = rl
            .iter()
            .zip(rr.iter())
            .map(|(a, b)| (*a as f64) * (*b as f64))
            .sum();
        let ea: f64 = rl.iter().map(|x| (*x as f64) * (*x as f64)).sum();
        let eb: f64 = rr.iter().map(|x| (*x as f64) * (*x as f64)).sum();
        let ncc = dot / (ea * eb).sqrt();
        assert!(ncc.abs() < 0.1, "两段扫频不够正交：ncc={ncc}");

        // 2) 合成「录音」：左 1000 采样、右 1500 采样，右路弱 30% + 20 dB 噪声
        let lag_l = 1000usize;
        let lag_r = 1500usize;
        let mut obs = shift(&rl, lag_l);
        obs.resize(obs.len().max(lag_r + rr.len()), 0.0);
        for (i, v) in rr.iter().enumerate() {
            obs[lag_r + i] += 0.7 * v;
        }
        let obs = add_noise(&obs, 20.0, 31);

        let cl = cross_correlate_detailed(&rl, &obs);
        let cr = cross_correlate_detailed(&rr, &obs);
        let l_ms = cl.lag_ms(SYNC_RATE);
        let r_ms = cr.lag_ms(SYNC_RATE);
        let want_l = lag_ms(lag_l as f64, SYNC_RATE);
        let want_r = lag_ms(lag_r as f64, SYNC_RATE);
        assert!((l_ms - want_l).abs() <= 0.1, "左路 {l_ms} 期望 {want_l}");
        assert!((r_ms - want_r).abs() <= 0.1, "右路 {r_ms} 期望 {want_r}");

        // 3) 真正要报给用户的是差值
        let delta = l_ms - r_ms;
        let want_delta = want_l - want_r;
        assert!(
            (delta - want_delta).abs() <= 0.1,
            "delta {delta} 期望 {want_delta}"
        );
    }

    /// 带内扫频的频带参数会被夹到合法范围，反序也会自动纠正。
    #[test]
    fn banded_sweep_clamps_and_orders_bands() {
        // 反序（降频）不应 panic，且与正序长度一致
        let up = sweep_signal_banded(50, 0.8, SYNC_RATE, 600.0, 2_400.0);
        let down = sweep_signal_banded(50, 0.8, SYNC_RATE, 2_400.0, 600.0);
        assert_eq!(up.len(), down.len());
        assert_eq!(up.len(), (SYNC_RATE as usize * 50) / 1000);
        // 超出奈奎斯特/低于 20 Hz 会被夹住而不是产生 NaN
        let weird = sweep_signal_banded(20, 0.8, SYNC_RATE, -5_000.0, 1e9);
        assert!(
            weird.iter().all(|v| *v != i16::MIN),
            "夹取后不该出现饱和异常值"
        );
        assert_eq!(weird.len(), (SYNC_RATE as usize * 20) / 1000);
    }

    #[test]
    fn recovers_zero_lag() {
        let reference = sweep_ref(50, 0.8);
        let observed = add_noise(&reference, 20.0, 11);
        let (lag, peak) = cross_correlate(&reference, &observed);
        assert_eq!(lag, 0, "零滞后应算回 0");
        assert!(peak > 0.5, "峰值太低：{peak}");
    }

    #[test]
    fn still_finds_peak_at_zero_snr() {
        let reference = sweep_ref(60, 0.9);
        let observed = add_noise(&shift(&reference, 1500), 0.0, 23);
        let c = cross_correlate_detailed(&reference, &observed);
        assert!(
            c.lag.abs_diff(1500) <= 3,
            "0 dB SNR 下滞后偏差过大：{}",
            c.describe()
        );
        assert!(c.peak > 0.15, "0 dB SNR 下峰值太低：{}", c.describe());
    }

    #[test]
    fn sub_sample_interpolation_is_accurate() {
        let reference = sweep_ref(50, 0.8);
        let observed = shift_fractional(&reference, 800.5, 900);
        let c = cross_correlate_detailed(&reference, &observed);
        assert!(
            (c.lag_sub - 800.5).abs() <= 1.0,
            "亚采样滞后应接近 800.5：{}",
            c.describe()
        );
        // 抛物线插值必须真的比整数解更接近真值
        let int_err = (c.lag as f64 - 800.5).abs();
        let sub_err = (c.lag_sub - 800.5).abs();
        assert!(sub_err <= int_err, "插值没有改善：{}", c.describe());
    }

    #[test]
    fn click_train_correlates_at_its_period() {
        // 点击串在周期倍数处有假峰，但真峰一定在第一个周期内的正确位置
        let reference = probe_to_f32(&probe_signal(SyncProbe::ClickTrain, 40, 0.9));
        let observed = shift(&reference, 3000);
        let c = cross_correlate_detailed(&reference, &observed);
        assert_eq!(c.lag, 3000, "{}", c.describe());
    }

    #[test]
    fn correlation_degenerate_inputs_are_safe() {
        let s = vec![0.1f32; 16];
        assert_eq!(cross_correlate(&[], &s), (0, 0.0));
        assert_eq!(cross_correlate(&s, &[]), (0, 0.0));
        // observed 比 reference 短
        assert_eq!(cross_correlate(&s, &[0.0f32; 4]), (0, 0.0));
        // 全零输入不该出 NaN
        let c = cross_correlate_detailed(&[0.0f32; 8], &[0.0f32; 32]);
        assert!(c.peak.is_finite() && c.lag_sub.is_finite());
    }

    #[test]
    fn lag_ms_converts() {
        assert!((lag_ms(1000.0, 44_100) - 22.675_736).abs() < 1e-5);
        assert_eq!(lag_ms(1000.0, 0), 0.0);
    }

    // ---- 延迟分配 ----

    fn result(left: f64, right: f64) -> SyncResult {
        SyncResult {
            left_ms: left,
            right_ms: right,
            delta_ms: left - right,
            confidence: 1.0,
            peak_snr: 9.0,
            method: SyncMethod::Simultaneous,
        }
    }

    #[test]
    fn delay_ms_for_slows_the_early_channel() {
        // 左路更晚 12 ms → 该给右路垫 12 ms
        assert_eq!(delay_ms_for(&result(12.0, 0.0)), (0, 12));
        // 右路更晚 12.4 ms → 该给左路垫 12 ms（四舍五入）
        assert_eq!(delay_ms_for(&result(0.0, 12.4)), (12, 0));
        // 已经对齐
        assert_eq!(delay_ms_for(&result(0.0, 0.0)), (0, 0));
        // 0.4 ms 的差四舍五入到 0，不该产生 1 ms 的过冲
        assert_eq!(delay_ms_for(&result(0.4, 0.0)), (0, 0));
        // 非有限值不能 panic
        let mut bad = result(0.0, 0.0);
        bad.delta_ms = f64::NAN;
        assert_eq!(delay_ms_for(&bad), (0, 0));
        // 方法版与自由函数一致
        assert_eq!(result(5.0, 0.0).delays(), (0, 5));
    }

    // ---- apply_delay ----

    #[test]
    fn apply_delay_pads_silence() {
        let mono = vec![100i16; 100];
        let pcm = interleave_stereo_i16(&mono);
        assert_eq!(pcm.len(), 400);

        // 0 ms：原样返回
        assert_eq!(apply_delay(&pcm, 0).unwrap(), pcm);

        // 10 ms @44100 = 441 帧 = 1764 字节
        let delayed = apply_delay(&pcm, 10).unwrap();
        assert_eq!(delayed.len(), 400 + 441 * 4);
        assert!(delayed[..441 * 4].iter().all(|b| *b == 0), "前面必须是静音");
        assert_eq!(&delayed[441 * 4..], &pcm[..], "原始数据必须整体后移");

        // 负值拒绝
        let err = apply_delay(&pcm, -1).unwrap_err();
        assert!(matches!(err, AudioError::InvalidArgument(_)), "{err}");

        // 长度不是帧长整数倍时拒绝
        assert!(apply_delay(&pcm[..3], 10).is_err());

        // 显式采样率版本
        let at48k = apply_delay_at(&pcm, 10, 48_000, 2).unwrap();
        assert_eq!(at48k.len(), 400 + 480 * 4);
    }

    // ---- 体检结论 ----

    #[test]
    fn mic_verdict_thresholds() {
        assert_eq!(classify_mic_level(-20.0, 1000), MicVerdict::Signal);
        assert_eq!(
            classify_mic_level(MIC_SIGNAL_DBFS + 0.1, 1000),
            MicVerdict::Signal
        );
        assert_eq!(
            classify_mic_level(MIC_SIGNAL_DBFS, 1000),
            MicVerdict::NoiseFloor
        );
        // 本机 Realtek 实测 -63.1 dBFS
        assert_eq!(classify_mic_level(-63.1, 1000), MicVerdict::NoiseFloor);
        assert_eq!(classify_mic_level(MIC_NOISE_DBFS, 1000), MicVerdict::Silent);
        // 本机 Chu2 DSP 实测 -90.3 dBFS
        assert_eq!(classify_mic_level(-90.3, 1000), MicVerdict::Silent);
        assert_eq!(
            classify_mic_level(f64::NEG_INFINITY, 1000),
            MicVerdict::Silent
        );
        // 一个点都没拿到
        assert_eq!(classify_mic_level(-20.0, 0), MicVerdict::Unavailable);
        // 有本底噪声也能用于自动标定：设备确实在采数据，探测信号够响就能出峰。
        // 本机实测同一只麦两次量到 -43.3 / -46.7 dBFS，正好跨过门限——把门限当可用性判据
        // 会让判定在门限附近反复横跳。
        assert!(MicVerdict::Signal.is_usable());
        assert!(MicVerdict::NoiseFloor.is_usable());
        assert!(!MicVerdict::Silent.is_usable());
        assert!(!MicVerdict::Unavailable.is_usable());
    }

    #[test]
    fn pcm_stats_matches_known_values() {
        // 全零 = 数字静默
        let s = pcm_stats(&vec![0u8; 400]);
        assert_eq!(s.samples, 200);
        assert_eq!(s.peak, 0.0);
        assert_eq!(s.peak_dbfs(), f64::NEG_INFINITY);
        assert_eq!(s.verdict(), MicVerdict::Silent);

        // ±1 LSB 抖动（本机 Chu2 DSP 的实测形状）
        let mut jitter = Vec::new();
        for i in 0..200i32 {
            let v: i16 = if i % 2 == 0 { 1 } else { -1 };
            jitter.extend_from_slice(&v.to_le_bytes());
        }
        let s = pcm_stats(&jitter);
        assert_eq!(s.peak, 1.0 / 32_768.0);
        assert_eq!(s.verdict(), MicVerdict::Silent);

        // 满幅正弦
        let full = interleave_stereo_i16(&[32_767i16, -32_768, 32_767, -32_768]);
        let s = pcm_stats(&full);
        assert!(s.peak > 0.999);
        assert_eq!(s.verdict(), MicVerdict::Signal);
    }

    #[test]
    fn mic_health_unavailable_shape() {
        let h = MicHealth::unavailable("打不开采集设备：测试");
        assert_eq!(h.verdict, MicVerdict::Unavailable);
        assert_eq!(h.samples, 0);
        assert!(h.describe().contains("打不开"));
        assert!(h.describe().contains("测试"));
    }

    // ---- 端点分类 ----

    #[test]
    fn mic_name_classification() {
        assert_eq!(classify_mic_name("麦克风 (Chu2 DSP)"), MicKind::Physical);
        assert_eq!(
            classify_mic_name("麦克风阵列 (Realtek High Definition Audio)"),
            MicKind::Physical
        );
        assert_eq!(classify_mic_name("立体声混音 (Realtek)"), MicKind::Loopback);
        assert_eq!(classify_mic_name("Stereo Mix (Realtek)"), MicKind::Loopback);
        assert_eq!(
            classify_mic_name("麦克风 (Steam Streaming Microphone)"),
            MicKind::Virtual
        );
        assert_eq!(
            classify_mic_name("麦克风阵列 (UU远程虚拟音频设备)"),
            MicKind::Virtual
        );
        assert_eq!(classify_mic_name("Line In (Something)"), MicKind::Unknown);
        // 回环判定优先于「麦克风」字样
        assert_eq!(
            classify_mic_name("麦克风 (立体声混音回环)"),
            MicKind::Loopback
        );
    }

    #[test]
    fn selector_for_handles_guid_and_substring() {
        assert_eq!(
            selector_for("麦克风"),
            DeviceSelector::name_contains("麦克风")
        );
        assert_eq!(
            selector_for(" {11111111-2222-3333-4444-555555555555} "),
            DeviceSelector::Id("{11111111-2222-3333-4444-555555555555}".to_string())
        );
    }

    // ---- 降混 ----

    #[test]
    fn downmix_picks_the_louder_channel() {
        // 左路满幅、右路接近零 → 必须选左路
        let mut bytes = Vec::new();
        for _ in 0..8 {
            bytes.extend_from_slice(&30_000i16.to_le_bytes());
            bytes.extend_from_slice(&1i16.to_le_bytes());
        }
        let mono = downmix_to_mono(&bytes);
        assert_eq!(mono.len(), 8);
        assert!(mono[0] > 0.9, "应选到左路：{mono:?}");

        // 反过来选右路
        let mut bytes = Vec::new();
        for _ in 0..8 {
            bytes.extend_from_slice(&1i16.to_le_bytes());
            bytes.extend_from_slice(&30_000i16.to_le_bytes());
        }
        let mono = downmix_to_mono(&bytes);
        assert!(mono[0] > 0.9, "应选到右路：{mono:?}");
    }

    // ---- 手动对齐 ----

    #[test]
    fn alignment_clicks_are_sparse_with_accents() {
        let clicks = alignment_clicks(2_000, 0.9);
        assert_eq!(clicks.len(), 2 * SYNC_RATE as usize);
        let peak = clicks.iter().map(|v| v.unsigned_abs()).max().unwrap();
        assert!(peak > 20_000, "点击串太轻：{peak}");
        // 2 秒 / 300ms 间隔 ≈ 6~7 下，静默必须占绝大多数
        let zeros = clicks.iter().filter(|v| **v == 0).count();
        assert!(zeros > clicks.len() * 9 / 10, "静默比例太低：{zeros}");
        // 每 4 下加重一次：至少有一个明显更轻的点击
        let period = SYNC_RATE as usize * ALIGN_CLICK_PERIOD_MS as usize / 1000;
        let first_peak = clicks[..period]
            .iter()
            .map(|v| v.unsigned_abs())
            .max()
            .unwrap();
        let second_peak = clicks[period..period * 2]
            .iter()
            .map(|v| v.unsigned_abs())
            .max()
            .unwrap();
        assert!(
            first_peak > second_peak,
            "重拍应比弱拍响：{first_peak} vs {second_peak}"
        );
    }

    #[test]
    fn alignment_clicks_shift_by_exact_frame_count() {
        // 手动对齐的数学核心：垫 7 ms 就是把整串后移 7ms
        let clicks = alignment_clicks(1_000, 0.9);
        let pcm = interleave_stereo_i16(&clicks);
        let shifted = apply_delay(&pcm, 7).unwrap();
        let frames = 7 * SYNC_RATE as usize / 1000;
        assert_eq!(shifted.len(), pcm.len() + frames * 4);
        // 互相关能把这个 7ms 位移算回来（左/右两路的差就是这么来的）
        let a = probe_to_f32(&clicks);
        let b = probe_to_f32(&clicks);
        let observed: Vec<f32> = std::iter::repeat_n(0.0f32, frames).chain(b).collect();
        let c = cross_correlate_detailed(&a, &observed);
        assert_eq!(c.lag, frames);
        assert!((c.lag_ms(SYNC_RATE) - 7.0).abs() < 0.05, "{}", c.describe());
    }

    // ---- 配置 ----

    #[test]
    fn sync_config_window_and_defaults() {
        let cfg = SyncConfig::default();
        assert_eq!(
            cfg.window_ms(),
            cfg.pre_roll_ms + cfg.probe_ms + cfg.tail_ms
        );
        assert_eq!(cfg.probe, SyncProbe::Sweep);

        let cfg = SyncConfig::new("麦克风", "音箱 A")
            .with_probe_ms(80)
            .with_amplitude(0.5)
            .with_probe(SyncProbe::Mls)
            .with_pre_roll_ms(1_500)
            .with_tail_ms(200);
        assert_eq!(cfg.window_ms(), 1_500 + 80 + 200);
        assert_eq!(cfg.probe, SyncProbe::Mls);
        assert!(cfg.describe().contains("音箱 A"));
    }

    // ---- 极性 / 梳状 ----

    #[test]
    fn signed_correlation_sees_inversion() {
        let rate = 48_000u32;
        let probe = polarity_probe(120, 0.8, rate);
        let reference = probe_to_f32(&probe);
        let lag = 700usize;

        let mut same = vec![0.0f32; lag];
        same.extend_from_slice(&reference);
        let c_same = cross_correlate_signed(&reference, &same);
        assert!(c_same.peak > 0.5, "同相应为正峰：{}", c_same.peak);
        assert!((c_same.lag as i64 - lag as i64).abs() <= 2);

        let mut flipped = vec![0.0f32; lag];
        flipped.extend(reference.iter().map(|x| -x));
        let c_flip = cross_correlate_signed(&reference, &flipped);
        assert!(c_flip.peak < -0.5, "反相应为负峰：{}", c_flip.peak);
        assert!((c_flip.lag as i64 - lag as i64).abs() <= 2);

        // 关键：普通（取最大正值）的互相关看不到反相
        let c_abs = cross_correlate_detailed(&reference, &flipped);
        assert!(c_abs.peak > 0.0, "argmax 版必然给正值：{}", c_abs.peak);
    }

    #[test]
    fn goertzel_finds_the_tone() {
        let rate = 48_000u32;
        let n = 4_800usize;
        let seg: Vec<f32> = (0..n)
            .map(|i| (std::f64::consts::TAU * 1_000.0 * i as f64 / rate as f64).sin() as f32)
            .collect();
        let on = goertzel_magnitude(&seg, rate, 1_000.0);
        let off = goertzel_magnitude(&seg, rate, 1_500.0);
        assert!(on > 0.4, "1 kHz 幅度太小：{on}");
        assert!(off < 0.02, "1.5 kHz 不该有能量：{off}");
    }

    #[test]
    fn top_peaks_finds_two_separated_copies() {
        let mut curve = vec![0.0f32; 2_000];
        curve[500] = 0.9;
        curve[501] = 0.85;
        curve[700] = -0.8;
        let peaks = top_peaks(&curve, 2, 24);
        assert_eq!(peaks.len(), 2);
        assert_eq!(peaks[0].0, 500, "相邻的 501 应当被 min_sep 挡掉");
        assert_eq!(peaks[1].0, 700);
        assert!(peaks[1].1 < 0.0, "第二个峰的符号要保留：{}", peaks[1].1);
    }

    /// 把参考信号按已知延迟放到一段录音里（前面垫静音）。
    fn place(reference: &[f32], lag: usize, gain: f32) -> Vec<f32> {
        let mut out = vec![0.0f32; lag];
        out.extend(reference.iter().map(|x| x * gain));
        out
    }

    #[test]
    fn comb_curve_is_one_when_in_phase_and_aligned() {
        let rate = 48_000u32;
        let probe_ms = 600u32;
        let reference = probe_to_f32(&polarity_probe(probe_ms, 0.8, rate));
        let lag = 1_000usize;

        let cap_l = place(&reference, lag, 1.0);
        let cap_r = place(&reference, lag, 0.8);
        // 同相 + 对齐 → 两路直接相加
        let sum: Vec<f32> = reference.iter().map(|x| x * 1.8).collect();
        let cap_b = place(&sum, lag, 1.0);

        let curve = comb_curve_from(
            &cap_l, rate, lag as f64, &cap_r, rate, lag as f64, &cap_b, rate, lag as f64, probe_ms,
        );
        assert_eq!(curve.len(), COMB_POINTS);
        for p in &curve {
            assert!(
                (p.ratio - 1.0).abs() < 0.05,
                "同相且对齐时 ratio 应当≈1，实测 {:.3} @ {:.0} Hz",
                p.ratio,
                p.freq_hz
            );
        }
    }

    #[test]
    fn comb_curve_sees_anti_phase_as_broad_cancellation() {
        let rate = 48_000u32;
        let probe_ms = 600u32;
        let reference = probe_to_f32(&polarity_probe(probe_ms, 0.8, rate));
        let lag = 1_000usize;

        let cap_l = place(&reference, lag, 1.0);
        let cap_r = place(&reference, lag, 0.8);
        // 反相 + 对齐 → 相减：|1-0.8|/(1+0.8) = 0.111
        let diff: Vec<f32> = reference.iter().map(|x| x * 0.2).collect();
        let cap_b = place(&diff, lag, 1.0);

        let curve = comb_curve_from(
            &cap_l, rate, lag as f64, &cap_r, rate, lag as f64, &cap_b, rate, lag as f64, probe_ms,
        );
        let mean: f64 = curve.iter().map(|p| p.ratio).sum::<f64>() / curve.len() as f64;
        assert!(
            (mean - 0.1111).abs() < 0.05,
            "反相且对齐时 ratio 应当≈0.111，实测 {mean:.3}"
        );
    }

    #[test]
    fn comb_curve_shows_notches_when_offset() {
        let rate = 48_000u32;
        let probe_ms = 2_000u32;
        let reference = probe_to_f32(&polarity_probe(probe_ms, 0.8, rate));
        let lag = 1_000usize;
        let delta = (0.003 * rate as f64).round() as usize; // 3 ms

        let cap_l = place(&reference, lag, 1.0);
        let cap_r = place(&reference, lag, 1.0);
        // 同相但右路晚 3 ms：合成 = s(t) + s(t-Δ)
        let mut cap_b = place(&reference, lag, 1.0);
        for (i, x) in reference.iter().enumerate() {
            let j = lag + i + delta;
            if j < cap_b.len() {
                cap_b[j] += *x;
            }
        }

        let curve = comb_curve_from(
            &cap_l, rate, lag as f64, &cap_r, rate, lag as f64, &cap_b, rate, lag as f64, probe_ms,
        );
        let min = curve.iter().map(|p| p.ratio).fold(f64::INFINITY, f64::min);
        let max = curve.iter().map(|p| p.ratio).fold(0.0f64, f64::max);
        assert!(min < 0.6, "3 ms 偏移应当挖出深谷，实测谷底 {min:.3}");
        assert!(max > 0.9, "梳状应当也有接近 1 的峰顶，实测峰顶 {max:.3}");
    }

    #[test]
    fn polarity_probe_covers_the_band() {
        let probe = polarity_probe(200, 0.8, 48_000);
        assert!(!probe.is_empty());
        assert!(probe.iter().any(|&s| s != 0));
        // 峰值不该削顶
        let peak = probe.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
        assert!(peak < i16::MAX as u16, "不该削顶：{peak}");
    }

    #[test]
    fn tone_signal_has_the_right_frequency_and_length() {
        let t = tone_signal(1_000.0, 200, 0.8, 48_000);
        assert_eq!(t.len(), 9_600);
        let f: Vec<f32> = t.iter().map(|&s| s as f32 / i16::MAX as f32).collect();
        let est = crate::convert::estimate_frequency(&f, 48_000).expect("应能估出频率");
        assert!((est - 1_000.0).abs() < 5.0, "estimate_frequency = {est}");
        let peak = t.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
        assert!(peak > 20_000, "幅度应接近 0.8 满幅：{peak}");
        assert!(peak < i16::MAX as u16, "不该削顶：{peak}");
    }

    #[test]
    fn best_tone_window_locates_the_tone() {
        let rate = 48_000u32;
        let win = 4_800usize; // 100 ms
        let start = 30_000usize;
        let mut s = vec![0.0f32; rate as usize];
        for (i, &v) in tone_signal(1_000.0, 200, 0.8, rate).iter().enumerate() {
            s[start + i] = v as f32 / i16::MAX as f32;
        }
        let (m, at) = best_tone_window(&s, rate, 1_000.0, win);
        assert!(m > 0.2, "单音窗幅度太小：{m}");
        assert!(
            at.abs_diff(start) <= win,
            "窗起点 {at} 应落在单音开始处 {start} 附近"
        );
        // 换一个频率应当几乎量不到东西
        let (m2, _) = best_tone_window(&s, rate, 3_000.0, win);
        assert!(m2 < m * 0.2, "3 kHz 处不该有能量：{m2} vs {m}");
    }

    #[test]
    fn direct_arrival_ignores_a_stronger_inverted_reflection() {
        // 实测形态：直达声 +0.4 先到，7 ms 后一个反相强反射 -0.6
        let peaks = vec![(100.0f64, 0.40f32), (101.7, -0.26), (107.3, -0.60)];
        let (ms, v) = direct_arrival(&peaks, (107.3, -0.60));
        assert_eq!(ms, 100.0, "直达声是最早的显著峰");
        assert!((v - 0.40).abs() < 1e-6);
        // 全部低于阈值时退回 fallback
        let weak = vec![(10.0f64, 0.01f32)];
        let (_, v2) = direct_arrival(&weak, (10.0, 0.01));
        assert!((v2 - 0.01).abs() < 1e-6);
    }
}
