<#
.SYNOPSIS
    OverlayWidget one-shot dev install: cert + sign + Add-AppxPackage.

.DESCRIPTION
    Steps:
      1. If OverlayWidget_Dev.pfx is missing, generate a self-signed cert
         (CN=OverlayWidget Dev) and export PFX + CER.
      2. If the cert is not yet in LocalMachine\TrustedPeople, import it.
         This step requires admin rights.
      3. signtool sign the MSIX.
      4. Remove old package (if any), then Add-AppxPackage the new one.

    Subject "CN=OverlayWidget Dev" must match the Publisher in
    Package.appxmanifest exactly (prompt section 10, pitfall 5).

.NOTES
    First run requires elevated PowerShell.
    After that, if the cert is already trusted, plain user works.
#>

[CmdletBinding()]
param(
    [string]$Configuration = "Debug",
    [string]$Platform = "x64",
    [string]$Subject = "CN=OverlayWidget Dev",
    [string]$PfxPassword = "OverlayWidget"
)

$ErrorActionPreference = "Stop"
$ProjectRoot = Split-Path -Parent $PSScriptRoot
$PfxPath = Join-Path $ProjectRoot "OverlayWidget_Dev.pfx"
$CerPath = Join-Path $ProjectRoot "OverlayWidget_Dev.cer"
$AppPackagesDir = Join-Path $ProjectRoot "AppPackages"

# PKI module provides Cert: PSDrive + New-SelfSignedCertificate.
# Import explicitly so -NoProfile / restricted shells work.
Import-Module PKI -ErrorAction Stop

function Test-IsAdmin {
    $id = [System.Security.Principal.WindowsIdentity]::GetCurrent()
    $p = New-Object System.Security.Principal.WindowsPrincipal($id)
    return $p.IsInRole([System.Security.Principal.WindowsBuiltInRole]::Administrator)
}

function Resolve-SignTool {
    $candidates = @(
        "C:\Program Files (x86)\Windows Kits\10\bin\10.0.26100.0\x64\signtool.exe",
        "C:\Program Files (x86)\Windows Kits\10\bin\10.0.22621.0\x64\signtool.exe"
    )
    foreach ($p in $candidates) {
        if (Test-Path $p) { return $p }
    }
    throw "signtool.exe not found in any known Windows SDK path"
}

