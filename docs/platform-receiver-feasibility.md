# 接收端（`receiver/`）在 macOS / Linux 上的编译、运行与命令行部署可行性

> **评估对象**：`receiver/` 子工作区里的两个「手机→电脑出声」接收端 exe
> - `receiver/crates/airplay-recv`：链接 `shairplay 0.10`（LGPL-3.0-or-later，AirPlay 1 / RAOP），自写 cpal 输出
> - `receiver/crates/miplay-recv`：链接 `receiver/vendor/fusionplay/FusionPlay.MiPlaySdk`（AGPL-3.0-only，小米妙播），SDK 内部自己用 cpal 出声
>
> **评估性质**：只读分析 + 本文档。**未执行 `cargo build`**、**未修改 `receiver/` 下任何源码**。
> **与 `docs/platform-feasibility.md` 的关系**：那份文档管的是**主程序**（`crates/stereo-link` + `stereo-audio`，89% 是 WASAPI 绑定，必须重写后端）；**本文是另一个东西**——接收端本来就走 `shairplay` / `cpal` / 原生 socket，跨平台成本完全不是一个量级。两份文档的数字不要混用。

---

## 基线声明（先读，否则行号会漂）

- 行号/行数取本机快照：`HEAD = bcca6a1`（`chore(release): 0.5.5`）**+ 未提交改动**，取样时间 2026-10-21 22:32–23:10（Asia/Shanghai）。
- 取样时 `git status` 显示 **`receiver/` 整个目录是未跟踪的（`?? receiver/`）**，且主工作区有并发改动。本文所有 `receiver/...` 行号**只对这个快照成立**；如果 `receiver/` 正在被另一路改，请以「符号名 + 模块」为准，行号当参考。
- 本文引用的第三方源码全部来自本机 cargo registry：`C:\Users\Administrator\.cargo\registry\src\index.crates.io-1949cf8c6b5b557f\`（`shairplay-0.10.0`、`cpal-0.17.3`、`mdns-sd-0.21.6`、`socket2-0.6.5`）。
- **`Cargo.lock` 已经是「全平台解析」状态**：`cpal 0.17.3`、`shairplay 0.10.0`、`mdns-sd 0.21.6`、`astro-dnssd 0.3.6`、`getifs 0.6.1`、`if-addrs 0.15.0`（vendored shim）、`socket2 0.6.5`、`windows-sys 0.45.0 / 0.61.2`、`oboe 0.6.1`、`symphonia 0.6`。也就是说**非 Windows 需要的依赖已经被解析进 lock**（`astro-dnssd`、`objc2*`、`coreaudio-rs` 等只在 macOS 生效的部分也已锁定），这是跨平台构建能走通的第一个硬指标。
- **三个工具链前置条件**（与源码无关，但决定「能不能开始编译」）：
  1. **rustc ≥ 1.94**：`shairplay 0.10.0` 是 `edition = "2024"` + `rust-version = "1.94"`；`FusionPlay.MiPlaySdk` 与 vendored `if-addrs` shim 也是 `edition = "2024"`。老工具链直接拒绝解析。
  2. **Linux 需要 ALSA 开发包 + pkg-config**：`cpal` 在 Linux 上对 `alsa = "0.11"` 是**无条件依赖**（`cpal-0.17.3/Cargo.toml:142-144`），`alsa-sys` 用 pkg-config 找 `libasound`。
  3. **macOS 需要 Xcode 命令行工具**（链接系统 framework），但**不需要** `bindgen`/`libclang`——`coreaudio-rs 0.14`、`objc2 0.6` 都是纯 Rust FFI 声明，靠 `#[link]` 链系统库。

### 证据等级

| 标记 | 含义 |
| --- | --- |
| `[源码]` | 仓库内源码行，带仓库相对路径 + 行号 |
| `[依赖源码]` | 本机 cargo registry 里的第三方 crate 源码，带文件名 + 行号 |
| `[静态]` | 由源码结构推断的编译期结论（**本次未编译**，任务禁止 build） |
| `[推断]` | 由平台语义推断的运行期行为（未在真机跑过） |
| `[未验证]` | 本次未能复核（外部资料未联网核对），不作定论 |

> **本次没有联网复核任何 Apple / Avahi 官方文档**（沿用 `docs/platform-feasibility.md` 记录的同一条限制：`web_search` 返回 `HTTP 401 Authentication Fails`）。凡「mDNSResponder 独占 5353」「macOS 需要 SO_REUSEPORT 才能共享端口」这类**操作系统行为**，都标 `[推断]`/`[未验证]`，并给出「一行命令即可验证」的办法（见 §8）。代码层面的结论（谁在哪个 cfg 下编译、绑哪个端口、用什么 API）则是**可复核的**，全部标 `[源码]` / `[依赖源码]`。

---

## 0. 一页速查

**一句话结论：`airplay-recv` 是「改两行就能跨平台」；`miplay-recv` 是「能编译，但 macOS 上 5353 端口大概率绑不上」。**

| | Windows（今天） | Linux | macOS |
| --- | --- | --- | --- |
| **`airplay-recv` 编译** | ✅ | ✅ 需 `libasound2-dev` + `pkg-config` + rustc ≥1.94 | ✅ 无额外系统库（除 Xcode CLT） |
| **`airplay-recv` 出声** | ✅ WASAPI | ✅ ALSA（经 `default` → PipeWire/PulseAudio 插件） | ✅ CoreAudio（会改设备名义采样率） |
| **`airplay-recv` mDNS `_raop._tcp`** | ✅ `mdns-sd` | ✅ `mdns-sd`，与 avahi 可共存，但**单播 QU 有被抢答风险** | ✅ **原生 Bonjour**（`astro-dnssd`，走系统 API，**不抢 5353**） |
| **`miplay-recv` 编译** | ✅ | ⚠️ **能过**（`#[cfg(windows)]` 隔离干净、socket2 组播 API 无 feature gate） | ⚠️ **能过**，同上 |
| **`miplay-recv` 发现（mDNS/Lyra）** | ✅ 专用 Windows DNS-SD + 自建 UDP 应答 | ⚠️ **可能能跑**：SO_REUSEADDR 在 Linux 够用，但要和 avahi 抢单播应答 | ❌ **大概率起不来**：只设 SO_REUSEADDR 就 bind `0.0.0.0:5353`，macOS 上 mDNSResponder 已占 |
| **`miplay-recv` 出声** | ✅ | ✅ ALSA | ✅ CoreAudio |
| **需要 root / 特权端口** | 否 | **否**（5000/8899/55982/5353 全部 >1024，不需要 `CAP_NET_BIND_SERVICE`） | 否 |
| **命令行部署** | 现有 `start.ps1` + 防火墙规则 | systemd **user unit**（推荐）+ `ufw`/`firewalld` | launchd **LaunchAgent**（推荐） |

**必须改的代码点（按优先级）**：

1. **P0**：`miplay-recv` 的状态文件路径只认 `%LOCALAPPDATA%`（`receiver/crates/miplay-recv/src/main.rs:380-385`），Linux/macOS 上退化成「当前目录 `./miplay-recv.json`」，systemd 下 cwd=`/` 直接不可写 → **启动即失败**。
2. **P0（仅 macOS）**：SDK 的 mDNS 应答 socket 只设 `SO_REUSEADDR`（`receiver/vendor/fusionplay/FusionPlay.MiPlaySdk/src/discovery.rs:997-999`）→ macOS 绑 5353 失败。
3. **P1**：两个 exe 都只处理 `ctrl_c()`（`airplay-recv/src/main.rs:209`、`miplay-recv/src/main.rs:165`），systemd/launchd 发 **SIGTERM** 时不会走 `stop()` → mDNS 不发 goodbye，手机端留幽灵设备。
4. **P1**：`airplay-recv` 用固定 `--rate 44100` 开流（`receiver/crates/airplay-recv/src/audio.rs:124-131`），且注释里依赖的是 **WASAPI 的 AUTOCONVERTPCM**（`audio.rs:120`）；macOS CoreAudio 会**强改设备采样率**，Linux ALSA 只做「就近取整」→ 两头都可能出问题（详见 §3.1.3）。

---

## 1. 结论表

### 1.1 主表（Windows / macOS / Linux × 两个接收端 × 三件能力）

图例：✅ 现成可用 ｜ ⚠️ 能跑但有前置条件/需改 ｜ ❌ 需要改代码或绕路

| 维度 | Windows（今天） | Linux | macOS |
| --- | --- | --- | --- |
| **airplay-recv 编译** | ✅ | ✅ 装 `pkg-config libasound2-dev` 即可；`cpal` 的 `alsa` 依赖在 Linux 上不可关闭（`cpal-0.17.3/Cargo.toml:142-144`） | ✅ `coreaudio-rs`/`objc2*` 全是纯 Rust 绑定（`cpal-0.17.3/Cargo.toml:242-289`，条件写的是 `target_vendor = "apple"` 而不是 `target_os = "macos"`，但 macOS 满足） |
| **airplay-recv 声卡输出** | ✅ WASAPI（`default_host()` → WasapiHost） | ✅ ALSA（`default_host()` → AlsaHost，`cpal-0.17.3/src/platform/mod.rs:685-729`） | ✅ CoreAudio（`default_host()` → CoreAudioHost，`cpal-0.17.3/src/platform/mod.rs:731-751`） |
| **airplay-recv mDNS** | ✅ `mdns-sd`（`shairplay-0.10.0/Cargo.toml:214-218`，`not(target_os="macos")`） | ✅ `mdns-sd` 纯 Rust，自建 responder，`ServiceDaemon` 设 `SO_REUSEADDR` + `SO_REUSEPORT`（`mdns-sd-0.21.6/src/service_daemon.rs:951-966`） | ✅ **`astro-dnssd 0.3`**（`shairplay-0.10.0/Cargo.toml:219-221`）：调系统 Bonjour，进程本身**不持有 5353** |
| **airplay-recv 可达性** | ✅ 需放行防火墙 | ✅ | ✅ 首次运行会有「本地网络」授权弹窗 |
| **miplay-recv 编译** | ✅ | ⚠️ 能过（见 §2.4、§3.3） | ⚠️ 能过（同上） |
| **miplay-recv 发现** | ✅ 走 Windows DNS-SD（可关）+ 自建 UDP 应答 | ⚠️ 自建 UDP 应答；与 avahi 共享 5353，**可能收不到单播查询** | ❌ bind `0.0.0.0:5353` 预计 `EADDRINUSE`（`[推断]`，见 §8 一行验证） |
| **miplay-recv 声卡输出** | ✅ SDK 内部 cpal/WASAPI | ✅ SDK 内部 cpal/ALSA | ✅ SDK 内部 cpal/CoreAudio |
| **命令行部署** | `start.ps1` + `netsh`/`New-NetFirewallRule` | systemd user unit（本文 §5.2 给了可复制草稿） | launchd LaunchAgent（本文 §5.3 给了可复制草稿） |
| **整体判定** | 现状 | **airplay-recv 可用；miplay-recv 大概率可用（需 P0 状态路径 + P1 SIGTERM）** | **airplay-recv 可用（改 2 行更好）；miplay-recv 编译能过、运行要改 SDK 的 socket 选项** |

