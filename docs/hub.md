# `hub` 子命令：系统声音 → 局域网 WAV 服务 → 小米音箱拉流

> 这是用户选定的**唯一路线**（用户原话 m01340：*"不使用AirPlay,使用小米妙播,AirPlay的延迟高,放弃AirPlay的路线,仅使用小米妙播的路线"*）。
> 妙播（MiPlay）的播放模型是「云端下发一条 URL，音箱自己来拉」，所以本机只要**把系统声音变成一个音箱能拉的 HTTP WAV 流**，
> 延迟就只取决于局域网 + 音箱自己的缓冲，不再依赖 MiXPlay 那套 PC 侧接收端。

## 1. 链路

```
 Windows 默认输出端点（例如「耳机 (Chu2 DSP)」）
        │  WASAPI 共享模式回环采集（20 ms 缓冲，48k float → 44.1k/2ch/s16 自动转换）
        ▼
 AudioCapture ──► RingBuffer（采集环，5 s）──► hub-pump 线程（8192 B/块）
                                                    │  ChannelRouter：每台设备一条 RingBuffer（5 s）
                                                    │    · mixdown / left / right / stereo + gain
                                                    ▼
                        HTTP GET /hub/<did>.wav  ←── axum 流式响应：44 字节 WAV 头 + 无限 PCM
                                                    ▲
        小米云 MiNA `player_play_url`（URL 里是本机 LAN IP）── 音箱自己按这个 URL 来拉
```

* 云端只负责**送一条 URL**，音频数据一个字节都不经过小米服务器。
* 每台设备一条独立环形缓冲，所以「左右分离」（`--split`）就是让第 1 台只读左声道、第 2 台只读右声道——立体声组合的两只箱子各自拉自己那一路。
* WAV 头里的 `data` 长度写 `0x7FFFFF00`（约 2 GB 的「假装无限」写法，和 MiXPlay 自带的 `server.py` / Python 版 hub 一致）：
  音箱会一直读到连接结束为止，不会因为头部声明长度到了就停。

## 2. 快速开始

```powershell
# 列出 did / MiNA uuid（两种都能传给 --devices）
.\target\debug\miplay-bridge.exe devices

# 最小用法：把系统声音同时投放给两只音箱（所有设备听同一份混音）
.\target\debug\miplay-bridge.exe hub --devices 501718792,934947888 --gain 0.7

# 立体声组合：第 1 台左、第 2 台右
.\target\debug\miplay-bridge.exe hub --devices 501718792,934947888 --split --gain 0.7

# 先起服务，由网页点按钮决定何时投放
.\target\debug\miplay-bridge.exe hub --devices 501718792,934947888 --no-autostart

# 只停止（不起服务、不采集）
.\target\debug\miplay-bridge.exe hub --devices 501718792,934947888 --stop

# 只想看计划、不下发（不会建流、不会开设备）
.\target\debug\miplay-bridge.exe hub --devices 501718792,934947888 --split --dry-run
```

启动后打印：网页控制地址（`http://<本机LAN IP>:8931/`）、拉流地址模板、实际采集端点与协商格式、每台设备的目标 URL 与声道。

### 参数

| 参数 | 默认 | 说明 |
| --- | --- | --- |
| `--devices <did[,did…]>` | 必填 | did 或 MiNA uuid；`--dry-run` 时可不填 |
| `--port <端口>` | `8931` | HTTP 服务端口（Python 版 hub 占着 8930） |
| `--split` | 关 | 左右分离：第 1 台左、第 2 台右，其余混音 |
| `--swap` | 关 | `--split` 时左右对调 |
| `--gain <0~8>` | `1.0` | 采集增益，逐样本饱和限幅（不会回绕） |
| `--volume <0~100>` | 不发 | 投放前给设备设一次音量（只发指令；**默认不动设备音量**） |
| `--capture-device <子串>` | 系统默认输出 | 采集哪个输出端点 |
| `--url-base <URL>` | 自动探测 | 覆盖下发给音箱的地址前缀（多网卡时可手动指定） |
| `--no-firewall` | 关 | 不尝试加 Windows 防火墙入站规则 |
| `--no-autostart` | 关 | 不自动下发，等网页/API 触发 |
| `--seconds <秒>` | 一直跑 | 到点自动收尾（和 Ctrl-C 走同一条收尾路径） |
| `--dry-run` | 关 | 只打印计划 |
| `--stop` | 关 | 只停止设备后退出 |
| `-h, --help` | | 帮助 |

### 网页控制

`web/index.html` 同时被 `serve`(8900) 和 `hub`(8931) 提供，页面里的「**系统声音投放**」面板：

