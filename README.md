# miplay-bridge

把**小米妙播（MiPlay）**上的设备变成 Windows「声音输出」列表里可以直接选择的音频端点，并提供一个网页前端来查看和管理这些设备。

> 用 Rust 重写 / 参考实现，目标是替代当前「小米云下发 + 音箱回拉 HTTP 流」的高延迟方案。

---

## 目标

1. **Windows 原生音频端点** —— 每个妙播设备（小爱音箱、立体声组合…）在 Windows 的
   `设置 → 系统 → 声音 → 输出` 和音量合成器里以独立播放设备出现，任何程序都能直接选它当输出。
2. **低延迟** —— 当前方案体感 1~2 秒（云指令 + 音箱端缓冲）。目标是把这个数量级压下来。
3. **网页前端** —— 浏览器里查看账号下的妙播设备、在线状态、音量、当前播放，并能直接投放。
4. **小米账号登录** —— 支持复用已有凭据 / 扫码登录 / 账密登录，凭据本地保存且**绝不入库**。

## 当前状态

| 阶段 | 内容 | 状态 |
| --- | --- | --- |
| P0 | 项目骨架、git 版本控制 | ✅ |
| P0 | 小米云 MiNA 客户端（设备发现 / 播放控制 / 音量）Rust 移植 | 🚧 |
| P1 | 网页前端（设备列表 + 投放控制） | 🚧 |
| P1 | **虚拟音频端点技术选型**（最大风险点） | 🔬 调研中 |
| P1 | **MiPlay 协议可行性**（能否当 LAN 发送端） | 🔬 调研中 |
| P1 | **音箱局域网直达通路**（RAOP / 本地控制） | 🔬 调研中 |
| P2 | 虚拟端点实现 + 音频管线打通 | ⏳ |
| P2 | 延迟优化与端到端调优 | ⏳ |

调研结论会落到 `research/` 下，并在 `docs/architecture.md` 里汇总成最终架构决策。

## 目录结构

```
miplay-bridge/
├─ crates/
│  ├─ miplay-core/     # 设备模型、小米账号凭据、MiNA 云 API 客户端
│  └─ miplay-server/   # 主程序：HTTP API + 静态前端
├─ web/                # 网页前端（零构建依赖的静态页面）
├─ docs/               # 架构决策、API 参考
└─ research/           # 技术调研（vad / miplay / lan）
```

## 构建与运行

需要 Rust 1.82+（本机已验证 1.99.0）。

```powershell
cargo build --release
cargo run -p miplay-server
```

默认监听 `http://127.0.0.1:8900`。

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

## 参考项目与许可证

| 项目 | 用途 | 许可证 |
| --- | --- | --- |
| [MiService](https://github.com/Yonsm/MiService) (`miservice`) | 小米云 API 的权威参考实现 | MIT |
| [MiXPlay](https://github.com/juneix/MiXPlay) | 妙播桥接、音箱控制、架构参考 | MIT |
| [FusionPlay-Android](https://github.com/rosienosiesie/FusionPlay-Android) | MiPlay 协议参考文献 | **AGPL-3.0（只读参考，禁止抄代码）** |

本项目的原创代码采用 MIT OR Apache-2.0。

## 与 MiXPlay 的关系

MiXPlay 是一个成熟的同类项目（Python + Rust sidecar），本项目**复用它的经验、但不复用它的进程**：
MiXPlay 让 PC 变成 AirPlay/妙播/DLNA 的**接收端**；本项目的方向相反 ——
让 PC 把音频**送出去**，并且以 Windows 音频端点的形式暴露。
