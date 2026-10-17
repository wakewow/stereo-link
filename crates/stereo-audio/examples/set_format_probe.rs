//! 探测 2：用 VT_BLOB 读取/原样写回 PKEY_AudioEngine_DeviceFormat，验证 COM 路径能否持久化。
//!
//! 用法：`cargo run -q -p stereo-audio --example set_format_probe`

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    use windows::core::GUID;
    use windows::Win32::Foundation::PROPERTYKEY;
    use windows::Win32::Media::Audio::{
        eConsole, eRender, IMMDeviceEnumerator, MMDeviceEnumerator,
    };
    use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoTaskMemAlloc, CoUninitialize, CLSCTX_ALL,
        COINIT_MULTITHREADED, STGM, STGM_READWRITE,
    };
    use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;

    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok()? };
    struct Com;
    impl Drop for Com {
        fn drop(&mut self) {
            unsafe { CoUninitialize() };
        }
    }
    let _com = Com;

    let key = PROPERTYKEY {
        fmtid: GUID::from_u128(0xf19f064d_082c_4e27_bc73_6882a1bb8e4c),
        pid: 0,
    };

    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };
    let device = unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole)? };
    let id = unsafe { device.GetId() }?;
    let id_s = unsafe { id.to_string() }?;
    println!("default render device: {id_s}");

    let store: IPropertyStore = unsafe { device.OpenPropertyStore(STGM_READWRITE)? };
    let pv = unsafe { store.GetValue(&key)? };
    let vt = unsafe { pv.Anonymous.Anonymous.vt };
    println!("propvariant vt = {vt:?} (0x{:04x})", vt.0 as u32);
    let (cb, data) = unsafe {
        let blob = pv.Anonymous.Anonymous.Anonymous.blob;
        (
            blob.cbSize,
            std::slice::from_raw_parts(blob.pBlobData, blob.cbSize as usize).to_vec(),
        )
    };
    println!("VT_BLOB cbSize = {cb}");
    println!("blob = {}", hex(&data));

    // 原样写回
    let mem = unsafe { CoTaskMemAlloc(data.len()) } as *mut u8;
    unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), mem, data.len()) };
    let pv_out = PROPVARIANT {
        Anonymous: windows::Win32::System::Com::StructuredStorage::PROPVARIANT_0 {
            Anonymous: std::mem::ManuallyDrop::new(
                windows::Win32::System::Com::StructuredStorage::PROPVARIANT_0_0 {
                    vt,
                    wReserved1: 0,
                    wReserved2: 0,
                    wReserved3: 0,
                    Anonymous:
                        windows::Win32::System::Com::StructuredStorage::PROPVARIANT_0_0_0 {
                            blob: windows::Win32::System::Com::BLOB {
                                cbSize: data.len() as u32,
                                pBlobData: mem,
                            },
                        },
                },
            ),
        },
    };
    let mut pv_out = pv_out;
    match unsafe { store.SetValue(&key, &pv_out) } {
        Ok(()) => match unsafe { store.Commit() } {
            Ok(()) => println!("SetValue + Commit = OK (identical VT_BLOB written back)"),
            Err(e) => println!("Commit ERR: {e:?}"),
        },
        Err(e) => println!("SetValue ERR: {e:?}"),
    }

    // 从 new store 读回确认
    let store2: IPropertyStore = unsafe { device.OpenPropertyStore(STGM(0))? };
    let pv2 = unsafe { store2.GetValue(&key)? };
    let cb2 = unsafe { pv2.Anonymous.Anonymous.Anonymous.blob.cbSize };
    let data2 = unsafe {
        std::slice::from_raw_parts(pv2.Anonymous.Anonymous.Anonymous.blob.pBlobData, cb2 as usize)
            .to_vec()
    };
    println!("readback cbSize = {cb2}, same = {}", data2 == data);
    unsafe { windows::Win32::System::Com::CoTaskMemFree(Some(id.0.cast())) };
    Ok(())
}

#[cfg(windows)]
fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect::<Vec<_>>().join(" ")
}

#[cfg(not(windows))]
fn main() {
    println!("windows only");
}
