# 命令行与无头部署（`stereo-link-cli`）

StereoLink 除了 GUI，还有一个同包产出的控制台程序 `stereo-link-cli.exe`。
**界面上能做的事，命令行都能做；界面展示的状态，命令行都能以 JSON 拿到。**
这是项目的开发规范（见仓库根 `AGENTS.md` §2），也是自动化测试与服务器部署的基础。

- 主程序：`stereo-link.exe`（GUI）
- 控制台程序：`stereo-link-cli.exe`
- 两者共享同一份配置（`%APPDATA%\stereo-link\config.json`）与控制面协议。

## 1. 实例模型

| 现状 | `status` / 动作类命令 | `log` / `config` / `endpoints` / `doctor` |
| --- | --- | --- |
| GUI 或 `serve` 在跑 | 通过控制面（`127.0.0.1` 随机端口，token 校验）执行 | 走实例；`log` 也能直接读文件 |
| 没有实例 | `status` 给离线概览（配置 + 上次日志路径） | 直接读文件 / 直接枚举端点，无需实例 |

控制面细节：实例启动时写 `%APPDATA%\stereo-link\control.json`（`port` / `token` / `pid` / `mode`），退出时删除；如果进程被强杀，下一次 `read` 会用 120 ms TCP 探测判定文件是陈的并自动清掉，不会卡住 CLI。

## 2. 全局开关

| 开关 | 说明 |
| --- | --- |
| `--json`, `-j` | 输出一个 JSON 文档（脚本用）；默认是人读文本 |
| `-v`, `--verbose` | 把 tracing 日志同时打到控制台 |
| `-V`, `--version` | 版本 |
| `-h`, `--help` | 帮助 |

退出码：`0` 成功，`1` 命令失败/实例报错，`2` 用法错误。

## 3. 命令参考

### status

```powershell
stereo-link-cli status
stereo-link-cli status --json
```

文本模式打印：模式（gui/serve）、pid、运行时长、路由是否在跑、每路端点/排队/欠载/丢帧、接收端状态与输出声卡、系统默认输出、配置与日志路径。

`--json` 输出**实例返回的数据文档本体**（不再包一层 `data`），顶层字段：
`mode`（`gui` / `serve` / 离线时没有该字段）、`pid`、`version`、`protocol`、`uptime_seconds`、
`routing`（`running` + `engine`：`running/rate/source/routes[]/captured_frames/level_l/level_r`）、
`receivers`（`any_running/device/items[]`，每项 `kind/label/running/status/error/exe`）、
`endpoints`（`count/items[]`：`id/name/flow/is_default/mix_format/error`）、`config`（`path/split/capture/cast/theme`）、
`extra`（`default_output/log_path/receivers_managed` 等）。

没有实例时 `status --json` 输出 `{"running": false, "reason": "..."}`，退出码仍为 0（这是「没有实例」而不是错误）。

### log

```powershell
stereo-link-cli log                      # 实例在跑读内存，否则读落盘文件
stereo-link-cli log -n 50 --level WARN   # 只要 WARN 以上，最近 50 行
stereo-link-cli log -f                   # 只读文件（即使实例在跑）
stereo-link-cli log --json
```

日志文件：`%APPDATA%\stereo-link\logs\stereo-link.log`（超 2 MB 轮转为 `stereo-link.log.1`）。
接收端（`airplay-recv` / `miplay-recv`）是子进程，它们的输出会被 `ReceiverManager` 转发进同一份日志，行首带 `[AirPlay]` / `[小米妙播]` 前缀。

### routing

```powershell
stereo-link-cli routing status
stereo-link-cli routing start
stereo-link-cli routing stop
stereo-link-cli routing restart
stereo-link-cli routing mute
stereo-link-cli routing unmute
```

### receiver

```powershell
stereo-link-cli receiver list
stereo-link-cli receiver start airplay
stereo-link-cli receiver start miplay
stereo-link-cli receiver stop all
stereo-link-cli receiver restart miplay
```

设备名固定 `StereoLink`；输出声卡跟随系统默认（接收端启动时读取）。启动前会清理上次留下的同名孤儿进程，可用环境变量 `STEREOLINK_KEEP_STALE_RECEIVERS=1` 跳过清理。

### endpoints

```powershell
stereo-link-cli endpoints           # 全部
stereo-link-cli endpoints render    # 播放（render）端点
stereo-link-cli endpoints capture   # 采集（capture）端点
stereo-link-cli endpoints --json
```

每行含端点 id、名称、混音格式、是否默认。WASAPI 里多块声卡可能同名（例如多个「扬声器」），配置里请用**端点 id**。

