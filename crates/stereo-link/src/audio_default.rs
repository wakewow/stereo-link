//! 把 Windows 默认输出切到/切回 StereoLink 的虚拟声卡。
//!
//! 左右分声道路由的采集源是虚拟声卡的回环流：只有当系统默认输出就是虚拟声卡时，
//! 播放器（浏览器/音乐软件）的声音才会流进虚拟声卡、被路由链路采到。
//! 忘了切就会出现「面板显示采集中，但音箱没声音」——所以面板负责自动切 + 一键切。
//!
//! 实现方式：内嵌一段 PowerShell 脚本（`scripts/switch-audio-default.ps1`，操作
//! `IPolicyConfig::SetDefaultEndpoint` COM 接口），运行时写到临时目录再调用。
//! 放在脚本里而不是 Rust 里做 COM，是为了少引一套 COM 绑定、脚本也方便单独调试。

use std::path::PathBuf;
use std::process::Command;

/// 内嵌的切换脚本。保持 ASCII-only：Windows PowerShell 5.1 读无 BOM 文件按 ANSI 解析。
const SWITCH_SCRIPT: &str = include_str!("../../../scripts/switch-audio-default.ps1");

/// 虚拟声卡的端点名提示词，**按优先级排列**。
///
/// 第一位是自研驱动「立体声链虚拟声卡」（1.0 起的产品形态）；后面是历史安装：
/// 借用 Steam Streaming Speakers 改名的「立体声链设备」等，
/// 再往后是零安装 D 模式常用的外部虚拟设备（UU远程），
/// 最后兜底是没改过名的 Steam 设备本身（过渡期还能用）。
const VAD_NAME_HINTS: &[&str] = &[
    "立体声链虚拟声卡",
    "立体声链设备",
    "立体声链",
    "StereoLink",
    "UU远程",
    "Steam Streaming",
];

/// `powershell.exe` 启动参数：不加载 profile、非交互、绕执行策略，且不弹黑框。
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 切默认输出的结果（给界面提示用）。
#[derive(Debug, Clone)]
pub struct SwitchOutcome {
    /// 切换前的默认输出端点 id（None = 本来就是虚拟声卡，不用恢复）。
    pub previous_id: Option<String>,
    /// 脚本/系统返回的一句话。
    pub message: String,
}

/// 当前系统默认输出端点。
pub fn default_render() -> Option<stereo_audio::EndpointInfo> {
    stereo_audio::default_endpoint().ok()
}

/// 按名字判断是不是虚拟声卡。
pub fn is_virtual_name(name: &str) -> bool {
    VAD_NAME_HINTS.iter().any(|h| name.contains(h))
}

/// 在一批已枚举的端点里按 [`VAD_NAME_HINTS`] 的优先级挑一块虚拟声卡。
///
/// 界面每秒都会解析当前采集源，不能每次都重新枚举设备——先拿列表里现成的。
pub fn pick_virtual(renders: &[stereo_audio::EndpointInfo]) -> Option<stereo_audio::EndpointInfo> {
    for hint in VAD_NAME_HINTS {
        if let Some(ep) = renders.iter().find(|e| e.name.contains(hint)) {
            return Some(ep.clone());
        }
    }
    None
}

/// 找出虚拟声卡端点（没有就返回 None，比如驱动被卸了）。
///
/// 按 [`VAD_NAME_HINTS`] 的优先级找：自研虚拟声卡优先于历史借用的 Steam 设备，
/// 避免两块都叫「立体声链…」的端点同时存在时选错。
#[allow(dead_code)] // 供示例/诊断脚本使用的旧版 UI 线程回退接口；正式界面走 endpoint_poll 后台线程
pub fn virtual_endpoint() -> Option<stereo_audio::EndpointInfo> {
    let list = stereo_audio::list_endpoints().ok()?;
    let renders: Vec<stereo_audio::EndpointInfo> = list
        .into_iter()
        .filter(|e| e.flow == stereo_audio::DataFlow::Render)
        .collect();
    pick_virtual(&renders)
}

/// 按端点 id 找端点（id 形如 `{0.0.0.00000000}.{guid}`）。
#[allow(dead_code)] // 供示例/诊断脚本使用的旧版 UI 线程回退接口；正式界面走 endpoint_poll 后台线程
pub fn endpoint_by_id(id: &str) -> Option<stereo_audio::EndpointInfo> {
    if id.trim().is_empty() {
        return None;
    }
    stereo_audio::list_endpoints()
        .ok()?
        .into_iter()
        .find(|e| e.id.eq_ignore_ascii_case(id.trim()))
}

