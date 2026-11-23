@echo off
rem Runs nib.ps1 without needing a PowerShell execution-policy change.
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0nib.ps1" %*
exit /b %ERRORLEVEL%