### 1.2 三句话

1. **`airplay-recv` 的跨平台成本≈0**：它自己的 446 行音频层只用 `cpal` 的公共 trait（`DeviceTrait`/`HostTrait`/`StreamTrait`），`shairplay` 已经在 crate 级别做了 `macos → astro-dnssd` / `others → mdns-sd` 的分流，**macOS 上连 5353 都不用自己绑**（交给系统 Bonjour）。
2. **`miplay-recv` 的跨平台障碍不在「编译」，在「它自己实现了一个 mDNS responder」**：`discovery.rs` 里 Windows 专用代码全部 `#[cfg(windows)]` 隔离（71 处 cfg，见 §2.4），但**自建 UDP 应答器是三个平台共用的**——`MiPlayAdvertisement::register`（`discovery.rs:150`）在所有平台都会 `create_responder_socket`（`:281`）并起线程跑 `responder_loop`（`:286-349`）；`#[cfg(windows)]` 的那些（`:372-462`）只是 Windows 额外叠的一层 DNS-SD。**所以 macOS 上 5353 绑不上，就是致命失败**，不是「少个优化」。
3. **真正的部署障碍是「谁是音频会话的主人」**：Linux 上必须用 **user unit**（`XDG_RUNTIME_DIR` 决定 PipeWire/Pulse 能不能连上），macOS 上必须用 **LaunchAgent**（LaunchDaemon 起不了「当前登录用户的音频会话」）。这决定了 §5 的 unit 模板长什么样。

---

## 2. 依赖逐个判平台

### 2.1 总表

| crate | 版本（lock） | 谁在用 | Windows | Linux | macOS | 条件编译证据 |
| --- | --- | --- | --- | --- | --- | --- |
| `shairplay` | 0.10.0 | airplay-recv | ✅ | ✅ | ✅ | `shairplay-0.10.0/Cargo.toml:214-221` 三段 target 条件 |
| `mdns-sd` | 0.21.6 | shairplay（非 macOS） | ✅ | ✅ | —（不参与） | 同上，`cfg(not(target_os="macos"))` |
| `astro-dnssd` | 0.3.6 | shairplay（macOS） | — | — | ✅ 链接系统 `dns_sd` | 同上，`cfg(target_os="macos")` |
| `gethostname` | 1.x | shairplay（非 macOS） | ✅ | ✅ | — | `Cargo.toml:214-216` |
| `cpal` | 0.17.3 | airplay-recv、SDK（非 android） | ✅ WASAPI | ✅ ALSA | ✅ CoreAudio | `cpal-0.17.3/src/host/mod.rs` 逐模块 cfg |
| `alsa` | 0.11 | cpal（linux/bsd） | — | ✅ | — | `cpal-0.17.3/Cargo.toml:142-144` |
| `coreaudio-rs` | 0.14 | cpal（apple） | — | — | ✅ | `cpal-0.17.3/Cargo.toml:242-249` |
| `oboe` | 0.6.1 | SDK（**仅 android**） | — | — | — | SDK `Cargo.toml` `[target.'cfg(target_os="android")']` |
| `tokio` | 1.x（`rt-multi-thread`,`macros`,`signal`） | 两个 exe | ✅ | ✅ | ✅ | 纯异步，无平台代码 |
| `socket2` | 0.6.5 | SDK | ✅ | ✅ | ✅ | 见 §2.5（关键：组播/`set_ttl_v4` 无 feature gate） |
| `getifs` + vendored `if-addrs` shim | 0.6.1 / 0.15.0 | SDK | ✅ | ✅ | ✅ | 见 §2.4 |
| `windows-sys` | 0.61.2 | SDK | ✅ | —（不参与） | — | SDK `Cargo.toml` `[target.'cfg(windows)'.dependencies]` |
| `symphonia`（aac） | 0.6 | SDK | ✅ | ✅ | ✅ | 纯 Rust 解码器 |

### 2.2 `shairplay`（决定性的一环，也是最省心的一环）

- 版本/许可：`shairplay-0.10.0`，`edition = "2024"`，`rust-version = "1.94"`，**LGPL-3.0-or-later**（`shairplay-0.10.0/Cargo.toml:12-15`）；`unsafe_code = "forbid"`（同一 manifest）。
- **它自己就有 macOS 分支**，这是最关键的一条：

  ```toml
  # shairplay-0.10.0/Cargo.toml:214-221
  [target.'cfg(not(target_os = "macos"))'.dependencies.gethostname]
  version = "1"
  [target.'cfg(not(target_os = "macos"))'.dependencies.mdns-sd]
  version = "0.21"
  [target.'cfg(target_os = "macos")'.dependencies.astro-dnssd]
  version = "0.3"
  ```

  README 也写明了设计意图：`🌐 Cross-platform | macOS (native Bonjour) + Linux (pure Rust mDNS)`（`shairplay-0.10.0/README.md:49`），以及「On macOS, mDNS service registration uses astro-dnssd … On Linux, mdns-sd provides a pure Rust mDNS implementation with no native dependencies.」（`README.md:54`）。
- `src/net/mdns.rs` 的分流（全部 `[依赖源码]`）：
  - `223-283`：`#[cfg(target_os = "macos")]` 分支，`MdnsService { _raop_reg: Option<astro_dnssd::RegisteredDnsService>, _airplay_reg: … }`，`register_raop` 用 `astro_dnssd::DNSServiceBuilder::new("_raop._tcp", port).with_name(raop_name).with_txt_record(...)`。
  - `287-374`：`#[cfg(not(target_os = "macos"))]` 分支，`mdns_sd::ServiceDaemon::new()` + `ServiceInfo::new(...).enable_addr_auto()` + `daemon.register(svc)`，`Drop` 里 unregister + shutdown。
  - 其它 cfg 只有 `#[cfg(feature = "ap2")]`（`mdns.rs:35-138`、`252-362`），本项目**未启用 `ap2`**（`receiver/crates/airplay-recv/Cargo.toml` 里 `shairplay = "0.10"`，没开 feature）。
- **推论（重要）**：默认 feature 下 **只注册 `_raop._tcp`**，`_airplay._tcp` 的注册整段被 `#[cfg(feature="ap2")]` 编译掉了（`mdns.rs:252-276`、`329-362`）。所以这台电脑在 iPhone 上表现为**「AirPlay 1 / RAOP 设备」**——能选、能出声，但不会出现在需要 `_airplay._tcp` 的新版「隔空播放」聚合列表里。这一点**与平台无关**，Windows 今天也是这个行为。
- 全部运行期依赖是纯 Rust：`aes/base64/bytes/cbc/ctr/ed25519-dalek/hex/hmac/httparse/md-5/plist/rand/rsa/sha1/sha2/subtle/thiserror/tokio/tracing/x25519-dalek`。**没有 C 依赖、没有 `build.rs`**（`Cargo.toml:17` `build = false`）。
- `src/dacp/mod.rs:37/64` 是**唯一另一个平台 cfg**：`#[cfg(target_os = "macos")] fn discover_dacp_port(...)` 恒返回 `None`（注释说 astro-dnssd 没有同步 browse+resolve），调用方回落到 3689。**不影响编译，也不影响本程序**（本程序不提供 DACP 遥控）。

### 2.3 `mdns-sd` / `astro-dnssd`

- `mdns-sd 0.21.6` README 自述：`- supports macOS, Linux and Windows.`（`README.md:11`），并且 `we tested with some existing common tools (e.g. Avahi on Linux, dns-sd on MacOS ...)`（`README.md:29`）。实现是纯 Rust，自己起 responder，`_new_socket_bind` 里对 socket 设 `set_reuse_address(true)` **和** `set_reuse_port(true)`（`mdns-sd-0.21.6/src/service_daemon.rs:951-966`）。
- `astro-dnssd 0.3.6`：C 库绑定，走系统 DNS-SD（macOS 上在 `libSystem` 里，Windows 走 `dnsapi`，其它 Unix 走 `pkg-config` 找 avahi-compat-libdns_sd——从 lock 里的 `pkg-config`/`winapi` 依赖可看出）。**本机 registry 里没有它的源码**（Windows 上从没下载过），所以 macOS 首次构建需要能访问 crates.io 拉取。`[源码]`+`[未验证]`（`astro-dnssd` 的 build.rs 是否在 macOS 上也 probe `dns_sd` 未能读到）
- 对 **`airplay-recv` 而言 macOS 上根本不加载 `mdns-sd`** —— 任务里「mdns-sd 只支持 Linux/Windows」这条线索，对 shairplay 来说**已经被 `astro-dnssd` 分流解决了**；`mdns-sd` 的 README 也自称支持 macOS，两边都不构成障碍。

### 2.4 FusionPlay SDK（miplay-recv 的全部风险集中地）

manifest（`receiver/vendor/fusionplay/FusionPlay.MiPlaySdk/Cargo.toml`）：

