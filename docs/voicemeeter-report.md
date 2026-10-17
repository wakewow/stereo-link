# VoiceMeeter Banana 双蓝牙 L/R 分离 + 自建虚拟声卡：调研报告

> 任务：task-4（Lead 派发）。只做外部调研 + 本机**只读**检查；未安装 VoiceMeeter、未装/改任何驱动、未改 boot 配置、未改代码。
> 本机环境：Windows 11 专业版 Build 26100；`Confirm-SecureBootUEFI` = False；`bcdedit` 无 testsigning；WDK 10.0.26100 与 VS 2022 已装。
> 证据级别标注：**【官方】** 厂商/微软原文　**【社区实测】** 第三方可复现测量　**【社区】** 教程/问答/论坛　**【本机实测】** 本机只读检查输出　**【估算】** 推算，未实测。

---

## 0. TL;DR

1. **VoiceMeeter Banana 能做到「A1 只出左声道、A2 只出右声道」**，但方式不是「BUS 上的 pan 旋钮」，而是 **8×8 Output Matrix（In1→BUS A1=100%/In2=0，In2→BUS A2=100%/A1=0）**；需要手工配置，不是开箱即用。**【社区】**（VB-Audio 中文授权站教程 ×2，含 Win11 实测步骤）
2. **它不需要 ASIO，也不需要 VBAN**：Banana 的 A2/A3 物理输出只支持 WDM/KS/MME，蓝牙音箱就是 WDM 端点。**【官方】**（Banana 规格页）
3. **它不会比我们现有的「Rust 引擎直推两个 WASAPI 端点」更可靠**。两者走同一条 Windows 蓝牙栈、同一个 A2DP 时钟漂移问题；VoiceMeeter 只多提供「矩阵路由 + 每路静态延迟补偿」，而这两件事我们引擎里实现成本极低。VoiceMeeter 整链比裸输出**多约 40ms**（社区实测），且 MME 输出会再加约 150ms。**【社区实测】【官方】**
4. **自签内核驱动在这台机器上装不上（即使 Secure Boot 已关）**：微软政策从 Win10 1607 起「不再加载非 Dev Portal 签名的新内核驱动」，例外只覆盖 **cross-signed** 驱动（证书须 2015-07-29 前签发）等情形，**普通自签不在例外内**；本仓库 `docs/virtual-soundcard.md` 已记录过一次实测失败：`0xC0000428 STATUS_INVALID_IMAGE_HASH`。**【官方】【本机实测】**
5. **「自己新建一个虚拟音频端点」的现实路线**（按推荐度）：
   - **R1（推荐，马上可做）**：安装器内嵌 **VB-CABLE**（VB-Audio 官方明确允许「随应用分发 / 嵌入安装包」，MIT 之外唯一明确可嵌入的已签名虚拟声卡驱动）+ 装完把端点显示名改成我们的品牌（复用现有改名脚本）。用户操作：管理员运行 + 重启一次；品牌归属：驱动仍是 VB-Audio。
   - **R2（自有品牌，中期）**：自研 WDM/PortCls 虚拟音频驱动（SYSVAD 派生）→ 本机 testsigning 开发 → 产品走 **attestation signing**（需要 EV 代码签名证书 + Partner Center 硬件仪表盘；不额外收签名费，EV 证书约 $200–500/年 **【估算】**）。工作量约 3–6 周。
   - **R3（现状，保底）**：继续借用 Steam Streaming Speakers + 改名；零成本，但依赖 Steam、不能随产品分发 Valve 驱动、品牌不属于我们。
   - **VoiceMeeter 不进产品依赖链**，只写进 FAQ 作为用户自选对照方案。

---

## 1. 问题 1：VoiceMeeter Banana 能否「两个蓝牙输出分别出 L/R」

### 1.1 结论

**能，但属于「高级手工配置」，不是产品级开箱能力。** 官方文档没有「双蓝牙 L/R 分离」这个用例；可行路径是社区教程反复验证的同一套：

1. 系统默认输出 → `VoiceMeeter Input (VB-Audio VoiceMeeter VAIO)`；
2. Banana 的 Hardware Out：**A1 = 音箱 1，A2 = 音箱 2**（优先 `WDM:`，爆音换 KS/MME）；
3. 虚拟输入条上点亮 **A1、A2**；
4. 打开 **8×8 Output Matrix**：In1 = 左声道（FL）、In2 = 右声道（FR）；BUS A1 行设 In1=100%、In2=0%；BUS A2 行设 In1=0%、In2=100%；
5. 若两台音箱本身延迟不同：`Menu → System Settings / Options → Monitoring Synchro Delay`，给快的那一路加 **50–300ms**（建议从 100ms 起试）。

