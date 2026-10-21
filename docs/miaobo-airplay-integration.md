# 把「小米妙播」和「AirPlay」集成进 StereoLink —— 方案与工程量

> 状态：方案草案（基于 2026-10-21 的工作区代码 + 三份调研报告）
> 调研底稿（gitignored）：`research/integration/airplay.md`、`research/integration/miaobo.md`、`research/integration/mixplay-arch.md`
>
> ⚠️ **本文是「四方向综述」**（发送端 / 接收端 / 云投放 / 局域网直推），用于横向比较与工程量估计。
> 若你要的是**用户已确认的方向——「手机把音乐投放到电脑」**，请直接看落地版：
> **`docs/phone-to-pc-receive.md`（v2，2026-10-21）**。它修正了本文两处与最新源码证据冲突的地方：
> ① 妙播接收的媒体通道是**PC 出站**连手机 RTSP（不是手机回连 PC），见 §5.3 第 3 条；
> ② FusionPlay SDK **不向调用方交付 PCM**，所以妙播接收的落地方式与工时都变了（v2 §4.3）。
> 本文的方向 A（妙播云投放）、方向 B（AirPlay 接收）结论仍然有效。

---

## 0. TL;DR

1. **先决条件是干同一件事**：当前引擎是「一次采集 → N 路**单声道** i16 → 各写一个 **WASAPI 设备**」。
   而妙播和 AirPlay 都是**立体声网络传输**。所以不论做哪个方向，第一步都是把
   `route.rs` 里写死的「WASAPI sink」和「WASAPI loopback source」抽成 trait，并加一条**立体声 tap**。
   这一步约 **3–5 人日**，是后面所有工作的共同地基。
2. **最快的落地**：妙播**云投放**（PC 声音 → 小爱音箱）。旧仓库 `crates/miplay-core` + `crates/miplay-server/src/hub.rs`
   就是完整实现，`git show 661c59b^:` 可直接恢复，MIT/Apache 干净。**1–2 周**，延迟 0.5–2 s。
3. **最有产品价值**：AirPlay **接收端**（iPhone → PC → 左右分声道）。`shairplay` crate 已实测在
   本机 MSVC 上编译通过（release 示例 5.44 MB）。**2–3 周**，但 **LGPL-3.0 是硬约束**，必须先决策。
4. **最大投入、最大不确定**：妙播**局域网接收端**（手机「投屏/妙播」直接投到 PC，不经小米云）。
   协议已被完整还原（8899 + SafetyAuth + 反向 RTSP + RTP/MPEG-TS/AAC），**5–7 周**，
   但「第三方发送/接收是否被音箱认」在真机上是**已证否过**的，必须先做 spike 再投人。
5. **不要做的两件事**：AirPlay **发送端**（用户 2026-10-17 已明确否决：~355 ms vs 现在 80 ms，
   `661c59b` 已删除）；妙播**局域网发送端**（除非只保 LX06/S12 且必须低延迟）。

| 方向 | 谁播给谁 | 建议 | 工作量 | 延迟 | 许可风险 |
|---|---|---|---|---|---|
| **A** 妙播云投放（发） | PC → 小爱音箱 | ✅ **先做** | 1–2 周 | 0.5–2 s | 无 |
| **B** AirPlay 接收（收） | iPhone/Mac → PC | ✅ **接着做** | 2–3 周 | 0.5–1 s | **LGPL** |
| **C** 妙播局域网接收（收） | 小米手机 → PC | ⚠️ spike 后再说 | 5–7 周 | 0.65–1.5 s | 无（自研） |
| **D** AirPlay 发送（发） | PC → HomePod/小爱 | ❌ 不复活 | 3–4 周 | ~355 ms | GPL-3.0 |
| **E** 妙播局域网发送（发） | PC → 小爱音箱 | ❌ 不进主线 | +3–4 周 | 未知 | 无 |

---

## 1. 先厘清「集成」指哪一方

用户提到的 MiXPlay（`juneix/MiXPlay`）本身**已经**同时支持 AirPlay 与妙播，它和本项目的区别是方向相反：

