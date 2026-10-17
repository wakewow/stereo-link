<#
    install.ps1 - trust our self-signed certificate and install the Scream-derived
                  virtual audio driver, capturing every piece of evidence needed to
                  answer: "can a self-signed kernel driver load on this machine
                  WITHOUT enabling test signing?"

    System changes it makes (all reversible, all listed in the run journal):
      1. import the PUBLIC certificate (drivers\certs\miplay-dev.cer) into
         Cert:\LocalMachine\Root and Cert:\LocalMachine\TrustedPublisher
      2. pnputil /add-driver  -> stage drivers\pkg\Scream.inf into the DriverStore
      3. devcon install <inf> *Scream -> create the root-enumerated device
    It never touches bcdedit, never removes/changes other audio devices and never
    changes the default playback device.

    Usage:
        # install + full evidence capture (writes research\vad\driver-install-run.*)
        powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\drivers\install.ps1
        # roll back everything this script created
        ... -File .\drivers\install.ps1 -Uninstall
        # trust + verify signatures only, do not create a device
        ... -File .\drivers\install.ps1 -TrustOnly
#>
[CmdletBinding()]
param(
    [string]$PkgDir      = '',
    [string]$CerPath     = '',
    [string]$EvidenceDir = '',
    [switch]$TrustOnly,
    [switch]$Uninstall,
    [switch]$KeepCertificateOnUninstall
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$Root = $PSScriptRoot
if (-not $PkgDir)      { $PkgDir      = Join-Path $Root 'pkg' }
if (-not $CerPath)     { $CerPath     = Join-Path $Root 'certs\miplay-dev.cer' }
if (-not $EvidenceDir) { $EvidenceDir = Join-Path (Split-Path $Root -Parent) 'research\vad' }

$Kit     = Join-Path ${env:ProgramFiles(x86)} 'Windows Kits\10'
$SignTool= Join-Path $Kit 'bin\10.0.26100.0\x64\signtool.exe'
$DevCon  = Join-Path $Kit 'Tools\10.0.26100.0\x64\devcon.exe'
$Inf     = Join-Path $PkgDir 'Scream.inf'
$Sys     = Join-Path $PkgDir 'Scream.sys'
$HwId    = '*Scream'

$journal = [ordered]@{}
$script:log = New-Object System.Collections.Generic.List[string]
function Say([string]$m) { Write-Host $m; $script:log.Add($m) }
function Run([string]$label, [scriptblock]$body) {
    Say "--- $label"
    $out = & $body 2>&1 | Out-String
    Say $out.TrimEnd()
    return $out
}

# --------------------------------------------------------------- preconditions
foreach ($t in @($SignTool, $DevCon, $Inf, $Sys, $CerPath)) {
    if (-not (Test-Path $t)) { throw "Missing required file: $t" }
}
$isAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $isAdmin) { throw 'This script must run elevated (Administrator).' }
$ErrorActionPreference = 'Continue'   # native tools write to stderr on failure
New-Item -ItemType Directory -Force -Path $EvidenceDir | Out-Null

$cert = New-Object System.Security.Cryptography.X509Certificates.X509Certificate2($CerPath)
$thumb = $cert.Thumbprint

# ------------------------------------------------------------------- snapshots
function Get-Snapshot {
    $render = 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Render'
    [ordered]@{
        time            = (Get-Date).ToString('o')
        secureBoot      = (Confirm-SecureBootUEFI -ErrorAction SilentlyContinue)
        bcdedit         = ((bcdedit /enum '{current}') -join "`n")
        renderEndpoints = @(Get-ChildItem $render -ErrorAction SilentlyContinue).Count
        renderGuids     = @(Get-ChildItem $render -ErrorAction SilentlyContinue | ForEach-Object { $_.PSChildName })
        screamDevices   = @(Get-PnpDevice -ErrorAction SilentlyContinue | Where-Object { $_.InstanceId -like '*Scream*' -or $_.FriendlyName -like '*Scream*' } |
                             Select-Object Status, Class, FriendlyName, InstanceId, Problem, ProblemDescription)
        screamService   = @(Get-Service -Name Scream -ErrorAction SilentlyContinue | Select-Object Name, Status, StartType)
        oemDrivers      = @(pnputil /enum-drivers 2>&1 | Select-String -Pattern 'Published Name|原始名称|Provider Name|Class Name|Scream' | ForEach-Object { $_.Line.Trim() })
        trustRoot       = [bool](Get-ChildItem Cert:\LocalMachine\Root -ErrorAction SilentlyContinue | Where-Object Thumbprint -eq $thumb)
        trustPublisher  = [bool](Get-ChildItem Cert:\LocalMachine\TrustedPublisher -ErrorAction SilentlyContinue | Where-Object Thumbprint -eq $thumb)
    }
}