* 设备多选（复选框）+ `投放到选中设备` / `停止选中设备` / `全部停止` / `全选` / `清空` / `复制启动命令`；
* 每 1.5 s 轮询状态：投放中/空闲、拉流客户端数、**落后秒数**（该设备最慢客户端落后直播写入位置多少）、已发 MB、下发次数与失败数、声道、最近客户端接入时间；
* 面板在 8931 上打开是同源；在 8900 上打开会跨源调 8931（hub 侧带 `Access-Control-Allow-Origin: *` 与 OPTIONS 预检处理）。

### HTTP API（hub 服务）

| 方法 | 路径 | 说明 |
| --- | --- | --- |
| GET | `/` | 设备总览 + 系统声音投放面板（同一个 index.html） |
| GET | `/health` | `ok` |
| GET | `/hub/{did}.wav` | 流式 WAV（44 字节头 + 无限 PCM）；`{did}` 也接受 MiNA uuid，带不带 `.wav` 都行 |
| GET | `/api/hub/status` | 完整状态 JSON（见下） |
| POST | `/api/hub/start` | body `{"devices":[…可选]}`；缺省=全部在册设备；下发 `play_url` 并置为投放中 |
| POST | `/api/hub/stop` | 同上；先关流再让云端停播 |

`status` 关键字段：`devices[].{did,name,online,active,clients,lag_seconds,sent/bytes_sent,skipped_bytes,plays_sent,play_failures,reissues,last_client_seconds_ago,last_error,source,url}`、
`capture.{device,negotiated,format,backlog_seconds,ring_retained_seconds,ring_seconds,frames,capturing,seconds_since_audio,audio_recent}`、`clients_total`、`uptime_seconds`、`url_base`。

> 两个「秒数」的含义别混：`lag_seconds` 是**客户端落后直播位置**（诊断用，正常 ≈ 0）；`ring_retained_seconds` 是 `RingBuffer::available()`，即环里**保留了多少历史**（我们的环固定保留满 5 s，即使客户端已读完，这个数也会显示 5.0——它不是延迟）。

## 3. 代码位置

| 位置 | 内容 |
| --- | --- |
| `crates/miplay-server/src/hub.rs` | 整个 `hub` 子命令（参数解析、设备台账、HTTP、采集泵、收尾；`#[cfg(windows)] mod win` 装 Windows 专有部分） |
| `crates/miplay-server/src/main.rs` | `mod hub;` + `usage()` 里的一行 + `match cmd` 的 `"hub"` 分支 |
| `web/index.html` | 投放面板（`.panel` / `#hub-list` / hub JS 段） |
| `crates/miplay-audio` | 复用：`AudioCapture`（WASAPI 回环）、`RingBuffer`、`ChannelRouter`/`ChannelRoute`/`ChannelSource`、`build_stream_wav_header` |
| `crates/miplay-core` | 复用：`MiToken`、`MinaClient::{device_list,play_on,stop,set_volume,find}` |

采集参数：`CaptureConfig::default().with_mode(Loopback).with_samplerate(44100).with_channels(2)` 且 `buffer_ms = 20`（默认 200 ms 对「拉到就放」太肥）。
泵线程每次从采集环读 **`PUMP_CHUNK = 1764 B`（正好 10 ms）**——`research/latency/latency-report.md`（task-14）实测：读块 50 ms → 10 ms 把 PC 侧采集链路从 31.1 ms 压到 12.4 ms。
每条流的环：`ChannelRouter::for_seconds(routes, 44100, 2, 5.0)`（task-14 实测服务端环形缓冲排队深度 60 s 内 median=p90=max=**0.000 s**，所以 5 s 纯粹是抗卡顿安全垫，**不要为了降延迟去改小它**）。

多台设备的下发是**并发**的（`futures_util::future::join_all`），不是 for 循环串行：task-14 实测立体声对的两只箱子先后下发时，先连上的那只可能在另一只连上后约 **0.2 s** 断开。

断线重发策略（`reissue_loop`，扫描周期 2 s，两次下发最小间隔 2 s）：

| 情况 | 等待阈值 |
| --- | --- |
| 从没连上过（`since_client = None`） | `REISSUE_AFTER` = **8 s**（按下发之后的等待时间算） |
| 连上过又掉了 | `RECONNECT_AFTER` = **2 s** |
| 连续重发仍没人连上，且当前没有声音 | 2 s × 2ⁿ（n = 连续重发次数），封顶 **30 s** |
| 当前正在出声音（`audio_recent`） | **不吃退避**，直接走 2 s 快路径 |

最后一条是必需的：音箱在纯静音约 6.4 s 后会主动断开连接，如果退避到 30 s，用户重新放声音时要等半分钟才有人来拉流。
设备离线（`online=false`）时直接跳过，不空转下发。

## 4. 延迟构成（哪些是实测、哪些是估计）

