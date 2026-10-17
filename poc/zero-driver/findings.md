# findings.md —— 零驱动兜底 PoC 实测记录

任务：task-12（零驱动兜底 PoC）。写作用域：`miplay-bridge/poc/zero-driver/`。
实测环境：Windows 10.0.26100 AMD64（Administrator，High 完整性级别），
LAN IP `192.168.1.4`，采集 venv
`D:\Backup\Documents\deepseek-harness\default-workspace\mixplay-pc-audio\capture\.venv\Scripts\python.exe`
（`soundcard` + `numpy`，**没有** `miservice`）。
被改名的设备：`ROOT\STEAMSTREAMINGSPEAKERS\0000`（Valve Steam Streaming Speakers，未被占用的虚拟端点）。
音箱控制后端：**`miplay-bridge serve --port 8901`**（本项目自己的 Rust 服务，不用 MiXPlay 桌面端）。

> 状态标记约定：**【已实测】**= 本机跑过并留下证据文件；**【未验证】**= 本机条件不具备，未跑过。

---

## 0. 结论速览（全部为 2026-10-17 23:18–23:21 的真实运行）

| 环节 | 状态 | 客观证据 |
|---|---|---|
| 端点在 Windows 声音列表里改名 | **【已实测】✅** | run 3：`扬声器 (Steam Streaming Speakers)` → **`扬声器 (小米妙播设备)`**，4 个观测点一致 |
| 改名后能用新名字选到端点采集 | **【已实测】✅** | run 3/4：新进程 soundcard 枚举到 `扬声器 (小米妙播设备)`；采集 peak **−11.73 dBFS** / RMS −15.09 dBFS，**丢帧警告 0** |
| 端到端把声音送到音箱 | **【已实测】✅** | run 3：HTTP 服务被 **192.168.1.171（音箱）** 拉走 **634880 B（620 KB ≈ 3.60 s）**；下发返回 `ok:true`；`stop` 返回 `ok:true` |
| 回滚 + 无残留 | **【已实测】✅** | run 3/4：`RESIDUE-FREE`；设备名/端点名复原、`FriendlyName` 值恢复"不存在"、重新 `reg export` 与备份 **SHA256 逐字节相同** |
| 可复现性 | **【已实测】✅** | run 4（`-SkipSpeaker`，23:20:38）与 run 3 完全一致的改名+回滚结果，无一条 WARN |
| `--backend mina`（`miservice.play_by_url`） | **【未验证】** | 本 venv 无 `miservice`，需带凭据的环境；本 PoC 走的是 `--backend miplay` |
| MiXPlay 桌面端通道 | **【已实测·不可达】** | `--check` 连 `127.0.0.1:8820` 报 `ConnectionRefusedError(10061)`；已被 `--backend miplay` 取代 |
| 人耳确认"听到了" | **【待 lead 组织】** | 音箱侧可观测证据齐全（下方 §2），人耳确认由 lead 找用户做 |

**一句话**：零驱动能让 **`扬声器 (小米妙播设备)`** 真的出现在 Windows 声音输出列表里，并且选它出声
能把声音送到 `192.168.1.171` 的音箱；但名字永远带 `扬声器` 前缀，且一台机器只有一个这样的端点（§5）。

---

## 1. 改名：四态实测（本目录脚本真机执行，run 3）

| 态 | 设备层 `FriendlyName` | `Get-PnpDevice` 设备名 | 声音列表里的端点名 | 证据文件 |
|---|---|---|---|---|
| ① 改名前 | 值**不存在** | `Steam Streaming Speakers` | `扬声器 (Steam Streaming Speakers)` | `backups/e2e-20261017-231859.json`（phases.pre） |
| ② 写值后（未重启设备） | `小米妙播设备` | `小米妙播设备` | `扬声器 (Steam Streaming Speakers)`（AEB 缓存未刷） | `backups/rename-20261017-231908.json` 内的 `Write-Step 'write FriendlyName'` 阶段 |
| ③ disable/enable 后 | `小米妙播设备` | `小米妙播设备` | **`扬声器 (小米妙播设备)`** | 同上（`refresh AudioEndpointBuilder` 之后） |
| ④ 回滚 + 重启后 | 值**不存在** | `Steam Streaming Speakers` | `扬声器 (Steam Streaming Speakers)` | `backups/rollback-20261017-231957.json`（`RESIDUE-FREE`） |

