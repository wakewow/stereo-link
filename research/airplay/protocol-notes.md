# AirPlay 2 / RAOP 协议笔记（小米音箱 L16A 实测）

本文件记录 miplay-airplay 的**协议依据**：哪些是实测观测、哪些是读实现推断、哪些是我们踩过的坑。
每条事实都带来源（命令/响应/源码行）。所有推送都**只读协议**，不改设备配置、不配对落盘、不升级。

* 目标设备：`192.168.1.21`（Xiaomi Sound-左，型号 **L16A**，did 501718792，Wi-Fi `5C:02:14:2B:B3:C6`）
* 另一只：`192.168.1.171`（Xiaomi Sound，**L16B**，did 934947888）—— **不支持 AirPlay，见 §7**
* 实现：`crates/miplay-airplay`（后端 `cidermill-airplay 0.1.1`）
* 参照实现：pyatv 0.18.0（Python，同机实测成功）、shairport-sync（接收端，线路格式）、
  cidermill-airplay 源码（sender+receiver 对称实现）
* 参照记录：`research/airplay/reference_sequence.json`（pyatv 成功会话的 16 条 exchange，字节级）、
  `research/airplay/pairing_matrix.log`、`research/airplay/tone_run.log`

---

## 1. 设备自述（mDNS + `/info`）

### 1.1 mDNS 广播（只有 3 类，无 DLNA/Cast/Spotify/Matter）

`_raop._tcp.local.`（仅 .21）
```
cn=0,1  da=true  et=0  ft=0xC05F8A00,0x1C340  fv=1.106.3  md=0,1,2  am=L16A
sf=0x4  tp=UDP  vn=65537  vs=366.0
pk=be290bd4d4b9be518aaf69881f893cdcfcea477c40f361d17a14179cce25cd6f
```
* `cn=0,1` → 只收 LPCM(0) 与 ALAC(1)，**不收 AAC**（实测 ANNOUNCE AAC → `406`，与之一致）
* `et=0` → 不要求加密推送；**TXT 里没有 `pw`** → 无密码
* `pk` = 32 字节设备公钥（AirPlay 2 特征，供配对）

`_airplay._tcp.local.`（仅 .21）
```
acl=0  deviceid=5E:02:14:2B:B3:C6  features=0xC05F8A00,0x1C340  rsf=0x4  fv=1.106.3
flags=0x4  model=L16A  manufacturer=XiaoMi  serialNumber=34471/A1T321695
protovers=1.1  srcvers=366.0  pi=5E:02:14:2B:B3:C6  gid=5E:02:14:2B:B3:C6  gcgl=0
pk=<同上>
```
`_mi-connect._udp.local.`（两台都有，56666，小米自研本地协议，里面带 `apps=[5]` 与 device_id）

`/info`（`GET /info RTSP/1.0`，200，543 字节 `application/x-apple-binary-plist`）：
```
sdk = "AirPlay;2.0.10"        protocolVersion = "1.1"      sourceVersion = "366.0"
model = "L16A"                manufacturer = "XiaoMi"       name = "Xiaomi Sound-1695"
deviceID = "5E:02:14:2B:B3:C6"  pi = "5E:02:14:2B:B3:C6"
firmwareRevision = "1.106.3"  firmwareBuildDate = "Dec 15 2025"  build = "53.0"
hardwareRevision = "1.0.0"    OSInfo = "10.0.1"             statusFlags = 4
features = 496157849520640    PTPInfo = "PTP-AirPlay;2.0.10"  keepAliveLowPower = true
```
`Server: AirTunes/366.0`，`sdk=AirPlay;2.0.10` + PTP → **Apple MFi AirPlay 2 接收端实现**。

### 1.2 features 位（`0x1C340C05F8A00`）

