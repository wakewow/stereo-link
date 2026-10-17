# 虚拟声卡：在 Windows 输出列表里直接选「扬声器 (立体声链设备)」

> 目标：给 StereoLink 一个普通的「系统输出」入口——任何程序把输出切到它，声音就进入左右声道
> 分离链路，不需要在每个播放器里单独选设备。
>
> 实现：**借用机器上已有的已签名虚拟声卡（Steam Streaming Speakers），只改设备显示名**。
> 不写驱动、不签驱动、不装用户态服务。

## 1. 整条链路

```text
任何播放器
   │  （输出选到「扬声器 (立体声链设备)」）
   ▼
虚拟声卡端点（Steam Streaming Speakers，设备名改成了「立体声链设备」）
   │  WASAPI loopback 采集
   │  48 kHz/2ch/32f 等混音格式 → 44100 Hz / 2 ch / 16 bit 交错 PCM
   ▼
stereo-link 面板内的路由引擎（stereo-audio）
   ├─ 左声道 → 播放设备 A（例如蓝牙音箱 1）
   └─ 右声道 → 播放设备 B（例如蓝牙音箱 2）
```

虚拟声卡本身**不处理音频**：它只是一个有 render 端点的空设备，声音写进去就被回环采走
（Steam 客户端平时用它把游戏声音串流到其它设备）。StereoLink 借它的端点 + 设备名。

## 2. 为什么不是「装一个自己的驱动」

用户态程序**不可能**凭空注册 render 端点：端点由 `AudioEndpointBuilder` 从内核驱动注册的
`KSCATEGORY_AUDIO` 接口派生。三条路线都评估/试过：

| 路线 | 结论 |
| --- | --- |
| 自研 / 改造驱动（Scream 派生） | 签名能装上，但驱动载不起来：`0xC0000428 STATUS_INVALID_IMAGE_HASH`（`CM_PROB_UNSIGNED_DRIVER`）。必须开 test signing 或买 EV 证书做 attestation，不适合普通用户 |
| USB/IP 虚拟设备 | Windows 侧 VHCI 只有未签名的 `usbip-win2`，同样要常开 test signing |
| **借用已有虚拟声卡 + 改设备名** | ✅ **唯一免驱动、免证书、免重启的可行路线**，当前实现 |

Steam Streaming Speakers 是 Valve 随 Steam 安装的**已签名**虚拟声卡（不是 StereoLink 分发
的），文件名 `C:\Windows\System32\drivers\SteamStreamingSpeakers.sys`，约 40 KB，内核态运行，
没有用户态进程。本仓库只改它的注册表显示名，不碰驱动文件。

## 3. 改名做了什么

活路是写**设备硬件键**里的 `FriendlyName`，再让 `AudioEndpointBuilder` 重建一次端点：

```powershell
# 1) 备份整个设备键
reg export "HKLM\SYSTEM\CurrentControlSet\Enum\ROOT\STEAMSTREAMINGSPEAKERS\0000" backup.reg /y

# 2) 写设备级名字
reg add "HKLM\SYSTEM\CurrentControlSet\Enum\ROOT\STEAMSTREAMINGSPEAKERS\0000" `
  /v FriendlyName /t REG_SZ /d "立体声链设备" /f

# 3) 让端点显示名跟着更新（会短暂 disable/enable 设备一次）
pnputil /disable-device "ROOT\STEAMSTREAMINGSPEAKERS\0000"
pnputil /enable-device  "ROOT\STEAMSTREAMINGSPEAKERS\0000"
```

结果：设备名 `立体声链设备` → Windows 输出列表里显示为 **`扬声器 (立体声链设备)`**。
前缀「扬声器 」是系统按端点类型自动加的，不是我们写的。

已经确认的**死路**（不要再试）：

- `HKLM\SYSTEM\CurrentControlSet\Control\MMDevices\Audio\Render\{GUID}\Properties` 下的 `,14`
  （设备属性存储里的 FriendlyName）：`reg add` 报 `Access is denied`，官方
  `IPropertyStore::SetValue` 也是 `0x80070005 E_ACCESSDENIED`（提权管理员同样被拒，是
  Audiosrv 的 ACL 保护），而且音频服务重启后会被覆盖。
- 想去掉「扬声器 」前缀：那是端点/驱动 pin 名，零驱动改不掉。

## 4. 脚本与面板入口

| 入口 | 作用 |
| --- | --- |
| `安装虚拟声卡.cmd` | 管理员运行；把设备名改成「立体声链设备」，输出列表出现「扬声器 (立体声链设备)」 |
| `卸载虚拟声卡.cmd` | 管理员运行；按备份 `.reg` 回滚设备名，找不到备份时删除 `FriendlyName` 值 |
| 面板 →「虚拟声卡」→「应用重命名」 | 面板内改名：弹 UAC 后执行同一套脚本，设备 disable/enable 一次（约 2–5 秒音频可能中断） |
| `scripts\rename-endpoint.ps1` | 底层实现，也可以手动跑（见下） |

脚本参数：

| 参数 | 说明 |
| --- | --- |
| `-DeviceInstanceId` | 设备实例路径，Steam Streaming Speakers 固定为 `ROOT\STEAMSTREAMINGSPEAKERS\0000` |
| `-NewName` | 新设备名（面板限制 1–64 个字符、不能含 `\ / : * ? " < > \|`、不能全是空格） |
| `-Rollback <reg>` | 从某个备份 `.reg` 回滚；**不能和 `-DeviceInstanceId` 同时传**（不同参数集，会报 `AmbiguousParameterSet`） |
| `-BackupDir` | 备份目录，默认 `scripts\backups` |
| `-DryRun` | 只打印将要做的事，不写注册表 |
| `-BackupOnly` | 只备份、不改名 |

