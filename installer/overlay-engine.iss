#define AppName "overlay-engine"
#define AppVersion "0.1.0"
#define AppPublisher "overlay-engine"
#define Platform "x64"
#define StageDir "..\dist\overlay-engine-0.1.0-x64"

[Setup]
AppId={{9F2A7D2E-1C2B-4BB4-9A9F-0B5F1D8C2E20}
AppName={#AppName}
AppVersion={#AppVersion}
AppPublisher={#AppPublisher}
DefaultDirName={localappdata}\Programs\overlay-engine
DefaultGroupName=overlay-engine
DisableProgramGroupPage=yes
OutputDir=..\dist
OutputBaseFilename=overlay-engine-{#AppVersion}-{#Platform}-Setup
Compression=lzma2
SolidCompression=yes
ArchitecturesAllowed=x64os
ArchitecturesInstallIn64BitMode=x64os
PrivilegesRequired=admin
UninstallDisplayIcon={app}\core-server.exe
WizardStyle=modern

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Types]
Name: "full"; Description: "Full installation"
Name: "custom"; Description: "Custom installation"; Flags: iscustom

[Components]
Name: "core"; Description: "Core Server"; Types: full custom; Flags: fixed
Name: "desktop"; Description: "Desktop Window Monitor"; Types: full custom
Name: "gamebar"; Description: "Xbox Game Bar Widget"; Types: full custom

[Tasks]
Name: "autostart"; Description: "Start overlay-engine when I sign in"; GroupDescription: "Startup:"; Flags: unchecked
Name: "desktopicon"; Description: "Create desktop shortcut"; GroupDescription: "Shortcuts:"; Flags: checkedonce
Name: "startmenu"; Description: "Create Start Menu folder"; GroupDescription: "Shortcuts:"; Flags: checkedonce

[Files]
Source: "{#StageDir}\app\core-server.exe"; DestDir: "{app}"; Components: core; Flags: ignoreversion
Source: "{#StageDir}\app\renderer.dll"; DestDir: "{app}"; Components: core; Flags: ignoreversion skipifsourcedoesntexist
Source: "{#StageDir}\app\desktop-window-monitor.exe"; DestDir: "{app}"; Components: desktop; Flags: ignoreversion
Source: "{#StageDir}\widget\*"; DestDir: "{app}\widget"; Components: gamebar; Flags: ignoreversion recursesubdirs createallsubdirs
Source: "{#StageDir}\scripts\*"; DestDir: "{app}\scripts"; Flags: ignoreversion recursesubdirs createallsubdirs
Source: "{#StageDir}\manifest.json"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist

[Icons]
Name: "{autodesktop}\overlay-engine"; Filename: "{sys}\WindowsPowerShell\v1.0\powershell.exe"; Parameters: "-NoProfile -WindowStyle Hidden -ExecutionPolicy Bypass -File ""{app}\Start-overlay-engine.ps1"""; WorkingDir: "{app}"; Comment: "Start overlay-engine"; Tasks: desktopicon
Name: "{group}\Start overlay-engine"; Filename: "{sys}\WindowsPowerShell\v1.0\powershell.exe"; Parameters: "-NoProfile -WindowStyle Hidden -ExecutionPolicy Bypass -File ""{app}\Start-overlay-engine.ps1"""; WorkingDir: "{app}"; Comment: "Start overlay-engine"; Tasks: startmenu
Name: "{group}\Stop overlay-engine"; Filename: "{sys}\WindowsPowerShell\v1.0\powershell.exe"; Parameters: "-NoProfile -WindowStyle Hidden -ExecutionPolicy Bypass -File ""{app}\Stop-overlay-engine.ps1"""; WorkingDir: "{app}"; Comment: "Stop overlay-engine"; Tasks: startmenu
Name: "{group}\Uninstall overlay-engine"; Filename: "{uninstallexe}"; Comment: "Uninstall overlay-engine"; Tasks: startmenu

[Run]
Filename: "powershell.exe"; Parameters: "-NoProfile -ExecutionPolicy Bypass -File ""{app}\scripts\install.ps1"" -Release -SourceDir ""{app}"" -InstallDir ""{app}"" {code:GetInstallBackendArgs} -SkipUninstallRegistry -Quiet"; Flags: runhidden waituntilterminated

[UninstallRun]
Filename: "powershell.exe"; Parameters: "-NoProfile -ExecutionPolicy Bypass -File ""{app}\scripts\uninstall.ps1"" -Release -InstallDir ""{app}"" -RemoveWidget -Quiet"; Flags: runhidden waituntilterminated; RunOnceId: "overlay-engine-release-uninstall"

[Code]
function StopExistingOverlayEngine(): Boolean;
var
  ScriptPath: String;
  Script: String;
  Params: String;
  ResultCode: Integer;
begin
  ScriptPath := ExpandConstant('{tmp}\overlay-engine-stop-existing.ps1');
  Script :=
    'param([string]$Root)' + #13#10 +
    '$ErrorActionPreference = ''Stop''' + #13#10 +
    '$rootFull = [System.IO.Path]::GetFullPath($Root).TrimEnd(''\'')' + #13#10 +
    '$rootPrefix = $rootFull + ''\''' + #13#10 +
    '$targets = New-Object ''System.Collections.Generic.List[int]''' + #13#10 +
    'foreach ($name in @(''core-server.exe'', ''desktop-window-monitor.exe'')) {' + #13#10 +
    '  $filter = "Name = ''$name''"' + #13#10 +
    '  foreach ($p in @(Get-CimInstance Win32_Process -Filter $filter -ErrorAction SilentlyContinue)) {' + #13#10 +
    '    if (-not $p.ExecutablePath) { continue }' + #13#10 +
    '    $full = [System.IO.Path]::GetFullPath($p.ExecutablePath)' + #13#10 +
    '    if ($full.StartsWith($rootPrefix, [System.StringComparison]::OrdinalIgnoreCase) -and -not $targets.Contains([int]$p.ProcessId)) {' + #13#10 +
    '      $targets.Add([int]$p.ProcessId)' + #13#10 +
    '    }' + #13#10 +
    '  }' + #13#10 +
    '}' + #13#10 +
    'foreach ($id in $targets) { Stop-Process -Id $id -Force -ErrorAction SilentlyContinue }' + #13#10 +
    '$deadline = (Get-Date).AddSeconds(8)' + #13#10 +
    'do {' + #13#10 +
    '  $alive = @($targets | Where-Object { Get-Process -Id $_ -ErrorAction SilentlyContinue })' + #13#10 +
    '  if ($alive.Count -eq 0) { exit 0 }' + #13#10 +
    '  Start-Sleep -Milliseconds 200' + #13#10 +
    '} while ((Get-Date) -lt $deadline)' + #13#10 +
    'exit 1' + #13#10;

  if not SaveStringToFile(ScriptPath, Script, False) then begin
    Result := False;
    exit;
  end;

  Params := '-NoProfile -ExecutionPolicy Bypass -File "' + ScriptPath + '" "' + ExpandConstant('{app}') + '"';
  Result := Exec(ExpandConstant('{sys}\WindowsPowerShell\v1.0\powershell.exe'), Params, '', SW_HIDE, ewWaitUntilTerminated, ResultCode) and (ResultCode = 0);
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  if StopExistingOverlayEngine() then
    Result := ''
  else
    Result := 'Unable to stop running overlay-engine processes. Close overlay-engine, Core Server, and Desktop Window Monitor manually, then run setup again.';
end;

function GetInstallBackendArgs(Param: String): String;
var
  Components: String;
begin
  Components := 'Core';
  if WizardIsComponentSelected('desktop') then
    Components := Components + ',DesktopMonitor';
  if WizardIsComponentSelected('gamebar') then
    Components := Components + ',GameBarWidget';

  Result := '-Components ' + Components;
  if WizardIsTaskSelected('autostart') then
    Result := Result + ' -AutoStart';
  if WizardIsTaskSelected('desktopicon') then
    Result := Result + ' -CreateDesktopShortcut';
  if WizardIsTaskSelected('startmenu') then
    Result := Result + ' -CreateStartMenu';
end;
