@echo off
chcp 65001 >nul
setlocal

rem ---------------------------------------------------------------------------
rem Build name-match-mcp for Windows x64 and copy the exe into dist\.
rem Run it from anywhere: it always builds the directory the script lives in.
rem ---------------------------------------------------------------------------

set "TARGET=x86_64-pc-windows-msvc"
set "BINARY=name-match-mcp.exe"

cd /d "%~dp0"
if errorlevel 1 (
    echo [ERROR] Cannot enter script directory: %~dp0
    exit /b 1
)

echo [1/4] Working directory: %CD%

where cargo >nul 2>nul
if errorlevel 1 (
    echo [ERROR] cargo not found on PATH. Install Rust from https://rustup.rs and reopen the shell.
    exit /b 1
)

echo [2/4] Checking rustup target %TARGET% ...
set "TARGET_FOUND="
for /f "usebackq tokens=* delims=" %%T in (`rustup target list --installed`) do (
    if /i "%%T"=="%TARGET%" set "TARGET_FOUND=1"
)
if not defined TARGET_FOUND (
    echo       Target missing, installing ...
    rustup target add %TARGET%
    if errorlevel 1 (
        echo [ERROR] rustup target add %TARGET% failed.
        exit /b 1
    )
)

echo [3/4] Building release binary for %TARGET% ...
cargo build --release --target %TARGET%
if errorlevel 1 (
    echo [ERROR] cargo build failed.
    exit /b 1
)

set "SOURCE=target\%TARGET%\release\%BINARY%"
if not exist "%SOURCE%" (
    echo [ERROR] Expected binary not found: %SOURCE%
    exit /b 1
)

echo [4/4] Copying artifact to dist\ ...
if not exist "dist" (
    mkdir "dist"
    if errorlevel 1 (
        echo [ERROR] Cannot create dist directory.
        exit /b 1
    )
)
copy /y "%SOURCE%" "dist\%BINARY%" >nul
if errorlevel 1 (
    echo [ERROR] Copy to dist\%BINARY% failed.
    exit /b 1
)

echo.
echo Build succeeded.
echo   Artifact: %CD%\dist\%BINARY%
for %%F in ("dist\%BINARY%") do echo   Size:     %%~zF bytes
echo   Register this exe as a stdio MCP server in your client config.

endlocal
exit /b 0
