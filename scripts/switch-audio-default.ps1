# Switch the Windows default audio output endpoint (classic IPolicyConfig COM trick).
#
# This script is shipped inside the desktop panel (embedded with include_str! and written
# to %TEMP%\stereo-link\switch-audio-default.ps1 at runtime), so keep it ASCII-only:
# Windows PowerShell 5.1 reads BOM-less files as ANSI, and non-ASCII comments break parsing.
#
# Usage:
#   powershell -NoProfile -ExecutionPolicy Bypass -File switch-audio-default.ps1 -Id '{0.0.0.00000000}.{guid}'
#   powershell -NoProfile -ExecutionPolicy Bypass -File switch-audio-default.ps1 -List
#   powershell -NoProfile -ExecutionPolicy Bypass -File switch-audio-default.ps1 -NameLike 'Xiaomi Sound'
param(
    [string]$Id = '',
    [string]$NameLike = '',
    [switch]$List,
    [string]$EndpointsExe = (Join-Path $PSScriptRoot '..\target\debug\endpoints.exe')
)

$ErrorActionPreference = 'Stop'

$cs = @'
using System;
using System.Runtime.InteropServices;

namespace StereoLinkAudioSwitch {
    [ComImport, Guid("870AF99C-171D-4F9E-AF0D-E63DF40C2BC9")]
    internal class CPolicyConfigClient { }

    [Guid("F8679F50-850A-41CF-9C72-430F290290C8"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    internal interface IPolicyConfig {
        void GetMixFormat([MarshalAs(UnmanagedType.LPWStr)] string pszDeviceName, out IntPtr ppFormat);
        void GetDeviceFormat([MarshalAs(UnmanagedType.LPWStr)] string pszDeviceName, [MarshalAs(UnmanagedType.Bool)] bool bDefault, out IntPtr ppFormat);
        void ResetDeviceFormat([MarshalAs(UnmanagedType.LPWStr)] string pszDeviceName);
        void SetDeviceFormat([MarshalAs(UnmanagedType.LPWStr)] string pszDeviceName, IntPtr pEndpointFormat, IntPtr MixFormat);
        void GetProcessingPeriod([MarshalAs(UnmanagedType.LPWStr)] string pszDeviceName, [MarshalAs(UnmanagedType.Bool)] bool bDefault, out IntPtr pmftDefaultPeriod, out IntPtr pmftMinimumPeriod);
        void SetProcessingPeriod([MarshalAs(UnmanagedType.LPWStr)] string pszDeviceName, IntPtr pmftPeriod);
        void GetShareMode([MarshalAs(UnmanagedType.LPWStr)] string pszDeviceName, out IntPtr pMode);
        void SetShareMode([MarshalAs(UnmanagedType.LPWStr)] string pszDeviceName, IntPtr pMode);
        void GetPropertyValue([MarshalAs(UnmanagedType.LPWStr)] string pszDeviceName, [MarshalAs(UnmanagedType.Bool)] bool bFxStore, ref IntPtr key, out IntPtr pv);
        void SetPropertyValue([MarshalAs(UnmanagedType.LPWStr)] string pszDeviceName, [MarshalAs(UnmanagedType.Bool)] bool bFxStore, ref IntPtr key, ref IntPtr pv);
        void SetDefaultEndpoint([MarshalAs(UnmanagedType.LPWStr)] string pszDeviceName, int role);
        void SetEndpointVisibility([MarshalAs(UnmanagedType.LPWStr)] string pszDeviceName, [MarshalAs(UnmanagedType.Bool)] bool bVisible);
    }

    public static class Switcher {
        public static void SetDefault(string deviceId) {
            var pc = (IPolicyConfig)new CPolicyConfigClient();
            // role: 0 = Console, 1 = Multimedia, 2 = Communications
            pc.SetDefaultEndpoint(deviceId, 0);
            pc.SetDefaultEndpoint(deviceId, 1);
            pc.SetDefaultEndpoint(deviceId, 2);
        }
    }
}
'@
Add-Type -TypeDefinition $cs

function Get-RenderEndpoints {
    if (-not (Test-Path $EndpointsExe)) {
        throw "endpoints.exe not found: $EndpointsExe (build it with: cargo build -p stereo-audio --bin endpoints)"
    }
    $text = (& $EndpointsExe 2>&1 | Out-String)
    $result = @()
    foreach ($line in ($text -split "`r?`n")) {
        if ($line -notmatch '^\s+\[render') { continue }
        if ($line -notmatch '(\{0\.0\.[01]\.00000000\}\.\{[0-9a-fA-F-]{36}\})\s*$') { continue }
        $endpointId = $Matches[1]
        $name = ($line -replace '^\s+\[[^\]]*\]\s+', '') -replace '\s+\|.*$', ''
        $result += [pscustomobject]@{ Name = $name.Trim(); Id = $endpointId }
    }
    return $result
}

if ($Id) {
    [StereoLinkAudioSwitch.Switcher]::SetDefault($Id)
    "Default output set to: $Id"
    exit 0
}

$devices = @(Get-RenderEndpoints)

if ($List) {
    $devices | Sort-Object Name | ForEach-Object { "{0}  <-  {1}" -f $_.Name, $_.Id }
    exit 0
}

if (-not $NameLike) {
    Write-Output 'Nothing to do. Use -Id, -List or -NameLike.'
    exit 1
}

$match = $devices | Where-Object { $_.Name -like "*$NameLike*" } | Select-Object -First 1
if (-not $match) {
    Write-Output "No enabled render endpoint matching '$NameLike'. Available:"
    $devices | Sort-Object Name | ForEach-Object { "  {0}  <-  {1}" -f $_.Name, $_.Id }
    exit 1
}
[StereoLinkAudioSwitch.Switcher]::SetDefault($match.Id)
"Default output set to: $($match.Name)"
"Device id: $($match.Id)"
