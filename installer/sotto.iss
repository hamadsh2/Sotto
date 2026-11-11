; SPDX-License-Identifier: GPL-3.0-only
; Sotto installer (Inno Setup 6). Built by GitHub Actions on a version tag:
;   ISCC.exe /DAppVersion=0.1.0 /DBinDir=<dir with sotto.exe> /DIconFile=<sotto.ico> installer\sotto.iss
;
; - Per-user install in %LOCALAPPDATA%\Programs\Sotto: no admin rights, and the in-app
;   updater can replace sotto.exe there without admin rights too.
; - Start menu entry, optional desktop icon, clean uninstall.
; - Uninstall removes Sotto's "Start with Windows" entry but keeps your settings in
;   %APPDATA%\Sotto, so reinstalling brings them back.
; - VB-Cable is never bundled; Sotto shows the official link if it's missing.

#ifndef AppVersion
  #error AppVersion is required (/DAppVersion=x.y.z)
#endif
#ifndef BinDir
  #error BinDir is required (/DBinDir=...)
#endif
#ifndef IconFile
  #error IconFile is required (/DIconFile=...)
#endif
#define RepoDir AddBackslash(SourcePath) + ".."

[Setup]
; Never change AppId: Windows uses it to recognise upgrades and the uninstall entry.
AppId={{F62C2347-9DBC-47BC-812D-271B6AC542FD}
AppName=Sotto
AppVersion={#AppVersion}
AppVerName=Sotto {#AppVersion}
AppPublisher=hamadsh2
AppPublisherURL=https://github.com/hamadsh2/Sotto
AppSupportURL=https://github.com/hamadsh2/Sotto/issues
AppUpdatesURL=https://github.com/hamadsh2/Sotto/releases
AppCopyright=Sotto contributors, GPL-3.0-only
PrivilegesRequired=lowest
DefaultDirName={userpf}\Sotto
DisableDirPage=auto
DefaultGroupName=Sotto
DisableProgramGroupPage=yes
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
OutputBaseFilename=Sotto-{#AppVersion}-setup
OutputDir={#RepoDir}\dist
SetupIconFile={#IconFile}
UninstallDisplayIcon={app}\sotto.exe
UninstallDisplayName=Sotto
VersionInfoVersion={#AppVersion}
VersionInfoProductName=Sotto
VersionInfoDescription=Sotto installer
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
; Sotto is closed by the [Code] below before files are replaced.
CloseApplications=no

[Tasks]
Name: "desktopicon"; Description: "Create a desktop icon"; GroupDescription: "Shortcuts:"; Flags: unchecked

[Files]
Source: "{#BinDir}\sotto.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#RepoDir}\LICENSE"; DestDir: "{app}"; DestName: "LICENSE.txt"; Flags: ignoreversion
Source: "{#RepoDir}\assets\THIRD_PARTY_LICENSES.txt"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\Sotto"; Filename: "{app}\sotto.exe"
Name: "{userdesktop}\Sotto"; Filename: "{app}\sotto.exe"; Tasks: desktopicon

[Registry]
; Only removes Sotto's own "Start with Windows" value on uninstall; creates nothing.
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: none; ValueName: "Sotto"; Flags: uninsdeletevalue

[Run]
Filename: "{app}\sotto.exe"; Description: "Start Sotto"; Flags: nowait postinstall skipifsilent

[Code]
// Sotto keeps running in the tray when its window is closed, so stop it before
// replacing or removing its files. Its settings are saved half a second after each
// change, so nothing is lost. Only sotto.exe is affected.
procedure StopSotto();
var
  Code: Integer;
begin
  Exec(ExpandConstant('{sys}\taskkill.exe'), '/IM sotto.exe /F', '', SW_HIDE,
    ewWaitUntilTerminated, Code);
  Sleep(500);
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  StopSotto();
  Result := '';
end;

function InitializeUninstall(): Boolean;
begin
  StopSotto();
  Result := True;
end;
