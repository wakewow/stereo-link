# 上游来源 / Upstream provenance

本目录下的 `scream-derived/` 是 **Scream** 的派生副本（Derivative Work），用于 miplay-bridge 的
「妙播虚拟音频输出端点」v1 方案（见 `../research/vad/vad-options.md` §9.1）。

| 项目 | 值 |
|---|---|
| 上游仓库 | https://github.com/duncanthrax/scream |
| 上游 commit | `d789743c248b11d1df7e5ecc546b1bc60b90cd91`（2026-08-10，"adding freertr's streamer link"） |
| 快照日期 | 2026-10-17（`git clone --depth 1`） |
| 许可证 | **MS-PL**（Microsoft Public License），见 `scream-derived/LICENSE-MS-PL.txt` |
| 派生文件 | `scream-derived/*` 中的全部文件均为上游 `Scream/` 目录的原样副本 |
| 未纳入 | 上游 git 历史、`Receivers/`、`Install/`（预编译二进制）、`tools/`、`doc/`、`.github/` |

## 我们相对上游的改动

（随阶段推进在此登记；**查验差异请以 `git diff` 为准**）

| 阶段 | 文件 | 改动 | 原因 |
|---|---|---|---|
| 阶段 2 | `scream-derived/Scream.vcxproj` | *（未使用，见下）* | 本机没有 WDK 的 VS 集成（BuildTools-only），因此改用 `build.ps1` 直接驱动 `cl.exe`/`link.exe` |
| 阶段 2 | `build.ps1` | 新增（本仓库自己的文件，非上游） | 可复现的无-VS 构建；参数取自上游 `Scream.vcxproj` 与 WDK 的 `WindowsDriver.*.props` |
| 阶段 4 | `scream-derived/Scream.inf` | `[Strings]` 显示名 → `妙播` | 端点显示名来自 INF 的 DeviceDesc（见 `../research/vad/vad-options.md` §8） |

## MS-PL 义务（我们如何遵守）

- 保留版权与许可证全文：`scream-derived/LICENSE-MS-PL.txt`（上游 `LICENSE` 原样）。
- 修改过的文件在文件头或本表登记「已修改」。
- 不以「Scream」名义发布我们的分支（上游商标/名称为描述性使用）；产品名用 `miplay-bridge`。
- 源码形式分发（本项目即源码仓库），符合 MS-PL §3(B)/(D)。

## 关键源码事实（供后来者速查，均已在本地副本核对）

- `scream-derived/savedata.cpp:9-10`：`MULTICAST_TARGET "239.255.77.77"`、`MULTICAST_PORT 4010`
- `scream-derived/savedata.cpp:11-13`：`PCM_PAYLOAD_SIZE 1152`、`HEADER_SIZE 5`、`CHUNK_SIZE 1157`
- `scream-derived/savedata.cpp:415`：内核态 `WskSendTo()` 发 UDP（这就是我们不需要自己写用户态 IPC 的原因）
- `scream-derived/savedata.cpp:478-481`：包头 5 字节（sample-rate marker / 位深 / 声道数 / channel mask）
- `scream-derived/adapter.cpp:86-102`：可从注册表 `HKLM\SYSTEM\CurrentControlSet\Services\Scream\Options` 覆盖 `UnicastIPv4/UnicastPort/UnicastSrcPort`
- `scream-derived/adapter.cpp:501,506`：每个驱动实例注册 `"Topology"` + `"Wave"` 两个 KS 子设备 → **1 实例 = 1 个 render 端点**
