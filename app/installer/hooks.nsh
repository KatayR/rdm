; RDM's own steps in the setup (bundle > windows > nsis > installerHooks in
; tauri.bundle.json). The install steps run in a first install and in every update; the
; uninstall steps do not run in an update, which copies the new files over the old ones.

; Ends the programs of RDM's folder. The setup itself closes only the app; the command
; line, the browser's helper and ffmpeg are in the same folder, and a running program
; cannot be replaced or deleted. The app goes first: it is what starts ffmpeg.
!macro RdmEndPrograms
  !insertmacro CheckIfAppIsRunning "$INSTDIR\${MAINBINARYNAME}.exe" "${PRODUCTNAME}"
  !insertmacro CheckIfAppIsRunning "$INSTDIR\rdm-host.exe" "${PRODUCTNAME} (the browsers' helper)"
  !insertmacro CheckIfAppIsRunning "$INSTDIR\rdm.exe" "${PRODUCTNAME} (the command line)"
  !insertmacro CheckIfAppIsRunning "$INSTDIR\ffmpeg.exe" "${PRODUCTNAME} (ffmpeg)"
!macroend

!macro NSIS_HOOK_PREINSTALL
  ; The browsers start the helper for every download, and the helper starts the app: in the
  ; middle of the copying that would hold a file fast, or bring the old app back. So the
  ; browsers are first made to forget the helper (the keys `rdm browser install` writes,
  ; cli/src/browser.rs; a test there keeps the two in step). The step after the copying
  ; tells them again.
  DeleteRegKey HKCU "Software\Google\Chrome\NativeMessagingHosts\com.rdm.host"
  DeleteRegKey HKCU "Software\BraveSoftware\Brave-Browser\NativeMessagingHosts\com.rdm.host"
  !insertmacro RdmEndPrograms

  ; A file an earlier version installed and this one no longer has is deleted here, by
  ; name (the setup removes nothing by itself). There is none yet.
!macroend

!macro NSIS_HOOK_POSTINSTALL
  ; The browsers are told where this version's helper is, and the extension's files are
  ; written from it, so neither is left from the version before.
  nsExec::Exec '"$INSTDIR\rdm.exe" browser install'
  Pop $0
  ; Compared as text: what is popped is "error" when the program could not be run.
  ${If} $0 != 0
    DetailPrint "rdm browser install: $0"
    ${If} $UpdateMode <> 1
    ${AndIf} $PassiveMode <> 1
    ${AndIfNot} ${Silent}
      MessageBox MB_OK|MB_ICONEXCLAMATION "RDM is installed, but the browsers could not be told about it ($0). Downloads will not be taken from the browser until this works: run $\"$INSTDIR\rdm.exe$\" browser install to see why."
    ${EndIf}
  ${EndIf}
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  ; First what can still be refused (a program that does not end): were the browsers told
  ; to forget the helper before that, RDM would stay installed without them.
  !insertmacro RdmEndPrograms
  ${If} $UpdateMode <> 1
    ; While rdm.exe is still there: the browsers' registration goes with RDM.
    nsExec::Exec '"$INSTDIR\rdm.exe" browser uninstall'
    Pop $0
  ${EndIf}
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
  ; "Delete the application data": the setup's own step looks in a folder named after the
  ; identifier, where only the windows' data is. The list, the tools and the extension's
  ; files are in RDM's data folder.
  ${If} $DeleteAppDataCheckboxState = 1
  ${AndIf} $UpdateMode <> 1
    RMDir /r "$APPDATA\rdm"
  ${EndIf}
!macroend
