# 阶段 4：冒烟验证 —— 「往端点播 1 kHz，从 UDP 收包并验证峰值 > 0」结果报告

- 任务：task-11「自签虚拟音频驱动：让『妙播』出现在 Windows 声音输出列表（WDK + Scream 派生）」
- 作者：teammate `vad-researcher`；本报告写于 2026-10-17 23:1x–23:2x（Asia/Shanghai），系统 Windows 11 26100 / AMD64 / 本机 LAN `192.168.1.4`
- 在本阶段的写作用域：`miplay-bridge/drivers/**` + 本文件（`research/vad/driver-smoke.md`）
- 前置阶段：`wdk-setup.md`（阶段 1）、`driver-build.md`（阶段 2）、`driver-install.md`（阶段 3）

---

## 0. TL;DR（先说结论，不粉饰）

**端到端冒烟未能在真机完成，原因不在冒烟本身，而在阶段 3 的前置条件不成立：自签驱动能装入、不能被内核加载**（`StartService 失败 577` / PnP `Problem 0xc0000428`，决定性证据见 `driver-install.md` §2.4），因此系统里**根本不存在「妙播」渲染端点**（实测：渲染端点候选里只有 7 个非妙播设备，见 §3.1 负例输出）。

但冒烟链路上**除「内核驱动把 PCM 变成 UDP 包」这一环以外的每一段，都已用真机实测独立打通**：

| 环节 | 状态 | 证据 |
| --- | --- | --- |
| ① 往「非默认的虚拟渲染端点」播 1 kHz 正弦（WASAPI 共享模式 + 自动重采样） | ✅ 实测通过 | §3.1：`扬声器 (Steam Streaming Speakers)`，exit 0，`wasapi-autoconvert` 44100→48000 |
| ② 选择器能正确匹配/拒绝设备（端点不存在时给出候选清单并 exit 1） | ✅ 实测通过 | §3.1 负例 |
| ③ Scream 线上格式（5B 头 + 1152B PCM）编码/解码、频率测量 | ✅ 实测通过 | §3.2：`peak=30000`、`measured 1000.0 Hz`、50/50、84/84、334/334 |
| ④ 组播接收可与第二个接收者并存 | ✅ 实测通过 | §3.2 |
| ⑤ 端点显示名 = INF 里的 DeviceDesc（已改为「小米妙播设备」） | ⛔ 未能真机确认（驱动装不上） | §5 |
| ⑥ 端点把播放的 PCM 发成 UDP 组播 | ⛔ 完全未验证（就是驱动本身） | 无 —— 驱动未加载，端点不存在 |

**要拿到 ⑥ 的证据，唯一的路是先解决驱动签名/加载问题**（三条候选见 `driver-install.md` §7：开 test signing、EV 证书 + attestation 签名、改用已签名第三方 VAD），**这不是冒烟阶段能绕过的**。本报告把「一旦驱动能加载，照抄即可跑完」的完整流程与判定标准固化在 §4。

---

## 1. 判定标准（预先登记，避免事后挪靶）

| # | 检查项 | 期望值 | 测量方法 | 本次结果 |
| --- | --- | --- | --- | --- |
| 1 | 驱动设备加载 | `Get-PnpDevice -InstanceId 'ROOT\MEDIA\0001'` → `Status=OK` | PowerShell | ⛔ 设备不存在（驱动未加载） |
| 2 | 端点出现在 Render 列表 | 出现 `扬声器 (小米妙播设备)` | `endpoint-tone` 候选清单 / `endpoint-snapshot.ps1` | ⛔ 未出现 |
| 3 | 端点计数 | 26 → 27 | `(Get-ChildItem 'HKLM:\...\MMDevices\Audio\Render').Count` | ⛔ 仍 26 |
| 4 | 播 1 kHz 时收到 UDP | `packets > 0`、`peak > 0` | `scream-udp-capture.py` | ⚠ 合成流 PASS；驱动流未测 |
| 5 | 实测频率 | 1000 Hz ±3 % | `zero_cross_hz()`（过零率） | ⚠ 合成流 1000.0 Hz（误差 0.00 %） |
| 6 | 名字 | 显示 `扬声器 (小米妙播设备)` | 同 #2 | ⛔ 未测 |

