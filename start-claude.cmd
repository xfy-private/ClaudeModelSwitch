@echo off
setlocal EnableExtensions EnableDelayedExpansion
rem ============================================================
rem  ClaudeRelay launcher (relay-isolated Claude Code)
rem  Loads claude-settings.json -- a copy of ~/.claude/settings.json
rem  whose ANTHROPIC_* env points at the relay -- so your normal
rem  Claude Code setup (settings.json) stays untouched.
rem  usage: start-claude.cmd [claude args...]   e.g. start-claude.cmd --print hi
rem ============================================================
set "DIR=%~dp0"
set "PORT=8787"
set "RELAY=%DIR%claude-relay.exe"
set "CFG=%DIR%config.json"
set "SETTINGS=%DIR%claude-settings.json"
set "BASE_URL=http://127.0.0.1:%PORT%"

echo [ClaudeRelay] checking relay at %BASE_URL% ...

rem --- 1) ensure relay is up ---
call :is_up
if errorlevel 1 (
    echo [ClaudeRelay] relay not running - starting minimized. tray icon opens config / quit.
    start "ClaudeRelay" /min "%RELAY%" --config "%CFG%" --autostart
    set /a tries=0
    :waitloop
    set /a tries+=1
    if !tries! gtr 60 (
        echo [ClaudeRelay] startup timeout. run "%RELAY%" manually to see errors.
        exit /b 1
    )
    timeout /t 1 /nobreak >nul
    call :is_up
    if errorlevel 1 goto waitloop
    echo [ClaudeRelay] relay ready.
)

rem --- 2) show default model for info ---
set "DEFAULT_MODEL="
for /f "usebackq delims=" %%m in (`""%RELAY%" --print-default-model --config "%CFG%""`) do set "DEFAULT_MODEL=%%m"

rem --- 2b) task slots -> sentinel (sub-agents/compact don't follow /model; bare names 400) ---
if defined DEFAULT_MODEL (
    set "ANTHROPIC_DEFAULT_OPUS_MODEL=%DEFAULT_MODEL%"
    set "ANTHROPIC_DEFAULT_SONNET_MODEL=%DEFAULT_MODEL%"
    set "ANTHROPIC_DEFAULT_HAIKU_MODEL=%DEFAULT_MODEL%"
)

echo [ClaudeRelay] base url : %BASE_URL%
if defined DEFAULT_MODEL echo [ClaudeRelay] model    : %DEFAULT_MODEL%
echo [ClaudeRelay] settings : %SETTINGS%
echo [ClaudeRelay] config   : %BASE_URL%  (/model prefix-model to switch provider)
echo [ClaudeRelay] launching Claude Code...
claude --settings "%SETTINGS%" %*
exit /b %errorlevel%

:is_up
rem returns 0 = ready, 1 = down
powershell -NoProfile -Command "try{Invoke-WebRequest -UseBasicParsing -Uri '%BASE_URL%/healthz' -TimeoutSec 1|Out-Null;exit 0}catch{exit 1}"
exit /b %errorlevel%
