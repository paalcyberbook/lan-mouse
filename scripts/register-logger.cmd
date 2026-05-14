@echo off
setlocal enabledelayedexpansion

REM ---------------------------------------------------------------------------
REM  Lan Mouse+ -- cb-logger registration helper (Windows).
REM
REM  For debugging-only cross-OS log shipping. Prompts for the shared API
REM  key, runs lan-mouse.exe once so it registers + caches a bearer token,
REM  then optionally persists the key to the user's environment and joins
REM  a group via invite code.
REM
REM  Double-click to run. Safe to re-run -- it just re-registers (or, more
REM  usually, reads the existing cached token and prints "shipping to ...").
REM ---------------------------------------------------------------------------

title Lan Mouse+ -- cb-logger registration

echo.
echo ====================================================================
echo  Lan Mouse+ -- register this host with cb-logger
echo ====================================================================
echo.
echo  This will register this Windows host as a logging client so its
echo  output can be merged with logs from your Linux and macOS hosts
echo  while you are chasing a multi-OS bug.
echo.
echo  This is a debugging tool, not production telemetry. Turn it off by
echo  clearing the LOGGER_APIKEY environment variable when you are done.
echo.

REM --- locate lan-mouse.exe -------------------------------------------------
set "LANMOUSE="
where /q lan-mouse.exe
if not errorlevel 1 (
    for /f "delims=" %%P in ('where lan-mouse.exe') do (
        if not defined LANMOUSE set "LANMOUSE=%%P"
    )
)
if not defined LANMOUSE if exist "%~dp0bin\lan-mouse.exe" set "LANMOUSE=%~dp0bin\lan-mouse.exe"
if not defined LANMOUSE if exist "%ProgramFiles%\Lan Mouse+\bin\lan-mouse.exe" set "LANMOUSE=%ProgramFiles%\Lan Mouse+\bin\lan-mouse.exe"
if not defined LANMOUSE if exist "%ProgramFiles%\Lan Mouse+\lan-mouse.exe" set "LANMOUSE=%ProgramFiles%\Lan Mouse+\lan-mouse.exe"

if not defined LANMOUSE (
    echo ERROR: could not find lan-mouse.exe.
    echo Tried: PATH, "%~dp0bin\", and "%ProgramFiles%\Lan Mouse+\".
    echo.
    echo Install Lan Mouse+ first ^(installer or portable zip^), then re-run.
    echo.
    pause
    exit /b 1
)
echo Using: %LANMOUSE%
echo.

REM --- prompt for API key (masked) -----------------------------------------
echo Paste your cb-logger API key. Input is hidden.
for /f "delims=" %%K in ('powershell -NoProfile -Command "$s = Read-Host -Prompt 'API key' -AsSecureString; [Runtime.InteropServices.Marshal]::PtrToStringAuto([Runtime.InteropServices.Marshal]::SecureStringToBSTR($s))"') do set "LOGGER_APIKEY=%%K"

if not defined LOGGER_APIKEY (
    echo No key entered. Aborting.
    pause
    exit /b 1
)
echo.

REM --- trigger registration -------------------------------------------------
echo Registering...
set "REG_LOG=%TEMP%\lan-mouse-register-%RANDOM%.log"
"%LANMOUSE%" --help >nul 2>"%REG_LOG%"
type "%REG_LOG%"
del "%REG_LOG%" >nul 2>&1
echo.

REM --- verify token cache ---------------------------------------------------
set "TOKEN=%APPDATA%\lan-mouse\remote-log-token.json"
if not exist "%TOKEN%" (
    echo ERROR: token cache not found at "%TOKEN%".
    echo Registration likely failed -- scroll up for the cause
    echo ^(wrong key? no network? proxy?^).
    echo.
    pause
    exit /b 1
)

echo Registered. Token cached at:
echo   %TOKEN%
echo.
powershell -NoProfile -Command "Get-Content -Raw '%TOKEN%' | ConvertFrom-Json | Select-Object name, client_id | Format-List"
echo.

REM --- offer to persist env var --------------------------------------------
set "PERSIST="
set /p PERSIST=Save LOGGER_APIKEY to your user environment so future shells inherit it? [y/N]:
if /i "!PERSIST!"=="y" (
    setx LOGGER_APIKEY "!LOGGER_APIKEY!" >nul
    if errorlevel 1 (
        echo Failed to persist.
    ) else (
        echo Saved. New PowerShell / cmd windows will see it; this one will not until reopened.
    )
)
echo.

REM --- offer to join a group -----------------------------------------------
set "JOIN="
set /p JOIN=Group invite code ^(blank to skip^):
if not "!JOIN!"=="" (
    echo.
    "%LANMOUSE%" cli logger join "!JOIN!"
    echo.
)

echo Done. Quick checks you can run now:
echo   "%LANMOUSE%" cli logger status
echo   "%LANMOUSE%" cli logger tail --limit 20
echo.
pause
endlocal
