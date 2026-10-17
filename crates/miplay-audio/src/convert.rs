//! PCM 采样格式转换 + 流式线性重采样（全部纯函数，可离线单测）。
//!
//! 采集链路的对外契约是 **s16le 交错**；WASAPI 混音格式通常是 f32（本机是
//! 48000/2ch/32f），所以这里提供 f32 ↔ s16 的转换，以及在 WASAPI 不接受
//! 44100 时用的流式重采样器（48k → 44.1k）。

use crate::endpoint::PcmFormat;
use crate::error::{AudioError, Result};

/// 支持的 PCM 采样格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(missing_docs)] // S16/S24/S32/F32 即格式本身
pub enum SampleFormat {
    S16,
    S24,
    S32,
    F32,
}

impl SampleFormat {
    /// 单个采样的字节数。
    pub fn bytes_per_sample(self) -> usize {
        match self {
            SampleFormat::S16 => 2,
            SampleFormat::S24 => 3,
            SampleFormat::S32 | SampleFormat::F32 => 4,
        }
    }

    /// 位深（= 字节数 × 8）。
    pub fn bits(self) -> u16 {
        (self.bytes_per_sample() * 8) as u16
    }

    /// 是否是浮点格式。
    pub fn is_float(self) -> bool {
        matches!(self, SampleFormat::F32)
    }

    /// 按格式把整块字节解码成 f32（标称 -1.0..1.0）。
    pub fn decode_into(self, bytes: &[u8], out: &mut Vec<f32>) -> Result<()> {
        let bps = self.bytes_per_sample();
        if bytes.len() % bps != 0 {
            return Err(AudioError::invalid(format!(
                "{:?} 数据长度 {} 不是采样字节数 {} 的整数倍",
                self,
                bytes.len(),
                bps
            )));
        }
        out.reserve(bytes.len() / bps);
        match self {
            SampleFormat::S16 => {
                for c in bytes.chunks_exact(2) {
                    out.push(s16_to_f32(i16::from_le_bytes([c[0], c[1]])));
                }
            }
            SampleFormat::S24 => {
                for c in bytes.chunks_exact(3) {
                    let v = (c[0] as i32) | ((c[1] as i32) << 8) | ((c[2] as i32) << 16);
                    let v = (v << 8) >> 8; // 符号扩展到 32 位
                    out.push(v as f32 / 8_388_608.0);
                }
            }
            SampleFormat::S32 => {
                for c in bytes.chunks_exact(4) {
                    let v = i32::from_le_bytes([c[0], c[1], c[2], c[3]]);
                    out.push(v as f32 / 2_147_483_648.0);
                }
            }
            SampleFormat::F32 => {
                for c in bytes.chunks_exact(4) {
                    out.push(f32::from_le_bytes([c[0], c[1], c[2], c[3]]));
                }
            }
        }
        Ok(())
    }

    /// 按格式把 f32 编码成字节，追加到 `out`。
    pub fn encode_into(self, samples: &[f32], out: &mut Vec<u8>) {
        let bps = self.bytes_per_sample();
        out.reserve(samples.len() * bps);
        match self {
            SampleFormat::S16 => {
                for &s in samples {
                    out.extend_from_slice(&f32_to_s16(s).to_le_bytes());
                }
            }
            SampleFormat::S24 => {
                for &s in samples {
                    let v = (s.clamp(-1.0, 1.0) * 8_388_607.0).round() as i32;
                    let b = v.to_le_bytes();
                    out.extend_from_slice(&b[0..3]);
                }
            }
            SampleFormat::S32 => {
                for &s in samples {
                    let v = (s.clamp(-1.0, 1.0) * 2_147_483_647.0).round() as i32;
                    out.extend_from_slice(&v.to_le_bytes());
                }
            }
            SampleFormat::F32 => {
                for &s in samples {
                    out.extend_from_slice(&s.to_le_bytes());
                }
            }
        }
    }
}

/// f32（标称 -1.0..1.0）→ i16，饱和处理。
#[inline]
pub fn f32_to_s16(x: f32) -> i16 {
    (x.clamp(-1.0, 1.0) * 32_767.0).round() as i16
}

