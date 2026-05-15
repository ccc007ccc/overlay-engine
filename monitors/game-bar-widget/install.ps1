<#
.SYNOPSIS
    OverlayWidget Game Bar 小组件安装脚本。

.DESCRIPTION
    默认保留开发模式：msbuild + dev cert + signtool + Add-AppxPackage。
    -PackageOnly 用于 release staging：构建并签名 MSIX，不安装。
    -InstallOnly 用于 release 安装器：只安装已签名 MSIX，不依赖 MSBuild/signtool。
#>

[CmdletBinding()]
param(
    [ValidateSet('Debug', 'Release')]
    [string]$Configuration = 'Release',

    [ValidateSet('x64')]
    [string]$Platform = 'x64',

    [switch]$SkipBuild,
    [switch]$SkipInstall,
    [switch]$PackageOnly,
    [switch]$InstallOnly,

    [string]$MsixPath,
    [string]$DependencyDir,
    [switch]$TrustDevCertificate,
    [string]$DevCertPath,

    [ValidateSet('Dev', 'Pfx', 'None')]
    [string]$SignMode = 'Dev',
    [string]$PfxPath,
    [string]$PfxPassword,
    [string]$TimestampUrl
)

$ErrorActionPreference = 'Stop'

$WidgetRoot      = $PSScriptRoot
$Csproj          = Join-Path $WidgetRoot 'OverlayWidget.csproj'
$AppPackagesDir  = Join-Path $WidgetRoot 'AppPackages'
$DevPfxPath      = Join-Path $WidgetRoot 'OverlayWidget_Dev.pfx'
$DevCerPath      = Join-Path $WidgetRoot 'OverlayWidget_Dev.cer'
$CertSubject     = 'CN=OverlayWidget Dev'
$DevPfxPassword  = 'OverlayWidget'

function Write-Step([int]$n, [int]$total, [string]$msg) {
    Write-Host ''
    Write-Host "[$n/$total] $msg" -ForegroundColor Cyan
}

