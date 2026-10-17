# install-vad.ps1 - trust our self-signed certificate and install the StereoLink
#                  virtual audio driver, capturing all evidence for the question
#                  "does a self-signed kernel driver load on this machine?"
#
# ASCII only (Windows PowerShell 5.1 reads BOM-less .ps1 as ANSI).
#
# What it changes on the machine (all reversible with -Uninstall):
#   1. imports the PUBLIC certificate (certs\stereolink-dev.cer) into
#      Cert:\LocalMachine\Root and Cert:\LocalMachine\TrustedPublisher
#   2. pnputil /add-driver <inf> /install   -> stages the package in the DriverStore
#   3. devcon install <inf> *StereoLinkVad  -> creates the root-enumerated devnode
#      (pnputil cannot create a device instance; for a root-enumerated driver a
#       devcon-style device creation step is required)
# It never touches bcdedit, never removes/changes other audio devices and never
# changes the default playback device.
#
# Usage:
#   powershell -NoProfile -ExecutionPolicy Bypass -File .\install-vad.ps1
#   powershell -NoProfile -ExecutionPolicy Bypass -File .\install-vad.ps1 -Uninstall
#   powershell -NoProfile -ExecutionPolicy Bypass -File .\install-vad.ps1 -TrustOnly
[CmdletBinding()]
param(
    [string]$PkgDir  = '',
    [string]$CertDir = '',
    [string]$HwId    = '*StereoLinkVad',
    [string]$ServiceName = 'StereoLinkVad',
    [switch]$Uninstall,
    [switch]$TrustOnly,
    [switch]$KeepCertificateOnUninstall
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$Root = $PSScriptRoot
if (-not $PkgDir)  { $PkgDir  = Join-Path $Root 'pkg-stereolink' }
if (-not $CertDir) { $CertDir = Join-Path $Root 'certs' }
$EvidenceDir = Join-Path $Root 'evidence'

$Kit      = Join-Path ${env:ProgramFiles(x86)} 'Windows Kits\10'
$SignTool = Join-Path $Kit 'bin\10.0.26100.0\x64\signtool.exe'
$DevCon   = Join-Path $Kit 'Tools\10.0.26100.0\x64\devcon.exe'
$Inf      = Join-Path $PkgDir 'StereoLinkVad.inf'
$Sys      = Join-Path $PkgDir 'StereoLinkVad.sys'
$CerPath  = Join-Path $CertDir 'stereolink-dev.cer'
$Render   = 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Render'

$script:log = New-Object System.Collections.Generic.List[string]
function Say([string]$m) { Write-Host $m; $script:log.Add($m) }

foreach ($t in @($SignTool, $DevCon, $Inf, $Sys, $CerPath)) {
    if (-not (Test-Path $t)) { throw "Missing required file: $t" }
}
$isAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $isAdmin) { throw 'This script must run elevated (Administrator).' }
$ErrorActionPreference = 'Continue'   # native tools write diagnostics to stderr
New-Item -ItemType Directory -Force -Path $EvidenceDir | Out-Null

$cert  = New-Object System.Security.Cryptography.X509Certificates.X509Certificate2($CerPath)
$thumb = $cert.Thumbprint

function Get-OursDevices {
    $list = New-Object System.Collections.Generic.List[object]
    foreach ($d in @(Get-PnpDevice -ErrorAction SilentlyContinue)) {
        $svc = ''
        try { $svc = (Get-PnpDeviceProperty -InstanceId $d.InstanceId -KeyName 'DEVPKEY_Device_Service' -ErrorAction Stop).Data } catch { $svc = '' }
        if ($svc -eq $ServiceName -or $d.InstanceId -like '*StereoLinkVad*') { $list.Add($d) }
    }
    return $list
}

function Get-Snapshot {
    $devs = Get-OursDevices
    $detail = @()
    foreach ($d in $devs) {
        $problem = ''
        try { $problem = (Get-PnpDeviceProperty -InstanceId $d.InstanceId -KeyName 'DEVPKEY_Device_ProblemCode' -ErrorAction Stop).Data } catch { $problem = '' }
        $detail += [ordered]@{
            instanceId = $d.InstanceId
            status     = "$($d.Status)"
            class      = "$($d.Class)"
            friendly   = "$($d.FriendlyName)"
            problem    = "$problem"
        }
    }
    [ordered]@{
        time            = (Get-Date).ToString('o')
        secureBoot      = (Confirm-SecureBootUEFI -ErrorAction SilentlyContinue)
        bcdedit         = ((bcdedit /enum '{current}') -join "`n")
        renderEndpoints = @(Get-ChildItem $Render -ErrorAction SilentlyContinue).Count
        renderGuids     = @(Get-ChildItem $Render -ErrorAction SilentlyContinue | ForEach-Object { $_.PSChildName })
        ourDevices      = $detail
        service         = @(Get-Service -Name $ServiceName -ErrorAction SilentlyContinue | Select-Object Name, Status, StartType)
        sysFile         = (Test-Path (Join-Path $env:SystemRoot "System32\drivers\StereoLinkVad.sys"))
    }
}

