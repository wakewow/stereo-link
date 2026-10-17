# 架构决策记录（ADR）

> 状态：**进行中**。每条结论都标注了证据来源与置信度；标 🔬 的是仍在验证中的，
> 不要把 🔬 当作已定论。最后更新：**用户否决 AirPlay 路线后（m01340）**。

---

## 0. ⚠️ 路线决定（最新，覆盖下文所有相反表述）

**用户原话（m01340）**：

> 不使用AirPlay,使用小米妙播,AirPlay的延迟高,放弃AirPlay的路线,仅使用小米妙播的路线

**决定**：

| 路线 | 状态 |
| --- | --- |
| §2.1 LAN 直推 MiPlay 私有协议 | ❌ 技术上不可行（账号绑定的 Lyra 信任层），已否决 |
| §2.2 RAOP / AirPlay 直达 | ❌ **用户否决**（延迟高）。代码与调研保留归档，**不再演进** |
| §2.3 云 `play_url` + 本机 HTTP 流 | ✅ **当前唯一路线**，用户已人耳确认出声 |

因此项目的全部工程投入集中在 §2.3：**把它从「能出声的脚本」做成「低延迟、可用的产品」**。
`crates/miplay-airplay`、`stream` 子命令、`research/lan/`、`research/airplay/` 一律**只归档不改动**。

---

## 1. 目标与约束

**用户目标（原话 m00497）**：把小米妙播上的设备做成 Windows「选择声音输出」里能选的设备，
配一个网页前端查看妙播设备，用 Rust 重写，git 版本控制与审计。
动机是**当前方案延迟太高**。

**硬约束（已核实，不可绕）**

| 约束 | 证据 |
| --- | --- |
| 音箱的播放只能由小米云下发触发 | task-6：音箱本地 8899/TCP 未授权即静默；miio UDP 54321 hello 有应答但加密 RPC 无应答；80/443/8080 关闭 |
| 免费/无 Pro 环境下没有「组播」概念 | task-3：云端无 group 对象，参考实现也无 group；立体声组合必须**每只 did 各发一条指令** |
| 用户态无法注册音频 render 端点 | task-5：端点由 AudioEndpointBuilder 从内核驱动的 `KSCATEGORY_AUDIO` 接口派生 |
| 妙播设备在局域网里**不是**可直接推流的接收端 | task-6 见下 §2.1 |

---

## 2. 三条音频通路，逐条判定

### 2.1 ❌ LAN 直推 MiPlay 私有协议（已否决）

task-6 用 Python 独立实现了 MiPlay 兼容握手：四元组 MD5 派生密钥 → AES-128-CBC +
链式 IV + 私有 CRC-32，在 L16A/L16B 两台真机上**均完成 `SAFETY_INFO → SAFETY_AUTH →
SAFETY_AUTH_ACK` 双向 HMAC 互认证，无需小米账号、无需证书**。

但**认证之后一切静默**：复刻 HyperOS 真实发送端序列后，所有指令无应答，
`OPEN(wfd://…?mirrorMode=1)` 既无 `OPEN_ACK` 也无 RTSP 回连，连接被断开（两台一致）。

卡点是**账号绑定的 Lyra / Mi Connect v3 信任层**（P-256/HKDF-SHA256/AES-256-GCM、
`com.milink.service:smartplay`、`account_hash_hex`、`external_account_required`、
`miplay_lan_removed`），互连阶段只走到 2/5。FusionPlay 源码原话佐证：
传输连接与 SafetyAuth「刻意不被视为可播放路由」。

→ **不要把 LAN 直推排进架构。** 详见 `research/miplay/miplay-protocol.md`。

### 2.2 ❌ RAOP / AirPlay 直达（**已被用户否决**，以下为历史记录）

> 用户理由：延迟高。实测延迟预算约 355 ms（`crates/miplay-airplay` 的 `LatencyBudget`），
> 但用户体感「延迟太高」，明确要求放弃。
> **不再投入。** 代码 `crates/miplay-airplay`、`miplay-bridge stream` 子命令、
> `research/lan/`、`research/airplay/`、`docs/end-to-end.md` 全部保留归档。

历史调研结论（`research/lan/`）：

