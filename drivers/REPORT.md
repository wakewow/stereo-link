# StereoLink 虚拟声卡（A 路线：自研驱动）实施报告 — task-1

> 作者：teammate `vad-driver`　生成时间：2026-10-18（本机时间，Asia/Shanghai）
> 全部写入范围：`.scratch\vad\**`（仓库 `drivers/**` 未创建、未修改；`crates/**`、`scripts/**`、`docs/**`、`Cargo.toml`、根 `target/` 未改动）
> 状态：**构建 ✅ / 自签 ✅ / 安装 ✅（设备节点已建）/ 加载 ❌ 预期失败**；`bcdedit /set testsigning on` **按 Lead 指令未执行**（留到重启窗口）。

---

## 0. 结论速览

| 项目 | 结果 |
|---|---|
| 驱动工程 | 从 git `661c59b^` 恢复的 Scream 派生源码（41 文件），在 `.scratch/vad/drivers/` 可复现构建 |
| 构建产物 | `drivers\out\x64\Release\StereoLinkVad.sys`　30,720 B　未签名 SHA256 `9F4546…4455` |
| 打包产物 | `pkg-stereolink\StereoLinkVad.sys`（38,200 B，含签名）/ `StereoLinkVad.inf`（7,962 B）/ `stereolinkvad.cat`（8,494 B）/ `LICENSE-MS-PL.txt` |
| 签名 | ✅ 自签证书 + Inf2Cat + signtool（含 DigiCert 时间戳）。`signtool verify /pa` = **0（Successfully verified）** |
| 内核策略校验 | ❌ `signtool verify /kp` = 1，原文 **“Signing Cert does not chain to a Microsoft Root Cert.”** ← 这就是必须开测试签名的硬证据 |
| 安装 | ✅ `pnputil /add-driver … /install` 成功（发布名 **`oem19.inf`**）+ `devcon install` 建出设备节点 |
| 设备实例 id | **`ROOT\MEDIA\0001`**（Class `MEDIA`，友好名 **`立体声链虚拟声卡`**） |
| 当前状态 | `Status = Error`，**ProblemCode 52 (`CM_PROB_UNSIGNED_DRIVER`)**，ProblemStatus **`0xC0000428`**（`STATUS_INVALID_IMAGE_HASH`）——与本项目历史结论一致，**属预期** |
| 端点是否出现 | 未出现（render 端点 31 → 31）。驱动没加载 ⇒ 不会注册 KS 端点。**需重启 + 测试签名后验证** |
| 端点目标名 | **`扬声器 (立体声链虚拟声卡)`**（前半“扬声器”由系统本地化，后半来自 INF） |
| 许可证 | 上游 **MS-PL**（`Copyright (c) 2015 Microsoft`）。**允许改名与再分发**，须保留版权/许可声明；详见 §6 |
| 与 Steam 驱动 | **零冲突**：`SteamStreamingSpeakers` / `SteamStreamingMicrophone` 未被触碰（仍 `OK`）。但**端点名匹配有歧义风险**，详见 §7 |
| WASAPI 回环采集 | **静态结论：架构上支持**（本驱动是 render-only，正好就是回环采集需要的形态）；**实证待重启**，详见 §8 |
| 重启前必做 | 仅一条：`bcdedit /set testsigning on`（§9）。**尚未执行** |

---

## 1. 交付物与证据索引（全部在 `.scratch\vad\`）

```
drivers\                       恢复出来的驱动工程（含 build.ps1、scream-derived\、out\）
drivers\build.ps1              构建脚本（新增 -BinName 参数，默认 Scream；用来产出 StereoLinkVad.sys）
drivers\out\x64\Release\StereoLinkVad.sys      编译产物（未签名）
pristine\                      恢复后的原样副本（42 文件，含 UPSTREAM.md，只读参照）
inf-src\StereoLinkVad.inf.utf8 我们自己的 INF 主本（UTF-8，LF；可读可 diff）
make_package.ps1               INF 转 UTF-16LE+BOM + 拷 .sys + 拷 LICENSE → pkg-stereolink\（ASCII 脚本）
sign-vad.ps1                   自签证书 + Inf2Cat + signtool（ASCII 脚本）
install-vad.ps1                信任证书 + pnputil 暂存 + devcon 建设备 + 抓日志；-Uninstall 一键回滚
pkg-stereolink\                已签名驱动包（.inf / .sys / .cat / LICENSE-MS-PL.txt）
pkg-check\                     make_package.ps1 复现产物（证明 INF 可复现）
certs\stereolink-dev.cer       公钥证书 820 B
certs\stereolink-dev.pfx       私钥 2,654 B（口令不入库，见 sign-vad.ps1 参数，仅开发用）
logs\build.stereolinkvad.log   构建日志
logs\sign-vad.log              Inf2Cat + 签名日志（Signability test: Errors None / Warnings None）
evidence\install-vad-run.log   安装全过程原始输出（人读）
evidence\install-vad-run.json  安装全过程结构化数据（机器读）
evidence\sign-vad.json         签名/校验结构化数据
utf8view\                      给 PowerShell 5.1 看的 UTF-8 转码副本（见 §11 陷阱）
drivers.tar                    从 git 恢复用的归档（可删）
```