```powershell
# 手动改名（管理员 PowerShell；脚本自己会先备份到 scripts\backups）
powershell -ExecutionPolicy Bypass -File scripts\rename-endpoint.ps1 `
  -DeviceInstanceId ROOT\STEAMSTREAMINGSPEAKERS\0000 -NewName "立体声链设备"

# 试运行
powershell -ExecutionPolicy Bypass -File scripts\rename-endpoint.ps1 `
  -DeviceInstanceId ROOT\STEAMSTREAMINGSPEAKERS\0000 -NewName "立体声链设备" -DryRun

# 从备份回滚
powershell -ExecutionPolicy Bypass -File scripts\rename-endpoint.ps1 `
  -Rollback scripts\backups\backup-Enum-ROOT_STEAMSTREAMINGSPEAKERS_0000-Steam_Streaming_Speakers-<时间戳>.reg
```

备份目录 `scripts\backups\` 下每次改动会留下：

- `backup-Enum-ROOT_STEAMSTREAMINGSPEAKERS_0000-Steam_Streaming_Speakers-<时间戳>.reg`：
  改名前导出的整个设备键（文件名里的前缀取自**原始设备名**）；
- 同名 `.meta.json`：改动前的设备名、端点名等元数据；
- `rename-<时间戳>.json` / `rollback-<时间戳>.json`：每次执行的结果证据。

**重复安装会污染备份**：第二次「改名」时，备份里的 `FriendlyName` 已经是「立体声链设备」，
拿它回滚等于没回滚。所以卸载脚本只挑文件名含 `Steam_Streaming_Speakers` 的原始备份。

## 5. 安装 / 卸载 / 改名步骤

安装（一次即可）：

1. 右键 `安装虚拟声卡.cmd` →「以管理员身份运行」，UAC 确认（需要管理员）。
2. 脚本备份注册表 → 写设备名 → disable/enable 设备 → 校验端点显示名。
3. 「设置 → 系统 → 声音 → 输出」里出现 **「扬声器 (立体声链设备)」** 即成功；面板诊断
   也会显示虚拟声卡可用。

卸载（恢复原名）：

1. 先退出面板（停止路由、默认输出切回）。
2. 右键 `卸载虚拟声卡.cmd` →「以管理员身份运行」，UAC 确认。
3. 脚本从 `scripts\backups\` 里挑最早的 `backup-Enum-*Steam_Streaming_Speakers*.reg` 回滚；
   找不到备份时回退为删除 `FriendlyName` 值，设备名恢复成 Steam Streaming Speakers。
4. 设备管理器里若显示黄色感叹号，先「禁用」再「启用」一次。

改名（面板内）：

1. 面板右侧「虚拟声卡」区，在「虚拟声卡名称」输入框里填新名字。
2. 点「应用重命名」→ UAC 确认 → 设备 disable/enable 一次（约 2–5 秒，期间音频可能中断）。
3. 完成后点「刷新设备」，输出列表和面板都会显示新名字。改名失败可在 `scripts\backups\`
   里找到改动前的备份回滚。

## 6. 常见问题

| 现象 | 处理 |
| --- | --- |
| 输出列表里没有「扬声器 (立体声链设备)」 | 管理员运行 `安装虚拟声卡.cmd`；若跑过，检查是否被卸载脚本还原；设备管理器 → 声音、视频和游戏控制器里确认 Steam Streaming Speakers 存在且启用 |
| 装的时候提示找不到设备 | 确认 Steam 已安装且这台机器上出现过 Steam Streaming Speakers；没有该设备就无法借用，需要先让 Steam 安装它（或用已安装的其它同签名虚拟声卡，改脚本里的实例 id） |
| 改名后输出列表名字没变 | 点面板「刷新设备」；仍不变则在设备管理器里禁用/启用一次该设备，或重跑脚本；UAC 被取消会回滚 |
| 改名后提示名字不合法 | 名字需 1–64 个字符、不能含 `\ / : * ? " < > \|`、不能全是空格 |
| 卸载后还有旧名字 | 确认卸载时选的是 `Steam_Streaming_Speakers` 的原始备份；面板缓存点「刷新设备」；必要时重启 Windows |
| Steam 更新 / 系统重装后又变回旧名 | 重跑 `安装虚拟声卡.cmd`（或面板「应用重命名」）即可；驱动本身不受影响 |
| 备份目录里有很多 .reg | 每次安装/改名都会留一份；卸载只挑原始名备份，可手工清理旧的 |
| 改名前需要退出面板吗 | 不需要，但改名前建议先停止路由，避免 2–5 秒的设备重启造成播放异常 |

## 7. 注意

- 改名需要管理员权限；设备 disable/enable 会有约 2–5 秒的音频中断，属正常现象。
- 驱动归 Steam/Valve 所有，本仓库不修改、不重签、不分发驱动文件，只改注册表里的显示名。
- 「扬声器 (立体声链设备)」只是**名字**：如果 Steam 客户端把驱动卸载了，端点会消失，需要重装
  Steam 或重新安装该驱动。
- 虚拟声卡没有用户态进程，也不联网；数据只在 `stereo-link.exe` 进程里流转。
