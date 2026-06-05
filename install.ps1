<#
.SYNOPSIS
    overlay-engine 安装脚本：默认开发安装；-Release 时作为发布版安装后端。

.DESCRIPTION
    默认行为保留原开发流程：cargo + msbuild + dev cert + signtool + Add-AppxPackage。
    Release 模式安装已构建好的 staging payload，支持组件选择、自启、快捷方式和开始菜单。
#>

[CmdletBinding()]
param(
    [switch]$Release,

    [ValidateSet('Debug', 'Release')]
    [string]$Configuration = 'Release',

    [ValidateSet('x64')]
    [string]$Platform = 'x64',

    [switch]$SkipRust,
    [switch]$SkipCSharp,
    [switch]$SkipInstall,
    [switch]$Clean,

    [string]$SourceDir,
    [string]$InstallDir = (Join-Path $env:LOCALAPPDATA 'Programs\overlay-engine'),
    [string[]]$Components = @('Core', 'DesktopMonitor', 'GameBarWidget'),
    [switch]$AutoStart,
    [switch]$CreateDesktopShortcut,
    [switch]$CreateStartMenu,
    [switch]$Quiet,
    [switch]$SkipUninstallRegistry,
    [string]$LogPath,

    [ValidateSet('Standalone', 'Inno')]
    [string]$InstallHost = 'Standalone'
)

$ErrorActionPreference = 'Stop'

$ProjectRoot     = $PSScriptRoot
$CsharpDir       = Join-Path $ProjectRoot 'monitors\game-bar-widget'
$Csproj          = Join-Path $CsharpDir 'OverlayWidget.csproj'
$AppPackagesDir  = Join-Path $CsharpDir 'AppPackages'
$PfxPath         = Join-Path $CsharpDir 'OverlayWidget_Dev.pfx'
$CerPath         = Join-Path $CsharpDir 'OverlayWidget_Dev.cer'
$CertSubject     = 'CN=OverlayWidget Dev'
$PfxPassword     = 'OverlayWidget'
$ScheduledTaskName = 'overlay-engine Core'
$StartupShortcutName = 'overlay-engine.lnk'
$StartupApprovedEnabled = [byte[]](0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00)
$AppName = 'overlay-engine'
$CoreRegistryKey = 'HKCU:\Software\overlay-engine\Core'

function Write-InstallHost([string]$Message, [ConsoleColor]$Color = [ConsoleColor]::Gray) {
    if (-not $Quiet) { Write-Host $Message -ForegroundColor $Color }
}

function Write-Step([int]$n, [int]$total, [string]$msg) {
    Write-InstallHost ''
    Write-InstallHost "[$n/$total] $msg" Cyan
}

function Test-IsAdmin {
    $id = [System.Security.Principal.WindowsIdentity]::GetCurrent()
    $p  = New-Object System.Security.Principal.WindowsPrincipal($id)
    return $p.IsInRole([System.Security.Principal.WindowsBuiltInRole]::Administrator)
}

function Resolve-Cargo {
    $cmd = Get-Command cargo.exe -ErrorAction SilentlyContinue
    if (-not $cmd) { throw 'cargo.exe not found in PATH. Install rustup from https://rustup.rs/' }
    return $cmd.Source
}

function Resolve-MSBuild {
    $pfx86 = [Environment]::GetEnvironmentVariable('ProgramFiles(x86)')
    if (-not $pfx86) { $pfx86 = 'C:\Program Files (x86)' }
    $vswhere = Join-Path $pfx86 'Microsoft Visual Studio\Installer\vswhere.exe'
    if (Test-Path $vswhere) {
        $instPaths = @(& $vswhere -latest -prerelease -products * `
            -requires Microsoft.Component.MSBuild `
            -property installationPath 2>$null)
        foreach ($inst in $instPaths) {
            if (-not $inst) { continue }
            foreach ($v in 'Current', '17.0', '16.0') {
                $msb = Join-Path $inst "MSBuild\$v\Bin\MSBuild.exe"
                if (Test-Path $msb) { return $msb }
            }
        }
    }
    $cmd = Get-Command msbuild.exe -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }
    throw 'MSBuild.exe not found. Install Visual Studio with the UWP/MSIX workload.'
}

function Resolve-SignTool {
    $candidates = @(
        'C:\Program Files (x86)\Windows Kits\10\bin\10.0.26100.0\x64\signtool.exe',
        'C:\Program Files (x86)\Windows Kits\10\bin\10.0.22621.0\x64\signtool.exe'
    )
    foreach ($p in $candidates) {
        if (Test-Path $p) { return $p }
    }
    throw 'signtool.exe not found in any known Windows SDK path.'
}