**构建 → 签名 → 安装的可复现链**（每条都能独立重跑）：

```powershell
cd .scratch\vad
powershell -NoProfile -ExecutionPolicy Bypass -File .\drivers\build.ps1 -Config Release -BinName StereoLinkVad
powershell -NoProfile -ExecutionPolicy Bypass -File .\make_package.ps1
powershell -NoProfile -ExecutionPolicy Bypass -File .\sign-vad.ps1    # 需 -PfxPassword 或 $env:STEREOLINK_DEV_PFX_PASSWORD
powershell -NoProfile -ExecutionPolicy Bypass -File .\install-vad.ps1
```

> 复现性已实测：重跑 `make_package.ps1` 得到的 `StereoLinkVad.inf` 与**实际被签名、被安装的那一份逐字节相同**
> （SHA256 `01CFD4A919EEEB2414E0C579927FE8A8F62F202E9A089D8565A68642CF24FAFD`）。
> 唯一差别是 `.sys`：`pkg-stereolink\` 里的是签名版（38,200 B），`out\` 里的是未签名版（30,720 B）——签名是**附加**在 PE 尾部的。

---

## 2. 驱动工程与构建（本机工具链事实）

| 事实 | 值 |
|---|---|
| 恢复方式 | `git -C . archive 661c59b^ drivers \| tar -x -C .scratch\vad`（注意：`git archive` + 管道给 `tar` 会报 `bad header checksum`，改用 `-o` 落盘再解包即可） |
| 源码血统 | Scream（`github.com/duncanthrax/scream`，commit `d789743c248b11d1df7e5ecc546b1bc60b90cd91`）← MSVAD（微软 WDK 示例） |
| WDK | `C:\Program Files (x86)\Windows Kits\10`，使用 **10.0.26100.0**（`bin\10.0.26100.0\x64\signtool.exe`、`bin\10.0.26100.0\x86\Inf2Cat.exe`、`Tools\10.0.26100.0\x64\devcon.exe`） |
| 编译器 | VS 2022 BuildTools（**无 WDK VS 集成**），MSVC 14.44.35207，`build.ps1` 直接驱动 `cl.exe`/`link.exe` |
| Shell | 只有 **Windows PowerShell 5.1**（本机无 `pwsh`）；`[IO.File]::ReadAllLines` 不认 PowerShell 的 `cd`，要用绝对路径 |
| 管理员 | 已是 High integrity，全程无 UAC 弹窗 |

**相对上游我们改了什么**（`inf-src\StereoLinkVad.inf.utf8` vs `pristine\pkg\Scream.inf`，逐行 diff 得出）：

- 结构、`Include=ks.inf,wdmaudio.inf`、`Needs=KS.Registration, WDMAUDIO.Registration`、`AddInterface` 三条、`[Strings]` 键名布局：**完全沿用上游**。
- 标识符：`CatalogFile` → `stereolinkvad.cat`；拷贝文件 → `StereoLinkVad.sys`；服务名 → `StereoLinkVad`；硬件 id → **`*StereoLinkVad`**；`Provider`/`MfgName` → `StereoLink`；`Manufacturer` 段只保留 `NTAMD64`（上游有 4 个架构，我们只构建 x64）。
- 显示名（这就是“改名”的全部内容）：`DeviceDesc` / `Wave.szPname` / `Topo.szPname` → `立体声链虚拟声卡`，`SvcDesc` → `立体声链虚拟声卡 (StereoLink Virtual Audio)`。
  （恢复出来的旧副本原来用的是早期产品名，上游原本是 `Scream (WDM)`。）
- 内核代码：`scream.rc` 里的产品名/描述已改；`.cpp/.h` 逻辑**未改动**。

**已知小坑（建议后续修）**：`DriverVer = 02/22/2007, 6.0.6000.1` 是从上游原样继承的旧版本号。同版本号重装时 DriverStore 会判定“无更新”，`pnputil /add-driver` 可能走旧副本路径。**下次要改动驱动内容时，请同时把 `DriverVer` 递增**（例如 `10/18/2026, 1.0.0.1`），否则改完文件装上去可能还是老的。

---

## 3. 签名与内核策略校验（核心证据）

- 证书：自签，`CN=StereoLink Virtual Audio Driver (dev)`，`New-SelfSignedCertificate -Type CodeSigningCert`，指纹 `ECF9A32EB678CA80766229C5D1BC3BEB3FB772BB`，有效期 2026-10-18 → 2031-10-18。
- `Inf2Cat /driver:pkg-stereolink /os:10_GE_X64 /verbose` → `Signability test complete. Errors: None / Warnings: None`，产出 catalog。
- `signtool sign /v /fd SHA256 /f certs\stereolink-dev.pfx /p <pwd> /tr http://timestamp.digicert.com /td SHA256` → `.sys` 与 `.cat` 均签名成功（时间戳由 DigiCert 签发，链到 `DigiCert Assured ID Root CA`）。
- **把我们的根证书导入 `LocalMachine\Root` 之后**（`install-vad.ps1` 第 1 步自动做）：

