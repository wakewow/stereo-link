# findings.md —— 零驱动兜底 PoC 实测记录

任务：task-12（零驱动兜底 PoC）。写作用域：`miplay-bridge/poc/zero-driver/`。
实测环境：Windows 10.0.26100 AMD64（Administrator，High 完整性级别），
LAN IP `192.168.1.4`，采集 venv
`D:\Backup\Documents\deepseek-harness\default-workspace\mixplay-pc-audio\capture\.venv\Scripts\python.exe`
（`soundcard` + `numpy`，**没有** `miservice`）。

> 状态标记约定：**【已实测】**= 本机跑过并留下证据文件；**【待执行】**= 等 lead 的 go（task-9 冲突）；
> **【未验证】**= 本机条件不具备，未跑过。

---

## 0. 结论速览

| 环节 | 状态 | 客观证据 |
|---|---|---|
| 端点改名机制（设备层 `FriendlyName` + disable/enable） | **【已实测】**（task-8，四态表） | `research/vad/rename-probe.md`：改名前 `扬声器 (Steam Streaming Speakers)` → 写值后端点名不变 → 重启设备后 `扬声器 (小米妙播设备)` → 回滚后逐字节复原 |
| `rename-endpoint.ps1`（本目录，参数化 + `-Rollback`） | **【已实测】**（只读三验证） | §2：`-DryRun` / `-BackupOnly` / `-Rollback -DryRun` 全 PASS |
| 本目录里的**真实改名 + 回滚** | **【待执行】** | 等 lead 的 go（task-9 正在用音频端点跑测试） |
| 按名字子串选端点 + WASAPI loopback 采集 | **【已实测】** | §3.1：peak −20.68 dBFS / RMS −24.04 dBFS，丢帧警告 0 |
| 裸 PCM → WAV HTTP 无限流 | **【已实测】** | §3.2：写入 4.000 s，客户端读到 3.071 s，WAV 头校验通过 |
| 采集→HTTP 合并链路 | **【已实测】** | §3.3：写入 2.972 s，逐块电平稳定，0 丢帧警告 |
| 控制通道探活（MiXPlay HTTP） | **【已实测】** | §4.1：`--check` 退出码 2，`ConnectionRefusedError(10061)` |
| **音箱真出声（端到端）** | **【待执行】** | 需 ① MiXPlay 桌面端在跑 + ② 等 lead 的 go 做改名 |
| `--backend mina`（`miservice.MinaService.play_by_url`） | **【未验证】** | 本 venv 无 `miservice`，需要带凭据的环境 |

---

## 1. 改名机制：客观事实（来自 task-8 的四态实测，本目录脚本沿用同一手法）

审计的四个观测点（注册表直读 / SetupAPI / `Get-PnpDevice` 设备+端点 / 新进程 soundcard 枚举）
在每一态上互相印证：

| 态 | 设备层 `FriendlyName` | `Get-PnpDevice` 设备名 | Windows 声音列表里的端点名 | 注册表 `{b3f8fa53-…},6` |
|---|---|---|---|---|
| 改名前 | 值**不存在** | `Steam Streaming Speakers` | `扬声器 (Steam Streaming Speakers)` | `Steam Streaming Speakers` |
| 写值后（未重启设备） | `小米妙播设备` | `小米妙播设备` | `扬声器 (Steam Streaming Speakers)`（AEB 缓存未刷） | 未变 |
| disable/enable 后 | `小米妙播设备` | `小米妙播设备` | **`扬声器 (小米妙播设备)`** | `小米妙播设备` |
| 回滚 + 重启后 | 值**不存在** | `Steam Streaming Speakers` | `扬声器 (Steam Streaming Speakers)` | `Steam Streaming Speakers` |

要点：
* 生效的关键是 **disable/enable**（刷 AudioEndpointBuilder 的接口 FriendlyName），
  只写注册表不重启设备是看不到变化的。
* 端点名的构成是 `驱动 pin 名 + " (" + 接口 FriendlyName + ")"`。
  **`扬声器` 这个前缀来自驱动的 `{a45c254e-…},2`，零驱动无法覆盖**——这是本路线的天花板。
* 反面：`MMDevices\Audio\Render\<guid>\Properties` 是死路（`reg add` 报
  `ERROR: Access is denied.`，COM `IPropertyStore::SetValue` 报 `0x80070005 E_ACCESSDENIED`），
  本 PoC **完全不碰**它。

---

## 2. `rename-endpoint.ps1` 只读验证（本目录，真实改名前）

