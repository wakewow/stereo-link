# 方案：手机音乐投放到电脑（接收方向）

> ⚠️ **本文是方案稿（架构设计 / 取舍记录），不是最终实现。**
> 最终落地的实现选择了另一条更简单的路：**两个独立接收端 exe，各自把声音直接交给本机声卡**，
> 与 StereoLink 的左右分声道引擎完全解耦（不做 IPC、不改 `route.rs`、不做 fan_out）。
> 实际代码在 `receiver/`，使用说明见 **[receiver.md](./receiver.md)**；本文中 §5 的 IPC 契约、
> §6 的 R1–R8 改造点、§9 里依赖引擎的里程碑**均未采用**，保留作设计参考。
>
> 面向 StereoLink（原 miplay-bridge）仓库。
> 目标：**iPhone / 小米（安卓）手机把音乐投放到这台 Windows PC**，PC 用现有引擎把声音送到左右两只蓝牙音箱（或单只音箱 / 声卡）。
> 上游三件套：`shairplay-rust`（AirPlay 接收）、`FusionPlay-Android`（小米妙播接收 SDK）、`miservice`（小米账号 / 米家云）。
> 状态：**v2 方案稿，未写代码**。v2 相对 v1 修正了三处（都已用源码/编译实证）：
> ① 妙播的媒体方向是**反的**——PC 出站连手机的 RTSP server，不是手机连 PC（§4.2）；
> ② FusionPlay SDK **不给 PCM**，妙播必须「先让它自己播 + 采集」或「fork 加出口」（§4.3）；
> ③ miservice 与接收链路**完全无关**，只是可选的第二段链路（§1.1、§8.2）。
> 文中「实测」= 本仓库或本机已完成验证；「待验证」= 尚无证据。

---

## 0. TL;DR

| 问题 | 结论 |
|---|---|
| 接收方向要做几件事？ | **两条协议 + 一套 IPC + 引擎加一种音源**。协议层不自己发明。 |
| AirPlay 接收用什么？ | `shairplay` 0.10（LGPL-3.0）→ 独立子进程 `airplay-bridge.exe`，输出 PCM。iPhone 系统级「隔空播放」，**手机端不装任何 App**。 |
| 小米妙播接收用什么？ | `FusionPlay-Android` 的 `FusionPlay.MiPlaySdk`（纯 Rust，AGPL-3.0）→ 独立子进程 `miplay-bridge.exe`。小米/红米手机用系统「投放到音箱」，**手机端不装 App**。 |
| **妙播的 PCM 从哪来？（关键）** | **SDK 的公共 API 不给你任何一帧音频**——`MiPlayReceiver::start(config, emitter)` 的 `emitter` 只吐 JSON 事件，`audio_pcm_decoded` 只有帧数/峰值统计；真正的 PCM 在 SDK 内部直接交给 cpal/oboe 播放。所以妙播有两条落地路径，见 §4.3：**a) 零改：子进程播到端点，主程序用现有 WASAPI loopback 采集（1–2 天，先验证）**；**b) fork SDK 加一个 feature-gated PCM 出口（3–5 天，产品级，改动须按 AGPL 公开）**。 |
| miservice 是接收必需的吗？ | **完全不需要，它与「手机→PC」这件事无关。** 接收端免账号（握手密钥是 TCP 四元组的纯函数，不含任何账号材料）。miservice 只在**第二段链路**有用：PC 拿到音频后再云投给小米音箱（`player_play_url`）。它是 Python，我们**不引入 Python 运行时**，只把它当协议参照，用旧 `miplay-core` 的 Rust 代码（300–400 行）重写。 |
| 为什么不直接链进主程序？ | LGPL / AGPL 都会感染单 exe（MIT OR Apache-2.0）。**统一走「独立子进程 + 本地 IPC」**，这是同一问题的唯一低成本解法，也是 MiXPlay 的实际做法。 |
| 工作量 | AirPlay 端到端可用 **2–3 周**（M0–M3.5）；加上妙播 **+8–10 天**（含 2–3 天 spike）；若要去掉 AGPL/LGPL（商用闭源）再 **+6–10 周**自研。 |
| 最大风险 | 不是协议细节，而是 ①**小米手机到底能不能投到这台 PC**（未在 L16A/L16B 上验证，只能靠 spike 结清，见 §11）；②**Windows 防火墙按程序路径放行**（AirPlay 有入站 RTSP+UDP；妙播有入站 TCP 8899 + UDP 55982/5353）；③妙播接收端需要**持久化接收者身份**，否则手机每次当新设备。 |

---

## 1. 先把「三件套」的职责摆正

这一步最容易做错，先划清边界再看架构。

| 上游 | 许可 | 实际提供什么 | 在我们方案里的位置 |
|---|---|---|---|
| **shairplay-rust** | LGPL-3.0-or-later | AirPlay 1（RAOP）+ AirPlay 2 的**接收端**完整实现：mDNS 服务宣告、RTSP 会话、FairPlay 兼容、ALAC/AAC 解密解码，交给你 **f32 交错 PCM** | 编成 `airplay-bridge.exe`（**独立进程**，不静态链进主 exe） |
| **FusionPlay-Android** | **AGPL-3.0-only** | 小米妙播的**接收端 SDK**（`FusionPlay.MiPlaySdk`，纯 Rust，7 个 .rs 文件）：mDNS `_mi-connect._udp`/`_lyra-mdns._udp` 宣告、TCP 8899 帧与鉴权、`OPEN wfd://`、**出站** WFD RTSP 协商、RTP/MPEG-TS/AAC 解封装与（可选）AES 解密、AAC→f32 PCM 解码。**但它把 PCM 直接喂给 cpal/oboe，公共 API 只给 JSON 事件、不给音频** | 编成 `miplay-bridge.exe`（**独立进程**）。因为拿不到 PCM，落地要么「让它自己播 + 主程序采集」，要么「fork 加出口」，见 §4.3；长期也可只当**协议规范**自研替代 |
| **miservice** | MIT（Python） | 小米账号登录、`passToken → serviceToken`、米家云 `device_list`、ubus `player_play_url` | **不进最终产物**。① 联调期用来验证云端行为；② 需要的 2–3 个 HTTP 调用用 **Rust 重写**（旧仓库 `crates/miplay-core/src/{token,mina}.rs` 已经写好，自有 MIT/Apache，`git show 661c59b^:<path>` 取回即可） |

> 一句话记法：**shairplay 解决 iPhone，FusionPlay 解决小米手机，miservice 解决「再往外送」。**
> 「手机 → PC」这一段完全不需要小米账号；需要账号的是「PC → 小爱音箱」那一段。

