//! COM / WASAPI 底层封装（仅 Windows 编译）。
//!
//! 这里只放「薄封装 + 设备枚举 + 格式解析」，业务逻辑在 [`crate::capture`]。

use std::ffi::c_void;

use windows::core::{IUnknown, GUID, PWSTR};
use windows::Win32::Foundation::{PROPERTYKEY, S_OK};
use windows::Win32::Media::Audio::{
    eCapture, eConsole, eRender, EDataFlow, IAudioClient, IMMDevice, IMMDeviceCollection,
    IMMDeviceEnumerator, MMDeviceEnumerator, DEVICE_STATE_ACTIVE, WAVEFORMATEX,
};
use windows::Win32::System::Com::StructuredStorage::{PropVariantClear, PROPVARIANT};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_MULTITHREADED, STGM_READ,
};
use windows::Win32::System::Variant::VT_LPWSTR;

use crate::convert::SampleFormat;
use crate::endpoint::{DataFlow, DeviceSelector, EndpointInfo, FormatInfo, PcmFormat};
use crate::error::{AudioError, Result};

/// `RPC_E_CHANGED_MODE`：线程已经用 STA 初始化过 COM。
const RPC_E_CHANGED_MODE: windows::core::HRESULT = windows::core::HRESULT(0x8001_0106u32 as i32);

/// `PKEY_Device_FriendlyName`，{a45c254e-df1c-4efd-8020-67d146a850e0}, 14
/// （functiondiscoverykeys_devpkey.h；这里本地定义，避免多拉一个 windows feature）。
const PKEY_DEVICE_FRIENDLY_NAME: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID {
        data1: 0xa45c_254e,
        data2: 0xdf1c,
        data3: 0x4efd,
        data4: [0x80, 0x20, 0x67, 0xd1, 0x46, 0xa8, 0x50, 0xe0],
    },
    pid: 14,
};

/// `KSDATAFORMAT_SUBTYPE_PCM` = {00000001-0000-0010-8000-00AA00389B71}（内存序）。
const SUBTYPE_PCM: [u8; 16] = [
    0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71,
];
/// `KSDATAFORMAT_SUBTYPE_IEEE_FLOAT` = {00000003-0000-0010-8000-00AA00389B71}（内存序）。
const SUBTYPE_IEEE_FLOAT: [u8; 16] = [
    0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71,
];

/// 100 纳秒单位 ← 毫秒。
pub(crate) fn ms_to_hns(ms: u32) -> i64 {
    ms as i64 * 10_000
}

/// RAII COM 初始化。线程里第一个拿到它的负责 `CoUninitialize`。
pub(crate) struct ComGuard {
    uninit: bool,
}

impl ComGuard {
    pub(crate) fn new() -> Result<Self> {
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if hr == S_OK {
            Ok(Self { uninit: true })
        } else if hr == RPC_E_CHANGED_MODE || hr.is_ok() {
            // STA 已初始化 或 S_FALSE（本线程已初始化过）：直接用，不重复反初始化。
            Ok(Self { uninit: false })
        } else {
            Err(AudioError::wasapi(
                hr.0,
                "CoInitializeEx(COINIT_MULTITHREADED)",
            ))
        }
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.uninit {
            unsafe { CoUninitialize() };
        }
    }
}

pub(crate) fn enumerator() -> Result<IMMDeviceEnumerator> {
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None::<&IUnknown>, CLSCTX_ALL) }
        .map_err(|e| AudioError::wasapi(e.code().0, "CoCreateInstance(MMDeviceEnumerator)"))
}

pub(crate) fn edataflow(flow: DataFlow) -> EDataFlow {
    match flow {
        DataFlow::Render => eRender,
        DataFlow::Capture => eCapture,
    }
}

pub(crate) fn default_device(en: &IMMDeviceEnumerator, flow: DataFlow) -> Result<IMMDevice> {
    unsafe { en.GetDefaultAudioEndpoint(edataflow(flow), eConsole) }
        .map_err(|e| AudioError::wasapi(e.code().0, "GetDefaultAudioEndpoint"))
}