- `edition = "2024"`、`AGPL-3.0-only`、`crate-type = ["rlib"]`。
- 平台依赖**全部条件化**，没有一条裸依赖：

  | 声明 | 生效平台 | 说明 |
  | --- | --- | --- |
  | `oboe = "0.6.1"`（`[target.'cfg(target_os="android")']`） | Android | 不参与 mac/Linux |
  | `cpal = "0.17"`（`[target.'cfg(not(target_os="android"))']`） | Win/mac/Linux | **mac/Linux 的唯一音频后端** |
  | `windows-sys 0.61`（`[target.'cfg(windows)'.dependencies]`，features `Win32_Foundation`/`Win32_NetworkManagement_IpHelper`/`Win32_NetworkManagement_Ndis`/`Win32_Networking_WinSock`/`Win32_System_Registry`） | Windows | **非 Windows 不参与编译**，`Win32_System_Registry` 目前甚至没被用（无害） |
  | `if-addrs = { path = "../FusionPlay.AndroidIfAddrs" }` | 全平台 | vendored shim，见下 |

- **`#[cfg(windows)]` 隔离得很干净**。`src/` 下 71 处平台 cfg，Windows 专用代码全部落在 `cfg` 里，且**没有任何非 cfg 的「同名同签名」外部引用**：

  | 文件 | cfg 行号 | 干的事 |
  | --- | --- | --- |
  | `discovery.rs` | `12`、`372-462`、`555`、`589`、`966-971`、`1038`、`1279`、`2288-2293` | 调 `windows-sys` 的 IpHelper/Ndis（网卡友好名、`if_nametoindex` 等） |
  | `discovery.rs` | `12` + `208` + `2240/2244` | Windows DNS-SD（`DnsServiceRegister`）注册，**默认关闭**（`discovery_platform_policy`），非 Windows 不编译 |
  | `lib.rs` | `359-381` | `host_platform()` / `default_model()`，是 **`cfg!` 运行期字符串**，非 Windows 分支已写好：macOS → `"macOS"` / `"Mac"`，其它（含 Linux）→ `"Rust"` / `"FusionPlay receiver"` |
  | `media.rs` | 大量 `cfg(not(target_os="android"))` / `cfg(target_os="android")` | cpal 出声（非 android）vs Oboe（android） |

- `lib.rs` 的模块文档自己写着：`The same Rust API is available on Windows, macOS and Android.`（`lib.rs` 顶部）。**注意里面没有 Linux**——Linux 能编译，但厂商没承诺。
- vendored `if-addrs` shim（`receiver/vendor/fusionplay/FusionPlay.AndroidIfAddrs`）：包名 `if-addrs 0.15.0`、`lib name = if_addrs`、依赖 `getifs 0.6.1`，193 行，只做转发。唯一的平台 cfg 是 `#[cfg(windows)] pub adapter_name: String`（`src/lib.rs:22-23`、构造处 `187-188`），而 **SDK 全文没有任何地方读 `adapter_name`** → 非 Windows 不会缺字段。`getifs` 在 apple/linux 上走 `getifaddrs`，`Cargo.toml` 按平台选 `linux-raw-sys`/`libc`/`windows-sys`（lock 里同时存在 `if-addrs 0.15.0 (path)` 和 crates.io 上的 `if-addrs 0.15.0`，是**两个不同包**，不冲突）。

**方言结论**：`miplay-recv` 在 macOS/Linux **编译大概率能过**（唯一要祈祷的是 `edition 2024` + 新版 `getifs`/`socket2` 的 API 稳定性，以及 `astro-dnssd` 那条不属于 miplay 的路径）。

### 2.5 组播 / 发现 API 在 mac/Linux 上到底有没有

`discovery.rs` 的非 Windows 分支（`229-350`）自己造 DNS 报文 + 用 `socket2` 收发。用到的 API 逐个核对（`socket2-0.6.5/src/socket.rs`、`src/sys/unix.rs`）：

| API | 调用点（`discovery.rs`） | socket2 里是否有平台/feature 限制 | mac/Linux |
| --- | --- | --- | --- |
| `join_multicast_v4(&group, &iface_ip)` | `1030`（responder）、`~1107` 附近（unicast 侧） | 无 gate（`socket.rs:1312` 是普通 `pub fn`） | ✅ |
| `set_multicast_if_v4(&ip)` | `1011`、`1101` | 只有 `cfg(not(target_os="wasi"))`（`socket.rs:1546`） | ✅ |
| `set_ttl_v4(255)` | `1020`、`1107` | 无 gate（`socket.rs:1630`） | ✅ |
| `set_multicast_ttl_v4` | 同族 | 无 gate（`socket.rs:1604`） | ✅ |
| `set_multicast_loop_v4(bool)` | `1022`、`1110` | 无 gate | ✅ |
| `set_reuse_address(true)` | `998`、`1053` | 无 gate | ✅ |
| **`set_reuse_port(true)`** | **没有调用** | **`cfg(all(feature = "all", unix…))`（`socket2-0.6.5/src/sys/unix.rs:2237-2247`）** | Linux/mac 上「有这个 API」，但 **SDK 没开 `features=["all"]`，也没调用** |

**这条很关键**：`set_reuse_port` 在 socket2 里需要 **feature `all`** 才存在（`unix.rs:2237-2246` 的 `#[cfg(all(feature = "all", not(any(...)))]`）。所以「让 miplay-recv 在 macOS 上能绑 5353」的最小改动不只是加一行调用，还要在 SDK 的 `Cargo.toml` 里把 `socket2 = "0.6"` 改成 `socket2 = { version = "0.6", features = ["all"] }`（或者用 `libc::setsockopt` 直接干）。

### 2.6 `cpal` 的后端矩阵（回答「有没有 PulseAudio 后端」）

- 后端模块（`cpal-0.17.3/src/host/mod.rs`）：`aaudio`（android）、`alsa`（linux/bsd）、`asio`（windows+feature）、`audioworklet`、`coreaudio`（`any(macos, ios)`）、`jack`（feature + 多平台）、`wasapi`（windows）、`webaudio`、`null`（兜底）。
- **没有 PulseAudio / PipeWire 原生后端**。代码里还留着 `// TODO: Add pulseaudio here eventually.`（`src/platform/mod.rs:684`，就在 Linux `mod platform_impl` 上方）。Linux 上访问 PipeWire/Pulse 的唯一路径是 **ALSA 的插件层**（`default` → `pipewire` / `pulse`），见 §3.2.3。
- `default_host()`：Linux = `AlsaHost`，macOS = `CoreAudioHost`（`src/platform/mod.rs:685-751`）；**JACK 只有开 `jack` feature 才有**，本工作区没开（`receiver/Cargo.toml` 里 `cpal = "0.17"` 是默认 feature）。
- `DeviceTrait` 的 `description()` 与 `id()` **每个后端都实现了**（包括 ALSA `src/host/alsa/mod.rs:420-450`、CoreAudio `src/host/coreaudio/macos/device.rs:390-450`），所以两个接收端里用 `device.description().map(|d| d.name().to_string())` / `device.id()` 的代码（`airplay-recv/src/audio.rs:244-256`、`miplay-recv/src/main.rs:423-447`）在 mac/Linux 上**语义有差异但能编译**：
  - ALSA：`name()` 直接返回 PCM 名（`hw:CARD=X,DEV=0` / `default`），`id()` 也是这个字符串；
  - CoreAudio：`name()` 是人类可读设备名，`id()` 是 UID 字符串；
  - WASAPI：`id()` 是端点 ID，`name()` 是友好名。
  → 于是 `--device #2` 的「第 N 个输出设备」在三个平台上**枚举顺序不同**，部署脚本里最好用 `id()` 或名字子串，而不是序号。

---

## 3. 明确回答

### 3.1 `airplay-recv` 在 macOS

#### 3.1.1 能否编译 —— ✅

`[静态]` 依据：

1. **接收端自己的代码没有任何平台 cfg**：`main.rs`（234 行）+ `audio.rs`（446 行）全文没有 `#[cfg(windows)]`、没有 `windows-sys`、没有注册表/WASAPI API；`main.rs` 只 import `shairplay::{AudioFormat, AudioHandler, AudioSession, BindConfig, RaopServer}`（`main.rs:17`）。
2. **唯一的「Windows 味」是注释和提示语**：`audio.rs:119-121` 的注释假设 WASAPI 的 `AUTOCONVERTPCM|SRC_DEFAULT_QUALITY` 会替它重采样（**mac 上不成立**，见 3.1.3）；`main.rs:205` 打印「先放行 Windows 防火墙」。都是**文案/假设**，不是编译错误。
3. **shairplay 对 macOS 是一等公民**（§2.2），`cpal` 的 apple 依赖全是纯 Rust 绑定（§2.1）。
4. 本地 IP 探测用的是 `UdpSocket::bind("0.0.0.0:0")` + `connect("223.5.5.5:80")` 再取 `local_addr()`（`main.rs:226-232`）——**纯标准库，跨平台**，不依赖任何 Windows API。（副作用：离线/无默认路由时会拿不到 IP，需要 `--bind` 或改探测目标。）

**首次构建的额外开销**：新拉 `astro-dnssd 0.3.6` + `objc2 0.6` + `objc2-core-audio` 等（lock 里已有，但本机没下载过源码）。`[未验证]` 如果 `astro-dnssd` 的 build.rs 在 macOS 上仍 probe `dns_sd`，需要 `PKG_CONFIG_PATH` 或干脆忽略（macOS 的 `dns_sd` 在系统 SDK 里，正常情况直接 `#[link]` 成功）。

#### 3.1.2 能否 mDNS 播报 `_raop._tcp` —— ✅（而且是三种平台里最干净的）