### 1.1 为什么 miservice 与接收链路无关

很多人会以为「小米妙播必须登录小米账号」。事实是：**妙播的接收端不参与账号密钥交换**——鉴权密钥 `auth_key` 是 TCP 四元组的纯函数（`md5` 派生，与账号/证书/证书链全部无关），FusionPlay 的接收端也正是靠这一点做到「不登录也能被手机发现并接收」（它的三种 key scope 里，局域网信任用的是 `LOGICAL_TRUST_LOCAL_NETWORK = 0x40`，不需要 `account_hash`）。所以：

- 只想「手机投到电脑」→ **不需要 miservice，一行 Python 都不用跑**。
- 想要「手机投到电脑，电脑再把声音分给客厅小爱」→ 需要 miservice 的那两个调用（`device_list` + `player_play_url`），用 Rust 重写。
- 想要「控制台里显示『XXX 的手机』而不是一串 IP」→ miservice **帮不上**（手机不是米家设备，只有音箱在设备表里）。接收端直接显示手机自报的名字/型号即可，miservice 的设备表只用来给**输出端音箱**取名。

**结论：miservice 放到 M5（可选增强）做，不要挡在主干上。**

---

## 2. 目标与验收

### 2.1 用户故事

1. iPhone 打开控制中心 → 隔空播放 → 看到 **「StereoLink」** → 选中 → 手机上任何 App（Apple Music / 网易云 / B 站）的声音从 PC 的两只音箱出来，**左右声道分别在左音箱 / 右音箱**。
2. 小米手机音乐 App / 系统播放器 → 投放到音箱 → 同样能看到 PC 并投放。
3. 手机端**不安装任何 App**（iPhone 用系统隔空播放；小米用系统妙播）。
4. PC 端切换音源只需在 GUI 里点一下：**本机声卡采集 / AirPlay 接收 / 妙播接收**。

### 2.2 验收判据

| 项 | 判据 |
|---|---|
| AirPlay 可用 | iPhone 首发延迟（按下播放 → 出声）**< 1.5 s**；连续播放 30 min 无断流、无爆音；暂停/继续/拖进度正确跟随 |
| 妙播可用 | 小米手机投放成功率高（同一 WiFi 下 10 次投放 ≥ 9 次成功）；断开后 PC 侧 3 s 内回到待机 |
| 音质 | 端到端保持源采样率（44.1k/48k），**不经历非整数比 SRC**；左右分声道后无串音 |
| 稳定性 | 手机锁屏 / 切歌 / 离开 WiFi 再回来，PC 不崩、能自动恢复待机 |
| 合规 | 分发包内包含 LGPL / AGPL 文本与对应源码获取方式；主 exe 仍是 MIT OR Apache-2.0 |

---

## 3. 总体架构：协议桥子进程（sidecar）

```
┌──────────────────────────── StereoLink.exe（MIT OR Apache-2.0，单 exe）────────────────────────────┐
│  GUI (egui, app.rs)                                                                               │
│   音源选择：① 本机声卡采集  ② AirPlay 接收  ③ 妙播接收     ← 新增（config.rs / app.rs）           │
│                                                                                                   │
│  ┌── 音源 A：现有采集线程 ──┐        ┌── 音源 B：新增 bridge 线程 ──┐                            │
│  │ capture_loop             │        │ bridge_loop                  │                            │
│  │  WASAPI loopback         │        │  本地 TCP 收 PCM 块          │                            │
│  │  → CaptureDsp(f32 L/R)   │        │  → 解帧 → f32 L/R            │                            │
│  └──────────┬───────────────┘        └──────────────┬───────────────┘                            │
│             └──────────────► fan_out(left[], right[]) ◄────────────────┘   ← 抽取自现有 2197-2222 │
│                                    │                                                              │
│                    route.rs 现有链路：mix_sample → gain → invert → SinkState 队列                  │
│                                    │  （队列满丢最旧、队列空补静音 = 天然 jitter buffer）           │
│                          ┌─────────┴──────────┐                                                   │
│                    左音箱端点（L）          右音箱端点（R）                                        │
└───────────────┬──────────────────────────────────────────────┬────────────────────────────────────┘
                │ 127.0.0.1:59731（主进程 listen，sidecar 连回）│ 127.0.0.1:59732
   ┌────────────▼─────────────┐              ┌─────────────────▼──────────┐
   │ airplay-bridge.exe       │              │ miplay-bridge.exe          │
   │ = shairplay 0.10         │              │ = FusionPlay.MiPlaySdk     │
   │ LGPL-3.0，独立进程        │              │ AGPL-3.0，独立进程          │
   │ iPhone 主动连 PC（监听方） │              │ PC 主动连手机（外连方）      │
   │ mDNS _raop/_airplay      │              │ mDNS _mi-connect/_lyra     │
   │ RTSP 7000 + 动态 UDP 入站 │              │ TCP 8899 + UDP 55982 入站   │
   │ 直接拿到 f32 PCM（trait）  │              │ 无 PCM 出口，见 §4.3        │
   └──────────────────────────┘              └────────────────────────────┘
```

> 注：妙播的 PCM 出口有两条接法（§4.3）——**模式 a 复用图中左侧的 WASAPI 采集路径**（根本不经过 IPC），**模式 b 才走右侧的 IPC 路径**。两条最终都汇到同一个 `fan_out`，所以引擎、左右分声道、延迟校准、统计永远只有一套代码。这也是把它拆成 M4a/M4b 两个里程碑却几乎不增加总工作量的原因。

### 3.1 三个设计决策与理由

**① 为什么是「独立进程」，不是 FFI / 静态链接。**
主仓库是 `MIT OR Apache-2.0`，要出**单 exe**。LGPL 静态链入会触发「可重链接 / 提供目标文件」义务；AGPL 静态链入会让整个产品变 AGPL。而**独立进程 + 本地 socket 通信**是业界公认的「聚合（aggregation）」而非衍生作品，主程序可以把它们当作可替换的协议组件。MiXPlay 就是这个结构（Python hub + 3 个 Rust sidecar 子进程），它自己的 `THIRD-PARTY-NOTICES.md` 也是这么声明的。
**代价**：多了 1–2 个 exe（`airplay-bridge.exe` 约 5.4 MB、`miplay-bridge.exe` 待测），启动/退出要管进程生命周期，IPC 多约 1–3 ms。
**收益**：许可风险从「必须改产品许可」降到「附上许可文本与源码链接」，且 sidecar 崩了主程序不受影响（可自动重拉）。

