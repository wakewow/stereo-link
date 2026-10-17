# 小米妙播 MiPlay 协议逆向报告

> 产出：`miplay-protocol-analyst`（共享任务 task-6）
> 日期：2026-10-17
> 范围：判定「第三方能否作为 MiPlay 发送端，在局域网内低延迟把音频推给小米音箱」，并给出次优低延迟方案
> 方法：只读逆向（源码精读 + 二进制 strings + 本机实机探测）。未对音箱做任何破坏性操作；唯一向音箱写入的动作是既有的 `play_url` 出声链路（≤10 s，音量未改动）。

---

## 0. 结论摘要（TL;DR）

| 问题 | 判定 | 置信度 |
|---|---|---|
| **第三方自作发送端，走 8899 控制面在 LAN 内直推音频给小米音箱** | **不可行** | 高（对已测路径是**已证否**） |
| 「任何形式的第三方直推」是否绝对不可能 | 存疑（未穷尽证明；卡点已定位，见 §9.3） | — |
| 音箱是否暴露可用的本地控制接口 | **部分**：miio UDP 54321 有 hello 应答，但加密 RPC 全部无应答；无 80/443/8080/ubus | 高 |
| 成对音箱（卧室立体声组合）寻址 | **发两条指令（每个 did 一条）**；参考实现均无 group 概念 | 中（推测，附证据） |

**卡点一句话**：传输层加密不是障碍（我们已完整破解并通过实机握手），真正的门是**小米账号绑定的 Lyra / Mi Connect v3 信任层**——它位于「发现」与「媒体会话」之间；没建立它，音箱接受 TCP 连接与 SafetyAuth，然后**静默忽略一切控制指令**，并在收到媒体类指令时直接断开连接。

---

## 1. 方法、合规与可复现性

### 1.1 合规提示（重要）

- **FusionPlay-Android 是 AGPL-3.0**（仓库根 `LICENSE` = GNU AFFERO GENERAL PUBLIC LICENSE v3, 19 Nov 2007；`src/FusionPlay.MiPlaySdk/Cargo.toml` 声明 AGPL 族许可）。本报告只引用**协议事实、字段布局、常量值**用于互操作分析，**不得把其代码复制进 miplay-bridge**。若将来需要以 FusionPlay 代码为基础，整个 miplay-bridge 必须同样以 AGPL-3.0 发布（含网络服务条款），这是一个产品级决策，需 Lead/用户确认。
- MiXPlay 的 `bin/miplay-bridge.exe` 为第三方闭源二进制（无许可证声明）。我们**仅做只读 strings 提取**，未 patch、未执行、未修改，也未把其代码/资源并入本项目。提取物仅作协议线索，属于互操作研究范畴。
- 全程只读使用用户凭据（`C:\Users\Administrator\conf\.mi.token`、`config.json`、`mixplay-pc-audio\scout\device_list_raw.json`），未外传、未写入任何 git 跟踪文件。

### 1.2 分析对象与版本

| 对象 | 标识 |
|---|---|
| FusionPlay-Android | commit `b43fe1130c3fdb233dcb41c2da174b858eb44cc2` (2026-08-29), VERSION 1.2.4，clone 到 `%TEMP%\miplay-re\FusionPlay-Android`（只读） |
| MiXPlay | 1.4.6；`mixplay\miplay_bridge.py`（414 行，Python 明文）；`mixplay\bin\miplay-bridge.exe` |
| miplay-bridge.exe | size 1716736 B，**sha256 `88c5b852d49dbbfeb8f70b57b92ef518054fb8e8cbb7a91ccc744272e67b4a13`** |
| 音箱 | L16B（Xiaomi Sound，did=934947888，MiNA `0c9041b3-…-497a266b1e93`，192.168.1.171）；L16A（Xiaomi Sound-左，did=501718792，MiNA `f58c6453-…-1335bb84172b`，192.168.1.21）；L05C（小爱音箱Play增强版，did=931892755） |

本报告引用 FusionPlay 源码时统一用仓库相对路径 `src/FusionPlay.MiPlaySdk/src/<file>:<line>`。

### 1.3 交付脚本（均在 `research/miplay/`）

| 脚本 | 用途 |
|---|---|
| `probe_miplay_handshake.py` | 8899 控制面 SafetyAuth 互认证 + 帧收发库（第一版） |
| `probe_miplay_sender.py` | 完整发送端骨架：帧编解码、AES/CRC、握手、RTSP 服务器（被后续脚本 import） |
| `probe_miplay_postauth.py` | 认证后指令阶梯探测（0x40/0x14/0x16/0x6c/0x1e…） |
| `probe_miplay_firstcmd.py` | “认证后第一条指令”对照实验（排除首帧差异） |
| `probe_miplay_full_sender.py` | **决定性实验**：逐帧复刻 HyperOS 真实发送端序列 + 等待 OPEN_ACK / 回连 RTSP |
| `probe_miplay_env.py` | mDNS 全量 dump（SRV/TXT）+ 候选本地端口探测 |
| `probe_miio_udp.py` / `probe_miio_local.py` | miio UDP 54321 hello 与加密 RPC（本地无云控制通道）验证 |
| `probe_miplay_keysearch.py` | 在 exe/固件线索中检索密钥材料（无结果） |
| `probe_speaker_pull.py` | **待用**：测量音箱拉本地 HTTP 流时的缓冲/预读行为（`Content-Length` vs `chunked` 两种模式） |
| `mixplay_miplay_bridge.exe.strings.txt` | miplay-bridge.exe 可打印字符串全量（16277 条，只读提取） |

---

## 2. 问题 1：FusionPlay-Android 是接收端还是发送端？

### 2.1 结论：**接收端（Receiver / Sink）**，而且明确定位为「小米妙播兼容**接收** SDK」

证据（全部已验证）：

1. `README.md`：“FusionPlay 是一个面向全平台的局域网音乐投放**接收器**，支持 MiPlay AirPlay DLNA”。
2. `src/FusionPlay.MiPlaySdk/README.md:3`：“独立的纯 Rust 小米妙播音频兼容**接收** SDK”；同文件 13–24 行说明：小米公开的妙播 SDK 是**手机应用侧入口**，并没有公开通用接收端的设备证书、硬件信任根或准入协议；日志 `vendor_attestation_verified=false` 表示只做了兼容会话认证（**没有官方设备认证**）。
3. 代码结构：`ReceiverIdentity`（lib.rs:93）、`ReceiverConfig`（lib.rs:117）、`ReceiverController`（lib.rs:192）、`MiPlayReceiver::start`（lib.rs:242）——全是接收侧 API。
4. `lib.rs:348 pub const fn lyra_command_port(&self) -> u16` 对外暴露 Lyra 命令端口，说明它是被手机发现/连接的一方。
5. 媒体面反向 RTSP（§7.1）：接收端**主动外连**发送端，这是接收端行为。