- 走 `astro-dnssd`：`DNSServiceBuilder::new("_raop._tcp", info.port).with_name(&info.raop_name).with_txt_record(txt_map(&info.raop_txt)).register()`（`shairplay-0.10.0/src/net/mdns.rs:241-250`）。
- **进程自己不 bind 5353**：注册交给系统的 mDNSResponder，所以**没有端口冲突，也不需要防火墙放行 5353**（Bonjour 是系统进程在收包）。这是 macOS 相对 Linux 的巨大优势。
- 广播名字是 `{HWADDR}@{name}`（`raop_name`，`mdns.rs` 里由 `AirPlayServiceInfo::new` 构造），`HWADDR` 来自 `main.rs:21` 的 `DEVICE_MAC = [0x02,0x53,0x54,0x52,0x4c,0x4e]`。**这个 MAC 是硬编码常量**——如果同一网段跑两台接收端，`_raop._tcp` 名字会撞。`[源码]`
- TXT 记录是纯常量表：`RAOP_TXTVERS=1`、`RAOP_CH=2`、`RAOP_SV=false`、`RAOP_DA=true`、`RAOP_SR=44100`、`RAOP_SS=16`、`RAOP_VN=3`、`RAOP_TP=TCP,UDP`、`RAOP_MD=0,1,2`、`RAOP_SM=false`、`RAOP_EK=1`、`GLOBAL_FEATURES=0x7`、`GLOBAL_VERSION=130.14`（`mdns.rs:13-30`）。`RAOP_SR=44100` 是**写死的**，和 `--rate` 无关 → 如果为了迁就 48k 声卡改成 `--rate 48000`，广播里仍声称 44.1k，**这是既有 bug，非平台问题**，但部署时值得记一笔。
- 缺什么：**只缺 `_airplay._tcp`**（AP2），那是 `feature = "ap2"` 的事，不是 macOS 的事。iOS 的「隔空播放」列表会显示 RAOP 设备，够用。

#### 3.1.3 出声的实际差异（部署前必须知道）

- **CoreAudio 会改设备的「名义采样率」**：`cpal` 的 macOS host 在 `build_stream` 时会调 `set_sample_rate(device, config.sample_rate)`（`cpal-0.17.3/src/host/coreaudio/macos/device.rs:751`），写的是 `kAudioDevicePropertyNominalSampleRate`；并且请求的速率必须落在设备声明的 `mMinimum..=mMaximum` 区间内，否则返回 `BuildStreamError::StreamConfigNotSupported`（同文件 `110-115`）。`[依赖源码]`
  - 后果 A：`--rate 44100` 在绝大多数 Mac 内置扬声器/耳机上**能开**，但会把**全系统**输出切到 44.1k（其他 App 一起受影响）。
  - 后果 B：某些 USB DAC / HDMI 只支持 48k 时，**直接报错退出**——接收端此时给的是 `anyhow` 上下文「无法在设备上建立音频流」之类（`audio.rs:140` 的 `.with_context`），**没有自动回落**。→ §4 改点 #4。
- **没有 WASAPI 那层自动重采样**：`audio.rs:120` 的注释明确依赖 WASAPI 的 `AUTOCONVERTPCM`；CoreAudio/ALSA 都没有等价物。好消息是 `audio.rs` 自带 `LinearResampler`（`audio.rs:332` 附近）在「源采样率 ≠ 目标采样率」时会被启用（`audio.rs:398-400` 的 `needs_resample`），所以只要**开流成功**、后续 44.1k 数据进来是能对上的；问题是**开流那一步**卡在设备是否接受 44100。
- **本地网络权限**：macOS 14+ 对「接收 UDP 组播/入站连接」的 App 弹「本地网络」授权。本程序**不自己绑 5353**（Bonjour 走系统），但仍要收 `RTSP(TCP 5000)` 与 RTP UDP → **大概率会弹窗**，第一次需要人工点「允许」。`[未验证]`（Apple 对「本地网络」的判定粒度文档未联网核对；但同类音频接收端普遍会弹）
- **应用防火墙**：入站 TCP 5000 会被 mac 防火墙拦，需在「系统设置 → 网络 → 防火墙 → 选项」里允许，或用 `sudo /usr/libexec/ApplicationFirewall/socketfilterfw --add <exe>` 预授权（`[未验证]`：该路径与参数需在真机确认）。

### 3.2 `airplay-recv` 在 Linux

#### 3.2.1 能否编译 —— ✅（前提是装了 ALSA 头文件）

- 前置：`sudo apt install build-essential pkg-config libasound2-dev`（Debian/Ubuntu）或 `sudo dnf install alsa-lib-devel pkgconf-pkg-config gcc`（Fedora）。原因：`cpal` 的 `alsa = "0.11"` 在 Linux 上**不可关闭**（`cpal-0.17.3/Cargo.toml:142-144`，没有 `optional = true`），`alsa-sys` 用 pkg-config 探测 `libasound`。
- `mdns-sd` 是纯 Rust（README:54 也说 "no native dependencies"），不引入 avahi 的开发包——**但它的运行期行为要和 avahi-daemon 共存**，见下。
- 其余同 macOS：`shairplay` 无 C 依赖，`tokio` 纯异步。

#### 3.2.2 mDNS 播报与 avahi 共存 —— ⚠️ 能，但有真实风险

**事实（`[依赖源码]`）**：

- shairplay 在非 macOS 上用 `mdns_sd::ServiceDaemon`（`shairplay-0.10.0/src/net/mdns.rs:295-374`）。
- mdns-sd 的内部 socket 对 `0.0.0.0:5353` **同时设了 `SO_REUSEADDR` 和 `SO_REUSEPORT`**（`mdns-sd-0.21.6/src/service_daemon.rs:951-966`），所以**能**在 avahi 已占用 5353 的情况下 bind 成功。
- mdns-sd README 明确说和 `Avahi` 做过兼容性测试（`README.md:29`）。

**风险（`[推断]`，Linux `SO_REUSEPORT` 语义）**：

1. **单播（QU / unicast response）会被哈希分流**：Linux 对 `SO_REUSEPORT` 的 **unicast** 报文按四元组哈希选一个 socket 投递。avahi 和 mdns-sd 都在监听 `0.0.0.0:5353`，如果 iPhone 发的是**单播**查询（`QU` bit 置位，回给 5353 端口），**可能被投给 avahi**，而 avahi 不会替 shairplay 回答 → 表现为「手机偶尔发现不了设备」。
2. **组播查询没有这个问题**：发到 `224.0.0.251:5353` 的报文，凡是 `join_multicast_v4` 过这个组的 socket 都能收到 → AirPlay 扫描绝大多数是组播 `QM`，**日常使用大概率没事**。
3. **avahi 自己不会冒充你**：avahi 只为「通过它 API 注册的服务」应答，它不会凭空回答 `_raop._tcp` 的 PTR 查询 → 不会出现「搜到了但连不上」的假设备。

**缓解（按推荐顺序）**：

| 做法 | 效果 | 代价 |
| --- | --- | --- |
| 接收端专机/专容器：`systemctl mask --now avahi-daemon` | 彻底消灭竞争 | 该机失去 `.local` 解析、打印机发现 |
| 保留 avahi，但在 `/etc/avahi/avahi-daemon.conf` 里 `[server] use-ipv4=yes` + 把接收端所在网卡从 avahi 的接口列表里去掉（`allow-interfaces=` 只留 loopback 等） | 只让 avahi 管别的网卡 | 需要重启 avahi |
| 什么都不做，靠「重扫一次/多等几秒」 | 大多数场景能发现 | 偶发「看不到设备」 |

`[未验证]`：第 3 条到底多频繁，必须在真机 + 真 iPhone 上跑 30 分钟才能量化。本文不给百分比。

#### 3.2.3 声卡输出：ALSA 直连 vs PipeWire/PulseAudio 插件

- `cpal::default_host()` = **ALSA**（`cpal-0.17.3/src/platform/mod.rs:685-729`），`host.default_output_device()` = `Device { pcm_id: "default" }`（`src/host/alsa/mod.rs:119-125`、`654-660`，常量 `DEFAULT_DEVICE = "default"` 在 `86` 行）。
- **cpal 没有 PulseAudio 后端**（`src/platform/mod.rs:684` 的 TODO 还在）。所以「走 PipeWire/Pulse」这件事完全取决于 **`default` 这个 ALSA PCM 指向谁**：
  - 装了 `pipewire-alsa`（桌面发行版默认）→ `default` 是 `pipewire` → 自动进 PipeWire 混音（**共享、可与其他 App 共存**），延迟由 PipeWire quantum 决定；
  - 只有 PulseAudio → `default` 是 `pulse` → 同样共享；
  - 都没有、或用户 `--device hw:0,0` → **独占**声卡（其他 App 不能出声）。
- **`--rate 44100` 在 ALSA 上的行为与 WASAPI 不同**：`cpal` 的 ALSA 后端用 `hw_params.set_rate(config.sample_rate, alsa::ValueOr::Nearest)`（`cpal-0.17.3/src/host/alsa/mod.rs:1304` 附近，`init_hw_params` 内），**只做「就近取整」，不报错**。→ 如果 `default` 背后是**只支持 48k 的裸硬件**，44100 会被静默取整到 48000，而数据仍按 44100 送 → **音调偏高**（AirPlay 1 的 44.1k 内容 ≈ 快了 8.8%）。
  - 走 `pipewire`/`pulse`/`dmix` 这类插件时，插件自己做 SRC，没有问题；
  - 只有 `--device hw:...`（直连）才会踩坑。**部署脚本里不要给 `--device hw:*`**。
- **RT 优先级没有**：`cpal` 的 `audio_thread_priority` 是 optional feature（`cpal-0.17.3/Cargo.toml:145-147`），本工作区没开 → 音频线程是普通 SCHED_OTHER，机器忙时可能 xrun（爆音）。缓解：systemd `Nice=-10`（见 §5.2），或真的开 feature 后自行提权（不建议，feature 名与行为未验证）。
- **`--list-devices` 的序号不稳定**：ALSA 枚举顺序 = `snd_device_name_hint` 的顺序，与 `id()`（PCM 名）一致；部署时用名字/ID，别用 `#N`（§2.6）。

### 3.3 `miplay-recv` 在 macOS / Linux

#### 3.3.1 能否编译 —— ⚠️ 能过（详见 §2.4）

