# 阶段 3 · 自签虚拟音频驱动安装：**装得上，载不起来**

> **本文件回答一个唯一问题**：在 Secure Boot 已关闭、HVCI 未运行的前提下，只靠「自签证书 +
> 导入到 `LocalMachine\Root` / `LocalMachine\TrustedPublisher`」，能否让一个自研内核驱动
> **免 test signing** 装入并启动，从而让「妙播」出现在 Windows 的声音输出列表里？
>
> **答案：不能。** 驱动包能装、设备节点能造、Authenticode 校验能过，但内核 Code Integrity
> 在**启动驱动镜像**时拒绝执行：`0xC0000428 (STATUS_INVALID_IMAGE_HASH)`。
> 要让它跑起来，必须 **① 开 test signing（需重启）**，或 **② 用微软签名的驱动/证书**。
>
> 采集窗口：**2026-10-17 23:04 → 23:14 (+08:00)**，本机 Windows 11 26100 / AMD64 / Administrator。
> 下面每条结论后面都是本机实际命令的原始输出（证据文件索引见 §8）。

---

## 0. TL;DR —— lead 要的四个答案

| 问题 | 结论 | 证据 |
| --- | --- | --- |
| `pnputil /add-driver` 装包 | ✅ **成功**，exit 0，`Published Name: oem19.inf` | `pnp…` §2.2 |
| `devcon install` 造设备 | ✅ 命令 exit 0，但**它打印的 `Drivers installed successfully` 是误导**：设备随后进入 Error | §2.3 |
| `signtool verify /pa` | ✅ **exit 0**（签名完整，DigiCert 时间戳） | §2.1 |
| `signtool verify /kp` | ❌ exit 1：`Signing Cert does not chain to a Microsoft Root Cert.` | §2.1 |
| 新端点出现在 Render 列表？ | ❌ **没有**。26 → 26，一个都没多 | §2.5 |
| 确切错误码 | `CM_PROB_UNSIGNED_DRIVER (0x34)` + problem status **`0xC0000428`**；`sc start` → **577 ERROR_INVALID_IMAGE_HASH**；CodeIntegrity 事件 **3004** | §2.4 |
| 是不是 `0xE000024B`？ | **不是**。`0xE000024B` 是「驱动包被拒装」；我们连包都装上了，卡在**镜像加载**阶段 | §2.4 |
| 必须开 test signing 吗？ | **是**（自签路线下）；替代品是微软 attestation 签名（要 EV 证书）或改用已签名的第三方 VAD | §6 |
| 是 HVCI / 内存完整性造成的吗？ | **不是**。`SecurityServicesRunning=0`（HVCI 根本没运行），VBS 开着但没启用任何安全服务 | §3 |
| INF 改名生效了吗？ | ✅ **生效**：设备 FriendlyName = `小米妙播设备`，内核服务显示名 = `小米妙播设备 (虚拟声卡)` | §2.4 |
| 机器改干净了吗？ | ✅ 已回滚，逐条断言通过（设备/包/服务/文件/证书全部消失，26 个端点状态不变） | §4 |

**一句话给用户**：想让「妙播」进 Windows 输出列表，「自己写驱动」这条路**绕不开**微软签名或
test signing；而在**不花钱、不重启改启动配置**的前提下，唯一现成可用的做法是安装一个
**已被微软签名**的第三方虚拟声卡（VB-CABLE 等），再由我们的程序给它改名/供流。

---

## 1. 装了什么（可复现清单）

### 1.1 驱动来源与许可
| 项 | 值 |
| --- | --- |
| 上游 | `duncanthrax/scream`（MS-PL），commit `d789743c248b11d1df7e5ecc546b1bc60b90cd91`（2026-08-10） |
| 本仓库副本 | `drivers/scream-derived/`（仅源码 + `LICENSE-MS-PL.txt`，未改一行代码） |
| 编译产物 | `drivers/out/x64/Release/Scream.sys`，30 208 B，sha256 `6ce6dfad903854b71a18216f6c3710b06995a3cb88e34a1293ba65aec8f96596` |
| 编译方式 | `drivers/build.ps1`：直接驱动 `cl.exe` / `link.exe`（WDK 未装 VS 集成，MSBuild 用不了，详见 `driver-build.md`） |