**这条对我们是决定性的**：FusionPlay 给不出“发送端怎么推”的实现，但它逐字节给出了**接收端期望发送端做什么**（即发送端义务的完整规范），这正是我们用来构造发送端实验的依据。

---

## 3. 设备发现（Discovery）

来源：`src/FusionPlay.MiPlaySdk/src/discovery.rs`（常量 17–29 行）+ `probe_miplay_env.py` 实机 dump。

| 项 | 值 | 来源 |
|---|---|---|
| mDNS 组 | `224.0.0.251:5353`（v6 `[0xff02::0x00fb]`） | discovery.rs:132-133 |
| 服务类型 1 | `_lyra-mdns._udp.local.`（qname 字节精确 `\x0a_lyra-mdns\x04_udp\x05local\x00`） | discovery.rs:27, 1818 |
| 服务类型 2 | `_mi-connect._udp.local.`（`\x0b_mi-connect\x04_udp\x05local\x00`） | discovery.rs:28, 1964 |
| Mi Connect 发现端口 | `56666`（`MI_CONNECT_DISCOVERY_PORT`） / Lyra 控制端口 `5353` | discovery.rs:19-21 |
| TTL | Lyra `120 s`；on-demand Mi Connect `10 s`（空闲 12 s 退出） | discovery.rs:22-25 |
| 宣告间隔 | `60 s` | discovery.rs:26 |
| 历史/退役实例名 | `2433CD31`、`D4349E25`、`Xiaomi17ProMax(_TnyWeXvGA)` | discovery.rs:83, 87 |

### 3.1 实机 mDNS（已验证，2026-10-17）

```
PTR  _mi-connect._udp.local -> 小爱音箱-1695._mi-connect._udp.local   (from 192.168.1.21)
SRV  小爱音箱-1695._mi-connect._udp.local -> L16A-1738.local:56666    ttl=10
TXT  小爱音箱-1695._mi-connect._udp.local:
       name=小爱音箱-1695   dev=4   sec=2   flags=Ag==   version=65545
       idHash=ZGEz          apps=[5]
       appsData=<base64 protobuf: mico.device_id = f58c6453-0268-4df5-a220-1335bb84172b>  ← L16A 的 MiNA
A     ... -> 192.168.1.21
```

要点：
- 音箱**主动以 mDNS 宣告自己**（`dev=4` = 音箱类；`apps=[5]`；`sec=2`），无需小米云即可被发现。`appsData` 里是 protobuf，字段就是设备的 MiNA（与云侧 `device_list` 的 MiNA 一致）。
- FusionPlay 侧对应实现：`MiPlayAdvertisement`(discovery.rs:143)、`register`(150)、`build_txt_data`(2122)、`build_mi_connect_txt`(2042)、`build_app_data`(2362)、`encode_xiaomi_debug_ip`(2174)，以及 Windows DNS-SD 开关 `ENABLE_WINDOWS_DNS_SD_REGISTRATION: bool = false`(discovery.rs:383) 与 `spawn_windows_dnssd_responder`(591)。
- 有效性：本次窗口只捕获到 L16A 的 SRV/TXT（L16B 未在该窗口应答），但两台的 A 记录/控制端口均可达（§5.4）。**mDNS 宣告 = 已验证；两台 TXT 字段完全一致 = 未逐台验证**。

---

## 4. 控制通道：TCP 8899

### 4.1 帧格式（已验证：既来自源码，也来自本机实机收发）

来源 `src/FusionPlay.MiPlaySdk/src/protocol.rs:1842-1868 (write_frame)`，并与实机 DEVICE_ID 招呼逐字节吻合：

```
偏移  长度  字段
0     1     magic = 0x24 ('$')
1     1     outer_type      （0x00 = 媒体/控制命名空间；0x14 = 兼容安全命名空间 wrap；0x04 = ALONE 播放器命名空间）
2     1     command
3     2     sequence (u16 BE，写入时截断自 u32)
5     4     body length (u32 BE)
9     N     body（明文或密文）
```

实机样本（音箱 accept 后**立即主动**下发，明文）：
`24 00 28 02 C9 | 00 00 00 0D | "7036458993394"` → outer=0x00, cmd=0x28(DEVICE_ID), seq=713, len=13。
L16A 同结构：`24 00 28 01 9F | 00 00 00 0D | "6966745247931"`（seq=415）。
> 注意：不同会话该十进制号不同（L16B 另一次为 `7411576332029`）→ **这是每会话的临时数字设备号，不是稳定 did**。这是一个容易误判的点（我第一版假设它是设备 id，已修正）。

`MAX_CONTROL_BODY_LENGTH = 16 * 1024 * 1024`（protocol.rs:1825）。

### 4.2 兼容安全握手（已验证：两台真机全部通过）

`protocol.rs:1235-1332`（接收端实现）+ `protocol.rs:2912-3144`（抓包回放测试）。

顺序（**顺序是承重结构**，protocol.rs:1246-1253, 1301-1304）：

```
S -> C : DEVICE_ID (0x28, 明文, 数字设备号)              # 音箱 accept 后立刻发
C -> S : SAFETY_INFO (cmd 0x00, outer 0x14, 明文, wrap("cmd",
             {"aesIvTypes":"3","aesKeyTypes":"3","authAlgorithmTypes":"7",
              "authKeyTypes":"1","integrityTypes":"1"}))
S -> C : SAFETY_INFO_ACK (0x01) 立刻紧跟 SAFETY_AUTH (0x02, 自己的 32hex 挑战)
C -> S : SAFETY_AUTH (0x02, 自己的 32hex 挑战) + SAFETY_AUTH_ACK (0x03,
             wrap("ack", {"authMsgAck":"<HMAC-SHA256(auth_key, S 的挑战)>","result":"0"}))
S -> C : SAFETY_AUTH_ACK (0x03, HMAC-SHA256(auth_key, C 的挑战))
```

- `authMsg` 挑战为 32 位十六进制串（随机）；HMAC 输入是**挑战的 ASCII 字节**，输出 hex。
- 密钥协商**与本机账号、证书、任何预共享秘密无关**：`generate_auth_key`（protocol.rs:1991-2010）是 TCP 四元组的纯函数：
  `auth_key = hex(md5(concat(每位数字字符 + 0x31) of "local_ip local_port remote_ip remote_port"))`
  回归测试 protocol.rs:2415-2422：`192.168.31.128:8899` + `192.168.31.207:49668` ⇒ `b0175093fe7e51f8ac3cdeb42cd9f513`。
- 会话 AES key = `auth_key` 的**前 16 个 ASCII 字符**（protocol.rs:1077-1078）。
- 结论：**握手完全可被第三方复现**——我们已用 Python 独立实现并与两台真机完成双向 HMAC 验证（`** MUTUAL AUTH VERIFIED **`）。

### 4.3 控制报文加密

`protocol.rs:1870-1932`（`ControlCipher`）：

