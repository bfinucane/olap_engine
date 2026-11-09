@echo off
rem Rebuild the demo model from CSV+SQL and verify its content fingerprint.
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0seed-demo.ps1" %*
exit /b %ERRORLEVEL%