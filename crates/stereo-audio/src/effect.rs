//! 音色 DSP 链（2→2，采集域、拆路之前）。
//!
//! 三套「调音预设」，都只是对一对立体声做音色/声场塑形的量（不是任何品牌的授权算法，
//! 也不含对象音频/渲染器；名字表示「按该品牌家用产品的听感倾向调的一组参数」）：
//!
//! - **类杜比（Dolby 风格）**：加宽明显 + 低频扎实 + 4.5 kHz 空气感，包围感最强；
//! - **丹拿（Dynaudio 风格）**：几乎不加宽、低频收得早（90 Hz 以下单声道化）、
//!   高频略柔（8.5 kHz −1.5 dB），监听式的干净、结像稳；
//! - **哈曼卡顿（Harman Kardon 风格）**：中等加宽、V 型走向（低频 + 1.8 kHz presence
//!   与 10 kHz 高频都抬一点），流行/人声更「亮、冲、有氛围」。
//!
//! 处理链（全部零新增延迟，不需要前瞻）：
//!
//! 1. 低频单声道化：两路各过一个预设截止频率的一极点低通，取中/侧低频，去掉低频侧
//!    声道（低频没有方向感，反相残留只会让两箱互相抵消）；
//! 2. 低频提升：中声道低频乘 bass_boost_db（低架式）；
//! 3. M/S 加宽：侧声道乘 1 + widen × amount；
//! 4. presence：预设频率的一极点高通提升 presence_db × amount dB；
//! 5. 高架：预设频率以上按 treble_db × amount dB 提升或衰减（丹拿是衰减）；
//! 6. 立体声联动软限幅：瞬时 attack / 50 ms release，天花板 −1 dBFS。
//!
//! 开/关通过约 15 ms 的干湿淡入淡出完成，不会「啪」一声；关闭时不改任何样本
//! （bit-exact bypass，采集格式不受影响）。切换预设只重算几个滤波系数，不重启引擎、
//! 不断音（限幅器与滤波器状态保留，避免切换瞬间跳变）。

/// 加宽强度上限（amount = 1.0 为默认，1.5 已经很夸张）。
pub const MAX_AMOUNT: f32 = 1.5;
/// 加宽强度下限（0 = 不加宽）。
pub const MIN_AMOUNT: f32 = 0.0;
/// 低频提升上限（dB）。
pub const MAX_BASS_DB: f32 = 12.0;
/// 低频提升下限（dB）。
pub const MIN_BASS_DB: f32 = 0.0;
/// 限幅器天花板（约 -1 dBFS）。
pub const LIMITER_CEILING: f32 = 0.891;
/// 低频单声道化的缺省低通截止频率（Hz）；各预设可在 [EffectPreset::profile] 里覆盖。
const DEFAULT_BASS_MONO_HZ: f32 = 180.0;
/// 开/关淡入淡出时长（秒）。
const MIX_SECONDS: f32 = 0.015;
/// 限幅器 release 时长（秒）。
const LIMITER_RELEASE_SECONDS: f32 = 0.05;

/// 一套调音预设的 DSP 参数（采样率无关；系数在 [StereoEffect] 里按实际采样率算）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EffectProfile {
    /// 加宽系数：width = 1 + widen × amount。
    pub widen: f32,
    /// 低频单声道化的低通截止频率（Hz）。
    pub bass_mono_hz: f32,
    /// presence 高通的截止频率（Hz）。
    pub presence_hz: f32,
    /// presence 提升量（dB，乘 amount）。
    pub presence_db: f32,
    /// 高架/高通的转折频率（Hz）。
    pub treble_hz: f32,
    /// 转折频率以上的提升量（dB，乘 amount；负数 = 衰减）。
    pub treble_db: f32,
}

/// 调音预设。
///
/// 引擎把它存成一个原子量（[EffectPreset::code]），热切换不需要重启路由。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EffectPreset {
    /// 类杜比：宽 + 低频 + 空气感。
    #[default]
    Dolby,
    /// 丹拿：监听式、干净、高频柔。
    Dynaudio,
    /// 哈曼卡顿：V 型、亮、有氛围。
    Harman,
}

