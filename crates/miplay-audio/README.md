# miplay-audio

把 **Windows 正在播放的系统声音**抓成 PCM，拆声道后分发给多个输出目标（音箱 / HTTP 流）。

这是 `mixplay-pc-audio/pc_stereo_hub.py`（已在 Python 上验证过的链路）的 Rust 版，语义对齐：

```text
扬声器端点 ──WASAPI loopback──▶ AudioCapture ──44100/2ch/s16le──▶ RingBuffer
                                                                    │
                                             ChannelRouter ─────────┘
                                                   ├─ left  流 (RingBuffer)
                                                   ├─ right 流 (RingBuffer)
                                                   └─ 任意 N 条流
```

对齐的要点：

| 点 | 取值 |
| --- | --- |
| 采样格式 | 44100 Hz / 2 ch / 16 bit **交错** s16le |
| WAV 头 `data` 大小 | 写死 `0x7FFFFF00`（≈2 GiB 假长度，音箱接受），当无限长流 |
| 环形缓冲 | 默认 5 秒（44100×2ch×16bit → 882000 字节） |
| 新读者起点 | 从**当前**写位置开始（不补历史） |
| 慢读者 | 直接跳帧丢旧数据，不阻塞写入者 |
| 左右分离 | 把 L / R **各复制成 2ch**，各自成一条流 |

HTTP 层不在本 crate 内（归 `miplay-server`）；本 crate 只负责「采到干净 PCM + 分好流 + 给出可复用的 WAV 头」。

---

## 1. 快速开始

```powershell
# 列本机端点（名字 / id / 是否默认 / 混音格式）
cargo run -p miplay-audio --bin endpoints

# 真机回环自测：本进程播 1kHz 正弦，同时回环采集，算 peak/RMS/主频并写 WAV
cargo run -p miplay-audio --bin loopback -- --freq 1000 --seconds 5 --amp 0.4 --out capture.wav

# 静音对照（应当采到全零 / 或压根没有数据包）
cargo run -p miplay-audio --bin loopback -- --no-play --expect-silence --seconds 2

# 单测（不需要声卡的部分全绿；集成测试要真声卡）
cargo test -p miplay-audio
```

`loopback` 的退出码：`0` 符合预期；`1` 参数/设备错误；`2` 该有声却采到全零（失败）；`3` 静音对照里采到非零。

最小用法：

```rust
use miplay_audio::{AudioCapture, CaptureConfig, ChannelRouter, ChannelRoute, ChannelSource};
use std::time::Duration;

let mut capture = AudioCapture::new(CaptureConfig::default())?; // 默认 render 端点 + 回环
capture.start()?;                                              // 起后台采集线程

let router = ChannelRouter::for_seconds(
    vec![
        ChannelRoute::new("left", ChannelSource::Left),
        ChannelRoute::new("right", ChannelSource::Right),
    ],
    44_100,
    2,
    5.0,
);

loop {
    let chunk = capture.read_vec(44_100 / 50, Duration::from_millis(200))?; // 10ms 一份
    if !chunk.is_empty() {
        router.write_s16_stereo(&chunk); // 内部按 rout 拆帧分发，不阻塞
    }
}
# Ok::<(), miplay_audio::AudioError>(())
```

## 2. 公开 API

### 采集（`capture`，仅 Windows）

```rust
pub struct AudioCapture { /* … */ }

impl AudioCapture {
    pub fn new(config: CaptureConfig) -> Result<Self>;          // 解析端点 + 协商格式（还没开始采）
    pub fn start(&mut self) -> Result<()>;                      // 起后台线程，持续把 PCM 写进内部 RingBuffer
    pub fn stop(&mut self);                                     // 停线程（Drop 也会调）
    pub fn read(&self, buf: &mut [u8], timeout: Duration) -> Result<usize>;
    pub fn read_vec(&self, max_bytes: usize, timeout: Duration) -> Result<Vec<u8>>;
    pub fn ring(&self) -> Arc<RingBuffer>;                      // 原始 44100/2ch/s16le 交错
    pub fn cursor(&self) -> u64;
    pub fn negotiated_format(&self) -> Option<&NegotiatedFormat>; // 端点名/id/混音格式/转换路径
    pub fn config(&self) -> &CaptureConfig;
    // 还可以查：device_name / device_id / mode / samplerate / channels / is_running / last_error
}

pub struct CaptureConfig {           // Default = 默认 render 端点 + Loopback + 44100/2ch + 5s + 200ms
    pub device: DeviceSelector,
    pub samplerate: u32,
    pub channels: u16,               // 1 或 2
    pub mode: CaptureMode,           // Loopback | Capture
    pub ring_seconds: f32,
    pub buffer_ms: u32,              // WASAPI 共享缓冲时长
}

pub enum CaptureMode { Loopback, Capture }
pub enum ConversionPath { WasapiAutoConvert, Manual, Passthrough }   // as_str() → "wasapi-autoconvert" / …

pub fn list_endpoints() -> Result<Vec<EndpointInfo>>;
pub fn resolve_endpoint(sel: &DeviceSelector, mode: CaptureMode) -> Result<EndpointInfo>;
pub fn default_endpoint() -> Result<EndpointInfo>;
```

