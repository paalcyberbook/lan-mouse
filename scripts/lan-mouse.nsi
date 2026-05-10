; NSIS installer script for Lan Mouse+.
;
; Expects the staged output directory at $STAGE_DIR to contain:
;   bin\lan-mouse.exe
;   bin\<all DLLs>
;   launch-lan-mouse.exe
;   README.txt
;
; Invoked as:
;   makensis -DSTAGE_DIR=/output -DOUT_FILE=/build/lan-mouse-windows-x86_64-installer.exe scripts/lan-mouse.nsi

!ifndef STAGE_DIR
  !error "STAGE_DIR not defined"
!endif
!ifndef OUT_FILE
  !error "OUT_FILE not defined"
!endif

!include "MUI2.nsh"
!include "LogicLib.nsh"

Name "Lan Mouse+"
OutFile "${OUT_FILE}"
; Install dir stays "Lan Mouse" so upgrades from unbranded builds don't
; leave an orphan directory; the user-facing Name above carries the plus.
InstallDir "$PROGRAMFILES64\Lan Mouse"
InstallDirRegKey HKLM "Software\LanMouse" "InstallDir"
RequestExecutionLevel admin
Unicode true

!define MUI_ABORTWARNING
!define MUI_ICON   "..\build-aux\lan-mouse.ico"
!define MUI_UNICON "..\build-aux\lan-mouse.ico"

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_COMPONENTS
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "English"

Section "Lan Mouse+ (required)" SecCore
  SectionIn RO
  SetOutPath "$INSTDIR"
  File /r "${STAGE_DIR}\bin"
  File "${STAGE_DIR}\README.txt"

  WriteRegStr HKLM "Software\LanMouse" "InstallDir" "$INSTDIR"

  ; Add/Remove Programs entry
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\LanMouse" \
    "DisplayName" "Lan Mouse+"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\LanMouse" \
    "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\LanMouse" \
    "DisplayIcon" '"$INSTDIR\bin\lan-mouse.exe"'
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\LanMouse" \
    "Publisher" "feschber"
  WriteRegDWORD HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\LanMouse" \
    "NoModify" 1
  WriteRegDWORD HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\LanMouse" \
    "NoRepair" 1

  WriteUninstaller "$INSTDIR\uninstall.exe"
SectionEnd

Section "Start Menu shortcut" SecStartMenu
  CreateDirectory "$SMPROGRAMS\Lan Mouse"
  CreateShortCut "$SMPROGRAMS\Lan Mouse\Lan Mouse.lnk" \
    "$INSTDIR\bin\lan-mouse.exe" "" "$INSTDIR\bin\lan-mouse.exe" 0
  CreateShortCut "$SMPROGRAMS\Lan Mouse\Uninstall.lnk" \
    "$INSTDIR\uninstall.exe"
SectionEnd

Section /o "Desktop shortcut" SecDesktop
  CreateShortCut "$DESKTOP\Lan Mouse.lnk" \
    "$INSTDIR\bin\lan-mouse.exe" "" "$INSTDIR\bin\lan-mouse.exe" 0
SectionEnd

