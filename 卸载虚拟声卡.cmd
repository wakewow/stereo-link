@echo off
setlocal enabledelayedexpansion
chcp 65001 >nul
title StereoLink 立体声链 - 卸载虚拟声卡
cd /d "%~dp0"

echo ============================================================
echo  StereoLink 立体声链 - 虚拟声卡卸载
echo  移除自研驱动 StereoLinkVad 及其输出端点，需要重启一次
echo ============================================================
echo.

net session >nul 2>&1
if errorlevel 1 goto no_admin

tasklist /fi "imagename eq stereo-link.exe" 2>nul | findstr /i /c:"stereo-link.exe" >nul
if not errorlevel 1 goto panel_running

echo 本脚本会做这些事：
echo   - 移除虚拟声卡设备与「扬声器 (立体声链虚拟声卡)」输出端点
echo   - 从驱动仓库删除 StereoLinkVad 驱动包，删除内核服务和自签证书
echo   - 可选同时关闭测试签名模式 testsigning
echo   - 需要重启电脑后完全生效，本脚本不会自动重启
echo.
choice /c YN /m "确定继续卸载"
if errorlevel 2 goto cancelled

rem ------------------------------------------------------------ 移除设备实例 ----
set "ANY=0"
powershell -NoProfile -ExecutionPolicy Bypass -Command "$svc='StereoLinkVad'; foreach($d in @(Get-PnpDevice -Class MEDIA -ErrorAction SilentlyContinue)){ $s=''; try { $s=[string](Get-PnpDeviceProperty -InstanceId $d.InstanceId -KeyName 'DEVPKEY_Device_Service' -ErrorAction Stop).Data } catch { $s='' }; if($s -eq $svc){ Write-Output $d.InstanceId } }" >"%TEMP%\stereolink-vad-devices.txt"
for /f "usebackq tokens=*" %%D in ("%TEMP%\stereolink-vad-devices.txt") do (
  echo 移除设备：%%D
  pnputil /remove-device "%%D" >nul
  if errorlevel 1 echo   [警告] 移除失败，可能设备正被占用；重启后再运行一次本脚本。
  set "ANY=1"
)
if "!ANY!"=="0" echo 未发现 StereoLinkVad 设备，跳过设备移除。

rem ------------------------------------------------------------ 删除驱动包 ----
pnputil /enum-drivers >"%TEMP%\stereolink-drivers.txt" 2>&1
powershell -NoProfile -ExecutionPolicy Bypass -Command "$t = [IO.File]::ReadAllText($env:TEMP + '\stereolink-drivers.txt'); $b = $t -split '(?:\r?\n){2,}'; foreach($x in $b){ if($x -match '(?i)stereolinkvad\.inf'){ $m = [regex]::Match($x, '(?im)\boem\d+\.inf\b'); if($m.Success){ Write-Output $m.Value } } }" >"%TEMP%\stereolink-vad-oem.txt"
set "FOUND=0"
for /f "usebackq tokens=*" %%O in ("%TEMP%\stereolink-vad-oem.txt") do (
  echo 删除驱动包：%%O
  pnputil /delete-driver "%%O" /uninstall /force
  if errorlevel 1 echo   [警告] 删除失败，重启后再运行一次本脚本。
  set "FOUND=1"
)
if "!FOUND!"=="0" echo 未在驱动仓库里找到 stereolinkvad.inf，跳过。

rem ------------------------------------------------------------ 删除内核服务 ----
reg query "HKLM\SYSTEM\CurrentControlSet\Services\StereoLinkVad" >nul 2>&1
if errorlevel 1 goto service_done
echo 删除内核服务 StereoLinkVad ...
sc delete StereoLinkVad >nul
if errorlevel 1 echo   [警告] 删除服务失败，重启后系统会自动清理。
:service_done

rem ------------------------------------------------------------ 删除自签证书 ----
powershell -NoProfile -ExecutionPolicy Bypass -Command "$n=0; foreach($store in @('Root','TrustedPublisher')){ foreach($c in @(Get-ChildItem (('Cert:\LocalMachine\') + $store) -ErrorAction SilentlyContinue)){ if($c.Subject -like '*StereoLink Virtual Audio Driver*'){ Remove-Item (('Cert:\LocalMachine\') + $store + '\' + $c.Thumbprint) -Force; Write-Output ($store + ' cert removed ' + $c.Thumbprint); $n = $n + 1 } } }; if($n -eq 0){ Write-Output 'no cert found' }"
if errorlevel 1 echo   [警告] 证书删除失败，可在 certmgr.msc 里手动删除 StereoLink Virtual Audio Driver。

rem ------------------------------------------------------------ testsigning ----
echo.
choice /c YN /m "是否同时关闭测试签名模式 testsigning"
if errorlevel 2 goto keep_ts
bcdedit /set testsigning off >nul
if errorlevel 1 echo   [警告] 关闭失败，请手动执行 bcdedit /set testsigning off。
if not errorlevel 1 echo 已关闭 testsigning：重启后「测试模式」水印会消失，驱动也就不再加载。
goto reboot_hint

:keep_ts
echo 保留 testsigning：以后重新安装驱动可以直接加载，不需要再改启动项。
echo 想关闭时随时可以手动执行 bcdedit /set testsigning off，然后重启。

:reboot_hint
echo.
echo ============================================================
echo  卸载步骤已完成，请重启电脑。
echo  重启后确认：
echo   1. 设置 - 系统 - 声音 - 输出 里不再有「扬声器 (立体声链虚拟声卡)」
echo   2. pnputil /enum-devices /instanceid "ROOT\MEDIA\0001" 返回设备不存在
echo   3. 若之前默认输出指向虚拟声卡，重启后系统会自动切到其它输出设备
echo ============================================================
goto done

:cancelled
echo.
echo 已取消，没有做任何修改。

:done
echo.
echo 本窗口不会自动重启电脑。
pause
exit /b 0

:panel_running
echo [错误] 检测到 StereoLink 面板（stereo-link.exe）正在运行。
echo        请先退出面板（停止路由、恢复默认输出）再运行卸载脚本。
pause
exit /b 1

:no_admin
echo [错误] 需要管理员权限。请右键本文件，选择「以管理员身份运行」。
pause
exit /b 1
