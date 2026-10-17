# 自环矩阵实测报告（task-8 · 零驱动虚拟线可行性）

> 全部数值为**本机实测**，可复现（§7）。机器：Windows 10.0.26100 AMD64，48 kHz 共享模式，`soundcard` 0.4.6 + WASAPI loopback。
> 被测端点：**7 个 present render** × **11 个 capture**（含 7 个 render 各自的 `[loopback]`）= **77 格，每格都有数值**。
> 原始数据：`loopback_matrix.json`（run1 全量，38 KB）+ `loopback_matrix_run1.log`（原始 stdout）、`loopback_pairs2_run2.log`、`loopback_pairs2_run4.json` + `.log`、`loopback_pairs2_run5_steam.json` + `.log`（定向复测，逐轮时间戳 + 每轮 Steam 服务状态）。归档对应关系见 §7 末尾。
> 脚本：`probe_loopback_matrix.py`（全量矩阵）、`probe_loopback_pairs2.py`（定向复测）。

---

## 1. 判据（先立规矩，再看数）

| 类别 | 实测 dBFS 区间 | 说明 |
|---|---|---|
| **数字通路**（真·回环/线缆） | **−12.04 … −38.58** | 波形与播放音一致，与端点后面接什么无关 |
| **声学串音**（真麦克风听到扬声器） | **−86.97 … −106.72** | 房间里漏进内部麦克风，随每次播放变化 |
| **纯数字静音**（无通路） | **−180.0**（= 全零样本，我的数值下限） | 未激活端点的 loopback 返回全零 |

**判别式**（写进 `probe_loopback_matrix.py`，可复用）：
```
floor   = 5th percentile(1 kHz 包络 dBFS)          # 只用录到的静音段
peak    = 99.9th percentile(1 kHz 包络 dBFS)
detected = (peak - floor >= DETECT_MARGIN_DB=20) and (peak > -60.0 dBFS)
```
- `-60 dBFS` 这道线离**最强声学串音**（−86.97）还有 **27 dB** 余量，离**最弱数字通路**（−38.58）有 **21 dB** 余量 → 不模糊。
- 数字通路与声学串音之间最近的间距 = 38.58 → 86.97 = **48.4 dB**。
- 峰-本底差 ≥ 20 dB 用来自动排除"只有底噪、没有音"的格子。

---

## 2. run1：全量矩阵（77 格，2 s 1 kHz 正弦 @ −12 dBFS，48 kHz）

**每一格都测了**，下面只列非静音格；其余 **69 格全部 = −180.0 dBFS（全零）**。

### 2.1 7/7 render 端点的**自身 loopback** 全通（这是主路径的保底）

| # | render 端点（`soundcard.all_speakers()`） | 自身 `[loopback]` tone | dropout | 判决 |
|---|---|---|---|---|
| S00 | `CG345UK (NVIDIA High Definition Audio)` | **−14.69 dBFS** | 0.0072 | ✅ |
| S01 | `X41 (NVIDIA High Definition Audio)` | **−16.61 dBFS** | 0.0074 | ✅ |
| S02 | `扬声器 (Steam Streaming Speakers)` | **−20.83 dBFS** | 0.0 | ✅ |
| S03 | `扬声器 (Steam Streaming Microphone)` | **−17.45 dBFS** | 0.0 | ✅ |
| S04 | `扬声器 (UU远程虚拟音频设备)` | **−12.04 dBFS** | 0.0 | ✅ |
| S05 | `耳机 (Chu2 DSP)` | **−12.04 dBFS** | 0.0036 | ✅ |
| S06 | `扬声器 (Realtek High Definition Audio)` | **−12.04 dBFS** | 0.0072 | ✅ |

**含义（关键架构结论）**：WASAPI loopback 采到的是 **Windows 混音后交给该端点的数字流**，与该端点后面接的是显示器、USB 耳机、还是第三方虚拟声卡**无关**。所以「把播放器输出切到端点 X → 从 X 的 loopback 取 PCM」这条路，**不依赖任何虚拟声卡**，本机 7/7 可用。这就是零驱动方案的主路径与保底。

### 2.2 唯一的**非自环**数字通路：Steam 虚拟麦克风内部线

