Unicode true
RequestExecutionLevel user
ManifestDPIAware true
SetCompressor /SOLID lzma

!include "MUI2.nsh"
!include "LogicLib.nsh"
!include "x64.nsh"

!ifndef VERSION
  !error "VERSION must be supplied by scripts/package.ps1"
!endif
!ifndef STAGE_DIR
  !error "STAGE_DIR must point at the prepared SSDownload package directory"
!endif
!ifndef OUTPUT_FILE
  !define OUTPUT_FILE "SSDownload-${VERSION}-Setup.exe"
!endif
!ifdef SIGN_SCRIPT
  !uninstfinalize 'powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "${SIGN_SCRIPT}" -Path "%1" ${SIGN_ARGS}' = 0
!endif

!define PRODUCT_NAME "SSDownload"
!define PRODUCT_PUBLISHER "SSDownload Project"
!define PRODUCT_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\SSDownload"
!define RUN_KEY "Software\Microsoft\Windows\CurrentVersion\Run"

Name "${PRODUCT_NAME} ${VERSION}"
OutFile "${OUTPUT_FILE}"
InstallDir "$LOCALAPPDATA\Programs\SSDownload"
InstallDirRegKey HKCU "${PRODUCT_KEY}" "InstallLocation"
ShowInstDetails show
ShowUninstDetails show
BrandingText "SSDownload ${VERSION} — kullanıcı başına kurulum"

VIProductVersion "${VERSION}.0"
VIAddVersionKey /LANG=1033 "ProductName" "SSDownload"
VIAddVersionKey /LANG=1033 "CompanyName" "SSDownload Project"
VIAddVersionKey /LANG=1033 "FileDescription" "SSDownload per-user installer"
VIAddVersionKey /LANG=1033 "FileVersion" "${VERSION}"
VIAddVersionKey /LANG=1033 "ProductVersion" "${VERSION}"
VIAddVersionKey /LANG=1033 "LegalCopyright" "Copyright (c) 2026 SSDownload contributors"

!define MUI_ABORTWARNING
!define MUI_ICON "${STAGE_DIR}\app.ico"
!define MUI_UNICON "${STAGE_DIR}\app.ico"
!define MUI_FINISHPAGE_RUN "$INSTDIR\ssdownload.exe"
!define MUI_FINISHPAGE_RUN_TEXT "SSDownload uygulamasını başlat"

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_LICENSE "${STAGE_DIR}\LICENSE.txt"
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_COMPONENTS
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_UNPAGE_FINISH

!insertmacro MUI_LANGUAGE "Turkish"
!insertmacro MUI_LANGUAGE "English"

Function ValidateInstallDirectory
  IfFileExists "$INSTDIR\ssdownload.install" valid_directory
  FindFirst $0 $1 "$INSTDIR\*"
  directory_entry:
    StrCmp $1 "" empty_directory
    StrCmp $1 "." next_directory_entry
    StrCmp $1 ".." next_directory_entry
    FindClose $0
    IfSilent unsafe_directory
    MessageBox MB_ICONSTOP "Boş bir klasör veya mevcut SSDownload kurulum klasörünü seçin. Diğer dosyaların üzerine yazılmayacak."
  unsafe_directory:
    SetErrorLevel 2
    Abort
  next_directory_entry:
    FindNext $0 $1
    Goto directory_entry
  empty_directory:
    FindClose $0
  valid_directory:
FunctionEnd

Function .onVerifyInstDir
  Call ValidateInstallDirectory
FunctionEnd

Section "SSDownload (gerekli)" SEC_CORE
  SectionIn RO
  SetShellVarContext current
  Call ValidateInstallDirectory

  ${IfNot} ${RunningX64}
    MessageBox MB_ICONSTOP "SSDownload, 64-bit Windows 10 veya 11 gerektirir."
    Abort
  ${EndIf}

  ; Ask an existing copy to stop through its normal IPC path. Never terminate it forcibly.
  IfFileExists "$INSTDIR\ssdownload.exe" 0 +3
    ExecWait '$\"$INSTDIR\ssdownload.exe$\" --quit' $0
    Sleep 1200

  RMDir /r "$INSTDIR\browser"
  SetOutPath "$INSTDIR"
  File "${STAGE_DIR}\ssdownload.exe"
  File "${STAGE_DIR}\ssdownload-cli.exe"
  File "${STAGE_DIR}\LICENSE.txt"
  File "${STAGE_DIR}\THIRD-PARTY-NOTICES.txt"
  ; Remove only application-owned documents that earlier versions installed.
  Delete "$INSTDIR\CHANGELOG.md"
  Delete "$INSTDIR\RECOVERY.md"
  Delete "$INSTDIR\RELEASING.md"
  Delete "$INSTDIR\IMPLEMENTATION_PLAN.md"
  Delete "$INSTDIR\plan.md"
  Delete "$INSTDIR\TESTING.md"
  RMDir /r "$INSTDIR\verification"
  File "${STAGE_DIR}\README.md"
  File "${STAGE_DIR}\PRIVACY.md"
  File "${STAGE_DIR}\sbom.cdx.json"
  SetOutPath "$INSTDIR\licenses"
  File /r "${STAGE_DIR}\licenses\*.*"
  SetOutPath "$INSTDIR"
  File "${STAGE_DIR}\app.ico"

  SetOutPath "$INSTDIR\browser"
  File /r "${STAGE_DIR}\browser\*.*"
  SetOutPath "$INSTDIR"

  FileOpen $0 "$INSTDIR\ssdownload.install" w
  FileWrite $0 "SSDownload ${VERSION}$\r$\n"
  FileClose $0
  WriteUninstaller "$INSTDIR\Uninstall.exe"

  CreateDirectory "$SMPROGRAMS\SSDownload"
  CreateShortcut "$SMPROGRAMS\SSDownload\SSDownload.lnk" "$INSTDIR\ssdownload.exe" "" "$INSTDIR\app.ico" 0 SW_SHOWNORMAL
  CreateShortcut "$SMPROGRAMS\SSDownload\SSDownload kaldır.lnk" "$INSTDIR\Uninstall.exe"

  WriteRegStr HKCU "${PRODUCT_KEY}" "DisplayName" "SSDownload"
  WriteRegStr HKCU "${PRODUCT_KEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "${PRODUCT_KEY}" "Publisher" "${PRODUCT_PUBLISHER}"
  WriteRegStr HKCU "${PRODUCT_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "${PRODUCT_KEY}" "DisplayIcon" "$INSTDIR\ssdownload.exe,0"
  WriteRegStr HKCU "${PRODUCT_KEY}" "UninstallString" '$\"$INSTDIR\Uninstall.exe$\"'
  WriteRegStr HKCU "${PRODUCT_KEY}" "QuietUninstallString" '$\"$INSTDIR\Uninstall.exe$\" /S'
  WriteRegDWORD HKCU "${PRODUCT_KEY}" "NoModify" 1
  WriteRegDWORD HKCU "${PRODUCT_KEY}" "NoRepair" 1

  ; Registration is a required installer operation, not a copy-only approximation.
  ExecWait '$\"$INSTDIR\ssdownload.exe$\" --register-host' $0
  ${If} $0 != 0
    MessageBox MB_ICONSTOP "Tarayıcı Native Messaging kaydı oluşturulamadı (çıkış kodu $0). Kurulum tamamlanamadı."
    Abort
  ${EndIf}
