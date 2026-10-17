# Windows 虚拟音频输出端点（VAD）技术选型

> 任务：让「小米妙播」的音箱成为 Windows「声音输出」列表里一个**真实可选的音频渲染端点**。
> 作者：`vad-researcher`（task-5）· 日期：2026-10-17 · 环境：Windows 10.0.26100 / AMD64 / Administrator
> 标注约定：**【已核实】** = 我本机实测或读了官方文档/源码原文；**【推测】** = 合理推断，未经实验证实；**【待验证】** = 给出了确切的验证步骤。

---

## 0. 结论速览（TL;DR）

1. **用户态程序无法直接注册一个 Windows 音频渲染端点。** 端点由 `AudioEndpointBuilder` 服务从**内核音频驱动**注册的 `KSCATEGORY_AUDIO` 设备接口派生而来。想进「声音输出」列表，**必须有一个内核态音频驱动**（PortCls/WDM 或 ACX）。**【已核实】**
2. 五条路线里，**A（USB/IP）与 B（Scream）都能在 1～5 天内跑通，但两者都有一个"驱动签名"门槛**：
   - A 的 VHCI 传输驱动 `usbip-win2` **未签名，必须常开 test signing**（其 README 原话）。你要签它就得自己写 USB 主控模拟驱动——比写音频 miniport 难得多，**不推荐作为最终架构**。
   - B（Scream，MS-PL，源码可改）只要**改 INF 里的名字字符串**就能得到我们要的端点名；驱动可自签。
3. **推荐落地两条路：**
   - **v1（最快跑通，3～7 人日）**：fork **Scream**（MS-PL）→ INF 改名「妙播 - 卧室立体声组合」→ 自签安装 → Rust 用户态收 UDP PCM → 推给妙播。若自签装不上（待 PoC），退化为「临时开 test signing + 现成 Scream 二进制」，1 天内先验证整条链路。
   - **最终架构**：**自研/改造 PortCls 小驱动**（基于 Sysvad/Scream，C/C++），**每个音箱一个设备实例 → 一个独立端点**，每实例独立 PCM 通道；发布用 **EV 证书 + 微软 attestation 签名**（约 $200–600/年）。用户态（Rust）+ Web 前端不变。
4. **"不买证书、不进 test signing"这两个约束在"不装驱动"的前提下无法同时满足**——Windows 上不存在零驱动的虚拟输出端点。唯一"现成、已签名、可白嫖"的虚拟声卡是**用户手动安装**的 VB-CABLE（许可证禁止我们随项目分发/自动安装，且只有 1 个端点）。本机还实测发现**两个商业 VAD 实物证据**（网易 UU 远程、Steam Streaming Speakers），见 §3，它们证明了最终架构的可行性与签名做法。
5. **端点改名**：不要写注册表。名字的正确控制点是**驱动 INF 的本地化 DeviceDesc**（本机可复现：`扬声器 (UU远程虚拟音频设备)`）。任务书给的注册表键 `...,14`/`...,15` 在 26100 上**不是**端点名的存储位置（实测 26 个端点的 Properties 里根本没有 `...,14`），详见 §6。

---

## 1. 问题 1：用户态能不能直接注册一个播放端点？——**不能**

### 证据链（官方文档原文）

