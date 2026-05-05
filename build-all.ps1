<#
.SYNOPSIS
    OverlayWidget end-to-end build: cargo -> msbuild -> signtool -> Add-AppxPackage.

.DESCRIPTION
    一条命令端到端：

      1. cargo build --release  (rust-renderer/)         -> renderer.dll
      2. msbuild OverlayWidget.csproj /restore /p:...     -> *.msix
      3. csharp-shell\scripts\install-dev.ps1             -> cert + signtool + Add-AppxPackage

    任意一步失败立刻退出（$ErrorActionPreference = 'Stop' 全程贯穿）。

    环境要求：
    - rustup / cargo 在 PATH
    - Visual Studio (任意版本，UWP/MSIX 工作负载) 安装且 vswhere 找得到 MSBuild
    - signtool.exe 在已知 Windows SDK 路径下（install-dev.ps1 里硬编码了若干 fallback）
    - 首次运行需要 Admin（导 cert 进 LocalMachine\TrustedPeople）；之后 cert 已信任，
      普通用户可跑

.PARAMETER Configuration
    Debug 或 Release，默认 Release。
    Release 启用 .NET Native AOT (UseDotNetNativeToolchain=true)，体积大、启动快、不依赖框架包。
    Debug 走标准 IL，需要 Microsoft.NET.CoreRuntime/CoreFramework framework appx 配套安装
    (install-dev.ps1 会自动从 AppPackages 旁边的 Dependencies 目录捎带)。

.PARAMETER Platform
    目前只接受 x64（OverlayWidget.csproj 没配 ARM64 target；rust-renderer 也没 cross-target 设置）。
    ARM64 解锁是单独的工作项，不在本脚本范围。

.PARAMETER SkipRust
    跳过 cargo 编译。改 C# 不动 Rust 时省时间。

.PARAMETER SkipCSharp
    跳过 msbuild。只想确认 Rust 编得过时用。

.PARAMETER SkipInstall
    只编译不装。CI / 检视 MSIX 输出时用。

.PARAMETER Clean
    cargo clean + msbuild /t:Clean 后再编译。
    平时增量编译够快，clean 只在改 build 配置 / 怀疑产物坏了时打。

.EXAMPLE
    .\build-all.ps1
    完整 Release 构建 + 安装。

.EXAMPLE
    .\build-all.ps1 -Configuration Debug -SkipRust
    Debug 重 build C#，复用上次的 renderer.dll。

.EXAMPLE
    .\build-all.ps1 -SkipInstall
    只产 MSIX，不动本机已安装的版本。

.NOTES
    - 与 csharp-shell\scripts\install-dev.ps1 配套：本脚本把"编译"那段补齐，
      install 复用既有的 cert / signtool / Add-AppxPackage 逻辑，DRY。
    - csproj 里 <Content Include="..\target\release\renderer.dll"> 写死了
      Rust release 输出路径——所以 -Configuration Debug 也用 release renderer.dll，符合预期
      （Rust 库不需要随 C# 切 Debug/Release）。
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

$ProjectRoot   = $PSScriptRoot
$RustDir       = Join-Path $ProjectRoot 'rust-renderer'
$CsharpDir     = Join-Path $ProjectRoot 'csharp-shell'
$Csproj        = Join-Path $CsharpDir 'OverlayWidget.csproj'
$InstallScript = Join-Path $CsharpDir 'scripts\install-dev.ps1'
# v0.7 起 workspace 根 target/，cargo 不再写 rust-renderer/target/。
# csproj 里 <Content Include="..\target\release\renderer.dll"> 也是这个路径。
$RendererDll   = Join-Path $ProjectRoot 'target\release\renderer.dll'

function Write-Step([int]$n, [int]$total, [string]$msg) {
    Write-Host ""
    Write-Host "[$n/$total] $msg" -ForegroundColor Cyan
}

function Resolve-Cargo {
    $cmd = Get-Command cargo.exe -ErrorAction SilentlyContinue
    if (-not $cmd) {
        throw "cargo.exe not found in PATH. Install rustup from https://rustup.rs/"
    }
    return $cmd.Source
}

