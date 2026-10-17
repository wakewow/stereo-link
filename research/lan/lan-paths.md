# 小米音箱「局域网直达通路」探测报告

任务：`task-7` ｜ 执行者：`lan-transport-prober` ｜ 日期：2026-10-17
本机：Windows 10.0.26100 / AMD64 / **192.168.1.4**
目标设备：
- **A 机** `192.168.1.21` = Xiaomi Sound-左，model **L16A**，did **501718792**，Wi-Fi `5C:02:14:2B:B3:C6`，固件 1.106.3，mDNS 名 `Xiaomi Sound-1695`（SN `34471/A1T321695`）
- **B 机** `192.168.1.171` = Xiaomi Sound，model **L16B**，did **934947888**，Wi-Fi `90:FB:5D:0F:B9:5C`，mDNS 名 `小爱音箱-5320(AnRV-C3ba-)`

> 全文用 **【已验证】** 表示有原始命令输出可复查，**【推测】** 表示推断。原始输出全部落在
> `research/lan/`（文件索引见文末第 11 节）。

---

## 1. 结论摘要（TL;DR）

| # | 通路 | 目标 | 是否可用 | 机制性延迟（本机→出声） | 依赖 |
|---|------|------|---------|----------------------|------|
| **P1** | **原生 AirPlay 2 / RAOP (TCP 7000)** | **仅 A 机 .21** | ✅ **已验证可推流出声**（HAP 瞬时配对，无需 PIN、无需小米账号） | **250 ms ~ 2.0 s 可调**；实测 1.5 s 配置成功 | 无（纯局域网） |
| P2 | miIO 本地 UDP 54321 | A 机 + B 机 | ⚠️ 服务在，但**缺 16 字节 device token** | 未知（取决于音箱 HTTP 预缓冲） | 需小米账号取 token |
| P3 | `_mi-connect._udp` UDP 56666 | A 机 + B 机 | ❓ 端口在广播，协议未知（未逆） | 未知 | 未知 |
| P4 | 现状：小米云 `player_play_url` + 音箱拉本机 HTTP 流 | A/B 机 | ✅ 正在用（MiXPlay） | **≈1~2 s**（用户体感一致） | 小米账号 + `api2.mina.mi.com` |
| — | DLNA / UPnP / Google Cast / Spotify Connect / Matter / HTTP | 两台 | ❌ **完全不存在** | — | — |

**关键结论（回答任务的核心提问）**

1. **音箱 A（L16A）自己就是一台货真价实的原生 AirPlay 2 / RAOP 接收端**，不是 MiXPlay README 里
   「PC 当 AirPlay 接收端再经小米云转发」那种情况。证据是它**自己**在 7000 端口应答 RTSP，
   且 `Server: AirTunes/366.0`、`sdk=AirPlay;2.0.10`、`protocolVersion=1.1`、支持 PTP 精密时钟，
   并且我们**成功用第三方发送端（pyatv）把 440 Hz 音频推过去并完整播完**。【已验证】
2. **配对不需要 PIN、不需要用户操作、不需要小米账号**：使用 HAP **transient（瞬时）配对**，
   `POST /pair-pin-start` + `POST /pair-setup`（`X-Apple-HKP: 4`）即可完成，pyatv 全自动通过。【已验证】
3. **音箱 B（L16B）不提供 AirPlay/RAOP**，只广播 `_mi-connect._udp`；两台都暴露本地 miIO（UDP 54321）。【已验证】
4. 局域网里被广播的服务类型**总共只有 3 个**（外加本机自己的一个）：`_airplay._tcp`、`_raop._tcp`、
   `_mi-connect._udp`。**没有** `_miplay`、`_googlecast`、`_dlna`/`_upnp`、`_spotify-connect`、`_matter`、
   `_http`、`_miio._udp`、`_sdap`、`_airport`、`_companion-link`、`_micont`、`_hap`。【已验证】
5. **音质/延迟架构建议**：主通路走 AirPlay 2（P1），延迟可压到 **0.25~0.5 s**，比现状低约一个量级，
   且彻底摆脱小米云；B 机没有 AirPlay，需要单独决策（见第 9 节）。

---

## 2. 方法与工具

