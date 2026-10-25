@echo off
title Install Skreenshare proektor on the projector (adb over Wi-Fi)
if not exist "%~dp0install-on-projector.ps1" goto nops
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0install-on-projector.ps1"
echo.
pause
exit /b
:nops
echo File install-on-projector.ps1 not found next to this .bat.
echo Fajl install-on-projector.ps1 ne najden: rasspakujte ves zip-arhiv celikom (Extract All), a ne tolko .bat.
pause
exit /b 1
