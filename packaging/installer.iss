; Sky Remote Assistance 安装脚本（Inno Setup 6）
#define MyAppName "3F-Shell"
#define MyAppVersion "1.0.0"
#define MyAppExeName "sky-remote-assistance.exe"

[Setup]
; 注意：AppId 花括号需双重转义
AppId={{8F3B4A2C-1E5D-4B7E-9A6F-2C3D4E5F6071}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppPublisher=Sky Workspace
AppComments=Windows 下对 Linux 的远程 SSH 登录与文件系统可视化操作
DefaultDirName={autopf}\SkyRemoteAssistance
DefaultGroupName={#MyAppName}
DisableProgramGroupPage=yes
ArchitecturesInstallIn64BitMode=x64compatible
ArchitecturesAllowed=x64compatible
OutputDir=..\dist
OutputBaseFilename=3F-Shell-Setup-{#MyAppVersion}
SetupIconFile=app.ico
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
UninstallDisplayIcon={app}\{#MyAppExeName}
; 卸载时保留用户数据（accounts.tsv / transfers.log），避免误删账号与传输记录

[Languages]
Name: "chinesesimp"; MessagesFile: "ChineseSimplified.isl"
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "..\target\release\{#MyAppExeName}"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\target\release\*.dll"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\emoji-icon-font.ttf"; DestDir: "{app}"; Flags: ignoreversion
Source: "app.ico"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; IconFilename: "{app}\app.ico"
Name: "{autodesktop}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; IconFilename: "{app}\app.ico"; Tasks: desktopicon

[Run]
Filename: "{app}\{#MyAppExeName}"; Description: "{cm:LaunchProgram,{#MyAppName}}"; Flags: nowait postinstall skipifsilent
