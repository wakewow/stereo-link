# 妙播拉流路线端到端延迟实测与瓶颈拆解

- 任务：task-14（owner: win-audio-capture）
- 日期：2026-10-18（本机时钟）
- 被测路线（唯一路线，用户已确认放弃 AirPlay）：
  `PC 系统声音 → WASAPI 回环采集 → 本机 WAV HTTP 服务(8930) → 小米云 play_url 下发 → 音箱自己来拉流 → 音箱出声`
- 被测实现：`mixplay-pc-audio/pc_stereo_hub.py` + `capture/sysaudio.py`（**未改产品代码**；探针只 import 它们并 monkeypatch HTTP Handler 打点）
- 探针：`miplay-bridge/research/latency/hub_latency_probe.py`、`ubus_status.py`（本目录）
- 设备：默认 render 端点「耳机 (Chu2 DSP)」，混音格式 48000/2ch/f32，共享模式 AUTOCONVERT 输出 44100/2ch/s16le；
  左音箱 `501718792` @192.168.1.21，右音箱 `934947888` @192.168.1.171，PC 192.168.1.4

> 一句话结论：**这条路线里"服务端+采集+云端触发"三段合计只占 ≈140–200 ms，而且已经被我测到毫米级可复现；
> 真正的延迟在音箱内部（拉流到出声），本机拿不到数值。要给出绝对总延迟，只能做声学测量（手机放音箱旁录音）。
> 服务端已经不再是瓶颈，继续优化服务端最多再省 ~20 ms。**

---

## 0. 测点总图

```
 ①PC 内播放(winsound/浏览器)          ④音箱内部（未知）
        │                                   │
        ▼                                   ▼
 [WASAPI 回环]→[sysaudio 缓冲]→[hub 环形缓冲]→[socket]→(LAN)→[音箱 HTTP 客户端]→[解码/渲染]→[出声]
        └─────── ②采集链路 ─────┘ └─ ③服务端 ─┘        └ ⑤云端触发(下发) ┘
   ②=本文实测 31.1ms(或 12.4ms)  ③=实测 0.000s 排队  ⑤=实测 125–172ms  ④=拿不到
```

---

## 1. 分量总表

| # | 分量 | 实测值（中位数/范围） | 测量方式 | 类型 | 能否压缩 |
|---|------|----------------------|----------|------|----------|
| ② | 采集链路：起音→PCM 可被读出（chunk 50ms，现状） | **31.1 ms**（22.8–39.7，n=5） | winsound 播 200ms 1kHz，回环流里检测起音 | 实测 | 可，见下 |
| ②' | 同上，chunk_ms=10 | **12.4 ms**（首次预热 52.8 为异常值；去掉后 6.8 ms，0.6–22.9，n=5） | 同上 | 实测 | **省 ≈19 ms** |
| ③ | hub 环形缓冲排队深度（写头−客户端读头） | **0.000 s**（median/max，60 s，n=913×2 只） | 每秒采样读位置 | 实测 | 已最短，无需压 |
| ③' | 服务端 socket 排队 | 上限 `SO_SNDBUF`=65536 B=**372 ms 音频**；60 s 内实测累计阻塞 **64/80 ms**（≈11–14 KB 等效） | `getsockopt(SO_SNDBUF)` + 每次 write 计时 | 实测 | 已接近最短 |
| ③'' | 跳帧（慢客户端丢数据） | **0 次 / 60 s** | `RingBuffer.read_from` 计数 | 实测 | — |
| ⑤ | 云端触发：`control/play` POST 发出→音箱首个 GET | **125–172 ms**（左 median 140，右 median 156，n=10） | 服务端 accept 时间戳 − POST 发出时间戳 | 实测 | 部分（受小米云限制） |
| ⑤' | 其中 MiXPlay+云 POST 往返本身 | **125–157 ms**（median 148.5，n=10） | POST 前后时间戳 | 实测 | 同上 |
| ① | 播放器自身输出缓冲（浏览器/winsound） | 未测（winsound 含在其内部） | — | 未测 | 由播放器决定 |
| ④ | 音箱内部：收包→解码→渲染→出声 | **拿不到**（无进度字段、无麦克风） | `player_get_play_status` + 连接行为推断 | 未测 | 未知（**瓶颈**） |
| ⑥ | 声学测量（PC 出声 vs 音箱出声 delta） | **不可行**（见 §5） | — | 未测 | — |