来源（均为 **【社区】**，但来自 VB-Audio 官方中文授权站/其教程体系，含截图步骤）：
- 《闲置蓝牙音箱组双声道，Win11 实测指南》 http://www.vb-audio.cn/post/165.html （镜像 http://www.voicemeeter.cn/post/165.html ）
- 《Win10双蓝牙音频同步输出》 http://www.vb-audio.cn/post/35.html （A1/A2/A3 三个 Hardware Out，逐设备调延迟）
- 《别浪费你的蓝牙音箱！一招教你把它变成电脑的"第二扬声器"》 http://vb-audio.cn/post/147.html （Monitoring Synchro Delay 50–300ms，建议 100ms 起）

> 证据缺口（必须如实说明）：Banana 用户手册是 PDF，`web_fetch` 不支持 `application/pdf`，因此**没有抓到手册原文**；「8×8 Output Matrix」在官方产品页的规格表里没有单列（页面只写 BUS 支持「Master Full Parametric EQ (6 Cells / 8 channels + **Gain/Delay per channel**)」与「Physical Output Synchronization Delay in system settings dialog box」）。所以「矩阵」这一步定为**社区级证据**，不是官方规格级。

### 1.2 路由矩阵能力：per-channel 还是整路？

- **BUS / 输出层面**：Banana 有 **A1/A2/A3 + B1/B2 共 5 条 BUS**（单层）；每路物理输出可独立设同步延迟（官方规格页）。**【官方】**
- **通道层面**：官方规格写 BUS Processing 含「Master Full Parametric EQ（6 Cells / **8 channels + Gain/Delay per channel**）」——即 BUS 内部按 8 通道处理、每通道有独立增益/延迟；但**官方没有把它描述成「L/R pan 旋钮」**。
- 实际实现 L/R 分离靠的是 **Output Matrix（输入通道 → BUS 的交叉点增益）**，这是社区教程给出的做法：**不是**「每条 BUS 有一个左右平衡旋钮」，而是「在矩阵里把左声道只接到 A1、右声道只接到 A2」。**【社区】**
- 旁证：Voicemeeter Potato 手册摘录（PDF 搜索摘要）提到「BUS outputs are copied in the ASIO output channels in logical order A1, A2, A3, A4 and A5」——说明 BUS 是离散输出通道，不是靠 pan 混合。**【社区/文档摘录】**

### 1.3 需要 ASIO 吗？需要 VBAN 吗？

**都不需要。** **【官方】**（https://vb-audio.com/Voicemeeter/banana.htm ）：
- Output A1 支持 WDM/KS/MME/ASIO；**A2、A3 只支持 WDM/KS/MME**（8k–192kHz，1–8ch）。
- 蓝牙音箱在 Windows 里就是 WDM 音频端点，所以 A2 = 蓝牙音箱完全落在支持范围内。
- VBAN 是网络音频协议，与本需求无关；Banana 的 VBAN 是可选功能。
- 官方延迟优化文档建议「A1 用最低延迟的 ASIO 设备」；如果 A1 也接蓝牙（WDM），就没有 ASIO 可用——**官方承认 WDM/KS 有些能 256 样本稳定、有些不能，MME 稳定但延迟高**。**【官方】**（https://voicemeeter.com/how-to-optimize-your-latency-in-voicemeeter/ ）

### 1.4 Windows 层面对「同时两个蓝牙输出」的限制

- **系统 UI 没有「同时输出到多台设备」的原生选项**。微软 Q&A 上独立顾问的原文：
  > "Unfortunately, there is no option on Windows to do that, you need to use a third-party tool to set multiple audio outputs simultaneously" —— https://learn.microsoft.com/en-gb/answers/questions/5657664/multiple-blue-tooth-speakers-to-play-in-sync-windo **【社区】**
- 另一条微软 Q&A（3 台蓝牙音箱同时出声）：
  > "It is possible to play sound on three different audio devices in Windows using a third-party program, as this feature is not natively supported... The key thing is that this method may introduce slight audio latency which you would have to bear." —— https://learn.microsoft.com/en-us/answers/questions/3890314/can-i-have-sound-play-through-multiple-bluetooth-s **【社区】**
  > 同一帖另一个回答解释了蓝牙多点连接的带宽限制："While a Bluetooth device can pair with up to 7 other Bluetooth devices, it can only connect with one at a time... Multipoint... doesn't work well in practice."
- VB-Audio 中文授权站的说法更直接：「**Win11 本身不支持同时向两个蓝牙设备输出独立音频信号**」。**【社区】**
- **注意区分**：系统 UI 不能「一键双输出」≠ 应用不能分别向两个端点播放。我们的 Rust 引擎正是用 WASAPI 分别打开两个 render 端点（已在本机验证可用，0.2–0.35s），VoiceMeeter 做的是同一件事，只是多了一层虚拟设备与混音引擎。