- 音箱在 mDNS 上同时广播 `_airplay._tcp` 与 `_raop._tcp`，端口 **7000**；
  `_raop._tcp` 的 TXT：`et=0,1`（加密类型 0/1）、`cn=0,1`、`tp=UDP`、`vn=65537`、`da=true`。
- `GET /info` 返回完整 bplist：`AirPlay;2.0.10`、`PTP-AirPlay;2.0.10`、
  `protocolVersion 1.1`、`sourceVersion 366.0`、`features 496157849520640`、`model L16A`。
- `OPTIONS` → `200`，`Public: ANNOUNCE, SETUP, RECORD, PAUSE, FLUSH, FLUSHBUFFERED, TEARDOWN, OPTIONS, POST, GET, PUT`
- **`ANNOUNCE` → `200 OK`**，多种变体都接受（裸 L16、L16+Apple-Challenge、ALAC、
  ALAC+Apple-Challenge、ALAC+X-Apple headers、`c=IN IP4 0.0.0.0`）；只有 AAC → `406`。
- **`SETUP` 是墙**：同连接续发 → `403 Forbidden`；每条新连接 → `455 Method Not Valid In This State`。
- 后来用 pyatv 走 HAP transient 配对把整条链路打通了（`research/lan/airplay2_stream.json`），
  但**踩到一个隐藏根因**：AirPlay 2 接收端会扣住 SETUP 的回复，直到它自己发出的
  32 字节 NTP 时序包被应答；Windows 防火墙默认丢入站 → 必须为发送端进程加 UDP 入站规则
  （见 `docs/firewall-requirement.md`）。修好后协议全 200 OK、~355 ms 预算，
  **但音箱始终不出声**，真因未定论 —— 现已无必要追查。

### 2.3 ✅ 云 `play_url` 触发 + 本机 HTTP 流（**当前唯一路线**）

链路：`本机音频 → 采集 → 本机 HTTP 服务提供无限长 WAV 流 → 用 MiNA play_url 让音箱来拉`。

**已在真机跑通**（Python 实现 `mixplay-pc-audio/pc_stereo_hub.py`，用户确认两只音箱都出声）：

- 推流入口也可用 MiXPlay 的 `POST /api/v1/audio/stream`（Header `X-Target-ID`），
  但免费档只允许**一条**管道（`audio_hub.py:385 acquire_source` 会停掉已有管道），
  所以左右分离必须自建流服务，MiXPlay 只负责代发 `control/play`。
  **产品化后不再依赖 MiXPlay**：直接用 `miplay-core` 的 `MinaClient::play_by_url`。
- WAV 头 `dataSize` 写死 `0x7FFFFF00`（假长度），音箱接受；ffmpeg/ffprobe 会因此报错，属正常。
- 实测：`扬声器 (Chu2 DSP)` 采集 → 拆 L/R → 各自一条无限流 → 两只音箱分别拉到
  `ip=192.168.1.171`（Xiaomi Sound）与 `ip=192.168.1.21`（Xiaomi Sound-左），**左右分离人耳确认**。

**延迟构成**：云指令往返（每次起播）+ 音箱端拉流缓冲。优化方向 = **减少云触发次数**
（常驻流，起播后不断开）而不是压缩网络传输。**具体分量正在实测中（task-14）。**

---

## 3. 虚拟音频端点（最大工程风险）

task-5 结论：**用户态无法注册 render 端点**。端点由 AudioEndpointBuilder 从内核驱动注册的
`KSCATEGORY_AUDIO` 接口派生，客户端只读不写。本机 26 个 render 端点的
`SWD\MMDEVAPI\{...}` 实例 ID 全部指向真实 devnode，可作旁证。

候选路线：