四个独立观测点（注册表直读 / SetupAPI / `Get-PnpDevice` 设备+端点 / **新进程** soundcard 枚举）在每态互相印证：

```
 0  [loopback]  CG345UK (NVIDIA High Definition Audio)
 1  [loopback]  X41 (NVIDIA High Definition Audio)
 2  [loopback]  扬声器 (小米妙播设备)          <-- 改的就是它（run 3/4 都在位）
 3  [loopback]  扬声器 (Steam Streaming Microphone)
 ...
共 11 个 capture 端点
```

* 端点在改名期间会**消失再出现**：`after disable: status=Error` → `after enable: status=OK`
  （run 3 实测）。这正是 §5 第 3 条的实测依据。
* 改名后该设备**多出一个原本隐藏的 capture 端点** `内部 AUX 插座 (小米妙播设备)`
  （GUID `{06a4c98d-4ce3-4137-9523-079effea94db}`，`not-present`，不进声音列表）——
  端点名的构成是 `驱动 pin 名 + " (" + 接口 FriendlyName + ")"` 的直接证据。
* `扬声器` 前缀来自驱动 pin 名（`{a45c254e-…},2`），**零驱动改不掉**（task-8 已证）。

---

## 2. 端到端出声（run 3，canonical）

一键脚本：

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\e2e-zero-driver.ps1 -Go -Seconds 8
```

| 阶段 | 结果 |
|---|---|
| A 改名前快照 | dev=`Steam Streaming Speakers`(OK)，EP=`扬声器 (Steam Streaming Speakers)`，备份 `e2e-pre-20261017-231859.reg` 1706 B |
| B 改名 | `reg add` 成功并读回 `小米妙播设备`；disable/enable；EP→`扬声器 (小米妙播设备)`；备份 `backup-…-20261017-231902.reg` 1706 B + `.meta.json` |
| C 改名后快照 | 4 个观测点：设备名 / 端点名 / `FriendlyName` / **新进程** soundcard 列表，全部一致 ✅ |
| D 采集新端点 | `peak_dbfs=-11.73`、`rms_dbfs=-15.09`、`seconds=8.011`、`blocks=345`、**`discontinuity_warnings=0`**，`loopback=true` |
| E 推给音箱 | 下发 `{'api':'player_play_url','device':'Xiaomi Sound','did':'934947888','ok':True}`；**音箱 192.168.1.171 拉走 634880 B（620 KB ≈ 3.60 s）**；服务端共写 995328 B = 5.642 s；`stop ok:true` |
| F 回滚 | `RESIDUE-FREE`（§3） |

细节与诚实说明：

* **音箱只拉走 3.60 s，服务端写了 5.64 s**：音箱拉了 620 KB 后主动断开，随后又重连两次各拉 **0 字节**。
  这是"音箱自己决定拉到哪"的直接证据（§5 第 6 条）。
* 测试音：1 kHz 正弦，振幅 **0.70 FS**（按 lead 要求 ≥0.7 FS），音箱侧**未改硬件音量**（保持 55）。
* 采集用的是 `--source capture`（真采被改名端点的 loopback），不是直接推测试音。
* **人耳确认：待 lead 组织**。本轮 run 2（23:15:11，同样成功，音箱拉走 712704 B）与 run 3
  各让音箱播了约 3.6 s 的 1 kHz 单音；如果用户当时在场，听到的就是这个。
* 参考：run 2 的推送字节数 712704 B（696 KB ≈ 4.04 s），与 run 3 同量级 → 两次独立成功。

---

## 3. 回滚 + 无残留验证（**硬性验收项**）

回滚命令（`e2e-zero-driver.ps1` 的 F 阶段就是自动跑这条）：

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\rename-endpoint.ps1 `
    -Rollback '.\backups\backup-Enum-ROOT_STEAMSTREAMINGSPEAKERS_0000-Steam_Streaming_Speakers-20261017-231902.reg'