### 1.5 蓝牙（WDM）输出的已知稳定性问题

**都是社区级，且都没有官方"同时两个 BT 输出"的保证**：
- 同一台蓝牙适配器同时连两台音箱「对蓝牙模块有压力」；建议**一台连内置蓝牙、一台连 USB BT 5.0 适配器**分担。**【社区】**（vb-audio.cn post/165）
- 蓝牙 4.0 及以下 / 廉价适配器可能不支持多设备音频流。**【社区】**（同上）
- A1/A2 无声时先 `Menu → Restart Audio Engine`；蓝牙断连/重连后经常需要手动重启音频引擎。**【社区】**（同上）
- 延迟补偿是**静态**的：「蓝牙物理延迟动态波动，软件补偿只能静态对齐」——同一篇教程自己承认。**【社区】**（同上）
- 官方延迟文档另一条相关坑：WDM 独占模式「由于 WASAPI 中的错误，它可能会在 48 小时会话内生成崩溃或 BSOD」，所以 UI 已不提供；这是 WDM 路径的稳定性风险。**【官方/中文站译文】**（https://www.vb-audio.cn/post/97.html ）

### 1.6 延迟量级（对比我们的 Rust 引擎直推）

| 环节 / 路径 | 数值 | 级别 |
| --- | --- | --- |
| VoiceMeeter 引擎理论下限 | **256 样本 @44.1/48kHz ≈ 5ms**（官方原文："A buffer size of 256 samples at 44.1 or 48 kHz results in a latency of 5 ms... basically the minimum latency we can expect to manage reliable from the Windows Operating System"） | **【官方】** https://voicemeeter.com/how-to-optimize-your-latency-in-voicemeeter/ |
| VoiceMeeter 整链相对裸输出 | **+约 40ms**（HeSuVi Wiki 实测：CS:GO 点击→声音，10 次平均；Nothing 134ms / APO-only 135ms / **APO+VoiceMeeter(Banana) 174ms** / APO+VB-Cable 195ms） | **【社区实测】** https://sourceforge.net/p/hesuvi/wiki/Audio%20Latency%20Measurements/ |
| MME 输出接口 | **+约 150ms**（VB-Audio 中文站：「Virtual ASIO 接口会增加 1 个缓冲区延迟，而 MME 接口会增加 150ms」） | **【官方/译文】** https://www.vb-audio.cn/post/97.html |
| 蓝牙 A2DP 链路本身 | 约 150–250ms（SBC/AAC 典型值，见上一份延迟调研报告） | **【社区实测】**（汇总在 `.scratch/latency-research.md`） |
| **我们的 Rust 引擎直推两个 WASAPI 端点** | **0.2–0.35s（已验证可用）** | **【本机实测】**（Lead 已确认） |
| VoiceMeeter 路径（VAIO → VM 引擎 → A2 WDM → 蓝牙 → 音箱） | 估算 **0.25–0.45s**（= 我们引擎的链路 + VM 混音/VAIO 约 20–50ms + 静态延迟补偿量） | **【估算】** |

**结论：VoiceMeeter 不可能比直推更低延迟**——它必然多一段虚拟设备 + 混音引擎；它的价值只在「矩阵路由 + 每路可调延迟」，而这两点在 `stereo-audio` 引擎里各是几十行代码。

### 1.7 版本差异与授权（能否随产品分发）

| 版本 | I/O 与 BUS | 关键差异 | 可否随产品分发 |
| --- | --- | --- | --- |
| Voicemeeter（标准） | 3 In（2 物理/1 虚拟）、3 Out（2 物理/1 虚拟）、**2 BUS（A/B）** | 基础版；只有 1 个虚拟输入 | AS IS 可分发（须注明来源 + donationware），企业分发需 license |
| **Voicemeeter Banana** | 5 In（3 物理/2 虚拟）、5 Out（3 物理/2 虚拟）、**5 BUS（A1/A2/A3 + B1/B2）** | 多路硬件输出、每路同步延迟、参数 EQ、录音机 | 同上；许可费建议价 100 份 15.0 EUR/份、1000 份 10.71、10000 份 8.33 |
| Voicemeeter Potato | 8 In（5 物理/3 虚拟）、8 Out、**5 BUS A1–A5 + B1–B3（多层）** | 内建混响/延迟、AUX Send/Return | **明确不能随产品分发/捆绑，但可预装** |
| VB-CABLE | 1 条虚拟音频线（Input/Output 端点） | 虚拟声卡驱动本体 | **明确允许**：可随应用（含商业）分发，**可嵌入安装包**（silent install），须注明来源 + donationware；**VB-CABLE A+B、C+D 不能分发** |

