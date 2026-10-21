# 手机 → 电脑出声：接收端使用说明

把**这台电脑的声卡**变成手机音乐投放的目标：iPhone 用「隔空播放（AirPlay）」，小米/红米用「小米妙播」，
手机上点一下，声音就从电脑的声卡出来。

::: tip 和 StereoLink 的关系
这两个接收端和 StereoLink 的**左右分声道引擎完全解耦**：独立进程、独立子工作区、只负责「收下来 → 交给本机声卡」，
不经过 `route.rs`、不碰虚拟声卡、不做 IPC。放着 StereoLink 不用也能单独跑。
:::

## 0. 在 StereoLink 面板里直接用（推荐）

打开 StereoLink，左侧导航第 4 项就是**「手机投放」**：

* **设备名固定是 `StereoLink`** —— 手机里看到、选的就是这个名字。
* **输出声卡 = Windows 当前的「默认播放设备」**，页面上会显示接收端实际打开的那块声卡；
  在系统里换默认声卡，接收端会自动重启跟着走（页面上写着「跟随系统默认」）。
* 两个接收端各占一行，显示状态（运行中 / 未运行 / 异常）和接收端 exe 路径；
  「开始 / 停止」单独控制，右上角「全部停止」一次收掉。
* 勾上**「随面板启动」**（默认勾着）后，面板一启动就会把接收端拉起来；取消勾选就不再自动拉。
* **「妙播低延迟（约 0.2 秒）」**：默认不勾。勾上后妙播接收端用 `--group-delay-ms 220 --min-queue-ms 160` 启动，
  把回放延迟从 ~0.8 秒压到 ~0.22 秒（详见 §9）。改这个开关会自动重启妙播接收端，AirPlay 不受影响。
* 接收端的原始输出会转发到面板的**「日志」页**（行首带 `[AirPlay]` / `[妙播]` 前缀）。
* 从面板**退出**（关闭行为选「退出」）时会一起停掉子进程，不会留下占着 5000 端口的孤儿进程。

::: tip 面板只是「遥控器」
面板通过子进程方式拉起接收端，接收端自己独立完成 mDNS 播报和音频输出；面板关掉但进程还在时，
手机依然能投过来（此时手动结束进程即可）。命令行用法见 §2 起。
:::

## 1. 目录结构

```
receiver/                        ← 独立 cargo workspace（与主工作区分离）
├── Cargo.toml                   ← workspace 定义（MSRV 1.94、许可隔离的说明都在注释里）
├── start.ps1                    ← 一键启动脚本（编译 + 防火墙 + 开两个窗口）
├── crates/
│   ├── airplay-recv/            ← AirPlay 1 接收端（iPhone / iPad / Mac）
│   │   ├── Cargo.toml
│   │   └── src/{main.rs, audio.rs}
│   └── miplay-recv/             ← 小米妙播接收端（小米 / 红米手机）
│       ├── Cargo.toml
│       └── src/main.rs
├── tools/probe_miplay_mdns.py   ← 妙播 mDNS 自检脚本（不用手机也能验证播报）
├── vendor/
│   ├── README.md                ← 打过补丁的 FusionPlay SDK 的来龙去脉（AGPL 修改声明）
│   └── fusionplay/              ← MiPlaySdk + AndroidIfAddrs 源码（低延迟补丁）
└── target/release/*.exe

crates/stereo-link/src/receiver.rs   ← 面板「手机投放」页的子进程管理（拉起/停止/跟随默认声卡）
```

为什么单独一个 workspace：`shairplay` 是 **LGPL-3.0-or-later**、`FusionPlay.MiPlaySdk` 是 **AGPL-3.0-only**，
都不能链进 `MIT OR Apache-2.0` 的主程序；而且前者要求 rustc 1.94（主工作区是 1.88）。
分成独立可执行文件后，许可只覆盖这两个小 exe。

## 2. 快速开始

```powershell
# 1) 编译（第一次约 1–2 分钟，之后增量几秒）
cd E:\AI\MiPlay\receiver
cargo build --release

# 2) 看有哪些输出设备，挑一个（序号很重要，见 §5）
.\target\release\airplay-recv.exe --list-devices
.\target\release\miplay-recv.exe  --list-devices

# 3) 启动（两个接收端各开一个窗口）
cd E:\AI\MiPlay\receiver
.\start.ps1 -Release
```