/// i16 → f32（与 [`f32_to_s16`] 互为近似逆运算）。
#[inline]
pub fn s16_to_f32(x: i16) -> f32 {
    x as f32 / 32_768.0
}

/// 把 f32 样本编码成 s16le 字节，追加到 `out`。
pub fn encode_s16le_into(samples: &[f32], out: &mut Vec<u8>) {
    SampleFormat::S16.encode_into(samples, out);
}

/// 把 s16le 字节解码成 f32 样本。
pub fn decode_s16le_into(bytes: &[u8], out: &mut Vec<f32>) -> Result<()> {
    SampleFormat::S16.decode_into(bytes, out)
}

/// 交错声道重映射：`from` → `to`。
///
/// * `from == 1`：单声道复制到所有输出声道；
/// * `to == 1`：所有输入声道平均（mixdown）；
/// * 其余情况：按顺序取前 `to` 个声道，不足补 0，多余的丢弃。
pub fn remap_channels(input: &[f32], from: usize, to: usize) -> Vec<f32> {
    assert!(from > 0 && to > 0, "声道数必须大于 0");
    if from == to {
        return input.to_vec();
    }
    let frames = input.len() / from;
    let mut out = Vec::with_capacity(frames * to);
    for f in 0..frames {
        let src = &input[f * from..f * from + from];
        if to == 1 {
            let sum: f32 = src.iter().sum();
            out.push(sum / from as f32);
        } else if from == 1 {
            for _ in 0..to {
                out.push(src[0]);
            }
        } else {
            for c in 0..to {
                out.push(*src.get(c).unwrap_or(&0.0));
            }
        }
    }
    out
}

/// 流式线性插值重采样器（可跨 chunk 调用，内部保留相位与上一帧）。
///
/// 质量足够「把系统声音送到音箱」这个场景；不是带限重采样，48k→44.1k 的
/// 镜像分量在 -30 dB 量级，实测正弦峰值/RMS 误差 < 0.5%。
#[derive(Debug, Clone)]
pub struct StreamResampler {
    from_rate: u32,
    to_rate: u32,
    channels: usize,
    step: f64,
    t: f64,
    last: Vec<f32>,
    started: bool,
}

impl StreamResampler {
    /// 建一个流向重采样器（采样率相同则是 passthrough）。
    pub fn new(from_rate: u32, to_rate: u32, channels: usize) -> Self {
        assert!(from_rate > 0 && to_rate > 0, "采样率必须大于 0");
        assert!(channels > 0, "声道数必须大于 0");
        Self {
            from_rate,
            to_rate,
            channels,
            step: from_rate as f64 / to_rate as f64,
            t: 0.0,
            last: vec![0.0; channels],
            started: false,
        }
    }

    /// 是否只是原样直通（采样率相同）。
    pub fn is_passthrough(&self) -> bool {
        self.from_rate == self.to_rate
    }

    /// 每个输出帧对应的输入帧数（= from/to）。
    pub fn step(&self) -> f64 {
        self.step
    }

    /// 处理一块交错 f32 输入，输出追加到 `out`。
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        let ch = self.channels;
        if input.is_empty() {
            return;
        }
        let frames = input.len() / ch;
        if frames == 0 {
            return;
        }
        if self.is_passthrough() {
            out.extend_from_slice(&input[..frames * ch]);
            self.last.copy_from_slice(&input[(frames - 1) * ch..frames * ch]);
            self.started = true;
            return;
        }
        let mut t = self.t;
        while t < frames as f64 {
            let i0f = t.floor();
            let frac = (t - i0f) as f32;
            let i0 = i0f as isize;
            if i0 < 0 {
                if !self.started {
                    break;
                }
                for c in 0..ch {
                    let a = self.last[c];
                    let b = input[c];
                    out.push(a + (b - a) * frac);
                }
            } else {
                let i = i0 as usize;
                if i + 1 >= frames {
                    // 需要下一块的第一个帧才能插值，保留相位，等下次调用
                    break;
                }
                for c in 0..ch {
                    let a = input[i * ch + c];
                    let b = input[(i + 1) * ch + c];
                    out.push(a + (b - a) * frac);
                }
            }
            t += self.step;
        }
        self.t = t - frames as f64;
        self.last
            .copy_from_slice(&input[(frames - 1) * ch..frames * ch]);
        self.started = true;
    }
}