来源：**【官方】**
- Banana 规格：https://vb-audio.com/Voicemeeter/banana.htm （v2.1.3.0，SEP 2026，XP–Win11 32/64 & ARM64）
- 标准版规格：https://vb-audio.com/Voicemeeter/index.htm （v1.1.3.0）
- Potato 规格：https://vb-audio.com/Voicemeeter/potato.htm （v3.1.3.0）
- 授权/分发条款：https://vb-audio.com/Services/licensing.htm
- VB-CABLE 产品页：https://vb-audio.com/Cable/index.htm

授权原文摘录（licensing.htm）：
> "It's free to distribute or promote Voicemeeter (or Banana) AS IS to End Users if our donationware model stays applicable"
> "We allow to distribute VB-CABLE package with your Application (free or commercial) and possibly embed it in your installation package (VB-CABLE silent installation) if our donationware model is applicable"
> "significant donation as licensing fee (e.g 500, 1000, 2000 USD)"（建议大公司）
> 「Potato 不能随产品分发，但可预装；VB-CABLE A+B / C+D 不能分发」

其他：**Voicemeeter Remote API 免费可用于任何项目**（官方 licensing 页）。**【官方】**

---

## 2. 问题 2：自建虚拟音频端点的所有现实路线

### 2.1 Windows 机制：为什么必须写内核驱动

- **render 端点不是应用能注册的**：端点由 `AudioEndpointBuilder` 从内核音频驱动暴露的 `KSCATEGORY_AUDIO` 接口派生。微软官方音频样例页写明：`TabletAudioSample` 演示「如何开发一个 WDM 音频驱动，暴露多个音频设备……用**虚拟音频设备**代替真实硬件适配器」——即虚拟端点 = WDM 驱动。**【官方】** https://learn.microsoft.com/en-us/windows-hardware/drivers/audio/sample-audio-drivers
- **APO 不能新增端点**：APO 是「COM host object」「inserted in the software device pipe on stream initialization」，即**挂在已有设备/流上的 DSP**（EQ/混响/AEC 等），不是设备。**【官方】** https://learn.microsoft.com/en-us/windows-hardware/drivers/audio/audio-processing-object-architecture
- **AudioGraph / Windows.Media.Audio** 是应用层音频图 API，同样不产生系统 render 端点。
- 结论：**没有纯用户态途径**；「新增一个自己的 render 端点」= 写并签名一个内核音频驱动（或复用别人的已签名驱动 + 改名）。

### 2.2 未开测试签名时，自签内核驱动能加载吗？

**不能（除非是 2015-07-29 前证书链的 cross-signed 驱动）。**

微软官方原文（**【官方】**，https://learn.microsoft.com/en-us/windows-hardware/drivers/install/kernel-mode-code-signing-policy--windows-vista-and-later- ）：
> "Starting with Windows 10, version 1607, Windows will not load any new kernel-mode drivers which are not signed by the Dev Portal."
> 例外（cross-signed 才可能被接受）：
> - The PC was upgraded from an earlier release of Windows to Windows 10, version 1607.
> - **Secure Boot is off in the BIOS.**
> - Driver was signed with an end-entity certificate issued **prior to July 29th 2015** that chains to a supported cross-signed CA.
> - boot 驱动不被拦截，但会被 PCA 移除。

**关键区分（容易误判）**：本机 Secure Boot = False 确实命中「Secure Boot off」例外，**但那个例外只对 cross-signed 驱动有效**（签名链里有微软 cross-signing CA，且证书须 2015-07-29 前签发）。**普通自签证书（self-signed / 只有自家根）不属于 cross-signed，因此不适用任何例外**，Win10 1607+/Win11 不会加载它。

本机直接实测证据：`docs/virtual-soundcard.md` 第 34 行记录过上一次尝试——自研/改造的 Scream 派生驱动「签名能装上，但驱动载不起来：`0xC0000428 STATUS_INVALID_IMAGE_HASH`（`CM_PROB_UNSIGNED_DRIVER`）」。**【本机实测】**

### 2.3 testsigning vs 每次开机 F7（禁用签名强制）

