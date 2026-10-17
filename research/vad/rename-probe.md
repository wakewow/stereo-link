# 端点改名实测报告（task-8 · 零驱动路线核心未知项）

> 本文件全部结论都是**本机实测**，可直接复现（§5 给了复制粘贴的命令）。
> 被测机器：Windows 10.0.26100 AMD64，用户 `pc-20251004quhb\administrator`（进程完整性级别 = **High / 已提权**）。
> 被测对象：`扬声器 (Steam Streaming Speakers)`——本机 7 个在用的 render 端点之一，实验期间**没有任何程序在用它**（Steam 客户端未运行，见 §7）。
> 实测时间窗：2026-10-17 22:23 – 22:35（UTC+8）。
> 原始取证文件：`device-hwkey-rename-states.json`（三态机读快照）、`device-hwkey-rename-test.log`（带毫秒时间戳的过程日志）、`device-rename-states.json` / `rename-state-before-device.json`（SetupAPI 路径）、`backup-*.reg`（回滚用备份）。

---

## 1. 结论（TL;DR）

**零驱动可以让「小米妙播设备」出现在 Windows 声音输出列表里，但名字形态是 `扬声器 (小米妙播设备)`——前缀「扬声器」去不掉。**

| 问题 | 答案 | 证据 |
|---|---|---|
| 能不能通过**端点属性**改名（`MMDevices\...\Properties` 写 `,14` / `,2` / `{b3f8fa53-…},6`）？ | **不能**（两条路都被拒：裸注册表 Access denied、官方 `IPropertyStore::SetValue` = `E_ACCESSDENIED`） | §3.1、§3.2 |
| 能不能通过 **SetupAPI 设备属性**（`SetupDiSetDevicePropertyW(DEVPKEY_Device_FriendlyName)`）改名？ | **不能**（该设备上 pid=14 属性**不存在** → `ERROR_NOT_FOUND`；API 也拒绝创建） | §3.3 |
| 能不能通过**设备硬件键** `FriendlyName` + 设备 disable/enable 改名？ | **能**，实测三态闭合，端点显示名确实变成 `扬声器 (小米妙播设备)`，回滚后完全复原 | §2、§4 |
| 名字能不能做成干净的「小米妙播设备」（无前缀）？ | **不能**。`扬声器` 来自驱动 pin 名 `{a45c254e-…},2`，零驱动改不掉 → 要干净名字仍必须写驱动 INF | §4.3 |
| 生效是否即时？ | 写值后**不生效**（音频栈缓存），必须对**设备**做一次 disable/enable 才刷新 | §2 三态 |

**对零驱动路线的意义**：只要机器上**已经存在**一个可借用的 root 枚举虚拟声卡端点（本机有 2 个：Steam、UU 远程），就能用「设备级改名 + 设备重启」把它变成声音列表里的 `扬声器 (小米妙播设备)`，再用 WASAPI loopback 采它（自环矩阵已证明 7/7 render 端点的 loopback 全可用，见 `loopback-matrix.md`）——**全程不写、不签、不装我们自己的驱动**。
代价与边界：名字不干净（带 `扬声器` 前缀）、依赖第三方驱动存在、需要管理员权限、改名动作会中断该端点约 1–2 秒。见 §6。

---

## 2. 三态实测（改名前 / 改名后 / 回滚后）

被测设备 = `ROOT\STEAMSTREAMINGSPEAKERS\0000`；被测端点 = `SWD\MMDEVAPI\{0.0.0.00000000}.{4a82bbbc-667f-4a9b-b4ce-520a165e6240}`。

四个观测点（`BEFORE` → `AFTER-WRITE` → `AFTER-WRITE-RESTART` → `AFTER-ROLLBACK`），每一格都是四路独立取证：注册表硬件键、`SetupDiGetDevicePropertyW`、`Get-PnpDevice`（设备层 + 端点层）、以及**新进程** `soundcard.all_speakers()`（绕过进程内缓存）。原始机读数据见 `device-hwkey-rename-states.json`。

