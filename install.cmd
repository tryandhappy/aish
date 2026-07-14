@echo off
setlocal
REM aish installer bootstrap for cmd.exe / double-click.
REM Runs the sibling install.ps1 (ExecutionPolicy bypassed) if present; otherwise
REM fetches and runs the latest install.ps1 from the repository.
if exist "%~dp0install.ps1" (
    powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0install.ps1" %*
) else (
    powershell -NoProfile -ExecutionPolicy Bypass -Command "irm https://raw.githubusercontent.com/tryandhappy/aish/main/install.ps1 | iex"
)
echo.
REM Pause only when double-clicked from Explorer (so the window stays open to read
REM the result). When run from a console (e.g. the documented curl one-liner),
REM %cmdcmdline% is the parent cmd.exe line and does not contain this script name,
REM so it does not block.
echo %cmdcmdline% | find /i "%~nx0" >nul && pause