### 1.2 自签证书（仅开发/验证用）
| 项 | 值 |
| --- | --- |
| 主体 | `CN=miplay-bridge Self-Signed Driver` |
| 指纹 | `36546DDA88D5A560028DA9C48DCFE4BB492CAD18` |
| 公钥文件 | `drivers/certs/miplay-dev.cer`（810 B，sha256 `c0f32c1a08e7c970ef63439fc59b8774a58499fb5cc9ee7b729e923f3d6536a2`） |
| 私钥 | **不在仓库里**（`%TEMP%\vad-research\certs\*.pfx`，本机临时目录） |
| 有效期 | 2026-10-17 → 2029-10-17 |
| 生成/签名脚本 | `drivers/sign.ps1`（自签 + `signtool /fd sha256 /tr http://timestamp.digicert.com /td sha256`） |

### 1.3 驱动包
| 文件 | 大小 | sha256 | 状态 |
| --- | --- | --- | --- |
| `drivers/pkg/Scream.sys` | 37 672 B | `aa9247b34d7f17afc2f15e2e9235524535db96f6d1afe8d81ceb949fea8ff64f` | 我们的编译产物 + 7464 B Authenticode 签名（PE 证书表 offset=30208，已核实） |
| `drivers/pkg/scream.cat` | 8 405 B | `50a8a9ff3780b94ddfc729ffdb780ea014357e3c171ccc8fb7be82dc0bbabd2d` | 已签名（CAT 由 `Inf2Cat.exe /os:10_GE_X64` 生成） |
| `drivers/pkg/Scream.inf` | 7 230 B | `b4a08a5421843793e02dc7e71d714c0fa03a531270690e51a214b682314fc05e` | 本地化副本：`Scream.DeviceDesc` / `Scream.Wave.szPname` / `Scream.Topo.szPname` = **`小米妙播设备`**（`drivers/make-localized-inf.py` 生成，`ClassGUID`/`CatalogFile`/`DriverVer` 未动） |

### 1.4 实际执行的命令（两条，本机原样跑过）
```powershell
# ① 编译 + 自签（生成 drivers\pkg\*）
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\drivers\build.ps1
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\drivers\sign.ps1

# ② 导入信任 → 验证 → 装包 → 造设备 → 抓快照与事件（写 research\vad\driver-install-run.*）
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\drivers\install.ps1
```
`install.ps1` 只做三件事，全部可逆，且**从不碰 `bcdedit`**、不改其它音频设备、不改默认播放设备：
1. `Import-Certificate` 把**公钥** cer 导进 `Cert:\LocalMachine\Root` 与 `Cert:\LocalMachine\TrustedPublisher`；
2. `pnputil /add-driver drivers\pkg\Scream.inf /install`；
3. `devcon.exe install drivers\pkg\Scream.inf *Scream` 造 root 枚举设备。

---

## 2. 逐条证据（全部为原始输出）

### 2.1 信任导入成功，Authenticode 通过，但内核链校验失败
`research/vad/driver-install-run.json` → `trustImported=true`；`verify`：
```
--- Import-Certificate -> Root              -> 36546DDA88D5A560028DA9C48DCFE4BB492CAD18  CN=miplay-bridge Self-Signed Driver
--- Import-Certificate -> TrustedPublisher  -> 同上

signtool verify /pa  exit=0
    Signing Certificate Chain:
    Issued to: miplay-bridge Self-Signed Driver
    ...
    The signature is timestamped: Sat Oct 17 23:06:58 2026
    Successfully verified: ...\drivers\pkg\Scream.sys
    Number of files successfully Verified: 1

signtool verify /kp  exit=1
    SignTool Error: Signing Cert does not chain to a Microsoft Root Cert.
```
→ **`/pa` 过了不等于能用**：`/pa` 走的是用户态 Authenticode 策略（信任我们导入的根），
`/kp` 走的是**内核模式**策略（必须链到 Microsoft 根）。这两条命令的差异就是整件事的缩影。

### 2.2 驱动包装入成功
```
Microsoft PnP Utility
Adding driver package:  Scream.inf
Driver package added successfully.
Published Name:         oem19.inf
Total driver packages:  1
Added driver packages:  1
exit=0
```

