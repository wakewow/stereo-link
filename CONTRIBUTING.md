# 参与开发（Contributing）

StereoLink 立体声链是一个 Windows 桌面小工具：把本机任意音频设备组合成左右声道，分别送给两台（或多台）音箱。
欢迎 issue 和 PR。**提交前请先读下面三条硬性约定。**

## 三条硬性约定

1. **不要用 PowerShell 重定向写源码。**
   `Set-Content` / `>` 会按 ANSI 落盘，含中文的 `.rs` 会被写成无效 UTF-8（本项目踩过一次，修了很久）。
   改文件请用编辑器 / IDE，或明确指定 UTF-8 的 API（如 Python 的 `open(..., encoding="utf-8")`）。
2. **中文注释与文案用 UTF-8（无 BOM）**，`.ps1` / `.cmd` 额外要求 **ASCII-only 或带 BOM**：
   Windows PowerShell 5.1 会把无 BOM 的 UTF-8 脚本按 ANSI 解析，中文会变乱码。
   现有 `scripts/gui-set-name.ps1` 就是纯 ASCII 的，可以照抄这个做法。
3. **不要提交任何本机专有信息**：端点 id、设备序列号、蓝牙地址、个人目录、账号令牌。
   默认配置里也不要有具体设备名（见 `crates/stereo-link/src/config.rs` 的约定）。

## 开发环境

- Windows 10/11 x64
- Rust **1.88+**（MSRV，见 `Cargo.toml` 的 `rust-version`）
- 可选：Visual Studio Build Tools（C++ 生成工具）、Windows SDK —— 编译内核驱动时才需要

```powershell
git clone <repo-url>
cd stereo-link
cargo test --workspace --lib      # 单元测试，不需要音频设备
cargo run -p stereo-link          # 启动面板（调试版会开控制台日志）
```

调试日志：

```powershell
$env:STEREOLINK_LOG_CONSOLE = "1"   # 面板日志同时打到控制台
$env:STEREOLINK_LOG = "debug"       # 更详细的 tracing 过滤
```

## 代码风格

- `cargo fmt` 必须通过；`cargo clippy --workspace --all-targets -- -D warnings` 必须干净。
- 公开 API 必须有文档注释（`crates/stereo-audio` 开了 `#![deny(missing_docs)]`）。
- `unsafe` 只用于必要的 Win32/WASAPI 调用，且必须写清楚为什么安全。
- 音频线程里不要 `unwrap()` 会失败的外部调用（端点可能随时被拔掉/切走）——
  参见 `crates/stereo-audio/src/route.rs` 的重试与错误上报写法。
- 中文文案与英文标识符混排时，注意面板布局不要因文案长度变化而抖动（列宽写死）。

## 测试

- 纯逻辑（DSP、解析、配置、选择器、重试退避）请写成 `#[cfg(test)]` 单元测试，CI 会跑 `--lib`。
- 需要真实音频设备 / 蓝牙音箱的验证请放到 `crates/stereo-audio/examples/` 下做成探针程序，
  并在 PR 描述里贴出实际输出（命令 + 关键数字）。
- 不要写依赖本机特定设备名的测试。

## 提交与 PR

- 提交信息建议 `<type>(<scope>): <中文摘要>`，`type` 用 `feat` / `fix` / `docs` / `refactor` / `test` / `chore`；
  破坏性改动加 `!`（如 `feat!:`）。
- PR 请说明：**改了什么、为什么、怎么验证的**；涉及音频链路的请附前后对比数字。
- 面板 UI 改动请附截图（用 PrintWindow 抓窗口本身，不要抓整个屏幕）。

## 内核驱动部分

`drivers/` 是从 [Scream](https://github.com/duncanthrax/scream) 派生的虚拟声卡驱动（MS-PL，见 `NOTICE`）。
它**不在 CI 里构建**：需要 WDK + 自签名证书 + 打开测试签名模式（`bcdedit /set testsigning on`，需重启，
且 Secure Boot 必须关闭）。构建与签名流程见 `drivers/REPORT.md` 与 `drivers/*.ps1`。

改动 `drivers/scream-derived/` 的文件时，请保留文件头部的 MS-PL 声明。

## 许可证

提交代码即表示同意以本仓库的许可证（见 `LICENSE` / `NOTICE`）发布你的贡献。
`drivers/scream-derived/**` 的衍生文件仍按 MS-PL 授权。
