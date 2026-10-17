#Requires -Version 5.1
<#
.SYNOPSIS
  MiPlay zero-driver PoC: rename an existing PnP audio device so that its audio
  endpoint appears in the Windows sound list under a new name.

.DESCRIPTION
  Mechanism (verified in research/vad/rename-probe.md, task-8):
    * The DEVICE (e.g. ROOT\STEAMSTREAMINGSPEAKERS\0000) gets a REG_SZ value
      FriendlyName under
        HKLM\SYSTEM\CurrentControlSet\Enum\<instance>
    * AudioEndpointBuilder then rebuilds the endpoint display name as
        "<endpoint kind> (<device FriendlyName>)"   e.g. "扬声器 (小米妙播设备)"
    * The change becomes visible after a device disable/enable cycle.
    * Writing MMDevices\Audio\<Render|Capture>\<guid>\Properties is a DEAD END
      (E_ACCESSDENIED / reg add "Access is denied") -- do not try it.

  This is a PoC helper, NOT the product path. See README.md.

  Safety model:
    * Rename mode: export the whole instance key to backups\*.reg + save a
      "<backup>.meta.json" before/after snapshot, THEN write, THEN disable/enable.
    * Rollback mode (-Rollback <file.reg>): delete the value we added, reg import
      the backup, disable/enable, then verify every registry value, the device
      name and all endpoint names against the meta snapshot.
    * -DryRun is fully read-only (no file writes, no registry writes, no restart).

.PARAMETER DeviceInstanceId
  PnP instance id of the audio device, e.g. ROOT\STEAMSTREAMINGSPEAKERS\0000.
  Accepts ROOT#...#0000 or the full HKLM\SYSTEM\CurrentControlSet\Enum\... path.
  Must be the concrete instance key ending in \0000.

.PARAMETER NewName
  New device FriendlyName (REG_SZ). Endpoint name becomes "<kind> (<NewName>)".

.PARAMETER Rollback
  A .reg backup previously produced by this script. Restores the key, restarts
  the device and verifies that nothing is left behind.

.PARAMETER BackupDir
  Where backups / evidence go. Default: <script dir>\backups

.PARAMETER BackupOnly
  Only export the key + snapshot. Change nothing (safe rehearsal / on-site archive).

.PARAMETER DryRun
  Read everything, print the plan, write nothing at all.

.PARAMETER RestartTimeoutSec
  How long to wait for the device to come back as OK after enable. Default 30.

.EXAMPLE
  # What would happen? (safe)
  powershell -NoProfile -ExecutionPolicy Bypass -File .\rename-endpoint.ps1 `
      -DeviceInstanceId 'ROOT\STEAMSTREAMINGSPEAKERS\0000' -NewName '小米妙播设备' -DryRun

.EXAMPLE
  # Do it (must be elevated). Leaves a backup + meta next to it.
  powershell -NoProfile -ExecutionPolicy Bypass -File .\rename-endpoint.ps1 `
      -DeviceInstanceId 'ROOT\STEAMSTREAMINGSPEAKERS\0000' -NewName '小米妙播设备'

