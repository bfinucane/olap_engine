@echo off
REM Versioned entry point for the DEVELOPMENT engine (isolated build + scratch DB; D13/D14).
REM The workspace-root start-engine-dev.cmd forwards here.
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0start-dev.ps1" %*
if errorlevel 1 (
  echo.
  echo   The development engine did not start or stopped with an error. The message is above.
  pause
)