function Get-DefaultReleaseSourceDir {
    $scriptParent = Split-Path -Parent $PSScriptRoot
    if ((Split-Path -Leaf $PSScriptRoot) -eq 'scripts' -and (Test-Path (Join-Path $scriptParent 'app'))) {
        return $scriptParent
    }
    if (Test-Path (Join-Path $PSScriptRoot 'app')) { return $PSScriptRoot }
    return $ProjectRoot
}

function Resolve-ReleaseComponents([string[]]$Requested) {
    $valid = @('Core', 'DesktopMonitor', 'GameBarWidget')
    $resolved = New-Object 'System.Collections.Generic.List[string]'

    foreach ($entry in @($Requested)) {
        if (-not $entry) { continue }
        foreach ($part in ($entry -split ',')) {
            $name = $part.Trim()
            if (-not $name) { continue }
            $canonical = $valid | Where-Object { $_.Equals($name, [System.StringComparison]::OrdinalIgnoreCase) } | Select-Object -First 1
            if (-not $canonical) { throw "Invalid component '$name'. Valid values: $($valid -join ', ')" }
            if (-not $resolved.Contains($canonical)) { $resolved.Add($canonical) }
        }
    }

    if (-not $resolved.Contains('Core')) { $resolved.Insert(0, 'Core') }
    return [string[]]$resolved.ToArray()
}

function Test-Component([string]$Name) {
    return @($script:Components) -contains $Name
}

function Copy-FileRequired([string]$Source, [string]$Destination) {
    if (-not (Test-Path $Source)) { throw "Required file missing: $Source" }
    $sourceFull = [System.IO.Path]::GetFullPath($Source)
    $destFull = [System.IO.Path]::GetFullPath($Destination)
    if ($sourceFull -eq $destFull) { return }
    $parent = Split-Path -Parent $Destination
    if (-not (Test-Path $parent)) { New-Item -ItemType Directory -Path $parent -Force | Out-Null }
    Copy-Item -Path $Source -Destination $Destination -Force
}

