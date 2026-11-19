@echo off
if not exist "%~dp0Launcher\Millida16.exe" (
  echo The hybrid launcher has not been built yet. Run Build.ps1 first.
  pause
  exit /b 1
)
start "" "%~dp0Launcher\Millida16.exe"