> 判定阈值说明：频率用**过零率**测量，对 1 kHz 正弦在 48 kHz 采样下每周期 48 个采样点，整数过零计数误差 ≤1 个周期 → 容差 3 % 足够宽松，不会把「基本正确」误判为失败。

---

## 2. 为什么现在跑不了（阶段 3 结论的时间锚点）

- 2026-10-17 23:04–23:07 安装：`pnputil /add-driver Scream.localized.inf /install` 成功（驱动**装入**路径通），`devcon install Scream.inf *Scream` 成功创建设备；但驱动**启动**被内核代码完整性拒绝：
  - `sc start Scream` → `StartService 失败 577`（`ERROR_INVALID_IMAGE_HASH`，即 `0xC0000428`「Windows 无法验证此文件的数字签名」）
  - 设备 PnP `Problem = 0xc0000428`，`Status = Error`
  - 证书已进 `Trusted Root` + `Trusted Publisher`、实时 `signtool verify /pa /kp /cat` 曾返回成功，仍被拒 → 证明**这不是信任链问题，而是内核 CI 不接受自签的即时生效**（Windows 10 1607 后的策略）
- 2026-10-17 23:09–23:13 已完整回滚（`install.ps1 -Uninstall` + 手工清残留）：Scream 服务/驱动/证书/设备全部移除，26 个 render 端点与基线一致（`endpoints-before-driver.json` vs `endpoints-after-rollback.json`）。
- 因此本阶段的冒烟**没有可放音的端点**；报告如实记录这一点，并把后续步骤写成可照抄的流程。

---

## 3. 已完成的离线验证（附原始输出）

### 3.1 播放侧：新增「只放音」工具，并在真实虚拟端点上实测通过

**为什么必须新增工具**：仓库里现成的 `miplay-audio --bin loopback` **不能**用于本用途——它在 `crates/miplay-audio/src/bin/loopback.rs:119` 先 `AudioCapture::new(cfg)?` 建立**采集**流，然后才播放；Scream 派生驱动只暴露 render pin（无 capture pin），因此该 bin 会直接失败、根本不放音。底层 API 是公开的：`pub fn play_sine(device: &DeviceSelector, freq_hz: f64, seconds: f64, amplitude: f32) -> Result<PlaybackInfo>`（`crates/miplay-audio/src/playback.rs:180`）。

**新增文件**（全新，位于本阶段写作用域内）：

- `drivers/tools/endpoint-tone/Cargo.toml` —— 独立包（带空 `[workspace]` 表，避免被根 workspace 收编），依赖 `miplay-audio = { path = "../../../crates/miplay-audio" }`
- `drivers/tools/endpoint-tone/src/main.rs` —— 安全设计：**拒绝默认设备**，必须显式给 `--name`/`--id`（避免验证过程中往用户当前默认播放设备灌声音，这是 Lead 的硬约束）

构建与 CLI 自检（2026-10-17 23:22:14）：

```
cargo build --release --manifest-path drivers\tools\endpoint-tone\Cargo.toml
    Finished `release` profile [optimized] target(s) in 11.89s
target\release\miplay-endpoint-tone.exe   256000 字节
& miplay-endpoint-tone.exe --help      → 正常打印用法
& miplay-endpoint-tone.exe --freq 1000 → 参数错误：必须显式指定 --name 或 --id…  exit=2
```

**实测 1（负例：端点不存在，2026-10-17 23:22:35）** —— 证明选择器解析与报错链路可用：

