# 阶段 2 — 从源码构建 Scream.sys（不依赖 msbuild）

- 日期：2026-10-17
- 执行：`vad-researcher`（task-11 阶段 2）
- 结论状态：**完成**——已产出格式正确的驱动映像；**尚未验证能否加载/工作**（阶 4）
- 上游文档：[wdk-setup.md](wdk-setup.md)（阶 1，解释了为什么不能用 msbuild）

---

## 0. TL;DR

| 项 | 值 |
|---|---|
| 产物 | `drivers/out/x64/Release/Scream.sys` |
| 大小 | **30,208 字节** |
| SHA256 | `6CE6DFAD903854B71A18216F6C3710B06995A3CB88E34A1293BA65AEC8F96596`（本次构建；见 §7 关于可复现性） |
| 形态 | PE32+ / `8664 machine (x64)` / subsystem **Native** / 入口 `GsDriverEntry` / 无导出 |
| 依赖 | `ntoskrnl.exe`、`portcls.sys`、`NETIO.SYS` |
| 签名 | **NotSigned**（阶 3 处理） |
| 重建 | `powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\drivers\build.ps1` |
| 警告 | 12 × `C4996`（`ExAllocatePoolWithTag` 弃用），0 error |

> **「构建成功」≠「驱动可用」**：本阶段只证明源码能在本机工具链下编译、链接成结构合法的 WDM 驱动；加载、端点出现、改名、播放全部留到阶 3/4 实测。

---

## 1. 源码来源与许可

| 项 | 值 |
|---|---|
| 上游 | https://github.com/duncanthrax/scream |
| commit | `d789743c248b11d1df7e5ecc546b1bc60b90cd91`（2026-08-10，master） |
| 许可证 | **MS-PL**（Microsoft Public License） |
| 本地副本 | `drivers/scream-derived/`，共 **26 个文件**：9×`.cpp`（adapter/common/hw/ivshmemsavedata/kshelper/minstream/mintopo/minwave/savedata）、12×`.h`、`scream.rc`、`Scream.inf`、`Scream.vcxproj`、`Scream.vcxproj.filters`、`LICENSE-MS-PL.txt` |
| 来源记录 | `drivers/UPSTREAM.md`（URL + commit + 许可 + 本地改动策略） |

补充：

- 上游 vcxproj 里列了 `netiodef.h`，但**仓库中并不存在**该文件；我们未使用它，编译也不依赖它（如实记录，避免后来者以为漏拷）。
- MS-PL 允许派生与再分发（含闭源分发），但要求保留版权/许可声明。本项目若对外分发驱动，需在「关于」或随包 LICENSE 中保留 MS-PL 声明——**这是 Lead/用户的合规决策点**，本阶段只记录事实。

---

## 2. 为什么不用 msbuild

见 [wdk-setup.md §4](wdk-setup.md)：本机 VS 2022 BuildTools 没有注册 `WindowsKernelModeDriver10.0` 工具集，`msbuild Scream.vcxproj` 报

```
error MSB8020: 无法找到 WindowsKernelModeDriver10.0 的生成工具(平台工具集 ="WindowsKernelModeDriver10.0")
```

因此阶段 2 的构建完全由 `drivers/build.ps1` 直接调用 `cl/rc/link` 完成。

---

## 3. 工具链解析（`build.ps1:39-75`）

