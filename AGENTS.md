# StereoLink 开发规范（AGENTS.md）

本文件是仓库的**强制约定**，人和 AI 协作者都要遵守。改代码前先读一遍；和本文冲突的做法以本文为准。

## 1. 项目形态

| 目录 | 内容 | 平台 |
| --- | --- | --- |
| `crates/stereo-audio/` | 音频引擎：WASAPI 采集 / 路由 / 播放 / 同步（Windows only） | Windows |
| `crates/stereo-link/` | GUI 外壳 + CLI + 无头 `serve` + 控制面 | Windows（GUI），CLI 部分跨平台可编 |
| `receiver/` | 手机投放接收端：`airplay-recv` / `miplay-recv`（**独立 workspace**，许可隔离） | 见 `docs/platform-receiver-feasibility.md` |
| `drivers/` `installer/` | 虚拟声卡驱动、Inno Setup 安装脚本 | Windows |
| `docs/` | 用户文档（`gui.md` / `cli.md` / `receiver.md` / 平台可行性） | — |

一个 `stereo-link` 包产出两个可执行文件：

- `stereo-link.exe`：GUI（Windows 子系统，**没有可用 stdout**）。
- `stereo-link-cli.exe`：控制台程序（`src/bin/stereo-link-cli.rs`），所有自动化都走它。

## 2. CLI 优先原则（硬规则）

> **所有用户可见的功能、状态、日志，都必须能通过 CLI 使用；测试时不允许只靠点按钮。**
> 这是本项目的开发规范，也是验收标准。

具体要求：

1. **动作必须可脚本化。** 界面上能做的操作（启停路由、启停接收端、改配置、播测试音、退出），必须同时有控制面命令 + `stereo-link-cli` 子命令。
2. **状态必须可机读。** 任何界面展示的状态（引擎路由、欠载/丢帧、接收端运行情况、输出声卡、配置、版本、运行时长）都必须能用 `status --json` 拿到，字段名用 `snake_case`。
3. **日志必须可取。** 日志写入 `%APPDATA%\stereo-link\logs\stereo-link.log`，`stereo-link-cli log` 必须能在**没有实例**时直接读文件；接收端（子进程）的输出要经 `ReceiverManager` 转发进同一份日志。
4. **新增功能检查清单**（缺一项不算完成）：
   - [ ] `control.rs` 的 `status_json` / 对应命令返回里加上新状态；
   - [ ] GUI 的 `handle_control` 与 `serve` 的 `State::handle` 都实现该命令；
   - [ ] `cli.rs` 加子命令（含 `--json` 输出）并在 `docs/cli.md` 记一行；
   - [ ] 加至少一个 Rust 单测（解析/构造层）+ 一次真实 CLI 冒烟（写成命令，贴进 PR/记录）。
5. **测试路径固定为无头**：
   ```powershell
   target\debug\stereo-link-cli.exe serve --no-receivers      # 或 --no-routing
   target\debug\stereo-link-cli.exe status --json | ConvertFrom-Json
   target\debug\stereo-link-cli.exe routing stop
   target\debug\stereo-link-cli.exe config set split.queue_ms 60
   target\debug\stereo-link-cli.exe quit
   ```
   只有**视觉**问题（颜色、布局、间距）才允许用截图作为证据；行为与状态必须有 CLI 断言。

## 3. 控制面约定（`crates/stereo-link/src/control.rs`）

- 传输：`127.0.0.1:随机端口` + `%APPDATA%\stereo-link\control.json`（`port` / `token` / `pid` / `mode`）。
- 协议：JSON Lines，一次连接一问一答；请求 `{"token": "...", "cmd": "...", ...}`，响应 `{"ok": true, "data": ...}` 或 `{"ok": false, "error": "..."}`。
- GUI 与 `serve` 内嵌**同一个** `ControlServer`，命令实现分别落在 `StereoLinkApp::handle_control` 与 `serve::State::handle`，两边行为必须一致。
- 退出要善后：停止路由、停止接收端、保存配置、删 `control.json`（`process::exit` 不会跑析构，必须显式 `clear_control_file()`）。
- 写新的 JSON 构造时优先复用 `control::engine_json` / `receivers_json` / `endpoints_json` / `config_summary`；`RouteEngineStats`、`EndpointInfo` 等**没有 serde 派生**，只能在 `control.rs` 里手写映射。

## 4. 日志约定

- 统一用 `tracing`；GUI 与 `serve` 都调用 `logbuf::install()`（内存环形 + 文件层，2 MB 轮转）。
- 控制台输出只在 `STEREOLINK_LOG_CONSOLE=1` 时开启（`serve` 自己会设）。
- 日志级别：用户可见的失败用 `warn`/`error`，正常流程用 `info`（放在 `[模块]` 前缀里，例如 `[投放] 已拉起 AirPlay`）。
- 不要把逐帧/逐包数据打进日志（会刷屏）；需要时用 `-v`（`RUST_LOG`）临时开。

## 5. 构建与测试

```powershell
# 编 GUI + CLI（GUI 在跑时先停，否则 os error 5）
Get-Process stereo-link -ErrorAction SilentlyContinue | Stop-Process -Force
cargo build --workspace
cargo build --workspace --release

# 三件套（CI 同款）
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

接收端是独立 workspace，单独构建（不要塞进主 workspace，许可证与 MSRV 都不同）：

```powershell
cd receiver
cargo build --release -p airplay-recv -p miplay-recv
```

## 6. 接收端与许可

- `airplay-recv`：链接 `shairplay`（LGPL-3.0-or-later）。只以**独立 exe** 形式分发，不静态链进主程序。
- `miplay-recv`：链接 FusionPlay MiPlay SDK（AGPL-3.0-only），且用了 `receiver/vendor/fusionplay/` 里的**已修改副本**（延迟参数环境变量化，见 `receiver/vendor/README.md`）。同样只以独立 exe 分发；修改副本必须保留上游版权头与改动说明。
- 设备名固定 `StereoLink`，输出声卡跟随系统默认（不写死端点名）。

## 7. 文档与提交

- 功能变更要同步：`docs/cli.md`（命令）、`docs/gui.md`（界面）、`docs/receiver.md`（接收端）、`README.md`（用户可见变化）、`CHANGELOG.md`（变更条目）。
- 提交信息用 Conventional Commits（`feat:` / `fix:` / `docs:` / `chore:`），中文正文，`BREAKING CHANGE:` 单独段。
- 不要提交 `target/`、`dist/` 产物、`%APPDATA%` 数据；不要把个人机器路径写进代码（用 `std::env` / `dirs` 推导）。

## 8. 平台现状

- **主程序（GUI + 引擎）目前只支持 Windows**：引擎 89% 的代码在 `#[cfg(windows)]`（WASAPI + 未公开的 `IPolicyConfig`）。Linux/macOS 的移植路径与工作量见 `docs/platform-feasibility.md`。
- **接收端**在 Linux/macOS 的编译与运行结论见 `docs/platform-receiver-feasibility.md`（AirPlay 端几乎零成本；妙播端 macOS 运行会因 5353 端口独占失败）。
- 因此「命令行部署」目前只承诺 Windows；Linux 可部署 CLI + 接收端（需系统 PulseAudio/PipeWire，见 `docs/cli.md` 的 systemd 示例），macOS 有已知限制。
