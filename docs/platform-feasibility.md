# StereoLink **主程序**在 macOS / Linux 上「直接部署运行」的可行性评估

> 评估对象：主程序（`crates/stereo-link` + `crates/stereo-audio`），**不含** `receiver/` 子项目、不含 `drivers/` 驱动本身。
> 评估性质：**只读分析 + 本文档**。未执行 `cargo build`、未修改 `crates/`、`drivers/`、`scripts/`、`installer/` 下任何文件。
> 与原问题对齐：回答「能不能直接部署运行」「要动多少」「哪条路最省」。

---

## 基线声明（先读，否则行号会漂）

- 本文所有行号 / 行数取 **工作区快照**：`HEAD = bcca6a1`（`chore(release): 0.5.5`）**+ 未提交改动**，取样时间 2026-10-21 22:50（Asia/Shanghai）。
- 取样时 `git status` 显示工作区**被并发修改**，以下文件是未跟踪的新增（本文按「已在树上」对待，但请知悉它们可能还在变）：
  `crates/stereo-audio/src/balance.rs`、`crates/stereo-link/src/control.rs`、`crates/stereo-link/src/receiver.rs`、`receiver/`。
- 因此本文数字**只对这个快照可复现**。复测命令见 §10。
- 与 `docs/android-port/02-codebase-port.md` 的关系：那份文档审计基线是 `0f6f6db`，行数/百分比**已经明显漂移**（例如它记 `app.rs` = 3264 行，本快照 = **4984 行**）。本文所有数字重新实测，不复用它的表；结论层面的一致性在 §4 末尾单独说明。

### 证据等级

| 标记 | 含义 |
| --- | --- |
| `[源码]` | 仓库内源码行，均带仓库相对路径 + 行号 |
| `[实测]` | 本次实际执行的命令输出 |
| `[静态]` | 由源码结构推断的编译期行为（**本次未编译验证**，任务禁止 build） |
| `[估算]` | 工作量估计，无实测支撑，仅供排期参考 |
| `[未验证]` | 本次未能复核（含联网复核失败），不作结论 |

> 本次 `web_search` 调用返回 `HTTP 401 Authentication Fails`，**所有平台 API 的外部资料均未能联网复核**。涉及具体 API 名 / 许可 / 路径的外部事实一律标 `[未验证]`，落地前请自行确认。

---

## 0. 一页速查

**一句话结论：这是一次「界面能搬、引擎要重写」的移植，不是配置问题。**

- 引擎 `crates/stereo-audio` 共 **13400 行**，其中 **11932 行（89.0%）在 `#[cfg(windows)]` 模块里**，且这些模块不是「加个 trait 就行」的薄封装——`route.rs`（3342 行）与 `sync.rs`（4269 行）把 WASAPI 的句柄生命周期、`GetMixFormat` 所有权、`AUDCLNT_*` 错误码、环形缓冲节奏全揉在一起。`[实测]`+`[源码]`
- 真正平台无关的只有 **1410 行（10.5%）**：`convert.rs`（555）、`ring.rs`（342）、`endpoint.rs`（311）、`error.rs`（66）、`lib.rs`（136，门控表）。
- 界面层 `crates/stereo-link` 共 **10473 行**，eframe/egui **本身跨平台**，而且 `crates/stereo-link/Cargo.toml:16-21` 已经把 `wayland` / `x11` feature 打开——GUI 框架这一层**没有障碍**。
- 但主程序**不是「GUI + 引擎」两层**，而是「GUI 直接调 Windows-only 引擎 API」：`crates/stereo-link/src/app.rs` **22 处**、`config.rs` **19 处**、`audio_default.rs` **12 处**、`control.rs` **4 处**、`endpoint_poll.rs` **3 处** 的 `stereo_audio::` 引用 `[实测]`，指向的符号全部被 `crates/stereo-audio/src/lib.rs:57-136` 的 `#[cfg(windows)]` 门控。**因此 `stereo-link` 在非 Windows 上必然编译失败** `[静态]`。
- Linux：**可行**。虚拟声卡这个 Windows 上最痛的部分（WDK 内核驱动）在 Linux 上是**一条 `pactl` 命令**；回环采集有 monitor source 原生支持。估算 **17–28 人日 ≈ 3.5–5.5 周**（1 主力 + 0.5 测试）。`[估算]`
- macOS：**勉强（可行但有硬前置）**。CoreAudio 没有公开的系统回环，必须依赖第三方虚拟设备（BlackHole / Loopback），且分发要 Developer ID + 公证。估算 **20–34 人日 ≈ 4–7 周**（走第三方虚拟设备路线 A）。`[估算]`

---

## 1. 结论表

### 1.1 主表（Windows / macOS / Linux × 四个维度）

图例：✅ 现成可用 ｜ ⚠️ 要改/有前置条件 ｜ ❌ 不可行，需重写

| 维度 | Windows（今天） | Linux | macOS |
| --- | --- | --- | --- |
| **主程序 GUI**（eframe/egui） | ✅ 完整 | ⚠️ **能跑**，需改 4 处（托盘、字体、配置路径、系统设置跳转）+ 打包/CI。eframe 的 `wayland`/`x11` feature 已开（`crates/stereo-link/Cargo.toml:16-21`） | ⚠️ **能跑**，同上；另需 Developer ID 签名 + 公证才能分发 |
| **引擎**（采集→拆声道→多设备输出） | ✅ 完整 | ❌ **要重写后端**：89% 的行是 WASAPI。纯逻辑（重采样/环形缓冲/数据模型）可留 | ❌ **要重写后端**，且采集路线本身受限（见下） |
| **虚拟声卡**（"不会被听见"的 render 端点） | ✅ 自研 WDK 驱动（可选高级路径）+ 第三方兜底（UU远程 / Steam Streaming） | ✅ **免费且简单**：`module-null-sink` / PipeWire null node，**无需内核驱动** | ⚠️ **无法自建到可分发**（要写 HAL AudioServerPlugIn，C++，重型）；实际做法＝**要求用户装 BlackHole** |
| **回环采集**（抓系统混音） | ✅ WASAPI loopback（`AUDCLNT_STREAMFLAGS_LOOPBACK`） | ✅ **原生支持**：`module-null-sink` 生成的 `*.monitor` source，或 PipeWire monitor port | ❌ **没有公开等价物**。三条替代路线：① 第三方虚拟设备（推荐）② Core Audio Process Tap（macOS 14.2+，需 TCC 授权）③ ScreenCaptureKit |
| **默认输出切换** | ✅ `IPolicyConfig::SetDefaultEndpoint`（未公开 COM，脚本形式） | ✅ **更简单**：`pactl set-default-sink` / `wpctl set-default` | ⚠️ CoreAudio `kAudioHardwarePropertyDefaultOutputDevice`，可行 |
| **整体判定** | 唯一成熟平台 | **可行**（17–28 人日 ≈ 3.5–5.5 周） | **勉强 / 有条件可行**（20–34 人日 ≈ 4–7 周） |

### 1.2 三句话总结

1. **引擎层 = 100% WASAPI 绑定**（就"设备 I/O"这件事而言），`crates/stereo-audio/src/lib.rs:57-74` 一行不改就只能在 Windows 上编译。
2. **界面层 = 跨平台**，eframe/egui 的 Linux/macOS 后端是成熟路径，代价集中在 4 个"集成点"和打包签名。
3. **真正的分水岭不是操作系统，是"系统混音能不能被抓到"**：Linux 有官方答案（monitor source），macOS 没有（必须借第三方设备或新 API）。

---

## 2. 现状清点（实测数字）

统计方法：字节级 LF 计数（`PowerShell Get-Content` 会按 ANSI 解码中文，不可用），命令见 §10。

### 2.1 `crates/stereo-audio`（引擎）—— 13400 行