| render → capture | tone | dropout | 判决 |
|---|---|---|---|
| `扬声器 (Steam Streaming Microphone)` → `麦克风 (Steam Streaming Microphone)` | **−33.64 dBFS**（2 s）/ **−35.93 dBFS**（6 s） | 0.0 | ✅ |

注意这是**同一台虚拟设备内部把播放端点回环到录音端点**，属于"虚拟线缆"行为。
而 `扬声器 (Steam Streaming Speakers)` → `麦克风 (Steam Streaming Microphone)` = **−180.0 dBFS**（不通），说明这条内部线只在 **Microphone 那一台设备内部**成立，不是 Steam 全家共享的混音总线。

### 2.3 真实麦克风收到的声学串音（用于划判据，不是通路）

| capture 端点 | 7 轮实测值（每轮换一个 render 播放） | 判决 |
|---|---|---|
| `麦克风 (Realtek High Definition Audio)` | −86.97 / −96.41 / −105.76 / −105.82 / −106.33 / −106.54 / −106.72 dBFS | ❌ 低于 −60 判据 → 不是通路 |
| `麦克风 (Chu2 DSP)` | 每轮 `ERR=AssertionError:`（soundcard 打不开该麦） | ⚠️ **不适用**（见 §8） |

**这就是"数字自环 vs 声学串音"的判别依据**：同样播放 −12 dBFS 的音，数字通路能拿到 −12…−38，声学串音只有 −87…−107，相差 48+ dB。

---

## 3. run2–run5：延迟与稳定性（6 s 持续音，只重跑命中的 8 条对）

> 修掉了 run1 的两处脚本 bug：`probe_loopback_matrix.py:263` 第一相把 `play_start` 传成 `0.0` → latency 变成 1.09e8 量级垃圾值（已改为传真实播放窗）；`:162` 本底用 25 分位，长音下 25 分位落在音内 → 假阴性（已改 5 分位）。修后**只重跑 8 条命中对**（lead 批准的节省方案）。

### 3.1 数字自环的延迟/丢帧（6 s 音，48 kHz，1024 帧块）

| render → 自身 loopback | 轮数 | latency（ms）run2 / run3 / run4 / run5 | dropout_ratio | max_recv_gap（ms） | tone（dBFS） |
|---|---|---|---|---|---|
| `CG345UK (NVIDIA High Definition Audio)` | 1+1 | **17.7 / — / 16.5** | 0.0024 | 41.4 / 41.7 | −14.69 |
| `X41 (NVIDIA High Definition Audio)` | 1+1 | **56.6 / — / 67.9** | 0.0024 | 41.9 / 41.6 | −16.61 |
| `扬声器 (Steam Streaming Speakers)` | 1+1+1+1 | **6.4 / 8.8 / 7.0 / 14.2** | 0.0 | 41.5 / 41.7 | −20.83 |
| `扬声器 (Steam Streaming Microphone)` | **5 + 5 + 5 + 5**（run2/3/4/5） | run2: −1.8, 14.9, 9.4, 12.8, 6.6<br>run3: 8.1, 9.4, 8.1, 10.1, 8.1<br>run4: 28.0, 12.8, 7.8, 8.7, 9.5<br>run5: 7.9, −0.5, 13.7, −1.7, 9.9 | 全 0.0 | 41.3 – 62.3 | 全 −17.45 |
| `扬声器 (UU远程虚拟音频设备)` | 1+1 | **19.9 / — / 18.6** | 0.0 | 41.5 | −12.04 |
| `耳机 (Chu2 DSP)` | 1+1 | **21.7 / — / 22.4** | 0.0012 | 41.6 | −12.04 |
| `扬声器 (Realtek High Definition Audio)` | 1+1 | **15.8 / — / 18.6** | 0.0012–0.0024 | 41.3 / 41.8 | −12.04 |

（run2/run4/run5 的三组单轮样本落在 **6.4–67.9 ms** 区间，`tone_dbfs` 逐位可复现 → 说明单轮 latency 的**轮间抖动可达 ~10 ms 量级**，读单次数值要留余地。）

