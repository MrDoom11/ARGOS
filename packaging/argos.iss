#ifndef AppVersion
  #error Compile com packaging/build.ps1 para definir a versão do ARGOS.
#endif

#if !defined(SensorInstaller) || !defined(SensorSha256)
  #error Compile com packaging/build.ps1 para incluir o sensor de temperatura.
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
Source: "{#SensorInstaller}"; DestName: "ARGOS-Sensor-Setup.exe"; Flags: dontcopy noencryption
Source: "..\target\x86_64-pc-windows-msvc\release\argos.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\target\x86_64-pc-windows-msvc\release\argos-gui.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "pawnio\AMDFamily17.bin"; DestDir: "{app}"; Flags: ignoreversion
Source: "pawnio\IntelMSR.bin"; DestDir: "{app}"; Flags: ignoreversion
Source: "pawnio\COPYING"; DestDir: "{app}\pawnio"
Source: "pawnio\NOTICE.txt"; DestDir: "{app}\pawnio"
Source: "pawnio\PawnIO.Modules-0.2.11-source.zip"; DestDir: "{app}\pawnio"
Source: "..\LICENSE"; DestDir: "{app}"

[Icons]
Name: "{userprograms}\ARGOS"; Filename: "{app}\argos-gui.exe"; WorkingDir: "{app}"

[Run]
Filename: "{app}\argos-gui.exe"; WorkingDir: "{app}"; Description: "Abrir o painel do ARGOS"; Flags: postinstall nowait skipifsilent

[Code]
const
  ArgosUninstallKey = 'Software\Microsoft\Windows\CurrentVersion\Uninstall\{#ArgosAppId}_is1';
  SensorKey = 'Software\ARGOS\CpuSensor';
  SensorServiceKey = 'SYSTEM\CurrentControlSet\Services\ArgosCpuSensor';
  PawnIoServiceKey = 'SYSTEM\CurrentControlSet\Services\PawnIO';
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

procedure ReportRunError(const Error: String; FromUpdater: Boolean);
begin
  Log(Error);
  if not FromUpdater then
    MsgBox(Error, mbError, MB_OK);
end;

function CachedPortableVersion: String;
var
  ArgosExe: String;
  ResultCode: Integer;
  Output: TExecOutput;
begin
  Result := '';
  try
    ArgosExe := PackageArgos;
    if not ExecAndCaptureOutput(ArgosExe, 'portable-cache-version', ExtractFileDir(ArgosExe),
      SW_SHOWNORMAL, ewWaitUntilTerminated, ResultCode, Output) then
      Exit;
    if (ResultCode <> 0) or Output.Error or (GetArrayLength(Output.StdOut) <> 1) then
      Exit;
    if SafeVersion(Output.StdOut[0]) then
      Result := Output.StdOut[0];
  except
    Log('Não foi possível consultar a versão portátil atualizada. ' + GetExceptionMessage);
  end;
end;

procedure RunWithoutInstalling(FromUpdater: Boolean);
var
  ArgosExe, InstalledExe, Version, CachedVersion, InstalledLabel, Params: String;
  ResultCode, Choice: Integer;
  Output: TExecOutput;