| 文件 | 行数 | 平台门控 | 角色 |
| --- | ---: | --- | --- |
| `crates/stereo-audio/src/balance.rs` | 810 | `#[cfg(windows)]`（`lib.rs:73-74`） | 音量平衡校准（端点曲线 + 声学锚点） |
| `crates/stereo-audio/src/bin/endpoints.rs` | 58 | 无门控，但**调用** Windows-only API | 端点列表 CLI |
| `crates/stereo-audio/src/btsplit.rs` | 749 | `#[cfg(windows)]`（`lib.rs:57-58`） | 蓝牙/多端点分声道直出 |
| `crates/stereo-audio/src/capture.rs` | 871 | `#[cfg(windows)]`（`lib.rs:59-60`） | WASAPI 回环采集 |
| `crates/stereo-audio/src/com.rs` | 409 | `#[cfg(windows)]`（`lib.rs:61-62`） | COM 初始化 / 设备枚举 / 默认端点 |
| `crates/stereo-audio/src/convert.rs` | 555 | **纯逻辑** | 重采样、格式转换、去交织 |
| `crates/stereo-audio/src/endpoint.rs` | 311 | **纯逻辑** | 端点/格式数据模型 |
| `crates/stereo-audio/src/endpoint_format.rs` | 431 | `#[cfg(windows)]`（`lib.rs:63-64`） | 端点设备格式读写 + `IPolicyConfig` |
| `crates/stereo-audio/src/error.rs` | 66 | **纯逻辑**（`From<windows::core::Error>` 单独 gate 在 `error.rs:58-59`） | 错误类型 |
| `crates/stereo-audio/src/format_support.rs` | 438 | `#[cfg(windows)]`（`lib.rs:65-66`） | 按设备能力枚举输出格式 |
| `crates/stereo-audio/src/lib.rs` | 136 | 门控表本身 | 导出面 |
| `crates/stereo-audio/src/playback.rs` | 239 | `#[cfg(windows)]`（`lib.rs:67-68`） | 测试音播放 |
| `crates/stereo-audio/src/ring.rs` | 342 | **纯逻辑** | 抖动队列 / 环形缓冲 |
| `crates/stereo-audio/src/route.rs` | 3342 | `#[cfg(windows)]`（`lib.rs:69-70`） | **路由引擎主干**（采集线程 + N 路 sink 线程） |
| `crates/stereo-audio/src/sync.rs` | 4269 | `#[cfg(windows)]`（`lib.rs:71-72`） | 声学自校验 / 麦克风 / 延迟测量 |
| `crates/stereo-audio/src/volume.rs` | 374 | `#[cfg(windows)]`（`lib.rs:120-126`） | 端点音量读写 |
| **合计** | **13400** | **Windows-only 11932（89.0%）／纯逻辑 1410（10.5%）／其余 58** | |

### 2.2 `crates/stereo-link`（主程序）—— 10473 行

| 文件 | 行数 | 非 Windows 上能否编译 | 备注 |
| --- | ---: | --- | --- |
| `crates/stereo-link/src/app.rs` | 4984 | ❌ | 22 处 `stereo_audio::` 引用，全部指向 Windows-only 符号 |
| `crates/stereo-link/src/audio_default.rs` | 218 | ❌ | 内嵌 PowerShell 脚本 + `stereo_audio::list_endpoints` |
| `crates/stereo-link/src/close_flow.rs` | 263 | ⚠️ | 关闭行为（托盘/最小化），逻辑本身可移植 |
| `crates/stereo-link/src/config.rs` | 778 | ⚠️ | 19 处 `stereo_audio::`（`BalanceCalibration` / `BalanceAnchor` 等）→ 连带失败 |
| `crates/stereo-link/src/control.rs` | 521 | ⚠️ | **新增未接线**；纯 `std::net` TCP + JSON，本身跨平台，只有 `%APPDATA%` 路径是 Windows 的 |
| `crates/stereo-link/src/endpoint_poll.rs` | 445 | ❌ | 轮询线程直接调 `stereo_audio::get_endpoint_format` |
| `crates/stereo-link/src/fonts.rs` | 91 | ⚠️ | 从 `%WINDIR%\Fonts` 找中文字体 |
| `crates/stereo-link/src/icons.rs` | 536 | ✅ | 纯 Painter 手绘 |
| `crates/stereo-link/src/logbuf.rs` | 152 | ✅ | 纯内存日志缓冲 |
| `crates/stereo-link/src/main.rs` | 144 | ⚠️ | `windows_subsystem` 属性 + 12 个 `mod` 全部无条件声明 |
| `crates/stereo-link/src/receiver.rs` | 628 | ⚠️ | `std::process` 拉子进程，仅 2 处 `#[cfg(windows)]` |
| `crates/stereo-link/src/theme.rs` | 888 | ✅ | 设计系统 |
| `crates/stereo-link/src/toast.rs` | 136 | ✅ | 浮层提示 |
| `crates/stereo-link/src/tray.rs` | 689 | ❌ | 整文件 `#![cfg(windows)]`（`tray.rs:22`） |
| **合计** | **10473** | | |

---

## 3. 证据清单：所有 Windows-only 依赖与调用点

### 3.1 依赖声明（Cargo）

| 位置 | 内容 |
| --- | --- |
| `crates/stereo-audio/Cargo.toml:16-26` | `[target.'cfg(windows)'.dependencies]` → `windows = "0.62"`，feature：`Win32_Foundation` / `Win32_Media` / `Win32_Media_Audio` / `Win32_Media_Audio_Endpoints` / `Win32_System_Com` / `Win32_System_Com_StructuredStorage` / `Win32_System_Variant` / `Win32_UI_Shell_PropertiesSystem` |
| `crates/stereo-audio/Cargo.toml:27-29` | `windows-core = "0.62"`（同样在 cfg 表下）——注释说明是 `#[windows::core::interface]` 宏展开需要同版本 `windows_core` |
| `crates/stereo-link/Cargo.toml:33-41` | `[target.'cfg(windows)'.dependencies]` → `windows = "0.62"`，feature：`Win32_Foundation` / `Win32_Graphics_Gdi` / `Win32_System_LibraryLoader` / `Win32_System_Threading` / `Win32_UI_Shell` / `Win32_UI_WindowsAndMessaging`（**全部服务于托盘**） |
| `crates/stereo-link/Cargo.toml:16-21` | `eframe = "0.31.1"`，`default-features = false`，features 含 **`wayland` / `x11`** ← 界面层**已经**为 Linux 打开通路 |

> **重要**：两个 manifest 的 Windows 依赖**都写在 `cfg(windows)` 表里**，所以 `cargo build -p stereo-audio --lib` 在 Linux 上大概率能过（剩下的纯逻辑模块不引用 `windows`）`[静态]`。失败点是 **`bin/endpoints.rs`** 和 **`stereo-link`**。

### 3.2 引擎层 A：设备枚举 / COM 初始化 / 默认端点

文件：`crates/stereo-audio/src/com.rs`

| 行号 | 调用点 | 作用 |
| --- | ---: | --- |
| `com.rs:7-18` | `use windows::core::{IUnknown, GUID, PWSTR}` / `Win32::Foundation::{PROPERTYKEY, S_OK}` / `Win32::Media::Audio::{...}` / `Win32::System::Com::StructuredStorage::{PropVariantClear, PROPVARIANT}` / `Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL}` / `Win32::System::Variant::VT_LPWSTR` | 全部 COM 基础 |
| `com.rs:25` | `RPC_E_CHANGED_MODE = 0x8001_0106` | 线程 COM 单元模式冲突的处理 |
| `com.rs:27-29` | `PKEY_Device_FriendlyName = {a45c254e-df1c-4efd-8020-67d146a850e0}, 14` | 读端点友好名 |
| `com.rs:53-78` | `ComGuard`：`CoInitializeEx(None, COINIT_MULTITHREADED)`（`:60`）+ `CoUninitialize`（`:78`） | RAII COM 生命周期 |
| `com.rs:84-85` | `CoCreateInstance(&MMDeviceEnumerator, ...)` | 拿设备枚举器 |
| `com.rs:96-97` | `en.GetDefaultAudioEndpoint(edataflow(flow), eConsole)` | **默认端点** |
| `com.rs:102-103` | `en.EnumAudioEndpoints(edataflow(flow), DEVICE_STATE_ACTIVE)` | **设备枚举** |
| `com.rs:121` | `CoTaskMemFree` | 释放 COM 分配的字符串 |
| `com.rs:154-156` | `dev.Activate::<IAudioClient>(CLSCTX_ALL, None)` | 激活音频客户端 |
| `com.rs:216-223` | `client.GetMixFormat()` + `CoTaskMemFree` | 读混音格式 |