| 观测项 | BEFORE | AFTER-WRITE（刚写完，未重启设备） | AFTER-WRITE-RESTART（设备 disable/enable 后） | AFTER-ROLLBACK（删值 + 重启设备） |
|---|---|---|---|---|
| 时间戳 | 22:33:56.481 | 22:33:59.724 | 22:34:06.817 | 22:34:16.045 |
| 硬件键 `…\Enum\ROOT\STEAMSTREAMINGSPEAKERS\0000\FriendlyName` | `<absent>` | `小米妙播设备` | `小米妙播设备` | `<absent>`（值已删除） |
| `SetupDiGetDevicePropertyW(pid=14, fmtid a45c254e-…)` | `<no FriendlyName>` | `小米妙播设备` | `小米妙播设备` | `<no FriendlyName>` |
| **设备** PnP FriendlyName / Status | `Steam Streaming Speakers` / OK | **`小米妙播设备`** / OK | `小米妙播设备` / OK | `Steam Streaming Speakers` / OK |
| **端点** PnP FriendlyName / Status | `扬声器 (Steam Streaming Speakers)` / OK | `扬声器 (Steam Streaming Speakers)` / OK（**未变**） | **`扬声器 (小米妙播设备)`** / OK | `扬声器 (Steam Streaming Speakers)` / OK |
| MMDevices `{a45c254e-…},2`（pin 名 = 前缀） | `扬声器` | `扬声器` | `扬声器` | `扬声器` |
| MMDevices `{a45c254e-…},14`（端点级 FriendlyName） | `<absent>` | `<absent>` | `<absent>` | `<absent>` |
| MMDevices `{b3f8fa53-…},6`（括号里那段） | `Steam Streaming Speakers` | `Steam Streaming Speakers` | **`小米妙播设备`** | `Steam Streaming Speakers` |
| 新进程 `soundcard.all_speakers()` 里的名字 | `扬声器 (Steam Streaming Speakers)` | `扬声器 (Steam Streaming Speakers)` | **`扬声器 (小米妙播设备)`** | `扬声器 (Steam Streaming Speakers)` |

三态一致性（脚本自己打印的对照行，原文见 `device-hwkey-rename-test.log`）：

```
=== THREE-STATE ===       before_EP="扬声器 (Steam Streaming Speakers)" | after_EP="扬声器 (Steam Streaming Speakers)" | afterRestart_EP="扬声器 (小米妙播设备)" | rollback_EP="扬声器 (Steam Streaming Speakers)"
=== THREE-STATE(device) === before="Steam Streaming Speakers" | after="小米妙播设备" | afterRestart="小米妙播设备" | rollback="Steam Streaming Speakers"
```

**回滚验证结论：完全复原。** 被改动的唯一一处是硬件键里**新增**的一个 `FriendlyName` 值（原始状态就是「该值不存在」），回滚 = `reg delete` 删掉它；删后硬件键值不存在、设备名、端点名、MMDevices 三个属性键、soundcard 枚举结果全部与 BEFORE 逐字节一致。设备与端点 Status 全程 `OK`，没有留下任何残留状态。

---

## 3. 被否定的三条路（含原始错误码，别混「被拒」和「成功但无效」）

### 3.1 路 A：裸注册表写端点属性 → **被拒（Access denied）**
目标：`HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Render\{4a82bbbc-667f-4a9b-b4ce-520a165e6240}\Properties`

| 命令 | 结果 |
|---|---|
| `Set-ItemProperty -Path '<MMDevices 键>' -Name 'DSHCanary' -Value 'x' -Type String` | `Set-ItemProperty : 不允许所请求的注册表访问权。`（exit 1） |
| `reg add '<MMDevices 键>' /v DSHCanary /t REG_SZ /d x /f` | `ERROR: Access is denied.`（exit 1） |
| `reg add '<MMDevices 键>' /v '{a45c254e-df1c-4efd-8020-67d146a850e0},14' /t REG_SZ /d '小米妙播设备' /f` | `ERROR: Access is denied.`（exit 1） |
| **对照组**：`reg add 'HKLM\SOFTWARE\DSH-ACL-Test' /v x /t REG_SZ /d y /f` | `The operation successfully completed.`（exit 0） |