- `#[cfg(target_os = "android")]` 之外的分支**确实只用 cpal**（SDK `Cargo.toml`: `[target.'cfg(not(target_os="android"))'.dependencies] cpal = "0.17"`；`media.rs` 里所有 `cfg(target_os="android")` 对应 Oboe，`cfg(not(...))` 对应 cpal）。
- Windows 专属依赖/代码全部隔离（`windows-sys` 只在 `[target.'cfg(windows)']`，`discovery.rs` 里 12/372-462/555/589/966-1038/1279/2288 全是 `#[cfg(windows)]`）。
- vendored `if-addrs` shim 在非 Windows 下只少一个**没人读**的 `adapter_name` 字段（`FusionPlay.AndroidIfAddrs/src/lib.rs:22-23`、`187-188`）。
- socket2 的组播/`set_ttl_v4` 一族**没有 feature gate**（§2.5），所以 SDK 里 `create_responder_socket` 那段在 unix 上能编译。
- 唯一「依赖闭包外」的风险：**`edition 2024` + `getifs 0.6.1` 的 unix 实现**在具体发行版上是否 100% 编译通过，`[未验证]`——这属于「首次 `cargo check` 才会知道」的那类，但没有任何结构性的墙。

#### 3.3.2 发现（mDNS/Lyra）—— macOS ❌ / Linux ⚠️

先看它到底怎么发：

- **`create_responder_socket`**（`discovery.rs:991-1036`，`[源码]`）：
  - `Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))`（`995` 附近）
  - `set_reuse_address(true)`（`998`，**只有这一个 socket 选项**）
  - `bind(&SocketAddrV4::new(bind_ip, MDNS_PORT))`，`bind_ip` 默认 `0.0.0.0`（`1008-1009`，错误上下文含端口）
  - `set_multicast_if_v4(&local_ip)`（`1011`）
  - `set_ttl_v4(255)`（`1020`，注释说明这是 Windows 单播应答需要的 TTL）
  - `set_multicast_loop_v4(diagnostic_multicast_loopback)`（`1022`）
  - `join_multicast_v4(&MDNS_GROUP, &local_ip)`（`1030`）
- 常量：`MDNS_PORT = 5353`、`LYRA_CONTROL_PORT = 5353`（只是 SRV 记录里通告的端口）、`MI_CONNECT_DISCOVERY_PORT = 56_666`（只通告不 bind）（`discovery.rs:17-19`）。
- 真正的监听端口：**TCP 8899**（`protocol.rs:22` `CONTROL_PORT`，`protocol.rs:867` `TcpListener::bind((local_ip, 8899))`）、**UDP 55982**（`lyra.rs:26` `LYRA_COMMAND_PORT`，`lyra.rs:88` `UdpSocket::bind((local_ip, 55982))`）、**UDP 5353**（应答器）、外加 RTP 用的 ephemeral UDP（`media.rs:718` bind `0.0.0.0:0`）。

**macOS** ❌（`[推断]`，一条命令可验证）：

- `bind(0.0.0.0:5353)` 时只带 `SO_REUSEADDR`。macOS 的 5353 由 **mDNSResponder** 常驻持有；BSD 语义下，共享同一 UDP 端口要求**所有**绑定方都设 `SO_REUSEPORT`，只有 `SO_REUSEADDR` 会返回 `EADDRINUSE`。
- 后果：`MiPlayReceiver::start()` 在初始化发现层时失败（错误信息会带 `bind MiPlay discovery socket to 0.0.0.0:5353`，`discovery.rs:1009` 的 `with_context`），**进程直接退出**。
- 即使补上 `set_reuse_port(true)`：还有第二层问题——macOS 上多个 `SO_REUSEPORT` socket 共享 5353 时，**单播投递由内核决定给谁**，我们的应答器可能收不到手机的查询 → 手机上「看得到设备但连不上/不出声」。`[未验证]`
- **正确修法**（macOS 专用）：用系统 DNS-SD 注册服务（`astro-dnssd`，和 shairplay 同一条路），只把**精确的 Lyra 应答**留在自建 UDP socket 上；但 Lyra 的响应是「内嵌指令端点」的自定义报文（`discovery.rs` 注释描述 HyperOS 缓存会尝试回连），通用库发不出来 → 实际上等于**给 SDK 加一个 macOS 后端**，工作量远大于一行 `set_reuse_port`。`[估算]` 2–4 人日。

**Linux** ⚠️（`[推断]`）：

- Linux 的 `SO_REUSEADDR` 规则：只要**所有**绑定同一 `(addr, port)` 的 socket 都设了 `SO_REUSEADDR`，`bind` 就成功。avahi 的组播 socket（为给客户端共享而设计）**应当**设了 `SO_REUSEADDR` → SDK 的 bind **通常能成功**。`[未验证]`（avahi 源码本次未读；若失败，现象与 macOS 相同：`bind ... 0.0.0.0:5353` 报 `Address already in use`，一条命令即可确认，见 §8）
- 代价和 §3.2.2 同构：**单播 QU 查询可能被投递给 avahi**，我们的应答器收不到 → 偶发「手机发现不了/连不上」。
- 另一个 Linux-only 细节：`join_multicast_v4(&MDNS_GROUP, &local_ip)` 要求 `local_ip` 是**本机某个接口的地址**。`--ip` 由用户给（`main.rs` 里 `--ip` 是 `Ipv4Addr` 并拒绝 loopback/unspecified），SDK 的 `detect_network_interface` 在非 Windows 走 `getifs`（`discovery.rs:165/306/606/1255/2205/2255` 调 `if_addrs`）→ 能给出 `en0`/`wlan0` 这类名字。**如果 `--ip` 填了 VPN/Docker 网桥的地址，组播会发到错误接口** → 部署时务必显式传 `--ip <局域网地址>`。
- 还有一个 Linux 上「看起来能跑但其实没声音」的坑：`set_multicast_loop_v4(false)` 是默认值，但如果部署在同一台机器上跑诊断（`--diag-loopback`），需要 loopback=true 才收得到自己的包。

#### 3.3.3 「组播 API 在 mac/Linux 是否可用」逐条回答

| 问题 | 答案 | 依据 |
| --- | --- | --- |
| `join_multicast_v4` 可用？ | ✅ 三平台都有 | `socket2-0.6.5/src/socket.rs:1312`，无 gate |
| `set_multicast_if_v4` 可用？ | ✅（除 wasi） | `socket.rs:1546`，`cfg(not(target_os="wasi"))` |
| `set_ttl_v4` 可用？ | ✅ | `socket.rs:1630`，无 gate |
| `set_multicast_ttl_v4` 可用？ | ✅ | `socket.rs:1604` |
| `set_multicast_loop_v4` 可用？ | ✅ | 无 gate |
| `set_reuse_port` 可用？ | **要 `features=["all"]` + 仅 unix** | `src/sys/unix.rs:2237-2247` |
| Windows 那套 IpHelper/DNS-SD 会不会在 mac/Linux 被编译？ | **不会**，全在 `#[cfg(windows)]` | `discovery.rs` 71 处 cfg |

---

## 4. 需要改的代码点

> 行号是快照 `bcca6a1` + 未提交改动（`receiver/` 未跟踪）。**优先按符号名找**，行号会漂。

| # | 文件:行 | 现状 | 平台问题 | 建议改法 | 优先级 |
| --- | --- | --- | --- | --- | --- |
| 1 | `receiver/crates/miplay-recv/src/main.rs:380-385`（`default_state_path`，只读 `LOCALAPPDATA`）+ `:63-64`（调用点 `Identity::load_or_create`，定义在 `:331`） | 无 `LOCALAPPDATA` 时返回相对路径 `miplay-recv.json` | mac/Linux 没有 `LOCALAPPDATA` → 落到 cwd；systemd 下 cwd=`/`（不可写）→ **启动即失败** | 加 `XDG_STATE_HOME` → `$HOME/.local/state/StereoLink/`；macOS → `$HOME/Library/Application Support/StereoLink/`；兜底 `std::env::temp_dir()`。**同时**在 unit 里显式 `--state` | **P0** |
| 2 | `receiver/vendor/fusionplay/FusionPlay.MiPlaySdk/src/discovery.rs:998`（`create_responder_socket` 内，只有 `set_reuse_address(true)`）+ SDK `Cargo.toml` 的 `socket2 = "0.6"` | 只设 `SO_REUSEADDR` | macOS 无法 bind 5353（`[推断]`） | `#[cfg(unix)] socket.set_reuse_port(true)?;` + `socket2 = { version = "0.6", features = ["all"] }`；风险仍在（单播分流），**这是「先让它能起来」的最小补丁**，不是终态 | **P0（mac）** |
| 3 | `receiver/crates/airplay-recv/src/main.rs:209`、`receiver/crates/miplay-recv/src/main.rs:165-172` | 只 `tokio::signal::ctrl_c().await` | systemd/launchd 停服务发 **SIGTERM** → 不走 `server.stop()` / `controller().stop()` → **不发 mDNS goodbye**，手机端留幽灵设备；systemd 记 `failed` | `#[cfg(unix)]` 用 `tokio::signal::unix::signal(SignalKind::terminate())` 与 `ctrl_c()` 做 `select!`；`tokio` 的 `signal` feature 已开（`receiver/Cargo.toml:27`），**无需加依赖** | **P1** |
| 4 | `receiver/crates/airplay-recv/src/audio.rs:124-131`（`StreamConfig { sample_rate: rate }`）+ `:120` 注释 | 固定用 `--rate`（默认 44100）开流，注释假设 WASAPI 自动重采样 | macOS 超范围直接 `StreamConfigNotSupported`；Linux `hw:` 上静默取整 → 变调 | 开流失败时回落：先试 `--rate`，失败再试 `default_output_config().sample_rate()`（`audio.rs:112` 已经拿了 `default_config`），并用 `LinearResampler` 兜住源速率差异 | **P1** |
| 5 | `receiver/crates/miplay-recv/src/main.rs:299`、`:302`（`--help` 文本）、`:58` 附近注释 | 「Windows 网卡友好名」「`%LOCALAPPDATA%`」「WASAPI 端点常重名」 | unix 用户会被误导（`--iface` 在 Linux 要传 `wlan0`，mac 传 `en0`） | 按 `cfg`/`std::env::consts::OS` 换文案 | P2 |
| 6 | `receiver/crates/airplay-recv/src/main.rs:205`、`receiver/crates/miplay-recv/src/main.rs:159` | 打印「先放行 Windows 防火墙」 | 文案平台错误 | 同上 | P2 |
| 7 | `receiver/crates/airplay-recv/src/main.rs:215-223`（`init_tracing`） | `tracing_subscriber::fmt()` 未关 ANSI | journald 里带颜色转义 | `.with_ansi(std::io::stdout().is_terminal())`（`miplay-recv/src/main.rs:501-512` 已经是 `.with_ansi(false)`，可照抄） | P2 |
| 8 | `receiver/crates/airplay-recv/src/main.rs:21`（`DEVICE_MAC`） | 硬编码 `02:53:54:52:4c:4e` | 同网段两台设备 `_raop._tcp` 名字撞 | 加 `--mac` 参数，或从 `hostname`/机器 ID 派生后两位 | P2 |
| 9 | `receiver/crates/airplay-recv/src/main.rs:226-232`、`miplay-recv/src/main.rs:412-421`（`local_ipv4`） | 连 `223.5.5.5:80` 探测 | 无外网路由时拿不到 IP；且这会把**默认路由网卡**当地址（不一定是想要的网卡） | 加 `--ip`/`--bind` 显式覆盖（airplay 已有 `--bind`，miplay 已有 `--ip`），并在探测失败时打印可操作的提示 | P2 |
| 10 | `receiver/vendor/fusionplay/FusionPlay.MiPlaySdk/src/lib.rs:359-380` | Linux 会通告 `platform="Rust"`、`model="FusionPlay receiver"` | 小米手机端显示怪名字（不影响功能） | 加 Linux 分支：`"Linux"` / `"Linux PC"`；`--name` + `--type` 已能覆盖显示名 | P3 |
| 11 | `receiver/vendor/fusionplay/FusionPlay.AndroidIfAddrs/src/lib.rs:22-23` | `#[cfg(windows)] adapter_name` | 已兼容，**无需改** | — | — |
| 12 | `receiver/start.ps1:52-61`（`Get-NetFirewallRule` / `New-NetFirewallRule`） | Windows 专用防火墙规则 | mac/Linux 无对应物 | 新增 `receiver/packaging/`：`systemd/*.service`、`launchd/*.plist`、`ufw.rules`、`README` | P2 |
| 13 | 两个 `main.rs` 都没有 `--daemon`/pidfile | 前台运行 | 被 systemd/launchd 托管时不需要 | **不要加**（见 §5.4）；真要加，只加 `--log-stdout`/`--log-file` 之类 | — |
| 14 | 默认设备概念：`airplay-recv/src/audio.rs:112`（`default_output_device()`）、`miplay-recv/src/main.rs:449-469`（`resolve_output_device`） | 直接取「默认设备」，用 `--rate` 覆盖流配置 | Linux 的 `default` 可能是 PipeWire/Pulse 插件（共享混音，好），也可能是 `hw:`（独占）；macOS 选默认设备会**改系统采样率**；`#N` 序号在三个平台枚举顺序不同 | 启动时把「实际选中的设备名/ID/采样率」打到日志（`device_name`/`device_id` 已经存在，`audio.rs:244-256`），便于部署排障；文档/README 里明确「用 ID 或名字，不要用 `#N`」 | P2 |

