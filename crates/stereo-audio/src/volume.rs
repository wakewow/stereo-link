//! 音频端点音量（`IAudioEndpointVolume`）读写。
//!
//! 为什么需要它：面板会把 Windows 默认输出切到「采集源」（UU远程虚拟音频设备、
//! 自研虚拟声卡…等虚拟设备）。系统音量滑块改的是**采集源端点**的音量；而回环抓到的
//! 数据是写进左右两路目标音箱端点的，音箱响度只由**各目标端点自己的音量节点**决定。
//! 虚拟设备的音量节点不影响回环流，所以滑块对音箱完全无效。要让音量真正联动，
//! 必须直接改目标端点的音量。
//!
//! 这里是最小封装：读 / 写端点主音量与静音，走
//! `IMMDevice::Activate(IAudioEndpointVolume)`，**不需要管理员权限**，也不打断播放流。
//!
//! 另外提供 [`capture_endpoint_curve`]：扫描一只端点「标量 → 端点自报 dB」的真实曲线，
//! 供两只不同型号音箱的音量平衡校准使用——典型曲线（`VOLUME_TAPER`）只是近似，
//! 同一个标量在不同端点上的实际 dB 可以差好几 dB。

/// 端点音量快照（读自 `IAudioEndpointVolume`）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EndpointVolume {
    /// 主音量线性标量，`0.0..=1.0`（Windows 音量滑块的百分比 / 100）。
    pub scalar: f32,
    /// 是否静音。
    pub muted: bool,
    /// 主音量 dB 值（端点不报告时为 `None`，例如部分虚拟设备）。
    pub db: Option<f32>,
    /// 端点报告的声道数（主音量节点；通常与混音格式声道数一致）。
    pub channels: u16,
}

impl EndpointVolume {
    /// 人读描述，例如 `音量 62%（0.620），未静音，-8.3 dB，2 声道`。
    pub fn describe(&self) -> String {
        let pct = self.scalar.clamp(0.0, 1.0) * 100.0;
        let muted = if self.muted { "已静音" } else { "未静音" };
        let db = self
            .db
            .map(|d| format!("{d:.1} dB"))
            .unwrap_or_else(|| "dB 未知".to_string());
        format!(
            "音量 {pct:.0}%（{:.3}），{muted}，{db}，{} 声道",
            self.scalar, self.channels
        )
    }
}

/// 曲线扫描的标量采样点：从最大音量（1.0）一路降到 5%。
///
/// 覆盖常见的听音量程，顺序与 Windows 音量滑块方向一致（响 → 轻）；顺序本身不影响
/// 结果，[`capture_endpoint_curve`] 返回前会把采样点按标量升序重排。
///
/// 扫描会**临时改变端点音量**：实现会先把端点静音再逐点写值，结束后恢复原值，
/// 详见 [`capture_endpoint_curve`]。
pub const CURVE_SCALARS: [f32; 11] = [1.0, 0.9, 0.8, 0.7, 0.6, 0.5, 0.4, 0.3, 0.2, 0.1, 0.05];

/// 规范化曲线采样点：标量夹到 `0.0..=1.0`、丢弃任一维非有限（NaN / ±inf）的点、
/// 按标量升序重排、同一标量只保留输入中**第一个**出现的点。
///
/// 为什么需要：设备会把请求的标量量化到固定档位（例如 0.30 实际报 0.2969），
/// 不同采样点可能落到同一标量；排序与去重让调用方拿到一条可直接插值的单调曲线。
/// 纯逻辑，与 WASAPI 无关，任何平台都能单测。
pub(crate) fn normalize_curve(points: Vec<(f32, f32)>) -> Vec<(f32, f32)> {
    let mut cleaned: Vec<(f32, f32)> = points
        .into_iter()
        .filter(|(scalar, db)| scalar.is_finite() && db.is_finite())
        .map(|(scalar, db)| (scalar.clamp(0.0, 1.0), db))
        .collect();
    // 排序是稳定的：同一标量的多个点保持输入顺序，下面的去重才能稳定地「留第一个」。
    cleaned.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    // `dedup_by` 删除的是「后一个」，因此每个标量保留最先出现的那一点。
    cleaned.dedup_by(|later, earlier| later.0 == earlier.0);
    cleaned
}

#[cfg(windows)]
mod win {
    use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
    use windows::Win32::Media::Audio::IMMDevice;
    use windows::Win32::System::Com::CLSCTX_ALL;

    use crate::com::{enumerator, resolve, ComGuard};
    use crate::endpoint::{DataFlow, DeviceSelector};
    use crate::error::{AudioError, Result};