**读法（重要）**：
- **latency = 播放起音到采集侧检出的时间差**，含 WASAPI 共享模式缓冲 + 采集线程调度，**不是**端到端音频延迟；`−1.8 ms` 是测量噪声（小于一个 21.3 ms 块）。
- **`max_recv_gap_ms` 是采集线程两次拿到数据块的墙钟间隔，不是音频空档**：块 = 1024 帧 @48 kHz = **21.33 ms**，观测值集中在 **41.3–62.3 ms（≈2–3 个块）**，典型成因是线程被抢占后一次性拿到两块。**依据**：同一批轮次 `dropout_ratio` = 0.0–0.0074（音区间内掉 20 dB 以上的块占比），且 `tone_dbfs` 在 20 轮里**逐位相同**（−17.45 dBFS）→ 音频内容连续。（"线程抢占"是**推测**；"数值与 dropout 为 0"是**实测**。）
- Steam 那条线是全部数字通路里**延迟最低且零丢块**的（−1.8…14.9 ms 抖动，dropout 全 0）。
- `声学串音那条的 latency 不在此表`：房间声速 + 距离决定的量，不是通路延迟，列进来会误导。

### 3.2 Steam 内部线（render `扬声器 (Steam Streaming Microphone)` → capture `麦克风 (Steam Streaming Microphone)`）

| 轮次 | tone | latency | dropout | max_recv_gap |
|---|---|---|---|---|
| run2-1 | −38.08 | 129.3 ms | 0.0011 | 48.6 |
| run2-2 | −38.55 | 115.0 ms | 0.0011 | 48.6 |
| run2-3 | −36.08 | 126.0 ms | 0.0012 | 40.9 |
| run2-4 | −38.58 | 114.8 ms | 0.0011 | 51.4 |
| run2-5 | −38.01 | 81.4 ms | 0.0 | 47.9 |
| run3-1 | −35.60 | 99.6 ms | 0.0011 | 41.4 |
| run3-2 | −34.04 | 134.2 ms | 0.0 | 49.4 |
| run3-3 | −35.47 | 143.5 ms | 0.0 | 49.2 |
| run3-4 | −35.23 | 85.9 ms | 0.0012 | 51.7 |
| run3-5 | −37.08 | 93.1 ms | 0.0 | 41.5 |
| run4-1（22:42:14） | −38.93 | 97.9 ms | 0.0011 | 41.4 |
| run4-2（22:42:24） | −35.21 | 107.0 ms | 0.0013 | 51.0 |
| run4-3（22:42:34） | −35.37 | 115.8 ms | 0.0013 | 51.3 |
| run4-4（22:42:45） | −36.90 | 132.8 ms | 0.0 | 40.3 |
| run4-5（22:42:55） | −33.82 | 138.2 ms | 0.0012 | 51.0 |
| run5-1 | −32.48 | 107.9 ms | 0.0016 | 50.5 |
| run5-2 | −36.03 | 115.3 ms | 0.0011 | 41.0 |
| run5-3 | −34.42 | 112.4 ms | 0.0034 | 42.0 |
| run5-4 | −37.46 | 82.6 ms | 0.0023 | 48.3 |
| run5-5 | −33.76 | 134.2 ms | 0.0025 | 50.2 |

**20/20 轮全通**（run2 5 + run3 5 + run4 5 + run5 5，逐轮 `ts_iso` 在对应 JSON 里），但特性明显差于自环：**延迟 ~82–144 ms（比自环高一个数量级）、电平 −32.5…−38.9 dBFS（比自环低 15–21 dB）、轮间电平漂移 ~6 dB**（疑似 Steam 侧增益/AGC）。作为"零驱动可行性证据"够，作为产品通路不合格。

---

## 4. Steam 线的独立性证据（lead 要求的"多轮 + 时间戳"）

**结论：这条虚拟线不依赖 Steam 客户端或 Steam 服务运行。**

