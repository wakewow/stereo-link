//! 按设备**真实能力**枚举输出格式（`IAudioClient::IsFormatSupported`，仅 Windows）。
//!
//! 共享模式（`AUDCLNT_SHAREMODE_SHARED`）下音频引擎几乎只接受端点自己的混音格式，
//! 用它列下拉框会把设备其实放不了的格式也列出来；所以判断「设备到底能播哪些
//! 采样率 / 声道 / 位深」必须用**独占模式**（`AUDCLNT_SHAREMODE_EXCLUSIVE`）
//! 逐个候选格式探测：
//!
//! * 独占：`S_OK` = 设备真正支持；`S_FALSE` 或其它的错误 HRESULT = 不支持。
//! * 共享：`S_OK` 或 `S_FALSE` 都算「可用」（`S_FALSE` 表示引擎会用最接近的格式
//!   重采样，也就是配合 `AUTOCONVERTPCM` 能出声）。共享模式探测时
//!   `IsFormatSupported` 的 `ppClosestMatch` 必须非空，否则返回 `E_POINTER`。
//!
//! 候选矩阵见 [`CANDIDATE_SAMPLERATES`] × [`CANDIDATE_CHANNELS`] ×
//! [`CANDIDATE_BIT_DEPTHS`]（共 90 个）。24 bit PCM 按 WAVEFORMATEXTENSIBLE
//! 惯例装进 32-bit 容器（`wBitsPerSample = 32`、`wValidBitsPerSample = 24`），
//! 32 bit 只探测 IEEE float。
//!
//! 用法（`endpoint_id` 可以是端点 id，也可以是友好名子串，或 `"default"`）：
//!
//! ```text
//! let formats = stereo_audio::format_support::supported_formats("<音箱 A 名字子串>")?;
//! println!("{}", stereo_audio::format_support::supported_summary("<音箱 A 名字子串>")?);
//! ```
//!
//! 注意：`IsFormatSupported` 只反映驱动接受哪些格式，不代表设备当前没被别的
//! 程序独占（真正 `Initialize` 仍可能失败）；GUI 用作下拉框过滤已经足够。

use std::ffi::c_void;

use windows::Win32::Foundation::{S_FALSE, S_OK};
use windows::Win32::Media::Audio::{
    IAudioClient, IMMDeviceEnumerator, AUDCLNT_SHAREMODE, AUDCLNT_SHAREMODE_EXCLUSIVE,
    AUDCLNT_SHAREMODE_SHARED, WAVEFORMATEX,
};
use windows::Win32::System::Com::CoTaskMemFree;

use crate::com::{activate_client, enumerator, resolve, ComGuard, ResolvedDevice};
use crate::endpoint::{DataFlow, DeviceSelector};
use crate::endpoint_format::{build_waveformat_ext, parse_waveformat, EndpointFormat};
use crate::error::{AudioError, Result};

/// 候选采样率（Hz）：44.1k / 48k / 88.2k / 96k / 176.4k / 192k。
pub const CANDIDATE_SAMPLERATES: [u32; 6] = [44_100, 48_000, 88_200, 96_000, 176_400, 192_000];

/// 候选声道数：单声道 / 立体声 / 4.0 / 5.1 / 7.1。
pub const CANDIDATE_CHANNELS: [u16; 5] = [1, 2, 4, 6, 8];

/// 候选位深：`(valid_bits, is_float)`；32 bit 只探 IEEE float。
pub const CANDIDATE_BIT_DEPTHS: [(u16, bool); 3] = [(16, false), (24, false), (32, true)];

/// 一个候选格式（探测用的最小三元组）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Candidate {
    samplerate: u32,
    channels: u16,
    valid_bits: u16,
    is_float: bool,
}

impl Candidate {
    /// 生成 40 字节 WAVEFORMATEXTENSIBLE 请求块。
    fn waveformat(self) -> Vec<u8> {
        build_waveformat_ext(
            self.samplerate,
            self.channels,
            self.valid_bits,
            self.is_float,
        )
    }
}

