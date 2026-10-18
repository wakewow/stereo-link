//! 端点默认格式（`PKEY_AudioEngine_DeviceFormat`）的读取与设置。
//!
//! 走 `IMMDevice::OpenPropertyStore(STGM_READWRITE)` + `IPropertyStore::SetValue/Commit`：
//! 这是 Windows 声音设置里“设备默认格式”的官方属性存储路径，**不需要管理员权限**，
//! 也不需要直接改 `MMDevices` 注册表（那里连 SYSTEM 都没有写权限）。
//!
//! 存储格式是 `VT_BLOB`（40 字节 WAVEFORMATEXTENSIBLE）。

use anyhow::{bail, Context, Result};

/// 端点当前默认格式（WAVEFORMATEXTENSIBLE 解析结果）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EndpointFormat {
    /// 采样率（Hz）。
    pub samplerate: u32,
    /// 声道数。
    pub channels: u16,
    /// 容器位宽。
    pub bits: u16,
    /// 有效位宽（优先取 WAVEFORMATEXTENSIBLE 的 `wValidBitsPerSample`）。
    pub valid_bits: u16,
    /// 每帧字节数。
    pub block_align: u16,
    /// 每秒字节数。
    pub avg_bytes_per_sec: u32,
    /// 是否为 IEEE float（否则按整数 PCM 处理）。
    pub is_float: bool,
    /// 声道掩码（1ch=0x4 / 2ch=0x3）。
    pub channel_mask: u32,
}

impl EndpointFormat {
    /// 例：`48000 Hz / 2 ch / 16 bit`（浮点会带 `float`）。
    pub fn describe(&self) -> String {
        let kind = if self.is_float { " float" } else { "" };
        format!(
            "{} Hz / {} ch / {}{} bit",
            self.samplerate, self.channels, self.valid_bits, kind
        )
    }
}

/// 常见可选格式（用于界面下拉框）。
pub const COMMON_FORMATS: &[(u32, u16, u16, bool)] = &[
    (44100, 2, 16, false),
    (48000, 2, 16, false),
    (48000, 2, 24, false),
    (96000, 2, 24, false),
    (44100, 2, 32, true),
    (48000, 2, 32, true),
];

/// Windows PCM GUID：{00000001-0000-0010-8000-00aa00389b71}
const PCM_GUID: [u8; 16] = [
    0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71,
];

/// IEEE float GUID：{00000003-0000-0010-8000-00aa00389b71}
const FLOAT_GUID: [u8; 16] = [
    0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71,
];

fn channel_mask(channels: u16) -> u32 {
    match channels {
        1 => 0x4,   // FRONT_CENTER
        2 => 0x3,   // FL | FR
        4 => 0x33,  // FL FR BL BR
        6 => 0x3f,  // 5.1
        8 => 0x63f, // 7.1
        _ => 0,
    }
}

/// 把 WAVEFORMATEX / WAVEFORMATEXTENSIBLE 字节解析成 [`EndpointFormat`]。
pub fn parse_waveformat(bytes: &[u8]) -> Result<EndpointFormat> {
    if bytes.len() < 18 {
        bail!("WAVEFORMATEX 太短：{} 字节", bytes.len());
    }
    let tag = u16::from_le_bytes([bytes[0], bytes[1]]);
    let channels = u16::from_le_bytes([bytes[2], bytes[3]]);
    let samplerate = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
    let avg_bytes_per_sec = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
    let block_align = u16::from_le_bytes([bytes[12], bytes[13]]);
    let bits = u16::from_le_bytes([bytes[14], bytes[15]]);
    let cb_size = u16::from_le_bytes([bytes[16], bytes[17]]) as usize;
    if tag == 0xFFFE {
        if bytes.len() < 18 + cb_size || cb_size < 22 {
            bail!(
                "WAVEFORMATEXTENSIBLE 不完整：len={}, cbSize={cb_size}",
                bytes.len()
            );
        }
        let valid_bits = u16::from_le_bytes([bytes[18], bytes[19]]);
        let mask = u32::from_le_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
        let guid = &bytes[24..40];
        let is_float = guid == FLOAT_GUID;
        Ok(EndpointFormat {
            samplerate,
            channels,
            bits,
            valid_bits: if valid_bits == 0 { bits } else { valid_bits },
            block_align,
            avg_bytes_per_sec,
            is_float,
            channel_mask: mask,
        })
    } else {
        Ok(EndpointFormat {
            samplerate,
            channels,
            bits,
            valid_bits: bits,
            block_align,
            avg_bytes_per_sec,
            is_float: tag == 3,
            channel_mask: channel_mask(channels),
        })
    }
}

