@echo off
setlocal
cd /d "%~dp0"

if not exist "target\release\touchcursor.exe" (
  echo Building TouchCursor for the first launch...
  cargo build --release
  if errorlevel 1 (
    echo.
    echo Build failed. Install Rust and check the error above.
    pause
    exit /b 1
  )
)

start "" "%~dp0target\release\touchcursor.exe"