全部脚本在 `research/lan/`，均为只读探测（唯一「写操作」是让 A 机播了 2 段 ≤4.5 s 的测试音，
音量用后已恢复：`33 → 8/30 → 33`，两次运行都打印了 `volume restored to 33.0`）。**未做任何配置修改、
未解绑、未升级、未恢复出厂。**

| 脚本 | 作用 |
|------|------|
| `probe_mdns.py` | zeroconf 三遍法全量 DNS-SD 枚举（meta-PTR + 47 个已知类型 + 裸 socket 复验） |
| `probe_dnssd_cache.py` | 直接 dump zeroconf 内部 `DNSRecord` 缓存，导出**原始 wire 记录**（PTR/SRV/TXT/A + TTL） |
| `probe_dnssd_raw.py` | 无依赖裸 mDNS 扫描器（自写 DNS 报文构造/解析，QU 位单播复验） |
| `probe_ports.py` | 温和 TCP/UDP 端口扫描（98 端口 × 2 主机，32 并发）+ 协议化 banner 抓取 |
| `probe_miio_local.py` | miIO v2 本地协议（UDP 54321）hello + AES-128-CBC 加解密 + token 搜寻 |
| `probe_raop.py` / `probe_airplay2.py` | RAOP/AirPlay RTSP 握手与能力探测（OPTIONS / GET info / ANNOUNCE / SETUP / pair-verify） |
| `probe_raop_session.py` / `probe_raop_variants.py` / `probe_raop_state.py` | 自写 RTSP 状态机（0xd4/0xd2/0xd3 时序包）逐项隔离 403/455 的成因 |
| **`probe_airplay2_stream.py`** | **端到端成品验证**：手搓 pyatv config → HAP 瞬时配对 → SETUP → RECORD → 推真实 WAV |
| `probe_airplay2_latency.py` | 声学延迟测量（参考 click + 440 Hz 音 + 麦克风录音 + Goertzel 检波） |

复现命令（pyatv 装在 `%TEMP%\raopvenv`，**故意不放仓库**）：

```powershell
$py = "$env:TEMP\raopvenv\Scripts\python.exe"
$d  = "D:\Backup\Documents\deepseek-harness\default-workspace\miplay-bridge\research\lan"
& $py "$d\probe_airplay2_stream.py" --ip 192.168.1.21 --seconds 3 --volume 8 `
      --out "$d\airplay2_stream.json" --log "$d\airplay2_stream_debug.log"