| | MiXPlay | StereoLink（本项目） |
|---|---|---|
| 角色 | 局域网**接收**中枢：手机 → 中枢 → 音箱 | PC 上的**采集/分发**工具：系统声音 → 多台设备 |
| 形态 | Python hub + 3 个 Rust sidecar（`airplay-bridge`/`dlna-bridge`/`miplay-bridge`）+ 2 个 Cython 扩展 | 单 exe Rust + 虚拟声卡驱动 |
| 卖点 | 跨协议全屋同步、每台输出设备一个虚拟 AirPlay 接收器 | 左右声道**物理分离**到两台音箱、零 SRC |
| 许可 | **专有**（禁止修改/再分发/逆向） | MIT OR Apache-2.0 |

所以「集成进这个项目」只有三种合理解读，本方案按**第二种**展开：

1. ❌ 把 StereoLink 的特性塞进 MiXPlay —— 无必要且不合法（专有许可禁止修改/再分发）。
2. ✅ **把妙播 / AirPlay 的能力做进 StereoLink**（本文档）。
3. ⚠️ 让 StereoLink **调用** MiXPlay —— 不推荐：MiXPlay 的「API」只有 README 一句话
   （`AirPlay、MiPlay、DLNA、OwnTone、API`），无公开文档；免费档 `acquire_source` 只允许一条管道；
   强行依赖等于把单一 exe 改成需要装 Docker/Python 环境。

---

## 2. 现有代码的接入点（最关键的一节）

### 2.1 现在的数据流

```text
播放器 → 虚拟声卡端点（Windows 默认输出）
           │ WASAPI loopback，按端点原生混音格式（通常 48000/2ch/32f）
           ▼
      capture_loop()                    crates/stereo-audio/src/route.rs:2040
           │ CaptureFormat + CaptureDsp 转成 left[]/right[] i16
           │ 每帧 mix_sample(channel) → 单声道 i16 → 每路一个 VecDeque<i16>
           ▼                                route.rs:2197-2222
      SinkState.queue: VecDeque<i16>     route.rs:871
           │ sink 线程 pop → DelayLine → 编码
           ▼
      SinkOutput::AutoConvert / Converted   route.rs:1433
           │ encode_native_stereo() / MixConverter
           ▼
      WASAPI IAudioRenderClient 写缓冲     route.rs:1929
```

GUI 侧只有一条路径：`app.rs:1461 route_engine_config()` 把 `SplitConfig{left_endpoint, right_endpoint}`
翻译成两条 `RouteConfig`，然后 `RouteEngineHandle::start()`（`app.rs:1527`）。
（`btsplit.rs` 是旧 API，源码自己写了「本模块为保留旧 API 而存在」，GUI 已不引用。）

### 2.2 三个硬编码点 = 三处必须改的地方

| # | 位置 | 现在写死的东西 | 为什么网络传输不行 |
|---|---|---|---|
| 1 | `route.rs:144` `RouteConfig.endpoint: String` | 一个 **WASAPI 友好名子串** | 没有「这台设备是网络设备」的判别，GUI 的设备下拉也只列 WASAPI 端点 |
| 2 | `route.rs:1762-1766` `sink_loop` | 无条件 `ComGuard` + `com::enumerator()` + `com::resolve(..DataFlow::Render)` | 网络 sink 不需要 COM/WASAPI，且**要先建连、再按协议协商采样率** |
| 3 | `route.rs:2040` `capture_loop` | 只有一个采集源：WASAPI loopback | AirPlay/妙播接收端会**多出第二个音频源**（网络进来的 PCM），要能并存、能切 |

### 2.3 第四个坑（容易忽略）：**每路只有单声道**

`SinkState.queue: VecDeque<i16>`（`route.rs:871`）存的是 `mix_sample(Left|Right|Both)` 之后的
**单声道**样本：`Both` 是 `(L+R)/2` 下混。这对「一台音箱放一个声道」是对的，
但**妙播和 AirPlay 的音源是立体声**——直接把网络音频塞进现有队列，会先被下混成单声道。

因此网络输出需要一个**在声道拆分之前**的 tap：

```rust
// 建议新增（伪代码）
pub trait PcmTap: Send {                       // 立体声网络出口
    fn push_stereo(&mut self, l: &[i16], r: &[i16]);  // 采集块原生帧，不拆声道
}
pub trait NetworkSink: Send + 'static {        // 独立线程长跑，自带重连
    fn run(self, rx: mpsc::Receiver<StereoBlock>) -> Result<()>;
    fn describe(&self) -> String;
}
```