/// 一次性重采样（内部建临时 [`StreamResampler`]，测试与批处理用）。
pub fn resample_f32(input: &[f32], channels: usize, from_rate: u32, to_rate: u32) -> Vec<f32> {
    let mut rs = StreamResampler::new(from_rate, to_rate, channels);
    let mut out = Vec::new();
    rs.process(input, &mut out);
    out
}

/// 通道交错 → 拆成「每声道一条连续数组」（测试 / 分析用）。
pub fn deinterleave(input: &[f32], channels: usize) -> Vec<Vec<f32>> {
    let mut out = vec![Vec::with_capacity(input.len() / channels.max(1)); channels];
    for (i, s) in input.iter().enumerate() {
        out[i % channels].push(*s);
    }
    out
}

/// 生成一段 s16le 交错正弦 PCM（相位从 0 开始）。
///
/// 真机验证用：播放它、回环采回来，峰值/RMS/频率都能直接对照。
pub fn build_sine_s16(freq_hz: f64, seconds: f64, amplitude: f32, format: PcmFormat) -> Vec<u8> {
    let frames = (format.samplerate as f64 * seconds.max(0.0)) as usize;
    let amp = amplitude.clamp(0.0, 1.0) as f64;
    let channels = format.channels.max(1) as usize;
    let mut samples = Vec::with_capacity(frames * channels);
    for i in 0..frames {
        let s =
            (amp * (2.0 * std::f64::consts::PI * freq_hz * i as f64 / format.samplerate as f64).sin())
                as f32;
        for _ in 0..channels {
            samples.push(s);
        }
    }
    let mut out = Vec::with_capacity(samples.len() * (format.bits as usize / 8));
    SampleFormat::S16.encode_into(&samples, &mut out);
    out
}