fn candidate_triples() -> Vec<Candidate> {
    let mut out = Vec::with_capacity(
        CANDIDATE_SAMPLERATES.len() * CANDIDATE_CHANNELS.len() * CANDIDATE_BIT_DEPTHS.len(),
    );
    for &samplerate in &CANDIDATE_SAMPLERATES {
        for &channels in &CANDIDATE_CHANNELS {
            for &(valid_bits, is_float) in &CANDIDATE_BIT_DEPTHS {
                out.push(Candidate {
                    samplerate,
                    channels,
                    valid_bits,
                    is_float,
                });
            }
        }
    }
    out
}

/// 候选三元组 → [`EndpointFormat`]（自建字节必定可解析）。
fn candidate_format(candidate: Candidate) -> EndpointFormat {
    parse_waveformat(&candidate.waveformat()).expect("本模块构造的 WAVEFORMATEXTENSIBLE 必定能解析")
}

/// 候选格式矩阵（未探测）。GUI 可拿它来显示「哪些不支持」。
pub fn candidate_formats() -> Vec<EndpointFormat> {
    candidate_triples()
        .into_iter()
        .map(candidate_format)
        .collect()
}

/// 探测模式：独占（判断真实能力）/ 共享（判断能否直接出声）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProbeMode {
    Exclusive,
    Shared,
}

impl ProbeMode {
    fn sharemode(self) -> AUDCLNT_SHAREMODE {
        match self {
            Self::Exclusive => AUDCLNT_SHAREMODE_EXCLUSIVE,
            Self::Shared => AUDCLNT_SHAREMODE_SHARED,
        }
    }

    /// `IsFormatSupported` 的 HRESULT 判读规则。
    fn accepts(self, hr: windows::core::HRESULT) -> bool {
        match self {
            Self::Exclusive => hr == S_OK,
            // 共享模式下 S_FALSE = 「不是精确匹配，但引擎会重采样到最接近的格式」。
            Self::Shared => hr == S_OK || hr == S_FALSE,
        }
    }
}

/// 8 字节对齐的 40 字节缓冲：`WAVEFORMATEXTENSIBLE` 对对齐有要求，
/// 直接拿 `Vec<u8>` 的指针传给 WASAPI 不严谨。
#[repr(C, align(8))]
struct AlignedFormat([u8; 40]);

/// 探测一个格式。`wfx` 是 [`build_waveformat_ext`] 生成的 40 字节块。
///
/// 注意：共享模式下 `ppClosestMatch` 必须传一个**非空输出指针**，否则 WASAPI
/// 直接返回 `E_POINTER(0x80004003)`（真机实测；独占模式传 NULL 则没问题）。
/// 因此两种模式都传 `&mut closest`，返回后按 COM 约定释放引擎分配的格式块。
fn probe(client: &IAudioClient, mode: ProbeMode, wfx: &[u8]) -> bool {
    let mut buffer = AlignedFormat([0u8; 40]);
    let len = wfx.len().min(buffer.0.len());
    buffer.0[..len].copy_from_slice(&wfx[..len]);
    let mut closest: *mut WAVEFORMATEX = std::ptr::null_mut();
    // SAFETY: buffer 至少 8 字节对齐且包含 40 字节有效内存；`closest` 是合法的
    // 可写输出指针。WASAPI 写回的格式块由 CoTaskMemAlloc 分配、必须 CoTaskMemFree。
    let hr = unsafe {
        client.IsFormatSupported(
            mode.sharemode(),
            buffer.0.as_ptr() as *const WAVEFORMATEX,
            Some(&mut closest as *mut *mut WAVEFORMATEX),
        )
    };
    if !closest.is_null() {
        // SAFETY: 非空说明 WASAPI 用 CoTaskMemAlloc 分配了「最接近的格式」块。
        unsafe { CoTaskMemFree(Some(closest as *const c_void)) };
    }
    mode.accepts(hr)
}