推荐做法：`Shared` 里新增 `taps: Vec<Arc<Mutex<dyn PcmTap>>>`，在 `route.rs:2197`
那个循环**之前**喂一次 `(dsp.left, dsp.right)`。这样既不动现有的左右分声道路径，
又让「妙播/ AirPlay 输出整段立体声」成为可能。

### 2.4 采样率与声道数：先说清三条不变量

- 引擎采样率 `shared.rate` 是**全局唯一**的，且默认跟随采集源原生混音率（`route.rs:1281`，通常 48 kHz）。
- 妙播要求 **AAC-LC 48 kHz / 2ch**（WFD 协商里写死），AirPlay 传统上是 **44.1 kHz ALAC/S16**。
  所以：**妙播天然匹配**；**AirPlay 需要一个 48k→44.1k 的 SRC**（`rubato` 或已有 `StreamResampler`）。
- 反过来，如果未来要把 AirPlay 当**第二个采集源**（方向 B），它的 44.1 kHz 与本机 48 kHz
  会同时存在 → 引擎的「单一 rate」假设要放宽成「每路自己的转换」，这条改动比想象中大，要提前排。

---

## 3. 方向 A：妙播云投放（PC 声音 → 小爱音箱）—— 先做这个

### 3.1 原理（一句话）

`音频不经云、控制经云`：本机起一个 HTTP 服务把 PCM 包成 WAV 流，再用小米账号的 serviceToken
调米家云的 `mediaplayer.player_play_url`，让音箱**主动来拉**这个 URL。

```text
PC: WASAPI 采集 → 5 s 环形缓冲 → HTTP GET /hub/<did>.wav（假 Content-Length 0x7FFFFF00）
                                          ▲
小米云 api2.mina.mi.com  ── play_url ─────┘  音箱自己 1× 实时拉流并解码
```

### 3.2 直接可复用的旧代码（MIT/Apache，自己的）

```bash
git show 661c59b^:crates/miplay-core/src/mina.rs      # 459 行：MiNA 云 API + play_by_url
git show 661c59b^:crates/miplay-core/src/token.rs     # 179 行：.mi.token 解析/刷新
git show 661c59b^:crates/miplay-core/src/device.rs    # 229 行：device_list / did / MiNA
git show 661c59b^:crates/miplay-server/src/hub.rs     # 1652 行：HTTP 流服务 + 重发 + 防火墙
git show 661c59b^:crates/miplay-server/src/hub/engine.rs  # 1032 行：状态机 + 电平 + 指令
```

关键常量都在 `hub.rs`：`DEFAULT_PORT=8931`、`SAMPLE_RATE=44100`、`PREFILL=1200ms`、`REISSUE_AFTER=8s`、
`RING_SECONDS=5.0`、`STREAM_CHUNK=32KiB`、`PUMP_CHUNK=1764`。

### 3.3 要做的事（按顺序）

1. 恢复 `miplay-core` 为 `crates/stereo-xiaomi`，只留 `mina/token/device`（去掉 web 与 CLI 依赖）。
2. 恢复 `hub.rs` 的 HTTP 流服务，但**输入改成本引擎的立体声 tap**（第 2.3 节），
   而不是自己去 WASAPI 采集——这样它天然复用虚拟声卡的采集，且可以和左右分声道**同时**工作。
3. **重采样 48k → 44.1k**：现有 `hub` 硬编码 44100（旧实现是采集端直接 44.1k）。
   要么让 tap 输出 44.1k（`StreamResampler`），要么把 WAV 头写成 48k（音箱是否接受 48k WAV 需实测）。
4. 凭据：沿用 `%USERPROFILE%\conf\.mi.token` / `%APPDATA%\stereo-link\.mi.token` / `env MIPLAY_TOKEN`，
   与 MiService/MiXPlay 兼容（JSON：`deviceId/userId/passToken/micoapi:[ssecurity,serviceToken]`）。
   **落盘要加密**（旧实现是明文，这是要修的）。