* `read` / `read_vec`：读的是**内部 RingBuffer**，超时返回空，不阻塞采集线程；`Err(Closed)` 表示采集已停。
* 采集线程碰到端点失效（`AUDCLNT_E_DEVICE_INVALIDATED`）会停下来并记在 `last_error()`，上层重建即可。

### 设备选择（`endpoint`）

```rust
pub enum DeviceSelector {
    Default,                                  // 该 flow 的系统默认端点
    NameContains(String),                     // 友好名子串，不区分大小写（"Chu2"、"Realtek"）
    Id(String),                               // 完整端点 id（"{0.0.0.00000000}.{guid}"），也接受包含匹配
}

pub struct EndpointInfo { pub id, pub name, pub flow: DataFlow, pub is_default: bool,
                          pub mix_format: Option<FormatInfo>, pub error: Option<String> }
pub struct FormatInfo   { pub sample_format: Option<SampleFormat>, pub channels, pub samplerate,
                          pub bits, pub block_align, pub avg_bytes_per_sec,
                          pub raw_format_tag: u16, pub is_extensible: bool }
pub struct PcmFormat    { pub samplerate: u32, pub channels: u16, pub bits: u16 }
```

* `Loopback` 模式只列 `render` 端点；`Capture` 模式只列 `capture` 端点。
* 名字子串命中多个时取第一个（`DeviceSelector::matches()` 也单独暴露，便于自己写更细的策略）。
* 不要求用户记 GUID：`CaptureConfig::default().with_device_name("Chu2")` 即可。

### 环形缓冲（`ring`）

```rust
pub struct RingBuffer { /* … */ }

impl RingBuffer {
    pub fn new(capacity_bytes: usize) -> Self;
    pub fn for_seconds(seconds: f32, bytes_per_second: u32) -> Self;
    pub fn write(&self, data: &[u8]);
    pub fn cursor(&self) -> u64;                       // 写到哪儿了（绝对字节偏移）
    pub fn earliest_cursor(&self) -> u64;              // 还能读到的最旧位置
    pub fn read_from(&self, pos: u64, max_bytes: usize, timeout: Duration) -> Result<ReadOutcome>;
    pub fn close(&self);
}

pub struct ReadOutcome { pub data: Vec<u8>, pub next: u64, pub skipped: u64, pub closed: bool }
```

语义与 Python 版一致：`pos` 比最旧数据还旧 → 从最旧处开始（跳帧，`skipped` 记录丢了多少字节）；`pos` 已到写位置 → 等 `timeout`；`close()` 后 `read_from` 立即返回不再阻塞。`write` 单次写入超过容量时只保留尾部。

### 声道分发（`channels`）

```rust
pub enum ChannelSource { Left, Right, Mixdown, Stereo }   // Left/Right 复制成 2ch；Mixdown 取平均；Stereo 原样

pub struct ChannelRoute { pub id: String, pub source: ChannelSource, pub gain: f32 }
pub struct ChannelRouter { /* … */ }

impl ChannelRouter {
    pub fn new(routes: Vec<ChannelRoute>, capacity_bytes: usize) -> Self;
    pub fn for_seconds(routes: Vec<ChannelRoute>, samplerate: u32, channels: u16, seconds: f32) -> Self;
    pub fn write_s16_stereo(&self, pcm: &[u8]) -> usize;   // 返回消费的帧数，尾部半帧忽略
    pub fn ring(&self, id: &str) -> Option<Arc<RingBuffer>>;
    pub fn routes(&self) -> Vec<ChannelRoute>;             // 也有 ids() / rings() / frames() / close()
}

// 常用组合
pub fn stereo_routes(left_id: &str, right_id: &str) -> Vec<ChannelRoute>;
pub fn mixdown_routes<I: IntoIterator<Item = S>, S: Into<String>>(ids: I) -> Vec<ChannelRoute>;
```

