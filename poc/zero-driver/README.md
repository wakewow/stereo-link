# 零驱动兜底 PoC：把已有虚拟端点改名为「小米妙播设备」并推流到音箱

> ⚠️ **这是 PoC / 兜底演示，不是产品路径。** 产品路径是自签内核驱动（见
> `research/vad/vad-options.md` §9.1），能让端点**干净地**叫 `妙播 - 卧室立体声组合`。
> 本目录的价值是：**今晚就能在真机上看到「小米妙播设备」出现在 Windows 声音输出列表里**，
> 并且能把这条线采到的声音推给小爱音箱。它不写一行驱动、不装任何东西、不碰系统安全设置。
> **已知不可靠点见文末 §6，上线前必须读。**

---

## 1. 它由什么组成

| 文件 | 作用 |
|---|---|
| `rename-endpoint.ps1` | 把某个音频**设备**的 FriendlyName 改成任意名字（含 `-Rollback` 回滚模式） |
| `capture_from_endpoint.py` | 按**端点友好名子串**选中端点，用 WASAPI loopback 采出 44100/2ch/s16le 裸 PCM |
| `push_to_speaker.py` | 自建 WAV HTTP 服务（`/audio.wav` 无限流）+ 下发 `play_url` 让小爱音箱自己来拉 |
| `backups/` | 改名前的注册表备份（`.reg` + `.meta.json`）与自检证据 JSON |

三个文件互相独立，可以单独用。

### 原理（一句话）

Windows 的端点显示名是**运行时拼出来的**：

```
端点显示名 = 驱动 pin 名(注册表 ,2) + " (" + 设备接口 FriendlyName + ")"
```

`设备接口 FriendlyName` 在**设备重新枚举**时会从设备层 `FriendlyName` 刷新。
而设备层那个键 `HKLM\SYSTEM\CurrentControlSet\Enum\<实例>\0000` 的 ACL 是
`BUILTIN\Administrators: FullControl`——**提权管理员可以直接写**（这是和
`MMDevices\...\Properties` 那条死路的关键区别，后者连管理员都被拒）。

所以：写设备层 `FriendlyName` → disable/enable 刷 AudioEndpointBuilder 缓存 → 端点改名成功。
但 pin 名（`扬声器`、`耳机`…）来自驱动，**零驱动改不掉**，所以最终形态是
`扬声器 (小米妙播设备)`，不是干净的 `小米妙播设备`。

---

## 2. 前置条件

* Windows 10/11（本机实测：Windows 10.0.26100 AMD64，Administrator 账户）。
* **改名**需要提权 PowerShell（`BUILTIN\Administrators` 对该 Enum 键有 FullControl）。
* **采集/推流**用项目自带的采集 venv（含 `soundcard` + `numpy`）：
  ```
  D:\Backup\Documents\deepseek-harness\default-workspace\mixplay-pc-audio\capture\.venv\Scripts\python.exe
  ```
* **推给音箱**需要 MiXPlay 桌面端在跑（默认 `http://127.0.0.1:8820`）。
  本 venv **没有** `miservice`，所以脚本默认走 MiXPlay 的 HTTP 控制通道；`--backend mina`
  那条路是留给「有 miservice + 小米账号凭据的环境」的，本机**未验证**。
* 音箱与 PC 同一局域网（本机实测 LAN IP `192.168.1.4`）。

---

## 3. 用法

### 3.0 先看现状：端点叫什么是运行时算出来的

```powershell
# 端点（Windows 声音列表里看到的）
Get-PnpDevice -Class AudioEndpoint | Where-Object Status -eq OK |
  Select-Object Status, FriendlyName, InstanceId | Format-Table -AutoSize

# 设备层（改名的抓手）
Get-PnpDevice | Where-Object { $_.InstanceId -like 'ROOT\*' } |
  Select-Object Status, FriendlyName, InstanceId | Format-Table -AutoSize
```

### 3.1 改名（**加 `-DryRun` 只读干跑；不加就是真写**）

```powershell
cd D:\Backup\Documents\deepseek-harness\default-workspace\miplay-bridge\poc\zero-driver

# a) 干跑：只读，不写文件、不写注册表、不重启设备
powershell -NoProfile -ExecutionPolicy Bypass -File .\rename-endpoint.ps1 `
    -DeviceInstanceId 'ROOT\STEAMSTREAMINGSPEAKERS\0000' -NewName '小米妙播设备' -DryRun

# b) 真改（需提权）：先 reg export 备份 -> 写 FriendlyName -> disable/enable -> 打印改名前/后端点名
powershell -NoProfile -ExecutionPolicy Bypass -File .\rename-endpoint.ps1 `
    -DeviceInstanceId 'ROOT\STEAMSTREAMINGSPEAKERS\0000' -NewName '小米妙播设备'
```

脚本会打印：
```
改名前: 设备名 = Steam Streaming Speakers | 端点名 = 扬声器 (Steam Streaming Speakers)
改名后: 设备名 = 小米妙播设备         | 端点名 = 扬声器 (小米妙播设备)
备份  : backups\backup-Enum-ROOT_STEAMSTREAMINGSPEAKERS_0000-小米妙播设备-<时间戳>.reg
```