- 算法：**AES-128-CBC**；密钥 = `auth_key[:16]` 的 ASCII 字节；初始 IV = key；**链式 IV**：每次操作后 `iv := 上一次密文的最后 16 字节`。
- 填充：`pad = 16 - (plain.len() % 16)` 个 **0x00**（不是 PKCS#7），pad 长度记在密文头。
- 线上 body 布局：`00 07 01 E0 <pad>` + `integrity(BE32)` + `ciphertext`（示例见 protocol.rs:2876-2909 的逐字节抓包测试 `encrypted_auth_frames_match_captured_mipcaudio_bytes`）。
- `integrity` = MiPlay 私有 CRC-32：非反射多项式 `0x04C11DB7`，初值 `0xFFFFFFFF`，**无最终异或**（protocol.rs:1939-1958）。工作实现见 `probe_miplay_sender.py`。**MiPlay 在解密前先校验 integrity**，值不对则手机端直接关 socket（protocol.rs:1897-1900）。
- 明文包装 `wrap_payload(key,data)`（protocol.rs:1960-1971）：`[key_len u8][key ascii][30 u32 LE][data_len u8][data]`，常见 key = `"cmd"` / `"ack"`（`unwrap` 要求 data_type==30）。

### 4.4 指令表（protocol.rs:25-88）

| 命名空间 | 指令 |
|---|---|
| 安全 (outer 0x14) | SAFETY_INFO 0x00/0x01, SAFETY_AUTH 0x02/0x03 |
| 主 (outer 0x00) | **OPEN 0x00 / OPEN_ACK 0x01**、PAUSE 0x04/0x05、RESUME 0x06/0x07、SET_VOLUME 0x0c/0x0d、GET_VOLUME 0x0e/0x0f、SET_MEDIA_INFO 0x12/0x13、GET_MEDIA_INFO 0x14、HEART_BEAT 0x1a/0x1b、GET_STATE 0x1c/0x1d、NOTIFY 0x22、GET_VERSION 0x36/0x37、SET_PLAY_SOURCE 0x40/0x41、PLAYER 反向控制 ACK 0x45/0x47/0x49/0x4b/0x4d、SET_POSITION 0x56/0x57、SET_MEDIA_STATE 0x5e/0x5f、DEVICE_ID 0x28、AUTH_ACK 0x29、GET_DEVICE_INFO 0x1e/0x1f、SET_DEVICE_INFO 0x58/0x59、GET_MIRROR_MODE 0x34/0x35、**SET_MIRROR_KEY 0x6c/0x6d** |
| ALONE (outer 0x04) | SET_STATE 0x14/0x15, GET_STATE 0x16/0x17, SET_MEDIA_INFO 0x18/0x19（`0x0416 ≠ 0x0016`，protocol.rs:79-81；回 ACK 必须保留 0x04 高字节，否则手机的**反向控制**会卡死，protocol.rs:3061-3063） |

关键语义：
- `OPEN`（cmd 0x00, outer 0x00）**必须加密**（protocol.rs:1469 `OPEN if frame.encrypted`），body = `wfd://<发送端IP>:<端口>?mirrorMode=1`，用 `parse_wfd_url` 解析（protocol.rs:2043-2057；测试 protocol.rs:3147-3152）。
- `GET_MIRROR_MODE` 必须答 `[0,0,0,0,1]`（protocol.rs:70；模式 1 = 小米手机音频投送路由；模式 2 可传输音频但 HyperOS 拒绝接收端媒体控制，protocol.rs:1387-1390）。
- `NOTIFY`（0x22）是接收端→发送端的**非请求通知**，序列号必须单调递增（`FIRST_ENCRYPTED_NOTIFICATION_SEQUENCE = 8`，protocol.rs:71-77）；复用固定序号会让后续 pause/next 被手机当作重复帧静默丢弃。
- `SET_MIRROR_KEY`(0x6c) 携带媒体密钥 JSON：`{"authKey","streamKey","streamIV"}`，各恰好 16 ASCII 字节（protocol.rs:1393-1417；media.rs:198），ACK 0x6d body `[0]`。
- 互连阶段（`InterconnectStage`，protocol.rs:113-133）：`transport_connected → secure_channel_established → identity_exchanged → capabilities_exchanged → media_session_established`。
- **原话（关键）** protocol.rs:106-112：
  > “A transport connection and the legacy SafetyAuth exchange are deliberately not treated as a playable route. Xiaomi's public HyperOS architecture puts **device trust and capability negotiation between discovery and the media session**, so callers must wait for `MediaSessionEstablished` before exposing playback controls.”

---

## 5. 媒体面：反向 RTSP + RTP/MPEG-TS/AAC

来源 `src/FusionPlay.MiPlaySdk/src/media.rs`（接收端视角，即**发送端义务的规范**）。

### 5.1 拓扑（决定性）

接收端是 **RTSP 客户端**，主动外连发送端：`media.rs:917 fn run_rtsp_receiver(...)` 内 `TcpStream::connect_timeout(&remote, 5s)`，remote 取自主控通道 `OPEN` 里的 `wfd://host:port`（media.rs:928-930，注释 media.rs:939-943「已连接小米妙播 RTSP 源」）。
⇒ **发送端必须在自己的机器上监听一个 RTSP 服务器**（每个会话动态端口），音箱会连过来。

### 5.2 RTSP/WFD 协商（media.rs:1052-1220）

```
R->S : OPTIONS * RTSP/1.0     User-Agent: stagefright/1.1 (Linux;Android 4.1)
                              Require: org.wfa.wfd1.0
                              lib_version: audio-display-release2.1 2.1.5071614
                              [authMsg:<32hex 随机挑战>]
S->R : 200, Public: org.wfa.wfd1.0, GET_PARAMETER, SET_PARAMETER
           authKeyType:2  authAlgorithmVal:4  authMsgAck:<HMAC-SHA256(authKey, 挑战)>
S->R : GET_PARAMETER (body 含 wfd_audio_codecs_v2)
R->S : （另开两条 TCP 连接回同一 ip:port：“image” + “multi”/音频；记住本地端口）
       wfd_audio_codecs_v2: 15 3 3
       wfd_video_formats: none
       wfd_client_rtp_ports: RTP/AVP/TCP;interleaved mode=play
       miplay_support_image: none
       wfd_standby_resume_capability: supported
       wfd_content_SP_protection: 4 1 256 3 1 1 0 0
       wfd_support_secure_win:enable
       device_info: -1 -1 -1 -1 -1 -1 -1
S->R : SET_PARAMETER  wfd_timeoffset:<i64 微秒>  wfd_presentation_URL:<url>  wfd_trigger_method: SETUP
R->S : SETUP {url}  CSeq: 2  Transport: RTP/AVP/TCP;interleaved=0-1
                     MultiPort: image_port=..;multi_port=..
S->R : 200 + Session: ...
R->S : PLAY {url}  CSeq: 3
R    : 标记 stream_started（codec=aac, 48000 Hz, 2ch, 16bit, transport=rtp_mpegts_tcp）
默认 presentation url: rtsp://{host}/wfd1.0/streamid=0（media.rs:948）
```

