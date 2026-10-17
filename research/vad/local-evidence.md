# VAD 选型：本机实测证据与复现步骤

> 配套 `vad-options.md`。所有标注【已核实】的结论都能用下面的命令复现。
> 环境：Windows 10.0.26100 / AMD64 / Administrator / Secure Boot=False / test signing 关闭 / cargo 1.99.0

---

## 1. 构建/签名环境

```powershell
# WDK：km 头文件不存在 => WDK 未安装（Include/bin 里只有 Windows SDK 目录）
Test-Path 'C:\Program Files (x86)\Windows Kits\10\Include\*\km'      # False
Get-ChildItem 'C:\Program Files (x86)\Windows Kits\10\Include' -Directory | % Name
#   -> 10.0.19041.0, 10.0.26100.0
Get-ChildItem 'C:\Program Files (x86)\Windows Kits\10\bin' -Directory | % Name
#   -> 10.0.14393.0 … 10.0.26100.0, arm64, x64, x86

# VS 2022 BuildTools + MSVC 14.44.35207 已装
Test-Path 'C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools'   # True
Get-ChildItem 'C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Tools\MSVC' -Directory | % Name
#   -> 14.44.35207

# test signing / Secure Boot
bcdedit /enum '{current}'          # 无 testsigning 行；hypervisorlaunchtype Auto
Confirm-SecureBootUEFI             # False
cargo --version; rustc --version   # 1.99.0
```

**结论**：编译驱动需先装 WDK；本机 **Secure Boot 已关、test signing 关闭**，v1 的"自签免 test signing"路线有理论可行性（必须实测）。

---

## 2. 端点 = 驱动 devnode（回答"用户态能否注册端点"）

```powershell
Get-PnpDevice -Class AudioEndpoint | Sort FriendlyName | % { "{0,-10} {1}  [{2}]" -f $_.Status,$_.FriendlyName,$_.InstanceId }
```

实测输出（摘录）：

```
OK         CG345UK (NVIDIA High Definition Audio)  [SWD\MMDEVAPI\{0.0.0.00000000}.{06283053-...}]
OK         X41 (NVIDIA High Definition Audio)      [SWD\MMDEVAPI\{0.0.0.00000000}.{0E41DD27-...}]
OK         麦克风阵列 (UU远程虚拟音频设备)          [SWD\MMDEVAPI\{0.0.1.00000000}.{F56543A0-...}]
OK         扬声器 (Realtek High Definition Audio)   [SWD\MMDEVAPI\{0.0.0.00000000}.{F4DD190E-...}]
OK         扬声器 (Steam Streaming Speakers)        [SWD\MMDEVAPI\{0.0.0.00000000}.{4A82BBBC-...}]
OK         扬声器 (UU远程虚拟音频设备)              [SWD\MMDEVAPI\{0.0.0.00000000}.{901DB3C5-...}]
Unknown    扬声器 (Xiaomi Speaker-4162)             [SWD\MMDEVAPI\{0.0.0.00000000}.{9809A2E0-...}]
Unknown    扬声器 (立体声组合)                      [SWD\MMDEVAPI\{0.0.0.00000000}.{14BD01D3-...}]
```

- 所有端点的实例 ID 都是 `SWD\MMDEVAPI\{...}`，其 `{b3f8fa53-…},2` 指向真实 devnode（HDAUDIO / USB / BTHENUM / ROOT\…）→ **端点必须有内核驱动**。
- 本机**已装两个商业虚拟声卡**：网易 UU 远程、Valve Steam Streaming Speakers（都是 root 枚举 + WHQL 签名）→ 任务书"本机没有任何第三方虚拟音频设备"应修正为"没有任何用户自行安装的虚拟音频线"。

---

## 3. 端点显示名的真实来源【关键】

```powershell
$base='HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Render'
Get-ChildItem $base | % {
  $p = Get-ItemProperty "$($_.PSPath)\Properties"
  "{0} | desc={1} | iface={2} | fn14={3}" -f $_.PSChildName,
     $p.'{a45c254e-df1c-4efd-8020-67d146a850e0},2',
     $p.'{b3f8fa53-0004-438e-9003-51a46e139bfc},6',
     $p.'{a45c254e-df1c-4efd-8020-67d146a850e0},14'
}
```

实测（26 个 render 端点）：

| 端点 | `{a45c254e-…},2`（DeviceDesc） | `{b3f8fa53-…},6`（Interface FriendlyName） | 系统显示名 | `{a45c254e-…},14` |
|---|---|---|---|---|
| Realtek 扬声器 | `扬声器` | `Realtek High Definition Audio` | 扬声器 (Realtek High Definition Audio) | **不存在** |
| UU 远程 | `扬声器` | `UU远程虚拟音频设备` | 扬声器 (UU远程虚拟音频设备) | **不存在** |
| Steam | `扬声器` | `Steam Streaming Speakers` | 扬声器 (Steam Streaming Speakers) | **不存在** |
| NVIDIA HDMI | `CG345UK` / `X41` | `NVIDIA High Definition Audio` | CG345UK (NVIDIA High Definition Audio) | **不存在** |
| Xiaomi/USB | `扬声器` | `Xiaomi Speaker-4162` | 扬声器 (Xiaomi Speaker-4162) | **不存在** |

