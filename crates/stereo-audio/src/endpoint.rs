//! 端点描述类型（纯数据，跨平台可编译，可离线单测）。

use std::fmt;

use crate::convert::SampleFormat;

/// 数据流方向。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataFlow {
    /// 播放端点（扬声器 / 耳机）。回环采集就是采它的输出。
    Render,
    /// 采集端点（麦克风 / 立体声混音 / 虚拟声卡）。
    Capture,
}

impl DataFlow {
    /// `"render"` / `"capture"` 字符串形式。
    pub fn as_str(self) -> &'static str {
        match self {
            DataFlow::Render => "render",
            DataFlow::Capture => "capture",
        }
    }
}

impl fmt::Display for DataFlow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 采集输出格式（对外契约是 44100/2ch/16bit）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(missing_docs)] // 字段名即语义（samplerate/channels/bits）
pub struct PcmFormat {
    pub samplerate: u32,
    pub channels: u16,
    pub bits: u16,
}

impl PcmFormat {
    /// 构造：采样率 / 声道数 / 位深。
    pub const fn new(samplerate: u32, channels: u16, bits: u16) -> Self {
        Self {
            samplerate,
            channels,
            bits,
        }
    }

    /// 一帧（所有声道）的字节数。
    pub fn frame_bytes(&self) -> usize {
        self.channels as usize * (self.bits as usize).div_ceil(8)
    }

    /// 每秒字节数（= 采样率 × 帧长）。
    pub fn bytes_per_second(&self) -> u32 {
        self.samplerate * self.frame_bytes() as u32
    }

    /// 指定时长对应多少字节。
    pub fn bytes_for_seconds(&self, seconds: f32) -> usize {
        (self.bytes_per_second() as f64 * seconds.max(0.0) as f64) as usize
    }
}

impl fmt::Display for PcmFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} Hz / {} ch / {}-bit {}{}",
            self.samplerate,
            self.channels,
            self.bits,
            if self.bits == 32 { "float " } else { "" },
            if self.bits == 32 { "" } else { "PCM" }
        )
    }
}

/// 端点的混音格式（WASAPI `GetMixFormat` 的结果）。
#[derive(Debug, Clone, PartialEq)]
#[allow(missing_docs)] // 与 WAVEFORMATEX 字段一一对应，字段名保留原生拼写
pub struct FormatInfo {
    /// `None` 表示是我们没见过的格式（既不是 PCM 也不是 IEEE float）。
    pub sample_format: Option<SampleFormat>,
    pub channels: u16,
    pub samplerate: u32,
    pub bits: u16,
    pub block_align: u16,
    pub avg_bytes_per_sec: u32,
    pub raw_format_tag: u16,
    /// 是否 WAVE_FORMAT_EXTENSIBLE（0xFFFE）。
    pub is_extensible: bool,
}

impl FormatInfo {
    /// 一帧字节数（= `nBlockAlign`）。
    pub fn frame_bytes(&self) -> usize {
        self.block_align as usize
    }

    /// 每秒字节数（= `nAvgBytesPerSec`）。
    pub fn bytes_per_second(&self) -> u32 {
        self.avg_bytes_per_sec
    }

    /// 关键字段是否都有效（能安全用于格式协商）。
    pub fn is_valid(&self) -> bool {
        self.channels > 0 && self.samplerate > 0 && self.block_align > 0
    }

    /// 人读描述，例如 `48000 Hz / 2 ch / 32-bit float (extensible)`。
    pub fn describe(&self) -> String {
        let kind = match (self.sample_format, self.is_extensible) {
            (Some(SampleFormat::F32), _) => "float",
            (Some(_), _) => "int",
            (None, _) => "unknown",
        };
        format!(
            "{} Hz / {} ch / {}-bit {} (tag=0x{:04X}{}{})",
            self.samplerate,
            self.channels,
            self.bits,
            kind,
            self.raw_format_tag,
            if self.is_extensible { ", extensible" } else { "" },
            self.sample_format
                .map(|s| format!(", {:?}", s))
                .unwrap_or_default()
        )
    }
}

impl fmt::Display for FormatInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.describe())
    }
}

/// 一个音频端点的信息。
#[derive(Debug, Clone, PartialEq)]
#[allow(missing_docs)] // id/name/flow/is_default/mix_format/error 均为直读字段
pub struct EndpointInfo {
    pub id: String,
    pub name: String,
    pub flow: DataFlow,
    pub is_default: bool,
    /// 激活失败时为 `None`（`error` 里放原因），不影响列出其它端点。
    pub mix_format: Option<FormatInfo>,
    pub error: Option<String>,
}