`wfd_content_SP_protection: 4 1 256 3 1 1 0 0` 中的 **256** 与 FusionPlay 实现的「每 PES 只加密前 min(len,256) 字节」完全吻合——两条独立来源互相印证（FusionPlay 源码 vs MiXPlay sidecar 字符串 `0x154ad9`）。

### 5.3 媒体封装与编解码（media.rs:1286-1660）

- 帧：`'$' <channel:1> <len_be16>` + RTP 包（RFC2326 interleaved；len 必须 12..=65535，否则 `unexpected MiPlay data marker 0x{..}`）。
- RTP：version 2，`seq=bytes[2..4]`，`timestamp=bytes[4..8]`，处理 CSRC/扩展/padding；**RTP 时钟 = 90000 Hz**（`MIPLAY_RTP_CLOCK_HZ`，media.rs:1935）——尽管 payload type 33 = RTP/MP2T。
- Payload 必须是整数个 **188 字节 MPEG-TS 包**（media.rs:1350 `chunks_exact(188)`）；解析要求 `packet[0]==0x47`、**PID == 0x1100**、处理 adaptation field、按 payload_unit_start_indicator 组 PES。
- PES：必须以 `00 00 01 C0` 开头（音频 stream_id 0xC0），长度 `pes[4..6]` BE，`header_data_length=pes[8]`，PTS 取 `pes[9..14]`。
- 基本流 = **AAC ADTS**（同步 `0xFF` 且 `data[1]&0xF6==0xF0`；7 或 9 字节头）。采样率必须 **48000**（`MIPLAY_SAMPLE_RATE_HZ = 48_000`，media.rs:1934；`MIPLAY_AAC_FRAME_MICROS = 1024*1e6/48000`，media.rs:1967），2 声道 16 bit。
- 媒体加密（media.rs:1507-1604）：若 PES 负载已以 ADTS 同步头开头 → 视为明文直通；否则当存在 `StreamKeys` 时，只对每个 PES 负载的**前 `min(len,256)/16*16` 字节**做 AES-128-CBC 解密，密钥 `stream_key`，IV 取自 **PES private data**（PES_extension_flag 与 PES_private_data_flag 解析出 16 字节，`fn pes_private_data_iv`，media.rs:1575-1604）。
- **明文回退路径存在**（protocol.rs:1482-1489）：
  > “TV receivers do not participate in the Xiaomi-account PC key exchange. HyperOS therefore opens the WFD/RTSP route directly and sends an unprotected MPEG-TS audio stream. Requiring SetMirrorKey here incorrectly turns a valid TV session into an immediate disconnect.”
  即协议本身允许 **完全不加密的 MPEG-TS/AAC 流**（面向电视接收端）。**注意：这是“接收端允许”，不等于“音箱接受陌生发送端”**——见 §8/§9。

### 5.4 接收端缓冲与延迟预算（media.rs:1934-1974）

| 常量 | 值 |
|---|---|
| `MIPLAY_GROUP_PLAY_DELAY_MILLIS` | **800** |
| `MIPLAY_MINIMUM_QUEUE_DELAY_MILLIS` | **650** |
| `MIPLAY_MAX_BUFFER_MILLIS` | **1500** |
| `MIPLAY_MAX_OUTPUT_LATENCY_MILLIS` | 250.0 |
| `MIPLAY_SYNC_DEADBAND_MILLIS` | 2 |
| 漂移校正窗口 / 最大速率修正 | 12 s / 0.25% |
| 丢帧阈值（初始/运行） | 2 ms / 25 ms |
| 延迟上报间隔 / RTSP 轮询 | 250 ms / 50 ms |
| 时钟同步 | 40 字节 UDP timer 包，每轮 10 样本，fit 历史 32，最少 20 轮，重同步 50 次，强制轮询 100 ms，样本间隔 10 ms，最大 RTT 250 ms，最短 fit 跨度 5 s，最大频差 0.0005，相位/速率平滑 0.25，最大相位步进 5000 µs |

⇒ **MiPlay 自身的设计目标是接收端 0.65–1.5 s 的抖动缓冲**，即它本来就不是超低延迟协议。但这是**接收端（音箱）**的缓冲，不等于我们做发送端能控制的下限——不过它给出了「即使打通，端到端也大概率在数百毫秒量级」的先验。

---

## 6. 第二个控制/信任通道：Lyra（Mi Connect v3）

`src/FusionPlay.MiPlaySdk/src/lyra.rs`：`LYRA_COMMAND_PORT = 55_982`(26)、`KCP_PUSH=0x51`/`KCP_ACK=0x52`(28-29)、`LOGICAL_TRUST_LOCAL_NETWORK: u64 = 0x40`(30)、`SESSION_KEY_SALT:[u8;32]`(31)；`start_lyra_server`(79)、`handle_payload`(272)、`encrypt_payload`/`decrypt_payload`(743/756)、`extract_phone_account`(767)、protobuf 助手(773-859)、`kcp_header(...)`(904)。

MiXPlay sidecar 字符串（只读提取，含 sha256 与偏移）把这一层的性质讲得最清楚：

```
0x1540e0  P-256/HKDF-SHA256/AES-256-GCM
0x15457b  com.milink.service:smartplay
0x152a2e  crates\miplay-bridge\src\lyra.rs
0x1529fc  Xiaomi Lyra command socket
```
其余关键串：`lyra_secure_channel_established`、`missing Lyra auth frame`、`client_channel_id` / `server_channel_id` / `expected_channel_id`、`missing_session_key`、`encrypted logical frame before ECDH`、**`account_hash_hex`**、**`external_account_required`**、`account_required`、`vendor_service_required`、`identity_provenance=local_system`、`xiaomi_miplay_v3`、`miplay_lan_removed`、`query_scoped_lyra_and_mi_connect_without_legacy_lan_tv`、`legacy_lan_discovery`、`official_wired_support`、`5.1.25`、`1.10.fullCn`、`Release.0616209`、`transport_connected secure_channel_established identity_exchanged capabilities_exchanged media_session_established transport_disconnected discovery_interface_inventory`。

读法：
- 现代（HyperOS）妙播的局域网信道 = **Lyra = 小米互联 v3**：P-256 ECDH + HKDF-SHA256 + AES-256-GCM，KCP 逻辑帧，服务标识 `com.milink.service:smartplay`。
- 其中 **`account_hash_hex` / `external_account_required` / `account_required`** 直接表明：这条信道要求**账号身份**；`vendor_service_required` 表明还需要厂商服务背书。
- `miplay_lan_removed` / `query_scoped_lyra_and_mi_connect_without_legacy_lan_tv` 说明小米在新版本里**把旧的局域网直连路径移除了**，只保留 Lyra/Mi-Connect 范围内的查询。
- 阶段名与 FusionPlay 的 `InterconnectStage` 一一对应 → 两条独立来源一致：**传输层认证 ≠ 可播放**。