**可测部分合计：≈156–203 ms（chunk 50ms）/ ≈137–184 ms（chunk 10ms）。**

---

## 2. 采集链路（分量 ②）

**方法**：`--phase delay`。播 200 ms/1 kHz/0.4 FS 的 WAV（`winsound.SND_ASYNC`），
记录 PlaySound 调用时刻；后台线程持续 `cap.read(16384)`（16384 B = 93 ms 音频），
在 PCM 里用幅度阈值找起音，用"块内第几帧"把分辨率细化到 1 帧（0.023 ms）。

**结果**

| chunk_ms | n | 各次 (ms) | median | mean | 范围 |
|---|---|---|---|---|---|
| 50（sysaudio 默认，现状） | 5 | 39.7 / 22.8 / 31.1 / 36.9 / 31.0 | **31.1** | 32.3 | 22.8–39.7 |
| 10 | 6 | 52.8(预热) / 3.9 / 0.6 / 6.8 / 18.0 / 22.9 | **12.4** | 17.5 | 0.6–52.8 |

原始数据：`capture_delay_chunk50.csv`、`capture_delay_chunk10.csv`；日志 `probe_delay_stdout.log`、`probe_chunk10_stdout.log`。

**误差来源**：① `winsound` 自身的输出延迟（未标定，可能 10–30 ms，三次实验同源所以**只影响绝对值、不影响 50↔10 之差**）；
② 采集块边界（chunk_ms 决定，正是我们要压的项）；③ 起音检测阈值（0.25×peak 迟滞），只会偏大。
④ 首次运行设备预热（chunk10 的第 1 次 52.8 ms 明显是预热，报告同时给出含/不含两种统计）。

**能不能压**：
- `chunk_ms: 50 → 10` 直接省 **≈19 ms**（median 31.1→12.4）。代价是每秒多 40 次系统调用（可忽略）。
- 探针侧的 `read()` 请求是 16384 B(93 ms)，但 `sysaudio.read()` 有数据即返回，所以**读请求大小不是瓶颈**，块大小才是。
- 稳态实测：60 s 内 659 次 read，中位 16384 B/93 ms，p90 110 ms。
- 说明：任务描述里写"块大小默认 10ms"，实际 `SystemAudioCapture` 默认是 **chunk_ms=50**（`sysaudio.py:131`），
  recorder blocksize = 44100×50/1000 = 2205 帧 = 8820 B。

> 注：本项测的是"PC 内两段代码之间的相对延迟"。真实播放器（浏览器）自己的输出缓冲不在此列，无法用回环测出。

---

## 3. 服务端（分量 ③）

### 3.1 稳态 60 s（`--phase steady --steady-seconds 60`，播 1 kHz 循环音，5.0 s 环形缓冲）

| 项 | 左（192.168.1.21） | 右（192.168.1.171） |
|---|---|---|
| 连接时长 | 60.203 s | 60.047 s |
| 服务端发出 | 10,616,832 B (10.12 MB) | 10,584,064 B (10.09 MB) |
| 读取次数 | 649 | 647 |
| 平均速率 | 176.4 kB/s（= 44100×4 B/s，**正好实时**） | 176.4 kB/s |
| write 阻塞(>5ms) 次数 | 4 | 5 |
| write 阻塞累计 | 64.0 ms | 80.0 ms |
| 缓冲深度 median / p10 / p90 / max | **0.000 / 0.000 / 0.000 / 0.000 s**（n=913） | 同左（n=913） |
| 跳帧 | 0 | 0 |

- `SO_SNDBUF` = 65536 B（= 372 ms 音频），两侧相同。
- 原始数据：`latency_summary_steady.json`、`depth_samples.csv`（1921 行）、`probe_steady_stdout.log`。

