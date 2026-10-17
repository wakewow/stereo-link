# 零驱动兜底 PoC：把已有虚拟端点改名为「小米妙播设备」并推流到音箱

> ⚠️ **这是 PoC / 兜底演示，不是产品路径。** 产品路径是自签内核驱动（见
> `research/vad/vad-options.md` §9.1），能让端点**干净地**叫 `妙播 - 卧室立体声组合`。
> 本目录的价值是：**真机上验证过「小米妙播设备」能出现在 Windows 声音输出列表里**，
> 并且能把这条线采到的声音推给小爱音箱（端到端已跑通）。它不写一行驱动、
> 不装任何东西、不碰系统安全设置。
> **已知不可靠点见文末 §6，上线前必须读。**

---

## 1. 它由什么组成

| 文件 | 作用 |
|---|---|
| `rename-endpoint.ps1` | 把某个音频**设备**的 FriendlyName 改成任意名字（含 `-Rollback` 回滚 + 无残留校验） |
| `capture_from_endpoint.py` | 按**端点友好名子串**选中端点，用 WASAPI loopback 采出 44100/2ch/s16le 裸 PCM |
| `push_to_speaker.py` | 自建 WAV HTTP 服务（`/audio.wav` 无限流）+ 向 `miplay-bridge` 下发 `play_url`，让音箱自己来拉 |
| `e2e-zero-driver.ps1` | 一键端到端：快照 → 改名 → 采集 → 推流 → **自动回滚 + 无残留校验**（不加 `-Go` 只打印计划） |
| `backups/` | 改名前的注册表备份（`.reg` + `.meta.json`）与每次运行的证据 JSON |

四个脚本互相独立，可以单独用；`e2e-zero-driver.ps1` 只是把它们串起来。

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
* **推给音箱**默认走本项目自己的服务（实测用的就是它）：
  ```
  miplay-bridge serve --port 8901        # 默认后端 http://127.0.0.1:8901
  ```
  `--backend mixplay`（MiXPlay 桌面端 `http://127.0.0.1:8820`）和 `--backend mina`
  （`miservice.play_by_url`，需账号凭据）都保留，但 MiXPlay 桌面端**本机没在跑**、
  本 venv **没有** `miservice`，这两条**未验证**。
* 音箱与 PC 同一局域网（本机实测 LAN IP `192.168.1.4`，音箱 `192.168.1.171`）。
* 首次跑 `push_to_speaker.py` 会尝试自动添加防火墙入站规则
  （`MiPlay zero-driver PoC 8931`，TCP 8931）；已加过就跳过。

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
备份  : backups\backup-Enum-ROOT_STEAMSTREAMINGSPEAKERS_0000-Steam_Streaming_Speakers-<时间戳>.reg
```

模式速查：`-DryRun` 只读 / `-BackupOnly` 只备份 / 都不加 = 备份+改名 / `-Rollback <reg>` = 回滚+校验。

### 3.2 回滚

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\rename-endpoint.ps1 `
    -Rollback '.\backups\backup-Enum-ROOT_STEAMSTREAMINGSPEAKERS_0000-Steam_Streaming_Speakers-20261017-231902.reg'
```

回滚后脚本会逐项打印恢复校验，最后给出 `RESIDUE-FREE`（见 §5）。

### 3.3 采集自检（不碰音箱）

```powershell
& $py .\capture_from_endpoint.py --list
& $py .\capture_from_endpoint.py --endpoint-name '小米妙播设备' --seconds 3 `
      --play-tone 1000 --stats-json .\backups\selftest-capture.json
```

（注意参数名是 `--stats-json`，早期草稿里写的 `--evidence` 不存在。）

### 3.4 推给音箱 / 一键端到端

```powershell
# 只探活控制通道（默认 miplay 后端）
& $py .\push_to_speaker.py --check

# 最短链路：测试音 -> 音箱（不需要采集）
& $py .\push_to_speaker.py --source tone --seconds 5

# 端到端：采集「小米妙播设备」回环 -> 音箱
& $py .\push_to_speaker.py --source capture --endpoint-name 小米妙播设备 `
      --play-tone 1000 --level 0.7 --seconds 8

