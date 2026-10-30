@echo off
REM Versioned entry point for MODEL VERIFICATION. Logic in verify-model.ps1.
REM Examples:
REM   verify-model.cmd -DatabaseFile <db.bin>
REM   verify-model.cmd -DatabaseFile <db.bin> -Regenerate
REM   verify-model.cmd -BaseUrl http://127.0.0.1:8080/v1 -Token <bearer>
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0verify-model.ps1" %*
exit /b %errorlevel%