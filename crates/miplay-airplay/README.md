# miplay-airplay — AirPlay 2 发送端（把 PCM 低延迟直推小米音箱）

本 crate 是 miplay-bridge 的**局域网直达通路**：不经过小米云、不需要小米账号，直接把
44.1 kHz 立体声 PCM 推给支持 AirPlay 2 的小米音箱（实测目标：`192.168.1.21`，Xiaomi Sound-左 / L16A）。

目标与实测（见 [`research/airplay/protocol-notes.md`](../../research/airplay/protocol-notes.md) 与
[`research/airplay/tone_run.log`](../../research/airplay/tone_run.log)）：

| 项 | 现状（MiXPlay 云通路） | 本 crate（局域网 RAOP/AirPlay2） |
|---|---|---|
| 数据路径 | 小米云 `player_play_url` 下发 + 音箱回拉本机 HTTP 流 | 本机 → 音箱 RTSP/7000 + RTP/UDP |
| 端到端延迟 | 1~2 s（`sync_delay_ms=2000` 就是它自己配的） | **约 350 ms（估计）**，见下文「实测延迟」 |
| 需要账号 | 是 | **否**（HAP 瞬时配对，无 PIN、无落盘凭据） |
| 需要改音箱 | — | **否**（只读协议；TEARDOWN 后无残留） |

---

## 1. 协议步骤（实测走通的那一条）

每一步都在真机上有响应码佐证，完整原始输出见 `research/airplay/protocol-notes.md`。

```
                 TCP/7000 (RTSP)                      UDP
本机 ─────────────────────────────────────────────────────────────────────
 1. GET  /info                 -> 200  application/x-apple-binary-plist
 2. POST /pair-setup  ×2       -> 200  HAP 瞬时配对（固定 PIN 3939，不落盘）
 3. (控制通道切到 ChaCha20-Poly1305 加密帧)
 4. SETUP#1  (plist)           -> 200  {eventPort, timingPort}
 5. 连 eventPort               (HAP 加密事件通道)
 6. SETUP#2  streams=[...]     -> 200  {dataPort, controlPort}   ← 见「坑」
 7. 连 dataPort / controlPort ──────────► UDP 数据口 + 控制口
 8. RECORD                     -> 200
 9. FLUSH                      -> 200   带 RTP-Info
10. 实时推 RTP 音频包 ──────────────────► UDP 数据口，352 帧/包，ALAC 或 L16
11. 每 1 s 发同步包 (0xd4) ─────────────► UDP 控制口，声明延迟（250 ms = 11025 帧）
12. 每 2 s POST /feedback      -> 200
13. 收尾 POST /teardown        -> 200   不留会话
```

时序是**接收端主动**：音箱按第 4 步声明的 `timingPort` 往本机发 32 字节 NTP 请求
（type `0xd2`），我们必须回 `0xd3`。

### ★ 坑（花了一整轮才定位）：接收端会扣住第 6 步的回复

> cidermill-airplay 在 AppleTV 上的观察，本机在 L16A 上复现一致：
> **`SETUP` 里声明 `timingProtocol: NTP` 与一个端口后，接收端会往该 UDP 口发 32 字节时序请求，
> 并在它被应答之前「扣住自己的 SETUP 回复」。不做时钟同步也行，但这个握手不是可选项。**

如果这一步的入站 UDP 被挡住（**Windows 防火墙默认入站 Block，且只为 python.exe 与三个
bridge EXE 建过规则**），症状是：

* 第 1~5 步全部正常（TCP 出站不受影响），第 6 步**读超时**：
  `io error: ... (os error 10060)`；
* 设备端看起来「不回包」，其实是它在等我们回时序包；
* 反复中断的会话还会让音箱**短暂拒绝后续连接**（`GET /info` 也开始超时），需要几十秒恢复。

**产品必须做这件事**：给 `miplay-bridge.exe` 加入站 UDP 放行（Private + Public），
规则要**绑程序路径**（时序口是临时端口，按端口写规则没用）。产品化要求见
[`docs/firewall-requirement.md`](../../docs/firewall-requirement.md)。本次实测用的临时规则与命令：

```powershell
# 放行（安装器应当自动做这一步）
New-NetFirewallRule -DisplayName "miplay-bridge (AirPlay 时序入站)" `
  -Direction Inbound -Program "<安装路径>\miplay-bridge.exe" -Action Allow `
  -Profile Private,Public -Protocol UDP

# 本轮排查用的临时规则，用完可删：
Get-NetFirewallRule -DisplayName "miplay-bridge probe*" | Remove-NetFirewallRule
```

---

## 2. API

