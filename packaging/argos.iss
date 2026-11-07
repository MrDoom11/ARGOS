#ifndef AppVersion
  #error Compile com packaging/build.ps1 para definir a versão do ARGOS.
#endif

#define ArgosAppId "{618CF44B-59D7-424F-89F7-6B2714DF1BD8}"

[Setup]
AppId={{#ArgosAppId}
AppName=ARGOS
AppVersion={#AppVersion}
DefaultDirName={userpf}\ARGOS
DisableProgramGroupPage=yes
UninstallDisplayIcon={app}\argos.exe
PrivilegesRequired=lowest
CloseApplications=no
MinVersion=10.0.22000
ArchitecturesAllowed=x64os
ArchitecturesInstallIn64BitMode=x64os
WizardStyle=modern
OutputDir=..\target\dist
OutputBaseFilename=ARGOS-Setup

[Languages]
Name: "ptbr"; MessagesFile: "compiler:Languages\BrazilianPortuguese.isl"

[Files]
Source: "..\target\x86_64-pc-windows-msvc\release\argos.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\target\x86_64-pc-windows-msvc\release\argos-gui.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\LICENSE"; DestDir: "{app}"

[Icons]
Name: "{userprograms}\ARGOS"; Filename: "{app}\argos-gui.exe"; WorkingDir: "{app}"

[Run]
Filename: "{app}\argos-gui.exe"; WorkingDir: "{app}"; Description: "Abrir o painel do ARGOS"; Flags: postinstall nowait skipifsilent

[Code]
const
  ArgosUninstallKey = 'Software\Microsoft\Windows\CurrentVersion\Uninstall\{#ArgosAppId}_is1';
  AccessReadWrite = $C0000000;
  OpenExisting = 3;
  OpenAlways = 4;
  FileAttributeNormal = $80;
  ErrorSharingViolation = 32;
  ErrorLockViolation = 33;

var
  LaunchLock: THandle;

function CreateFile(const FileName: String; Access, ShareMode: DWORD;
  SecurityAttributes: UINT_PTR; CreationDisposition, FlagsAndAttributes: DWORD;
  TemplateFile: THandle): THandle;
  external 'CreateFileW@kernel32.dll stdcall setuponly';

function CloseHandle(Handle: THandle): BOOL;
  external 'CloseHandle@kernel32.dll stdcall setuponly';

function ArgosStateDir: String;
begin
  Result := GetEnv('LOCALAPPDATA');
  if Result <> '' then
    Result := AddBackslash(Result) + 'ARGOS';
end;

function PackageArgos: String;
begin
  Result := ExpandConstant('{tmp}\') + '{app}\argos.exe';
  if not FileExists(Result) then
    ExtractTemporaryFiles('{app}\argos.exe');
end;

function SafeVersion(const Version: String): Boolean;
var
  I: Integer;
begin
  Result := False;
  if Version = '' then
    Exit;
  for I := 1 to Length(Version) do
    if Pos(Version[I], '0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz.-+') = 0 then
      Exit;
  Result := True;
end;

function InstalledArgos(var ArgosExe, Version: String): Boolean;
var
  Location: String;
begin
  Result := False;
  if not RegQueryStringValue(HKCU64, ArgosUninstallKey, 'InstallLocation', Location) or
    (Location = '') then
    Exit;
  ArgosExe := AddBackslash(Location) + 'argos.exe';
  Result := FileExists(ArgosExe);
  Version := '';
  if Result then
    RegQueryStringValue(HKCU64, ArgosUninstallKey, 'DisplayVersion', Version);
end;

function PanelIsOpen: Boolean;
var
  Dir: String;
  Handle: THandle;
  ErrorCode: Integer;
begin
  Result := FindWindowByClassName('ArgosMonitorWindow') <> 0;
  Dir := ArgosStateDir;
  if Result or (Dir = '') then
    Exit;
  Handle := CreateFile(AddBackslash(Dir) + 'panel.lock', AccessReadWrite, 0, 0,
    OpenExisting, FileAttributeNormal, 0);
  if Handle <> THandle(-1) then
    CloseHandle(Handle)
  else
  begin
    ErrorCode := DLLGetLastError;
    Result := (ErrorCode = ErrorSharingViolation) or (ErrorCode = ErrorLockViolation);
  end;
end;

function ProcessError(const Output: TExecOutput; const Fallback: String): String;
begin
  Result := Fallback;
  if GetArrayLength(Output.StdErr) > 0 then
    if Trim(Output.StdErr[0]) <> '' then
      Result := Output.StdErr[0];
end;

procedure RunWithoutInstalling;
var
  ArgosExe, InstalledExe, Version, InstalledLabel: String;
  ResultCode: Integer;
  Output: TExecOutput;
begin
  try
    ArgosExe := '';
    if not PanelIsOpen and InstalledArgos(InstalledExe, Version) then
    begin
      InstalledLabel := 'Abrir a versão &instalada';
      if SafeVersion(Version) then
        InstalledLabel := InstalledLabel + ' (' + Version + ')';
      case TaskDialogMsgBox('Qual versão do ARGOS você quer executar?',
        'Escolha entre o pacote aberto e a cópia instalada.', mbInformation,
        MB_YESNOCANCEL, ['&Executar a versão deste pacote ({#AppVersion})', InstalledLabel, '&Cancelar'], 0) of
        IDYES: ArgosExe := '';
        IDNO: ArgosExe := InstalledExe;
      else
        Exit;
      end;
    end;

    if ArgosExe = '' then
      ArgosExe := PackageArgos;
    if not ExecAndCaptureOutput(ArgosExe, 'gui', ExtractFileDir(ArgosExe), SW_SHOWNORMAL,
      ewWaitUntilTerminated, ResultCode, Output) then
      MsgBox('Falha ao abrir o ARGOS. ' + SysErrorMessage(ResultCode), mbError, MB_OK)
    else if ResultCode <> 0 then
      MsgBox(ProcessError(Output, 'O ARGOS encerrou com erro (código ' + IntToStr(ResultCode) + ').'),
        mbError, MB_OK);
  except
    MsgBox('Falha ao preparar o ARGOS. ' + GetExceptionMessage, mbError, MB_OK);
  end;
end;

function AcquireLaunchLock: String;
var
  Dir: String;
  ErrorCode: Integer;
begin
  Result := '';
  if LaunchLock <> 0 then
    Exit;
  Dir := ArgosStateDir;
  if Dir = '' then
    Result := 'O perfil do Windows não está disponível.'
  else if not ForceDirectories(Dir) then
    Result := 'Não foi possível preparar a pasta do ARGOS no perfil do Windows.'
  else
  begin
    LaunchLock := CreateFile(AddBackslash(Dir) + 'launch.lock', AccessReadWrite, 0, 0,
      OpenAlways, FileAttributeNormal, 0);
    if LaunchLock = THandle(-1) then
    begin
      ErrorCode := DLLGetLastError;
      LaunchLock := 0;
      if (ErrorCode = ErrorSharingViolation) or (ErrorCode = ErrorLockViolation) then
        Result := 'O ARGOS está abrindo ou já existe uma instalação em andamento. Tente novamente.'
      else
        Result := 'Não foi possível preparar a instalação. ' + SysErrorMessage(ErrorCode);
    end;
  end;
end;

procedure ReleaseLaunchLock;
begin
  if LaunchLock <> 0 then
  begin
    CloseHandle(LaunchLock);
    LaunchLock := 0;
  end;
end;

function CheckInstallation: String;
var
  ArgosExe, Params, Version: String;
  ResultCode: Integer;
  Output: TExecOutput;
begin
  Params := 'prepare-install';
  if WizardSilent then
    Params := Params + ' --silent';
  if RegKeyExists(HKCU64, ArgosUninstallKey) then
  begin
    if not RegQueryStringValue(HKCU64, ArgosUninstallKey, 'DisplayVersion', Version) or
      not SafeVersion(Version) then
    begin
      Result := 'Não foi possível identificar a versão instalada do ARGOS.';
      Exit;
    end;
    Params := Params + ' --installed-version ' + Version;
  end;

  ArgosExe := PackageArgos;
  if not ExecAndCaptureOutput(ArgosExe, Params, ExtractFileDir(ArgosExe), SW_SHOWNORMAL,
    ewWaitUntilTerminated, ResultCode, Output) then
  begin
    Result := 'Não foi possível verificar se o ARGOS pode ser instalado. ' + SysErrorMessage(ResultCode);
    Exit;
  end;
  case ResultCode of
    0: Result := '';
    2: Result := 'A instalação foi interrompida porque o painel do ARGOS está aberto.';
    3: Result := 'Uma versão mais recente do ARGOS já está instalada. ' +
      'Use Somente executar para abrir a versão deste pacote.';
  else
    Result := ProcessError(Output, 'Não foi possível verificar se o ARGOS pode ser instalado.');
  end;
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  try
    Result := AcquireLaunchLock;
    if Result = '' then
      Result := CheckInstallation;
  except
    Result := 'Falha ao preparar a instalação do ARGOS. ' + GetExceptionMessage;
  end;
  if Result <> '' then
    ReleaseLaunchLock;
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssPostInstall then
    ReleaseLaunchLock;
end;

procedure DeinitializeSetup;
begin
  ReleaseLaunchLock;
end;

function InitializeSetup: Boolean;
begin
  Result := True;
  if WizardSilent then
    Exit;

  case TaskDialogMsgBox('Como você quer usar o ARGOS?',
    'Escolha se deseja abrir o painel agora ou instalar o ARGOS neste computador.',
    mbInformation, MB_YESNOCANCEL, ['&Somente executar', '&Instalar', '&Cancelar'], 0) of
    IDYES:
      begin
        RunWithoutInstalling;
        Result := False;
      end;
    IDNO: Result := True;
  else
    Result := False;
  end;
end;