impl EndpointInfo {
    /// 人读一行，例如 `[render 默认] 耳机 (Chu2 DSP) | 48000 Hz / 2 ch / ... | {0.0.0.0}`。
    pub fn describe(&self) -> String {
        format!(
            "[{}{}] {} | {} | {}",
            self.flow,
            if self.is_default { " 默认" } else { "" },
            self.name,
            self.mix_format
                .as_ref()
                .map(|m| m.describe())
                .unwrap_or_else(|| "<无格式>".to_string()),
            self.id
        )
    }
}

impl fmt::Display for EndpointInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.describe())
    }
}

/// 设备选择器：按友好名子串或完整 device id。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum DeviceSelector {
    /// 系统默认端点（render 用 eConsole 角色）。
    #[default]
    Default,
    /// 友好名**子串**匹配（大小写不敏感），例如 `"Chu2"`、`"扬声器"`。
    NameContains(String),
    /// 完整端点 id（`list_endpoints()` 返回的 `id`）。
    Id(String),
}

impl DeviceSelector {
    /// 按友好名子串选择（大小写不敏感），例如 `"Chu2"`、`"扬声器"`。
    pub fn name_contains(s: impl Into<String>) -> Self {
        DeviceSelector::NameContains(s.into())
    }

    /// 按完整端点 id 选择（[`EndpointInfo::id`] 的值）。
    pub fn id(s: impl Into<String>) -> Self {
        DeviceSelector::Id(s.into())
    }

    /// 是否匹配某个端点。
    pub fn matches(&self, name: &str, id: &str) -> bool {
        match self {
            DeviceSelector::Default => false,
            DeviceSelector::NameContains(pat) => {
                let pat = pat.trim().to_lowercase();
                !pat.is_empty() && name.to_lowercase().contains(&pat)
            }
            DeviceSelector::Id(want) => want == id,
        }
    }

    /// 人读描述（出错信息里会用到）。
    pub fn describe(&self) -> String {
        match self {
            DeviceSelector::Default => "默认端点".to_string(),
            DeviceSelector::NameContains(p) => format!("名字含 \"{p}\""),
            DeviceSelector::Id(i) => format!("id={i}"),
        }
    }
}

impl fmt::Display for DeviceSelector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.describe())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pcm_format_byte_math() {
        let f = PcmFormat::new(44_100, 2, 16);
        assert_eq!(f.frame_bytes(), 4);
        assert_eq!(f.bytes_per_second(), 176_400);
        assert_eq!(f.bytes_for_seconds(5.0), 882_000);
        assert_eq!(PcmFormat::new(48_000, 1, 24).frame_bytes(), 3);
        assert!(f.to_string().contains("44100 Hz / 2 ch"));
    }

    #[test]
    fn selector_matches_name_case_insensitively() {
        let sel = DeviceSelector::name_contains("chu2");
        assert!(sel.matches("耳机 (Chu2 DSP)", "{0.0.0.00000000}.{abc}"));
        assert!(sel.matches("CHU2", "x"));
        assert!(!sel.matches("扬声器 (Realtek)", "x"));
        // 空模式不应匹配任何东西（避免误选第一个设备）
        assert!(!DeviceSelector::name_contains("  ").matches("anything", "anything"));
    }

    #[test]
    fn selector_matches_full_id_exactly() {
        let sel = DeviceSelector::id("{0.0.0.00000000}.{dead-beef}");
        assert!(sel.matches("任意名字", "{0.0.0.00000000}.{dead-beef}"));
        assert!(!sel.matches("任意名字", "{0.0.0.00000000}.{other}"));
        assert!(!DeviceSelector::Default.matches("任意名字", "any"));
    }

    #[test]
    fn format_info_describe() {
        let fi = FormatInfo {
            sample_format: Some(SampleFormat::F32),
            channels: 2,
            samplerate: 48_000,
            bits: 32,
            block_align: 8,
            avg_bytes_per_sec: 384_000,
            raw_format_tag: 0xFFFE,
            is_extensible: true,
        };
        assert!(fi.is_valid());
        assert_eq!(fi.frame_bytes(), 8);
        let d = fi.describe();
        assert!(d.contains("48000 Hz"), "{d}");
        assert!(d.contains("float"), "{d}");
        assert!(d.contains("extensible"), "{d}");
    }
}