### 3.3 引擎层 B：回环采集（`IAudioCaptureClient`）

#### `crates/stereo-audio/src/capture.rs`（871 行）

| 行号 | 调用点 |
| --- | ---: |
| `capture.rs:20-25` | `use windows::Win32::Media::Audio::{IAudioCaptureClient, IAudioClient, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_LOOPBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY}` + `Win32::System::Com::CoTaskMemFree` |
| `capture.rs:6-7` | 模块文档自述两条降级路径：A（首选）= `AUTOCONVERTPCM | SRC_DEFAULT_QUALITY`，B = 退回 `GetMixFormat` 原生格式 |
| `capture.rs:33-34` | `AUDCLNT_E_DEVICE_INVALIDATED: i32 = 0x8889_0004` |
| `capture.rs:300-301` | `struct` 字段 `client: IAudioClient`, `capture: IAudioCaptureClient` |
| `capture.rs:326` | `base_flags |= AUDCLNT_STREAMFLAGS_LOOPBACK` ← **回环开关本体** |
| `capture.rs:334-336` | `flags_a` + `client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags_a, hns, 0, &wfx, None)` |
| `capture.rs:344-348` | 路径 B：`client_b.GetMixFormat()` → `Initialize(SHARED, base_flags, hns, 0, mix_ptr, None)` → `CoTaskMemFree` |
| `capture.rs:374-376` | `client.GetService()` → `IAudioCaptureClient`；`client.Start()` |
| `capture.rs:426-427` | `IAudioCaptureClient::GetBuffer`；`flags & AUDCLNT_BUFFERFLAGS_SILENT` |
| `capture.rs:306-307` | `map_err` → `AUDCLNT_E_DEVICE_INVALIDATED` 专判 |

#### `crates/stereo-audio/src/route.rs`（3342 行，引擎主干）

| 行号 | 调用点 |
| --- | ---: |
| `route.rs:53-58` | `IAudioCaptureClient, IAudioRenderClient, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_LOOPBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY` + `CoTaskMemFree` |
| `route.rs:1365-1387` | 错误码分类：`AUDCLNT_E_DEVICE_INVALIDATED = 0x8889_0004`（`:1380`）、`AUDCLNT_E_SERVICE_NOT_RUNNING = 0x8889_0010`（`:1382`）、`E_ACCESSDENIED` 归入可重试（`:1387`） |
| `route.rs:1652-1680` | `GetMixFormat` 所有权注释（`:1652`）+ `client.GetMixFormat()`（`:1658`）+ `Initialize`（`:1666-1675`）+ `CoTaskMemFree`（`:1676`） |
| `route.rs:1791-1834` | render 侧：`AUTOCONVERTPCM | SRC_DEFAULT_QUALITY`（`:1791`）→ `Initialize(SHARED, ...)`（`:1793-1794`）→ `GetService()` → `IAudioRenderClient`（`:1833-1834`） |
| `route.rs:1942` / `route.rs:1960` | `write_silence` / `write_bytes`（都走 `IAudioRenderClient::GetBuffer`） |
| `route.rs:2053-2107` | 采集侧：`Activate` → `IAudioClient`（`:2053`）、**明确不加 `AUTOCONVERTPCM`** 的理由（`:2062-2068`）、`GetMixFormat`（`:2070`）、`Initialize(SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK, ...)`（`:2078-2083`）、`GetService()` → `IAudioCaptureClient`（`:2106`） |
| `route.rs:2160` | `flags & AUDCLNT_BUFFERFLAGS_SILENT` |
| `route.rs:3252` | 测试夹具里直接用 `0x88890008`（`AUDCLNT_E_UNSUPPORTED_FORMAT`） |

#### `crates/stereo-audio/src/sync.rs`（4269 行）

| 行号 | 调用点 |
| --- | ---: |
| `sync.rs:51-65` | 全文件 **36 处** `#[cfg(windows)]`（全仓最多）；`:63-65` 导入 `IAudioCaptureClient` / `AUDCLNT_SHAREMODE_EXCLUSIVE` / `AUDCLNT_S_BUFFER_EMPTY` |
| `sync.rs:1142` | 注释：独占模式只接受 `48000/1/16`，其它组合返回 `AUDCLNT_E_UNSUPPORTED_FORMAT` |
| `sync.rs:1250` | `Initialize(AUDCLNT_SHAREMODE_EXCLUSIVE, 0, hns, 0, &fmt, None)` |
| `sync.rs:1261` | `client.GetService()` → `IAudioCaptureClient` |
| `sync.rs:1277` | `flags & AUDCLNT_BUFFERFLAGS_SILENT` |
| `sync.rs:1290` | `AUDCLNT_S_BUFFER_EMPTY` 判空 |

> **说明**：`sync.rs` 的声学自校验（麦克风互相关测延迟）在模块文档里明确写了「本机实测只有**独占模式**能采到真实声学信号」（`lib.rs:84-87`）。这是一条**与平台强绑定**的经验结论，Linux（PulseAudio/ALSA）与 macOS（CoreAudio）都需要重新实测是否能用共享模式跑通。

### 3.4 引擎层 C：多设备输出（`IAudioRenderClient`）

| 位置 | 调用点 |
| --- | --- |
| `crates/stereo-audio/src/playback.rs:9-13` | `IAudioRenderClient, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, CoTaskMemFree` |
| `crates/stereo-audio/src/playback.rs:91-131` | `Initialize` → `GetService()` → `IAudioRenderClient`（`:119-120`）→ `client.Start()`（`:131`） |
| `crates/stereo-audio/src/playback.rs:149` | `IAudioRenderClient::GetBuffer` |
| `crates/stereo-audio/src/btsplit.rs:14` | 模块文档：每个端点一条独立线程，轮询 `IAudioClient::GetCurrentPadding` |
| `crates/stereo-audio/src/btsplit.rs:29-31` | 同上导入面 |
| `crates/stereo-audio/src/btsplit.rs:452-469` | `Initialize`（`:454-463`）→ `GetService()`（`:468-469`） |
| `crates/stereo-audio/src/btsplit.rs:544` / `:561` | `write_silence` / `write_stereo` |

### 3.5 引擎层 D：端点格式 / 默认设备切换（**未公开 COM**）

文件：`crates/stereo-audio/src/endpoint_format.rs`

| 行号 | 内容 |
| --- | ---: |
| `endpoint_format.rs:1-3` | 模块文档：读写 `PKEY_AudioEngine_DeviceFormat`（`IMMDevice::OpenPropertyStore(STGM_READWRITE)` + `IPropertyStore::SetValue/Commit`） |
| `endpoint_format.rs:149-160` | `#[cfg(windows)] mod win` + 导入 `GUID, HSTRING, PCWSTR` / `PROPERTYKEY` / `IMMDeviceEnumerator, MMDeviceEnumerator, WAVEFORMATEX` / `PROPVARIANT` / `CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL` / `IPropertyStore` |
| `endpoint_format.rs:162-165` | `PKEY_DEVICE_FORMAT` |
| `endpoint_format.rs:170-194` | **手写 `IPolicyConfig` 接口**（`#[windows::core::interface("f8679f50-850a-41cf-9c72-430f290290c8")]`，`:174-175`）；`:173` 注释明确「`SetPropertyValue/SetDefaultEndpoint/SetEndpointVisibility`，用到再补」——**这是 Windows 未公开 API，没有任何跨平台对应物** |
| `endpoint_format.rs:197-198` | `RPC_E_CHANGED_MODE` |
| `endpoint_format.rs:208-221` | 手动 `CoInitializeEx(COINIT_MULTITHREADED)`，失败判 `RPC_E_CHANGED_MODE` |
| `endpoint_format.rs:228` / `:232` | `CoCreateInstance(&MMDeviceEnumerator, ...)` / `device_by_id` |
| `endpoint_format.rs:268-282` | `IPolicyConfig::GetMixFormat`（`:271-272` 创建 policy 对象）+ `CoTaskMemFree`（`:282`） |
| `endpoint_format.rs:288-303` | **注释警告不要退回 `IPropertyStore::SetValue`**（会写出 Windows 不认的格式），必须用 `IPolicyConfig::SetDeviceFormat` |
| `endpoint_format.rs:331-340` | 默认格式设置入口（再次 `CoCreateInstance`） |
| `endpoint_format.rs:355` | `GetDefaultAudioEndpoint(eRender, eConsole)` |
| `endpoint_format.rs:362` | `IPropertyStore` 回读校验 |