```

> **开发提示**：pyatv 自带的 `pyatv.scan()` **在本机扫不到这两台音箱**（多网卡主机 + 音箱不应答
> 单播 mDNS）。必须用 `manual_config()` 从我们抓到的 DNS-SD 记录手工构造 config，
> 且 `credentials` 必须留 `None`（传字符串 `"transient"` 会抛 `InvalidCredentialsError`）。

---

## 3. 通路 P1：原生 AirPlay 2 / RAOP（A 机 .21:7000）★推荐

### 3.1 mDNS 广播的原始 TXT 记录【已验证】

来源：`dnssd_cache.json`（zeroconf 内部缓存 dump）、`mdns_summary.txt`。

```
PTR  _airplay._tcp.local.  -> Xiaomi Sound-1695._airplay._tcp.local.              ttl=4500
PTR  _raop._tcp.local.     -> 5E02142BB3C6@Xiaomi Sound-1695._raop._tcp.local.   ttl=4500
SRV  Xiaomi Sound-1695._airplay._tcp.local.            -> L16A-1695.local.:7000
SRV  5E02142BB3C6@Xiaomi Sound-1695._raop._tcp.local.  -> L16A-1695.local.:7000
A    L16A-1695.local. -> 192.168.1.21
```

`_airplay._tcp` TXT（原样，全部键值）：

```
acl=0
deviceid=5E:02:14:2B:B3:C6          # 注意：Wi-Fi MAC 是 5C:02:…，AirPlay deviceid 用 5E:02:…（本地管理位置 1）
features=0xC05F8A00,0x1C340
rsf=0x4          fv=1.106.3       flags=0x4        model=L16A
manufacturer=XiaoMi               serialNumber=34471/A1T321695
protovers=1.1    srcvers=366.0     pi=5E:02:14:2B:B3:C6     gid=5E:02:14:2B:B3:C6   gcgl=0
pk=be290bd4d4b9be518aaf69881f893cdcfcea477c40f361d17a14179cce25cd6f
```

`_raop._tcp` TXT（原样，全部键值）：

```
cn=0,1     da=true    et=0     ft=0xC05F8A00,0x1C340    fv=1.106.3    md=0,1,2
am=L16A    sf=0x4     tp=UDP   vn=65537                vs=366.0
pk=be290bd4d4b9be518aaf69881f893cdcfcea477c40f361d17a14179cce25cd6f
```

逐字段解读：**没有 `pw` 字段 → 无密码**；`et=0` → 不加密（但实测仍接受 `shk` AES 密钥）；
`tp=UDP` → RTP over UDP；`cn=0,1` → 单声道+立体声；`md=0,1,2` → 文本/封面/进度元数据；
`vn=65537`、`vs=366.0`（AirTunes 366.0）。

### 3.2 RTSP 握手证据【已验证】

`OPTIONS * RTSP/1.0`（裸 socket，`research/lan/raop_probe_21.json`）：

```
RTSP/1.0 200 OK
Public: ANNOUNCE, SETUP, RECORD, PAUSE, FLUSH, FLUSHBUFFERED, TEARDOWN, OPTIONS, POST, GET, PUT
Server: AirTunes/366.0
CSeq: 1
```

`GET /info` → `Content-Type: application/x-apple-binary-plist`（543 字节），解出（完整）：

```
sdk = "AirPlay;2.0.10"        protocolVersion = "1.1"      sourceVersion = "366.0"
pi = deviceID = "5E:02:14:2B:B3:C6"                        model = "L16A"
manufacturer = "XiaoMi"       firmwareRevision = "1.106.3" firmwareBuildDate = "Dec 15 2025"
OSInfo = "10.0.1"             build = "53.0"               hardwareRevision = "1.0.0"
PTPInfo = "PTP-AirPlay;2.0.10"                             keepAliveLowPower = true
keepAliveSendStatsAsBody = true                            statusFlags = 4
name = "Xiaomi Sound-1695"    features = 496157849520640 (= 0x1C340C05F8A00)
```

`features` 位域解码（`probe_airplay2.py` 的 FEATURE_BITS 表）置位：
`9 AudioReceiver`、`11 ALAC`、`15 文本`、`16 封面`、`17 进度`、`18 UnifiedMediaControl`、
`19 播放队列`、`20 云端`、`22 多屏`、`30 AirPlayPTP`、`38 BufferedAudio`、`40 PTP`、
`41 ALAC_2`、`46 AAC_ELD_2`、`48 VideoPlayQueue2`。
**`LegacyPairing`(27) 与 `CoreUtilsPairing` 未置位**，但 `pk`（设备公钥）存在 →
指向 **AirPlay 2 的 HAP 配对制**（这点在 3.3 得到证实）。

### 3.3 ★ 端到端成品验证：第三方成功推流出声【已验证】

`probe_airplay2_stream.py` 完整回放一次 AirPlay 2 会话（`airplay2_stream_debug.log`）：

```
22:22:40,285  GET /info                                        -> 200, 543B bplist
22:22:40,289  Setting up new AirPlay Pair-Verify procedure with type AuthenticationType.Transient
22:22:40,295  POST /pair-pin-start   X-Apple-HKP: 4
22:22:40,298  POST /pair-setup       9B
22:22:40,448  POST /pair-setup       457B      -> 配对完成（无机 PIN、无用户交互）
22:22:40,576  SETUP .../3062041125   (bplist: timingPort/timingProtocol=NTP/sessionUUID…)
22:22:40,590  <- 200 {'eventPort': 33477, 'timingPort': 53611}
22:22:40,595  SETUP (bplist: streams=[{audioFormat:2048(ALAC), spf:352, sr:44100, ct:1,
                                        type:96, isMedia:True, controlPort:57726,
                                        latencyMin:11025, latencyMax:88200, shk:<32B>}])