`pyatv.protocols.airplay.utils.parse_features("0xC05F8A00,0x1C340")` 实跑置位：
`9` SupportsAirPlayAudio、`11` AudioRedundant、`15/16/17` 元数据(text/artwork/progress)、
`18/19/20` AudioFormats、`30` HasUnifiedAdvertiserInfo、`38` SupportsUnifiedMediaControl、
`40` SupportsBufferedAudio、`41` SupportsPTP、`46` HKPairingAndAccessControl、
`48` CoreUtilsPairingAndEncryption。
**未置位**：`27` LegacyPairing、`43` SystemPairing。
→ `pyatv.auth.extract_credentials()` 因此判定必须走 **HAP 瞬时配对**（`TRANSIENT_PIN = "3939"`，
不落盘、不需要用户输入）。实测确实如此，配对 341~354 ms 完成。

---

## 2. 成功的报文序列（本 crate 走通的那一条）

```
 1. GET /info                    RTSP/1.0            -> 200 (543B bplist)
 2. POST /pair-setup (M1,M3)     application/x-apple-binary-plist, X-Apple-HKP: 4
                                 -> 200 (409B: state=2 salt(16) publicKey(384))
                                 -> 200 (69B : state=4 proof(64))
 3. 控制通道切 ChaCha20-Poly1305 加密帧（key = HKDF-SHA512(pair-setup 共享密钥)）
 4. SETUP#1   rtsp://<本机IP>/<session-id>        -> 200 {eventPort, timingPort}
    头部：Content-Type: application/x-apple-binary-plist
          Active-Remote / DACP-ID / Client-Instance（CSeq 由发送端递增）
    body（14 键，二进制 plist）：
      deviceID, groupContainsGroupLeader=false, isMultiSelectAirPlay=true,
      macAddress, model="iPhone14,3", name, osBuildVersion="20F66", osName="iPhone OS",
      osVersion="16.5", senderSupportsRelay=false, sessionUUID, sourceVersion="690.7.1",
      statsCollectionEnabled=false, timingPort=<本机 UDP 口>, timingProtocol="NTP"
 5. 连 eventPort（HAP 加密事件通道，TCP）
 6. SETUP#2   同一 TCP 连接，CSeq 递增            -> 200 {streams:[{type:96,dataPort,controlPort}]}
    body：streams=[{ audioFormat, audioMode="default", controlPort, ct, isMedia=true,
                     latencyMax=88200, latencyMin=11025, shk=<32B>, spf=352, sr=44100,
                     type=96, supportsDynamicStreamID=false, streamConnectionID=<同上 session-id> }]
 7. UDP 连 dataPort / controlPort
 8. RECORD                                        -> 200
 9. FLUSH                                         -> 200
10. 音频 RTP 包 → dataPort（352 帧/包）＋ 每 1 s 同步包 → controlPort
11. 每 2 s POST /feedback                         -> 200
12. POST /teardown                                -> 200
```

**实测时延**：`GET /info` 8~9 ms；配对 341~354 ms；SETUP#1 8 ms；SETUP#2 73 ms；
建流整体 573 ms；5 秒音频实发 632 包 / 222464 帧 / 915136 字节（126.4 包/s，理论 125.3）；
TEARDOWN 186 ms（`research/airplay/tone_run.log`）。

**pyatv 参照序列的 CSeq 阶梯**（`reference_sequence.json`）：
`0` GET /info → `1` SETUP#1 → `2` SETUP#2 → `3` volume(500) → `4` progress → `5` dmap →
`6` RECORD → `7` feedback → `8` FLUSH → `9` volume(200) → `10`/`11` feedback → `12` TEARDOWN；
`Session: 0` 头出现在 dmap/FLUSH/TEARDOWN 上。pyatv **从未做 pair-verify**，只 `pair-pin-start` + 两次 pair-setup。

---

## 3. 线路格式（发送端需要的全部）

来源：`cidermill-airplay src/audio/rtp.rs`（常量均 `pub`）+ shairport-sync `rtp.c`。

* **音频包**：12 字节 RTP 头 + 负载；`packet[1] & ~0x80 == 0x60` 表示音频；
  负载 = ALAC 帧或 16 bit 大端 LPCM；352 帧/包 ⇒ 每包 1408 字节（+ ALAC 4 字节帧头）。
