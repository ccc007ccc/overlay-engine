<#
.SYNOPSIS
    Build overlay-engine release staging payload.
#>

[CmdletBinding()]
param(
    [ValidateSet('Release')]
    [string]$Configuration = 'Release',

    [ValidateSet('x64')]
    [string]$Platform = 'x64',

    [string]$Version = '0.1.0',

    [ValidateSet('Dev', 'Pfx', 'None')]
    [string]$SignMode = 'Dev',
    [string]$PfxPath,
    [string]$PfxPassword,
    [string]$TimestampUrl,

    [switch]$SkipWidget,
    [switch]$Clean
)

$ErrorActionPreference = 'Stop'

$ProjectRoot = Split-Path -Parent $PSScriptRoot
$DistRoot = Join-Path $ProjectRoot 'dist'
$StageDir = Join-Path $DistRoot "overlay-engine-$Version-$Platform"
$AppDir = Join-Path $StageDir 'app'
$WidgetDir = Join-Path $StageDir 'widget'
$ScriptsDir = Join-Path $StageDir 'scripts'

function Write-Step([string]$Message) {
    Write-Host ''
    Write-Host $Message -ForegroundColor Cyan
}

function Copy-Required([string]$Source, [string]$Destination) {
    if (-not (Test-Path $Source)) { throw "Required artifact missing: $Source" }
    $parent = Split-Path -Parent $Destination
    if (-not (Test-Path $parent)) { New-Item -ItemType Directory -Path $parent -Force | Out-Null }
    Copy-Item -Path $Source -Destination $Destination -Force
}

Write-Host 'overlay-engine Release Build' -ForegroundColor White
Write-Host "  Version       : $Version" -ForegroundColor DarkGray
Write-Host "  Platform      : $Platform" -ForegroundColor DarkGray
Write-Host "  StageDir      : $StageDir" -ForegroundColor DarkGray

Write-Step '[1/5] cargo build allowlist'
Push-Location $ProjectRoot
try {
    if ($Clean) { cargo clean }
    cargo build --release -p core-server --bin core-server
    if ($LASTEXITCODE -ne 0) { throw "cargo build core-server failed: exit $LASTEXITCODE" }
    cargo build --release -p desktop-window-monitor --bin desktop-window-monitor
    if ($LASTEXITCODE -ne 0) { throw "cargo build desktop-window-monitor failed: exit $LASTEXITCODE" }
    cargo build --release -p renderer --lib
    if ($LASTEXITCODE -ne 0) { throw "cargo build renderer failed: exit $LASTEXITCODE" }
}
finally {
    Pop-Location
}

Write-Step '[2/5] prepare staging directories'
if (Test-Path $StageDir) { Remove-Item -Path $StageDir -Recurse -Force }
New-Item -ItemType Directory -Path $AppDir, $WidgetDir, $ScriptsDir -Force | Out-Null

Write-Step '[3/5] copy allowlisted app artifacts'
$ReleaseDir = Join-Path $ProjectRoot 'target\release'
Copy-Required -Source (Join-Path $ReleaseDir 'core-server.exe') -Destination (Join-Path $AppDir 'core-server.exe')
Copy-Required -Source (Join-Path $ReleaseDir 'desktop-window-monitor.exe') -Destination (Join-Path $AppDir 'desktop-window-monitor.exe')
$renderer = Join-Path $ReleaseDir 'renderer.dll'
if (Test-Path $renderer) { Copy-Required -Source $renderer -Destination (Join-Path $AppDir 'renderer.dll') }

