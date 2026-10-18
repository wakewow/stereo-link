# 采集源与虚拟声卡：把系统声音送进 StereoLink 的那一路「无声」输出

> 一句话：StereoLink 从**一个「不会被听见」的本机输出端点**的 WASAPI 回环流里取声。
> 这个端点可以是任意虚拟音频设备（自研 StereoLink 虚拟声卡 / UU远程 / Steam Streaming），
> 也可以是一个没接音箱的物理输出。**零安装就能用**；自研虚拟声卡是可选的**高级路径**
> （需要管理员 + 开测试签名 + 重启一次，Secure Boot 必须关闭，桌面常驻「测试模式」水印）。
> 面板**不再提供虚拟声卡改名**。

## 1. 这条链路怎么走

```text
Windows 播放器（浏览器 / 音乐播放器 / 视频 / 游戏 …）
        │  默认输出 = 采集源
        ▼
采集源端点（任选一个「不会被听见」的本机输出）
  ├─ 虚拟音频设备：自研 StereoLink 虚拟声卡 / UU远程虚拟音频设备 / Steam Streaming
  └─ 空闲物理输出：板载扬声器口 / 空着的 HDMI、DP
        │  WASAPI loopback 采集（44.1 kHz / 2 ch / 16 bit）
        ▼
StereoLink 路由引擎（每 10 ms 一块）
        ├── 左声道 → 设备 A（如 Xiaomi Sound-1695）
        └── 右声道 → 设备 B（如 Xiaomi Sound-5320）
```

播放器不需要任何配置：只要 Windows 默认输出选到采集源（面板开始路由时会自动切），声音就会
流进分离链路。

## 2. 为什么需要一路「不会被听见」的音频

StereoLink 要在**系统级**拆左右声道，就必须先把所有播放器的声音汇到同一个端点：

- 播放器只会往 **Windows 默认输出**送声，用户态程序没法让任意播放器直接写给自己；
- 通用的取声方式是 **WASAPI 回环（loopback）**：在音频引擎里复制某个 render 端点的混音输出，
  不需要该端点真的接音箱；
- 所以采集源必须是一个**正常的 render（播放）端点**，并且开始路由时**同时是 Windows 默认输出**；
- 又因为这一路只是为了取声，采集源本身**不能真的出声**——否则用户会听到两遍（原声 + 蓝牙那路）
  或者从机箱喇叭漏音。这就是为什么推荐虚拟音频设备，或没接音箱的物理输出。

采集源不需要专门驱动，也不需要「录音端点」：WASAPI 回环是音频引擎侧能力，对任意 render 端点
都成立。自研虚拟声卡只是把「一个干净的、不会被听见的 render 端点」做得更确定。

## 3. 三种可用的采集源

| 类型 | 例子 | 说明 |
| --- | --- | --- |
| 虚拟音频设备（推荐） | 「扬声器 (立体声链虚拟声卡)」（自研驱动）、「扬声器 (UU远程虚拟音频设备)」、「扬声器 (Steam Streaming Speakers)」 | 无声，不会重复出声，也不会漏音；类别徽标显示 **`虚拟声卡（无声，推荐）`** |
| 空闲物理输出 | 板载 Realtek 扬声器口（没插音箱）、空着的 HDMI / DP 输出、没接设备的 USB 声卡 | 系统认为它是正常输出、可以回环采集；只要实际没接扬声器就不会出声。类别徽标显示 **`物理输出（可能出声）`** |
| 蓝牙音箱 / 已接音箱的输出 | 正在用的蓝牙音箱、插着耳机的声卡 | **不要选**：会重复出声（原声 + 蓝牙链路），类别徽标显示 **`蓝牙音箱（会重复出声，别选）`** |

采集源下拉框会给每个端点标上类别，鼠标悬停可看完整端点名。同名端点（例如两块同名显卡的
HDMI 输出）按端点 id 精确匹配，面板不会选错。

## 4. UU 远程兜底

本机既没有虚拟音频设备、也没有空闲物理输出时，用 UU远程临时借一个虚拟音频设备：

1. 安装并运行一次 UU远程（免费软件，会装好一个虚拟音频设备）；
2. 回到 StereoLink 面板点「刷新设备」；
3. 在「采集源」下拉里选 **「扬声器 (UU远程虚拟音频设备)」**；
4. 点「开始路由」即可。

面板在采集源不是虚拟设备时会给出琥珀色提示：已经检测到 UU远程就直接提示选它；没检测到就提示
先运行一次 UU远程。UU远程只是提供端点，不需要一直开着串流功能。

优点：零成本、零重启、不用开测试签名。注意：它是第三方软件，装 / 卸由用户自己决定；面板只把
它当普通音频端点使用。

## 5. 可选：自研虚拟声卡 StereoLinkVad（高级路径）

**什么时候需要**：机器上一个虚拟音频设备都没有，又不想装第三方软件，或者希望采集端点由
StereoLink 自己提供、名字固定可预期。

**安装代价**（三个副作用，装之前先确认能接受）：