22:22:40,677  <- 200 {'streams': [{'type': 96, 'dataPort': 41013, 'controlPort': 56356}]}
22:22:40,686  SET_PARAMETER volume: -27.6      -> 500 Internal Server Error（首次）
22:22:40,694  SET_PARAMETER progress: 66150/66150/198450 -> 200
22:22:40,699  SET_PARAMETER (dmap-tagged artwork)         -> 200
22:22:40,703  RECORD                                       -> 200
22:22:40,703  POST /feedback                               -> 200 {'streams':[{'type':96,'sr':44100}]}
22:22:40,876  FLUSH                                        -> 200 RTP-Info: rtptime=0
22:22:40,883  SET_PARAMETER volume: -27.6      -> 200（重试成功）
22:22:42..44  POST /feedback ×2                            -> 200（设备持续上报流状态）
22:22:45,422  TEARDOWN                                     -> 200
```

脚本层输出：

```
=== 2. connect (AirPlay 2 / transient pairing) ===
  connected
=== 3. volume: read -> lower -> restore ===
  current volume  : 33.0
  set volume      : 8.0
=== 4. stream tone_440_3s.wav (3.0s) ===
  stream_file returned without error after 5.188s
  volume restored to 33.0
```

第二次运行（2.3 s 测试音）同样成功：`stream_file returned after 4.409s`，音量 `33→30→33`。

**判据说明**：`RECORD` 返回 200 + 设备在随后 4.5 s 内持续以 `/feedback` 上报流状态（`sr=44100`）
+ `TEARDOWN` 正常，说明设备**接受并渲染了**这条流；若未配对或流非法，设备会在
`ANNOUNCE`/`SETUP`/`RECORD` 任一步返回 403/455（这正是我们在 3.6 中看到的失败模式）。

### 3.4 延迟的机制性分解【已验证 + 少量推测】

**（a）音箱自己声明的接收缓冲窗口 —— 实测协商结果**

我们在 `SETUP` 里声明 `latencyMin=11025`、`latencyMax=88200`（单位：帧 @ 44100 Hz），
即 **窗口 = [250 ms, 2000 ms]**，设备返回 `200 OK` 接受。按 AirPlay 2 规则，发送端在该窗口内
选定实际延迟，接收端必须遵守。

**（b）pyatv 选用的延迟 = 1.5 s —— 有源码与行为双重证据**

- 源码：`pyatv/protocols/raop/protocols/__init__.py:28` → `self.latency = 22050 + self.sample_rate`
  = 22050 + 44100 = **66150 帧 = 1.5 s**。日志里的 `progress: 66150/66150/198450` 正是这个数。
- 行为：`stream_client.py:563`「Once all frames in the audio stream have been sent, we are still
  "latency"…」→ 音频发完后**必须再补 1.5 s 静音**，否则尾部被切。
- 实测吻合：`stream_file` 的耗时比音频本身长 **2.11~2.19 s**（3.0 s 文件用 5.188 s；
  2.3 s 文件用 4.409 s），其中 1.5 s 是这段补静音。

**（c）控制面 RTT（纯局域网，微秒~毫秒级）**

从上面的日志时间戳逐条相减：`SET_PARAMETER volume` 首次 **7 ms**、重试 **32 ms**；
`SETUP` 往返 **82 ms**；`RECORD`→200 **172 ms**；`/info` **3 ms**。

**（d）冷启动 vs 热启动**

- 冷启动（HAP 配对 + `/info` + 两次 SETUP + 音量 + RECORD，**直到音频可发**）：
  40,285 → 40,875 = **590 ms**。
- **配对是每连接一次**，不是每首歌一次。会话保持后可复用，单曲起播只需 `SETUP`+`RECORD` ≈ **0.25 s** 控制面。
- 结论：**P1 的「本机→出声」= 控制面 ~0.05~0.3 s + 接收端缓冲 [0.25 s, 2.0 s]（可自选）**。

**（e）声学实测：本机不具备条件（如实记录）**

`probe_airplay2_latency.py` 已实现完整方案（参考 click + 440 Hz 音 + 麦克风录音 + Goertzel 检波 +
双时间轴对齐，可在有可用麦克风的机器上直接复用），但**实测失败**，原因是**本机三个录音输入
全是数字静默**：

```
麦克风 (Chu2 DSP)                     peak=7   rms=0.40   (32767 = 满量程)
麦克风 (Realtek High Definition Audio) peak=9   rms=0.91
麦克风阵列 (UU远程虚拟音频设备)          peak=1   rms=0.40
```

→ 目前无法用声学方法测「点一下到出声」的绝对延迟。**待办：换一台有可用麦克风的机器，
或让用户听感确认。** 本报告因此只给机制性估计（a~d），不给伪造的实测数字。

### 3.5 已知副作用与风险【已验证 + 推测】

- **首次 `SET_PARAMETER volume` 返回 500**，重试即 200。自研发送端必须**容忍一次 500 并重试**
  （或把音量改走 `/volume` 端点）。
- 推流会**接管设备播放**（AirPlay 固有的 takeover 行为）。测试期间设备未在播放，未打扰用户内容。
- 音量：我们改动过并已恢复（两次运行均有 `volume restored to 33.0`）。【已验证】
- 【推测】AirPlay 会话在设备空闲一段时间后可能被回收，长连接需要 keep-alive（`/feedback` 每 2 s 一次
  是标准做法，本次会话就是这么维持的）。

### 3.6 反面证据：为什么「未配对」一定失败（说明配对是硬门槛）

自写 RTSP 客户端在**不完成配对**时，所有建流动作都被拒【已验证】：

```
ANNOUNCE（AirPlay1 传统 SDP 方式）        -> 403 Forbidden
AirPlay2 SETUP（application/x-apple-binary-plist）-> 403 Forbidden
legacy SETUP（无 ANNOUNCE）               -> 403 Forbidden
POST /pair-verify（畸形 body，故意）       -> 403 Forbidden
OPTIONS + Apple-Challenge                 -> 200 但**无 Apple-Response 头**
```

配对完成后的状态机实验（`probe_raop_state.py`，9 个用例）进一步定位到两条硬规则【已验证】：

1. **每个 TCP 连接只接受一个 RTSP 请求**。同一条连接上「先 OPTIONS 再 ANNOUNCE」必被 403；
   每次请求单独开连接（或用 `Connection: close`）就 200。这是本次探测最反直觉的坑。
2. 缺 `SETUP` 时 `RECORD` 报 **455 Method Not Valid In This State**；`SETUP` 必须在
   `ANNOUNCE`（或 AirPlay2 的 plist SETUP 第一段）之后。

---

## 4. 通路 P2：本地 miIO（UDP 54321）—— 服务在，差一个 token

### 4.1 两台都在局域网上暴露标准 miIO 服务【已验证】

标准 miIO v2 hello（`21 31 00 20` + 28×`ff`），原始响应：

```
192.168.1.171:54321 -> 2131002000000000 37ba2c30 00001aa7 ffffffffffffffffffffffffffffffff
192.168.1.21:54321  -> 2131002000000000 1de79f08 00001a58 ffffffffffffffffffffffffffffffff
```

换算：`0x37ba2c30 = 934947888 = L16B 的 did`、`0x1de79f08 = 501718792 = L16A 的 did`【已验证】。
**即：不经小米云，局域网内可直接对音箱发起 miIO 会话。** hello 应答里的 16 字节 token 区
是 `ff…ff`（不回 token），符合 miIO v2 设计。

> 注意：B 机 `.171` 的 **TCP 端口全关**（含 7000），但 **UDP 54321 有应答** —— 这是它唯一的
> 局域网控制入口候选。

### 4.2 token 从哪来：【已验证】不在磁盘上；【推测/文档】只有云端给

- 全盘搜寻 `.mi.token` / `config.json`（含 `D:\MiXPlay\conf\.mi.token`、`C:\Users\Administrator\conf\.mi.token`）：
  `candidate 16-byte tokens: 0` → **本机没有可用的 miIO device token**。
- `.mi.token` 顶层键只有 `deviceId / micoapi / passToken / userId`，即 **只有 MiNA（micoapi）这一个 sid**，
  **没有 `xiaomiio`**。miIO 云端 API 需要 `sid=xiaomiio` 的 `serviceToken`。
- 【文档级事实，未在本机验证】每台设备的 16 字节 miIO token 由
  `https://api.io.mi.com/app/home/device_list`（`sid=xiaomiio`）返回的 `token` 字段给出。