Write-Step '[4/5] build/package Game Bar widget'
$widgetInstall = Join-Path $ProjectRoot 'monitors\game-bar-widget\install.ps1'
if (-not $SkipWidget) {
    $args = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $widgetInstall, '-PackageOnly', '-Configuration', $Configuration, '-Platform', $Platform, '-SignMode', $SignMode)
    if ($PfxPath) { $args += @('-PfxPath', $PfxPath) }
    if ($PfxPassword) { $args += @('-PfxPassword', $PfxPassword) }
    if ($TimestampUrl) { $args += @('-TimestampUrl', $TimestampUrl) }
    & powershell.exe @args
    if ($LASTEXITCODE -ne 0) { throw "widget package build failed: exit $LASTEXITCODE" }

    $msix = Get-ChildItem -Path (Join-Path $ProjectRoot 'monitors\game-bar-widget\AppPackages') -Filter 'OverlayWidget_*.msix' -Recurse |
        Where-Object { $_.Name -match "_${Platform}\.msix$" } |
        Sort-Object LastWriteTime -Descending |
        Select-Object -First 1
    if (-not $msix) { throw 'Widget MSIX not found after package build.' }
    Copy-Required -Source $msix.FullName -Destination (Join-Path $WidgetDir $msix.Name)
    if ($SignMode -eq 'Dev') {
        Copy-Required -Source (Join-Path $ProjectRoot 'monitors\game-bar-widget\OverlayWidget_Dev.cer') -Destination (Join-Path $WidgetDir 'OverlayWidget_Dev.cer')
    }

    $deps = Join-Path (Split-Path -Parent $msix.FullName) "Dependencies\$Platform"
    if (Test-Path $deps) {
        $depsDest = Join-Path $WidgetDir "Dependencies\$Platform"
        New-Item -ItemType Directory -Path $depsDest -Force | Out-Null
        Copy-Item -Path (Join-Path $deps '*') -Destination $depsDest -Recurse -Force
    }
}

Write-Step '[5/5] copy scripts and write manifest'
Copy-Required -Source (Join-Path $ProjectRoot 'install.ps1') -Destination (Join-Path $ScriptsDir 'install.ps1')
Copy-Required -Source (Join-Path $ProjectRoot 'uninstall.ps1') -Destination (Join-Path $ScriptsDir 'uninstall.ps1')
Copy-Required -Source $widgetInstall -Destination (Join-Path $ScriptsDir 'game-bar-widget-install.ps1')

$forbidden = @('demo-app.exe', 'demo-consumer.exe', 'desktop-demo-producer.exe', 'demo-producer.exe')
$foundForbidden = @()
foreach ($name in $forbidden) {
    $hits = @(Get-ChildItem -Path $StageDir -Filter $name -Recurse -ErrorAction SilentlyContinue)
    if ($hits.Count -gt 0) { $foundForbidden += $hits.FullName }
}
$patternHits = @(Get-ChildItem -Path $StageDir -Recurse -File -ErrorAction SilentlyContinue |
    Where-Object { $_.Name -like 'spike-*.exe' -or $_.Name -like 'diag-*.exe' })
if ($patternHits.Count -gt 0) { $foundForbidden += $patternHits.FullName }
if ($foundForbidden.Count -gt 0) { throw "Forbidden release files found:`n$($foundForbidden -join "`n")" }

$manifest = [ordered]@{
    name = 'overlay-engine'
    version = $Version
    platform = $Platform
    createdAt = (Get-Date).ToString('o')
    files = [ordered]@{
        core = 'app/core-server.exe'
        desktopMonitor = 'app/desktop-window-monitor.exe'
        renderer = if (Test-Path (Join-Path $AppDir 'renderer.dll')) { 'app/renderer.dll' } else { $null }
        gameBarMsix = if (-not $SkipWidget) { 'widget/' + (Get-ChildItem -Path $WidgetDir -Filter 'OverlayWidget_*.msix' | Select-Object -First 1).Name } else { $null }
    }
}
$manifest | ConvertTo-Json -Depth 5 | Set-Content -Path (Join-Path $StageDir 'manifest.json') -Encoding UTF8

Write-Host ''
Write-Host "Release staging ready: $StageDir" -ForegroundColor Green