5. 防火墙：放行 **TCP 入站**（端口可配，建议 8931+），按**程序路径**建规则更稳（见 §7.1）。
6. GUI：设备列表（`device_list` 的 did）多选 +「延迟补偿」滑块（旧实现默认 `sync_delay_ms=2000`）。

### 3.4 已经知道的坑

- 音箱按 **1× 实时**拉流、**不预取**；静默 6.4 s 会主动断开 → 必须持续喂静音，不能停。
- `REISSUE_AFTER=8s`：音箱侧播放状态容易被云判为「已结束」，需要周期性重发 `play_url`。
- WAV 头的 `dataSize` 必须写假长度 `0x7FFFFF00`，否则音箱拉到 EOF 就停。
- 立体声组合（两只音箱）**没有 group 概念**，要对每个 did 各发一条指令。
- 延迟 0.5–2 s 不可消除：云触发→首个 GET 实测 125–172 ms，音箱自身抖动缓冲 650–1500 ms。

---

## 4. 方向 B：AirPlay 接收端（iPhone/Mac → PC）—— 接着做

### 4.1 为什么先做接收而不是发送

- **不需要 MFi 芯片、不需要 Apple 授权**：开源实现广播 `et=0,3,5`（故意不带 `et=4` MFi-SAP），
  不实现 `/auth-setup`，iPhone 照样能连。
- **不需要 Bonjour/Avahi**：Windows 上用纯 Rust 的 `mdns-sd`（Apache-2.0 OR MIT）即可。
- 发送端历史上真机跑通过（~355 ms），但用户已否决，见 §6。

### 4.2 两条实现路线

**路线 B1：内嵌 `shairplay`（快，但 LGPL）**

- crate：`shairplay 0.10.0`，**LGPL-3.0-or-later**，lib 形态，MSRV 1.94。
- 本机实测：`x86_64-pc-windows-msvc` / rustc 1.99，`--features ap2` **编译通过**（dev 23.42 s）；
  release 完整接收端示例 `player.exe` **5.44 MB**（43.15 s）。
- 输出是 **f32 交错 PCM**，正好可以当「第二个采集源」。
- **硬约束**：单 exe 静态链进 LGPL 会触发「可重链接 / 提供目标文件」义务 → 三选一：
  ① 产品整体改 LGPL（与 MIT OR Apache-2.0 和商业分发冲突）；② 接收模块做成 **DLL 动态链接**；
  ③ 放弃 B1 走 B2。**这个决策必须写进立项文档，不能拖到发版前。**
- MSRV 冲突：仓库 pin 1.88，库要 1.94 → 可试 `shairplay 0.7`（MSRV 1.88，`ap2` feature 相同，未实测）。

**路线 B2：自研 AP1-only 接收端（干净，但慢）**

- 组件：`mdns-sd` + `rsa` + `aes/cbc` + ALAC 解码（`symphonia` 是否支持 ALAC 待验证）。
- AP1(RAOP) 只需：RTSP `OPTIONS/ANNOUNCE/SETUP/RECORD/FLUSH/TEARDOWN/SET_PARAMETER`、
  `rsaaeskey` 解密、AES-128-CBC、ALAC 44.1k/S16/2ch、RTP + 同步包 `0x54/0xd4`。
- 工作量 3–6 周协议 + 1–2 周联调；**无 AP2**（现代 iOS 仍兼容 AP1 接收端）。
- 全宽松许可，且代码完全可控——长期看这是正确选择。

### 4.3 工作量

| 阶段 | B1（shairplay） | B2（自研 AP1） |
|---|---|---|
| 库接入 / 协议实现 | 3–5 人日 | 3–6 周 |
| 真机联调（iPhone + 两台音箱） | 3–5 人日 | 1–2 周 |
| GUI（设备名、配对态、托盘常驻） | 1–2 人日 | 1–2 人日 |
| **合计** | **2–3 周** | **5–8 周** |

### 4.4 mDNS 与协议要点（实现时照抄）

- `_raop._tcp`：`txtvers=1 ch=2 cn=0,1 et=0,3,5 sv=false da=true sr=44100 ss=16 vn=65537 tp=TCP,UDP md=0,1,2`
- `_airplay._tcp`：`deviceid/features/flags/model/pk/srcvers/vv=2/sf`
  （`sf` 用 `0x004/0x204/0x604` 区分配对态，可避免 iOS 反复配对导致的 ~11 s 卡顿）