**不需要改的（确认过，避免白干）**：

- `receiver/Cargo.toml` 的 tokio features 已含 `signal`（SIGTERM 方案零依赖改动）。
- `cpal` 的 `--device/#N/id/名字子串` 三套解析（`airplay-recv/src/audio.rs:258-300`、`miplay-recv/src/main.rs:449-469`）在 ALSA/CoreAudio 上都能跑，只是序号语义不同（§2.6）。
- `shairplay` 的版本/feature 不用动：macOS 分支是 crate 内建的，`ap2` 不开也能用。
- `miplay-recv` 的 `local_ipv4`、`describe`、`resolve_output_device` 都只用 cpal 公共 API，无 Windows 依赖。

---

## 5. CLI 部署建议

### 5.1 通用前提（三平台都要）

| 项 | 结论 | 依据 |
| --- | --- | --- |
| 需要 root 吗 | **不需要**。所有监听端口都 >1024：airplay `TCP 5000`（可 `--port` 改）、`UDP 5353`（mdns-sd，仅 Linux/Windows）；miplay `TCP 8899`、`UDP 55982`、`UDP 5353` | `main.rs` 常量 + SDK 端口常量（§3.3.2） |
| 需要 `CAP_NET_BIND_SERVICE` / `AmbientCapabilities` 吗 | **不需要**（同上）。只有你想 `--port 80` 才需要；不建议 | 同上 |
| 需要 `CAP_NET_RAW` 吗 | 不需要（没有用 raw socket） | — |
| 需要加 `audio` 组吗 | 只在**直连 `hw:` 设备**时需要；走 `default`（PipeWire/Pulse）不需要。稳妥做法：`SupplementaryGroups=audio`（若该组存在） | ALSA `default` 的解析路径 |
| 防火墙要开哪些端口 | airplay：`TCP 5000`、`UDP 5353`（仅 Linux/Windows）；miplay：`TCP 8899`、`UDP 55982`、`UDP 5353`；**另加 RTP 的 ephemeral UDP**（两端都是 `bind(0)`，端口不固定） | `audio.rs`/`media.rs` 的 `bind(0)` |
| 日志 | airplay：tracing → **stdout**；miplay：tracing → **stderr** + 事件 `println!` → **stdout**。托管时**两个流都要收** | `main.rs` 的 `init_tracing` |
| 前台/后台 | 两个都是**前台阻塞**（`ctrl_c()` 等信号），**没有 `--daemon`** | `main.rs:209` / `:165` |

> **RTP 端口怎么办**：RAOP 的 RTP/控制/计时端口都是 `bind(0)` 由内核分配（shairplay `handlers_ap1.rs` 里 `server_port/control_port/timing_port` 都取自 `local_addr()`）。**因此不能只放行固定端口**。实践建议：按「来源 = 局域网网段」放行 UDP 到本机（`ufw` 用 `from 192.168.0.0/16` + 不限端口，或 nftables 一条 `ip saddr 192.168.0.0/16 udp accept`），比逐端口放行靠谱。

### 5.2 Linux：systemd **user unit**（推荐）

**为什么必须是 user unit**：`cpal` → ALSA → `default` → `pipewire`/`pulse` 插件，需要连**当前用户**的 `$XDG_RUNTIME_DIR/pipewire-0` 或 Pulse socket。system unit（root）默认拿不到这些环境变量 → `default` PCM 打不开（报 `DeviceNotAvailable` 或 ALSA 的 "cannot open default"）。`[推断]`（ALSA 插件层报错文本未实测）。

文件：`~/.config/systemd/user/airplay-recv.service`

```ini
[Unit]
Description=StereoLink AirPlay receiver (RAOP)
Documentation=file:///opt/miplay/docs/platform-receiver-feasibility.md
After=network-online.target pipewire.service wireplumber.service pulseaudio.service
Wants=network-online.target
# 端口被占用时不要无限重启
StartLimitIntervalSec=60
StartLimitBurst=5

[Service]
Type=simple
ExecStart=/opt/miplay/airplay-recv --name "客厅电脑" --port 5000
Restart=always
RestartSec=2
TimeoutStopSec=5
KillSignal=SIGTERM
# 日志全部进 journald（airplay 写 stdout）
StandardOutput=journal
StandardError=journal
SyslogIdentifier=airplay-recv
# 音频线程提一点优先级，降低 xrun/爆音概率（cpal 未启用 audio_thread_priority）
Nice=-10
# 直连 hw: 设备时才需要；不存在该组时删掉这行
SupplementaryGroups=audio
# 降权但保留用户身份（user unit 本来就是该用户，通常不需要 User=）
NoNewPrivileges=yes
PrivateTmp=yes
ProtectSystem=strict
ProtectHome=read-only
# 需要读 $HOME 下的东西就删掉 ProtectHome，或改成 read-only

[Install]
WantedBy=default.target
```

文件：`~/.config/systemd/user/miplay-recv.service`

```ini
[Unit]
Description=StereoLink MiPlay receiver (Xiaomi)
After=network-online.target pipewire.service wireplumber.service
Wants=network-online.target
StartLimitIntervalSec=60
StartLimitBurst=5

[Service]
Type=simple
# 身份文件必须显式给绝对路径，否则 default_state_path() 会落到 cwd
ExecStart=/opt/miplay/miplay-recv --name "客厅电脑" --ip 192.168.1.23 --state %S/miplay/miplay-recv.json
Restart=always
RestartSec=2
TimeoutStopSec=5
KillSignal=SIGTERM
StateDirectory=miplay
StandardOutput=journal
StandardError=journal
SyslogIdentifier=miplay-recv
Nice=-10
SupplementaryGroups=audio
NoNewPrivileges=yes
PrivateTmp=yes

[Install]
WantedBy=default.target
```

安装与开机自启：

```bash
# 1) 装二进制（不要用 root 跑）
sudo install -Dm755 target/release/airplay-recv /opt/miplay/airplay-recv
sudo install -Dm755 target/release/miplay-recv   /opt/miplay/miplay-recv

# 2) 让 user unit 在没有登录会话时也活着（关键！否则重启后不会起）
loginctl enable-linger "$USER"

# 3) 启用
systemctl --user daemon-reload
systemctl --user enable --now airplay-recv.service
systemctl --user enable --now miplay-recv.service

# 4) 看日志
journalctl --user -u airplay-recv -f
journalctl --user -u miplay-recv -f

# 5) 改参数后重启
systemctl --user restart miplay-recv

# 6) 防火墙（三选一，按发行版）
sudo ufw allow 5000/tcp && sudo ufw allow 8899/tcp
sudo ufw allow 5353/udp && sudo ufw allow 55982/udp
sudo ufw allow from 192.168.0.0/16 to any   # RTP ephemeral UDP 靠这条兜底
# Fedora: sudo firewall-cmd --permanent --add-port=5000/tcp --add-port=8899/tcp \
#           --add-port=5353/udp --add-port=55982/udp && sudo firewall-cmd --reload
```

**说明几点**：

