//! PCM 处理：`s16le` 交错 → RTP 需要的 `i32` 交错、静音补齐、按整包切分。
//!
//! 这里刻意不依赖任何协议实现，全部可离线单元测试。

/// 本 crate 对外承诺的输入格式：44.1 kHz / 2 声道 / 16 bit 小端 / 交错。
///
/// AirPlay 实时流在设备上的接受窗口以 44.1 kHz 计（见 [`crate::latency`]），所以格式固定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PcmFormat {
    /// 采样率（Hz）。
    pub sample_rate: u32,
    /// 声道数。
    pub channels: u8,
}

impl PcmFormat {
    /// 默认格式：44.1 kHz 立体声 16 bit。
    pub const S16LE_44100_STEREO: Self = Self {
        sample_rate: 44_100,
        channels: 2,
    };

    /// 每帧字节数（16 bit × 声道数）。
    #[must_use]
    pub const fn frame_bytes(&self) -> usize {
        self.channels as usize * 2
    }

    /// 每秒字节数。
    #[must_use]
    pub const fn bytes_per_second(&self) -> usize {
        self.sample_rate as usize * self.frame_bytes()
    }

    /// 字节数 → 帧数（向下取整）。
    #[must_use]
    pub const fn frames(&self, bytes: usize) -> usize {
        bytes / self.frame_bytes()
    }

    /// 帧数 → 字节数。
    #[must_use]
    pub const fn bytes_for_frames(&self, frames: usize) -> usize {
        frames * self.frame_bytes()
    }

    /// 字节数 → 毫秒。
    #[must_use]
    pub fn ms_for_bytes(&self, bytes: usize) -> f64 {
        bytes as f64 * 1000.0 / self.bytes_per_second() as f64
    }
}

/// `s16le` 交错字节 → `i32` 交错样本（协议层要求 `i32`，内部再转大端 16 bit）。
///
/// 末尾不足一个样本的字节被忽略（调用方保证流是帧对齐的，见 [`PacketChunker`]）。
#[must_use]
pub fn s16le_to_i32(bytes: &[u8]) -> Vec<i32> {
    bytes
        .chunks_exact(2)
        .map(|pair| i32::from(i16::from_le_bytes([pair[0], pair[1]])))
        .collect()
}

/// `frames` 帧静音（`channels` 声道交错）。
#[must_use]
pub fn silence_i32(frames: usize, channels: u8) -> Vec<i32> {
    vec![0i32; frames * usize::from(channels)]
}

/// 把任意大小的 PCM 字节流切成「恰好一个 RTP 包」的样本块。
///
/// RTP 包的帧数是固定的（44.1 kHz 实时流＝352 帧≈7.98 ms），而采集端给的块大小是任意的，
/// 所以必须在这里做对齐缓冲。缓冲上限由 [`PacketChunker::trim_to`] 控制：宁可丢最老的音频，
/// 也不让延迟随时间无界增长。
#[derive(Debug)]
pub struct PacketChunker {
    format: PcmFormat,
    frames_per_packet: usize,
    buf: Vec<u8>,
    dropped_bytes: u64,
}

impl PacketChunker {
    /// 新建一个按 `frames_per_packet` 切分的缓冲。
    #[must_use]
    pub fn new(format: PcmFormat, frames_per_packet: u32) -> Self {
        Self {
            format,
            frames_per_packet: frames_per_packet as usize,
            buf: Vec::with_capacity(format.bytes_for_frames(frames_per_packet as usize) * 4),
            dropped_bytes: 0,
        }
    }

    /// 一个 RTP 包需要的字节数。
    #[must_use]
    pub const fn packet_bytes(&self) -> usize {
        self.format.bytes_for_frames(self.frames_per_packet)
    }

    /// 当前缓冲的字节数。
    #[must_use]
    pub fn queued_bytes(&self) -> usize {
        self.buf.len()
    }

    /// 当前缓冲对应的毫秒数（我们自己这一侧的排队延迟）。
    #[must_use]
    pub fn queued_ms(&self) -> f64 {
        self.format.ms_for_bytes(self.buf.len())
    }

    /// 累计丢弃的字节数（缓冲超限时丢最老的音频）。
    #[must_use]
    pub const fn dropped_bytes(&self) -> u64 {
        self.dropped_bytes
    }

    /// 追加 PCM（任意长度）。
    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// 缓冲超过 `max_bytes` 时丢弃最老的音频（按整帧对齐），返回本次丢弃的字节数。
    pub fn trim_to(&mut self, max_bytes: usize) -> usize {
        if self.buf.len() <= max_bytes {
            return 0;
        }
        let excess = self.buf.len() - max_bytes;
        let frame = self.format.frame_bytes();
        let aligned = excess.div_ceil(frame) * frame;
        let aligned = aligned.min(self.buf.len());
        self.buf.drain(..aligned);
        self.dropped_bytes += aligned as u64;
        aligned
    }

    /// 取一个整包；不足一包返回 `None`（不补静音）。
    pub fn take_packet(&mut self) -> Option<Vec<i32>> {
        let need = self.packet_bytes();
        if self.buf.len() < need {
            return None;
        }
        let rest = self.buf.split_off(need);
        let packet = std::mem::replace(&mut self.buf, rest);
        Some(s16le_to_i32(&packet))
    }

    /// 取一个整包，不足的部分用静音补齐（欠载：采集端没跟上）。
    ///
    /// 补静音而不是等待，是为了让 RTP 时间轴保持连续——接收端按时间戳渲染，
    /// 时间轴一断就会打乱它的播放位置。返回 `(样本, 是否发生了欠载)`。
    pub fn take_packet_padded(&mut self) -> Option<(Vec<i32>, bool)> {
        let need = self.packet_bytes();
        if self.buf.is_empty() {
            return None;
        }
        if self.buf.len() >= need {
            return self.take_packet().map(|samples| (samples, false));
        }
        let mut packet = std::mem::take(&mut self.buf);
        packet.resize(need, 0);
        Some((s16le_to_i32(&packet), true))
    }
}