### config

```powershell
stereo-link-cli config path
stereo-link-cli config show
stereo-link-cli config get split.queue_ms
stereo-link-cli config set split.queue_ms 60
stereo-link-cli config set cast.low_latency true
stereo-link-cli config set theme dark
```

- key 用点号分隔，映射到 `config.json` 的 JSON 路径；值是 JSON 优先（`true`、`42`、`[1,2]`），解析不了就当字符串。
- `set` 会先反序列化整份 `AppConfig` 校验，写坏了会拒绝并保留原文件。
- 有实例在跑时改完会同步到实例；没有实例时直接改文件。

### tone

```powershell
stereo-link-cli tone --freq 440 --seconds 1.0
stereo-link-cli tone --device "扬声器"        # 按名字片段选设备
```

### doctor

```powershell
stereo-link-cli doctor
stereo-link-cli doctor --json
```

环境自检：配置文件是否存在/可解析、日志目录可写、播放端点数量、虚拟声卡是否安装、两个接收端 exe 是否能找到、是否有实例在跑。每项带 `ok` 与说明，最后汇总。

### serve（无头）

```powershell
stereo-link-cli serve                      # 路由 + 接收端 + 控制面
stereo-link-cli serve --no-receivers       # 只跑路由
stereo-link-cli serve --no-routing         # 只跑接收端
```

- 不创建窗口、不加载 GPU/egui，200 ms 主循环：收控制面请求 + `poll` 接收端 + 刷新端点 + 同步系统默认声卡。
- 启动时自动按配置拉起接收端、按 `split.enabled` 启路由（与 GUI 行为一致）。
- **退出**：`stereo-link-cli quit`、`Ctrl+C`、`SIGTERM`（systemd `stop`）都会先停路由、停接收端、删 `control.json`，再退出；不会留下孤儿进程。
- 控制台会打印 `StereoLink v0.5.5 无头服务运行中（mode=serve）：控制面 127.0.0.1:xxxxx，Ctrl+C 退出`。

### 其它

```powershell
stereo-link-cli ping
stereo-link-cli quit        # 让 GUI 或 serve 退出
```

## 4. 无头测试流程（推荐验收方式）

```powershell
# 1. 起无头实例（不占接收端端口）
target\debug\stereo-link-cli.exe serve --no-receivers

# 2. 断言状态
$s = target\debug\stereo-link-cli.exe status --json | ConvertFrom-Json
$s.mode            # "serve"
$s.routing.running # True

# 3. 驱动动作
target\debug\stereo-link-cli.exe routing stop
target\debug\stereo-link-cli.exe routing start
target\debug\stereo-link-cli.exe config set split.queue_ms 60

# 4. 收尾
target\debug\stereo-link-cli.exe quit
```

不再需要「启动 GUI → 移动鼠标点按钮 → 截图」来验证行为；只有纯视觉（颜色/布局）问题才用截图。

## 5. 命令行部署

### 5.1 前置

1. 构建产物（或安装包）里的两个 exe 放在同一目录：
   `stereo-link.exe`、`stereo-link-cli.exe`。
