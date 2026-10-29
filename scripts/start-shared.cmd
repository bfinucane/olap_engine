@echo off
REM Versioned entry point for the SHARED engine (frozen release; D14).
REM The workspace-root start-engine.cmd forwards here.
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0start-shared.ps1" %*
if errorlevel 1 (
  echo.
  echo   The shared engine did not start or stopped with an error. The message is above.
  pause
)