SectionEnd

Section /o "Windows ile başlat" SEC_AUTOSTART
  SetShellVarContext current
  ExecWait '$\"$INSTDIR\ssdownload.exe$\" --enable-autostart' $0
SectionEnd

LangString DESC_SEC_CORE ${LANG_TURKISH} "Uygulama, tarayıcı eklentileri, lisanslar, Başlat menüsü kısayolları ve Native Messaging kaydı."
LangString DESC_SEC_CORE ${LANG_ENGLISH} "Application, browser extensions, licenses, Start Menu shortcuts, and Native Messaging registration."
LangString DESC_SEC_AUTOSTART ${LANG_TURKISH} "SSDownload uygulamasını oturum açıldığında arka planda başlatır."
LangString DESC_SEC_AUTOSTART ${LANG_ENGLISH} "Starts SSDownload in the background when you sign in."
!insertmacro MUI_FUNCTION_DESCRIPTION_BEGIN
  !insertmacro MUI_DESCRIPTION_TEXT ${SEC_CORE} $(DESC_SEC_CORE)
  !insertmacro MUI_DESCRIPTION_TEXT ${SEC_AUTOSTART} $(DESC_SEC_AUTOSTART)
!insertmacro MUI_FUNCTION_DESCRIPTION_END

Section "Uninstall"
  SetShellVarContext current

  ; Gracefully stop the app, then let its own implementation remove manifests and HKCU keys.
  IfFileExists "$INSTDIR\ssdownload.exe" 0 +4
    ExecWait '$\"$INSTDIR\ssdownload.exe$\" --quit' $0
    Sleep 1200
    ExecWait '$\"$INSTDIR\ssdownload.exe$\" --unregister-host' $0

  ; Only the owning executable may unregister its profile. Another portable or
  ; installed copy may currently own the browser keys; never delete them blindly.
  ExecWait '$\"$INSTDIR\ssdownload.exe$\" --disable-autostart' $0
  DeleteRegKey HKCU "${PRODUCT_KEY}"

  Delete "$SMPROGRAMS\SSDownload\SSDownload.lnk"
  Delete "$SMPROGRAMS\SSDownload\SSDownload kaldır.lnk"
  RMDir "$SMPROGRAMS\SSDownload"

  Delete /REBOOTOK "$INSTDIR\ssdownload.exe"
  Delete /REBOOTOK "$INSTDIR\ssdownload-cli.exe"
  Delete "$INSTDIR\LICENSE.txt"
  Delete "$INSTDIR\THIRD-PARTY-NOTICES.txt"
  Delete "$INSTDIR\README.md"
  Delete "$INSTDIR\PRIVACY.md"
  Delete "$INSTDIR\plan.md"
  Delete "$INSTDIR\CHANGELOG.md"
  Delete "$INSTDIR\RECOVERY.md"
  Delete "$INSTDIR\RELEASING.md"
  Delete "$INSTDIR\IMPLEMENTATION_PLAN.md"
  Delete "$INSTDIR\TESTING.md"
  Delete "$INSTDIR\sbom.cdx.json"
  RMDir /r "$INSTDIR\verification"
  RMDir /r "$INSTDIR\licenses"
  Delete "$INSTDIR\app.ico"
  RMDir /r "$INSTDIR\browser"
  Delete "$INSTDIR\ssdownload.install"
  Delete /REBOOTOK "$INSTDIR\Uninstall.exe"
  RMDir "$INSTDIR"

  ; Intentionally preserve $LOCALAPPDATA\SSDownload and every downloaded file.
SectionEnd