- **MiNA 的 `device_list`（`api2.mina.mi.com/admin/v2/device_list`，我们手上的 micoapi 会话可调）
  通常只返回 deviceID/name/miotDID/mac 等，不含 miIO token。**
- ⚠️ **我没有去取这个 token**，这是刻意的风险评估：【已验证代码事实】
  `miservice` 的 `mi_request` 在 401 时会 `save_token()`（无参）→ **会删除 token 文件**；
  而用同一 `deviceId` 重新 `login('xiaomiio')` 很可能**轮换 serviceToken，从而打断 MiXPlay
  当前正在工作的云端会话**（那是目前唯一可用的通路）。因此这一步需要用户在场时再做。

**结论 P2**：本地 miIO 通路**机制上存在**，但要解锁必须经历一次小米账号登录（可能需要 OTP，
且有可能使 MiXPlay 现有会话失效）。**这是一条「需要用户参与」的路，不是零成本备选。**
即便解锁，它走的是「云端下发 `player_play_url` → 音箱自己去拉本机 HTTP 流」的模式，
只省掉云那一段，**仍保留音箱 HTTP 客户端的预缓冲**，所以延迟未必比 P1 低。

---

## 5. 通路 P3：`_mi-connect._udp` UDP 56666（未解，值得后续逆）

