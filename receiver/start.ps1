# start.ps1 —— 一键启动「手机 → 电脑出声」接收端
#
#   .\start.ps1                          # 同时开 AirPlay + 妙播
#   .\start.ps1 -Mode airplay            # 只开 AirPlay（iPhone）
#   .\start.ps1 -Mode airplay -Ap1       # AirPlay 只走 AP1 实时流（延迟更低，看视频用）
#   .\start.ps1 -Mode miplay             # 只开妙播（小米手机）
#   .\start.ps1 -Device "#10"            # 指定声卡（序号用 --list-devices 查；也可写完整 id）
#   .\start.ps1 -Name "书房电脑"           # 投放列表里显示的名字
#   .\start.ps1 -Release                 # 用 release（体积小、启动快）；首次会自动编译
#
# 提示：请用 PowerShell 7（pwsh）运行；加防火墙规则需要管理员权限，
#       非管理员时脚本会跳过加规则并提醒你手动放行。

param(
    [ValidateSet("airplay", "miplay", "both")]
    [string]$Mode = "both",
    [string]$Name = "StereoLink",
    [string]$Device = "",
    [string]$Ip = "",
    [int]$Port = 5000,
    [switch]$Release,
    # AirPlay 默认 1 + 2 双支持；加 -Ap1 只走 AP1 实时流（延迟更低）。
    [switch]$Ap1
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $MyInvocation.MyCommand.Path
$cfg = if ($Release) { "release" } else { "debug" }

function Get-Exe([string]$crate) {
    Join-Path $root "target\$cfg\$crate.exe"
}

# ---------------------------------------------------------------- 1. 编译
$need = @()
if ($Mode -in @("airplay", "both") -and -not (Test-Path (Get-Exe "airplay-recv"))) { $need += "airplay-recv" }
if ($Mode -in @("miplay", "both") -and -not (Test-Path (Get-Exe "miplay-recv"))) { $need += "miplay-recv" }
if ($need.Count -gt 0) {
    Write-Host "首次运行，正在编译：$($need -join ', ')（首次约 1-2 分钟）…" -ForegroundColor Cyan
    $cargoArgs = @("build")
    if ($cfg -eq "release") { $cargoArgs += "--release" }
    foreach ($crate in $need) { $cargoArgs += @("-p", $crate) }
    Push-Location $root
    try { & cargo @cargoArgs } finally { Pop-Location }
    if ($LASTEXITCODE -ne 0) { throw "编译失败，请检查上面的 cargo 输出" }
}

# ---------------------------------------------------------------- 2. 防火墙
# 入站放行（只对本程序）；没有管理员权限时只提醒，不中断。
$rules = @()
if ($Mode -in @("airplay", "both")) { $rules += @{ Name = "StereoLink airplay-recv"; Exe = (Get-Exe "airplay-recv") } }
if ($Mode -in @("miplay", "both")) { $rules += @{ Name = "StereoLink miplay-recv"; Exe = (Get-Exe "miplay-recv") } }

foreach ($rule in $rules) {
    $existing = Get-NetFirewallRule -DisplayName $rule.Name -ErrorAction SilentlyContinue
    if ($existing) { continue }
    try {
        New-NetFirewallRule -DisplayName $rule.Name -Direction Inbound -Action Allow `
            -Program $rule.Exe -Profile Private, Public -Protocol Any | Out-Null
        Write-Host "已添加防火墙入站放行：$($rule.Name)" -ForegroundColor DarkGray
    } catch {
        Write-Host "⚠️  无法添加防火墙规则（需要管理员权限）。" -ForegroundColor Yellow
        Write-Host "   请用管理员 PowerShell 执行一次：" -ForegroundColor Yellow
        Write-Host "   New-NetFirewallRule -DisplayName '$($rule.Name)' -Direction Inbound -Action Allow -Program '$($rule.Exe)' -Profile Private,Public -Protocol Any" -ForegroundColor Yellow
    }
}

# ---------------------------------------------------------------- 3. 启动
# 各自独立窗口：关掉窗口 = 停掉那个接收端。
# 这里用 cmd 的 start 而不是 Start-Process：某些机器（比如通过代理/封装启动
# PowerShell 时）环境变量里会同时存在 HTTP_PROXY 与 http_proxy，
# Start-Process 会因此抛「已添加了具有相同键的项」，start 没有这个限制。
$common = @()
if ($Name) { $common += @("--name", $Name) }
if ($Device) { $common += @("--device", $Device) }

function Start-ReceiverWindow([string]$Exe, [string[]]$Arguments, [string]$Title) {
    $line = 'start "' + $Title + '" "' + $Exe + '"'
    foreach ($arg in $Arguments) { $line += ' "' + $arg + '"' }
    & cmd.exe /c $line
}

if ($Mode -in @("airplay", "both")) {
    $a = @()
    if ($Ip) { $a += @("--bind", $Ip) }
    $a += @("--port", "$Port") + $common
    if ($Ap1) { $a += "--ap1" }
    Start-ReceiverWindow (Get-Exe "airplay-recv") $a "AirPlay 接收端"
}

if ($Mode -in @("miplay", "both")) {
    $m = @()
    if ($Ip) { $m += @("--ip", $Ip) }
    $m += $common
    Start-ReceiverWindow (Get-Exe "miplay-recv") $m "妙播接收端"
}

Write-Host ""
Write-Host "已启动（$Mode）。手机端：" -ForegroundColor Green
Write-Host "  iPhone：控制中心 → 音频输出（隔空播放）→ 选「$Name」"
Write-Host "  小米/红米：控制中心 → 小米妙播 → 选「$Name」"
Write-Host "  播放没声音？先在这场窗口里确认声卡，再检查手机和电脑是否同一 Wi-Fi/局域网。"
