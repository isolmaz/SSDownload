@echo off
setlocal
"%~dp0ssdownload.exe" --register-host
if errorlevel 1 (
  echo SSDownload browser integration registration failed. 1>&2
  exit /b 1
)
echo SSDownload browser integration was registered for the current user.
