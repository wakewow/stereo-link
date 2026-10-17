@echo off
chcp 65001 >nul
cd /d "%~dp0"
title 卸载虚拟串流声卡
echo ============================================================
echo  卸载虚拟串流声卡：把设备名改回原样
echo ============================================================
echo.
echo  会从 poc\zero-driver\backups\ 里找最近一次安装的备份 .reg，回滚它。
echo  如果找不到备份，则退化为「直接删掉 FriendlyName 值」（同样可恢复）。
echo.
echo  需要管理员权限。如果没有，请右键“以管理员身份运行”。
echo ------------------------------------------------------------
pause

set "LATEST="
for /f "delims=" %%F in ('dir /b /o-d "poc\zero-driver\backups\*.reg" 2^>nul') do (
  if not defined LATEST set "LATEST=poc\zero-driver\backups\%%F"
)

if defined LATEST (
  echo [i] 使用备份: %LATEST%
  powershell -NoProfile -ExecutionPolicy Bypass -File "poc\zero-driver\rename-endpoint.ps1" -DeviceInstanceId "ROOT\STEAMSTREAMINGSPEAKERS\0000" -Rollback "%LATEST%"
) else (
  echo [!] 没找到备份，直接删除 FriendlyName 值。
  powershell -NoProfile -ExecutionPolicy Bypass -File "poc\zero-driver\rename-endpoint.ps1" -DeviceInstanceId "ROOT\STEAMSTREAMINGSPEAKERS\0000" -Rollback ""
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