* **同步包**：20 字节，`packet[1] == 0xd4`，`flags`（7 = iTunes，4 = AirPlay），
  字段序：`[2:4] flags | [4:8] now_without_latency | [8:16] NTP 64 位 | [16:20] now`。
  接收端按 `latency = now - now_without_latency` 决定播放推迟量。
* **时序包**：32 字节，请求 `type 0xd2`（接收端→发送端），应答 `0xd3`，`seqno = 7` 固定。
* **重传**：请求 `0x55`（4 字节前缀）/ 应答 `0x56`（8 字节前缀）——本 crate 未实现。
* **加密**：音频包在 AirPlay 2 下用 ChaCha20-Poly1305（8 字节 nonce 补零到 12，AAD = 包内 8 字节）；
  密钥由配对共享密钥经 HKDF-SHA512 派生（`Control-Salt` / `Control-Write-Encryption-Key` /
  `Control-Read-Encryption-Key`）；流密钥来自 SETUP#2 的 `shk`。
* **延迟由发送端声明**：SETUP#2 的 `latencyMin/latencyMax` + 每个同步包的 `now_without_latency`。
  设备窗口 = **11025 帧（250 ms）~ 88200 帧（2000 ms）**（`format.rs:156-164`）。

---

## 4. 延迟预算（实测口径）

| 项 | 实测/来源 | 值 |
|---|---|---|
| 排队（我方缓冲） | tone 输出 | 0~19 ms |
| 声明延迟 | 我们取窗口下限 | **250 ms（11025 帧）** |
| 设备内部渲染 | **未上报**（`arrival_to_render_latency_ms = None`），预算按估计 | ~100 ms |
| 端到端估计 | 上三项之和 | **~350 ms** |
| 现状云通路（对比） | MiXPlay `config.json`：`sync_delay_ms=2000` | 1~2 s |

### 4.1 「~350 ms」里哪部分是真测的（务必分清）

**设备未上报 `arrival_to_render_latency_ms`**（本次 SETUP 回复里 `audio_buffer_size` 与
stream id 同样是 `None`），所以预算里的 **100 ms 设备渲染是估计值（`ASSUMED_DEVICE_RENDER_MS`），
不是实测**。可确证的只有前两项：

| 项 | 性质 | 值 |
|---|---|---|
| 声明延迟 | **我们设的**（= 设备窗口下限） | 250 ms |
| 排队 | **实测**（每 0.5 s 采样的排队毫秒） | 0~19 ms |
| 设备渲染 | **估计，非实测** | ~100 ms |

⇒ **可确证下界 = 250 + 0~19 ≈ 250~269 ms；端到端 ~350 ms 这个数含一个未验证的 100 ms 估计项。**
250 ms 是**接收端窗口下限**，不是端到端可达值；即使设备渲染算 0，最好情况也只有 ~265 ms。

### 4.2 独立验证（Lead 跑的第二条曲线，含人耳）

Lead 用同一 `tone.exe` 跑 **30 秒 587 Hz、振幅 0.80、音量 0.65**，用户**确认听到**：

```
RTSP：StreamSetup/Feedback/Record/Flush 全 200   建流 585 ms   TEARDOWN 78 ms
喂入 30.01 s / 实发 30.04 s   3764 包 / 1324928 帧 / 5450272 字节
发包率 125.4/s（理论 125.3）  丢弃 0  欠载 15   延迟预算 ~350 ms
```

两条曲线（5 s / 30 s）在包率、丢弃、预算上一致 ⇒ 配速与背压稳定，不是一次侥幸。
本机**无可用麦克风**（全静默 peak 7/32767），所以「真出声」只能靠人耳（已完成）；
协议层证据是上表，声学/延迟的仪器测量仍未做。

---

## 5. 失败模式与诊断手册（都是本轮真实踩到的）

