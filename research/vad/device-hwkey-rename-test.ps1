# device-hwkey-rename-test.ps1 —— 设备级改名的最后一条程序化路径（可复现）
#
# 背景（前序实测）：
#   - 端点注册表 MMDevices 键：reg add / Set-ItemProperty 均 Access denied（audiosrv ACL）
#   - 官方 MMDevice API：IPropertyStore::SetValue(FriendlyName) = E_ACCESSDENIED(0x80070005)
#   - SetupDiSetDevicePropertyW(DEVPKEY_Device_FriendlyName)：pid=14 属性**不存在**
#     （lastErr=1168 ERROR_NOT_FOUND；正对照 pid=2 DeviceDesc 可读 → 编组无误）
#   - 但设备硬件键 HKLM\SYSTEM\CurrentControlSet\Enum\ROOT\STEAMSTREAMINGSPEAKERS\0000
#     ACL = BUILTIN\Administrators FullControl，DeviceDesc = @oem41.inf,...;Steam Streaming Speakers
#
# 本脚本：直接写硬件键的 FriendlyName（设备级改名的注册表形态），按
#   「写后不重启 → 重启设备(disable/enable)」两阶段观测端点显示名，最后**删除该值**回滚
#   （原始状态就是「该值不存在」，所以删除才是真回滚），并做三态比对。
#
# 踩坑记录：本文件是 UTF-8 无 BOM，PowerShell 5.1 按 GBK 解码 → 中文字面量乱码，故名字用码点拼装。
# 踩坑记录：reg.exe 要的是 'HKLM\...' 而不是 PowerShell 提供程序路径 'HKLM:\...'。
#
# 用法：powershell -NoProfile -ExecutionPolicy Bypass -File device-hwkey-rename-test.ps1
#       powershell ... -File device-hwkey-rename-test.ps1 -NoWrite     # 只读快照
param([switch]$NoWrite, [string]$NewName)

$ErrorActionPreference = 'Continue'
if (-not $NewName) { $NewName = -join ([char]0x5C0F, [char]0x7C73, [char]0x5999, [char]0x64AD, [char]0x8BBE, [char]0x5907) }  # 小米妙播设备
$base = 'D:\Backup\Documents\deepseek-harness\default-workspace\miplay-bridge\research\vad'
$py   = 'D:\Backup\Documents\deepseek-harness\default-workspace\mixplay-pc-audio\capture\.venv\Scripts\python.exe'
$DEV     = 'ROOT\STEAMSTREAMINGSPEAKERS\0000'
$DEVPATH = 'HKLM:\SYSTEM\CurrentControlSet\Enum\ROOT\STEAMSTREAMINGSPEAKERS\0000'   # PowerShell 提供程序用
$DEVREG  = 'HKLM\SYSTEM\CurrentControlSet\Enum\ROOT\STEAMSTREAMINGSPEAKERS\0000'    # reg.exe 用
$EP   = 'SWD\MMDEVAPI\{0.0.0.00000000}.{4a82bbbc-667f-4a9b-b4ce-520a165e6240}'
$EPG  = '{4a82bbbc-667f-4a9b-b4ce-520a165e6240}'
$PROPS = "HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Render\$EPG\Properties"
$IFACE = '{b3f8fa53-0004-438e-9003-51a46e139bfc},6'
$LOGF = Join-Path $base 'device-hwkey-rename-test.log'
$BAK  = Join-Path $base 'backup-Enum-ROOT-STEAMSTREAMINGSPEAKERS.reg'

Add-Type -Path (Join-Path $base 'DeviceRename.cs') -ErrorAction Stop
function Say($m) { $line = '[' + (Get-Date).ToString('HH:mm:ss.fff') + '] ' + $m; Write-Host $line; Add-Content -Path $LOGF -Value $line -Encoding UTF8 }
function RegGet($p, $n) { $v = (Get-ItemProperty -Path $p -Name $n -ErrorAction SilentlyContinue).$n; if ($null -eq $v) { return '<absent>' } return [string]$v }
function RegExists($p, $n) { return $null -ne (Get-ItemProperty -Path $p -Name $n -ErrorAction SilentlyContinue).$n }