补充键：`{a45c254e-…},24` = 枚举器（`ROOT`/`USB`/`BTHENUM`/`HDAUDIO`）；
`{b3f8fa53-…},2` = 设备实例 ID；`{233164c8-…},1` = KS 设备接口路径（如 `\\?\root#media#0000#{6994ad04-…}\newavespeaker`）。

**结论**：显示名 = `DeviceDesc + " (" + InterfaceFriendlyName + ")"`，运行时组装，**默认不落盘**；`...,14` 在 26100 上不是端点名的存储位置（任务书给的键值需修正：`{a45c254e-…},15` 是 `PKEY_Device_LocationInfo`，不是 Interface FriendlyName）。要改名 ⇒ **改驱动的 INF 本地化 DeviceDesc**（UU/Steam/Scream 都是这么做的）。

---

## 4. 两个商业 VAD 的签名与 INF 骨架（路线 D 的实证）

```powershell
# 设备 -> 驱动包
Get-ItemProperty 'HKLM:\SYSTEM\CurrentControlSet\Enum\ROOT\MEDIA\0000' | select DeviceDesc,Service,ClassGUID,HardwareID,Driver
#   DeviceDesc = @oem56.inf,%nevaudio_sa.devicedesc%;UU远程虚拟音频设备
#   Service = nevaudio ; HardwareID = NEVirtualDevice\nevaudio
Get-ItemProperty 'HKLM:\SYSTEM\CurrentControlSet\Enum\ROOT\STEAMSTREAMINGSPEAKERS\0000' | select DeviceDesc,Service,HardwareID
#   DeviceDesc = @oem41.inf,%steamstreamingspeakers.devicedesc%;Steam Streaming Speakers
#   HardwareID = ROOT\SteamStreamingSpeakers

# 签名
Get-AuthenticodeSignature 'C:\Windows\INF\oem56.inf'
#   Valid  Signer=CN=Microsoft Windows Hardware Compatibility Publisher  (网易 UU 远程)
Get-AuthenticodeSignature 'C:\Windows\System32\DriverStore\FileRepository\nevaudio.inf_amd64_b970e190e9bb42e0\nevaudio.sys'
#   Valid  157936B  NotBefore=2026/5/14 NotAfter=2027/5/12  TS=True
Get-AuthenticodeSignature 'C:\Windows\system32\drivers\SteamStreamingSpeakers.sys'
#   Valid  40736B   证书 2017/2/16–2018/5/10 已过期，但因 TS=True 仍 Valid
```

`C:\Windows\INF\oem56.inf` 关键行（网易 UU 远程，MSVAD/Sysvad 骨架）：

```
L29  %NEVAUDIO_SA.DeviceDesc%=NEVAUDIO_SA, NEVirtualDevice\nevaudio
L35  nevaudio.sys
L76  HKR,,FriendlyName,,%NEVAUDIO.WaveSpeaker.szPname%
L88  Include=ks.inf,wdmaudio.inf
L89  Needs=KS.Registration, WDMAUDIO.Registration
L114 ServiceBinary=%13%\nevaudio.sys
L146 NEVAUDIO_SA.DeviceDesc="UUYC Virtual Audio Device"      ; 英文段
L172 NEVAUDIO_SA.DeviceDesc="UU远程虚拟音频设备"            ; 中文段 -> 端点括号里的名字
```

**结论**：路线 D 就是"几十 KB 的 PortCls miniport + root 枚举 + INF 本地化名字 + 微软 attestation/WHQL 签名"。签名可跨证书过期存活（时间戳）。

---

## 5. Scream：现成二进制在本机装不上【关键】

```powershell
Get-AuthenticodeSignature 'C:\Users\Administrator\AppData\Local\Temp\vad-research\scream\Install\driver\x64\Scream.sys'
```

```
Status      = UnknownError
StatusMessage = A required certificate is not within its validity period when verifying against
                the current system clock or the timestamp in the signed file
Signer      = CN=Tom Kistner, O=Tom Kistner, ..., C=DE
NotBefore   = 2020/7/6   NotAfter = 2023/7/7   HasTimeStamper = False
Size        = 37592 B    FileVersion = 6.3.9600.17246
```

`Install/driver/{x64,x86,arm64}/` 各有 `Scream.sys` + `Scream.inf` + `scream.cat`；`Install/Install-x64.bat` 走 `devcon`。→ **要用现成 Scream 二进制必须开 test signing；否则必须重编 + 自签**。