---

## 7. 实机探测：第三方发送端到底能不能直推

### 7.1 已通过的环节（两台真机，**已验证**）

| 探测 | L16B 192.168.1.171 | L16A 192.168.1.21 |
|---|---|---|
| TCP 8899 可连 | ✅ | ✅ |
| accept 后立刻收到 DEVICE_ID 招呼 | ✅（一次性附带 13 位数字号） | ✅ |
| SAFETY_INFO 协商被接受 | ✅ | ✅ |
| SAFETY_AUTH 双向 HMAC 互认证 | ✅ `MUTUAL AUTH VERIFIED` | ✅ `MUTUAL AUTH VERIFIED` |

⇒ **传输层加密/Y 已经完全不构成障碍**：第三方无需账号、无需证书即可与音箱完成 MiPlay 兼容安全握手。

### 7.2 认证之后：**全部静默**

| 发送的帧（均加密，序号按抓包序） | 期望 | 实测 |
|---|---|---|
| SET_PLAY_SOURCE 0x40 `{"ref_channel":"controlcenter","ref_function":"single_room","ref_content":"music_qq"}` | 0x41 | **无应答** |
| GET_MEDIA_INFO 0x14（空） | NOTIFY 0x22 | **无应答** |
| ALONE GET_STATE outer 0x04 cmd 0x16 `{"getState":"1"}` | outer 0x04 cmd 0x17 | **无应答** |
| GET_STATE 0x1c | 0x1d | **无应答** |
| GET_DEVICE_INFO 0x1e | 0x1f | **无应答** |
| HEART_BEAT 0x1a | 0x1b | **无应答** |
| SET_MIRROR_KEY 0x6c | 0x6d | **无应答** |
| GET_MIRROR_MODE 0x34 | `[0,0,0,0,1]` | **无应答** |
| GET_VOLUME 0x0e（含 seq=0x0100、outer 0x14+wrap 变体） | 0x0f | **无应答** |
| SET_MEDIA_INFO 0x12 | 0x13 | **无应答** |
| SET_VOLUME 0x0c | 0x0d | **未测**（写操作，按“不做非授权写入”原则主动跳过） |
| SET_PLAY_SOURCE 替换为「认证后第一条」做对照 | 0x41 | **无应答**（排除首帧差异） |
| SET_DEVICE_INFO 0x58 | 0x59 | **立刻断开连接** |
| **OPEN 0x00（outer 0x00，加密 body `wfd://192.168.1.4:<port>?mirrorMode=1`）** | OPEN_ACK 0x01 + **音箱回连我们的 RTSP 服务器** | **无 ACK、无回连**；完整序列实验中被断开 |

决定性实验 `probe_miplay_full_sender.py`（逐帧复刻 FusionPlay 抓包得到的真实 HyperOS 发送端顺序：握手 → SET_PLAY_SOURCE → GET_MEDIA_INFO → ALONE_GET_STATE → GET_STATE → GET_DEVICE_INFO → HEART_BEAT → SET_MIRROR_KEY → OPEN，本地 RTSP 服务器已就绪并监听）：

```
################ L16B 192.168.1.171 ################
  <- RX DEVICE_ID ... plain='7411576332029'
  ** MUTUAL AUTH VERIFIED (result='0') **
  -> TX SET_PLAY_SOURCE / 0x14 / 0x16 / GET_STATE / GET_DEVICE_INFO / HEART_BEAT / SET_MIRROR_KEY
  -> TX OPEN ... wfd://192.168.1.4:5942?mirrorMode=1
   waiting up to 12 s for OPEN_ACK and/or an inbound RTSP connection ...
   <- peer closed after OPEN: ConnectionError('closed')
===== RESULT =====
  pre-OPEN ACKs: NONE
  OPEN_ACK: False
  inbound RTSP connections: 0
  ==> speaker did NOT open a media session
```

L16A（192.168.1.21）结果**完全一致**（OPEN 端口 6034，同样 `peer closed`，无回连）。

### 7.3 本地控制面探测（**已验证**）

`probe_miplay_env.py` + `probe_miio_udp.py` + `probe_miio_local.py`：

| 探测 | 结果 |
|---|---|
| TCP 54321 / 80 / 443 / 8080 | 全部不可用（超时/拒绝）；**唯一开放的 TCP 端口是 8899** |
| mDNS 宣告的 56666 | UDP 服务端口（TCP 探测不适用） |
| **miio UDP 54321 hello** | ✅ **三台音箱全部应答**。L16B 应答 `21310020 | 0020 | 00000000 | 37ba2c30 | 00001c4a | ff*16` → `device_id=0x37ba2c30=934947888` ✓，stamp=7242，token 字段全 0xFF |
| miio 加密 RPC（`miIO.info`、`player_get_play_status`、`get_prop`） | ❌ **全部无应答**。使用的 token 取自用户云 `device_list`：L16B `ARiFLajTddLuFa2X`、L16A `ulTv2P4KB2DDMc3o`（均为 16 字节 ASCII，符合 MiNA 音箱风格；帧格式按 miio v2 标准实现，握手 stamp/device_id 均正确解析） |

⇒ 结论：**miio 本地明文发现可用，但本地加密控制层未打通**（token 被拒或该机型需要 MiNA/`mico` 专用本地凭据）。这是**负面结果 + 明确的未决点**，不是“不存在”的证明。

---

## 8. 问题 2：MiXPlay 的 miplay 实现

### 8.1 `mixplay\miplay_bridge.py`（414 行，Python 明文）——**接收端模拟器**

MiXPlay 让 **PC 假装成一台妙播音箱**（手机把音乐投到 PC）。它的 Python 侧负责与 Rust sidecar 通信、并把 sidecar 解出的 PCM 混入本机播放。

**进程与身份**
- `DEFAULT_MIPLAY_CONTROL_PORT = 8899`（:16）——sidecar 在本机绑定 8899，扮演接收端；**端口被占用会让 sidecar 崩溃**，Python 侧专门告警“请检查 8899 端口占用或系统 mDNS 服务”（:244-249）。
- 持久身份 `miplay_identity.json`（:23-56）：`idm_instance_id`（UUID）、`lyra_instance_id`（8 hex 大写）、`media_device_id`（UUID）。注释明确：身份必须**单机终身稳定**，否则“HyperOS 手机端缓存的设备路由失效（妙播入口时有时无、出现同名死条目）”。
- 启动命令（:172-181）：
  `miplay-bridge --name <名> --expected-ip <本机LAN IP> --pcm-port <本机临时端口> --pcm-host 127.0.0.1 --idm-instance-id <UUID> --lyra-instance-id <8HEX> --media-device-id <UUID>`

