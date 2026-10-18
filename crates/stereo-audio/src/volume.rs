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

#[cfg(windows)]
mod win {
    use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
    use windows::Win32::Media::Audio::IMMDevice;
    use windows::Win32::System::Com::CLSCTX_ALL;

    use crate::com::{enumerator, resolve, ComGuard};
    use crate::endpoint::{DataFlow, DeviceSelector};
    use crate::error::{AudioError, Result};

    use super::EndpointVolume;

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
            return Err(AudioError::invalid(format!("音量必须是有限数值，收到 {scalar}")));
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
pub use win::{get_endpoint_volume, set_endpoint_mute, set_endpoint_volume};

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
}
