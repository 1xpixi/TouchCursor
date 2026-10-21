@echo off
setlocal
cd /d "%~dp0"
if errorlevel 1 exit /b 1

if exist "%~dp0touchcursor.exe" (
  start "" "%~dp0touchcursor.exe"
  exit /b
)

where cargo >nul 2>&1
if errorlevel 1 (
  echo Cargo was not found. Install Rust to build from source, or use the portable release ZIP.
  exit /b 1
)

echo Building TouchCursor from source...
cargo build --release --locked
if errorlevel 1 (
  echo.
  echo Build failed. TouchCursor was not launched. Check the error above.
  exit /b 1
)

start "" "%~dp0target\release\touchcursor.exe"
exit /b