### 3.6 引擎层 E：音量 / 格式能力探测

| 位置 | 内容 |
| --- | --- |
| `crates/stereo-audio/src/volume.rs:73-77` | `#[cfg(windows)] mod win` + `use ...::Endpoints::IAudioEndpointVolume` / `IMMDevice` / `CLSCTX_ALL` |
| `crates/stereo-audio/src/volume.rs:117-119` | `dev.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None)` |
| `crates/stereo-audio/src/volume.rs:292-293` | `pub use win::{capture_endpoint_curve, get_endpoint_volume, set_endpoint_mute, set_endpoint_volume}` |
| `crates/stereo-audio/src/volume.rs:295-302` | **唯一的「非 Windows 占位」先例**：`#[cfg(not(windows))] pub use non_windows::capture_endpoint_curve;` + 同签名占位模块。**但 `lib.rs:120-121` 把整个 `volume` 模块 gate 掉，这段占位现在是死代码** |
| `crates/stereo-audio/src/format_support.rs:1-11` | 模块文档：`IAudioClient::IsFormatSupported` 仅 Windows |
| `crates/stereo-audio/src/format_support.rs:30-35` | `IAudioClient, IMMDeviceEnumerator, AUDCLNT_SHAREMODE(_EXCLUSIVE/_SHARED), WAVEFORMATEX` + `CoTaskMemFree` |
| `crates/stereo-audio/src/format_support.rs:139-155` | `probe()` + `IsFormatSupported`（`:147`）+ `CoTaskMemFree`（`:155`） |
| `crates/stereo-audio/src/format_support.rs:176-177` | `with_client()`（COM 已初始化 + 端点已激活） |
| `crates/stereo-audio/src/format_support.rs:266-279` | `GetMixFormat`（`:273`）+ `CoTaskMemFree`（`:279`） |

### 3.7 界面/集成层：托盘（`Shell_NotifyIcon`）

文件：`crates/stereo-link/src/tray.rs`（689 行，**整文件 Windows-only**）

| 行号 | 内容 |
| --- | ---: |
| `tray.rs:22` | `#![cfg(windows)]` |
| `tray.rs:1-20` | 模块文档：自建隐藏窗口 + 独立消息泵线程 + `Shell_NotifyIconW`，**不使用 `tray-icon` 之类 crate**；`:14-17` 解释必须额外 `PostMessageW(WM_PAINT)` 才能叫醒隐藏的 winit 主循环 |
| `tray.rs:33-52` | Win32 导入面（`windows::core::{w, BOOL, PCWSTR}`；`Win32::Foundation`；`Win32::Graphics::Gdi`（`CreateDIBSection` 转 `HICON`）；`Win32::System::LibraryLoader::GetModuleHandleW`；`Win32::System::Threading::GetCurrentProcessId`；`Win32::UI::Shell::{Shell_NotifyIconW, NOTIFYICONDATAW, NIM_*}`；`Win32::UI::WindowsAndMessaging::{RegisterClassW, CreateWindowExW, GetMessageW, TrackPopupMenu, ...}`） |
| `tray.rs:93` | `hicon: HICON` |
| `tray.rs:116-117` | `WINIT_WINDOW_CLASS = "Window Class"` ← **依赖 winit 在 Windows 上的内部类名**，是移植时最脆的一处 |
| `tray.rs:270` / `:296` / `:587` | `PostMessageW` |
| `tray.rs:330` | 「真正的 Win32 初始化 + 消息循环」 |
| `tray.rs:544` | `PostQuitMessage(0)` |
| `tray.rs:613-645` | `CreateDIBSection` + `CreateIconIndirect`（`BI_RGB`，`:645`）把 RGBA 转 `HICON` |

### 3.8 界面/集成层：默认输出切换（内嵌 PowerShell）

文件：`crates/stereo-link/src/audio_default.rs`

| 行号 | 内容 |
| --- | ---: |
| `audio_default.rs:1-9` | 模块文档：内嵌 `scripts/switch-audio-default.ps1`，运行时写临时目录再调用；**故意不引入 COM 绑定** |
| `audio_default.rs:15` | `const SWITCH_SCRIPT: &str = include_str!("../../../scripts/switch-audio-default.ps1");` ← **打包进二进制的 PowerShell 代码** |
| `audio_default.rs:23-30` | `VAD_NAME_HINTS`：靠**端点名字**识别虚拟声卡（`"立体声链虚拟声卡"` / `"立体声链设备"` / `"立体声链"` / `"StereoLink"` / `"UU远程"` / `"Steam Streaming"`）← 名称匹配逻辑是 Windows 生态知识 |
| `audio_default.rs:33-34` | `#[cfg(windows)] const CREATE_NO_WINDOW: u32 = 0x0800_0000;` |
| `audio_default.rs:145-172` | 写临时脚本到 `std::env::temp_dir()/stereo-link`（`:145`）→ `Command::new("powershell.exe")`（`:155-172`） |
| `audio_default.rs:46-47` | `default_render()` 转发 `stereo_audio::default_endpoint()` |
| `audio_default.rs:58-96` | 从枚举列表里挑虚拟声卡（`pick_virtual` / `virtual_endpoint` / `resolve_virtual_endpoint`） |

### 3.9 界面/集成层：配置路径 / 字体 / 系统设置跳转 / 进程控制

| 位置 | Windows 耦合 |
| --- | --- |
| `crates/stereo-link/src/config.rs:3` | 文档：「存 `%APPDATA%\stereo-link\config.json`（没有 APPDATA 时退到 exe 同目录）」 |
| `crates/stereo-link/src/config.rs:449-459` | `if let Some(base) = std::env::var_os("APPDATA") { base/stereo-link/config.json }`，否则 exe 同目录（`:455-458`） |
| `crates/stereo-link/src/fonts.rs:14-20` | 候选字体全是 Windows 字体：`msyh.ttc`（微软雅黑）/ `msyh.ttf` / `simhei.ttf`（黑体）/ `Deng.ttf`（等线）/ `simsun.ttc`（宋体） |
| `crates/stereo-link/src/fonts.rs:23-26` | `std::env::var("WINDIR").unwrap_or_else(\|_\| "C:\\Windows")` + `Fonts` ← **硬编码 `C:\Windows`** |
| `crates/stereo-link/src/fonts.rs:54` | `STEREOLINK_FONT` 环境变量可覆盖 → **这是移植时的逃生门** |
| `crates/stereo-link/src/app.rs:2869-2871` | `Command::new("cmd").args(["/c", "start", "ms-settings:sound"])` ← 「系统声音设置」按钮 |
| `crates/stereo-link/src/main.rs:8` | `#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]` ← 正式版隐藏控制台（**注意：`cfg_attr` 只看 `debug_assertions`，不看 `target_os`**） |
| `crates/stereo-link/src/main.rs:10-30` | 12 个 `mod` 全部**无条件**声明（含 `tray`）→ `tray.rs` 靠自己的 `#![cfg(windows)]` 兜住 |
| `crates/stereo-link/src/app.rs:60` | `fn set_dark_mode(dark: bool)`：**本身跨平台**（egui Visuals）；`:71` 注释说明 Windows 11 上 winit 会翻成 `DwmSetWindowAttribute` |
| `crates/stereo-link/src/receiver.rs:18-19` / `:33-34` | `#[cfg(windows)] use std::os::windows::process::CommandExt;` + `CREATE_NO_WINDOW = 0x0800_0000` |
| `crates/stereo-link/src/receiver.rs:68-69` | 子进程名固定为 `airplay-recv.exe` / `miplay-recv.exe` |
| `crates/stereo-link/src/receiver.rs:284` / `:290` | `Command::new(&exe)` + `#[cfg(windows)]` 设置 creation flags |
| `crates/stereo-link/src/receiver.rs:30` | 提示语写死 `cargo build --release -p airplay-recv -p miplay-recv` |
| `crates/stereo-link/src/control.rs:32-48` | 控制面文件路径：`%APPDATA%\stereo-link\control.json`（`:33-36`）/ `%APPDATA%\stereo-link\logs\stereo-link.log`（`:41-47`） |
| `crates/stereo-link/src/endpoint_poll.rs:24` | `use stereo_audio::{...}` → 整个轮询线程依赖 Windows-only API |