**IPC 帧格式（已验证：源码逐行）**
- sidecar → Python：**stdout 上的按行 JSON**（`await self._proc.stdout.readline()` → `json.loads(line_str)`，:229-239），事件名取 `evt["event"]` 或 `evt["type"]`（:269）。
- Python → sidecar：**stdin 上的按行 JSON**（:363-370, :381-384）：
  - `{"command":"resume|pause|toggle|previous|next|seek","position_ms":N?}`
  - `{"command":"volume","percent":N}`
- 音频：sidecar **主动反连** Python 在 `127.0.0.1:0` 上监听的临时 TCP 端口（`asyncio.start_server(self._handle_pcm_client, "127.0.0.1", 0)`，:162-167），持续写**原始 PCM**；Python 以 4096 字节为单位读取（:200-215）→ `on_audio_frame(chunk)`。PCM 连接关闭即视为播放结束（`on_state_change("stopped")`）。
  ⇒ **MiXPlay 是接收端（被投送方），不是发送端**；任务书判断正确。

**事件名（源码解析 + exe 字符串）**
`metadata` / `media_info`（title, artist, album, artwork|cover|cover_url, duration_ms, position_ms, lyrics）、`lyrics`、`progress`、`playback_state{playing}`、`stream_started`、`media_channel_closed`、`lyra_transport_disconnected`、`volume{percent|volume}`、`bridge_ready{ip,name}`、`error{message,code}`、**`discovery_identity_ready{id_hash, mi_connect_device_type, account_required}`**、`discovery_*`（高频，仅 DEBUG）。
exe 侧另有：`transport_connected`、`secure_channel_established`、`identity_exchanged`、`capabilities_exchanged`、`media_session_established`、`transport_disconnected`、`discovery_interface_inventory`、`lyra_secure_channel_established`、`lyra_logical_connection_accepted`、`lyra_channel_frame_decrypted`、`lyra_transport_frame_trace`、`lyra_physical_sync`、`lyra_packet_received`、`miplay_lyra_frame_failed` 等。

**状态机**
`idle` → `playing`（`stream_started` 或 `playback_state.playing=true`）→ `paused`（`playback_state.playing=false`）→ `stopped`（`media_channel_closed` / `lyra_transport_disconnected` / PCM socket EOF）。

**`--expected-ip` 语义**
- 由 `MiPlayBridgeManager.__init__(host=...)` 传入，值即**本机 LAN IP**（:136, :175），也就是 sidecar 启动参数里的“我方地址”。
- 结合 exe 字符串 `shared_source_address`、`same_control_session`、`media_key_selected`、`RTSP auth key`、以及 `bridge_ready` 回显 `ip`：最合理的读法是——**它是本端对外宣告/绑定的 LAN IP**，用于（a）mDNS/Lyra/IDM 实例宣告与（b）对入站媒体会话做源地址一致性校验（与握手 key 的四元组推导同源）。
- **判定：语义 1（宣告地址）为已验证（调用点明确）；语义 2（源地址过滤/一致性校验）为推测**，依据是字符串共现，未做行为实验。它**不是**对端（手机）IP。

**与主控通道的关系**
MiXPlay 的 Python 播放路径走的是**小米云**（`play_url` → `player_play_music`），sidecar 只负责“让 PC 变成一台妙播音箱”（接收方向）。所以 MiXPlay **不能**提供第三方发送端能力。

### 8.2 `bin\miplay-bridge.exe` 只读 strings 关键偏移

sha256 `88c5b852d49dbbfeb8f70b57b92ef518054fb8e8cbb7a91ccc744272e67b4a13`（1 716 736 B）；全量 16 277 条落在 `mixplay_miplay_bridge.exe.strings.txt`。下表偏移为十进制/十六进制文件偏移：

| 偏移 | 字符串 |
|---|---|
| `0x152e01` | `_lyra-mdns._udp.local` |
| `0x152e16` | `_mi-connect._udp.local` |
| `0x1540e0` | `P-256/HKDF-SHA256/AES-256-GCM` |
| `0x15457b` | `com.milink.service:smartplay` |
| `0x1549b9` | `wfd_audio_codecs_v2: 15 3 3` |
| `0x154a5a` | `wfd_client_rtp_ports: RTP/AVP/TCP;interleaved mode=play` |
| `0x154ad9` | `wfd_content_SP_protection: 4 1 256 3 1 1 0 0` |
| `0x153a48` | `RTSP auth key` |
| `0x153b78` | `decrypt media AES-CBC prefix` |
| `0x155552` | `tv_unprotected_stream` |
| `0x15553d` | `shared_source_address` |
| `0x14c3fe` | `--expected-ip` |
| `0x152a2e` | `crates\miplay-bridge\src\lyra.rs` |
| `0x1529fc` | `Xiaomi Lyra command socket` |
| `0x155996` | `wfd://` |
| `0x1531c0` | `transport_connected secure_channel_established identity_exchanged capabilities_exchanged media_session_established transport_disconnected discovery_interface_inventory` |

另有：`--name --expected-ip --interface-name --idm-instance-id --lyra-instance-id --media-device-id --pcm-port --pcm-host`、`discovery_peer_query`、`RTSP auth key`、`media AES key`、`media AES IV`、`missing_session_key`、`account_hash_hex`、`external_account_required`。
注意：strings 中**没有** `AES-GCM` 之外的对称算法名，**没有 RC4**，**没有 miio 54321**，也**没有**服务端推送/`play_url` 相关串——与“它是接收端模拟器”一致。

---

## 9. 问题 3：第三方能不能当发送端在 LAN 内直推？——**不可行**

### 9.1 判定

**不可行（对“经 8899 控制面 + 反向 RTSP 直推”这条路径是已证否）。**
架构上不要把 LAN 直推当主路径；把云端 `play_url` + 本机 HTTP 流作为事实上的方案（§10），并把延迟优化做在“我们自己的 HTTP 服务端 + 播放触发次数”上。

### 9.2 卡点（按证据强度排序）

1. **账号绑定的信任层挡在媒体会话之前**（主因）
   - 实机：传输握手全通过后，**所有**控制指令静默；媒体类指令（SET_DEVICE_INFO/OPEN）导致**连接被断开**——这是“判定为未授权会话”的行为，而非“不支持该指令”。
   - FusionPlay 原话（protocol.rs:106-112）：传输连接与 SafetyAuth **刻意不被视为可播放路由**，小米把“设备信任与能力协商”放在发现与媒体会话之间。
   - 阶段阶梯只走到第 2/5 级（`secure_channel_established`），卡在 `identity_exchanged / capabilities_exchanged / media_session_established`。
2. **该信任层就是 Lyra / Mi Connect v3，且明确带账号语义**
   - MiXPlay 字符串：`P-256/HKDF-SHA256/AES-256-GCM`、`com.milink.service:smartplay`、`account_hash_hex`、`external_account_required`、`account_required`、`vendor_service_required`、`missing Lyra auth frame`。
   - `miplay_lan_removed` + `query_scoped_lyra_and_mi_connect_without_legacy_lan_tv`：小米在新版里**移除了旧局域网直连**，第三方“裸 8899”路径正是被移除的那条。