.EXAMPLE
  # Undo it and prove there is no residue.
  powershell -NoProfile -ExecutionPolicy Bypass -File .\rename-endpoint.ps1 `
      -Rollback .\backups\backup-Enum-ROOT_STEAMSTREAMINGSPEAKERS_0000-xxx.reg
#>
[CmdletBinding(DefaultParameterSetName = 'Rename')]
param(
    [Parameter(ParameterSetName = 'Rename', Mandatory = $true, Position = 0)]
    [string]$DeviceInstanceId,

    [Parameter(ParameterSetName = 'Rename', Mandatory = $true, Position = 1)]
    [string]$NewName,

    [Parameter(ParameterSetName = 'Rollback', Mandatory = $true)]
    [string]$Rollback,

    [Parameter(ParameterSetName = 'Rename')]
    [Parameter(ParameterSetName = 'Rollback')]
    [string]$BackupDir,

    [Parameter(ParameterSetName = 'Rename')]
    [Parameter(ParameterSetName = 'Rollback')]
    [switch]$BackupOnly,

    [Parameter(ParameterSetName = 'Rename')]
    [Parameter(ParameterSetName = 'Rollback')]
    [switch]$DryRun,

    [Parameter(ParameterSetName = 'Rename')]
    [Parameter(ParameterSetName = 'Rollback')]
    [int]$RestartTimeoutSec = 30
)

$ErrorActionPreference = 'Stop'
try { [Console]::OutputEncoding = [Text.Encoding]::UTF8 } catch { }

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
if (-not $BackupDir -or $BackupDir.Trim() -eq '') { $BackupDir = Join-Path $ScriptDir 'backups' }

$EnumSqlPrefix = 'HKLM\SYSTEM\CurrentControlSet\Enum\'
$EnumHklmPrefix = 'HKEY_LOCAL_MACHINE\SYSTEM\CurrentControlSet\Enum\'
$PropDeviceDesc = '{a45c254e-df1c-4efd-8020-67d146a850e0},2'
$PropIfaceName = '{b3f8fa53-0004-438e-9003-51a46e139bfc},6'
$PropDevIface = '{233164c8-1b2c-4c7d-bc68-b671687a2567},1'

# --------------------------------------------------------------------------- #
# small output helpers (ASCII messages so PS 5.1 consoles do not mangle them;
# device/endpoint NAMES themselves are printed as-is)
# --------------------------------------------------------------------------- #
function Write-Step([string]$msg) { Write-Host ''; Write-Host "== $msg" }
function Write-Info([string]$msg) { Write-Host "   $msg" }
function Write-Ok([string]$msg) { Write-Host "   [PASS] $msg" -ForegroundColor Green }
function Write-Bad([string]$msg) { Write-Host "   [FAIL] $msg" -ForegroundColor Red }
function Write-Warn([string]$msg) { Write-Host "   [WARN] $msg" -ForegroundColor Yellow }

# --------------------------------------------------------------------------- #
# identity / registry helpers
# --------------------------------------------------------------------------- #
function Normalize-InstanceId([string]$raw) {
    $s = $raw.Trim()
    $s = $s -replace '/', '\'
    $s = $s -replace '#', '\'
    $s = $s.Trim('\')
    foreach ($p in @($EnumHklmPrefix, $EnumSqlPrefix, 'HKEY_LOCAL_MACHINE\SYSTEM\CurrentControlSet\Enum')) {
        if ($s.ToUpperInvariant().StartsWith($p.ToUpperInvariant())) {
            $s = $s.Substring($p.Length)
            break
        }
    }
    return $s.Trim('\')
}

# --------------------------------------------------------------------------- #
# native tools
# --------------------------------------------------------------------------- #
# Windows PowerShell 5.1 turns ANY native stderr output into a *terminating*
# NativeCommandError while $ErrorActionPreference='Stop'. reg.exe prints
# "The operation completed successfully." on STDERR for `import` / `delete`, so the
# rollback used to die right after deleting the value (measured 2026-10-17 23:12 and
# 23:15: the device name was restored but the disable/enable never ran, leaving the
# endpoint named '扬声器 (小米妙播设备)'). Native tools therefore run with
# $ErrorActionPreference='Continue' and are judged by their exit code only.
function Invoke-Native([string]$exe, [string[]]$argv) {
    $prev = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try { $o = & $exe @argv 2>&1 } finally { $ErrorActionPreference = $prev }
    $code = $LASTEXITCODE
    $textOut = (($o | ForEach-Object {
                if ($_ -is [System.Management.Automation.ErrorRecord]) { "$($_.Exception.Message)" } else { "$_" }
            } | Where-Object { $_ -ne '' } | Select-Object -Unique) -join "`n").Trim()
    return [pscustomobject]@{ exit = $code; out = $textOut }
}

function Get-EnumRegPath([string]$inst) { return "$EnumSqlPrefix$inst" }
function Get-EnumPsPath([string]$inst) { return "HKLM:\SYSTEM\CurrentControlSet\Enum\$inst" }

function Get-RegTypeName($kind) {
    switch ("$kind") {
        'String' { return 'REG_SZ' }
        'ExpandString' { return 'REG_EXPAND_SZ' }
        'Binary' { return 'REG_BINARY' }
        'DWord' { return 'REG_DWORD' }
        'QWord' { return 'REG_QWORD' }
        'MultiString' { return 'REG_MULTI_SZ' }
        default { return "$kind" }
    }
}

function Convert-RegValueToString($v) {
    if ($null -eq $v) { return '<null>' }
    if ($v -is [byte[]]) { return [Convert]::ToBase64String($v) }
    if ($v -is [array]) { return ($v -join "`u{1}") }
    return [string]$v
}

# Snapshot of the instance key's OWN values: "valueName|REG_TYPE|value" sorted lines.
# Deterministic -> two snapshots can be compared as plain string arrays.
# NOTE: deliberately NOT recursive.  The device instance key contains a
# "Properties" subkey whose ACL does not grant BUILTIN\Administrators read access
# ("Requested registry access is not allowed"), and we never write there anyway.
# Deep coverage comes from the reg.exe export SHA256 comparison instead (reg.exe
# can read that subkey, the PowerShell registry provider cannot).
function Get-RegValuesSnapshot([string]$PsPath) {
    if (-not (Test-Path -LiteralPath $PsPath)) { throw "registry key not found: $PsPath" }
    $lines = New-Object System.Collections.Generic.List[string]
    $k = Get-Item -LiteralPath $PsPath
    foreach ($n in ($k.GetValueNames() | Sort-Object)) {
        $kind = Get-RegTypeName $k.GetValueKind($n)
        $val = $k.GetValue($n, $null, 'DoNotExpandEnvironmentNames')
        $lines.Add(("{0}|{1}|{2}" -f $n, $kind, (Convert-RegValueToString $val)))
    }
    $arr = $lines.ToArray()
    [Array]::Sort($arr, [StringComparer]::Ordinal)
    return , $arr
}

function Get-KeyValueSafe([string]$PsPath, [string]$name) {
    if (-not (Test-Path -LiteralPath $PsPath)) { return @{ exists = $false; value = $null } }
    $k = Get-Item -LiteralPath $PsPath
    if ($k.GetValueNames() -notcontains $name) { return @{ exists = $false; value = $null } }
    return @{ exists = $true; value = (Convert-RegValueToString $k.GetValue($name, $null, 'DoNotExpandEnvironmentNames')) }
}

