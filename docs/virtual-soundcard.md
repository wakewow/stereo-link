# 虚拟声卡：Windows 输出列表里的「扬声器 (立体声链虚拟声卡)」

> 实现：StereoLink 自研内核驱动 **StereoLinkVad**（render-only 虚拟声卡，从 Scream 派生，MS-PL）。
> 安装需要开启测试签名（testsigning）并重启一次；卸载会移除驱动、设备、服务与自签证书，
> 也可以顺手把 testsigning 关掉。

## 1. 这条链路怎么走

```text
Windows 播放器（浏览器 / 音乐播放器 / 视频 / 游戏 …）
        │  默认输出 = 「扬声器 (立体声链虚拟声卡)」
        ▼
System 默认播放端点：StereoLinkVad 提供的 render 端点
        │  WASAPI loopback 采集（44.1 kHz / 2 ch / 16 bit）
        ▼
StereoLink 路由引擎（每 10 ms 一块）
        ├── 左声道 → 设备 A（如 Xiaomi Sound-1695）
        └── 右声道 → 设备 B（如 Xiaomi Sound-5320）
```

播放器不需要任何配置：只要系统默认输出选到这台虚拟声卡，声音就会流进分离链路。

## 2. 为什么需要虚拟声卡

StereoLink 要在**系统级**拆左右声道，就必须有一个所有播放器都会把声音送进去的端点。
用户态程序没法把自己变成「系统默认播放设备」，只有内核音频驱动能提供这样的端点。
所以这个端点由一个小内核驱动提供；路由与播放仍在用户态完成（WASAPI），没有常驻服务、不联网。

驱动只提供 render（播放）端点，不提供录音端点；WASAPI 回环采集对 render 端点有效
（在音频引擎里取数），两者不冲突。

## 3. 驱动来源与许可

| 项目 | 说明 |
| --- | --- |
| 上游 | Scream（<https://github.com/duncanthrax/scream>，commit `d789743c248b11d1df7e5ecc546b1bc60b90cd91`），再往上是微软 WDK 的 MSVAD 示例 |
| 许可证 | MS-PL（Microsoft Public License） |
| 我们改了什么 | 设备/端点显示名改成「立体声链虚拟声卡」、硬件 id 改为 `*StereoLinkVad`、内核服务名改为 `StereoLinkVad`、补充构建/签名/安装脚本与 INF 注释 |
| 再分发 | MS-PL 允许改名与再分发（含二进制），但必须保留版权与许可声明；`LICENSE-MS-PL.txt` 全文随驱动包一起放了 |
| 商标 | MS-PL 不授予商标许可：`Scream` 只能作为出处说明出现，不能当产品名 |

驱动本体落地为 `C:\Windows\System32\drivers\StereoLinkVad.sys`（约 37 KB，内核态运行），
服务名 `StereoLinkVad`，设备类 `MEDIA`，实例 id **`ROOT\MEDIA\0001`**。