1. kit 根 `C:\Program Files (x86)\Windows Kits\10`；版本条自动探测：取 `Include\*` 中 `km\ntddk.h` 存在、版本号最高者 → **10.0.26100.0**（`build.ps1:43-49`）。
2. VS 实例：`vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64` → `C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools`（`build.ps1:59-64`）。
3. 导入 MSVC 环境：`cmd /c "vcvars64.bat && set"` 逐行塞回 `$env:`（`build.ps1:68-70`）→ `cl.exe` 在 PATH 上，`VCToolsVersion = 14.44.35207`。
4. 工具位置：`cl/rc/link` 来自 MSVC；`signtool/stampinf/makecert` 在 `Windows Kits\10\bin\10.0.26100.0\x64\`；**`Inf2Cat` 只在 `...\bin\10.0.26100.0\x86\`**；`devcon` 在 `...\Tools\10.0.26100.0\x64\`。

---

## 4. 编译参数与出处

| 参数 | 出处 |
|---|---|
| `/W3` | 上游 vcxproj `WarningLevel` = Level3 |
| `/GS` | `WindowsDriver.Common.props`：`BufferSecurityCheck=true` |
| `/Gz` | 同上：`CallingConvention=StdCall` |
| `/EHs-c-` | 同上：`ExceptionHandling=false` |
| `/GR-` | 同上：`RuntimeTypeInfo=false` |
| `/Gy` `/GF` | 同上：`FunctionLevelLinking=true`、`StringPooling=true` |
| `/Zp8` | 同上：`StructMemberAlignment=8Bytes` |
| `/Zc:wchar_t-` | 同上：`TreatWChar_tAsBuiltInType=false` |
| `/kernel` | `WindowsDriver.KernelMode.props` 为驱动追加；作用 = 让 `cl` 定义 `_KERNEL_MODE`（见 §5.1） |
| `/d1import_no_registry` `/d2AllowCompatibleILVersions` | `Common.props` 的 `AdditionalOptions` |
| `-D_WIN64 -D_AMD64_ -DAMD64` | `WindowsDriver.x64.props` |
| `-DPOOL_NX_OPTIN=1` | 上游 vcxproj `PreprocessorDefinitions` |
| `-DDBG=0` | `UseDebugLibraries=false`（fre/release 分支） |
| `-D_WIN32_WINNT=0x0603 -DWINVER=0x0603 -DNTDDI_VERSION=0x06030000` | 上游 vcxproj `TargetVersion=WindowsV6.3` |
| `-DIMP_IDmaChannel=` | 本机适配（见 §5.2） |
| **不定义** `UNICODE`/`_UNICODE` | 见 §5.3 |
| Release 追加 `/O2 /Os /d1nodatetime` | `Optimization=Full` + `FavorSizeOrSpeed=Size` + `AdditionalOptions` |
| 包含路径 | `Include\<ver>`、`Include\<ver>\km`、`km\crt`、`shared`、`um` |

---

## 5. 三个坑（现象 → 根因 → 修法 → 证据）

### 5.1 `_KERNEL_MODE` 不会自动定义 → 必须加 `/kernel`

- **现象**：`minstream.cpp` 报无法解析 `KeQueryInterruptTime` 等内核 API（`wdm.h` 中相关声明被 `_KERNEL_MODE` 条件包住）。
- **根因**：`/kernel` 开关除了设定编译期语义，还负责定义 `_KERNEL_MODE`；直接调 `cl` 不传它就缺这个宏。
- **验证方法（实测）**：写一个只有
  ```c
  #ifndef _KERNEL_MODE
  #error NOT_DEFINED
  #endif
  ```
  的临时 TU：不加 `/kernel` 触发 `#error`，加 `/kernel` 通过 → 结论确凿。
- **修法**：`build.ps1:105` 加 `/kernel`。

### 5.2 `IMP_IDmaChannel`：上游遗留 + 新版 `portcls.h` 的交互

- **现象 A**（什么都不定义）：`minstream.h(84): error C4430: 缺少类型说明符 - 假定为 int`——`IMP_IDmaChannel;` 被当成未知类型成员声明。
- **现象 B**（按直觉加 `PC_IMPLEMENTATION`）：`error C2535: 已经定义或声明成员函数`——上游 `minstream.h:64-81` 已经**逐条显式声明**了 13 个 `IDmaChannel` 成员，而本机 `portcls.h:869` 在 `PC_IMPLEMENTATION` 下又会用宏把同样 13 个成员展开一遍 → 重复声明。
- **证据**：本机 `C:\Program Files (x86)\Windows Kits\10\Include\10.0.26100.0\km\portcls.h` 第 869 行附近，`IMP_IDmaChannel` 宏只在 `#if ... PC_IMPLEMENTATION` 分支定义（实测）。
- **修法**：`-DIMP_IDmaChannel=`（宏定义为空，把 `minstream.h:84` 的残留符号消掉）→ C4430/C2535 均消失（`build.ps1:92-97`）。
- **推测（未核实）**：上游当年所用 WDK 的 `portcls.h` 对该宏的条件/内容不同，所以直接照抄源码跨 WDK 版本会踩到。**这是「照抄上游能编」的常见陷阱，后续若改 Scream 代码需先跑通本脚本再动。**

