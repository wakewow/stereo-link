@echo off
chcp 65001 >nul
cd /d "%~dp0"
title 安装虚拟串流声卡
echo ============================================================
echo  安装虚拟串流声卡：让 Windows 输出列表出现「扬声器 (小米妙播设备)」
echo ============================================================
echo.
echo  原理（已在 research/vad/rename-probe.md 三态实测）：
echo    借用机器上已有的一张已签名虚拟声卡（默认 Steam Streaming Speakers），
echo    改它的设备名 -> AudioEndpointBuilder 把端点显示名重建为
echo        「扬声器 (小米妙播设备)」
echo    不需要写驱动、不需要开 test signing。
echo.
echo  副作用：安装时会 disable/enable 一次该设备（约 3 秒，期间可能打断在用音频）；
echo          装完可在 设置 - 系统 - 声音 里看到并用它。
echo          卸载用同目录的「卸载虚拟声卡.cmd」，可完整回滚。
echo.
echo  需要管理员权限。如果没有，请右键“以管理员身份运行”。
echo ------------------------------------------------------------
pause

powershell -NoProfile -ExecutionPolicy Bypass -File "poc\zero-driver\rename-endpoint.ps1" -DeviceInstanceId "ROOT\STEAMSTREAMINGSPEAKERS\0000" -NewName "小米妙播设备"
set RC=%ERRORLEVEL%
echo.
if "%RC%"=="0" (
  echo [OK] 安装成功。Windows 输出列表应已出现「扬声器 (小米妙播设备)」。
  echo      下一步：双击「启动-妙播投放.cmd」。
) else (
  echo [X] 安装失败，退出码 %RC%。请看上面的日志。
)
echo.
pause
