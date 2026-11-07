#ifndef AppVersion
  #error Compile com packaging/build.ps1 para definir a versão do ARGOS.
#endif

[Setup]
AppId={{618CF44B-59D7-424F-89F7-6B2714DF1BD8}
AppName=ARGOS
AppVersion={#AppVersion}
DefaultDirName={userpf}\ARGOS
DisableProgramGroupPage=yes
UninstallDisplayIcon={app}\argos.exe
PrivilegesRequired=lowest
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
Source: "..\LICENSE"; DestDir: "{app}"

[Icons]
Name: "{userprograms}\ARGOS"; Filename: "{app}\argos.exe"; Parameters: "gui"; WorkingDir: "{app}"

[Run]
Filename: "{app}\argos.exe"; Parameters: "gui"; Description: "Abrir o painel do ARGOS"; Flags: postinstall nowait skipifsilent runhidden

[Code]
procedure RunWithoutInstalling;
var
  ArgosDir: String;
  ResultCode: Integer;
begin
  try
    ExtractTemporaryFiles('{app}\argos.exe');
    ArgosDir := ExpandConstant('{tmp}\') + '{app}';
    if not Exec(ArgosDir + '\argos.exe', 'gui', ArgosDir, SW_HIDE,
      ewWaitUntilTerminated, ResultCode) then
      MsgBox('Falha ao abrir o ARGOS. ' + SysErrorMessage(ResultCode), mbError, MB_OK)
    else if ResultCode <> 0 then
      MsgBox('O ARGOS encerrou com erro (código ' + IntToStr(ResultCode) + ').', mbError, MB_OK);
  except
    MsgBox('Falha ao preparar o ARGOS. ' + GetExceptionMessage, mbError, MB_OK);
  end;
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