### 2.3 设备节点创建「成功」
```
devcon.exe install drivers\pkg\Scream.inf *Scream
Device node created. Install is complete when drivers are installed...
Updating drivers for *Scream from ...\drivers\pkg\Scream.inf.
Drivers installed successfully.
exit=0
```
**注意：`exit=0` 与 "Drivers installed successfully" 都不代表设备起来了。**
`devcon` 报的是「我把驱动关联到设备了」，启动是随后由 PnP 管理器异步做的，结果只体现在
设备状态与事件日志里（下一节）。这是本次验证最容易误判的一点。

### 2.4 设备启动失败：决定性证据
`C:\Windows\INF\setupapi.dev.log`（永久日志，可用 `Select-String` 复现）：
```
L262: Hardware Id  = *Scream
L272: flq: Copying 'C:\windows\System32\DriverStore\FileRepository\scream.inf_amd64_097ebe1264264629\Scream.sys'
              to 'C:\windows\system32\drivers\Scream.sys'
L283: dvi: Configuration: oem19.inf:*scream,Scream
L287: dvi: Start: ROOT\MEDIA\0001
L288: !    dvi: Device 'ROOT\MEDIA\0001' not started: Device has problem: 0x34
              (CM_PROB_UNSIGNED_DRIVER), problem status: 0xc0000428.
```
事件日志（`drivers/out/evidence/post-install-rejection.txt` 有完整清单）：
```
[Microsoft-Windows-Kernel-PnP/Configuration] 23:07:10 Id=411
  设备 ROOT\MEDIA\0001 在启动时出现问题。服务: Scream 问题: 0x34 问题状态: 0xC0000428
[System] 23:07:10 Id=219  Microsoft-Windows-Kernel-PnP
  驱动程序\Driver\Scream加载失败。Device： ROOT\MEDIA\0001 状态： 0xC0000428
[Microsoft-Windows-CodeIntegrity/Operational] 23:07:10 与 23:08:02 两次 Id=3004
  Windows is unable to verify the image integrity of the file
  \Device\HarddiskVolume2\Windows\System32\drivers\Scream.sys because file hash could not be found
  on the system. ...
[System] 23:08:02 Id=7000 Service Control Manager
  由于下列错误，小米妙播设备 (虚拟声卡) 服务启动失败:  Windows cannot verify the digital signature
  for this file. ...
```
手工 `sc start Scream`：
```
StartService FAILED 577 (ERROR_INVALID_IMAGE_HASH):
Windows cannot verify the digital signature for this file.
```
设备属性（`Get-PnpDevice -InstanceId ROOT\MEDIA\0001`，安装后立即读取）：
```
InstanceId    : ROOT\MEDIA\0001
FriendlyName  : @oem19.inf,%Scream.DeviceDesc%;小米妙播设备
Status        : Error
Problem       : 52  (0x34 = CM_PROB_UNSIGNED_DRIVER)
ProblemStatus : 0xC0000428  (STATUS_INVALID_IMAGE_HASH)
```
**关于错误码**：`0xE000024B` 是 `pnputil` 拒装驱动包时会返回的码（签名/目录文件不匹配那一类），
本次**没有**出现 —— 包和 `.cat` 都是好的。我们的失败发生在更后面一环：**已发布的镜像被
Code Integrity 拒绝加载**。因此「换个 INF/重签 cat」这类修补**不可能**解决问题。

**顺带的好消息：改名机制已被证明可用**。上面 `FriendlyName` 已经是本地化后的
`小米妙播设备`；SCM 事件 7045/7000 里的服务名也是 `小米妙播设备 (虚拟声卡)`。也就是说，
`Scream.DeviceDesc` 本地化 → 设备名 → 端点名这条链路是通的，
一旦驱动能加载，输出列表里就会是 **「扬声器 (小米妙播设备)」**（`扬声器` 前缀由 Windows 对
speaker 端点硬编码，去不掉，这是预期行为）。

### 2.5 没有产生任何端点
```
before: time=2026-10-17T23:07:08  secureBoot=False  renderEndpoints=26
after : time=2026-10-17T23:07:13  secureBoot=False  renderEndpoints=26
```
MMDevices 的 Render 键数量前后一致，新增 GUID 列表为空。**音频端点是由能启动的 KS 过滤器
创建的；驱动没起来，端点自然不存在。** 所以「驱动装上了」在本项目里**不是**任何意义上的进展，
只有「端点出现 + 出声」才算。

---

## 3. 为什么会这样：策略链