| 校验 | 退出码 | 结论 |
|---|---|---|
| `signtool verify /pa StereoLinkVad.sys` | **0** | `Successfully verified`（用户态信任链 OK） |
| `signtool verify /pa stereolinkvad.cat` | **0** | 同上 |
| `signtool verify /kp StereoLinkVad.sys` | **1** | **`SignTool Error: Signing Cert does not chain to a Microsoft Root Cert.`** |

> 结论：**自签证书无论怎么导入，都过不了内核模式代码签名策略（/kp）**——内核只认链到 Microsoft 根的证书（WHQL/Attestation 签名），或者 **测试签名模式**。这就是 `0xC0000428` 的唯一原因，也说明本机除“开测试签名/买微软签名”外没有第三条路。

---

## 4. 安装结果（未启用测试签名时的真实状态）

```
pnputil /add-driver …\StereoLinkVad.inf /install
  → Driver package added successfully.  Published Name: oem19.inf   (exit 0)
devcon install …\StereoLinkVad.inf *StereoLinkVad
  → Device node created.  Updating drivers for *StereoLinkVad …  Drivers installed successfully.
```

| 项目 | 值 |
|---|---|
| 设备实例 id | **`ROOT\MEDIA\0001`** |
| 设备类 | `MEDIA` |
| 友好名（设备管理器） | `立体声链虚拟声卡`（来自 INF `[StereoLinkVad_Device.NT.HW.AddReg] HKR,,FriendlyName,,%DeviceDesc%`） |
| 状态 | `Status = Error`，`ProblemCode = 52`，`ProblemStatus = 0xC0000428` |
| 问题描述原文 | *“Windows cannot verify the digital signature for the drivers required for this device… (Code 52)”* |
| 内核服务 | `StereoLinkVad`，`DisplayName = 立体声链虚拟声卡 (StereoLink Virtual Audio)`，`Type=1`，`Start=3`（Manual），`ErrorControl=1`，`ImagePath = \SystemRoot\system32\drivers\StereoLinkVad.sys` |
| 文件落地 | `C:\Windows\System32\drivers\StereoLinkVad.sys` ✅ 已复制 |
| 代码完整性事件 | `Microsoft-Windows-CodeIntegrity/Operational` **Event 3004**：*“…unable to verify the image integrity of the file \Device\HarddiskVolume2\Windows\System32\drivers\StereoLinkVad.sys because file hash could not be found on the system…”* |
| 内核 PnP 事件 | `Kernel-PnP` Event 219（驱动加载失败）+ `Service Control Manager` Event 7045（服务已安装） |
| 渲染端点数量 | 安装前 **31** → 安装后 **31**（驱动未加载，故无新端点；符合预期） |
| Steam 驱动 | `ROOT\STEAMSTREAMINGSPEAKERS\0000` Status **OK**、`ROOT\STEAMSTREAMINGMICROPHONE\0000` Status **OK** —— 未卸载、未改名、未触碰 |

