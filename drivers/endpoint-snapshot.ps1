<#
    endpoint-snapshot.ps1 - dump every Windows audio *render* endpoint with its
    display name, PnP instance id, state and the MMDevices property values that
    actually exist on this build (26100), so a before/after diff can prove that
    installing the virtual driver did not disturb any other audio device.

    The MMDevices\...\Properties values are serialized PROPVARIANT blobs
    (REG_BINARY), not plain strings, so they are decoded here explicitly.

    Usage:
        powershell.exe -NoProfile -ExecutionPolicy Bypass -File endpoint-snapshot.ps1 -OutFile <path.json>
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$OutFile
)
$ErrorActionPreference = 'Continue'
Set-StrictMode -Version Latest

$RenderRoot = 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Render'

function ConvertFrom-SerializedPropVariant {
    param([byte[]]$Bytes)
    if (-not $Bytes -or $Bytes.Length -lt 8) { return $null }
    $vt = [BitConverter]::ToUInt16($Bytes, 0)
    switch ($vt) {
        31 { # VT_LPWSTR
            $s = [Text.Encoding]::Unicode.GetString($Bytes, 8, $Bytes.Length - 8)
            return $s.TrimEnd([char]0)
        }
        19 { return [BitConverter]::ToUInt32($Bytes, 8) }               # VT_UI4
        18 { return [BitConverter]::ToUInt16($Bytes, 8) }               # VT_UI2
        11 { return ([BitConverter]::ToUInt16($Bytes, 8) -ne 0) }       # VT_BOOL
        5  { return [BitConverter]::ToDouble($Bytes, 8) }               # VT_R8
        3  { return [BitConverter]::ToInt32($Bytes, 8) }                # VT_I4
        default { return $null }
    }
}

$endpoints = New-Object System.Collections.Generic.List[object]
foreach ($key in @(Get-ChildItem $RenderRoot -ErrorAction SilentlyContinue)) {
    $guid = $key.PSChildName
    $state = (Get-ItemProperty -Path $key.PSPath -Name DeviceState -ErrorAction SilentlyContinue).DeviceState

    $props = [ordered]@{}
    $pk = Join-Path $key.PSPath 'Properties'
    foreach ($p in @(Get-ItemProperty -Path $pk -ErrorAction SilentlyContinue)) {
        foreach ($prop in @($p.PSObject.Properties)) {
            if ($prop.Name -like 'PS*') { continue }
            $val = $null
            if ($prop.Value -is [byte[]]) { $val = ConvertFrom-SerializedPropVariant -Bytes $prop.Value }
            else { $val = $prop.Value }
            if ($null -ne $val -and "$val" -ne '') { $props[$prop.Name] = $val }
        }
    }

    # map the MMDevices endpoint GUID to the PnP audio endpoint device
    $pnp = $null
    $pnpDev = @(Get-PnpDevice -ErrorAction SilentlyContinue |
                Where-Object { $_.InstanceId -like "*$guid*" })
    if ($pnpDev.Count -gt 0) {
        $pnp = [ordered]@{
            instanceId   = $pnpDev[0].InstanceId
            friendlyName = $pnpDev[0].FriendlyName      # <-- the string Windows shows in the device list
            status       = "$($pnpDev[0].Status)"
            problem      = "$($pnpDev[0].Problem)"
            problemText  = "$($pnpDev[0].ProblemDescription)"
            class        = "$($pnpDev[0].Class)"
        }
        $desc = Get-PnpDeviceProperty -InstanceId $pnpDev[0].InstanceId -KeyName 'DEVPKEY_Device_DeviceDesc' -ErrorAction SilentlyContinue
        if ($desc) { $pnp['deviceDesc'] = "$($desc.Data)" }
    }

    $endpoints.Add([ordered]@{
        guid             = $guid
        deviceState      = $state
        pnp              = $pnp
        mmdevicesProps   = $props
    })
}

# non-endpoint MEDIA class devices too (driver packages, topology filters, ...)
$mediaDevices = @(Get-PnpDevice -Class MEDIA -ErrorAction SilentlyContinue |
    Select-Object @{n = 'status'; e = { "$($_.Status)" } },
                  @{n = 'problem'; e = { "$($_.Problem)" } },
                  @{n = 'friendlyName'; e = { "$($_.FriendlyName)" } },
                  @{n = 'instanceId'; e = { "$($_.InstanceId)" } })

$bcdLines = @()
$bcdError = $null
try {
    $bcdLines = @(& bcdedit.exe /enum '{current}' 2>&1 | ForEach-Object { [string]$_ })
} catch {
    $bcdError = $_.Exception.Message
}

$result = [ordered]@{
    capturedAt     = (Get-Date).ToString('o')
    computer       = $env:COMPUTERNAME
    secureBoot     = (Confirm-SecureBootUEFI -ErrorAction SilentlyContinue)
    bcdedit        = $bcdLines
    bcdeditError   = $bcdError
    renderCount    = $endpoints.Count
    # NOTE: @($endpoints) is unsafe here - PowerShell 5.1 throws
    # "Argument types do not match" when array-wrapping a List[object] whose
    # elements are OrderedDictionary; List.ToArray() is the reliable form.
    render         = $endpoints.ToArray()
    mediaClassPnp  = $mediaDevices
    screamPackages = @(pnputil /enum-drivers 2>&1 | Select-String -Pattern 'scream' -SimpleMatch | ForEach-Object { "$($_.Line)".Trim() })
}

$json = $result | ConvertTo-Json -Depth 8
Set-Content -Path $OutFile -Value $json -Encoding UTF8
Write-Host ("wrote {0}  ({1} render endpoints, {2} MEDIA devices)" -f $OutFile, $endpoints.Count, $mediaDevices.Count)
foreach ($e in $endpoints) {
    $n = if ($e.pnp) { $e.pnp.friendlyName } else { '(no PnP device)' }
    Write-Host ("  {0}  state={1,-3} {2}" -f $e.guid, $e.deviceState, $n)
}