| 维度 | `bcdedit /set testsigning on` + 重启 | 开机 F8/F7「Disable Driver Signature Enforcement」 |
| --- | --- | --- |
| 生效范围 | 持续生效（写入 BCD） | **只对当前系统会话禁用 load-time 签名强制，重启后不保留**（微软原文） |
| 水印 | 桌面右下角显示 **"Test Mode"**（官方原文：开启后桌面显示 Test Mode） | 无持久水印 |
| 对 Secure Boot | 命令返回 "The value is protected by Secure Boot policy and cannot be modified or deleted" 时，必须先**进 BIOS 关 Secure Boot** | 需在启动菜单里手动选，**每次重启都要重做** |
| HVCI / Memory Integrity | Win10 1507+ 若开启 HVCI，驱动必须用**自建测试证书签名**（不能是未签名二进制） | 同理受 HVCI 影响 |
| 用户代价 | 一次重启 + 永久水印；Secure Boot 关闭后整体安全性下降 | 每次开机都要人工操作；无人值守/远程场景基本不可行 |
| 适合场景 | **开发/内部自用**（本机 Secure Boot 已关，正好适合） | 临时验证，不适合产品 |

来源：**【官方】**
- testsigning：https://learn.microsoft.com/en-us/windows-hardware/drivers/install/the-testsigning-boot-configuration-option
- 未签名驱动 + F8：https://learn.microsoft.com/en-us/windows-hardware/drivers/install/installing-an-unsigned-driver-during-development-and-test （原文："This setting does not persist across system restarts."）
- 本机现状（只读）：`bcdedit /enum {current}` 无 testsigning 条目；`Confirm-SecureBootUEFI`=False；`UEFISecureBootEnabled`=0；VBS 运行中但 **HVCI=0**（因此 testsigning 不会被 HVCI 额外拦）。

### 2.4 产品化签名：attestation signing 现状与成本

微软官方（**【官方】**，https://raw.githubusercontent.com/MicrosoftDocs/windows-driver-docs/staging/windows-driver-docs-pr/dashboard/driver-signing-offerings.md ，ms.date 2026-03-23；以及 https://learn.microsoft.com/en-us/windows-hardware/drivers/dashboard/code-signing-attestation ）：

- **Attestation signing** 定位是 **"for testing scenarios"**，**不需要 HLK**，支持 Win10 Desktop+、内核/用户态驱动；**不能发布到 Windows Update 面向零售**（零售需 WHCP/HLK 认证）。
- 必须先注册 **Windows Hardware Dev Center（Partner Center 硬件仪表盘）**，而注册**需要 EV 代码签名证书**（"an EV code signing certificate is required to establish a dashboard account"）。
- 流程：打 CAB（sys+inf+pdb+cat，cat 仅供公司内部验证，微软会重签）→ 用 EV 证书 `SignTool sign /fd sha256` 签 CAB → Partner Center 提交 → 下载微软重签的驱动包。
- 签出的驱动**不是 "Windows Certified"**，但「微软签名表示 Windows 信任该驱动」，可在普通（Secure Boot 开启的）Win10/11 上安装加载。
- 另有 **preproduction signing**（早期开发、可开 Secure Boot，仅限显式配置信任的设备）与 WHCP（正式零售认证）。
- **费用**：微软未在文档里收签名费（成本主要是 EV 证书 + 维护仪表盘账户）；**EV 代码签名证书行业价约 $200–500/年**【估算，未从官方页取得价目】。

对产品的含义：**attestation 路线可行**（用户机器 Secure Boot 开着也能装），但不能以"驱动已认证"宣传；每版驱动都要重新提交签 CAB；证书/账户是长期年费项。

### 2.5 现成「已签名 + 允许再分发」的虚拟音频驱动对比

| 驱动 | 签名状态 | 许可证 / 再分发 | 品牌归属 | 能否当"我们的端点" |
| --- | --- | --- | --- | --- |
| **VB-CABLE**（VB-Audio） | 已签名（官方支持 XP→Win11 32/64/Arm64，安装步骤不要求关签名/测试模式）**【官方安装说明】** | **donationware；明确允许随应用分发 + 可嵌入安装包（silent install）**，须注明来源 + donationware；A+B/C+D 不可分发 **【官方】** | VB-CABLE | 端点可改名成我们的品牌（注册表 FriendlyName），**驱动仍是 VB-Audio**；合法性与我们当前改名 Steam 端点完全同类 |
| **Scream**（duncanthrax） | GitHub releases 是 **signed builds**；README 明确 cross-signed + Win10 1607 例外（含 Secure Boot off）；**Win11 安装按 README 必须走 Test Mode**（`bcdedit /set testsigning on`）→ 发行版不能算"Secure Boot 下免配置可用" **【官方 README】** | **MS-PL**（源自微软 MSVAD 示例）→ 允许再分发/修改，须保留版权与许可文本；签名是作者的个人/项目签名，不是我们的 | Scream | 不能改签名做自有品牌；定位是**网络声卡**（PCM multicast UDP 239.255.77.77:4010，每包 5B 头 + 1152B PCM，内核级处理无用户态），不是本地直通端点 |
| **Virtual Audio Cable**（Muzychenko） | 商业产品，多年维护，已签名 | **shareware**：FULL 版本「each copy can be distributed only together with its lawfully obtained license」；**TRIAL 版本可以「freely distributed together with another product, including commercial ones」**（不可改包、不可收费）；Audio Repeater 免费独立使用 **【官方 license.txt】** | Virtual Audio Cable | TRIAL 可捆绑但有限制（试用功能限制），品牌仍是 VAC |
| **VoiceMeeter VAIO**（VB-Audio） | 已签名，随 Voicemeeter 安装 | Banana 可 AS IS 分发；Potato 不可分发；虚拟输入端点属于 VoiceMeeter 安装包 | Voicemeeter | 不适合当独立端点（要捆绑整个混音台） |
| **Steam Streaming Speakers**（Valve） | 已签名（本机实测：Valve Corporation，17.56.13.764，2017-07-20，INF `oem41.inf`） | 随 Steam 安装，**没有公开再分发授权** | Valve / Steam | 当前现状（改名借用）；不能随产品分发 |
| NVIDIA Virtual Audio / UU 远程虚拟音频 | 已签名（本机已装） | 随各自产品（显卡驱动 / 网易 UU）安装，无再分发授权 | NVIDIA / 网易 | 不能用于我们产品 |