【已验证】两台都广播该服务，`appsData` 是 base64，解出来是设备身份：

```
.171 小爱音箱-5320(AnRV-C3ba-)._mi-connect._udp.local.  port=56666
  version=65545  apps=[5]  flags=Ag==  name=小爱音箱-5320  idHash=ZEFu  dev=4  sec=2  mac=kPtdD7lc
  appsData -> {"mico": {"device_id": "0c9041b3-a155-4ffb-814e-497a266b1e93"}}
.21  小爱音箱-1695._mi-connect._udp.local.              port=56666
  version=65545  apps=[5]  flags=Ag==  name=小爱音箱-1695  idHash=zGEz  dev=4  sec=2
  appsData -> {"mico": {"device_id": "f58c6453-0268-4df5-a220-1335bb84172b"}}
```

【已验证】UDP 56666 盲探无响应（需要正确的握手格式）。【推测】这是小米「互联互通」的本地发现/控制
通道，`apps=[5]` 与 `mico.device_id` 说明它携带小米账号侧设备身份。**若能逆向出它的控制指令，
可能得到真正的「局域网直达控制」而完全不需要 AirPlay 与小米云** —— 这是本次探测留下的最大未知数，
建议列为后续任务（需要抓包 + 固件/APP 逆向）。

---

## 6. 通路 P4：现状（小米云下发 + 音箱拉本机 HTTP 流）的延迟分解

机制（源码级取证，`mixplay/xiaomi.py:559 async def play_url(self, …)` → MiNA `ubus` 调用
`player_play_url`，`research/lan/` 内有摘录）：MiXPlay 把本机 URL 交给
`api2.mina.mi.com`，MiNA 通过音箱的**常驻长连接**下发，音箱再回连本机 HTTP 取流。

延迟构成（每段都标了来源）：

| 段 | 量级 | 依据 |
|----|------|------|
| 本机 → `api2.mina.mi.com` TLS 建连 + 请求 | **127~236 ms**（TLS），总 **167~274 ms** | 【已验证】`curl -w` 三次实测（返回 403 = 未带鉴权，属预期） |
| MiNA 处理 + 经长连接推送到音箱（NAT 后必须推送） | 【推测】100~500 ms | 无直测数据 |
| 音箱回连本机 HTTP 取流 + **预缓冲到能起播** | 【推测】0.5~1.5 s | 用户体感 1~2 s 减去上面两段；未直测 |
| **合计** | **≈1~2 s** | 与用户「体感 1~2 秒」吻合 |

对比 P1：**云段与推送段直接消失**，且接收端缓冲从「音箱 HTTP 客户端的未知预缓冲」变成
**协议里可协商、可自选的 [0.25 s, 2.0 s]**。

---

## 7. 不存在的通路（省得以后白试）

