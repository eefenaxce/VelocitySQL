; VelocitySQL installer for Windows.
;
; Build it after the release binaries exist:
;   iscc /DAppVersion=0.1.0 /DSourceDir=..\target\release installer\velocitysql.iss
;
; What it does, in the order a user meets it: copies the two binaries under
; Program Files, puts them on the system PATH, gives the server a data directory
; in ProgramData (a service or a task never starts in a directory anyone chose),
; and optionally registers a task that starts the server at boot.

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef SourceDir
  #define SourceDir "..\target\release"
#endif

[Setup]
; The AppId identifies the installation across versions; it never changes.
AppId={{7B3E5C2A-9D41-4E8B-A6F2-1C0D9B7E4A55}
AppName=VelocitySQL
AppVersion={#AppVersion}
AppVerName=VelocitySQL {#AppVersion}
AppPublisher=VelocitySQL
AppPublisherURL=https://github.com/eefenaxce/VelocitySQL
AppSupportURL=https://github.com/eefenaxce/VelocitySQL/issues
DefaultDirName={autopf}\VelocitySQL
DefaultGroupName=VelocitySQL
DisableProgramGroupPage=yes
LicenseFile=..\LICENSE
OutputDir=..\dist
OutputBaseFilename=velocitysql-{#AppVersion}-x86_64-pc-windows-msvc-setup
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
PrivilegesRequired=admin
ArchitecturesAllowed=x64
ArchitecturesInstallIn64BitMode=x64
UninstallDisplayName=VelocitySQL {#AppVersion}

[Files]
Source: "{#SourceDir}\velocitysql-server.exe"; DestDir: "{app}\bin"; Flags: ignoreversion
Source: "{#SourceDir}\velocitysql-cli.exe"; DestDir: "{app}\bin"; Flags: ignoreversion
Source: "..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\testdata\*.sql"; DestDir: "{app}\samples"; Flags: ignoreversion
; Kept next to the binaries so an administrator can re-register the task later
; with a different port or binding.
Source: "register-task.ps1"; DestDir: "{app}\installer"; Flags: ignoreversion

[Dirs]
; The snapshot lives here, and the account the server runs as has to be able to
; write it. `users-modify` also lets the installed console keep a database of its
; own in the same place.
Name: "{commonappdata}\VelocitySQL"; Permissions: users-modify
Name: "{commonappdata}\VelocitySQL\data"; Permissions: users-modify

[Icons]
Name: "{group}\VelocitySQL Console"; Filename: "{app}\bin\velocitysql-cli.exe"

[Registry]
Root: HKLM; Subkey: "SYSTEM\CurrentControlSet\Control\Session Manager\Environment"; \
    ValueType: expandsz; ValueName: "Path"; ValueData: "{olddata};{app}\bin"; \
    Check: NeedsAddPath('{app}\bin')

[Run]
; Starting the server is the user's decision; leaving it stopped means the
; database only exists while a console window does.
Filename: "{app}\bin\velocitysql-cli.exe"; Parameters: "--demo"; \
    Description: "Open the VelocitySQL console (sample database loaded)"; \
    Flags: postinstall nowait skipifsilent unchecked

[UninstallRun]
Filename: "{sys}\schtasks.exe"; Parameters: "/Delete /F /TN ""VelocitySQL"""; \
    Flags: runhidden; RunOnceId: "DeleteTask"

[Code]
var
  ConfigPage: TInputQueryPage;
  OptionsPage: TInputOptionWizardPage;
  ServerPort: Integer;
  AllowRemote: Boolean;
  AutoStart: Boolean;

const
  TaskName = 'VelocitySQL';

function NeedsAddPath(Param: string): Boolean;
var
  OrigPath: string;
begin
  if not RegQueryStringValue(HKLM,
    'SYSTEM\CurrentControlSet\Control\Session Manager\Environment', 'Path', OrigPath) then
  begin
    Result := True;
    exit;
  end;
  Result := Pos(';' + Uppercase(Param) + ';', ';' + Uppercase(OrigPath) + ';') = 0;
end;

procedure RemoveFromPath(Param: string);
var
  Paths: string;
  Position: Integer;
begin
  if not RegQueryStringValue(HKLM,
    'SYSTEM\CurrentControlSet\Control\Session Manager\Environment', 'Path', Paths) then
    exit;
  Position := Pos(';' + Uppercase(Param) + ';', ';' + Uppercase(Paths) + ';');
  if Position = 0 then
    exit;
  Delete(Paths, Position, Length(Param) + 1);
  RegWriteExpandStringValue(HKLM,
    'SYSTEM\CurrentControlSet\Control\Session Manager\Environment', 'Path', Paths);
end;

function SnapshotPath: string;
begin
  Result := ExpandConstant('{commonappdata}\VelocitySQL\data\velocitysql.snapshot');
end;

procedure InitializeWizard;
begin
  ConfigPage := CreateInputQueryPage(wpSelectTasks,
    'Server configuration', 'How the server should listen',
    'The server accepts PostgreSQL clients on this port. The default is the one ' +
    'the documentation uses.');
  ConfigPage.Add('Port number:', False);
  ConfigPage.Values[0] := '5210';

  OptionsPage := CreateInputOptionPage(ConfigPage.ID,
    'Startup and access', 'Who may reach the server, and when',
    'Stopping the server or reaching it from another machine?',
    True, False);
  OptionsPage.Add('Start the server automatically at boot');
  OptionsPage.Add('Accept connections from other machines (changes the binding to 0.0.0.0)');
  OptionsPage.Values[0] := True;
  OptionsPage.Values[1] := False;
end;

function NextButtonClick(CurPageID: Integer): Boolean;
begin
  Result := True;
  if CurPageID = ConfigPage.ID then
  begin
    if not TryStrToInt(ConfigPage.Values[0], ServerPort) or (ServerPort < 1) or
       (ServerPort > 65535) then
    begin
      MsgBox('The port has to be a number between 1 and 65535.', mbError, MB_OK);
      Result := False;
    end;
  end;
end;

// Registers (or re-registers) the boot-time task through PowerShell, which
// takes the executable and its arguments as two separate values: a path with
// spaces then needs no quoting at all, which `schtasks /TR` makes surprisingly
// hard to get right.
//
// A console program cannot be a Windows service. The service control manager
// only talks to a program that implements its protocol and answers anything else
// with error 1053; a task that runs at startup as SYSTEM gives the same "the
// database is up before anyone logs in" result without that protocol.
procedure RegisterTask;
var
  Code: Integer;
  Host: string;
  Parameters: string;
begin
  if AllowRemote then
    Host := '0.0.0.0'
  else
    Host := '127.0.0.1';
  Parameters := Format('-NoProfile -ExecutionPolicy Bypass -File "%s" ' +
    '-Exe "%s" -HostBinding %s -Port %d -Snapshot "%s" -TaskName "%s"',
    [ExpandConstant('{app}\installer\register-task.ps1'),
     ExpandConstant('{app}\bin\velocitysql-server.exe'), Host, ServerPort,
     SnapshotPath, TaskName]);
  if not Exec(ExpandConstant('{sys}\WindowsPowerShell\v1.0\powershell.exe'),
      Parameters, '', SW_HIDE, ewWaitUntilTerminated, Code) or (Code <> 0) then
    MsgBox('The scheduled task could not be registered (PowerShell exited with ' +
      IntToStr(Code) + '). Start the server by hand from ' +
      ExpandConstant('{app}\bin') + '.', mbError, MB_OK);
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssPostInstall then
  begin
    ServerPort := StrToInt(ConfigPage.Values[0]);
    AllowRemote := OptionsPage.Values[1];
    AutoStart := OptionsPage.Values[0];
    if AutoStart then
      RegisterTask;
  end;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usUninstall then
    RemoveFromPath(ExpandConstant('{app}\bin'));
end;
