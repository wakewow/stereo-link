# 构建发布用的 release 版 stereo-link.exe，并把**构建机路径**从二进制里抹掉。
#
# 为什么需要：Rust 会把「panic 位置」的源文件路径编进 .rdata，例如
#     C:\Users\<你>\.cargo\registry\src\index.crates.io-<hash>\anyhow-1.0.104\src\lib.rs
#     E:\path\to\repo\target\release\build\glutin_egl_sys-<hash>\out\egl_bindings.rs
# 直接发布会暴露开发机的用户名与目录结构。--remap-path-prefix 把这些前缀改写成中性名字。
#
# 用法：
#   powershell -NoProfile -ExecutionPolicy Bypass -File scripts\build-release.ps1
#   powershell ... -File scripts\build-release.ps1 -CopyToDist     # 顺便更新 dist\stereo-link.exe
#
# 注意：RUSTFLAGS 变化会让 cargo 重新编译全部依赖（首次约 1~3 分钟）。

[CmdletBinding()]
param(
    [switch]$CopyToDist
)

$ErrorActionPreference = 'Stop'

$root       = Split-Path -Parent $PSScriptRoot
$cargoHome  = if ($env:CARGO_HOME)  { $env:CARGO_HOME }  else { Join-Path $env:USERPROFILE '.cargo' }
$rustupHome = if ($env:RUSTUP_HOME) { $env:RUSTUP_HOME } else { Join-Path $env:USERPROFILE '.rustup' }

$remap = @(
    "--remap-path-prefix=$root=.",
    "--remap-path-prefix=$cargoHome=cargo",
    "--remap-path-prefix=$rustupHome=rustup"
) -join ' '

Write-Host "仓库根目录 : $root"
Write-Host "CARGO_HOME : $cargoHome"
Write-Host "RUSTUP_HOME: $rustupHome"
Write-Host "RUSTFLAGS  : $remap"
Write-Host ''

$env:RUSTFLAGS = $remap
Push-Location $root
try {
    # Windows PowerShell 5.1 会把原生命令写到 stderr 的每一行（cargo 的
    # `Compiling ... / Finished ...` 进度）包成 ErrorRecord。本脚本开头设了
    # `$ErrorActionPreference = 'Stop'`，于是这些进度行会被当成终止性错误抛出，
    # 构建刚起步就中断（PowerShell 7.3+ 默认不这样，所以只在 5.1 上复现）。
    # 这里临时静音这些进度行——它们不是错误——成败只看 $LASTEXITCODE。
    $prevEap = $ErrorActionPreference
    $ErrorActionPreference = 'SilentlyContinue'
    try {
        cargo build --release -p stereo-link
        $cargoExit = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $prevEap
    }
    if ($cargoExit -ne 0) { throw "cargo build 失败（exit $cargoExit）" }
    Write-Host 'cargo 构建完成（进度输出已被 PowerShell 5.1 归入 stderr 并静音）'
} finally {
    Pop-Location
}

$exe = Join-Path $root 'target\release\stereo-link.exe'
if (-not (Test-Path $exe)) { throw "找不到产物 $exe" }

# 自检：产物里不应再出现构建机的用户名 / 仓库路径
$bytes = [System.IO.File]::ReadAllBytes($exe)
$text  = [System.Text.Encoding]::GetEncoding(28591).GetString($bytes)   # ISO-8859-1：逐字节映射，便于在二进制里找 ASCII 路径
$needles = @($root, $env:USERNAME, $cargoHome, $rustupHome) | Where-Object { $_ }
$bad = @()
foreach ($n in $needles) { if ($text.Contains($n)) { $bad += $n } }
Write-Host ''
Write-Host ("产物: $exe  (" + $bytes.Length + " B)")
if ($bad.Count -gt 0) {
    Write-Host ("[警告] 产物里仍能看到: " + ($bad -join ', ')) -ForegroundColor Yellow
} else {
    Write-Host '[OK] 产物里没有构建机路径 / 用户名' -ForegroundColor Green
}

if ($CopyToDist) {
    $dist = Join-Path $root 'dist'
    New-Item -ItemType Directory -Force -Path $dist | Out-Null
    Copy-Item $exe (Join-Path $dist 'stereo-link.exe') -Force
    Write-Host ("已复制到 " + (Join-Path $dist 'stereo-link.exe'))
}