```
powershell -NoProfile -ExecutionPolicy Bypass -File .\rename-endpoint.ps1 `
    -DeviceInstanceId 'ROOT\STEAMSTREAMINGSPEAKERS\0000' -NewName '小米妙播设备' -DryRun
```

| 用例 | 结果 | 证据 |
|---|---|---|
| 干跑（`-DryRun`） | ✅ 只读：不写文件/注册表、不重启设备，逐条打印将要执行的写操作；exit 0 | 终端输出 |
| 只备份（`-BackupOnly`） | ✅ `.reg` **1706 B** + `.meta.json` **4295 B** | `backups/backup-Enum-ROOT_STEAMSTREAMINGSPEAKERS_0000-Steam_Streaming_Speakers-20261017-230025.reg` / `.meta.json` |
| 回滚干跑（`-Rollback <reg> -DryRun`） | ✅ 打印恢复项与逐项校验项，未写入 | 终端输出 |

备份内容（`reg export` 语义，实测）：含实例键下的 `DeviceDesc`、`Service`、`ClassGUID`、
`Capabilities`、`ConfigFlags`、`ContainerID`、`HardwareID`、`CompatibleIDs`、
`Driver`、`LocationInformation`、`Manufacturer`、`Class`、`Icon`、`MatchingDeviceId`、
`ContainerID` 等全部值，**以及 `Device Parameters` 子键**（`reg export` 默认递归）；
**不含** `Properties` 子键——因为 ACL 不允许枚举它（这也是 §1 那条死路的直接证据）。

---

## 3. 采集 / HTTP 链路（现有端点代跑，真实数字）

### 3.1 采集端点 + loopback

```
& $py .\capture_from_endpoint.py --endpoint-name 'Steam Streaming Speakers' \
      --seconds 3 --play-tone 1000 --evidence .\backups\selftest-capture-steam.json
```

| 指标 | 数值 |
|---|---|
| 端点 | `扬声器 (Steam Streaming Speakers)`（`loopback=True`） |
| 格式 | 44100 Hz / 2ch / s16le |
| 采集时长 | 3.019 s（130 blocks） |
| 峰值 | **−20.68 dBFS** |
| RMS | **−24.04 dBFS** |
| 丢帧警告 | **0** |

判据（来自 `research/vad/loopback-matrix.md`）：数字自环 ≥ −60 dBFS；
真实麦克风收到的**声学串音**在 −86.97 ~ −106.72 dBFS。差 50+ dB，
所以 −20.68 dBFS 是**确定性数字通路**，不是麦克风拾音。

### 3.2 WAV HTTP 服务（`--source tone`，不碰音箱）

```
& $py .\push_to_speaker.py --source tone --seconds 4 --local-selftest --port 8932
      --evidence .\backups\selftest-push-tone.json
```

| 指标 | 数值 |
|---|---|
| 服务端写入 | 705600 B = **4.000 s**（目标 4.0 s） |
| 客户端读到 | 541760 B = **3.071 s** |
| WAV 头 | `52494646 24ffff7f 57415645 666d7420 …` = `RIFF` + size `0x7FFFFF24` + `WAVE` + `fmt ` ✅ |
| `data` chunk size | `0x7FFFFF00` ≈ 2 GiB（**故意**：无限流的写法，照 `pc_stereo_hub.py`） |
| 证据文件 | `backups/selftest-push-tone.json` |

客户端少读约 1 秒是**设计**：`main()` 先 `sleep(1.0)` 垫数据，避免音箱拉到空流。

### 3.3 采集 → HTTP 合并链路

```
& $py .\push_to_speaker.py --source capture --endpoint-name 'Steam Streaming Speakers' \
      --play-tone 1000 --seconds 4 --local-selftest --port 8935
      --evidence .\backups\selftest-push-capture.json
