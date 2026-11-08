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
  PawnIoKey = 'Software\Microsoft\Windows\CurrentVersion\Uninstall\PawnIO';
  PawnIoServiceKey = 'SYSTEM\CurrentControlSet\Services\PawnIO';
  RestartRequired = 3010;

var
  DriverNeedsRestart: Boolean;

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
    Log('Serviço de temperatura retornou ' + IntToStr(Code) + '.');
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssPostInstall then
  begin
    InstallPawnIo;
    if not ManageSensor('--install') then
      RaiseException('Não foi possível registrar o serviço de temperatura do ARGOS.');
    if not RegWriteDWordValue(HKLM64, SensorKey, 'Protocol', 1) then
      RaiseException('Não foi possível registrar a versão do serviço de temperatura.');
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
      RaiseException('Não foi possível remover o serviço de temperatura do ARGOS.');
    RegDeleteValue(HKLM64, SensorKey, 'Protocol');
    RegDeleteKeyIfEmpty(HKLM64, SensorKey);
  end;
end;
