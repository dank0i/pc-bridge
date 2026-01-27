@echo off
cd /d %~dp0

echo Pulling latest changes...
git pull
if %ERRORLEVEL% NEQ 0 (
    echo Warning: git pull failed, continuing with local files...
)

REM Create userConfig.json from example if it doesn't exist
if not exist userConfig.json (
    if exist userConfig.example.json (
        echo Creating userConfig.json from example...
        copy userConfig.example.json userConfig.json >nul
        echo.
        echo ==========================================
        echo  userConfig.json created!
        echo  Edit it with your settings, then rebuild.
        echo ==========================================
        echo.
    ) else (
        echo ERROR: userConfig.example.json not found!
        pause
        exit /b 1
    )
)

echo Generating Windows resources...
go-winres make
if %ERRORLEVEL% NEQ 0 (
    echo Warning: go-winres failed, continuing without resources...
)

echo Building PC Agent...
go build -ldflags="-H windowsgui -s -w" -o pc-agent.exe .
if %ERRORLEVEL% EQU 0 (
    echo Build successful: pc-agent.exe
) else (
    echo Build failed!
    pause
)
