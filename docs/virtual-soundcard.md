# 虚拟串流声卡：在 Windows 输出列表里直接选「扬声器 (小米妙播设备)」

> 状态：**已在本机落地并端到端验证通过**（人耳确认左右分离）。
> 目标：把小米妙播设备变成 Windows 的一个普通播放设备——任何程序把输出切到它，声音就进音箱。

## 1. 整条链路

```
任何播放器
   │  （把输出选到「扬声器 (小米妙播设备)」）
   ▼
虚拟声卡端点（借用机器上已有的已签名虚拟声卡，改名为「小米妙播设备」）
   │  WASAPI loopback（wasapi-autoconvert 48k/2ch/32f → 44100/2ch/s16）
   ▼
miplay-bridge hub：环形缓冲 → 按通道拆分 → 每条流一个 WAV HTTP 端点
   │  http://<本机IP>:8931/hub/<did>.wav
   ▼
小米云 player_play_url 下发（每台音箱一条）→ 音箱自己来拉流
   ▼
Xiaomi Sound-左（左声道） + Xiaomi Sound（右声道）
```

关键点：**音箱不是被推流的，是被云指令叫去拉流的**。所以云只在下发瞬间参与，
之后音频全程走局域网，这也是为什么 30 秒投放里云端只被调用两次。

## 2. 为什么不是「装一个自己的驱动」

用户在态程序**不可能**注册 render 端点：端点由 `AudioEndpointBuilder` 从内核驱动
注册的 `KSCATEGORY_AUDIO` 接口派生（见 `research/vad/vad-options.md`）。三条路都试过/评估过：

| 路线 | 结论 |
|---|---|
| 自研 / 改造驱动（Scream 派生） | 签名能装、但载不起来：`0xC0000428 STATUS_INVALID_IMAGE_HASH`，`CM_PROB_UNSIGNED_DRIVER`。必须开 test signing 或买 EV 证书做 attestation |
| USB/IP 虚拟设备（Virtual-Cables） | Windows 侧 VHCI 只有未签名的 `usbip-win2`，同样要常开 test signing |
| **借用已有虚拟声卡 + 改名** | ✅ **唯一免驱动、免证书、免重启的路**，本机已落地 |

## 3. 操作（三个双击脚本）

| 脚本 | 作用 |
|---|---|
| `安装虚拟声卡.cmd` | 让输出列表出现「扬声器 (小米妙播设备)」（需管理员） |
| `启动-妙播投放.cmd` | 开始抓这个端点并投放到两只音箱，关窗口即停 |
| `卸载虚拟声卡.cmd` | 从备份 `.reg` 回滚设备名（需管理员，可完整还原） |

装好之后：
1. 双击 `启动-妙播投放.cmd`
2. Windows → 设置 → 系统 → 声音 → 输出，选 **「扬声器 (小米妙播设备)」**
3. 正常放歌。也可以只让某个播放器单独把输出切到它，不影响系统其他声音

> **双向已验证（2026-10-18 01:24）**
> - 安装：`[PASS] endpoint renamed` → 端点显示名 `扬声器 (小米妙播设备)`
> - 卸载：`RESIDUE-FREE: rolled back and verified`，逐项断言
>   `device friendly name restored` / `endpoint name(s) restored` /
>   `MMDevices Properties unchanged`，端点显示名恢复 `扬声器 (Steam Streaming Speakers)`
>
> 两个坑（都已写进脚本）：
> 1. `rename-endpoint.ps1` 的 `-Rollback` 和 `-DeviceInstanceId` 属于**不同 PowerShell
>    参数集**，同时传会报 `AmbiguousParameterSet`。回滚只能传 `-Rollback`。
> 2. **重复安装会污染备份**：第二次安装的「改名前」备份里 FriendlyName 已经是
>    `小米妙播设备`，拿它回滚等于没回滚。所以卸载脚本只挑文件名含
>    `Steam_Streaming_Speakers` 的原始备份。

## 4. 改名做了什么（三态实测，见 `research/vad/rename-probe.md`）

活路是写**硬件键**再刷一次端点缓存：