```
[1792250555180] 选择器: 名字含 "不存在的妙播端点"
...
[1792250555288] 播放失败: 找不到匹配的设备：名字含 "不存在的妙播端点"（render 方向）；
  本机候选：CG345UK (NVIDIA High Definition Audio) [render] {{0.0.0.00000000}.{06283053-...}} /
  X41 (NVIDIA High Definition Audio) [render] {{0.0.0.00000000}.{0e41dd27-...}} /
  扬声器 (Steam Streaming Speakers) [render] {{0.0.0.00000000}.{4a82bbbc-...}} /
  扬声器 (Steam Streaming Microphone) [render] {{0.0.0.00000000}.{4f24a0b8-...}} /
  扬声器 (UU远程虚拟音频设备) [render] {{0.0.0.00000000}.{901db3c5-...}} /
  耳机 (Chu2 DSP) [render] {{0.0.0.00000000}.{de8c89d1-...}} /
  扬声器 (Realtek High Definition Audio) [render] {{0.0.0.00000000}.{f4dd190e-...}}
exit=1
```

（顺带确认：候选里**没有**「妙播」端点 —— 驱动确实没加载。）

**实测 2（正例：播到一个真实的虚拟渲染端点，2026-10-17 23:22:24）** —— 选择 `Steam Streaming Speakers`（本机已存在的虚拟 render 端点，**无物理输出、不外放**，因此不会打扰用户；它与我们的「妙播」端点在 WASAPI 层面同类）：

```
[1792250544214] 选择器: 名字含 "Steam Streaming Speakers"
[1792250544214] 正弦: 1000.0 Hz / 1.00 s / amp 0.20（格式 44100/2ch/16，由播放层重采样到端点格式）
[1792250544515] 开始播放
[1792250546515] 播放完成: 扬声器 (Steam Streaming Speakers) [{0.0.0.00000000}.{4a82bbbc-667f-4a9b-b4ce-520a165e6240}]
  混音格式 48000 Hz / 2 ch / 32-bit float (tag=0xFFFE, extensible, F32)
  ← 播放 44100 Hz / 2 ch / 16-bit PCM (1.000s, 44100 帧, wasapi-autoconvert)
exit=0
```

**这段输出的技术含义**（我核实）：用户态通过 WASAPI 共享模式成功打开了一个**非默认**的虚拟渲染端点，`play_sine` 内部做 44100/2ch/16 → 端点混音格式 48000/2ch/f32 的自动重采样（`wasapi-autoconvert`）。即：**「把 1 kHz 正弦送进虚拟端点」这件事在用户态已经证明可行**，缺的只是端点背后有我们的驱动。

### 3.2 接收侧：合成 Scream 流自测（含一个被修掉的「丢包」假象）

工具：`drivers/scream-udp-selftest.py`（构造 5 字节头 + 1152 字节 PCM 的合成流、按 1 ms 节拍发到 `239.255.77.77:4010`，再用自己的 socket 收回来验头/验峰值/验频率）与 `drivers/scream-udp-capture.py`（纯接收端，可 `--json` 落盘）。

**PASS 结果（2026-10-17 23:23–23:24）**：

```
$ python drivers\scream-udp-selftest.py --seconds 0.3
sent=50  received=50  pcm_bytes=57600
header -> {'sample_rate': 48000, 'bits': 16, 'channels': 2, 'channel_mask': 3}
peak=30000  rms=21212.7  measured=1000.0 Hz  (expected 1000 Hz, error 0.00%)
  [PASS] header sample_rate == 48000     [PASS] header bits == 16
  [PASS] header channels == 2            [PASS] all datagrams arrived
  [PASS] peak > 0 (not silence)          [PASS] measured tone within 3% of 1000 Hz
RESULT: PASS                                        → exit=0

$ python drivers\scream-udp-selftest.py --seconds 0.5   → sent=84  received=84   RESULT: PASS  exit=0
$ python drivers\scream-udp-selftest.py --seconds 2.0   → sent=334 received=334  RESULT: PASS  exit=0
```