1. **内核模式代码签名（KMCS）强制**：自 Windows 10 1607 起，新的内核驱动必须由
   **Microsoft 开发人员门户签名**（attestation 或 WHQL），否则不加载。
   - 文档：[Kernel-mode code signing policy (Windows Vista and later)](https://learn.microsoft.com/en-us/windows-hardware/drivers/install/kernel-mode-code-signing-policy--windows-vista-and-later-)
   - 文档：[Driver signing policy](https://learn.microsoft.com/en-us/windows-hardware/drivers/install/driver-signing-policy) /
     [Attestation signing](https://learn.microsoft.com/en-us/windows-hardware/drivers/dashboard/code-signing-attestation)
2. **我们导入了 Trusted Root / Trusted Publisher，为什么还不行**：这两个存储影响的是
   **用户态**的 Authenticode 与「驱动包是否被信任安装」；它们**不能让一个非微软根的证书
   获得内核加载许可**。本次实测就是这个结论的直接证据（§2.1 vs §2.4）。
3. **排除 HVCI / 内存完整性**：`Win32_DeviceGuard` → `VirtualizationBasedSecurityStatus=2`，
   但 `SecurityServicesRunning=0`（**HVCI 未运行**）、`CodeIntegrityPolicyEnforcementStatus=2`
   （KMCS 强制中）。即使关掉 VBS 也不会改变结论：拦住我们的是 KMCS，不是 HVCI。
4. **Secure Boot 不是变量**：本机 `Secure Boot = False`（UEFI 存在但已关）。开着只会更严，
   关着也**不会**放宽 KMCS。
5. **唯一的「例外通道」是 test signing**：`bcdedit /set testsigning on` + 重启后，内核
   改为接受「任意受信任根签发」的驱动镜像，自签证书随即生效。代价是桌面水印「测试模式」、
   每次启动的信任面扩大。

> **关于「免 test signing 的传说」**：网上流传的 `UpgradedSystem=1`、
> 「Secure Boot 关掉就能装 cross-signed 驱动」等做法，都建立在 **2015-07-29 之前签发的
> cross-signed 证书**上（微软早已吊销/停发这条链）。2026 年新做的自签证书不属于该例外，
> 本次实测也证实了这一点。**未做**未经授权的注册表/启动项改动。

---

## 4. 回滚：已完成，并逐条验证（这是硬要求）

`install.ps1 -Uninstall` 跑完后仍残留了两处（**已手工清干净，并把修复写回脚本**）：

| 残留 | 原因 | 处理 |
| --- | --- | --- |
| `ROOT\MEDIA\0001` 设备未删 | 脚本在 `Set-StrictMode -Version Latest` 下用 `(Get-PnpDeviceProperty …).Data` 探测服务名，遇到 `$null.Data` 抛 `PropertyNotFoundStrict`，导致设备循环整体失效 | 已改为 `try/catch` 探测；本次用 `pnputil /remove-device "ROOT\MEDIA\0001"` 手工删除（exit 0） |
| `HKLM\SYSTEM\CurrentControlSet\Services\Scream` 服务键残留 | `pnputil /delete-driver /uninstall` 删掉了包与文件，但没有删掉内核服务键 | 已用 `sc delete Scream` 删除，并在脚本里加了「删残留服务 + 断言」步骤 |

回滚后的最终断言（23:12–23:13 实测）：
```
ROOT\MEDIA\0001 设备            : 不存在
oem19.inf / scream.inf 包       : 均不存在（pnputil /enum-drivers）
Services\Scream 服务键           : 不存在（sc delete SUCCESS）
C:\Windows\System32\drivers\Scream.sys : 不存在
DriverStore\FileRepository\scream.inf_* : 0 个
Cert:\LocalMachine\{Root,TrustedPublisher,My} 中 CN=MiPlay* : 0 条
bcdedit testsigning             : 仍然 ABSENT（全程未启用 test signing）
render 端点                    : 26 个（与装前一致）
MEDIA 类设备                    : 13 个（与装前一致）
```
setupapi.dev.log 里也留下了完整的回滚轨迹，可直接复现查验：
```
L300: >>>  [Delete Device - ROOT\MEDIA\0001]        L302: pnputil.exe /remove-device ROOT\MEDIA\0001
L331: {Add Service: Scream} -> Deleted service 'Scream'.
L896/897: DeleteFile: 'C:\windows\system32\drivers\Scream.sys'
L901-L913: Unpublish / Deindex / Unregister / Delete Directory  scream.inf_amd64_097ebe1264264629
```

**端点快照对比**（`drivers/endpoint-snapshot.ps1` + `drivers/endpoint-diff.ps1`）：
```
before: research\vad\endpoints-before-driver.json    26 render endpoints, 13 MEDIA devices, secureBoot=False
after : research\vad\endpoints-after-rollback.json   26 render endpoints, 13 MEDIA devices, secureBoot=False
ADDED 0 / REMOVED 0 / CHANGED 1
```
唯一 CHANGED 的端点 **不是**我们造成的，见下节。

---

## 5. 时序交叉污染说明（重要，避免误判）

Lead 在 23:05 前后通知：`vad-rename-prober`（task-12）在同一时间窗内对
`ROOT\STEAMSTREAMINGSPEAKERS\0000` 这个未占用的 Steam 端点做**真实改名**实验。我的快照确实
抓到了它的痕迹：

```
CHANGED {4a82bbbc-667f-4a9b-b4ce-520a165e6240}
  before: state=1; name=扬声器 (Steam Streaming Speakers); desc=Steam Streaming Speakers
  after : state=1; name=扬声器 (小米妙播设备);              desc=小米妙播设备
```
归属判定（三条独立理由）：
1. 该端点属于 `ROOT\STEAMSTREAMINGSPEAKERS\0000`，服务名 `SteamStreamingSpeakers`，
   与我们的 `ROOT\MEDIA\0001` / 服务 `Scream` 无关；
2. 我们的驱动**从未启动**（§2.4），不可能创建或改名任何端点；一个音频驱动也无法改写
   别人端点的 MMDevices 属性；
3. 时间线吻合：我的装前快照 23:04:28 里该端点仍是 `Steam Streaming Speakers`，
   装后/回滚后（23:13:06）变成 `小米妙播设备` —— 正是 task-12 的 1 分钟窗口
   （其备份 `backup-Enum-ROOT-STEAMSTREAMINGSPEAKERS.reg` 时间戳 22:31，实验脚本
   `device-hwkey-rename-test.ps1` 22:33）。

**结论**：`endpoints-after-rollback.json` 与本报告的「26 → 26」结论成立；上面那一条 CHANGED
应归入 task-12 的报告，**不要**当成「自签驱动新增了端点」。反过来也请 task-12 注意：
窗口内**没有**出现任何 `Scream` / `ROOT\MEDIA\*` 端点（因为驱动没起来）。

> 附带发现（对项目有用）：Steam 端点的显示名能被写进
> `MMDevices\Audio\Render\{guid}\Properties`（`{a45c254e-…},2` 与 `{b3f8fa53-…},15`），
> 且不需要任何驱动 —— 这正是 `rename-probe.md` 的内容。它意味着：
> **只要有一个能创建端点的（微软签名的）驱动，命名就完全在我们的控制内。**

---

## 6. 本驱动服务的设备范围（Lead 要求写清）

驱动（或任何替代它的 VAD）**不是**为了给卧室立体声组合提供低延迟通路 —— 那条路已经有更好的解：
原生 AirPlay 2 直推 `Xiaomi Sound-左 L16A (192.168.1.21)` 即可整组出声，左右分离正常，
延迟约 300 ms，**完全绕开小米云和账号**（见 `docs/ear-test-stereo-pair.md`）。

「妙播设备出现在 Windows 输出列表」这件事的价值在于**那些没有本地直连通路的设备**：

| 设备 | 本地通路 | 是否需要本驱动/端点 |
| --- | --- | --- |
| 立体声组合（L16B + L16A） | ✅ AirPlay 2 直推整组 | ❌ 不需要（有更优解） |
| **小爱音箱 Play 增强版 L05C（did 931892755）** | ❌ **只能走小米云** | ✅ **主要服务对象** |
| 将来任何非 AirPlay 的妙播设备（蓝牙音箱、老款小爱、Sound 系列其它型号） | 视型号 | ✅ 目标范围 |

换句话说：**端点的存在意义 = 把「只能走云」的设备变成系统级可选输出**，让任何 Windows 程序
（浏览器、播放器、游戏）都能直接选它出声，而不需要为每个 App 做适配。

---

## 7. 下一步：三条出路（都需要用户拍板）

| 方案 | 做法 | 花钱 | 需要重启/改启动配置 | 产品可分发 | 风险 |
| --- | --- | --- | --- | --- | --- |
| **① 开 test signing（仅开发机）** | `bcdedit /set testsigning on` + 重启 → 自签驱动立即能装能跑（本机 Secure Boot 已关，HVCI 未运行，条件具备） | ¥0 | ✅ 重启一次；桌面出现「测试模式」水印；系统信任面扩大 | ❌ 不能给最终用户这么干 | 低（开发机）；产品化不可接受 |
| **② 微软 attestation 签名（产品级）** | 买 **EV 代码签名证书**（约 $200–600/年，需硬件令牌/HSM）→ 注册 Partner Center → 提交 `.cab`（`Inf2Cat` 产物）→ 微软签名 `.cat` 回来后即可在任意 Win10/11 上安装 | 💰 年费 | ❌ 不需要 | ✅ **可以** | 中：证书采购周期、attestation 审核、后续每次更新都要重签 |
| **③ 不自研驱动，改用已签名的第三方 VAD** | 让用户装 VB-CABLE（VB-Audio，免费；商用分发需其授权）/ Virtual Audio Cable（付费），我们的程序把音频推到它的端点，并把端点名改成 `妙播 - <设备名>` | ¥0（用户自装） | ❌ | ⚠️ 需遵守对方 EULA，不能捆绑安装 | 低技术风险；用户体验多一步手动安装 |
| （④ 自写驱动 + WHQL/HLK 正式认证） | 完整 HLK 测试 + 微软 WHQL 签名 | 💰💰 + 大量工时 | ❌ | ✅ | 高成本，本项目规模不划算 |

**我的建议排序**（供 Lead 与用户确认）：
1. **立刻可验证**：请用户批准 **①**（仅本机、可随时 `bcdedit /set testsigning off` 回退），
   我们用已经编译并签好的包把「端点出现 + 出声」跑通 —— 全部材料已就绪，只差这一个开关。
2. **产品路线**：与用户确认是走 **②（买 EV 证书，自己驱动）** 还是 **③（依赖已签名第三方 VAD）**。
   若用户短期不想花钱、也不接受装第三方 VAD，则本项目「妙播 → Windows 输出列表」这一条硬需求
   在技术上**无法**在「不开 test signing、不买证书」的前提下实现（本报告即为证据）。
3. 无论哪条路，`drivers/` 里的构建/签名/安装/回滚脚本都可继续用；切换的只是**信任来源**。

---

## 8. 复现与证据索引

| 文件 | 内容 |
| --- | --- |
| `research/vad/driver-install-run.json` / `.log` | 本次安装的完整日志（trust 导入、`/pa`、`/kp`、`pnputil`、`devcon`、前后快照、CodeIntegrity 事件） |
| `drivers/out/evidence/post-install-rejection.txt` | 拒绝证据汇编：setupapi.dev.log 摘录 + CodeIntegrity 3004 + Kernel-PnP 411/219 + SCM 7045/7000 + 回滚轨迹 |
| `drivers/out/evidence/rollback-run.{json,log}` | `install.ps1 -Uninstall` 的执行记录 |
| `drivers/out/rollback.log` | 回滚全过程 stdout（含手工补删设备/服务前的状态） |
| `research/vad/endpoints-before-driver.json` | 装前端点快照（26 个，23:04:28） |
| `research/vad/endpoints-after-rollback.json` | 回滚后端点快照（26 个，23:13:06） |
| `drivers/endpoint-snapshot.ps1` / `endpoint-diff.ps1` | 快照与差分工具（可随时重跑） |
| `research/vad/wdk-setup.md` / `driver-build.md` | 阶段 1 / 阶段 2 报告（WDK 安装、如何在没有 VS 集成的机器上把驱动编出来） |

复现「内核拒绝自签驱动」最小步骤（**会改系统，需授权**）：
```powershell
.\drivers\sign.ps1            # 生成/复用自签证书并签 .sys/.cat
.\drivers\install.ps1         # 导信任 -> 装包 -> 造设备 -> 抓证据
Get-PnpDevice -InstanceId 'ROOT\MEDIA\0001' | fl Status, Problem, ProblemStatus   # 0x34 / 0xC0000428
sc start Scream               # 577 ERROR_INVALID_IMAGE_HASH
.\drivers\install.ps1 -Uninstall   # 回滚（含残留服务清理与断言）
```