驱动包（`StereoLinkVad.inf` / `StereoLinkVad.sys` / `stereolinkvad.cat` / `LICENSE-MS-PL.txt`）
由 `.scratch/vad/` 下的构建流水线生成（该目录是本机开发目录，不进仓库）。仓库里如果存在
`drivers\pkg-stereolink\`，安装脚本会优先用仓库里的包；否则回退到 `.scratch\vad\pkg-stereolink\`。

## 4. 构建与自签（开发机）

需要 WDK 10.0.26100.0（`signtool.exe`、`Inf2Cat.exe`、`devcon.exe`）与 VS 生成工具：

```powershell
cd .scratch\vad
powershell -File .\drivers\build.ps1 -Config Release -BinName StereoLinkVad
powershell -File .\make_package.ps1        # 生成 pkg-stereolink\
powershell -File .\sign-vad.ps1            # 自签证书 + Inf2Cat + signtool 签名
powershell -File .\install-vad.ps1         # 信任证书 + 暂存驱动 + 创建设备（不含 bcdedit）
```

签名用自签开发证书（`CN=StereoLink Virtual Audio Driver (dev)`，有效期 2026–2031，
PFX 口令是本机开发口令）。自签证书只能过用户态的 `signtool verify /pa`；
内核加载要求驱动证书链到微软根，所以 `signtool verify /kp` 会报
`Signing Cert does not chain to a Microsoft Root Cert.`——这就是必须开测试签名的原因。

**改驱动内容时记得同时递增 INF 里的 `DriverVer`**：同版本号重装时 DriverStore 可能复用旧副本。

## 5. 安装（用户）

前提：Windows 10 / 11 x64、管理员权限、**Secure Boot 关闭**（开着的话测试签名驱动无法加载）。

1. 右键仓库根目录的 `安装虚拟声卡.cmd` → 「以管理员身份运行」，UAC 确认。
2. 脚本自动完成：
   - 检查 Secure Boot（开着会直接报错退出）；
   - 检查 `bcdedit` 里的 `testsigning`，没开就执行 `bcdedit /set testsigning on`；
   - 把自签证书导入「受信任的根证书颁发机构」和「受信任的发布者」（`certutil -addstore`）；
   - `pnputil /add-driver StereoLinkVad.inf /install` 暂存驱动包；
   - 如果没有设备实例，用 WDK 的 `devcon install StereoLinkVad.inf *StereoLinkVad`
     创建 root 设备节点（`pnputil` 只能装驱动，不能建 root 设备）；
   - 已有设备时跳过安装，只做状态检查。
3. 如果脚本提示需要重启：**重启电脑**（脚本不会自己重启）。重启后桌面右下角出现
   「测试模式」水印，属正常现象。
4. 重启后验证（脚本也可以再跑一次，会自动走「已存在设备」分支）：

```powershell
pnputil /enum-devices /instanceid "ROOT\MEDIA\0001"   # 期望 Status: Started
cargo run -q -p stereo-audio --bin endpoints           # 列表里应有「扬声器 (立体声链虚拟声卡)」
```

之后「设置 → 系统 → 声音 → 输出」里就能看到 **「扬声器 (立体声链虚拟声卡)」**，打开
`stereo-link.exe` 给左右声道各选一台设备即可。

> 说明：`devcon.exe` 不在系统 PATH 里，脚本按以下顺序查找：
> WDK `Tools\10.0.26100.0\x64\devcon.exe` → 仓库 `tools\devcon.exe` → 在 WDK 目录里搜任意
> `*\x64\devcon.exe`。驱动包目录也可以作为脚本的第一个参数传进来。
> UAC 被取消、或 Secure Boot 开着时，脚本只退出、不改系统。

## 6. 卸载

先退出 StereoLink 面板（停止路由、恢复默认输出），然后右键 `卸载虚拟声卡.cmd` →
「以管理员身份运行」。脚本会做：

1. 移除所有用 `StereoLinkVad` 服务的设备节点（`pnputil /remove-device`）；
2. 在驱动仓库里找到原始 INF 名是 `stereolinkvad.inf` 的包（发布名通常是 `oem19.inf`），
   执行 `pnputil /delete-driver <发布名> /uninstall /force`；
3. `sc delete StereoLinkVad` 删除内核服务；
4. 从「受信任的根证书颁发机构」和「受信任的发布者」里删除 StereoLink 自签证书；
5. 询问是否同时关闭 testsigning（`bcdedit /set testsigning off`）；
6. 提示重启（脚本不会自己重启）。

重启后「扬声器 (立体声链虚拟声卡)」会从输出列表消失，驱动文件、服务、证书都不在了。

手动卸载（等价命令，供高级用户复核）：

```powershell
pnputil /remove-device "ROOT\MEDIA\0001"
pnputil /delete-driver oem19.inf /uninstall /force   # 先确认 oem19.inf 是 stereolinkvad.inf！
sc.exe delete StereoLinkVad
bcdedit /set testsigning off                         # 可选
# 重启
```

## 7. testsigning 的代价与撤销

- **桌面水印**：右下角「测试模式」字样，只影响观感。
- **Secure Boot**：开测试签名要求 Secure Boot 关闭；以后重新打开 Secure Boot 后驱动会加载失败。
- **安全模型**：测试签名模式允许加载自签内核驱动，属于开发/自用阶段的折中。
- 未来走向正式发布时，应改用 EV 代码签名证书 + 微软 attestation/WHQL 签名（周期长、要花钱，
  但没有这些限制）。

关闭 testsigning：`bcdedit /set testsigning off`，重启后生效（驱动随即不再加载）。

## 8. 实例 id、端点名与改名

| 项目 | 值 |
| --- | --- |
| 设备实例 id | `ROOT\MEDIA\0001` |
| 内核服务 / 驱动文件 | `StereoLinkVad` / `C:\Windows\System32\drivers\StereoLinkVad.sys` |
| 默认端点名 | **「扬声器 (立体声链虚拟声卡)」** |
| 端点名来源 | 「扬声器」前缀由 Windows 按设备类型本地化；括号里是设备名，来自 INF `[Strings]` 的 `Wave.szPname` / `Topo.szPname`（以及 `HKR,,FriendlyName`） |

**在面板里改名**：虚拟声卡卡片 →「设备名」输入框 →「应用改名」。面板会释放内嵌的
`scripts\gui-set-name.ps1` 并调用（脚本自己弹 UAC），流程是：

1. 用 `reg export` 备份整个 `Enum\ROOT\MEDIA\0001` 键到 `%TEMP%\stereo-link\backups\`；
2. 写入 `HKLM\SYSTEM\CurrentControlSet\Enum\ROOT\MEDIA\0001` 的 `FriendlyName`；
3. 重启 `AudioEndpointBuilder`（约 2–4 秒音频中断），轮询读回校验；
4. 面板刷新设备列表。

校验规则：名字 1–64 个字符、不能含 `\ / : * ? " < > |`、不能全是空格；不合法时面板红字提示、
不会发起提权。UAC 取消则什么都不改；日志在 `%TEMP%\stereo-link\rename.log`。