**修复记录（重要，避免后人踩同一个假象）**：修 `SO_RCVBUF` 之前，`--seconds 0.5` 曾报

```
sent=84  received=57  pcm_bytes=65664
peak=30000  rms=21212.7  measured=1000.0 Hz  (expected 1000 Hz, error 0.00%)
RESULT: FAIL (all datagrams arrived)
```

我当时怀疑是协议/组播问题，实测根因**在测试工具自身**：`--seconds 0.5` 一次爆发 84×1157 ≈ 97 KB，超过 socket 默认 64 KiB 接收缓冲（脚本是「先全发完、再开始收」），于是尾部丢包。修法：在 `drivers/scream-udp-capture.py:103` 的 `open_receiver()` 里**在 `bind()` 之前**设置 `SO_RCVBUF = 4 MiB`（Windows 只在 bind 前设置才生效），修后 84/84、334/334 全到达。**注意：这纯粹是测试脚本的缓冲问题，不是驱动的行为**——真机验证时接收端应在运行中持续读取（我们的守护进程会这么做）。

**双接收者并存验证**（2026-10-17 23:24:28–23:24:42）：先起 `scream-udp-capture.py --seconds 12 --json …`，在其窗口内连打 3 次 0.3 s 合成流；接收端 JSON 实测：

```json
{"group":"239.255.77.77","port":4010,"iface":"192.168.1.4","seconds":12.658,
 "packets":50,"payload_bytes":57600,"bits":16,"channels":2,"sample_rate":48000,
 "channel_mask":3,"peak":30000,"peak_dbfs":-0.77,"rms":21212.7,"measured_hz":1000.0,
 "first_datagram_hex":"00 10 02 03 00 00 00 00 00 4b 0f 4b 0f 54 1e 54 1e d8 2c …",
 "expect_hz":1000.0,"expect_hz_error_pct":0.0}
RESULT: 50 datagrams, 57600 PCM bytes, peak=30000, measured 1000.0 Hz (expected 1000 Hz)
```

同窗口内三次自测各自也报 `sent=50 received=50 → PASS`。含义：**组播允许多个接收者同时加入**（一个独立接收进程 + 另一个进程内的接收 socket 都解出了同一段流）→ 产品守护进程与调试工具可以并存监听，不需要抢端口。（本次捕获窗口实际覆盖了其中的爆发段，故计数为 50；三次爆发的完整抓取未逐帧对齐，不作更强断言。）

**本阶段产生的证据文件**（均在 `drivers/out/evidence/`）：

| 文件 | 内容 |
| --- | --- |
| `smoke_selftest.log` | 三档 `--seconds` 全 PASS 的原始输出 |
| `smoke_capture.json` | 独立接收端 JSON（双接收者并存那次） |
| `smoke_capture.log` | 早期 0 包样本（job 启动晚于爆发，非协议问题） |
| `smoke_capture2.log` | `Start-Process` 在本机因 `NO_PROXY/no_proxy` 重复键失败的空样本（工具环境限制，记录备查） |

---

## 4. 驱动一旦能加载：照抄即可跑完的冒烟流程

### 4.1 前置断言（必须全绿才继续；任一红则停）

```powershell
# ① 驱动设备加载正常
Get-PnpDevice -InstanceId 'ROOT\MEDIA\0001' | Select-Object Status, Problem, FriendlyName, InstanceId
#    期望：Status=OK、Problem=CM_PROB_NONE(0)

# ② 端点计数 26 → 27
(Get-ChildItem 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Render').Count

# ③ 端点名字（用我们的选择器看候选清单最省事）
& target\release\miplay-endpoint-tone.exe --name '妙播' --seconds 0.1 --delay-ms 0   # 不该报「找不到匹配的设备」
#    期望候选里出现：扬声器 (小米妙播设备)
```

### 4.2 起接收端（必须先起，再放音）