**② 为什么 IPC 走本地 TCP，而不是 stdout / 命名管道。**
Windows GUI 进程没有控制台，stdout 无法自然使用；stdout 还是文本模式、无帧边界、无背压。命名管道要额外处理多实例与安全描述符。**本地 TCP + 启动时把端口和一次性 token 作为命令行参数传给 sidecar**，是最简单且跨语言（Rust ↔ Rust，未来可能是别的语言）的选择，也天然支持「主进程先起、sidecar 后连」的重连模型。

**③ 为什么主进程是 listen 侧，sidecar 是 connect 侧。**
由主进程 `bind 127.0.0.1:0` 拿一个临时端口 → 起 sidecar 时把 `--ipc 127.0.0.1:<port> --token <32hex>` 传进去。好处：**不会端口冲突、不会被别的程序抢端口、天然单实例**（token 不对就拒连），而且 sidecar 重启后能自动重连回同一个端口。

---

## 4. 两条接收链路的时序

### 4.1 AirPlay（iPhone / iPad / Mac）

```
iPhone                          PC（airplay-bridge）                     StereoLink
  │  ① mDNS 查询 _raop._tcp/_airplay._tcp                                    │
  │  ←─── ② 应答：设备名 "StereoLink"、deviceid、features、pk、端口 7000      │
  │  ③ TCP 7000 / RTSP：OPTIONS → ANNOUNCE（RSA+AES-128 解 ALAC 密钥）        │
  │  ④ SETUP（RTP 端口）→ RECORD                                              │
  │  ⑤ RTP 音频包 ────────────────→ 解密/解 ALAC/AAC → f32 交错 PCM           │
  │                                     │ ── IPC PCM 块（含 PTS）────────────→ │ 扇出 → 左右分声道
  │  ⑥ 同步包 / 时序包（UDP）───────→    │                                     │
  │  ⑦ TEARDOWN                                                            │
```

要点：
- **AP1 与 AP2 都要支持**：`shairplay` 打开 `ap2` feature 后两种都能接。`sf` / `et` 的 TXT 取值要正确，否则 iOS 会出现「转圈 10 s 才连上」或反复配对（`et=0,3,5` 不带 `4`；`sf` 按配对态给 `0x004 / 0x204 / 0x604`）。
- 接收端**不需要 MFi 芯片、不需要 Apple 授权、不需要 Bonjour 服务**（mDNS 用纯 Rust `mdns-sd` 自己发）。
- **Windows 防火墙**：数据/控制/时序口都是临时端口，必须**按程序路径**放行入站 UDP（详见 §10）。

### 4.2 小米妙播（小米 / 红米手机）

> ⚠️ 方向与 AirPlay **相反**：`OPEN` 里的 `wfd://` 地址是**手机的**，PC 拿着它**出站**连手机上的 RTSP server。PC 侧**不监听**任何 RTSP 端口。

```
小米手机（发送端，跑 RTSP server）        PC（miplay-bridge，接收端）
  │  ① mDNS 查询 _mi-connect._udp / _lyra-mdns._udp                    │
  │  ←─── ② 应答：name/dev/sec/flags/version/idHash/apps/appsData         │
  │         （TXT 必须带 mac=，否则 HyperOS 的 BonjourGovernor 丢端点）    │
  │  ③ 手机连 PC 的 TCP 8899：DEVICE_ID(0x28) → 鉴权握手                 │
  │     （legacy 明文序列 / 兼容安全 0x14 命名空间 + SafetyAuth）          │
  │  ④ OPEN(0x00, 加密) body = "wfd://<手机IP>:<port>?mirrorMode=1"      │
  │  ⑤ PC 用这个地址出站连手机的 WFD RTSP server：                        │
  │     OPTIONS(带 authMsg 挑战) → 200(带 authMsgAck)                   │
  │     → GET_PARAMETER → SET_PARAMETER(timeoffset / mirror_key)        │
  │     → SETUP(Transport: RTP/AVP/TCP;interleaved=0-1) → PLAY          │
  │  ⑥ RTP over 同一条 TCP（interleaved）：MPEG-TS(PID 0x1100) → PES → AAC│
  │     （可选：每个 PES 前 min(len,256)/16*16 字节 AES-128-CBC 解密）     │
  │  ⑦ AAC-LC 48 kHz/2ch 解码 → PCM  ← 在 SDK 内部，见 §4.3              │
  │  ⑧ 心跳 / GET_STATE / 反向 PLAYER 控制 ACK（PC → 手机）               │
```

要点（每条都有出处，别省）：
- **拓扑与直觉相反**：接收端主动外连发送端，PC **不需要**监听 RTSP。这是本链路与 AirPlay 最大的结构差异——也意味着妙播方向**不需要**为动态媒体端口建防火墙规则。
- **必须绑 `0.0.0.0`**：`_lyra-mdns` / `_mi-connect` 的 UDP 要绑通配地址，只绑 LAN IP 会收不到多播。
- **mDNS 的 QU 位要单播回复、IP TTL 固定 255**；`_lyra-mdns` 的 SRV 端口写 5353 而实际监听 55982（上游已知不一致，别照抄）。
- **接收者身份要持久化**：`ReceiverIdentity`（idm / lyra / media 三组 id）与名字落盘，否则手机每次当新设备。
- **采样率被硬编码 48000**：SDK 遇到非 48 kHz 直接报 `unsupported MiPlay AAC sample rate`；输出设备不支持 48 kHz 也会失败。这决定了 IPC 侧按 48 kHz 协商。
- **同一台机器只能跑一个妙播接收端**（mDNS 记录 + 8899 绑定都会冲突），不要为左右声道各起一个。
- `GET_MIRROR_MODE` 必须答 `[0,0,0,0,1]`；`NOTIFY` 序列号必须单调递增（从 8 起）；反向控制 ACK 必须保留 outer 高字节（`0x0416` ≠ `0x0016`）。
- 明文回退路径**存在且合法**（面向电视接收端），但策略上优先走 SDK 的加密分支。

### 4.3 妙播的 PCM 到底怎么拿（先 a 后 b）

`FusionPlay.MiPlaySdk` 的公共面只有两样东西：`MiPlayReceiver::start(config, emitter)` 和 `receiver.controller()`。`emitter` 是 `Arc<dyn Fn(serde_json::Value)>`，吐出的事件（`receiver_ready` / `stream_started` / `audio_pcm_decoded` / `clock_synchronized` / `playback_state` / `volume` …）**都不含 PCM**；`audio_pcm_decoded` 只有帧数、peak、rms 三个统计量。PCM 在 SDK 内部：AAC ADTS → symphonia → **f32 交错 48 kHz/2ch、每帧 1024 样本** → 内部 `PcmQueue` → cpal（Windows/macOS）/ oboe（Android）直接出声。

所以只有两条路：