impl EffectPreset {
    /// 全部预设（GUI 下拉框/CLI 提示用）。
    pub const ALL: [Self; 3] = [Self::Dolby, Self::Dynaudio, Self::Harman];

    /// 稳定英文 id（写配置文件、CLI 参数、JSON 字段都用它）。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Dolby => "dolby",
            Self::Dynaudio => "dynaudio",
            Self::Harman => "harman",
        }
    }

    /// 界面显示名。
    pub const fn label(self) -> &'static str {
        match self {
            Self::Dolby => "类杜比",
            Self::Dynaudio => "丹拿",
            Self::Harman => "哈曼卡顿",
        }
    }

    /// 解析英文 id（也接受中文名，方便手敲 CLI）。
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "dolby" | "dolby_atmos" | "杜比" | "类杜比" => Some(Self::Dolby),
            "dynaudio" | "丹拿" => Some(Self::Dynaudio),
            "harman" | "harman_kardon" | "harman-kardon" | "哈曼" | "哈曼卡顿" => {
                Some(Self::Harman)
            }
            _ => None,
        }
    }

    /// 压缩成 u8 存原子量/配置文件（增减预设时注意保持旧 id 不变）。
    pub const fn code(self) -> u8 {
        match self {
            Self::Dolby => 0,
            Self::Dynaudio => 1,
            Self::Harman => 2,
        }
    }

    /// 从 [EffectPreset::code] 还原；未知值回退到默认预设（向前兼容）。
    pub const fn from_code(code: u8) -> Self {
        match code {
            1 => Self::Dynaudio,
            2 => Self::Harman,
            _ => Self::Dolby,
        }
    }

    /// 该预设的 DSP 参数。
    pub const fn profile(self) -> EffectProfile {
        match self {
            Self::Dolby => EffectProfile {
                widen: 0.70,
                bass_mono_hz: DEFAULT_BASS_MONO_HZ,
                presence_hz: 4_500.0,
                presence_db: 1.5,
                treble_hz: 9_000.0,
                treble_db: 0.0,
            },
            Self::Dynaudio => EffectProfile {
                widen: 0.15,
                bass_mono_hz: 90.0,
                presence_hz: 2_600.0,
                presence_db: 1.0,
                treble_hz: 8_500.0,
                treble_db: -1.5,
            },
            Self::Harman => EffectProfile {
                widen: 0.45,
                bass_mono_hz: 200.0,
                presence_hz: 1_800.0,
                presence_db: 1.8,
                treble_hz: 10_000.0,
                treble_db: 1.2,
            },
        }
    }

    /// 切换预设时建议一起套用的（强度, 低音 dB, 软限幅）。
    pub const fn recommended(self) -> (f32, f32, bool) {
        match self {
            Self::Dolby => (1.0, 4.0, true),
            Self::Dynaudio => (0.8, 2.5, true),
            Self::Harman => (1.0, 6.0, true),
        }
    }
}

/// 音效参数。
///
/// 引擎把它拆成几个原子量存放，运行中可热改（不用重启路由）；所有字段在
/// [EffectSettings::sanitized] 里夹到合法范围，非法值一律回默认。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EffectSettings {
    /// 是否启用（关闭时引擎 bit-exact 直通，不做任何处理）。
    pub enabled: bool,
    /// 调音预设（决定加宽系数、单声道化截止、presence/high-shelf 曲线）。
    pub preset: EffectPreset,
    /// 强度（全局缩放），0.0..=1.5；1.0 = 预设的完整效果。
    pub amount: f32,
    /// 低频提升（dB），0.0..=12.0；0 = 不提升。
    pub bass_boost_db: f32,
    /// 是否启用立体声联动限幅（避免加宽后削顶）。
    pub limiter: bool,
}

impl Default for EffectSettings {
    /// 默认关；参数预置成「打开即好听」的一组（类杜比预设）。
    fn default() -> Self {
        let preset = EffectPreset::default();
        let (amount, bass_boost_db, limiter) = preset.recommended();
        Self {
            enabled: false,
            preset,
            amount,
            bass_boost_db,
            limiter,
        }
    }
}

