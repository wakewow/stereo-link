# Rust AirPlay 发送端库调研（task-10 第一步，限时 45 分钟）

> 结论先行：**找到一个真正能用的 Rust AirPlay 2 发送端库：`cidermill-airplay` 0.1.1**（发送端 + 接收端，
> sans-io，HAP 瞬时配对、加密控制通道、实时 ALAC/LPCM 音频、NTP 计时全都有，并自带
> `examples/send_audio.rs`）。**唯一硬伤是许可证 `GPL-3.0-or-later`**，与本 workspace 的
> `MIT OR Apache-2.0` 不兼容（见文末「许可证」一节，需 lead/用户决策）。
> 其余候选全部是接收端、空壳或 AGPL。

## 1. 调研方法（可复现）

1. crates.io 注册表检索（原始 JSON，非网页摘要）：
   - `https://crates.io/api/v1/crates?q=airplay&per_page=40` → 命中 36 个 crate
   - `https://crates.io/api/v1/crates?q=raop&per_page=30` → 命中 12 个 crate
2. **下载真实源码包逐一读代码**（不看 README 下结论）：
   ```
   https://static.crates.io/crates/<name>/<name>-<ver>.crate   # tar.gz
   解包到 %TEMP%\ap-survey\<name>-<ver>\
   ```
3. 实际编译验证（不是「看起来能用」）：
   ```
   cargo init --name aptest --vcs none && cargo add cidermill-airplay && cargo build
   → Locking 67 packages / Compiling cidermill-airplay v0.1.1 / Finished in 9.22s / exit 0
   ```

判定标准（任务书给定）：① 能做**发送端**（不是接收端）；② 能推**实时 PCM**（不是只能推本地文件）；
③ 许可证是否宽松（MIT/Apache/BSD 好；GPL/AGPL 要注意）。

## 2. 候选逐一判定

| crate | 版本 | 方向（读源码判定） | 实时音频 | 许可证 | 判定 |
|---|---|---|---|---|---|
| **cidermill-airplay** | 0.1.1 | **发送端 + 接收端**（`src/client.rs` 75 KB、`src/session.rs` 47 KB、`examples/send_audio.rs`） | **是**（ALAC + LPCM，`StreamType::Realtime`） | **GPL-3.0-or-later** | ✅ **可用**，许可证需决策 |
| airplay-rs | 0.0.1 | 发送端（`src/sender/*`），但核心是 FairPlay/SRP 旧式配对 + 屏幕镜像 | 部分（`endpoint/audio_stream.rs` 8 KB） | **AGPL-3.0-only** | ❌ AGPL + 0.0.1 + vendor 了 GPL 的 Go 项目 doubletake |
| shairplay | 0.10.0 | 接收端（`raop::handlers_ap1`，「Pure Rust AirPlay **server** library」） | — | — | ❌ 方向相反 |
| openairplay2 / openairplay1 | 0.5.0 / 0.3.0 | 接收端（「audio **receiver** library (network in, PCM out)」） | — | — | ❌ 方向相反 |
| rairplay | 1.0.0 | 接收端（「receiving and processing」） | — | — | ❌ 方向相反 |
| ras | 0.1.1 | 接收端（「AirPlay **server** implementing RAOP」） | — | — | ❌ 方向相反 |
| rplay | 0.0.1 | 空壳（整包 970 字节，`src/main.rs` 45 字节） | — | — | ❌ 无实现 |
| fake-airplay | 0.1.3 | 伪造服务（用于测试发现流程） | — | — | ❌ 非播放 |
| wechat-finder-dlna | 0.4.0 | 伪装成电视**接收**投屏 | — | — | ❌ 方向相反 |
| librespot / 其它 | — | Spotify Connect，与 AirPlay 无关 | — | — | ❌ 协议不同 |

## 3. `cidermill-airplay` 0.1.1 详细判定（读源码所得，非 README 转述）

**为什么它满足需求：**

- `src/client.rs:112 pub use client::{Auth, ControlChannel, MediaSession, SenderConnection};`
  ——完整的 RTSP 客户端：`ControlChannel::builder(host).port(7000).connect()` →
  `authenticate(Auth::Transient(TransientStyle::Ios26), &mut OsRng)`（**这就是我们实测 pyatv 走的
  HAP 瞬时配对，无需 PIN、无需账号**）→ `start_media_session()` → `session_request()`。
- `src/audio/mod.rs:26` 导出 `Action, AudioPacketizer, AudioSendConfig, AudioSendState, DataTransport,
  Step, TimingMode`；`AudioSendState::poll()` 是**状态机**，告诉调用方下一步该发什么
  （`Action::Send` / `ConnectData` / `StartStreaming` / `Finished` / `Failed`）。