/// 解析端点：先按 id，再回退按友好名子串；`"default"`/空串 = 默认 render 端点。
fn resolve_any(en: &IMMDeviceEnumerator, endpoint_id: &str) -> Result<ResolvedDevice> {
    let want = endpoint_id.trim();
    if want.is_empty() || want.eq_ignore_ascii_case("default") {
        return resolve(en, &DeviceSelector::Default, DataFlow::Render);
    }
    if let Ok(resolved) = resolve(en, &DeviceSelector::Id(want.to_owned()), DataFlow::Render) {
        return Ok(resolved);
    }
    resolve(
        en,
        &DeviceSelector::NameContains(want.to_owned()),
        DataFlow::Render,
    )
}

/// 在 COM 已初始化、端点已激活的 `IAudioClient` 上执行探测。
fn with_client<T>(endpoint_id: &str, probe: impl FnOnce(&IAudioClient) -> Result<T>) -> Result<T> {
    let _com = ComGuard::new()?;
    let en = enumerator()?;
    let resolved = resolve_any(&en, endpoint_id)?;
    let client = activate_client(&resolved.device)?;
    probe(&client)
}

/// 读取 WASAPI 返回的格式块字节（长度 = 18 + cbSize，cbSize 封顶 22）。
fn read_format_block(ptr: *const u8) -> Vec<u8> {
    // SAFETY: 调用方保证 ptr 指向 WASAPI 返回的合法格式块（至少 18 字节）。
    let header = unsafe { std::slice::from_raw_parts(ptr, 18) };
    let cb_size = u16::from_le_bytes([header[16], header[17]]) as usize;
    let len = 18 + cb_size.min(22);
    // SAFETY: 同上；WAVEFORMATEXTENSIBLE 最多 40 字节。
    unsafe { std::slice::from_raw_parts(ptr, len) }.to_vec()
}

/// 枚举某个端点**独占模式**下真正支持的格式（按候选矩阵顺序）。
///
/// `endpoint_id` 可以是 `IMMDevice::GetId()` 的端点 id、友好名子串（大小写不敏感），
/// 或 `"default"`（默认播放端点）。`S_OK` 才算支持；单条探测失败只算不支持，
/// 端点本身解析/激活失败才返回 `Err`。
pub fn supported_formats(endpoint_id: &str) -> Result<Vec<EndpointFormat>> {
    probe_all(endpoint_id, ProbeMode::Exclusive)
}

/// 枚举某个端点在**共享模式**下「可用」的格式（`S_OK` 或 `S_FALSE` 都算）。
///
/// 共享模式判定通常只有混音格式是 `S_OK`，其余是 `S_FALSE`（引擎会重采样），
/// 因此这个列表一般很长，只适合回答「能不能直接出声」，不适合判断设备真实能力。
pub fn shared_formats(endpoint_id: &str) -> Result<Vec<EndpointFormat>> {
    probe_all(endpoint_id, ProbeMode::Shared)
}

fn probe_all(endpoint_id: &str, mode: ProbeMode) -> Result<Vec<EndpointFormat>> {
    with_client(endpoint_id, |client| {
        let candidates = candidate_triples();
        let mut supported = Vec::with_capacity(candidates.len());
        for candidate in candidates {
            if probe(client, mode, &candidate.waveformat()) {
                supported.push(candidate_format(candidate));
            }
        }
        Ok(supported)
    })
}

/// 单个格式快速判定：该端点在**独占模式**下是否真正支持 `fmt`。
///
/// 24 bit PCM 会按 WAVEFORMATEXTENSIBLE 惯例装进 32-bit 容器再探测。
pub fn format_supported(endpoint_id: &str, fmt: &EndpointFormat) -> Result<bool> {
    probe_one(endpoint_id, fmt, ProbeMode::Exclusive)
}

/// 单个格式快速判定：该端点在**共享模式**下是否可用（`S_OK`/`S_FALSE` 都算）。
pub fn shared_format_supported(endpoint_id: &str, fmt: &EndpointFormat) -> Result<bool> {
    probe_one(endpoint_id, fmt, ProbeMode::Shared)
}

fn probe_one(endpoint_id: &str, fmt: &EndpointFormat, mode: ProbeMode) -> Result<bool> {
    let wfx = build_waveformat_ext(fmt.samplerate, fmt.channels, fmt.valid_bits, fmt.is_float);
    with_client(endpoint_id, |client| Ok(probe(client, mode, &wfx)))
}