每条路由一个独立 `RingBuffer`，互不阻塞：客户端 A 卡住不会影响 B。

### WAV 头（`wav`）与格式转换（`convert`）

```rust
pub const FAKE_DATA_SIZE: u32 = 0x7FFF_FF00;
pub const WAV_HEADER_LEN: usize = 44;

pub fn build_wav_header(channels: u16, rate: u32, bits: u16, data_size: u32) -> [u8; 44];
pub fn build_stream_wav_header(channels: u16, rate: u32) -> [u8; 44];   // 16bit + FAKE_DATA_SIZE
pub fn write_wav(path: &Path, channels: u16, rate: u32, bits: u16, pcm: &[u8]) -> io::Result<()>;

// convert：格式转换 / 重采样 / 频率估计（内部用，也对外）
pub enum SampleFormat { S16, S24, S32, F32 }
pub fn f32_to_s16(x: f32) -> i16;                  // 夹紧 + 四舍五入：0.5 → 16384
pub fn s16_to_f32(x: i16) -> f32;
pub fn remap_channels(input: &[f32], from: u16, to: u16) -> Vec<f32>;
pub struct StreamResampler { /* 线性插值，跨 chunk 保持相位 */ }
pub fn estimate_frequency(samples: &[f32], samplerate: u32) -> Option<f64>;
pub fn build_sine_s16(freq_hz: f64, seconds: f32, amplitude: f32, format: PcmFormat) -> Vec<u8>;
```

## 3. 格式协商（实测行为）

`AudioCapture::new()` 打开共享模式流时按这个顺序来：

1. 读端点 **mix format**（本机默认端点 = 48000 Hz / 2 ch / 32-bit float，extensible）。
2. 请求我们想要的 44100/2ch/16bit，带 `AUTOCONVERTPCM | SRC_DEFAULT_QUALITY` → Windows 音频引擎内部替我们转采样/转格式，`ConversionPath::WasapiAutoConvert`。
3. 引擎不接受就退到 `GetMixFormat()` + 手工转换（`ConversionPath::Manual`：`SampleFormat::decode_into` → `remap_channels` → `StreamResampler` → 编码成 s16le）。
4. 请求的格式恰好就是混音格式 → `Passthrough`。

**实测确认输出真的是 44100/2ch/s16**（不是「打开了就算」）：`loopback --seconds 5` 采到 1114724 字节 = 278681 帧 = 6.319 秒 @44100，正弦主频 1000.0 Hz（若采样率标错，频率会明显偏）。

> 注意：`AUTOCONVERTPCM` 只影响**我们这条流**，不改系统设置、不改默认设备、不需要管理员权限。

## 4. 环形缓冲 / 分发语义

* 采集线程只做一件事：`read_vec` → `ring.write()`。写不进任何客户端的东西，也不等待任何客户端。
* HTTP 每个连接：先写 44 字节 WAV 头，然后 `pos = ring.cursor()`，循环 `read_from(pos, 8192, 200ms)` 写响应体，直到连接断开 / `closed`。
* 客户端断线、卡住、来晚了，都只影响它自己（最多丢帧），采集不受影响 —— 这是 Python 版验证过的模型，原样保留。

## 5. 真机实测记录（本机 Windows，2026-10-17）

环境：Windows NT 10.0.26100（Windows 11 26200）；默认 render 端点
**耳机 (Chu2 DSP)**，id `{0.0.0.00000000}.{de8c89d1-43d8-4aaf-85bd-8d8c5864725a}`，
混音格式 **48000 Hz / 2 ch / 32-bit float**（tag=0xFFFE extensible F32），协商为 `wasapi-autoconvert` → 44100/2ch/16bit。

### 测试 A：本进程播放 + 回环采集（1kHz @0.4 FS，5 秒）