impl EffectSettings {
    /// 把非有限值和越界值夹回合法范围（默认参数兜底）。
    pub fn sanitized(mut self) -> Self {
        if !self.amount.is_finite() {
            self.amount = 1.0;
        }
        self.amount = self.amount.clamp(MIN_AMOUNT, MAX_AMOUNT);
        if !self.bass_boost_db.is_finite() {
            self.bass_boost_db = 4.0;
        }
        self.bass_boost_db = self.bass_boost_db.clamp(MIN_BASS_DB, MAX_BASS_DB);
        self
    }
}

/// 音效处理器（每个引擎实例一个，采样率固定）。
///
/// 只处理双声道交错 f32 缓冲；帧数不变、不分配（首包预分配一次干声缓冲）。
#[derive(Debug)]
pub struct StereoEffect {
    /// 当前干湿混合量（0 = 完全直通，1 = 全湿）。
    mix: f32,
    /// 开/关渐变系数（每帧）。
    mix_coeff: f32,
    /// 限幅器 release 系数（每帧）。
    release_coeff: f32,
    /// 引擎采样率（Hz），切换预设时按它重算系数。
    rate: f32,
    /// 当前预设（变了才重算系数）。
    preset: EffectPreset,
    /// 低频单声道化低通系数。
    bass_a: f32,
    /// presence 低通系数（高通 = 原信号 − 低通）。
    presence_a: f32,
    /// 高架低通系数（高架 = 原信号 − 低通）。
    treble_a: f32,
    /// 左/右低频低通状态。
    lp_l: f32,
    lp_r: f32,
    /// 左/右 presence 低通状态。
    presence_lp_l: f32,
    presence_lp_r: f32,
    /// 左/右高架低通状态。
    treble_lp_l: f32,
    treble_lp_r: f32,
    /// 限幅器当前增益（1 = 不压）。
    limit_gain: f32,
    /// 干声副本（干湿混合用；复用，不每包分配）。
    dry: Vec<f32>,
}

impl StereoEffect {
    /// 按引擎采样率构造（默认预设 = 类杜比）。
    pub fn new(rate: u32) -> Self {
        let rate = rate.max(8_000) as f32;
        let preset = EffectPreset::default();
        let profile = preset.profile();
        Self {
            mix: 0.0,
            mix_coeff: one_pole_coeff(MIX_SECONDS, rate),
            release_coeff: one_pole_coeff(LIMITER_RELEASE_SECONDS, rate),
            rate,
            preset,
            bass_a: one_pole_coeff_hz(profile.bass_mono_hz, rate),
            presence_a: one_pole_coeff_hz(profile.presence_hz, rate),
            treble_a: one_pole_coeff_hz(profile.treble_hz, rate),
            lp_l: 0.0,
            lp_r: 0.0,
            presence_lp_l: 0.0,
            presence_lp_r: 0.0,
            treble_lp_l: 0.0,
            treble_lp_r: 0.0,
            limit_gain: 1.0,
            dry: Vec::new(),
        }
    }

    /// 当前预设。
    pub fn preset(&self) -> EffectPreset {
        self.preset
    }

    /// 切换预设：只重算系数，滤波器/限幅器状态保留（播放中切不会跳变、不会断音）。
    fn set_preset(&mut self, preset: EffectPreset) {
        if self.preset == preset {
            return;
        }
        self.preset = preset;
        let profile = preset.profile();
        self.bass_a = one_pole_coeff_hz(profile.bass_mono_hz, self.rate);
        self.presence_a = one_pole_coeff_hz(profile.presence_hz, self.rate);
        self.treble_a = one_pole_coeff_hz(profile.treble_hz, self.rate);
    }