| | 做法 | 工作量 | 代价 |
|---|---|---|---|
| **a. 零改**（先做） | 不改 SDK：写一个 `main.rs` 让它把音频播到指定输出设备（`ReceiverConfig` 有 `output_device`），主程序沿用**现有的 WASAPI loopback 采集**去采那个端点 | 1–2 天（含身份持久化） | 需要一个可采集的输出端点（真声卡 / 现有 `drivers/` 的虚拟端点 / VB-CABLE）；多一跳设备缓冲（+10–30 ms）；音源仍从「采集线程」进，**`route.rs` 一行都不用改** |
| **b. fork 加出口**（产品化） | fork SDK 或打 patch crate，在内部 `PcmQueue` 入队处加 feature-gated 的 PCM sink，把 f32 块推给我们的 IPC | 3–5 天 | 改的是 AGPL 作品 → 必须公开这部分改动（把完整源码树放 `crates/bridge-miplay/` 即可）；升级上游要 rebase |

**建议顺序**：先花 2–3 天做 a 的 **spike**——它能在一周内把「小米手机到底能不能投给这台 PC」这个**最大的不确定性**结清，而这不需要先付 fork 的代价。spike 成功后再投入 b 做产品级体验；spike 失败则妙播整条线暂停，只交 AirPlay（iPhone 那条仍然完整可用）。

> a 的隐藏好处：它顺便验证 mDNS 宣告、8899 握手、`OPEN`、媒体解密全部打通（SDK 自己会吐事件/日志），失败时能立刻定位卡在哪一层。

---

## 5. IPC 契约 v1（字节级定义）

主进程 `bind 127.0.0.1:0` → 端口经命令行传给 sidecar；sidecar 用 TCP 连回，先校验 `--token`（32 hex，握手首帧带）。

### 5.1 帧头（32 字节，小端）

| 偏移 | 长度 | 字段 | 说明 |
|---|---|---|---|
| 0 | 4 | `magic` | `b"SLB1"` |
| 4 | 1 | `kind` | 0=PCM 1=META(JSON) 2=HELLO 3=CONFIG 4=STATS 5=BYE |
| 5 | 1 | `version` | 1 |
| 6 | 2 | `flags` | bit0 = `pts_valid` |
| 8 | 4 | `seq` | u32，帧序号 |
| 12 | 4 | `rate` | u32，PCM 采样率（非 PCM 填 0） |
| 16 | 1 | `channels` | u8，固定 2（妙播/AirPlay 都是立体声） |
| 17 | 1 | `sample_fmt` | 0 = s16le，1 = f32le |
| 18 | 2 | `reserved` | 0 |
| 20 | 4 | `frames` | u32，本块帧数 |
| 24 | 8 | `pts_us` | u64，本块首样本呈现时间（微秒，会话时钟）；`0xFFFF_FFFF_FFFF_FFFF` = 无效 |
| 32 | N | `payload` | PCM 或 UTF-8 JSON |

### 5.2 会话流程

```
sidecar → HELLO   {"protocol":1,"name":"airplay-bridge","version":"0.1.0",
                   "capabilities":{"formats":["s16le","f32le"],"rates":[44100,48000],"chunk_frames":1024},
                   "token":"<32hex>"}
主进程 → CONFIG  {"format":"f32le","rate":48000,"channels":2,"chunk_frames":1024}
sidecar → PCM    …连续帧…
sidecar → META   {"event":"session-start","peer":"iPhone","codec":"alac","rate":44100}
        → META   {"event":"paused"} / {"event":"resumed"} / {"event":"volume","value":0.6}
        → META   {"event":"session-end"}
主进程 → BYE     停止时通知优雅退出（sidecar 也必须有超时自杀：5 s 收不到数据即退出）
```

### 5.3 块大小、采样格式、采样率：三个必须由 sidecar 归一的东西

现有 `route.rs` 的 `SinkState` 本来就是为「两条独立时钟链路」设计的：**队列满丢最旧、队列空补静音**。只要 sidecar 保证下面三条，主程序**不需要新增 jitter buffer**：

1. **块大小恒定**（1024 帧 = 21.3 ms @48k），丢包时用静音补齐而不是缩小块。
   > 两个上游都**不保证**固定块：`shairplay` 的 `audio_process(&mut self, samples: &[f32])` 每调用一次就是**一个解码后的包**（AP1 352 帧/包、AP2 buffered 也是按包交付），SDK 内部则是 1024 样本/帧。所以**重新分块是 sidecar 的职责**：上游给多少先塞进环形缓冲，凑满 1024 帧再发一帧 IPC。
2. **采样格式固定 f32le。** 两个上游内部都是 f32 交错（`shairplay` 的 `AudioSession` 直接给 f32；FusionPlay 解码后也是 f32），选 s16le 只会多两次转换 + 量化损失。f32 的 IPC 带宽 = 48k×2ch×4B ≈ 384 kB/s，本机回环毫无压力。
3. **采样率对不同协议不同**：AirPlay 是 44.1 kHz（AP1 RAOP 原生，AP2 也可能是 44.1k），妙播被硬编码 48 kHz。**不要假设统一 48k**。处理方式：IPC 里如实上报 `rate`，`bridge_loop` 用仓库已有的 `convert::StreamResampler` 把 44.1k → 引擎采样率（或反向）。**这样切音源时不需要重开 WASAPI 端点**，也就不会出现「切换音源时音箱咔一声」。

关于时间戳：**两个上游都不给逐帧 PTS**（`AudioSession::audio_process` 只有样本；FusionPlay 内部有 PES PTS 但对外只给 `presentation_clock_updated` 的时钟偏移）。所以 `pts_us` 只用来**诊断/测量抖动**，不要拿它做播放决策——播放决策交给 `SinkState` 的队列水位。如果将来要做双音箱严格同步，才需要把 `presentation_clock_updated` 接进来做补偿（见 §11 H3）。

另两个必须显式处理的点：
- **首次预卷**：sidecar 报 `session-start` 后先攒 `queue_ms`（默认 300 ms，妙播 400 ms）再出音，避免开头爆音。
- **FLUSH / seek**：`shairplay` 会调 `audio_flush()`，妙播会给 `stream_stopped`；两者都要立刻**清空 bridge 侧缓冲**并通知主进程丢队列，否则拖进度后会有几百毫秒的旧音频。

---

## 6. 主程序改造点（精确到文件与行）

以下行号基于当前工作区（`0.5.5`）。**注意：工作区已有未提交改动，动手前先 `git status` 确认基线。**

### 6.1 `crates/stereo-audio/src/route.rs`（核心，约 300–400 行新增）