```

| 指标 | 数值 |
|---|---|
| 写入 | 524288 B = **2.972 s** |
| 客户端读到 | 434176 B = 2.461 s |
| 每段采集峰值 | −20.68 / −20.68 / −20.68 dBFS（三段 1 s，**逐段一致**） |
| 丢帧警告 | **0** |
| 无效端点时的行为 | ✅ 明确报错 `[选择失败] 没有名字包含 '小米妙播设备' 的 capture 端点。` 并写进证据 JSON 的 `capture_error`，不静默假成功 |

### 3.4 复现踩坑（都已在脚本里处理，记录以免复踩）

* `soundcard` 的 `all_microphones(include_loopback=True)` 才有 `[loopback]` 端点；
  **capture 端点不能直接按 friendly name 精确匹配**——改名后要按**子串**选。
* 音频端点的 PnP `InstanceId` 是 `SWD\MMDEVAPI\{0.0.0.00000000}.{GUID}`，
  **不是** `ROOT\...`。改名的抓手是它背后那个设备（`{233164c8-…},1` 属性的值指向的
  `ROOT\...` 实例）。
* 后台跑 python 必须加 `-u`（否则日志缓冲到进程结束才吐）。
* 本机 `Start-Process` 有 bug（`NO_PROXY`/`no_proxy` 键冲突），一律用直接调用或后台作业。

---

## 4. 控制通道

### 4.1 探活【已实测】

```
& $py .\push_to_speaker.py --check
[探活] MiXPlay http://127.0.0.1:8820 ...
        不可达: URLError(ConnectionRefusedError(10061, '由于目标计算机积极拒绝，无法连接。'))
        -> 先启动 MiXPlay 桌面端（默认 http://127.0.0.1:8820）
exit=2
```

**客观事实：本机当前 MiXPlay 桌面端没有运行**，所以 §0 表里"音箱真出声"必然还没跑。
脚本已实现的通道是 `POST http://127.0.0.1:8820/api/v1/control/play`
`{"action":"play_url","target":"<did>","url":"http://<lan>:<port>/audio.wav"}`
（MiXPlay 内部走小米云 `player_play_url`，等价于 "play_by_url"）。

### 4.2 目标与安全边界【已实现】

* 默认目标 `did=934947888`（Xiaomi Sound，`192.168.1.171`）。
* **保留目标 `did=501718792`（`192.168.1.21`）**：脚本默认**拒绝**，要覆盖必须显式 `--force`
  （那是 task-10 AirPlay 测试的目标）。
* `--seconds` 默认 8 s，超过 10 s 自动截到 10 s（除非 `--force`）。
* 音量默认不动；`--volume` 显式传才设。

---

## 5. 这条路线作为产品路径的不可靠点（**≥3 条，均为具体机制**）

1. **名字带着 `扬声器` 前缀，零驱动去不掉**——前缀来自驱动 pin 名（`{a45c254e-…},2`），
   最终只能得到 `扬声器 (小米妙播设备)`，做不到产品要的 `妙播 - 卧室立体声组合`。
2. **写入位置是 PnP 管理的设备缓存键**（`HKLM\SYSTEM\CurrentControlSet\Enum\...`）：
   设备重装、驱动更新、`pnputil` 重新枚举、系统还原都可能冲掉 `FriendlyName`，
   而每次刷回去都要再 disable/enable 一次。
3. **生效必须 disable/enable**：该端点在系统里**消失再出现**，会打断正在播放的音频，
   并让正在使用它的进程（包括我们自己的采集线程）拿到设备丢失错误。
4. **一台机器只能有一个「小米妙播设备」**：手法是改**已有设备**的名字，
   而一个虚拟声卡实例 = 一个 render 端点 + 一个 capture 端点；
   要"每个妙播设备一个 Windows 输出设备"必须有 N 个独立虚拟声卡实例——那正是要驱动的部分。
5. **需要 Machine 级注册表写权限 + 提权**，且这是未文档化 hack：
   微软对端点属性的官方立场是 "Clients can read these properties, but should not set them."
   （`research/vad/vad-options.md` §8.1）。
6. **多一跳网络**：音箱自己来拉 HTTP，起播时机与缓冲不受控；本机 HTTP 服务也无鉴权。

→ 完整讨论与用法见 `README.md` §6。

---

## 6. 还要做什么

1. **等 lead 的 go** → 执行 `rename-endpoint.ps1` 真实改名（只改
   `ROOT\STEAMSTREAMINGSPEAKERS\0000` 这个未被占用的端点）→ 记录改名前/后/回滚后三态
   → 复跑 §3.3 的采集链路确认端点名变化后一切照旧 → 跑端到端出声 → **回滚并验证无残留**。
2. **需要 MiXPlay 桌面端在跑**（或换一个有 `miservice` + 凭据的 python 环境）。
   这是"音箱出声"唯一的当前阻塞。
3. 端到端出声后补：HTTP 请求日志（客户端 IP / UA / 拉取字节数）、
   音箱端可观测的 `result=True`、以及**端到端延迟**（下发 → 首次拉流 → 出声明）。
   人耳确认由 lead 组织。
