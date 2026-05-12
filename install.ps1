<#
.SYNOPSIS
    OverlayWidget v1.0 一键安装（开发版）：cargo + msbuild + signtool + Add-AppxPackage。

.DESCRIPTION
    端到端流程，任一步失败立刻退出（$ErrorActionPreference='Stop'）：

      1. cargo build --release (core-server + desktop-window-monitor + renderer)
      2. msbuild OverlayWidget.csproj  -> AppPackages\..\*.msix
      3. 自签 dev 证书（首次自动生成，CN=OverlayWidget Dev）
      4. 导 cert 到 LocalMachine\TrustedPeople  <- 首次需要 Admin
      5. signtool sign MSIX
      6. 卸旧版 + Add-AppxPackage  -> 装到系统

    完成后：
      - 先启动 core-server：.\target\release\core-server.exe
      - 再打开 Xbox Game Bar (Win+G) -> Widget store -> 'Overlay Widget'
      - 或启动桌面监视器：.\target\release\desktop-window-monitor.exe
    卸载用 .\uninstall.ps1。

.PARAMETER Configuration
    Debug 或 Release，默认 Release。

.PARAMETER Platform
    目前只接受 x64。

.PARAMETER SkipRust
    跳过 cargo 编译。只改 C# 时省时间。

.PARAMETER SkipCSharp
    跳过 msbuild。只想确认 Rust 编得过时用。

.PARAMETER SkipInstall
    只编译不装。CI / 检视 MSIX 输出时用。

.PARAMETER Clean
    cargo clean + msbuild /t:Clean 后再编译。

.EXAMPLE
    .\install.ps1
    完整 Release 构建 + 安装。首次需要 Admin（导 cert）。

.EXAMPLE
    .\install.ps1 -SkipRust
    只重编 C# widget，复用上次的 Rust 产物。

.EXAMPLE
    .\install.ps1 -SkipRust -SkipCSharp
    跳过编译，直接签名+装当前 MSIX。
#>

[CmdletBinding()]
param(
    [ValidateSet('Debug', 'Release')]
    [string]$Configuration = 'Release',

    [ValidateSet('x64')]
    [string]$Platform = 'x64',

    [switch]$SkipRust,
    [switch]$SkipCSharp,
    [switch]$SkipInstall,
    [switch]$Clean
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

# ============================================================
# 工具
# ============================================================

function Write-Step([int]$n, [int]$total, [string]$msg) {
    Write-Host ""
    Write-Host "[$n/$total] $msg" -ForegroundColor Cyan
}

function Test-IsAdmin {
    $id = [System.Security.Principal.WindowsIdentity]::GetCurrent()
    $p  = New-Object System.Security.Principal.WindowsPrincipal($id)
    return $p.IsInRole([System.Security.Principal.WindowsBuiltInRole]::Administrator)
}

function Resolve-Cargo {
    $cmd = Get-Command cargo.exe -ErrorAction SilentlyContinue
    if (-not $cmd) {
        throw "cargo.exe not found in PATH. Install rustup from https://rustup.rs/"
    }
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
    throw "MSBuild.exe not found. Install Visual Studio with the UWP/MSIX workload."
}

function Resolve-SignTool {
    $candidates = @(
        "C:\Program Files (x86)\Windows Kits\10\bin\10.0.26100.0\x64\signtool.exe",
        "C:\Program Files (x86)\Windows Kits\10\bin\10.0.22621.0\x64\signtool.exe"
    )
    foreach ($p in $candidates) {
        if (Test-Path $p) { return $p }
    }
    throw "signtool.exe not found in any known Windows SDK path."
}

# ---- 步骤计数 ----
$totalSteps = 0
if (-not $SkipRust)    { $totalSteps++ }
if (-not $SkipCSharp)  { $totalSteps++ }
if (-not $SkipInstall) { $totalSteps += 4 }
if ($totalSteps -eq 0) {
    Write-Host "Nothing to do (all steps skipped)." -ForegroundColor Yellow
    return
}
$step = 0

Write-Host "OverlayWidget v1.0 install" -ForegroundColor White
Write-Host "  Configuration : $Configuration" -ForegroundColor DarkGray
Write-Host "  Platform      : $Platform" -ForegroundColor DarkGray
Write-Host "  Clean         : $Clean" -ForegroundColor DarkGray

# ============================================================
# Step 1: cargo build --release (all workspace members)
# ============================================================
if (-not $SkipRust) {
    $step++
    Write-Step $step $totalSteps "cargo build --release"

    $cargo = Resolve-Cargo
    Push-Location $ProjectRoot
    try {
        if ($Clean) {
            Write-Host "  cargo clean" -ForegroundColor DarkGray
            & $cargo clean
            if ($LASTEXITCODE -ne 0) { throw "cargo clean failed: exit $LASTEXITCODE" }
        }

        & $cargo build --release
        if ($LASTEXITCODE -ne 0) { throw "cargo build failed: exit $LASTEXITCODE" }
    }
    finally {
        Pop-Location
    }

    # 检查关键产物
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
        Write-Host ("  ok:`n" + ($products -join "`n")) -ForegroundColor Green
    }
}

# ============================================================
# Step 2: msbuild OverlayWidget.csproj -> MSIX
# ============================================================
if (-not $SkipCSharp) {
    $step++
    Write-Step $step $totalSteps "msbuild OverlayWidget ($Configuration|$Platform)"

    if (-not (Test-Path $Csproj)) { throw "csproj not found: $Csproj" }

    $msbuild = Resolve-MSBuild
    Write-Host "  using: $msbuild" -ForegroundColor DarkGray

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

    Write-Host "  ok: msbuild done" -ForegroundColor Green
}