原始输出见 `evidence\install-vad-run.log` 与 `evidence\install-vad-run.json`。

---

## 5. 端点命名机制：INF 里到底哪个字段决定「扬声器 (XXX)」

分两半，**只有后半由我们控制**：

1. **后半 `(立体声链虚拟声卡)`** —— 由 KS filter 的友好名决定，来自 INF 里这一串：
   - `[StereoLinkVad_Device.NT.Interfaces]`
     `AddInterface=%KSCATEGORY_RENDER%,%KSNAME_Wave%,StereoLinkVad_Device.I.Wave`
   - `[StereoLinkVad_Device.I.Wave.AddReg]`
     `HKR,,FriendlyName,,%Wave.szPname%`　←　**这一条是端点括号里那个名字**
   - 拓扑侧同理：`AddInterface=%KSCATEGORY_AUDIO%,%KSNAME_Topology%…` + `HKR,,FriendlyName,,%Topo.szPname%`
   - `[Strings]` 里：`Wave.szPname="立体声链虚拟声卡"`、`Topo.szPname="立体声链虚拟声卡"`（当前值）
   - 上游对照：`pristine\scream-derived\Scream.upstream.inf` 里 `Scream.DeviceDesc="Scream (WDM)"` ⇒ 端点就是 `Speakers (Scream (WDM))`。
2. **前半 `扬声器`** —— 是 Windows 对 render pin（`KSNODETYPE_SPEAKER`）的**系统本地化名**，**INF 改不了**。中文系统上就是「扬声器」。
3. 另外两条只是旁支，不影响端点显示名，但顺手也指向同一个 `%DeviceDesc%`：
   - `HKLM,%MediaCategories%\%NameGuid%,Name,,%DeviceDesc%`（KS 类别名，`{946A7B1A-EBBC-422a-A81F-F07C8D40D3B4}`）
   - `HKR,Drivers\wave\wdmaud.drv,Description,,%DeviceDesc%` 与 `…\mixer\wdmaud.drv,…`（给旧 MME 看的）
4. 设备管理器里显示的设备名来自 `[StereoLinkVad_Device.NT.HW.AddReg] HKR,,FriendlyName,,%DeviceDesc%`。

> 所以：**要改端点名，只改 `[Strings]` 的 `Wave.szPname`/`Topo.szPname`（以及 `DeviceDesc` 保持一致）**。改完必须重跑 `make_package → sign-vad → install-vad`（INF 参与 catalog 哈希）。

---

## 6. 许可证结论（MS-PL）：能否改名、能否再分发、必须保留什么

**许可证文件与出处**

- 上游许可证正文：`.scratch\vad\pristine\scream-derived\LICENSE-MS-PL.txt`
  （上游 `LICENSE` 原样；同文件也在 `drivers\scream-derived\LICENSE-MS-PL.txt` 与 `pkg-stereolink\LICENSE-MS-PL.txt`）
- 上游来源登记：`.scratch\vad\pristine\UPSTREAM.md`（仓库 `duncanthrax/scream`，commit `d789743c248b11d1df7e5ecc546b1bc60b90cd91`，快照 2026-10-17）
- 上游署名：`pristine\pkg\Scream.inf` 的 `[Strings] SCR="Tom Kistner"`

**原文摘录（逐字）**

> `LICENSE-MS-PL.txt:1-2`
> `The Microsoft Public License (MS-PL)`
> `Copyright (c) 2015 Microsoft`

> `§2. Grant of Rights (A)`：`each contributor grants you a non-exclusive, worldwide, royalty-free copyright license to reproduce its contribution, prepare derivative works of its contribution, and distribute its contribution or any derivative works that you create.`

> `§3. Conditions and Limitations (A)`：`No Trademark License- This license does not grant you rights to use any contributors' name, logo, or trademarks.`

> `§3 (C)`：`If you distribute any portion of the software, you must retain all copyright, patent, trademark, and attribution notices that are present in the software.`

> `§3 (D)`：`If you distribute any portion of the software in source code form, you may do so only under this license by including a complete copy of this license with your distribution. If you distribute any portion of the software in compiled or object code form, you may only do so under a license that complies with this license.`

> `§3 (E)`：`The software is licensed "as-is." You bear the risk of using it.`

**结论**