- `src/audio/format.rs:156-164`：
  ```rust
  /// The `latencyMin` a sender names, in frames: a quarter second.
  pub fn latency_min(&self) -> u32 { 11_025 }      // 0.25 s @ 44.1 kHz
  /// The `latencyMax` a sender names, in frames: two seconds.
  pub fn latency_max(&self) -> u32 { 88_200 }      // 2.0 s
  ```
  **与我从设备 SETUP 得到的接受窗口完全一致**（latencyMin=11025 / latencyMax=88200），说明它按真机行为写的。
- `examples/send_audio.rs`（165 行，完整可跑）就是我们要的 PoC：`HOST[:PORT] [CREDENTIALS] [SECONDS] [--lpcm]`，
  不给凭据时**自动瞬时配对**，`--lpcm` 走 LPCM（`StreamFormat::PCM_44100_16_STEREO`）。
- 测试策略可信（README「How it is tested」+ `tests/fixtures/` 实际存在）：
  `tests/audio_pyatv_vectors.rs`、`tests/fixtures/airplay-hap-session-pyatv.json` —— 用 **pyatv 0.18.0
  自己产出的字节**当黄金向量，与我这次实测所依赖的参考实现同源；另有 iPhone→lab receiver 的真实抓包夹具。
- 最小 Rust 版本 1.89；本机 rustc/cargo 1.99.0（`cargo build` 实测通过）。

**已知缺口（源码/README 自述，需在 README 里对用户诚实说明）：**

- 不含 FairPlay（作者明确拒绝实现）；后果是「Apple TV/HomePod 拿不到 sender 认证」，但**实测我们的
  小米 L16A 不需要 FairPlay**（pyatv 无 FairPlay 也推流成功），所以对本项目无影响。
- 不含音频编码器（「ALAC frames are verbatim」）→ 用 LPCM 就没有编码器需求；用 ALAC 需要调用方给现成帧。
- 不含 async 运行时，全是阻塞 I/O（`std::net`）→ 我们的 crate 需要用线程或 `spawn_blocking` 包一层。
- 版本 0.1.1、发布仅 2 天、下载量 30（2026-10-16 首发）→ **新，未经社区大规模验证**；风险靠我们的真机实测兜底。
- 作者自述由 AI（Claude）辅助编写，人工做设计决策与验证。

## 4. 许可证（需要决策，不是技术问题）

| 项目 | 许可证 | 对本项目的影响 |
|---|---|---|
| miplay-bridge（本仓库） | `MIT OR Apache-2.0`（`Cargo.toml` `[workspace.package] license`） | 现状 |
| cidermill-airplay | **GPL-3.0-or-later** | 链接它 → 分发整个二进制时必须按 GPL 提供源码 |
| airplay-rs | AGPL-3.0-only | 更严（网络服务也触发） |

作者在 README 里写明：「The copyright holder can grant separate terms; open an issue to ask.」
→ 存在**向作者申请宽松授权**这条路。

**给 lead 的三个选项（本 crate 的实现与它们解耦，可后换）：**

1. **接受 GPL-3.0-or-later**：miplay-bridge 整体按 GPL 分发。仅自用、不分发时几乎无实际影响；
   若将来要开源成 MIT/Apache 项目则不行。
2. **保留为可选后端**：`miplay-airplay` 把 `cidermill-airplay` 放在 feature 后面（如 `gpl-backend`），
   默认关闭；默认实现走自研（从 pyatv MIT 参考实现移植，工作量大，约数天）。
3. **申请授权**：向作者开 issue 要一份非 GPL 授权。

本次 PoC 按选项 1 实现，**并在 `crates/miplay-airplay/README.md` 顶部显著标注许可证影响**，
代码结构按选项 2 预留（协议后端集中在 `session.rs` 一个模块内，替换只影响该文件）。

## 5. 结论

- **存在可用的现成 Rust 发送端实现**，无需从零移植 pyatv：`cidermill-airplay` 0.1.1。
- 采用它 → **跳过任务书的第二步（移植 pyatv）**，直接进入「用库 + 真机实测 + 诚实延迟评估」。
- `research/airplay/protocol-notes.md` 仍按要求产出：记录协议步骤与**加密边界**，来源是本库源码 +
  我此前的 pyatv 真机实测（两者互为印证），而不是空泛复述。

## 6. 原始证据索引

| 内容 | 位置 |
|---|---|
| crates.io 检索原始 JSON | 本文档 §1 的两条 URL（可直接重放） |
| 三份源码包（cidermill-airplay / airplay-rs / rplay） | `%TEMP%\ap-survey\`（临时目录，未入库） |
| 编译验证输出 | 本文档 §1.3（exit 0，9.22 s） |
| pyatv 真机实测（本次任务的协议基线） | `../lan/lan-paths.md`、`../lan/airplay2_stream.json` |