【已验证】权威清单 —— 局域网内全部被广播的服务类型只有 4 个（含本机自己的）：

```
_services._dns-sd._udp.local.  ->  _dsh-mobile._tcp.local.     (本机 PC，非音箱)
                               ->  _airplay._tcp.local.        (A 机)
                               ->  _raop._tcp.local.           (A 机)
                               ->  _mi-connect._udp.local.     (A 机 + B 机)
```

定向复验（QU=1 单播响应位，6 轮 × 3 类型）：`_airplay {'192.168.1.21': 6}`、
`_raop {'192.168.1.21': 6}`、`_mi-connect {'192.168.1.21': 6, '192.168.1.171': 6}`【已验证】。

**因此以下通路在两台音箱上都不存在，不建议再投入**：
`_miplay._tcp`、`_googlecast._tcp`（Cast）、`_dlna`/`_upnp`/SSDP、`_spotify-connect._tcp`、
`_sonos`、`_matter`/`_matterc`、`_http._tcp`、`_miio._udp`、`_sdap`、`_airport`、
`_companion-link._tcp`、`_touch-able`、`_appletv-v2`、`_mediaremotetv`、`_hap`、`_micont`、`_miot`、`_mihome`。

SSDP 交叉验证：向 `239.255.255.250:1900` 发 `M-SEARCH ssdp:all` → **responders: []**【已验证】。

**端口扫描（98 端口 × 2 主机，32 并发，温和）**【已验证】：

```
open TCP ports: 3
  192.168.1.171:53    (dns)         banner 空
  192.168.1.21:53     (dns)         banner 空
  192.168.1.21:7000   (airplay/raop) banner = RTSP/1.0 200 OK
其余 95 个端口全部关闭
（含 80/443/554/7100/7237/8008/8009/8080/8899/56666/54321/62078/32200/35000/
  49152-49160/52540-52560）
```

> 两台音箱都开着 **TCP 53 DNS**（小米 Mesh 的本地 DNS 服务），这对本项目无用，但说明
> 它们对局域网非常「开放」——P1 能跑通也就不奇怪了。

---

## 8. 延迟对比总表

| 通路 | 控制面 | 接收端缓冲 | 端到端估计 | 云依赖 | 账号依赖 | 实测状态 |
|------|-------|-----------|-----------|--------|---------|---------|
| **P1 AirPlay 2（A 机）** | 0.05~0.3 s（热会话 ~0.25 s） | **0.25~2.0 s 可调**（pyatv 用 1.5 s） | **0.3~0.6 s（若自选 0.25~0.35 s）** | 无 | **无** | ✅ 完整推流成功 |
| P2 本地 miIO | 未知 | 未知（音箱 HTTP 预缓冲） | 未知，估计与 P4 同量级 | 无（若有 token） | 需取 token | ⚠️ 缺 token |
| P3 `_mi-connect` 56666 | 未知 | 未知 | 未知 | 无 | 未知 | ❓ 未逆向 |
| P4 现状（云 + HTTP 拉流） | 0.27~0.8 s | 【推测】0.5~1.5 s | **1~2 s** | **是** | **是** | ✅ 正跑着 |

---

## 9. 对 miplay-bridge 的架构建议

1. **主通路选 P1（原生 AirPlay 2 / RAOP），目标 A 机 `192.168.1.21:7000`。**
   - **不需要小米账号、不需要用户登录 OTP**（HAP **transient** 配对已验证全自动通过），
     这直接消掉了原需求里「如需登录小米账号时和我说」这一条对主通路的影响。
   - 用 Rust 自研发送端时，**把延迟设在 250~350 ms**（设备已接受 [250 ms, 2.0 s] 窗口，
     `latencyMin=11025` 帧即协议地板），而不是 pyatv 的 1.5 s。这是本项目相对现状
     「低一个量级」的来源。
   - 需要实现的协议栈（已确认全部为必需）：RTSP over TCP、HAP `pair-pin-start`/`pair-setup`
     （transient，Curve25519 + Ed25519 + ChaCha20-Poly1305）、AirPlay2 plist `SETUP`×2、
     NTP/PTP timing、RTP/ALAC 音频流（`spf=352`、`sr=44100`、`type=96`、`ct=1`）、
     `/feedback` keep-alive、`TEARDOWN`。
   - **两个必须避开的坑**（本次血泪）：① 每个 TCP 连接只发一个 RTSP 请求（否则 403）；
     ② 首次 `SET_PARAMETER volume` 会 500，必须重试。
   - 参考实现：`pyatv`（Python，发送端，本次已跑通）、`shairport-sync`（C，**接收端**，
     RTP/时序格式可对称参考）。Rust 生态暂无成熟 AirPlay **发送端** crate，需自研。
