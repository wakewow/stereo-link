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
//! * [`play_pcm_s16`](crate::playback::play_pcm_s16) 会先往 WASAPI 缓冲里预填一整块
//!   静音再 `Start()`，所以真实出声时刻比调用时刻晚约 1 秒；录制窗口必须把这段
//!   「前置静音」算进去（见 [`SyncConfig::pre_roll_ms`]）。这段延迟对左右两路是**相同
//!   的固定值**，因此在 `delta_ms` 里自动抵消。
//!
//! # 本机现状（实测，2026-10-18）
//!
//! 本机的 **Chu2 DSP**（USB 复合设备 `VID_31B2&PID_0113` 的 MI_00，走微软通用
//! `USBAudio.sys`；采集端点 `{1cedfae2-ed08-46a2-bc71-ea7b7c4673ce}`）**麦克风是好的**：
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
        if f0.is_finite() { f0.clamp(20.0, nyq) } else { 20.0 },
        if f1.is_finite() { f1.clamp(20.0, nyq) } else { nyq },
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
    if frame_bytes == 0 || pcm.len() % frame_bytes != 0 {
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
    for ch in bytes.chunks_exact(2) {
        let v = i16::from_le_bytes([ch[0], ch[1]]) as f64 / 32_768.0;
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
    for f in bytes.chunks_exact(4) {
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
    if has(&["立体声混音", "stereo mix", "what u hear", "loopback", "回环", "监视器"]) {
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
        let a = if k % 4 == 0 { amp } else { amp * 0.45 };
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
            .chunks_exact(2)
            .map(|c| s16_to_f32(i16::from_le_bytes([c[0], c[1]])))
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
            AudioError::invalid(format!(
                "独占模式失败（{ex}）；共享模式也失败（{sh}）"
            ))
        }),
    }
}

/// 列出本机所有**活动**的采集端点，并给出类别与「值不值得先试」的提示。
///
/// 注意 [`list_endpoints`](crate::capture::list_endpoints) 只枚举活动端点
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
        Err(sim) => calibrate_pair_sequential(mic, left_endpoint, right_endpoint, cfg).map_err(
            |seq| {
                AudioError::invalid(format!(
                    "同时法失败（{sim}）；依次法也失败（{seq}）"
                ))
            },
        ),
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
        let power: f64 = signal.iter().map(|x| (*x as f64) * (*x as f64)).sum::<f64>() / n;
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
        assert!(probe_signal(SyncProbe::Sweep, 20, 0.0).iter().all(|v| *v == 0));

        // 非法 ms 会被夹住而不是 panic
        assert!(!probe_signal(SyncProbe::Sweep, 0, 1.0).is_empty());
    }

    #[test]
    fn probe_kind_parses_round_trip() {
        for kind in SyncProbe::ALL {
            assert_eq!(SyncProbe::from_str_opt(kind.as_str()), Some(kind));
        }
        assert_eq!(SyncProbe::from_str_opt("SWEEP"), Some(SyncProbe::Sweep));
        assert_eq!(SyncProbe::from_str_opt("clicktrain"), Some(SyncProbe::ClickTrain));
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
        assert!(weird.iter().all(|v| *v != i16::MIN), "夹取后不该出现饱和异常值");
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
        let c = cross_correlate_detailed(&vec![0.0f32; 8], &vec![0.0f32; 32]);
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
        assert_eq!(classify_mic_level(MIC_SIGNAL_DBFS + 0.1, 1000), MicVerdict::Signal);
        assert_eq!(classify_mic_level(MIC_SIGNAL_DBFS, 1000), MicVerdict::NoiseFloor);
        // 本机 Realtek 实测 -63.1 dBFS
        assert_eq!(classify_mic_level(-63.1, 1000), MicVerdict::NoiseFloor);
        assert_eq!(classify_mic_level(MIC_NOISE_DBFS, 1000), MicVerdict::Silent);
        // 本机 Chu2 DSP 实测 -90.3 dBFS
        assert_eq!(classify_mic_level(-90.3, 1000), MicVerdict::Silent);
        assert_eq!(classify_mic_level(f64::NEG_INFINITY, 1000), MicVerdict::Silent);
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
        assert_eq!(classify_mic_name("麦克风阵列 (Realtek High Definition Audio)"), MicKind::Physical);
        assert_eq!(classify_mic_name("立体声混音 (Realtek)"), MicKind::Loopback);
        assert_eq!(classify_mic_name("Stereo Mix (Realtek)"), MicKind::Loopback);
        assert_eq!(classify_mic_name("麦克风 (Steam Streaming Microphone)"), MicKind::Virtual);
        assert_eq!(classify_mic_name("麦克风阵列 (UU远程虚拟音频设备)"), MicKind::Virtual);
        assert_eq!(classify_mic_name("Line In (Something)"), MicKind::Unknown);
        // 回环判定优先于「麦克风」字样
        assert_eq!(classify_mic_name("麦克风 (立体声混音回环)"), MicKind::Loopback);
    }

    #[test]
    fn selector_for_handles_guid_and_substring() {
        assert_eq!(selector_for("麦克风"), DeviceSelector::name_contains("麦克风"));
        assert_eq!(
            selector_for(" {1cedfae2-ed08-46a2-bc71-ea7b7c4673ce} "),
            DeviceSelector::Id("{1cedfae2-ed08-46a2-bc71-ea7b7c4673ce}".to_string())
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
        let first_peak = clicks[..period].iter().map(|v| v.unsigned_abs()).max().unwrap();
        let second_peak = clicks[period..period * 2]
            .iter()
            .map(|v| v.unsigned_abs())
            .max()
            .unwrap();
        assert!(first_peak > second_peak, "重拍应比弱拍响：{first_peak} vs {second_peak}");
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
        assert_eq!(cfg.window_ms(), cfg.pre_roll_ms + cfg.probe_ms + cfg.tail_ms);
        assert_eq!(cfg.probe, SyncProbe::Sweep);

        let cfg = SyncConfig::new("麦克风", "Xiaomi Sound")
            .with_probe_ms(80)
            .with_amplitude(0.5)
            .with_probe(SyncProbe::Mls)
            .with_pre_roll_ms(1_500)
            .with_tail_ms(200);
        assert_eq!(cfg.window_ms(), 1_500 + 80 + 200);
        assert_eq!(cfg.probe, SyncProbe::Mls);
        assert!(cfg.describe().contains("Xiaomi Sound"));
    }
}
