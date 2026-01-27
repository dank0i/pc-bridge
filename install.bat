@echo off
echo Installing PC Agent as Windows Service...
echo.

REM Check for admin rights
net session >nul 2>&1
if %errorLevel% neq 0 (
    echo ERROR: Please run as Administrator!
    pause
    exit /b 1
)

REM Stop existing service if running
sc query PCAgentService >nul 2>&1
if %ERRORLEVEL% EQU 0 (
    echo Stopping existing service...
    sc stop PCAgentService
    timeout /t 2 /nobreak >nul
    echo Removing existing service...
    sc delete PCAgentService
    timeout /t 2 /nobreak >nul
)

REM Get current directory
set "AGENT_PATH=%~dp0pc-agent.exe"

REM Create the service
echo Creating service...
sc create PCAgentService binPath= "%AGENT_PATH%" start= auto DisplayName= "PC Agent Service"

REM Set description
sc description PCAgentService "Home Assistant PC Agent - monitors games, power events, and handles commands"

REM Start the service
echo Starting service...
sc start PCAgentService

echo.
echo Done! Service installed and started.
echo.
echo To check status: sc query PCAgentService
echo To stop:         sc stop PCAgentService
echo To remove:       sc delete PCAgentService
echo.
pause