/// 解析虚拟声卡端点：优先用记住的端点 id（用户改名后名字变了、端点 id 不变），
/// 记不住再退回按名字提示匹配。
#[allow(dead_code)] // 供示例/诊断脚本使用的旧版 UI 线程回退接口；正式界面走 endpoint_poll 后台线程
pub fn resolve_virtual_endpoint(remembered_id: Option<&str>) -> Option<stereo_audio::EndpointInfo> {
    if let Some(id) = remembered_id {
        if let Some(ep) = endpoint_by_id(id) {
            return Some(ep);
        }
    }
    virtual_endpoint()
}

/// 把默认输出切到指定端点；返回切换前的默认输出端点 id 供之后恢复。
pub fn switch_to_endpoint_id(
    endpoint_id: &str,
    vad: &stereo_audio::EndpointInfo,
) -> anyhow::Result<SwitchOutcome> {
    let current = default_render();
    let already = current
        .as_ref()
        .map(|ep| ep.id.eq_ignore_ascii_case(&vad.id))
        .unwrap_or(false);
    let previous_id = match current {
        Some(ep) if !ep.id.eq_ignore_ascii_case(&vad.id) => Some(ep.id),
        _ => None,
    };
    if already {
        return Ok(SwitchOutcome {
            previous_id: None,
            message: format!("当前默认输出已经是「{}」", vad.name),
        });
    }
    let message = switch_to_id(endpoint_id)?;
    Ok(SwitchOutcome {
        previous_id,
        message,
    })
}

/// 按端点 id 切默认输出（恢复旧设备也走这里）。
pub fn switch_to_id(endpoint_id: &str) -> anyhow::Result<String> {
    run_script(&["-Id", endpoint_id])
}

/// 按名字子串切默认输出（手动调试用；面板走 id，枚举不到端点时可临时用它）。
#[allow(dead_code)]
pub fn switch_to_name(name_like: &str) -> anyhow::Result<String> {
    run_script(&["-NameLike", name_like])
}

/// 把内嵌脚本写到临时目录，返回脚本路径。
fn script_path() -> anyhow::Result<PathBuf> {
    let dir = std::env::temp_dir().join("stereo-link");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("switch-audio-default.ps1");
    // 每次都重写：脚本随 exe 版本变化，不做「已存在就复用」的缓存。
    std::fs::write(&path, SWITCH_SCRIPT)?;
    Ok(path)
}

fn run_script(args: &[&str]) -> anyhow::Result<String> {
    let path = script_path()?;
    let mut cmd = Command::new("powershell.exe");
    cmd.args([
        "-NoProfile",
        "-NonInteractive",
        "-ExecutionPolicy",
        "Bypass",
        "-File",
    ])
    .arg(&path)
    .args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let out = cmd
        .output()
        .map_err(|e| anyhow::anyhow!("启动 powershell 失败：{e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if !out.status.success() {
        anyhow::bail!(
            "切换默认输出失败（{}）{}",
            out.status,
            if stderr.is_empty() {
                format!("：{stdout}")
            } else {
                format!("：{stderr}")
            }
        );
    }
    Ok(if stderr.is_empty() {
        stdout
    } else {
        format!("{stdout}（{stderr}）")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn virtual_name_hints_match_known_names() {
        assert!(is_virtual_name("扬声器 (立体声链虚拟声卡)"));
        assert!(is_virtual_name("扬声器 (立体声链设备)"));
        assert!(is_virtual_name("StereoLink Device"));
        // 零安装 D 模式：借用 UU远程自带的虚拟音频设备当采集源
        assert!(is_virtual_name("扬声器 (UU远程虚拟音频设备)"));
        assert!(is_virtual_name("扬声器 (Steam Streaming Speakers)"));
        assert!(!is_virtual_name("扬声器 (立体声组合)"));
        assert!(!is_virtual_name("扬声器 (Living Room Speaker)"));
    }

    #[test]
    fn embedded_script_is_ascii_only() {
        // PowerShell 5.1 会按 ANSI 读无 BOM 脚本，非 ASCII 会直接解析失败。
        assert!(
            SWITCH_SCRIPT.is_ascii(),
            "scripts/switch-audio-default.ps1 必须保持 ASCII-only（注释也不要写中文）"
        );
        assert!(SWITCH_SCRIPT.contains("SetDefaultEndpoint"));
    }
}
