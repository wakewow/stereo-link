# device-rename-test.ps1 —— task-8 设备级改名实验（可复现）
#
# 目的：验证「零驱动改名」的最后一条程序化路径：
#   端点显示名 = `{a45c254e-...},2`（驱动 pin 名，如「扬声器」）+ "(" + `{b3f8fa53-...},6`（设备接口名）+ ")"
#   对 root 枚举的虚拟音频设备，`{b3f8fa53-...},6` 来自设备自身的显示名（本机实测：该设备
#   **没有** FriendlyName 属性 → 名字来自 INF 的 DeviceDesc）。故尝试用
#   SetupDiSetDevicePropertyW(DEVPKEY_Device_FriendlyName) 给设备加一个 FriendlyName，
#   看它能否覆盖 INF DeviceDesc 并改变端点显示名。
#
# 三态：BEFORE（原样）→ AFTER-SET（改名）→ AFTER-ROLLBACK（回滚）
# 回滚：先尝试 reg delete 删掉新增的属性键（恢复「属性不存在」的原状），
#       若被拒则退回「把 FriendlyName 写成与 DeviceDesc 完全相同的文字」并在报告中标注残余。
#
# 用法：pwsh -File device-rename-test.ps1            # 完整跑（含回滚）
#       pwsh -File device-rename-test.ps1 -NoWrite   # 只读快照
param([switch]$NoWrite, [string]$NewName = '小米妙播设备')

$ErrorActionPreference = 'Continue'
$base = 'D:\Backup\Documents\deepseek-harness\default-workspace\miplay-bridge\research\vad'
$py   = 'D:\Backup\Documents\deepseek-harness\default-workspace\mixplay-pc-audio\capture\.venv\Scripts\python.exe'
$DEV  = 'ROOT\STEAMSTREAMINGSPEAKERS\0000'
$EP   = 'SWD\MMDEVAPI\{0.0.0.00000000}.{4a82bbbc-667f-4a9b-b4ce-520a165e6240}'
$EPG  = '{4a82bbbc-667f-4a9b-b4ce-520a165e6240}'
$PROPS = "HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Render\$EPG\Properties"
$PINNAME = '{a45c254e-df1c-4efd-8020-67d146a850e0},2'   # 驱动 pin 名（扬声器）
$FRIENDLY= '{a45c254e-df1c-4efd-8020-67d146a850e0},14'  # 端点友好名（本机初始不存在）
$IFACE   = '{b3f8fa53-0004-438e-9003-51a46e139bfc},6'    # 设备接口名（蒸汽音箱）
$LOGF = Join-Path $base 'device-rename-test.log'

Add-Type -Path (Join-Path $base 'DeviceRename.cs') -ErrorAction Stop

function Say($m) { $line = "[{0}] {1}" -f (Get-Date).ToString('HH:mm:ss.fff'), $m; Write-Host $line; Add-Content -Path $LOGF -Value $line }

function RegGet($name) {
    $v = (Get-ItemProperty -Path $PROPS -Name $name -ErrorAction SilentlyContinue).$name
    if ($null -eq $v) { return '<absent>' }
    return [string]$v
}