- RTSP 默认端口 5000，建议改 7000（避开 Windows 上其他软件占用）。
- 接收缓冲建议 **250–500 ms**；发送端会声明 `latencyMin=11025`(0.25 s) / `latencyMax=88200`(2 s)。
- **注意**：`shairplay` 的 PTP 只绑定/排空 UDP 319/320，**offset 没接进播放**
  → 没有真时钟同步、没有多房间。要同步得自己接。

### 4.5 与「左右分声道」结合时的真实问题

- 好处：接收端 PCM 直接进 tap，**天然能左右分给两台音箱**，这是本项目独一无二的价值。
- 新问题：**时钟域变成三个** —— AirPlay anchor(PTP/NTP) × 本机声卡晶振 × 两台蓝牙音箱各自晶振。
  现有 `sync.rs` 只做**一次性静态校准**（麦克风互相关），跟不住漂移。
- 折中：先做「一次性静态对齐 + 队列丢帧吸收」（现有机制），端到端 0.5–1.0 s；
  要做长稳，得加一个**慢速比例校正**（旧的 `hub/engine.rs` 里已有类似思路可参考）。

---

### 4.6 MiXPlay 的架构给出一条产品级启发（值得抄思路）

MiXPlay 的核心不是「协议支持多」，而是**每台输出设备各开一个虚拟 AirPlay 接收器**：
官方截图里 iPhone 的隔空播放列表同时出现「全屋播放 2 / 主卧 L / 客厅 / 小爱 Art-白」，
控制台里每个胶囊对应一组端口（`52500/8899/49152`、`52501`、`52540`、`52542` …）。

对 StereoLink 的直接启发：**不要只做一个接收端，而是做「左右两个」**——

```text
iPhone 隔空播放列表
  ├─ StereoLink 左   → 只收 L 声道 → 本机左音箱 / 蓝牙 A
  └─ StereoLink 右   → 只收 R 声道 → 本机右音箱 / 蓝牙 B
```

这样用户可以在 iPhone 上分别选左右，或者用 AirPlay 自身的多房间把同一首歌送到两个接收器。
代价是：两个 mDNS 实例（不同 deviceid/端口）+ 两套 RTSP 会话 + 两条独立的时钟域，
比单实例多约 **3–5 人日**，并且「左右两只箱子之间的相对延迟」要由我们自己校准
（AirPlay 多房间同步本身不保证跨接收器样本级对齐）。

另外可抄的两个点：多实例的**端口分配思路**（基数 + 偏移，避免临时端口冲突）、
`miair-next` 那种「组件—来源—许可」三段式清单写法（本项目 §8 已采用）。

---

## 5. 方向 C：妙播局域网接收端（小米手机 → PC，不经云）—— 高投入、先 spike

### 5.1 协议已经完整还原（可自研，无许可问题）

| 层 | 事实 |
|---|---|
| 发现 | mDNS `_mi-connect._udp` @56666、`_lyra-mdns._udp` @5353；TXT 含 `dev/apps/appsData(mico.device_id=MiNA)` |
| 控制 | **TCP 8899**，帧 `24 <outer:1> <cmd:1> <seq:u16be> <len:u32be> <body>`；body 上界 16 MiB |
| 鉴权 | legacy 明文 `0x28→0x36/0x29→0x1e→0x37→0x58…`；兼容安全握手 `0x14/0x00,0x01,0x02,0x03`；现代分支 `0x1400-0x1403` + SafetyData |
| 加密 | AES-128-CBC，key=`auth_key[:16]` ASCII，**链式 IV**（`iv := 上次密文末 16B`），0x00 填充，body 前有 **私有 CRC-32**（非反射 `0x04C11DB7`，init `0xFFFFFFFF`，无终异或） |
| auth_key | **TCP 四元组的纯函数**：`hex(md5(每位数字字符+0x31 of "lip lport rip rport"))`，与账号/证书无关 |
| 开流 | `OPEN(0x00)` 密文，body `wfd://<发送端IP>:<端口>?mirrorMode=1` → **接收端反连我们的 RTSP 服务器** |
| 媒体 | `'$' <ch> <len:be16>` + RTP（clock **90000 Hz**）→ 188 B MPEG-TS（**PID 0x1100**）→ PES `00 00 01 C0` → **AAC-LC 48k/2ch**；可选每 PES 前 `min(len,256)/16*16` 字节 AES-128-CBC |
| 缓冲 | 接收端常量 650/800/1500 ms，`MAX_OUTPUT_LATENCY=250 ms`，同步死区 2 ms，漂移窗口 12 s / 速率修正上限 0.25% |

