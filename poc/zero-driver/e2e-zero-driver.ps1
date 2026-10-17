<#
.SYNOPSIS
  One-command end-to-end run for the zero-driver PoC (rename -> capture -> speaker
  -> rollback -> no-residue verification). Read-only unless you pass -Go.

.DESCRIPTION
  Phases (each one records objective evidence into a single JSON report):
    A  pre-state snapshot (device name, endpoint name, FriendlyName value, reg export)
    B  rename      (delegates to rename-endpoint.ps1, which backs up first)
    C  post-state  (device name, endpoint name, registry value, fresh-process endpoint list)
    D  capture     (capture_from_endpoint.py on the RENAMED endpoint, tone loopback)
    E  speaker     (push_to_speaker.py -> MiXPlay play_url; skipped with -SkipSpeaker)
    F  rollback    (rename-endpoint.ps1 -Rollback <backup>) + no-residue verification

  Safety:
    * Without -Go the script only prints the plan (nothing is written/renamed).
    * The rename is always preceded by a reg export of the whole instance key.
    * Rollback runs in a finally-style block even if phase D/E fail, unless -KeepRenamed.
    * It never touches the user's default playback device and never writes
      MMDevices\...\Properties (documented dead end).

.EXAMPLE
  # Just show what would happen
  powershell -NoProfile -ExecutionPolicy Bypass -File .\e2e-zero-driver.ps1

.EXAMPLE
  # Real run, 8 s tone, speaker push enabled
  powershell -NoProfile -ExecutionPolicy Bypass -File .\e2e-zero-driver.ps1 -Go -Seconds 8 -Volume 20

.EXAMPLE
  # Real run without the speaker hop (MiXPlay not running)
  powershell -NoProfile -ExecutionPolicy Bypass -File .\e2e-zero-driver.ps1 -Go -SkipSpeaker
#>
[CmdletBinding()]
param(
    [string]$DeviceInstanceId = 'ROOT\STEAMSTREAMINGSPEAKERS\0000',
    [string]$NewName          = '小米妙播设备',
    [string]$EndpointMatch    = '4A82BBBC-667F-4A9B-B4CE-520A165E6240',
    [string]$TargetDid        = '934947888',
    [string]$Python           = 'D:\Backup\Documents\deepseek-harness\default-workspace\mixplay-pc-audio\capture\.venv\Scripts\python.exe',
    [double]$Seconds          = 8,
    [int]$Volume              = 0,
    [switch]$SkipSpeaker,
    [switch]$KeepRenamed,
    [switch]$Go
)

$ErrorActionPreference = 'Stop'
try { [Console]::OutputEncoding = [Text.Encoding]::UTF8 } catch { }

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$BackupDir = Join-Path $ScriptDir 'backups'
if (-not (Test-Path $BackupDir)) { New-Item -ItemType Directory -Path $BackupDir | Out-Null }
$RenamePs1 = Join-Path $ScriptDir 'rename-endpoint.ps1'
$CapPy     = Join-Path $ScriptDir 'capture_from_endpoint.py'
$PushPy    = Join-Path $ScriptDir 'push_to_speaker.py'
$Stamp     = Get-Date -Format 'yyyyMMdd-HHmmss'
$ReportPath = Join-Path $BackupDir "e2e-$Stamp.json"
$Report = [ordered]@{
    started   = (Get-Date).ToString('s')
    mode      = if ($Go) { 'GO' } else { 'PLAN-ONLY' }
    device    = $DeviceInstanceId
    new_name  = $NewName
    target_did = $TargetDid
    seconds   = $Seconds
    phases    = [ordered]@{}
}

function Say([string]$m)  { Write-Host $m }
function Head([string]$m) { Write-Host ''; Write-Host "########## $m ##########" }
function Ok([string]$m)   { Write-Host "   [PASS] $m" -ForegroundColor Green }
function Bad([string]$m)  { Write-Host "   [FAIL] $m" -ForegroundColor Red }
function Warn([string]$m) { Write-Host "   [WARN] $m" -ForegroundColor Yellow }

function Get-EndpointName([string]$pattern) {
    $ep = Get-PnpDevice -Class AudioEndpoint -ErrorAction SilentlyContinue |
          Where-Object { $_.InstanceId -like "*$pattern*" } | Select-Object -First 1
    if (-not $ep) { return $null }
    return $ep.FriendlyName
}

function Get-HwFriendly([string]$inst) {
    $k = "HKLM:\SYSTEM\CurrentControlSet\Enum\$inst"
    $v = Get-ItemProperty -Path $k -Name FriendlyName -ErrorAction SilentlyContinue
    if ($null -eq $v -or -not ($v.PSObject.Properties.Name -contains 'FriendlyName')) { return '<absent>' }
    return [string]$v.FriendlyName
}