begin
  try
    ArgosExe := '';
    Params := 'gui --portable';
    if FromUpdater then
      Params := 'gui --portable-exact'
    else if not PanelIsOpen and InstalledArgos(InstalledExe, Version) then
    begin
      Choice := IDNO;
      CachedVersion := CachedPortableVersion;
      if CachedVersion <> '' then
        Choice := TaskDialogMsgBox('Uma versão portátil atualizada está disponível',
          'Abra a versão já salva ou escolha entre o pacote aberto e a cópia instalada.', mbInformation,
          MB_YESNOCANCEL, ['&Abrir portátil atualizada (' + CachedVersion + ')',
          '&Escolher outra versão', '&Cancelar'], 0);
      if (Choice <> IDYES) and (Choice <> IDNO) then
        Exit;
      if Choice = IDNO then
      begin
        InstalledLabel := 'Abrir a versão &instalada';
        if SafeVersion(Version) then
          InstalledLabel := InstalledLabel + ' (' + Version + ')';
        case TaskDialogMsgBox('Qual versão do ARGOS você quer executar?',
          'Escolha entre o pacote aberto e a cópia instalada.', mbInformation,
          MB_YESNOCANCEL, ['&Executar a versão deste pacote ({#AppVersion})', InstalledLabel, '&Cancelar'], 0) of
          IDYES: Params := 'gui --portable-exact';
          IDNO:
            begin
              ArgosExe := InstalledExe;
              Params := 'gui';
            end;
        else
          Exit;
        end;
      end;
    end;

    if ArgosExe = '' then
    begin
      ArgosExe := PackageArgos;
      ExtractTemporaryFiles('{app}\*.bin');
      ExtractTemporaryFiles('{app}\pawnio\*');
    end;
    if not ExecAndCaptureOutput(ArgosExe, Params, ExtractFileDir(ArgosExe), SW_SHOWNORMAL,
      ewWaitUntilTerminated, ResultCode, Output) then
      ReportRunError('Falha ao abrir o ARGOS. ' + SysErrorMessage(ResultCode), FromUpdater)
    else if ResultCode <> 0 then
      ReportRunError(ProcessError(Output, 'O ARGOS encerrou com erro (código ' + IntToStr(ResultCode) + ').'),
        FromUpdater);
  except
    ReportRunError('Falha ao preparar o ARGOS. ' + GetExceptionMessage, FromUpdater);
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

procedure ProvisionCpuSensor;
var
  Protocol: Cardinal;
  Code: Integer;
  Component: String;
  Guard: TFileStream;
begin
  if RegQueryDWordValue(HKLM64, SensorKey, 'Protocol', Protocol) and (Protocol = 1) and
    RegKeyExists(HKLM, SensorServiceKey) and RegKeyExists(HKLM, PawnIoServiceKey) and
    FileExists(ExpandConstant('{commonpf64}\ARGOS Sensor\argos-sensor.exe')) then
    Exit;
  if not Exec(ExpandConstant('{app}\argos.exe'), 'cpu-temperature-supported',
    ExpandConstant('{app}'), SW_HIDE, ewWaitUntilTerminated, Code) or (Code <> 0) then
    Exit;

  ExtractTemporaryFile('ARGOS-Sensor-Setup.exe');
  Component := ExpandConstant('{tmp}\ARGOS-Sensor-Setup.exe');
  Guard := TFileStream.Create(Component, fmOpenRead or fmShareDenyWrite);
  try
    if CompareText(GetSHA256OfStream(Guard), '{#SensorSha256}') <> 0 then
      RaiseException('O pacote do sensor de temperatura não passou na validação.');
    if not ShellExec('runas', Component, '/SP- /VERYSILENT /SUPPRESSMSGBOXES /NORESTART',
      ExpandConstant('{sys}'), SW_HIDE, ewWaitUntilTerminated, Code) then
      Log('Sensor de temperatura não provisionado. ' + SysErrorMessage(Code))
    else if Code <> 0 then
      Log('Instalador do sensor de temperatura retornou ' + IntToStr(Code) + '.');
  finally
    Guard.Free;
  end;
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssPostInstall then
  begin
    try
      ProvisionCpuSensor;
    except
      Log('Falha ao preparar o sensor de temperatura. ' + GetExceptionMessage);
    end;
    ReleaseLaunchLock;
  end;
end;

procedure DeinitializeSetup;
begin
  ReleaseLaunchLock;
end;

function RunPortableRequested: Boolean;
var
  I: Integer;
begin
  Result := False;
  for I := 1 to ParamCount do
    if CompareText(ParamStr(I), '/ARGOS-RUN-PORTABLE') = 0 then
    begin
      Result := True;
      Exit;
    end;
end;

function InitializeSetup: Boolean;
begin
  if RunPortableRequested then
  begin
    RunWithoutInstalling(True);
    Result := False;
    Exit;
  end;

  Result := True;
  if WizardSilent then
    Exit;

  case TaskDialogMsgBox('Como você quer usar o ARGOS?',
    'Escolha se deseja abrir o painel agora ou instalar o ARGOS neste computador.',
    mbInformation, MB_YESNOCANCEL, ['&Somente executar', '&Instalar', '&Cancelar'], 0) of
    IDYES:
      begin
        RunWithoutInstalling(False);
        Result := False;
      end;
    IDNO: Result := True;
  else
    Result := False;
  end;
end;