对照组证明**不是沙箱/全局注册表禁用**，就是这棵音频键被拦。ACL 快照（`Get-Acl`）：owner = `NT AUTHORITY\SYSTEM`；`NT SERVICE\Audiosrv` Allow `SetValue, CreateSubKey, CreateLink, Delete, ReadKey`；`NT SERVICE\AudioEndpointBuilder` 同上；`BUILTIN\Administrators` Allow `SetValue, ReadKey`；`BUILTIN\Users` Allow `QueryValues, SetValue`。**ACL 里没有任何 Deny 项，却仍然拒绝写入**——具体拦截机制（audiosrv 持有键句柄 / 事务性注册表 / 键级保护）**未查明【未验证】**，但这不影响结论：这条路在提权管理员下也走不通。

### 3.2 路 B：官方 MMDevice API → **被拒（E_ACCESSDENIED）**
工具：`EndpointRename.cs`（`IMMDeviceEnumerator` / `IMMDevice` / `IPropertyStore` 的 C# COM 互操作，`MpvRename.EndpointTool`）。
调用链与逐条 HRESULT（原始 stdout 见 §5 命令 1）：

```
GetDevice("{0.0.0.00000000}.{4a82bbbc-…}")      hr=0x00000000  OK
OpenPropertyStore(STGM_READWRITE=2)            hr=0x00000000  OK
SetValue(PKEY_Device_FriendlyName, "小米妙播设备") hr=0x80070005  FAIL (E_ACCESSDENIED)
Commit()                                       hr=0x00000000  OK（提交成功但里面没有任何写入）
GetValue(PKEY_Device_FriendlyName) 读回          = "扬声器 (Steam Streaming Speakers)"   ← 未变
OpenPropertyStore(STGM_WRITE=1) 再试            SetValue 仍 0x80070005
```

**注意这是「被拒」，不是「成功但名字不变」**：`Commit` 返回 S_OK 只是因为属性存储里没有待提交项。没有任何写入发生，因此**没有产生需要回滚的改动**，「改名后」= 与改名前完全相同。

### 3.3 路 C：SetupAPI 设备属性 → **无效（属性不存在）**
工具：`DeviceRename.cs`（`MpvDev.DeviceTool::SetFriendly/Restart`，走 `SetupDiSetDevicePropertyW` + `DEVPKEY_Device_FriendlyName` = fmtid `{a45c254e-df1c-4efd-8020-67d146a850e0}`, pid **14**）+ `DevicePropDiag.cs`（诊断）。

| 调用 | 结果 |
|---|---|
| `SetupDiSetDevicePropertyW(pid=14)` | `False`，`lastErr=1004`（`ERROR_INVALID_FLAGS`） |
| `SetupDiGetDevicePropertyW(pid=2)` **正对照**（`DEVPKEY_Device_DeviceDesc`） | `True`，值 = `Steam Streaming Speakers` → 证明属性键编组/结构体无误，失败不是「我写错了结构」 |
| `SetupDiGetDevicePropertyW(pid=14)` 诊断 | `False`，`lastErr=1168`（`ERROR_NOT_FOUND`）→ **该设备上根本没有 pid=14 这个属性** |
| `SetupDiGetDeviceRegistryPropertyW(SPDRP_FRIENDLYNAME=0x0C)` | `False`，`lastErr=13`（`ERROR_INVALID_DATA`） |

反过来，一旦 §4 用硬件键把 `FriendlyName` 创建出来，`SetupDiGetDevicePropertyW(pid=14)` **立刻能读到**（三态表第 2 行 `小米妙播设备`）。所以 SetupAPI 不是「不能读」，而是「属性不存在时 `SetDeviceProperty` 拒绝创建」。
`Restart()`（`DIF_PROPERTYCHANGE` + `DICS_DISABLE` → `DICS_ENABLE`）这条边路在本机**可用**：返回 `state=2 → state=1, callInstaller=True, lastErr=0`，设备重枚举成功。§4 正是复用它。

---

## 4. 成功路径：设备硬件键 `FriendlyName` + 设备 disable/enable

### 4.1 为什么是这里
`SWD\MMDEVAPI\...` 端点背后是 root 枚举的软件设备 `ROOT\STEAMSTREAMINGSPEAKERS\0000`，其硬件键
`HKLM\SYSTEM\CurrentControlSet\Enum\ROOT\STEAMSTREAMINGSPEAKERS\0000` 的 ACL 是 **`BUILTIN\Administrators` = FullControl**（owner SYSTEM、Users 只读），**没有 audiosrv 那种拦截**——所以同一个提权管理员在这里能写成功，在 MMDevices 那棵树里被拒。