```

脚本输出（run 3，节选）：

```
== restore registry
   deleted FriendlyName (was '小米妙播设备')
   reg import: The operation completed successfully.
== restart device (disable/enable)
     after disable: status=Error
     after enable : status=OK
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

### 3.1 手工复验四条命令（可直接贴进任何复核流程）

```powershell
$py   = 'D:\Backup\Documents\deepseek-harness\default-workspace\mixplay-pc-audio\capture\.venv\Scripts\python.exe'
$inst = 'ROOT\STEAMSTREAMINGSPEAKERS\0000'

# 1) Enum 键里不许有 FriendlyName 残留（改名前它就是"不存在"）
reg query "HKLM\SYSTEM\CurrentControlSet\Enum\$inst" /v FriendlyName
# 期望: ERROR: The system was unable to find the specified registry key or value.   <-- 已实测（23:11/23:19 两次）

# 2) 设备名回到原名 + 状态 OK
Get-PnpDevice -InstanceId $inst | Select-Object Status, FriendlyName
# 期望: OK  Steam Streaming Speakers                                              <-- 已实测

# 3) 端点名回到原名（PnP 侧 + 新进程 soundcard 侧，两个独立观测点）
Get-PnpDevice -Class AudioEndpoint | Where-Object { $_.InstanceId -like '*4A82BBBC*' } |
  Select-Object Status, FriendlyName
# 期望: OK  扬声器 (Steam Streaming Speakers)                                      <-- 已实测
& $py .\capture_from_endpoint.py --list | Select-String '妙播|Steam Streaming Speakers'
# 期望: 只剩 "扬声器 (Steam Streaming Speakers)"，没有"小米妙播设备"           <-- 已实测

# 4) 重新 export 与备份逐字节比对（脚本自动做；SHA256 值见上方 run 3 输出）
```

`rollback-*.json` 里还固化了 5 项检查（`checks.registry_values_match_snapshot` /
`reexport_sha256_matches_backup` / `enum_friendly_absent` / `device_name_restored` /
`endpoint_names_restored` / `mmdevices_properties_unchanged`），全部 `true`。

**当前机器状态（23:21 复核）**：设备名 `Steam Streaming Speakers`、端点名 `扬声器 (Steam Streaming Speakers)`、
`FriendlyName` 不存在、`Get-PnpDevice` Status=OK —— 无残留。

---

## 4. 踩坑与两个真实 bug（诚实记录，含我早先的错误推断）

1. **`$ErrorActionPreference='Stop'` + 原生命令 stderr = 回滚静默中断（真因，已修）**。
   Windows PowerShell 5.1 会**把原生命令写往 stderr 的任何输出当成终止性 `NativeCommandError`**，
   而 `reg.exe import` / `reg.exe delete` 在**成功**时也会往 stderr 打印
   `The operation completed successfully.`。结果：run 1/run 2 的回滚在执行完"删值"后立刻中断，
   **disable/enable 根本没跑**，端点名于是停在上一次的 `扬声器 (小米妙播设备)`，
   而我当时**误判为"AEB 需要两次刷新"**。run 3/run 4 的修复版（`Invoke-Native` 把所有
   `reg.exe`/`pnputil.exe` 调用包进 `$ErrorActionPreference='Continue'`，只按退出码判断）
   一次刷新即复原，retry 循环一次都没触发 → **根因是脚本 bug，不是 AEB 缓存慢**。
   修好后 `-Rollback` 稳定给出 `RESIDUE-FREE`。
2. **`.ps1` 里的中文与 BOM（真因，已修）**。用编辑器/工具保存后脚本会丢掉 UTF-8 BOM，
   PowerShell 5.1 于是按 GBK 解码，`小米妙播设备` 变成 **`灏忕背濡欐挱璁惧`**（run 2 实测，
   还把乱码真的写进了注册表和端点名，随后已回滚清理）。
   修法：写回 UTF-8 **BOM**；每次编辑 `*.ps1` 后都要复核（本目录脚本现已带 BOM）。
3. **改名的抓手是设备，不是端点**。音频端点的 PnP `InstanceId` 是
   `SWD\MMDEVAPI\{0.0.0.00000000}.{GUID}`，**不是** `ROOT\...`；要动的是它背后
   `{233164c8-1b2c-4c7d-bc68-b671687a2567},1` 指向的那个 `ROOT\...` 设备实例。