### 5.2 为什么必须先 spike

历史上**发送端**方向在真机上是**已证否**的：8899 上 SAFETY 互认证通过后，控制指令全部静默、
`OPEN` 无 `OPEN_ACK`、无 RTSP 回连、连接被断开。卡点被定位为**小米账号绑定的 Lyra / Mi Connect v3 信任层**
（互连只走到 5 个阶段中的第 2 个）。

但新证据**修正了这个结论**：LX06/S12 真机抓包显示发送端走的是**纯 legacy 明文、无 SafetyAuth/Lyra**，
失败点是首帧 `0x0058` 的**源身份形状**（105 B vs 73 B；假身份立刻 `10053`）——
即「数学上不可能」是错的，卡的是「身份像不像 Xiaomi 自家进程」。

对**接收端**方向（手机投给 PC）而言，这个风险更低：控制通道上我们是**被连**的一方（手机连 PC:8899），身份要求也不同。
但仍需先验证：「现代 HyperOS 手机是否愿意连一个第三方兼容接收端」。

> 方向澄清（v2 修正）：控制通道 PC 被连，**媒体通道 PC 是出站方**——`OPEN` 里带的 `wfd://<手机IP>:<port>` 是手机的地址，PC 要主动去连手机的 RTSP server。详见 `docs/phone-to-pc-receive.md` §4.2/§4.3（含「SDK 不给你 PCM」这一关键限制）。

### 5.3 spike 清单（**先做，2–3 天就能有结论，决定要不要投 5–7 周**）

1. 用 `mdns-sd` 宣告一个 `_mi-connect._udp` 接收端（含正确 TXT `dev/apps/appsData`，**必须带 `mac=`**）。
2. 起 TCP 8899，走完 legacy 明文 + SafetyAuth 两条路径，验证手机是否进入 `OPEN`。
3. 收到 `OPEN` 后，从它的 `wfd://<手机IP>:<port>` 里取地址，**PC 主动出站连手机的 RTSP server**，走完 OPTIONS/GET_PARAMETER/SET_PARAMETER/SETUP/PLAY。
4. 收到第一包 RTP/MPEG-TS，用 `ffmpeg` 落盘验证 AAC 可解。
5. **同时验证一台 L16A/L16B 和一台老 LX06**（音箱型号差异大）。

只有 1–4 全绿，才进入 5–7 周的正式实现。
（FusionPlay-Android 的 7 个 `.rs` 文件可当**协议说明书**读，但**不能抄**——AGPL-3.0。）

### 5.4 如果做，工作量

帧+加密+auth_key 1.5 周｜8899 会话状态机 1.5 周｜mDNS 宣告+持久身份 4 天｜
反向 RTSP+RTP/TS/PES+媒体解密 1.5 周｜AAC 解码+重采样+时钟恢复+抖动缓冲 1 周｜
接 tap 拆 L/R + 延迟校准 4 天｜控制回传（音量/暂停）4 天 → **≈5–7 周**。

依赖：`aes`/`cbc`、`md-5`、`hmac`/`sha2`、`crc`、`mdns-sd`、`prost`、`symphonia`、`rubato`（全 MIT/Apache）。

---

## 6. 不建议做的方向（写在这里避免反复讨论）

### 6.1 AirPlay 发送端（PC → HomePod / 小爱 Sound）

- 历史代码 `crates/miplay-airplay`（589 行 `lib.rs` + `latency.rs`/`pcm.rs`/`worker.rs`）真机跑通过：
  TCP/7000 RTSP → `GET /info` → `pair-setup` ×2（固定 PIN 3939）→ ChaCha20-Poly1305 控制通道 →
  `SETUP`/`RECORD` → RTP（352 帧/包，ALAC 或 L16）→ 每秒 `0xd4` 同步包 → `/feedback`。