function Snap([string]$tag) {
    $o = [ordered]@{}
    $o.tag = $tag
    $o.ts  = (Get-Date).ToString('o')
    $o.device_friendly     = [MpvDev.DeviceTool]::GetFriendlyById($DEV)
    $dev = Get-PnpDevice -InstanceId $DEV -ErrorAction SilentlyContinue
    $o.device_pnp_status   = if ($dev) { [string]$dev.Status } else { '<none>' }
    $o.device_pnp_name     = if ($dev) { [string]$dev.FriendlyName } else { '<none>' }
    $ep = Get-PnpDevice -InstanceId $EP -ErrorAction SilentlyContinue
    $o.endpoint_pnp_name   = if ($ep) { [string]$ep.FriendlyName } else { '<none>' }
    $o.endpoint_pnp_status = if ($ep) { [string]$ep.Status } else { '<none>' }
    $o.reg_pinname  = RegGet $PINNAME
    $o.reg_friendly = RegGet $FRIENDLY
    $o.reg_iface    = RegGet $IFACE
    $json = & $py -u (Join-Path $base 'probe_endpoint_names.py') 2>$null
    $names = ($json | ConvertFrom-Json)
    $o.soundcard_speakers   = ($names.speakers -join ' | ')
    $o.soundcard_mics_steam = (($names.microphones | Where-Object { $_ -match 'Steam' }) -join ' | ')
    $o.device_props_key_exists = Test-Path "HKLM:\SYSTEM\CurrentControlSet\Enum\$DEV\Properties\{a45c254e-df1c-4efd-8020-67d146a850e0}"
    return [pscustomobject]$o
}

function Show([string]$tag, $s) {
    Say ($tag + ' | dev_friendly=' + $s.device_friendly + ' | dev_pnp=' + $s.device_pnp_name + '/' + $s.device_pnp_status +
         ' | ep_pnp=' + $s.endpoint_pnp_name + '/' + $s.endpoint_pnp_status +
         ' | reg_pinname=' + $s.reg_pinname + ' | reg_,14=' + $s.reg_friendly + ' | reg_iface=' + $s.reg_iface +
         ' | props_key=' + $s.device_props_key_exists)
}

$states = @()
Say ('=== device-rename-test START (NoWrite=' + $NoWrite + ' NewName=' + $NewName + ') ===')
Say ('device=' + $DEV)
Say ('endpoint=' + $EP)
$s0 = Snap 'BEFORE'; $states += $s0; Show 'BEFORE' $s0

if (-not $NoWrite) {
    Say ('--- SET FriendlyName = ' + $NewName + ' ---')
    Say ([MpvDev.DeviceTool]::SetFriendly($DEV, $NewName))
    Start-Sleep -Seconds 2
    $s1 = Snap 'AFTER-SET'; $states += $s1; Show 'AFTER-SET' $s1

    if ($s1.endpoint_pnp_name -eq $s0.endpoint_pnp_name) {
        Say 'endpoint name unchanged after set -> device restart (disable/enable)'
        Say ([MpvDev.DeviceTool]::Restart($DEV))
        Start-Sleep -Seconds 5
        $s1r = Snap 'AFTER-SET-RESTART'; $states += $s1r; Show 'AFTER-SET-RESTART' $s1r
        Say ('AFTER-SET-RESTART soundcard_speakers=' + $s1r.soundcard_speakers)
    }

    Say '--- ROLLBACK: try reg delete Enum\...\Properties\{a45c254e-...} ---'
    $pk = 'HKLM\SYSTEM\CurrentControlSet\Enum\' + $DEV + '\Properties\{a45c254e-df1c-4efd-8020-67d146a850e0}'
    $del = reg delete $pk /f 2>&1
    Say ('reg delete -> exit=' + $LASTEXITCODE + ' out=' + $del)
    if ($LASTEXITCODE -ne 0) {
        Say "delete denied -> fallback: FriendlyName := original INF DeviceDesc 'Steam Streaming Speakers'"
        Say ([MpvDev.DeviceTool]::SetFriendly($DEV, 'Steam Streaming Speakers'))
    }
    Say ([MpvDev.DeviceTool]::Restart($DEV))
    Start-Sleep -Seconds 5
    $s2 = Snap 'AFTER-ROLLBACK'; $states += $s2; Show 'AFTER-ROLLBACK' $s2
    Say ('AFTER-ROLLBACK soundcard_speakers=' + $s2.soundcard_speakers)
}

$states | ConvertTo-Json -Depth 5 | Set-Content -Path (Join-Path $base 'device-rename-states.json') -Encoding UTF8
Say '=== device-rename-test END; states -> device-rename-states.json ==='