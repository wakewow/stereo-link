//! 自研虚拟声卡（StereoLinkVad，render-only）的改名操作。
//!
//! 改名必须写 `HKLM\SYSTEM\CurrentControlSet\Enum\ROOT\MEDIA\0001`
//! 的 `FriendlyName`，这一步需要管理员令牌；面板本身通常不是管理员，
//! 所以走 `scripts/gui-set-name.ps1`：脚本自己用 UAC 重新启动，然后
//! **自己**完成「备份 Enum 键 → 写 FriendlyName → 重启 AudioEndpointBuilder → 校验」
//! （不调用 `scripts/rename-endpoint.ps1`；那个脚本是命令行工具，备份落在
//! `scripts\backups\`，而本路径的备份落在 `%TEMP%\stereo-link\backups\`）。
//!
//! 端点显示名由 Windows 组装成「扬声器 (设备名)」，所以这里改的是设备名。

use anyhow::{bail, Context, Result};
use std::io::Write as _;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// 虚拟声卡设备的 PnP 实例 id（自研 StereoLinkVad 驱动）。
pub const VIRTUAL_DEVICE_INSTANCE: &str = r"ROOT\MEDIA\0001";

/// 面板里显示的建议名字（对应端点「扬声器 (立体声链虚拟声卡)」）。
pub const SUGGESTED_NAME: &str = "立体声链虚拟声卡";

const RENAME_SCRIPT: &str = include_str!("../../../scripts/gui-set-name.ps1");

/// 校验用户输入的名字：1..=64 个字符、非纯空白、不含路径/非法字符。
pub fn validate_name(name: &str) -> Result<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        bail!("名字不能为空");
    }
    if trimmed.chars().count() > 64 {
        bail!("名字最长 64 个字符");
    }
    if trimmed.chars().any(|c| r#"/\:*?"<>|"#.contains(c) || c.is_control()) {
        bail!(r#"名字里不能有 / \ : * ? " < > | 这些字符"#);
    }
    Ok(trimmed.to_string())
}

fn temp_dir() -> Result<PathBuf> {
    let dir = std::env::temp_dir().join("stereo-link");
    std::fs::create_dir_all(&dir).with_context(|| format!("创建临时目录失败：{}", dir.display()))?;
    Ok(dir)
}

/// 把内嵌脚本落盘（每次覆盖，保证和 exe 同版本）。
pub fn materialize_script() -> Result<PathBuf> {
    let dir = temp_dir()?;
    let path = dir.join("gui-set-name.ps1");
    let mut file = std::fs::File::create(&path)
        .with_context(|| format!("写入脚本失败：{}", path.display()))?;
    file.write_all(RENAME_SCRIPT.as_bytes())?;
    Ok(path)
}

/// 调用脚本改名，等待完成。返回脚本日志末尾若干行，便于在面板里显示。
pub fn rename_virtual_card(new_name: &str) -> Result<Vec<String>> {
    let name = validate_name(new_name)?;
    let script = materialize_script()?;
    let log = temp_dir()?.join("rename.log");
    let _ = std::fs::remove_file(&log);

    let mut cmd = std::process::Command::new("powershell.exe");
    cmd.arg("-NoProfile")
        .arg("-ExecutionPolicy")
        .arg("Bypass")
        .arg("-File")
        .arg(&script)
        .arg("-NewName")
        .arg(&name)
        .arg("-InstanceId")
        .arg(VIRTUAL_DEVICE_INSTANCE)
        .arg("-LogFile")
        .arg(&log)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = cmd.spawn().context("启动改名脚本失败")?;

    // 最多等 120 秒：其中包含用户点 UAC 的时间。
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut exit_code: Option<i32> = None;
    while Instant::now() < deadline {
        if let Ok(text) = std::fs::read_to_string(&log) {
            if let Some(code) = text
                .lines()
                .rev()
                .find_map(|l| l.trim().strip_prefix("EXITCODE="))
            {
                exit_code = Some(code.trim().parse().unwrap_or(1));
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    let _ = child.try_wait();
    let tail: Vec<String> = std::fs::read_to_string(&log)
        .unwrap_or_default()
        .lines()
        .rev()
        .take(8)
        .map(str::to_string)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    match exit_code {
        Some(0) => Ok(tail),
        Some(code) => bail!("改名脚本失败（EXITCODE={code}）"),
        None => bail!("改名超时（可能取消了 UAC 授权）"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_names() {
        assert_eq!(validate_name("  立体声链设备 ").unwrap(), "立体声链设备");
        assert_eq!(validate_name("StereoLink Device").unwrap(), "StereoLink Device");
        assert!(validate_name("").is_err());
        assert!(validate_name("   ").is_err());
        assert!(validate_name("a/b").is_err());
        assert!(validate_name(&"x".repeat(65)).is_err());
    }

    #[test]
    fn embedded_script_is_ascii_only() {
        // Windows PowerShell 5.1 会按 ANSI 读取无 BOM 脚本，中文会乱码 → 必须纯 ASCII。
        assert!(RENAME_SCRIPT.is_ascii(), "gui-set-name.ps1 必须只包含 ASCII 字符");
        assert!(RENAME_SCRIPT.contains("EXITCODE="));
        assert!(RENAME_SCRIPT.contains("FriendlyName"));
        assert!(RENAME_SCRIPT.contains("AudioEndpointBuilder"));
        assert!(RENAME_SCRIPT.contains("RunAs"), "非提权运行时必须自己弹 UAC");
    }
}