- **否决理由**：实测延迟 ~355 ms，而当前虚拟声卡链路只有 ~80 ms；用户 2026-10-17 明确决定
  「不使用 AirPlay，AirPlay 的延迟高，仅使用小米妙播的路线」，`661c59b` 已删除该 crate。
- 若将来要复活：发送端库候选 `cidermill-airplay`（**GPL-3.0-or-later**）、`airplay-rs`（**AGPL-3.0**），
  许可全都不兼容 MIT OR Apache-2.0。

### 6.2 妙播局域网发送端（PC → 小爱音箱，低延迟）

- 只有「必须低延迟 + 只保 LX06/S12」时才值得；关键未知是 `mSourceBtMac` 的算法（未破译）。
- 工程量在方向 C 之上再 **+3–4 周**（要写 RTSP server + AAC 编码 + 防火墙 49555–49565）。

---

## 7. Windows 上的坑（按踩到的概率排序）

### 7.1 防火墙：必须**按程序路径**放行入站，不是按端口

- AirPlay 的 data/control/timing 端口都是**临时端口**，按端口建规则无效；
  妙播要音箱来拉 HTTP，也要 TCP 入站。
- 症状：`io error: ... (os error 10060)`（连接超时）；反复中断会让音箱**短暂拒绝连接**。
- 仓库历史 `git show 661c59b^:docs/firewall-requirement.md` 有真机根因与现成 `netsh`/PowerShell 命令。

### 7.2 mDNS

- 不需要装 Bonjour，但 **UDP 5353** 要给程序放行；与已装 Bonjour / iTunes 共存需实测。

### 7.3 多网卡

- 多网卡（WSL / Hyper-V / VPN 虚拟网卡）会让 iPhone 连到错网卡 →
  `shairplay` 的 `BindConfig::addrs([LAN IP])` 必须显式绑 LAN IP。

### 7.4 单 exe / 无后台服务

- 窗口一关，iPhone 立刻看不到设备 → 需要**托盘常驻**（项目已有 `tray.rs`）。

### 7.5 音频并存

- 接收端开流时，本机其它播放是否会闪断/切采样率；虚拟声卡作为默认输出时的回环状态要测。

---

## 8. 依赖与许可决策表

| 用途 | 候选 | 许可 | 与 MIT OR Apache-2.0 兼容 | 决策 |
|---|---|---|---|---|
| AirPlay 接收（快） | `shairplay 0.10.0` | **LGPL-3.0-or-later** | ⚠️ 静态链接不兼容 | DLL 动态链接 或 整体改许可 |
| AirPlay 接收（干净） | 自研 AP1 | 自有 | ✅ | 长期方案 |
| AirPlay 发送 | `cidermill-airplay` / `airplay-rs` | GPL-3.0 / AGPL-3.0 | ❌ | 不做 |
| 妙播云 | 自有 `miplay-core`（恢复） | MIT/Apache | ✅ | 直接复用 |
| 妙播局域网 | 自研 | 自有 | ✅ | spike 后决定 |
| mDNS | `mdns-sd` | Apache-2.0 OR MIT | ✅ | 采用 |
| 重采样 | `rubato` | MIT | ✅ | 需要时引入 |
| 参考实现（只读） | `FusionPlay-Android` | **AGPL-3.0** | ❌ **一碰传染** | 只读事实，不抄代码 |
| 参考实现（只读） | `Para-Ecoli/miplay-bridge` | **CC BY-NC-SA 4.0** | ❌ 禁商用 | 只读事实 |
| 参考实现（只读） | `SUlTlUS/MiPlayForWindows` | **无许可** | ❌ 默认保留所有权利 | 只读 `docs/` |
| 参考实现 | `juneix/MiXPlay` | **专有** | ❌ 禁修改/再分发 | 只读公开文档 |

**合规红线（三条）**：

1. AGPL 代码一碰，整个单 exe 必须 AGPL（含 §13 网络服务条款），与商业分发不可调和。
2. 逆向只做「互操作事实」：**不分发固件/证书/密钥**、不做「免账号登录」。
3. 文案写「兼容小米妙播协议」「兼容 AirPlay 协议」，**不写**「官方支持」「小米授权」。