Scream 关键源码事实（本地副本 `%TEMP%\vad-research\scream\Scream\`）：

- `savedata.cpp:9-10` `MULTICAST_TARGET "239.255.77.77"`, `MULTICAST_PORT 4010`
- `savedata.cpp:11-13` `PCM_PAYLOAD_SIZE 1152`, `HEADER_SIZE 5`, `CHUNK_SIZE 1157`
- `savedata.cpp:478-481` 头 4 字节写在 `offset+1..+4`（采样率 marker / 位深 / 声道数 / channel mask）
- `savedata.cpp:415` `WskSendTo(...)`（内核 WSK socket 直接发 UDP）
- `adapter.cpp:86-102` 从 `HKLM\SYSTEM\CurrentControlSet\Services\Scream\Options` 读 `UnicastIPv4/UnicastPort/UnicastSrcPort`
- `adapter.cpp:501,506` 每实例只注册一个 `"Topology"` + 一个 `"Wave"` 子设备 → **1 实例 = 1 render 端点**
- `Scream.inf` `[Strings] Scream.DeviceDesc="Scream (WDM)"`，`[MicrosoftDS.NTAMD64] %Scream.DeviceDesc%=Scream,*Scream`（`*Scream` = 软件设备 HWID）

---

## 6. Virtual-Cables / usbip-win2（路线 A）

```powershell
Get-ChildItem "$env:TEMP\vad-research\Virtual-Cables" -Recurse -File | % { "{0,10}  {1}" -f $_.Length,$_.FullName }
```

实测要点：
- 无 `.inf/.sys/.cat`；核心 Go 源码：`usbip/protocol.go` 5,205B、`usbip/server.go` 15,717B、`uac1/descriptors.go` 5,955B、`uac1/device.go` 6,975B、`audio/ring.go` 1,823B、`cmd/virtualcables/main.go` 10,006B（+GUI 28,754B）；`LICENSE.txt` = **BSD 2-Clause**；`LICENSES\usbip-win2_BSD-2-Clause.txt`。
- `Virtual_Cables_Setup.exe`（3,482,382B）→ `Get-AuthenticodeSignature` = **NotSigned**；ASCII 扫不到 `testsigning/pnputil/.sys/.inf`（载荷压缩，结论不可用）。
- README：L4 不装音频驱动、走 USB/IP + 内置 `usbaudio.sys`；L5/L98/L113 传输驱动 = `vadimgrn/usbip-win2`；L28 最多 32 条 cable；L29 每条 = 播放+录音端点；L31 UAC1；L60-66 固定 48k/立体声/16bit；L73 配置传输要管理员；L79 应用内"Download and Install Driver"。

`usbip-win2` README（master）原文：
```
L11 The driver is not signed, Windows Test Signing Mode must be enabled
L13 You can donate to purchase an EV certificate which is required for signing the driver
L22 UDE driver is an USB/IP client
L23 USB class upper filter driver usbip2_filter is used as companion for UDE driver
L141 set HWID=ROOT\USBIP_WIN2\UDE
L154-156 Disable Windows Test Signing Mode ... USBip will not work until the test signing will be reenabled
```

**结论**：路线 A 用户态部分完全可行（模拟 USB 设备），但 Windows 侧必须依赖**未签名**的 VHCI → **test signing 常开**（除非自己写并签名一个 VHCI 驱动）。

---

## 7. 小米音箱历史端点（附赠情报）

```
扬声器 (Xiaomi Speaker-4162)   InstanceId = {1}.USB\VID_2717&PID_5097&MI_00\6&191F8A2&0&0000   DeviceState=4
麦克风 (Xiaomi Speaker-4162)   InstanceId = {1}.USB\VID_2717&PID_5097&MI_00\...                 DeviceState=8
扬声器 (立体声组合)            InstanceId = {1}.BTHENUM\{0000110B-0000-1000-8000-00805F9B34FB}_VID&00021D6B_PID&0246\...
```
- VID `2717` = 小米；`MI_00` + `usbaudio` → 音箱曾以 **USB 音频**接入（**USB Audio Class**，Windows 直接出端点）。
- `BTHENUM\{0000110B…}` = 蓝牙 **A2DP AudioSink** → 音箱也曾以蓝牙输出端点出现。
- DeviceState：1=ACTIVE，4=NOTPRESENT，8=UNPLUGGED（当前都是残留，不可用）。
- 意义：设备侧的 USB/BT 两条"当输出设备"的路本来就通，但都不走妙播协议；妙播的价值在 Wi-Fi 组播/立体声组合 → 仍需本项目。

---

## 8. 复现用副本位置

```
%TEMP%\vad-research\
  Virtual-Cables\      (BSD-2-Clause, Go)
  usbip-win2\          (BSD-2-Clause, C/C++ + README)
  usbipd-win\          (服务端方向)
  scream\              (MS-PL, duncanthrax)
  scream-arcnmx\       (C/C++ fork)
```
（均为 `git clone --depth 1`；如需长期保留请复制进项目 `research/vad/vendor-notes/`，不要提交第三方全量源码。）