/// 构造 WAVEFORMATEXTENSIBLE 字节（40 字节）。
pub fn build_waveformat_ext(
    samplerate: u32,
    channels: u16,
    valid_bits: u16,
    is_float: bool,
) -> Vec<u8> {
    let container_bits: u16 = if is_float {
        32
    } else {
        valid_bits.next_power_of_two()
    };
    let block_align = channels * container_bits / 8;
    let avg = samplerate * block_align as u32;
    let mut out = Vec::with_capacity(40);
    out.extend_from_slice(&0xFFFEu16.to_le_bytes());
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&samplerate.to_le_bytes());
    out.extend_from_slice(&avg.to_le_bytes());
    out.extend_from_slice(&block_align.to_le_bytes());
    out.extend_from_slice(&container_bits.to_le_bytes());
    out.extend_from_slice(&22u16.to_le_bytes());
    out.extend_from_slice(&valid_bits.to_le_bytes());
    out.extend_from_slice(&channel_mask(channels).to_le_bytes());
    out.extend_from_slice(if is_float { &FLOAT_GUID } else { &PCM_GUID });
    out
}

#[cfg(windows)]
mod win {
    use super::*;
    use windows::core::{GUID, HSTRING, PCWSTR};
    use windows::Win32::Foundation::PROPERTYKEY;
    use windows::Win32::Media::Audio::{IMMDeviceEnumerator, MMDeviceEnumerator, WAVEFORMATEX};
    use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
        COINIT_MULTITHREADED, STGM,
    };
    use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;

    const PKEY_DEVICE_FORMAT: PROPERTYKEY = PROPERTYKEY {
        fmtid: GUID::from_u128(0xf19f064d_082c_4e27_bc73_6882a1bb8e4c),
        pid: 0,
    };

    /// `CLSID_CPolicyConfigClient`：未公开接口，Windows 自己改设备格式/默认输出就用它。
    const CLSID_CPOLICYCONFIGCLIENT: GUID = GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9);

    /// `IPolicyConfig`（IID 固定，方法顺序与 Windows SDK `policyconfig.h` 一致，
    /// 顺序错了会直接崩，不要调整）。只声明前 4 个方法就够用；后面还有
    /// GetProcessingPeriod/SetProcessingPeriod/GetShareMode/SetShareMode/GetPropertyValue/
    /// SetPropertyValue/SetDefaultEndpoint/SetEndpointVisibility，用到再补。
    #[windows::core::interface("f8679f50-850a-41cf-9c72-430f290290c8")]
    unsafe trait IPolicyConfig: windows::core::IUnknown {
        unsafe fn get_mix_format(
            &self,
            device_id: PCWSTR,
            format: *mut *mut WAVEFORMATEX,
        ) -> windows::core::Result<()>;
        unsafe fn get_device_format(
            &self,
            device_id: PCWSTR,
            is_default: i32,
            format: *mut *mut WAVEFORMATEX,
        ) -> windows::core::Result<()>;
        unsafe fn reset_device_format(&self, device_id: PCWSTR) -> windows::core::Result<()>;
        unsafe fn set_device_format(
            &self,
            device_id: PCWSTR,
            endpoint_format: *const WAVEFORMATEX,
            mix_format: *const WAVEFORMATEX,
        ) -> windows::core::Result<()>;
    }

    /// RPC_E_CHANGED_MODE：线程已经用 STA 初始化过 COM。
    const RPC_E_CHANGED_MODE: windows::core::HRESULT =
        windows::core::HRESULT(0x8001_0106u32 as i32);

    /// RAII COM 初始化；UI 线程可能已被 winit/egui 初始化成 STA，要容忍。
    struct ComScope {
        uninit: bool,
    }

    impl Drop for ComScope {
        fn drop(&mut self) {
            if self.uninit {
                unsafe { CoUninitialize() };
            }
        }
    }

    fn com_scope() -> Result<ComScope> {
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if hr.0 == 0 {
            Ok(ComScope { uninit: true })
        } else if hr == RPC_E_CHANGED_MODE || hr.is_ok() {
            Ok(ComScope { uninit: false })
        } else {
            bail!(
                "CoInitializeEx(COINIT_MULTITHREADED) 失败：0x{:08x}",
                hr.0 as u32
            )
        }
    }

    fn enumerator() -> Result<IMMDeviceEnumerator> {
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
            .context("创建 MMDeviceEnumerator 失败")
    }

    fn device_by_id(endpoint_id: &str) -> Result<windows::Win32::Media::Audio::IMMDevice> {
        let en = enumerator()?;
        let id = HSTRING::from(endpoint_id);
        unsafe { en.GetDevice(&id) }.with_context(|| format!("找不到音频端点：{endpoint_id}"))
    }

    fn read_blob(store: &IPropertyStore) -> Result<Vec<u8>> {
        let pv: PROPVARIANT = unsafe { store.GetValue(&PKEY_DEVICE_FORMAT) }
            .context("读取 PKEY_AudioEngine_DeviceFormat 失败")?;
        let vt = unsafe { pv.Anonymous.Anonymous.vt };
        if vt.0 != 65 {
            bail!("设备格式属性不是 VT_BLOB（vt=0x{:04x}）", vt.0 as u32);
        }
        let (cb, data) = unsafe {
            let blob = pv.Anonymous.Anonymous.Anonymous.blob;
            (
                blob.cbSize,
                std::slice::from_raw_parts(blob.pBlobData, blob.cbSize as usize).to_vec(),
            )
        };
        if cb == 0 {
            bail!("设备格式属性为空");
        }
        Ok(data)
    }

    /// 读取某个端点的当前默认格式。
    pub fn get_format(endpoint_id: &str) -> Result<EndpointFormat> {
        let _guard = com_scope()?;
        let device = device_by_id(endpoint_id)?;
        let store: IPropertyStore =
            unsafe { device.OpenPropertyStore(STGM(0)) }.context("打开端点属性存储失败")?;
        let bytes = read_blob(&store)?;
        parse_waveformat(&bytes)
    }

    /// 读取某个端点当前生效的混音格式（`IPolicyConfig::GetMixFormat`）。
    pub fn get_mix_format(endpoint_id: &str) -> Result<EndpointFormat> {
        let _guard = com_scope()?;
        let policy: IPolicyConfig =
            unsafe { CoCreateInstance(&CLSID_CPOLICYCONFIGCLIENT, None, CLSCTX_ALL) }
                .context("创建 IPolicyConfig 失败")?;
        let id = HSTRING::from(endpoint_id);
        let mut ptr: *mut WAVEFORMATEX = std::ptr::null_mut();
        unsafe { policy.get_mix_format(PCWSTR(id.as_ptr()), &mut ptr) }
            .context("GetMixFormat 失败")?;
        if ptr.is_null() {
            bail!("GetMixFormat 返回空指针");
        }
        let bytes = unsafe { std::slice::from_raw_parts(ptr as *const u8, 40).to_vec() };
        unsafe { CoTaskMemFree(Some(ptr as *const std::ffi::c_void)) };
        parse_waveformat(&bytes)
    }

    /// 设置端点默认格式，返回设置后读回的格式。
    ///
    /// 用 Windows 未公开的 `IPolicyConfig::SetDeviceFormat`（Windows 声音设置面板自己就用
    /// 它改「默认格式」）：**立即生效、不需要重启音频服务、也不需要管理员**。
    /// 注意不要退回 `IPropertyStore::SetValue`——那条路会写出 Windows 不认的
    /// `VT_VECTOR|VT_UI1` 布局，看起来成功实际不生效。
    pub fn set_format(
        endpoint_id: &str,
        samplerate: u32,
        channels: u16,
        valid_bits: u16,
        is_float: bool,
    ) -> Result<EndpointFormat> {
        let _guard = com_scope()?;
        let bytes = build_waveformat_ext(samplerate, channels, valid_bits, is_float);
        set_via_policy_config(endpoint_id, &bytes, samplerate, channels)?;

        // 回读验证：设备格式（属性存储）+ 混音格式（GetMixFormat，真正生效的那个）。
        let device = device_by_id(endpoint_id)?;
        let store_ro: IPropertyStore =
            unsafe { device.OpenPropertyStore(STGM(0)) }.context("重新打开端点属性存储失败")?;
        let readback = read_blob(&store_ro)?;
        let applied = parse_waveformat(&readback).with_context(|| {
            format!(
                "格式已写入但读回无法解析：{}",
                readback
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            )
        })?;
        match get_mix_format(endpoint_id) {
            Ok(mix) if mix.samplerate == samplerate => {}
            Ok(mix) => anyhow::bail!(
                "设备格式已写入，但混音格式仍为 {} Hz / {} ch（请求 {} Hz）",
                mix.samplerate,
                mix.channels,
                samplerate
            ),
            Err(e) => anyhow::bail!("设备格式已写入，但无法确认混音格式：{e}"),
        }
        Ok(applied)
    }

    /// 用 IPolicyConfig 接口设置设备格式（Windows 声音设置里“默认格式”那一项）。
    fn set_via_policy_config(
        endpoint_id: &str,
        endpoint_wfx: &[u8],
        samplerate: u32,
        channels: u16,
    ) -> Result<()> {
        let policy: IPolicyConfig =
            unsafe { CoCreateInstance(&CLSID_CPOLICYCONFIGCLIENT, None, CLSCTX_ALL) }
                .context("创建 IPolicyConfig 失败")?;
        let mix = build_waveformat_ext(samplerate, channels, 32, true);
        let id = HSTRING::from(endpoint_id);
        unsafe {
            policy.set_device_format(
                PCWSTR(id.as_ptr()),
                endpoint_wfx.as_ptr() as *const WAVEFORMATEX,
                mix.as_ptr() as *const WAVEFORMATEX,
            )
        }
        .context("SetDeviceFormat 失败")
    }

    /// 默认播放端点的 (id, 格式)。
    pub fn default_render() -> Result<(String, EndpointFormat)> {
        use windows::Win32::Media::Audio::{eConsole, eRender};
        let _guard = com_scope()?;
        let en = enumerator()?;
        let device = unsafe { en.GetDefaultAudioEndpoint(eRender, eConsole) }
            .context("取默认播放端点失败")?;
        let id_ptr = unsafe { device.GetId() }?;
        let id = unsafe { id_ptr.to_string() }?;
        let store: IPropertyStore =
            unsafe { device.OpenPropertyStore(STGM(0)) }.context("打开端点属性存储失败")?;
        let bytes = read_blob(&store)?;
        let format = parse_waveformat(&bytes)?;
        Ok((id, format))
    }

    // 避免未使用告警（PCWSTR 在部分 API 可选参数里需要）。
    #[allow(dead_code)]
    fn _unused(_: PCWSTR) {}
}