1. **可以改名**：§2(A) 明确授予“prepare derivative works … and distribute … any derivative works”。把 `Scream (WDM)` 改成 `立体声链虚拟声卡`、把服务名/硬件 id 改成 `StereoLinkVad`/`*StereoLinkVad` 都在授权范围内。
2. **不能再叫“Scream”当产品名**：§3(A) 不授予商标许可，所以“Scream”只能作为**描述性出处说明**出现（我们在 INF 头部与本文档就是这么做）。
3. **可以再分发**（含**闭源二进制**形式）：§3(D) 允许“compiled or object code form”，条件是那一层的分发许可**与本许可证相容**。对内部分发/自用没有额外义务；对外发布二进制时建议同时附许可证与来源说明。
4. **必须保留的声明**（§3(C)，硬性）：
   - `LICENSE-MS-PL.txt` 全文（已放入 `pkg-stereolink\`，与 INF 头部“A complete copy of that licence ships alongside this package”一致）；
   - 源码文件头里既有的 Microsoft / Tom Kistner 版权与出处注释（**本次一个都没删**，只改了字符串与标识符）；
   - 上游来源与 commit（`pristine\UPSTREAM.md` 已登记）。
5. 血统要写全：MSVAD（Microsoft，MS-PL）→ Scream（Tom Kistner，MS-PL）→ StereoLinkVad。我们这一层的改动/新增文件（`build.ps1`、`make_package.ps1`、`sign-vad.ps1`、`install-vad.ps1`、`StereoLinkVad.inf`）按 §3(C) 同样保留出处注释。

---

## 7. 与 Steam Streaming Speakers 的冲突分析（这条要立刻处理）

**当前本机 render 端点里的两个“立体声链”**（`HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Render`）：

| 端点 GUID | 显示名 | 来自 |
|---|---|---|
| `{4a82bbbc-667f-4a9b-b4ce-520a165e6240}` | `立体声链设备` | **Steam Streaming Speakers 被改名的那个**（`ROOT\STEAMSTREAMINGSPEAKERS\0000`） |
| （重启后新增） | `立体声链虚拟声卡` | **我们自己的驱动**（`ROOT\MEDIA\0001`，即 `扬声器 (立体声链虚拟声卡)`） |

**硬件层面不冲突**：两个是不同 devnode、不同服务、不同实例 id，Windows 可以同时存在，互不干扰。

**软件层面有真实踩坑风险**（重要）：

- 我们代码里的默认采集源端点是 `source_endpoint: "立体声链"` 这种**子串匹配**：
  `crates/stereo-audio/src/route.rs:175`、`route.rs:1111`、`crates/stereo-audio/src/btsplit.rs:70`。
- 匹配实现 `crates/stereo-audio/src/com.rs:289-303`（`DeviceSelector::NameContains`）**遍历到第一个命中就 `return`，既不排序也不报歧义**。
- ⇒ 一旦我们的端点出现，`"立体声链"` 会命中**两个**端点，选哪个取决于枚举顺序 —— **可能静默把 Steam 的当成自己的**，采集到空数据还找不到原因。

**建议（按代价从小到大）**：

1. **立刻（一行级）**：把默认值从 `"立体声链"` 改成更长的唯一子串 **`"立体声链虚拟声卡"`**（三处：`route.rs:175`、`route.rs:1111`、`btsplit.rs:70`；对应断言 `route.rs:1136`、`route.rs:1440`、`route.rs:1529` 也要跟着改）。
2. **更稳（推荐）**：让 `com.rs` 的 `NameContains` 分支在**多个命中时报错**（把候选列表按现有 `describe_one` 格式吐出来），而不是静默取第一个。这样任何命名歧义都会立刻暴露。
3. **最稳**：链路固定用 `DeviceSelector::Id`（`com.rs:273-288` 已支持），或按**设备实例 id** 判定（我们的恒定以 `ROOT\MEDIA\` 开头、Steam 的是 `ROOT\STEAMSTREAMINGSPEAKERS\`）—— 名字随便改都不会抓错。
4. **清理历史债**：我们自己的端点跑通后，建议把 Steam Streaming Speakers 的显示名**恢复成原名**（`立体声链设备` → `Steam Streaming Speakers`）。那个改名本身就是 A 路线要取代的临时手段，两个“立体声链”并存只会制造混淆。

**建议的端点显示名：保持 `立体声链虚拟声卡`**（即 `扬声器 (立体声链虚拟声卡)`）。理由：与 `立体声链设备` 在子串上可区分、语义准确、且**当前已经签名+装好**；改名就要重跑签名与 catalog，收益为零。

---

## 8. WASAPI 回环采集能否工作（静态分析结论 + 待验证项）

**静态结论：架构上可行，且本驱动“只有 render 没有 capture”正是回环采集需要的形态。**

证据（读的是 `.scratch\vad\drivers\scream-derived\`，与上游一致）：

| 证据 | 含义 |
|---|---|
| `scream.h:58` `#define MAX_OUTPUT_STREAMS 0 // Number of capture streams` | 驱动**不提供采集 pin** |
| `scream.h:59` `#define MAX_INPUT_STREAMS 1 // Number of render streams` | 只有 **1 路 render**（`minwave.cpp:232-233` 把它填进 `m_MaxInputStreams`） |
| `wavtable.h:70-71` `KSPIN_DATAFLOW_IN, KSPIN_COMMUNICATION_SINK` | 波形 pin 是 **sink（接受播放数据）** |
| INF 只 `AddInterface=%KSCATEGORY_RENDER%…`（`KSCATEGORY_CAPTURE` 只当常量声明、从不注册） | 系统里只会多出 **1 个 render 端点**，不会多出 capture 端点 |
| `savedata.cpp:415` `WskSendTo()`、`adapter.cpp:501,506` | 数据被驱动取走后经 UDP 发走 —— 也就是我们要拦下来的那一路 |