| 副作用 | 说明 |
| --- | --- |
| 开启测试签名（testsigning） | 自签内核驱动必须靠它加载；桌面右下角会常驻「测试模式」水印 |
| Secure Boot 必须关闭 | 开着 Secure Boot 时测试签名驱动无法加载；安装脚本检测到会直接退出 |
| 管理员权限 + 重启一次 | 安装 / 卸载都要「以管理员身份运行」，且都要重启一次才能生效 |

> 只想要零安装路线的话，本节可以整段跳过。

### 5.1 驱动来源与许可

| 项目 | 说明 |
| --- | --- |
| 上游 | Scream（<https://github.com/duncanthrax/scream>，commit `d789743c248b11d1df7e5ecc546b1bc60b90cd91`），再往上是微软 WDK 的 MSVAD 示例 |
| 许可证 | MS-PL（Microsoft Public License） |
| 我们改了什么 | 设备 / 端点显示名改成「立体声链虚拟声卡」、硬件 id 改为 `*StereoLinkVad`、内核服务名改为 `StereoLinkVad`、补充构建 / 签名 / 安装脚本与 INF 注释 |
| 再分发 | MS-PL 允许改名与再分发（含二进制），但必须保留版权与许可声明；`LICENSE-MS-PL.txt` 全文随驱动包一起放了 |
| 商标 | MS-PL 不授予商标许可：`Scream` 只能作为出处说明出现，不能当产品名 |

驱动本体落地为 `C:\Windows\System32\drivers\StereoLinkVad.sys`（约 37 KB，内核态运行），
服务名 `StereoLinkVad`，设备类 `MEDIA`，实例 id **`ROOT\MEDIA\0001`**，端点显示名
**「扬声器 (立体声链虚拟声卡)」**。

