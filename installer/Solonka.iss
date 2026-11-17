; Установщик Солонки (Inno Setup 6). Собирается скриптом scripts\build-installer.ps1.
; Ставится в профиль пользователя без прав администратора.
;   полный:      модель распознавания внутри, Solonka-Setup-<версия>.exe
;   обновление:  /DUpdateOnly — без модели, ставится только поверх установленной программы
;                (его скачивает встроенное автообновление), Solonka-Update-<версия>.exe

#ifndef AppVersion
  #define AppVersion "1.0.0"
#endif
#ifndef BinDir
  #define BinDir "..\target\release"
#endif

#define AppName "Солонка"
#define AppExe "solonka.exe"
#define LegacyExe "shit-counter.exe"
#define AppIdGuid "{6F3C2A51-8E4B-4C7D-9A1E-5B2D7C0E9F13}"
#define RunKey "Software\Microsoft\Windows\CurrentVersion\Run"

[Setup]
; AppId не менять: по нему обновление находит установленную программу.
AppId={{#AppIdGuid}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher=denprog5
AppPublisherURL=https://github.com/denprog5/solonka
AppSupportURL=https://github.com/denprog5/solonka/issues
AppUpdatesURL=https://github.com/denprog5/solonka/releases
VersionInfoVersion={#AppVersion}
DefaultDirName={autopf}\Solonka
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
OutputDir=..\dist
#ifdef UpdateOnly
OutputBaseFilename=Solonka-Update-{#AppVersion}
#else
OutputBaseFilename=Solonka-Setup-{#AppVersion}
#endif
SetupIconFile=..\assets\icon.ico
WizardSmallImageFile=..\assets\wizard-small.png,..\assets\wizard-small-2x.png
UninstallDisplayIcon={app}\{#AppExe}
UninstallDisplayName={#AppName}
Compression=lzma2/ultra64
SolidCompression=yes
LZMAUseSeparateProcess=yes
WizardStyle=modern
; Приложение висит в трее: перед обновлением файлов его нужно закрыть.
CloseApplications=force
RestartApplications=no

[Languages]
Name: "ru"; MessagesFile: "compiler:Languages\Russian.isl"

[Tasks]
Name: "autostart"; Description: "Запускать вместе с Windows"; GroupDescription: "Дополнительно:"
Name: "desktopicon"; Description: "Ярлык на рабочем столе"; GroupDescription: "Дополнительно:"; Flags: unchecked

[InstallDelete]
; Остатки версии 0.1, когда программа называлась «Счётчик матов».
Type: files; Name: "{app}\{#LegacyExe}"
Type: files; Name: "{autoprograms}\Счётчик матов.lnk"
Type: files; Name: "{autodesktop}\Счётчик матов.lnk"

[Files]
Source: "{#BinDir}\{#AppExe}"; DestDir: "{app}"; Flags: ignoreversion
#ifndef UpdateOnly
Source: "{#BinDir}\models\gigaam-v2-ru\model.int8.onnx"; DestDir: "{app}\models\gigaam-v2-ru"; Flags: ignoreversion
Source: "{#BinDir}\models\gigaam-v2-ru\tokens.txt"; DestDir: "{app}\models\gigaam-v2-ru"; Flags: ignoreversion
#endif

[Icons]
Name: "{autoprograms}\{#AppName}"; Filename: "{app}\{#AppExe}"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExe}"; Tasks: desktopicon

[Registry]
; Автозапуск дальше переключается из меню трея. Тихое обновление его не трогает,
; чтобы не вернуть то, что пользователь выключил.
Root: HKCU; Subkey: "{#RunKey}"; ValueType: string; ValueName: "Solonka"; ValueData: """{app}\{#AppExe}"""; Tasks: autostart; Check: not WizardSilent

[Run]
Filename: "{app}\{#AppExe}"; Description: "Запустить {#AppName}"; Flags: nowait postinstall skipifsilent
; Автообновление запускает установщик с /RELAUNCH=1 — после него программа стартует снова.
Filename: "{app}\{#AppExe}"; Flags: nowait; Check: IsRelaunch

[UninstallRun]
Filename: "{sys}\taskkill.exe"; Parameters: "/F /IM {#AppExe}"; Flags: runhidden; RunOnceId: "KillApp"

[Code]
function IsRelaunch: Boolean;
begin
  Result := WizardSilent and (ExpandConstant('{param:relaunch|0}') = '1');
end;

procedure KillApp;
var
  Code: Integer;
begin
  Exec(ExpandConstant('{sys}\taskkill.exe'), '/F /IM {#AppExe}', '', SW_HIDE, ewWaitUntilTerminated, Code);
  Exec(ExpandConstant('{sys}\taskkill.exe'), '/F /IM {#LegacyExe}', '', SW_HIDE, ewWaitUntilTerminated, Code);
end;

#ifdef UpdateOnly
// Облегчённый установщик без модели: только поверх уже установленной программы.
function InitializeSetup: Boolean;
var
  Dir: String;
begin
  Result := RegQueryStringValue(HKCU, 'Software\Microsoft\Windows\CurrentVersion\Uninstall\{#AppIdGuid}_is1',
    'InstallLocation', Dir) and FileExists(AddBackslash(Dir) + 'models\gigaam-v2-ru\model.int8.onnx');
  if not Result then
    SuppressibleMsgBox('Это обновление для уже установленной Солонки.' + #13#10 +
      'Для первой установки скачайте Solonka-Setup со страницы github.com/denprog5/solonka/releases',
      mbError, MB_OK, IDOK);
end;
#endif

// Закрыть работающий экземпляр до копирования файлов (Restart Manager не всегда закрывает трей-приложения).
function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  KillApp;
  Result := '';
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  // Автозапуск версии 0.1 переносим под новое имя.
  if (CurStep = ssPostInstall) and RegValueExists(HKCU, '{#RunKey}', 'ShitCounter') then
  begin
    RegWriteStringValue(HKCU, '{#RunKey}', 'Solonka', '"' + ExpandConstant('{app}\{#AppExe}') + '"');
    RegDeleteValue(HKCU, '{#RunKey}', 'ShitCounter');
  end;
end;

// Счётчик и настройки лежат в %APPDATA%\Solonka — спрашиваем, удалять ли их.
procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  Dir: String;
begin
  if CurUninstallStep = usUninstall then
    // Автозапуск мог включить и установщик, и меню трея.
    RegDeleteValue(HKCU, '{#RunKey}', 'Solonka');
  if CurUninstallStep = usPostUninstall then
  begin
    Dir := ExpandConstant('{userappdata}\Solonka');
    // При тихом удалении данные пользователя сохраняем.
    if DirExists(Dir) and (not UninstallSilent) and (MsgBox('Удалить также настройки и сохранённый счётчик?' + #13#10 + Dir,
        mbConfirmation, MB_YESNO or MB_DEFBUTTON2) = IDYES) then
      DelTree(Dir, True, True, True);
  end;
end;