function Get-DeviceName([string]$inst) {
    $d = Get-PnpDevice -InstanceId $inst -ErrorAction SilentlyContinue
    if (-not $d) { return $null }
    return [string]$d.FriendlyName
}

function Get-Snapshot([string]$inst, [string]$pattern) {
    [ordered]@{
        device_name     = Get-DeviceName $inst
        device_status   = (Get-PnpDevice -InstanceId $inst -ErrorAction SilentlyContinue).Status
        endpoint_name   = Get-EndpointName $pattern
        hw_friendlyname = Get-HwFriendly $inst
    }
}

function Invoke-Py([string]$exe, [string[]]$argv, [string]$label) {
    # Native stderr (e.g. a harmless reg.exe "The operation completed successfully.")
    # becomes a terminating NativeCommandError while $ErrorActionPreference='Stop',
    # which used to abort the phase before the exit code could be inspected.
    $prev = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try { $out = & $exe @argv 2>&1 | Out-String -Width 400 } finally { $ErrorActionPreference = $prev }
    $code = $LASTEXITCODE
    Write-Host $out
    return @{
        exit  = $code
        label = $label
        text  = (($out -split "`r?`n" | Where-Object { $_.Trim() -ne '' } | Select-Object -Last 40) -join "`n")
    }
}

Say "MiPlay zero-driver PoC -- end-to-end runner"
Say "  device   : $DeviceInstanceId"
Say "  new name : $NewName"
Say "  endpoint : *$EndpointMatch*"
Say "  target   : did=$TargetDid   tone ${Seconds}s   volume=$(if ($Volume) { "$Volume%" } else { 'unchanged' })"
Say "  speaker  : $(if ($SkipSpeaker) { 'SKIPPED (-SkipSpeaker)' } else { 'push_to_speaker.py --backend miplay (miplay-bridge :8901)' })"
Say "  rollback : $(if ($KeepRenamed) { 'NO (-KeepRenamed)' } else { 'yes, always (unless a hard failure prevents it)' })"
Say "  report   : $ReportPath"

if (-not $Go) {
    Say ''
    Warn 'PLAN-ONLY mode: nothing will be written, renamed or played. Re-run with -Go to execute.'
    Say 'Plan:'
    Say "  A) snapshot current device/endpoint names + reg export baseline"
    Say "  B) powershell -File rename-endpoint.ps1 -DeviceInstanceId '$DeviceInstanceId' -NewName '$NewName'"
    Say "  C) verify device name + endpoint name changed (4 observation points)"
    Say "  D) & python capture_from_endpoint.py --endpoint-name '$NewName' --seconds $Seconds --play-tone 1000"
    if (-not $SkipSpeaker) {
        Say "  E) & python push_to_speaker.py --source capture --endpoint-name '$NewName' --target-did $TargetDid --seconds $Seconds"
    }
    Say "  F) powershell -File rename-endpoint.ps1 -Rollback <backup.reg>  then verify no residue"
    return
}

# --------------------------------------------------------------------------- #
Head 'A) pre-state snapshot'
$pre = Get-Snapshot $DeviceInstanceId $EndpointMatch
$Report.phases.pre = $pre
$pre.RegExport = Join-Path $BackupDir "e2e-pre-$Stamp.reg"
& reg export "HKLM\SYSTEM\CurrentControlSet\Enum\$DeviceInstanceId" $pre.RegExport /y | Out-Null
Say "   device_name     = $($pre.device_name)  ($($pre.device_status))"
Say "   endpoint_name   = $($pre.endpoint_name)"
Say "   hw FriendlyName = $($pre.hw_friendlyname)"
Say "   reg backup      = $($pre.RegExport) ($((Get-Item $pre.RegExport).Length) B)"

$before = (Get-ChildItem $BackupDir -Filter '*.reg' | Select-Object -ExpandProperty Name)
$renamed = $false
$backupReg = $null
$captureEvidence = $null
$speakerEvidence = $null