驱动包（`StereoLinkVad.inf` / `StereoLinkVad.sys` / `stereolinkvad.cat` / `LICENSE-MS-PL.txt`）
由 `.scratch/vad/` 下的构建流水线生成（该目录是本机开发目录，不进仓库）。仓库里如果存在
`drivers\pkg-stereolink\`，安装脚本会优先用仓库里的包；否则回退到 `.scratch\vad\pkg-stereolink\`。

### 5.2 构建与自签（开发机）

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

### 5.3 安装（用户）

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
`stereo-link.exe`，在「采集源」里选它即可。

> 说明：`devcon.exe` 不在系统 PATH 里，脚本按以下顺序查找：
> WDK `Tools\10.0.26100.0\x64\devcon.exe` → 仓库 `tools\devcon.exe` → 在 WDK 目录里搜任意
> `*\x64\devcon.exe`。驱动包目录也可以作为脚本的第一个参数传进来。
> UAC 被取消、或 Secure Boot 开着时，脚本只退出、不改系统。

### 5.4 卸载

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

### 5.5 testsigning 的代价与撤销

- **桌面水印**：右下角「测试模式」字样，只影响观感。
- **Secure Boot**：开测试签名要求 Secure Boot 关闭；以后重新打开 Secure Boot 后驱动会加载失败。
- **安全模型**：测试签名模式允许加载自签内核驱动，属于开发 / 自用阶段的折中。
- 未来走向正式发布时，应改用 EV 代码签名证书 + 微软 attestation/WHQL 签名（周期长、要花钱，
  但没有这些限制）。

关闭 testsigning：`bcdedit /set testsigning off`，重启后生效（驱动随即不再加载）。

### 5.6 render-only 与 WASAPI 回环

| 项目 | 结论 |
| --- | --- |
| 端点类型 | 仅 render（播放），没有录音端点 |
| 回环采集 | 已验证可用（自研虚拟声卡实测连续采集 83759 帧、峰值 0.8014） |
| 采样格式 | 共享格式 48 kHz / 2 ch / 32 bit float；StereoLink 内部转 44.1 kHz / 16 bit 后路由 |
| 用户态开销 | 无驱动侧混音线程；只在有程序播放时占 CPU，静音时基本为 0 |

## 6. 采集源、默认输出与恢复

采集源必须**同时是 Windows 默认输出**，否则播放器的声音不会流进它——这是整套方案里最容易踩的
一条。面板的处理：

- 「开始路由时自动切为默认输出（退出时切回）」默认勾选；开始路由时自动切，停止 / 正常退出时
  切回原来的设备；
- 顶部状态和「采集源」卡都会显示当前默认输出是不是采集源（一致时带 √）；
- 也可以手动点「切为默认输出」「恢复原输出」；
- 切换前的默认输出端点 id 记在 `capture.previous_output`；异常退出后下次打开可利用
  `capture.last_real_output` 兜底恢复；
- **强杀 / 崩溃 / 断电不会自动恢复**，需要在「设置 → 系统 → 声音」里手动切回，或下次打开面板点
  「恢复原输出」。

手动验证：开始路由后打开「设置 → 系统 → 声音」，默认输出应显示为你的采集源；此时用任意播放器
放歌，面板「声道路由」里的两路电平应随之跳动。

## 7. 不再提供改名

面板**不再提供虚拟声卡改名**：早期版本的「设备名」输入框、「应用改名」按钮及相关代码
（`virtual_card.rs`）已删除。原因很简单——改名只影响端点在 Windows 列表里的显示名，对采集、
路由、延迟都没有影响，却需要 UAC 提权并短暂中断音频，收益不值。

- 虚拟声卡端点名固定为 **「扬声器 (立体声链虚拟声卡)」**；其中「扬声器」前缀由 Windows 按设备
  类型本地化，括号里是设备名，来自驱动 INF `[Strings]` 的 `Wave.szPname` / `Topo.szPname`。
- 确实要改默认名：改 INF 的 `Wave.szPname` / `Topo.szPname` 并递增 `DriverVer`，重新打包、
  签名、安装。
- 仓库里保留的 `scripts\gui-set-name.ps1`、`scripts\rename-endpoint.ps1` 只是底层开发工具，
  面板不调用，普通用户不需要用（`rename-endpoint.ps1` 会额外 disable/enable 一次设备，Win11 上
  可能留下 pending-reboot）。

## 8. 与 Steam Streaming Speakers 的关系

早期版本没有自己的驱动，临时借用了 Steam 随客户端安装的已签名虚拟声卡
（Steam Streaming Speakers），只改设备显示名。这条路线的缺点（不属于我们、Steam 卸载后端点
消失、名字只是表面功夫）已经被「任意端点 + 可选自研驱动」取代。现在 Steam 的那台设备已恢复原厂
显示名并保持停用：

- 不要启用、改名或删除 Steam 的 `SteamStreamingSpeakers.sys` / `SteamStreamingMicrophone.sys`
  及其设备——那是 Steam 自己的东西，和 StereoLink 无关；
- 如果你机器上恰好有 Steam Streaming Speakers，它可以像其它虚拟音频设备一样被选作采集源。

## 9. 常见问题

| 问题 | 处理 |
| --- | --- |
| 采集源下拉里不知道选哪个 | 选徽标为 `虚拟声卡（无声，推荐）` 的端点；其次是没接音箱 / 显示器的物理输出；别选蓝牙音箱 |
| 本机没有任何虚拟音频设备 | 运行一次 UU远程（免费）→ 面板点「刷新设备」→ 选「扬声器 (UU远程虚拟音频设备)」；或安装自研虚拟声卡（高级路径） |
| 采集源是物理输出，会不会出声 | 只要那个口没接音箱 / 显示器就不会；不确定就换虚拟音频设备 |
| 开始路由后没声音 | 确认「系统默认输出」与采集源一致（带 √）；不一致就点「切为默认输出」 |
| 输出列表里没有「扬声器 (立体声链虚拟声卡)」（装过自研驱动） | 管理员运行 `安装虚拟声卡.cmd`，按提示重启；重启后仍没有就跑 `pnputil /enum-devices /instanceid "ROOT\MEDIA\0001"` 看状态 |
| 设备管理器里 StereoLinkVad 带黄色叹号 / Code 52 | 说明测试签名没生效：确认 `bcdedit /enum {current}` 里有 `testsigning Yes`，改完要重启；Secure Boot 必须是关闭状态 |
| 桌面右下角出现「测试模式」水印 | testsigning 已开启的正常现象，不影响使用；卸载时可选关闭 |
| 安装时提示 Secure Boot 已开启 | 进 BIOS/UEFI 关闭 Secure Boot 再装；开 Secure Boot 时测试签名驱动无法加载 |
| 安装时提示找不到 devcon.exe | 安装 WDK 10.0.26100.0，或把 devcon.exe 放到仓库 `tools\devcon.exe` |
| 想改虚拟声卡显示名 | 面板已不提供改名（见第 7 节）；显示名不影响采集，一般不需要改 |
| 卸载后列表里还有旧端点 | 重启一次让设备节点彻底消失；确认 `pnputil /enum-devices /instanceid "ROOT\MEDIA\0001"` 返回设备不存在 |
| 卸载后还想用 | 重新跑 `安装虚拟声卡.cmd`；如果 testsigning 还在，可以少重启一次 |
| 会不会和 Steam 的虚拟声卡冲突 | 不会：两者是不同端点，选一个当采集源即可，不要同时启用；Steam 那台保持停用 |
| 驱动二进制在哪、是否随仓库分发 | 构建产物在 `.scratch\vad\pkg-stereolink\`；仓库目前不提交驱动二进制，按 5.2 节自行构建签名 |

## 10. 注意

- 驱动属于自签开发件：只适合本机 / 自用，对外分发会让每台机器都要开测试签名，正式发布前应换成
  微软签名的驱动。
- MS-PL 要求保留许可证与版权声明；驱动包里已经带了 `LICENSE-MS-PL.txt`，不要删。
- 改格式、装 / 卸驱动都会动系统音频栈，短暂中断音频是正常现象。
- 采集源必须同时是 Windows 默认输出；异常退出不会自动恢复原输出。