3. **媒体密钥来自账号侧的密钥交换**
   - `SET_MIRROR_KEY` 的 `authKey/streamKey/streamIV` 由**发送端（手机/PC）**下发（protocol.rs:1393-1417）；FusionPlay 注释指出电视接收端**不参与**“Xiaomi-account PC key exchange”，所以才走 `tv_unprotected_stream` 明文流。
   - 反面：**音箱不是电视**。电视的明文回退路径（protocol.rs:1482-1489）**不适用于** L16A/L16B——实机测试中，即便我们完全不提密钥、或提交密钥，音箱都在 OPEN 处不理会/断开。
4. **音箱不接受“陌生发送端”这一角色**：它接受 TCP + SafetyAuth（因为那是无状态兼容层），但从不把该会话升级为可操作系统会话；对照实验（不同的“认证后第一条指令”）排除了顺序问题。

### 9.3 存疑之处（诚实标注，避免过度断言）

- 我们**没有**完成 Lyra 握手（P-256 ECDH + account hash + AES-256-GCM 逻辑帧），因此**不能证明数学上不可能**：也不能排除“用用户自己的账号身份在 PC 上重建 Lyra 后，音箱愿意接受”的可能。
- 但（a）Lyra 是 KCP + protobuf + 账号哈希 + 厂商服务背书的完整栈，（b）即便打通，音箱是否把“PC 型发送端”纳入允许清单仍是未知，（c）MiPlay 协议自身的接收缓冲（650–1500 ms）决定它**本来就不是低延迟协议**——所以即使成功，收益也远小于成本。**投入产出比不支持继续深挖。**
- 能推翻本结论的最小实验：用一部 HyperOS 手机做真机抓包（交换机镜像口 / PC 上 `Wireshark` 监听 8899+mDNS+Lyra），对比“手机→音箱”在 8899 认证之后**还发了什么**（尤其是 Lyra 通道与 `SET_MIRROR_KEY` 的密钥来源）。这是唯一能把“不可行”升级为“已证否”或翻案的实验；本机没有手机端抓包条件，故留作未决项。

---

## 10. 问题 4：次优低延迟方案（不经小米云 / 降低音箱端缓冲）

### 10.1 不经小米云直接命令音箱拉 URL ——**当前无可用本地通道**

| 候选 | 实测 |
|---|---|
| miio TCP 54321 | 关闭 |
| **miio UDP 54321** | **hello 应答 ✅，但加密 RPC 无应答 ❌**（token 被拒/需专用凭据） |
| 80 / 443 / 8080（HTTP/ubus over HTTP） | 关闭 |
| mDNS 宣告的 56666 | 只谈 Lyra/Mi-Connect（账号绑定，同 §6） |
| 8899 | 妙播控制口，未授权会话下静默（§7.2） |
| `mijiia` / `miot` LAN | 未发现任何宣告或开放端口 |

⇒ 结论：**「绕过 api2.mina.mi.com 直接命令音箱」目前只有 miio 这条路有希望，但加密层未打通**。可行下一步（未做）：用 `python-miio` 官方库对 L16B/L16A 直接 `miIO.info` 复核（排除我自研 miio v2 帧细节问题），以及尝试 MiNA/`mico` 本地协议。**注意**：小米音箱的 miio token 在部分机型上是“云 token”，本地 RPC 需要设备端未轮换的 token；`external_account_required` 风格的门禁同样可能存在。

### 10.2 音箱拉本机 HTTP 时的缓冲策略 ——**未测到，但有测量工具与优化清单**

原计划：用 `probe_speaker_pull.py`（本机 HTTP 服务端 + 通过用户既有 `play_url` 触发，播 ≤6 s 440 Hz 低音量正弦）记录音箱 HTTP 客户端的**请求头、是否 Range、读取速率、是否一次性读完整个文件**，从而直接量出「音箱缓冲多少秒」。
**未能执行的原因**：探测期间 MiXPlay 的 Web（`http://127.0.0.1:8820` / `192.168.1.4:8820`）已不在监听，无法通过既有 API 触发播放；我可用的出声链路因此暂时断开。脚本已就绪，MiXPlay 恢复后即可跑（`probe_speaker_pull.py <speaker_ip> <did> [length|chunked]`）。

在此给出**基于协议事实的优化清单**（待上述测量确认，逐条标注依据）：

1. **降低播放触发次数**：`play_url` 走云（`api2.mina.mi.com`）+ ubus `player_play_music`（`play_behavior=REPLACE_ALL`，必填 `audio_id`）——每次触发都是一次云端往返，**每次换曲/续播只触发一次**，把整个会话的音频用一条长连接推完，是当前最大且最稳的收益（依据：既有实测的可用链路）。
2. **用 chunked（无 Content-Length）让音箱边下边播**：`Transfer-Encoding: chunked` + `Content-Type: audio/wav`。若音箱尊重 HTTP/1.1 流式语义，它会按到达速率消费而不等“文件长度已知”；`length` 优先，`chunked` 作为 A/B（依据：HTTP 语义 + 我们对音箱拉流行为**尚无实测**）。
3. **WAV 头 + 持续 data**：先发 44 字节标准头（`RIFF/WAVE/fmt/data`），随后持续写 PCM；但注意 44 字节头里的 `data` 长度字段若为 `0xFFFFFFFF` 或按真实总长填，行为可能不同——三种变体（真实长度 / 0 / 0xFFFFFFFF）值得 A/B（依据：WAV 头字段语义；**未实测**）。
4. **采样格式**：MiXPlay 侧路径固定 **44100 Hz / 16 bit / 立体声**（依据：本机既有播放链路的实现约定）——避开 48 kHz 重采样，也避免发送端做重采样引入的额外时延。
5. **发送节拍**：服务端按 1× 实时速率推送，仅保留极小的起播提前量（几十到一百毫秒级），**不要**预推整个文件（预推会诱发音箱端大缓冲）。`probe_speaker_pull.py` 的两个模式正是用来验证这一点的。
6. **避免 `icy-metaint`**：这是 Icecast/SHOUTcast 元数据插入头，小米音箱是否支持未知，且引入元数据分片解析风险；在没有实测支持前不要用。
7. **可选：小分片 + 固定 Content-Length 分段**：把长流切成若干“已知小长度”的分段，逐段请求（音箱会重新发起 HTTP 请求）——能否降低缓冲取决于音箱的缓冲实现，属于实验项。

> 端到端延迟预算（粗估，依据：既有链路观察）：云触发往返 + 音箱起播判定（可能 0.5–1.5 s，量级参照 MiPlay 自身的 650–1500 ms 抖动缓冲设计）+ 本机流推送的起播提前量。**要压缩的主要是前两项**；第三项由我们把控。

### 10.3 其它可借的小米本地协议