官方链接：
- Scream README：https://github.com/duncanthrax/scream （原文："A ZIP file containing signed builds for Windows 10 on x64, x86 and arm64"；Win11 安装步骤 = 开 Test Mode → `pnputil /add-driver` → 关 Test Mode）
- VAC 许可：https://vac.muzychenko.net/en/license.txt ；分发政策：https://vac.muzychenko.net/en/manual/distrib.htm
- VB-CABLE：https://vb-audio.com/Cable/index.htm ；授权：https://vb-audio.com/Services/licensing.htm

### 2.6 Windows 自带能力（APO / AudioGraph / 新 API）

**结论：不能新增 render 端点。** 依据见 2.1：端点必须由内核音频驱动暴露（官方 SYSVAD 文档），APO 只是挂在设备流上的 DSP（官方 APO 架构文档），AudioGraph 是应用层音频图。没有"注册一个虚拟输出设备"的用户态 API。

---

## 3. 结论：三条可执行路线（排序 + 推荐）

### 3.1 排序表

| 优先级 | 路线 | 可靠性 | 用户要做什么 | 品牌归属 | 工作量 | 说明 |
| --- | --- | --- | --- | --- | --- | --- |
| **R1（推荐，立即可做）** | **安装器内嵌 VB-CABLE + 装后把端点改名为「立体声链设备」**（复用 `安装虚拟声卡.cmd` / `scripts/gui-set-name.ps1` 的改名逻辑，只把目标实例 ID 从 Steam 换成 VB-CABLE） | 高：已签名驱动、官方允许嵌入分发、Secure Boot 开/关都能装；不依赖 Steam | 管理员运行安装器 → 重启一次（驱动安装要求）→ 输出列表出现我们的端点名 | 驱动：VB-Audio；端点显示名：我们的（合法，且与现在改 Steam 名同性质） | **小**（1–3 天：打包 silent install + 换实例 ID + 卸载/回滚 + 文档） | 满足「自己新建一个端点」的**用户可感知结果**；不满足"自有驱动品牌" |
| **R2（自有品牌，中期）** | **自研 WDM/PortCls 虚拟音频驱动**（SYSVAD 派生），本机 testsigning 开发 → 产品 attestation signing | 中高，但驱动崩溃=蓝屏，需长期维护 | 管理员装驱动 + 重启（attestation 签名后 Secure Boot 开启的机器也能装） | **完全自有**（驱动名、端点名、厂商都是我们） | **大**（3–6 周：驱动开发/格式协商/多客户端/时钟 + 安装器 + 签名流程 + Win11 26100 兼容 + 测试）；成本 EV 证书约 $200–500/年【估算】 | 唯一能真正"自己的虚拟声卡"的路线；建议作为独立里程碑 |
| R3（保底/现状） | 继续借用 Steam Streaming Speakers + 改名 | 中：依赖 Steam 安装、Steam 更新/系统重装会失效 | 已装 Steam 才能借用 | Valve（不可分发） | 0（已有） | 保持现状，等 R1/R2 落地后替换 |
| ✗ 不推荐 | 把 VoiceMeeter 作为产品依赖 | 不更高：同一条蓝牙栈 + 多一层混音引擎，+40ms 起 | 装 VM + 手工配矩阵 + 每次引擎重启 | VB-Audio | 中 | 只写 FAQ 作对照方案 |
| ✗ 不推荐 | 自研驱动但走"普通自签 + 关 Secure Boot/F7" | 低：Win10 1607+ 政策 + 本机已实测 `0xC0000428` | 关 Secure Boot / 开 Test Mode / 每次 F7 | 我们 | 中 | 只适合内部开发机，不适合产品 |

