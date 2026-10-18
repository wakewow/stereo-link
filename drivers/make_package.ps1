# make_package.ps1 - assemble the StereoLink virtual audio driver package that
#                    sign-vad.ps1 signs and install-vad.ps1 installs.
#
# ASCII only (Windows PowerShell 5.1 reads a BOM-less .ps1 as ANSI).
#
#   in : inf-src\StereoLinkVad.inf.utf8          (UTF-8 master, human editable)
#        drivers\out\x64\Release\StereoLinkVad.sys (produced by drivers\build.ps1)
#   out: pkg-stereolink\StereoLinkVad.inf         (UTF-16LE + BOM, INF requires Unicode)
#        pkg-stereolink\StereoLinkVad.sys
#
# Run build first:
#   powershell -NoProfile -ExecutionPolicy Bypass -File .\drivers\build.ps1 `
#       -Config Release -BinName StereoLinkVad
#
# After this script, run sign-vad.ps1 (Inf2Cat + signtool); the catalog hashes
# the .inf and the .sys, so any change here invalidates the old .cat.
[CmdletBinding()]
param(
    [string]$SrcInf = '',
    [string]$SrcSys = '',
    [string]$PkgDir = ''
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$Root = $PSScriptRoot
if (-not $PkgDir) { $PkgDir = Join-Path $Root 'pkg-stereolink' }
if (-not $SrcInf) { $SrcInf = Join-Path $Root 'inf-src\StereoLinkVad.inf.utf8' }
if (-not $SrcSys) { $SrcSys = Join-Path $Root 'drivers\out\x64\Release\StereoLinkVad.sys' }

foreach ($t in @($SrcInf, $SrcSys)) { if (-not (Test-Path $t)) { throw "Missing input: $t" } }
New-Item -ItemType Directory -Force -Path $PkgDir | Out-Null
Get-ChildItem $PkgDir -Filter *.cat -ErrorAction SilentlyContinue | Remove-Item -Force
Write-Host '      removed stale *.cat (Inf2Cat must run again on the new hashes)'

# INF: UTF-8 master -> UTF-16LE + BOM.  The master is kept with LF endings (git
# friendly); WIN-DRIVER INFs want CRLF, and normalising to CRLF here is what makes
# the output byte-identical to the package that was signed and installed.
# Only the .ps1 files in this directory must stay pure ASCII; the INF is read as
# UTF-8 here so the Chinese [Strings] values survive.
$text = [IO.File]::ReadAllText($SrcInf, [Text.Encoding]::UTF8)
$text = ($text -replace "`r`n", "`n") -replace "`n", "`r`n"
$infOut = Join-Path $PkgDir 'StereoLinkVad.inf'
[IO.File]::WriteAllText($infOut, $text, (New-Object Text.UnicodeEncoding($false, $true)))
$bytes = [IO.File]::ReadAllBytes($infOut)
Write-Host ("      INF : " + $infOut + "  " + $bytes.Length + " bytes  head=" +
            (($bytes[0..3] | ForEach-Object { $_.ToString('X2') }) -join ' '))

Copy-Item $SrcSys (Join-Path $PkgDir 'StereoLinkVad.sys') -Force
Write-Host ("      SYS : " + (Join-Path $PkgDir 'StereoLinkVad.sys'))

Write-Host ''
Write-Host 'SHA256'
foreach ($f in Get-ChildItem $PkgDir -File) {
    Write-Host ("  {0,-22} {1}  {2}" -f $f.Name, (Get-FileHash $f.FullName -Algorithm SHA256).Hash, $f.Length)
}
Write-Host ''
Write-Host 'Next: .\sign-vad.ps1   then   .\install-vad.ps1'