### 3.10 部署层：虚拟声卡驱动 / 安装包 / 脚本 / CI

| 位置 | 内容 | 平台含义 |
| --- | --- | --- |
| `drivers/pkg-stereolink/StereoLinkVad.inf` / `StereoLinkVad.sys` / `stereolinkvad.cat` | WDK/WDM **内核驱动**（Scream / MSVAD 血统） | 内核态，Linux/macOS 完全无对应形态 |
| `drivers/scream-derived/*.cpp` / `*.vcxproj` | 驱动源码（`adapter.cpp` / `hw.cpp` / `minwave.cpp` / `mintopo.cpp` / `toptable.h` …） | MSVC + WDK 工具链 |
| `drivers/install-vad.ps1` / `sign-vad.ps1` / `make_package.ps1` | 安装 / 自签 / 打包 | PowerShell + `signtool` |
| `drivers/REPORT.md:17` | `signtool verify /kp` 失败原文：「Signing Cert does not chain to a Microsoft Root Cert.」 | **自签内核驱动过不了签名策略**，必须开测试签名模式 |
| `drivers/REPORT.md:25` | 「本驱动是 render-only，正好就是回环采集需要的形态」（实证待重启后验证） | 采集源形态说明 |
| `drivers/REPORT.md:253-262` | `bcdedit /set testsigning on` + Secure Boot 必须为 False | 部署门槛（桌面出现「测试模式」水印） |
| `scripts/switch-audio-default.ps1` | `IPolicyConfig::SetDefaultEndpoint` 的 PowerShell 实现 | 被 `audio_default.rs:15` 内嵌 |
| `scripts/rename-endpoint.ps1` + `scripts/backups/*.reg` | 直接改 `HKLM\...\Enum` 注册表给端点改名 | **仅开发工具**，非运行时路径 |
| `scripts/build-release.ps1` / `gui-set-name.ps1` / `make-icon.py` | 发布脚本 | Windows 发布流程 |
| `installer/stereo-link.iss:57` | `ArchitecturesAllowed=x64compatible` | Inno Setup，Windows-only |
| `installer/stereo-link.iss:59` | `MinVersion=10.0.17763`（Win10 1809+） | 同上 |
| `installer/stereo-link.iss:13` | 注释：驱动是可选高级路径（`testsigning` + 重启） | 部署分层 |
| `.github/workflows/ci.yml:21-23` | 唯一 job `windows:`，`runs-on: windows-latest` | **仓库从未在 Linux/macOS 上编译过** |
| `.github/workflows/ci.yml:50-52` | MSRV job 也在 `windows-latest` | 同上 |
| `.github/workflows/ci.yml:47-48` | 注释：内核驱动不在 CI 构建（需 WDK + 自签名 + 测试签名模式） | 同上 |

### 3.11 不存在的耦合（澄清，避免误判）

以下是**常见怀疑但实际不存在**的 Windows 绑定，可以省掉排查：

- **注册表 / 开机自启**：全仓 `crates/stereo-link` 内**没有** `HKEY` / `Run\` / `reg add` 的运行时调用 `[实测]`。`config.rs` 的 `auto_start: bool`（`config.rs:436`）指的是「启动后自动开始路由」，**不是**登录自启。
- **DPI / 缩放**：`crates/stereo-link` 与 `crates/stereo-audio` 内**没有** `SetProcessDpiAwareness` / `pixels_per_point` 的手动设置 `[实测]`，全部交给 winit/eframe 默认处理 → Linux/macOS 由 winit 对应后端负责，**零改动**。
- **全局热键 / 单实例互斥体**：未发现。
- **`stereo-link-cli`**：不存在（`crates/stereo-link/Cargo.toml:10-12` 只有一个 `[[bin]]`）。

---

## 4. 分层结论：引擎必需 vs 界面/集成

按「换平台时要动什么」把代码切成 6 层：

| 层 | 内容 | 平台性 | 移植代价 |
| --- | --- | --- | --- |
| **L0 纯 DSP / 数据模型** | `convert.rs`(555) `ring.rs`(342) `endpoint.rs`(311) `error.rs`(66) `lib.rs`(136) | ✅ 完全跨平台 | **0**（`endpoint.rs:81` / `:217-221` 只有注释提到 WASAPI 与 Windows 显示名格式，无代码依赖） |
| **L1 设备 I/O** | `com.rs`(409) `capture.rs`(871) `playback.rs`(239) `btsplit.rs`(749) `format_support.rs`(438) `volume.rs`(374) | ❌ **100% WASAPI** | **重写**，无一可留 |
| **L2 路由调度** | `route.rs`(3342) | ⚠️ 一半可留 | **抽出后可留约 60%**：抖动队列、重试退避、统计快照是纯逻辑；但采集/渲染线程与 COM 句柄、`AUDCLNT_*` 错误码、`GetMixFormat` 所有权深度交织 |
| **L3 平台策略** | `endpoint_format.rs`(431) `audio_default.rs`(218) | ❌ 最"薄"但最"OS" | **重写**（但 Linux 上只有几十行）；这是 `IPolicyConfig` 这类未公开 API 的集中地 |
| **L4 界面** | `app.rs`(4984) `theme.rs`(888) `icons.rs`(536) `toast.rs`(136) `logbuf.rs`(152) `close_flow.rs`(263) `tray.rs`(689) `fonts.rs`(91) | ✅ 主体跨平台 | **只改 4 个集成点**：托盘(689 行整文件)、字体(91 行)、配置路径(`config.rs:449-459`)、设置跳转(`app.rs:2869`)。**`app.rs` 4984 行本身几乎不用动**——前提是 L1/L2 提供了同名 API |
| **L5 部署** | `drivers/`(内核驱动) `installer/` `.github/workflows/ci.yml` | ❌ Windows-only | Linux **归零**（虚拟 sink 是运行时命令）；macOS **转为外部依赖** + 签名公证 |

### 4.1 关键判断

1. **「引擎必需」的部分恰好是「100% WASAPI」的部分。** L1 + L2 + L3 = 409+871+239+749+438+374+3342+431 = **6853 行**（占引擎 51%），全部要按平台重写。
2. **「界面/集成层」才是 eframe 的地盘，而它基本是免费的。** eframe/egui 在 Linux（X11/Wayland）与 macOS 上都是第一梯队路径，`crates/stereo-link/Cargo.toml:16-21` 甚至已经提前开了 `wayland`/`x11`。
3. **主程序今天编译不过，不是因为 GUI，是因为 GUI 直接吃 Windows-only 引擎符号。** `app.rs` **22 处** + `config.rs` **19 处** + `audio_default.rs` **12 处** + `endpoint_poll.rs` **3 处** + `control.rs` **4 处** 的 `stereo_audio::` 引用 `[实测]`，指向的 `RouteEngineHandle` / `list_endpoints` / `BalanceCalibration` / `supported_formats` / `SPLIT_SAMPLE_RATE` 全部被 `lib.rs:57-136`（19 处 `#[cfg(windows)]`）门控。→ **`cargo build -p stereo-link` 在 Linux 上必然失败** `[静态]`。
4. **与 `docs/android-port/02-codebase-port.md` 的结论同构但角度不同**：那份文档按"行数比例"给出「约 4 成可留 / 关键子系统重写」；本文按"分层"给出「GUI 可留、引擎全换、部署归零」。两者不矛盾——**Android 与桌面 Linux/macOS 的差别在于「平台能力边界」：Linux 有 monitor source 与 null-sink，Android 两者都没有**。这正是 Android 那份文档 §0 第 4 条的核心，也是本任务相对它最重要的增量结论。