/// 把格式列表拼成一行摘要（` · ` 分隔）；空列表返回 `（无）`。
///
/// 例：`48000 Hz / 2 ch / 16 bit · 96000 Hz / 2 ch / 24 bit`。
pub fn format_list_summary(formats: &[EndpointFormat]) -> String {
    if formats.is_empty() {
        return "（无）".to_owned();
    }
    formats
        .iter()
        .map(|format| format.describe())
        .collect::<Vec<_>>()
        .join(" · ")
}

/// 某个端点独占模式支持格式的一行摘要（GUI / CLI 直接打印用）。
pub fn supported_summary(endpoint_id: &str) -> Result<String> {
    Ok(format_list_summary(&supported_formats(endpoint_id)?))
}

/// 某个端点共享模式可用格式的一行摘要。
pub fn shared_summary(endpoint_id: &str) -> Result<String> {
    Ok(format_list_summary(&shared_formats(endpoint_id)?))
}

/// 读取端点当前生效的**混音格式**（`IAudioClient::GetMixFormat`）。
///
/// 与设备默认格式（`endpoint_format::get_format` 读的
/// `PKEY_AudioEngine_DeviceFormat`）不同：混音格式是音频引擎实际工作的格式。
pub fn mix_format(endpoint_id: &str) -> Result<EndpointFormat> {
    with_client(endpoint_id, |client| {
        // SAFETY: GetMixFormat 返回 CoTaskMemAlloc 分配的格式块。
        let ptr = unsafe { client.GetMixFormat() }
            .map_err(|e| AudioError::wasapi(e.code().0, "IAudioClient::GetMixFormat"))?;
        if ptr.is_null() {
            return Err(AudioError::invalid("GetMixFormat 返回空指针"));
        }
        let bytes = read_format_block(ptr as *const u8);
        unsafe { CoTaskMemFree(Some(ptr as *const c_void)) };
        parse_waveformat(&bytes).map_err(|e| AudioError::invalid(format!("混音格式无法解析：{e}")))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::core::HRESULT;

    #[test]
    fn candidate_matrix_has_expected_shape() {
        let formats = candidate_formats();
        assert_eq!(
            formats.len(),
            CANDIDATE_SAMPLERATES.len() * CANDIDATE_CHANNELS.len() * CANDIDATE_BIT_DEPTHS.len()
        );
        assert_eq!(formats.len(), 90);
        // 矩阵两端都在。
        assert!(formats.iter().any(|f| f.samplerate == 44_100
            && f.channels == 1
            && f.valid_bits == 16
            && !f.is_float));
        assert!(formats.iter().any(|f| f.samplerate == 192_000
            && f.channels == 8
            && f.valid_bits == 32
            && f.is_float));
        // 三元组无重复。
        let mut seen = std::collections::HashSet::new();
        for format in &formats {
            assert!(seen.insert((
                format.samplerate,
                format.channels,
                format.valid_bits,
                format.is_float
            )));
        }
    }

    #[test]
    fn candidate_matrix_waveformat_round_trip() {
        for format in candidate_formats() {
            let bytes = build_waveformat_ext(
                format.samplerate,
                format.channels,
                format.valid_bits,
                format.is_float,
            );
            assert_eq!(bytes.len(), 40);
            let parsed = parse_waveformat(&bytes).unwrap();
            assert_eq!(parsed, format, "候选格式往返不一致：{format:?}");
        }
    }

    #[test]
    fn candidate_derived_fields_follow_wave_format_rules() {
        let formats = candidate_formats();

        // 24 bit PCM：装进 32-bit 容器，valid_bits 保持 24。
        let pcm24 = formats
            .iter()
            .find(|f| {
                f.samplerate == 48_000 && f.channels == 2 && f.valid_bits == 24 && !f.is_float
            })
            .expect("48k/2ch/24bit 候选存在");
        assert_eq!(pcm24.bits, 32);
        assert_eq!(pcm24.block_align, 8);
        assert_eq!(pcm24.avg_bytes_per_sec, 48_000 * 8);
        assert_eq!(pcm24.channel_mask, 0x3);
        assert_eq!(pcm24.describe(), "48000 Hz / 2 ch / 24 bit");

        // 32-bit float：容器 32 bit，bits 和 valid_bits 都是 32。
        let f32_format = formats
            .iter()
            .find(|f| f.samplerate == 44_100 && f.channels == 2 && f.valid_bits == 32 && f.is_float)
            .expect("44.1k/2ch/32float 候选存在");
        assert_eq!(f32_format.bits, 32);
        assert_eq!(f32_format.block_align, 8);
        assert_eq!(f32_format.describe(), "44100 Hz / 2 ch / 32 float bit");

        // 多声道掩码：5.1 = 0x3f、7.1 = 0x63f、单声道 = 0x4。
        assert!(formats
            .iter()
            .any(|f| f.channels == 6 && f.channel_mask == 0x3f));
        assert!(formats
            .iter()
            .any(|f| f.channels == 8 && f.channel_mask == 0x63f));
        assert!(formats
            .iter()
            .any(|f| f.channels == 1 && f.channel_mask == 0x4));
    }

    #[test]
    fn waveformat_extensible_header_is_consistent() {
        // 独立按 WAVEFORMATEXTENSIBLE 布局检查字节，而不是只靠 parse 自证。
        let bytes = build_waveformat_ext(48_000, 2, 24, false);
        assert_eq!(bytes.len(), 40);
        assert_eq!(u16::from_le_bytes([bytes[0], bytes[1]]), 0xFFFE);
        assert_eq!(u16::from_le_bytes([bytes[2], bytes[3]]), 2);
        assert_eq!(
            u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
            48_000
        );
        assert_eq!(
            u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]),
            48_000 * 8
        );
        assert_eq!(u16::from_le_bytes([bytes[12], bytes[13]]), 8);
        assert_eq!(u16::from_le_bytes([bytes[14], bytes[15]]), 32); // 容器位宽
        assert_eq!(u16::from_le_bytes([bytes[16], bytes[17]]), 22); // cbSize
        assert_eq!(u16::from_le_bytes([bytes[18], bytes[19]]), 24); // valid bits
        assert_eq!(
            u32::from_le_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]),
            0x3
        );
        // PCM GUID = {00000001-0000-0010-8000-00aa00389b71}（内存序）。
        assert_eq!(
            &bytes[24..40],
            &[
                0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38,
                0x9b, 0x71
            ]
        );

        // float 版本 GUID 首字节是 3。
        let float_bytes = build_waveformat_ext(48_000, 2, 32, true);
        assert_eq!(float_bytes[24], 0x03);
        assert!(parse_waveformat(&float_bytes).unwrap().is_float);
    }

    #[test]
    fn probe_accept_rules_match_wasapi_semantics() {
        assert!(ProbeMode::Exclusive.accepts(HRESULT(S_OK.0)));
        assert!(!ProbeMode::Exclusive.accepts(HRESULT(S_FALSE.0)));
        assert!(!ProbeMode::Exclusive.accepts(HRESULT(0x8007_0057u32 as i32)));

        assert!(ProbeMode::Shared.accepts(HRESULT(S_OK.0)));
        assert!(ProbeMode::Shared.accepts(HRESULT(S_FALSE.0)));
        assert!(!ProbeMode::Shared.accepts(HRESULT(0x8007_0057u32 as i32)));
    }

    #[test]
    fn sharemode_maps_to_wasapi_constants() {
        assert_eq!(ProbeMode::Exclusive.sharemode().0, 1);
        assert_eq!(ProbeMode::Shared.sharemode().0, 0);
    }

    #[test]
    fn summary_formats_list_like_gui() {
        let formats = vec![
            parse_waveformat(&build_waveformat_ext(48_000, 2, 16, false)).unwrap(),
            parse_waveformat(&build_waveformat_ext(96_000, 2, 24, false)).unwrap(),
        ];
        assert_eq!(
            format_list_summary(&formats),
            "48000 Hz / 2 ch / 16 bit · 96000 Hz / 2 ch / 24 bit"
        );
        assert_eq!(format_list_summary(&[]), "（无）");
    }
}
