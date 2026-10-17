@echo off
chcp 65001 >nul
setlocal EnableExtensions
cd /d "%~dp0"
title 卸载虚拟声卡 — StereoLink 立体声链

set "PS1=%~dp0scripts\rename-endpoint.ps1"
set "DEVICE=ROOT\STEAMSTREAMINGSPEAKERS\0000"
set "BACKUP_DIR=%~dp0scripts\backups"

echo ============================================================
echo  卸载虚拟声卡：把设备名改回原样
echo ============================================================
echo.
echo  优先用「原始状态」的备份回滚：scripts\backups\ 下文件名含
echo  Steam_Streaming_Speakers 的 backup-Enum-*.reg（不受重复安装影响）。
echo  找不到原始备份时，回退为直接删除 FriendlyName 值并重启一次设备。
echo.
echo  卸载时设备会 disable/enable 一次（约 2-5 秒，期间音频可能中断）。
echo  需要管理员权限；请右键本文件，选择「以管理员身份运行」。
echo ------------------------------------------------------------
net session >nul 2>&1
if errorlevel 1 goto :no_admin
if not exist "%PS1%" goto :no_script
reg query "HKLM\SYSTEM\CurrentControlSet\Enum\%DEVICE%" >nul 2>&1
if errorlevel 1 goto :no_device

set "BACKUP="
for /f "delims=" %%F in ('dir /b /o:d "%BACKUP_DIR%\backup-Enum-*Steam_Streaming_Speakers*.reg" 2^>nul') do if not defined BACKUP set "BACKUP=%BACKUP_DIR%\%%F"

if defined BACKUP goto :rollback
goto :fallback

:rollback
echo.
echo [1/2] 使用原始备份回滚：
echo       %BACKUP%
rem 注意：-Rollback 与 -DeviceInstanceId 属于不同参数集，不能同时传
powershell -NoProfile -ExecutionPolicy Bypass -File "%PS1%" -Rollback "%BACKUP%"
if errorlevel 1 goto :rollback_failed
goto :done

:fallback
echo.
echo [1/2] scripts\backups\ 里没有找到 Steam_Streaming_Speakers 的原始备份。
echo       回退为直接删除 FriendlyName 值，并重启 AudioEndpointBuilder。
reg delete "HKLM\SYSTEM\CurrentControlSet\Enum\%DEVICE%" /v FriendlyName /f >nul 2>&1
powershell -NoProfile -ExecutionPolicy Bypass -Command "Restart-Service AudioEndpointBuilder -Force -ErrorAction SilentlyContinue; Start-Sleep -Seconds 3" >nul 2>&1
echo       已删除设备显示名，Windows 会恢复为「Steam Streaming Speakers」。
echo       想再改成「立体声链设备」等任意名字：打开 StereoLink 面板，
echo       在「虚拟声卡」区用「应用重命名」，或重跑「安装虚拟声卡.cmd」。
goto :done

:done
echo.
echo [2/2] 卸载完成。
echo       请到 设置 -^> 系统 -^> 声音 -^> 输出 确认设备名已恢复。
echo       备份文件仍保留在 scripts\backups\，确认无误后可自行删除。
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
echo        请确认本文件位于仓库根目录，且 scripts\ 下的脚本存在。
echo.
pause
exit /b 1

:no_device
echo.
echo [错误] 找不到 Steam Streaming Speakers 设备（%DEVICE%）。
echo        可能已经卸载过了，或本机从未安装过 Steam。
echo.
pause
exit /b 1

:rollback_failed
echo.
echo [失败] 回滚没有成功，请查看上面的输出。
echo        可以手动挑一个更早的备份重试：
echo        powershell -ExecutionPolicy Bypass -File scripts\rename-endpoint.ps1 -Rollback ^<备份.reg^>
echo.
pause
exit /b 1