`start.ps1` 做的事：缺 exe 就先编译 → 给两个 exe 各加一条**入站放行**防火墙规则 → 用 `cmd start` 各开一个窗口。
关掉窗口 = 停掉对应接收端。

常用变体：

```powershell
.\start.ps1 -Release -Mode airplay     # 只开 AirPlay
.\start.ps1 -Release -Mode miplay      # 只开妙播
.\start.ps1 -Release -Device "#10"     # 两个接收端都输出到第 10 个设备
.\start.ps1 -Release -Name "书房电脑"   # 手机上显示的名字
```

手动跑（调试时更方便，日志直接在眼前）：

```powershell
.\target\release\airplay-recv.exe --name StereoLink --device "#10"
.\target\release\miplay-recv.exe  --name StereoLink --device "#10"
```

## 3. 手机端操作

| 手机 | 操作 |
|---|---|
| iPhone / iPad | 控制中心 → 点「隔空播放」图标（两个矩形叠在一起的按钮）→ 音频输出里选 **StereoLink** |
| iPhone（音乐 App 内） | 正在播放页 → 底部 AirPlay 图标 → 选 **StereoLink** |
| Mac | 菜单栏控制中心 → 声音 → 输出 → StereoLink |
| 小米 / 红米 | 控制中心 → **小米妙播** → 选 **StereoLink**（也可在音乐 App 的投送按钮里选） |

前提：手机和电脑在**同一个局域网**（同 Wi-Fi / 同路由器；跨 VLAN、AP 隔离会看不到）。

## 4. 两个接收端对比

