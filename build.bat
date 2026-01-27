@echo off
cd /d %~dp0

echo Updating...
git pull >nul 2>&1
if %ERRORLEVEL% NEQ 0 (
    echo Warning: Update failed, continuing with local files...
)

REM Create userConfig.json from example if it doesn't exist
if not exist userConfig.json (
    REM Restore example from git if deleted
    if not exist userConfig.example.json (
        git checkout userConfig.example.json >nul 2>&1
    )
    if exist userConfig.example.json (
        echo Creating userConfig.json from example...
        copy userConfig.example.json userConfig.json >nul
        del userConfig.example.json >nul 2>&1
        echo.
        echo ==========================================
        echo  userConfig.json created!
        echo  Edit it with your settings before running.
        echo ==========================================
        echo.
    ) else (
        echo ERROR: userConfig.example.json not found!
        pause
        exit /b 1
    )
)

echo Generating Windows resources...
go-winres make >nul 2>&1
if %ERRORLEVEL% NEQ 0 (
    echo Warning: go-winres failed, continuing without resources...
)

echo Building PC Agent...
go build -ldflags="-H windowsgui -s -w" -o pc-agent.exe .
if %ERRORLEVEL% EQU 0 (
    echo Build successful: pc-agent.exe
) else (
    echo Build failed!
)

REM Clean up build artifacts
del *.syso >nul 2>&1
echo Cleaned up build artifacts...

echo.
echo Press any key to exit...
pause >nul