| # | 位置 | 现状 | 改法 |
|---|---|---|---|
| R1 | `route.rs:2192-2222` | 扇出逻辑内联在 `capture_loop` 里 | **抽取** `fn fan_out(shared: &Arc<Shared>, left: &[f32], right: &[f32], frames: usize)`，`capture_loop` 与新的 `bridge_loop` 共用。这是最小、最干净的切口——扇出只依赖 `mix_sample` / `apply_gain` / `invert_sample` / `SinkState`，与「数据从哪来」无关 |
| R2 | `RouteEngineConfig`（`route.rs:210-230`） | `source_endpoint: String` | 新增 `source: SourceConfig`（`Endpoint{endpoint}` \| `Bridge{addr, token, prefill_ms}`）；`source_endpoint` 保留用于兼容旧配置 |
| R3 | `run_capture`（`route.rs:2001`） | 只跑 WASAPI loopback | 按 `config.source` 分派到 `capture_loop` 或新的 `run_bridge` / `bridge_loop` |
| R4 | 新增 `bridge_loop`（约 200 行） | — | TCP 连接 → 握手 → 解帧 → s16/f32 → f32 立体声缓冲 → 调用 `fan_out`。含重连（指数退避）、token 校验、超时自杀 |
| R5 | `effective_engine_rate`（`route.rs:1281`） | 端点混音率 | `Bridge` 源用协商采样率（44.1k/48k）；`rate: Some(_)` 仍优先 |
| R6 | `RouteChannel`（`route.rs:102-111`） | `Left` / `Right` / `Both` | **可选（二期）** 加 `Stereo`，保真立体声到单只音箱。需同步改 `SinkState.queue` 语义（推 2 个样本/帧）、`encode_native_stereo`（`route.rs:1980`）与 `MixConverter`。**一期不做**：现有 `Left`/`Right` 已能满足「两只音箱分别放左右声道」 |
| R7 | `RouteEngineStats` / `stats()` | 只报采集/路由 | 增加 `source_kind`、`bridge_state`、`bridge_peer`、`bridge_jitter_ms`，给 GUI 显示 |
| R8 | `bridge_loop` 内的重采样 | — | 把 sidecar 的原生率（44.1k/48k）用 `convert::StreamResampler` 归一到引擎采样率，**保证切音源不必重开 WASAPI 端点**（不重开端点 = 不咔声、不重连蓝牙） |
| R9 | `Shared` / `fan_out` 的时钟源 | 采集线程用自己的帧计数 | bridge 线程用**到达时刻 `Instant` + 累积帧数**推播放时刻（上游无 PTS），并与 `SinkState` 的水位配合做漂移吸收 |

### 6.2 `crates/stereo-link/src/app.rs`（GUI，约 150–250 行）

- `engine: Option<RouteEngineHandle>`（`app.rs:679`）旁边加 `bridges: BridgeSupervisor`（拉子进程、管端口/token、进程退出后自动重启、`Drop` 时杀干净）。
- `route_engine_config()`（`app.rs:1461-1500`）：按当前音源选择填 `RouteEngineConfig.source`。
- 新增面板：「音源」单选 + sidecar 状态灯（运行中/对端设备名/抖动）+ 「在 iPhone 隔空播放里显示的名字」+ 「小米妙播设备名」。

### 6.3 `crates/stereo-link/src/config.rs`

- `AppConfig`（`config.rs:369`）新增 `receive: ReceiveConfig`：
  ```rust
  pub struct ReceiveConfig {
      pub source: ReceiveSource,      // Off | AirPlay | MiPlay
      pub display_name: String,       // 默认 "StereoLink"
      pub airplay_port: u16,          // 默认 7000
      pub miplay_port: u16,           // 默认 8899
      pub prefill_ms: u32,            // 默认 300
      pub persist_identity: bool,     // 妙播接收者身份持久化，默认 true
  }
  ```
- 配置文件里**不存 token**（每次启动现生成）。

### 6.4 新增 `crates/bridge-proto`（约 150 行，MIT/Apache，被主程序与两个 sidecar 共用）

纯数据结构 + 序列化 + 握手状态机，无网络逻辑。**共享 crate 能让主程序与 sidecar 的帧定义只有一个真相来源**，避免两边漂移。

> 两个 sidecar 各自是独立 crate：`crates/bridge-airplay`、`crates/bridge-miplay`，用 `cargo build --release -p bridge-airplay` 单独出 exe，不进入主 exe 的依赖图（这正是许可隔离的物理保证）。

---

## 7. 许可与合规

### 7.1 三种可选模式

| 模式 | 做法 | 合规成本 | 评价 |
|---|---|---|---|
| **A. 双 sidecar（推荐）** | AirPlay = `shairplay` 独立进程；妙播 = FusionPlay 独立进程 | 低：附 LGPL/AGPL 全文 + 对应源码获取方式；不改上游代码 | **先做这个**，最快可用 |
| **B. AirPlay 自研 + 妙播 sidecar** | AirPlay 按规范自研 AP1（3–6 周）；妙播先 sidecar | 中：只剩 AGPL 一项 | 二期；想商用闭源必须先做 |
| **C. 全自研** | 两条都按规范自研 | 零许可约束，工作量最大（+6–10 周） | 三期 / 长期 |

### 7.2 模式 A 的具体义务

- **LGPL-3.0（shairplay）**：分发包需 ①附 LGPL 全文；②说明该组件可被替换（独立进程天然满足）；③提供该 crate 的修改后源码（若我们改了；不改则给上游版本号 + crates.io 链接即可）。
- **AGPL-3.0（FusionPlay）**：分发包需 ①附 AGPL 全文；②**提供对应源码**（含我们的构建脚本与任何修改）。若我们**修改**它并作为网络服务对外提供，§13 要求向网络用户提供源码——**我们只在本机跑、不对外提供服务，这个条款不触发**。仍然建议：`miplay-bridge` 里避免「把 AGPL 代码抄进我们自己的 MIT 文件」。
  - **§4.3 模式 a（不改 SDK）**：义务最轻——附全文 + 说明来源 commit，审计友好。
  - **§4.3 模式 b（fork 加 PCM 出口）**：我们成了 AGPL 衍生作品的作者，**那 30–50 行出口代码必须按 AGPL 公开**（放进 `crates/bridge-miplay/`，随分发包给源码即可）。这在授权上是干净的，只是每次跟上游升级要 rebase。
  - **红线（不可让步）**：绝不能把 `FusionPlay.MiPlaySdk` 作为 `[dependencies]` 链进任何 MIT/Apache crate——链接即产生 AGPL 作品。它只能存在于**独立 exe 的依赖图**里。