pub(crate) fn devices(en: &IMMDeviceEnumerator, flow: DataFlow) -> Result<Vec<IMMDevice>> {
    let col: IMMDeviceCollection =
        unsafe { en.EnumAudioEndpoints(edataflow(flow), DEVICE_STATE_ACTIVE) }
            .map_err(|e| AudioError::wasapi(e.code().0, "EnumAudioEndpoints"))?;
    let n = unsafe { col.GetCount() }.map_err(|e| AudioError::wasapi(e.code().0, "GetCount"))?;
    let mut out = Vec::with_capacity(n as usize);
    for i in 0..n {
        match unsafe { col.Item(i) } {
            Ok(d) => out.push(d),
            Err(e) => return Err(AudioError::wasapi(e.code().0, format!("Item({i})"))),
        }
    }
    Ok(out)
}

fn pwstr_to_string(p: PWSTR) -> Result<String> {
    if p.is_null() {
        return Err(AudioError::invalid("设备字符串为空指针"));
    }
    let s = unsafe { p.to_string() }
        .map_err(|e| AudioError::invalid(format!("设备名字符串不是合法 UTF-16：{e}")))?;
    unsafe { CoTaskMemFree(Some(p.0 as *const c_void)) };
    Ok(s)
}

pub(crate) fn device_id(dev: &IMMDevice) -> Result<String> {
    let p =
        unsafe { dev.GetId() }.map_err(|e| AudioError::wasapi(e.code().0, "IMMDevice::GetId"))?;
    pwstr_to_string(p)
}

pub(crate) fn device_name(dev: &IMMDevice) -> Result<String> {
    let store = unsafe { dev.OpenPropertyStore(STGM_READ) }
        .map_err(|e| AudioError::wasapi(e.code().0, "OpenPropertyStore"))?;
    let mut pv: PROPVARIANT = unsafe { store.GetValue(&PKEY_DEVICE_FRIENDLY_NAME) }
        .map_err(|e| AudioError::wasapi(e.code().0, "GetValue(FriendlyName)"))?;
    // SAFETY: `GetValue` 成功后 `pv` 已初始化；只在 `vt == VT_LPWSTR` 时读 `pwszVal`。
    let name = unsafe {
        if pv.Anonymous.Anonymous.vt == VT_LPWSTR {
            let p = pv.Anonymous.Anonymous.Anonymous.pwszVal;
            if p.is_null() {
                String::new()
            } else {
                p.to_string().unwrap_or_default()
            }
        } else {
            String::new()
        }
    };
    unsafe { PropVariantClear(&mut pv) }
        .map_err(|e| AudioError::wasapi(e.code().0, "PropVariantClear"))?;
    Ok(name)
}

pub(crate) fn activate_client(dev: &IMMDevice) -> Result<IAudioClient> {
    unsafe { dev.Activate::<IAudioClient>(CLSCTX_ALL, None) }
        .map_err(|e| AudioError::wasapi(e.code().0, "IMMDevice::Activate(IAudioClient)"))
}

unsafe fn read_array<const N: usize>(p: *const u8, off: usize) -> [u8; N] {
    let mut b = [0u8; N];
    // SAFETY: 调用方保证 `p` 至少可读 `off + N` 字节（WASAPI 返回的格式块）。
    unsafe { std::ptr::copy_nonoverlapping(p.add(off), b.as_mut_ptr(), N) };
    b
}