| 路线 | 判定 |
| --- | --- |
| USB/IP（Virtual-Cables 思路） | ❌ Windows 侧 VHCI 只能靠 `usbip-win2`，其驱动**未签名**，必须开 Test Signing；仓库内无 `.inf/.sys` |
| 端点改名（改 `MMDevices\…\Properties`） | ❌ 死路：`reg add …,14` → Access denied；官方 `IPropertyStore::SetValue` → `E_ACCESSDENIED` |
| **端点改名（改 `Enum\<设备>\0000\FriendlyName` + disable/enable）** | ✅ task-8 三态实测通过，回滚无残留（`research/vad/rename-probe.md`） |
| **复用已装虚拟声卡的自环** | ✅ task-8 矩阵：7/7 render 端点自身 `[loopback]` 全通，−12.04…−20.83 dBFS，延迟 6.4–67.9 ms |
| fork Scream（MS-PL）改 INF + 自签 | ⚠️ task-11 实测：驱动**装得上但加载不了** —— `0xC0000428 STATUS_INVALID_IMAGE_HASH`、`CM_PROB_UNSIGNED_DRIVER 0x34`；需开 test signing 或 MS attestation/EV 证书。机器已干净回滚 |
| 自研 PortCls 小驱动，每音箱一个实例 | 最终形态，8~15 人日，发布需 EV 证书做 attestation 签名 |

**零驱动能做到什么（task-8 最终裁定）**：能让「小米妙播设备」出现在 Windows 声音输出列表，
但形态固定为 `扬声器 (小米妙播设备)`（前缀「扬声器」去不掉），**且无法新增端点，只能借用机器上
已有的虚拟声卡**（本机 2 个）。多设备零驱动做不到。

→ 结论：**采集默认输出端点不需要任何虚拟声卡**（这是主路径，task-8 已证）；
虚拟端点只是「让每个音箱在 Windows 输出列表里独立可选」的锦上添花，不是主路径前提。

---

## 4. 选定架构（分层，按风险解耦）

```
┌─────────────────────────────────────────────────────────────┐
│ 任意 Windows 程序                                             │
│   └─ 选择输出设备：默认端点（或「小米妙播 · <音箱名>」）        │
└───────────────────────────┬─────────────────────────────────┘
                            │ (1) 采集
┌───────────────────────────▼─────────────────────────────────┐
│ miplay-audio (Rust)  ✅ 已交付（121 测试全绿）                 │
│   WASAPI 回环/端点采集 → 44100/2ch/s16 → 声道分发 → RingBuffer  │
└───────────────────────────┬─────────────────────────────────┘
                            │ (2) 无限长 WAV 流（每目标一条）
┌───────────────────────────▼─────────────────────────────────┐
│ miplay-server (Rust, axum)  ← task-15 进行中                  │
│   GET /hub/<did>.wav  +  MiNA play_by_url 触发（尽量少）        │
└───────────────────────────┬─────────────────────────────────┘
                            │ (3) 云指令让音箱来拉流
┌───────────────────────────▼─────────────────────────────────┐
│ 小米云 MiNA  →  小爱音箱（LAN 内拉取本机流）                     │
└─────────────────────────────────────────────────────────────┘
```

**分层理由**：第 (3) 段是唯一被小米云绑定的部分，不可替换；第 (1)(2) 段完全本地，
是延迟优化与多设备分发的主战场；虚拟端点（第 0 段）是独立风险，**失败也不影响 (1)(2)(3) 的价值** ——
没有虚拟端点时，用户仍可用「采集默认输出设备」的方式把整机声音送出去。

---

## 5. 延迟策略

| 手段 | 说明 |
| --- | --- |
| 常驻流 | 音箱起播后**不主动断开**，避免每次播放都付一次云指令往返 |
| 减少云触发 | 只在目标集合变化/连接断开时重新下发，不随每次播放重新触发 |
| 环形缓冲按 5 秒 | 慢客户端跳帧而不是拖慢整体；新读者从当前时刻开始 |
| 端到端实测 | **task-14 进行中**：服务端读滞后（= 音箱侧缓冲深度）是最客观的指标 |

---

## 6. 未决问题

1. 🔬 **音箱拉流缓冲到底有多深** —— 由 task-14 用「服务端读滞后」直接测出。
2. 🔬 **这条路线能压到多少毫秒** —— task-14 给出分量表后才能回答。
3. 🔬 **零驱动能否做到「多设备」** —— task-8 已裁定：**不能**；需要驱动（Scream 派生 3~7 人日）。
4. **虚拟端点产品化** —— task-12 的 PoC（改名 + 回环采集 + 云推流）已跑通，可移植为正式功能。

> 已作废的未决问题（AirPlay 相关）：RAOP `SETUP` 为什么 403/455；AirPlay 协议全 200 但不出声的真因。