- `%S` 是 systemd 的「state home」specifier，user unit 里展开为 `$XDG_STATE_HOME`（默认 `~/.local/state`）；`StateDirectory=miplay` 会**自动创建**该目录。两者配合就不需要改 Rust 代码（改点 #1 可以延后，**但 unit 里必须写 `--state`**，否则仍会落地到 cwd）。
- `After=pipewire.service` 在**用户** manager 里有效（PipeWire 的用户实例）；如果你用的是 PulseAudio，换成 `pulseaudio.service`（或直接删掉这两个 `After=`，`Restart=always` 会兜底）。
- `ProtectHome=read-only` 与 `StateDirectory` 不冲突（后者在 `~/.local/state`，也需要写 → 如果开了 `ProtectHome=read-only`，`StateDirectory` 仍可写，因为 systemd 会对 `~/.local/state` 单独挂载）。**最稳妥：先不加 `ProtectHome`**，跑通再加。
- **不要**加 `AmbientCapabilities=CAP_NET_BIND_SERVICE`：所有端口 >1024，加了也没用；加了反而让人误以为需要 root。
- 如果坚持用 system unit（`/etc/systemd/system/*.service`），必须 `User=<你的用户名>` + `Environment=XDG_RUNTIME_DIR=/run/user/1000`（UID 换成实际值），否则 **PipeWire 连不上**。不推荐。

### 5.3 macOS：launchd **LaunchAgent**（推荐）

**为什么是 LaunchAgent 而不是 LaunchDaemon**：DAE 在 `system` 域、没有用户音频会话，CoreAudio 拿不到「当前登录用户的输出设备」（能拿到，但和登录用户抢默认设备、且会遇到 TCC/本地网络授权在 daemon 下的特殊处理）。Agent 在 `gui/$(id -u)` 域，等价于「登录用户的 App」。

文件：`~/Library/LaunchAgents/com.stereolink.airplay-recv.plist`

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.stereolink.airplay-recv</string>

    <key>ProgramArguments</key>
    <array>
        <string>/opt/miplay/airplay-recv</string>
        <string>--name</string>
        <string>Mac mini</string>
        <string>--port</string>
        <string>5000</string>
    </array>

    <!-- 崩溃/被杀后自动拉起；正常退出（0）不重启 -->
    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
    </dict>
    <key>ThrottleInterval</key>
    <integer>10</integer>

    <key>RunAtLoad</key>
    <true/>

    <!-- 前台交互优先级：保音频线程的调度 -->
    <key>ProcessType</key>
    <string>Interactive</string>
    <key>Nice</key>
    <integer>-5</integer>

    <key>StandardOutPath</key>
    <string>/Users/REPLACE_ME/Library/Logs/StereoLink/airplay-recv.out.log</string>
    <key>StandardErrorPath</key>
    <string>/Users/REPLACE_ME/Library/Logs/StereoLink/airplay-recv.err.log</string>

    <key>EnvironmentVariables</key>
    <dict>
        <key>RUST_LOG</key>
        <string>airplay_recv=info,shairplay=warn</string>
    </dict>

    <!-- 只用登录会话（Aqua），避免在 ssh/后台会话里乱起 -->
    <key>LimitLoadToSessionType</key>
    <string>Aqua</string>
</dict>
</plist>
```

文件：`~/Library/LaunchAgents/com.stereolink.miplay-recv.plist`

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.stereolink.miplay-recv</string>

    <key>ProgramArguments</key>
    <array>
        <string>/opt/miplay/miplay-recv</string>
        <string>--name</string>
        <string>Mac mini</string>
        <string>--ip</string>
        <string>192.168.1.23</string>
        <string>--state</string>
        <string>/Users/REPLACE_ME/Library/Application Support/StereoLink/miplay-recv.json</string>
    </array>

    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
    </dict>
    <key>ThrottleInterval</key>
    <integer>10</integer>
    <key>RunAtLoad</key>
    <true/>
    <key>ProcessType</key>
    <string>Interactive</string>
    <key>Nice</key>
    <integer>-5</integer>

    <key>StandardOutPath</key>
    <string>/Users/REPLACE_ME/Library/Logs/StereoLink/miplay-recv.out.log</string>
    <key>StandardErrorPath</key>
    <string>/Users/REPLACE_ME/Library/Logs/StereoLink/miplay-recv.err.log</string>

    <key>LimitLoadToSessionType</key>
    <string>Aqua</string>
</dict>
</plist>
```

装载（**注意 `launchctl load/unload` 已废弃，用 `bootstrap/bootout`**）：

```bash
UID_=$(id -u)
mkdir -p ~/Library/Logs/StereoLink

# 装二进制
sudo mkdir -p /opt/miplay
sudo install -m755 target/release/airplay-recv /opt/miplay/airplay-recv

# 装载（已存在则先 bootout）
launchctl bootout   gui/$UID_ ~/Library/LaunchAgents/com.stereolink.airplay-recv.plist 2>/dev/null || true
launchctl bootstrap gui/$UID_ ~/Library/LaunchAgents/com.stereolink.airplay-recv.plist
launchctl enable    gui/$UID_/com.stereolink.airplay-recv
launchctl kickstart -k gui/$UID_/com.stereolink.airplay-recv   # 立刻跑一次

# 状态/日志
launchctl print gui/$UID_/com.stereolink.airplay-recv | head -40
tail -f ~/Library/Logs/StereoLink/airplay-recv.out.log

# 卸载
launchctl bootout gui/$UID_ ~/Library/LaunchAgents/com.stereolink.airplay-recv.plist
```

**macOS 特有注意**：

- `KeepAlive.SuccessfulExit=false` 的含义：**非 0 退出才重启**。所以如果 miplay 因为 5353 绑定失败而退出，会每 10s 重启一次（`ThrottleInterval`）→ 日志会刷。想让「起不来就别再试」用 `KeepAlive=false` + 手动 `kickstart`；想让「永远活着」用 `KeepAlive=true`（**注意它会连优雅退出也重启**）。
- **首次运行要人工授权**：①「本地网络」弹窗（§3.1.3）②防火墙「是否允许传入连接」→ 二者都**只弹一次**，点错就要去「系统设置」里改。批量部署时建议先在目标机上手动跑一遍前台，再装 plist。
- **日志不轮转**：launchd 只会追加 `StandardOutPath`。长期跑要加 `newsyslog.d` 条目（`/etc/newsyslog.d/stereolink.conf`）或改用 `os_log`（需要改代码）。`[未验证]`：`newsyslog` 的 `J`/`N` 参数在 macOS 上的具体行为未联网核对。
- **Gatekeeper**：从源码本机构建的二进制**不需要**签名；但如果把 exe 拷到另一台 Mac（或经浏览器下载），会被 Gatekeeper 拦 → 需要 `codesign --sign -`（ad-hoc）+ `xattr -d com.apple.quarantine`，或 Developer ID + 公证。`[未验证]`
- **Apple Silicon**：`cargo build --release --target aarch64-apple-darwin`（默认就是本机 arch）；若要给 Intel Mac 用则加 `--target x86_64-apple-darwin`（交叉编译 CoreAudio 绑定没问题，都是 FFI 声明）。

### 5.4 要不要加 `--daemon` 参数？—— 不加

| 方案 | 评价 |
| --- | --- |
| 自建 `--daemon`（fork/setsid/写 pidfile/重定向日志） | ❌ 在现代 Linux/macOS 上是**反模式**：systemd/launchd 已经负责守护、重启、日志、顺序依赖；自己 fork 会让 `Type=simple` 的进程跟踪失效（systemd 认为主进程退了 → 反复重启） |
| 依赖 `Type=forking` + pidfile | ⚠️ 能用但脆弱（pidfile 残留、两次 fork 的竞态），且 Rust 里做 daemonize 要引 `nix`/`daemonize` crate，凭空加依赖 |
| 保持前台 + `StandardOutput=journal` | ✅ **推荐**（本仓库现状就是前台，什么都不用改） |
| （可选，未来）`sd_notify` + `Type=notify` | 只在需要「等到 mDNS 注册成功才算启动完成」时才有价值；要加 `libsystemd`/`sd-notify` 依赖，收益不大 |

结论：**保持前台，不要加 `--daemon`**。如果确实需要在终端里「后台跑一下就忘掉」，用 `nohup ./airplay-recv ... >/tmp/airplay.log 2>&1 &` 就够，或者直接用 systemd-run：

```bash
systemd-run --user --unit=airplay-tmp /opt/miplay/airplay-recv --name "临时"
```

### 5.5 日志建议

| 端 | 今天写哪 | 建议 |
| --- | --- | --- |
| airplay-recv | tracing 默认 fmt → **stdout**（带 ANSI，若 `ansi` feature 默认开启） | 托管时 `StandardOutput=journal` 即可；**加 `.with_ansi(false)`**（改点 #7），否则 `journalctl` 里全是 `\x1b[2m` |
| miplay-recv | tracing → **stderr**（已 `.with_ansi(false)`，`main.rs:512`）；事件 `[妙播] ...` → **stdout** | 两个流都重定向到 journal；**不要只收 stdout**，启动失败的错误在 stderr |
| 统一 | 都没有日志文件 | 交给 journald（`journalctl --user -u ...`）/ launchd 的 `Standard*Path`；不自己写文件、不自己轮转 |

### 5.6 部署后自检清单

```bash
# ---- Linux ----
systemctl --user status airplay-recv miplay-recv
journalctl --user -u airplay-recv -n 50 --no-pager
# 服务真的在监听？（5000 应该 LISTEN，5353/55982 是 UDP 不会显示 LISTEN）
ss -lntup | grep -E ':(5000|8899|55982|5353)\b'
# mDNS 广播能不能看到自己
avahi-browse -rt _raop._tcp        # airplay-recv（需要 avahi-utils）
avahi-browse -rt _mi-connect._udp  # miplay-recv
# 声卡设备清单
./airplay-recv --list-devices
aplay -L | head -20                # default 指向谁

# ---- macOS ----
launchctl print gui/$(id -u)/com.stereolink.airplay-recv | grep -E 'state|pid|last exit'
dns-sd -B _raop._tcp               # 应能看到你的 {MAC}@名字
/usr/bin/log show --last 2m --predicate 'process == "airplay-recv"' | tail -20
./airplay-recv --list-devices
```