该键原有内容（节选）：`DeviceDesc = @oem41.inf,%steamstreamingspeakers.devicedesc%;Steam Streaming Speakers`、`Service = SteamStreamingSpeakers`、`ClassGUID = {4d36e96c-e325-11ce-bfc1-08002be10318}`。

### 4.2 机制（实测推出，不是推断）
端点显示名 = **`{a45c254e-…},2`（驱动 pin 名）+ `" (" + {b3f8fa53-…},6`（设备接口 FriendlyName）+ `")"`**。
`{b3f8fa53-…},6` 的值由音频栈在**设备重枚举**时从设备层 FriendlyName 取。所以：

1. 写硬件键 `FriendlyName` → **设备层**名字立刻变（`Get-PnpDevice` 马上显示 `小米妙播设备`），但端点层不变（AEB 缓存着旧接口名）；
2. 设备 disable/enable → 设备重枚举 → AEB 重新读接口名 → 端点显示名刷新为 `扬声器 (小米妙播设备)`，同时 `{b3f8fa53-…},6` 落盘为 `小米妙播设备`；
3. `{a45c254e-…},2` 始终是 `扬声器`（**由驱动 pin 提供，零驱动改不到**），`{a45c254e-…},14` 始终不存在（端点级属性从未落盘）。

### 4.3 对多设备命名的直接推论
- 借现成虚拟声卡 → 最终显示名只能是 **`扬声器 (<我们给设备起的名字>)`**，例如 `扬声器 (小米妙播设备)`、`扬声器 (妙播 - 客厅)`。
- 想拿到**完全干净**的名字（`小米妙播 - 客厅`，无 `扬声器` 前缀），必须让 pin 名也归我们控制 → **必须写驱动 INF**（这是 `vad-options.md` §8.3 路径 1 的结论，本次实测从反面确认了它：不写驱动就拿不到干净名字）。
- 多设备（每个妙播音箱一个 Windows 输出设备）同理：借来的端点数量 = 机器上已有的虚拟声卡数量（本机 2 个 render 端点：Steam、UU），**无法按需增加**。要 N 个可控端点 → 仍然是驱动（多实例）。

### 4.4 副作用与风险（实测）
- 设备 disable/enable 期间该端点消失约 1–2 秒（本机实测重启 + 等待 6 秒后稳定 OK）；若该端点正被播放，会打断一次。
- 需要**管理员**权限（写 HKLM + 重启设备）。
- 本次改的是**无人使用**的 Steam 端点，且**实验后已完全回滚**。
- `FriendlyName` 值在驱动重装 / INF 重装 / Windows 大版本升级后大概率被重建清掉——**【推测，未验证】**。
- 是否只对 **root 枚举的软件设备**有效（对 HDAUDIO/USB/BTHENUM 真实硬件是否同样有效）——**未验证**，本次只验证了 root 软件设备；不过"虚拟声卡"恰好都属于这一类，所以对本项目是够的。
- **只 `Restart-Service Audiosrv`（不重启设备）能否刷新端点名**——**未验证**（lead 未批准重启音频服务；但既然设备级 disable/enable 已验证有效，这条只是"更省事的替代"，不影响结论）。

---

## 5. 可复现命令（复制粘贴即可）

前置：管理员 PowerShell。所有产物写到 `research\vad\`。

```powershell
# 0) 公共变量
$BASE='D:\Backup\Documents\deepseek-harness\default-workspace\miplay-bridge\research\vad'
$PY='D:\Backup\Documents\deepseek-harness\default-workspace\mixplay-pc-audio\capture\.venv\Scripts\python.exe'
$DEV='ROOT\STEAMSTREAMINGSPEAKERS\0000'
$DEVREG='HKLM\SYSTEM\CurrentControlSet\Enum\ROOT\STEAMSTREAMINGSPEAKERS\0000'

# 1) 官方 API 改名会被拒（观察 HRESULT）
Add-Type -Path "$BASE\EndpointRename.cs"
[MpvRename.EndpointTool]::TrySetFriendlyName('{0.0.0.00000000}.{4a82bbbc-667f-4a9b-b4ce-520a165e6240}','小米妙播设备')
[MpvRename.EndpointTool]::GetFriendlyName('{0.0.0.00000000}.{4a82bbbc-667f-4a9b-b4ce-520a165e6240}')