### 5.3 不能定义 `UNICODE`

- **现象**：一旦 `/DUNICODE`，`savedata.cpp(314)` 报 `C2664`：`RtlIpv4StringToAddressW` 参数不匹配（上游传的是 `PCHAR`）。
- **根因**：上游是 **ANSI** 驱动（`char` 字符串），WDK 对内核模式默认不定义 `UNICODE`；我们把 `UNICODE` 传进去反而把它切成宽字符版本。
- **证据**：产物导入符号里有 **`RtlIpv4StringToAddressA`**（A=ANSI，见 §7），从二进制侧反证了这条修法生效。
- **修法**：不定义（`build.ps1:98-100` 注释写明原因）。

---

## 6. 链接参数与出处

| 参数 | 出处/理由 |
|---|---|
| `/DRIVER /SUBSYSTEM:NATIVE /MACHINE:X64` | `WindowsDriver.KernelMode.Wdm.props`（`DriverType=WDM`） |
| `/ENTRY:GsDriverEntry` | 同上 x64 分支 `EntryPointSymbol`；`Gs` 前缀来自 `BufferOverflowK.lib`，**故该库必须排在库列表首位** |
| `/INCREMENTAL:NO /OPT:REF /OPT:ICF /RELEASE /MERGE:_TEXT=.text /MERGE:_PAGE=PAGE /SECTION:INIT,d /VERSION:10.0 /osversion:10.0` | `Wdm.props` Link 默认 + 上游 vcxproj |
| 库：`BufferOverflowK.lib;ntoskrnl.lib;hal.lib;wmilib.lib;portcls.lib;netio.lib;stdunk.lib` | 上游 `Scream.vcxproj` 的 `Win10|x64` 链接依赖 |
| （剔除 `/CHECKSUM`） | MSVC 14.44 的 `link.exe` 不认该选项，报无效选项（实测）；PE 校验和与驱动能否加载无关，签名/Inf2Cat 阶段会另行处理 |

---

## 7. 产物验证（`dumpbin` / `Get-AuthenticodeSignature` 实测）

```
8664 machine (x64)
A5CC entry point (000000014000A5CC)      ← GsDriverEntry
10.00 image version
6.00 subsystem version
D000 size of image
1 subsystem (Native)
节：.text .rdata .data .pdata PAGE INIT .rsrc .reloc
```

导入表（按 DLL 归组，符号为实测列表节选）：

| DLL | 符号（节选） |
|---|---|
| `ntoskrnl.exe` | `ExAllocatePoolWithTag`, `ExFreePoolWithTag`, `IoAllocateIrp`, `IoAllocateMdl`, `IoBuildDeviceIoControlRequest`, `IoGetDeviceInterfaces`, `IoGetDeviceObjectPointer`, `IoQueueWorkItem`, `KeBugCheckEx`, `KeInitializeEvent`, `KeWaitForSingleObject`, `MmBuildMdlForNonPagedPool`, `MmGetSystemRoutineAddress`, `RtlIpv4StringToAddressA`, `RtlQueryRegistryValues`, `RtlInitUnicodeString` … |
| `portcls.sys` | `PcAddAdapterDevice`, `PcInitializeAdapterDriver`, `PcNewMiniport`, `PcNewPort`, `PcNewServiceGroup`, `PcRegisterAdapterPowerManagement`, `PcRegisterPhysicalConnection`, `PcRegisterSubdevice` |
| `NETIO.SYS` | `WskRegister`, `WskCaptureProviderNPI`, `WskReleaseProviderNPI`, `WskDeregister` |

