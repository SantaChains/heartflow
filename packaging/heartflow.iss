; heartflow (hf) Windows amd64 安装包 —— Inno Setup 6
; CI 调用约定(release.yml windows-installer job,启用后自动执行):
;   ISCC.exe /DVersion=<x.y.z> packaging\heartflow.iss
; 产物:packaging\dist\heartflow-<x.y.z>-win-amd64-setup.exe
; 正文内相对路径以本脚本所在目录(packaging\)为基准

#ifndef Version
  #define Version "0.0.0"
#endif

[Setup]
AppId={{4E1D2A7C-9B3F-4D8E-A6C5-2F0B7E9D1A38}
AppName=heartflow
AppVersion={#Version}
AppVerName=heartflow {#Version}
AppPublisher=heartflow
DefaultDirName={autopf}\heartflow
DisableProgramGroupPage=yes
DisableWelcomePage=no
LicenseFile=..\LICENSE
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
; 默认 per-user(免管理员);对话框允许用户选择 per-machine
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
OutputDir=dist
OutputBaseFilename=heartflow-{#Version}-win-amd64-setup
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
UninstallDisplayIcon={app}\hf.exe

[Files]
Source: "..\target\x86_64-pc-windows-msvc\release\hf.exe"; DestDir: "{app}"; Flags: ignoreversion

[Registry]
; per-machine 安装写系统 PATH,per-user 安装写用户 PATH;仅当未包含时追加
Root: HKLM; Subkey: "SYSTEM\CurrentControlSet\Control\Session Manager\Environment"; \
  ValueType: expandsz; ValueName: "Path"; ValueData: "{olddata};{app}"; \
  Check: IsAdminInstallMode and NeedsAddPath(HKLM, "SYSTEM\CurrentControlSet\Control\Session Manager\Environment")
Root: HKCU; Subkey: "Environment"; \
  ValueType: expandsz; ValueName: "Path"; ValueData: "{olddata};{app}"; \
  Check: (not IsAdminInstallMode) and NeedsAddPath(HKCU, "Environment")

[Code]
function NeedsAddPath(Root: Integer; Subkey: string): Boolean;
var
  OrigPath: string;
begin
  if not RegQueryStringValue(Root, Subkey, 'Path', OrigPath) then
    OrigPath := '';
  Result := Pos(Lowercase(ExpandConstant('{app}')), Lowercase(OrigPath)) = 0;
end;