/// 解析 `WAVEFORMATEX` / `WAVEFORMATEXTENSIBLE` 原始内存。
///
/// 直接按字节读，避免依赖 windows crate 里可标量/不可标量的嵌套类型名。
pub(crate) unsafe fn format_info_from_raw(p: *const u8) -> FormatInfo {
    // SAFETY: 调用方保证 `p` 指向 WASAPI 返回的、至少 18 字节的格式块。
    let hdr = unsafe { read_array::<18>(p, 0) };
    let raw_format_tag = u16::from_le_bytes([hdr[0], hdr[1]]);
    let channels = u16::from_le_bytes([hdr[2], hdr[3]]);
    let samplerate = u32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]);
    let avg_bytes_per_sec = u32::from_le_bytes([hdr[8], hdr[9], hdr[10], hdr[11]]);
    let block_align = u16::from_le_bytes([hdr[12], hdr[13]]);
    let bits = u16::from_le_bytes([hdr[14], hdr[15]]);
    let cb_size = u16::from_le_bytes([hdr[16], hdr[17]]);

    let is_extensible = raw_format_tag == 0xFFFE && cb_size >= 22;
    // WAVEFORMATEXTENSIBLE: Format(18) + Samples(2) + dwChannelMask(4) + SubFormat(16) → 偏移 24
    let sub_format = if is_extensible {
        // SAFETY: is_extensible 已确认 cbSize >= 22，SubFormat 落在偏移 24..40。
        Some(unsafe { read_array::<16>(p, 24) })
    } else {
        None
    };

    let sample_format = match (raw_format_tag, bits, sub_format) {
        (1, 16, _) => Some(SampleFormat::S16),
        (1, 24, _) => Some(SampleFormat::S24),
        (1, 32, _) => Some(SampleFormat::S32),
        (3, 32, _) => Some(SampleFormat::F32),
        (0xFFFE, _, Some(sub)) if sub == SUBTYPE_PCM => match bits {
            16 => Some(SampleFormat::S16),
            24 => Some(SampleFormat::S24),
            32 => Some(SampleFormat::S32),
            _ => None,
        },
        (0xFFFE, _, Some(sub)) if sub == SUBTYPE_IEEE_FLOAT => Some(SampleFormat::F32),
        _ => None,
    };

    FormatInfo {
        sample_format,
        channels,
        samplerate,
        bits,
        block_align,
        avg_bytes_per_sec,
        raw_format_tag,
        is_extensible,
    }
}

pub(crate) fn mix_format(client: &IAudioClient) -> Result<FormatInfo> {
    let p = unsafe { client.GetMixFormat() }
        .map_err(|e| AudioError::wasapi(e.code().0, "IAudioClient::GetMixFormat"))?;
    if p.is_null() {
        return Err(AudioError::invalid("GetMixFormat 返回空指针"));
    }
    let info = unsafe { format_info_from_raw(p as *const u8) };
    unsafe { CoTaskMemFree(Some(p as *const c_void)) };
    if !info.is_valid() {
        return Err(AudioError::UnsupportedFormat(info.describe()));
    }
    Ok(info)
}

/// 构造 16-bit PCM 的 `WAVEFORMATEX`（共享模式 + AUTOCONVERTPCM 时用它请求目标格式）。
pub(crate) fn pcm16_waveformatex(f: PcmFormat) -> WAVEFORMATEX {
    let block_align = f.frame_bytes() as u16;
    WAVEFORMATEX {
        wFormatTag: 1, // WAVE_FORMAT_PCM
        nChannels: f.channels,
        nSamplesPerSec: f.samplerate,
        nAvgBytesPerSec: f.samplerate * block_align as u32,
        nBlockAlign: block_align,
        wBitsPerSample: f.bits,
        cbSize: 0,
    }
}

/// 解析出来的端点。
pub(crate) struct ResolvedDevice {
    pub device: IMMDevice,
    pub id: String,
    pub name: String,
    pub is_default: bool,
}

fn describe_one(dev: &IMMDevice, flow: DataFlow) -> String {
    let name = device_name(dev).unwrap_or_else(|_| "<名字读不到>".into());
    let id = device_id(dev).unwrap_or_else(|_| "<id 读不到>".into());
    format!("{name} [{flow}] {{{id}}}")
}

