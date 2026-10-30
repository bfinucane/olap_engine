@echo off
REM Versioned entry point for RELEASE PROMOTION (D14). Run this as the USER, with the
REM shared server STOPPED. The logic is in promote-release.ps1; the machine's execution
REM policy refuses unsigned .ps1 files, so the wrapper relaunches PowerShell with Bypass.
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0promote-release.ps1" %*
if errorlevel 1 (
  echo.
  echo   Promotion did not complete. The message is above.
  pause
)