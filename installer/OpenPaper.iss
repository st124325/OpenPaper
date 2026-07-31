#define MyAppName "OpenPaper"
#define MyAppVersion "0.0.2"
#define MyAppPublisher "OpenPaper"
#define MyAppExeName "OpenPaper.exe"

[Setup]
AppId={{EAA5AA56-EE79-4BC5-9F77-8180C64E4370}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppPublisher={#MyAppPublisher}
SetupIconFile=..\assets\OpenPaper.ico
UninstallDisplayIcon={app}\{#MyAppExeName}
DefaultDirName={autopf}\OpenPaper
DefaultGroupName=OpenPaper
OutputDir=..\dist
OutputBaseFilename=OpenPaper-Setup-win-x64
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible

[Files]
Source: "..\dist\OpenPaper-win-x64\*"; DestDir: "{app}"; Flags: recursesubdirs ignoreversion

[Icons]
Name: "{autoprograms}\OpenPaper"; Filename: "{app}\{#MyAppExeName}"
Name: "{autodesktop}\OpenPaper"; Filename: "{app}\{#MyAppExeName}"; Tasks: desktopicon

[Tasks]
Name: "desktopicon"; Description: "Создать ярлык на рабочем столе"; Flags: unchecked

[Run]
Filename: "{app}\{#MyAppExeName}"; Description: "Запустить OpenPaper"; Flags: nowait postinstall skipifsilent
