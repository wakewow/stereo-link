<#
    build.ps1 - build Scream.sys (PortCls/WDM virtual audio driver) for miplay-bridge.

    Why not msbuild?
      This machine has Visual Studio 2022 *BuildTools* only, and installing the WDK
      (Microsoft.WindowsWDK.10.0.26100, installed 2026-10-17) does NOT register the
      WDK Visual Studio integration: the instance's
      VC\v170\Platforms\x64\PlatformToolsets\ contains only v143, there is no
      WindowsKernelModeDriver10.0 toolset and no
      MSBuild\Microsoft\VC\v170\Microsoft.Cpp.<plat>.WindowsKernelModeDriver.props,
      so msbuild fails with MSB8020 ("The build tools for WindowsKernelModeDriver10.0
      cannot be found"). Instead of modifying the Visual Studio installation we drive
      cl.exe / rc.exe / link.exe directly with the same flags the WDK props would use
      (see research/vad/driver-build.md for the flag-by-flag provenance).

    Usage:
        pwsh -File build.ps1                 # Release x64 -> out\x64\Release\Scream.sys
        pwsh -File build.ps1 -Clean
        pwsh -File build.ps1 -Config Debug
#>
[CmdletBinding()]
param(
    [ValidateSet('Release', 'Debug')] [string]$Config = 'Release',
    [ValidateSet('x64')]              [string]$Platform = 'x64',
    [string]$KitVersion = '',
    [switch]$Clean,
    [switch]$KeepObjects
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$Root   = $PSScriptRoot
$SrcDir = Join-Path $Root 'scream-derived'
$OutDir = Join-Path $Root "out\$Platform\$Config"
$ObjDir = Join-Path $OutDir 'obj'
$BinName = 'Scream'

# ---------------------------------------------------------------- toolchain ---
$KitRoot = Join-Path ${env:ProgramFiles(x86)} 'Windows Kits\10'
if (-not (Test-Path $KitRoot)) { throw "WDK/SDK not found at $KitRoot" }

if (-not $KitVersion) {
    $KitVersion = Get-ChildItem (Join-Path $KitRoot 'Include') -Directory |
        Where-Object { Test-Path (Join-Path $_.FullName 'km\ntddk.h') } |
        Sort-Object { [version]$_.Name } -Descending |
        Select-Object -First 1 -ExpandProperty Name
}
if (-not $KitVersion) { throw 'No Windows Kits include dir with km\ntddk.h found.' }

$IncRoot  = Join-Path $KitRoot "Include\$KitVersion"
$KmLib    = Join-Path $KitRoot "Lib\$KitVersion\km\$Platform"
$KitBin   = Join-Path $KitRoot "bin\$KitVersion\x64"      # signtool, stampinf, makecert
$KitBin86 = Join-Path $KitRoot "bin\$KitVersion\x86"      # Inf2Cat lives here only
$KitTools = Join-Path $KitRoot "Tools\$KitVersion\x64"    # devcon

foreach ($p in @($IncRoot, $KmLib)) { if (-not (Test-Path $p)) { throw "Missing WDK path: $p" } }

$vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
if (-not (Test-Path $vswhere)) { throw 'vswhere.exe not found; Visual Studio Build Tools missing.' }
$vsPath = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
if (-not $vsPath) { throw 'No VS instance with the MSVC x64 toolset found.' }
$vcvars = Join-Path $vsPath 'VC\Auxiliary\Build\vcvars64.bat'
if (-not (Test-Path $vcvars)) { throw "vcvars64.bat not found at $vcvars" }

# Import the MSVC environment (cl/link/rc on PATH).
Write-Host "[1/5] MSVC environment from $vcvars"
cmd.exe /c "`"$vcvars`" >nul 2>&1 && set" | ForEach-Object {
    if ($_ -match '^([^=]+)=(.*)$') { Set-Item -Path ("env:" + $matches[1]) -Value $matches[2] }
}
$cl = (Get-Command cl.exe -ErrorAction SilentlyContinue)
if (-not $cl) { throw 'cl.exe not on PATH after vcvars64.' }
Write-Host ("      cl.exe  : " + $cl.Source)
Write-Host ("      MSVC    : " + $env:VCToolsVersion)
Write-Host ("      WDK     : " + $KitVersion + "  (km libs: " + $KmLib + ")")

# ------------------------------------------------------------------ outputs ---
if ($Clean -and (Test-Path $OutDir)) {
    Write-Host "[2/5] clean $OutDir"
    Remove-Item $OutDir -Recurse -Force
}
New-Item -ItemType Directory -Force -Path $ObjDir | Out-Null

# ------------------------------------------------------------------- flags ----
# Provenance: Windows Kits\10\build\10.0.26100.0\WindowsDriver.Common.props
#            + WindowsDriver.<arch>.props + Scream.vcxproj (upstream)
$defines = @(
    '_WIN64', '_AMD64_', 'AMD64',            # WindowsDriver.<arch>.props
    'POOL_NX_OPTIN=1',
    'DBG=0',                                 # fre build (UseDebugLibraries=false)
    '_WIN32_WINNT=0x0603', 'WINVER=0x0603', 'NTDDI_VERSION=0x06030000',  # TargetVersion WindowsV6.3
    # Upstream Scream declares the 13 IDmaChannel methods explicitly in
    # minstream.h:64-81 *and* still carries the vestigial `IMP_IDmaChannel;` at
    # minstream.h:84. portcls.h only defines IMP_IDmaChannel when
    # PC_IMPLEMENTATION is set (portcls.h:869), which would then redeclare all 13
    # -> C2535; leaving it undefined gives C4430. Neutralise it explicitly.
    'IMP_IDmaChannel='
    # No UNICODE/_UNICODE: upstream Scream is an ANSI driver (wave strings are
    # char based, e.g. savedata.cpp:314 passes PCHAR to RtlIpv4StringToAddress),
    # and the WDK does not define UNICODE for drivers by default.
)
$clFlags = @(
    '/nologo', '/c', '/W3', '/GS', '/Gz', '/EHs-c-', '/GR-', '/Gy', '/GF', '/Zp8',
    '/Zc:wchar_t-',
    '/kernel',                               # defines _KERNEL_MODE (wdm.h gates KeQueryInterruptTime and more on it)
    '/d1import_no_registry', '/d2AllowCompatibleILVersions',
    ('/I' + $IncRoot), ('/I' + (Join-Path $IncRoot 'km')), ('/I' + (Join-Path $IncRoot 'km\crt')),
    ('/I' + (Join-Path $IncRoot 'shared')), ('/I' + (Join-Path $IncRoot 'um'))
)
if ($Config -eq 'Release') {
    $clFlags += @('/O2', '/Os', '/d1nodatetime')
} else {
    $clFlags += @('/Od', '/Zi', '/DDBG=1')
}
$clFlags += ($defines | ForEach-Object { '/D' + $_ })

# Link deps: WindowsDriver.KernelMode.Wdm.props  +  Scream.vcxproj Win10|x64
$linkLibs = @(
    'BufferOverflowK.lib', 'ntoskrnl.lib', 'hal.lib', 'wmilib.lib',
    'portcls.lib', 'netio.lib', 'stdunk.lib'
)

# ------------------------------------------------------------------ compile ---
$sources = Get-ChildItem $SrcDir -Filter *.cpp | Sort-Object Name
if (-not $sources) { throw "No .cpp sources in $SrcDir" }
Write-Host ("[3/5] compiling " + $sources.Count + " translation units ($Config $Platform)")
$objs = @()
foreach ($s in $sources) {
    $obj = Join-Path $ObjDir ($s.BaseName + '.obj')
    $objs += $obj
    $clArgs = $clFlags + @(('/Fo' + $obj), ('/Fd' + (Join-Path $ObjDir 'vc.pdb')), $s.FullName)
    Write-Host ("      cl " + $s.Name)
    & cl.exe @clArgs
    if ($LASTEXITCODE -ne 0) { throw "cl.exe failed on $($s.Name) (exit $LASTEXITCODE)" }
}

# ------------------------------------------------------------- resources ------
Write-Host '[4/5] compiling resources (scream.rc)'
$res = Join-Path $ObjDir ($BinName + '.res')
& rc.exe /nologo /fo $res `
    ('/d_WIN64') ('/d_AMD64_=1') ('/dAMD64') `
    ('/i' + $SrcDir) ('/i' + $IncRoot) ('/i' + (Join-Path $IncRoot 'um')) ('/i' + (Join-Path $IncRoot 'shared')) `
    (Join-Path $SrcDir 'scream.rc')
if ($LASTEXITCODE -ne 0) { throw "rc.exe failed (exit $LASTEXITCODE)" }

# ------------------------------------------------------------------- link -----
Write-Host '[5/5] linking'
$sys = Join-Path $OutDir ($BinName + '.sys')
$linkArgs = @(
    '/nologo', '/DRIVER', '/SUBSYSTEM:NATIVE', '/MACHINE:X64',
    '/ENTRY:GsDriverEntry',            # buffer-overflow-check wrapper in BufferOverflowK.lib
    '/INCREMENTAL:NO', '/OPT:REF', '/OPT:ICF', '/RELEASE',
    '/MERGE:_TEXT=.text', '/MERGE:_PAGE=PAGE', '/SECTION:INIT,d',
    '/VERSION:10.0', '/osversion:10.0',
    ('/LIBPATH:' + $KmLib),
    ('/OUT:' + $sys)
)
if ($Config -eq 'Debug') { $linkArgs += @('/DEBUG', ('/PDB:' + (Join-Path $OutDir ($BinName + '.pdb')))) }
$linkArgs += @($res) + $objs + $linkLibs
& link.exe @linkArgs
if ($LASTEXITCODE -ne 0) { throw "link.exe failed (exit $LASTEXITCODE)" }

if (-not $KeepObjects) { Remove-Item $ObjDir -Recurse -Force }

Write-Host ''
Write-Host "OK  ->  $sys"
Get-Item $sys | ForEach-Object { Write-Host ("        {0} bytes, {1}" -f $_.Length, $_.LastWriteTime) }