function Snap([string]$tag) {
    $o = [ordered]@{ tag = $tag; ts = (Get-Date).ToString('o') }
    $o.hwkey_friendlyname = RegGet $DEVPATH 'FriendlyName'
    $o.hwkey_value_exists = RegExists $DEVPATH 'FriendlyName'
    $o.setupdi_friendly  = [MpvDev.DeviceTool]::GetFriendlyById($DEV)
    $dev = Get-PnpDevice -InstanceId $DEV -ErrorAction SilentlyContinue
    $o.device_pnp_name   = if ($dev) { [string]$dev.FriendlyName } else { '<none>' }
    $o.device_pnp_status = if ($dev) { [string]$dev.Status } else { '<none>' }
    $ep = Get-PnpDevice -InstanceId $EP -ErrorAction SilentlyContinue
    $o.endpoint_pnp_name   = if ($ep) { [string]$ep.FriendlyName } else { '<none>' }
    $o.endpoint_pnp_status = if ($ep) { [string]$ep.Status } else { '<none>' }
    $o.reg_pinname = RegGet $PROPS '{a45c254e-df1c-4efd-8020-67d146a850e0},2'
    $o.reg_epfriend= RegGet $PROPS '{a45c254e-df1c-4efd-8020-67d146a850e0},14'
    $o.reg_iface   = RegGet $PROPS $IFACE
    $names = (& $py -u (Join-Path $base 'probe_endpoint_names.py') 2>$null | ConvertFrom-Json)
    $o.soundcard_speakers = ($names.speakers -join ' | ')
    return [pscustomobject]$o
}
function Show($tag, $s) {
    Say ($tag + ' | hw.FriendlyName="' + $s.hwkey_friendlyname + '" exists=' + $s.hwkey_value_exists +
         ' | setupdi="' + $s.setupdi_friendly + '" | dev_pnp=' + $s.device_pnp_name + '/' + $s.device_pnp_status +
         ' | EP_pnp=' + $s.endpoint_pnp_name + '/' + $s.endpoint_pnp_status +
         ' | reg_pin=' + $s.reg_pinname + ' reg_epFriend=' + $s.reg_epfriend + ' reg_iface=' + $s.reg_iface)
}

$states = @()
Say ('=== hwkey-rename-test START (NoWrite=' + $NoWrite + ') NewName="' + $NewName + '" len=' + $NewName.Length + ' ===')
if (-not (Test-Path $BAK)) { $e = reg export 'HKLM\SYSTEM\CurrentControlSet\Enum\ROOT\STEAMSTREAMINGSPEAKERS' $BAK /y 2>&1; Say ('reg export -> exit=' + $LASTEXITCODE + ' ' + $e) }
Say ('backup: exists=' + (Test-Path $BAK) + ' size=' + (Get-Item $BAK -ErrorAction SilentlyContinue).Length)
Say ('raw before: ' + ((reg query $DEVREG /v FriendlyName 2>&1) -join ' / '))

$s0 = Snap 'BEFORE'; $states += $s0; Show 'BEFORE' $s0

if (-not $NoWrite) {
    Say ('--- WRITE hw key FriendlyName = "' + $NewName + '" (REG_SZ) ---')
    $w = reg add $DEVREG /v FriendlyName /t REG_SZ /d $NewName /f 2>&1
    Say ('reg add -> exit=' + $LASTEXITCODE + ' ' + $w)
    Say ('raw after write: ' + ((reg query $DEVREG /v FriendlyName 2>&1) -join ' / '))
    Start-Sleep -Seconds 2
    $s1 = Snap 'AFTER-WRITE'; $states += $s1; Show 'AFTER-WRITE' $s1

    Say '--- restart device (disable/enable) ---'
    Say ([MpvDev.DeviceTool]::Restart($DEV))
    Start-Sleep -Seconds 6
    $s2 = Snap 'AFTER-WRITE-RESTART'; $states += $s2; Show 'AFTER-WRITE-RESTART' $s2
    Say ('AFTER-WRITE-RESTART soundcard_speakers=' + $s2.soundcard_speakers)

    Say '--- ROLLBACK: delete the FriendlyName value (original state = value absent) ---'
    $r = reg delete $DEVREG /v FriendlyName /f 2>&1
    Say ('reg delete -> exit=' + $LASTEXITCODE + ' ' + $r)
    if ($LASTEXITCODE -ne 0) { $r2 = reg add $DEVREG /v FriendlyName /t REG_SZ /d "" /f 2>&1; Say ('fallback reg add empty -> exit=' + $LASTEXITCODE + ' ' + $r2) }
    Start-Sleep -Seconds 2
    Say ([MpvDev.DeviceTool]::Restart($DEV))
    Start-Sleep -Seconds 6
    $s3 = Snap 'AFTER-ROLLBACK'; $states += $s3; Show 'AFTER-ROLLBACK' $s3
    Say ('AFTER-ROLLBACK soundcard_speakers=' + $s3.soundcard_speakers)
    Say ('raw after rollback: ' + ((reg query $DEVREG /v FriendlyName 2>&1) -join ' / '))
    Say ('=== THREE-STATE === before_EP="' + $s0.endpoint_pnp_name + '" | after_EP="' + $s1.endpoint_pnp_name +
         '" | afterRestart_EP="' + $s2.endpoint_pnp_name + '" | rollback_EP="' + $s3.endpoint_pnp_name + '"')
    Say ('=== THREE-STATE(device) === before="' + $s0.device_pnp_name + '" | after="' + $s1.device_pnp_name +
         '" | afterRestart="' + $s2.device_pnp_name + '" | rollback="' + $s3.device_pnp_name + '"')
}
$states | ConvertTo-Json -Depth 5 | Set-Content -Path (Join-Path $base 'device-hwkey-rename-states.json') -Encoding UTF8
Say '=== hwkey-rename-test END ==='