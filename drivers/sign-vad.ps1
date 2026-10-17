# sign-vad.ps1 - self-signed cert + Inf2Cat + signtool for the StereoLink virtual
#                audio driver package (Scream-derived, MS-PL).
#
# ASCII only on purpose: Windows PowerShell 5.1 reads a BOM-less script as ANSI,
# so Chinese text in a .ps1 would be mangled.  Keep every string here ASCII.
#
# Usage:
#   powershell -NoProfile -ExecutionPolicy Bypass -File .\sign-vad.ps1
#   powershell -NoProfile -ExecutionPolicy Bypass -File .\sign-vad.ps1 -NewCert
[CmdletBinding()]
param(
    [string]$PkgDir   = '',
    [string]$CertDir  = '',
    [string]$Subject  = 'CN=StereoLink Virtual Audio Driver (dev)',
    [string]$Os       = '10_GE_X64',
    [string]$PfxPassword = 'stereolink-dev-only',
    [string]$TimestampUrl = 'http://timestamp.digicert.com',
    [switch]$NewCert,
    [switch]$SkipTimestamp
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$Root = $PSScriptRoot
if (-not $PkgDir)  { $PkgDir  = Join-Path $Root 'pkg-stereolink' }
if (-not $CertDir) { $CertDir = Join-Path $Root 'certs' }

$Kit      = Join-Path ${env:ProgramFiles(x86)} 'Windows Kits\10'
$BinX64   = Join-Path $Kit 'bin\10.0.26100.0\x64'
$BinX86   = Join-Path $Kit 'bin\10.0.26100.0\x86'
$Inf2Cat  = Join-Path $BinX86 'Inf2Cat.exe'
$SignTool = Join-Path $BinX64 'signtool.exe'
foreach ($t in @($Inf2Cat, $SignTool)) { if (-not (Test-Path $t)) { throw "Missing tool: $t" } }

New-Item -ItemType Directory -Force -Path $CertDir, $PkgDir | Out-Null
$PfxPath = Join-Path $CertDir 'stereolink-dev.pfx'
$CerPath = Join-Path $CertDir 'stereolink-dev.cer'
$Sys     = Join-Path $PkgDir 'StereoLinkVad.sys'
$Inf     = Join-Path $PkgDir 'StereoLinkVad.inf'

foreach ($f in @($Sys, $Inf)) { if (-not (Test-Path $f)) { throw "Missing package file: $f (run build first)" } }

# ------------------------------------------------------------- 1. certificate --
Write-Host '[1/5] code-signing certificate'
if ($NewCert -and (Test-Path $PfxPath)) { Remove-Item $PfxPath, $CerPath -Force -ErrorAction SilentlyContinue }
if ($NewCert -or -not (Test-Path $PfxPath) -or -not (Test-Path $CerPath)) {
    $cert = New-SelfSignedCertificate -Subject $Subject -Type CodeSigningCert `
        -CertStoreLocation 'Cert:\CurrentUser\My' -KeyAlgorithm RSA -KeyLength 2048 `
        -HashAlgorithm SHA256 -KeyExportPolicy Exportable -NotAfter (Get-Date).AddYears(5)
    $pwd = ConvertTo-SecureString -String $PfxPassword -Force -AsPlainText
    Export-PfxCertificate -Cert $cert -FilePath $PfxPath -Password $pwd | Out-Null
    Export-Certificate  -Cert $cert -FilePath $CerPath | Out-Null
    Write-Host "      created new cert in CurrentUser\My"
} else {
    Write-Host "      reusing $PfxPath"
    # Windows PowerShell 5.1 Get-PfxCertificate has no -Password parameter.
    $cert = Get-ChildItem Cert:\CurrentUser\My -CodeSigningCert -ErrorAction SilentlyContinue |
        Where-Object { $_.Subject -eq $Subject } | Sort-Object NotAfter -Descending | Select-Object -First 1
    if (-not $cert) {
        $secure = ConvertTo-SecureString -String $PfxPassword -Force -AsPlainText
        $cert = New-Object System.Security.Cryptography.X509Certificates.X509Certificate2($PfxPath, $secure)
    }
}
Write-Host ("      subject : " + $cert.Subject)
Write-Host ("      thumb   : " + $cert.Thumbprint)
Write-Host ("      notAfter: " + $cert.NotAfter.ToString('u'))
Write-Host ("      pfx     : " + $PfxPath)
Write-Host ("      cer     : " + $CerPath)

# -------------------------------------------------------------- 2. Inf2Cat -----
Write-Host "[2/5] Inf2Cat /os:$Os /driver:$PkgDir"
Get-ChildItem $PkgDir -Filter *.cat -ErrorAction SilentlyContinue | Remove-Item -Force
& $Inf2Cat /driver:"$PkgDir" /os:$Os /verbose
$inf2catExit = $LASTEXITCODE
Write-Host "      Inf2Cat exit=$inf2catExit"
$cat = Get-ChildItem $PkgDir -Filter *.cat | Select-Object -First 1
if (-not $cat) { throw "Inf2Cat did not produce a .cat in $PkgDir" }
Write-Host ("      catalog : " + $cat.Name + "  " + $cat.Length + " bytes")

# ---------------------------------------------------------------- 3. sign ------
Write-Host '[3/5] signtool sign (SHA-256, embedded)'
$signArgs = @('sign', '/v', '/fd', 'SHA256', '/f', $PfxPath, '/p', $PfxPassword)
if (-not $SkipTimestamp) { $signArgs += @('/tr', $TimestampUrl, '/td', 'SHA256') }
$signSections = [ordered]@{}
foreach ($target in @($Sys, $cat.FullName)) {
    Write-Host ("--- sign " + (Split-Path $target -Leaf))
    $out = & $SignTool @signArgs $target 2>&1 | Out-String
    $signSections[(Split-Path $target -Leaf)] = @{ exit = $LASTEXITCODE; output = $out }
    Write-Host ("      exit=" + $LASTEXITCODE)
    ($out -split "`r?`n" | Where-Object { $_ -match 'Done Adding|Successfully signed|Number of files|Error|failed' }) |
        ForEach-Object { Write-Host ("      " + $_.Trim()) }
}

# ---------------------------------------------------------------- 4. verify ----
Write-Host '[4/5] signtool verify'
$ErrorActionPreference = 'Continue'   # native tools write diagnostics to stderr
$ver = [ordered]@{}
foreach ($mode in @('pa', 'kp')) {
    Write-Host "--- verify /$mode StereoLinkVad.sys"
    $out = & $SignTool verify /v /$mode $Sys 2>&1 | Out-String
    $ver["sys_$mode"] = @{ exit = $LASTEXITCODE; output = $out }
    Write-Host ("      exit=" + $LASTEXITCODE)
    ($out -split "`r?`n" | Select-Object -First 14) | ForEach-Object { Write-Host ("      " + $_) }
}
Write-Host '--- verify /pa catalog'
$out = & $SignTool verify /v /pa $cat.FullName 2>&1 | Out-String
$ver['cat_pa'] = @{ exit = $LASTEXITCODE; output = $out }
Write-Host ("      exit=" + $LASTEXITCODE)
$ErrorActionPreference = 'Stop'

# ---------------------------------------------------------------- 5. record ----
$record = [ordered]@{
    time       = (Get-Date).ToString('o')
    subject    = $cert.Subject
    thumbprint = $cert.Thumbprint
    notAfter   = $cert.NotAfter.ToString('o')
    pfx        = $PfxPath
    cer        = $CerPath
    inf2cat    = @{ os = $Os; exit = $inf2catExit }
    catalog    = @{ path = $cat.FullName; bytes = $cat.Length }
    sign       = $signSections
    verify     = $ver
}
$record | ConvertTo-Json -Depth 6 | Set-Content (Join-Path $Root 'evidence\sign-vad.json') -Encoding UTF8
Write-Host ''
Write-Host 'SUMMARY'
Write-Host ("  thumbprint    : " + $cert.Thumbprint)
Write-Host ("  verify /pa sys: exit " + $ver['sys_pa'].exit)
Write-Host ("  verify /kp sys: exit " + $ver['sys_kp'].exit)
Write-Host ("  verify /pa cat: exit " + $ver['cat_pa'].exit)
Write-Host ("  record        : " + (Join-Path $Root 'evidence\sign-vad.json'))
