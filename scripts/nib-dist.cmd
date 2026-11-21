@echo off
rem Runs nib-dist.ps1 without needing a PowerShell execution-policy change.
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0nib-dist.ps1" %*
exit /b %ERRORLEVEL%
