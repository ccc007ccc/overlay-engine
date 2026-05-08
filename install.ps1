<#
.SYNOPSIS
    OverlayWidget 一键安装（开发版）：cargo + msbuild + signtool + Add-AppxPackage。

.DESCRIPTION
    端到端流程，任一步失败立刻退出（$ErrorActionPreference='Stop'）：

      1. cargo build -p renderer --release         -> target\release\renderer.dll
      2. msbuild OverlayWidget.csproj              -> AppPackages\..\*.msix
      3. 自签 dev 证书（首次自动生成，CN=OverlayWidget Dev）
      4. 导 cert 到 LocalMachine\TrustedPeople     <- 首次需要 Admin
      5. signtool sign MSIX
      6. 卸旧版 + Add-AppxPackage                  -> 装到系统

    完成后 Xbox Game Bar (Win+G) -> Widget store 能看到 "Overlay Widget"。
    卸载用 .\uninstall.ps1。

.PARAMETER Configuration
    Debug 或 Release，默认 Release。
    Release 启用 .NET Native AOT，体积大、启动快、不依赖框架包。
    Debug 走标准 IL，需要 Microsoft.NET.CoreRuntime/CoreFramework framework appx 配套
    （脚本会自动从 AppPackages 旁边的 Dependencies 目录捎带）。

.PARAMETER Platform
    目前只接受 x64。

.PARAMETER SkipRust
    跳过 cargo 编译。改 C# 不动 Rust 时省时间。

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
    .\install.ps1 -Configuration Debug -SkipRust
    Debug 重 build C#，复用上次的 renderer.dll。

.EXAMPLE
    .\install.ps1 -SkipRust -SkipCSharp
    跳过编译，直接签名+装当前 MSIX。

.NOTES
    要求：rustup/cargo 在 PATH；Visual Studio 装 UWP/MSIX 工作负载（vswhere 找 MSBuild）；
    Windows SDK 提供 signtool.exe（脚本硬编码 26100/22621 两个 fallback 路径）。
    csproj <Content Include="..\target\release\renderer.dll"> 写死 release 路径，
    -Configuration Debug 也用 release renderer.dll，符合预期。
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
$RustDir         = Join-Path $ProjectRoot 'rust-renderer'
$CsharpDir       = Join-Path $ProjectRoot 'monitors\game-bar-widget'
$Csproj          = Join-Path $CsharpDir 'OverlayWidget.csproj'
$AppPackagesDir  = Join-Path $CsharpDir 'AppPackages'
# v0.7 起 workspace 根 target/，cargo 不再写 rust-renderer/target/。
# csproj 里 <Content Include="..\target\release\renderer.dll"> 也是这个路径。
$RendererDll     = Join-Path $ProjectRoot 'target\release\renderer.dll'
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
    # vswhere 是 MS 官方 VS 定位工具，跨大版本可靠。Windows PowerShell 5.1 的
    # `${env:ProgramFiles(x86)}` 语法会因括号被识别成调用而失败，改走 [Environment]。
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
    throw "MSBuild.exe not found. Install Visual Studio with the UWP/MSIX workload, or run from a Developer PowerShell."
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
if (-not $SkipInstall) { $totalSteps += 4 }   # cert生成 + cert导入 + sign + AddAppx
if ($totalSteps -eq 0) {
    Write-Host "Nothing to do (all steps skipped)." -ForegroundColor Yellow
    return
}
$step = 0

Write-Host "OverlayWidget install" -ForegroundColor White
Write-Host "  Configuration : $Configuration" -ForegroundColor DarkGray
Write-Host "  Platform      : $Platform" -ForegroundColor DarkGray
Write-Host "  Clean         : $Clean" -ForegroundColor DarkGray

# ============================================================
# Step 1: cargo build --release
# ============================================================
if (-not $SkipRust) {
    $step++
    Write-Step $step $totalSteps "cargo build -p renderer --release"

    $cargo = Resolve-Cargo
    Push-Location $ProjectRoot
    try {
        if ($Clean) {
            Write-Host "  cargo clean -p renderer" -ForegroundColor DarkGray
            & $cargo clean -p renderer
            if ($LASTEXITCODE -ne 0) { throw "cargo clean failed: exit $LASTEXITCODE" }
        }

        & $cargo build -p renderer --release
        if ($LASTEXITCODE -ne 0) { throw "cargo build failed: exit $LASTEXITCODE" }
    }
    finally {
        Pop-Location
    }

    if (-not (Test-Path $RendererDll)) {
        throw "cargo succeeded but $RendererDll missing. Check Cargo.toml [lib] crate-type contains cdylib."
    }
    $size = (Get-Item $RendererDll).Length
    Write-Host ("  ok: renderer.dll ({0:N0} bytes)" -f $size) -ForegroundColor Green
}

# ============================================================
# Step 2: msbuild OverlayWidget.csproj -> MSIX
# ============================================================
if (-not $SkipCSharp) {
    $step++
    Write-Step $step $totalSteps "msbuild OverlayWidget ($Configuration|$Platform)"

    if (-not (Test-Path $Csproj)) { throw "csproj not found: $Csproj" }

    if (-not (Test-Path $RendererDll)) {
        throw "renderer.dll missing at $RendererDll. Re-run without -SkipRust first."
    }

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
    # UWP SideloadOnly 模式下 AppPackages 目录后缀只有 `_Test`（不带 Configuration），
    # 形如 `OverlayWidget_0.1.0.0_x64_Test\OverlayWidget_0.1.0.0_x64.msix`。
    # 按 .msix 文件名的 `_<Platform>.msix` 后缀匹配，再按 LastWriteTime 取最新一次构建。
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
# Step 6: Remove old + Add-AppxPackage（带 dependencies + retry）
# ============================================================
$step++
Write-Step $step $totalSteps "Add-AppxPackage"

$existingPkg = Get-AppxPackage -Name 'OverlayWidget' -ErrorAction SilentlyContinue
if ($existingPkg) {
    Write-Host "  removing existing $($existingPkg.PackageFullName)" -ForegroundColor Yellow
    Remove-AppxPackage -Package $existingPkg.PackageFullName
}

# Debug build 依赖 Microsoft.NET.CoreRuntime/CoreFramework/VCLibs framework 包；
# Release build (UseDotNetNativeToolchain=true) 编原生码不需要。
$msixDir = Split-Path -Parent $msix
$depsDir = Join-Path $msixDir "Dependencies\$Platform"
$depPaths = @()
if (Test-Path $depsDir) {
    $depPaths = Get-ChildItem -Path $depsDir -Filter '*.appx' -ErrorAction SilentlyContinue |
        ForEach-Object { $_.FullName }
}

# 跳过本机已装的 framework 依赖：
# 大多数 Windows 通过 Windows Update 已带这些包；Store/StorePurchaseApp 占用时
# 重装会触发 0x80073D02 (RESOURCE_IN_USE)。
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

# 0x80073D02 重试：先正常装；遇 RESOURCE_IN_USE 改 -ForceApplicationShutdown 再来一次。
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
    Write-Host "  installing with $($missingDeps.Count) framework deps from $depsDir" -ForegroundColor Yellow
    Invoke-AddAppxWithRetry -Msix $msix -Deps $missingDeps
}
else {
    Invoke-AddAppxWithRetry -Msix $msix
}
Write-Host "  ok: installed" -ForegroundColor Green

Write-Host ""
Write-Host "All done." -ForegroundColor Green
Write-Host "Open Xbox Game Bar (Win+G) -> Widget store -> 'Overlay Widget'." -ForegroundColor Cyan