# --------------------------------------------------------------------------- #
# device / endpoint discovery  (read-only)
# --------------------------------------------------------------------------- #
function Get-DeviceInfo([string]$inst) {
    $d = $null
    try { $d = Get-PnpDevice -InstanceId $inst -ErrorAction Stop } catch { $d = $null }
    if ($null -eq $d) {
        throw "PnP device not found: '$inst'. Hint: Get-PnpDevice -Class AudioEndpoint ; Get-PnpDevice | Where-Object InstanceId -like 'ROOT*'"
    }
    return [ordered]@{
        instanceId   = $inst
        friendlyName = [string]$d.FriendlyName
        status       = [string]$d.Status
        present      = [bool]$d.Present
        deviceClass  = [string]$d.Class
    }
}

function Get-AudioEndpointInfo([string]$inst) {
    # An endpoint belongs to the device when one of the MMDevices\...\Properties
    # values contains the device interface path (root#...#0000#...).  Read-only.
    $needle = ($inst -replace '\\', '#').ToLowerInvariant()
    $rows = New-Object System.Collections.Generic.List[object]
    foreach ($flow in @('Render', 'Capture')) {
        $root = "HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\$flow"
        if (-not (Test-Path -LiteralPath $root)) { continue }
        foreach ($ep in @(Get-ChildItem -LiteralPath $root)) {
            $propsPath = Join-Path $ep.PSPath 'Properties'
            if (-not (Test-Path -LiteralPath $propsPath)) { continue }
            $props = Get-Item -LiteralPath $propsPath
            $pvals = [ordered]@{}
            $matched = $false
            foreach ($n in $props.GetValueNames()) {
                $vs = Convert-RegValueToString $props.GetValue($n, $null, 'DoNotExpandEnvironmentNames')
                $pvals[$n] = $vs
                if ($vs.ToLowerInvariant().Contains($needle)) { $matched = $true }
            }
            if (-not $matched) { continue }
            $guid = [string]$ep.PSChildName
            # Audio endpoints are PnP devices with an instance id such as
            #   SWD\MMDEVAPI\{0.0.0.00000000}.{4A82BBBC-...}   (render)
            #   SWD\MMDEVAPI\{0.0.1.00000000}.{4A82BBBC-...}   (capture)
            # so look them up by GUID instead of guessing the role prefix.
            $name = $null
            $stat = $null
            $epInstance = $null
            try {
                $pd = Get-PnpDevice -Class AudioEndpoint -ErrorAction Stop |
                    Where-Object { $_.InstanceId -like "*$guid*" } |
                    Select-Object -First 1
                if ($null -ne $pd) {
                    $epInstance = [string]$pd.InstanceId
                    $name = [string]$pd.FriendlyName
                    $stat = [string]$pd.Status
                }
            } catch { }
            $kind = $null
            $inner = $null
            if ($name -match '^(?<kind>[^(]+?)\s*\((?<inner>.*)\)\s*$') {
                $kind = $matches['kind'].Trim()
                $inner = $matches['inner']
            }
            if (-not $kind) {
                # fall back to the driver-pin DeviceDesc property (e.g. "扬声器"), which
                # is exactly the hard-coded prefix AEB puts in front of the device name
                $d = [string]$pvals[$PropDeviceDesc]
                if ($d) { $kind = $d }
            }
            $rows.Add([ordered]@{
                flow           = $flow
                guid           = $guid
                pnpInstanceId  = $epInstance
                friendlyName   = $name
                status         = $stat
                nameKind       = $kind
                nameInner      = $inner
                devicePath     = ($pvals.Values | Where-Object { $_.ToLowerInvariant().Contains($needle) } | Select-Object -First 1)
                threeKeys      = [ordered]@{
                    deviceDesc   = $pvals[$PropDeviceDesc]
                    ifaceName    = $pvals[$PropIfaceName]
                    devIfacePath = $pvals[$PropDevIface]
                }
            })
        }
    }
    return @($rows.ToArray())
}

function Format-EndpointTable($eps) {
    if ($null -eq $eps -or @($eps).Count -eq 0) { return '   (no audio endpoint found for this device)' }
    $rows = @($eps) | ForEach-Object {
        $st = [string]$_.status
        if (-not $st) { $st = 'not-present' }
        $nm = [string]$_.friendlyName
        if (-not $nm) { $nm = '<not in the Windows sound list>' }
        [pscustomobject]@{
            Flow   = $_.flow
            Status = $st
            Name   = $nm
            Guid   = $_.guid
        }
    }
    return ($rows | Format-Table -AutoSize | Out-String -Width 200).TrimEnd()
}

function Get-EndpointSnapshot($eps) {
    return @(@($eps) | ForEach-Object {
            [ordered]@{
                flow         = $_.flow
                guid         = $_.guid
                friendlyName = $_.friendlyName
                status       = $_.status
                nameKind     = $_.nameKind
                nameInner    = $_.nameInner
                threeKeys    = $_.threeKeys
            }
        })
}

# --------------------------------------------------------------------------- #
# device restart (disable + enable) = the only way to make AEB rebuild the name
# --------------------------------------------------------------------------- #
function Wait-DeviceStatus([string]$inst, [string[]]$want, [int]$timeoutSec) {
    $deadline = (Get-Date).AddSeconds($timeoutSec)
    $last = ''
    do {
        try { $last = [string](Get-PnpDevice -InstanceId $inst -ErrorAction Stop).Status } catch { $last = 'MISSING' }
        if ($want -contains $last) { return $last }
        Start-Sleep -Milliseconds 400
    } while ((Get-Date) -lt $deadline)
    return $last
}