/// 按选择器在指定方向的端点里挑一个。
pub(crate) fn resolve(
    en: &IMMDeviceEnumerator,
    sel: &DeviceSelector,
    flow: DataFlow,
) -> Result<ResolvedDevice> {
    let default_id = default_device(en, flow)
        .ok()
        .and_then(|d| device_id(&d).ok());
    match sel {
        DeviceSelector::Default => {
            let device = default_device(en, flow)
                .map_err(|_| AudioError::NoDefaultDevice(format!("{flow} 方向没有默认端点")))?;
            let id = device_id(&device)?;
            let name = device_name(&device).unwrap_or_default();
            Ok(ResolvedDevice {
                device,
                id,
                name,
                is_default: true,
            })
        }
        DeviceSelector::Id(want) => {
            for device in devices(en, flow)? {
                let id = device_id(&device)?;
                if &id == want {
                    let name = device_name(&device).unwrap_or_default();
                    let is_default = default_id.as_deref() == Some(id.as_str());
                    return Ok(ResolvedDevice {
                        device,
                        id,
                        name,
                        is_default,
                    });
                }
            }
            Err(AudioError::DeviceNotFound(format!(
                "id={want}（{flow} 方向）"
            )))
        }
        DeviceSelector::NameContains(_) => {
            let all = devices(en, flow).unwrap_or_default();
            let mut matched: Vec<ResolvedDevice> = Vec::new();
            let mut others: Vec<String> = Vec::new();
            for device in all {
                let id = device_id(&device)?;
                let name = device_name(&device).unwrap_or_default();
                let is_default = default_id.as_deref() == Some(id.as_str());
                if sel.matches(&name, &id) {
                    matched.push(ResolvedDevice {
                        device,
                        id,
                        name,
                        is_default,
                    });
                } else {
                    others.push(describe_one(&device, flow));
                }
            }
            match matched.len() {
                0 => Err(AudioError::DeviceNotFound(format!(
                    "{}（{flow} 方向）；本机候选：{}",
                    sel.describe(),
                    if others.is_empty() {
                        "<无活动端点>".to_string()
                    } else {
                        others.join(" / ")
                    }
                ))),
                1 => Ok(matched.remove(0)),
                _ => {
                    // 多个端点撞名（自研虚拟声卡 vs 改名过的 Steam 设备）：先试精确匹配，
                    // 仍不唯一就报错，绝不静默挑第一个。
                    let exact: Vec<usize> = matched
                        .iter()
                        .enumerate()
                        .filter(|(_, d)| sel.matches_exact(&d.name, &d.id))
                        .map(|(i, _)| i)
                        .collect();
                    if exact.len() == 1 {
                        return Ok(matched.swap_remove(exact[0]));
                    }
                    let names: Vec<String> = matched
                        .iter()
                        .map(|d| format!("「{}」({})", d.name, d.id))
                        .collect();
                    Err(AudioError::DeviceNotFound(format!(
                        "{}（{flow} 方向）匹配到 {} 个端点，请用完整名字或 id 指定：{}",
                        sel.describe(),
                        names.len(),
                        names.join(" / ")
                    )))
                }
            }
        }
    }
}

/// 列出本机所有活动端点（render + capture），带默认标记与混音格式。
pub(crate) fn list_endpoints() -> Result<Vec<EndpointInfo>> {
    let _com = ComGuard::new()?;
    let en = enumerator()?;
    let mut out = Vec::new();
    for flow in [DataFlow::Render, DataFlow::Capture] {
        let default_id = default_device(&en, flow)
            .ok()
            .and_then(|d| device_id(&d).ok());
        for device in devices(&en, flow)? {
            let id = device_id(&device).unwrap_or_default();
            let name = device_name(&device).unwrap_or_default();
            let is_default = default_id.as_deref() == Some(id.as_str());
            let (mix_format, error) = match activate_client(&device) {
                Ok(client) => match mix_format(&client) {
                    Ok(f) => (Some(f), None),
                    Err(e) => (None, Some(e.to_string())),
                },
                Err(e) => (None, Some(e.to_string())),
            };
            out.push(EndpointInfo {
                id,
                name,
                flow,
                is_default,
                mix_format,
                error,
            });
        }
    }
    Ok(out)
}

/// 解析选择器并返回端点信息（不打开流，用于预检）。
pub(crate) fn resolve_endpoint(sel: &DeviceSelector, flow: DataFlow) -> Result<EndpointInfo> {
    let _com = ComGuard::new()?;
    let en = enumerator()?;
    let d = resolve(&en, sel, flow)?;
    let (mix_format, error) = match activate_client(&d.device) {
        Ok(client) => match mix_format(&client) {
            Ok(f) => (Some(f), None),
            Err(e) => (None, Some(e.to_string())),
        },
        Err(e) => (None, Some(e.to_string())),
    };
    Ok(EndpointInfo {
        id: d.id,
        name: d.name,
        flow,
        is_default: d.is_default,
        mix_format,
        error,
    })
}