/// 用滞回过零法估计主频（Hz）。返回 `None` 表示样本太少或几乎没有信号。
pub fn estimate_frequency(samples: &[f32], rate: u32) -> Option<f64> {
    if samples.len() < 16 || rate == 0 {
        return None;
    }
    let peak = samples.iter().fold(0f32, |m, s| m.max(s.abs()));
    if peak <= 1e-6 {
        return None;
    }
    let thresh = peak * 0.25;
    let mut state = 0i32;
    let mut first = 0usize;
    let mut last = 0usize;
    let mut crossings = 0usize;
    for (i, &s) in samples.iter().enumerate() {
        if state <= 0 && s > thresh {
            state = 1;
            if crossings == 0 {
                first = i;
            } else {
                last = i;
            }
            crossings += 1;
        } else if state >= 0 && s < -thresh {
            state = -1;
        }
    }
    if crossings < 2 || last <= first {
        return None;
    }
    Some((crossings - 1) as f64 * rate as f64 / (last - first) as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(freq: f64, rate: u32, seconds: f64, amplitude: f32) -> Vec<f32> {
        let n = (rate as f64 * seconds) as usize;
        (0..n)
            .map(|i| {
                amplitude
                    * (2.0 * std::f64::consts::PI * freq * i as f64 / rate as f64).sin() as f32
            })
            .collect()
    }

    #[test]
    fn f32_to_s16_saturates_and_rounds() {
        assert_eq!(f32_to_s16(0.0), 0);
        assert_eq!(f32_to_s16(1.0), 32_767);
        assert_eq!(f32_to_s16(-1.0), -32_767);
        assert_eq!(f32_to_s16(2.0), 32_767);
        assert_eq!(f32_to_s16(-2.0), -32_767);
        assert_eq!(f32_to_s16(0.5), 16_384);
        assert_eq!(f32_to_s16(-0.5), -16_384);
        assert_eq!(f32_to_s16(4.0 / 32_767.0), 4);
    }

    #[test]
    fn s16_full_scale_maps_to_nominal_range() {
        assert!((s16_to_f32(i16::MIN) + 1.0).abs() < 1e-9);
        assert!((s16_to_f32(i16::MAX) - 0.999_969_5).abs() < 1e-6);
    }

    #[test]
    fn s16_encode_decode_roundtrip_within_one_lsb() {
        let src = sine(440.0, 44_100, 0.05, 0.8);
        let mut bytes = Vec::new();
        encode_s16le_into(&src, &mut bytes);
        assert_eq!(bytes.len(), src.len() * 2);
        let mut back = Vec::new();
        decode_s16le_into(&bytes, &mut back).unwrap();
        assert_eq!(back.len(), src.len());
        let max_err = src
            .iter()
            .zip(&back)
            .map(|(a, b)| (a - b).abs())
            .fold(0f32, f32::max);
        assert!(max_err <= 2.0 / 32_768.0, "max_err={max_err}");
    }

    #[test]
    fn decode_rejects_ragged_byte_counts() {
        let mut out = Vec::new();
        assert!(SampleFormat::S16.decode_into(&[0, 1, 2], &mut out).is_err());
        assert!(SampleFormat::S24.decode_into(&[0, 1, 2, 3], &mut out).is_err());
        assert!(SampleFormat::F32.decode_into(&[0, 1, 2], &mut out).is_err());
    }

    #[test]
    fn s24_and_s32_extremes() {
        let mut out = Vec::new();
        SampleFormat::S24
            .decode_into(&[0x00, 0x00, 0x80], &mut out)
            .unwrap();
        assert!((out[0] + 1.0).abs() < 1e-9, "{:?}", out);
        out.clear();
        SampleFormat::S24
            .decode_into(&[0xFF, 0xFF, 0x7F], &mut out)
            .unwrap();
        assert!((out[0] - 0.999_999_9).abs() < 1e-6);
        out.clear();
        SampleFormat::S32
            .decode_into(&i32::MIN.to_le_bytes(), &mut out)
            .unwrap();
        assert!((out[0] + 1.0).abs() < 1e-9);
    }

    #[test]
    fn f32_bytes_are_little_endian() {
        let mut out = Vec::new();
        SampleFormat::F32
            .decode_into(&0.5f32.to_le_bytes(), &mut out)
            .unwrap();
        assert_eq!(out, vec![0.5]);
        let mut enc = Vec::new();
        SampleFormat::F32.encode_into(&[-0.25], &mut enc);
        assert_eq!(enc, (-0.25f32).to_le_bytes().to_vec());
    }

    #[test]
    fn resampler_preserves_frequency_and_amplitude() {
        let src = sine(440.0, 48_000, 2.0, 0.5);
        let out = resample_f32(&src, 1, 48_000, 44_100);
        let expected_len = (src.len() as f64 * 44_100.0 / 48_000.0) as usize;
        assert!(
            (out.len() as i64 - expected_len as i64).abs() <= 2,
            "out={} expected={}",
            out.len(),
            expected_len
        );
        let freq = estimate_frequency(&out, 44_100).expect("freq");
        assert!((freq - 440.0).abs() < 1.0, "freq={freq}");
        let peak = out.iter().fold(0f32, |m, s| m.max(s.abs()));
        assert!((peak - 0.5).abs() < 0.01, "peak={peak}");
        let rms = (out.iter().map(|s| (s * s) as f64).sum::<f64>() / out.len() as f64).sqrt();
        let expected_rms = 0.5 / std::f64::consts::SQRT_2;
        assert!(
            (rms - expected_rms).abs() / expected_rms < 0.01,
            "rms={rms} expected={expected_rms}"
        );
    }

    #[test]
    fn resampler_is_chunk_invariant() {
        let src = sine(1000.0, 48_000, 0.5, 0.9);
        let whole = resample_f32(&src, 1, 48_000, 44_100);
        let mut rs = StreamResampler::new(48_000, 44_100, 1);
        let mut chunked = Vec::new();
        for chunk in src.chunks(1024 * 3) {
            rs.process(chunk, &mut chunked);
        }
        assert_eq!(whole.len(), chunked.len());
        let max_diff = whole
            .iter()
            .zip(&chunked)
            .map(|(a, b)| (a - b).abs())
            .fold(0f32, f32::max);
        assert!(max_diff < 1e-6, "max_diff={max_diff}");
    }

    #[test]
    fn resampler_passthrough_is_identity() {
        let src = sine(200.0, 44_100, 0.1, 0.3);
        let out = resample_f32(&src, 1, 44_100, 44_100);
        assert_eq!(out, src);
        assert!(StreamResampler::new(44_100, 44_100, 1).is_passthrough());
    }

    #[test]
    fn resampler_upsamples_too() {
        let src = sine(300.0, 22_050, 1.0, 0.4);
        let out = resample_f32(&src, 1, 22_050, 44_100);
        assert!((out.len() as i64 - 44_100).abs() <= 2);
        let freq = estimate_frequency(&out, 44_100).unwrap();
        assert!((freq - 300.0).abs() < 1.0, "freq={freq}");
    }

    #[test]
    fn stereo_resampler_keeps_channels_independent() {
        let l = sine(440.0, 48_000, 0.5, 0.5);
        let r = sine(880.0, 48_000, 0.5, 0.5);
        let mut inter = Vec::new();
        for (a, b) in l.iter().zip(&r) {
            inter.push(*a);
            inter.push(*b);
        }
        let out = resample_f32(&inter, 2, 48_000, 44_100);
        let ch = deinterleave(&out, 2);
        let fl = estimate_frequency(&ch[0], 44_100).unwrap();
        let fr = estimate_frequency(&ch[1], 44_100).unwrap();
        assert!((fl - 440.0).abs() < 1.5, "fl={fl}");
        assert!((fr - 880.0).abs() < 2.0, "fr={fr}");
    }

    #[test]
    fn remap_mono_to_stereo_and_mixdown() {
        let mono = vec![0.5, -0.5];
        assert_eq!(remap_channels(&mono, 1, 2), vec![0.5, 0.5, -0.5, -0.5]);
        let stereo = vec![1.0, 0.0, 0.0, 1.0];
        assert_eq!(remap_channels(&stereo, 2, 1), vec![0.5, 0.5]);
        assert_eq!(remap_channels(&stereo, 2, 2), stereo);
        // 6ch → 2ch 取前两个声道
        let six = vec![1.0, 0.5, 0.2, 0.1, 0.0, 0.0];
        assert_eq!(remap_channels(&six, 6, 2), vec![1.0, 0.5]);
    }

    #[test]
    fn estimate_frequency_rejects_silence() {
        assert!(estimate_frequency(&[0.0; 128], 44_100).is_none());
        assert!(estimate_frequency(&[0.0; 4], 44_100).is_none());
    }

    #[test]
    fn build_sine_s16_has_expected_peak_rms_and_frequency() {
        let format = PcmFormat::new(44_100, 2, 16);
        let pcm = build_sine_s16(1_000.0, 1.0, 0.4, format);
        assert_eq!(pcm.len(), 44_100 * 4);
        let samples: Vec<i16> = pcm
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect();
        // 左右声道交错且相同
        let left: Vec<i16> = samples.iter().step_by(2).copied().collect();
        let right: Vec<i16> = samples.iter().skip(1).step_by(2).copied().collect();
        assert_eq!(left, right);
        let peak = left.iter().map(|s| (*s as i32).abs()).max().unwrap();
        let expected_peak = (0.4f64 * 32_767.0).round() as i32;
        assert!((peak - expected_peak).abs() <= 1, "peak={peak}");
        let rms = (left.iter().map(|s| (*s as f64) * (*s as f64)).sum::<f64>() / left.len() as f64)
            .sqrt();
        let expected_rms = 0.4 / std::f64::consts::SQRT_2 * 32_767.0;
        assert!((rms - expected_rms).abs() / expected_rms < 0.01, "rms={rms}");
        let as_f32: Vec<f32> = left.iter().map(|s| *s as f32 / 32_768.0).collect();
        let freq = estimate_frequency(&as_f32, 44_100).unwrap();
        assert!((freq - 1_000.0).abs() < 0.5, "freq={freq}");
    }

    #[test]
    fn build_sine_s16_clamps_amplitude_and_handles_zero_length() {
        let format = PcmFormat::new(8_000, 1, 16);
        let pcm = build_sine_s16(440.0, 0.0, 2.0, format);
        assert!(pcm.is_empty());
        let pcm = build_sine_s16(440.0, 0.01, 2.0, format);
        assert_eq!(pcm.len(), 80 * 2);
        let peak = pcm
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]) as i32)
            .fold(0, |m, s| m.max(s.abs()));
        assert!(peak <= 32_767);
    }
}