# 2) 备份（硬要求）
reg export 'HKLM\SYSTEM\CurrentControlSet\Enum\ROOT\STEAMSTREAMINGSPEAKERS' "$BASE\backup-Enum-ROOT-STEAMSTREAMINGSPEAKERS.reg" /y

# 3) 一条龙：改名前快照 → 写值 → 重启设备 → 三态日志 → 回滚 → 再快照
powershell -NoProfile -ExecutionPolicy Bypass -File "$BASE\device-hwkey-rename-test.ps1"
#    （只读快照不写任何东西：加 -NoWrite；换名字：加 -NewName '妙播 - 客厅'）
#    产物：device-hwkey-rename-states.json / device-hwkey-rename-test.log
```

关键三行（脚本内部实际执行的动作）：

```powershell
reg add    'HKLM\SYSTEM\CurrentControlSet\Enum\ROOT\STEAMSTREAMINGSPEAKERS\0000' /v FriendlyName /t REG_SZ /d '小米妙播设备' /f   # exit 0
# 设备重枚举（触发端点名刷新）
[MpvDev.DeviceTool]::Restart('ROOT\STEAMSTREAMINGSPEAKERS\0000')   # DICS_DISABLE -> DICS_ENABLE , state=2 -> state=1, lastErr=0
# 回滚（原始状态 = 该值不存在，所以是 delete 而不是改回某个值）
reg delete 'HKLM\SYSTEM\CurrentControlSet\Enum\ROOT\STEAMSTREAMINGSPEAKERS\0000' /v FriendlyName /f   # exit 0
```

端点名交叉核对（新进程，绕过缓存）：

```powershell
& $PY -u "$BASE\probe_endpoint_names.py"        # 输出 JSON：speakers[] / microphones[]
```

**踩坑（复现时注意）**：
1. PowerShell 5.1 把 `-File` 脚本按 GBK 解码 → 脚本必须存成 **UTF-8 with BOM**，或像 `device-hwkey-rename-test.ps1` 那样用码点拼中文；否则 `小米妙播设备` 会变成 `灏忕背濡欐挱璁惧`。
2. `reg.exe` 要 `HKLM\...`，不能给它 PowerShell 提供程序路径 `HKLM:\...`（会报 `ERROR: Invalid key name`）。
3. 执行策略默认拒绝本地 ps1（`未进行数字签名`）→ 用 `powershell -NoProfile -ExecutionPolicy Bypass -File ...`。
4. `reg export` 的 .reg 文件在被 `reg import` 回去前**不要用编辑器另存**（UTF-16/BOM 会被 reg.exe 拒绝）。

### 回滚验证（复现时请照做）
```powershell
reg query 'HKLM\SYSTEM\CurrentControlSet\Enum\ROOT\STEAMSTREAMINGSPEAKERS\0000' /v FriendlyName   # 期望：ERROR: The system was unable to find the specified registry key or value
Get-PnpDevice -InstanceId 'ROOT\STEAMSTREAMINGSPEAKERS\0000' | Select FriendlyName,Status         # 期望：Steam Streaming Speakers / OK
Get-PnpDevice -InstanceId 'SWD\MMDEVAPI\{0.0.0.00000000}.{4a82bbbc-667f-4a9b-b4ce-520a165e6240}' | Select FriendlyName,Status
                                                                                                  # 期望：扬声器 (Steam Streaming Speakers) / OK
reg query 'HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Render\{4a82bbbc-667f-4a9b-b4ce-520a165e6240}\Properties' /v '{b3f8fa53-0004-438e-9003-51a46e139bfc},6'
                                                                                                  # 期望：Steam Streaming Speakers
