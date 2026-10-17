# 阶段 1 — WDK 安装与驱动工具链就绪

- 日期：2026-10-17
- 执行：`vad-researcher`（task-11 阶段 1）
- 结论状态：**完成**（WDK 可用；Visual Studio 集成缺失，已用绕过方案，见 §5）
- 下游文档：[driver-build.md](driver-build.md)（阶段 2）、[driver-install.md](driver-install.md)（阶段 3）、[driver-smoke.md](driver-smoke.md)（阶段 4）

---

## 0. TL;DR

1. WDK 已装：包 `Microsoft.WindowsWDK.10.0.26100`，版本 **10.1.26100.6584**，kit 头/库版本条 `10.0.26100.0`（与本机 OS 26100 对齐）。
2. **但 WDK 的 Visual Studio 集成没有注册**：`msbuild` 构建任何驱动工程都会失败（`error MSB8020`，原文见 §4）。原因：本机是 **VS 2022 BuildTools**（非完整 VS），且 WDK 是在 VS 之后用 winget 装的，不装 VSIX 扩展。
3. 因此**不修 VS、不用 msbuild**：用 `drivers/build.ps1` 直接驱动 `cl.exe / rc.exe / link.exe`。阶段 2 已产出格式正确的 `Scream.sys`（见 driver-build.md）。
4. **未改动任何启动/安全配置**：`bcdedit` 里没有 `testsigning` 行（未启用）、`Secure Boot` 仍为 `False`、全程未重启。阶段 3 的签名/安装决策完全基于当前状态。

---

## 1. 安装前环境（实测）

| 项 | 实测结果 | 依据 |
|---|---|---|
| OS | Windows 10.0.26100（`description Windows 11`），AMD64 | `bcdedit /enum {current}` |
| Visual Studio | **Build Tools 2022**，17.14.37710.0；MSVC 工具集 14.44.35207（v143） | `C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools` |
| Windows SDK | 头 `10.0.26100.0`（另有 10.0.19041.0）；Lib 10.0.14393.0…10.0.26100.0 | `Windows Kits\10\Include`、`\Lib` |
| WDK | **无**：`Include\10.0.26100.0\km`、`\km\acx` 均不存在；无 `WindowsDriver.*.props`；`WindowsDriver.Default.props` 的 `WindowsDriverInstalled` = False | `Test-Path` / `Test-Path` |
| signtool | 有（SDK 自带）：`Windows Kits\10\bin\{10.0.19041.0,10.0.26100.0}\x64\signtool.exe` | 目录列举 |
| 磁盘 | C: 空闲 ≈ 35.9 GB | `Get-PSDrive C` |
| PowerShell | 只有 **Windows PowerShell 5.1**（`powershell.exe`）；**没有** `pwsh`（PowerShell 7） | `Get-Command pwsh` 无结果 |
| winget | 1.29.380 | `winget --version` |

---

## 2. 安装动作

```powershell
# 可复现命令（无交互、静默）
winget install --id Microsoft.WindowsWDK.10.0.26100 -e `
  --accept-package-agreements --accept-source-agreements --silent
