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

VIProductVersion "${VERSION}.0"
VIAddVersionKey /LANG=1033 "ProductName" "SSDownload"
VIAddVersionKey /LANG=1033 "CompanyName" "SSDownload Project"
VIAddVersionKey /LANG=1033 "FileDescription" "SSDownload per-user installer"
VIAddVersionKey /LANG=1033 "FileVersion" "${VERSION}"
VIAddVersionKey /LANG=1033 "ProductVersion" "${VERSION}"
VIAddVersionKey /LANG=1033 "LegalCopyright" "Copyright (c) 2026 SSDownload contributors"
VIAddVersionKey /LANG=1055 "ProductName" "SSDownload"
VIAddVersionKey /LANG=1055 "CompanyName" "SSDownload Project"
VIAddVersionKey /LANG=1055 "FileDescription" "SSDownload kullanıcı başına yükleyici"
VIAddVersionKey /LANG=1055 "FileVersion" "${VERSION}"
VIAddVersionKey /LANG=1055 "ProductVersion" "${VERSION}"
VIAddVersionKey /LANG=1055 "LegalCopyright" "Copyright (c) 2026 SSDownload contributors"

!define MUI_ABORTWARNING
!define MUI_ICON "${STAGE_DIR}\app.ico"
!define MUI_UNICON "${STAGE_DIR}\app.ico"
!define MUI_FINISHPAGE_RUN "$INSTDIR\ssdownload.exe"
!define MUI_FINISHPAGE_RUN_TEXT "$(FINISH_RUN_TEXT)"

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_LICENSE "${STAGE_DIR}\LICENSE.txt"
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_COMPONENTS
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_UNPAGE_FINISH

; The first language is the fallback for a Windows display language the installer does not
; have, so English comes first and Turkish is chosen for a Turkish Windows.
!insertmacro MUI_LANGUAGE "English"
!insertmacro MUI_LANGUAGE "Turkish"

LangString BRANDING_TEXT ${LANG_ENGLISH} "SSDownload ${VERSION} — per-user installation"
LangString BRANDING_TEXT ${LANG_TURKISH} "SSDownload ${VERSION} — kullanıcı başına kurulum"
BrandingText "$(BRANDING_TEXT)"

LangString FINISH_RUN_TEXT ${LANG_ENGLISH} "Start SSDownload"
LangString FINISH_RUN_TEXT ${LANG_TURKISH} "SSDownload uygulamasını başlat"
LangString SEC_CORE_NAME ${LANG_ENGLISH} "SSDownload (required)"
LangString SEC_CORE_NAME ${LANG_TURKISH} "SSDownload (gerekli)"
LangString SEC_AUTOSTART_NAME ${LANG_ENGLISH} "Start with Windows"
LangString SEC_AUTOSTART_NAME ${LANG_TURKISH} "Windows ile başlat"
LangString ERR_INSTALL_DIR ${LANG_ENGLISH} "Choose an empty folder or an existing SSDownload installation folder. Other files will not be overwritten."
LangString ERR_INSTALL_DIR ${LANG_TURKISH} "Boş bir klasör veya mevcut SSDownload kurulum klasörünü seçin. Diğer dosyaların üzerine yazılmayacak."
LangString ERR_WINDOWS_64 ${LANG_ENGLISH} "SSDownload requires 64-bit Windows 10 or 11."
LangString ERR_WINDOWS_64 ${LANG_TURKISH} "SSDownload, 64-bit Windows 10 veya 11 gerektirir."
LangString ERR_REGISTER_HOST ${LANG_ENGLISH} "The browser Native Messaging registration could not be created (exit code $0). Setup could not be completed."
LangString ERR_REGISTER_HOST ${LANG_TURKISH} "Tarayıcı Native Messaging kaydı oluşturulamadı (çıkış kodu $0). Kurulum tamamlanamadı."
LangString DETAIL_COMPONENTS ${LANG_ENGLISH} "Downloading and installing missing components…"
LangString DETAIL_COMPONENTS ${LANG_TURKISH} "Eksik bileşenler indiriliyor ve kuruluyor…"
LangString WARN_COMPONENTS ${LANG_ENGLISH} "Missing components could not be downloaded. SSDownload will try again when it starts."
LangString WARN_COMPONENTS ${LANG_TURKISH} "Eksik bileşenler indirilemedi. SSDownload açıldığında yeniden deneyecek."
LangString SHORTCUT_UNINSTALL ${LANG_ENGLISH} "Uninstall SSDownload"
LangString SHORTCUT_UNINSTALL ${LANG_TURKISH} "SSDownload kaldır"

Function ValidateInstallDirectory
  IfFileExists "$INSTDIR\ssdownload.install" valid_directory
  FindFirst $0 $1 "$INSTDIR\*"
  directory_entry:
    StrCmp $1 "" empty_directory
    StrCmp $1 "." next_directory_entry
    StrCmp $1 ".." next_directory_entry
    FindClose $0
    IfSilent unsafe_directory
    MessageBox MB_ICONSTOP "$(ERR_INSTALL_DIR)"
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

Section "$(SEC_CORE_NAME)" SEC_CORE
  SectionIn RO
  SetShellVarContext current
  Call ValidateInstallDirectory

  ${IfNot} ${RunningX64}
    MessageBox MB_ICONSTOP "$(ERR_WINDOWS_64)"
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
  ; The uninstall shortcut is named in the installer's language; an earlier install may have
  ; left it under the other language's name.
  Delete "$SMPROGRAMS\SSDownload\SSDownload kaldır.lnk"
  Delete "$SMPROGRAMS\SSDownload\Uninstall SSDownload.lnk"
  CreateShortcut "$SMPROGRAMS\SSDownload\$(SHORTCUT_UNINSTALL).lnk" "$INSTDIR\Uninstall.exe"

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
    MessageBox MB_ICONSTOP "$(ERR_REGISTER_HOST)"
    Abort
  ${EndIf}

  ; Missing media components are fetched now; already installed ones return at once without a
  ; network request. A failure does not fail the installation: the app retries on its start.
  ; A silent install is the app's own update, which waits for Setup before it restarts the
  ; app; the app installs anything missing itself right after that restart.
  ; The generic text stays on screen while the command runs; its raw command line is not shown.
  ${IfNot} ${Silent}
    DetailPrint "$(DETAIL_COMPONENTS)"
    SetDetailsPrint none
    ExecWait '$\"$INSTDIR\ssdownload.exe$\" --setup-tools' $0
    SetDetailsPrint both
    ${If} $0 != 0
      DetailPrint "$(WARN_COMPONENTS)"
      MessageBox MB_ICONEXCLAMATION "$(WARN_COMPONENTS)"
    ${EndIf}
  ${EndIf}
SectionEnd

Section /o "$(SEC_AUTOSTART_NAME)" SEC_AUTOSTART
  SetShellVarContext current
  ExecWait '$\"$INSTDIR\ssdownload.exe$\" --enable-autostart' $0
SectionEnd

LangString DESC_SEC_CORE ${LANG_TURKISH} "Uygulama, tarayıcı eklentileri, lisanslar, Başlat menüsü kısayolları, Native Messaging kaydı ve eksik bileşenlerin indirilmesi."
LangString DESC_SEC_CORE ${LANG_ENGLISH} "Application, browser extensions, licenses, Start Menu shortcuts, Native Messaging registration, and downloading missing components."
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
  ; The uninstaller may run in another language than the installer did: remove both names.
  Delete "$SMPROGRAMS\SSDownload\SSDownload kaldır.lnk"
  Delete "$SMPROGRAMS\SSDownload\Uninstall SSDownload.lnk"
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
