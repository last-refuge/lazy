; Inno Setup script for the Lazy installer. Build with packaging\windows\build-windows.ps1.
; Per-user by default (no administrator needed); the dialog lets an administrator install for all users.

#ifndef Version
  #define Version "0.0.0"
#endif
#ifndef SourceDir
  #error SourceDir must be defined (the folder with lazy.exe and licenses\)
#endif
#ifndef OutDir
  #define OutDir "."
#endif

[Setup]
; The AppId identifies the program for upgrades and uninstalling; never change it.
AppId={{4A1BB6A5-2793-4059-AAB3-4FFCBC0498C4}
AppName=Lazy
AppVersion={#Version}
AppPublisher=Lazy
DefaultDirName={autopf}\Lazy
DefaultGroupName=Lazy
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir={#OutDir}
OutputBaseFilename=Lazy-Setup-{#Version}-windows-x64
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
UninstallDisplayIcon={app}\lazy.exe
CloseApplications=yes
; The new Lazy is started by [Run] below, not by the Restart Manager, so it is never started twice.
RestartApplications=no
#ifdef IconFile
SetupIconFile={#IconFile}
#endif

[Tasks]
Name: "desktopicon"; Description: "Create a &desktop shortcut"; Flags: unchecked

[Files]
Source: "{#SourceDir}\lazy.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\licenses\*"; DestDir: "{app}\licenses"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{autoprograms}\Lazy"; Filename: "{app}\lazy.exe"
Name: "{autodesktop}\Lazy"; Filename: "{app}\lazy.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\lazy.exe"; Description: "Start Lazy"; Flags: nowait postinstall skipifsilent
; An update started from inside Lazy runs this setup silently with /LAZYUPDATE=1 and has no
; "Start Lazy" box to tick, so the new Lazy is started here.
Filename: "{app}\lazy.exe"; Flags: nowait runasoriginaluser skipifdoesntexist; Check: UpdatedFromLazy

[Code]
function UpdatedFromLazy: Boolean;
begin
  Result := ExpandConstant('{param:LAZYUPDATE|0}') = '1';
end;
