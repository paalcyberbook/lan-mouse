; NSIS installer script for Lan Mouse.
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

Name "Lan Mouse"
OutFile "${OUT_FILE}"
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

Section "Lan Mouse (required)" SecCore
  SectionIn RO
  SetOutPath "$INSTDIR"
  File /r "${STAGE_DIR}\bin"
  File "${STAGE_DIR}\README.txt"

  WriteRegStr HKLM "Software\LanMouse" "InstallDir" "$INSTDIR"

  ; Add/Remove Programs entry
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\LanMouse" \
    "DisplayName" "Lan Mouse"
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

LangString DESC_SecCore      ${LANG_ENGLISH} "Core Lan Mouse files (required)."
LangString DESC_SecStartMenu ${LANG_ENGLISH} "Add Lan Mouse to the Start Menu."
LangString DESC_SecDesktop   ${LANG_ENGLISH} "Place a shortcut on the Desktop."

!insertmacro MUI_FUNCTION_DESCRIPTION_BEGIN
  !insertmacro MUI_DESCRIPTION_TEXT ${SecCore}      $(DESC_SecCore)
  !insertmacro MUI_DESCRIPTION_TEXT ${SecStartMenu} $(DESC_SecStartMenu)
  !insertmacro MUI_DESCRIPTION_TEXT ${SecDesktop}   $(DESC_SecDesktop)
!insertmacro MUI_FUNCTION_DESCRIPTION_END

Section "Uninstall"
  RMDir /r "$INSTDIR\bin"
  Delete "$INSTDIR\README.txt"
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"

  Delete "$SMPROGRAMS\Lan Mouse\Lan Mouse.lnk"
  Delete "$SMPROGRAMS\Lan Mouse\Uninstall.lnk"
  RMDir  "$SMPROGRAMS\Lan Mouse"
  Delete "$DESKTOP\Lan Mouse.lnk"

  DeleteRegKey HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\LanMouse"
  DeleteRegKey HKLM "Software\LanMouse"
SectionEnd
