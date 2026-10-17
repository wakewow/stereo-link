<#
    sign.ps1 - create a self-signed code-signing certificate, build the driver
               catalog (Inf2Cat) and sign Scream.sys + the catalog with signtool.

    What it does (each step is idempotent):
      1. (re)use a self-signed cert "CN=miplay-bridge Self-Signed Driver" with the
         Code Signing EKU; private key stays OUTSIDE the repo ($CertDir, default
         %TEMP%\vad-research\certs); only the public .cer is copied to
         drivers\certs\ for the record.
      2. stage the driver package (Scream.inf + Scream.sys -> drivers\pkg)
      3. Inf2Cat /os:<Os>  -> drivers\pkg\scream.cat
      4. signtool sign: embedded signature on Scream.sys, then signature on scream.cat
      5. signtool verify /pa (authenticode policy) and /kp (kernel policy) - the /kp
         result predicts whether the Windows loader will accept the signature.

    Does NOT touch any certificate store or install anything. The trust-store
    import (LocalMachine\Root + TrustedPublisher) and driver installation are a
    separate, explicitly approved step (see install.ps1 / driver-install.md).

    Usage:
        powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\drivers\sign.ps1
        ... -File .\drivers\sign.ps1 -NewCert -Os 10_GE_X64
#>
[CmdletBinding()]
param(
    # NOTE: defaults are resolved in the body - $PSScriptRoot is not populated yet
    # while param() defaults are evaluated under `powershell.exe -File`.
    [string]$SrcDir    = '',
    [string]$OutDir    = '',
    [string]$PkgDir    = '',
    [string]$CertDir   = (Join-Path $env:TEMP 'vad-research\certs'),
    [string]$CerRepoDir= '',
    [string]$Subject   = 'CN=miplay-bridge Self-Signed Driver',
    # Inf2Cat OS id: 10_GE_X64 = Windows 11 24H2 (build 26100). See `Inf2Cat /?`.
    [string]$Os        = '10_GE_X64',
    [string]$TimestampUrl = 'http://timestamp.digicert.com',
    [string]$PfxPassword = 'miplay-dev-only',
    [switch]$NewCert,
    [switch]$SkipTimestamp
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$Root = $PSScriptRoot
if (-not $SrcDir)     { $SrcDir     = Join-Path $Root 'scream-derived' }
if (-not $OutDir)     { $OutDir     = Join-Path $Root 'out\x64\Release' }
if (-not $PkgDir)     { $PkgDir     = Join-Path $Root 'pkg' }
if (-not $CerRepoDir) { $CerRepoDir = Join-Path $Root 'certs' }

$Kit     = Join-Path ${env:ProgramFiles(x86)} 'Windows Kits\10'
$BinX64  = Join-Path $Kit 'bin\10.0.26100.0\x64'     # signtool, makecert
$BinX86  = Join-Path $Kit 'bin\10.0.26100.0\x86'     # Inf2Cat lives only here
$Inf2Cat = Join-Path $BinX86 'Inf2Cat.exe'
$SignTool= Join-Path $BinX64 'signtool.exe'
foreach ($t in @($Inf2Cat, $SignTool)) { if (-not (Test-Path $t)) { throw "Missing tool: $t" } }

New-Item -ItemType Directory -Force -Path $CertDir, $CerRepoDir, $PkgDir | Out-Null
$PfxPath = Join-Path $CertDir 'miplay-dev.pfx'
$CerPath = Join-Path $CertDir 'miplay-dev.cer'

# ------------------------------------------------------------ 1. certificate --
if ($NewCert -or -not (Test-Path $PfxPath) -or -not (Test-Path $CerPath)) {
    Write-Host "[1/5] creating self-signed code-signing certificate"
    if ($NewCert -and (Test-Path $PfxPath)) { Remove-Item $PfxPath, $CerPath -Force -ErrorAction SilentlyContinue }
    $cert = New-SelfSignedCertificate `
        -Subject $Subject `
        -Type CodeSigningCert `
        -CertStoreLocation 'Cert:\CurrentUser\My' `
        -KeyAlgorithm RSA -KeyLength 2048 -HashAlgorithm SHA256 `
        -KeyExportPolicy Exportable `
        -NotAfter (Get-Date).AddYears(3)
    $pwd = ConvertTo-SecureString -String $PfxPassword -Force -AsPlainText
    Export-PfxCertificate -Cert $cert -FilePath $PfxPath -Password $pwd | Out-Null
    Export-Certificate  -Cert $cert -FilePath $CerPath | Out-Null
    Copy-Item $CerPath (Join-Path $CerRepoDir 'miplay-dev.cer') -Force
    Write-Host ("      cert  : " + $cert.Subject)
    Write-Host ("      thumb : " + $cert.Thumbprint)
} else {
    Write-Host "[1/5] reusing certificate $PfxPath"
    # Windows PowerShell 5.1's Get-PfxCertificate has no -Password parameter, so look the
    # cert up in the user store first (New-SelfSignedCertificate put it there) and only fall
    # back to loading the .pfx through the X509Certificate2 constructor.
    $cert = Get-ChildItem Cert:\CurrentUser\My -CodeSigningCert -ErrorAction SilentlyContinue |
        Where-Object { $_.Subject -eq $Subject } | Select-Object -First 1
    if (-not $cert) {
        $secure = ConvertTo-SecureString -String $PfxPassword -Force -AsPlainText
        $cert = New-Object System.Security.Cryptography.X509Certificates.X509Certificate2($PfxPath, $secure)
    }
    Write-Host ("      cert  : " + $cert.Subject + "  thumb " + $cert.Thumbprint)
}

# ------------------------------------------------------------- 2. stage files --
Write-Host "[2/5] staging driver package -> $PkgDir"
$sys = Join-Path $OutDir 'Scream.sys'
if (-not (Test-Path $sys)) { throw "Built driver not found: $sys (run build.ps1 first)" }
Copy-Item $sys (Join-Path $PkgDir 'Scream.sys') -Force
Copy-Item (Join-Path $SrcDir 'Scream.inf') (Join-Path $PkgDir 'Scream.inf') -Force

# --------------------------------------------------------------- 3. Inf2Cat ----
Write-Host "[3/5] Inf2Cat /os:$Os"
& $Inf2Cat /driver:"$PkgDir" /os:$Os /verbose
if ($LASTEXITCODE -ne 0) { throw "Inf2Cat failed (exit $LASTEXITCODE)" }
$cat = Get-ChildItem $PkgDir -Filter *.cat | Select-Object -First 1
if (-not $cat) { throw "Inf2Cat did not produce a .cat in $PkgDir" }
Write-Host ("      catalog: " + $cat.FullName + "  (" + $cat.Length + " bytes)")

# --------------------------------------------------------------- 4. signing ---
Write-Host "[4/5] signtool sign"
$signCommon = @('sign', '/v', '/fd', 'SHA256', '/f', $PfxPath, '/p', $PfxPassword)
if (-not $SkipTimestamp) { $signCommon += @('/tr', $TimestampUrl, '/td', 'SHA256') }
& $SignTool @signCommon (Join-Path $PkgDir 'Scream.sys')
if ($LASTEXITCODE -ne 0) { throw "signtool sign Scream.sys failed (exit $LASTEXITCODE)" }
& $SignTool @signCommon $cat.FullName
if ($LASTEXITCODE -ne 0) { throw "signtool sign catalog failed (exit $LASTEXITCODE)" }

# --------------------------------------------------------------- 5. verify ----
Write-Host "[5/5] signtool verify"
# signtool writes failures to stderr; with $ErrorActionPreference='Stop' PowerShell 5.1
# turns that into a terminating NativeCommandError, which would abort the script on an
# expected verification failure. Relax it just for the verify block and use $LASTEXITCODE.
$ErrorActionPreference = 'Continue'
$verify = @{}
foreach ($mode in @('pa', 'kp')) {
    Write-Host "--- verify /$mode Scream.sys"
    $out = & $SignTool verify /v /$mode (Join-Path $PkgDir 'Scream.sys') 2>&1
    $verify[$mode] = ($out -join "`n")
    $verify["${mode}Exit"] = $LASTEXITCODE
    $out | Select-Object -First 12 | ForEach-Object { "      " + $_ }
    Write-Host ("      exit=" + $LASTEXITCODE)
}
Write-Host "--- verify /pa catalog"
$out = & $SignTool verify /v /pa $cat.FullName 2>&1
$verify['cat'] = ($out -join "`n"); $verify['catExit'] = $LASTEXITCODE
$out | Select-Object -First 10 | ForEach-Object { "      " + $_ }
Write-Host ("      exit=" + $LASTEXITCODE)
$ErrorActionPreference = 'Stop'

# --------------------------------------------------------------- 6. summary ---
Write-Host ''
Write-Host "SUMMARY"
Write-Host ("  pfx (private, outside repo) : " + $PfxPath)
Write-Host ("  cer (public, in repo)       : " + (Join-Path $CerRepoDir 'miplay-dev.cer'))
Write-Host ("  thumbprint                  : " + $cert.Thumbprint)
Write-Host ("  signed sys                  : " + (Join-Path $PkgDir 'Scream.sys'))
Write-Host ("  signed cat                  : " + $cat.FullName)
Write-Host ("  verify /pa exit             : " + $verify['paExit'])
Write-Host ("  verify /kp exit             : " + $verify['kpExit'])
Write-Host ''
if ($verify['kpExit'] -ne 0) {
    Write-Host "NOTE: /kp (kernel-mode policy) failed - expected for a self-signed cert that does not"
    Write-Host "      chain to the Microsoft Code Integrity roots. Whether the loader accepts it depends on"
    Write-Host "      Secure Boot state; that is what the install test (install.ps1) measures."
}