& $PY -u "$BASE\probe_endpoint_names.py"                                                          # 期望：列表里是 扬声器 (Steam Streaming Speakers)
```
本机 2026-10-17 22:34:16 执行以上核对，**全部为期望值**（见 §2 表与 `device-hwkey-rename-states.json` 的 `AFTER-ROLLBACK` 记录）。

---

## 6. 零驱动路线：最小落地步骤

**前提**：机器上存在一个可借用的 root 枚举虚拟声卡 render 端点（本机 = Steam Streaming Speakers、UU远程虚拟音频设备）。
**能做到**：让 Windows 声音输出列表里出现 `扬声器 (小米妙播设备)`，并把播放到它的音频用 WASAPI loopback 采走（自环矩阵 7/7 通过）。
**做不到**：干净名字（无「扬声器」）、按需新增端点、不依赖第三方驱动。

最小步骤（全程零驱动、零签名、零 test signing）：

1. **枚举**现有 root 虚拟 render 端点，挑一个无人使用的（`MMDevice` 枚举 + `PKEY_Device_EnumeratorName == ROOT`）。
2. **改名**：`reg add HKLM\SYSTEM\CurrentControlSet\Enum\<设备实例>\0000 /v FriendlyName /t REG_SZ /d "小米妙播设备" /f`（需管理员）。
3. **刷新**：对该**设备**做一次 disable/enable（`SetupDiSetDevicePropertyW`+`DIF_PROPERTYCHANGE`，代码见 `DeviceRename.cs::Restart`）→ 端点显示名变成 `扬声器 (小米妙播设备)`。
4. **验证**：`IMMDeviceEnumerator` 重新枚举，按新名字找到端点（`probe_endpoint_names.py` 可作为最小验证脚本）。
5. **取音**：在端点 `X` 上开 WASAPI loopback（本项目 `mixplay-pc-audio\capture\sysaudio.py` 的 `SystemAudioCapture` 已实现）→ 拿到 PCM → 走妙播协议推送。实测延迟：数字自环 6.4–56.6 ms（见 `loopback-matrix.md`）。
6. **卸载/回滚**：`reg delete ... /v FriendlyName /f` + 设备 disable/enable（§5 回滚验证）。

**必须写进产品决策的三条限制**（lead 特别要求区分「可行性证据」与「推荐落地路径」）：
- 这条路线**不是可发布的最终方案**：端点是第三方的（随 Steam/UU 升级、卸载而变），名字带 `扬声器` 前缀，机器上必须有那个软件。它证明的是**"零驱动可行"**，不是**"推荐这么做"**。
- 每增加一个妙播设备，都需要机器上多一个真实存在的虚拟声卡端点 → **多设备必须靠驱动**。
- 干净名字 + 多端点 + 不依赖第三方 = **写驱动**：按 `vad-options.md` §9.1（Scream 派生 + 改 INF 名 + 自签）**3～7 人日**；产品化自研 PortCls miniport 按 §9.2 **8～15 人日**。

---

## 7. 附带证据：Steam 未运行（改名实验期间）

改名实验的每一步都在 Steam 客户端**未运行**时完成，期间端点可用、Status 全程 `OK`：
`Get-Service -Name 'Steam*'` → `Steam Client Service = Stopped`；`Get-Process steam,steamwebhelper,steamservice` → 0 个进程。
带时间戳的连续采样见 `steam-service-timeline.log`（22:35:15–22:36:43，每 4 秒一条，全程 `Stopped` / 0 进程）。
补充事实：SCM 服务名就是带空格的 **`Steam Client Service`**；查 `sc query SteamClientService` 会 `FAILED 1060 (The specified service does not exist as an installed service)`——第一版探针因此把服务状态记成了 `None`，已修（`probe_loopback_pairs2.py::svc_state`）。

---

## 8. 未验证项清单

| # | 未验证项 | 为什么没做 | 影响 |
|---|---|---|---|
| 1 | `Restart-Service Audiosrv -Force` 单独能否刷新端点名（不重启设备） | lead 未批准（会打断其他 teammate 的音频实测） | 只影响"更省事的替代动作"，不影响结论 |
| 2 | 对真实硬件端点（HDAUDIO/USB/BTHENUM）是否同样有效 | 有风险且无必要 | 本项目的虚拟声卡都是 root 软件设备 |
| 3 | Windows 设置 UI 手动改名落在哪个键（是否就是 `,14`） | 需要用户点鼠标；lead 决定打包成一次提问 | 只影响"用户能否自助改名"的产品设计 |
| 4 | `FriendlyName` 在驱动重装/系统升级后是否被清 | 需要重装驱动，破坏性大 | 影响方案耐久性评估 |
| 5 | MMDevices 树「ACL 无 Deny 却拒绝写入」的确切拦截机制 | 需要内核态调试，超出本任务 | 只影响原理洁癖，不影响可行性判断 |