模式速查：`-DryRun` 只读 / `-BackupOnly` 只备份 / 都不加 = 备份+改名 / `-Rollback <reg>` = 回滚+校验。

### 3.2 回滚

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\rename-endpoint.ps1 `
    -Rollback '.\backups\backup-Enum-ROOT_STEAMSTREAMINGSPEAKERS_0000-Steam_Streaming_Speakers-20261017-230025.reg'
```

回滚后脚本会逐项打印恢复校验（见 §5）。

### 3.3 采集自检（不碰音箱）

```powershell
& $py .\capture_from_endpoint.py --list
& $py .\capture_from_endpoint.py --endpoint-name '小米妙播设备' --seconds 3 `
      --play-tone 1000 --evidence .\backups\selftest-capture.json
```

### 3.4 推给音箱

```powershell
# 只探活控制通道
& $py .\push_to_speaker.py --check

# 最短链路：测试音 -> 音箱（不需要采集）
& $py .\push_to_speaker.py --source tone --seconds 5 --volume 20

# 端到端：采集「小米妙播设备」回环 -> 音箱（--play-tone 让虚拟端点有内容）
& $py .\push_to_speaker.py --source capture --endpoint-name 小米妙播设备 `
      --play-tone 1000 --seconds 8 --volume 20 --evidence .\backups\e2e.json
```

**默认目标是 `did=934947888`（Xiaomi Sound，192.168.1.171）。脚本会拒绝 `did=501718792`
（192.168.1.21），那是另一条测试线的目标，要覆盖得显式加 `--force`。** 默认短音 ≤10 秒；
`--seconds` 超了会自动截到 10 秒。

---

## 4. 实测记录（本机真机，2026-10-17）

### 4.1 `rename-endpoint.ps1` 三重只读验证（**尚未真实改名**）

| 验证 | 命令要点 | 结果 |
|---|---|---|
| 干跑 | `-DryRun` | ✅ 完全只读（不写文件/注册表/不重启设备），逐条打印将执行的写操作；退出码 0 |
| 备份 | `-BackupOnly` | ✅ 生成 `.reg` 1706 B + `.meta.json` 4295 B（含 5 个属性键快照） |
| 回滚干跑 | `-Rollback <reg> -DryRun` | ✅ 打印恢复项与校验项，未写入 |

> 真实改名 + 回滚的端到端**已在 `research/vad/rename-probe.md` 里用同一手法验证过**
> （四态：改名前 / 写值后 / 重启设备后 / 回滚后），结论是端点名变成 `扬声器 (小米妙播设备)`、
> 回滚后逐字节复原。本目录脚本是把它参数化 + 加了 `-Rollback` 模式。
> **在本目录里的真机执行等 lead 的 go**（task-9 正在用音频端点跑采集测试，
> disable/enable 会让它枚举到的端点消失再重现，可能造成它的假失败）。

### 4.2 采集链路自检（现有端点 `扬声器 (Steam Streaming Speakers)` 代替）

```
[结果] {"blocks": 43, "frames": 44032, "seconds": 0.998, "peak_dbfs": -20.68,
         "rms_dbfs": -24.04, "discontinuity_warnings": 0,
         "endpoint": "扬声器 (Steam Streaming Speakers)", "loopback": true,
         "rate": 44100, "channels": 2, "tone_hz": 1000.0}
```

* 1 kHz 测试音（幅度 0.25）经 **WASAPI loopback 数字自环**采回 **peak −20.68 dBFS / RMS −24.04 dBFS**，
  **丢帧警告 0**，逐块电平稳定（−20.68 / −20.68 / −20.68）。
* 判据：数字自环 ≥ −60 dBFS；声学串音在 −87 ~ −107 dBFS（见 `research/vad/loopback-matrix.md`）。
  差 50+ dB，所以 −20.68 是**确定性数字通路**，不是麦克风串音。

### 4.3 WAV HTTP 服务自检（不碰音箱）

| 用例 | 写入 | 客户端读到 | WAV 头 | 证据 |
|---|---|---|---|---|
| `--source tone --seconds 4 --local-selftest` | 705600 B = **4.000 s** | 541760 B = 3.071 s | ✅ `RIFF/WAVE/fmt/data`，`data` size = 0x7FFFFF00（无限流） | `backups/selftest-push-tone.json` |
| `--source capture --endpoint-name 'Steam Streaming Speakers' --seconds 4` | 524288 B = **2.972 s** | 434176 B = 2.461 s | ✅ | `backups/selftest-push-capture.json` |

客户端读到的时间比写入少约 1 秒，是脚本**故意的**：先 `sleep(1.0)` 垫数据再让客户端连，
避免音箱拉到空流。可复现。

### 4.4 控制通道

