# 产品硬性要求：Windows 防火墙必须放行 UDP 入站

> **这是 AirPlay 2 发送端在 Windows 上能否工作的决定性条件。** 没有这条放行，
> 一切看起来都像「网络超时」，而实际上协议、配对、加密、报文全是对的。

## 症状

`AirPlaySender::connect()` 在 **StreamSetup** 那一步挂住，直到读超时：

```text
Session("AirPlay 协议错误：io error: ... (os error 10060)")
```

而**同一个设备**用 pyatv（Python）推流完全正常 —— 因为 `python.exe` 早就在防火墙放行名单里，
我们自己的 `tone.exe` / `miplay-bridge.exe` 不在。

## 根因

AirPlay 2 接收端会**扣住 SETUP 的回复，直到它自己发出的 32 字节 NTP 时序包被应答**
（cidermill 在 Apple TV 上的实测结论，本项目在 L16A 上复现一致）。

时序包是**音箱主动发回发送端进程**的 UDP 报文。Windows 默认**阻止入站**，
于是这个包被丢弃 → 音箱永远等不到应答 → 永远不回 SETUP → 我方读到 10060。

> 注意：这不是「音箱连不上我们」，而是「**我们收不到音箱的回包**」。
> 所以只放行出站、或用 TCP 规则，都**没有用**，必须是 **UDP 入站**。

## 修复

```powershell
New-NetFirewallRule -DisplayName 'miplay-bridge' -Direction Inbound -Action Allow `
  -Protocol UDP -Program '<miplay-bridge.exe 的绝对路径>' -Profile Private,Public
```

实测：加规则后 **StreamSetup 立刻 200（73 ms）**，整条链路打通。

## 实测证据（加规则之后，本机 → 192.168.1.21:7000）

| 指标 | 值 |
|---|---|
| RTSP 步骤 | `StreamSetup 200` / `Feedback 200` / `Record 200` / `Flush 200` |
| 建流耗时 | 585 ms（含 HAP transient 配对） |
| 喂入 / 实发音频 | 30.01 s / **30.04 s** |
| 包 / 帧 / 字节 | 3764 包 / 1,324,928 帧 / 5,450,272 字节 |
| 平均发包率 | **125.4 包/秒**（理论 125.3） |
| 丢弃字节 | **0** |
| 欠载包 | 15（30 秒内，可接受） |
| 同步包 | 31 |
| TEARDOWN | 78 ms |
| 延迟预算 | 排队 0 + 声明 250 + 设备渲染 ~100（估计）= **约 350 ms**（目标 ≤400 ms） |
| **人耳** | **用户确认「听到了」** |

## 对产品的影响

1. `miplay-bridge.exe` 首次运行时**必须**请求/安装这条 UDP 入站规则，否则 AirPlay 路径对用户表现为「连不上音箱」。
   **不能只在文档里写** —— 用户不会去查。
2. 规则要绑**程序路径**（不是端口），因为发送端用的是**临时端口**（本次 data=37204、control=52642）。
3. 首次安装需要提权；应在 README 与安装脚本里明确说明。

## 复现与清理

```powershell
# 查看
Get-NetFirewallRule -DisplayName 'miplay-bridge*' | Format-Table DisplayName,Enabled,Direction,Action,Profile

# 删除临时探测规则（队友做 task-10 时加的）
Get-NetFirewallRule -DisplayName "miplay-bridge probe*" | Remove-NetFirewallRule
```