| | `airplay-recv` | `miplay-recv` |
|---|---|---|
| 协议 | AirPlay 1（RAOP，RTSP 5000） | 小米妙播（Lyra / mi-connect，私有协议） |
| 适用 | iPhone / iPad / Mac | 小米 / 红米手机、小米平板 |
| 上游 | [`shairplay` 0.10](https://crates.io/crates/shairplay) | `FusionPlay.MiPlaySdk` 1.0.4（git） |
| 许可 | LGPL-3.0-or-later（独立 exe） | AGPL-3.0-only（独立 exe） |
| 出声方式 | 库回调交错 f32 → 抖动缓冲 → cpal/WASAPI | SDK 内部 cpal 直接出声，只需指定设备 |
| 设备身份 | 固定 MAC `02:53:54:52:4C:4E` | 首次运行生成并持久化到 `%LOCALAPPDATA%\StereoLink\miplay-recv.json` |
| 发现方式 | mDNS `_raop._tcp` | mDNS `_lyra-mdns._udp` + `_mi-connect._udp` |

两个可以同时开，互不冲突（AirPlay 占 5000，妙播的控制端口是动态的）。

## 5. 选输出声卡（最容易踩坑的一步）

先用 `--list-devices` 看清单，每行一个设备：

```
本机输出设备（--device 写 #序号 或 id；名字精确匹配也行）：
  [ 1] CG345UK｜48000 Hz / 2 ch / F32｜id=wasapi:{...}
  ...
  [10] 扬声器｜44100 Hz / 2 ch / F32｜id=wasapi:{...} ← 系统默认
```

::: warning WASAPI 端点会大量重名
这台机器上有 **6 个**设备都叫「扬声器」。所以 `--device 扬声器` 会直接报错并列出候选，这是故意的——
它不会替你瞎猜。三种选法，优先级从高到低：

1. `--device "#10"` —— 序号，最省事；
2. `--device "wasapi:{0.0.0.00000000}.{...}"` —— 完整 id，最稳（重启、插拔都不变）；
3. `--device "Realtek"` —— 名字子串，**只在唯一命中时**才用。
:::

不写 `--device` 就走系统默认输出设备。

```powershell
.\target\release\airplay-recv.exe --device "#10"      # 第 10 个
.\target\release\airplay-recv.exe --device default    # 系统默认
```

## 6. 参数速查

`airplay-recv`：

| 参数 | 说明 | 默认 |
|---|---|---|
| `-n, --name <名字>` | 手机投放列表里显示的名字 | `StereoLink` |
| `-p, --port <端口>` | RTSP 端口，被占用自动往后找 | `5000` |
| `-r, --rate <采样率>` | 输出采样率，与音源一致最省 CPU | `44100` |
| `-d, --device <声卡>` | 名字/id/`#序号`/`default` | 系统默认 |
| `--bind <IP>` | 只监听指定网卡（多网卡机器建议写） | 全部 |
| `--list-devices` | 列出输出设备后退出 | |
| `-v, --verbose` | 调试日志（含 mDNS/TXT 细节） | |

`miplay-recv`：

| 参数 | 说明 | 默认 |
|---|---|---|
| `-n, --name <名字>` | 显示名 | `StereoLink` |
| `-d, --device <声卡>` | `#序号` / id / 名字 | 系统默认 |
| `--ip <IPv4>` | 本机局域网地址，不填自动探测 | 自动 |
| `--iface <网卡名>` | 一般不用填 | 自动 |
| `--volume <0-100>` | 接收端音量初值 | `50` |
| `--state <文件>` | 身份持久化文件 | `%LOCALAPPDATA%\StereoLink\miplay-recv.json` |
| `--list-devices` | 列出输出设备后退出 | |
| `--quiet` | 只打印报错事件（面板拉起时用，避免刷屏） | 关 |
| `--group-delay-ms <ms>` | 妙播目标延迟：越小越跟手，见 §9 | `800` |
| `--min-queue-ms <ms>` | 排队延迟下限，一般只跟着上面一起调小 | `650` |
| `--diag-loopback` | 自检专用：让 mDNS 响应也能回到本机（见 §8） | 关 |

改 `--name` 会改手机里显示的名字；`miplay-recv` 的身份文件删掉会重新生成（手机端可能需要重新发现一次）。

## 7. 防火墙

两个接收端都要**入站放行**（手机是主动连进来的）。`start.ps1` 会自动加，规则名分别是
`StereoLink airplay-recv` 和 `StereoLink miplay-recv`（Inbound / Allow / Private+Public / Any 协议）。

手动加（管理员 PowerShell）：

```powershell
$exe = "E:\AI\MiPlay\receiver\target\release\airplay-recv.exe"
New-NetFirewallRule -DisplayName "StereoLink airplay-recv" -Direction Inbound -Action Allow `
    -Program $exe -Profile Private,Public -Protocol Any
```

去掉规则：`Remove-NetFirewallRule -DisplayName "StereoLink airplay-recv"`。

Windows 首次运行没加规则时，系统会弹「是否允许访问网络」——要勾**专用网络**（家里/公司网）并允许；
漏点了就按上面手动补，或者 `start.ps1` 重跑一次。

## 8. 不用手机的自检

**AirPlay**：iPhone 上直接看投放列表里有没有 `StereoLink` 就是最快的验证。

**妙播**（小米手机不在手边时）：

```powershell
python E:\AI\MiPlay\receiver\tools\probe_miplay_mdns.py
```

两个判据：

- `_lyra-mdns._udp`：**只要收到来自本机的应答就算过**（这一路的实例 ID 不带显示名）；
- `_mi-connect._udp`：应答里必须能看到 `StereoLink` 字样。

多网卡机器上（VMware / Hyper-V 会多出 192.168.137.1、172.28.x.x 这类网卡）加 `--interface 192.168.1.4` 再跑一次。
本机自测时还要给 `miplay-recv` 加 `--diag-loopback`，否则 SDK 会关掉 multicast loop，
同一台机器发出的查询收不回来——**这只影响自检，真机不受影响**。

## 9. 常见问题

**手机看不到设备？**
1. 防火墙（最常见）——见 §7；
2. 手机和电脑不在同一网段（一个连 Wi-Fi、一个连有线但不同子网，或路由器开了 AP 隔离）；
3. 电脑有多个网卡，mDNS 播到了错的网卡上 → `airplay-recv --bind 192.168.1.4`；
4. 接收端窗口被关了 / 崩了 —— 看窗口里的报错。

**AirPlay 添加了但没声音？**
- 看窗口打印的「🔈 输出声卡」是不是你想要的那个（重名设备容易选错，改用 `#序号`）；
- Windows 音量合成器里看看这个 exe 的音量是不是被压到 0；
- 设备被独占（某些 ASIO/独占模式程序占着声卡）→ 换设备或关掉那个程序。

**妙播连上了但没声音？**
- 妙播的音量控制在**接收端**（`--volume`），手机音量键不一定同步；
- SDK 要求 48 kHz，用 `--list-devices` 确认目标设备支持 48 kHz（本机大多数设备都是）；
- 设备名/id 写错时 SDK 会打一条 warning 并**回落到默认设备**，所以「连上了没声音」先看是不是落到别的声卡了。

**端口冲突？**
`airplay-recv` 的 5000 被占（比如 MiXPlay 正在跑）会自动往后找端口；手机端看到的设备照样能用。
妙播的控制端口每次启动随机取。

**声音断续 / 延迟偏大？**
- 断续：多半是 WiFi 抖动（2.4G 拥挤、隔墙），换 5G 或用网线；
- 延迟：AirPlay 端内置约 200 ms 抖动缓冲（听音乐无感，看视频会对不上口型）；
- **妙播比手机/小爱音箱慢半拍？** 这是上游 SDK 的默认行为：它把「目标延迟」写死成 800 ms、
  排队下限 650 ms（为了和小米「全屋播放」里其它设备严格对齐），所以只投本机时也要先攒 0.65~0.8 秒。
  两个办法：

  ```powershell
  # 命令行
  .\target\release\miplay-recv.exe --group-delay-ms 220 --min-queue-ms 160
  ```
  ```
  面板：「手机投放」页 → 勾选「妙播低延迟（约 0.2 秒）」
  ```

  原理见 `receiver/vendor/README.md`：`receiver/vendor/` 里放的是**打过补丁**的 FusionPlay SDK，
  把那两个常量改成可用环境变量覆盖（`FUSIONPLAY_MIPLAY_GROUP_DELAY_MS` /
  `FUSIONPLAY_MIPLAY_MIN_QUEUE_MS`），默认值仍等于上游值。代价：和小米音箱一起「全屋播放」时，
  本机会比它们早约 0.6 秒——那种场景就别开低延迟。

**和 MiXPlay 一起用会怎样？**
MiXPlay 也会注册 `_raop._tcp` / `_lyra-mdns._udp` 并占 5000 端口，手机上可能出现**两个**同类设备
（名字里带 MiXPlay 的那条 + `StereoLink`）。想省心就二选一，别同时开。

## 10. 许可说明

- `airplay-recv` 静态链接 `shairplay`（**LGPL-3.0-or-later**）：作为**独立可执行文件**使用，只要保留许可声明、
  并能在被要求时提供该 exe（含其依赖库）对应的源码即可。本仓库自带 `receiver/` 全部源码与 `Cargo.lock`，
  重新 `cargo build --release` 即可复现，满足 LGPL 的替换要求。
- `miplay-recv` 链接 `FusionPlay.MiPlaySdk`（**AGPL-3.0-only**）：整个 `miplay-recv` 按 AGPL-3.0 分发。
  **不要把它链进主程序**，也不要和闭源组件合并；单独放这个 exe、随仓库提供源码即可。
  其中 `receiver/vendor/fusionplay/` 是**改过的上游副本**（只改了时间常量，见
  `receiver/vendor/README.md` 的改动清单），按 AGPL 要求随源码一起提供。
- 主程序 StereoLink 本体仍是 `MIT OR Apache-2.0`，不受影响。

## 11. 验证状态

已验证：

- [x] 两个 crate `cargo build --release` 通过（Windows x86_64，rustc 1.99）；
- [x] `airplay-recv` mDNS 播报被同一网络上的第三方（Python `zeroconf`）发现，TXT 记录字段正常
      （`cn=0,1 sv=false da=true vn=3 md=0,1,2 ch=2 sr=44100 ek=1 tp=TCP,UDP`）；
- [x] `miplay-recv` 的 Lyra / mi-connect 应答用 `tools/probe_miplay_mdns.py` 本机自检通过；
- [x] 设备选择（重名报错、`#序号`、完整 id）行为符合预期；
- [x] 低延迟补丁编译通过，启动横幅正确显示「目标延迟：220 ms，排队下限：160 ms」；
- [x] 面板「手机投放」页渲染正常（两行状态、随面板启动、低延迟勾选框、接收端路径）。

待真机确认：

- [ ] iPhone 在「隔空播放」列表里看到 `StereoLink` 并成功出声（延迟、音质）；
- [ ] 小米手机「小米妙播」列表里看到 `StereoLink` 并成功出声；
- [ ] 手机端断开/重连、切歌、音量同步的行为。