# 或者一键（不加 -Go 只打印计划，什么都不改）
powershell -NoProfile -ExecutionPolicy Bypass -File .\e2e-zero-driver.ps1
powershell -NoProfile -ExecutionPolicy Bypass -File .\e2e-zero-driver.ps1 -Go -Seconds 8
```

**默认目标是 `did=934947888`（Xiaomi Sound，192.168.1.171）。脚本会拒绝 `did=501718792`
（192.168.1.21），那是另一条测试线的目标，要覆盖得显式加 `--force`。** 默认短音 ≤10 秒；
`--seconds` 超了会自动截到 10 秒；`--level` 默认 0.7 FS。**脚本不会改音箱硬件音量。**

---

## 4. 实测记录（本机真机，2026-10-17）

### 4.1 `rename-endpoint.ps1` 三重只读验证 + 真实改名

| 验证 | 命令要点 | 结果 |
|---|---|---|
| 干跑 | `-DryRun` | ✅ 完全只读（不写文件/注册表/不重启设备），逐条打印将执行的写操作；退出码 0 |
| 备份 | `-BackupOnly` | ✅ 生成 `.reg` 1706 B + `.meta.json` 4295 B（含实例键 9 个值的快照 + SHA256） |
| 回滚干跑 | `-Rollback <reg> -DryRun` | ✅ 打印恢复项与校验项，未写入 |
| **真实改名** | 不加开关 | ✅ `扬声器 (Steam Streaming Speakers)` → **`扬声器 (小米妙播设备)`**（见 §4.5） |

> 真实改名 + 回滚在本目录脚本上真跑过 3 次（23:15 / 23:18 / 23:20），每次结束都回到
> `扬声器 (Steam Streaming Speakers)`。**当前机器无残留**（23:21 复核）。

### 4.2 采集链路自检

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

### 4.4 控制通道（`miplay-bridge`）

```
& $py .\push_to_speaker.py --check
[探活] miplay http://127.0.0.1:8901 ...
        GET /api/health -> HTTP 200 {"account":{"logged_in":...},"ok":true,
                                      "service":"miplay-bridge","version":"0.1.0"}
        GET /api/devices -> 3 个设备（934947888 Xiaomi Sound L16B online
                                      player_play_url stereo_mode=true）
退出码 0
```

`--backend mixplay`（MiXPlay 桌面端 `127.0.0.1:8820`）本机**没在跑**：
`URLError(ConnectionRefusedError(10061))`，退出码 2。本 PoC 用的是 `miplay` 后端。

### 4.5 端到端实测（canonical run 3，`-Go -Seconds 8`，23:18:59）

| 阶段 | 客观结果 |
|---|---|
| A 改名前快照 | dev=`Steam Streaming Speakers`(OK)；EP=`扬声器 (Steam Streaming Speakers)`；备份 `e2e-pre-*.reg` 1706 B |
| B 改名 | `reg add` 成功并读回 `小米妙播设备`；`after disable: status=Error` → `after enable: status=OK`；EP→`扬声器 (小米妙播设备)` |
| C 改名后快照 | 4 个观测点（设备名 / 端点名 / `FriendlyName` / **新进程** soundcard 列表）全部一致 ✅ |
| D 采集新端点 | `peak_dbfs=-11.73`、`rms_dbfs=-15.09`、`seconds=8.011`、`blocks=345`、**`discontinuity_warnings=0`**、`loopback=true` |
| E 推给音箱 | `{'api':'player_play_url','did':'934947888','ok':True}`；**音箱 192.168.1.171 拉走 634880 B（620 KB ≈ 3.60 s）**；服务端共写 995328 B = 5.642 s；`stop ok:true` |
| F 回滚 | **`RESIDUE-FREE`**（§5） |

* 音箱只拉走 3.60 s（服务端写了 5.64 s）：音箱拉完 620 KB 主动断开，随后又重连两次各拉 **0 字节**。
* 测试音 1 kHz、振幅 **0.70 FS**；**未改音箱硬件音量**（保持 55）。
* run 2（23:15:11）同样成功，音箱拉走 **712704 B**（696 KB ≈ 4.04 s）→ **两次独立成功**。
* **人耳确认待 lead 组织**；本目录只给客观证据。

### 4.6 可复现性（run 4，`-Go -SkipSpeaker -Seconds 6`，23:20:38）

改名 → 采集（peak −11.73 dBFS，0 丢帧警告）→ 回滚全流程 **PASS，且无一条 WARN**，
与 run 3 结果一致，证明脚本可重复执行、不会累积残留。

---

## 5. 回滚后如何验证「无残留」

`-Rollback <reg>` 会打印逐项校验，实测输出（run 3）：

```
== verify: no residue
   [PASS] registry values identical to pre-rename snapshot (9 values)
   [PASS] fresh reg export is byte-identical to the backup (SHA256 B39290A8A12A63D9B8915A536C8FFCCC68DC08DDD749CCE1BDEE3E23AADF864A)
   [PASS] Enum\...\FriendlyName is absent again
   [PASS] device status OK
   [PASS] device friendly name restored: 'Steam Streaming Speakers'
   [PASS] endpoint name(s) restored: 扬声器 (Steam Streaming Speakers) |
   [PASS] MMDevices Properties (deviceDesc / ifaceName / devIfacePath) unchanged
== ROLLBACK RESULT
   RESIDUE-FREE: rolled back and verified.
```

等价的手工复验（可直接贴进任何复核流程）：

```powershell
$py   = 'D:\Backup\Documents\deepseek-harness\default-workspace\mixplay-pc-audio\capture\.venv\Scripts\python.exe'
$inst = 'ROOT\STEAMSTREAMINGSPEAKERS\0000'

# 1) Enum 键里不许有 FriendlyName 残留
reg query "HKLM\SYSTEM\CurrentControlSet\Enum\$inst" /v FriendlyName
# 期望: ERROR: The system was unable to find the specified registry key or value.
#       （"值不存在"才是改名前状态；空字符串不算恢复）

