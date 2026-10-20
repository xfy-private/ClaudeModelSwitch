@echo off
setlocal EnableExtensions DisableDelayedExpansion
pushd "%~dp0"
if errorlevel 1 exit /b 1

set "RELAY_OFFLINE="
set "RELAY_NPM_FLAGS="
set "RELAY_CARGO_SOURCE="
set "RELAY_NO_PAUSE="

:arguments
if "%~1"=="" goto prepare
if /i "%~1"=="--offline" (
    set "RELAY_OFFLINE=--offline"
    set "RELAY_NPM_FLAGS=--offline"
) else if /i "%~1"=="--mirror" (
    set "RELAY_CARGO_SOURCE=--config "source.crates-io.replace-with='rsproxy'" --config "source.rsproxy.registry='sparse+https://rsproxy.cn/index/'""
) else if /i "%~1"=="--no-pause" (
    set "RELAY_NO_PAUSE=1"
) else (
    echo Usage: build.bat [--offline] [--mirror] [--no-pause]
    goto failed
)
shift
goto arguments

:prepare
if exist "%USERPROFILE%\.cargo\bin\cargo.exe" set "PATH=%USERPROFILE%\.cargo\bin;%PATH%"
where gcc.exe >nul 2>nul
if errorlevel 1 if exist "C:\TDM-GCC-64\bin\gcc.exe" set "PATH=C:\TDM-GCC-64\bin;%PATH%"
for %%T in (cargo.exe rustc.exe rustup.exe python.exe node.exe npm.cmd npx.cmd gcc.exe ar.exe powershell.exe) do (
    where %%T >nul 2>nul
    if errorlevel 1 (
        echo Missing dependency: %%T
        goto failed
    )
)

echo [1/5] Check Windows GNU target...
rustup target list --installed | findstr /c:"x86_64-pc-windows-gnu" >nul
if errorlevel 1 (
    if defined RELAY_OFFLINE (
        echo GNU target missing. Run: rustup target add x86_64-pc-windows-gnu
        goto failed
    )
    rustup target add x86_64-pc-windows-gnu
    if errorlevel 1 goto failed
)

echo [2/5] Build embedded Web UI...
if not exist "webui\tailwind\node_modules\.bin\tailwindcss.cmd" (
    pushd "webui\tailwind"
    call npm.cmd ci --ignore-scripts --no-audit --no-fund %RELAY_NPM_FLAGS%
    if errorlevel 1 (
        popd
        goto failed
    )
    popd
)
python.exe webui\build_ui.py
if errorlevel 1 goto failed

echo [3/5] Prepare compatible GNU linker...
set "RELAY_BUILD_SUPPORT=%CD%\target\build-support"
if not exist "%RELAY_BUILD_SUPPORT%" mkdir "%RELAY_BUILD_SUPPORT%"
if errorlevel 1 goto failed
for /f "delims=" %%S in ('rustc.exe --print sysroot') do set "RELAY_RUST_SYSROOT=%%S"
for /f "tokens=2" %%H in ('rustc.exe -vV ^| findstr /b /c:"host:"') do set "RELAY_RUST_HOST=%%H"
set "RELAY_LLD=%RELAY_RUST_SYSROOT%\lib\rustlib\%RELAY_RUST_HOST%\bin\rust-lld.exe"
if not exist "%RELAY_LLD%" (
    echo Rust bundled linker missing: "%RELAY_LLD%"
    goto failed
)
copy /y "%RELAY_LLD%" "%RELAY_BUILD_SUPPORT%\ld.exe" >nul
if errorlevel 1 goto failed
ar.exe rcs "%RELAY_BUILD_SUPPORT%\libgcc_eh.a"
if errorlevel 1 goto failed
powershell.exe -NoProfile -Command "$ErrorActionPreference='Stop'; $specs = & gcc.exe -dumpspecs; if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }; $specs -replace '--exclude-libs=libpthread.a','' | Set-Content -LiteralPath (Join-Path $env:RELAY_BUILD_SUPPORT 'gcc.specs') -Encoding ascii"
if errorlevel 1 goto failed
set "LIBRARY_PATH=%RELAY_BUILD_SUPPORT%;%LIBRARY_PATH%"
set "COMPILER_PATH=%RELAY_BUILD_SUPPORT%;%COMPILER_PATH%"
set "RELAY_GCC_SPECS=%RELAY_BUILD_SUPPORT:\=/%/gcc.specs"

echo [4/5] Build release executable...
cargo.exe %RELAY_CARGO_SOURCE% rustc --locked %RELAY_OFFLINE% --release --target x86_64-pc-windows-gnu -- -C "link-arg=-specs=%RELAY_GCC_SPECS%"
if errorlevel 1 goto failed

echo [5/5] Export executable...
if not exist "dist" mkdir "dist"
if errorlevel 1 goto failed
copy /y "target\x86_64-pc-windows-gnu\release\claude-relay.exe" "dist\claude-relay.exe" >nul
if errorlevel 1 goto failed
powershell.exe -NoProfile -Command "$ErrorActionPreference='Stop'; $file=Get-Item -LiteralPath 'dist\claude-relay.exe'; if ($file.Length -gt 6MB) { throw 'Release executable exceeds the 6 MiB size limit' }; Write-Host ('Output: ' + $file.FullName); Write-Host ('Size: ' + [Math]::Round($file.Length / 1MB, 2) + ' MiB')"
if errorlevel 1 goto failed
echo Build succeeded.
popd
if not defined RELAY_NO_PAUSE pause
exit /b 0

:failed
echo Build failed. See the error above.
popd
if not defined RELAY_NO_PAUSE pause
exit /b 1