function Resolve-MSBuild {
    # vswhere 是 MS 官方 VS 定位工具，跨大版本可靠。优先用它。
    # 注意：Windows PowerShell 5.1 的 `${env:ProgramFiles(x86)}` 语法会因括号被识别成调用而失败，
    # 改走 [Environment]::GetEnvironmentVariable 是 PS 5.1 / 7 都吃的写法。
    $pfx86 = [Environment]::GetEnvironmentVariable('ProgramFiles(x86)')
    if (-not $pfx86) { $pfx86 = 'C:\Program Files (x86)' }
    $vswhere = Join-Path $pfx86 'Microsoft Visual Studio\Installer\vswhere.exe'
    if (Test-Path $vswhere) {
        # -prerelease 让 Insiders / Preview 版也被找到
        $instPaths = @(& $vswhere -latest -prerelease -products * `
            -requires Microsoft.Component.MSBuild `
            -property installationPath 2>$null)
        foreach ($inst in $instPaths) {
            if (-not $inst) { continue }
            # VS 2022/2026: MSBuild\Current\Bin；VS 2019: MSBuild\16.0\Bin
            foreach ($v in 'Current', '17.0', '16.0') {
                $msb = Join-Path $inst "MSBuild\$v\Bin\MSBuild.exe"
                if (Test-Path $msb) { return $msb }
            }
        }
    }
    # PATH fallback (Developer PowerShell 场景)
    $cmd = Get-Command msbuild.exe -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }
    throw "MSBuild.exe not found. Install Visual Studio with the UWP / MSIX workload, or run from a Developer PowerShell."
}

# ---- 计算总步数（用于 [n/total] 进度展示）----
$totalSteps = 0
if (-not $SkipRust)    { $totalSteps++ }
if (-not $SkipCSharp)  { $totalSteps++ }
if (-not $SkipInstall) { $totalSteps++ }
if ($totalSteps -eq 0) {
    Write-Host "Nothing to do (all steps skipped)." -ForegroundColor Yellow
    return
}
$step = 0

Write-Host "OverlayWidget build-all" -ForegroundColor White
Write-Host "  Configuration : $Configuration" -ForegroundColor DarkGray
Write-Host "  Platform      : $Platform" -ForegroundColor DarkGray
Write-Host "  Clean         : $Clean" -ForegroundColor DarkGray

# ============================================================
# Step 1: cargo build --release
# ============================================================
if (-not $SkipRust) {
    $step++
    Write-Step $step $totalSteps "cargo build -p renderer --release  (workspace)"

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

    # csproj 里 <Content Include="..\target\release\renderer.dll"> 是 release 路径写死。
    # 没编过 Rust 时这步会拷贝失败，给出友好提示。
    if (-not (Test-Path $RendererDll)) {
        throw "renderer.dll missing at $RendererDll. Run without -SkipRust first, or build rust-renderer manually."
    }

    $msbuild = Resolve-MSBuild
    Write-Host "  using: $msbuild" -ForegroundColor DarkGray

    # /restore: 让老式 csproj 的 PackageReference 自动 NuGet restore
    # AppxBundle / UapAppxPackageBuildMode / AppxPackageSigningEnabled 三项都是 csproj 已声明的默认，
    # 这里显式覆盖避免本机环境变量 / .user 文件污染默认值
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

    if ($Clean) {
        $msbArgs += '/t:Clean;Build'
    }
    else {
        $msbArgs += '/t:Build'
    }

    if ($VerbosePreference -eq 'Continue') {
        $msbArgs += '/v:normal'
    }
    else {
        $msbArgs += '/v:minimal'
    }

    & $msbuild @msbArgs
    if ($LASTEXITCODE -ne 0) { throw "msbuild failed: exit $LASTEXITCODE" }

    # 校验 MSIX 真的产了。
    # 注意：UWP SideloadOnly 模式下 AppPackages 目录后缀只有 `_Test`，不带 Configuration
    # （eg. `OverlayWidget_0.1.0.0_x64_Test\OverlayWidget_0.1.0.0_x64.msix`）。
    # 所以按 .msix 文件名的 `_<Platform>.msix` 后缀匹配，再按 LastWriteTime 取最新一次构建。
    $msixPattern = "_${Platform}\.msix$"
    $appPkgDir = Join-Path $CsharpDir 'AppPackages'
    $msix = Get-ChildItem -Path $appPkgDir -Filter 'OverlayWidget_*.msix' -Recurse -ErrorAction SilentlyContinue |
        Where-Object { $_.Name -match $msixPattern } |
        Sort-Object LastWriteTime -Descending |
        Select-Object -First 1

    if (-not $msix) {
        throw "msbuild succeeded but no MSIX matching '$msixPattern' under $appPkgDir."
    }
    Write-Host ("  ok: {0}" -f $msix.FullName) -ForegroundColor Green
}

# ============================================================
# Step 3: cert + signtool + Add-AppxPackage  (delegate to install-dev.ps1)
# ============================================================
if (-not $SkipInstall) {
    $step++
    Write-Step $step $totalSteps "sign + install (delegate to install-dev.ps1)"

    if (-not (Test-Path $InstallScript)) {
        throw "install script missing: $InstallScript"
    }

    # install-dev.ps1 自身 $ErrorActionPreference='Stop'，throw 会直接冒上来。
    & $InstallScript -Configuration $Configuration -Platform $Platform
}

Write-Host ""
Write-Host "All done." -ForegroundColor Green
if (-not $SkipInstall) {
    Write-Host "Open Xbox Game Bar (Win+G) -> Widget store -> 'Overlay Widget'." -ForegroundColor Cyan
}