function Test-PathInsideRoot([string]$Path, [string]$RootDir) {
    if (-not $Path -or -not $RootDir) { return $false }
    $rootFull = [System.IO.Path]::GetFullPath($RootDir).TrimEnd('\')
    $full = [System.IO.Path]::GetFullPath($Path)
    return $full.Equals($rootFull, [System.StringComparison]::OrdinalIgnoreCase) -or
        $full.StartsWith($rootFull + '\', [System.StringComparison]::OrdinalIgnoreCase)
}

function Get-ReleaseManifest([string]$RootDir) {
    $manifestPath = Join-Path $RootDir 'manifest.json'
    if (-not (Test-Path $manifestPath)) { return $null }
    try { return Get-Content -Path $manifestPath -Raw | ConvertFrom-Json } catch { return $null }
}

function Get-ReleaseVersion([string]$RootDir) {
    $manifest = Get-ReleaseManifest -RootDir $RootDir
    if ($manifest -and $manifest.version) { return [string]$manifest.version }
    return '0.1.4'
}

function Stop-InstalledProcess([string]$Name, [string]$RootDir) {
    $targets = New-Object 'System.Collections.Generic.List[int]'
    $filter = "Name = '$Name.exe'"

    foreach ($p in @(Get-CimInstance Win32_Process -Filter $filter -ErrorAction SilentlyContinue)) {
        if (-not $p.ExecutablePath) { continue }
        if ((Test-PathInsideRoot -Path $p.ExecutablePath -RootDir $RootDir) -and -not $targets.Contains([int]$p.ProcessId)) {
            $targets.Add([int]$p.ProcessId)
        }
    }

    foreach ($p in @(Get-Process -Name $Name -ErrorAction SilentlyContinue)) {
        $path = $null
        try { $path = $p.MainModule.FileName } catch { }
        if (-not $path) { continue }
        if ((Test-PathInsideRoot -Path $path -RootDir $RootDir) -and -not $targets.Contains([int]$p.Id)) {
            $targets.Add([int]$p.Id)
        }
    }

    foreach ($id in $targets) {
        Write-InstallHost "  stopping $Name pid=$id" Yellow
        Stop-Process -Id $id -Force -ErrorAction SilentlyContinue
    }

    $deadline = (Get-Date).AddSeconds(8)
    do {
        $alive = @($targets | Where-Object { Get-Process -Id $_ -ErrorAction SilentlyContinue })
        if ($alive.Count -eq 0) { return }
        Start-Sleep -Milliseconds 200
    } while ((Get-Date) -lt $deadline)

    throw "Unable to stop installed $Name process(es): $($alive -join ', ')"
}

function New-ShortcutFile(
    [string]$Path,
    [string]$TargetPath,
    [string]$Arguments,
    [string]$WorkingDirectory,
    [string]$Description,
    [string]$IconLocation
) {
    $parent = Split-Path -Parent $Path
    if (-not (Test-Path $parent)) { New-Item -ItemType Directory -Path $parent -Force | Out-Null }
    $shell = New-Object -ComObject WScript.Shell
    $shortcut = $shell.CreateShortcut($Path)
    $shortcut.TargetPath = $TargetPath
    $shortcut.Arguments = $Arguments
    $shortcut.WorkingDirectory = $WorkingDirectory
    $shortcut.Description = $Description
    if ($IconLocation) { $shortcut.IconLocation = $IconLocation }
    $shortcut.Save()
}

function Get-PowerShellShortcutTarget { return (Get-Command powershell.exe -ErrorAction Stop).Source }

function New-LauncherScripts([string]$RootDir) {
    $startScript = Join-Path $RootDir 'Start-overlay-engine.ps1'
    $stopScript = Join-Path $RootDir 'Stop-overlay-engine.ps1'

    Set-Content -Path $startScript -Encoding UTF8 -Value @'
$ErrorActionPreference = 'Stop'
$root = [System.IO.Path]::GetFullPath($PSScriptRoot).TrimEnd('\')
$core = Join-Path $root 'core-server.exe'
$alreadyRunning = $false
foreach ($p in @(Get-Process -Name 'core-server' -ErrorAction SilentlyContinue)) {
    $path = $null
    try { $path = $p.MainModule.FileName } catch { }
    if (-not $path) { continue }
    $full = [System.IO.Path]::GetFullPath($path)
    if ($full.Equals($core, [System.StringComparison]::OrdinalIgnoreCase)) {
        $alreadyRunning = $true
        break
    }
}
if (-not $alreadyRunning) {
    Start-Process -FilePath $core -WorkingDirectory $root -WindowStyle Hidden
}
'@

    Set-Content -Path $stopScript -Encoding UTF8 -Value @'
$ErrorActionPreference = 'SilentlyContinue'
$root = [System.IO.Path]::GetFullPath($PSScriptRoot).TrimEnd('\')
function Test-PathInsideRoot([string]$Path, [string]$RootDir) {
    if (-not $Path -or -not $RootDir) { return $false }
    $rootFull = [System.IO.Path]::GetFullPath($RootDir).TrimEnd('\')
    $full = [System.IO.Path]::GetFullPath($Path)
    return $full.Equals($rootFull, [System.StringComparison]::OrdinalIgnoreCase) -or
        $full.StartsWith($rootFull + '\', [System.StringComparison]::OrdinalIgnoreCase)
}
foreach ($name in @('core-server', 'desktop-window-monitor')) {
    foreach ($p in @(Get-Process -Name $name -ErrorAction SilentlyContinue)) {
        $path = $null
        try { $path = $p.MainModule.FileName } catch { }
        if (-not $path) { continue }
        if (Test-PathInsideRoot -Path $path -RootDir $root) {
            Stop-Process -Id $p.Id -Force
        }
    }
}
'@
}

function Remove-RegistryValueIfExists([string]$Path, [string]$Name) {
    if (Get-ItemProperty -Path $Path -Name $Name -ErrorAction SilentlyContinue) {
        Remove-ItemProperty -Path $Path -Name $Name -Force
    }
}

function Register-OverlayAutoStart([string]$RootDir) {
    Unregister-OverlayAutoStart
    $ps = Get-PowerShellShortcutTarget
    $startScript = Join-Path $RootDir 'Start-overlay-engine.ps1'
    $cmd = "`"$ps`" -NoProfile -WindowStyle Hidden -ExecutionPolicy Bypass -File `"$startScript`""

    $runKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
    if (-not (Test-Path $runKey)) { New-Item -Path $runKey -Force | Out-Null }
    New-ItemProperty -Path $runKey -Name $AppName -Value $cmd -PropertyType String -Force | Out-Null

    $approvedRunKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run'
    if (-not (Test-Path $approvedRunKey)) { New-Item -Path $approvedRunKey -Force | Out-Null }
    New-ItemProperty -Path $approvedRunKey -Name $AppName -Value $StartupApprovedEnabled -PropertyType Binary -Force | Out-Null
}

function Unregister-OverlayAutoStart {
    if (Get-ScheduledTask -TaskName $ScheduledTaskName -ErrorAction SilentlyContinue) {
        Unregister-ScheduledTask -TaskName $ScheduledTaskName -Confirm:$false
    }

    Remove-RegistryValueIfExists -Path 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run' -Name $AppName
    Remove-RegistryValueIfExists -Path 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run' -Name $AppName
    Remove-RegistryValueIfExists -Path 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\StartupFolder' -Name $StartupShortcutName

    $startupDir = [Environment]::GetFolderPath('Startup')
    $startupLink = Join-Path $startupDir $StartupShortcutName
    if (Test-Path $startupLink) { Remove-Item $startupLink -Force }
}

function New-OverlayShortcuts([string]$RootDir, [bool]$Desktop, [bool]$StartMenu) {
    $ps = Get-PowerShellShortcutTarget
    $startArgs = "-NoProfile -WindowStyle Hidden -ExecutionPolicy Bypass -File `"$(Join-Path $RootDir 'Start-overlay-engine.ps1')`""
    $stopArgs = "-NoProfile -WindowStyle Hidden -ExecutionPolicy Bypass -File `"$(Join-Path $RootDir 'Stop-overlay-engine.ps1')`""
    $icon = Join-Path $RootDir 'core-server.exe'
    $iconLocation = if (Test-Path $icon) { "$icon,0" } else { $null }

    if ($Desktop) {
        $desktopDir = [Environment]::GetFolderPath('DesktopDirectory')
        New-ShortcutFile -Path (Join-Path $desktopDir 'overlay-engine.lnk') -TargetPath $ps -Arguments $startArgs -WorkingDirectory $RootDir -Description 'Start overlay-engine' -IconLocation $iconLocation
    }

    $programs = [Environment]::GetFolderPath('Programs')
    $group = Join-Path $programs 'overlay-engine'
    $oldGameBarLink = Join-Path $group 'Open Xbox Game Bar.lnk'
    if (Test-Path $oldGameBarLink) { Remove-Item $oldGameBarLink -Force }

    if ($StartMenu) {
        New-ShortcutFile -Path (Join-Path $group 'Start overlay-engine.lnk') -TargetPath $ps -Arguments $startArgs -WorkingDirectory $RootDir -Description 'Start overlay-engine' -IconLocation $iconLocation
        New-ShortcutFile -Path (Join-Path $group 'Stop overlay-engine.lnk') -TargetPath $ps -Arguments $stopArgs -WorkingDirectory $RootDir -Description 'Stop overlay-engine' -IconLocation $iconLocation
        $uninstallScript = Join-Path $RootDir 'scripts\uninstall.ps1'
        $uninstallArgs = "-NoProfile -ExecutionPolicy Bypass -File `"$uninstallScript`" -Release -InstallDir `"$RootDir`""
        New-ShortcutFile -Path (Join-Path $group 'Uninstall overlay-engine.lnk') -TargetPath $ps -Arguments $uninstallArgs -WorkingDirectory $RootDir -Description 'Uninstall overlay-engine' -IconLocation $iconLocation
    }
}

function Write-UninstallRegistry([string]$RootDir, [string]$Version) {
    $key = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\overlay-engine'
    if (-not (Test-Path $key)) { New-Item -Path $key -Force | Out-Null }
    $uninstallScript = Join-Path $RootDir 'scripts\uninstall.ps1'
    $cmd = "powershell.exe -NoProfile -ExecutionPolicy Bypass -File `"$uninstallScript`" -Release -InstallDir `"$RootDir`""
    New-ItemProperty -Path $key -Name DisplayName -Value 'overlay-engine' -PropertyType String -Force | Out-Null
    New-ItemProperty -Path $key -Name DisplayVersion -Value $Version -PropertyType String -Force | Out-Null
    New-ItemProperty -Path $key -Name Publisher -Value 'overlay-engine' -PropertyType String -Force | Out-Null
    New-ItemProperty -Path $key -Name InstallLocation -Value $RootDir -PropertyType String -Force | Out-Null
    New-ItemProperty -Path $key -Name DisplayIcon -Value (Join-Path $RootDir 'core-server.exe') -PropertyType String -Force | Out-Null
    New-ItemProperty -Path $key -Name UninstallString -Value $cmd -PropertyType String -Force | Out-Null
    New-ItemProperty -Path $key -Name QuietUninstallString -Value "$cmd -Quiet" -PropertyType String -Force | Out-Null
    New-ItemProperty -Path $key -Name NoModify -Value 1 -PropertyType DWord -Force | Out-Null
    New-ItemProperty -Path $key -Name NoRepair -Value 1 -PropertyType DWord -Force | Out-Null
}

function Write-CoreLocationRegistry([string]$RootDir, [string]$Version) {
    $coreExe = Join-Path $RootDir 'core-server.exe'
    if (-not (Test-Path $coreExe)) { throw "Core executable not found: $coreExe" }
    if (-not (Test-Path $CoreRegistryKey)) { New-Item -Path $CoreRegistryKey -Force | Out-Null }
    New-ItemProperty -Path $CoreRegistryKey -Name InstallDir -Value $RootDir -PropertyType String -Force | Out-Null
    New-ItemProperty -Path $CoreRegistryKey -Name CoreExe -Value $coreExe -PropertyType String -Force | Out-Null
    New-ItemProperty -Path $CoreRegistryKey -Name Version -Value $Version -PropertyType String -Force | Out-Null
}

function Get-InstalledAppxPackageInfo([string]$Name) {
    $pkg = Get-AppxPackage -Name $Name -ErrorAction SilentlyContinue | Select-Object -First 1
    if (-not $pkg) { return $null }
    return [ordered]@{
        name = [string]$pkg.Name
        packageFullName = [string]$pkg.PackageFullName
        publisher = [string]$pkg.Publisher
        version = [string]$pkg.Version
    }
}

function Get-CertificateThumbprint([string]$CertificatePath) {
    if (-not $CertificatePath -or -not (Test-Path $CertificatePath)) { return $null }
    $cert = [System.Security.Cryptography.X509Certificates.X509Certificate2]::new($CertificatePath)
    try { return [string]$cert.Thumbprint }
    finally { $cert.Dispose() }
}

function Invoke-ReleaseInstall {
    $script:Components = Resolve-ReleaseComponents $Components

    if (-not $SourceDir) { $SourceDir = Get-DefaultReleaseSourceDir }
    $SourceDir = [System.IO.Path]::GetFullPath($SourceDir)
    $InstallDir = [System.IO.Path]::GetFullPath($InstallDir)

    $appSource = Join-Path $SourceDir 'app'
    if (-not (Test-Path $appSource) -and (Test-Path (Join-Path $SourceDir 'core-server.exe'))) {
        $appSource = $SourceDir
    }
    $scriptsSource = Join-Path $SourceDir 'scripts'
    $widgetSource = Join-Path $SourceDir 'widget'
    if (-not (Test-Path $appSource)) { throw "Release app payload not found: $appSource" }

    Write-InstallHost 'overlay-engine Release Install' White
    Write-InstallHost "  SourceDir    : $SourceDir" DarkGray
    Write-InstallHost "  InstallDir   : $InstallDir" DarkGray
    $releaseVersion = Get-ReleaseVersion -RootDir $SourceDir

    Write-InstallHost "  Components   : $($Components -join ', ')" DarkGray
    Write-InstallHost "  AutoStart    : $AutoStart" DarkGray
    Write-InstallHost "  InstallHost  : $InstallHost" DarkGray
    Write-InstallHost "  Version      : $releaseVersion" DarkGray

    Stop-InstalledProcess -Name 'core-server' -RootDir $InstallDir
    Stop-InstalledProcess -Name 'desktop-window-monitor' -RootDir $InstallDir

    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
    New-Item -ItemType Directory -Path (Join-Path $InstallDir 'scripts') -Force | Out-Null

    Copy-FileRequired -Source (Join-Path $appSource 'core-server.exe') -Destination (Join-Path $InstallDir 'core-server.exe')
    $rendererSource = Join-Path $appSource 'renderer.dll'
    if (Test-Path $rendererSource) { Copy-FileRequired -Source $rendererSource -Destination (Join-Path $InstallDir 'renderer.dll') }

    if (Test-Component 'DesktopMonitor') {
        Copy-FileRequired -Source (Join-Path $appSource 'desktop-window-monitor.exe') -Destination (Join-Path $InstallDir 'desktop-window-monitor.exe')
    }

    if (Test-Path $scriptsSource) {
        $scriptsDest = Join-Path $InstallDir 'scripts'
        if ([System.IO.Path]::GetFullPath($scriptsSource) -ne [System.IO.Path]::GetFullPath($scriptsDest)) {
            Copy-Item -Path (Join-Path $scriptsSource '*') -Destination $scriptsDest -Recurse -Force
        }
    }

    $configLines = New-Object 'System.Collections.Generic.List[string]'
    $configLines.Add('# Generated by overlay-engine installer.')
    if (Test-Component 'DesktopMonitor') {
        $configLines.Add('Monitor.DesktopWindow.Path=desktop-window-monitor.exe')
        $configLines.Add('Monitor.DesktopWindow.MaxInstancesPerApp=16')
        $configLines.Add('Monitor.DesktopWindow.WindowModes=bordered,borderless,borderless-fullscreen')
        $configLines.Add('Monitor.DesktopWindow.Flags=click-through')
    }
    if (Test-Component 'GameBarWidget') {
        $configLines.Add('Monitor.GameBar.Available=true')
        $configLines.Add('Monitor.GameBar.MaxInstances=1')
        $configLines.Add('Monitor.GameBar.StartPolicy=user-manual')
    }
    Set-Content -Path (Join-Path $InstallDir 'config.ini') -Encoding UTF8 -Value $configLines

    New-LauncherScripts -RootDir $InstallDir

    $installedWidget = $false
    $devCert = $null
    $widgetPackage = $null
    if (Test-Component 'GameBarWidget') {
        $widgetScript = Join-Path $InstallDir 'scripts\game-bar-widget-install.ps1'
        if (-not (Test-Path $widgetScript)) { $widgetScript = Join-Path $scriptsSource 'game-bar-widget-install.ps1' }
        if (-not (Test-Path $widgetScript)) { throw 'game-bar-widget-install.ps1 not found in release scripts.' }
        $msix = Get-ChildItem -Path $widgetSource -Filter 'OverlayWidget_*.msix' -Recurse -ErrorAction SilentlyContinue | Sort-Object LastWriteTime -Descending | Select-Object -First 1
        if (-not $msix) { throw "Game Bar MSIX not found under $widgetSource" }
        $depDir = Join-Path $widgetSource "Dependencies\$Platform"
        $args = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $widgetScript, '-InstallOnly', '-Platform', $Platform, '-MsixPath', $msix.FullName)
        if (Test-Path $depDir) { $args += @('-DependencyDir', $depDir) }
        $devCert = Join-Path $widgetSource 'OverlayWidget_Dev.cer'
        if (Test-Path $devCert) { $args += @('-TrustDevCertificate', '-DevCertPath', $devCert) }
        if ($LogPath) {
            & powershell.exe @args *>&1 | Tee-Object -FilePath $LogPath -Append
            $widgetExitCode = $LASTEXITCODE
        }
        else {
            & powershell.exe @args
            $widgetExitCode = $LASTEXITCODE
        }
        if ($widgetExitCode -ne 0) { throw "Game Bar widget install failed: exit $widgetExitCode" }
        $installedWidget = $true
        $widgetPackage = Get-InstalledAppxPackageInfo -Name 'OverlayWidget'
    }

    Write-CoreLocationRegistry -RootDir $InstallDir -Version $releaseVersion

    if ($AutoStart) { Register-OverlayAutoStart -RootDir $InstallDir } else { Unregister-OverlayAutoStart }
    if ($InstallHost -eq 'Standalone') {
        New-OverlayShortcuts -RootDir $InstallDir -Desktop:$CreateDesktopShortcut -StartMenu:$CreateStartMenu
    }

    $state = [ordered]@{
        installDir = $InstallDir
        installHost = $InstallHost
        version = $releaseVersion
        components = @($Components)
        autoStart = [bool]$AutoStart
        desktopShortcut = [bool]$CreateDesktopShortcut
        startMenu = [bool]$CreateStartMenu
        shortcutsOwner = if ($InstallHost -eq 'Inno') { 'Inno' } else { 'PowerShell' }
        uninstallOwner = if ($InstallHost -eq 'Inno') { 'Inno' } else { 'PowerShell' }
        startupRegistryValueName = if ($AutoStart) { $AppName } else { $null }
        scheduledTaskName = $null
        msixPackageName = if ($installedWidget) { 'OverlayWidget' } else { $null }
        msixPackageFullName = if ($widgetPackage) { $widgetPackage.packageFullName } else { $null }
        msixPublisher = if ($widgetPackage) { $widgetPackage.publisher } else { $null }
        certificateThumbprint = Get-CertificateThumbprint -CertificatePath $devCert
        coreRegistryKey = 'HKCU:\Software\overlay-engine\Core'
        installedAt = (Get-Date).ToString('o')
    }
    $state | ConvertTo-Json -Depth 6 | Set-Content -Path (Join-Path $InstallDir 'install-state.json') -Encoding UTF8

    if (-not $SkipUninstallRegistry -and $InstallHost -eq 'Standalone') { Write-UninstallRegistry -RootDir $InstallDir -Version $releaseVersion }

    Write-InstallHost ''
    Write-InstallHost 'Release install complete.' Green
    Write-InstallHost "Start from: $InstallDir\Start-overlay-engine.ps1" Cyan
}

if ($Release) {
    if ($LogPath) {
        $logParent = Split-Path -Parent $LogPath
        if ($logParent -and -not (Test-Path $logParent)) { New-Item -ItemType Directory -Path $logParent -Force | Out-Null }
        Set-Content -Path $LogPath -Encoding UTF8 -Value @(
            'overlay-engine install backend log',
            "Started: $((Get-Date).ToString('o'))"
        )
        try {
            Invoke-ReleaseInstall
            Add-Content -Path $LogPath -Encoding UTF8 -Value "Completed: $((Get-Date).ToString('o'))"
        }
        catch {
            Add-Content -Path $LogPath -Encoding UTF8 -Value @(
                "ERROR: $($_.Exception.Message)",
                "STACK: $($_.ScriptStackTrace)"
            )
            throw
        }
    }
    else {
        Invoke-ReleaseInstall
    }
    return
}

$totalSteps = 0
if (-not $SkipRust)    { $totalSteps++ }
if (-not $SkipCSharp)  { $totalSteps++ }
if (-not $SkipInstall) { $totalSteps += 4 }
if ($totalSteps -eq 0) {
    Write-InstallHost 'Nothing to do (all steps skipped).' Yellow
    return
}
$step = 0

Write-InstallHost 'overlay-engine v1.0 install' White
Write-InstallHost "  Configuration : $Configuration" DarkGray
Write-InstallHost "  Platform      : $Platform" DarkGray
Write-InstallHost "  Clean         : $Clean" DarkGray

if (-not $SkipRust) {
    $step++
    Write-Step $step $totalSteps 'cargo build --release'

    $cargo = Resolve-Cargo
    Push-Location $ProjectRoot
    try {
        if ($Clean) {
            Write-InstallHost '  cargo clean' DarkGray
            & $cargo clean
            if ($LASTEXITCODE -ne 0) { throw "cargo clean failed: exit $LASTEXITCODE" }
        }

        & $cargo build --release
        if ($LASTEXITCODE -ne 0) { throw "cargo build failed: exit $LASTEXITCODE" }
    }
    finally {
        Pop-Location
    }

    $coreServer = Join-Path $ProjectRoot 'target\release\core-server.exe'
    $desktopConsumer = Join-Path $ProjectRoot 'target\release\desktop-window-monitor.exe'
    $rendererDll = Join-Path $ProjectRoot 'target\release\renderer.dll'

    $products = @()
    foreach ($f in @($coreServer, $desktopConsumer, $rendererDll)) {
        if (Test-Path $f) {
            $sz = (Get-Item $f).Length
            $products += "    $(Split-Path $f -Leaf) ({0:N0} bytes)" -f $sz
        }
    }
    if ($products.Count -gt 0) {
        Write-InstallHost ("  ok:`n" + ($products -join "`n")) Green
    }
}

if (-not $SkipCSharp) {
    $step++
    Write-Step $step $totalSteps "msbuild OverlayWidget ($Configuration|$Platform)"

    if (-not (Test-Path $Csproj)) { throw "csproj not found: $Csproj" }

    $msbuild = Resolve-MSBuild
    Write-InstallHost "  using: $msbuild" DarkGray

    $msbArgs = @(
        $Csproj
        "/p:Configuration=$Configuration"
        "/p:Platform=$Platform"
        '/p:AppxBundle=Never'
        "/p:AppxBundlePlatforms=$Platform"
        '/p:UapAppxPackageBuildMode=SideloadOnly'
        '/p:AppxPackageSigningEnabled=false'
        '/restore'
        '/m'
    )
    if ($Clean) { $msbArgs += '/t:Clean;Build' } else { $msbArgs += '/t:Build' }
    if ($VerbosePreference -eq 'Continue') { $msbArgs += '/v:normal' } else { $msbArgs += '/v:minimal' }

    & $msbuild @msbArgs
    if ($LASTEXITCODE -ne 0) { throw "msbuild failed: exit $LASTEXITCODE" }

    Write-InstallHost '  ok: msbuild done' Green
}

function Get-LatestMsix {
    $pattern = "_${Platform}\.msix$"
    $cand = Get-ChildItem -Path $AppPackagesDir -Filter 'OverlayWidget_*.msix' -Recurse -ErrorAction SilentlyContinue |
        Where-Object { $_.Name -match $pattern } |
        Sort-Object LastWriteTime -Descending |
        Select-Object -First 1
    if (-not $cand) { throw "MSIX not found under $AppPackagesDir for $Platform. Re-run without -SkipCSharp first." }
    return $cand.FullName
}

if ($SkipInstall) {
    Write-InstallHost ''
    Write-InstallHost 'Skip install (-SkipInstall set). MSIX produced.' Yellow
    return
}

$step++
Write-Step $step $totalSteps 'dev certificate'

if (-not (Test-Path $PfxPath)) {
    Write-InstallHost "  generating self-signed cert ($CertSubject)..." DarkGray
    Import-Module PKI -ErrorAction Stop
    $certParams = @{
        Type              = 'CodeSigningCert'
        Subject           = $CertSubject
        KeyUsage          = 'DigitalSignature'
        FriendlyName      = 'OverlayWidget Dev'
        CertStoreLocation = 'Cert:\CurrentUser\My'
        TextExtension     = @('2.5.29.37={text}1.3.6.1.5.5.7.3.3', '2.5.29.19={text}')
    }
    $cert = New-SelfSignedCertificate @certParams
    $securePass = ConvertTo-SecureString -String $PfxPassword -Force -AsPlainText
    Export-PfxCertificate -Cert $cert -FilePath $PfxPath -Password $securePass | Out-Null
    Export-Certificate -Cert $cert -FilePath $CerPath | Out-Null
    Remove-Item -Path "Cert:\CurrentUser\My\$($cert.Thumbprint)" -Force
    Write-InstallHost "  ok: PFX -> $PfxPath" Green
}
else {
    Write-InstallHost "  ok: $PfxPath" Green
}

$step++
Write-Step $step $totalSteps 'trust certificate'

Import-Module PKI -ErrorAction Stop
$existing = @(Get-ChildItem -Path 'Cert:\LocalMachine\TrustedPeople' -ErrorAction SilentlyContinue |
    Where-Object { $_.Subject -eq $CertSubject })

if ($existing.Count -eq 0) {
    if (-not (Test-IsAdmin)) { throw 'Admin rights required to import cert into LocalMachine\TrustedPeople. Re-run from elevated PowerShell.' }
    Import-Certificate -FilePath $CerPath -CertStoreLocation 'Cert:\LocalMachine\TrustedPeople' | Out-Null
    Write-InstallHost '  ok: imported' Green
}
else {
    Write-InstallHost "  ok: already trusted (thumbprint=$($existing[0].Thumbprint))" Green
}

$step++
Write-Step $step $totalSteps 'signtool sign'

$msix = Get-LatestMsix
Write-InstallHost "  $msix" DarkGray
$signtool = Resolve-SignTool
& $signtool sign /fd SHA256 /f $PfxPath /p $PfxPassword $msix
if ($LASTEXITCODE -ne 0) { throw "signtool failed: exit $LASTEXITCODE" }
Write-InstallHost '  ok: signed' Green

$step++
Write-Step $step $totalSteps 'Add-AppxPackage'

$existingPkg = Get-AppxPackage -Name 'OverlayWidget' -ErrorAction SilentlyContinue
if ($existingPkg) {
    Write-InstallHost "  removing existing $($existingPkg.PackageFullName)" Yellow
    Remove-AppxPackage -Package $existingPkg.PackageFullName
}

$msixDir = Split-Path -Parent $msix
$depsDir = Join-Path $msixDir "Dependencies\$Platform"
$depPaths = @()
if (Test-Path $depsDir) {
    $depPaths = Get-ChildItem -Path $depsDir -Filter '*.appx' -ErrorAction SilentlyContinue | ForEach-Object { $_.FullName }
}

function Test-FrameworkDepInstalled {
    param([string]$DepPath)
    $pkgName = [System.IO.Path]::GetFileNameWithoutExtension($DepPath)
    return ($null -ne (Get-AppxPackage -Name $pkgName -ErrorAction SilentlyContinue))
}

$missingDeps = @()
foreach ($d in $depPaths) {
    if (Test-FrameworkDepInstalled $d) {
        Write-InstallHost "  skip (already installed): $(Split-Path $d -Leaf)" DarkGray
    } else {
        $missingDeps += $d
    }
}

function Invoke-AddAppxWithRetry {
    param([string]$Msix, [string[]]$Deps)
    $params = @{ Path = $Msix; ErrorAction = 'Stop' }
    if ($Deps -and $Deps.Count -gt 0) { $params.DependencyPath = $Deps }

    try {
        Add-AppxPackage @params
        return
    } catch {
        if ($_.Exception.Message -notmatch '0x80073D02') { throw }
        Write-InstallHost '  resource in use (0x80073D02), retrying with -ForceApplicationShutdown...' Yellow
    }
    $params.ForceApplicationShutdown = $true
    try {
        Add-AppxPackage @params
    } catch {
        if ($_.Exception.Message -match '0x80073D02') {
            throw 'Add-AppxPackage failed with 0x80073D02 even after -ForceApplicationShutdown. Close Microsoft Store manually, then retry.'
        }
        throw
    }
}

if ($missingDeps.Count -gt 0) {
    Write-InstallHost "  installing with $($missingDeps.Count) framework deps from $depsDir" Yellow
    Invoke-AddAppxWithRetry -Msix $msix -Deps $missingDeps
}
else {
    Invoke-AddAppxWithRetry -Msix $msix
}
Write-InstallHost '  ok: installed' Green

Write-InstallHost ''
Write-InstallHost 'All done.' Green
Write-InstallHost 'Usage:' Cyan
Write-InstallHost '  1. Start core-server:  .\target\release\core-server.exe' White
Write-InstallHost '  2. Open Game Bar:      Win+G -> Widget store -> ''Overlay Widget''' White
Write-InstallHost '  3. Or desktop monitor: .\target\release\desktop-window-monitor.exe' White
