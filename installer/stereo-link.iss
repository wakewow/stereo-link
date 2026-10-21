; StereoLink 立体声链 —— Inno Setup 6 安装脚本
;
; 构建：
;   "C:\Program Files (x86)\Inno Setup 6\ISCC.exe" installer\stereo-link.iss
; 产物：
;   dist\stereo-link-setup-<版本>.exe
;
; 前置：先跑 `cargo build --release -p stereo-link`，让 dist\stereo-link.exe 是最新的。
; 说明：
;   * 默认**按用户安装**（%LOCALAPPDATA%\Programs\StereoLink），不需要管理员权限；
;     在安装向导第一页选择「为所有用户安装」会改用 Program Files（需要 UAC）。
;   * 安装包**不含**自研虚拟声卡驱动（自签内核驱动，需要关闭 Secure Boot + 开启
;     testsigning + 重启）。驱动属于可选高级路径，源码与驱动包在仓库的 drivers\ 下，
;     安装方法见 docs\virtual-soundcard.md。
;   * 安装包会把 MIT/Apache 双许可、NOTICE 以及随 exe 编入的四个字体许可一起装到
;     {app} 下（LICENSES\），满足二进制再分发的许可要求。

#define AppName        "StereoLink 立体声链"
#define AppNameShort   "StereoLink"
#define AppVersion     "0.6.0"
#define AppExe         "stereo-link.exe"
#define AppPublisher   "StereoLink contributors"
#define AppURL         "https://github.com/wakewow/stereo-link"

[Setup]
AppId={{9F2B7C41-6E3A-4D58-9B17-3C8E5A0D4F62}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher={#AppPublisher}
AppPublisherURL={#AppURL}
AppSupportURL={#AppURL}/issues
AppUpdatesURL={#AppURL}/releases
VersionInfoVersion={#AppVersion}.0
VersionInfoCompany={#AppPublisher}
VersionInfoDescription={#AppName} 安装程序
VersionInfoProductName={#AppNameShort}
VersionInfoProductVersion={#AppVersion}

DefaultDirName={autopf}\{#AppNameShort}
DefaultGroupName={#AppNameShort}
DisableProgramGroupPage=yes
AllowNoIcons=yes
LicenseFile=license.txt
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog commandline

OutputDir=..\dist
OutputBaseFilename=stereo-link-setup-{#AppVersion}
SetupIconFile=stereo-link.ico
UninstallDisplayIcon={app}\{#AppExe}
UninstallDisplayName={#AppName}

Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0.17763
CloseApplications=yes
RestartApplications=no

[Languages]
Name: "chinese"; MessagesFile: "compiler:Languages\ChineseSimplified.isl"
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "..\dist\{#AppExe}";      DestDir: "{app}";                Flags: ignoreversion
; 手机投放接收端（独立许可：LGPL / AGPL，见 docs\receiver.md 与 LICENSES\）
Source: "..\dist\receivers\*.exe"; DestDir: "{app}\receivers";      Flags: ignoreversion
; 控制台程序：命令行 / 无头 serve / 自动化测试都靠它（见 docs\cli.md）
Source: "..\dist\stereo-link-cli.exe"; DestDir: "{app}";           Flags: ignoreversion
Source: "..\README.md";           DestDir: "{app}";                Flags: ignoreversion
Source: "..\CHANGELOG.md";        DestDir: "{app}";                Flags: ignoreversion
Source: "..\LICENSE-MIT";         DestDir: "{app}";                Flags: ignoreversion
Source: "..\LICENSE-APACHE";      DestDir: "{app}";                Flags: ignoreversion
Source: "..\NOTICE";              DestDir: "{app}";                Flags: ignoreversion
Source: "..\LICENSES\*";          DestDir: "{app}\LICENSES";       Flags: ignoreversion recursesubdirs createallsubdirs
Source: "..\docs\gui.md";         DestDir: "{app}\docs";           Flags: ignoreversion
Source: "..\docs\cli.md";         DestDir: "{app}\docs";           Flags: ignoreversion
Source: "..\docs\virtual-soundcard.md"; DestDir: "{app}\docs";     Flags: ignoreversion
Source: "..\docs\images\*";       DestDir: "{app}\docs\images";    Flags: ignoreversion

[Icons]
Name: "{group}\{#AppName}";                  Filename: "{app}\{#AppExe}"
Name: "{group}\使用手册（面板）";            Filename: "{app}\docs\gui.md"
Name: "{group}\使用手册（命令行与无头部署）"; Filename: "{app}\docs\cli.md"
Name: "{group}\使用手册（采集源与虚拟声卡）"; Filename: "{app}\docs\virtual-soundcard.md"
Name: "{group}\打开安装目录";                Filename: "{app}"
Name: "{group}\{cm:UninstallProgram,{#AppName}}"; Filename: "{uninstallexe}"
Name: "{autodesktop}\{#AppName}";            Filename: "{app}\{#AppExe}"; Tasks: desktopicon

[Run]
Filename: "{app}\{#AppExe}"; Description: "{cm:LaunchProgram,{#AppName}}"; Flags: nowait postinstall skipifsilent

[UninstallDelete]
Type: filesandordirs; Name: "{tmp}\stereo-link"

[Code]
{ 卸载时询问是否连带删除用户配置与日志（%APPDATA%\stereo-link）。 }
procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  Dir: String;
begin
  if CurUninstallStep = usPostUninstall then
  begin
    Dir := ExpandConstant('{userappdata}\stereo-link');
    if DirExists(Dir) then
    begin
      { 静默卸载（/VERYSILENT）不弹窗，保留用户配置。 }
      if (not UninstallSilent) and
         (MsgBox('是否一并删除 StereoLink 的配置与日志？' + #13#10 + #13#10 + Dir,
                 mbConfirmation, MB_YESNO) = IDYES) then
        DelTree(Dir, True, True, True);
    end;
  end;
end;