**结论**：环形缓冲里**从来没有积压**——音箱客户端的读位置一直贴在写头上，
所以 `RING_SECONDS=5.0` **不是延迟项**，它只是"客户端卡顿时别丢数据"的安全垫。
实测 60 s 内零次跳帧、write 阻塞累计仅 64/80 ms（0.11%/0.13%），
即"数据在服务端 socket 里排队"的等效时间平均只有 ~1 ms 量级。

### 3.2 环形缓冲大小的对照实验（`--ring-seconds 0.5`）

| 环形缓冲 | 缓冲深度 | 连接寿命 | 发出 |
|---|---|---|---|
| 5.0 s（hub 现状） | 0.000 s | 60.0 s（两侧全程） | 10.6 MB |
| 0.5 s | 连接期间同样 0.000 s | **左 0.3 s / 右 0.7 s** | 左 0.07 MB / 右 0.15 MB |

⚠️ **这个对照实验有混淆**：把环形缓冲从 5 s 缩到 0.5 s 后，两只音箱都在 1 秒内主动断开
（日志同时出现 `soundcard … data discontinuity in recording` 警告，说明采集侧当时有断流）。
所以**不能**用它得出"环形缓冲大小不影响音箱行为"的结论；它反而说明
**音箱 HTTP 客户端对"数据断流"很敏感**（见 §4.3）。
能确定的只有：在 5 s 缓冲、采集正常的 60 s 里，缓冲深度全程 0.000 s。

### 3.3 关于 `RING_SECONDS=5.0` 是否"只在写满时才影响"

任务问的就是这一点，答案是**是**，并且有实测支撑：
- 新客户端从 `ring.cursor()`（当前写头）开始读，**不补历史**（`pc_stereo_hub.py:207`），
  所以容量多大都不影响开始时刻的延迟；
- 只有当客户端读得比采集慢时，`read_from` 才会丢掉最旧数据（跳帧）——60 s 实测跳帧 0 次，
  说明稳态下容量纯属安全边际；
- 唯一会用到容量上限的场景是客户端卡顿/断线（本次对照实验里断线后"含断线空转"深度涨到 14.8 s），
  这时容量决定"能补多少旧数据"，与延迟无关。

**能不能压**：不需要压，也不能靠它省延迟。建议**不要**把 5 s 改小（0.5 s 的实验已经出现断流敏感性），
如果目标是更低的"断流风险"，反而应该加大或做主动补齐。

---

## 4. 云端触发与音箱行为（分量 ⑤ / ④）

### 4.1 触发开销（`--phase trigger --trigger-repeats 5`，n=5×2=10）

| 项 | 数值 |
|---|---|
| POST 往返（MiXPlay 8820 → 小米云 → 返回 `{'ok': True}`） | 125 / 125 / 140 / 140 / 156 / 156 / 156 / 156 / 157 ms → **median 148.5，范围 125–157** |
| POST 发出 → 音箱首个 HTTP GET | 左 140/140/140/156/156；右 125/156/156/172/172 → **median 左 140 / 右 156，范围 125–172** |
| GET 在 POST 返回之后多久出现 | −31 … +16 ms，**median +15 ms**（负值=音箱比我们的 POST 响应还先开始拉流） |

**结论**：从我们发指令到音箱开始拉流 = **125–172 ms**，其中绝大部分是 MiXPlay+小米云的一次往返
（125–157 ms）。注意这是**一次性启动成本**，不是每秒都付；而"音画/音声不同步"的持续延迟不来自它。

原始数据：`trigger_times.csv`、`trigger_times_prefill.csv`、`latency_summary_trigger.json`、`probe_trigger_stdout.log`。

### 4.2 音箱内部进度：**拿不到**（任务允许的"拿不到就写拿不到"）

按任务建议试了 `player_get_play_status`。PowerShell 5.1 给原生 exe 传 JSON 会吃掉引号
（`message 不是合法 JSON`），所以写了 `ubus_status.py` 用 `subprocess(argv)` 直传，实测：