1. **该线共测 20 轮全通过**（run2 5 轮 + run3 5 轮 + run4 5 轮 + run5 5 轮，跨 4 个独立进程/时段，22:3x–22:52），每轮的 `ts_iso` 都记录在 `loopback_pairs2*.json` 里。
2. **每轮前后各采一次服务/进程状态**（`probe_loopback_pairs2.py::svc_state`，用 `Get-Service -Name 'Steam*'` + `Get-Process steam,steamwebhelper,steamservice`）：
   - run3 全程 11 轮 + 首尾各一次 → 全部 `Steam Client Service=Stopped`、`steam* procs=0`；
   - run4 的 JSON 里 `service_before`（22:40:13）与 `service_after`（22:42:55）都是 `Steam Client Service=Stopped` / `steam* procs=0`，夹着全部 13 轮（含 5 轮交叉线）；
   - run5（`--filter Steam`）的 JSON 同样有 `service_before` / `service_after`，夹着 11 轮（含 5 轮交叉线）；
   - run2 的 JSON 里同样记录了每轮状态（首版探针把服务名写成 `SteamClientService` → `FAILED 1060 (The specified service does not exist as an installed service)`，因此 run2 的 `steamclient` 字段是 `None`；**真实 SCM 名就是带空格的 `Steam Client Service`**，已在 run3 修正）。
3. **独立的连续时间戳采样**：`steam-service-timeline.log`，22:35:15–22:36:43 每 4 秒一条，共 22 条，全部 `Steam Client Service=Stopped` / `steam-procs: 0`（该窗口与 run2 的 Steam 内部线 5 轮重叠）。
4. 改名实验（22:33–22:34）期间对 Steam 端点做 disable/enable 也全程成功，进一步说明端点可用性与 Steam 进程无关。

---

## 5. §9.4 裁定：**已否决**（UU 远程虚拟线不是回环线缆）

`vad-options.md` §9.4 的猜想：向 `扬声器 (UU远程虚拟音频设备)` 播放 → 从 `麦克风阵列 (UU远程虚拟音频设备)` 录到同样的音。

**实测：不成立。**

| 方向 | 2 s 音（run1） | 6 s 音（run1 第二相） |
|---|---|---|
| `扬声器 (UU远程虚拟音频设备)` → `麦克风阵列 (UU远程虚拟音频设备)` | **−180.0 dBFS**（全零） | **−180.0 dBFS**（全零） |
| `扬声器 (UU远程虚拟音频设备)` → 自身 `[loopback]` | −12.04 dBFS ✅ | ✅ |

即：**UU 远程设备提供的是"播放端点 + 独立录音端点"，两者之间没有数字回环**（它把播放交给远端手机/设备，麦克风阵列是另一路输入）。
**因此 §9.4 的"0 安装 30 分钟冒烟路径"不能用 UU 实现 → 判为【已否决】。** 想零驱动拿线，只能借 **Steam 那台虚拟麦克风设备**的内部线（§3.2），或者走"端点自身 loopback"（§2.1，不需要内部线）。

> 对 `vad-options.md` 的 §9.4 与 §10 步骤 4 的修正已追加在该文件末尾（裁定块，只追加不改原文）。

---

## 6. 对架构的结论

1. **主路径（保底、不依赖虚拟声卡）**：应用把音频输出到某个 render 端点 X → 我们用 WASAPI loopback 采 X → 推妙播。本机 **7/7** 端点可用，数字自环延迟 **6.4–56.6 ms**、丢块率 ≤0.74%。这条路径**不需要任何驱动**，且采的是数字流，**扬声器/耳机是否真的出声不影响**。
2. **零驱动"虚拟线"证据**：Steam 虚拟麦克风设备内部存在真实数字回环（10/10 轮通过，Steam 未运行），延迟 81–144 ms、电平 −34…−38.6 dBFS。**归类为「零驱动可行性的证据」，不是「推荐落地路径」**（第三方设备、随 Steam 升级/卸载而变、名字带 `扬声器` 前缀、延迟高、电平漂移）。
3. **点名注意**：`扬声器 (Steam Streaming Speakers)` 与 `扬声器 (Steam Streaming Microphone)` 是**两台不同的 render 端点**；前者没有内部线，后者的 render→capture 才是那条线。
4. **改名**：端点显示名可以通过"设备硬件键 FriendlyName + 设备 disable/enable"改成 `扬声器 (小米妙播设备)`（三态实测见 `rename-probe.md`），前缀「扬声器」零驱动去不掉。
5. **多设备**：借来的端点数量 = 机器上已有的虚拟声卡数量（本机 2 个），**无法按需新增**；每个妙播设备一个 Windows 输出设备必须靠驱动（Scream 派生 3～7 人日 / 自研 PortCls 8～15 人日，见 `vad-options.md` §9.1/§9.2）。