    use super::{normalize_curve, EndpointVolume, CURVE_SCALARS};

    /// 写后读回允许的误差：部分设备/驱动会把音量量化成固定档位（约 1%～2%）。
    const SCALAR_EPS: f32 = 0.02;

    /// 选择器解析规则：
    /// * `default`（不分大小写）→ 默认播放端点；
    /// * 以 `{0.0.` 开头 → 端点 id（`{0.0.0.00000000}.{guid}`）精确匹配；
    /// * 其它 → 端点友好名子串匹配。
    fn selector_for(selector: &str) -> DeviceSelector {
        let s = selector.trim();
        if s.eq_ignore_ascii_case("default") {
            DeviceSelector::Default
        } else if s.starts_with("{0.0.") {
            DeviceSelector::Id(s.to_string())
        } else {
            DeviceSelector::NameContains(s.to_string())
        }
    }

    /// 音量节点对播放/采集端点都存在；先按播放方向找，找不到再按采集方向找。
    fn resolve_any(selector: &str) -> Result<IMMDevice> {
        let en = enumerator()?;
        let sel = selector_for(selector);
        match resolve(&en, &sel, DataFlow::Render) {
            Ok(found) => Ok(found.device),
            Err(render_err) => match resolve(&en, &sel, DataFlow::Capture) {
                Ok(found) => Ok(found.device),
                // 两个方向都找不到：报播放方向的错误（候选列表更有用）。
                Err(_) => Err(render_err),
            },
        }
    }

