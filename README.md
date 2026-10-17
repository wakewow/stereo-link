# StereoLink 立体声链

把本机播放设备组合成一对左右声道：系统声音拆成左 / 右两路，分别推给两台设备（例如两只蓝牙音箱），
同时在面板里实时显示每条链路的状态。

![StereoLink 面板](docs/images/gui.png)

- 程序形态：单个 `stereo-link.exe`（EGUI/OpenGL 绘制），没有安装包、没有后台服务、没有网页前端。
- 采集源是「虚拟声卡」：StereoLink 自研内核驱动 **StereoLinkVad**（render-only，从 Scream 派生，
  MS-PL）安装后在 Windows 输出列表里提供 **「扬声器 (立体声链虚拟声卡)」**。任何播放器都不用
  单独配置。安装需要开启测试签名（testsigning）并重启一次，见「快速开始」。
- 路由与播放是用户态 WASAPI；内核侧只有这台虚拟声卡，没有常驻服务、不联网。

## 这是什么、怎么工作

```text
        Windows 播放器（浏览器 / 音乐播放器 / 视频 / 游戏 …）
                          │  默认输出 = 「扬声器 (立体声链虚拟声卡)」
                          ▼
        虚拟声卡端点（自研驱动 StereoLinkVad，render-only）
                          │  WASAPI loopback 采集
                          │  44.1 kHz / 2 ch / 16 bit
                          ▼
                   StereoLink 路由引擎
                    ├── 左声道 → 设备 A（如 Xiaomi Sound-1695）  ≈10 ms 一块
                    └── 右声道 → 设备 B（如 Xiaomi Sound-5320）
```

- **左 / 右分离**：左声道整份送给设备 A，右声道整份送给设备 B。两路各自独立起流、独立失败：
  一台设备掉线不影响另一台。
- **任意本机播放设备**：蓝牙音箱、USB 声卡、耳机、HDMI 输出都可以，只要出现在「本机播放设备」
  列表里；左右各选一台即可（引擎本身支持任意多路，也支持「双声道混音」单只音箱听完整节目）。
- **自动切默认输出**：开始路由时自动把 Windows 默认输出切到虚拟声卡，正常退出时切回原设备；
  面板上也有「切为默认输出 / 恢复原输出」按钮，可关掉自动切换。