; Optional: install Lan Mouse+ as a Windows Service. Required for input
; control to keep working when a UAC prompt or admin-elevated app is in
; focus on this PC, since UIPI blocks input from a Medium-integrity process
; into a higher-integrity window. The service runs as LocalSystem.
;
; NOTE: cross-session input injection (so events from a session-0 service
; reach the user's interactive desktop) is the next iteration of work — for
; now installing the service gives you the lifecycle hooks but the helper
; that bridges session 0 → user session is a follow-up.
Section /o "Install as Windows Service (control admin/UAC apps)" SecService
  ; nsExec runs the bundled lan-mouse.exe to register the service with SCM.
  ; The installer itself is already elevated (RequestExecutionLevel admin)
  ; so the SCM call succeeds without an additional UAC prompt.
  nsExec::ExecToLog '"$INSTDIR\bin\lan-mouse.exe" cli service install'
  Pop $0
  ${If} $0 == 0
    nsExec::ExecToLog '"$INSTDIR\bin\lan-mouse.exe" cli service start'
    Pop $0
  ${EndIf}
SectionEnd

; "Send via Lan Mouse+" Explorer right-click verb on files and folders.
; On Windows 11 the entry sits under "Show more options" / Shift+F10 — a
; proper top-level shell extension would need a signed COM DLL or MSIX.
Section "Explorer right-click 'Send via Lan Mouse+'" SecExplorerMenu
  ; Files
  WriteRegStr HKLM "Software\Classes\*\shell\LanMousePlusSend" \
    "" "Send via Lan Mouse+"
  WriteRegStr HKLM "Software\Classes\*\shell\LanMousePlusSend" \
    "Icon" '"$INSTDIR\bin\lan-mouse.exe",0'
  WriteRegStr HKLM "Software\Classes\*\shell\LanMousePlusSend\command" \
    "" '"$INSTDIR\bin\lan-mouse.exe" cli send-file --path "%1"'

  ; Folders
  WriteRegStr HKLM "Software\Classes\Directory\shell\LanMousePlusSend" \
    "" "Send via Lan Mouse+"
  WriteRegStr HKLM "Software\Classes\Directory\shell\LanMousePlusSend" \
    "Icon" '"$INSTDIR\bin\lan-mouse.exe",0'
  WriteRegStr HKLM "Software\Classes\Directory\shell\LanMousePlusSend\command" \
    "" '"$INSTDIR\bin\lan-mouse.exe" cli send-file --path "%1"'
SectionEnd

LangString DESC_SecCore         ${LANG_ENGLISH} "Core Lan Mouse files (required)."
LangString DESC_SecStartMenu    ${LANG_ENGLISH} "Add Lan Mouse to the Start Menu."
LangString DESC_SecDesktop      ${LANG_ENGLISH} "Place a shortcut on the Desktop."
LangString DESC_SecExplorerMenu ${LANG_ENGLISH} "Add a 'Send via Lan Mouse+' entry to the Explorer right-click menu for files and folders."
LangString DESC_SecService      ${LANG_ENGLISH} "Install as a Windows Service so it can keep accepting remote input even when a UAC prompt or admin app is on top."

!insertmacro MUI_FUNCTION_DESCRIPTION_BEGIN
  !insertmacro MUI_DESCRIPTION_TEXT ${SecCore}         $(DESC_SecCore)
  !insertmacro MUI_DESCRIPTION_TEXT ${SecStartMenu}    $(DESC_SecStartMenu)
  !insertmacro MUI_DESCRIPTION_TEXT ${SecDesktop}      $(DESC_SecDesktop)
  !insertmacro MUI_DESCRIPTION_TEXT ${SecExplorerMenu} $(DESC_SecExplorerMenu)
  !insertmacro MUI_DESCRIPTION_TEXT ${SecService}      $(DESC_SecService)
!insertmacro MUI_FUNCTION_DESCRIPTION_END

Section "Uninstall"
  ; Best-effort: stop and remove the service if it was installed. Ignore
  ; errors so an upgrade-style uninstall on a host that never installed
  ; the service still completes cleanly.
  nsExec::ExecToLog '"$INSTDIR\bin\lan-mouse.exe" cli service stop'
  Pop $0
  nsExec::ExecToLog '"$INSTDIR\bin\lan-mouse.exe" cli service uninstall'
  Pop $0

  RMDir /r "$INSTDIR\bin"
  Delete "$INSTDIR\README.txt"
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"

  Delete "$SMPROGRAMS\Lan Mouse\Lan Mouse.lnk"
  Delete "$SMPROGRAMS\Lan Mouse\Uninstall.lnk"
  RMDir  "$SMPROGRAMS\Lan Mouse"
  Delete "$DESKTOP\Lan Mouse.lnk"

  ; Explorer right-click verbs
  DeleteRegKey HKLM "Software\Classes\*\shell\LanMousePlusSend"
  DeleteRegKey HKLM "Software\Classes\Directory\shell\LanMousePlusSend"

  DeleteRegKey HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\LanMouse"
  DeleteRegKey HKLM "Software\LanMouse"
SectionEnd