try {
    Head 'B) rename (delegates to rename-endpoint.ps1)'
    & powershell -NoProfile -ExecutionPolicy Bypass -File $RenamePs1 `
        -DeviceInstanceId $DeviceInstanceId -NewName $NewName -BackupDir $BackupDir
    $rc = $LASTEXITCODE
    if ($rc -ne 0) { Bad "rename-endpoint.ps1 exit=$rc" } else { Ok 'rename-endpoint.ps1 exit=0' }
    $renamed = ($rc -eq 0)

    $after = (Get-ChildItem $BackupDir -Filter '*.reg' | Select-Object -ExpandProperty Name)
    $newRegs = $after | Where-Object { $before -notcontains $_ }
    if ($newRegs) {
        $backupReg = Join-Path $BackupDir ($newRegs | Select-Object -Last 1)
        Say "   new backup      = $backupReg ($((Get-Item $backupReg).Length) B)"
    } else {
        Warn 'no new .reg backup detected -- rollback will fall back to the phase-A export'
        $backupReg = $pre.RegExport
    }

    Head 'C) post-state snapshot'
    $post = Get-Snapshot $DeviceInstanceId $EndpointMatch
    $Report.phases.post = $post
    Say "   device_name     = $($post.device_name)"
    Say "   endpoint_name   = $($post.endpoint_name)"
    Say "   hw FriendlyName = $($post.hw_friendlyname)"
    if ($post.device_name -eq $NewName) { Ok 'device-level rename took effect' } else { Bad "device name is still '$($post.device_name)'" }
    if ($post.endpoint_name -like "*$NewName*") { Ok 'endpoint display name contains the new name' } else { Bad "endpoint name is still '$($post.endpoint_name)'" }

    Say '   fresh-process endpoint list (soundcard):'
    $list = Invoke-Py $Python @($CapPy, '--list') 'list'
    $Report.phases.list = $list
    if ($list.text -match [regex]::Escape($NewName)) { Ok 'a NEW process sees the renamed endpoint' } else { Warn 'fresh-process list did not show the new name' }

    Head 'D) capture from the renamed endpoint'
    $capJson = Join-Path $BackupDir "e2e-capture-$Stamp.json"
    $captureEvidence = Invoke-Py $Python @($CapPy, '--endpoint-name', $NewName, '--seconds', "$Seconds",
                                           '--play-tone', '1000', '--level', '0.7', '--stats-json', $capJson) 'capture'
    $Report.phases.capture = $captureEvidence
    Say "   evidence        = $capJson"

    if (-not $SkipSpeaker) {
        Head 'E) push to speaker (MiXPlay play_url)'
        $pushJson = Join-Path $BackupDir "e2e-push-$Stamp.json"
        $pushArgs = @($PushPy, '--source', 'capture', '--endpoint-name', $NewName, '--play-tone', '1000',
                      '--level', '0.7', '--seconds', "$Seconds", '--target-did', $TargetDid, '--evidence', $pushJson)
        if ($Volume -gt 0) { $pushArgs += @('--volume', "$Volume") }
        $speakerEvidence = Invoke-Py $Python $pushArgs 'push'
        $Report.phases.speaker = $speakerEvidence
        Say "   evidence        = $pushJson"
    } else {
        Warn 'phase E skipped (-SkipSpeaker)'
    }
}
catch {
    Bad "phase failed: $_"
    $Report.phases.error = "$_"
}
finally {
    Head 'F) rollback + no-residue verification'
    if ($KeepRenamed) {
        Warn "-KeepRenamed set: leaving the device renamed to '$NewName'. Remember to roll back manually."
        $Report.phases.rollback = @{ skipped = $true; reason = '-KeepRenamed' }
    }
    elseif (-not $renamed) {
        Warn 'rename phase did not succeed -- running rollback anyway to be safe'
    }
    if (-not $KeepRenamed) {
        if (-not $backupReg) { $backupReg = $pre.RegExport }
        Say "   restoring from  = $backupReg"
        & powershell -NoProfile -ExecutionPolicy Bypass -File $RenamePs1 -Rollback $backupReg -BackupDir $BackupDir
        $rc2 = $LASTEXITCODE
        $final = Get-Snapshot $DeviceInstanceId $EndpointMatch
        $Report.phases.rollback = [ordered]@{
            exit             = $rc2
            restored_from    = $backupReg
            device_name      = $final.device_name
            endpoint_name    = $final.endpoint_name
            hw_friendlyname  = $final.hw_friendlyname
            device_status    = $final.device_status
        }
        Say "   device_name     = $($final.device_name)"
        Say "   endpoint_name   = $($final.endpoint_name)"
        Say "   hw FriendlyName = $($final.hw_friendlyname)   (must be <absent>)"
        if ($final.hw_friendlyname -eq '<absent>' -and $final.device_name -eq $pre.device_name) { Ok 'state matches pre-run baseline' }
        if ($final.device_name -eq $pre.device_name) { Ok 'device name restored' } else { Bad "device name is '$($final.device_name)', expected '$($pre.device_name)'" }
        if ($final.endpoint_name -eq $pre.endpoint_name) { Ok 'endpoint name restored' } else { Bad "endpoint name is '$($final.endpoint_name)', expected '$($pre.endpoint_name)'" }
        if ($final.hw_friendlyname -eq '<absent>') { Ok 'no FriendlyName residue in the Enum key' } else { Bad "residue: FriendlyName = $($final.hw_friendlyname)" }
        if ($final.device_status -eq 'OK') { Ok 'device status OK' } else { Bad "device status = $($final.device_status)" }
    }
}

$Report.finished = (Get-Date).ToString('s')
$Report | ConvertTo-Json -Depth 8 | Set-Content -Path $ReportPath -Encoding UTF8
Say ''
Say "report written: $ReportPath"