| 症状 | 真实原因 | 判据 |
|---|---|---|
| 同一 TCP 连接上第二个 RTSP 请求 → `403 Forbidden` | 固件的 RTSP 走 **AirPlay 2 流程**；旧式 RAOP 会话（OPTIONS→ANNOUNCE→SETUP）在这台机器上不成立 | 6 次隔离试验：新连接恒 200；`authenticate` 后的 AirPlay2 流里同连接多请求正常（pyatv CSeq 1→12 同连接） |
| `ANNOUNCE` 后 `SETUP` → `455 Method Not Valid In This State` | 同上：没有 AirPlay 1 的会话句柄（`ANNOUNCE 200` 不返回 `Session` 头） | 9 用例状态机矩阵 |
| `ANNOUNCE` AAC → `406 Not Acceptable` | TXT `cn=0,1` 只支持 LPCM/ALAC | 变体矩阵 9 例 |
| **SETUP#2 无响应 → `os error 10060` 读超时** | **Windows 防火墙挡住音箱发来的 32 字节时序请求**；接收端在时序被应答前扣住 SETUP 回复 | 加规则前后：无规则超时；有规则 `StreamSetup -> 200 (73 ms)` |
| 反复中断后连 `GET /info` 也超时 | 设备被遗留的未完成会话「占住」，几十秒后自愈 | 配对矩阵 V1/V2 的连败，等待后恢复 |
| 音量 `SET_PARAMETER volume` → `500` | 固件对该写法偶发拒绝（pyatv 序列里也有一次，稍后重试成功 200） | 参照序列 CSeq 3 = 500、CSeq 9 = 200 |

**防火墙自查命令**（默认入站 Block，只有 python.exe 与三个 bridge EXE 有规则）：
```powershell
Get-NetFirewallProfile -PolicyStore ActiveStore | Select Name,Enabled,DefaultInboundAction
Get-NetFirewallRule -Direction Inbound -Enabled True |
  Where-Object DisplayName -match 'python|bridge|miplay' | Select DisplayName,Action,Profile
```

**本轮为验证加的临时规则**（UDP 入站、绑程序路径、Private+Public；产品化要求见
`docs/firewall-requirement.md`）：

```
规则名：miplay-bridge probe (temp, lan-transport-prober) setup_probe.exe
        miplay-bridge probe (temp, lan-transport-prober) tone.exe

删除：  Get-NetFirewallRule -DisplayName "miplay-bridge probe*" | Remove-NetFirewallRule
```

> 结论链（一句话版）：**时序回包必须在设备「扣住 SETUP 回复」之前到达**，
> 因此任何 Windows 发送端进程都必须有 UDP 入站放行，且规则要绑**程序路径**
> （时序口是临时端口，按端口写规则没用）。

---

## 6. 完整排除链（比结论更值钱的部分）

从「SETUP#2 发出后 10 秒读超时」到「防火墙拦了时序回包」，一共排除了 6 层假设。
每一步都留下可复查的证据，**顺序不要颠倒**——它也是后来人排查同类问题的模板。

**第 0 步：先确认「不是连不上，而是某一问没答」。**
拿到的错误是 `io error: … (os error 10060)`；cidermill `error.rs` 里 `Connect` 变体带地址
（`could not connect to {addr}: …`），**不带地址的 `Io` 变体**说明 TCP 早连上了。
同时 Python 裸 RTSP 对同一 IP 连发 3 次 `OPTIONS` 全部 0.01 s 返回 `200`、`GET /info` 200
⇒ **设备健康，问题在客户端细节**。又用 `Get-NetTCPConnection` 采样确认：我们与设备的
7000 与 eventPort 两条 TCP 都 ESTABLISHED 7~9 s 后才由我方关闭。

**第 1 步：报文形状。** 把 pyatv 成功报文抽成 `reference_sequence.json`（16 条 exchange，字节级），
再让 `tests/wire_requests.rs` 断言我们的 SETUP#2 与它**逐字段相同**
（`audioFormat=2048/ct=1/spf=352/sr=44100/type=96/latencyMin=11025/latencyMax=88200/shk/isMedia/
audioMode="default"/supportsDynamicStreamID=false/streamConnectionID`）⇒ **排除 body 形状**。

**第 2 步：SETUP#1 字段集。** cidermill 默认不写 `isMultiSelectAirPlay` 等可选键，而 pyatv 写了
——补成 pyatv 的 14 键全集后，SETUP#1 仍 200、SETUP#2 仍静默 ⇒ **排除字段集**
（pyatv 的 `timingProtocol` 值实测就是 `"NTP"`，与 cidermill 一致，也排除）。