```
采集：耳机 (Chu2 DSP) ← loopback [wasapi-autoconvert] 混音格式 48000 Hz / 2 ch / 32-bit float → 输出 44100 Hz / 2 ch / 16-bit PCM
采样：1114724 字节（6.319 秒，278681 帧）
左声道：peak=13108 (0.4000 FS) 整段 RMS=8243.95 (0.2516 FS)
有声区间：4.943 秒 / 218000 帧（阈值 = peak/64）
        左声道 RMS=9320.95 (0.2845 FS)；正弦理论 RMS=9268.76（peak/√2）；实测/理论=1.0056
        期望 peak（--amp 0.400 × 32767）= 13107，实测 = 13108（比值 1.0001）
主频：1000.0 Hz
声道关系：左右最大差 = 3 LSB
结论：采集到真实音频 ✅   （退出码 0）
```

### 测试 B：**另一个进程**播放 + 本程序回环采集（跨进程，1kHz @0.5 FS，2 秒）

播放端是独立的 Python 进程（`soundcard` WASAPI，`evidence/play_sine.py`），采集端是 `loopback.exe --no-play --seconds 8`（自己不出声）：

```
播放进程：耳机 (Chu2 DSP) | 1000.0Hz 2.0s amp=0.5 @48000Hz
采样：416180 字节（2.359 秒，104045 帧）
左声道：peak=16385 (0.5000 FS) 整段 RMS=10559.53 (0.3223 FS)
右声道：peak=16385 (0.5000 FS) 整段 RMS=10559.53 (0.3223 FS)
有声区间：1.938 秒 / 85456 帧；左声道 RMS=11651.54 (0.3556 FS)
        正弦理论 RMS=11585.94（peak/√2）；实测/理论=1.0057
主频：1000.0 Hz
声道关系：左右最大差 = 3 LSB
结论：采集到真实音频 ✅   （退出码 0）
```

这证明回环抓到的**确实是系统正在播的声音**（来自其它进程），而不是本程序自己灌进去的数据。

### 测试 C：静音对照

`--no-play --expect-silence`：本机没有程序出声时，回环端点**一个数据包都不投递**（空读若干次，采到 0 字节）。
所以「peak=0」有两种形态：0 字节（没东西在播）和 N 秒全零 PCM（数字静音），两者都算对照通过、都不算成功采集。

### 证据文件（`crates/miplay-audio/evidence/`）

| 文件 | 说明 |
| --- | --- |
| `loopback_capture.wav` | 测试 A 的原始采集（1114724 字节 PCM，44100/2ch/s16） |
| `loopback_capture.log` | 测试 A 完整 stdout |
| `crossprocess_capture.wav` | 测试 B 的原始采集（416180 字节 PCM） |
| `crossprocess_capture.log` | 测试 B 完整 stdout（含 Python 播放端输出） |
| `endpoints.log` | `endpoints.exe` 输出的本机 11 个活动端点 + 默认端点 |
| `play_sine.py` | 跨进程测试用的播放脚本（需要 `soundcard`+`numpy` 的 Python） |

### 端点清单（`endpoints.exe` 摘要）

11 个活动端点：7 render / 4 capture。render 有 CG345UK(NVIDIA)、X41(NVIDIA)、扬声器(Steam Streaming Speakers)、扬声器(Steam Streaming Microphone)、扬声器(UU远程虚拟音频设备)、**耳机(Chu2 DSP)=默认**、扬声器(Realtek HD Audio)；capture 有麦克风(Chu2 DSP)=默认、麦克风(Realtek)、麦克风阵列(UU远程)、麦克风(Steam Streaming Microphone)。全部 render 端点混音格式都是 48000/2ch/32f。

## 6. 单元测试

`cargo test -p miplay-audio`：**52 个单测 + 1 个真机集成测试 + 2 个 doctest，全绿**（`--lib` 部分不需要声卡，任何机器都能跑）。

覆盖点：`RingBuffer` 绕回 / 跳帧 / 超时 / 单次超容量写 / `close()` 唤醒阻塞读者、`build_wav_header` 44 字节逐字段正确性（并与 Python `struct.pack` 的结果逐字节比对）、假长度头 `0x7FFFFF00`、声道映射（Left/Right 复制、Mixdown 平均、Stereo 原样、gain、尾部半帧）、格式转换（S16/S24/S32/F32 编解码往返与饱和）、重采样（440Hz 48k→44.1k 保频保幅、22.05k→44.1k 升采样、**chunk 不变性**：分块喂与整块喂结果一致）、`estimate_frequency`、`build_sine_s16`、端点解析与配置校验。

真机集成测试 `tests/loopback_smoke.rs`：默认端点回环 + 播 1kHz → 断言行数、peak>0、peak 接近 0.4 FS、主频 1kHz、左右差 ≤8 LSB。没声卡/没播放设备的机器上会自动 **skip**（打印「跳过」），不会误报失败。