```
$ python ubus_status.py --once --did 934947888
[00:30:48] right (934947888) code=0 dt=1.80s {"status": 2, "volume": 36, "loop_type": 2}
原始响应: {"code":0,"data":{"code":0,"info":"{ \"status\": 2, \"volume\": 36, \"loop_type\": 2 }"},
          "message":"Msg has been successfully proxy to the device, ..."}
```

- 返回里**只有 status / volume / loop_type，没有任何 position/progress/duration 字段**
  → 无法用"播放进度 vs 已发送字节"反推音箱缓冲深度。查一次耗时 1.80 s（云往返），
  分辨率也远不足以测亚秒级起播延迟。
- 该查询是只读的（`MiToken::load_default()` 只读凭据文件、不写、不改 MiXPlay 状态）。

**因此：音箱"收包→出声"这一段没有数值，我不会编。**

### 4.3 顺带测到的音箱 HTTP 客户端行为（对产品有影响，值得单独记）

| 场景 | 连接寿命 | 读走字节 | 备注 |
|---|---|---|---|
| steady 60 s，有测试音 | 60.05 / 60.20 s | 10.09 / 10.12 MB | 全程稳定，速率正好实时 |
| trigger 阶段（无测试音） | 6.30–6.47 s（10 次全部） | 1.11–1.15 MB | 每次都在 ~6.4 s 由音箱侧关闭（`write` 报 BrokenPipe），下次下发时重连；原因未定位 |
| trigger + 预垫 5 s 缓冲 | 6.67 / 7.09 s（第 1 次） | 1.125 / 1.203 MB | **即使服务端有 5 s 现成数据也不突发预取** |
| 只对左箱下发，24 s 观察 | 24 s | 4.20 MB | 单独下发时稳定；两台先后下发时左箱曾 1.1 s 断开（见下） |

- **读节奏是实时节流**：72 次/6.67 s、77 次/7.09 s、348 次/32.34 s，每次 16384 B ≈ 每 93 ms 一次
  → 音箱按播放速率拉流，**没有**"一次拉几秒再说"的贪婪预取。
  （第 2 次预垫实验里它也这样，因为 hub 从 `cursor()` 起读，预垫的 5 s 根本不会发出去；
  这一点也说明"预垫不能降延迟"，只会被 `read_from` 丢掉。）
- 两台先后下发时，左箱曾在右箱连上后 ~0.2 s 断开（`probe_single_left_stdout.log` 与 steady 日志对比），
  单下发则 24 s 稳定 → **立体声对的两只箱子不适合用两次独立 `play_url` 依次下发**，
  建议按"组"下发或确保第二只下发不打断第一只（需要产品路径专项验证）。
- 若音箱在重连间隙丢数据，听感就是断续/卡顿；稳态 60 s 没发生，但无音/静音场景会被触发。

---

## 5. 声学测量（分量 ⑥）：**不可行**

- PC 麦克风在 PC 边，音箱在另一个房间（192.168.1.21 / .171），本机麦克风采不到音箱声；
- 想可靠拾音必须把手机/麦克风放到音箱旁，这超出本任务授权范围（也不该由我动用户的手机/摆位）；
- 因此本报告**不给出**总延迟的绝对秒数。

**给 lead 的可行做法（唯一能定论的办法）**：用手机在音箱旁录音，同时录到 PC 扬声器的"脉冲"与音箱的"同一个脉冲"，
在录音里量两个脉冲的间隔 = 端到端总延迟。建议 PC 侧放 1 kHz 短脉冲（本目录 `tone_200ms_1k.wav` 可直接用），
重复 5 次取中位数。

---

## 6. 这条路线能压到多少？

**分两段看：**

1. **服务端 + 云端触发（我们代码能控制的部分）：实测 ≈137–203 ms**
   - 采集链路：31.1 ms → 改 `chunk_ms=10` 后 **12.4 ms**（省 ≈19 ms，改一行参数）
   - 服务端排队：**0.000 s**（0 ms）——已无优化空间
   - 云端触发（一次性启动成本）：125–172 ms——受小米云限制，可尝试的只有
     "让流一直活着、不要每次重新 play_url"（复用会话），但那是产品改动，需要 lead 决策
   - **本段下限 ≈ 140 ms 量级**（12 ms 采集 + 125 ms 一次云往返）

