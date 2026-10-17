# 端到端：PC 系统声音 → 小米音箱（局域网直达，不走小米云）

> 目标：把 **PC 上正在放的声音**（任意应用，含 `winsound`、浏览器、播放器）实时推到音箱，
> 端到端 **≤ 400 ms**，而不是现状云端通路的 1~2 s。
> 本文记录**这台机器上实测到的**数字；凡是估计值一律标出来。
>
> 实测环境：Windows 10.0.26100 / AMD64 / 本机 `192.168.1.4`，
> 音箱 `192.168.1.21`（Xiaomi Sound-左，L16A，AirTunes/366.0），2026-10-17 晚。

## 1. 一张图

```
 任意应用（winsound / 浏览器 / 播放器）
        │  系统混音（实测端点：耳机 (Chu2 DSP)，48000 Hz / 2ch / 32-bit float）
        ▼
 ┌─────────────────────────── miplay-audio ───────────────────────────┐
 │ WASAPI 回环采集            ← IAudioClient::Initialize(buffer = --buffer-ms，默认 20 ms)
 │ 环形缓冲 5 s               ← 只在采集线程追不上时溢出（实测不丢）
 │ 48k float → 44.1k s16le    ← wasapi-autoconvert，逐块转换，无额外缓冲
 └───────────────────────────────┬────────────────────────────────────┘
                                 │ 10 ms 一块（--chunk-ms），std::sync::mpsc 有界(8)
                                 ▼
 ┌─────────────────────────── miplay-airplay ─────────────────────────┐
 │ PacketChunker             ← 352 帧/包（= 7.98 ms），不足一包补静音并记欠载
 │ 增益                      ← --gain（线性，上限 8×，饱和不回绕）
 │ 排队上限 40 ms            ← 超出丢最老的（MAX_QUEUED_MS，计入 dropped_pcm_bytes）
 │ ALAC / L16 打包 + ChaCha20-Poly1305（shk）
 └───────────────────────────────┬────────────────────────────────────┘
                                 │ RTP/UDP → 音箱 7000 会话给的数据口；提前 16 ms 发（2 包）
                                 ▼
 ┌────────────────────────── AirPlay 2 接收端（音箱自己） ────────────┐
 │ 抖动缓冲 + 按**声明的延迟**推迟播放 ← 我们声明 250 ms（= 设备窗口下限 11025 帧）
 │ DAC / 功放                         ← 设备未上报内部渲染延迟（预算里按 100 ms 估）
 └───────────────────────────────┬────────────────────────────────────┘
                                 ▼
                               出声
```

配对用的是 **HAP 瞬时配对**（PIN `3939`，无需用户输入、不落盘、不需要小米账号），
控制通道加密后走 AirPlay 2 的 `SETUP`(会话) → `SETUP`(流) → `RECORD` → 周期 `/feedback`；
这条通路**只在局域网内**，一个字节都不经过 `api2.mina.mi.com`。

## 2. 延迟构成（每一环：实测还是估计）

| 环节 | 量级 | 依据 |
|---|---|---|
| WASAPI 采集缓冲 | **20 ms**（`--buffer-ms`，默认值已从 200 改小） | 代码路径（`IAudioClient::Initialize`），未单独测端到端贡献 |
| 采集环形缓冲 5 s | 只在溢出时丢 | 实测 `capture` 侧无丢弃上报 |
| 48k float → 44.1k s16le | 逐块转换，无额外缓冲 | 代码路径（`ConversionPath::WasapiAutoconvert`） |
| 分块 10 ms | 最多一个块 | 代码路径（`--chunk-ms`） |
| **我方排队** | **≤ 40 ms**，实测稳态 **7~40 ms** | **实测**（`stream_stats` 的「排队」= `SenderStats::queued_bytes`） |
| 发包提前量 | +16 ms（2 包） | 实测（`SEND_AHEAD_PACKETS = 2`，为的是绕开 Windows 15.6 ms 定时器粒度） |
| 网络单程 | < 1 ms（同一台交换机） | 与声明窗口相比可忽略 |
| **声明延迟** | **250 ms**（= 11025 帧） | **我们设的**；设备窗口下限 `latencyMin` 由设备 `_raop` TXT 与 SETUP 响应给出，不能再低 |
| **设备内部渲染** | 预算里按 **100 ms（估计）** | **估计值**：设备未上报 `arrival_to_render_latency_ms` / `audio_buffer_size` / stream id（三次真机都返回 `None`） |
| 合计 | **357 ~ 390 ms** | 见下 |

