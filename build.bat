@echo off
echo Pulling latest changes...
git pull
if %ERRORLEVEL% NEQ 0 (
    echo Warning: git pull failed, continuing with local files...
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