### 3.2 明确推荐

**推荐 R1 + 把 R2 立项**：

1. **现在**：删除「依赖 Steam」这个单点——在安装器里用 **VB-CABLE（官方允许的嵌入方式）** 建一个自己的虚拟端点，安装后把它的端点显示名改成「立体声链设备」（与现在对 Steam 端点做的事完全一样，只是目标换成合法可再分发的驱动）。用户操作只有「管理员运行 + 重启一次」。
2. **同时**：`stereo-audio` 引擎里补两件事（VoiceMeeter 能做到、我们更该自己做）：
   - **每路输出的可调延迟补偿**（0–300ms，UI 滑杆）——用于对齐两台延迟不同的蓝牙音箱；VM 的 `Monitoring Synchro Delay` 思路，官方量级 50–300ms。
   - **双端点时钟漂移监测**（长时播放时哪一路在堆积），这是 VM 也解决不了、只能靠重采样/丢帧缓解的问题。
3. **中期**：若用户坚持"驱动也必须是我们的品牌"，走 R2（SYSVAD 派生 + attestation signing + EV 证书）；本机 WDK 10.0.26100 / VS 2022 / signtool x64 已就绪，开发机 Secure Boot 已关、HVCI 已关，适合 testsigning 迭代。
4. **VoiceMeeter 只作为用户自选对照工具**写进 FAQ（「想自己验证双蓝牙 L/R 分离可以用 Banana：A1/A2 + Output Matrix + Synchro Delay」），不进安装器、不当依赖。

### 3.3 回答任务里的两个"预期"问题

- 「在 Secure Boot 关闭、无测试签名的 Win11 上，自签内核驱动能否加载？」→ **不能**（除 cross-signed 旧证书例外）。本机实测过 `0xC0000428`。权威出处见 §2.2。**【官方】+【本机实测】**
- 「用 Windows 自带能力（APO、AudioGraph、新 API）能否新增 render 端点？」→ **不能**。见 §2.1/§2.6。**【官方】**

---

## 4. 本机只读证据（2026-10-18，未做任何修改）

| 检查 | 结果 |
| --- | --- |
| OS | Windows 11 专业版，Build **26100**，Version 10.0.26100 |
| Secure Boot | `Confirm-SecureBootUEFI` = **False**；`HKLM:\SYSTEM\CurrentControlSet\Control\SecureBoot\State\UEFISecureBootEnabled` = **0** |
| testsigning | `bcdedit /enum {current}` **无 testsigning 条目**（关闭）；`bootmenupolicy Standard`；`hypervisorlaunchtype Auto` |
| VBS / HVCI | `Win32_DeviceGuard`：VirtualizationBasedSecurityStatus=2（运行），SecurityServicesRunning={0}；`HypervisorEnforcedCodeIntegrity\Enabled` = **0** |
| 驱动工具链 | WDK `C:\Program Files (x86)\Windows Kits\10`：Include 10.0.19041.0 / 10.0.26100.0；bin 10.0.19041.0 / 10.0.26100.0 均含 **x64 signtool**；**Visual Studio 2022** 已装 |
| 虚拟音频相关 OEM 驱动 | `oem49/41` Valve（Steam Streaming Speakers/Microphone，2017）；`oem55/56` NetEase（UU 远程虚拟音频）；`oem28/44/29/48/22/27` NVIDIA MEDIA（含虚拟音频）；`oem24/59` Microsoft APO；`oem42` VentoByte；`oem40` Nefarius。**无 VB-CABLE / VoiceMeeter / Scream** |
| 当前「立体声链设备」 | `ROOT\STEAMSTREAMINGSPEAKERS\0000`，Provider **Valve Corporation**，DriverVersion **17.56.13.764**，DriverDate **2017-07-20**，Manufacturer "Valve Corporation Audio DDK"，INF `oem41.inf` |
| 渲染端点（状态 OK） | `扬声器 (Xiaomi Sound-1695)`、`扬声器 (立体声链设备)`、`扬声器 (UU远程虚拟音频设备)`、`扬声器 (Realtek High Definition Audio)`、`耳机 (Chu2 DSP)`、`CG345UK (NVIDIA HDA)`、`X41 (NVIDIA HDA)`、`扬声器 (Steam Streaming Microphone)` |
| 蓝牙 A2DP 当前可用输出 | **只有 `Xiaomi Sound-1695` 一台**（其余 Xiaomi Sound-5320 / Speaker-4162 / Speaker Portable-2492 / EDIFIER W820NB 均为未连接状态） |
| 历史尝试 | `docs/virtual-soundcard.md`：Scream 派生驱动加载失败 `0xC0000428 STATUS_INVALID_IMAGE_HASH`（`CM_PROB_UNSIGNED_DRIVER`）→ 必须 testsigning 或 EV/attestation，不适合普通用户 |

