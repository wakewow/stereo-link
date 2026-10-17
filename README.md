# miplay-bridge

把**小米妙播（MiPlay）**上的设备变成 Windows「声音输出」列表里可以直接选择的音频端点，并提供一个网页前端 / **桌面投放面板 exe** 来查看和管理这些设备。

> 用 Rust 重写。**当前唯一路线：PC 系统声音 → 本机局域网 WAV 流 → 小米云 `play_url` → 音箱自己来拉流。**
>
> 日常使用推荐 **`miplay-bridge-desktop.exe`（桌面投放面板）**：双击打开，窗口里点投放、看连接和电平，不用开控制台。

---

## 路线决定（2026-10-17）

用户原话：

> 不使用AirPlay,使用小米妙播,AirPlay的延迟高,放弃AirPlay的路线,仅使用小米妙播的路线

| 路线 | 状态 |
| --- | --- |
| LAN 直推 MiPlay 私有协议 | ❌ 不可行（账号绑定的 Lyra / Mi Connect v3 信任层） |
| RAOP / AirPlay 2 直达 | ❌ **用户否决**（延迟高）。代码保留归档，不再演进 |
| **云 `play_url` + 本机 HTTP 流** | ✅ **唯一路线**，已人耳确认出声 |

详见 `docs/architecture.md`。

## 目标

1. **低延迟** —— 把「PC 出声」到「音箱出声」的延迟压到可接受范围。**当前正在实测拆解各分量**（`research/latency/`）。
2. **Windows 原生音频端点** —— 妙播设备以独立播放设备出现在 `设置 → 系统 → 声音 → 输出`。
3. **网页前端** —— 浏览器里查看账号下的妙播设备、在线状态、音量、当前播放，并能直接投放/停止。
4. **小米账号登录** —— 支持复用已有凭据 / 扫码登录 / 账密登录，凭据本地保存且**绝不入库**。

## 当前状态

| 模块 | 内容 | 状态 |
| --- | --- | --- |
| P0 | 项目骨架、git 版本控制 | ✅ |
| P0 | 小米云 MiNA 客户端（设备发现 / `play_by_url` / 音量 / ubus）Rust 移植 | ✅ 12 测试 |
| P0 | 网页前端（设备卡片 + 播放状态 + 音量） | ✅ |
| P1 | **系统音频回环采集 + 环形缓冲 + 声道分发**（`crates/miplay-audio`） | ✅ 121 测试全绿 |
| P1 | 虚拟音频端点调研（`research/vad/`） | ✅ 结论明确 |
| P1 | MiPlay 协议可行性（`research/miplay/`） | ✅ 不可行，已否决 |
| P1 | 音箱局域网通路探测（`research/lan/`） | ✅ 结论明确 |
| P2 | **妙播拉流 hub 产品化**（`hub` 子命令 + 网页控制） | 🚧 task-15 |
| P2 | **延迟实测与瓶颈拆解** | 🚧 task-14 |
| P2 | **桌面投放面板**（`crates/miplay-desktop`，单 exe = GUI + hub 引擎） | ✅ 真机投放出声 / 28 测试 |
| P2 | 虚拟端点产品化（零驱动改名 / 自签驱动） | ⏸ 非主路径，待定 |
| — | ~~AirPlay 2 发送端（`crates/miplay-airplay`）~~ | 📦 已归档 |

## 目录结构

```
miplay-bridge/
├─ crates/
│  ├─ miplay-core/      # 设备模型、小米账号凭据、MiNA 云 API 客户端
│  ├─ miplay-audio/     # WASAPI 回环采集、RingBuffer、声道分发、WAV 头
│  ├─ miplay-airplay/   # AirPlay 2 发送端（📦 已归档，不再演进）
│  ├─ miplay-desktop/   # 桌面投放面板（eframe/egui；单 exe = GUI + hub 引擎）
│  └─ miplay-server/    # 主程序：CLI + HTTP API + 静态前端 + hub 引擎
├─ web/                 # 网页前端（零构建依赖的静态页面）
├─ docs/                # 架构决策、API 参考、端到端与防火墙说明
├─ research/            # 技术调研（vad / miplay / lan / airplay / latency）
├─ poc/zero-driver/     # 零驱动兜底 PoC（端点改名 + 回环采集 + 云推流）
├─ drivers/             # 自签虚拟音频驱动（Scream 派生，装得上但加载不了）
└─ tools/               # 一次性辅助脚本
```