```rust
use miplay_airplay::{AirPlaySender, SenderConfig};

// 建流：HAP 瞬时配对 + SETUP + RECORD，返回时已在推流状态
let cfg = SenderConfig::new("miplay-bridge")   // 设备上显示的名字
    .with_latency_ms(250)                      // 声明延迟，默认 250（= 设备窗口下限）
    .with_codec(Codec::Alac);                  // Alac（默认，与 pyatv 实测路径一致）| Lpcm
let mut tx = AirPlaySender::connect("192.168.1.21:7000".parse()?, cfg).await?;

// 推 PCM：s16le、双声道、44.1 kHz，任意分块（内部按 352 帧切包并实时配速）
tx.send_pcm(&pcm_bytes).await?;                // 有背压；喂太快会 await

tx.set_volume(0.6).await?;                     // 0.0~1.0，映射到设备 dB（0.0 = 静音）
tx.stop().await?;                              // TEARDOWN，消费 self

// 观测
tx.status();          // Status::{Connecting, Streaming, Stopped, Failed}
tx.stats();           // SenderStats：包数/帧数/字节/欠载/丢弃
tx.session_info();    // SessionInfo：data/control 端口、设备自报渲染延迟…
tx.latency_budget();  // LatencyBudget：排队 + 声明 + 设备渲染，及与 400ms 目标的差
```

配套类型：`Codec`、`SenderConfig`、`Error`、`Status`、`SenderStats`、`SessionInfo`、
`LatencyBudget`（`src/latency.rs`，含 `DEVICE_LATENCY_MIN_FRAMES = 11_025` /
`DEVICE_LATENCY_MAX_FRAMES = 88_200` / `TARGET_TOTAL_MS = 400.0`）。

实现分层：协议后端集中在 `src/worker.rs`（唯一依赖 `cidermill-airplay` 的文件），
`src/lib.rs`（API/状态机）、`src/pcm.rs`（s16le↔i32、分块）、`src/latency.rs`（延迟预算）
都是纯逻辑，可单测、可替换。

---

## 3. 实测延迟（真机，`192.168.1.21`）

`cargo run -p miplay-airplay --offline --example tone -- 192.168.1.21 5`
（完整输出：[`research/airplay/tone_run.log`](../../research/airplay/tone_run.log)）

```
建流耗时 573 ms，状态 Streaming
设备给的端口：data=59587 control=34797
RTSP：StreamSetup -> 200 OK / Feedback -> 200 OK / Record -> 200 OK / Flush -> 200 OK
喂入 5.00 秒，实发 632 包 / 222464 帧 / 915136 字节，同步包 6，欠载 12，丢弃 0 字节
平均发包率 126.4 包/秒（理论 125.3）
收尾（TEARDOWN）186 ms
延迟预算：排队 0.0 + 声明 250.0 + 设备渲染 100 ms（估计）（未上报） = 约 350 ms
```

**独立验证（Lead 跑的第二条曲线，含人耳）**：30 秒 587 Hz、振幅 0.80、音量 0.65，
RTSP 全 200，建流 585 ms，喂入 30.01 s / 实发 30.04 s，3764 包 / 1324928 帧 / 5450272 字节，
发包率 125.4/s（理论 125.3），**丢弃 0**、欠载 15，TEARDOWN 78 ms，预算 ~350 ms，
**用户确认听到**。两条曲线在包率、丢弃、预算上一致 ⇒ 配速与背压稳定，不是一次侥幸。

**端到端（PC 系统声音 → 音箱）实测，task-13**：`miplay-bridge stream --target 192.168.1.21 --seconds 12`
（WASAPI 回环连续喂 12.58 s，采集端点 48 kHz/2ch/32-bit float → 44.1 kHz/2ch/s16）：

```
喂入 2218988 B（12.58 s），实发 1509 包 / 531168 帧 / 2185032 B，同步包 13
排队 23.5 ms（稳态 30~40），欠载 0 包，丢弃 91572 B，峰值 0.354 FS，RMS 0.247 FS
TEARDOWN 179 ms
延迟预算：排队 23.5 + 声明 250.0 + 设备渲染 100 ms（估计） = 约 374 ms（目标 ≤400，差 -26）
```

两处必须说清的细节：
* **丢弃几乎全在连接瞬间**：`t=1.0s` 已丢 91504 B，之后 10 秒只再丢 68 B。
  原因见下一条（上限），不是持续丢音频。
* **排队上限从 120 ms 改成 40 ms**：同一场景在旧上限下排队稳稳顶到 109~117 ms，
  端到端 459 ms（**超目标 59 ms**）。压低上限**不增加总丢弃量**（多出来的那部分本来就要丢），
  只是不再「攒着再丢」——换来的就是这 85 ms 延迟。改动见
  [`src/lib.rs` `MAX_QUEUED_MS`](src/lib.rs)。

**延迟构成（哪一项能压、哪一项不能）**

