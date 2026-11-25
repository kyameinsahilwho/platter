; Tauri NSIS installer hooks (tauri.conf.json > bundle > windows > nsis > installerHooks).
;
; Platter usually runs elevated (it asks for administrator rights at startup), but the installer and
; uninstaller are per-user and unelevated, so they can't end it themselves. Before files are replaced
; or removed, ask a running Platter to close (src/util.rs, CLOSE_MESSAGE: let through UIPI, closes the
; window the way its X button does) and wait up to 10 s for the process to go.

!include LogicLib.nsh

!macro PLATTER_CLOSE_RUNNING
  Push $0
  Push $1
  Push $2
  System::Call 'user32::RegisterWindowMessageW(w "app.platter.disks.close") i .r1'
  FindWindow $0 "PlatterMainWindow"
  ${If} $0 <> 0
  ${AndIf} $1 <> 0
    DetailPrint "Closing Platter..."
    ; SMTO_ABORTIFHUNG, 5 s.
    System::Call 'user32::SendMessageTimeoutW(p r0, i r1, p 0, p 0, i 2, i 5000, *p .r2)'
    ; The single-instance mutex goes away with the process. An elevated Platter's mutex can't be
    ; opened from here (access denied, error 5), which still means it is running.
    StrCpy $2 0
    ${Do}
      Sleep 200
      IntOp $2 $2 + 1
      System::Call 'kernel32::OpenMutexW(i 0x00100000, i 0, w "Local\app.platter.disks.instance") p .r0 ?e'
      Pop $1
      ${If} $0 <> 0
        System::Call 'kernel32::CloseHandle(p r0)'
      ${ElseIf} $1 <> 5
        ${ExitDo}
      ${EndIf}
    ${LoopUntil} $2 >= 50
    ; Let WebView2 let go of its files too.
    Sleep 500
  ${EndIf}
  Pop $2
  Pop $1
  Pop $0
!macroend

!macro NSIS_HOOK_PREINSTALL
  !insertmacro PLATTER_CLOSE_RUNNING
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  !insertmacro PLATTER_CLOSE_RUNNING
!macroend