## 构建与运行

需要 Rust 1.82+（本机已验证 1.99.0）。

```powershell
cargo build --workspace
cargo run -q -p miplay-server -- devices        # 列出账号下的妙播设备
cargo run -q -p miplay-server -- serve --port 8901   # 网页前端
cargo build --release -p miplay-desktop        # 桌面投放面板 exe
```

网页前端默认 `http://127.0.0.1:8900`（`serve` 默认端口），`serve --port 8901` 可改。

## 桌面投放面板（推荐日常使用）

```
cargo build --release -p miplay-desktop
# 双击 target\release\miplay-bridge-desktop.exe
```

一个 exe 里同时跑 GUI 和 hub 引擎：窗口开着 = 8931 在监听 = 播放器随时放歌都有声音。
面板里可以投放/停止、看每台设备的在线/连接/延迟/已发流量、实时左右电平、日志，以及端口/增益/音量/
左右分离/采集端点等设置；关窗口会先让云端 stop 再关流。

详细说明（架构、配置、排障）见 [`docs/desktop-gui.md`](docs/desktop-gui.md)。

## 凭据

程序默认从下面这些位置按顺序查找小米账号凭据（只读）：

1. `%USERPROFILE%\conf\.mi.token`
2. `%APPDATA%\miplay-bridge\.mi.token`
3. 环境变量 `MIPLAY_TOKEN` / `MIPLAY_TOKEN_FILE`

凭据文件格式与 [MiService](https://github.com/Yonsm/MiService) / MiXPlay 兼容：

```json
{
  "deviceId": "…",
  "userId": "…",
  "passToken": "V1:…",
  "micoapi": ["<ssecurity>", "<serviceToken>"]
}
```

> ⚠️ 这个文件里是**明文**凭据。本仓库的 `.gitignore` 已把它排除，请勿手动提交。

## 网络要求

**Windows 防火墙必须为发送端进程放行 UDP 入站**，否则 AirPlay 路径会卡在 SETUP 超时
（历史结论，见 `docs/firewall-requirement.md`）。妙播拉流路径需要放行 **TCP 入站**（音箱来拉流）。

## 参考项目与许可证

| 项目 | 用途 | 许可证 |
| --- | --- | --- |
| [MiService](https://github.com/Yonsm/MiService) (`miservice`) | 小米云 API 的权威参考实现 | MIT |
| [MiXPlay](https://github.com/juneix/MiXPlay) | 妙播桥接、音箱控制、架构参考 | MIT |
| [FusionPlay-Android](https://github.com/rosienosiesie/FusionPlay-Android) | MiPlay 协议参考文献 | **AGPL-3.0（只读参考，禁止抄代码）** |
| [Scream](https://github.com/duncanthrax/scream) | 虚拟声卡驱动派生（`drivers/`） | MS-PL |

本项目的原创代码采用 MIT OR Apache-2.0。

## 与 MiXPlay 的关系

MiXPlay 是一个成熟的同类项目（Python + Rust sidecar），本项目**复用它的经验、但不复用它的进程**：
MiXPlay 让 PC 变成 AirPlay/妙播/DLNA 的**接收端**；本项目的方向相反 ——
让 PC 把音频**送出去**，并且以 Windows 音频端点的形式暴露。

> 注意：早期验证阶段依赖 MiXPlay 的 `POST /api/v1/control/play` 代发云指令；
> 产品化后（task-15）改为**直接用 `miplay-core` 的 `MiNA play_by_url`**，不再需要 MiXPlay 在跑。