#[cfg(windows)]
pub use win::{default_render, get_format, set_format};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_and_parse_round_trip_16bit() {
        let bytes = build_waveformat_ext(44100, 2, 16, false);
        assert_eq!(bytes.len(), 40);
        let f = parse_waveformat(&bytes).unwrap();
        assert_eq!(f.samplerate, 44100);
        assert_eq!(f.channels, 2);
        assert_eq!(f.valid_bits, 16);
        assert!(!f.is_float);
        assert_eq!(f.block_align, 4);
        assert_eq!(f.avg_bytes_per_sec, 44100 * 4);
        assert_eq!(f.channel_mask, 3);
        assert!(f.describe().contains("44100 Hz / 2 ch / 16 bit"));
    }

    #[test]
    fn build_and_parse_round_trip_float() {
        let bytes = build_waveformat_ext(48000, 2, 32, true);
        let f = parse_waveformat(&bytes).unwrap();
        assert!(f.is_float);
        assert_eq!(f.block_align, 8);
    }

    #[test]
    fn parse_known_vad_blob() {
        // 本机虚拟声卡的原始设备格式（16 bit / 48 kHz / 2 ch）。
        let bytes: Vec<u8> = vec![
            0xfe, 0xff, 0x02, 0x00, 0x80, 0xbb, 0x00, 0x00, 0x00, 0xee, 0x02, 0x00, 0x04, 0x00,
            0x10, 0x00, 0x16, 0x00, 0x10, 0x00, 0x03, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71,
        ];
        let f = parse_waveformat(&bytes).unwrap();
        assert_eq!(f.samplerate, 48000);
        assert_eq!(f.valid_bits, 16);
        assert!(!f.is_float);
        assert_eq!(f.block_align, 4);
    }

    #[test]
    fn rejects_truncated() {
        assert!(parse_waveformat(&[0u8; 10]).is_err());
        assert!(parse_waveformat(&build_waveformat_ext(48000, 2, 16, false)[..20]).is_err());
    }

    #[test]
    fn channel_masks() {
        assert_eq!(channel_mask(1), 0x4);
        assert_eq!(channel_mask(2), 0x3);
        assert_eq!(channel_mask(6), 0x3f);
    }
}
