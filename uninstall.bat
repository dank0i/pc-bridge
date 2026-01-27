@echo off
echo Uninstalling PC Agent Service...
echo.

REM Check for admin rights
net session >nul 2>&1
if %errorLevel% neq 0 (
    echo ERROR: Please run as Administrator!
    pause
    exit /b 1
)

echo Stopping service...
sc stop PCAgentService
timeout /t 3 /nobreak >nul

echo Removing service...
sc delete PCAgentService

echo.
echo Service removed.
pause