```powershell
$py = 'C:\Users\Administrator\AppData\Roaming\uv\python\cpython-3.12.11-windows-x86_64-none\python.exe'
& $py drivers\scream-udp-capture.py --seconds 20 --expect-hz 1000 --iface 192.168.1.4 `
      --json drivers\out\evidence\smoke_driver_udp.json --verbose
```

（`--iface 192.168.1.4` 是本机 LAN 地址；Scream 驱动默认从它绑定的接口发组播，若驱动绑定到别的接口，用 `--iface 0.0.0.0` 或列出接口再试。）

### 4.3 放音（另一个窗口；`--delay-ms` 留给接收端启动）

```powershell
$env:CARGO_TARGET_DIR = "$PWD\target"
cargo run --release --manifest-path drivers\tools\endpoint-tone\Cargo.toml -- `
      --name 妙播 --freq 1000 --seconds 5 --amp 0.4 --delay-ms 2000
```

### 4.4 判定

| 检查 | 期望 | 说明 |
| --- | --- | --- |
| 捕获 JSON `packets` | > 0（5 s 播放 ≈ 430 个包 @48 kHz/1152 帧一包） | 0 包 = 驱动没发/组播接口不对/端点没被真正写入 |
| `peak` | > 0（0.4 幅度 ≈ 13107） | 0 = 静音（驱动跑了但没数据） |
| `measured_hz` | 1000 ± 30 Hz | 与 §1 #5 同口径 |
| `sample_rate/bits/channels` | 48000 / 16 / 2 | 驱动固定格式 |
| 播放侧 exit code | 0，且 `PlaybackInfo` 里是「妙播」端点 | 若仍报「找不到匹配的设备」，回到 4.1 |

可能的正常偏差（先按此解释，再怀疑驱动）：首次写入前端点可能被 audiosrv 保持静音（Scream 的 `StartType=3` 是 demand，首次写端点才拉起服务）；`--amp 0.4` 下 16-bit 峰值约 13107，若用 0.2 则约 6553。

### 4.5 清理（验证完必须做）

```powershell
# 卸载驱动 + 删服务 + 删设备（幂等，见 install.ps1 -Uninstall）
powershell -NoProfile -ExecutionPolicy Bypass -File drivers\install.ps1 -Uninstall
# 需要的话连测试证书一起清（-KeepCertificate 可保留）
```

---

## 5. 端点改名：INF 侧已就位，真机待验证

- 生成脚本：`drivers/make-localized-inf.py` → `drivers/out/Scream.localized.inf`，把上游 `Scream.DeviceDesc="Scream (WDM)"` 改成 **`小米妙播设备`**，并按 INF 本地化规则写成 UTF-16LE + `[Strings.0804]`（校验方式：读前两字节应为 `FF FE`）。
- 显示名规则（由 task-8 `rename-probe.md` 的真机实测确立）：Windows 渲染端点的显示名形如 **`扬声器 (设备FriendlyName)`**，「扬声器」是 speaker 端点硬编码前缀，**不可去掉**；我们能控制的只有括号内内容 → 最终形态 **`扬声器 (小米妙播设备)`**。
- 因此**换名 = 改 INF 里的 `Scream.DeviceDesc` → 重新生成 INF → 重装驱动**（PNP 重新枚举设备、读取新描述符）。**不需要**改 `MMDevices\Audio\Render\...\Properties` 注册表，也不需要重启 audiosrv。
- 想要「妙播 - 卧室立体声组合」这种**每箱一名**：需要每个音箱一个**独立驱动实例/端点**（多实例）。task-11 按用户决定只做 **1 个总端点**，多实例留给后续架构（方案见 `vad-options.md` §最终架构）。

---

## 6. 未做 / 未验证清单（诚实边界）