- `mijiia` / `miot` 局域网直连：**未发现任何宣告/开放端口**（mDNS 查询 `_mijiia._tcp` 无应答，端口探测无结果）。
- Mi Connect（56666 / Lyra 55982）：协议存在，但**同属账号绑定的信任层**，不是“绕过账号”的捷径。
- MiNA（`api2.mina.mi.com`）：**就是云端**，不满足“不经云”的目标，但它是**当前唯一已验证可用**的控制路径。

---

## 11. 问题 5：立体声组合（L16A + L16B）怎么寻址？

**已知**：左 L16A did=501718792（192.168.1.21，MiNA `f58c6453-…`）+ 右 L16B did=934947888（192.168.1.171，MiNA `0c9041b3-…`）在妙播 UI 里成对为「卧室立体声组合」；云 `device_list` 中**没有 group 对象**，只有 `stereo_mode_v2` 能力位（此前侦察结论）。

**参考实现怎么寻址成对设备**：**都不支持**。
- FusionPlay（接收端）：`MiPlayAdvertisement` / `lyra_instance(device_id, local_ip)` / `legacy_mi_connect_instance` 全部以**单设备**为单位；SDK 里没有任何 group/stereo/room 抽象，Lyra 也是**每 peer 一条**控制信道。
- MiXPlay：一个 sidecar 进程 = **一台虚拟音箱**（`--name/--idm-instance-id/--lyra-instance-id/--media-device-id` 单身份），也没有组合概念。
- 唯一在线上抓到的“组”语义：`SET_PLAY_SOURCE` 的 body `{"ref_channel":"controlcenter","ref_function":"single_room","ref_content":"music_qq"}`——存在 `ref_function` 字段且取值为 `single_room`，说明协议里**有“房间/组”概念**，但抓到的样本是单房间（推测：手机在妙播里对成对音箱可能以“一个房间”下发，第三方 API 不暴露该对象）。

**协议侧最有力的证据**：接收端常量里有一个专门为“组合播放”准备的延迟——`MIPLAY_GROUP_PLAY_DELAY_MILLIS = 800`（media.rs:1934 区域），以及 `MIPLAY_MINIMUM_QUEUE_DELAY_MILLIS = 650`、时钟同步与漂移校正机制。这说明**“组合播放”是靠发送端分别驱动每个成员 + 各成员用共享时钟/固定 800 ms 组延迟对齐**实现的，而不是靠一个“组对象”接收一条指令。

**建议（结论）**：**发两条指令，每个 did 一条**，同一 URL、`play_behavior=REPLACE_ALL`、`audio_id` 各自唯一；若两只箱子起播不同步，再考虑“先都准备、后同时触发”，或用 `SET_POSITION`(0x56) / `wfd_timeoffset` 对齐（后者仅在直连媒体会话里可用，而该会话当前不可达）。
**标注**：以上为**推测 + 间接证据**（云侧无 group 对象是已验证事实；两条指令是我们已验证可用的发送方式）；**待验证**：两只音箱同时播放时的实际对齐误差（可听测试，或读回 `player_get_play_status` 的播放位置比对）。

---

## 12. 未决问题与建议的下一步

| # | 未决项 | 建议动作 | 代价 |
|---|---|---|---|
| 1 | 手机→音箱真机抓包（唯一能翻案的实验） | 交换机镜像/手机侧抓包，看 8899 认证后是否建立 Lyra 及密钥来源 | 中（需设备） |
| 2 | 音箱拉流缓冲到底多少秒 | MiXPlay 恢复后跑 `probe_speaker_pull.py`（先 length 后 chunked） | 低（10 分钟） |
| 3 | miio 本地加密 RPC 为何无应答 | 用 `python-miio` 官方库复核 token/帧；再试 `mico`/MiNA 本地协议 | 低-中 |
| 4 | 立体声组合同步误差 | 两条 `play_url` 同 URL 播放并比对状态 | 低 |
| 5 | FusionPlay 若采用，AGPL-3.0 传染 | 产品决策：AGPL 发布 or 只借鉴协议事实 | — |

---

## 附录 A：脚本清单与复现命令

```powershell
$py = 'C:\Users\Administrator\AppData\Roaming\uv\python\cpython-3.12.11-windows-x86_64-none\python.exe'
$d  = 'D:\Backup\Documents\deepseek-harness\default-workspace\miplay-bridge\research\miplay'

& $py "$d\probe_miplay_full_sender.py" 192.168.1.171   # 决定性实验：完整 HyperOS 发送端序列
& $py "$d\probe_miplay_env.py"                          # mDNS dump + 端口探测
& $py "$d\probe_miio_udp.py" 192.168.1.171 192.168.1.21 # miio hello
& $py "$d\probe_miio_local.py" --info --call player_get_play_status
& $py "$d\probe_speaker_pull.py" 192.168.1.171 934947888 length   # 待 MiXPlay 恢复
```

## 附录 B：关键常量速查

| 常量 | 值 | 来源 |
|---|---|---|
| 控制端口 | TCP 8899 | protocol.rs:22 |
| 协议版本 | `2.1.5071614` | protocol.rs:23 |
| 帧头 | `24 <outer> <cmd> <seq u16be> <len u32be>` | protocol.rs:1842 |
| 加密 | AES-128-CBC / 零填充 / 链式 IV / 自定义 CRC-32 | protocol.rs:1870-1958 |
| auth key | `md5(digit+0x31 拼接的四元组)` 的 32 hex，取前 16 ASCII 作 AES key | protocol.rs:1991-2010 |
| 媒体 | 反向 RTSP + RTP/MP2T/TCP + MPEG-TS PID 0x1100 + AAC-LC ADTS 48 kHz/2ch/16bit | media.rs:917-1660 |
| 媒体加密 | 每 PES 前 `min(len,256)/16*16` 字节 AES-128-CBC，IV 取 PES private data | media.rs:1507-1604 |
| 接收端缓冲 | 650 / 800 / 1500 ms；输出延迟上限 250 ms | media.rs:1934-1974 |
| Lyra | P-256 + HKDF-SHA256 + AES-256-GCM，KCP，`com.milink.service:smartplay`，账号绑定 | lyra.rs + exe 字符串 |
| miplay-bridge.exe | sha256 `88c5b852…b4a13` | 本报告 §1.2 |

## 附录 C：一句话交接

**不要把 LAN 直推排进架构**：传输层已破（第三方可完成 MiPlay 兼容互认证，两台真机验证），但媒体会话被**账号绑定的 Lyra/Mi Connect v3 信任层**挡死，实机上表现为「认证成功 → 所有指令静默 → 媒体指令断开连接」；音箱端不暴露可用的本地控制通道（miio 加密 RPC 无应答）。可行方案 = 云端 `play_url` 触发一次 + 本机 HTTP 流持续推送，延迟优化集中在**减少云触发次数**与**音箱端缓冲行为实测**（`probe_speaker_pull.py` 待跑）。