- **MIT（miservice）**：若直接移植其代码进 Rust，保留版权声明；若只参考协议自己写，无义务。
- **红线**：主 exe（`stereo-link.exe`）不得静态链入任何 LGPL/AGPL 代码；不得把 `bridge-*` 的代码复制到主 crate。

### 7.3 分发产物结构

```
StereoLink-0.6.0/
├─ stereo-link.exe           ← MIT OR Apache-2.0
├─ airplay-bridge.exe        ← 内含/基于 LGPL-3.0 的 shairplay
├─ miplay-bridge.exe         ← 内含/基于 AGPL-3.0 的 FusionPlay.MiPlaySdk
├─ LICENSES/
│   ├─ LICENSE-MIT
│   ├─ LICENSE-APACHE
│   ├─ LGPL-3.0.txt
│   ├─ AGPL-3.0.txt
│   └─ THIRD-PARTY.md        ← 组件、版本、许可、源码获取方式（必须逐项写清）
└─ README.md
```

> **不要重犯 MiXPlay 的错误**：它的 `README` 说 FusionPlay 是 AGPL，`THIRD-PARTY-NOTICES.md` 却标 MIT，`pyproject.toml` 又写 MIT 而仓库是专有。**许可声明必须与包元数据、仓库 LICENSE 三者一致**。

---

## 8. miservice：两种用法与实施路径

### 8.1 我们不引入 Python

StereoLink 是单 exe、零外部运行时。**不要在产物里塞 Python**（体积 + 部署 + 杀软误报三重代价）。正确做法：

- **联调期**：`uv tool install miservice` 起一个 Python 侧对照脚本，验证「小米账号 → device_list → play_url」在真机上的行为，把**真实请求/响应**录成 fixture（JSON）存进 `research/`。
- **产品期**：用 Rust 重写所需调用。旧仓库已经写好了：
  - `git show 661c59b^:crates/miplay-core/src/token.rs`（179 行）= 凭据读取 + `passToken → serviceToken`
  - `git show 661c59b^:crates/miplay-core/src/mina.rs`（459 行）= `MINA_BASE = https://api2.mina.mi.com`、`device_list`、ubus `player_play_url`
  - 这两个文件是**本仓库自有代码**（MIT OR Apache-2.0），直接取回即可，不引入任何新许可。

### 8.2 两种用法（按优先级）

| 用途 | 需要什么 | 何时做 | 价值 |
|---|---|---|---|
| **① 设备/音箱命名** | `device_list` | M5 | GUI 显示「客厅小爱」「卧室左」而不是友好名字符串 |
| **② 收到的音乐再云投给小米音箱** | `player_play_url` + 本机 HTTP 流（旧 `hub.rs` 1652 行已实现，含 `PREFILL=1200ms`、`ensure_firewall`） | M5（复用旧代码，1–2 周） | **真正的差异化**：「手机 → PC → 全屋小爱」；与现有左右分声道互补，这才是 StereoLink 的主场 |

> 原来设想的第三项「账号信任增强」**已删除**：接收端不参与账号密钥交换，`auth_key` 是 TCP 四元组纯函数，加账号也不改变握手结果。别为它付成本。

### 8.3 凭据

复用旧格式（与 MiService / MiXPlay 兼容），三处任一：

```
%USERPROFILE%\conf\.mi.token
%APPDATA%\StereoLink\.mi.token
环境变量 MIPLAY_TOKEN
```

JSON：`{"deviceId":"…","userId":…,"passToken":"…","micoapi":["<ssecurity>","<serviceToken>"]}`。
**明文存储**是现状（沿用），建议 M5 起改 DPAPI（`CryptProtectData`）加密落盘；至少在 GUI 里提示风险。

### 8.4 Rust 重写清单（约 300–400 行，仅 M5 需要）

**不要照 miservice 逐行移植**——它的 `MiAccount.login` 在某类异常路径下会**删掉用户的 token 文件**，这个坑不能带进产品。旧仓库里已经是干净版本，直接取回：

| 文件 | 行数 | 内容 |
|---|---|---|
| `crates/miplay-core/src/token.rs` | 179 | 凭据读取（`.mi.token` / env `MIPLAY_TOKEN`）、`passToken → serviceToken` 刷新 |
| `crates/miplay-core/src/mina.rs` | 459 | `MINA_BASE = https://api2.mina.mi.com`、`UA_MINA`（伪装米家 iOS 客户端）、`device_list`、`player_play_url`、`PLAY_MUSIC_HARDWARE` 白名单、`DEFAULT_AUDIO_ID` / `DEFAULT_CP_ID` |
| `crates/miplay-core/src/device.rs` | 229 | 设备模型与解析 |

依赖只有 `reqwest` / `serde_json` / `sha1` / `md5` / `base64` / `rand`，与 GUI 无耦合。取回命令见 §12。

---

## 9. 里程碑与工作量

| 里程碑 | 内容 | 交付判据 | 估时（1 人） |
|---|---|---|---|
| **M0 合规与骨架** | 建 `crates/bridge-proto`；定 IPC 契约；准备 `LICENSES/`；`git status` 基线确认 | 三方 crate 能 `cargo check`；IPC 单测通过 | 2–3 d |
| **M1 AirPlay sidecar 出 PCM** | 基于 `shairplay` 0.10 写 `crates/bridge-airplay`；`--ipc` 握手；把 PCM 打到 socket | iPhone 隔空播放能看到设备并播放；用 `ffplay`/文件落盘验证 PCM 正确 | 5–7 d |
| **M2 主程序接收音源** | 抽取 `fan_out`；新增 `bridge_loop`；`SourceConfig`；预卷；重连 | 不经音箱、先写 WAV 验证：手机播什么，PC 得到什么 | 3–5 d |
| **M3 端到端 + GUI** | 音源选择面板、状态灯、命名、托盘常驻；左/右分声道到两只蓝牙音箱 | **iPhone → 两只音箱分别出左右声道**（§2.2 验收全过） | 3–5 d |
| **M3.5 防火墙安装器** | 安装时按程序路径加 4 条入站规则（AirPlay TCP+UDP、妙播 TCP+UDP，见 §10） | 全新机器装完即可用，无需手动点弹窗 | 1–2 d |
| **M4a 妙播 spike（决策点）** | 不改 SDK：`crates/bridge-miplay` 先只做一个 `main.rs`，`MiPlayReceiver::start` 播到指定端点，打全量事件日志。**不接主程序** | 小米手机「投放到音箱」列表里出现这台 PC、投放后 PC 出声（判据只有一条：**能不能收到**） | **2–3 d** |
| **M4b 妙播产品化** | §4.3 模式 b：fork 加 feature-gated PCM 出口 → 走同一套 IPC；身份持久化；AAC 48k 归一 | 接进主程序后 30 min 无断流；`暂停/继续/音量` 与手机端双向一致 | **5–7 d** |
| **M5 miservice 层（可选）** | Rust 版 token/mina（§8.4）；设备表；转发到小爱（复用旧 `hub.rs`） | 手机→PC→客厅小爱同时出声 | 5–8 d |
| **M6 许可净化（可选）** | 自研 AP1 替换 shairplay；自研妙播替换 FusionPlay（§7.1 模式 C） | 分发包内不再有 AGPL/LGPL 组件 | +3–6 周（AP1）/ +5–7 周（妙播） |