| 项 | 状态 | 原因 |
| --- | --- | --- |
| 端点把 PCM 发成 UDP（冒烟的核心 ⑥） | ⛔ 未验证 | 驱动无法加载（阶段 3） |
| 端点显示名真机确认 | ⛔ 未验证 | 同上（INF 已改好并可复现生成） |
| 26 → 27 端点计数 | ⛔ 未验证 | 同上 |
| 播放时的端到端延迟/抖动 | ⛔ 未测 | 同上 |
| 多音箱多端点 | ⛔ 未做 | 用户决定 v1 只做 1 个总端点 |
| 本机 `Start-Process` 可用性 | ⚠ 受限 | PowerShell 5.1 遇 `NO_PROXY`/`no_proxy` 同时存在时抛 `ArgumentException`；用后台 job 或前台顺序执行替代 |

---

## 7. 复现命令汇总（本次实际执行过的）

```powershell
# 构建只放音工具
$env:CARGO_TARGET_DIR = 'D:\Backup\Documents\deepseek-harness\default-workspace\miplay-bridge\target'
cargo build --release --manifest-path drivers\tools\endpoint-tone\Cargo.toml

# 播放侧负例 / 正例
& target\release\miplay-endpoint-tone.exe --name '不存在的妙播端点' --seconds 1 --delay-ms 100   # exit=1
& target\release\miplay-endpoint-tone.exe --name 'Steam Streaming Speakers' --freq 1000 --seconds 1 --amp 0.2 --delay-ms 300   # exit=0

# 接收侧自测（三档）
& $py drivers\scream-udp-selftest.py --seconds 0.3   # PASS 50/50
& $py drivers\scream-udp-selftest.py --seconds 0.5   # PASS 84/84（修 SO_RCVBUF 后）
& $py drivers\scream-udp-selftest.py --seconds 2.0   # PASS 334/334
```

**本阶段新增/修改的文件**（全部在写作用域内）：

- 新增 `drivers/tools/endpoint-tone/{Cargo.toml, src/main.rs}`（只放音的正弦工具；拒绝默认设备）
- 修改 `drivers/scream-udp-capture.py`（`open_receiver()` 在 bind 前设 `SO_RCVBUF=4 MiB`，并留下注释说明原因与实测数字）
- 新增 `drivers/out/evidence/{smoke_selftest.log, smoke_capture.json, smoke_capture.log, smoke_capture2.log}`
- 新增本文件 `research/vad/driver-smoke.md`

未提交任何 git commit（按任务要求，由 Lead 统一提交）。

---

## 8. 收尾状态核验（2026-10-17 23:27，本机实测，全部「OK」）

阶段 3 的安装痕迹在阶段 4 开始前就已回滚；本阶段结束前又整体复查一次，确认环境与基线一致、**未开启 test signing、未触碰任何已有音频设备**：

| 检查 | 命令/方法 | 结果 |
| --- | --- | --- |
| Scream 内核服务 | `Get-Service -Name Scream` | OK：不存在 |
| 驱动包残留 | `pnputil /enum-drivers` 过滤 `scream` | OK：无相关 `oem*.inf` |
| 设备节点 | `Get-PnpDevice -InstanceId 'ROOT\MEDIA\0001'` | OK：不存在（仅剩本机原有 `ROOT\MEDIA\0000` = UU远程虚拟音频设备，Status=OK） |
| 渲染端点计数 | `(Get-ChildItem 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Render').Count` | 26（= 基线 `endpoints-before-driver.json`） |
| 测试证书 | `Cert:\LocalMachine\{Root,TrustedPublisher}` 中最近 1 天新增 | OK：均为 0 |
| 驱动文件 | `C:\Windows\System32\drivers\Scream.sys` | OK：不存在 |
| **test signing** | `bcdedit /enum {current}` | OK：**输出中无 testsigning 行**（未启用，符合用户「不开 test signing」的授权边界） |

即：本次任务对系统**没有留下任何需要用户手动清理的残留**；后续若用户批准某条签名路线，从 §4 的 4.1 开始即可，不必先做清理。

