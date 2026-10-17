@echo off
chcp 65001 >nul
cd /d "%~dp0"
title 卸载虚拟串流声卡
echo ============================================================
echo  卸载虚拟串流声卡：把设备名改回原样
echo ============================================================
echo.
echo  优先用「原始状态」的备份（文件名含 Steam_Streaming_Speakers）回滚，
echo  这样不受重复安装影响。找不到就退化为直接删除 FriendlyName 值。
echo.
echo  需要管理员权限。如果没有，请右键“以管理员身份运行”。
echo ------------------------------------------------------------
pause

set "LATEST="
for /f "delims=" %%F in ('dir /b /o-d "poc\zero-driver\backups\backup-Enum-*Steam_Streaming_Speakers*.reg" 2^>nul') do (
  if not defined LATEST set "LATEST=poc\zero-driver\backups\%%F"
)

if defined LATEST (
  echo [i] 使用原始备份: %LATEST%
  rem 注意: -Rollback 与 -DeviceInstanceId 属于不同参数集，不能同时传
  powershell -NoProfile -ExecutionPolicy Bypass -File "poc\zero-driver\rename-endpoint.ps1" -Rollback "%LATEST%"
) else (
  echo [!] 没找到备份，直接删除 FriendlyName 值再刷一次端点缓存。
  reg delete "HKLM\SYSTEM\CurrentControlSet\Enum\ROOT\STEAMSTREAMINGSPEAKERS\0000" /v FriendlyName /f >nul 2>&1
  pnputil /disable-device "ROOT\STEAMSTREAMINGSPEAKERS\0000" >nul 2>&1
  timeout /t 2 /nobreak >nul
  pnputil /enable-device "ROOT\STEAMSTREAMINGSPEAKERS\0000" >nul 2>&1
)
set RC=%ERRORLEVEL%
echo.
if "%RC%"=="0" (
  echo [OK] 已回滚。输出列表里应恢复为「扬声器 (Steam Streaming Speakers)」。
) else (
  echo [X] 回滚失败，退出码 %RC%。改名前状态的完整备份见 poc\zero-driver\backups\。
)
echo.
pause