/// [`apply_gain_s16le`] 允许的最大增益（8× ≈ +18 dB）。
pub const MAX_GAIN: f32 = 8.0;

/// 对 `s16le` 交错 PCM 就地施加线性增益，**饱和**到 `i16` 范围（不回绕）。
///
/// 采集侧（WASAPI 回环）拿到的系统声音常常偏小，需要在上游补增益。放在这里是因为它必须和
/// 对 PCM 的其它处理（[`s16le_to_i32`]）用同一套格式假设，而且可以离线单测：
/// 回绕会变成刺耳的爆音，所以越界必须夹到 `i16` 边界。
///
/// # Errors
/// `gain` 非有限值或不在 `0.0..=[`MAX_GAIN`]` 时返回 [`crate::Error::Config`]；
/// 调用方一次校验、逐块调用即可（热路径上只有两次比较）。
pub fn apply_gain_s16le(pcm: &mut [u8], gain: f32) -> crate::Result<()> {
    if !gain.is_finite() || !(0.0..=MAX_GAIN).contains(&gain) {
        return Err(crate::Error::Config(format!(
            "gain={gain} 必须是 0.0..={MAX_GAIN} 的有限值"
        )));
    }
    if (gain - 1.0).abs() <= f32::EPSILON {
        return Ok(());
    }
    for sample in pcm.chunks_exact_mut(2) {
        let value = f32::from(i16::from_le_bytes([sample[0], sample[1]])) * gain;
        let clamped = value
            .round()
            .clamp(f32::from(i16::MIN), f32::from(i16::MAX)) as i16;
        sample.copy_from_slice(&clamped.to_le_bytes());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_arithmetic() {
        let f = PcmFormat::S16LE_44100_STEREO;
        assert_eq!(f.frame_bytes(), 4);
        assert_eq!(f.bytes_per_second(), 176_400);
        assert_eq!(f.bytes_for_frames(352), 1408);
        assert_eq!(f.frames(1408), 352);
        assert!((f.ms_for_bytes(1408) - 7.981).abs() < 0.01);
    }

    #[test]
    fn s16le_decodes_signed_samples_in_order() {
        // 1, -2, 32767, -32768 的小端字节
        let bytes = [0x01, 0x00, 0xFE, 0xFF, 0xFF, 0x7F, 0x00, 0x80];
        assert_eq!(s16le_to_i32(&bytes), vec![1, -2, 32_767, -32_768]);
    }

    #[test]
    fn silence_has_one_sample_per_channel_per_frame() {
        assert_eq!(silence_i32(3, 2), vec![0; 6]);
        assert_eq!(silence_i32(0, 2).len(), 0);
    }

    #[test]
    fn chunker_aligns_oddly_sized_writes() {
        let f = PcmFormat::S16LE_44100_STEREO;
        let mut c = PacketChunker::new(f, 4); // 每包 4 帧 = 16 字节
        c.push(&[1, 2, 3]); // 不足一帧
        assert!(c.take_packet().is_none());
        assert_eq!(c.queued_bytes(), 3);
        c.push(&[4; 29]); // 共 32 字节 = 恰好两包
        let first = c.take_packet().expect("第一包");
        assert_eq!(first.len(), 4 * 2);
        let second = c.take_packet().expect("第二包");
        assert_eq!(second.len(), 4 * 2);
        assert!(c.take_packet().is_none());
        assert_eq!(c.queued_bytes(), 0);
    }

    #[test]
    fn padded_packet_reports_underrun_and_keeps_timeline() {
        let f = PcmFormat::S16LE_44100_STEREO;
        let mut c = PacketChunker::new(f, 4);
        assert!(c.take_packet_padded().is_none(), "空缓冲不该产出包");
        // 0x7FFF（小端 FF 7F）重复 4 次 = 2 帧立体声
        c.push(&[0xFF, 0x7F, 0xFF, 0x7F, 0xFF, 0x7F, 0xFF, 0x7F]);
        let (samples, underrun) = c.take_packet_padded().expect("补齐后的包");
        assert!(underrun, "不足一包必须报告欠载");
        assert_eq!(samples.len(), 4 * 2, "补到整包帧数");
        assert_eq!(&samples[..4], &[32_767, 32_767, 32_767, 32_767]);
        assert_eq!(&samples[4..], &[0, 0, 0, 0], "缺的部分是静音");
        assert_eq!(c.queued_bytes(), 0);
    }

    #[test]
    fn trim_drops_oldest_audio_on_frame_boundary() {
        let f = PcmFormat::S16LE_44100_STEREO;
        let mut c = PacketChunker::new(f, 4);
        c.push(&[0xAA; 40]); // 10 帧
        assert_eq!(c.queued_bytes(), 40);
        let dropped = c.trim_to(16);
        assert_eq!(dropped, 24, "多出的 24 字节按整帧丢弃");
        assert_eq!(c.queued_bytes(), 16);
        assert_eq!(c.dropped_bytes(), 24);
        assert_eq!(c.trim_to(16), 0, "未超限不丢");
    }

    #[test]
    fn queued_ms_tracks_buffer_depth() {
        let f = PcmFormat::S16LE_44100_STEREO;
        let mut c = PacketChunker::new(f, 352);
        c.push(&vec![0u8; f.bytes_per_second() / 10]); // 100 ms
        assert!((c.queued_ms() - 100.0).abs() < 0.01);
    }
}