> 参考：MiXPlay 自己就有许可事故——`README` 说 FusionPlay 是 AGPL，
> `THIRD-PARTY-NOTICES.md` 却标成 MIT；PyPI 元数据写 `License: MIT` 而仓库是专有。
> 抄它的架构可以，抄它的许可声明会踩雷。

---

## 9. 里程碑与总工作量

| 阶段 | 内容 | 产出 | 人日 | 依赖 |
|---|---|---|---|---|
| **M0** | 引擎抽象：`Source`/`Sink` trait + 立体声 `PcmTap` + 配置 schema 迁移 | 改动 `route.rs`（≤300 行）+ `config.rs` + GUI 设备下拉 | **3–5** | — |
| **M1** | 妙播云投放（恢复 `miplay-core` + `hub`，接 tap） | PC 声音 → 小爱音箱 | **5–10** | M0 |
| **M2** | AirPlay 接收 spike（shairplay，许可证走 DLL） | iPhone → PC → 左右分声道 demo；可选「左右两个虚拟接收器」（§4.6） | **2–3 周**（+3–5 人日） | M0 + 许可决策 |
| **M3** | 妙播局域网 spike（5.3 节 5 条） | Go / No-Go 结论 | **1–2 周** | M0 |
| **M4** | （若 M3 通过）妙播接收端正式实现 | 小米手机 → PC | **5–7 周** | M3 |
| **M5** | 统一「网络设备」GUI：设备发现、配对态、延迟校准、托盘常驻 | 产品化 | **1–2 周** | M1/M2 |

**建议的排期（单人）**：M0 → M1（4 周内出第一个可用功能）→ M2（第 5–7 周）→ M3 spike（第 8–9 周）→ 视结果投 M4。

---

## 10. 风险与未验证清单（写代码前逐条打掉）

**许可/法务**
- [ ] `shairplay` LGPL：DLL 动态链接是否满足可重链接义务（需法务确认，不是技术问题）。
- [ ] 之前是否已对外分发过含 GPL/AGPL 的二进制。
- [ ] `shairplay 0.7` 能否在 rustc 1.88 上编译（仓库 MSRV）。

**AirPlay**
- [ ] Windows 上 AP2 端到端可用性（上游 CI 只对 windows-gnu 做 `cargo check --lib`）。
- [ ] `shairplay` 的 AP1 TXT **缺 `pk`**，iOS 是否接受。
- [ ] 5353 与已装 Bonjour Service 共存。
- [ ] 端口 5000 被占时库的行为。
- [ ] `symphonia` 是否支持 ALAC（决定 B2 成本）。
- [ ] FairPlay 逆向的 DMCA 风险（B2 若要做 AP2 缓冲模式会碰到 `/fp-setup`）。

**妙播**
- [ ] 现代 HyperOS 手机是否愿意连第三方兼容**接收端**（M3 的核心问题）。
- [ ] 音箱 HTTP 拉流的真实缓冲秒数（决定方向 A 的延迟下限）。
- [ ] 48 kHz WAV 是否被音箱接受（否则方向 A 必须做 44.1k 重采样）。
- [ ] `mSourceBtMac` 算法（只影响方向 E）。
- [ ] L16A/L16B 是否接受「正确形状」的身份序列（只影响方向 E）。

---

## 11. 立刻可做的第一步（不用等任何人）

```bash
# 1) 只读查看旧实现（不改工作区）
git show 661c59b^:crates/miplay-server/src/hub.rs | less
git show 661c59b^:crates/miplay-core/src/mina.rs | less

# 2) 验证 shairplay 在本机 MSVC 上能编（确认 5.44 MB / 43 s 这个数）
cargo new --lib /tmp/spike-shair && cd /tmp/spike-shair
cargo add shairplay --features ap2
cargo build --release

# 3) 确认 M1 的前提：把 .mi.token 放好，跑 device_list，看小爱音箱是否在线且支持妙播
#    （凭据格式见 §3.3 第 4 条；speaker 支持型号见 MiXPlay speaker.md）
```

**M0 的第一个 commit 应该只做一件事**：把 `route.rs` 里 `sink_loop` 的 WASAPI 调用抽到
`trait PcmSink` 后面（不新增任何网络代码）。这样 M1/M2/M3 才能真正并行。