    fn activate(dev: &IMMDevice) -> Result<IAudioEndpointVolume> {
        unsafe { dev.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None) }.map_err(|e| {
            AudioError::wasapi(e.code().0, "IMMDevice::Activate(IAudioEndpointVolume)")
        })
    }

    fn read(volume: &IAudioEndpointVolume) -> Result<EndpointVolume> {
        let scalar = unsafe { volume.GetMasterVolumeLevelScalar() }
            .map_err(|e| AudioError::wasapi(e.code().0, "GetMasterVolumeLevelScalar"))?;
        let muted = unsafe { volume.GetMute() }
            .map_err(|e| AudioError::wasapi(e.code().0, "GetMute"))?
            .as_bool();
        // dB 是可选信息：部分虚拟设备不报告，读失败就当没有。
        let db = unsafe { volume.GetMasterVolumeLevel() }
            .ok()
            .filter(|d| d.is_finite());
        let channels = unsafe { volume.GetChannelCount() }
            .ok()
            .and_then(|n| u16::try_from(n).ok())
            .unwrap_or(0);
        Ok(EndpointVolume {
            scalar: scalar.clamp(0.0, 1.0),
            muted,
            db,
            channels,
        })
    }

    /// 读取端点当前音量（`selector` 见模块文档：id / 名字子串 / `default`）。
    pub fn get_endpoint_volume(selector: &str) -> Result<EndpointVolume> {
        let _com = ComGuard::new()?;
        let dev = resolve_any(selector)?;
        let volume = activate(&dev)?;
        read(&volume)
    }

    /// 设置端点主音量并读回校验，返回设置后端点实际报告的 [`EndpointVolume`]。
    ///
    /// `scalar` 会被夹到 `0.0..=1.0`；非有限值直接报错。写后读回误差超过 2% 视为
    /// 「设备没有接受该值」并报错（避免 GUI 显示成功但实际没变）。
    pub fn set_endpoint_volume(selector: &str, scalar: f32) -> Result<EndpointVolume> {
        if !scalar.is_finite() {
            return Err(AudioError::invalid(format!(
                "音量必须是有限数值，收到 {scalar}"
            )));
        }
        let target = scalar.clamp(0.0, 1.0);
        let _com = ComGuard::new()?;
        let dev = resolve_any(selector)?;
        let volume = activate(&dev)?;
        unsafe { volume.SetMasterVolumeLevelScalar(target, std::ptr::null()) }
            .map_err(|e| AudioError::wasapi(e.code().0, "SetMasterVolumeLevelScalar"))?;
        let after = read(&volume)?;
        if (after.scalar - target).abs() > SCALAR_EPS {
            return Err(AudioError::invalid(format!(
                "端点音量写入后读回不一致：请求 {target:.3}，实际 {:.3}（设备可能量化或拒绝）",
                after.scalar
            )));
        }
        Ok(after)
    }

    /// 逐点写标量并读回，**不做 2% 读回校验**，返回端点实际报告的状态。
    ///
    /// 供曲线扫描使用：部分设备会把音量量化到固定档位（例如请求 0.30 实际落在 0.2969），
    /// 用严格的 [`set_endpoint_volume`] 扫描会在第一个这样的点上失败。曲线扫描关心的恰好
    /// 是「设备实际落在哪个标量、报多少 dB」，所以这里只要求写入成功、读得回来。
    fn set_scalar_lenient(volume: &IAudioEndpointVolume, scalar: f32) -> Result<EndpointVolume> {
        if !scalar.is_finite() {
            return Err(AudioError::invalid(format!(
                "音量必须是有限数值，收到 {scalar}"
            )));
        }
        let target = scalar.clamp(0.0, 1.0);
        unsafe { volume.SetMasterVolumeLevelScalar(target, std::ptr::null()) }
            .map_err(|e| AudioError::wasapi(e.code().0, "SetMasterVolumeLevelScalar"))?;
        read(volume)
    }

    /// 扫描一只端点「标量 → 端点自报 dB」的曲线，返回按标量升序的 `(实际标量, 自报 dB)`。
    ///
    /// # 为什么需要
    ///
    /// `VOLUME_TAPER` 只是**典型**音量曲线：同一个标量在不同型号 / 驱动的端点上，端点自报
    /// 的 `GetMasterVolumeLevel()` 实测可以差好几 dB。要把两只不同型号的音箱调到等响，必须
    /// 知道它们各自真实的 dB 曲线，而不能直接套用典型曲线。
    ///
    /// # 副作用：会临时改音量
    ///
    /// 扫描会逐点写端点主音量，**期间端点音量会反复跳变**。为避免打扰用户：
    ///
    /// 1. 先把端点**静音**——`GetMasterVolumeLevel()` 与静音状态无关，静音下照样读得到
    ///    真实 dB，所以用户完全听不到扫描过程；
    /// 2. 结束后**先恢复原标量、再恢复原静音状态**；两步任一失败都会把错误带回调用方
    ///    （此时端点状态可能已和用户设置不一致，必须让上层知道）。
    ///
    /// # 返回与容错
    ///
    /// * 每点都用宽松写（不校验 2% 读回误差，见 `set_scalar_lenient`），设备量化后保留它
    ///   实际落在的标量；
    /// * 端点不报告 dB 的采样点直接跳过；若**所有**点都没有 dB（部分虚拟设备），返回
    ///   [`AudioError::InvalidArgument`] 说明该设备不支持基于 dB 的曲线扫描；
    /// * 返回前由纯函数 `normalize_curve` 夹紧、排序、去重，因此结果一定是标量升序。
    ///
    /// `selector` 规则同 [`get_endpoint_volume`]：`default` / `{0.0.` 开头的端点 id /
    /// 其它按端点名字子串匹配。
    pub fn capture_endpoint_curve(selector: &str) -> Result<Vec<(f32, f32)>> {
        let _com = ComGuard::new()?;
        let dev = resolve_any(selector)?;
        let volume = activate(&dev)?;

        // 先记下用户原来的状态，扫描结束原样恢复。
        let original = read(&volume)?;
        // 先静音：扫描期间音量会反复跳变，静音后用户听不到；读 dB 不受静音影响。
        unsafe { volume.SetMute(true, std::ptr::null()) }
            .map_err(|e| AudioError::wasapi(e.code().0, "SetMute"))?;

        let mut raw: Vec<(f32, f32)> = Vec::with_capacity(CURVE_SCALARS.len());
        let mut scan_err: Option<AudioError> = None;
        for target in CURVE_SCALARS {
            match set_scalar_lenient(&volume, target) {
                Ok(found) => {
                    // 设备不报告 dB：该点无法用于 dB 校准，跳过。
                    if let Some(db) = found.db {
                        raw.push((found.scalar, db));
                    }
                }
                Err(err) => {
                    // 硬错误（设备失效等）：停止扫描，但仍然要把原状态恢复回去。
                    scan_err = Some(err);
                    break;
                }
            }
        }

        // 恢复原状态：先回标量（此时仍静音，用户听不到），再回静音状态。
        let restore_scalar =
            unsafe { volume.SetMasterVolumeLevelScalar(original.scalar, std::ptr::null()) }
                .map_err(|e| AudioError::wasapi(e.code().0, "SetMasterVolumeLevelScalar(恢复)"));
        let restore_mute = unsafe { volume.SetMute(original.muted, std::ptr::null()) }
            .map_err(|e| AudioError::wasapi(e.code().0, "SetMute(恢复)"));
        // 恢复失败的优先级高于扫描失败：端点状态已经和用户设置不一致了。
        restore_scalar?;
        restore_mute?;
        if let Some(err) = scan_err {
            return Err(err);
        }

        let curve = normalize_curve(raw);
        if curve.is_empty() {
            return Err(AudioError::invalid(
                "该端点不报告主音量 dB（GetMasterVolumeLevel 全部失败），不支持曲线扫描",
            ));
        }
        Ok(curve)
    }

    /// 设置端点静音状态并读回校验，返回设置后的 [`EndpointVolume`]。
    pub fn set_endpoint_mute(selector: &str, muted: bool) -> Result<EndpointVolume> {
        let _com = ComGuard::new()?;
        let dev = resolve_any(selector)?;
        let volume = activate(&dev)?;
        unsafe { volume.SetMute(muted, std::ptr::null()) }
            .map_err(|e| AudioError::wasapi(e.code().0, "SetMute"))?;
        let after = read(&volume)?;
        if after.muted != muted {
            return Err(AudioError::invalid(format!(
                "端点静音写入后读回不一致：请求 muted={muted}，实际 muted={}",
                after.muted
            )));
        }
        Ok(after)
    }
}

