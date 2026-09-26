; NSIS installer for the desktop app (voelin.exe). See README.md.
;
;   makensis /DVERSION=0.1.0 packaging\windows\installer.nsi
;
; Paths are relative to this file. Optional defines:
;   BINARY    the executable (default ..\..\target\release\voelin.exe)
;   OUTDIR    where the installer goes (default: next to this file)
;   SIGN      a signing command; "%1" is replaced by the file to sign, e.g.
;             /DSIGN="signtool sign /fd SHA256 /tr http://timestamp.digicert.com /td SHA256 /a %1"
;

Unicode true
SetCompressor /SOLID lzma

!ifndef VERSION
	!define VERSION "0.1.0"
!endif
!ifndef BINARY
	!define BINARY "..\..\target\release\voelin.exe"
!endif
!ifndef OUTDIR
	!define OUTDIR "."
!endif

!define APP_NAME "Voelin"
!define APP_ID "io.github.faumaray.Voelin"
!define PUBLISHER "Faumaray"
!define EXE "voelin.exe"
!define UNINSTALL_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\${APP_ID}"

!ifdef SIGN
	; Sign the installer and the uninstaller it writes (NSIS 3.08+).
	!finalize '${SIGN}'
	!uninstfinalize '${SIGN}'
!endif

Name "${APP_NAME}"
OutFile "${OUTDIR}\voelin-${VERSION}-setup.exe"
InstallDir "$PROGRAMFILES64\${APP_NAME}"
InstallDirRegKey HKLM "${UNINSTALL_KEY}" "InstallLocation"
RequestExecutionLevel admin

VIProductVersion "${VERSION}.0"
VIAddVersionKey "ProductName" "${APP_NAME}"
VIAddVersionKey "ProductVersion" "${VERSION}"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "CompanyName" "${PUBLISHER}"
VIAddVersionKey "FileDescription" "${APP_NAME} installer"
VIAddVersionKey "LegalCopyright" "MIT OR Apache-2.0"

!include "MUI2.nsh"
!define MUI_FINISHPAGE_RUN "$INSTDIR\${EXE}"
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"

Section "Install"
	SetOutPath "$INSTDIR"
	File "/oname=${EXE}" "${BINARY}"
	; Also shown in the app's About page.
	File "/oname=THIRD_PARTY_NOTICES.md" "..\..\THIRD_PARTY_NOTICES.md"
	WriteUninstaller "$INSTDIR\uninstall.exe"

	CreateShortcut "$SMPROGRAMS\${APP_NAME}.lnk" "$INSTDIR\${EXE}"

	WriteRegStr HKLM "${UNINSTALL_KEY}" "DisplayName" "${APP_NAME}"
	WriteRegStr HKLM "${UNINSTALL_KEY}" "DisplayVersion" "${VERSION}"
	WriteRegStr HKLM "${UNINSTALL_KEY}" "Publisher" "${PUBLISHER}"
	WriteRegStr HKLM "${UNINSTALL_KEY}" "InstallLocation" "$INSTDIR"
	WriteRegStr HKLM "${UNINSTALL_KEY}" "DisplayIcon" "$INSTDIR\${EXE}"
	WriteRegStr HKLM "${UNINSTALL_KEY}" "UninstallString" '"$INSTDIR\uninstall.exe"'
	WriteRegStr HKLM "${UNINSTALL_KEY}" "QuietUninstallString" '"$INSTDIR\uninstall.exe" /S'
	WriteRegDWORD HKLM "${UNINSTALL_KEY}" "NoModify" 1
	WriteRegDWORD HKLM "${UNINSTALL_KEY}" "NoRepair" 1
SectionEnd

Section "Uninstall"
	; User data in %APPDATA%\voelin (identities, bookmarks, history) is kept.
	Delete "$INSTDIR\${EXE}"
	Delete "$INSTDIR\THIRD_PARTY_NOTICES.md"
	Delete "$INSTDIR\uninstall.exe"
	RMDir "$INSTDIR"
	Delete "$SMPROGRAMS\${APP_NAME}.lnk"
	DeleteRegKey HKLM "${UNINSTALL_KEY}"
SectionEnd