---

## 5. Linux 方案与工作量

### 5.1 能力映射表

| Windows 概念 | Linux 等价物 | 难度 |
| --- | --- | --- |
| WASAPI loopback 采集 | **monitor source**：PulseAudio `module-null-sink` 生成的 `<sink>.monitor`；PipeWire 里是 node 的 `monitor` port | 🟢 简单 |
| 虚拟声卡（render 端点、不出声） | `pactl load-module module-null-sink sink_name=stereolink latency_msec=10`（或 PipeWire 的 `null-audio-sink` node） | 🟢 简单（**无需驱动、无需管理员**） |
| `IMMDeviceEnumerator` 枚举 | `pactl list short sinks` / `pactl list sinks`（解析）或 `libpulse-binding` | 🟢 简单 |
| `GetDefaultAudioEndpoint` | `pactl get-default-sink` | 🟢 简单 |
| `IPolicyConfig::SetDefaultEndpoint` | `pactl set-default-sink <name>`（PipeWire: `wpctl set-default <id>`） | 🟢 简单 |
| `IAudioClient::Initialize(LOOPBACK)` + `IAudioCaptureClient` | `parec --device=<sink>.monitor --format=s16le --rate=48000 --channels=2` 子进程；或 `pw-record`；或 `libpulse` 原生 | 🟡 中 |
| `IAudioRenderClient` 多路输出 | 每设备一条线程 → 每设备一个 `pa_simple` / `pw_stream` / `cpal` stream | 🟡 中 |
| `IAudioEndpointVolume` | `pactl set-sink-volume` / `wpctl set-volume` | 🟢 简单 |
| `IPolicyConfig::SetDeviceFormat` | `pactl` 不支持改端点默认格式；PipeWire 用 `pw-metadata` 或直接按实际格式协商 | 🟡 中（**建议直接放弃「改端点格式」功能**） |
| `Shell_NotifyIcon` 托盘 | 弃用，或 `tray-icon` crate（StatusNotifier / AppIndicator） | 🟡 中 |
| `WINDIR\Fonts` 中文字体 | fontconfig：`fc-match "Noto Sans CJK SC"`；常见路径 `/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc` | 🟢 简单 |
| `%APPDATA%` | `$XDG_CONFIG_HOME`（默认 `~/.config`） | 🟢 简单 |
| `cmd /c start ms-settings:sound` | `pavucontrol` / `gnome-control-center sound` / `systemsettings5 kcm_pulseaudio`（探测后调） | 🟢 简单 |
| `.exe` 子进程 | 无扩展名的可执行文件 | 🟢 简单 |
| WDK 内核驱动 | **不需要** | 🟢 归零 |

### 5.2 三个技术选择（按改动量从小到大）

1. **`parec` 子进程**（最小）：直接 spawn `parec`，从 stdout 读 PCM。**0.5 人日**，缺点是多一个进程、启动延迟、错误传播靠退出码。
2. **`libpulse-binding`**（推荐）：进程内直接连 PulseAudio，`pa_simple` 或异步 API。**2–3 人日**，PipeWire 通过 `pipewire-pulse` 兼容层也能用。
3. **`pipewire-rs` 原生**（最低延迟）：真 PipeWire 客户端，能拿 `monitor` port 并精确控制 quantum。**3–5 人日**，且**只支持 PipeWire**（PulseAudio-only 发行版不支持）。

> **建议选 2**：覆盖面最广（PulseAudio + PipeWire 都通），工作量可控，且 `parec` 可作为 1 的临时方案先跑通端到端。

### 5.3 工作量估算

| 工作项 | 人日 | 说明 |
| --- | ---: | --- |
| 引擎后端拆分（`backend::wasapi` / `backend::pulse` 或直接 `#[cfg]` 双实现） | 5–8 | 把 `lib.rs:57-74` 的 9 个模块 gate 拆成 backend 目录；`route.rs` 3342 行需要按「采集线程 / sink 线程 / 调度」切开，**不用 trait 抽象也能做**（直接第二套实现），但复制粘贴会留下双份维护成本 |
| Linux 采集（`libpulse` monitor source） | 2–3 | 含 `null-sink` 自动创建/销毁 |
| Linux 多设备输出 | 2–3 | 每设备一线程 + 抖动队列（`ring.rs` 直接复用） |
| 设备枚举 + 默认 sink 切换 + 虚拟 sink 管理 | 1–2 | 大部分是 `pactl` 调用 |
| 音量（可选） | 1 | 可先砍掉 |
| 界面去 Windows 化（托盘/字体/配置/设置跳转/`windows_subsystem`） | 2–4 | `app.rs` 主体不动 |
| 打包 + CI（AppImage 或 `.deb` + `ubuntu-22.04` runner） | 1–2 | |
| **真机测试**（PipeWire 与 PulseAudio 两套、双蓝牙 A2DP、欠载/重连） | 3–5 | **最容易被低估的一项** |
| **合计** | **17–28 人日 ≈ 3.5–5.5 周** | `[估算]`，1 主力 + 0.5 测试 |

**判定：可行。** 风险集中在「双蓝牙 A2DP 的左右同步」——Linux 上蓝牙编解码延迟抖动比 Windows 更大，`sync.rs` 的校准能力（4269 行）如果要保留，几乎等于重写一遍。

### 5.4 Linux 特有的坑

- **实时优先级**：低延迟需要 `rtprio` / `memlock` 限制（`/etc/security/limits.d/` 或 systemd `LimitRTPRIO=`），否则 `pw-record` 会欠载。
- **`module-null-sink` 的 `latency_msec`**：默认偏大（客户端 buffer），要显式调小；PipeWire 侧则调 `quantum` / `node.latency`。
- **蓝牙双设备**：`bluez` + PipeWire 支持同时两副 A2DP，但 codec（SBC/AAC/aptX）选择与 MTU 会影响延迟，需要实测。
- **headless 场景**：PipeWire/PulseAudio 是 **user session** 服务，`systemd --user` + D-Bus 是前提；纯 SSH 无登录会话时 `systemctl --user` 不可用（见 §8.3）。

---

## 6. macOS 方案与工作量

### 6.1 能力映射表

| Windows 概念 | macOS 等价物 | 难度 |
| --- | --- | --- |
| WASAPI loopback 采集 | **没有公开等价物**（见 §6.2） | 🔴 硬限制 |
| 虚拟声卡 | **无法以可分发的方式自建**（需 HAL `AudioServerPlugIn`，C++）；实际＝**要求用户装 BlackHole / Loopback** `[未验证]` | 🔴 硬限制 |
| 设备枚举 | CoreAudio `AudioObjectGetPropertyData(kAudioHardwarePropertyDevices)` | 🟢 简单 |
| 默认输出 | 读：`kAudioHardwarePropertyDefaultOutputDevice`；写：`AudioObjectSetPropertyData` 到 `kAudioObjectSystemObject` | 🟢 简单 |
| 多设备输出 | ① 每设备独立 `AudioUnit`（HAL output）同时开；② `AudioHardwareCreateAggregateDevice` 聚合设备（= Audio MIDI Setup 的「多输出设备」，自带采样同步） | 🟡 中 |
| 端点格式 | `kAudioDevicePropertyStreamFormat` / `AvailableVirtualFormats`；**不需要** `IPolicyConfig` 那套未公开 API | 🟢 简单 |
| 端点音量 | `kAudioDevicePropertyVolumeScalar`（或 `kAudioHardwareServiceDeviceProperty_VirtualMainVolume`） | 🟢 简单 |
| 托盘 | `NSStatusItem`（`tray-icon` crate 支持 macOS） | 🟡 中 |
| 中文字体 | `/System/Library/Fonts/PingFang.ttc`（或 `Hiragino Sans GB`） | 🟢 简单 |
| 配置路径 | `~/Library/Application Support/stereo-link/` | 🟢 简单 |
| 设置跳转 | `open "x-apple.systempreferences:com.apple.Sound-Settings.extension"` | 🟢 简单 |
| 分发 | Developer ID 签名 + `notarytool` 公证 + `.dmg`；未签名/未公证的 app 在 Gatekeeper 下需用户手动放行 | 🔴 成本项（99 USD/年 + 流程） |