#[cfg(windows)]
pub use win::{
    capture_endpoint_curve, get_endpoint_volume, set_endpoint_mute, set_endpoint_volume,
};

/// 非 Windows 平台没有 WASAPI，端点曲线扫描不可用；这里保留同签名占位实现。
#[cfg(not(windows))]
pub use non_windows::capture_endpoint_curve;

/// 非 Windows 平台的占位实现（模块本身只在 Windows 上编译，这里只是保持签名一致，
/// 方便上层的按平台分支写法）。
#[cfg(not(windows))]
mod non_windows {
    use crate::error::{AudioError, Result};

    /// 非 Windows 平台恒返回「不支持」。
    pub fn capture_endpoint_curve(_selector: &str) -> Result<Vec<(f32, f32)>> {
        Err(AudioError::invalid("端点曲线扫描仅支持 Windows"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describe_contains_percent_and_state() {
        let v = EndpointVolume {
            scalar: 0.62,
            muted: false,
            db: Some(-8.3),
            channels: 2,
        };
        let s = v.describe();
        assert!(s.contains("62%"), "{s}");
        assert!(s.contains("未静音"), "{s}");
        assert!(s.contains("-8.3 dB"), "{s}");
        assert!(s.contains("2 声道"), "{s}");
    }

    #[test]
    fn describe_handles_missing_db() {
        let v = EndpointVolume {
            scalar: 1.0,
            muted: true,
            db: None,
            channels: 0,
        };
        let s = v.describe();
        assert!(s.contains("100%"), "{s}");
        assert!(s.contains("已静音"), "{s}");
        assert!(s.contains("dB 未知"), "{s}");
    }

    #[test]
    fn normalize_curve_sorts_clamps_and_dedups() {
        let got = normalize_curve(vec![
            (0.5, -10.0),
            (f32::NAN, -3.0), // 标量非有限 → 丢弃
            (0.5, -99.0),     // 同一标量 → 只留第一个
            (1.5, -1.0),      // 越界 → 夹到 1.0
            (-0.2, 2.0),      // 越界 → 夹到 0.0
            (0.2, f32::NAN),  // dB 非有限 → 丢弃
            (0.1, -40.0),
            (0.9, f32::INFINITY), // dB 非有限 → 丢弃
        ]);
        assert_eq!(
            got,
            vec![(0.0, 2.0), (0.1, -40.0), (0.5, -10.0), (1.0, -1.0)],
            "{got:?}"
        );
    }

    #[test]
    fn normalize_curve_handles_empty_and_keeps_sorted_order() {
        // 空输入 / 全是无效点 → 空曲线（调用方据此报「设备不支持」）。
        assert!(normalize_curve(Vec::new()).is_empty());
        assert!(normalize_curve(vec![(f32::NAN, 1.0), (0.5, f32::NAN)]).is_empty());

        // 乱序输入按标量升序输出；设备量化出的标量原样保留，不吸附到采样点。
        let got = normalize_curve(vec![(0.8, -5.0), (0.2969, -18.5), (0.2, -1.0)]);
        assert_eq!(got, vec![(0.2, -1.0), (0.2969, -18.5), (0.8, -5.0)]);
        assert!(got.windows(2).all(|w| w[0].0 <= w[1].0), "{got:?}");
    }
}
