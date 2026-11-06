; Inno Setup script for the Windows tray client. Built by
; .github/workflows/windows.yml:
;   iscc installer\delegation.iss /DAppVersion=0.1.0.123
; Per-user install: no admin prompt, nothing outside the user's profile.

#ifndef AppVersion
  #define AppVersion "0.0.0.0"
#endif
#define AppName "Delegation"
#define AppExe "delegation-tray.exe"

[Setup]
; Keep this GUID fixed: it's how upgrades find the existing install.
AppId={{6C1D7E0B-2F4A-4C39-9B8E-5D3A1F27C840}
AppName={#AppName}
AppVersion={#AppVersion}
AppPublisher=Delegation
VersionInfoVersion={#AppVersion}
PrivilegesRequired=lowest
DefaultDirName={localappdata}\Programs\{#AppName}
DisableProgramGroupPage=yes
DisableDirPage=yes
OutputDir=Output
OutputBaseFilename=DelegationTray-Setup-{#AppVersion}
SetupIconFile=..\client\assets\delegation.ico
UninstallDisplayIcon={app}\{#AppExe}
WizardStyle=modern
Compression=lzma2
SolidCompression=yes
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
; Shut a running tray down before replacing its exe, and don't restart it
; afterwards (the "Launch" checkbox does that).
CloseApplications=force
RestartApplications=no

[Files]
Source: "..\target\release\{#AppExe}"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{userprograms}\{#AppName}"; Filename: "{app}\{#AppExe}"

[Registry]
; Start at login. Removed again on uninstall.
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; \
  ValueName: "{#AppName}"; ValueData: """{app}\{#AppExe}"""; Flags: uninsdeletevalue

[Run]
Filename: "{app}\{#AppExe}"; Description: "Launch {#AppName} now"; Flags: nowait postinstall skipifsilent

[UninstallRun]
; Stop the tray so its exe can be deleted.
Filename: "{sys}\taskkill.exe"; Parameters: "/F /IM {#AppExe}"; Flags: runhidden; RunOnceId: "StopTray"

; %APPDATA%\delegation-client is deliberately left in place on uninstall:
; it holds this machine's private keys for each server. Delete it by hand
; to forget all pairings.
