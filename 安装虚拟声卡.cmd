@echo off
setlocal enabledelayedexpansion
chcp 65001 >nul
title StereoLink 立体声链 - 安装虚拟声卡
cd /d "%~dp0"

echo ============================================================
echo  StereoLink 立体声链 - 虚拟声卡安装
echo  自研驱动 StereoLinkVad（render-only），安装后需要重启一次
echo ============================================================
echo.

net session >nul 2>&1
if errorlevel 1 goto no_admin

rem ---------------------------------------------------------------- 驱动包定位 --
set "PKG="
if not "%~1"=="" if exist "%~1\StereoLinkVad.inf" set "PKG=%~1"
if not defined PKG if exist "%~dp0drivers\pkg-stereolink\StereoLinkVad.inf" set "PKG=%~dp0drivers\pkg-stereolink"
if not defined PKG if exist "%~dp0.scratch\vad\pkg-stereolink\StereoLinkVad.inf" set "PKG=%~dp0.scratch\vad\pkg-stereolink"
if not defined PKG goto no_package

set "INF=%PKG%\StereoLinkVad.inf"
if not exist "%PKG%\StereoLinkVad.sys" goto no_package

rem ---------------------------------------------------------------- 证书定位 ----
set "CERT="
if exist "%PKG%\..\certs\stereolink-dev.cer" set "CERT=%PKG%\..\certs\stereolink-dev.cer"
if not defined CERT if exist "%~dp0drivers\certs\stereolink-dev.cer" set "CERT=%~dp0drivers\certs\stereolink-dev.cer"
if not defined CERT if exist "%~dp0.scratch\vad\certs\stereolink-dev.cer" set "CERT=%~dp0.scratch\vad\certs\stereolink-dev.cer"
if not defined CERT goto no_cert

echo 驱动包：%PKG%
echo 证书  ：%CERT%
echo.

rem -------------------------------------------------------------- 系统前提检查 --
powershell -NoProfile -Command "try { if (Confirm-SecureBootUEFI) { exit 1 } else { exit 0 } } catch { exit 0 }"
if errorlevel 1 goto secure_boot_on

set "TS=0"
for /f "tokens=1,*" %%A in ('bcdedit /enum "{current}" 2^>nul ^| findstr /i "testsigning"') do (
  if /i "%%B"=="Yes" set "TS=1"
)
set "NEED_REBOOT=0"

if "!TS!"=="1" goto ts_on

echo [1/3] 开启测试签名模式 testsigning ...
bcdedit /set testsigning on >nul
if errorlevel 1 goto no_testsigning
set "NEED_REBOOT=1"
echo       已开启。重启后桌面右下角会出现「测试模式」水印，属正常现象。
goto ts_done

:ts_on
echo [1/3] testsigning 已开启，跳过。

:ts_done
echo.

rem -------------------------------------------------------------- 已安装检查 ----
call :read_vad_state
if not "!DEV!"=="" goto dev_exists

rem ---------------------------------------------------------------- 安装驱动 ----
echo [2/3] 未发现 StereoLinkVad 设备，开始安装驱动 ...
echo       导入自签证书到 Root 和 TrustedPublisher ...
certutil -addstore -f Root "%CERT%" >nul
if errorlevel 1 goto cert_failed
certutil -addstore -f TrustedPublisher "%CERT%" >nul
if errorlevel 1 goto cert_failed

echo       pnputil /add-driver ...
pnputil /add-driver "%INF%" /install >nul
if errorlevel 1 echo       [警告] pnputil /add-driver 返回非零，继续尝试创建设备。

call :find_devcon
if not defined DEVCON goto no_devcon

echo       devcon install ...
"%DEVCON%" install "%INF%" *StereoLinkVad >"%TEMP%\stereolink-devcon-install.log" 2>&1
if errorlevel 1 echo       [警告] devcon 返回非零，日志：%TEMP%\stereolink-devcon-install.log

echo       等待系统创建设备 ...
timeout /t 5 /nobreak >nul
call :read_vad_state
if "!DEV!"=="" goto install_failed

echo [3/3] 设备已创建：!DEV!
if /i "!DEVSTATUS!"=="OK" goto verify_endpoint
set "NEED_REBOOT=1"
goto reboot_hint

rem ------------------------------------------------------------ 已存在设备分支 --
:dev_exists
echo [2/3] 已存在 StereoLinkVad 设备：!DEV!
if /i "!DEVSTATUS!"=="OK" goto verify_endpoint

echo       设备当前状态为「!DEVSTATUS!」，需要重启后驱动才会加载。
set "NEED_REBOOT=1"
goto reboot_hint

rem ---------------------------------------------------------------- 端点验证 ----
:verify_endpoint
echo [3/3] 驱动已加载，检查输出端点 ...
set "EP_OK=0"
set "EPS=%~dp0target\debug\endpoints.exe"
if not exist "%EPS%" set "EPS=%~dp0target\release\endpoints.exe"
if not exist "%EPS%" goto endpoint_check_done
"%EPS%" >"%TEMP%\stereolink-endpoints.txt" 2>&1
rem 中文端点名通过环境变量传给 PowerShell，保证内联 -Command 文本是纯 ASCII
set "EPNAME=立体声链虚拟声卡"
powershell -NoProfile -Command "$t=[IO.File]::ReadAllText($env:TEMP+'\stereolink-endpoints.txt',[Text.Encoding]::UTF8); if($t.Contains($env:EPNAME)){ exit 0 } else { exit 1 }"
if not errorlevel 1 set "EP_OK=1"
:endpoint_check_done
if "!EP_OK!"=="1" goto endpoint_ok