为什么“没有 capture pin”不是问题：**WASAPI 回环（`AUDCLNT_STREAMFLAGS_LOOPBACK`）是音频引擎侧的能力**，它是用 `eRender` 端点 + 回环标志打开流，**不需要驱动提供 capture 端点**。因此：

- ✅ 前提条件全部满足：端点以 `KSCATEGORY_RENDER` 注册、格式域为 PCM / 44.1k–192k / 16–32 bit / 1–8 ch（`wavtable.h`）、拓扑提供 speaker 节点。
- ⚠️ **唯一真正的未知**：`audiodg` 能否以共享模式打开这个 KS pin。如果 KS 格式协商和我们 `CaptureMode::Loopback` 期望的共享格式对不上，症状会是“**端点出现了，但回环流拿不到数据/打不开**”。这必须实证，不能纸上定论。
- ⚠️ 次要未知：本驱动的 `KSDATAFORMAT` 支持域里**没有 IEEE float**（仅 PCM）。引擎共享格式通常是 float32，但引擎会自己做转换；我们的采集侧还开了 `AUTOCONVERTPCM`，所以风险低、仍需实测。

**重启后怎么实证**（见 §10 第 5–7 步）：`endpoints.exe` 看端点 → `format_probe --all` 看每个 render 端点的共享格式与回环探测（`format_probe.rs:207-212` 就是用 `CaptureMode::Loopback` 去解析的）→ 真正放一段音频走 `route_demo`/`stereo-link.exe` 看是否收到数据。

---

## 9. 重启前清单（Lead 控制时机；**目前一步都没执行**）

```powershell
# 1) 唯一必需的一步：开启测试签名模式（管理员）
bcdedit /set testsigning on

# 2) 复核（应出现一行 `testsigning             Yes`）
bcdedit /enum {current}

# 3) 复核 Secure Boot 仍为 False（本机实测 False；若为 True，测试签名不会生效，必须先关 Secure Boot）
Confirm-SecureBootUEFI

# 4) 记录重启前的现状（问题码应为 52 / 0xC0000428）
pnputil /enum-devices /instanceid "ROOT\MEDIA\0001"

# 5) 重启（由 Lead 安排；不要在这里自动重启）
shutdown /r /t 0
```

- 重启后桌面右下角会出现 **“测试模式”水印**（正常现象，回滚后消失）。
- 现在**没有**开启测试签名，也**没有**任何其他 boot 配置改动（`bcdedit /enum {current}` 里既没有 `testsigning` 也没有 `nointegritychecks`）。

---

## 10. 重启后验证清单（端点是否真的出现）