```

- 结果：exit code 0（后台 job `pwsh-4148e96b7fba4879`），耗时数分钟（未精确计时）。
- 装后版本：`winget list` → `Microsoft.WindowsWDK.10.0.26100` **10.1.26100.6584**。
- 未安装任何 VSIX / VS 扩展（这是 §4 缺口的根因）。
- 未安装 Visual Studio（本来就没打算装；BuildTools 已够用）。

---

## 3. 装了什么（实测清单，均为 `C:\Program Files (x86)\Windows Kits\10\` 下）

| 路径 | 说明 |
|---|---|
| `Include\10.0.26100.0\km\ntddk.h`、`wdm.h`、`portcls.h`、`ks.h` … | 内核模式头（PortCls/WDM 驱动编译所需，本次全部到位） |
| `Include\10.0.26100.0\km\acx\acx.h`、`acxcircuit.h`、`acxdevice.h`、`acxdriver.h` … | **ACX 头也在**（若将来走 ACX 路线可用） |
| `Lib\10.0.26100.0\km\x64\portcls.lib`、`ks.lib`、`BufferOverflowK.lib`、`ntoskrnl.lib`、`hal.lib`、`wmilib.lib`、`netio.lib`、`stdunk.lib` …，及子目录 `acx\` | km 链接库 |
| `build\10.0.26100.0\WindowsDriver.Common.props`、`WindowsDriver.x64.props`、`WindowsDriver.KernelMode.Wdm.props`、`WindowsDriver.Default.props` … | msbuild 用的驱动构建属性（**本机 msbuild 用不上，见 §4；但参数可抄**） |
| `bin\10.0.26100.0\x64\signtool.exe`、`stampinf.exe`、`makecert.exe` | 签名/INF 工具 |
| `bin\10.0.26100.0\x86\Inf2Cat.exe` | **只在 x86 目录**（生成 .cat 必须用它） |
| `Tools\10.0.26100.0\x64\devcon.exe` | 设备安装 CLI |
| （无 `Windows Kits\10\Samples`） | 示例驱动代码只在 GitHub，本机没有；需要就 clone |

- kit 目录合计 **4.26 GB / 22,890 文件**（含原有的 SDK 部分）；安装后 C: 空闲 36.7 GB，磁盘不是约束。
  （注：安装前后两次空闲空间采样之间还有其他临时文件增删，故不把差值当作 WDK 净占用。）

---

## 4. 关键缺口：Visual Studio 集成未注册（实测证据）

命令：

```powershell
$msb = 'C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\MSBuild\Current\Bin\MSBuild.exe'
& $msb Scream.vcxproj /p:Configuration=Win10 /p:Platform=x64 /p:WindowsTargetPlatformVersion=10.0.26100.0
```

输出（原样，`Microsoft.CppBuild.targets(463,5)`）：

```
error MSB8020: 无法找到 WindowsKernelModeDriver10.0 的生成工具(平台工具集 ="WindowsKernelModeDriver10.0")。
若要使用 WindowsKernelModeDriver10.0 生成工具进行生成，请安装 WindowsKernelModeDriver10.0 生成工具。
```

佐证：

- `...\BuildTools\MSBuild\Microsoft\VC\v170\Platforms\x64\PlatformToolsets\` 下**只有 `v143`**，没有 `WindowsKernelModeDriver10.0`。
- `...\MSBuild\Microsoft\VC\v170\` 递归搜 `*WindowsKernelModeDriver*` → **无结果**。
- 另有一次 `error MSB8013: 此项目不包含配置和平台组合 Win10 Release|x64` 是我最初把配置名写成 `Win10 Release` 造成的假问题——该工程真实配置名是 `Win10` / `Win7 Debug` / `Win8.1 Release` …（见 `Scream.vcxproj:4-112` 的 `ProjectConfiguration Include="Win10|x64"` 等），**不是**工具链问题。

判断（我的推测，未逐条验证）：WDK 的 VS 集成通过 VSIX 扩展安装；winget 的 WDK 包只安装 headers/libs/tools/build-props，不负责 VSIX；本机是 BuildTools-only 且 WDK 后装，所以扩展没注册。要修需要重装/修复 VS 或手工找 WDK 里的 vsix 安装——**本项目不走这条路**。

---

## 5. 决定：不修 VS，直接 cl/rc/link

`drivers/build.ps1` 自解析工具链并逐条调用编译器（详见 [driver-build.md](driver-build.md)）：

- 用 `vswhere.exe` 找带 `Microsoft.VisualStudio.Component.VC.Tools.x86.x64` 的 VS 实例 → 取 `VC\Auxiliary\Build\vcvars64.bat` → `cmd /c "vcvars64.bat && set"` 导入环境（`build.ps1:59-75`）。
- 自动探测 kit 版本：`Include\*\km\ntddk.h` 存在且版本号最高者 → `10.0.26100.0`（`build.ps1:43-49`）。
- 编译参数逐项抄自 WDK props 与上游 vcxproj，并在 `driver-build.md §4-§5` 记录出处。

理由：

1. 不改动 VS 安装（修复/重装风险不可控，且对最终交付没有价值）；
2. 我们只需要 `.sys`（阶 3 再加 INF/CAT），本质就是 `cl → .obj`、`rc → .res`、`link → .sys` 三步；
3. `Inf2Cat` / `signtool` / `devcon` 都是独立 CLI，**不依赖 msbuild**，阶 3、阶 4 同样不受影响。

代价：将来若要构建 ACX（`.vcxproj` + ACX props）或 Sysvad 工程，同样得绕过 msbuild 或先修 VS 集成；IDE 里按 F5 构建驱动不可用。

---

## 6. 安全/启动状态：未改动（阶 3 决策依据）

| 检查 | 结果 |
|---|---|
| `bcdedit /enum {current}` | 只有 `description Windows 11`、`hypervisorlaunchtype Auto`；**无 `testsigning` 行**（= 未启用 test signing） |
| `Confirm-SecureBootUEFI` | **False**（UEFI 存在但 Secure Boot 已关闭） |
| 重启次数 | 本次 WDK 安装**未要求**也未执行重启 |

> 这两条是阶 3「自签驱动能否免 test signing 装入」实验的起点：Secure Boot 已关，理论上「自签 + 证书入 `Trusted Root`/`Trusted Publisher`」有可能被加载器接受，**但能否在 Win11 26100 上成立必须实测**（阶 3 的验收标准）。

---

## 7. 卸载/回滚

```powershell
winget uninstall --id Microsoft.WindowsWDK.10.0.26100
```

- kit 目录可能残留（`Windows Kits\10` 下 km/acx 不会被完全清掉），需要手工删。
- 本阶段**没有**安装驱动、没有改注册表、没有改 `bcdedit`，因此除 WDK 之外无系统状态变更。

---

## 8. 遗留项

- [ ] 阶段 3 用本阶段装出的 `signtool` / `makecert` / `Inf2Cat` 完成自签与 .cat 生成（**Inf2Cat 在 x86 目录**）。
- [ ] 若用户后续要求「构建 ACX 驱动」，需先解决 VS 集成缺失（重装 VS+WDK 或用 WDK 自带 vsix）。
- [ ] `pwsh` 不存在：所有脚本说明里改用 `powershell.exe -ExecutionPolicy Bypass`。

## 参考

- [Download the WDK](https://learn.microsoft.com/windows-hardware/drivers/download-the-wdk)
- [ACX 音频类扩展](https://learn.microsoft.com/windows-hardware/drivers/audio/acx)
- 本机路径（实测）：`C:\Program Files (x86)\Windows Kits\10\Include\10.0.26100.0\km\`、`...\Lib\10.0.26100.0\km\x64\`、`...\build\10.0.26100.0\`、`...\bin\10.0.26100.0\`、`...\Tools\10.0.26100.0\x64\devcon.exe`