2. 接收端 exe 放到 `receiver\target\release\`，或把 `receivers\` 放在主程序目录下，或用环境变量 `STEREOLINK_RECEIVER_DIR` 指定目录。
3. 防火墙：`airplay-recv.exe` / `miplay-recv.exe` 需要**入站放行**（Private/Public）。GUI 下打开投放页时会尝试加规则；纯命令行的机器请先执行：

```powershell
New-NetFirewallRule -DisplayName "StereoLink airplay-recv" -Direction Inbound -Action Allow -Program "$PWD\receiver\target\release\airplay-recv.exe" -Profile Private,Public
New-NetFirewallRule -DisplayName "StereoLink miplay-recv" -Direction Inbound -Action Allow -Program "$PWD\receiver\target\release\miplay-recv.exe" -Profile Private,Public
```

### 5.2 Windows：计划任务（推荐，用户登录后自启）

```powershell
$exe = "C:\Program Files\StereoLink\stereo-link-cli.exe"
$action  = New-ScheduledTaskAction -Execute $exe -Argument "serve" -WorkingDirectory "C:\Program Files\StereoLink"
$trigger = New-ScheduledTaskTrigger -AtLogOn
$settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -RestartCount 3 -RestartInterval (New-TimeSpan -Minutes 1)
Register-ScheduledTask -TaskName "StereoLink" -Action $action -Trigger $trigger -Settings $settings -RunLevel Highest
```

> 需要「Highest」是因为路由引擎要切换/独占音频端点；`serve` 本身不弹 UI，登录后自启即可。
> 停止/状态：`Stop-ScheduledTask -TaskName StereoLink` / `stereo-link-cli status`。

### 5.3 Windows：NSSM 注册为服务（无人登录也要跑）

```powershell
nssm install StereoLink "C:\Program Files\StereoLink\stereo-link-cli.exe" serve
nssm set StereoLink AppDirectory "C:\Program Files\StereoLink"
nssm set StereoLink AppStdout "C:\ProgramData\StereoLink\serve.log"
nssm set StereoLink AppStderr "C:\ProgramData\StereoLink\serve.err.log"
nssm set StereoLink Start SERVICE_AUTO_START
nssm start StereoLink
```

服务方式下进程没有交互桌面，**接收端与路由仍然可用**（它们只用 WASAPI/mDNS/网络），但要以「有音频设备的账户」运行（LocalSystem 在部分机器上枚举不到用户态音频端点，建议用 `nssm set StereoLink ObjectName ".\你的用户名" "密码"`）。

### 5.4 Linux：systemd user unit（CLI + 接收端）

主程序引擎是 Windows-only（见 `docs/platform-feasibility.md`），**Linux 上能部署的是 CLI + 接收端**（接收端是独立 workspace，可单独编译；结论见 `docs/platform-receiver-feasibility.md`）。适合把一台 Linux 机器/小主机当「手机投放接收盒」。

```ini
# ~/.config/systemd/user/stereolink-receivers.service
[Unit]
Description=StereoLink 手机投放接收端
After=network-online.target sound.target
Wants=network-online.target

[Service]
Type=simple
WorkingDirectory=%h/stereolink
# 两个接收端由一个小脚本/或直接跑其中一个
ExecStart=%h/stereolink/airplay-recv --name StereoLink
Restart=on-failure
RestartSec=3
# 可选：低延迟妙播
# Environment=FUSIONPLAY_MIPLAY_GROUP_DELAY_MS=220
# Environment=FUSIONPLAY_MIPLAY_MIN_QUEUE_MS=160

[Install]
WantedBy=default.target
```

```bash
systemctl --user daemon-reload
systemctl --user enable --now stereolink-receivers
journalctl --user -u stereolink-receivers -f
```

注意（详见接收端可行性文档）：

- 必须用 **user unit**（`XDG_RUNTIME_DIR` 与 PulseAudio/PipeWire 会话相关），不要装成 system unit。
- 端口 5000/8899、mDNS 5353 都 >1024，不需要 root，但 5353 可能被 `avahi-daemon` 占用（会失败）。
- AirPlay 端在 Linux 用 `mdns-sd` 播报，通常可直接工作。

### 5.5 macOS：launchd（有限支持）

```xml
~/Library/LaunchAgents/com.stereolink.receivers.plist
<dict>
  <key>Label</key><string>com.stereolink.receivers</string>
  <key>ProgramArguments</key>
  <array><string>/Users/you/stereolink/airplay-recv</string><string>--name</string><string>StereoLink</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key><string>/tmp/stereolink.out.log</string>
  <key>StandardErrorPath</key><string>/tmp/stereolink.err.log</string>
</dict>
```

```bash
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.stereolink.receivers.plist
launchctl print gui/$(id -u)/com.stereolink.receivers
```

已知限制：**妙播接收端在 macOS 会因 5353 端口独占失败**（`miplay-recv` 走 SDK 自己的 responder，只 `set_reuse_address` 就 `bind 0.0.0.0:5353`）；AirPlay 端则可运行。详见 `docs/platform-receiver-feasibility.md`。

## 6. 常见问题

| 现象 | 原因/处理 |
| --- | --- |
| `没有运行中的 StereoLink 实例` | GUI/`serve` 没在跑；先 `serve`。若确认在跑，看 `%APPDATA%\stereo-link\control.json` 是否残留（新版会自动清理） |
| `failed to remove file … stereo-link.exe (os error 5)` | 构建时 GUI 在跑：先 `Get-Process stereo-link \| Stop-Process -Force` |
| 手机看不到 `StereoLink` | 防火墙入站未放行（见 5.1）、手机与 PC 不在同一网段、mDNS 被占用 |
| 妙播延迟比别的设备大 | 默认 800 ms 是给「全屋播放」留的对齐延迟；`config set cast.low_latency true` 或接收端 `--group-delay-ms 220 --min-queue-ms 160` |
| 接收端残留占着 5000/8899 | 实例退出时会一起停；被强杀时下次启动会自动清理孤儿（`STEREOLINK_KEEP_STALE_RECEIVERS=1` 可关闭） |