# 后续 Step 3-6 都需要定位最新 MSIX
function Get-LatestMsix {
    $pattern = "_${Platform}\.msix$"
    $cand = Get-ChildItem -Path $AppPackagesDir -Filter 'OverlayWidget_*.msix' -Recurse -ErrorAction SilentlyContinue |
        Where-Object { $_.Name -match $pattern } |
        Sort-Object LastWriteTime -Descending |
        Select-Object -First 1
    if (-not $cand) {
        throw "MSIX not found under $AppPackagesDir for $Platform. Re-run without -SkipCSharp first."
    }
    return $cand.FullName
}

if ($SkipInstall) {
    Write-Host ""
    Write-Host "Skip install (-SkipInstall set). MSIX produced." -ForegroundColor Yellow
    return
}

# ============================================================
# Step 3: 自签 dev cert（必要时）
# ============================================================
$step++
Write-Step $step $totalSteps "dev certificate"

if (-not (Test-Path $PfxPath)) {
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
    $securePass = ConvertTo-SecureString -String $PfxPassword -Force -AsPlainText
    Export-PfxCertificate -Cert $cert -FilePath $PfxPath -Password $securePass | Out-Null
    Export-Certificate -Cert $cert -FilePath $CerPath | Out-Null
    Remove-Item -Path "Cert:\CurrentUser\My\$($cert.Thumbprint)" -Force
    Write-Host "  ok: PFX -> $PfxPath" -ForegroundColor Green
}
else {
    Write-Host "  ok: $PfxPath" -ForegroundColor Green
}

# ============================================================
# Step 4: cert 导入 LocalMachine\TrustedPeople（必要时，需 Admin）
# ============================================================
$step++
Write-Step $step $totalSteps "trust certificate"

Import-Module PKI -ErrorAction Stop
$existing = @(Get-ChildItem -Path 'Cert:\LocalMachine\TrustedPeople' -ErrorAction SilentlyContinue |
    Where-Object { $_.Subject -eq $CertSubject })

if ($existing.Count -eq 0) {
    if (-not (Test-IsAdmin)) {
        throw "Admin rights required to import cert into LocalMachine\TrustedPeople. Re-run from elevated PowerShell."
    }
    Import-Certificate -FilePath $CerPath -CertStoreLocation 'Cert:\LocalMachine\TrustedPeople' | Out-Null
    Write-Host "  ok: imported" -ForegroundColor Green
}
else {
    Write-Host "  ok: already trusted (thumbprint=$($existing[0].Thumbprint))" -ForegroundColor Green
}

# ============================================================
# Step 5: signtool sign
# ============================================================
$step++
Write-Step $step $totalSteps "signtool sign"

$msix = Get-LatestMsix
Write-Host "  $msix" -ForegroundColor DarkGray
$signtool = Resolve-SignTool
& $signtool sign /fd SHA256 /f $PfxPath /p $PfxPassword $msix
if ($LASTEXITCODE -ne 0) { throw "signtool failed: exit $LASTEXITCODE" }
Write-Host "  ok: signed" -ForegroundColor Green

# ============================================================
# Step 6: Remove old + Add-AppxPackage
# ============================================================
$step++
Write-Step $step $totalSteps "Add-AppxPackage"

$existingPkg = Get-AppxPackage -Name 'OverlayWidget' -ErrorAction SilentlyContinue
if ($existingPkg) {
    Write-Host "  removing existing $($existingPkg.PackageFullName)" -ForegroundColor Yellow
    Remove-AppxPackage -Package $existingPkg.PackageFullName
}

$msixDir = Split-Path -Parent $msix
$depsDir = Join-Path $msixDir "Dependencies\$Platform"
$depPaths = @()
if (Test-Path $depsDir) {
    $depPaths = Get-ChildItem -Path $depsDir -Filter '*.appx' -ErrorAction SilentlyContinue |
        ForEach-Object { $_.FullName }
}

function Test-FrameworkDepInstalled {
    param([string]$DepPath)
    $pkgName = [System.IO.Path]::GetFileNameWithoutExtension($DepPath)
    return ($null -ne (Get-AppxPackage -Name $pkgName -ErrorAction SilentlyContinue))
}

$missingDeps = @()
foreach ($d in $depPaths) {
    if (Test-FrameworkDepInstalled $d) {
        Write-Host "  skip (already installed): $(Split-Path $d -Leaf)" -ForegroundColor DarkGray
    } else {
        $missingDeps += $d
    }
}

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
            throw "Add-AppxPackage failed with 0x80073D02 even after -ForceApplicationShutdown. Close Microsoft Store manually, then retry."
        }
        throw
    }
}

if ($missingDeps.Count -gt 0) {
    Write-Host "  installing with $($missingDeps.Count) framework deps from $depsDir" -ForegroundColor Yellow
    Invoke-AddAppxWithRetry -Msix $msix -Deps $missingDeps
}
else {
    Invoke-AddAppxWithRetry -Msix $msix
}
Write-Host "  ok: installed" -ForegroundColor Green

Write-Host ""
Write-Host "All done." -ForegroundColor Green
Write-Host "Usage:" -ForegroundColor Cyan
Write-Host "  1. Start core-server:  .\target\release\core-server.exe" -ForegroundColor White
Write-Host "  2. Open Game Bar:      Win+G -> Widget store -> 'Overlay Widget'" -ForegroundColor White
Write-Host "  3. Or desktop monitor: .\target\release\desktop-window-monitor.exe" -ForegroundColor White
