@echo off
setlocal

echo ========================================
echo PC Agent (Rust) Build Script
echo ========================================
echo.

:: Pull latest from git
echo Pulling latest changes...
git pull
if errorlevel 1 (
    echo Warning: Git pull failed, continuing with local files
)
echo.

:: Check if Rust is installed
where cargo >nul 2>&1
if errorlevel 1 (
    echo ERROR: Rust/Cargo not found!
    echo Install from: https://rustup.rs/
    pause
    exit /b 1
)

:: Build release
echo Building release binary...
cargo build --release
if errorlevel 1 (
    echo.
    echo ERROR: Build failed!
    pause
    exit /b 1
)

:: Copy binary to current directory
echo.
echo Copying binary...
copy /Y "target\release\pc-agent.exe" "pc-agent.exe"

:: Copy config if it doesn't exist
if not exist "userConfig.json" (
    if exist "userConfig.example.json" (
        echo Creating userConfig.json from example...
        copy "userConfig.example.json" "userConfig.json"
        echo.
        echo IMPORTANT: Edit userConfig.json with your settings!
    )
)

echo.
echo ========================================
echo Build complete!
echo.
echo Binary: %CD%\pc-agent.exe
echo Size: 
for %%A in (pc-agent.exe) do echo   %%~zA bytes (%%~zA / 1048576 MB)
echo.
echo To run: pc-agent.exe
echo To install as service:
echo   sc create PCAgentService binPath= "%CD%\pc-agent.exe"
echo   sc start PCAgentService
echo ========================================
pause