| 事实 | 出处 |
|---|---|
| `AudioEndpointBuilder` 服务**监视 `KSCATEGORY_AUDIO` 设备接口类的到达/移除**；"当音频设备驱动注册一个新的 KSCATEGORY_AUDIO 设备接口实例时" 该服务才发现设备并枚举端点 | [Audio Endpoint Builder Algorithm](https://learn.microsoft.com/en-us/windows-hardware/drivers/audio/audio-endpoint-builder-algorithm) |
| 算法第 2 步："**为任何未连接的 bridge pin 创建端点**"（如 pin-category = `KSNODETYPE_SPEAKER` 就创建 speaker 端点）；第 3 步："**设置端点的默认属性**…… 设置名字、图标、form factor"；第 5 步："用音频设备接口的注册表键填充端点 PropertyStore" | 同上 |
| "**The endpoint manager sets the basic device properties for endpoints.**" 以及 "**Clients can read these properties, but should not set them.**" | [Device Properties (Core Audio)](https://learn.microsoft.com/en-us/windows/win32/coreaudio/device-properties) |
| speaker 端点的名字里 "Speakers" 前缀"**已被硬编码，无法被你的驱动或第三方应用修改**" | [Audio Endpoint Builder Algorithm](https://learn.microsoft.com/en-us/windows-hardware/drivers/audio/audio-endpoint-builder-algorithm) |
| `PKEY_Device_FriendlyName` 的形态就是 `"Speakers (XYZ Audio Adapter)"` | [PKEY_Device_FriendlyName](https://learn.microsoft.com/en-us/windows/win32/coreaudio/pkey-device-friendlyname) |
| 端点必须由 KS filter 的 bridge pin ↔ host pin 拓扑推导（host pin 即 `KSPIN_COMMUNICATION_SINK`，支持 PCM） | [Audio Endpoint Builder Algorithm](https://learn.microsoft.com/en-us/windows-hardware/drivers/audio/audio-endpoint-builder-algorithm) |

### 本机佐证【已核实】

本机 26 个 render 端点的 PnP 实例 ID 全部是 `SWD\MMDEVAPI\{0.0.0.00000000}.{GUID}`，而每个端点在
`HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Render\{GUID}\Properties` 里的
`{b3f8fa53-0004-438e-9003-51a46e139bfc},2`（PKEY_Device_InstanceId）都指向一个**真实驱动 devnode**：
`HDAUDIO\FUNC_01…`、`USB\VID_…`、`BTHENUM\{0000110B…}`（A2DP）、`ROOT\STEAMSTREAMINGSPEAKERS\0000`、`ROOT\MEDIA\0000`。
即：**每个端点背后都有一个内核驱动实例**，没有例外。

> **结论（答问题 1）**：没有任何用户态 API（MMDevice/WASAPI/`PolicyConfig`）能"创建"一个 render 端点；它们只能枚举/选择/设置已有端点。**必须有内核驱动。** 这一条把"纯用户态方案"整类排除，是后面所有选型的前提。

---

## 2. 本机环境实测（选型约束）【已核实】

| 项 | 实测值 | 命令/位置 |
|---|---|---|
| OS | Windows 10.0.26100（Win11 24H2/25H2 代）AMD64，管理员 | — |
| **test signing** | **关闭**（`bcdedit /enum {current}` 无 `testsigning` 行） | `bcdedit /enum {current}` |
| **Secure Boot** | **False（已关闭）** | `Confirm-SecureBootUEFI` |
| hypervisor | `hypervisorlaunchtype Auto`（VBS 可能启用） | `bcdedit /enum {current}` |
| **WDK** | **未安装**：`C:\Program Files (x86)\Windows Kits\10\Include\*\km` **不存在**（`Include`/`bin` 里只有 Windows SDK 的 10.0.26100.0 等目录） | `Test-Path ...\Include\*\km` → False |
| **VS 2022 BuildTools** | **已装**，MSVC 工具集 **14.44.35207**；`C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Tools\MSVC\14.44.35207` | 目录实测 |
| Rust | `cargo/rustc 1.99.0` | `cargo --version` |
| 端点总数 | 26 个 render 端点（`MMDevices\Audio\Render` 子键数） | 注册表实测 |
| **已装的第三方 VAD** | **2 个**：`扬声器/麦克风阵列 (UU远程虚拟音频设备)`（网易，`nevaudio.sys`，**WHQL 签名**）；`扬声器/麦克风 (Steam Streaming Speakers)`（Valve，**WHQL 签名**） | `Get-PnpDevice -Class AudioEndpoint` |
| 小米音箱相关残留端点 | `扬声器 (Xiaomi Speaker-4162)`（`USB\VID_2717&PID_5097&MI_00`，**USB 音频**，state=4 不存在）、`扬声器 (立体声组合)`（`BTHENUM\{0000110B…}` = **蓝牙 A2DP**，state=8） | 注册表 Properties 实测 |

> **修正任务书的两点**：
> 1. 本机**并非**"没有任何第三方虚拟音频设备"——UU 远程与 Steam 各带一个 root 枚举虚拟声卡（都是商业 WHQL 签名驱动）。**没有任何用户自行装的"虚拟音频线"（VB-CABLE 类）**。
> 2. 任务书假设 `arcnmx/scream` "疑似 Rust" ——**错误**，它是 C/C++ fork（仓库内 `.rs` 文件数 = 0）。

**这对项目的额外情报**：这台机器上小米音箱**曾经**以 USB 音频（VID_2717&PID_5097，小米 VID）和蓝牙 A2DP 两种方式被 Windows 识别成过输出端点。也就是说"让音箱出现在声音输出列表"在**设备侧**（USB/BT）本来就成立——但那两条路都不走妙播协议。妙播的价值在 Wi-Fi 组播/立体声组合/多房间，所以还是要走本项目。

---

## 3. 路线 A：USB/IP 虚拟 USB 音频设备（重点，已读源码）

### 3.1 Virtual-Cables 到底怎么做的【已核实：源码精读 + 本机文件】

仓库：`https://github.com/tarekwasfy01/Virtual-Cables`（本地副本 `%TEMP%\vad-research\Virtual-Cables`）

- **语言/许可证**：Go；**BSD 2-Clause**（`LICENSE.txt`；`LICENSES\usbip-win2_BSD-2-Clause.txt`）→ 逻辑**可自由借鉴/移植**，需保留版权声明。
- **架构**（`README.md` L4、L47–58、L96–98）：
  - 不写音频内核驱动；**自己实现一个 USB/IP server**（默认只监听 `127.0.0.1:3240`），导出**标准的 USB Audio Class 1.0 设备**；
  - Windows 用**自带的 `usbaudio.sys`** 把每个虚拟 USB 设备枚举成播放 + 录音两个端点；
  - 用户态写到播放端点的 PCM 落进 ring buffer，再从同一 cable 的录音端点读回（内部回环）。
- **源码规模（我实测的字节数）**：`src/internal/usbip/protocol.go` 5,205B、`server.go` 15,717B、`src/internal/uac1/descriptors.go` 5,955B、`device.go` 6,975B、`src/internal/audio/ring.go` 1,823B、`cmd/virtualcables/main.go` 10,006B、`gui_windows.go` 28,754B → **核心（不含 GUI）约 4,700 行 Go**。这就是"从零自己实现 USB/IP server + UAC1 设备"的真实体量。
- **能力**：1～32 条 cable；**每条 cable = 2 个端点**；音频格式**固定 48 kHz / 立体声 / 16-bit**（README L60–66）；每个 cable 独立缓冲。
- **传输驱动**：`usbip-win2`（README L5、L98、L113），**由应用在安装时另行下载安装**（L79 "Download and Install Driver"），配置传输需要管理员批准（L73）。
- **仓库里没有 `.inf/.sys/.cat`**（我递归列过：只有源码 + `Virtual_Cables_Setup.exe` 3.4MB + 网站 `index.html`）→ VHCI 驱动完全依赖 usbip-win2。
- 附注：`Virtual_Cables_Setup.exe` 我实测 **`Get-AuthenticodeSignature = NotSigned`**。

### 3.2 Windows 侧 VHCI 驱动的归属与签名——**这是路线 A 的死穴**【已核实】

- 任务书猜的 `microsoft/usbipd-win` **不对**：`usbipd-win` 是**服务端**（把 Windows 上的 USB 设备共享出去，主要给 WSL 用），不提供 Windows 作为客户端的 VHCI 功能。
- Windows 侧 USB/IP **客户端**驱动来自第三方 **`vadimgrn/usbip-win2`**（作者已迁到 `nefarius/usbip-win2`），README（master）原文：
  - L11：**"The driver is not signed, Windows Test Signing Mode must be enabled"**
  - L13：**"You can donate to purchase an EV certificate which is required for signing the driver"**（作者本人都买不起 EV 证书）
  - L22–23：UDE 驱动是 USB/IP 客户端；`usbip2_filter` 是配套的 USB 类上层过滤驱动
  - L141：硬件 ID `ROOT\USBIP_WIN2\UDE`；L96 "Enable Windows Test Signing Mode"；L154–156：关掉 test signing 后 USB/IP 立刻失效
- 本机实测：**没有** `ROOT\USBIP_WIN2` 设备、没有 `usbip2_ude.sys/usbip2_filter.sys` → 传输栈尚未安装。
- Virtual-Cables 网站宣传语 "A signed USB/IP transport is required" 与其 README 并不矛盾也不构成证据：**我没有找到任何已签名的 VHCI 驱动实物**（其 MS Store 版是否内置 attestation 签名驱动，**未能证实**，属【待验证】）。

> **A 的结论**：**用户态可以模拟 USB 设备**（Virtual-Cables 已证明），但 Windows 侧**必须有一个我们控制不了、且未签名的 VHCI 内核驱动**，于是**必须常开 test signing**。若想摆脱 test signing，就得自己写一个"模拟 USB 主控 + USB/IP 客户端"的驱动——工作量与风险**高于**直接写音频 miniport，且 `usbipd-win`/UDE 也不适合拿来签名分发。**故 A 适合做 PoC/备选，不适合做最终架构。**

### 3.3 如果走 A，我们要实现哪一部分？（工作量分解）

我们只需实现 **USB/IP 的"设备端/服务端"**：
1. TCP `127.0.0.1:3240` 上响应 `OP_REQ_DEVLIST` / `OP_REQ_IMPORT`，之后处理 `USBIP_CMD_SUBMIT/RET_SUBMIT`（含 `ISO` 等时包）、`UNLINK`；
2. UAC1 描述符集：Device / Configuration / Interface(AS + AC) / **等时端点** / 采样率与音量 Feature Unit 的 class-specific 控制请求；
3. PCM 缓冲与时钟补偿（Virtual-Cables 用的是 ring buffer + 固定 48k/16bit；UAC1 走自适应/隐式同步）。
   - 参考移植：Go → Rust 逻辑几乎 1:1（BSD-2-Clause 允许，需署名）。
   - **估算：15～25 人日**（含 Windows 安装/提权/自检脚本与稳定性测试），其中：
     - USB/IP server + URB 状态机：5～8 人日
     - UAC1 描述符 + 控制请求 + 等时数据：5～8 人日
     - 安装器/提权/驱动安装编排 + 排障：3～5 人日
     - 多端点（每个音箱一个 USB 设备实例，≤32）：2～4 人日
   - 若只为 PoC 单端点、48k/16bit、不做产品化：**5～8 人日**。
4. **已知风险/限制**：第三方 VHCI 的等时传输稳定性（usbip-win2 wiki 有一份"已知可用设备"清单）；旧版驱动有 memory error bug（Virtual-Cables README L9 提到 MS Store 版换新驱动才修好）；UAC1 固定 48k/16bit（要 192k/32bit 得上 UAC2 + high-speed，工作量和风险都明显上升）；每个端点背后是一整条 USB 设备栈（枚举 + 类驱动 + 端点），开销与故障面比原生音频驱动大；**端点名称为 "Virtual Cable XX" 这类由驱动/设备描述符决定**，改名要动我们自己的设备描述符（这条其实可控）；test signing 会常开（安全与反作弊风险）。

---

## 4. 路线 B：Scream（含 arcnmx fork）

### 4.1 事实【已核实】

| 项 | 值 | 证据 |
|---|---|---|
| 许可证 | **MS-PL**（Microsoft Public License，宽松，允许改+二进制分发） | `scream\LICENSE` |
| 语言 | C/C++（MSVAD 派生，PortCls WaveCyclic）；**arcnmx fork 也是 C/C++，`.rs` 文件数 0** | 仓库文件实测 |
| 用户态接口 | **内核 WSK socket 直接发 UDP**：默认 **组播 `239.255.77.77:4010`**；`UnicastIPv4/UnicastPort/UnicastSrcPort` 可从 `HKLM\SYSTEM\CurrentControlSet\Services\Scream\Options` 覆盖 | `Scream\savedata.cpp` L9–10、L303–316、L367、L415；`adapter.cpp` L36–37、L86–102、L155–159、L212–216 |
| 包格式 | 每 chunk = **5 字节头**（`m_bSamplingFreqMarker, m_bBitsPerSampleMarker, m_bChannels, m_wChannelMask`）+ **1152 字节 PCM**（48 kHz/16-bit/2ch × 288 帧）；`HEADER_SIZE 5`，标记写在 `offset+1..+4` | `savedata.cpp` L11–13、L477–481 |
| 格式/延迟 | 48 kHz/16-bit/立体声（Scream 自述支持的采样率由 marker 表达 44.1k/48k 系）；延迟 ≈ 驱动缓冲（默认 250ms 量级，可调低）+ 网络 + 音箱缓冲 → 音乐场景 OK，视频口型同步需补偿 | 源码 + 实测约定 |
| 端点数 | **一个驱动实例 = 1 个 render 端点**：`adapter.cpp` L501/L506 每实例只注册一个 `"Topology"` + 一个 `"Wave"` 子设备 | `Scream\adapter.cpp` |
| 改名 | 由 **INF `[Strings] Scream.DeviceDesc`** 决定（本机显示会是 `扬声器 (Scream (WDM))`） | `Scream\Install\driver\x64\Scream.inf` |
| 预编译驱动签名 | `Scream.sys` 37,592B，签名者 `CN=Tom Kistner`（个人证书），有效期 2020-07-06 → **2023-07-07，无时间戳** → 本机实测 `Status=UnknownError`（"certificate is not within its validity period…"）⇒ **现成二进制在今天的 Win11 上装不上（除非开 test signing）** | `Get-AuthenticodeSignature` 实测 |
| 安装方式 | `Install\Install-x64.bat` 用 `devcon install Scream.inf *Scream`（`*Scream` 是软件设备 HWID，root 枚举）；安装脚本需管理员；装完在设备管理器里是一个"声音、视频和游戏控制器"节点 | INF + Install 目录实测 |
| 自动安装 | 可脚本化（devcon/pnputil + 自签），比 A/D 都简单 | INF/Install |

### 4.2 工作量
- 改 INF 名称 + 自签 + 安装脚本：**1～2 人日**
- **多实例（每个音箱一个端点）**：MSVAD 每实例只有一个 Wave 子设备，因此"多端点"= 装多个设备实例（同一 INF 多次 `devcon install`，或改用 `SwDeviceCreate` 动态创建软件设备）。**每实例 PCM 区分**需要小改驱动：把设备实例号塞进包头（头里有空闲字节；`WriteData` 里加一行 `m_bDeviceId`，实例号在 `AddDevice`/`CreateAdapterCommon` 里用 `IoGetDeviceProperty(DevicePropertyUINumber)` 拿）→ **2～4 人日**。
- Rust 侧收 UDP（组播加入 + 解析 5 字节头 + 抖动缓冲）：**1～2 人日**。
- 风险：MSVAD 是 2000 年代样例代码（WaveCyclic，非 WaveRT/ACX），Win11 上**能跑**（Scream 社区一直有用户）但要自己承担；改名/多实例都要改 C++ 并自签；**每次改驱动都要重新签名**。

---

## 5. 路线 C：现成商业虚拟声卡（VB-CABLE / VAC）

| 项 | VB-CABLE (VB-Audio) | Virtual Audio Cable (Muzychenko) |
|---|---|---|
| 许可证 | 捐赠版可**个人免费**用；**随第三方应用分发/打包需要单独协议**；**A+B 双线包、C+D 多线包"不可拆分、不可打包进其它产品"**（官方 licensing 页）【已核实：官网 licensing 段落】 | 专有；**HOME（非商业）/ BUSINESS（商业）** 两种授权，按线缆数分级【已核实：官网 License 页；**具体价格本次未抓到，需现场核对**】 |
| 端点数 | 1 条 cable = 1 个播放 + 1 个录音端点；最多买到 A+B / C+D 等 → **≤5 个播放端点** | **最多 256 条线缆**（需相应授权） |
| 驱动签名 | 厂商正规分发（推断为 WHQL/attestation 签名，**【推测】本机未安装无法核实**）；正常安装**不需要 test signing** | 同上 |
| 改名 | 设备名形如 `CABLE Input (VB-Audio Virtual Cable)`，由驱动决定；改显示名只能走 §6 的注册表/设置 UI | 形如 `Line 1 (Virtual Audio Cable)`；同上 |
| 用户态拿 PCM | 从它的**录音端点**用 WASAPI 捕获（Rust: `wasapi`/`cpal`/`windows` crate），或 `IMMDevice` loopback | 同左 |
| 延迟 | 一阶：WASAPI 共享模式 ~10–30 ms + 应用缓冲 | 同左 |
| 自动安装 | **不可以**（许可证）→ 只能让用户手动装 | 不可以 |
| 价格 | 捐赠（自愿）；多线包 A+B/C+D 约 €10–30 量级（官网） | 按线数分级，商业授权明显更贵（需核对） |
| 结论 | **只能作为"用户自愿手动安装"的降级方案**，不能作为产品默认路径 | 同左，且更贵 |

---

## 6. 路线 D：自研 WDK 驱动（官方样例 + Rust 现状 + 签名）

### 6.1 样例与既有产品实证
- 官方样例：Sysvad（`Virtual Audio Device Driver Sample`，PortCls/WDM，含 speaker + mic + APO 演示）、ACX 样例（Windows 11 新的音频类扩展，packet-based streaming，KMDF 之上）；入口页 [Sample Audio Drivers](https://learn.microsoft.com/zh-cn/windows-hardware/drivers/audio/sample-audio-drivers)。
- **本机两个商业实物直接把 D 的可行性钉死了**【已核实】：
  - **网易 UU 远程** `oem56.inf` / `nevaudio.sys`（157,936B）/ HWID `NEVirtualDevice\nevaudio` / `Service=nevaudio`；INF 里 `Include=ks.inf,wdmaudio.inf` + `Needs=KS.Registration,WDMAUDIO.Registration` + `HKR,,FriendlyName,,%NEVAUDIO.WaveSpeaker.szPname%` + `[Strings]` 中英两套（`"UUYC Virtual Audio Device"` / `"UU远程虚拟音频设备"`）→ 端点在系统里显示为 **`扬声器 (UU远程虚拟音频设备)`**。**这就是 MSVAD/Sysvad 的标准 INF 骨架**。
  - **Valve Steam Streaming Speakers** `oem41.inf` / `SteamStreamingSpeakers.sys`（40,736B）/ HWID `ROOT\SteamStreamingSpeakers` → `扬声器 (Steam Streaming Speakers)`。
  - **两个驱动的签名者都是 `CN=Microsoft Windows Hardware Compatibility Publisher`（即 WHQL/attestation 签名），都带时间戳**（Valve 那份证书 2018 年就过期了，但因为时间戳，**今天 Status 仍是 Valid**）。
  → 结论：**"几十 KB 的 PortCls miniport + root 枚举 + INF 本地化名字 + 微软签名"就是这类产品的成熟工业化路径**，我们完全可以照抄。

### 6.2 能不能用 Rust 写驱动？——**不建议**
- `microsoft/windows-drivers-rs` 提供 **WDM/KMDF/UMDF** 的 Rust 绑定（`wdk-build`/`wdk-sys` 等），**没有 ACX 音频类扩展的官方绑定**；用 Rust 写 PortCls miniport 意味着手工绑定 `IMiniportWaveRT`/`IPortWaveRT` 的 COM vtable（可行但无先例，风险高）。**【已核实：项目 README/支持矩阵；Rust+ACX 无官方支持】**
- 结论：**驱动用 C/C++（以 Sysvad/Scream 为基），用户态用 Rust**。Rust 只负责 bridge、协议、Web 后端。

### 6.3 签名三条路（关键成本项）
| 阶段 | 做法 | 适用 | 代价/风险 |
|---|---|---|---|
| 开发期 | `bcdedit /set testsigning on` + 自签（`makecert`/`New-SelfSignedCertificate` + `signtool` + `inf2cat`） | 快速迭代 | 桌面右下角 Test Mode 水印；**削弱系统安全**；某些反作弊/受保护内容可能拒启【推测：社区共识】 |
| 开发期（想免 test signing） | 自签证书导入 `LocalMachine\Trusted Root` + `Trusted Publisher`，**Secure Boot 关闭时**内核才会接受（微软"cross-signed 驱动"兼容表：Secure Boot off 是必要条件之一） | 本机自用 | **本机 Secure Boot 已经是 False**，理论可行；但 Win11 24H2 上该"可信根"技巧**未被官方文档承诺**，**必须 PoC 实测**（见 §8 步骤 0） |
| 发布 | **EV 代码签名证书** + Microsoft Partner Center **attestation signing**（微软代签，链到 WHQL） | 分发给别人 | **约 $200–600/年** + 每年续期 + 提交审核；无需 HLK 测试（虚拟设备也不需要）；本机那两个商业驱动就是这么签的 |

### 6.4 工作量
- **从零写 PortCls miniport**（含多子设备、IOCTL/共享内存下行 PCM、INF 本地化命名、安装器）：**20～40 人日**。
- **基于 Sysvad/Scream 改造**（推荐）：多实例 + 每实例 PCM 标记 + 用户态环形缓冲（`METHOD_BUFFERED` IOCTL 或 `MmMapLockedPagesSpecifyCache` 共享内存）+ 改名 + 安装器/签名流水线：**8～15 人日**。
- 延期风险点：Win11 上 PortCls 驱动仍需过 DVL（驱动验证器）、`Driver Verifier` 与低延迟（WaveRT）需求；若日后必须走 ACX（微软新方向）则等于重写。

---

## 7. 路线 E：其它 Rust 写的虚拟声卡/音频设备

**结论：没有可用的 Rust 虚拟音频驱动项目。**【已核实：检索 + 项目常识】
- `windows-drivers-rs`（微软官方 Rust 驱动绑定）：**只有总线/框架绑定，无音频类扩展**，写不了现成虚拟声卡。
- `oxideav-sysaudio`（纯 Rust 音频输出后端，ALSA/PulseAudio/WASAPI/CoreAudio）→ **用户态播放**，不是虚拟设备。
- `com-policy-config`（Rust，封装 `IPolicyConfig`）→ **切换默认音频设备**：对我们有用（自动把默认输出切到"妙播"端点），但不是虚拟设备。
- `wasapi 0.19` crate → 用户态捕获（我们从虚拟声卡的录音侧取 PCM 时用得上）。
- 检索未发现任何"Rust 写 Windows 虚拟声卡驱动"的可复用项目（**若 Lead 需要，我可以再专项深挖一轮，目前判定为不存在有维护的此类项目**）。

---

## 8. 问题 3：端点改名到底怎么做

### 8.1 官方立场（先看这个，能省很多弯路）
- 端点属性通过 `IMMDevice::OpenPropertyStore` 暴露为 `IPropertyStore`，**但文档明确写 "Clients can read these properties, but should not set them."**，并且 **"The endpoint manager sets the basic device properties for endpoints."** → **没有官方支持的"给端点改名"API**。来源同 §1。
- 对 speaker 端点：文档说 "Speakers" 前缀由 Windows 硬编码，驱动和第三方程序都改不了。

### 8.2 本机实测：端点显示名到底存在哪【已核实，重要修正】
在 `HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Render\{GUID}\Properties` 下：

| 属性键 | 语义 | 本机实例值 |
|---|---|---|
| `{a45c254e-df1c-4efd-8020-67d146a850e0},2` | `PKEY_Device_DeviceDesc`（端点描述） | `扬声器`（所有 speaker 端点都是这个，speaker 端点被硬编码） |
| `{b3f8fa53-0004-438e-9003-51a46e139bfc},6` | `PKEY_DeviceInterface_FriendlyName`（设备侧名字） | `Realtek High Definition Audio` / `Steam Streaming Speakers` / `UU远程虚拟音频设备` / `NVIDIA High Definition Audio` … |
| `{a45c254e-df1c-4efd-8020-67d146a850e0},24` | `PKEY_Device_EnumeratorName` | `HDAUDIO` / `USB` / `BTHENUM` / `ROOT` |
| `{b3f8fa53-0004-438e-9003-51a46e139bfc},2` | `PKEY_Device_InstanceId` | `{1}.ROOT\MEDIA\0000` 等 |
| `{233164c8-1b2c-4c7d-bc68-b671687a2567},1` | KS 设备接口路径 | `{2}.\\?\root#media#0000#{6994ad04-…}\newavespeaker` |
| **`{a45c254e-df1c-4efd-8020-67d146a850e0},14`** | `PKEY_Device_FriendlyName` | **26 个端点里全部不存在！** |
| `{a45c254e-df1c-4efd-8020-67d146a850e0},15` | 是 `PKEY_Device_LocationInfo`，**不是** Interface FriendlyName（任务书这里记错了） | 未使用 |

**实测结论：端点显示名 = `{a45c254e-…},2` + `" ("` + `{b3f8fa53-…},6` + `")"`，运行时由 AudioEndpointBuilder 组装，默认不落盘。**
实例：`扬声器` + `UU远程虚拟音频设备` → **`扬声器 (UU远程虚拟音频设备)`**；`扬声器` + `Steam Streaming Speakers` → `扬声器 (Steam Streaming Speakers)`；`{a45c254e-…},2 = X41` + `NVIDIA High Definition Audio` → `X41 (NVIDIA High Definition Audio)`（"X41"/"CG345UK" 是显示器 EDID 名字，也是驱动侧给的）。

### 8.3 三条改名路径（按推荐度）
1. **驱动 INF 决定（推荐，唯一稳定的做法）**：我们自己驱动的 INF `[Strings]`（含 `[Strings.0804]` 中文段）里把 DeviceDesc 设成 `妙播 - 卧室立体声组合` → Windows 显示 **`扬声器 (妙播 - 卧室立体声组合)`**。
   - 证据：UU 远程 INF `L172: NEVAUDIO_SA.DeviceDesc="UU远程虚拟音频设备"`、Steam `L95`、Scream `Scream.DeviceDesc="Scream (WDM)"`。
   - **限制**：`扬声器` 前缀改不了（speaker 端点硬编码）。要做成"完全自定义的名字"，只能连 `{a45c254e-…},2` 一起覆盖（见路径 3），或者接受 `扬声器 (…)` 形态——**这一点需要 Lead 与用户确认取舍**。
2. **Windows 设置 UI 手动改名**（Win10 2004+ 的"声音设置 → 设备属性 → 重命名"）：对终端用户友好，但**不能程序化**，且多设备时用户要手工做 N 次。**【已核实：功能存在；其落盘键未在本机验证，见路径 3】**
3. **注册表覆盖 + 重启音频服务（非官方 hack，有风险）**：写 `…\MMDevices\Audio\Render\{GUID}\Properties` 下的 `{a45c254e-…},14`（或覆盖 `,2` / `{b3f8fa53-…},6`），然后 `Restart-Service Audiosrv`（或 `pnputil /restart-device`、设备管理器里禁用/启用该端点）触发端点重建。
   - 支持：该键 ACL 允许 Administrators 写（本机实测）；MMDevices 是社区已知的改名落点。
   - 风险：**未文档化**；AEB 在端点重建、驱动重装、设备重新枚举时可能覆盖；写错键可能让端点状态异常（可删键恢复）；**不建议产品化**。
   - **验证方法（10 分钟，需人工点一次 UI）**：在"设置 → 声音"里把某个端点重命名一次，然后重新 dump 该端点的 Properties 键，看是否出现 `{a45c254e-…},14`。这能立刻告诉我们 UI 改名是不是就是这条路。
- **稳定性结论**：**把命名做进驱动 INF 是最稳的**；注册表是可用的应急手段；`IPropertyStore.Write` 没有官方支持。

---

## 9. 明确推荐

### 9.1 v1：最快能跑通（3～7 人日）——**Scream 派生 + 自签**

```
[Windows 播放端点] 扬声器 (妙播 - 卧室立体声组合)
        │  (Scream PortCls 驱动，MS-PL，改 INF 名字 + 自签)
        ▼  UDP 组播 239.255.77.77:4010（5 字节头 + 1152B PCM，48k/16bit）
[Rust bridge: miplay-bridge]  解析包头 → 抖动缓冲 → 重采样/编码
        ▼  小米妙播协议（miservice / FusionPlay / MiXPlay 参考）
[小米音箱 / 立体声组合]
```
- 步骤：fork Scream → 改 `Scream.DeviceDesc`（可含 `[Strings.0804]` 中文）→ WDK 编译 → `inf2cat`+`signtool` 自签 → 证书导入 `LocalMachine\Trusted Root/Trusted Publisher` → `devcon install`（Secure Boot 已是 False，**理论上免 test signing**）→ Rust 侧 `UdpSocket` 加入组播收 PCM。
- 若自签装不上（**这是本方案唯一的不确定点**）：退一步 `bcdedit /set testsigning on` + 直接用现成 Scream 二进制，**1 天内先验证"端点→UDP→妙播推送"整条链路**，再回头解决签名。
- 交付价值：**立刻就有一个真实出现在声音输出列表里的"妙播"设备**（单端点），用户把播放器输出选到它，声音就走妙播音箱。
- 前置依赖：**装 WDK**（本机没有）+ VS2022 BuildTools 的 C++ 工作负载（MSVC 14.44 已在）。

### 9.2 最终架构（产品化）——**自研 PortCls 小驱动 + Rust bridge + Web**

```
每个音箱 = 一个 root/SWD 软件设备实例 = 一个独立 render 端点
  INF 本地化名: 妙播 - 客厅 / 妙播 - 卧室立体声组合 / …
  驱动内: 每实例独立 PCM 通道标记（包头 deviceId 或 IOCTL/共享内存）
        ▼  (共享内存环 / UDP)
Rust: miplay-bridge（路由、重采样、妙播协议、设备发现、Web API）
        ▼
Web 前端（查看/控制妙播设备、登录小米账号）
```
- 驱动基座：**Sysvad/Scream 改造（C/C++）**，不用 ACX（Rust 无绑定、微软新方向但无先例）。
- 多端点：多设备实例（`devcon`/`SwDeviceCreate` 动态创建）+ 每实例独立 PCM 通道；改名走 INF，**不碰注册表**。
- 签名：开发期 test signing（或自签+可信根）；发布期 **EV 证书 + attestation signing**（$200–600/年）。
- 工作量：**8～15 人日**（基于样例改造，含安装器与签名流水线）；从零写 20～40 人日。
- 为什么最终不用 A（USB/IP）：VHCI 是**别人写的、未签名的黑盒**（要签名等于自己写 USB 主控驱动，比音频 miniport 难）；等时传输稳定性受第三方驱动影响；UAC1 固定 48k/16bit；每端点一套 USB 栈；而 D 路线的**改名与多端点语义完全可控**、延迟更低、依赖更少。

### 9.3 关于"不买证书、不开 test signing"这一优先级
- **纯用户态做不到**（§1）。所有"虚拟输出端点"方案都要装驱动。
- 免证书 + 免 test signing 的**唯一现成**组合：**用户手动装 VB-CABLE（已签名）+ 单端点 + 应用内路由**（可能再加 Windows 自带/已装的 Steam Streaming Speakers、UU 远程等，但都不可依赖）。
- **自签 + Secure Boot 关闭**（本机满足）是"免 test signing、免买证书"的**第二条路**，但需要 PoC 验证（§10 步骤 0）。
- 若两者都不行，唯一剩下的就是 test signing 常开（水印 + 安全削弱 + 反作弊风险）或买 EV 证书做 attestation 签名。

### 9.4 一个 0 安装的 30 分钟实验（值得先做，可能有惊喜）
本机已装 **UU远程虚拟音频设备**（`nevaudio.sys`，WHQL 签名，内含称为 `newavespeaker` 的 Wave 接口）。如果它的播放端点会把 PCM 回环到它自己的录音端点（虚拟线缆常见行为），那么**零安装、零签名**就能拿到一条虚拟线：
播放到 `扬声器 (UU远程虚拟音频设备)` → 用 WASAPI 从 `麦克风阵列 (UU远程虚拟音频设备)` 抓 PCM → 推妙播。
- 实验：向该端点播 1 kHz 正弦 → 从其录音端点用 ffmpeg/WASAPI 录制 → 看是否有同样的音。
- **注意**：依赖第三方软件的内部行为，**许可证/EULA 与稳定性都不适合产品化**，只适合"今天就想听到声音"的冒烟验证。**【待验证】**

---

## 10. 待验证清单（PoC 步骤，按优先级）

| # | 验证项 | 方法 | 通过标准 | 估时 |
|---|---|---|---|---|
| 0 | **自签驱动能否免 test signing 装入（本机 Secure Boot=False）** | 装 WDK → 编一个最小 Sysvad/Scream 派生驱动 → 自签 + 导入 Trusted Root/Trusted Publisher → `pnputil /add-driver … /install` | 端点出现在声音输出列表且驱动状态正常，无需 test signing | 1～2 人日 |
| 1 | Scream 端到端（先不管签名） | test signing on + 现成 Scream 驱动 + Rust UDP 收包 → 妙播推送 | 音箱出声 | 0.5～1 人日 |
| 2 | 多实例 = 多端点 | 同一 INF 装 2 个实例，看是否出现 2 个端点；再加包头 deviceId 区分 | 两个端点、两路 PCM 可区分 | 1 人日 |
| 3 | UI 改名落盘位置 | 设置里重命名一个端点 → 重新 dump Properties | 确认 `,14` 是否出现 | 10 分钟（需人工点一次） |
| 4 | UU 远程虚拟线是否可回环 | 见 §9.4 | 录到同一音频 | 30 分钟 |
| 5 | **路线 A 冒烟（可选，用于对照）** | 开 test signing → 装 usbip-win2 驱动 → 跑 Virtual-Cables（或我们的 Rust server）→ 看端点是否出现、延迟多少 | 端点出现 + 音频可用 + 延迟可接受 | 0.5～1 人日 |
| 6 | 音箱侧链路（Lead/其他 teammate 负责） | 用 miservice/MiXPlay 的妙播协议推 PCM 到音箱 | 音箱出声 | — |

---

## 11. 需要 Lead 汇总后与用户确认的决策点（我不直接问用户）

1. **要几个端点？** 只做一个「妙播」总端点（v1 即可，Scream 够用），还是**每个音箱一个端点**（必须驱动多实例 + 每实例通道标记，+3～5 人日）？这直接决定 v1 是 3 天还是 1 周。
2. **能否接受开发期开 test signing？**（本机 Secure Boot 已关闭，可先试自签免 test signing，见步骤 0；若失败则要么 test signing 常开，要么买证书）
3. **是否愿意为发布购买 EV 代码签名证书（约 $200–600/年）**？不买 = 只能自签/测试模式，别人装不了。
4. **音频格式要求**：48 kHz/16-bit 立体声是否够？（Scream/UAC1 都是这个档；要 192k/32-bit 需 UAC2 或共享内存方案，工作量翻倍）
5. **端点名形态**：接受 `扬声器 (妙播 - 卧室立体声组合)`（前缀"扬声器"由 Windows 硬编码）吗？还是必须完全自定义（需额外的注册表 hack，非官方）？
6. **延迟容忍度**：端到端预计 300 ms～1 s（驱动缓冲 + 网络 + 音箱缓冲）；纯音乐场景没问题，视频同步需要补偿。
7. **命名/品牌**：驱动名建议 `miplay-bridge virtual audio`（INF 可见名用中文"妙播"）；Scream 派生版需保留 MS-PL 版权声明。

---

## 12. 来源清单

**微软官方文档**
- Audio Endpoint Builder Algorithm — https://learn.microsoft.com/en-us/windows-hardware/drivers/audio/audio-endpoint-builder-algorithm
- Device Properties (Core Audio) — https://learn.microsoft.com/en-us/windows/win32/coreaudio/device-properties
- PKEY_Device_FriendlyName — https://learn.microsoft.com/en-us/windows/win32/coreaudio/pkey-device-friendlyname
- Sample Audio Drivers（Sysvad / ACX 样例入口）— https://learn.microsoft.com/zh-cn/windows-hardware/drivers/audio/sample-audio-drivers
- The testsigning Boot Configuration Option — https://learn.microsoft.com/en-us/windows-hardware/drivers/install/the-testsigning-boot-configuration-option
- Driver signing policy（cross-signed 驱动在 Secure Boot 关闭等条件下的兼容表）— https://learn.microsoft.com/en-us/windows-hardware/drivers/install/driver-signing
- Developing Windows drivers for emulated USB host controllers and devices (UDE) — https://learn.microsoft.com/en-us/windows-hardware/drivers/usbcon/developing-windows-drivers-for-emulated-usb-host-controllers-and-devices

**上游项目**
- tarekwasfy01/Virtual-Cables（BSD-2-Clause）— https://github.com/tarekwasfy01/Virtual-Cables
- vadimgrn/usbip-win2（现 nefarius/usbip-win2；BSD-2-Clause；**README L11 驱动未签名、必须 test signing**）— https://github.com/vadimgrn/usbip-win2
- dorssel/usbipd-win（**服务端**方向，不提供 Windows 客户端 VHCI）— https://github.com/dorssel/usbipd-win
- duncanthrax/scream（MS-PL）— https://github.com/duncanthrax/scream
- arcnmx/scream（C/C++ fork，非 Rust）— https://github.com/arcnmx/scream
- microsoft/windows-drivers-rs（WDM/KMDF/UMDF Rust 绑定，无 ACX）— https://github.com/microsoft/windows-drivers-rs
- VB-Audio Cable（licensing）— https://vb-audio.com/Cable/index.htm
- Virtual Audio Cable（Muzychenko，授权/价格）— https://vac.muzychenko.net/en/

**本机实测证据（命令与原始输出见同目录 `local-evidence.md`）**
- `Get-PnpDevice -Class AudioEndpoint`（14 个端点，含 UU远程 / Steam / Xiaomi Speaker-4162 / 立体声组合）
- `HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Render\{GUID}\Properties` 全量 dump（26 个端点）
- `HKLM\SYSTEM\CurrentControlSet\Enum\ROOT\MEDIA\0000` / `ROOT\STEAMSTREAMINGSPEAKERS\0000` + `C:\Windows\INF\oem56.inf` / `oem41.inf`
- `Get-AuthenticodeSignature`：`nevaudio.sys` / `SteamStreamingSpeakers.sys`（两者 `Microsoft Windows Hardware Compatibility Publisher`，Valid）；`Scream.sys`（私人证书 + 过期 + 无时间戳 → `UnknownError`）
- `%TEMP%\vad-research\{Virtual-Cables, usbip-win2, scream, scream-arcnmx, usbipd-win}` 源码副本
- `bcdedit /enum {current}`、`Confirm-SecureBootUEFI`、`Test-Path "…Windows Kits\10\Include\*\km"`