### 6.2 采集路线三选一（这是 macOS 可行性的全部关键）

| 路线 | 做法 | 前置条件 | 人日 |
| --- | --- | --- | ---: |
| **A. 第三方虚拟设备**（推荐） | 用户装 BlackHole / Loopback，系统默认输出指过去，引擎从该设备的 **input** 侧采集（它内部把 output 环回成 input）。与 Windows 的「选一块虚拟声卡当采集源」**结构完全一致** | 用户手动装驱动 + 重启 `coreaudiod`；`audio_default.rs` 的 `VAD_NAME_HINTS` 名称表需要加 macOS 条目 | 5–8 |
| **B. Core Audio Process Tap** | macOS 14.2 起 Core Audio 提供「进程 tap」（`AudioHardwareCreateProcessTap` / `CATapDescription`，`[未验证]`：API 名称本次未能联网复核），可直接 tap 系统/应用音频，无需虚拟设备 | 需 TCC「系统音频录制」授权；Rust 绑定很新，可能得自己写 `objc2` FFI；只支持 14.2+ | 10–15 |
| **C. ScreenCaptureKit** | `SCStream` 带音频输出，抓系统音频 | 需「屏幕录制」权限；延迟与质量一般；Apple 对该 API 的行为在版本间有变动 | 8–12 |

**为什么 A 是「推荐」而不是「降级」**：它让 macOS 后端在结构上**和 Windows 后端同构**（都是"从某个设备采"），因此 `capture` 那一层的代码形态可以复用；B/C 则要求完全不同的采集模型，等于再写第三套。

### 6.3 工作量估算

| 路线 | 工作项 | 人日 |
| --- | --- | ---: |
| A | 引擎后端拆分（同 Linux） | 5–8 |
| A | CoreAudio 采集（从虚拟设备的 input 侧，`AudioUnit` 或 `cpal`） | 5–8 |
| A | CoreAudio 多设备输出 | 2–4 |
| A | 设备枚举 + 默认输出切换 + 音量 | 2–3 |
| A | 界面去 Windows 化 | 2–4 |
| A | 签名 + 公证 + `.dmg` + CI（`macos-14` runner，arm64 + x86_64 universal） | 2–3 |
| A | 真机测试（含 TCC 权限、蓝牙、休眠唤醒） | 2–4 |
| | **A 合计** | **20–34 人日 ≈ 4–7 周** |
| B | A 的全部工作项，但采集换成 Process Tap，且**没有可复用形态** | 30–50 人日 ≈ 6–10 周 |

**判定：勉强 / 有条件可行。** 三个硬前置：
1. 用户必须装第三方虚拟音频设备（`[未验证]`：BlackHole 为 GPL-3.0，只能作为"用户自装外部依赖"，**不能打进以 MIT OR Apache-2.0 发布的产物**——这一点和 `receiver.rs:6-10` 已建立的"许可隔离用独立 exe"思路一致，但这里连 exe 都不可行，只能是文档指引）。
2. Apple Developer Program 账号（签名 + 公证），否则用户要手动 `xattr -d com.apple.quarantine`。
3. 想在 14.2 以下（或不想装驱动）就**必须走 B/C**，代价翻倍。

---

## 7. 最小可行路径（三条提案 + 推荐）

### 提案 A：引擎抽象层（trait）+ Linux 后端 ⇒ **推荐给"要真上 Linux"**

```
crates/stereo-audio/src/
  lib.rs            # 只留 pub use，按 cfg 选 backend
  backend/
    mod.rs          # trait: Capture / Render / DeviceEnum / VolumePolicy / FormatPolicy
    wasapi/         # 现 route.rs/capture.rs/com.rs/... 原样搬进来
    pulse/          # Linux 新写
    coreaudio/      # macOS 新写（可选，后置）
```

- **不要一次性抽完美 trait。** 先做**最小 trait 面**：`list_endpoints` / `default_endpoint` / `open_capture(source, fmt)` / `open_render(target, fmt)` / `set_default_endpoint(id)`，其它（格式探测、独占模式、平衡校准）先 `#[cfg(windows)]` 留着。
- 成本：trait 化 3–5 人日 + Linux 后端 8–12 人日 + GUI 适配 2–4 人日 + 测试 3–5 人日 ≈ **16–26 人日**。
- 好处：macOS 后端可以之后按同样的缝插进来，边际成本降到 8–12 人日。

### 提案 B：主程序只支持 Windows，接收端 / 控制面跨平台 ⇒ **推荐给"目标是把手机声音收进来"**

- 事实支撑：`receiver/Cargo.toml:28` 已经用 **`cpal 0.17`**（跨平台：WASAPI / ALSA / CoreAudio），`receiver/crates/airplay-recv/Cargo.toml:18`、`receiver/crates/miplay-recv/Cargo.toml:20` 都是 `cpal.workspace = true`。**接收端今天就能在 Linux/macOS 上编译**（`cargo build -p airplay-recv`）`[静态]`。
- 成本：**0–5 人日**（主要是 macOS 上 cpal 的输出设备选择、以及接收端的守护/服务化）。
- 代价：**左右分声道这个"卖点"在非 Windows 上不存在**，产品在 Linux/macOS 上退化成"AirPlay/妙播接收器"。

### 提案 C：Linux 优先、macOS 明确不支持 ⇒ **推荐给"要控制投入"**

- 只做提案 A 的 Linux 分支；文档里明确写「macOS 需要用户自行安装 BlackHole + 自行承担不同步风险」，或直接标"暂不支持"。
- 成本 ≈ 提案 A 的 70%（16–20 人日）。
- 理由：Linux 的虚拟声卡是**一条命令**，macOS 的是**一个第三方内核级组件 + 开发者账号**，两者的"直接部署运行"难度不在一个量级。

### 推荐组合

> **先做提案 B（0–5 人日，立刻让"声音能进 Linux/macOS"成立），同时把提案 A 的 trait 面切出来（3–5 人日，不写后端）。**
> 这一步做完，产品叙事变成「Windows：完整左右分声道；Linux/macOS：接收端 + 只读控制面」。等真正有 Linux 用户需求时，再投 8–12 人日补 `pulse` 后端。
> **不建议**同时开 macOS 后端——它的两个硬前置（第三方虚拟设备 + 开发者账号）不是工程能解决的。

---

## 8. 与「CLI 化 / 无头部署」的关系

### 8.1 现状：主程序**没有** CLI

- `crates/stereo-link/Cargo.toml:10-12` 只有 `[[bin]] name = "stereo-link"`，`main.rs:34` 直接 `eframe::run_native`（`:62-66`），**没有任何参数解析**——`crates/stereo-link/src/` 内无 `std::env::args`、无 `clap` `[实测]`。
- 引擎侧最接近"CLI"的只有两个**示例/工具**，都不是产品入口：
  - `crates/stereo-audio/examples/route_demo.rs:34-54`：`--left/--right/--both/--source/--manual` 的完整参数面，**这实际上已经是一个手工 CLI 的雏形**；
  - `crates/stereo-audio/src/bin/endpoints.rs:11`：`fn main()` 打印端点列表。

### 8.2 已在路上的控制面（**并发新增，尚未接线**）

`crates/stereo-link/src/control.rs`（521 行，取样时未跟踪、未在 `main.rs:10-30` 声明 `mod control;`）已经实现了：

