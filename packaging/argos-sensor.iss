#if !defined(AppVersion) || !defined(PawnIoInstaller) || !defined(PawnIoSha256)
  #error Compile com packaging/build.ps1 para incluir o PawnIO oficial.
#endif

[Setup]
AppId={{A8EBDE80-E414-4703-A0FB-F4AF76C2B477}
AppName=ARGOS Sensor de CPU
AppVersion={#AppVersion}
DefaultDirName={commonpf64}\ARGOS Sensor
UsePreviousAppDir=no
DisableDirPage=yes
DisableProgramGroupPage=yes
PrivilegesRequired=admin
CloseApplications=no
MinVersion=10.0.22000
ArchitecturesAllowed=x64os
ArchitecturesInstallIn64BitMode=x64os
SetupMutex=Global\ArgosCpuSensorSetup
OutputBaseFilename=ARGOS-Sensor-Setup

[Languages]
Name: "ptbr"; MessagesFile: "compiler:Languages\BrazilianPortuguese.isl"

[Files]
Source: "{#PawnIoInstaller}"; DestDir: "{commonpf64}\ARGOS Sensor"; DestName: "PawnIO_setup.exe"; Flags: deleteafterinstall; Check: NeedPawnIo
Source: "..\target\x86_64-pc-windows-msvc\release\argos-sensor.exe"; DestDir: "{commonpf64}\ARGOS Sensor"; Flags: ignoreversion
Source: "pawnio\AMDFamily17.bin"; DestDir: "{commonpf64}\ARGOS Sensor"; Flags: ignoreversion
Source: "pawnio\IntelMSR.bin"; DestDir: "{commonpf64}\ARGOS Sensor"; Flags: ignoreversion
Source: "pawnio\COPYING"; DestDir: "{commonpf64}\ARGOS Sensor\pawnio"
Source: "pawnio\NOTICE.txt"; DestDir: "{commonpf64}\ARGOS Sensor\pawnio"
Source: "pawnio\PawnIO.Modules-0.2.11-source.zip"; DestDir: "{commonpf64}\ARGOS Sensor\pawnio"

[Code]
const
  SensorKey = 'Software\ARGOS\CpuSensor';
  SensorServiceName = 'ArgosCpuSensor';
  SensorServiceKey = 'SYSTEM\CurrentControlSet\Services\ArgosCpuSensor';
  MetricsRevision = 2;
  PawnIoKey = 'Software\Microsoft\Windows\CurrentVersion\Uninstall\PawnIO';
  PawnIoServiceKey = 'SYSTEM\CurrentControlSet\Services\PawnIO';
  RestartRequired = 3010;
  ScManagerConnect = $0001;
  ServiceQueryStatus = $0004;
  ServiceStop = $0020;
  ServiceControlStop = 1;
  ServiceStopped = 1;
  ServiceStartPending = 2;
  ServiceStopPending = 3;
  ErrorServiceCannotAcceptControl = 1061;
  ErrorServiceNotActive = 1062;

type
  TSensorServiceStatus = record
    ServiceType, CurrentState, ControlsAccepted, Win32ExitCode: DWORD;
    ServiceSpecificExitCode, CheckPoint, WaitHint: DWORD;
  end;

var
  DriverNeedsRestart: Boolean;

function OpenSCManager(MachineName, DatabaseName: UINT_PTR; Access: DWORD): THandle;
  external 'OpenSCManagerW@advapi32.dll stdcall setuponly';

function OpenService(Manager: THandle; const ServiceName: String; Access: DWORD): THandle;
  external 'OpenServiceW@advapi32.dll stdcall setuponly';

function CloseServiceHandle(Handle: THandle): BOOL;
  external 'CloseServiceHandle@advapi32.dll stdcall setuponly';

function QueryServiceStatus(Service: THandle; var Status: TSensorServiceStatus): BOOL;
  external 'QueryServiceStatus@advapi32.dll stdcall setuponly';

function ControlService(Service: THandle; Control: DWORD; var Status: TSensorServiceStatus): BOOL;
  external 'ControlService@advapi32.dll stdcall setuponly';

function SensorFileReady(const FileName: String): Boolean;
var
  Guard: TFileStream;
begin
  Result := True;
  if not FileExists(FileName) then
    Exit;
  try
    Guard := TFileStream.Create(FileName, fmOpenReadWrite or fmShareExclusive);
    Guard.Free;
  except
    Result := False;
  end;
end;

procedure StopInstalledSensor;
var
  ImagePath, SensorFile: String;
  Manager, Service: THandle;
  Status: TSensorServiceStatus;
  Attempt, Error: Integer;
begin
  if not RegKeyExists(HKLM, SensorServiceKey) then
    Exit;
  SensorFile := ExpandConstant('{commonpf64}\ARGOS Sensor\argos-sensor.exe');
  if not RegQueryStringValue(HKLM, SensorServiceKey, 'ImagePath', ImagePath) or
    (CompareText(ImagePath, '"' + SensorFile + '"') <> 0) then
    RaiseException('O serviço de sensores da CPU está configurado com um executável inesperado.');
  Manager := OpenSCManager(0, 0, ScManagerConnect);
  if Manager = 0 then
    RaiseException(SysErrorMessage(DLLGetLastError));
  try
    Service := OpenService(Manager, SensorServiceName, ServiceQueryStatus or ServiceStop);
    if Service = 0 then
      RaiseException(SysErrorMessage(DLLGetLastError));
    try
      for Attempt := 1 to 50 do
      begin
        if not QueryServiceStatus(Service, Status) then
          RaiseException(SysErrorMessage(DLLGetLastError));
        if Status.CurrentState = ServiceStopped then
        begin
          if SensorFileReady(SensorFile) then
            Exit;
        end
        else if (Status.CurrentState <> ServiceStartPending) and
          (Status.CurrentState <> ServiceStopPending) then
        begin
          if not ControlService(Service, ServiceControlStop, Status) then
          begin
            Error := DLLGetLastError;
            if (Error <> ErrorServiceNotActive) and (Error <> ErrorServiceCannotAcceptControl) then
              RaiseException(SysErrorMessage(Error));
          end;
        end;
        Sleep(100);
      end;
      RaiseException('O serviço de sensores da CPU não liberou o executável a tempo.');
    finally
      CloseServiceHandle(Service);
    end;
  finally
    CloseServiceHandle(Manager);
  end;
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  Result := '';
  try
    StopInstalledSensor;
  except
    Result := 'Não foi possível preparar a atualização dos sensores da CPU. ' + GetExceptionMessage;
  end;
end;

function PawnIoReady: Boolean;
var
  Major, Minor: Cardinal;
begin
  Result := RegQueryDWordValue(HKLM64, PawnIoKey, 'VersionMajor', Major) and
    RegQueryDWordValue(HKLM64, PawnIoKey, 'VersionMinor', Minor) and
    ((Major > 2) or ((Major = 2) and (Minor >= 2))) and
    RegKeyExists(HKLM, PawnIoServiceKey) and
    FileExists(ExpandConstant('{commonpf64}\PawnIO\PawnIOLib.dll'));
end;

function NeedPawnIo: Boolean;
begin
  Result := not PawnIoReady;
end;

procedure InstallPawnIo;
var
  Installer: String;
  Code: Integer;
begin
  if PawnIoReady then
    Exit;
  Installer := ExpandConstant('{commonpf64}\ARGOS Sensor\PawnIO_setup.exe');
  if CompareText(GetSHA256OfFile(Installer), '{#PawnIoSha256}') <> 0 then
    RaiseException('O instalador do PawnIO não passou na validação.');
  if not Exec(Installer, '-install -silent', ExpandConstant('{sys}'), SW_HIDE,
    ewWaitUntilTerminated, Code) then
    RaiseException('Não foi possível iniciar o PawnIO. ' + SysErrorMessage(Code))
  else if Code = RestartRequired then
    DriverNeedsRestart := True
  else if Code <> 0 then
    RaiseException('A instalação do PawnIO retornou ' + IntToStr(Code) + '.');
end;

function ManageSensor(const Command: String): Boolean;
var
  Dir: String;
  Code: Integer;
begin
  Dir := ExpandConstant('{commonpf64}\ARGOS Sensor');
  Result := Exec(AddBackslash(Dir) + 'argos-sensor.exe', Command, Dir, SW_HIDE,
    ewWaitUntilTerminated, Code) and (Code = 0);
  if not Result then
    Log('Serviço de sensores da CPU retornou ' + IntToStr(Code) + '.');
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssPostInstall then
  begin
    InstallPawnIo;
    if not ManageSensor('--install') then
      RaiseException('Não foi possível registrar o serviço de sensores da CPU.');
    if not RegWriteDWordValue(HKLM64, SensorKey, 'Protocol', 1) then
      RaiseException('Não foi possível registrar o protocolo do serviço de sensores da CPU.');
    if not RegWriteDWordValue(HKLM64, SensorKey, 'MetricsRevision', MetricsRevision) then
      RaiseException('Não foi possível registrar a revisão dos sensores da CPU.');
  end;
end;

function GetCustomSetupExitCode: Integer;
begin
  if DriverNeedsRestart then
    Result := RestartRequired
  else
    Result := 0;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usUninstall then
  begin
    if not ManageSensor('--uninstall') then
      RaiseException('Não foi possível remover o serviço de sensores da CPU.');
    RegDeleteValue(HKLM64, SensorKey, 'Protocol');
    RegDeleteValue(HKLM64, SensorKey, 'MetricsRevision');
    RegDeleteKeyIfEmpty(HKLM64, SensorKey);
  end;
end;