`scripts\rename-endpoint.ps1` 是通用命令行改名工具（支持 `-Rollback` 回滚、`-DryRun` 试运行、
备份到 `scripts\backups\`），它会额外 disable/enable 一次设备，在 Win11 上可能留下 pending-reboot，
面板不直接用它；一般不需要手动跑。

> 改名只改注册表里的运行时设备名，不改 INF：重装驱动后设备名会回到「立体声链虚拟声卡」。
> 想永久改默认名，要改 INF 的 `Wave.szPname` / `Topo.szPname` 并递增 `DriverVer`，重新打包、
> 签名、安装。

## 9. render-only 与 WASAPI 回环

| 项目 | 结论 |
| --- | --- |
| 端点类型 | 仅 render（播放），没有录音端点 |
| 回环采集 | 已验证可用（实测连续采集 83759 帧、峰值 0.8014） |
| 采样格式 | 共享格式 48 kHz / 2 ch / 32 bit float；StereoLink 内部转 44.1 kHz / 16 bit 后路由 |
| 用户态开销 | 无驱动侧混音线程；只在有程序播放时占 CPU，静音时基本为 0 |

## 10. 与 Steam Streaming Speakers 的关系

早期版本没有自己的驱动，临时借用了 Steam 随客户端安装的已签名虚拟声卡
（Steam Streaming Speakers），只改设备显示名。这条路线的缺点（不属于我们、Steam 卸载后端点
消失、名字只是表面功夫）已经被自研驱动取代，**现在不再需要 Steam**：

- 旧 Steam 设备 `ROOT\STEAMSTREAMINGSPEAKERS\0000` 的显示名已恢复成原厂的
  `Steam Streaming Speakers`，并保持停用状态；
- 不要再启用、改名或删除 Steam 的 `SteamStreamingSpeakers.sys` / `SteamStreamingMicrophone.sys`
  及其设备——那是 Steam 自己的东西，和 StereoLink 无关；
- 系统里现在应该只有一台 StereoLink 提供的虚拟声卡。

## 11. 常见问题

| 问题 | 处理 |
| --- | --- |
| 输出列表里没有「扬声器 (立体声链虚拟声卡)」 | 管理员运行 `安装虚拟声卡.cmd`，按提示重启；重启后仍没有就跑 `pnputil /enum-devices /instanceid "ROOT\MEDIA\0001"` 看状态 |
| 设备管理器里 StereoLinkVad 带黄色叹号 / Code 52 | 说明测试签名没生效：确认 `bcdedit /enum {current}` 里有 `testsigning Yes`，改完要重启；Secure Boot 必须是关闭状态 |
| 桌面右下角出现「测试模式」水印 | testsigning 已开启的正常现象，不影响使用；卸载时可选关闭 |
| 安装时提示 Secure Boot 已开启 | 进 BIOS/UEFI 关闭 Secure Boot 再装；开 Secure Boot 时测试签名驱动无法加载 |
| 安装时提示找不到 devcon.exe | 安装 WDK 10.0.26100.0，或把 devcon.exe 放到仓库 `tools\devcon.exe` |
| 改名弹 UAC 后没反应 / 取消 | 取消 UAC 不会改任何东西；失败原因看 `%TEMP%\stereo-link\rename.log` |
| 改名后「声音」列表里还是旧名 | 等 2–4 秒并点面板「刷新设备」；必要时重启面板 |
| 卸载后列表里还有旧端点 | 重启一次让设备节点彻底消失；确认 `pnputil /enum-devices /instanceid "ROOT\MEDIA\0001"` 返回设备不存在 |
| 卸载后还想用 | 重新跑 `安装虚拟声卡.cmd`；如果 testsigning 还在，可以少重启一次 |
| 会不会和 Steam 的虚拟声卡冲突 | 不会：Steam 那台已停用并恢复原名，StereoLink 只用自己的 `ROOT\MEDIA\0001` |
| 驱动二进制在哪、是否随仓库分发 | 构建产物在 `.scratch\vad\pkg-stereolink\`；仓库目前不提交驱动二进制，按第 4 节自行构建签名 |

## 12. 注意

- 驱动属于自签开发件：只适合本机/自用，对外分发会让每台机器都要开测试签名，正式发布前应换成
  微软签名的驱动。
- MS-PL 要求保留许可证与版权声明；驱动包里已经带了 `LICENSE-MS-PL.txt`，不要删。
- 改格式、改名、装/卸驱动都会动系统音频栈，短暂中断音频是正常现象。