- `control.rs:1-14`：**本机控制面**——GUI / `serve` 在 `127.0.0.1` 随机端口开 **JSON 行协议**，`stereo-link-cli` 连上去读状态、发命令；实例把 `{port, token, pid, mode}` 写进 `control.json`，退出删除。
- `control.rs:17` `TcpListener` / `control.rs:131` `TcpListener::bind((Ipv4Addr::LOCALHOST, 0))` / `control.rs:110-111` `addr()`。
- `control.rs:129` `ControlServer::start(mode: &str)`，`mode` 只区分 `"gui"` / `"serve"`。
- `control.rs:300` `engine_json` / `:341` `receivers_json` / `:363` `endpoints_json` / `:388` `config_summary` / `:415` `status_json` / `:434` `config_get` / `:447` `config_set`。
- `control.rs:26-27` `PROTOCOL_VERSION: u32 = 1`（字段只增不改）。

**这对可移植性是好消息**：控制面是 **`std::net` + `serde_json`，完全跨平台**，唯一 Windows 味的是 `control.rs:32-48` 的 `%APPDATA%` 路径。也就是说——**只要引擎能跑，`serve` 模式就是天然的跨平台无头入口**。

### 8.3 Linux 上"无头跑"需要的条件清单

| # | 条件 | 现状 | 工作量 |
| --- | --- | --- | --- |
| 1 | **引擎要有非 Windows 后端** | ❌ 无 | 见 §5.3（10–13 人日） |
| 2 | **要有 `serve` 模式入口**（不带 eframe） | ⚠️ `control.rs` 已设计 `mode="serve"`，但**没有实现**；`main.rs` 无参数解析 | 2–3 人日（含 `--serve` / `--json` / `--config` 参数） |
| 3 | **二进制不能硬链 GUI** | ⚠️ eframe 在 `crates/stereo-link/Cargo.toml:16-21` 是**无条件依赖**；无头机器上仍要拉 X11/Wayland 库 | 1 人日（feature split：`gui` / `headless`，`default = ["gui"]`） |
| 4 | **配置/日志/控制文件路径要去 `%APPDATA%`** | ❌ `config.rs:449-459`、`control.rs:32-48` | 0.5 人日（`dirs` crate 或 `$XDG_CONFIG_HOME`） |
| 5 | **PipeWire/PulseAudio 的 user session** | — | 无头机的运维前提：`systemd --user` + D-Bus 可用；纯 SSH 无 linger 时 `systemctl --user` 会失败，需要 `loginctl enable-linger <user>` |
| 6 | **实时优先级** | — | `LimitRTPRIO` / `limits.d`，否则欠载 |
| 7 | **systemd 单元** | ❌ 无 | 0.5 人日 |

### 8.4 systemd user unit 草案（示意，未验证）

```ini
[Unit]
Description=StereoLink headless routing engine
After=pipewire.service pipewire-pulse.service

[Service]
Type=simple
ExecStart=%h/.local/bin/stereo-link --serve --no-gui
Restart=on-failure
LimitRTPRIO=95
Environment=RUST_LOG=info

[Install]
WantedBy=default.target
```

> 注意：`--serve` / `--no-gui` **今天都不存在**。这是目标形态，不是现状。

---

## 9. 风险与建议

### 9.1 高风险（按"会不会让项目卡死"排序）

| # | 风险 | 影响 | 缓解 |
| --- | --- | --- | --- |
| 1 | **macOS 没有原生系统回环** | 直接决定 macOS 是"4 周"还是"不可能" | 明确接受"要装 BlackHole"这个前置，写进产品文档；不要承诺"装完就能用" |
| 2 | **`route.rs`(3342) + `sync.rs`(4269) 与 WASAPI 深度交织** | 抽 trait 时容易变成"复制一份再改"，留下双份维护成本 | 只抽 §7 提案 A 那 5 个方法；`sync.rs`（声学校准）默认**不移植**，标"Windows only 功能" |
| 3 | **双蓝牙 A2DP 的左右声道同步** | 这是产品核心体验，Linux/macOS 上抖动更大 | 先做"能出声"，同步问题用 §5 的测试项单独排期；`sync.rs` 的独占模式结论在 Linux/macOS 需重新实测 |
| 4 | **CI 从未在非 Windows 上跑过**（`.github/workflows/ci.yml:21-23`、`:50-52` 都是 `windows-latest`） | 今天改的任何跨平台代码都是"没验证的代码" | 提案 B 的第一步就加 `ubuntu-latest` / `macos-14` 的 `cargo check --workspace` job（会立刻暴露 §4.1 第 3 条的编译失败） |
| 5 | **macOS 分发** | 未签名 app 在 Gatekeeper 下基本不可"直接部署" | 预算 99 USD/年 + 公证流程；或明确只提供源码编译 |
| 6 | **`tray.rs:116-117` 依赖 winit 内部类名** | 升级 winit/eframe 就可能静默失效 | 移植时**不要**试图复刻这套（Windows 专属）；非 Windows 用 `tray-icon` 或直接不做托盘 |
| 7 | **工作区正在被并发修改**（`control.rs` / `receiver.rs` / `balance.rs` 未跟踪） | 本文行号会漂 | 落地前按 §10 重测；`control.rs` 接线后 §8.2 的结论需重看 |

### 9.2 建议

1. **先花 0.5 天加一个跨平台 CI job**（`cargo check` on `ubuntu-latest`）。这是**最低成本、最高信息量**的一步：它会一次性把"哪些文件在非 Windows 上编译不过"列成清单，替代本文的 `[静态]` 推断。
2. **把"引擎"和"产品"分开定位**：Windows = 完整左右分声道；Linux/macOS = 接收端 + 控制面。**不要让"跨平台"成为 0.6 版本之前的承诺。**
3. **虚拟声卡这件事在 Linux 上归零、在 macOS 上是外部依赖** —— 这是三个平台**唯一**的结构性差异，也是最应该写进 README 的一句话。
4. **`control.rs` 这条线值得优先接线**：它是唯一"写了就三平台通用"的增量（`std::net` + serde_json），也是将来 `serve` / 无头部署的地基。
5. **不要移植 `sync.rs`**（4269 行，占引擎 32%）。它的价值是"Windows 上把两副蓝牙音箱对齐"，在 Linux/macOS 上需要完全重新实测，投入产出比最低。

---

## 10. 附录：复测命令

### 10.1 行数（字节级 LF 计数，避免 PowerShell ANSI 解码问题）

```powershell
cd E:\AI\MiPlay
foreach($f in (Get-ChildItem crates\stereo-audio\src,crates\stereo-link\src -Recurse -Filter *.rs -File | Sort-Object FullName)){
  $b=[System.IO.File]::ReadAllBytes($f.FullName); $lf=0; foreach($x in $b){ if($x -eq 10){$lf++} }
  "{0}|lf={1}|bytes={2}" -f $f.FullName.Replace((Get-Location).Path+'\',''),$lf,$b.Length
}
```

### 10.2 平台耦合面

```powershell
cd E:\AI\MiPlay
# 所有 cfg(windows) 门控
Select-String -Path crates\stereo-audio\src\*.rs,crates\stereo-link\src\*.rs -Pattern '#\[cfg\(windows\)\]' | Measure-Object
# Windows-only 符号面
Select-String -Path crates\stereo-audio\src\*.rs -Pattern 'IAudioClient|IAudioCaptureClient|IAudioRenderClient|IAudioEndpointVolume|IMMDeviceEnumerator|IPolicyConfig|CoCreateInstance|CoInitializeEx|CoTaskMemFree|AUDCLNT_'
```

### 10.3 编译验证（**本文未执行**，任务禁止；如需确认 §4.1 第 3 条请自己跑）

```bash
# 预期：--lib 通过（只剩纯逻辑模块）；bin 与 stereo-link 失败
cargo check -p stereo-audio --lib
cargo check -p stereo-audio
cargo check -p stereo-link
```

### 10.4 读中文文件

**必须用编辑器的 `read` 工具**，`PowerShell Get-Content` 会按 ANSI 解码导致中文乱码（本次实测已复现）。