# Step 1: generate certificate
if (-not (Test-Path $PfxPath)) {
    Write-Host "[1/5] Generating self-signed certificate ($Subject)..." -ForegroundColor Cyan
    $certParams = @{
        Type              = 'CodeSigningCert'
        Subject           = $Subject
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

    Write-Host "  PFX -> $PfxPath" -ForegroundColor Green
    Write-Host "  CER -> $CerPath" -ForegroundColor Green
}
else {
    Write-Host "[1/5] Certificate exists: $PfxPath" -ForegroundColor Green
}

# Step 2: import cert to TrustedPeople (admin)
$existing = Get-ChildItem -Path "Cert:\LocalMachine\TrustedPeople" -ErrorAction SilentlyContinue |
    Where-Object { $_.Subject -eq $Subject }

if ($existing.Count -eq 0) {
    if (-not (Test-IsAdmin)) {
        throw "Admin rights required to import cert into LocalMachine\TrustedPeople. Re-run from elevated PowerShell."
    }
    Write-Host "[2/5] Importing cert to LocalMachine\TrustedPeople..." -ForegroundColor Cyan
    Import-Certificate -FilePath $CerPath -CertStoreLocation "Cert:\LocalMachine\TrustedPeople" | Out-Null
    Write-Host "  imported" -ForegroundColor Green
}
else {
    Write-Host "[2/5] Certificate already trusted (thumbprint=$($existing[0].Thumbprint))" -ForegroundColor Green
}

# Step 3: locate MSIX
Write-Host "[3/5] Locating MSIX..." -ForegroundColor Cyan
# UWP SideloadOnly 模式下 AppPackages 目录后缀只有 `_Test`（不带 Configuration），
# eg. `OverlayWidget_0.1.0.0_x64_Test\OverlayWidget_0.1.0.0_x64.msix`。
# 所以按 .msix 文件名的 `_<Platform>.msix` 后缀匹配，再按 LastWriteTime 取最新一次构建。
# Configuration 区分由 msbuild 决定 MSIX 内容，不影响文件路径选择。
$pattern = "_${Platform}\.msix$"
$msixCandidates = Get-ChildItem -Path $AppPackagesDir -Filter "OverlayWidget_*.msix" -Recurse -ErrorAction SilentlyContinue |
    Where-Object { $_.Name -match $pattern } |
    Sort-Object LastWriteTime -Descending

if (-not $msixCandidates -or $msixCandidates.Count -eq 0) {
    throw "MSIX not found under $AppPackagesDir for $Platform. Run MSBuild first."
}
$msix = $msixCandidates[0].FullName
Write-Host "  $msix" -ForegroundColor Green

# Step 4: signtool
Write-Host "[4/5] Signing MSIX..." -ForegroundColor Cyan
$signtool = Resolve-SignTool
& $signtool sign /fd SHA256 /f $PfxPath /p $PfxPassword $msix
if ($LASTEXITCODE -ne 0) {
    throw "signtool failed with exit code $LASTEXITCODE"
}
Write-Host "  signed" -ForegroundColor Green

# Step 5: remove old + install new (with framework dependencies)
Write-Host "[5/5] Installing MSIX..." -ForegroundColor Cyan
$existingPkg = Get-AppxPackage -Name "OverlayWidget" -ErrorAction SilentlyContinue
if ($existingPkg) {
    Write-Host "  removing existing package $($existingPkg.PackageFullName)" -ForegroundColor Yellow
    Remove-AppxPackage -Package $existingPkg.PackageFullName
}

# Debug build depends on Microsoft.NET.CoreRuntime/CoreFramework/VCLibs framework packages.
# Release build (UseDotNetNativeToolchain=true) compiles to native and does not need them.
$msixDir = Split-Path -Parent $msix
$depsDir = Join-Path $msixDir "Dependencies\$Platform"
$depPaths = @()
if (Test-Path $depsDir) {
    $depPaths = Get-ChildItem -Path $depsDir -Filter "*.appx" -ErrorAction SilentlyContinue |
        ForEach-Object { $_.FullName }
}

# Skip framework deps that are already installed at-or-above the bundled version.
# Most Windows machines have these via Windows Update; trying to (re)install them while
# Store / StorePurchaseApp has them loaded triggers HRESULT 0x80073D02 (RESOURCE_IN_USE).
# 文件名形如 Microsoft.NET.Native.Runtime.2.2.appx → package name = filename without ext。
function Test-FrameworkDepInstalled {
    param([string]$DepPath)
    $pkgName = [System.IO.Path]::GetFileNameWithoutExtension($DepPath)
    $existing = Get-AppxPackage -Name $pkgName -ErrorAction SilentlyContinue
    return ($null -ne $existing)
}

$missingDeps = @()
foreach ($d in $depPaths) {
    if (Test-FrameworkDepInstalled $d) {
        Write-Host "  skip (already installed): $(Split-Path $d -Leaf)" -ForegroundColor DarkGray
    } else {
        $missingDeps += $d
    }
}

# 0x80073D02 重试：先正常装；遇 RESOURCE_IN_USE 改 -ForceApplicationShutdown 再来一次。
# Add-AppxPackage 的 HResult / Message 都带 0x80073D02，按 Message 匹配最稳。
function Invoke-AddAppxWithRetry {
    param(
        [string]$Msix,
        [string[]]$Deps
    )
    $params = @{ Path = $Msix; ErrorAction = 'Stop' }
    if ($Deps -and $Deps.Count -gt 0) { $params.DependencyPath = $Deps }

    try {
        Add-AppxPackage @params
        return
    } catch {
        if ($_.Exception.Message -notmatch '0x80073D02') { throw }
        Write-Host "  resource in use (0x80073D02), retrying with -ForceApplicationShutdown..." -ForegroundColor Yellow
    }

    $params.ForceApplicationShutdown = $true
    try {
        Add-AppxPackage @params
    } catch {
        if ($_.Exception.Message -match '0x80073D02') {
            throw "Add-AppxPackage failed with 0x80073D02 even after -ForceApplicationShutdown. Close Microsoft Store manually (or run 'wsreset'), then retry."
        }
        throw
    }
}

if ($missingDeps.Count -gt 0) {
    Write-Host "  installing with $($missingDeps.Count) framework dependencies from $depsDir" -ForegroundColor Yellow
    Invoke-AddAppxWithRetry -Msix $msix -Deps $missingDeps
}
else {
    Invoke-AddAppxWithRetry -Msix $msix
}
Write-Host "  installed" -ForegroundColor Green
Write-Host ""
Write-Host "Done. Open Xbox Game Bar (Win+G) -> Widget store -> look for 'Overlay Widget'." -ForegroundColor Cyan