echo       暂未在端点列表里看到「扬声器 (立体声链虚拟声卡)」，
echo       若刚重启完请稍等几秒，然后在「设置 - 系统 - 声音」里点一次刷新。
goto done_ok

:endpoint_ok
echo       已找到端点：扬声器 (立体声链虚拟声卡)

:done_ok
echo.
echo 安装完成。打开 stereo-link.exe，在面板里给左右声道各选一台设备即可。
goto done

rem ---------------------------------------------------------------- 重启提示 ----
:reboot_hint
echo.
echo ============================================================
echo  安装步骤已完成，但需要重启电脑后驱动才会加载（本脚本不会重启）。
echo  重启后请依次确认：
echo   1. pnputil /enum-devices /instanceid "ROOT\MEDIA\0001" 显示 Status: Started
echo   2. 设置 - 系统 - 声音 - 输出 里出现「扬声器 (立体声链虚拟声卡)」
echo   3. target\debug\endpoints.exe 的输出里能找到该端点
echo   4. 打开 stereo-link.exe，给左右声道各选一台设备并开始路由
echo ============================================================
goto done

rem -------------------------------------------------------------------- 工具 ----
:read_vad_state
set "DEV="
set "DEVSTATUS="
set "DEVLINE="
powershell -NoProfile -ExecutionPolicy Bypass -Command "$svc='StereoLinkVad'; $f=''; foreach($d in @(Get-PnpDevice -Class MEDIA -ErrorAction SilentlyContinue)){ $s=''; try { $s=[string](Get-PnpDeviceProperty -InstanceId $d.InstanceId -KeyName 'DEVPKEY_Device_Service' -ErrorAction Stop).Data } catch { $s='' }; if($s -eq $svc){ $f = $d.InstanceId + ' ' + $d.Status } }; if($f){ Write-Output $f } else { Write-Output 'NONE' }" >"%TEMP%\stereolink-vad-state.txt"
set /p "DEVLINE=" <"%TEMP%\stereolink-vad-state.txt"
if not defined DEVLINE goto :eof
if /i "!DEVLINE!"=="NONE" goto :eof
for /f "tokens=1,2" %%A in ("!DEVLINE!") do (
  set "DEV=%%A"
  set "DEVSTATUS=%%B"
)
goto :eof

:find_devcon
set "DEVCON="
set "WDK=%ProgramFiles(x86)%\Windows Kits\10\Tools"
if exist "%WDK%\10.0.26100.0\x64\devcon.exe" set "DEVCON=%WDK%\10.0.26100.0\x64\devcon.exe"
if defined DEVCON goto :eof
if exist "%~dp0tools\devcon.exe" set "DEVCON=%~dp0tools\devcon.exe"
if defined DEVCON goto :eof
for /f "delims=" %%F in ('dir /b /s "%WDK%\*\x64\devcon.exe" 2^>nul') do if not defined DEVCON set "DEVCON=%%F"
goto :eof

rem ------------------------------------------------------------------ 错误 ----
:no_admin
echo [错误] 需要管理员权限。请右键本文件，选择「以管理员身份运行」。
pause
exit /b 1

:no_package
echo [错误] 找不到驱动包文件 StereoLinkVad.inf / StereoLinkVad.sys。
echo        默认查找位置：
echo          %~dp0drivers\pkg-stereolink\
echo          %~dp0.scratch\vad\pkg-stereolink\
echo        也可以把驱动包目录作为第一个参数传进来。
pause
exit /b 1

:no_cert
echo [错误] 找不到自签证书 stereolink-dev.cer。
echo        默认查找位置：
echo          %~dp0drivers\certs\
echo          %~dp0.scratch\vad\certs\
pause
exit /b 1

:secure_boot_on
echo [错误] 本机 Secure Boot 已开启，测试签名的驱动无法加载。
echo        请先在 BIOS/UEFI 里关闭 Secure Boot，再重新运行本脚本。
pause
exit /b 1

:no_testsigning
echo [错误] 执行 bcdedit /set testsigning on 失败。
echo        请确认当前是管理员命令行，或手动执行：
echo          bcdedit /set testsigning on
pause
exit /b 1

:cert_failed
echo [错误] 证书导入失败。请确认 certutil 可用、当前是管理员权限。
pause
exit /b 1

:no_devcon
echo [错误] 找不到 devcon.exe（用于创建 root 设备实例）。
echo        安装 Windows Driver Kit 10.0.26100.0，或把 devcon.exe 放到：
echo          %~dp0tools\devcon.exe
pause
exit /b 1

:install_failed
echo [错误] 驱动安装后仍未发现 StereoLinkVad 设备。
echo        请检查 devcon 日志：%TEMP%\stereolink-devcon-install.log
echo        常见原因：驱动包不完整、WDK devcon 版本不匹配、设备实例被占用。
pause
exit /b 1

:done
echo.
echo 本窗口不会自动重启电脑。
pause
exit /b 0
