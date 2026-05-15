<#
.SYNOPSIS
    overlay-engine 卸载脚本：默认开发卸载 MSIX；-Release 时卸载发布版安装目录和系统集成。
#>

[CmdletBinding()]
param(
    [switch]$Release,
    [string]$InstallDir = (Join-Path $env:LOCALAPPDATA 'Programs\overlay-engine'),
    [switch]$RemoveWidget,
    [switch]$RemoveCert,
    [switch]$AllUsers,
    [switch]$RemoveUserData,
    [switch]$Quiet
)

$ErrorActionPreference = 'Stop'
$ScheduledTaskName = 'overlay-engine Core'
$AppName = 'overlay-engine'
$StartupShortcutName = 'overlay-engine.lnk'

function Write-UninstallHost([string]$Message, [ConsoleColor]$Color = [ConsoleColor]::Gray) {
    if (-not $Quiet) { Write-Host $Message -ForegroundColor $Color }
}

function Test-IsAdmin {
    $id = [System.Security.Principal.WindowsIdentity]::GetCurrent()
    $p  = New-Object System.Security.Principal.WindowsPrincipal($id)
    return $p.IsInRole([System.Security.Principal.WindowsBuiltInRole]::Administrator)
}

function Stop-InstalledProcess([string]$Name, [string]$RootDir) {
    if (-not $RootDir) { return }
    $rootFull = [System.IO.Path]::GetFullPath($RootDir).TrimEnd('\')
    $targets = New-Object 'System.Collections.Generic.List[int]'
    $filter = "Name = '$Name.exe'"

    foreach ($p in @(Get-CimInstance Win32_Process -Filter $filter -ErrorAction SilentlyContinue)) {
        if (-not $p.ExecutablePath) { continue }
        $full = [System.IO.Path]::GetFullPath($p.ExecutablePath)
        if ($full.StartsWith($rootFull, [System.StringComparison]::OrdinalIgnoreCase) -and -not $targets.Contains([int]$p.ProcessId)) {
            $targets.Add([int]$p.ProcessId)
        }
    }

    foreach ($p in @(Get-Process -Name $Name -ErrorAction SilentlyContinue)) {
        $path = $null
        try { $path = $p.MainModule.FileName } catch { }
        if (-not $path) { continue }
        $full = [System.IO.Path]::GetFullPath($path)
        if ($full.StartsWith($rootFull, [System.StringComparison]::OrdinalIgnoreCase) -and -not $targets.Contains([int]$p.Id)) {
            $targets.Add([int]$p.Id)
        }
    }

    foreach ($id in $targets) {
        Write-UninstallHost "  stopping $Name pid=$id" Yellow
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

function Remove-OverlayWidgetMsix([bool]$All) {
    $getParams = @{ Name = '*OverlayWidget*'; ErrorAction = 'SilentlyContinue' }
    if ($All) {
        if (-not (Test-IsAdmin)) { throw '-AllUsers requires Admin. Re-run from an elevated PowerShell.' }
        $getParams.AllUsers = $true
    }

    $pkgs = @(Get-AppxPackage @getParams)
    if ($pkgs.Count -eq 0) {
        Write-UninstallHost '  no OverlayWidget packages installed.' DarkGray
        return
    }

    foreach ($pkg in $pkgs) {
        Write-UninstallHost "  removing $($pkg.PackageFullName)" Yellow
        if ($All) { Remove-AppxPackage -Package $pkg.PackageFullName -AllUsers }
        else { Remove-AppxPackage -Package $pkg.PackageFullName }
    }
    Write-UninstallHost "  removed $($pkgs.Count) package(s)." Green
}

function Remove-OverlayShortcuts([string]$RootDir) {
    $desktop = [Environment]::GetFolderPath('DesktopDirectory')
    $desktopLink = Join-Path $desktop 'overlay-engine.lnk'
    if (Test-Path $desktopLink) { Remove-Item $desktopLink -Force }

    $startup = [Environment]::GetFolderPath('Startup')
    $startupLink = Join-Path $startup 'overlay-engine.lnk'
    if (Test-Path $startupLink) { Remove-Item $startupLink -Force }

    $programs = [Environment]::GetFolderPath('Programs')
    $group = Join-Path $programs 'overlay-engine'
    if (Test-Path $group) { Remove-Item $group -Recurse -Force }
}

function Remove-UninstallRegistry {
    $key = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\overlay-engine'
    if (Test-Path $key) { Remove-Item $key -Recurse -Force }
}

function Remove-RegistryValueIfExists([string]$Path, [string]$Name) {
    if (Get-ItemProperty -Path $Path -Name $Name -ErrorAction SilentlyContinue) {
        Remove-ItemProperty -Path $Path -Name $Name -Force
    }
}

function Remove-OverlayAutoStart {
    if (Get-ScheduledTask -TaskName $ScheduledTaskName -ErrorAction SilentlyContinue) {
        Write-UninstallHost '  removing scheduled task' Yellow
        Unregister-ScheduledTask -TaskName $ScheduledTaskName -Confirm:$false
    }

    Remove-RegistryValueIfExists -Path 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run' -Name $AppName
    Remove-RegistryValueIfExists -Path 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run' -Name $AppName
    Remove-RegistryValueIfExists -Path 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\StartupFolder' -Name $StartupShortcutName
}

function Remove-OverlayCertFromState($State) {
    if (-not $RemoveCert) { return }
    if (-not (Test-IsAdmin)) { throw '-RemoveCert requires Admin. Re-run from an elevated PowerShell.' }

    $thumb = $null
    if ($State -and $State.certificateThumbprint) { $thumb = [string]$State.certificateThumbprint }
    if ($thumb) {
        $path = "Cert:\LocalMachine\TrustedPeople\$thumb"
        if (Test-Path $path) {
            Write-UninstallHost "  removing cert $thumb" Yellow
            Remove-Item -Path $path -Force
        }
        return
    }

    $subject = 'CN=OverlayWidget Dev'
    $hits = @(Get-ChildItem -Path Cert:\LocalMachine\TrustedPeople -ErrorAction SilentlyContinue |
        Where-Object { $_.Subject -eq $subject })
    foreach ($c in $hits) {
        Write-UninstallHost "  removing dev cert $($c.Thumbprint)" Yellow
        Remove-Item -Path "Cert:\LocalMachine\TrustedPeople\$($c.Thumbprint)" -Force
    }
}

function Invoke-ReleaseUninstall {
    $InstallDir = [System.IO.Path]::GetFullPath($InstallDir)
    $statePath = Join-Path $InstallDir 'install-state.json'
    $state = $null
    if (Test-Path $statePath) {
        try { $state = Get-Content -Path $statePath -Raw | ConvertFrom-Json } catch { }
    }

    Write-UninstallHost 'overlay-engine Release Uninstall' White
    Write-UninstallHost "  InstallDir : $InstallDir" DarkGray

    Remove-OverlayAutoStart

    Stop-InstalledProcess -Name 'core-server' -RootDir $InstallDir
    Stop-InstalledProcess -Name 'desktop-window-monitor' -RootDir $InstallDir

    $hasWidget = $RemoveWidget
    if ($state -and $state.msixPackageName) { $hasWidget = $true }
    if ($hasWidget) {
        Write-UninstallHost '[MSIX] Removing Game Bar widget...' Cyan
        Remove-OverlayWidgetMsix -All:$AllUsers
    }

    Remove-OverlayCertFromState -State $state
    Remove-OverlayShortcuts -RootDir $InstallDir
    Remove-UninstallRegistry

    if (Test-Path $InstallDir) {
        Write-UninstallHost '  removing install directory' Yellow
        Remove-Item -Path $InstallDir -Recurse -Force
    }

    if ($RemoveUserData) {
        $dataDir = Join-Path $env:LOCALAPPDATA 'overlay-engine'
        if (Test-Path $dataDir) { Remove-Item $dataDir -Recurse -Force }
    }

    Write-UninstallHost ''
    Write-UninstallHost 'Release uninstall complete.' Green
}

if ($Release) {
    Invoke-ReleaseUninstall
    return
}

Write-UninstallHost '[1/2] Removing OverlayWidget MSIX packages...' Cyan
Remove-OverlayWidgetMsix -All:$AllUsers

if ($RemoveCert) {
    Write-UninstallHost '[2/2] Removing dev cert from LocalMachine\TrustedPeople...' Cyan
    if (-not (Test-IsAdmin)) { throw '-RemoveCert requires Admin. Re-run from an elevated PowerShell.' }

    $subject = 'CN=OverlayWidget Dev'
    $hits = @(Get-ChildItem -Path Cert:\LocalMachine\TrustedPeople -ErrorAction SilentlyContinue |
        Where-Object { $_.Subject -eq $subject })

    if ($hits.Count -eq 0) {
        Write-UninstallHost '  no matching cert.' DarkGray
    }
    else {
        foreach ($c in $hits) {
            Write-UninstallHost "  removing $($c.Thumbprint)" Yellow
            Remove-Item -Path "Cert:\LocalMachine\TrustedPeople\$($c.Thumbprint)" -Force
        }
        Write-UninstallHost "  removed $($hits.Count) cert(s)." Green
    }
}
else {
    Write-UninstallHost '[2/2] Keeping dev cert (pass -RemoveCert to delete).' DarkGray
}

Write-UninstallHost ''
Write-UninstallHost 'Uninstall complete.' Green
Write-UninstallHost "Verify: Settings -> Apps -> search 'Overlay Widget' should return nothing." DarkGray