**诚实标注**：`250 ms（声明）+ 7~40 ms（排队）` 是**可确证**的部分（≈ 257~290 ms）；
再加 `100 ms` 的估计值得到 357~390 ms，**达标（≤400 ms）但没有仪器测过「出声时刻」**
（本机没有可用麦克风：全静默 peak 7/32767，见 `research/lan/`）。

## 3. 验收证据（真机）

> 下面引用的 `.log` 是**本地原始输出**（`.gitignore` 里 `*.log` 不入库）；
> 关键读数已经逐字贴在本节里，克隆仓库的人只看本文也能核对这些数字。

### 3.1 12 秒连续推流（PC 系统声音，`--seconds 12`）

```
$ .\target\lan-prober\miplay-bridge.exe stream --target 192.168.1.21 --seconds 12
建流成功     582 ms（含 HAP 瞬时配对）
RTP 端口     数据 53895 控制 33979
RTSP 步骤    StreamSetup -> 200 OK | Feedback -> 200 OK | Record -> 200 OK | Flush -> 200 OK
喂入 PCM     2218988 B（12.58 s @ 44100 Hz/2 ch/s16le）
实发音频     1509 包 / 531168 帧 / 2185032 B，同步包 13
排队/欠载/丢弃 排队 23.5 ms（稳态 30~40），欠载 0 包，丢弃 91572 B
信号         峰值 0.354 FS，RMS 0.247 FS
TEARDOWN     179 ms
延迟预算     排队 23.5 + 声明 250.0 + 设备渲染 100 ms（估计）= 约 374 ms（目标 ≤400，差 -26）
```

原始输出：[`research/airplay/stream_run_queue40.log`](../research/airplay/stream_run_queue40.log)。

**验收口径的那一次（音源振幅 ≥0.7 FS）**（`--seconds 10`，音源 400 Hz / 0.75 FS）：

```
运行时长     10.44 s
喂入 PCM     1619352 B（9.18 s）
实发音频     1083 包 / 381216 帧 / 1568184 B，同步包 11
排队/欠载/丢弃 排队 0.0 ms（稳态 22~40），欠载 1 包，丢弃 95668 B
信号         峰值 0.750 FS，RMS 0.518 FS
TEARDOWN     197 ms
延迟预算     排队 0.0 + 声明 250.0 + 设备渲染 100 ms（估计）= 约 350 ms
```

原始输出：[`research/airplay/stream_run_075fs.log`](../research/airplay/stream_run_075fs.log)。
（峰值 0.750 FS 是**采集到的**信号幅度，用来证明「推出去的是声音而不是静音」，
也是之前 0.12 FS 用户听不见那个坑的对照。）

### 3.2 连续 3 分钟长跑（`--seconds 180`，安静音源 100 Hz / 0.02 FS）

```
运行时长     180.26 s
喂入 PCM     31850660 B（180.56 s）
实发音频     22556 包 / 7939712 帧 / 32661088 B，同步包 181
排队/欠载/丢弃 排队 7.1 ms（中途 12~15），欠载 1 包，丢弃 91968 B
信号         峰值 0.020 FS，RMS 0.014 FS（音源故意压到 0.02，别吵人）
RTSP 步骤    StreamSetup/Feedback/Record/Flush 全 200
TEARDOWN     248 ms
延迟预算     排队 7.1 + 声明 250.0 + 设备渲染 100 ms（估计）= 约 357 ms
```

**读数**：`dropped_pcm_bytes = 91968 B / 32661088 B = 0.28%`，且**几乎全部发生在连接后的第一秒**
（t=1.0 s 时已 91504 B，之后 179 秒只再丢 464 B）——那一下是「音源早就在放、我们中途接上」的
一次性突发，不是持续丢音频。`underrun_packets = 1`（180 秒里 1 个包 ≈ 8 ms）。
原始输出：[`research/airplay/stream_soak_180s.log`](../research/airplay/stream_soak_180s.log)。

### 3.3 Ctrl-C 必须立刻安静（TEARDOWN）