function Test-IsAdmin {
    $id = [System.Security.Principal.WindowsIdentity]::GetCurrent()
    $p  = New-Object System.Security.Principal.WindowsPrincipal($id)
    return $p.IsInRole([System.Security.Principal.WindowsBuiltInRole]::Administrator)
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

function Get-LatestMsix {
    $pattern = "_${Platform}\.msix$"
    $cand = Get-ChildItem -Path $AppPackagesDir -Filter 'OverlayWidget_*.msix' -Recurse -ErrorAction SilentlyContinue |
        Where-Object { $_.Name -match $pattern } |
        Sort-Object LastWriteTime -Descending |
        Select-Object -First 1
    if (-not $cand) { throw "MSIX not found under $AppPackagesDir for $Platform. Re-run without -SkipBuild first." }
    return $cand.FullName
}

function Ensure-DevCertificate {
    if (-not (Test-Path $DevPfxPath)) {
        Write-Host "  generating self-signed cert ($CertSubject)..." -ForegroundColor DarkGray
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
        $securePass = ConvertTo-SecureString -String $DevPfxPassword -Force -AsPlainText
        Export-PfxCertificate -Cert $cert -FilePath $DevPfxPath -Password $securePass | Out-Null
        Export-Certificate -Cert $cert -FilePath $DevCerPath | Out-Null
        Remove-Item -Path "Cert:\CurrentUser\My\$($cert.Thumbprint)" -Force
    }
    return @{ Pfx = $DevPfxPath; Password = $DevPfxPassword; Cer = $DevCerPath }
}

function Trust-DevCertificate {
    param(
        [string]$CertificatePath = $DevCerPath,
        [switch]$RequireAdmin
    )

    if (-not (Test-Path $CertificatePath)) { throw "Dev certificate not found: $CertificatePath" }

    Import-Module PKI -ErrorAction Stop
    $cert = [System.Security.Cryptography.X509Certificates.X509Certificate2]::new($CertificatePath)
    $existing = @(Get-ChildItem -Path 'Cert:\LocalMachine\TrustedPeople' -ErrorAction SilentlyContinue |
        Where-Object { $_.Thumbprint -eq $cert.Thumbprint })

    if ($existing.Count -eq 0) {
        if (-not (Test-IsAdmin)) {
            if ($RequireAdmin) { throw 'Admin rights are required to trust the OverlayWidget dev certificate.' }
            Write-Host '  skipping cert import (Admin rights required). If app install fails, run this script as Admin once.' -ForegroundColor Yellow
            return
        }
        Import-Certificate -FilePath $CertificatePath -CertStoreLocation 'Cert:\LocalMachine\TrustedPeople' | Out-Null
        Write-Host '  ok: imported' -ForegroundColor Green
    }
    else {
        Write-Host "  ok: already trusted (thumbprint=$($existing[0].Thumbprint))" -ForegroundColor Green
    }
}

function Invoke-BuildMsix {
    if (-not (Test-Path $Csproj)) { throw "csproj not found: $Csproj" }

    $msbuild = Resolve-MSBuild
    Write-Host "  using: $msbuild" -ForegroundColor DarkGray

    $msbArgs = @(
        $Csproj,
        "/p:Configuration=$Configuration",
        "/p:Platform=$Platform",
        '/p:AppxBundle=Never',
        "/p:AppxBundlePlatforms=$Platform",
        '/p:UapAppxPackageBuildMode=SideloadOnly',
        '/p:AppxPackageSigningEnabled=false',
        '/restore',
        '/m',
        '/t:Build',
        '/v:minimal'
    )

    & $msbuild @msbArgs
    if ($LASTEXITCODE -ne 0) { throw "msbuild failed: exit $LASTEXITCODE" }
}

function Invoke-SignMsix([string]$Path) {
    if ($SignMode -eq 'None') {
        Write-Host '  signing skipped (SignMode=None)' -ForegroundColor Yellow
        return
    }

    $signPfx = $PfxPath
    $signPassword = $PfxPassword
    if ($SignMode -eq 'Dev') {
        $dev = Ensure-DevCertificate
        $signPfx = $dev.Pfx
        $signPassword = $dev.Password
    }

    if (-not $signPfx) { throw '-PfxPath is required when SignMode=Pfx.' }
    if (-not (Test-Path $signPfx)) { throw "PFX not found: $signPfx" }

    $signtool = Resolve-SignTool
    $args = @('sign', '/fd', 'SHA256', '/f', $signPfx)
    if ($signPassword) { $args += @('/p', $signPassword) }
    if ($TimestampUrl) { $args += @('/tr', $TimestampUrl, '/td', 'SHA256') }
    $args += $Path

    & $signtool @args
    if ($LASTEXITCODE -ne 0) { throw "signtool failed: exit $LASTEXITCODE" }
}

function Test-FrameworkDepInstalled([string]$DepPath) {
    $pkgName = [System.IO.Path]::GetFileNameWithoutExtension($DepPath)
    return ($null -ne (Get-AppxPackage -Name $pkgName -ErrorAction SilentlyContinue))
}

function Invoke-AddAppxWithRetry([string]$Msix, [string[]]$Deps) {
    $params = @{ Path = $Msix; ErrorAction = 'Stop' }
    if ($Deps -and $Deps.Count -gt 0) { $params.DependencyPath = $Deps }

    try {
        Add-AppxPackage @params
        return
    } catch {
        if ($_.Exception.Message -notmatch '0x80073D02') { throw }
        Write-Host '  resource in use (0x80073D02), retrying with -ForceApplicationShutdown...' -ForegroundColor Yellow
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

function Invoke-InstallMsix([string]$Path, [string]$DepsDir) {
    if (-not (Test-Path $Path)) { throw "MSIX not found: $Path" }

    $existingPkg = Get-AppxPackage -Name 'OverlayWidget' -ErrorAction SilentlyContinue
    if ($existingPkg) {
        Write-Host "  removing existing $($existingPkg.PackageFullName)" -ForegroundColor Yellow
        Remove-AppxPackage -Package $existingPkg.PackageFullName
    }

    $depPaths = @()
    if ($DepsDir -and (Test-Path $DepsDir)) {
        $depPaths = Get-ChildItem -Path $DepsDir -Filter '*.appx' -ErrorAction SilentlyContinue | ForEach-Object { $_.FullName }
    }

    $missingDeps = @()
    foreach ($d in $depPaths) {
        if (Test-FrameworkDepInstalled $d) {
            Write-Host "  skip (already installed): $(Split-Path $d -Leaf)" -ForegroundColor DarkGray
        } else {
            $missingDeps += $d
        }
    }

    if ($missingDeps.Count -gt 0) {
        Write-Host "  installing with $($missingDeps.Count) framework deps from $DepsDir" -ForegroundColor Yellow
        Invoke-AddAppxWithRetry -Msix $Path -Deps $missingDeps
    }
    else {
        Invoke-AddAppxWithRetry -Msix $Path
    }
}

if ($InstallOnly) {
    if (-not $MsixPath) { throw '-MsixPath is required with -InstallOnly.' }
    if (-not $DependencyDir) { $DependencyDir = Join-Path (Split-Path -Parent $MsixPath) "Dependencies\$Platform" }
    if ($TrustDevCertificate -and -not $DevCertPath) { $DevCertPath = Join-Path (Split-Path -Parent $MsixPath) 'OverlayWidget_Dev.cer' }

    $totalSteps = if ($TrustDevCertificate) { 2 } else { 1 }
    $step = 0

    Write-Host 'OverlayWidget Widget InstallOnly' -ForegroundColor White
    if ($TrustDevCertificate) {
        $step++
        Write-Step $step $totalSteps 'trust certificate'
        Trust-DevCertificate -CertificatePath $DevCertPath -RequireAdmin
    }

    $step++
    Write-Step $step $totalSteps 'Add-AppxPackage'
    Invoke-InstallMsix -Path $MsixPath -DepsDir $DependencyDir
    Write-Host ''
    Write-Host 'Widget installation complete.' -ForegroundColor Green
    return
}

$totalSteps = 0
if (-not $SkipBuild) { $totalSteps++ }
if ($SkipInstall -and -not $PackageOnly) { }
else {
    if ($SignMode -ne 'None') { $totalSteps++ }
    if (-not $PackageOnly) {
        if ($SignMode -eq 'Dev') { $totalSteps++ }
        $totalSteps++
    }
}

if ($totalSteps -eq 0) {
    Write-Host 'Nothing to do (all steps skipped).' -ForegroundColor Yellow
    return
}

$step = 0
Write-Host 'OverlayWidget Widget-Only Install' -ForegroundColor White
Write-Host "  Configuration : $Configuration" -ForegroundColor DarkGray
Write-Host "  Platform      : $Platform" -ForegroundColor DarkGray
Write-Host "  SignMode      : $SignMode" -ForegroundColor DarkGray

if (-not $SkipBuild) {
    $step++
    Write-Step $step $totalSteps "msbuild OverlayWidget ($Configuration|$Platform)"
    Invoke-BuildMsix
    Write-Host '  ok: msbuild done' -ForegroundColor Green
}

$msix = if ($MsixPath) { $MsixPath } else { Get-LatestMsix }

if ($SkipInstall -and -not $PackageOnly) {
    Write-Host ''
    Write-Host 'Skip install (-SkipInstall set). MSIX produced.' -ForegroundColor Yellow
    return
}

if ($SignMode -ne 'None') {
    $step++
    Write-Step $step $totalSteps 'signtool sign'
    Write-Host "  $msix" -ForegroundColor DarkGray
    Invoke-SignMsix -Path $msix
    Write-Host '  ok: signed' -ForegroundColor Green
}

if ($PackageOnly) {
    Write-Host ''
    Write-Host "Package ready: $msix" -ForegroundColor Green
    return
}

if ($SignMode -eq 'Dev') {
    $step++
    Write-Step $step $totalSteps 'trust certificate'
    Trust-DevCertificate
}

$step++
Write-Step $step $totalSteps 'Add-AppxPackage'
if (-not $DependencyDir) { $DependencyDir = Join-Path (Split-Path -Parent $msix) "Dependencies\$Platform" }
Invoke-InstallMsix -Path $msix -DepsDir $DependencyDir
Write-Host '  ok: installed' -ForegroundColor Green

Write-Host ''
Write-Host 'Widget installation complete.' -ForegroundColor Green
Write-Host 'You can now open Xbox Game Bar (Win+G) to use the Overlay Widget.' -ForegroundColor Cyan