# 2) 设备名回到原名 + 状态 OK
Get-PnpDevice -InstanceId $inst | Select-Object Status, FriendlyName
# 期望: OK  Steam Streaming Speakers

# 3) 端点名回到原名（PnP 侧 + 新进程 soundcard 侧两个独立观测点）
Get-PnpDevice -Class AudioEndpoint | Where-Object { $_.InstanceId -like '*4A82BBBC*' } |
  Select-Object Status, FriendlyName
# 期望: OK  扬声器 (Steam Streaming Speakers)
& $py .\capture_from_endpoint.py --list | Select-String '妙播|Steam Streaming Speakers'
# 期望: 只剩 "扬声器 (Steam Streaming Speakers)"，没有"小米妙播设备"

# 4) 重新 export 与备份逐字节比对（SHA256）
reg export "HKLM\SYSTEM\CurrentControlSet\Enum\$inst" "$env:TEMP\now.reg" /y
Get-FileHash "$env:TEMP\now.reg" -Algorithm SHA256
# 期望: B39290A8A12A63D9B8915A536C8FFCCC68DC08DDD749CCE1BDEE3E23AADF864A（run 3 那次备份）
```

`rollback-*.json` 里固化了 5 项检查（`registry_values_match_snapshot` /
`reexport_sha256_matches_backup` / `enum_friendly_absent` / `device_name_restored` /
`endpoint_names_restored` / `mmdevices_properties_unchanged`），全部 `true`。

---

## 6. 为什么这不是产品路径（每条都给实测判定）

1. **名字永远带着 `扬声器` 前缀，改不掉。** pin 名 `扬声器` 来自驱动的
   `{a45c254e-…},2`，由驱动 INF 写死，零驱动无法覆盖。最终形态只能是
   `扬声器 (小米妙播设备)`，做不到产品要求的 `妙播 - 卧室立体声组合`。
   →**【已实测证实】**
2. **改的是 `HKLM\SYSTEM\CurrentControlSet\Enum\...`——PnP 管理的设备缓存位置。**
   设备重新安装、驱动更新、`pnputil` 重新枚举、系统还原都可能把 `FriendlyName` 冲掉，
   刷回去要再 disable/enable。→**【未验证】**（本轮 4 次改名/回滚都稳定；要验需重装驱动）
3. **必须 disable/enable 音频设备才生效。** 该端点会在全系统**消失再出现**
   （实测 `status Error → OK`），会打断该设备上正在播的声音，正在用它的进程会拿到设备丢失错误。
   →**【已实测证实】**
4. **一台机器只能有一个「小米妙播设备」。** 本手法改的是**已有设备**的名字，
   一个虚拟声卡实例只对应 1 render + 1 capture 端点。要 N 个输出设备就得有
   N 个独立虚拟声卡实例——那正是需要驱动（fork Scream / 自研 PortCls）的部分。
   →**【已实测证实（结构上限）】**
5. **依赖提权 + 未文档化 hack。** 普通用户、受控企业环境、开了注册表保护的机器都可能被拒；
   Windows 更新后行为无保证（`research/vad/vad-options.md` §8.1 微软官方立场：
   端点属性 "Clients can read these properties, but should not set them."）。
   →**【已实测证实】**（必须 Administrator；`MMDevices\...\Properties` 连管理员都拒写）
6. **多一跳网络，起播/缓冲不受控，HTTP 服务无鉴权。**
   实测音箱只拉走 3.60 s / 服务端写了 5.64 s，并两次以 **0 字节**重连；
   `/audio.wav` 在局域网内谁都能拉。→**【已实测证实】**
7. **依赖本机常驻控制服务**（`miplay-bridge serve :8901`，账号 token 过期即整体失效）。
   →**【已实测观察】**
8. **名字是纯字符串，编码错了会把乱码写进系统。** run 2 实测：脚本丢 BOM 被
   PowerShell 5.1 按 GBK 解码，`小米妙播设备` 变成 `灏忕背濡欐挱璁惧` 并真的写进了注册表
   （随后改名 + 回滚清理干净）。→**【已实测证实】**
   → 每次编辑 `*.ps1` 后都要确认文件是 **UTF-8 BOM**。

---

## 7. 已知限制 / TODO

* `--backend mina`（`miservice.MinaService.play_by_url`）与 `--backend mixplay`
  **未验证**：本 venv 没有 `miservice`，MiXPlay 桌面端本机也没在跑。
* **人耳确认**（"确实听到声音"）由 lead 组织用户做；本目录只给客观证据。
* 端到端延迟（下发 → 首次拉流 → 出声）**未测**：PC 侧只能证明"音箱拉走了 620 KB"，
  无法确证出声时刻。
* 采集→HTTP 目前是"每 1 秒重开一次 loopback 采集"的朴素实现（PoC 够用），
  没有跨段无缝拼接；产品化应常驻单一采集会话。
* 对外暴露的 HTTP 服务是**无鉴权**的（局域网内谁都能拉 `/audio.wav`），PoC 用完即关；
  产品化必须加 token。
* 采集端点名字改了以后，**正在使用旧名字的脚本会失效**（子串匹配也匹配不到）——
  这是"改名"本身的副作用，写自动化时要小心。
