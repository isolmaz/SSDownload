@echo off
setlocal
"%~dp0ssdownload.exe" --unregister-host
if errorlevel 1 (
  echo SSDownload browser integration removal failed. 1>&2
  exit /b 1
)
echo SSDownload browser integration was removed for the current user.