**合计：AirPlay 单协议可用 ≈ 2–3 周**（M0–M3.5）；**加上妙播 ≈ 3.5–4.5 周**（+M4a/M4b 共 8–10 天）；**商用去许可风险 ≈ +6–10 周**。

排期建议：
1. **先 M0–M3 出「iPhone 投放」可演示版本**——风险最低、用户感知最强，且完全在本机可控范围内。
2. **M4a 与 M0 并行启动**（它只需要一个空的 Rust bin + 端点，不依赖主程序改造）。**它的结论决定妙播这条线是否继续**：成功就排 M4b，失败就砍掉妙播只交 AirPlay，并把资源转到 M5/M6。
3. 不要按「先把妙播做完整」排序——妙播的未知项（§11）比 AirPlay 多一个数量级。

---

## 10. Windows 平台坑（必做清单）

| 坑 | 症状 | 处理 |
|---|---|---|
| **防火墙丢入站** | iPhone 连上但无声 / `io error (os error 10060)`；小米手机投放超时 | **必须按程序路径**放行，不能只放端口（媒体/时序口是临时端口）。① `airplay-bridge.exe`：TCP + UDP 全部入站（RTSP 7000 与动态 UDP 都在它上面）；② `miplay-bridge.exe`：**TCP 8899 + UDP 55982 + UDP 5353** 入站；妙播**不需要**任何动态 RTSP 端口（PC 是外连方，见 §4.2）。旧 `661c59b^:docs/firewall-requirement.md` 有根因分析与 `netsh` 命令，直接复用 |
| **多播没进循环**（仅妙播） | 手机列表里根本看不到这台 PC | UDP 5353 多播必须在**选中的 LAN 网卡**上 join；绑定用 `0.0.0.0` 而不是 LAN IP；IP TTL 固定 255；QU 位要单播回复 |
| **TXT 字段不全**（仅妙播） | 能看到设备但点投放就消失 | `_mi-connect` 的 TXT **必须带 `mac=`**，否则 HyperOS 的 BonjourGovernor 会把端点丢掉；`_lyra-mdns` 的 SRV 端口别照抄 5353（实际 55982） |
| **48 kHz 硬约束**（仅妙播） | 投放后无声/报错 | 输出设备必须支持 48 kHz；Windows 声音设置里把该端点采样率设成 48 kHz，或让 sidecar 内部重采样到 48 kHz 再进 IPC |
| **两个接收端抢端口**（仅妙播） | 第二个实例起不来 | TCP 8899 + mDNS 记录都会冲突。**一台机器只跑一个妙播接收端**，左右声道靠主程序分，不要起两个 |
| 反复中断后被拒连 | 音箱/手机短时间内拒绝重连 | 重连退避 ≥ 2 s；不要在同一秒内重试 |
| mDNS 与 Bonjour 抢 5353 | 设备时有时无 | `mdns-sd` 用 `SO_REUSEADDR` 绑 5353 + 组播加入；若冲突则改用 5354 并只在 `_raop` 上宣告 |
| 多网卡绑错地址 | 手机在另一网段找不到 | 用 `getifaddrs` 选默认路由所在网卡的 IPv4，显式 `bind`；同时宣告所有 LAN 地址 |
| GUI 关窗即消失 | iPhone 上设备列表里设备跳走 | 托盘常驻（已有 `crates/stereo-link/src/tray.rs`），关窗只隐藏 |
| 蓝牙 A2DP 与 WiFi 共存抖动 | 高负载下断续 | 蓝牙端点 20–40 ms 缓冲；WiFi 5 GHz；必要时提示用户 |
| 系统休眠 / 锁屏 | 会话中断 | 监听 `WM_POWERBROADCAST`，恢复后重拉 sidecar |
| 杀软误报 | sidecar 被拦 | 签名 + 安装器白名单；sidecar 不放 `%TEMP%` |

---

## 11. 风险与未验证项

### 11.1 高风险

| # | 风险 | 影响 | 缓解 |
|---|---|---|---|
| **H1** | **「小米手机能不能投到这台 PC」从未被验证过。** 所有已知的妙播成功案例都是「手机 → 小米音箱/电视」，没有「手机 → 第三方软件接收端」的公开实测；FusionPlay 声称可作接收端，但它自己的目录里**没有妙播的 example 或 bin**（`src/AirPlayReceiver.Core` 那个 CLI 只覆盖 AirPlay） | 妙播可能整条线不成立 | **M4a spike 是唯一答案**，2–3 天，与 M0 并行启动。失败就砍掉妙播只交 AirPlay（iPhone 那条独立可用），并把资源转 M5/M6 |
| **H2** | **FusionPlay 是 AGPL，且产品化几乎肯定要 fork**（加 PCM 出口） | 改动须按 AGPL 公开 | 模式 a 零改先验证；模式 b 只 fork「30–50 行出口」，源码树随包发布；绝不静态链进主 exe |
| **H3** | **shairplay 无真时钟同步**（PTP 包只排空、offset 未接进播放） | 多房间/严格双音箱同步做不到 | 我们只需「单一来源 + 左右分声道」，同一个 PCM 块天然同步，**不受影响**；将来要做多房间需自己补 PTP |
| **H4** | iOS 版本演进导致 AP2 握手变化 | 某天 iPhone 投不上 | 真机矩阵（iOS 17/18/26）回归；`shairplay` 上游较活跃，跟随升级 |
| **H5** | 蓝牙 A2DP 本身 150–250 ms 延迟 | 手机上按播放到音箱出声的体感偏慢 | 接收缓冲压到 300 ms 以内；GUI 显示实测端到端延迟，让用户知道瓶颈在蓝牙不在协议 |

### 11.2 未验证项

**已由调研结清**（证据见 `research/integration/shairplay-api.md` / `fusionplay-miservice-api.md`）：

