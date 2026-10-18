# stereo-audio

StereoLink（立体声链）的音频引擎库：把 **Windows 正在播放的系统声音**从 render 端点回环采成
PCM，按声道拆开，再实时推给本机任意多台播放设备（蓝牙音箱、USB 声卡、耳机……）。

```text
播放器 → 虚拟声卡端点（Windows 默认输出）
           │ WASAPI loopback 采集，统一成 44100 Hz / 2 ch / 16 bit 交错 PCM
           ▼
       AudioCapture（1 条采集线程）
           │ 每 10 ms 一块，按每条路由的「左 / 右 / 混音」把数据拆开
           ▼
       RouteEngine（每条路由 1 条播放线程 + 抖动队列，互不阻塞）
           ├─ 左声道 → 播放端点 A
           ├─ 右声道 → 播放端点 B
           └─ 双声道混音 → 播放端点 C（可选，任意多路）
```

本 crate 只负责「采到干净 PCM + 按声道路由 + 写进播放端点」，不提供 GUI、不联网、不改系统
默认输出（那是 `stereo-link` 面板的事）。

## 能力

- **WASAPI 回环采集**：共享模式；优先用 `AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM` 让系统重采样，
  失败则回退到 Rust 侧重采样 + 声道重映射（`ConversionPath` 会记录实际走通的路径）。
- **端点枚举**：`list_endpoints()` / `default_endpoint()` / `resolve_endpoint()`，支持按友好名
  子串或端点 id 选择（`DeviceSelector`），可列出混音格式与错误。
- **声道路由引擎**（`route`，仅 Windows）：任意多路，每路可选 `Left` / `Right` / `Both`
  （`Both` = `(L+R)/2`），独立增益与额外延迟，每条路由独立播放线程 + 抖动队列；某条路由失败
  只影响它自己。
- **固定两路引擎**（`btsplit`，仅 Windows）：早期的「左一台 / 右一台」实现，保留兼容。
- **播放**：`play_pcm_s16` 把 PCM 播到指定端点（面板的左右自检音）；`play_sine` 播正弦。
- **纯逻辑工具**：`convert`（f32↔s16、声道重映射、重采样、正弦、频率估计）、`ring`（有界环形
  缓冲，可多读者、慢读者丢旧数据）。
- **错误类型**：`AudioError`，每个 variant 带可直接展示中文说明。

## 平台

`convert` / `endpoint` / `error` / `ring` 是纯逻辑，任何平台都能编译和跑单测；
`capture` / `playback` / `route` / `btsplit` / `com` 只在 Windows 上编译。crate 开启
`#![deny(missing_docs)]`，公开 API 都有文档。

## 快速开始

```powershell
# 列出本机音频端点：名字 / id / 默认标记 / 混音格式
cargo run -p stereo-audio --bin endpoints

# 单测（--lib 部分不需要声卡，任何机器都能跑）
cargo test -p stereo-audio --lib
```

路由引擎真机自检（需要本机已有可用的播放端点）：

```powershell
# 9 秒内置测试音：0–3 s 左 440 Hz、3–6 s 右 880 Hz、6–9 s 双声道
# 测试音直接播到采集源端点，回环采下来后按路由分别推给各设备
cargo run -p stereo-audio --example route_demo -- --left "音箱A" --right "音箱B"
cargo run -p stereo-audio --example route_demo -- --left "音箱A" --right "音箱B" --both "USB 耳机"

# 不自动播测试音，只启动引擎等外部播放器出声（跑 15 秒后自动停）
cargo run -p stereo-audio --example route_demo -- --left "音箱A" --right "音箱B" --manual

# 旧固定两路引擎的演示（12 秒测试音：左声像 / 右声像 / 居中）
cargo run -p stereo-audio --example btsplit_demo -- "音箱A" "音箱B"
```

`route_demo` 的参数：`--left` / `--right`（至少一个）、`--both`（可选）、`--source`（回环采集
源，默认「立体声链设备」；设备名被改过的机器请传 `--source "<你的虚拟声卡名>"`）、
`--tone-to`（测试音播到哪，默认 = `--source`，传 `none` 关闭）、`--manual`（只启动引擎）、
`--wait`（`--manual` 等待秒数）。结束自动停引擎，不改系统默认输出。

## 最小用法

