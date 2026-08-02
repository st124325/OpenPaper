#define MyAppName "OpenPaper"
#define MyAppVersion "0.0.15"
#define MyAppPublisher "OpenPaper"
#define MyAppExeName "OpenPaper.exe"

[Setup]
AppId={{EAA5AA56-EE79-4BC5-9F77-8180C64E4370}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppPublisher={#MyAppPublisher}
SetupIconFile=..\assets\OpenPaper.ico
UninstallDisplayIcon={app}\{#MyAppExeName}
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
DefaultDirName={localappdata}\OpenPaper
UsePreviousAppDir=no
DefaultGroupName=OpenPaper
OutputDir=..\dist
OutputBaseFilename=OpenPaper-Setup-win-x64
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
CloseApplications=yes

[Files]
Source: "..\dist\OpenPaper-win-x64\*"; DestDir: "{app}"; Flags: recursesubdirs ignoreversion

[Icons]
Name: "{autoprograms}\OpenPaper"; Filename: "{app}\{#MyAppExeName}"
Name: "{autodesktop}\OpenPaper"; Filename: "{app}\{#MyAppExeName}"; Tasks: desktopicon

[Tasks]
Name: "desktopicon"; Description: "Create a desktop shortcut"

[Run]
Filename: "{app}\{#MyAppExeName}"; Description: "Launch OpenPaper"; Flags: nowait