- 无导出（WDM/PortCls 驱动经 `IoRegisterDriverReinitialization` 之类由系统加载，不从用户态直接调用），符合预期。
- **签名状态**：`Get-AuthenticodeSignature` → `Status: NotSigned`（阶 3 处理）。
- 与上游发布版对比：上游 x64 发布 `Scream.sys` 为 **37,592 字节**，本机重建为 **30,208 字节**。差异来自 MSVC/WDK 版本与编译选项（如优化/对齐），**说明我们的产物是重新构建的，而非下载来的二进制**——这正是阶 3 要签名、阶 4 要验证的对象。
- **可复现性**：连续两次构建大小相同（30,208 B）但 SHA256 不同；`dumpbin` 显示 PE `time date stamp` 随构建时间变化（实测），所以哈希不可当稳定标识。签名针对**最终文件**，不受影响；文档记录哈希仅为「当前产物指纹」。

---

## 8. 警告清单（12 × C4996）

全部为 `ExAllocatePoolWithTag` 在 WDK 10.0.26100 中被标记弃用：

| 文件:行 |
|---|
| `adapter.cpp:123 / 191 / 213 / 225 / 247` |
| `minstream.cpp:183 / 191 / 546` |
| `savedata.cpp:58 / 171` |
| `ivshmemsavedata.cpp:95 / 283` |

- 无 error，共 20 行警告文本（含 `C4996` 多行说明）。
- 上游 vcxproj 本身 `TreatWarningAsError=false` 且屏蔽了 `4228;4603;4627;4986;4987;4996`——即上游在旧 WDK 上也知道这些告警，**不新增风险**。若后续要清理，可改用 `ExAllocatePoolZero`，但需评估初始化语义，阶 4 不做。

---

## 9. 复现步骤（可直接粘贴）

```powershell
cd D:\Backup\Documents\deepseek-harness\default-workspace\miplay-bridge
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\drivers\build.ps1
# 期望输出末尾：
# [5/5] linking
# OK  ->  ...\drivers\out\x64\Release\Scream.sys
#         30208 bytes, <时间>
```

注意：

- **本机没有 `pwsh`**（PowerShell 7），只能用 `powershell.exe`；`pwsh -File build.ps1` 会 CommandNotFound。
- 默认执行策略限制脚本运行 → 需 `-ExecutionPolicy Bypass`。
- 清理：`.\drivers\build.ps1 -Clean`；保留中间产物：加 `-KeepObjects`（obj 在 `out\x64\Release\obj`）。

---

## 10. 未做 / 下一步

| 项 | 归属阶段 |
|---|---|
| 自签证书 + `signtool` 签 `.sys`；`.inf`/`.cat`（Inf2Cat） | 阶 3 → [driver-install.md](driver-install.md) |
| 验证「自签 + 信任链」能否**免 test signing** 装入（用户硬约束：未授权开 test signing） | 阶 3（**关键结论点**） |
| 实际安装、端点是否出现在「声音输出」、改名 | 阶 4 → [driver-smoke.md](driver-smoke.md) |
| 只构建了 x64 Release；Debug / 32 位未验证 | 暂不需要 |
| 未改动 `Scream.inf` 显示名（仍是 `Scream (WDM)` / `Scream Wave`） | 阶 4 改名时处理 |

---

## 11. 风险

1. **上游代码年代久远**：MSVAD/PortCls 派生，`DriverVer` 为 2007 年（见 `Scream.inf`），面向 Win7/Win10 时代；在 Win11 26100 上加载行为未知。
2. **构建成功不保证加载/运行**：阶 4 若实测失败，需准备回滚路径（还原点 + `pnputil /delete-driver` + 删服务），由阶 3/4 脚本落实。
3. **潜在 BSOD 风险**：内核驱动一旦出错是蓝屏级别；实验前应建系统还原点（阶 3 脚本会提示）。
4. **`IMP_IDmaChannel=` 属本地适配**，若将来换 WDK 版本可能又变；`build.ps1` 已把理由写在注释里，便于复查。

## 参考

- 上游：https://github.com/duncanthrax/scream （commit `d789743c248b11d1df7e5ecc546b1bc60b90cd91`，MS-PL）
- 本机 WDK：`C:\Program Files (x86)\Windows Kits\10\Include\10.0.26100.0\km\`（`portcls.h:869`、`minstream.h:64-84` 见 `drivers/scream-derived/`）
- 构建脚本：`drivers/build.ps1`（169 行，含逐行出处注释）
- 上游来源记录：`drivers/UPSTREAM.md`