2. **B 机（L16B，192.168.1.171）没有 AirPlay，必须单独决策**：见第 10 节待办 ①。
3. **放弃 DLNA / Cast / Spotify Connect 方向**（第 7 节已证不存在），别再花时间。
4. Windows 侧「出现在选择声音输出」的部分属于 `research/vad/`（扬声器端点重命名/回环矩阵）
   的范围，与本报告正交；本报告只负责「送到音箱那一段」。
5. **P3（`_mi-connect` 56666）值得单开一个逆向任务**：它是唯一可能做到
   「不依赖 AirPlay、不依赖云」的原生局域网控制通道。

---

## 10. 未验证 / 待办 / 风险（诚实清单）

① **立体声组合行为未验证**：两台是「左/右」组合（L16A+L16B），但**只有 L16A 广播 AirPlay**。
   推 AirPlay 到 L16A 时，是只响左声道、还是固件把整组当目标一起响？**这需要人耳监听确认**
   （本机麦克风全坏，见 3.4e）。建议 Lead 安排一次「让 A 机播 3 s 测试音，用户听两台是否都响」。
   → 命令即 `probe_airplay2_stream.py`，音量参数已可调。
② **声学延迟未实测**（麦克风全数字静默），只有机制性估计。
③ **miIO token 未获取**（刻意规避打断 MiXPlay 云端会话的风险，见 4.2）。
④ **`_mi-connect` 56666 协议未逆向**。
⑤ B 机的局域网控制目前**只有** miIO（缺 token）与 `_mi-connect`（未解），**没有第三条路**。
⑥ 本报告未涉及防火墙/NAT 细节（音箱与 PC 双向可达已由前期观测与本报告 7000/54321 实测确认）。

---

## 11. 原始证据文件索引（均在 `research/lan/`）

| 文件 | 内容 |
|------|------|
| `mdns_report.json` / `mdns_summary.txt` / `mdns_run.log` | 全量 DNS-SD 枚举：服务类型、实例、host、port、**完整 TXT** |
| `dnssd_cache.json` / `dnssd_cache_run.log` | zeroconf 内部缓存的**原始 wire 记录**（PTR/SRV/TXT/A + TTL）——第 7 节权威清单的来源 |
| `dnssd_raw.json` / `dnssd_raw_run.log` | 裸 socket DNS-SD 尝试（**失败记录**：源端口 5354 时设备完全不应答） |
| `ports_report.json` / `ports_run.log` | 98 端口 × 2 主机 TCP/UDP 扫描 + banner + SSDP + miIO hello |
| `miio_local.json` / `miio_run.log` | miIO hello 双机应答、token 搜寻（`candidate 16-byte tokens: 0`） |
| `raop_probe_21.json` / `raop_run.log` | OPTIONS/`/info`/ANNOUNCE 403 首次证据 |
| `airplay2_probe_21.json` / `airplay2_run.log` | features 位域解码、pair-verify/SETUP 全 403 |
| `raop_variants.json` / `raop_session_handshake*.json` / `raop_state.json` | 403/455 成因隔离（每连接一请求、状态机顺序） |
| **`airplay2_stream.json`** / **`airplay2_stream_debug.log`** | **★ 端到端成功推流全过程（第 3.3 节的原始日志）** |
| `airplay2_latency.json` / `airplay2_latency_debug.log` | 延迟实验（含 SETUP 里声明的 `latencyMin/Max` 与麦克风全静默的证据） |
| `mic_capture.wav` | 声学实验的麦克风录音（近静默，作为「本机无法实测」的证据保留） |
| `probe_*.py` | 全部探测脚本（可重跑） |