- **虚拟声卡改名**：面板里直接改名字（默认「立体声链虚拟声卡」），会弹 UAC 提权、写设备名并重启
  `AudioEndpointBuilder`（约 2–4 秒音频中断），改名前自动 `reg export` 备份到 `%TEMP%\stereo-link\backups\`。
- **播放设备格式设置**：每台设备都可以在面板里直接选采样率 / 位深（44.1 / 48 / 96 kHz，
  16 / 24 / 32 bit）。选中即生效：不用管理员、不用重启音频服务、不写注册表，改完立刻读回校验。
- **实测反馈**：每路实时电平、累计发送量、欠载次数；左右自检音；增益、抖动缓冲、左右微调延迟
  都在面板里可调。
- **诊断与日志**：面板内置诊断清单（虚拟声卡、默认输出、路由状态等），可一键复制；内嵌日志面板
  方便排障。

当前状态：Windows 10 / 11 x64，Rust 1.82+。虚拟声卡是自签内核驱动，安装时要开测试签名并重启
（卸载时可撤销），详见 [`docs/virtual-soundcard.md`](docs/virtual-soundcard.md)。

## 延迟

软件侧固定开销约 **80 ms**：抖动缓冲 40 ms + 每路渲染缓冲 40 ms（都可在面板里调小）。剩下的是
设备/链路自己的延迟：

| 链路 | 端到端延迟（估计） | 适合 |
| --- | --- | --- |
| 蓝牙 A2DP 直连（两只音箱各一路） | 约 0.2–0.35 s | 看视频、听歌、网课 |
| USB / 有线声卡 | 约 0.08–0.15 s | 看视频、游戏（取决于设备） |
| 同一对音箱的蓝牙「立体声组合」 | 约 0.2–0.35 s | 只能单房间、且两只箱必须组队 |
| 游戏 < 0.1 s | 需要有线耳机/声卡 | 蓝牙物理上做不到 |

想在面板里自己量：点「左右自检音」，两只箱会分别响不同频率；看面板上每路电平和欠载次数。

## 快速开始

1. **安装虚拟声卡**（只需一次，管理员权限）：右键仓库根目录的 `安装虚拟声卡.cmd` →
   「以管理员身份运行」，UAC 确认。脚本会自动开启测试签名（testsigning）、导入自签证书、装驱动；
   **按提示重启一次电脑**（脚本不会自己重启；重启后桌面出现「测试模式」水印属正常）。
   重启后确认：「设置 → 系统 → 声音 → 输出」出现「扬声器 (立体声链虚拟声卡)」，
   `pnputil /enum-devices /instanceid "ROOT\MEDIA\0001"` 显示 `Status: Started`。
2. **把两台播放设备连到本机**：蓝牙音箱在「设置 → 蓝牙和设备」里配对连接；USB 声卡/耳机插上即可。
3. **双击 `stereo-link.exe`**（或 `dist\stereo-link.exe`）。
4. 在「本机播放设备」表里给左右声道各选一台设备 → 点「开始路由」。面板会自动把 Windows 默认
   输出切到虚拟声卡，此时播放器的声音就会流进虚拟声卡、被拆成左右两路。
5. 需要还原环境时点「停止」或正常关闭面板：路由停止、默认输出切回原来的设备。

> 改名会弹 UAC；取消 UAC 不会破坏任何东西。安装/卸载驱动需要管理员权限，且都要重启一次。
> Secure Boot 开着的机器无法加载测试签名驱动，安装脚本会直接提示先关闭。

## 构建

```powershell
cargo build --release -p stereo-link
# 产物：target\release\stereo-link.exe（约 10 MB）
```

需要 Rust 1.82+（MSVC 工具链）。`cargo test --workspace` 跑全部单元测试。

虚拟声卡驱动是独立的 WDK 工程（不在 Rust workspace 里），构建/签名流水线在本机开发目录
`.scratch\vad\`，命令见[`docs/virtual-soundcard.md`](docs/virtual-soundcard.md) 的「构建与自签」。

## 配置与数据路径

| 路径 | 内容 |
| --- | --- |
| `%APPDATA%\stereo-link\config.json` | 面板配置（没有 `APPDATA` 时落在 exe 同目录） |
| `%TEMP%\stereo-link\switch-audio-default.ps1` | 切换默认输出用的临时脚本（从程序内嵌释放） |
| `%TEMP%\stereo-link\rename.log` | 面板改虚拟声卡名的执行日志 |
| `%TEMP%\stereo-link\backups\` | 面板「应用改名」前的注册表备份（`.reg`；改名脚本在临时目录运行，备份落在旁边） |
| `scripts\backups\` | 命令行改名工具 `scripts\rename-endpoint.ps1` 的备份目录 |
| `C:\Windows\System32\drivers\StereoLinkVad.sys` | 自研虚拟声卡驱动本体（约 37 KB，内核态） |

`config.json` 主要字段：

| 字段 | 含义 |
| --- | --- |
| `split.left_endpoint` / `split.right_endpoint` | 左 / 右声道目标端点（名字子串） |
| `split.source_endpoint` | 采集源；空 = 自动找虚拟声卡端点 |
| `split.gain` | 增益（0.0–2.0，默认 1.0） |
| `split.queue_ms` | 每路预排队音频长度（抖动缓冲，毫秒） |
| `split.left_delay_ms` / `split.right_delay_ms` | 左 / 右额外延迟（0–500 ms，用于对齐两只音箱） |
| `virtual_card.name` | 虚拟声卡设备名（默认「立体声链虚拟声卡」） |
| `virtual_card.endpoint_id` | 记住的虚拟声卡端点 id；改名后靠它继续识别（一般不用手改） |
| `virtual_card.auto_switch_default` | 开始路由时是否自动把默认输出切成虚拟声卡 |
| `virtual_card.previous_output` | 切换前的默认输出端点 id，用于退出时切回 |
| `virtual_card.last_real_output` | 最后一次见到的真实默认输出，异常退出后的兜底恢复 |
| `show_log` / `tone_seconds` / `auto_start` | 日志面板、自检音时长、启动时自动开始路由 |

`%TEMP%\stereo-link\backups\` 与 `scripts\backups\` 里的备份包含本机的设备实例路径，属于本机数据，不要提交到仓库或外传。

## 故障排查

| 现象 | 处理 |
| --- | --- |
| 输出列表里没有「扬声器 (立体声链虚拟声卡)」 | 管理员运行 `安装虚拟声卡.cmd` 并按提示重启；重启后确认 `pnputil /enum-devices /instanceid "ROOT\MEDIA\0001"` 为 `Status: Started`，设备管理器里 StereoLinkVad 无黄色叹号 |
| 设备管理器里 StereoLinkVad 带黄色叹号 / Code 52 | 测试签名没生效：`bcdedit /enum {current}` 应含 `testsigning Yes`；改完必须重启；Secure Boot 必须关闭 |
| 桌面右下角出现「测试模式」水印 | testsigning 已开启的正常现象；卸载虚拟声卡时可选关闭 |
| 面板提示「默认输出未切换」 | 点「虚拟声卡」区的「切为默认输出」；被其他程序独占或权限不足时也会失败，稍后重试 |
| 改了虚拟声卡名字后输出列表没变 | 等 2–4 秒并点「刷新设备」；改名需要 UAC 提权，取消会回滚，日志在 `%TEMP%\stereo-link\rename.log` |
| 改播放设备格式后没声音 / 声音异常 | 设备不支持该格式时可能自动回退或短时无声；把该行的格式下拉框选回原值即可（不写注册表，无需备份） |
| 只有一边有声 | 检查左 / 右声道选的是不是两台不同设备；看对应电平条是否在动；点「左右自检音」分别确认 |
| 声音断续、电平抖动 | 把「抖动缓冲」从 40 ms 往上调；缩短蓝牙距离、避开 2.4 GHz 干扰；蓝牙带宽不够时先断开其它蓝牙设备 |
| 退出后默认输出没恢复 | 只有正常退出面板才会恢复；异常退出（崩溃 / 任务管理器结束）请在「设置 → 系统 → 声音」里手动切回 |
| 想恢复虚拟声卡默认名 | 在面板「设备名」里改回 `立体声链虚拟声卡` →「应用改名」；要彻底移除驱动就运行 `卸载虚拟声卡.cmd` |
| 卸载后输出列表里还有旧端点 | 重启一次让设备节点彻底消失；`pnputil /enum-devices /instanceid "ROOT\MEDIA\0001"` 应返回设备不存在 |
| 看视频延迟明显 | 蓝牙链路固有延迟（≈0.2–0.35 s，取决于音箱固件），软件侧已压到约 80 ms；要求更低改用有线输出 |

## 目录结构

```text
stereo-link.exe            面板入口（crates/stereo-link）
crates/
├─ stereo-link/            GUI：面板、配置、虚拟声卡改名、默认输出切换
│  └─ src/app.rs           主界面（EGUI）
└─ stereo-audio/           音频库（无 GUI 依赖）
   ├─ src/route.rs         多路声道路由引擎（左/右/双声道，任意多路）
   ├─ src/capture.rs       WASAPI 回环采集
   ├─ src/playback.rs      WASAPI 播放（自检音、默认输出切换）
   ├─ src/endpoint.rs      端点枚举 / 设备选择器 / PcmFormat
   ├─ src/endpoint_format.rs  设备格式读写（IPolicyConfig）
   ├─ src/btsplit.rs       早期的「固定左右两路」实现（保留兼容）
   └─ src/ring.rs          环形缓冲 / 低延迟读