Say ("=== install.ps1  cert thumbprint " + $thumb + "  admin=" + $isAdmin)
$before = Get-Snapshot
$journal['before'] = $before
Say ("secureBoot=" + $before.secureBoot + "  testsigning in bcdedit=" + [bool]($before.bcdedit -match 'testsigning'))
Say ("render endpoints before=" + $before.renderEndpoints + "  existing Scream devices=" + $before.screamDevices.Count)

# --------------------------------------------------------------- uninstall path
if ($Uninstall) {
    Say '=== UNINSTALL'
    $devs = @(Get-PnpDevice -ErrorAction SilentlyContinue | Where-Object { $_.InstanceId -like '*Scream*' -or $_.FriendlyName -like '*Scream*' })
    foreach ($d in $devs) {
        Run ("remove device " + $d.InstanceId) { pnputil /remove-device $d.InstanceId }
    }
    $pkgs = @(pnputil /enum-drivers 2>&1 | Out-String)
    $oemNames = [regex]::Matches($pkgs, '(?im)^\s*(oem\d+\.inf)') | ForEach-Object { $_.Groups[1].Value } | Select-Object -Unique
    foreach ($oem in $oemNames) {
        # only delete packages whose original INF name is scream.inf
        $block = ($pkgs -split "(?im)^(?=\s*$([regex]::Escape($oem)))") | Where-Object { $_ -match [regex]::Escape($oem) }
        if ($block -and ($block -join ' ') -match '(?i)scream\.inf') {
            Run ("delete driver package $oem") { pnputil /delete-driver $oem /uninstall /force }
        }
    }
    if (-not $KeepCertificateOnUninstall) {
        foreach ($store in @('Root', 'TrustedPublisher')) {
            $hit = Get-ChildItem "Cert:\LocalMachine\$store" -ErrorAction SilentlyContinue | Where-Object Thumbprint -eq $thumb
            if ($hit) { Run ("remove cert from $store") { Remove-Item ("Cert:\LocalMachine\$store\" + $hit.Thumbprint) -Force } }
        }
        $userHit = Get-ChildItem Cert:\CurrentUser\My -ErrorAction SilentlyContinue | Where-Object Thumbprint -eq $thumb
        if ($userHit) { Run 'remove cert from CurrentUser\My' { Remove-Item ("Cert:\CurrentUser\My\" + $userHit.Thumbprint) -Force } }
    }
    $journal['uninstall'] = $true
    $journal['after'] = Get-Snapshot
    Say '=== state after uninstall'
    Say (($journal['after'] | ConvertTo-Json -Depth 4))
    $journal | ConvertTo-Json -Depth 6 | Set-Content (Join-Path $EvidenceDir 'driver-install-run.json') -Encoding UTF8
    $script:log -join "`r`n" | Set-Content (Join-Path $EvidenceDir 'driver-install-run.log') -Encoding UTF8
    exit 0
}

# -------------------------------------------------------------- 1. trust import
Say '=== [1/4] import public certificate into LocalMachine Root + TrustedPublisher'
foreach ($store in @('Root', 'TrustedPublisher')) {
    $exists = Get-ChildItem "Cert:\LocalMachine\$store" -ErrorAction SilentlyContinue | Where-Object Thumbprint -eq $thumb
    if ($exists) { Say ("  already present in $store") }
    else { Run ("Import-Certificate -> $store") { Import-Certificate -FilePath $CerPath -CertStoreLocation "Cert:\LocalMachine\$store" } }
}
$journal['trustImported'] = $true

# --------------------------------------------------------- 2. verify signatures
Say '=== [2/4] signtool verify after trust import'
$ver = [ordered]@{}
foreach ($mode in @('pa', 'kp')) {
    $out = & $SignTool verify /v /$mode $Sys 2>&1 | Out-String
    $ver[$mode] = @{ exit = $LASTEXITCODE; output = $out }
    Say ("  /$mode exit=" + $LASTEXITCODE)
    ($out -split "`n" | Where-Object { $_ -match 'Signing Certificate Chain|Issued to|Successfully verified|Error|not trusted|policy' } | Select-Object -First 8) | ForEach-Object { Say ("    " + $_.Trim()) }
}
$out = & $SignTool verify /v /pa (Join-Path $PkgDir 'scream.cat') 2>&1 | Out-String
$ver['cat'] = @{ exit = $LASTEXITCODE; output = $out }
Say ("  catalog /pa exit=" + $LASTEXITCODE)
$journal['verify'] = $ver

if ($TrustOnly) {
    $journal['trustOnly'] = $true
    Say 'TrustOnly: stopping before staging/installing the driver package.'
    $journal['after'] = Get-Snapshot
    $journal | ConvertTo-Json -Depth 6 | Set-Content (Join-Path $EvidenceDir 'driver-install-run.json') -Encoding UTF8
    $script:log -join "`r`n" | Set-Content (Join-Path $EvidenceDir 'driver-install-run.log') -Encoding UTF8
    exit 0
}

# ------------------------------------------------------------- 3. stage + install
Say '=== [3/4] stage driver package (pnputil /add-driver)'
$stage = pnputil /add-driver $Inf /install 2>&1 | Out-String
Say $stage
$stageExit = $LASTEXITCODE
$journal['pnputilAdd'] = @{ exit = $stageExit; output = $stage }

Say '=== [4/4] create root-enumerated device (devcon install)'
$devcon = & $DevCon install $Inf $HwId 2>&1 | Out-String
Say $devcon
$devconExit = $LASTEXITCODE
$journal['devconInstall'] = @{ exit = $devconExit; output = $devcon }

Start-Sleep -Seconds 3
$after = Get-Snapshot
$journal['after'] = $after
Say '=== device state after install'
Say (($after.screamDevices | ConvertTo-Json -Depth 4))
Say ("render endpoints after=" + $after.renderEndpoints + " (before " + $before.renderEndpoints + ")")
if ($after.renderEndpoints -gt $before.renderEndpoints) {
    $new = $after.renderGuids | Where-Object { $before.renderGuids -notcontains $_ }
    Say ("NEW render endpoint GUIDs: " + ($new -join ', '))
    foreach ($g in $new) {
        $pk = "HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Render\$g\Properties"
        $names = (Get-ItemProperty $pk -ErrorAction SilentlyContinue).PSObject.Properties |
                 Where-Object { $_.Name -notlike 'PS*' } | ForEach-Object { $_.Name }
        Say ("  $g properties: " + ($names -join ' | '))
        Say ("  $g state=" + (Get-ItemProperty "HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Render\$g" -Name DeviceState -ErrorAction SilentlyContinue).DeviceState)
    }
}

# service + code integrity evidence
Say '=== driver service + code integrity events'
Say ((Get-Service -Name Scream -ErrorAction SilentlyContinue | Format-List Name, Status, StartType | Out-String).Trim())
$ciLog = Get-WinEvent -LogName 'Microsoft-Windows-CodeIntegrity/Operational' -MaxEvents 12 -ErrorAction SilentlyContinue |
         Where-Object { $_.TimeCreated -gt (Get-Date).AddMinutes(-10) } |
         Select-Object TimeCreated, Id, LevelDisplayName, Message
$journal['codeIntegrity'] = @($ciLog)
foreach ($e in @($ciLog)) { Say ("  [" + $e.Id + "] " + ($e.Message -replace "`r?`n", ' ')) }
Say ((Get-WinEvent -FilterHashtable @{LogName='System'; StartTime=(Get-Date).AddMinutes(-10)} -ErrorAction SilentlyContinue |
      Where-Object { $_.ProviderName -match 'Kernel-PnP|Service Control Manager' } |
      Select-Object -First 8 TimeCreated, Id, ProviderName, @{n='Msg';e={($_.Message -replace "`r?`n",' ').Substring(0, [Math]::Min(160, $_.Message.Length))}} |
      Format-Table -AutoSize | Out-String).Trim())

$journal | ConvertTo-Json -Depth 6 | Set-Content (Join-Path $EvidenceDir 'driver-install-run.json') -Encoding UTF8
$script:log -join "`r`n" | Set-Content (Join-Path $EvidenceDir 'driver-install-run.log') -Encoding UTF8
Say ("journal -> " + (Join-Path $EvidenceDir 'driver-install-run.json'))