```powershell
# 1) 测试签名已生效
bcdedit /enum {current}                 # 期望：testsigning  Yes
# 2) 设备状态应从 Error/52 变成 OK
pnputil /enum-devices /class MEDIA
pnputil /enum-devices /instanceid "ROOT\MEDIA\0001"     # 期望：Status: OK，无 Problem code
Get-PnpDevice -InstanceId 'ROOT\MEDIA\0001' | Select Status,FriendlyName,ProblemDescription
# 3) 内核服务是否在跑（期望 Running 或按需启动）
Get-Service StereoLinkVad | Select Name,Status,StartType
# 4) 端点注册表：数量应从 31 → 32，新端点 DeviceState = 1 (ACTIVE)
$r='HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Render'
Get-ChildItem $r | ForEach-Object {
  $p = Get-ItemProperty "$($_.PSPath)\Properties" -ErrorAction SilentlyContinue
  foreach ($n in $p.PSObject.Properties.Name) {
    if ("$($p.$n)" -match '立体声链') { "{0}  {1} = {2}" -f $_.PSChildName, $n, $p.$n }
  }
}                                        # 期望看到 立体声链虚拟声卡 与 立体声链设备 两条
# 5) 我们的工具：列端点（期望出现 扬声器 (立体声链虚拟声卡)）
target\debug\endpoints.exe
#    没有现成 exe 就：cargo run -q -p stereo-audio --bin endpoints
# 6) 格式 + 回环探测（每个 render 端点，含新端点）
cargo run -q -p stereo-audio --example format_probe -- --all
# 7) 只探我们的端点
cargo run -q -p stereo-audio --example format_probe -- "立体声链虚拟声卡"
# 8) 代码完整性日志应不再出现 3004
Get-WinEvent -LogName Microsoft-Windows-CodeIntegrity/Operational -MaxEvents 30 |
  Where-Object Id -eq 3004 | Select -First 5 TimeCreated,Message
# 9) 端到端：让音频真的流过去
cargo run -q -p stereo-audio --example route_demo      # 或直接跑 target\debug\stereo-link.exe
```

**判定标准**：第 2 步 `Status: OK` 且第 4/5 步出现 `扬声器 (立体声链虚拟声卡)` ⇒ 驱动路线打通；第 7 步能拿到共享格式 ⇒ WASAPI 回环可行；第 9 步有数据 ⇒ 链路闭环。

---

## 11. 回滚方式（一条命令或手动四步）

