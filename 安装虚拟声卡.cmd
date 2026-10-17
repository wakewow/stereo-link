@echo off
chcp 65001 >nul
setlocal EnableExtensions
cd /d "%~dp0"
title 安装虚拟声卡 — StereoLink 立体声链

set "PS1=%~dp0scripts\rename-endpoint.ps1"
set "DEVICE=ROOT\STEAMSTREAMINGSPEAKERS\0000"
set "NAME=立体声链设备"

echo ============================================================
echo  安装虚拟声卡：让 Windows 输出列表出现「扬声器 (立体声链设备)」
echo ============================================================
echo.
echo  原理：借用机器上已有的已签名虚拟声卡（Steam Streaming Speakers），
echo        只改注册表里的设备显示名，不写驱动、不开 test signing。
echo        改名前会自动备份到 scripts\backups\，可用「卸载虚拟声卡.cmd」回滚。
echo.
echo  安装时设备会 disable/enable 一次（约 2-5 秒，期间音频可能中断）。
echo  需要管理员权限；请右键本文件，选择「以管理员身份运行」。
echo ------------------------------------------------------------
net session >nul 2>&1
if errorlevel 1 goto :no_admin
if not exist "%PS1%" goto :no_script
reg query "HKLM\SYSTEM\CurrentControlSet\Enum\%DEVICE%" >nul 2>&1
if errorlevel 1 goto :no_device

echo.
echo [1/2] 正在改名，先备份再写设备名...
powershell -NoProfile -ExecutionPolicy Bypass -File "%PS1%" -DeviceInstanceId "%DEVICE%" -NewName "%NAME%"
if errorlevel 1 goto :rename_failed

echo.
echo [2/2] 安装完成。
echo       打开 设置 -^> 系统 -^> 声音 -^> 输出，应能看到「扬声器 (%NAME%)」。
echo       如果没看到：稍等几秒、在面板里点「刷新设备」，或在设备管理器里
echo       禁用/启用一次 Steam Streaming Speakers。
echo.
echo       下一步：打开 StereoLink 面板，在「声道路由」里给左、右声道各选一台播放设备。
echo.
pause
exit /b 0

:no_admin
echo.
echo [错误] 需要管理员权限。
echo        请右键本文件，选择「以管理员身份运行」。
echo.
pause
exit /b 1

:no_script
echo.
echo [错误] 找不到改名脚本：%PS1%
echo        请确认本文件位于仓库根目录，且 scripts\rename-endpoint.ps1 存在。
echo.
pause
exit /b 1

:no_device
echo.
echo [错误] 找不到 Steam Streaming Speakers 设备（%DEVICE%）。
echo        本机需要先装过 Steam（Steam 会安装这张已签名的虚拟声卡），
echo        之后才能借用它的端点改名。
echo.
pause
exit /b 1

:rename_failed
echo.
echo [失败] 改名没有成功，请查看上面的输出。
echo        备份在 scripts\backups\，回滚方式：
echo        powershell -ExecutionPolicy Bypass -File scripts\rename-endpoint.ps1 -Rollback ^<备份.reg^>
echo.
pause
exit /b 1