```powershell
# 1) 备份（整个 Enum 键）
reg export "HKLM\SYSTEM\CurrentControlSet\Enum\ROOT\STEAMSTREAMINGSPEAKERS\0000" backup.reg /y
# 2) 写设备级名字
reg add "HKLM\SYSTEM\CurrentControlSet\Enum\ROOT\STEAMSTREAMINGSPEAKERS\0000" /v FriendlyName /t REG_SZ /d "小米妙播设备" /f
# 3) 让 AudioEndpointBuilder 重建端点显示名
pnputil /disable-device "ROOT\STEAMSTREAMINGSPEAKERS\0000"
pnputil /enable-device  "ROOT\STEAMSTREAMINGSPEAKERS\0000"
```

结果：设备名 `小米妙播设备` → 端点显示名 **`扬声器 (小米妙播设备)`**。

两个已知的**死路**（不要再试）：
- `HKLM\SYSTEM\CurrentControlSet\Control\MMDevices\Audio\Render\{GUID}\Properties`
  下的 `,14` —— `reg add` 直接 `Access is denied`，官方 `IPropertyStore::SetValue` 也是
  `0x80070005 E_ACCESSDENIED`（提权管理员同样被拒，是 Audiosrv 的 ACL 保护）。
- 想去掉「扬声器 」前缀 —— 那是驱动 pin 名，零驱动改不掉。

## 5. hub 的自动选择与参数

`hub` 会**自动优先**抓名为「小米妙播设备 / 小米妙播」的 render 端点；没有该端点时回退到
默认输出端点。启动横幅会明说走的是哪一条：

```
采集端点     [render] 扬声器 (小米妙播设备) | 48000 Hz / 2 ch / 32-bit float (…)
采集来源     虚拟串流声卡「扬声器 (小米妙播设备)」——播放器切到它即可（--capture-device 可覆盖）
```

显式覆盖：

```powershell
target\debug\miplay-bridge.exe hub --capture-device "扬声器 (小米妙播设备)" --devices 501718792,934947888 --split --gain 0.7
```

| 参数 | 说明 |
|---|---|
| `--devices <did,did>` | 目标小爱音箱，顺序决定左右（第 1 台=左） |
| `--split` | 左右分离分发（不加则每台都拿立体声） |
| `--swap` | 左右对调 |
| `--gain <倍数>` | 增益，默认 1.0，上限 8.0 |
| `--capture-device <名>` | 指定采集端点，默认自动优先虚拟声卡 |
| `--port <端口>` | HTTP 端口，默认 8931 |
| `--seconds <秒>` | 定时停止（默认一直跑） |
| `--no-firewall` | 不自动加防火墙规则 |

## 6. 实测记录（2026-10-18 01:22）

```
采集端点     [render] 扬声器 (小米妙播设备) | 48000 Hz / 2 ch / 32-bit float
采集来源     虚拟串流声卡「扬声器 (小米妙播设备)」
分发模式     左右分离（第 1 台=左，第 2 台=右）
目标设备     Xiaomi Sound-左 (501718792) L16A ← /hub/501718792.wav [left]
目标设备     Xiaomi Sound (934947888) L16B ← /hub/934947888.wav [right]

运行时长     32.4 s
分发帧数     546583
Xiaomi Sound-左   下发 1 次（失败 0）· 发流 2.09 MB · 跳帧 0 B · 残留客户端 0
Xiaomi Sound     下发 1 次（失败 0）· 发流 2.09 MB · 跳帧 0 B · 残留客户端 0
```

同时用 `tools/play_to_endpoint.py` 往虚拟端点播了 12 秒 440 Hz（左右每秒交替），
**人耳确认：「听到了，左右交替分开了」**。

## 7. 排错

| 现象 | 处理 |
|---|---|
| 输出列表里没有「扬声器 (小米妙播设备)」 | 跑 `安装虚拟声卡.cmd`（管理员）；若跑过，检查是否被卸载脚本还原 |
| 端点有但没声音 | 看 hub 窗口的 `[采集]` 行：出现「已经 4 秒没有新音频」说明没程序往该端点送声 |
| 一只响一只不响 | 看 `[流]` 行是否有两个客户端接入；用 `--volume 60` 显式设音量（该机型 AirPlay 音量会持久化在音箱上） |
| 左右反了 | 加 `--swap` |
| 关闭窗口后还在放 | 云侧会话已停止，但音箱可能还在放缓冲；用网页面板的「全部停止」或 `hub --stop` |