    /// 就地处理一段双声道交错 f32 数据（长度必须是偶数；奇数尾巴不碰）。
    ///
    /// 关闭时（且淡出到位）直接返回，样本一个字节都不改，保证 bit-exact 直通。
    pub fn process_interleaved(&mut self, frames: &mut [f32], settings: &EffectSettings) {
        let frames_len = frames.len() & !1;
        if frames_len == 0 {
            return;
        }
        let settings = settings.sanitized();
        self.set_preset(settings.preset);
        // 目标干湿量：开 = 1（全湿），关 = 0（直通）。逐帧趋近，约 15 ms 完成，
        // 避免开关瞬间的爆音；到 0 之后直接返回、一个样本都不改。
        let target = if settings.enabled { 1.0 } else { 0.0 };
        if (self.mix - target).abs() <= 1e-4 {
            self.mix = target;
        }
        if self.mix <= 0.0 && target <= 0.0 {
            // 完全直通（关闭且已淡出）：滤波器状态保持原样，下次淡入时会被干声盖住。
            return;
        }

        // 干声副本（混合用）。
        self.dry.resize(frames_len, 0.0);
        self.dry[..frames_len].copy_from_slice(&frames[..frames_len]);

        let profile = settings.preset.profile();
        let width = 1.0 + profile.widen * settings.amount;
        let bass_gain = db_to_linear(settings.bass_boost_db) - 1.0;
        let presence_gain = db_to_linear(profile.presence_db * settings.amount) - 1.0;
        let treble_gain = db_to_linear(profile.treble_db * settings.amount) - 1.0;

        for index in (0..frames_len).step_by(2) {
            let mut left = frames[index];
            let mut right = frames[index + 1];

            // 1) 低频拆中/侧：去掉低频侧声道（低频单声道化）。
            self.lp_l += self.bass_a * (left - self.lp_l);
            self.lp_r += self.bass_a * (right - self.lp_r);
            let low_mid = 0.5 * (self.lp_l + self.lp_r);
            let low_side = 0.5 * (self.lp_l - self.lp_r);
            left -= low_side;
            right += low_side;

            // 2) 中声道低频提升（低架）。
            if bass_gain != 0.0 {
                let boost = bass_gain * low_mid;
                left += boost;
                right += boost;
            }

            // 3) M/S 加宽。
            let mid = 0.5 * (left + right);
            let side = 0.5 * (left - right) * width;
            left = mid + side;
            right = mid - side;

            // 4) presence：预设频率的一极点高通提升。
            self.presence_lp_l += self.presence_a * (left - self.presence_lp_l);
            self.presence_lp_r += self.presence_a * (right - self.presence_lp_r);
            if presence_gain != 0.0 {
                left += presence_gain * (left - self.presence_lp_l);
                right += presence_gain * (right - self.presence_lp_r);
            }

            // 5) 高架：预设频率以上按 treble_db 提/降（丹拿是衰减，柔化高频）。
            self.treble_lp_l += self.treble_a * (left - self.treble_lp_l);
            self.treble_lp_r += self.treble_a * (right - self.treble_lp_r);
            if treble_gain != 0.0 {
                left += treble_gain * (left - self.treble_lp_l);
                right += treble_gain * (right - self.treble_lp_r);
            }

            // 6) 立体声联动软限幅（release 恒定推进，关掉时也会回到 1）。
            if settings.limiter {
                let peak = left.abs().max(right.abs());
                if peak > LIMITER_CEILING {
                    let target = LIMITER_CEILING / peak;
                    if target < self.limit_gain {
                        self.limit_gain = target; // 瞬时 attack
                    } else {
                        self.limit_gain += (target - self.limit_gain) * self.release_coeff;
                    }
                } else {
                    self.limit_gain += (1.0 - self.limit_gain) * self.release_coeff;
                }
                left *= self.limit_gain;
                right *= self.limit_gain;
            } else {
                self.limit_gain += (1.0 - self.limit_gain) * self.release_coeff;
            }

            let left = left.clamp(-1.0, 1.0);
            let right = right.clamp(-1.0, 1.0);

            // 干湿混合（逐帧推进，让开/关有 ~15 ms 的淡入淡出）。
            self.mix += (target - self.mix) * self.mix_coeff;
            if self.mix < 1e-4 {
                self.mix = 0.0;
            } else if self.mix > 1.0 - 1e-4 {
                self.mix = 1.0;
            }
            let mix = self.mix;
            frames[index] = (self.dry[index] + (left - self.dry[index]) * mix).clamp(-1.0, 1.0);
            frames[index + 1] =
                (self.dry[index + 1] + (right - self.dry[index + 1]) * mix).clamp(-1.0, 1.0);
        }
    }