用**真正的 `CTRL_C_EVENT`**在独立控制台里验证（`research/airplay/ctrlc_console_acceptance.py`：
`FreeConsole` → `AllocConsole` → `GenerateConsoleCtrlEvent(CTRL_C_EVENT, 0)`）：

```
GenerateConsoleCtrlEvent ok=True；退出码 0；0.22 s
=== 结束（Ctrl-C）===
运行时长     11.61 s
RTSP 步骤    StreamSetup -> 200 OK | Feedback -> 200 OK | Record -> 200 OK | Flush -> 200 OK
TEARDOWN     202 ms
```

原始输出：[`research/airplay/stream_ctrlc_console.log`](../research/airplay/stream_ctrlc_console.log)。
`TEARDOWN` 走完才退出 ⇒ 音箱侧会话被正常拆掉，不会「进程没了但音箱还在响」。

### 3.4 人耳

「两只音箱都听到」由 Lead 组织、用户确认（task-13 汇报）；
本文件只记录**机器可验证**的部分，不重复声称人耳结论。

## 4. 已知缺口（不装作没有）

1. **Ctrl-Break / 关窗不会 TEARDOWN**（真机实测，值得修）：`tokio::signal::ctrl_c()` 在 Windows
   上只注册 `CTRL_C_EVENT`；用 `CTRL_BREAK_EVENT` 打断时进程以 **`0xC000013A`
   (STATUS_CONTROL_C_EXIT) 0.01 s 硬退出**，没有统计、没有 TEARDOWN
   （`research/airplay/ctrl_c_acceptance.py` 的输出）。用户关掉控制台窗口（`CTRL_CLOSE_EVENT`）
   是同类问题。修法：把 `signal::windows::ctrl_break()` 与 `ctrl_close()` 一起放进 `select!`。
2. **设备渲染延迟未上报**：100 ms 是估计值，不是实测。
3. **静音时欠载计数会涨**：没有应用在放声音时，WASAPI 回环不投递缓冲，发送端只能补静音，
   于是 `underrun_packets` 增长（早期一次 22 s 静音运行里是 1178）。**它不是故障**，但看统计时要知道。
4. **LPCM 没做单独出声验证**：默认 `--codec alac`（与 pyatv 成功路径一致）；
   `--codec lpcm` 只验证到 `StreamSetup 200`。
5. **只有 L16A（`.21`）能收**：L16B（`.171`）不广播 `_raop/_airplay`，7000/7100/554 全拒。
6. **采集端点默认取「默认渲染端点」**：本机实测是「耳机 (Chu2 DSP)」。换了默认设备要用
   `--device <子串>` 指回来（脚本会打印实际用的端点与协商格式）。

## 5. 复现

```powershell
# 编译（本仓库 target\ 可能被别的 teammate 的进程占着，用一个自己的 target 目录最省事）
$env:CARGO_TARGET_DIR="$env:TEMP\miplay-target-lan"
cargo build --offline -p miplay-server

# 只看设备、不碰音箱
& "$env:TEMP\miplay-target-lan\debug\miplay-bridge.exe" stream --dry-run

# 真推：12 秒，系统声音
& "$env:TEMP\miplay-target-lan\debug\miplay-bridge.exe" stream --target 192.168.1.21 --seconds 12
```

**前置条件（必须）**：`miplay-bridge.exe` 需要**绑程序路径的 UDP 入站放行**，
否则音箱发往本机时序端口的 32 字节 NTP 请求被 Windows 防火墙丢掉，
接收端会一直扣住 `SETUP` 的回复，表现为 `io error 10060`（我们踩过一整个晚上）。
细节与产品要求见 [`firewall-requirement.md`](firewall-requirement.md)。

## 6. CI 回归（不需要真机）

`crates/miplay-airplay/tests/capture_to_packets.rs` 把「采集块 → 增益 → 分块 → RTP 包」
整条路用字节断言锁住（含补静音、时间线连续、增益饱和、排队裁剪）：

```powershell
cargo test --offline -p miplay-airplay -p miplay-server   # 24 + 7 + 9 + 3 + 8 + 1 = 52 passed
```

`capture_to_packets.rs` 的 7 个用例：8 ms 块一包一帧不差、10 ms 块跨包不丢帧、不足一包补静音并
打标、补静音仍推进 RTP 时间戳、增益饱和不回绕、非法增益被拒、排队被 `trim_to` 压到 40 ms。
