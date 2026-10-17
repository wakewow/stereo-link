@echo off
chcp 65001 >nul
set PYTHONUTF8=1
cd /d "%~dp0"
title 妙播投放 - 虚拟串流声卡
echo ============================================================
echo  妙播投放：把「扬声器 (小米妙播设备)」里的声音送到两只小爱音箱
echo ============================================================
echo.
echo  1) 本程序会抓取虚拟声卡端点「扬声器 (小米妙播设备)」的声音
echo  2) 然后在 Windows 声音设置里，把**播放设备**切到「扬声器 (小米妙播设备)」
echo     或者让某个播放器单独把输出选到它
echo  3) 你放什么，两只音箱就响什么（左箱=左声道，右箱=右声道）
echo.
echo  关掉这个窗口或按 Ctrl-C 即停止。
echo ------------------------------------------------------------
echo.

if not exist "target\debug\miplay-bridge.exe" (
  echo [!] 还没编译，先跑 cargo build --workspace
  pause
  exit /b 1
)

target\debug\miplay-bridge.exe hub --devices 501718792,934947888 --split --gain 0.7
echo.
echo [已停止]
pause