2. **音箱内部（收包 → 出声）：本机无法测**
   - 已排除的：不是服务端缓冲（0.000 s）、不是网络吞吐（10.6 MB/60 s 稳定实时）、不是预取贪婪（实测按实时节流）
   - 因此**稳态延迟的真实大头只能落在音箱自己的解码/渲染/抖动缓冲上**
   - 若用户听到的是"1 秒级"延迟，那 1 秒几乎全在音箱里，继续优化 PC 侧无意义；
   - 我的**推断**（不是实测）：因为客户端按实时速率消费、且不突发预取，它不会额外积压"几秒"，
     稳态总延迟更可能是 **200–500 ms 量级**；但这句话在声学测量之前**只能算假设**。

**压不下去的下限**：`≈125 ms（小米云一次下发）+ 音箱自身渲染/缓冲（未知）`。
PC 侧再怎么优化，稳态也不会低于"采集 ~12 ms + 音箱内部"。

---

## 7. 复现方法

```powershell
# 依赖：mixplay-pc-audio/capture/.venv（soundcard/numpy）；只 import 产品脚本，不修改它们
$base='C:\Users\Administrator\AppData\Roaming\uv\python\cpython-3.12.11-windows-x86_64-none\python.exe'
$sp='D:\Backup\Documents\deepseek-harness\default-workspace\mixplay-pc-audio\capture\.venv\Lib\site-packages'
$probe='D:\Backup\Documents\deepseek-harness\default-workspace\miplay-bridge\research\latency\hub_latency_probe.py'
$lat='D:\Backup\Documents\deepseek-harness\default-workspace\miplay-bridge\research\latency'
$env:PYTHONPATH=$sp

& $base -u $probe --phase delay   --delay-rounds 6 --chunk-ms 10 --out $lat --tag _chunk10   # ②
& $base -u $probe --phase steady  --steady-seconds 60            --out $lat --tag _steady    # ③
& $base -u $probe --phase trigger --trigger-repeats 5            --out $lat --tag _trigger   # ⑤
& $base -u $probe --phase trigger --trigger-repeats 3 --prefill-seconds 6 --tag _prefill     # 预垫对照
& $base -u $probe --phase steady  --steady-seconds 30 --ring-seconds 0.5 --tag _ring05       # 环形缓冲对照
& $base -u 'D:\...\research\latency\ubus_status.py' --once --did 934947888                    # ④
```

产出的证据文件（均在 `miplay-bridge/research/latency/`）：
`latency_summary_{steady,trigger,prefill,ring05,chunk10}.json`、`depth_samples.csv`、
`capture_delay_chunk50.csv`、`capture_delay_chunk10.csv`、`trigger_times.csv`、`trigger_times_prefill.csv`、
`probe_{delay,chunk10,steady,trigger,prefill,ring05,single_left}_stdout.log`、`tone_200ms_1k.wav`。

---

## 8. 未验证 / 存疑（不要当成结论）

1. **音箱内部延迟没有任何数值**（§4.2 拿不到、§5 声学不可行）——这是本报告最大的空白，也是**真正的瓶颈候选**。
2. 音箱连接在"无测试音"场景 ~6.4 s 后关闭的**原因未定位**（候选：客户端缓冲刷新、静音处理、会话 stop）。
   它**不等于**"音箱预取了 6.4 s"——1.13 MB 只是 6.4 s × 实时速率，实测读节奏是实时节流的。
3. 环形缓冲 0.5 s 的对照实验被采集侧断流混淆（`data discontinuity`），只能作为"音箱对断流敏感"的旁证。
4. `winsound` 的输出延迟未标定，所以 §2 的绝对值可能偏大；50↔10 ms 的**差值**可信。
5. 两台先后下发时左箱断开的复现次数少（1 次明显），需要产品路径多次复测才能定论。
6. `ubus_status.py` 每次查询 1.8 s，无法用于亚秒级时间分辨率的测量。