## 7. 怎么接到 HTTP 服务（`miplay-server`）

```rust
// 全局一份：
let mut capture = AudioCapture::new(CaptureConfig::default())?;
capture.start()?;
let router = Arc::new(ChannelRouter::for_seconds(stereo_routes("left", "right"), 44_100, 2, 5.0));

// 采集泵（std 线程即可）：
let cap = capture.ring();
let rt = router.clone();
std::thread::spawn(move || loop {
    let chunk = cap.read_from(cap.cursor(), 8192, Duration::from_millis(200)).unwrap();
    if !chunk.data.is_empty() { rt.write_s16_stereo(&chunk.data); }
});

// axum handler（GET /stream/left.wav，HTTP/1.0 或 chunked 都行）：
async fn left() -> Response {
    let ring = router.ring("left").unwrap();
    let stream = futures::stream::unfold((ring, ring.cursor()), |(ring, pos)| async move {
        let out = tokio::task::spawn_blocking(move || ring.read_from(pos, 8192, Duration::from_millis(200)))
            .await.unwrap().unwrap();
        if out.closed { return None; }
        let next = out.next;
        Some((Ok::<_, std::io::Error>(Bytes::from(out.data)), (ring, next)))
    });
    let mut resp = Body::from_stream(stream).into_response();
    resp.headers_mut().insert(CONTENT_TYPE, "audio/wav".parse().unwrap());
    // 别忘了先写 44 字节 build_stream_wav_header(44100, 2)
    resp
}
```

关键三条：① 先写 44 字节假长度头再写 PCM；② 每个连接从 `ring.cursor()`（当前时刻）开始；③ 用阻塞读 + `spawn_blocking`，别在 async 线程里 sleep。

## 8. 已知限制 / 未验证的点

**未验证（诚实清单）**

* **只在本机一个端点（耳机 Chu2 DSP，48000/2ch/32f）上实测过**。其它 10 个端点只列了混音格式，没逐个开流验证。
* 多声道（>2ch）端点、SPDIF、独占模式、蓝牙端点：未验证（采集端会把 >2ch 降到前 2 路）。
* Windows AC 电源状态切换、设备热插拔后的**自动重连**：没做（检测到 `DEVICE_INVALIDATED` 只停线程 + 记 `last_error`）。
* 长时间（小时级）稳定性、ring 溢出行为在真实客户端下的表现：只做了秒级测试。
* `Capture` 模式（采 capture 端点）代码路径已实现，但没接虚拟声卡实测过。
* 多端点并发采集（多个 `AudioCapture` 同时开）：未实测。
* 非 Windows 平台只编译纯逻辑模块（`capture`/`playback` 是 `#[cfg(windows)]`）。

**已知限制 / 注意点**

* 手工重采样是线性插值（正常路径走 `AUTOCONVERTPCM`，用不到它；手动路径音质一般）。
* 共享模式 `AUTOCONVERTPCM` 会引入极小的左右差异（实测 48k→44.1k 后 **≤3 LSB ≈ 0.02% FS**），不是声道串扰：本程序播的是左右相同的正弦。
* 用 `[console]::beep` / winmm `SoundPlayer` 做测试音时，本机实测**回环采不到**（走的是「声音映射器」而非 WASAPI 默认端点，原因未深究）；所以测试音请用本 crate 的 `play_sine`（真 WASAPI）或 Python `soundcard`。
* `playback` 模块（`play_sine` / `play_pcm_s16`）是**测试辅助**，只用来出声验证采集，不是给生产用的播放器。
* 采集会短暂占用端点（共享模式，不独占），用完 `stop()` 或 `Drop` 释放。

## 9. 复现命令速查

```powershell
cd miplay-bridge
cargo test -p miplay-audio                       # 52 单测 + 集成 + doctest
cargo run -p miplay-audio --bin endpoints        # 端点清单
cargo run -p miplay-audio --bin loopback -- --freq 1000 --seconds 5 --amp 0.4 --out crates/miplay-audio/evidence/loopback_capture.wav
# 跨进程（另开一个终端跑播放端）
cargo run -p miplay-audio --bin loopback -- --no-play --seconds 8 --out crates/miplay-audio/evidence/crossprocess_capture.wav
python crates/miplay-audio/evidence/play_sine.py 1000 2 0.5 48000
```
