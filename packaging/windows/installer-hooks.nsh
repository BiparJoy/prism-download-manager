; Prism NSIS installer hooks (included by Tauri's NSIS template).
; Registers the browser native messaging host and the magnet: link handler for the
; current user, and removes them again on uninstall.

; The browser keeps the native messaging host running while the extension is connected, which
; locks prism-native-host.exe. Stop it before files are replaced or removed; the extension
; reconnects on its own afterwards. Copies moved aside by an earlier update (see below) go too.
!macro PRISM_STOP_HOST
  DetailPrint "Stopping the browser helper..."
  nsExec::ExecToLog '"$SYSDIR\taskkill.exe" /F /IM prism-native-host.exe'
  Sleep 300
  Delete "$INSTDIR\prism-native-host.old*.exe"
!macroend

; The extension starts the host again with its next message, often before the installer gets
; to the file, and writing it then fails ("Error opening file for writing"). Windows lets a
; running program be renamed, so a host that is still there is moved aside and the new one is
; written under its name. The moved copy is deleted by the next update or uninstall.
!macro PRISM_FREE_HOST_FILE
  ClearErrors
  Delete "$INSTDIR\prism-native-host.exe"
  ${If} ${Errors}
    StrCpy $R9 0
    ${Do}
      IntOp $R9 $R9 + 1
    ${LoopWhile} ${FileExists} "$INSTDIR\prism-native-host.old$R9.exe"
    DetailPrint "The browser helper is in use; moving it aside."
    Rename "$INSTDIR\prism-native-host.exe" "$INSTDIR\prism-native-host.old$R9.exe"
  ${EndIf}
!macroend

!macro NSIS_HOOK_PREINSTALL
  !insertmacro PRISM_STOP_HOST
  !insertmacro PRISM_FREE_HOST_FILE
!macroend

!macro NSIS_HOOK_POSTINSTALL
  DetailPrint "Registering browser integration..."
  nsExec::ExecToLog '"$INSTDIR\prism-native-host.exe" register'
  WriteRegStr HKCU "Software\Classes\magnet" "" "URL:Magnet link"
  WriteRegStr HKCU "Software\Classes\magnet" "URL Protocol" ""
  WriteRegStr HKCU "Software\Classes\magnet\DefaultIcon" "" "$INSTDIR\prism.exe,0"
  WriteRegStr HKCU "Software\Classes\magnet\shell\open\command" "" '"$INSTDIR\prism.exe" "%1"'
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  DetailPrint "Removing browser integration..."
  nsExec::ExecToLog '"$INSTDIR\prism-native-host.exe" unregister'
  !insertmacro PRISM_STOP_HOST
  ; Only remove the magnet handler if it still points at this installation.
  ReadRegStr $0 HKCU "Software\Classes\magnet\shell\open\command" ""
  StrCmp $0 '"$INSTDIR\prism.exe" "%1"' 0 +2
    DeleteRegKey HKCU "Software\Classes\magnet"
!macroend