scripts/
├─ switch-audio-default.ps1  切 Windows 默认输出（IPolicyConfig，内嵌进程序）
├─ gui-set-name.ps1          面板改名用：写设备名 + 重启 AudioEndpointBuilder（内嵌进程序）
├─ rename-endpoint.ps1       通用命令行改名工具（备份 / 回滚 / 试运行）
└─ backups/                  rename-endpoint.ps1 的备份（本机数据，勿提交）
安装虚拟声卡.cmd / 卸载虚拟声卡.cmd   自研虚拟声卡的安装 / 卸载入口（需管理员、需重启）
docs/gui.md                 面板手册（界面、操作、改名/改格式、FAQ）
docs/virtual-soundcard.md   虚拟声卡：驱动来源、构建签名、安装卸载、testsigning、FAQ
```

驱动构建流水线与驱动包（`StereoLinkVad.inf` / `.sys` / `.cat` / `LICENSE-MS-PL.txt`）在
`.scratch\vad\`——本机开发目录，不进仓库；安装脚本也会在 `drivers\pkg-stereolink\` 找驱动包。

## 开发与测试

```powershell
cargo test --workspace                      # 单元测试
cargo run -p stereo-audio --bin endpoints   # 列出本机所有音频端点
cargo run -p stereo-audio --example route_demo -- --left "Xiaomi Sound-1695" --right "Xiaomi Sound-5320"
```

`route_demo` 是路由引擎的命令行自检：可选 `--both`（双声道混音）、`--source`（采集源子串）、
`--tone-to`（测试音播到哪台）、`--manual`（不自动播音，等外部声音）。

虚拟声卡驱动的构建、自签、安装与回滚命令见
[`docs/virtual-soundcard.md`](docs/virtual-soundcard.md) 第 4 节。

## 历史

本仓库早期是 `miplay-bridge`：把系统声音通过小米妙播云 API 投放到小米音箱（含 AirPlay 2 发送端）。
妙播云路线 PC 侧延迟约 0.25 s、但音箱固件内部缓冲接近 1 s，压不下来；AirPlay 2 在本机设备上
能建流但渲染静音，故都已被移除。虚拟声卡也经历过「借用 Steam Streaming Speakers 改名」的临时
方案，现已换成自研驱动 StereoLinkVad。现在产品只做「本机音频设备左右分声道」这一条纯用户态路线。

## 许可证

MIT（面板与音频库）。虚拟声卡驱动是从 Scream 派生的 MS-PL 代码（上游血统 MSVAD → Scream →
StereoLinkVad），许可证全文随驱动包提供（`LICENSE-MS-PL.txt`），再分发时需保留版权与许可声明；
驱动二进制目前不随仓库提交。
