@echo off
rem Runs build.ps1 without needing a PowerShell execution-policy change.
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0build.ps1" %*
exit /b %ERRORLEVEL%