```
& $py .\push_to_speaker.py --check
[探活] MiXPlay http://127.0.0.1:8820 ...
        不可达: URLError(ConnectionRefusedError(10061, ...))
        -> 先启动 MiXPlay 桌面端（默认 http://127.0.0.1:8820）   退出码 2
```

即：控制通道探测可用，但**本机当前 MiXPlay 桌面端没在跑**，所以"音箱出声"这一步还未实测。

---

## 5. 回滚后如何验证「无残留」

回滚（`-Rollback <reg>`）会打印逐项校验，等价于手工执行下面三条：

```powershell
$inst = 'ROOT\STEAMSTREAMINGSPEAKERS\0000'
$key  = "HKLM:\SYSTEM\CurrentControlSet\Enum\$inst"

# 1) 设备层 FriendlyName 必须"不存在"（改名前就是不存在，不是空字符串）
(Get-ItemProperty -Path $key -Name FriendlyName -ErrorAction SilentlyContinue) -eq $null
# 期望: True

# 2) 设备名必须回到原名
(Get-PnpDevice -InstanceId $inst).FriendlyName
# 期望: Steam Streaming Speakers

# 3) 端点名必须回到原名（AEB 缓存由脚本的 disable/enable 刷新）
(Get-PnpDevice -Class AudioEndpoint | Where-Object InstanceId -like "*4A82BBBC*").FriendlyName
# 期望: 扬声器 (Steam Streaming Speakers)

# 4) 靠 backup .reg 逐字节比对（脚本自动做，也可手工）
reg export "HKLM\SYSTEM\CurrentControlSet\Enum\$inst" "$env:TEMP\now.reg" /y
# 与备份文件对比：DeviceDesc / Service / ClassGUID / FriendlyName 四项应一致
```

`research/vad/rename-probe.md` 里的实测结果：回滚后设备名 = `Steam Streaming Speakers`、
端点名 = `扬声器 (Steam Streaming Speakers)`、`FriendlyName` 值**恢复为"不存在"**，
`Get-PnpDevice` Status 仍为 OK。

---

## 6. 为什么这不是产品路径（至少三条具体不可靠点）

1. **名字永远带着 `扬声器` 前缀，改不掉。** pin 名 `扬声器` 来自驱动的
   `{a45c254e-…},2`，由驱动 INF 写死，零驱动无法覆盖。所以最终形态只能是
   `扬声器 (小米妙播设备)`，做不到产品要求的 `妙播 - 卧室立体声组合`。
2. **改的是 `HKLM\SYSTEM\CurrentControlSet\Enum\...`——微软明确不保证持久化的位置。**
   它的 ACL 之所以放得开，是因为它是**设备安装时由 PnP 管理的缓存**：设备重新安装、
   驱动更新、`pnputil` 重新枚举、系统还原，都可能把 `FriendlyName` 冲掉，
   而刷回去需要再次 disable/enable。产品里等于"改完可能某天自己变回去"。
3. **必须 disable/enable 音频设备才生效。** 这一步会让该端点在整个系统里**消失再出现**
   （任何正在用它的程序——包括我们自己的采集线程——会拿到设备丢失错误），
   还会打断该设备上正在播的声音。产品里不能在"用户正在听歌"的时候做这个动作。
4. **一台机器只能有一个「小米妙播设备」。** 本手法改的是**已有设备**的名字，
   而每个虚拟声卡实例只对应一个 render 端点 + 一个 capture 端点。
   要"每个妙播设备一个 Windows 输出设备"，就得有 N 个独立虚拟声卡实例——
   这正是需要驱动（fork Scream / 自研 PortCls）的部分，零驱动做不到。
5. **依赖 Machine 级注册表写权限 + 提权。** 普通用户、受控企业环境、
   开了注册表保护/AppLocker 的机器上都可能被拒；而且这是**未文档化的 hack**，
   Windows 更新后行为无保证（`research/vad/vad-options.md` §8.1 里微软的官方立场是
   端点属性 "Clients can read these properties, but should not set them."）。
6. **这条链路的"真出声"还多一跳网络**：音箱自己来拉 HTTP 流，延迟和稳定性取决于
   Wi-Fi 与音箱端缓冲，比内核驱动直通多一个不可控环节（MiNA/MiXPlay 的 `play_url`
   只是"让音箱去拉一个 URL"，音箱什么时候开始拉、缓冲多少，都不受我们控制）。

---

## 7. 已知限制 / TODO

* `--backend mina`（直接用 `miservice.MinaService.play_by_url`）**未验证**：
  本 venv 没有 `miservice`，需要带账号凭据的环境。
* 真实改名 + 端到端出声**等 lead 的 go**（task-9 冲突，见 §4.1）。
* 采集→HTTP 链路目前是"每 1 秒重开一次 loopback 采集"的朴素实现（PoC 够用），
  没有做跨段拼接的无缝处理；正式产品里这段应该常驻单一采集会话。
* 对外暴露的 HTTP 服务是**无鉴权**的（局域网内谁都能拉 `/audio.wav`），PoC 用完即关；
  产品化必须加 token。