| 环节 | 量级 | 依据 |
| --- | --- | --- |
| WASAPI 共享模式采集缓冲 | **20 ms**（可调） | 实测：启动打印 `采集格式 … WASAPI 缓冲 20 ms`；`CaptureConfig.buffer_ms=20` |
| 采集环 → 泵线程 | **10 ms 读块**（`PUMP_CHUNK = 1764 B`） | task-14 实测：读块 50 ms→10 ms 让采集链路 31.1→12.4 ms（`research/latency/latency-report.md`） |
| 服务端环形缓冲排队 | **0 ms** | task-14 实测 60 s、913×2 次采样 median=p90=max=0.000 s，跳帧 0 次 |
| socket 发送排队 | ~1 ms 等效 | task-14 实测 `SO_SNDBUF`=65536 B（=372 ms 音频），60 s 内累计阻塞仅 64/80 ms |
| 本机分发 + HTTP | < 5 ms | 局域网；日志里下拉流地址后客户端 3 ms~0.74 s 内接入（含音箱解析 URL） |
| 云端触发 `play_url` → 音箱首个 GET | **125~172 ms**（一次性） | task-14 实测 10 次：median 左 140 / 右 156；其中 POST 往返本身 125~157 ms |
| 音箱侧缓冲 | **未知（关键未知项）** | 我们最多只能喂到「我们环里已有的 5 s」，音箱自己缓冲多少、什么策略没有公开信息，也没有麦克风可测声学延迟 |

**诚实结论**：这条路线的延迟下限不在我们手里，而在**音箱自己的缓冲 + 云端下发的一次往返**。我们能控制的是把本机侧压到 ~20-50 ms、
并把「本机到音箱」的传输放在局域网（音频不经过小米云）。用户对 Python 版 hub 的体感是 1~2 s，与「云下发 + 音箱缓冲」这两项吻合。
本次没有做声学实测（task-13 已确认本机麦克风全静默，peak=7/32767，无法做自动化回环测量）。

## 5. 实测证据（2026-10-18，本机 `192.168.1.4`，两只音箱 L16A `192.168.1.21` / L16B `192.168.1.171`）

1. **两只音箱都来拉流**（`--split`，30 s、587 Hz、0.72 FS 测试音）：
   ```
   [下发] Xiaomi Sound-左 (501718792) <- http://192.168.1.4:8931/hub/501718792.wav  [player_play_url]
   [下发] Xiaomi Sound    (934947888) <- http://192.168.1.4:8931/hub/934947888.wav   [player_play_url]
   [流] Xiaomi Sound-左 (501718792) 客户端 192.168.1.21:59958 接入，从当前时刻开始
   [流] Xiaomi Sound    (934947888) 客户端 192.168.1.171:46226 接入，从当前时刻开始
   ```
   两台各拉走 **5.12 MB ≈ 30.4 s**，`skipped_bytes=0`，`lag_seconds=0.0`，`capture.frames=1341050`（30.4 s）。
2. **断线重发真的会触发**（长时间无客户端时）：
   ```
   [重发] Xiaomi Sound (934947888) 已经 41.8 秒没有客户端接入
   [下发] Xiaomi Sound (934947888) <- …/934947888.wav [player_play_url]
   [流] Xiaomi Sound (934947888) 客户端 192.168.1.171:46234 接入
   ```
3. **Ctrl-C 优雅收尾**（真控制台 + `GenerateConsoleCtrlEvent(CTRL_C_EVENT)`，10 s 测试音）：
   ```
   收到 Ctrl-C，停止投放…
   [流] Xiaomi Sound-左 (501718792) 客户端 192.168.1.21:59984 结束：1.63 MB / 10.0 秒
   [停止] Xiaomi Sound-左 (501718792) => true
   [流] Xiaomi Sound (934947888) 客户端 192.168.1.171:46252 结束：1.65 MB / 10.2 秒
   [停止] Xiaomi Sound (934947888) => true
   [采集] 泵线程退出，共分发 1.7 MB
   === 结束 ===   运行时长 12.9 s   残留客户端 0
   ```
   进程退出码 **0**；之后 8931 不可达。
4. **网页按钮路径**（`--no-autostart`）：`POST /api/hub/start` → 两台 `ok=true`，5 s 内各出现 1 个拉流客户端；`POST /api/hub/stop`（body `{}`=全部）→ 两台 `ok=true`，`clients=0`、`active=false`。
5. **WAV 头原样**（第三方客户端 `curl` 拉 `/hub/501718792.wav`，取到 44 字节后因无音频可发而挂住）：
   ```
   RIFF 24-FF-FF-7F WAVE fmt  16 00 00 00 01 00 02 00 44-AC-00-00 10-B1-02-00 04 00 10 00 data 00-FF-FF-7F
   ```
   = PCM / 2 ch / 44100 Hz / 176400 B/s / block 4 / 16 bit / `data` 大小 `0x7FFFFF00`。