1. ✅ `shairplay` 的 PCM 交付形态：`AudioHandler` trait + `audio_process(&mut self, samples: &[f32])`，**交错 f32、±1.0**，没有帧数参数（`frames = samples.len()/channels`），**没有 PTS**；回调跑在库自己的线程上，必须立刻拷贝走。
2. ✅ `shairplay` 多实例：同进程可跑多个 `RaopServer`，`port` 自动 +1（最多 +100），真实端口读 `service_info().port`；mDNS 由库内置（`mdns-sd`）。
3. ✅ FusionPlay **公共 API 不交付 PCM**，只有 JSON 事件；端口/mDNS/免账号的全部细节也在该文件里。
4. ✅ miservice 与接收链路无关（见 §1.1、§8.2）。

**仍未验证（按优先级）**：

1. **小米手机能否发现并投放到一台运行 FusionPlay SDK 的 Windows PC**（H1，M4a 结清；需抓包看走到哪一层）。
2. FusionPlay 在 **Windows MSVC** 上能否编译（它主打 Android；代码是纯 Rust + cpal，预期能，但未实测）。
3. `ReceiverIdentity` 持久化后，手机是否把它当**同一台设备**（不再每次重新配对）。
4. 妙播真实端口：`_lyra-mdns` 的 SRV 写 5353 而代码监听 55982；`_mi-connect` 的 SRV 56666 没有实际绑定。以抓包为准。
5. HyperOS 1/2/3 与机型（L16A/L16B/LX06）在「投放到第三方接收端」上的行为差异。
6. `shairplay` 在 Windows 24H2 上的 **AP2 端到端**可用（上游 CI 只对 windows-gnu 做 `cargo check --lib`）。
7. `shairplay` 默认 RTSP 端口（5000 vs 7000）行为；5353 与 Windows 自带 Bonjour 共存。
8. AP1 TXT 里缺 `pk` 时 iOS 是否接受。
9. 双 sidecar 同时跑的 CPU / 延迟开销（预期 < 3% 单核）。
10. 切音源时 WASAPI 端点是否真的不需要重开（R8 的重采样就是为此设计；需实测确认不咔声）。
11. 蓝牙 A2DP 150–250 ms + 接收缓冲，端到端能否 < 1.5 s。

---

## 12. 第一步（可执行的命令）

```powershell
# 0) 基线确认（工作区目前有未提交改动，别把它们混进来）
cd E:\AI\MiPlay
git status --short

# 1) 拿旧仓库里可直接复用的 Rust 代码（自有 MIT/Apache，见 §8.4）
git show 661c59b^:crates/miplay-core/src/token.rs   > .scratch\old-token.rs
git show 661c59b^:crates/miplay-core/src/mina.rs    > .scratch\old-mina.rs
git show 661c59b^:crates/miplay-core/src/device.rs  > .scratch\old-device.rs
git show 661c59b^:crates/miplay-server/src/hub.rs   > .scratch\old-hub.rs

# 2) 建共享协议 crate（M0）
cargo new --lib crates\bridge-proto
cargo new --bin crates\bridge-airplay
cargo new --bin crates\bridge-miplay

# 3) 验证 shairplay 能编、能跑（先别接进主程序）
cd .scratch
cargo new --bin sp-probe; cd sp-probe
cargo add shairplay --features ap2
cargo run --release        # 实现 AudioHandler → 收 PCM 写 wav，iPhone 投放验证

# 4) 防火墙（管理员 PowerShell）——先手动验证，M3.5 再做进安装器
#    AirPlay：TCP(RTSP 7000) + UDP(数据/时序，动态端口) 都要入站
New-NetFirewallRule -DisplayName "StereoLink AirPlay TCP" -Direction Inbound -Program "E:\AI\MiPlay\target\release\airplay-bridge.exe" -Protocol TCP -Action Allow
New-NetFirewallRule -DisplayName "StereoLink AirPlay UDP" -Direction Inbound -Program "E:\AI\MiPlay\target\release\airplay-bridge.exe" -Protocol UDP -Action Allow
#    妙播：TCP 8899 + UDP 55982 + UDP 5353，无动态媒体端口（PC 是外连方）
New-NetFirewallRule -DisplayName "StereoLink MiPlay TCP"  -Direction Inbound -Program "E:\AI\MiPlay\target\release\miplay-bridge.exe" -Protocol TCP -Action Allow
New-NetFirewallRule -DisplayName "StereoLink MiPlay UDP"  -Direction Inbound -Program "E:\AI\MiPlay\target\release\miplay-bridge.exe" -Protocol UDP -Action Allow

# 5) M4a spike（与 M0 并行，2–3 天出结论）
git clone --depth 1 https://github.com/rosienosiesie/FusionPlay-Android .scratch\FusionPlay
# 在 .scratch\mp-probe 里依赖 FusionPlay.MiPlaySdk，写 main.rs：
#   MiPlayReceiver::start(config, emitter) → 播到指定 output_device，事件全量打印
# 判据：小米手机「投放到音箱」里出现这台 PC，且投放后 PC 出声
```

---

## 附录 A：与第一版文档的关系

`docs/miaobo-airplay-integration.md`（410 行）是**四方向综述**（发送端 / 接收端 / 云投放 / 局域网直推）。本文档是其中「接收方向」的**落地版**，并把用户指定三件套的具体分工、IPC 契约、许可方案、里程碑补齐。两者不冲突，可同时保留。

## 附录 B：调研材料

`research/integration/`（gitignored）：
- `airplay.md` — AirPlay 1/2 收发两向、shairplay 实编译、Windows 坑、Rust crate 与许可
- `miaobo.md` — 妙播两条链路、公开实现盘点、合规、工作量
- `mixplay-arch.md` — MiXPlay 进程拓扑与端口清单
- `shairplay-api.md` — shairplay 0.10 精确 API（PCM 交付方式、多实例、时间戳）
- `fusionplay-miservice-api.md` — FusionPlay 接收端 API + miservice 类与方法 + token 流程

## 附录 C：修订记录

| 版本 | 时间 | 变更 |
|---|---|---|
| v1 | 2026-10-21 | 首版：sidecar 架构、IPC 契约、改造点、里程碑 |
| **v2** | 2026-10-21 | 读源码后修正三处：① 妙播媒体通道方向（PC 出站，§4.2）；② FusionPlay 无 PCM 出口 → 新增 §4.3 双路线（模式 a 零改 spike / 模式 b fork），里程碑拆 M4a/M4b；③ miservice 与接收链路无关（§1.1、§8.2）。同时按实测补齐：shairplay `AudioHandler` 真实签名、妙播端口清单（TCP 8899 / UDP 55982 / UDP 5353，无动态 RTSP）、防火墙规则由 2 条改为 4 条、`f32le` 为 IPC 主格式、切源重采样（R8） |