4. **capture 端点不能按名字精确匹配**：`soundcard` 的 `all_microphones(include_loopback=True)`
   才带 `[loopback]`，且改名后名字带括号，必须**子串**匹配。
5. 后台跑 python 必须加 `-u`（否则日志缓冲到进程结束才吐）；
   本机 `Start-Process` 有 bug（`NO_PROXY`/`no_proxy` 键冲突），一律直接调用或后台作业。
6. **`MMDevices\Audio\Render\<guid>\Properties` 是死路**（`ERROR: Access is denied.` /
   `0x80070005 E_ACCESSDENIED`），本 PoC 完全不碰它 —— 这也是备份 `.reg` 里
   **不含 `Properties` 子键**的原因（`reg export` 枚举不动它）。

---

## 5. 这条路线作为产品路径的不可靠点（每条都给实测判定）

| # | 不可靠点 | 判定 |
|---|---|---|
| 1 | **名字永远带 `扬声器` 前缀**：pin 名来自驱动 `{a45c254e-…},2`，零驱动无法覆盖 | **已实测证实**：最终形态只能是 `扬声器 (小米妙播设备)`，做不到产品要的 `妙播 - 卧室立体声组合` |
| 2 | **写的是 PnP 管理的设备缓存键** `HKLM\SYSTEM\CurrentControlSet\Enum\...`：设备重装/驱动更新/`pnputil` 重枚举/系统还原都可能冲掉 | **未验证**（理论推断）：本轮 4 次改名/回滚都稳定，但没人真的重装过驱动；产品里等于"改完可能某天自己变回去" |
| 3 | **生效必须 disable/enable**：该端点在全系统**消失再出现** | **已实测证实**：`status Error → OK`；若在"用户正在听歌"时做，会打断该设备上正在播的声音，正在用它的进程会拿到设备丢失错误 |
| 4 | **一台机器只能有一个「小米妙播设备」**：改的是已有设备，一个虚拟声卡实例 = 1 render + 1 capture 端点 | **已实测证实**（结构上限）：要 N 个输出设备就得有 N 个独立虚拟声卡实例，那正是需要驱动的部分 |
| 5 | **需要提权 + 未文档化 hack**；微软官方立场 "Clients can read these properties, but should not set them."（`research/vad/vad-options.md` §8.1） | **已实测证实**：必须 Administrator（High IL）；`MMDevices\...\Properties` 连管理员都拒写，只有 Enum 键这条路能走 |
| 6 | **多一跳网络，起播/缓冲不受控，HTTP 服务无鉴权** | **已实测证实**：音箱只拉走 3.60 s / 服务端写了 5.64 s，并两次以 **0 字节** 重连；`/audio.wav` 局域网内谁都能拉 |
| 7 | **控制通道依赖本机常驻服务**（`miplay-bridge serve :8901`，且账号 token 过期就整体失效） | **已实测观察**：本轮靠 lead 预先起的服务才跑通；服务一停，音箱就收不到 `play_url` |
| 8 | **名字是纯字符串，脚本编码/终端编码错了就会把乱码写进系统**（run 2 实例） | **已实测证实**：修 BOM 后必须重新改名+回滚才能清掉乱码名 |

---

## 6. 未验证 / 后续

1. `--backend mina`（直接用 `miservice`）**未验证**：本 venv 无 `miservice`，需要带账号凭据的环境。
2. **人耳确认**（"确实听到声音"）由 lead 组织用户做；本文件只给客观证据。
3. 采集→HTTP 目前是"每 1 秒重开一次 loopback 采集"的朴素实现（PoC 够用），
   没有跨段无缝拼接；产品化应常驻单一采集会话。
4. 端到端延迟（下发 → 首次拉流 → 出声）本轮**未测**：音箱拉流日志只能证明"拉了 620 KB"，
   无法从 PC 侧确证出声时刻。
5. §5 第 2 条（Enum 键被 PnP 重枚举冲掉）**未验证** —— 要验证需要重装该虚拟声卡驱动，
   属于会动系统的操作，本轮不做。