function Restart-Device([string]$inst, [int]$timeoutSec) {
    Write-Info "disable/enable device: $inst"
    if (Get-Command Disable-PnpDevice -ErrorAction SilentlyContinue) {
        Disable-PnpDevice -InstanceId $inst -Confirm:$false -ErrorAction Stop
    } else {
        $r = Invoke-Native 'pnputil.exe' @('/disable-device', "$inst")
        if ($r.exit -ne 0) { throw "pnputil /disable-device failed (exit $($r.exit)): $($r.out)" }
    }
    $st = Wait-DeviceStatus $inst @('Error', 'Unknown', 'MISSING', 'Disabled') 15
    Write-Info "  after disable: status=$st"
    Start-Sleep -Milliseconds 400
    if (Get-Command Enable-PnpDevice -ErrorAction SilentlyContinue) {
        Enable-PnpDevice -InstanceId $inst -Confirm:$false -ErrorAction Stop
    } else {
        $r = Invoke-Native 'pnputil.exe' @('/enable-device', "$inst")
        if ($r.exit -ne 0) { throw "pnputil /enable-device failed (exit $($r.exit)): $($r.out)" }
    }
    $st2 = Wait-DeviceStatus $inst @('OK') $timeoutSec
    Write-Info "  after enable : status=$st2"
    if ($st2 -ne 'OK') { throw "device did not come back OK within ${timeoutSec}s (status=$st2); check Device Manager" }
    Start-Sleep -Milliseconds 800   # give AEB a moment to publish the endpoint
    return $st2
}

function Wait-EndpointsRenamed([string]$inst, [string[]]$wantNames, [int]$timeoutSec) {
    $deadline = (Get-Date).AddSeconds($timeoutSec)
    do {
        $eps = @(Get-AudioEndpointInfo $inst)
        $checked = 0
        $allOk = $true
        foreach ($e in $eps) {
            if (-not $e.status) { continue }   # not in the Windows sound list -> cannot be named
            $checked++
            if ($wantNames -notcontains $e.friendlyName) { $allOk = $false }
        }
        if ($checked -gt 0 -and $allOk) { return $eps }
        Start-Sleep -Milliseconds 500
    } while ((Get-Date) -lt $deadline)
    return @(Get-AudioEndpointInfo $inst)
}

# --------------------------------------------------------------------------- #
# backup / evidence
# --------------------------------------------------------------------------- #
function Export-KeyBackup([string]$inst, [string]$Label) {
    if (-not (Test-Path -LiteralPath $BackupDir)) {
        New-Item -ItemType Directory -Path $BackupDir -Force | Out-Null
    }
    $safeLabel = ($Label -replace '[^\w\-.]+', '_')
    if ($safeLabel.Length -gt 40) { $safeLabel = $safeLabel.Substring(0, 40) }
    $stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
    $file = Join-Path $BackupDir ("backup-Enum-{0}-{1}-{2}.reg" -f ($inst -replace '\\', '_'), $safeLabel, $stamp)
    $r = Invoke-Native 'reg.exe' @('export', (Get-EnumRegPath $inst), "$file", '/y')
    if ($r.exit -ne 0) { throw "reg export failed (exit $($r.exit)): $($r.out)" }
    if (-not (Test-Path -LiteralPath $file)) { throw "reg export reported success but file is missing: $file" }
    $len = (Get-Item -LiteralPath $file).Length
    if ($len -lt 100) { throw "backup file looks too small ($len bytes): $file" }
    Write-Info "backup: $file ($len bytes)"
    return $file
}

function Get-MetaPath([string]$regFile) {
    return ([IO.Path]::Combine([IO.Path]::GetDirectoryName($regFile),
            ([IO.Path]::GetFileNameWithoutExtension($regFile) + '.meta.json')))
}

function Write-JsonFile([string]$path, $obj) {
    $dir = [IO.Path]::GetDirectoryName($path)
    if ($dir -and -not (Test-Path -LiteralPath $dir)) { New-Item -ItemType Directory -Path $dir -Force | Out-Null }
    $json = $obj | ConvertTo-Json -Depth 12
    # UTF-8 WITH BOM: Windows PowerShell 5.1 `Get-Content` would otherwise decode a
    # BOM-less UTF-8 file as the ANSI codepage and mangle CJK names ("小米妙播设备").
    [IO.File]::WriteAllText($path, $json, (New-Object System.Text.UTF8Encoding($true)))
    Write-Info "evidence: $path"
}

function Write-Meta([string]$metaFile, $meta) {
    Write-JsonFile $metaFile $meta
}

function Read-Meta([string]$metaFile) {
    if (-not (Test-Path -LiteralPath $metaFile)) { return $null }
    return ([IO.File]::ReadAllText($metaFile, (New-Object System.Text.UTF8Encoding($true))) | ConvertFrom-Json)
}

function Compare-StringArrays([string[]]$a, [string[]]$b) {
    $missing = @($a | Where-Object { $b -notcontains $_ })
    $extra = @($b | Where-Object { $a -notcontains $_ })
    return @{ missing = $missing; extra = $extra; same = ($missing.Count -eq 0 -and $extra.Count -eq 0) }
}

