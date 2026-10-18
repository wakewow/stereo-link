#Requires -Version 5.1
<#
.SYNOPSIS
  StereoLink GUI helper: rename the virtual sound card device (self-elevating).

.DESCRIPTION
  Called by the StereoLink panel. The only supported way to rename the device is
  to write FriendlyName under

    HKLM\SYSTEM\CurrentControlSet\Enum\ROOT\STEAMSTREAMINGSPEAKERS\0000

  (requires an administrator token) and then restart the AudioEndpointBuilder
  service so the audio stack picks the new name up. Disable/Enable-PnpDevice is
  NOT used: on current Windows 11 the DeviceFormat/OEMFormat paths are fine but
  pnputil/Disable-PnpDevice can leave the device in "pending reboot" state.

  This wrapper exists because the GUI itself normally runs non-elevated while
  the rename needs an administrator token:
    * unelevated call  -> re-launches itself via UAC (Start-Process -Verb RunAs)
    * elevated call    -> writes the registry value, restarts AudioEndpointBuilder,
                          polls until the new name is visible, logs everything

  Log protocol (polled by the GUI):
    ELEVATE            - UAC prompt is being shown
    BACKUP=<path>      - registry backup written
    WROTE=<name>       - FriendlyName written
    RESTARTING         - AudioEndpointBuilder restart in progress
    VERIFIED=<name>    - readback after restart matches
    EXITCODE=<n>       - finished; 0 = success
#>
param(
    [Parameter(Mandatory = $true)]
    [string]$NewName,

    [string]$InstanceId = 'ROOT\MEDIA\0001',

    [string]$LogFile = ''
)

$ErrorActionPreference = 'Continue'

if ([string]::IsNullOrWhiteSpace($LogFile)) {
    $LogFile = Join-Path (Join-Path $env:TEMP 'stereo-link') 'rename.log'
}
$logDir = Split-Path -Parent $LogFile
if ($logDir -and -not (Test-Path $logDir)) {
    New-Item -ItemType Directory -Force -Path $logDir | Out-Null
}

function Write-Log([string]$message) {
    $stamp = (Get-Date).ToString('HH:mm:ss')
    Add-Content -Path $LogFile -Value ("{0} {1}" -f $stamp, $message) -Encoding UTF8
}

$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = New-Object Security.Principal.WindowsPrincipal($identity)
$isAdmin = $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)

if (-not $isAdmin) {
    Write-Log 'ELEVATE'
    $inner = '-NoProfile -ExecutionPolicy Bypass -File "{0}" -NewName "{1}" -InstanceId "{2}" -LogFile "{3}"' -f `
        $PSCommandPath, $NewName, $InstanceId, $LogFile
    try {
        Start-Process -FilePath 'powershell.exe' -ArgumentList $inner -Verb RunAs -WindowStyle Hidden
    } catch {
        Write-Log ("ELEVATE_FAILED " + $_.Exception.Message)
        Write-Log 'EXITCODE=1'
    }
    exit 0
}

Write-Log ("ELEVATED as " + $identity.Name)
$key = 'HKLM:\SYSTEM\CurrentControlSet\Enum\' + $InstanceId

if (-not (Test-Path $key)) {
    Write-Log ("ERROR key not found: " + $key)
    Write-Log 'EXITCODE=1'
    exit 1
}

# 1) Back up the whole Enum key before touching anything.
$backupDir = Join-Path $PSScriptRoot 'backups'
if (-not (Test-Path $backupDir)) {
    New-Item -ItemType Directory -Force -Path $backupDir | Out-Null
}
$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$safeInstance = ($InstanceId -replace '[\\/:*?"<>|]', '_')
$backup = Join-Path $backupDir ("backup-Enum-{0}.{1}.reg" -f $safeInstance, $stamp)
$regPath = 'HKLM\SYSTEM\CurrentControlSet\Enum\' + $InstanceId
& reg.exe export $regPath $backup /y *> $null
if (Test-Path $backup) {
    Write-Log ("BACKUP=" + $backup)
} else {
    Write-Log 'WARN backup failed (continuing)'
}

# 2) Write FriendlyName (REG_SZ).
try {
    $property = Get-ItemProperty -Path $key -Name 'FriendlyName' -ErrorAction SilentlyContinue
    if ($null -eq $property) {
        New-ItemProperty -Path $key -Name 'FriendlyName' -Value $NewName -PropertyType String -Force | Out-Null
    } else {
        Set-ItemProperty -Path $key -Name 'FriendlyName' -Value $NewName
    }
} catch {
    Write-Log ('ERROR write FriendlyName: ' + $_.Exception.Message)
    Write-Log 'EXITCODE=1'
    exit 1
}

$readback = (Get-ItemProperty -Path $key -Name 'FriendlyName' -ErrorAction SilentlyContinue).FriendlyName
if ($readback -ne $NewName) {
    Write-Log ('ERROR readback mismatch: ' + $readback)
    Write-Log 'EXITCODE=1'
    exit 1
}
Write-Log ("WROTE=" + $readback)

# 3) Restart AudioEndpointBuilder (audio drops for ~2-3 s). Audiosrv is its
#    dependent service and restarts with it.
Write-Log 'RESTARTING'
try {
    Restart-Service -Name 'AudioEndpointBuilder' -Force -ErrorAction Stop
} catch {
    Write-Log ('ERROR restart AudioEndpointBuilder: ' + $_.Exception.Message)
    Write-Log 'EXITCODE=1'
    exit 1
}

# 4) Poll until the service is running again and the value is still there.
$verified = $false
for ($i = 0; $i -lt 40; $i++) {
    Start-Sleep -Milliseconds 300
    $svc = Get-Service -Name 'AudioEndpointBuilder' -ErrorAction SilentlyContinue
    if ($svc -and $svc.Status -eq 'Running') {
        $now = (Get-ItemProperty -Path $key -Name 'FriendlyName' -ErrorAction SilentlyContinue).FriendlyName
        if ($now -eq $NewName) {
            $verified = $true
            break
        }
    }
}
if ($verified) {
    Write-Log ("VERIFIED=" + $NewName)
    Write-Log 'EXITCODE=0'
} else {
    Write-Log 'ERROR AudioEndpointBuilder did not come back with the new name'
    Write-Log 'EXITCODE=1'
}