---

## 6. 风险清单

| # | 风险 | 平台 | 概率 | 影响 | 缓解 |
| --- | --- | --- | --- | --- | --- |
| R1 | **miplay-recv 在 macOS 绑不上 5353**（只 `SO_REUSEADDR`） | mac | **高**（`[推断]`） | 进程启动即退出，服务不可用 | 改点 #2；验证命令见 §8 |
| R2 | 即使绑上，macOS 上单播应答被 mDNSResponder 抢走 | mac | 中 | 「能看到设备但连不上」 | 长期方案：SDK 加 macOS 后端（`astro-dnssd` + 保留 Lyra 应答 socket） |
| R3 | **状态文件路径落到 cwd** → systemd 下启动失败 | linux/mac | **高**（确定性） | miplay-recv 完全跑不起来 | unit 里显式 `--state`（§5.2/5.3）+ 改点 #1 |
| R4 | Linux 上 avahi 抢单播 QU 应答 | linux | 中 | 偶发「手机看不到设备」 | §3.2.2 的三种缓解 |
| R5 | `--rate 44100` 与设备不符：mac 报错退出 / Linux 变调 | mac/linux | 中 | 无声或音调错 | 改点 #4；`--device default`，**别用 `hw:`** |
| R6 | 无 SIGTERM 处理 → mDNS 不发 goodbye | 全 | 高（确定性） | 手机端残留幽灵设备；systemd 记 failed | 改点 #3 |
| R7 | macOS 首次运行的「本地网络」/防火墙授权被拒 | mac | 中 | 收不到 RTSP/RTP，但进程不报错 | 先手动前台跑一遍授权；文档写明 |
| R8 | 部署脚本仍按 Windows 路径/防火墙写 | 全 | 高（确定性） | 装不上 | 改点 #12 补 `packaging/` |
| R9 | **LGPL-3.0（shairplay）**：静态链接进 exe 后，分发时需要提供「重新链接」手段 | 全（既有） | 确定 | 许可合规 | 随包提供源码或 `.o`/`cargo vendor` 目录；本文不展开法律细节 |
| R10 | **AGPL-3.0（FusionPlay SDK）**：通过网络提供服务也可能触发源码义务 | 全（既有） | 确定 | 许可合规 | 自用/内部不构成分发；对外发布需评估；见仓库 `NOTICE`/LICENSE |
| R11 | 端口 5000 与常见服务（Flask/其他）冲突 | linux | 低 | airplay 起不来（`is_running()` 为 false，`main.rs:193` 会给出提示） | `--port 5001` |
| R12 | 多网卡（VPN/Docker）导致 `--ip` 选错接口 → 组播发错网卡 | linux/mac | 中 | 手机发现不了 | 显式 `--ip <局域网地址>`；SDK 的 `--iface` 在 unix 上填 `en0`/`wlan0` |
| R13 | `astro-dnssd` 首次构建需要联网 + 可能 probe `dns_sd` | mac | 低 | 构建失败 | 预先 `cargo fetch`；失败时查 `PKG_CONFIG_PATH`（`[未验证]`） |
| R14 | Rust 工具链 < 1.94 | 全 | 中 | `cargo check` 直接拒绝（edition 2024） | `rustup update stable` |
| R15 | CoreAudio 改全局采样率影响其他 App | mac | 低 | 其他播放器被切到 44.1k | 用 `--rate 48000`（会走内部 resampler）+ `RAOP_SR` 常量不匹配是既有问题（§3.1.2） |

---

## 7. 落地路线（如果真要上）

1. **第 0 步（不做任何改动，先验证假设）**：在真机 mac 上跑 `cargo check -p airplay-recv`、`cargo build -p airplay-recv`，然后 `dns-sd -B _raop._tcp` 确认能发现；`cargo run -p miplay-recv -- --diag-loopback` 观察 5353 是否报 `Address already in use`。**这一步就能把本文所有 `[推断]` 变成 `[实测]`。**（§8）
2. **airplay-recv**：修 #3（SIGTERM）、#4（采样率回落）、#7（ANSI）、#5/#6（文案）→ 大约半天。装 systemd user unit / launchd plist → 半天。
3. **miplay-recv Linux**：修 #1（状态路径，或在 unit 里 `--state` 绕开）、#3（SIGTERM）→ 半天；拿真小米手机验证发现率 → 视 R4 决定要不要禁 avahi。
4. **miplay-recv macOS**：先试 #2（`set_reuse_port` + socket2 `all`）→ 若仍收不到查询，评估 SDK 的 macOS 后端（2–4 人日）→ **这是唯一可能卡住的点**。
5. **打包**：新增 `receiver/packaging/{systemd,launchd,ufw}`，把 §5.2/§5.3 的模板参数化（用户名/路径/IP）。

---

## 8. 复测命令（在真实机器上一条条跑，把 `[推断]` 变证据）

```bash
# 0) 工具链
rustc --version          # 必须 >= 1.94
cargo --version

# 1) 编译（不做完整 build 时用 check 更快）
cargo check -p airplay-recv
cargo check -p miplay-recv

# ---- Linux 依赖 ----
sudo apt install -y pkg-config libasound2-dev build-essential
pkg-config --modversion alsa        # 应输出 >= 1.2

# 2) macOS：5353 到底能不能绑（验证 R1，一行 Python 就够，不需要改 Rust）
python3 - <<'PY'
import socket
for opt in (("only SO_REUSEADDR", True, False), ("SO_REUSEADDR+SO_REUSEPORT", True, True)):
    name, reuseaddr, reuseport = opt
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    if reuseaddr: s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    if reuseport and hasattr(socket, "SO_REUSEPORT"):
        s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEPORT, 1)
    try:
        s.bind(("0.0.0.0", 5353)); print(name, "-> bind OK")
    except OSError as e:
        print(name, "-> FAILED:", e)
    finally:
        s.close()
PY
# macOS 上预期：第一行 EADDRINUSE(48)，第二行 OK（若第二行也失败，说明连 SO_REUSEPORT 都不够）

# 3) 宣播/发现
dns-sd -B _raop._tcp            # macOS
avahi-browse -rt _raop._tcp     # Linux（需 avahi-utils）
avahi-browse -rt _mi-connect._udp

# 4) 声卡/采样率
./target/release/airplay-recv --list-devices
./target/release/miplay-recv --list-devices
./target/release/airplay-recv --rate 48000 --verbose   # 看 CoreAudio 是否接受

# 5) 端口占用
ss -lntup | grep -E ':(5000|8899|55982|5353)\b'
sudo lsof -iTCP:5000 -sTCP:LISTEN
```

---

## 附录 A 关键代码位置索引

| 主题 | 位置 |
| --- | --- |
| airplay 入口/参数/信号 | `receiver/crates/airplay-recv/src/main.rs:54`（parse）、`:159`（main）、`:193`（`is_running` 检查）、`:209`（ctrl_c）、`:226`（`local_ipv4`） |
| airplay 声卡 | `receiver/crates/airplay-recv/src/audio.rs:112`（默认设备）、`:124-131`（`StreamConfig`）、`:170`（`build_stream`）、`:244-256`（名字/ID）、`:258`（`find_device`）、`:332`（`LinearResampler`） |
| miplay 入口/参数 | `receiver/crates/miplay-recv/src/main.rs:200`（parse）、`:229`（`--iface`）、`:302`（`--state` 帮助）、`:380`（`default_state_path`）、`:412`（`local_ipv4`）、`:449`（`resolve_output_device`）、`:501`（`init_tracing`） |
| SDK 发现层 | `receiver/vendor/fusionplay/FusionPlay.MiPlaySdk/src/discovery.rs:17-19`（端口常量）、`:150`（非 Windows `register`）、`:464`（Windows `register`）、`:991-1036`（`create_responder_socket`）、`:1053`（IPv6 旁路） |
| SDK 平台字符串 | `receiver/vendor/fusionplay/FusionPlay.MiPlaySdk/src/lib.rs:359-380`（`host_platform` / `default_model`） |
| SDK 端口 | `discovery.rs:17`（5353）、`lyra.rs:26`（`LYRA_COMMAND_PORT=55_982`）、`protocol.rs:22`（`CONTROL_PORT=8899`） |
| if-addrs shim | `receiver/vendor/fusionplay/FusionPlay.AndroidIfAddrs/src/lib.rs:22-23`、`:187-188` |
| shairplay mdns 分流 | `shairplay-0.10.0/src/net/mdns.rs:223`（mac）、`:287`（others）、`:252`（只有 ap2 才注册 `_airplay._tcp`） |
| shairplay 平台依赖 | `shairplay-0.10.0/Cargo.toml:214-221` |
| cpal 平台 host | `cpal-0.17.3/src/platform/mod.rs:684-751`、`src/host/mod.rs` |

## 附录 B 与 `docs/platform-feasibility.md` 的关系

| 维度 | 主程序（`docs/platform-feasibility.md`） | 接收端（本文） |
| --- | --- | --- |
| 平台绑定程度 | 引擎 89% 行是 WASAPI 专用，必须重写后端 | airplay 0 行平台代码；miplay 的 Windows 代码全在 `#[cfg(windows)]` |
| Linux 可行性 | 17–28 人日（虚拟声卡/回环/SDK 切换） | **≈1–2 人日**（主要是部署脚本 + SIGTERM） |
| macOS 可行性 | 20–34 人日（回环采集无官方方案） | airplay **≈0–1 人日**；miplay **2–4 人日**（5353 后端） |
| 共同的坑 | 配置路径、防火墙、签名/公证 | 同左（另有 mDNS 端口语义） |
| 声卡模型 | Windows 端点概念（`IPolicyConfig`、WASAPI ID） | cpal 的 `DeviceId`（三平台统一 trait，语义不同） |

**一句话**：主程序是「重写引擎」，接收端是「换个打包方式」——两者不可类比。接收端的真正分水岭**不是操作系统，是「mDNS 应答器归谁」**：airplay 用系统 Bonjour（mac）/纯 Rust responder（Linux），miplay 用自建 responder 且未开 `SO_REUSEPORT`。