| 项 | 量级 | 可压缩性 |
|---|---|---|
| 排队（我方缓冲） | 0~40 ms（上限 40；`tone` 例子 14~19，系统声音采集 30~40） | 已很小；再压要靠更细的块与更紧的配速 |
| 声明延迟（`latencyMin`） | **250 ms = 11025 帧** | **不能**：这是设备窗口下限，收端按声明值推迟播放 |
| 设备内部渲染（DAC/功放） | 未上报；预算里按 100 ms 估计 | 无法从协议侧压缩；需示波器/麦克风实测 |
| 网络单程 | <1 ms（同一交换机） | 已被声明窗口吸收 |

**诚实评估：距 250 ms 下限还差多少** —— 250 ms 是**声明窗口的下限**，不是端到端可达值。
端到端 = 250（声明）+ 排队（系统声音采集下实测 23~40）+ 设备渲染（未知，估 100）≈ **374~390 ms**；
理论最好情况（设备渲染按 0 算）也 ≈ **275~290 ms**。**所以本设备上 ≤400 ms 达标，但「端到端 250 ms」做不到**；
要到那个量级只能绕开 AirPlay 的实时窗口（另找通路，见 `research/lan/lan-paths.md`）。

**未实测项（不装作测过）**：
* **`~350 ms` 里的 `100 ms` 设备渲染是估计值，不是实测**：本设备本次 SETUP **未上报**
  `arrival_to_render_latency_ms`（`audio_buffer_size`、stream id 同样是 `None`）。
  可确证的只有「声明 250 ms（我们设的）+ 排队 0~19 ms（实测）」≈ **250~269 ms**。
* 本机**没有可用麦克风**（全静默，peak 7/32767，见 `research/lan/`），
  所以「有没有真的出声」靠人耳确认（**已完成**：用户听到 30 秒 587 Hz）；但**声学延迟**（出声比
  数据晚多少）没有仪器测量，仍是估计。
* 音箱 DAC/功放输出级。
* WASAPI 环回采集缓冲（属 `miplay-audio`，不在本 crate）。

---

## 4. 已知限制

* **只有 L16A（`192.168.1.21`）能收**：另一只 Xiaomi Sound L16B（`192.168.1.171`）不广播
  `_raop/_airplay`，7000/7100/554 全拒（实测），只有 miIO(UDP 54321) 与 `_mi-connect._udp`。
  立体声组合是否「推一只整组响」由 Lead 做人耳判定。
* **需要 Windows 入站 UDP 放行**（见第 1 节末尾），否则卡在 SETUP#2。
* **ALAC 不省带宽**：`alac` crate 编的是逐字（verbatim）帧，1412 字节/包 vs 裸 PCM 1408，
  两者都约 1.4 Mbit/s。选 ALAC 只是与 pyatv 实测路径、设备 `ft` 位一致。
* 仅实现 **realtime + NTP** 路径；未实现 buffered/PTP、多房间、元数据（now-playing/artwork）、
  重传（`0x55/0x56`）与 `/feedback` 之外的统计。
* 单会话、单目标；未做重连/自愈（中断即 `Status::Failed`）。
* 音量走 `SET_PARAMETER volume`：这台固件对该写法**偶发 500**（pyatv 参照序列里也出现过一次），
  失败时会返回 `Err`，调用方需容忍。
* 48 kHz / 24 bit 未在真机验证（`StreamFormat` 里有常量，本设备窗口按 44.1 kHz 计）。

---

## 5. 许可（**必须先决策**）

依赖 `cidermill-airplay 0.1.1` 是 **GPL-3.0-or-later**，其 README 说明「版权方可以另议条款，
开 issue 问」。链接它会让本 crate 的**分发**落入 GPL。三条路（详见
[`research/airplay/rust-crates-survey.md`](../../research/airplay/rust-crates-survey.md)）：

1. 接受 GPL-3.0-or-later（自有部分也 GPL）；
2. 找上游要一个宽松/双许可例外（发 issue，成本低，值得先做）；
3. 照 `protocol-notes.md` 自研协议后端替换 `src/worker.rs`（本 crate 的分层就是为此准备的）。

在本仓库**未做决定前不要对外分发二进制**。

---

## 6. 复现

```powershell
# 单元/集成测试（不需要真机，全绿是验收条件）
cargo test -p miplay-airplay --offline

# 真机 5 秒 1 kHz（幅度 0.12、不改音箱音量、结尾 TEARDOWN）
cargo run -p miplay-airplay --offline --example tone -- 192.168.1.21 5

# 只建流、不发音频的协议探针（排查用；变体矩阵见 protocol-notes.md）
cargo run -p miplay-airplay --offline --example setup_probe -- 192.168.1.21 0 6
```