```rust,no_run
use stereo_audio::{RouteChannel, RouteConfig, RouteEngineConfig, RouteEngineHandle};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = RouteEngineConfig {
        source_endpoint: "立体声链设备".into(),
        routes: vec![
            RouteConfig::new("音箱 A", RouteChannel::Left),
            RouteConfig::new("音箱 B", RouteChannel::Right),
        ],
        ..Default::default()
    };
    config.validate()?;

    let mut engine = RouteEngineHandle::start(config)?; // 采集 + 每路播放线程
    std::thread::sleep(std::time::Duration::from_secs(10));
    println!("{}", engine.stats().describe());          // 每路电平 / 发送量 / 欠载
    engine.stop();
    Ok(())
}
```

只采集、不要路由时：

```rust,no_run
use std::time::Duration;
use stereo_audio::{AudioCapture, CaptureConfig};

let mut capture = AudioCapture::new(
    CaptureConfig::loopback_default().with_device_name("立体声链设备"),
)?;
capture.start()?;
// 读 10 ms（44.1k/2ch/s16le = 1764 字节），超时 500 ms
let pcm = capture.read_vec(1_764, Duration::from_millis(500))?;
```

## 模块

| 文件 | 作用 | 平台 |
| --- | --- | --- |
| `src/capture.rs` | WASAPI 回环 / 采集端点采集，输出 44.1k/2ch/s16le，写环形缓冲 | Windows |
| `src/route.rs` | 声道路由引擎：任意多路、左/右/混音、增益、延迟、统计 | Windows |
| `src/btsplit.rs` | 固定左/右两路引擎（旧实现，保留兼容） | Windows |
| `src/playback.rs` | 往指定 render 端点播放 PCM / 正弦测试音 | Windows |
| `src/endpoint.rs` | 端点/格式数据结构、`DeviceSelector`、混音格式解析 | 全平台 |
| `src/convert.rs` | PCM 转换：f32↔s16、声道重映射、重采样、正弦、频率估计 | 全平台 |
| `src/ring.rs` | 有界环形缓冲（`RingBuffer` / `ReadOutcome`） | 全平台 |
| `src/error.rs` | `AudioError` / `Result` | 全平台 |
| `src/com.rs` | COM 初始化守卫、设备属性读取（内部模块） | Windows |
| `src/bin/endpoints.rs` | 列出本机端点的小工具 | Windows |
| `examples/route_demo.rs` | 多设备路由真机自检 | Windows |
| `examples/btsplit_demo.rs` | 旧两路引擎真机自检 | Windows |
| `evidence/play_sine.py` | 外部播放正弦（手工验证采集用） | — |

## 设计要点

- **统一格式**：采集对外固定 44100 Hz / 2 ch / 16 bit 交错 s16le。WASAPI 混音格式不一致时
  优先让系统 `AUTOCONVERTPCM`，不行再在 Rust 侧转换，`NegotiatedFormat` 记录真实路径。
- **路由隔离**：1 条采集线程 + 每条路由 1 条播放线程；每路有自己的 `VecDeque<i16>` 抖动队列。
  队列空时补静音（计欠载），队列满时丢最旧数据（计 `dropped_ms`）；两条链路时钟互相独立，
  靠丢帧/补静音吸收漂移。
- **失败隔离**：某条路由打开失败只写进该路 `error`，引擎继续 running；只有全部路由失败时
  `RouteEngineHandle::start` 返回错误、运行中全部失败才停引擎。采集失败写 `source_error`。
- **延迟可调**：`queue_ms`（预排队抗抖动）与 `render_buffer_ms`（WASAPI 共享缓冲）决定软件侧
  延迟；`RouteConfig::delay_ms` 是启动时先输出静音，用来对齐两台设备。
- **增益两级**：`RouteEngineConfig::capture_gain` × `RouteConfig::gain`，范围 0–8。

## 测试

```powershell
cargo test -p stereo-audio --lib   # 纯逻辑：PCM 转换、端点解析、环形缓冲、路由配置校验
cargo test -p stereo-audio         # 加上 Windows 真机路径（默认端点/回环，不需要外接设备）
```

`--lib` 测试在任何平台都能跑；带 `cfg(windows)` 的测试在真机（有默认端点）上跑。真机自检脚本
`route_demo` / `btsplit_demo` 见上文，`evidence/play_sine.py` 用于从外部往某个端点播正弦，
配合观察回环采集是否拿到预期数据。

## 与 GUI 的关系

`stereo-link`（桌面面板）通过本 crate 的 `RouteEngineHandle` 启动路由、用 `play_pcm_s16` 播
左右自检音、用 `list_endpoints` 填设备列表；它自己负责配置持久化、虚拟声卡识别、切换系统默认
输出（`crates/stereo-link/src/audio_default.rs`）。本 crate 不依赖 GUI、不读配置文件。
