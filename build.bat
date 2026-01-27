@echo off
echo Building PC Agent...
go build -ldflags="-H windowsgui" -o pc-agent.exe .
if %ERRORLEVEL% EQU 0 (
    echo Build successful: pc-agent.exe
) else (
    echo Build failed!
    pause
)