**第 3 步：头部与关联。** 6 种 User-Agent（含空串、`AirPlay/366.0`、自定义）全部 200；
`CSeq` 由 `SenderConnection::encode()` 统一递增，没有问题；
`streamConnectionID` 实测**恰等于** RTSP URI 里的 session-id（3112104875）⇒ **排除头部/关联错配**。

**第 4 步：配对形状。** `Ios26`（先两个被放弃的 `/pair-verify` M1，`X-Apple-HKP: 6,3`）/
`Classic`（`X-Apple-HKP: 1`）/ `Ios26WithoutVerifyProbes`（`X-Apple-HKP: 4`）× 是否先
`POST /pair-pin-start` —— 4 种组合**全部**卡在 SETUP#2，而配对本身 100% 成功（341~354 ms）⇒
**排除配对**（`pairing_matrix.log`）。

**第 5 步：剩下唯一可能是「设备在等我们做某件事」。** 回头读 cidermill 的 `src/timing.rs`
模块文档，它对 AppleTV 的原始观察是：

> *a `SETUP` naming `timingProtocol: NTP` and a port makes the receiver send 32-byte timing
> requests to that UDP port and **withhold its `SETUP` reply until one is answered**.
> No clock sync is needed for a URL send, but the handshake is not optional.*

**第 6 步：查「我们有没有可能收不到那个包」。** 三个防火墙 profile 全 `Enabled` +
`DefaultInboundAction = Block`，放行规则只有 `python.exe` 与三个 bridge EXE
（**我们的 exe 一个都没有**）⇒ 音箱发往我们临时时序端口的 UDP 被静默丢弃。
加规则后 **`StreamSetup -> 200 OK（73 ms）`**，5 s / 30 s 两条曲线全通 ⇒ **确证**。

**为什么容易误判**：TCP 出站不受影响，所以前 5 步全部正常；失败表现是「对端不回包」，
看起来像设备 bug；而且时序口是临时端口，只有绑程序路径的规则才管用。

**其它噪声**（同期观察到、与根因无关）：反复中断的会话会让音箱短暂拒绝新连接
（连 `GET /info` 都超时），等几十秒自愈 —— 这也是第 4 步矩阵里 V1/V2 连败的原因，
不要把它当成第二个 bug。

---

## 7. 不能用的那台（L16B）

`192.168.1.171`：mDNS **只**广播 `_mi-connect._udp`（56666）；`7000/7100/554` 全部
`WinError 10061 积极拒绝`；`_raop/_airplay` 一律不应答（6 轮定向复验：`_airplay`/`_raop`
只有 .21 应答，`_mi-connect` 两台都答）。两台都暴露 **miIO UDP 54321**
（hello `21310020…` 回 `device_id` = 934947888 / 501718792，token 需云端下发，本轮未取）。
→ 立体声组合里的 L16B 只能等 **miIO/mi-connect 本地控制**或主箱（L16A）侧成组，详见
`research/lan/lan-paths.md`。

---

## 8. 参考

* cidermill-airplay 0.1.1（GPL-3.0-or-later）源码：`src/client.rs`（`ControlChannel`/`SenderConnection`）、
  `src/audio/script.rs`（`AudioSendState` 状态机）、`src/audio/rtp.rs`（线路常量）、
  `src/timing.rs`（时序握手，含「扣住 SETUP 回复」的原始观察）、`src/pairing.rs`（HAP/瞬时配对）
* pyatv 0.18.0：`protocols/raop/protocols/airplayv2.py`、`protocols/airplay/auth/hap_transient.py`、
  `protocols/airplay/utils.py:AirPlayFlags`
* shairport-sync `rtp.c`（接收端线路格式）
* AirPlay 2 features 位表：<https://emanuelecozzi.net/docs/airplay2/features/>
* 小米音箱的通路全景（含 mDNS/TXT 原始记录、端口扫描、miIO）：`research/lan/lan-paths.md`
* 候选库与许可调研：`research/airplay/rust-crates-survey.md`