---

## 5. 未证实 / 风险清单

- **VoiceMeeter 8×8 Output Matrix**：只有社区教程（含截图）证据；Banana 用户手册 PDF 未能抓取原文（`web_fetch` 不支持 PDF），官方规格页未单列该功能名。
- **VoiceMeeter 路径的实际端到端延迟**：没有实测（本机未安装 VM，任务禁止安装）。表中 0.25–0.45s 为估算。
- **同时两个蓝牙 A2DP 输出的稳定性**：无官方保证；本机当前只有一台 A2DP 端点处于连接状态，无法只读验证双路并发。
- **attestation signing 当前是否仍零签名费**：微软文档未写价格，只写需要 EV 证书；成本按行业价估算。
- **EV 证书价格**：约 $200–500/年，未取到 2026 年官方报价。
- **VB-CABLE 的 .cat 签名类型（WHQL/attestation/cross-signed）**：未逐字节核验；依据其官方安装说明（Win11/Arm64 支持、安装步骤不含关签名/Test Mode）判断为已获微软签名。
- **Scream 签名发行版在本机（Secure Boot 关）能否免 Test Mode 加载**：微软政策上 cross-signed + Secure Boot off 可行，但 Scream README 的 Win11 安装步骤仍要求 Test Mode；未实测（禁止安装）。

---

## 6. 来源汇总

**VoiceMeeter / VB-Audio**
- Banana 规格：https://vb-audio.com/Voicemeeter/banana.htm
- 标准版：https://vb-audio.com/Voicemeeter/index.htm
- Potato：https://vb-audio.com/Voicemeeter/potato.htm
- 延迟优化（官方）：https://voicemeeter.com/how-to-optimize-your-latency-in-voicemeeter/
- VB-CABLE：https://vb-audio.com/Cable/index.htm
- 授权/分发：https://vb-audio.com/Services/licensing.htm
- 中文授权站教程（矩阵/L-R/延迟补偿）：
  - http://www.vb-audio.cn/post/165.html
  - http://www.vb-audio.cn/post/35.html
  - http://vb-audio.cn/post/147.html
  - https://www.vb-audio.cn/post/97.html

**微软（签名 / 驱动 / 音频架构）**
- 驱动签名政策：https://learn.microsoft.com/en-us/windows-hardware/drivers/install/kernel-mode-code-signing-policy--windows-vista-and-later-
- 测试签名：https://learn.microsoft.com/en-us/windows-hardware/drivers/install/the-testsigning-boot-configuration-option
- 未签名驱动安装：https://learn.microsoft.com/en-us/windows-hardware/drivers/install/installing-an-unsigned-driver-during-development-and-test
- attestation signing 文档：https://learn.microsoft.com/en-us/windows-hardware/drivers/dashboard/code-signing-attestation
- 驱动签名选项（源码文档）：https://raw.githubusercontent.com/MicrosoftDocs/windows-driver-docs/staging/windows-driver-docs-pr/dashboard/driver-signing-offerings.md
- SYSVAD 虚拟音频驱动样例：https://learn.microsoft.com/en-us/windows-hardware/drivers/audio/sample-audio-drivers
- APO 架构：https://learn.microsoft.com/en-us/windows-hardware/drivers/audio/audio-processing-object-architecture
- Windows 多蓝牙输出限制（Q&A，社区）：https://learn.microsoft.com/en-gb/answers/questions/5657664/multiple-blue-tooth-speakers-to-play-in-sync-windo
- 同上（3 台蓝牙音箱）：https://learn.microsoft.com/en-us/answers/questions/3890314/can-i-have-sound-play-through-multiple-bluetooth-s

**虚拟音频驱动 / 延迟实测**
- Scream：https://github.com/duncanthrax/scream
- Scream README（签名/Win11 Test Mode/协议）：https://raw.githubusercontent.com/duncanthrax/scream/master/README.md
- Virtual Audio Cable 许可：https://vac.muzychenko.net/en/license.txt
- VAC 分发政策：https://vac.muzychenko.net/en/manual/distrib.htm
- HeSuVi 音频延迟实测：https://sourceforge.net/p/hesuvi/wiki/Audio%20Latency%20Measurements/

**本仓库内部**
- `docs/virtual-soundcard.md`（借用 Steam Streaming Speakers + 改名；历史 0xC0000428 记录）
- `docs/voicemeeter-report.md`（本文）