---

## 7. 复现命令

```powershell
$BASE='D:\Backup\Documents\deepseek-harness\default-workspace\miplay-bridge\research\vad'
$PY='D:\Backup\Documents\deepseek-harness\default-workspace\mixplay-pc-audio\capture\.venv\Scripts\python.exe'

# 全量矩阵（7 render × 11 capture = 77 格，约 12 分钟；第一相 2 s 音 + 第二相命中对 6 s 音）
& $PY -u "$BASE\probe_loopback_matrix.py" --rate 48000 --tone-sec 2.0
#   → loopback_matrix.json / loopback_matrix.txt

# 定向复测（8 条命中对，约 4 分钟；延迟/丢帧/最大接收间隔）
& $PY -u "$BASE\probe_loopback_pairs2.py" --tone-sec 6 --rounds 1 --steam-rounds 5 --outfile loopback_pairs2.json
# 只测 Steam 相关对（约 2 分钟，含每轮服务状态：Steam 独立性证据）
& $PY -u "$BASE\probe_loopback_pairs2.py" --tone-sec 6 --filter Steam --steam-rounds 5 --outfile loopback_pairs2_run5_steam.json

# 端点名交叉核对（新进程，绕开缓存）
& $PY -u "$BASE\probe_endpoint_names.py"
```
注意：Python 必须用上面这个 venv（含 `soundcard` 0.4.6 + numpy）；后台跑要加 `-u`。插入/拔出耳机、切换默认设备都会改变端点集合，数值不可跨状态比较。
每次运行都会覆盖 `loopback_pairs2.json`（脚本只写 JSON，没有 .txt）。本报告引用的运行的**原始证据**对应关系：run1 = `loopback_matrix.json` + `loopback_matrix.txt` + `loopback_matrix_run1.log`（全量 77 格）；run2 = `loopback_pairs2_run2.log`（8 对全量，stdout 已归档）；run4 = `loopback_pairs2_run4.json` + `loopback_pairs2_run4.log`（8 对全量，含 `service_before/after`）；run5 = `loopback_pairs2_run5_steam.json` + 日志（只跑 Steam 相关对，`--filter Steam`）；run3 是同一批 Steam 对的复测，其 stdout 当时未落盘、JSON 已被后续运行覆盖，**逐轮数值仅记录在 §3 表格里**（不属于可再核验的原始证据）。

---

## 8. 异常与未验证项

| # | 项 | 状态 |
|---|---|---|
| 1 | `麦克风 (Chu2 DSP)` 每轮 `AssertionError:`（空 message） | **未解决**，soundcard 打不开该采集端点（疑似通道数/采样率协商断言）。矩阵里标「不适用」，不影响 7×11 其余格子。 |
| 2 | `soundcard` 的 `SoundcardRuntimeWarning: data discontinuity in recording`（`mediafoundation.py:772`）在 6 s 长录中大量出现 | **已观测**：WASAPI 报告采集流不连续。但同批次 `dropout_ratio` ≈ 0–0.0012 且 `tone_dbfs` 逐轮一致 → 未造成可测的音频丢失。**"驱动 flag vs 真实丢样"未区分，属未验证**。 |
| 3 | 显示器/耳机端点（CG345UK、X41、Chu2 DSP）后面到底接没接实物 | **未确认**——不需要：loopback 采的是数字流，§2.1 的结论不依赖它。 |
| 4 | 48 kHz / 16 bit / 立体声以外的格式（192 k、32 bit float） | **未验证**（本轮全部按 48 kHz 共享模式）。 |
| 5 | 独占模式（WASAPI exclusive）是否影响 loopback 采集 | **未验证**。 |
| 6 | 声学串音那条路的延迟 | 未测（房间声速决定，对本任务无意义）。 |