**一键回滚**（推荐，`install-vad.ps1 -Uninstall` 会做下面全部，并且只删我们自己的东西）：

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .scratch\vad\install-vad.ps1 -Uninstall
```

**手动回滚 / 逐条复核**：

```powershell
bcdedit /set testsigning off                                              # 关闭测试签名（需重启生效）
pnputil /remove-device "ROOT\MEDIA\0001"                                  # 删设备节点
pnputil /delete-driver oem19.inf /uninstall /force                        # 删驱动包（先确认 oem19.inf 就是 StereoLinkVad.inf！）
sc.exe delete StereoLinkVad                                               # 删内核服务
Remove-Item Cert:\LocalMachine\Root\ECF9A32EB678CA80766229C5D1BC3BEB3FB772BB
Remove-Item Cert:\LocalMachine\TrustedPublisher\ECF9A32EB678CA80766229C5D1BC3BEB3FB772BB
shutdown /r /t 0
```

- 删 `oem19.inf` **之前必须复核**：`pnputil /enum-drivers` 里确认该条目的原始 INF 名是 `stereolinkvad.inf`（脚本已内建这个过滤，只删匹配项）。
- **绝对不要**卸载/改名/删除 Steam 的 `SteamStreamingSpeakers.sys` / `SteamStreamingMicrophone.sys` 及其设备；回滚脚本也不碰它们。
- 仓库侧无残留：`drivers/` 从未落进工作树（只存在于 `.scratch/vad/` 与 `drivers.tar`），`git status` 中与本次工作相关的改动**为零**。

---

## 12. 不开启测试签名的替代方案

| 方案 | 可行？ | 代价 / 结论 |
|---|---|---|
| **A. 买 EV 代码签名证书 + 微软 Attestation/WHQL 签名** | ✅ 唯一“正路” | 内核驱动可正式加载、零 boot 改动、无水印。需要：EV 证书（年费）+ 合作伙伴中心账号 + 对驱动做 HLK 测试并提交 `.cab` 拿微软签名。**周期数天到数周、要花钱**，不适合当前迭代节奏。它是这条路线未来要走向的终点。 |
| **B. 开测试签名（当前选择）** | ✅ | 一条 `bcdedit /set testsigning on` + 重启，零成本。代价：桌面水印；本机 Secure Boot 已是 False，**不存在 Secure Boot 冲突**（若以后打开 Secure Boot，测试签名会失效）。 |
| **C. 关掉驱动签名强制（`bcdedit /set nointegritychecks on`）** | ⚠️ 不推荐 | 也能放行，但那是**全局**禁用完整性检查，安全性和副作用都比测试签名差得多。本机 Secure Boot=False 时可行，但没必要。 |
| **D. 导入自签证书到本机 Root（已在做）** | ❌ 不够 | 只解决 `signtool /pa`（用户态），**过不了 `/kp` 内核策略**（§3 实测）。**必须**配合 B。 |
| **E. 第三方现成虚拟声卡**（VB-Cable / VoiceMeeter / 现有 Steam Speakers 改名） | ✅ 但违背决定 | 用户已拍板自研驱动（A 路线），且改名 Steam 设备本来就是被取代的临时手段。仅作为“驱动彻底跑不通”时的兜底。 |
| **F. 完全不装驱动：WASAPI 进程回环（Windows 10+ `AudioClient` process loopback）/ 应用内音频抓取** | ⚠️ 部分可用 | 无需任何驱动与签名，但只能抓**指定进程**的音频，无法像虚拟声卡那样成为**系统级默认播放端点**（其它 app 不会把音频送过来）。与目标「端点名为『扬声器 (立体声链虚拟声卡)』」不符。 |

> 结论：**在“必须是我们自己的端点”这个前提下，B 是唯一现实解**；中长期应把 A 列入计划（否则每个用户机器都要开测试签名）。

---

## 13. 最大不确定性与风险（按重要性）

1. **驱动能否在测试签名下真正加载 —— 最大未知。** 目前阻止它的只有签名（Code 52 / 0xC0000428），但 2020 年的 Scream 代码 + WDK 10.0.26100 组合里，**可能还藏着第二个错误**（例如 KS 注册、`ks.inf`/`wdmaudio.inf` 的 `Needs=` 处理、或 `DriverVer` 导致的旧副本复用）。只有重启后 `pnputil /enum-devices /instanceid "ROOT\MEDIA\0001"` 才能证实。
2. **WASAPI 回环能否真的取到数据**（§8）：架构上应该可以，但没有实证；若失败，症状是“端点出现但链路无声”，第一反应应查 `format_probe` 的共享格式与 `audiodg` 是否成功打开 pin。
3. **端点匹配歧义**（§7）：不修 `VAD_NAME_HINTS`，新端点出现后**很可能静默选错端点**。建议在验证前就把默认值改成 `立体声链虚拟声卡`。
4. **`DriverVer` 未递增**（§2）：同版本重装可能不生效，改驱动内容时务必递增。
5. 证书私钥 `certs\stereolink-dev.pfx` 的口令不写入仓库：用 `-PfxPassword` 或 `$env:STEREOLINK_DEV_PFX_PASSWORD` 提供；`.pfx` / `.pvk` / `.snk` 已被 `.gitignore` 忽略，**只能用于本机开发**，不可用于任何对外发布。

---

## 14. 附：本机环境事实（本次实测）

- Windows 11 build **26100**；shell 已是管理员（High integrity，无 UAC 弹窗）。
- `Confirm-SecureBootUEFI` = **False**（⇒ 测试签名不会被 Secure Boot 拦掉）。
- `bcdedit /enum {current}`：**无** `testsigning`、**无** `nointegritychecks`、`hypervisorlaunchtype Auto`、`bootmenupolicy Standard`。
- WDK：`C:\Program Files (x86)\Windows Kits\10`，使用 10.0.26100.0；另有 14393/15063/16299 等旧版本。
- VS 2022 BuildTools（无 WDK VS 集成）⇒ 用 `build.ps1` 直驱 `cl.exe`/`link.exe`。
- 本机只有 Windows PowerShell 5.1（无 `pwsh` 7）。
- **PowerShell 5.1 陷阱（踩过两次）**：无 BOM 的 UTF-8 文件会被按 ANSI 读 → 中文乱码（`Get-Content` 同样）。对策：脚本一律 ASCII；读 UTF-8 用 `[IO.File]::ReadAllText($p,[Text.Encoding]::UTF8)` 或 `-Encoding UTF8`；本目录的 `utf8view\` 就是为此留的转码副本。另外 `[IO.File]::*` 不认 PowerShell 的 `cd`，请用绝对路径。
- 渲染端点基线 **31** 个；相关既有设备：`ROOT\STEAMSTREAMINGSPEAKERS\0000`（显示名被改为 `立体声链设备`）、`ROOT\STEAMSTREAMINGMICROPHONE\0000`（`Steam Streaming Microphone`），两者均保持 `OK`、未被改动。