    /// 是否还有未完成的渐变（关闭后淡出中）。
    ///
    /// 采集侧的快路径要等它归零才能直接搬 i16，否则会在淡出中途硬切一刀。
    pub fn is_active(&self) -> bool {
        self.mix > 0.0
    }
}

/// 一极点低通/高通的平滑系数（时间常数形式）：1 - exp(-1 / (tau × fs))。
/// 用于 15 ms 干湿渐变、50 ms 限幅 release 这类「按时间」的平滑。
fn one_pole_coeff(seconds: f32, rate: f32) -> f32 {
    if seconds <= 0.0 || rate <= 0.0 {
        return 1.0;
    }
    1.0 - (-1.0 / (seconds * rate)).exp()
}

/// 一极点滤波器的平滑系数（截止频率形式）：1 - exp(-2π × fc / fs)。
/// 用于低频单声道化、presence、高架的转折；不能拿频率当时间常数传（那样系数会趋近 0，滤波器等于没动）。
fn one_pole_coeff_hz(hz: f32, rate: f32) -> f32 {
    if hz <= 0.0 || rate <= 0.0 {
        return 1.0;
    }
    (1.0 - (-2.0 * std::f32::consts::PI * hz / rate).exp()).clamp(0.0, 1.0)
}

/// dB → 线性增益。
fn db_to_linear(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::PI;

    fn tone(rate: f32, hz: f32, frames: usize, amplitude: f32) -> Vec<f32> {
        let mut out = Vec::with_capacity(frames * 2);
        for index in 0..frames {
            let phase = 2.0 * PI * hz * index as f32 / rate;
            let sample = amplitude * phase.sin();
            out.push(sample);
            out.push(sample);
        }
        out
    }

    /// 左右幅度不同的 1 kHz 立体声（用于比较加宽量）。
    fn wide_input(frames: usize) -> Vec<f32> {
        let rate = 48_000.0;
        let mut out = Vec::with_capacity(frames * 2);
        for index in 0..frames {
            let phase = 2.0 * PI * 1_000.0 * index as f32 / rate;
            out.push(0.3 * phase.sin());
            out.push(0.15 * phase.sin());
        }
        out
    }

    fn side_energy(frames: &[f32]) -> f32 {
        frames
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| (0.5 * (pair[0] - pair[1])).powi(2))
            .sum()
    }

    fn rms(frames: &[f32]) -> f32 {
        let sum: f32 = frames.iter().map(|sample| sample * sample).sum();
        (sum / frames.len().max(1) as f32).sqrt()
    }

    fn peak(frames: &[f32]) -> f32 {
        frames
            .iter()
            .fold(0.0f32, |acc, sample| acc.max(sample.abs()))
    }

    /// 处理一段输入，先跑静音把干湿渐变推到全湿（比较音色前必须做）。
    fn render(preset: EffectPreset, amount: f32, bass_db: f32, input: &[f32]) -> Vec<f32> {
        let mut effect = StereoEffect::new(48_000);
        let settings = EffectSettings {
            enabled: true,
            preset,
            amount,
            bass_boost_db: bass_db,
            limiter: false,
        };
        let mut warmup = vec![0.0f32; 96_000];
        effect.process_interleaved(&mut warmup, &settings);
        let mut out = input.to_vec();
        effect.process_interleaved(&mut out, &settings);
        out
    }

    #[test]
    fn disabled_is_bit_exact_bypass() {
        let mut effect = StereoEffect::new(48_000);
        let settings = EffectSettings::default();
        let input = tone(48_000.0, 1_000.0, 4_800, 0.3);
        let mut frames = input.clone();
        for _ in 0..3 {
            effect.process_interleaved(&mut frames, &settings);
        }
        assert_eq!(frames, input, "关闭时必须一个样本都不改");
    }

    #[test]
    fn enabled_keeps_frame_count_and_finite() {
        let mut effect = StereoEffect::new(48_000);
        for preset in EffectPreset::ALL {
            let settings = EffectSettings {
                enabled: true,
                preset,
                ..Default::default()
            };
            let input = tone(48_000.0, 1_000.0, 4_800, 0.3);
            let mut frames = input.clone();
            effect.process_interleaved(&mut frames, &settings);
            assert_eq!(frames.len(), input.len());
            assert!(frames.iter().all(|sample| sample.is_finite()));
        }
    }

    #[test]
    fn side_energy_increases_with_amount() {
        let input = wide_input(48_000);
        let before = side_energy(&input);
        let out = render(EffectPreset::Dolby, 1.0, 0.0, &input);
        let after = side_energy(&out);
        assert!(
            after > before * 1.5,
            "加宽后侧声道能量应从 {before} 变大，实际 {after}"
        );
    }

    #[test]
    fn anti_phase_bass_is_monoized() {
        let rate = 48_000.0;
        let frames = 48_000;
        let mut input = Vec::with_capacity(frames * 2);
        for index in 0..frames {
            let sample = 0.3 * (2.0 * PI * 50.0 * index as f32 / rate).sin();
            input.push(sample);
            input.push(-sample);
        }
        let before = side_energy(&input);
        // 单独验证低频单声道化：关掉加宽（amount = 0），否则加宽会把去掉的低频侧又放大回来。
        let out = render(EffectPreset::Dolby, 0.0, 0.0, &input);
        let after = side_energy(&out);
        assert!(
            after < before * 0.2,
            "50 Hz 反相低频应被单声道化：前 {before}，后 {after}"
        );
    }

    #[test]
    fn limiter_keeps_peak_under_ceiling() {
        let mut effect = StereoEffect::new(48_000);
        let settings = EffectSettings {
            enabled: true,
            preset: EffectPreset::Harman,
            amount: 1.5,
            bass_boost_db: 12.0,
            limiter: true,
        };
        // 先用 1 秒静音把干湿渐变推到全湿（否则淡入期的干声会掩盖限幅效果）。
        let mut warmup = vec![0.0f32; 96_000];
        effect.process_interleaved(&mut warmup, &settings);
        let mut frames = tone(48_000.0, 80.0, 48_000, 1.4);
        effect.process_interleaved(&mut frames, &settings);
        let peak = peak(&frames);
        assert!(
            peak <= LIMITER_CEILING + 0.02,
            "限幅后峰值应贴近天花板 {LIMITER_CEILING}，实际 {peak}"
        );
    }

    #[test]
    fn silence_stays_silent_without_nan() {
        for preset in EffectPreset::ALL {
            let mut effect = StereoEffect::new(44_100);
            let settings = EffectSettings {
                enabled: true,
                preset,
                ..Default::default()
            };
            let mut frames = vec![0.0f32; 1_024];
            effect.process_interleaved(&mut frames, &settings);
            assert!(frames.iter().all(|sample| *sample == 0.0));
        }
    }

    #[test]
    fn zero_amount_and_bass_is_close_to_identity() {
        for preset in EffectPreset::ALL {
            let mut effect = StereoEffect::new(48_000);
            let settings = EffectSettings {
                enabled: true,
                preset,
                amount: 0.0,
                bass_boost_db: 0.0,
                limiter: false,
            };
            let input = tone(48_000.0, 1_000.0, 480, 0.2);
            let mut frames = input.clone();
            effect.process_interleaved(&mut frames, &settings);
            for (index, (out, src)) in frames.iter().zip(input.iter()).enumerate() {
                assert!(
                    (out - src).abs() <= 1e-3,
                    "预设 {} 第 {index} 个样本偏差过大",
                    preset.label()
                );
            }
        }
    }

    #[test]
    fn sanitized_clamps_bad_values() {
        let settings = EffectSettings {
            enabled: true,
            preset: EffectPreset::Harman,
            amount: 9.0,
            bass_boost_db: f32::NAN,
            limiter: true,
        }
        .sanitized();
        assert_eq!(settings.amount, MAX_AMOUNT);
        assert_eq!(settings.bass_boost_db, 4.0);
        assert_eq!(settings.preset, EffectPreset::Harman);
    }

    #[test]
    fn preset_ids_round_trip_and_labels_are_unique() {
        let mut labels = Vec::new();
        for (index, preset) in EffectPreset::ALL.iter().enumerate() {
            assert_eq!(EffectPreset::from_code(preset.code()), *preset);
            assert_eq!(EffectPreset::parse(preset.as_str()), Some(*preset));
            assert_eq!(EffectPreset::parse(preset.label()), Some(*preset));
            assert_eq!(preset.code() as usize, index, "code 必须与 ALL 顺序一致");
            labels.push(preset.label());
        }
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), EffectPreset::ALL.len(), "预设显示名不能重复");
        assert_eq!(EffectPreset::parse("nope"), None);
        assert_eq!(EffectPreset::parse("  HARMAN "), Some(EffectPreset::Harman));
        assert_eq!(
            EffectPreset::from_code(255),
            EffectPreset::default(),
            "未知 code 回退默认预设"
        );
    }

    #[test]
    fn preset_profiles_have_expected_shape() {
        let dolby = EffectPreset::Dolby.profile();
        let dynaudio = EffectPreset::Dynaudio.profile();
        let harman = EffectPreset::Harman.profile();
        assert!(dolby.widen > harman.widen && harman.widen > dynaudio.widen);
        assert!(dynaudio.treble_db < 0.0, "丹拿应柔化高频");
        assert!(harman.treble_db > 0.0, "哈曼卡顿应提亮高频");
        assert!(dynaudio.bass_mono_hz < dolby.bass_mono_hz);
        for preset in EffectPreset::ALL {
            let (_, _, limiter) = preset.recommended();
            assert!(limiter, "预设推荐值默认开软限幅");
        }
        assert_eq!(EffectPreset::Dolby.recommended(), (1.0, 4.0, true));
        assert_eq!(EffectPreset::Dynaudio.recommended(), (0.8, 2.5, true));
        assert_eq!(EffectPreset::Harman.recommended(), (1.0, 6.0, true));
    }

    #[test]
    fn width_follows_preset() {
        let input = wide_input(48_000);
        let dolby = side_energy(&render(EffectPreset::Dolby, 1.0, 0.0, &input));
        let harman = side_energy(&render(EffectPreset::Harman, 1.0, 0.0, &input));
        let dynaudio = side_energy(&render(EffectPreset::Dynaudio, 1.0, 0.0, &input));
        assert!(
            dolby > harman && harman > dynaudio,
            "加宽量应满足 类杜比 > 哈曼卡顿 > 丹拿：{dolby} / {harman} / {dynaudio}"
        );
    }

    #[test]
    fn dynaudio_tames_highs_and_harman_brightens() {
        let input = tone(48_000.0, 8_000.0, 48_000, 0.2);
        let base = rms(&input);
        let dolby = rms(&render(EffectPreset::Dolby, 1.0, 0.0, &input)) / base;
        let dynaudio = rms(&render(EffectPreset::Dynaudio, 1.0, 0.0, &input)) / base;
        let harman = rms(&render(EffectPreset::Harman, 1.0, 0.0, &input)) / base;
        assert!(
            dynaudio < dolby,
            "丹拿在 8 kHz 应比类杜比更暗：丹拿 {dynaudio:.3}，类杜比 {dolby:.3}"
        );
        assert!(
            harman > dolby,
            "哈曼卡顿在 8 kHz 应比类杜比更亮：哈曼 {harman:.3}，类杜比 {dolby:.3}"
        );
    }

    #[test]
    fn preset_switch_mid_stream_stays_finite() {
        let mut effect = StereoEffect::new(48_000);
        let mut input = tone(48_000.0, 440.0, 96_000, 0.4);
        let mut first = EffectSettings {
            enabled: true,
            preset: EffectPreset::Dolby,
            ..Default::default()
        };
        effect.process_interleaved(&mut input[..48_000], &first);
        first.preset = EffectPreset::Dynaudio;
        first.amount = 0.8;
        first.bass_boost_db = 2.5;
        effect.process_interleaved(&mut input[48_000..], &first);
        assert_eq!(effect.preset(), EffectPreset::Dynaudio);
        assert!(input.iter().all(|sample| sample.is_finite()));
        assert!(peak(&input) <= 1.0);
    }
}
