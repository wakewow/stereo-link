# 安全策略（Security Policy）

## 报告漏洞

请**不要**开公开 issue。用 GitHub 的私有渠道：

> 仓库页面 → **Security** → **Report a vulnerability**（Private vulnerability reporting）

请附：影响版本 / 复现步骤 / 是否需要管理员权限 / 你观察到的实际影响。
我会尽快确认；确认后会在修复发布时致谢（除非你希望匿名）。

## 这个程序会碰哪些系统资源（威胁模型）

StereoLink 是**纯本机**工具：不发网络请求、不上传任何数据、不写注册表以外的持久状态（配置写在
`%APPDATA%\stereo-link\config.json`）。但它确实需要一些敏感权限，这也是安全审查的重点：

| 行为 | 位置 | 风险点 |
| --- | --- | --- |
| 读/写音频端点音量、静音 | WASAPI `IAudioEndpointVolume` | 只影响音频，无需提权 |
| 改音频设备默认格式 | `IPolicyConfig::SetDeviceFormat` | 只影响音频，无需提权 |
| 切换 Windows 默认输出 | `scripts/switch-audio-default.ps1`（面板释放到 `%TEMP%\stereo-link\` 后执行） | 脚本内容内嵌在 exe 里；`%TEMP%` 路径可被同用户进程改写 → 理论上存在同用户提权面 |
| 安装虚拟声卡驱动 | `drivers/install-vad.ps1`、`scripts/gui-set-name.ps1` | 需要管理员；会写 `HKLM\SYSTEM\CurrentControlSet\Enum\...`、`pnputil`、`bcdedit testsigning` |
| 内核驱动 `StereoLinkVad.sys` | `drivers/` | **自签名 + 测试签名模式**，不是微软签名的生产驱动；打开测试签名会降低系统整体的驱动加载门槛 |

已知且**有意为之**的限制：

- 内核驱动没有 EV 证书签名，只能靠 `bcdedit /set testsigning on` 加载（需重启、关闭 Secure Boot、
  桌面右下角常驻「测试模式」水印）。**不打算在未签名状态下让普通用户无感安装** —— 文档里明确写了代价。
- 面板不做自动更新、不联网校验，所以不存在「更新通道被劫持」的问题，但也没有远程撤销能力。

## 支持范围

只支持 Windows 10/11 x64。Linux/macOS 下这些 crate 基本为空实现，不提供安全支持。