Say ("=== install-vad.ps1  cert thumbprint " + $thumb + "  admin=" + $isAdmin)
$before = Get-Snapshot
Say ("secureBoot=" + $before.secureBoot + "  testsigning in bcdedit=" + [bool]($before.bcdedit -match 'testsigning'))
Say ("render endpoints before=" + $before.renderEndpoints + "  existing StereoLinkVad devices=" + $before.ourDevices.Count)

# ------------------------------------------------------------------ uninstall --
if ($Uninstall) {
    Say '=== UNINSTALL'
    foreach ($d in @(Get-OursDevices)) {
        Say ("--- remove device " + $d.InstanceId)
        Say ((pnputil /remove-device $d.InstanceId 2>&1 | Out-String).TrimEnd())
    }
    $pkgs = @(pnputil /enum-drivers 2>&1 | Out-String)
    $oemNames = [regex]::Matches($pkgs, '(?im)^\s*(oem\d+\.inf)') | ForEach-Object { $_.Groups[1].Value } | Select-Object -Unique
    foreach ($oem in $oemNames) {
        $block = ($pkgs -split "(?im)^(?=\s*$([regex]::Escape($oem)))") | Where-Object { $_ -match [regex]::Escape($oem) }
        if ($block -and ($block -join ' ') -match '(?i)stereolinkvad\.inf') {
            Say ("--- delete driver package " + $oem)
            Say ((pnputil /delete-driver $oem /uninstall /force 2>&1 | Out-String).TrimEnd())
        }
    }
    if (Test-Path "HKLM:\SYSTEM\CurrentControlSet\Services\$ServiceName") {
        Say "--- delete leftover kernel service $ServiceName"
        Say ((cmd /c "sc delete $ServiceName 2>&1" | Out-String).TrimEnd())
    }
    if (-not $KeepCertificateOnUninstall) {
        foreach ($store in @('Root', 'TrustedPublisher')) {
            $hit = Get-ChildItem "Cert:\LocalMachine\$store" -ErrorAction SilentlyContinue | Where-Object Thumbprint -eq $thumb
            if ($hit) { Remove-Item ("Cert:\LocalMachine\$store\" + $hit.Thumbprint) -Force; Say "--- removed cert from $store" }
        }
    }
    $after = Get-Snapshot
    Say ("service key present      : " + (Test-Path "HKLM:\SYSTEM\CurrentControlSet\Services\$ServiceName"))
    Say ("drivers\StereoLinkVad.sys: " + $after.sysFile)
    Say ("render endpoints         : " + $after.renderEndpoints + " (before " + $before.renderEndpoints + ")")
    @{ before = $before; after = $after; uninstall = $true } | ConvertTo-Json -Depth 8 |
        Set-Content (Join-Path $EvidenceDir 'install-vad-run.json') -Encoding UTF8
    $script:log -join "`r`n" | Set-Content (Join-Path $EvidenceDir 'install-vad-run.log') -Encoding UTF8
    exit 0
}

# -------------------------------------------------------------- 1. trust import
Say '=== [1/6] import public certificate into LocalMachine Root + TrustedPublisher'
foreach ($store in @('Root', 'TrustedPublisher')) {
    $exists = Get-ChildItem "Cert:\LocalMachine\$store" -ErrorAction SilentlyContinue | Where-Object Thumbprint -eq $thumb
    if ($exists) { Say ("  already present in $store") }
    else { Say ((Import-Certificate -FilePath $CerPath -CertStoreLocation "Cert:\LocalMachine\$store" | Out-String).TrimEnd()) }
}

# --------------------------------------------------------- 2. verify signatures
Say '=== [2/6] signtool verify after trust import'
$ver = [ordered]@{}
foreach ($mode in @('pa', 'kp')) {
    $out = & $SignTool verify /v /$mode $Sys 2>&1 | Out-String
    $ver["sys_$mode"] = @{ exit = $LASTEXITCODE; output = $out }
    Say ("  sys /$mode exit=" + $LASTEXITCODE)
}
$out = & $SignTool verify /v /pa (Join-Path $PkgDir 'stereolinkvad.cat') 2>&1 | Out-String
$ver['cat_pa'] = @{ exit = $LASTEXITCODE; output = $out }
Say ("  cat /pa exit=" + $LASTEXITCODE)

if ($TrustOnly) {
    Say 'TrustOnly: stopping before staging/installing.'
    @{ before = $before; verify = $ver; trustOnly = $true } | ConvertTo-Json -Depth 8 |
        Set-Content (Join-Path $EvidenceDir 'install-vad-run.json') -Encoding UTF8
    $script:log -join "`r`n" | Set-Content (Join-Path $EvidenceDir 'install-vad-run.log') -Encoding UTF8
    exit 0
}

# ------------------------------------------------------------ 3. pnputil stage
Say '=== [3/6] pnputil /add-driver StereoLinkVad.inf /install'
$stage = pnputil /add-driver $Inf /install 2>&1 | Out-String
Say $stage.TrimEnd()
$stageExit = $LASTEXITCODE
Say ("  exit=" + $stageExit)

# ------------------------------------------------------------- 4. devcon add --
Say '=== [4/6] devcon install StereoLinkVad.inf *StereoLinkVad (create root devnode)'
$devcon = & $DevCon install $Inf $HwId 2>&1 | Out-String
Say $devcon.TrimEnd()
$devconExit = $LASTEXITCODE
Say ("  exit=" + $devconExit)

Start-Sleep -Seconds 5
$after = Get-Snapshot

# -------------------------------------------------------------- 5. device state
Say '=== [5/6] device state after install'
foreach ($d in $after.ourDevices) { Say ("  " + ($d | ConvertTo-Json -Compress)) }
Say ("  render endpoints after=" + $after.renderEndpoints + " (before " + $before.renderEndpoints + ")")
$new = @($after.renderGuids | Where-Object { $before.renderGuids -notcontains $_ })
$epDetail = @()
foreach ($g in $new) {
    $props = Get-ItemProperty "$Render\$g\Properties" -ErrorAction SilentlyContinue
    $names = @()
    if ($props) {
        $names = $props.PSObject.Properties | Where-Object { $_.Name -notlike 'PS*' } |
            ForEach-Object { "$($_.Name)=$($_.Value)" }
    }
    $state = (Get-ItemProperty "$Render\$g" -Name DeviceState -ErrorAction SilentlyContinue).DeviceState
    $epDetail += [ordered]@{ guid = $g; deviceState = "$state"; properties = $names }
    Say ("  NEW endpoint " + $g + "  DeviceState=" + $state)
    foreach ($n in $names) { Say ("      " + $n) }
}

Say '=== pnputil /enum-devices /class Media (StereoLinkVad only)'
$mediaOut = pnputil /enum-devices /class Media 2>&1 | Out-String
Say (($mediaOut -split "`r?`n" | Select-String -Pattern 'StereoLink|Instance ID|Device Description|Status|Problem' -Context 0,0 | ForEach-Object { $_.Line }) -join "`n")

Say ("=== pnputil /enum-drivers (StereoLinkVad) ; service " + $ServiceName)
Say ((pnputil /enum-drivers 2>&1 | Out-String) -split "`r?`n" | Select-String -Pattern 'StereoLinkVad|stereolinkvad' | ForEach-Object { $_.Line })

$svc = Get-Service -Name $ServiceName -ErrorAction SilentlyContinue
Say ("  service: " + (($svc | Format-List Name,Status,StartType | Out-String).Trim()))

# --------------------------------------------------- 6. code integrity evidence
Say '=== [6/6] CodeIntegrity / Kernel-PnP events (last 10 minutes)'
$ci = @(Get-WinEvent -LogName 'Microsoft-Windows-CodeIntegrity/Operational' -MaxEvents 40 -ErrorAction SilentlyContinue |
        Where-Object { $_.TimeCreated -gt (Get-Date).AddMinutes(-10) } |
        Select-Object TimeCreated, Id, LevelDisplayName, Message)
foreach ($e in $ci) { Say ("  CI [" + $e.Id + "] " + ($e.Message -replace "`r?`n", ' ')) }
$pnp = @(Get-WinEvent -FilterHashtable @{LogName='System'; StartTime=(Get-Date).AddMinutes(-10)} -ErrorAction SilentlyContinue |
        Where-Object { $_.ProviderName -match 'Kernel-PnP|Service Control Manager' } |
        Select-Object -First 12 TimeCreated, Id, ProviderName, Message)
foreach ($e in $pnp) { Say ("  " + $e.ProviderName + " [" + $e.Id + "] " + ($e.Message -replace "`r?`n", ' ')) }

$journal = [ordered]@{
    before     = $before
    verify     = $ver
    pnputilAdd = @{ exit = $stageExit; output = $stage }
    devconAdd  = @{ exit = $devconExit; output = $devcon }
    after      = $after
    newEndpoints = $epDetail
    codeIntegrity = $ci
    pnpEvents     = $pnp
}
$journal | ConvertTo-Json -Depth 8 | Set-Content (Join-Path $EvidenceDir 'install-vad-run.json') -Encoding UTF8
$script:log -join "`r`n" | Set-Content (Join-Path $EvidenceDir 'install-vad-run.log') -Encoding UTF8
Say ("journal -> " + (Join-Path $EvidenceDir 'install-vad-run.json'))
