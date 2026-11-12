; Shrinkr Windows installer (Inno Setup 6+).
;
; Built by build-installer.ps1, which stages the two Shrinkr binaries,
; the bundled ffmpeg/ffprobe sidecars and FFMPEG-NOTES.txt, then passes
; AppVersion / NumericVersion / StagingDir / OutputDir as /D defines.
;
; Inno Setup 6 is free for non-commercial use; commercial distribution
; requires a license from https://jrsoftware.org (see its LICENSE.TXT).
;
; The install is per-user (%LOCALAPPDATA%\Programs\Shrinkr) on purpose:
; Shrinkr updates itself in place by replacing the executables and the
; bundled FFmpeg sidecars in its own folder, which needs a writable
; install directory and no elevation prompt.

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef NumericVersion
  #define NumericVersion "0.0.0.0"
#endif
#ifndef FileVersion
  #define FileVersion AppVersion
#endif
#ifndef StagingDir
  #define StagingDir "staging"
#endif
#ifndef OutputDir
  #define OutputDir "output"
#endif

[Setup]
; Keep this AppId stable across releases: it is the upgrade/uninstall identity.
AppId={{4D8134AB-7963-468F-82AC-FCB570190ED3}
AppName=Shrinkr
AppVersion={#AppVersion}
AppVerName=Shrinkr {#AppVersion}
AppPublisher=Shrinkr
AppPublisherURL=https://github.com/feelvm/Shrinkr
AppSupportURL=https://github.com/feelvm/Shrinkr/issues
AppUpdatesURL=https://github.com/feelvm/Shrinkr/releases
DefaultDirName={autopf}\Shrinkr
DefaultGroupName=Shrinkr
DisableProgramGroupPage=yes
; Per-user install: no UAC, and the in-app self-updater can replace files.
PrivilegesRequired=lowest
OutputDir={#OutputDir}
OutputBaseFilename=Shrinkr-Setup-{#FileVersion}-x64
SetupIconFile=..\..\dioxus-app\assets\icons\icon.ico
UninstallDisplayIcon={app}\shrinkr.exe
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
; WebView2 (required by the Dioxus GUI) ships with Windows 10+.
MinVersion=10.0
CloseApplications=yes
SetupLogging=yes
SetupMutex=ShrinkrSetup
VersionInfoVersion={#NumericVersion}
VersionInfoProductName=Shrinkr
VersionInfoProductVersion={#NumericVersion}
VersionInfoCompany=Shrinkr
VersionInfoDescription=Shrinkr Setup

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked
Name: "addtopath"; Description: "Add Shrinkr to PATH (shrinkr-cli)"; GroupDescription: "Command line:"

[Files]
Source: "{#StagingDir}\shrinkr.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#StagingDir}\shrinkr-cli.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#StagingDir}\ffmpeg.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#StagingDir}\ffprobe.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#StagingDir}\FFMPEG-NOTES.txt"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\Shrinkr"; Filename: "{app}\shrinkr.exe"
Name: "{autodesktop}\Shrinkr"; Filename: "{app}\shrinkr.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\shrinkr.exe"; Description: "{cm:LaunchProgram,Shrinkr}"; Flags: nowait postinstall skipifsilent

[Code]
const
  EnvironmentKey = 'Environment';
  WebView2Url = 'https://go.microsoft.com/fwlink/p/?LinkId=2124703';

function NormalizeDir(const Dir: string): string;
begin
  Result := Dir;
  while (Length(Result) > 1) and (Result[Length(Result)] = '\') do
    Delete(Result, Length(Result), 1);
end;

function PathContainsDir(const PathValue, Dir: string): Boolean;
var
  Rest, Part: string;
  P: Integer;
begin
  Result := False;
  Rest := PathValue + ';';
  while Rest <> '' do
  begin
    P := Pos(';', Rest);
    if P = 0 then
    begin
      Part := Rest;
      Rest := '';
    end
    else
    begin
      Part := Copy(Rest, 1, P - 1);
      Delete(Rest, 1, P);
    end;
    if (Part <> '') and (CompareText(NormalizeDir(Part), NormalizeDir(Dir)) = 0) then
    begin
      Result := True;
      Exit;
    end;
  end;
end;

function NeedsAddPath(const Dir: string): Boolean;
var
  PathValue: string;
begin
  if not RegQueryStringValue(HKCU, EnvironmentKey, 'Path', PathValue) then
    Result := True
  else
    Result := not PathContainsDir(PathValue, Dir);
end;

procedure AddToPath(const Dir: string);
var
  PathValue: string;
begin
  if not NeedsAddPath(Dir) then
    Exit;
  if RegQueryStringValue(HKCU, EnvironmentKey, 'Path', PathValue) and (PathValue <> '') then
    PathValue := PathValue + ';' + Dir
  else
    PathValue := Dir;
  RegWriteExpandStringValue(HKCU, EnvironmentKey, 'Path', PathValue);
end;

procedure RemoveFromPath(const Dir: string);
var
  PathValue, NewValue, Rest, Part: string;
  P: Integer;
begin
  if not RegQueryStringValue(HKCU, EnvironmentKey, 'Path', PathValue) then
    Exit;
  NewValue := '';
  Rest := PathValue + ';';
  while Rest <> '' do
  begin
    P := Pos(';', Rest);
    if P = 0 then
    begin
      Part := Rest;
      Rest := '';
    end
    else
    begin
      Part := Copy(Rest, 1, P - 1);
      Delete(Rest, 1, P);
    end;
    if (Part <> '') and (CompareText(NormalizeDir(Part), NormalizeDir(Dir)) <> 0) then
    begin
      if NewValue <> '' then
        NewValue := NewValue + ';';
      NewValue := NewValue + Part;
    end;
  end;
  if NewValue <> PathValue then
  begin
    if NewValue = '' then
      RegDeleteValue(HKCU, EnvironmentKey, 'Path')
    else
      RegWriteExpandStringValue(HKCU, EnvironmentKey, 'Path', NewValue);
  end;
end;

function WebView2Installed: Boolean;
begin
  Result :=
    DirExists(ExpandConstant('{commonpf}\Microsoft\EdgeWebView\Application')) or
    DirExists(ExpandConstant('{commonpf32}\Microsoft\EdgeWebView\Application')) or
    DirExists(ExpandConstant('{localappdata}\Microsoft\EdgeWebView\Application'));
end;

function InitializeSetup: Boolean;
var
  ErrorCode: Integer;
begin
  Result := True;
  if (not WebView2Installed) and (not WizardSilent) then
  begin
    if MsgBox('Shrinkr''s interface needs the Microsoft Edge WebView2 runtime, which was not found on this PC.'
              + #13#10 + #13#10
              + 'The install can continue, but Shrinkr will not start until WebView2 is installed.'
              + #13#10 + #13#10
              + 'Open the Microsoft WebView2 download page now?',
              mbConfirmation, MB_YESNO) = IDYES then
      ShellExec('open', WebView2Url, '', '', SW_SHOWNORMAL, ewNoWait, ErrorCode);
  end;
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if (CurStep = ssPostInstall) and WizardIsTaskSelected('addtopath') then
    AddToPath(ExpandConstant('{app}'));
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usPostUninstall then
    RemoveFromPath(ExpandConstant('{app}'));
end;