## 6. 已知限制与坑

* **只在 Windows 上可用**（WASAPI 回环）；非 Windows 编译成一句 `bail!`。
* **静音时不产生数据**：本机默认回环端点在无声时不给数据（静音抑制），此时服务只发空块、音箱收不到新 PCM。
  音箱会重连或等待，`--no-autostart` 下也可以先起服务、等真要放声音时再点投放。
* **音箱按实时速率拉流、不预取**（task-14 实测：每次 16384 B / 约 93 ms，正好 176.4 kB/s）：
  服务端预垫 5 秒数据它也不会突发拉走。所以**不要指望靠预垫降延迟**，`PREFILL` 只是让第一个 GET 就能拿到声音。
* **纯静音约 6.4 s 后音箱会主动关闭连接**（task-14 实测 10 次全部如此，原因未定位）→ 断线重连是**常态**：
  实现上按 §3 的退避表自动重发，网页上能直接看到每台设备的「重发 N」。
* **立体声对的两只箱子先后下发时，先连上的那只可能在另一只连上后约 0.2 s 断开**（task-14 实测）→
  我们的下发是并发的，并且掉线会在 2 s 内重连（有声音时不吃退避），所以这个抖动会被自动兜住。
* **不支持 Range 请求**：音箱不用，但第三方播放器若要 seek 会失败（我们是无限流）。
* **我们的环只有 5 s**：音箱拉得再快也只能提前 5 s；`skipped_bytes` 增长说明某个客户端落后超过了环容量（音箱侧网络抖动时会出现）。
* **Windows 防火墙**：默认入站 Block，启动时会尽力加 `miplay-bridge hub <端口>` 这条 TCP 入站规则（`--no-firewall` 可关）。
  它绑的是端口而不是程序路径；产品化时应当像 `docs/firewall-requirement.md` 要求的那样**绑程序路径**并由程序首次运行自行请求。
  手动删除：`Get-NetFirewallRule -DisplayName "miplay-bridge hub*" | Remove-NetFirewallRule`。
* **凭据**：`hub` 需要小米账号（下发指令走 MiNA 云），凭据来自 `MiToken::load_default()`；代码与日志都不打印 token。
* **测试坑（实测教训）**：用 `subprocess` + `CREATE_NEW_PROCESS_GROUP` 起子进程时 **CTRL_C_EVENT 送不到**（新进程组里 Ctrl+C 被禁用），
  表现为「事件发送成功但进程不退出」。验收脚本必须不加这个 flag（去掉后立刻优雅退出）。真实终端里按 Ctrl-C 不受影响。
* **设备音量**：默认不发 `--volume`，即不碰设备音量；`--volume` 只发云端指令，不改硬件配置（本次验收全程未改音量）。

## 7. 回归测试

```powershell
cargo test -p miplay-server --offline      # 21 passed（其中 hub 13 个、stream 8 个）
cargo build -p miplay-server --offline     # 0 warning
```

hub 的单测覆盖：WAV 头 44 字节结构、**泵读块必须是 10 ms/1764 B**、左右分离路由、混音+增益、慢客户端跳帧（不阻塞）、空环读超时、
URL 形态解析（`/hub/x.wav?q=1`、`.WAV`、结尾斜杠）、设备键匹配（did / MiNa uuid / 路径）、
设备列表解析（去空白/去重）、客户端计数不下溢、状态 JSON 字段、`args` 解析（含各类非法值）、
重发判据（8 s / 2 s / 退避翻倍且封顶 / 有声音走快路径）。

手工验收（脚本在 `%TEMP%`，不进仓库；原理见 §5）：

```powershell
# 真 Ctrl-C：AllocConsole + GenerateConsoleCtrlEvent(CTRL_C_EVENT, 0)
python "$env:TEMP\hub_ctrlc_acceptance.py" --after 10
```

## 8. 未决 / 待办

1. **声学延迟未实测**（缺麦克风）。要拿到端到端数字，需要外部录音设备或让音箱播放特征音再用手机录。
2. **音箱侧缓冲策略未知**：`lag_seconds` 只能看到「它落后我们多少」，看不到「它内部还有多少没放」。
3. **音箱偶发重连**（几十秒无客户端时）已用重发兜住，但根因未查明（可能是固件在流空闲时主动断开）。
4. `hub` 只处理 Ctrl-C；`CTRL_BREAK`/关窗口（`CTRL_CLOSE_EVENT`）仍会立即终止而不收尾（`stream` 子命令同样如此）。
5. 立体声组合的「两只箱子是否真的一左一右」需要人耳确认（`docs/ear-test-stereo-pair.md` 是 Lead 组织的验收记录）。