function Get-Sha256Hex([string]$path) {
    return (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash
}

# --------------------------------------------------------------------------- #
# shared: resolve + read current state
# --------------------------------------------------------------------------- #
function Resolve-Target([string]$rawInstance) {
    $inst = Normalize-InstanceId $rawInstance
    if ($inst -notmatch '\\0000$') {
        throw "DeviceInstanceId must be the concrete device instance key ending in \0000 (got '$inst')."
    }
    $psPath = Get-EnumPsPath $inst
    if (-not (Test-Path -LiteralPath $psPath)) { throw "registry key does not exist: $(Get-EnumRegPath $inst)" }
    return $inst
}

function Get-FullState([string]$inst) {
    $dev = Get-DeviceInfo $inst
    $eps = @(Get-AudioEndpointInfo $inst)
    $reg = Get-KeyValueSafe (Get-EnumPsPath $inst) 'FriendlyName'
    return [ordered]@{
        device        = $dev
        endpoints     = @(Get-EndpointSnapshot $eps)
        enumFriendlyName = $reg
    }
}

function Show-State($state) {
    Write-Info ("device  : {0}  [status={1}, present={2}, class={3}]" -f `
            $state.device.friendlyName, $state.device.status, $state.device.present, $state.device.deviceClass)
    $fw = $state.enumFriendlyName
    if ($fw.exists) { Write-Info ("Enum\...\FriendlyName = '{0}' (exists)" -f $fw.value) }
    else { Write-Info "Enum\...\FriendlyName = <absent>" }
    Write-Info 'audio endpoint(s) of this device:'
    Write-Host (Format-EndpointTable $state.endpoints)
}

# =========================================================================== #
# ROLLBACK MODE
# =========================================================================== #
if ($PSCmdlet.ParameterSetName -eq 'Rollback') {
    Write-Host '=== MiPlay zero-driver PoC: ROLLBACK ==='
    if (-not (Test-Path -LiteralPath $Rollback)) { throw "backup file not found: $Rollback" }
    $regFile = (Resolve-Path -LiteralPath $Rollback).Path

    $text = [IO.File]::ReadAllText($regFile)
    $m = [regex]::Match($text, '(?im)^\s*\[(?<key>HKEY_LOCAL_MACHINE[^\]]+)\]\s*$')
    if (-not $m.Success) { throw "not a regedit .reg export: $regFile" }
    $keyPath = $m.Groups['key'].Value.Trim()
    if (-not $keyPath.ToUpperInvariant().StartsWith($EnumHklmPrefix.ToUpperInvariant())) {
        throw "backup key is not under HKLM\SYSTEM\CurrentControlSet\Enum\: $keyPath"
    }
    $inst = $keyPath.Substring($EnumHklmPrefix.Length).Trim('\')
    if ($inst -notmatch '\\0000$') {
        throw "backup must be an export of the device INSTANCE key (…\0000), got: $keyPath"
    }
    $psPath = Get-EnumPsPath $inst
    $regPath = Get-EnumRegPath $inst

    Write-Step "target from backup"
    Write-Info "instance : $inst"
    Write-Info "backup   : $regFile"
    $metaFile = Get-MetaPath $regFile
    $meta = Read-Meta $metaFile
    if ($null -eq $meta) {
        Write-Warn "no meta file next to the backup ($metaFile): name/snapshot verification will be skipped"
    } else {
        Write-Info ("meta     : {0} (renamed from '{1}' to '{2}' at {3})" -f `
                $metaFile, $meta.before.device.friendlyName, $meta.newName, $meta.renamedAt)
    }

    $backupHasFriendly = [regex]::IsMatch($text, '(?im)^\s*"FriendlyName"\s*=')
    $backupFriendlyValue = $null
    if ($backupHasFriendly) {
        $fm = [regex]::Match($text, '(?im)^\s*"FriendlyName"\s*=\s*"(?<v>(?:[^"\\]|\\.)*)"')
        if ($fm.Success) { $backupFriendlyValue = $fm.Groups['v'].Value }
    }
    Write-Info ("backup FriendlyName: {0}" -f $(if ($backupHasFriendly) { "'$backupFriendlyValue'" } else { '<absent -> delete after rollback>' }))

    Write-Step 'state BEFORE rollback'
    Show-State (Get-FullState $inst)

    if ($DryRun) {
        Write-Step 'DRY RUN -- planned actions (nothing is changed)'
        Write-Info ("1) delete HKLM\...\Enum\{0}\FriendlyName  (only if it exists)" -f $inst)
        if ($backupHasFriendly) {
            Write-Info ("2) put the pre-rename FriendlyName back: '{0}'" -f $backupFriendlyValue)
        } else {
            Write-Info '2) nothing to put back (the backup had no FriendlyName)'
        }
        Write-Info ("   + best-effort reg import `"{0}`"" -f $regFile)
        Write-Info ("3) disable/enable {0} to let AudioEndpointBuilder rebuild the name" -f $inst)
        Write-Info '4) verify registry values / device name / endpoint names against the meta snapshot'
        Write-Info '   + compare a fresh reg export with the backup (SHA256)'
        Write-Ok 'dry run: no changes were made'
        exit 0
    }

    $results = [ordered]@{}
    $ok = $true

    $before = Get-FullState $inst
    $backupSha = Get-Sha256Hex $regFile

    Write-Step 'restore registry'
    # The only value this script ever writes is the instance key's FriendlyName, so the
    # restore is: delete it, then put the pre-rename value back if there was one.
    # `reg import` is a best-effort extra (it must never touch the ACL-protected
    # "Properties" subkey; reg.exe export skips that subkey, so the import is safe).
    $cur = Get-KeyValueSafe $psPath 'FriendlyName'
    if ($cur.exists) {
        $r = Invoke-Native 'reg.exe' @('delete', $regPath, '/v', 'FriendlyName', '/f')
        if ($r.exit -ne 0) { throw "reg delete FriendlyName failed (exit $($r.exit)): $($r.out)" }
        Write-Info "deleted FriendlyName (was '$($cur.value)')"
    } else {
        Write-Info 'FriendlyName already absent'
    }
    $r = Invoke-Native 'reg.exe' @('import', "$regFile")
    if ($r.exit -ne 0) {
        Write-Warn "reg import returned $($r.exit) ($($r.out)) -- continuing, value-level verification below is authoritative"
    } else {
        Write-Info "reg import: $($r.out)"
    }
    if ($backupHasFriendly) {
        $restored = Get-KeyValueSafe $psPath 'FriendlyName'
        if (-not $restored.exists -or $restored.value -ne $backupFriendlyValue) {
            $r = Invoke-Native 'reg.exe' @('add', $regPath, '/v', 'FriendlyName', '/t', 'REG_SZ', '/d', "$backupFriendlyValue", '/f')
            if ($r.exit -ne 0) { throw "could not restore the pre-rename FriendlyName '$backupFriendlyValue' (exit $($r.exit)): $($r.out)" }
            Write-Info "restored pre-rename FriendlyName: '$backupFriendlyValue'"
        } else {
            Write-Info "pre-rename FriendlyName already restored: '$backupFriendlyValue'"
        }
    }

    Write-Step 'restart device (disable/enable)'
    Restart-Device $inst $RestartTimeoutSec | Out-Null

    # AEB does not always drop the cached endpoint interface name on the first cycle:
    # measured 2026-10-17 23:12 -- after the registry revert, one disable/enable still left
    # the endpoint named '扬声器 (小米妙播设备)'; a SECOND cycle restored
    # '扬声器 (Steam Streaming Speakers)'. So retry until the endpoint names match the
    # pre-rename names (max 3 extra cycles) instead of trusting a single refresh.
    if ($null -ne $meta -and $null -ne $meta.before) {
        # Compare only endpoints that are actually PRESENT: the pre-rename snapshot also
        # contains the not-in-the-sound-list capture endpoint (empty friendly name), which
        # would otherwise never match and make this loop cycle needlessly.
        $wantNames0 = @(@($meta.before.endpoints) | Where-Object { $_.status } | ForEach-Object { [string]$_.friendlyName })
        for ($try = 1; $try -le 3; $try++) {
            $nowNames0 = @(Get-AudioEndpointInfo $inst | Where-Object { $_.status } | ForEach-Object { [string]$_.friendlyName })
            $cmp0 = Compare-StringArrays $wantNames0 $nowNames0
            if ($cmp0.same) { break }
            Write-Warn ("endpoint name(s) still stale before refresh #{0}: {1} -> extra disable/enable" -f $try, (@($nowNames0) -join ' | '))
            Start-Sleep -Seconds 2
            Restart-Device $inst $RestartTimeoutSec | Out-Null
        }
    }

    Write-Step 'verify: no residue'
    # 1) registry values == meta snapshot (if available)
    if ($null -ne $meta -and $null -ne $meta.beforeSnapshot) {
        $now = Get-RegValuesSnapshot $psPath
        $want = @($meta.beforeSnapshot)
        $cmp = Compare-StringArrays $want $now
        $results['registry_values_match_snapshot'] = $cmp.same
        if ($cmp.same) {
            Write-Ok ("registry values identical to pre-rename snapshot ({0} values)" -f @($now).Count)
        } else {
            $ok = $false
            Write-Bad ("registry values DIFFER from pre-rename snapshot: {0} missing, {1} extra" -f $cmp.missing.Count, $cmp.extra.Count)
            foreach ($x in @($cmp.missing | Select-Object -First 10)) { Write-Info "  missing: $x" }
            foreach ($x in @($cmp.extra | Select-Object -First 10)) { Write-Info "  extra  : $x" }
        }
    }
    # 2) fresh export byte-identical to backup
    $tmp = Join-Path $env:TEMP ("zero-driver-reexport-{0}.reg" -f [guid]::NewGuid().ToString('N').Substring(0, 8))
    $r = Invoke-Native 'reg.exe' @('export', $regPath, "$tmp", '/y')
    if ($r.exit -eq 0 -and (Test-Path -LiteralPath $tmp)) {
        $nowSha = Get-Sha256Hex $tmp
        $same = ($nowSha -eq $backupSha)
        $results['reexport_sha256_matches_backup'] = $same
        if ($same) { Write-Ok "fresh reg export is byte-identical to the backup (SHA256 $nowSha)" }
        else {
            Write-Warn "fresh reg export differs byte-wise from backup (backup $backupSha / now $nowSha)"
            Write-Info "  -> value-level comparison above is authoritative; byte diff is usually value ORDER"
        }
        Remove-Item -LiteralPath $tmp -Force -ErrorAction SilentlyContinue
    } else {
        Write-Warn "could not re-export for byte comparison (exit $($r.exit))"
    }
    # 3) device name / status
    $nowState = Get-FullState $inst
    Show-State $nowState
    $results['device_name_restored'] = $true
    $results['device_status_ok'] = ($nowState.device.status -eq 'OK')
    $results['enum_friendly_absent'] = (-not $nowState.enumFriendlyName.exists)
    if (-not $nowState.enumFriendlyName.exists) { Write-Ok 'Enum\...\FriendlyName is absent again' }
    else { $ok = $false; Write-Bad ("Enum\...\FriendlyName still present: '{0}'" -f $nowState.enumFriendlyName.value) }
    if (-not $results['device_status_ok']) { $ok = $false; Write-Bad ("device status is {0}, expected OK" -f $nowState.device.status) }
    else { Write-Ok 'device status OK' }

    if ($null -ne $meta) {
        $wantDev = [string]$meta.before.device.friendlyName
        if ($nowState.device.friendlyName -eq $wantDev) {
            Write-Ok "device friendly name restored: '$wantDev'"
        } else {
            $ok = $false
            $results['device_name_restored'] = $false
            Write-Bad ("device friendly name is '{0}', expected '{1}'" -f $nowState.device.friendlyName, $wantDev)
        }
        # 4) endpoint names
        $wantNames = @(@($meta.before.endpoints) | ForEach-Object { [string]$_.friendlyName })
        $nowNames = @($nowState.endpoints | ForEach-Object { [string]$_.friendlyName })
        $cmpNames = Compare-StringArrays $wantNames $nowNames
        $results['endpoint_names_restored'] = $cmpNames.same
        if ($cmpNames.same) { Write-Ok ("endpoint name(s) restored: {0}" -f (@($nowNames) -join ' | ')) }
        else {
            $ok = $false
            Write-Bad 'endpoint name(s) NOT restored'
            foreach ($x in @($cmpNames.missing | Select-Object -First 10)) { Write-Info "  expected: $x" }
            foreach ($x in @($cmpNames.extra | Select-Object -First 10)) { Write-Info "  actual  : $x" }
        }
        # 5) the three MMDevices property keys per endpoint
        $propOk = $true
        $wantProps = @{}
        foreach ($e in @($meta.before.endpoints)) { $wantProps[[string]$e.guid] = $e.threeKeys }
        foreach ($e in @($nowState.endpoints)) {
            $want = $wantProps[[string]$e.guid]
            if ($null -eq $want) { continue }
            foreach ($k in @('deviceDesc', 'ifaceName', 'devIfacePath')) {
                $w = [string]$want.$k
                $n = [string]$e.threeKeys.$k
                if ($w -ne $n) {
                    $propOk = $false
                    Write-Bad ("endpoint {0} property {1}: expected '{2}' got '{3}'" -f $e.guid, $k, $w, $n)
                }
            }
        }
        $results['mmdevices_properties_unchanged'] = $propOk
        if ($propOk) { Write-Ok 'MMDevices Properties (deviceDesc / ifaceName / devIfacePath) unchanged' }
        else { $ok = $false }
    }

    $evidence = [ordered]@{
        _about       = 'MiPlay zero-driver PoC rollback evidence'
        at           = (Get-Date).ToString('o')
        instanceId   = $inst
        backup       = $regFile
        backupSha256 = $backupSha
        dryRun       = $false
        checks       = $results
        before       = $before
        after        = $nowState
        result       = $(if ($ok) { 'RESIDUE_FREE' } else { 'RESIDUE_FOUND' })
    }
    $evFile = [IO.Path]::Combine([IO.Path]::GetDirectoryName($regFile),
        ('rollback-' + (Get-Date -Format 'yyyyMMdd-HHmmss') + '.json'))
    Write-JsonFile $evFile $evidence

    Write-Step 'ROLLBACK RESULT'
    if ($ok) { Write-Host '   RESIDUE-FREE: rolled back and verified.' -ForegroundColor Green; exit 0 }
    Write-Host '   RESIDUE FOUND: see checks above / evidence json.' -ForegroundColor Red
    exit 1
}

# =========================================================================== #
# RENAME MODE
# =========================================================================== #
Write-Host '=== MiPlay zero-driver PoC: RENAME ==='
$inst = Resolve-Target $DeviceInstanceId
$psPath = Get-EnumPsPath $inst
$regPath = Get-EnumRegPath $inst
if ($NewName.Trim().Length -lt 1) { throw 'NewName is empty' }

Write-Step 'target'
Write-Info "instance : $inst"
Write-Info "new name : $NewName"

$state = Get-FullState $inst
Write-Step 'state BEFORE'
Show-State $state

$beforeSnap = Get-RegValuesSnapshot $psPath
$regKey = Get-Item -LiteralPath $psPath
$hadFriendly = ($regKey.GetValueNames() -contains 'FriendlyName')
$expRender = @(); $expCapture = @(); $expActive = @(); $expInactive = @()
foreach ($e in @($state.endpoints)) {
    if ($e.nameKind) {
        $want = ("{0} ({1})" -f $e.nameKind, $NewName)
        if ($e.flow -eq 'Render') { $expRender += $want } else { $expCapture += $want }
        if ($e.status) { $expActive += $want }   # currently in the Windows sound list
        else { $expInactive += $want }           # e.g. a hidden capture pin; may stay hidden
    }
}

if ($DryRun) {
    Write-Step 'DRY RUN -- plan (nothing is changed)'
    Write-Info ("1) reg export `"{0}`" -> {1}\backup-Enum-*.reg + *.meta.json" -f $regPath, $BackupDir)
    if ($hadFriendly) {
        Write-Info ("2) FriendlyName already exists ('{0}'), it will be OVERWRITTEN with '{1}'" -f $state.enumFriendlyName.value, $NewName)
    } else {
        Write-Info ("2) reg add `"{0}`" /v FriendlyName /t REG_SZ /d `"{1}`" /f" -f $regPath, $NewName)
    }
    Write-Info ("3) disable/enable {0} (refreshes AudioEndpointBuilder)" -f $inst)
    Write-Info '4) expected endpoint name(s) after the restart:'
    foreach ($n in $expActive) { Write-Info "     $n" }
    foreach ($n in $expInactive) { Write-Info "     $n   (currently not in the sound list -- may stay hidden)" }
    if (-not $expActive -and -not $expInactive) { Write-Warn '     (none: could not derive a pin name for any endpoint)' }
    Write-Info '5) rollback afterwards: -Rollback <the backup .reg>  (verifies registry + names + Properties)'
    Write-Ok 'dry run: no changes were made'
    exit 0
}

$backupFile = Export-KeyBackup $inst $state.device.friendlyName
$metaFile = Get-MetaPath $backupFile
$meta = [ordered]@{
    _about          = 'MiPlay zero-driver PoC rename backup. Keep together with the .reg backup.'
    script          = 'rename-endpoint.ps1'
    at              = (Get-Date).ToString('o')
    renamedAt       = (Get-Date).ToString('o')
    instanceId      = $inst
    regPath         = $regPath
    newName         = $NewName
    hadFriendlyName = [bool]$hadFriendly
    oldFriendlyName = $state.enumFriendlyName.value
    backupSha256    = (Get-Sha256Hex $backupFile)
    before          = $state
    beforeSnapshot  = $beforeSnap
    expectedEndpoints = @($expRender + $expCapture)
}
Write-Meta $metaFile $meta

if ($BackupOnly) {
    Write-Step 'BACKUP ONLY'
    Write-Ok "backup + meta written, nothing else changed: $backupFile"
    exit 0
}

Write-Step 'write FriendlyName'
$r = Invoke-Native 'reg.exe' @('add', $regPath, '/v', 'FriendlyName', '/t', 'REG_SZ', '/d', "$NewName", '/f')
if ($r.exit -ne 0) {
    throw "reg add failed (exit $($r.exit)): $($r.out)`n  -> this script needs an ELEVATED shell and a device key you may write (see research/vad/rename-probe.md)."
}
Write-Info "reg add: $($r.out)"
$written = Get-KeyValueSafe $psPath 'FriendlyName'
if (-not $written.exists -or $written.value -ne $NewName) {
    throw "read-back mismatch: expected '$NewName', got '$($written.value)'"
}
Write-Ok "FriendlyName written and read back: '$($written.value)'"

Write-Step 'refresh AudioEndpointBuilder (disable/enable)'
Restart-Device $inst $RestartTimeoutSec | Out-Null

Write-Step 'wait for the endpoint name to be rebuilt'
$afterEps = @(Wait-EndpointsRenamed $inst @($expActive) $RestartTimeoutSec)
$afterState = Get-FullState $inst
Show-State $afterState

$present = @($afterEps | Where-Object { $_.status })
$renamedOk = ($present.Count -gt 0)
foreach ($e in $present) {
    if ($expActive -notcontains $e.friendlyName) { $renamedOk = $false }
}

Write-Step 'expected vs actual'
foreach ($n in $expActive) { Write-Info "expected: $n" }
foreach ($n in $expInactive) { Write-Info "expected: $n   (was hidden before the rename)" }
foreach ($e in $afterEps) { Write-Info ("actual  : {0}  [{1}, {2}]" -f $e.friendlyName, $e.flow, $(if ($e.status) { $e.status } else { 'not-present' })) }

Write-Step 'RENAME RESULT'
if ($renamedOk) {
    Write-Ok "endpoint renamed. Windows sound list should now show: $(@($present | ForEach-Object { $_.friendlyName }) -join ' | ')"
} else {
    Write-Bad 'endpoint did NOT get the expected name after the restart'
}

$evidence = [ordered]@{
    _about         = 'MiPlay zero-driver PoC rename evidence'
    at             = (Get-Date).ToString('o')
    instanceId     = $inst
    newName        = $NewName
    backup         = $backupFile
    meta           = $metaFile
    dryRun         = $false
    backupOnly     = $false
    expectedNames  = @($expRender + $expCapture)
    before         = $state
    after          = $afterState
    renamedOk      = [bool]$renamedOk
}
$evFile = Join-Path (Split-Path -Parent $backupFile) ('rename-' + (Get-Date -Format 'yyyyMMdd-HHmmss') + '.json')
Write-JsonFile $evFile $evidence

Write-Step 'next steps'
Write-Info "rollback: powershell -NoProfile -ExecutionPolicy Bypass -File .\rename-endpoint.ps1 -Rollback `"$backupFile`""
Write-Info 'audio check / push: see README.md (capture_from_endpoint.py + push_to_speaker.py)'

if ($renamedOk) { exit 0 } else { exit 1 }
