<#
.SYNOPSIS
    OverlayWidget 一键安装（开发版）。

.DESCRIPTION
    顺序执行：
      1. cargo build --release   (rust-renderer)        -> renderer.dll
      2. msbuild OverlayWidget.csproj                    -> signed MSIX
      3. signtool + Add-AppxPackage                      -> 装到系统
    完成后在 Xbox Game Bar (Win+G) -> Widgets menu 能看到 "Overlay Widget"。

    本脚本 = build-all.ps1 的外壳，语义上是"双脚本方案"的一半
    （另一半 = uninstall.ps1）。内部实现没变，全部委托给 build-all.ps1 +
    monitors/game-bar-widget/scripts/install-dev.ps1。

.PARAMETER Configuration
    Debug 或 Release，默认 Release（.NET Native AOT，无 framework deps 依赖）。

.PARAMETER SkipBuild
    只跑安装，不重新编译。适合 MSIX 已就位、只想换一台机器装或重新 Add-AppxPackage。

.EXAMPLE
    .\install.ps1
    端到端全流程。首次运行需要 Admin（导 cert 到 LocalMachine\TrustedPeople）。

.EXAMPLE
    .\install.ps1 -SkipBuild
    跳过编译，直接签名+装。

.NOTES
    卸载：运行 .\uninstall.ps1  或  Get-AppxPackage *OverlayWidget* | Remove-AppxPackage
    也可以在 Windows 设置 -> 应用 -> 搜 "Overlay Widget" -> 卸载。
#>

[CmdletBinding()]
param(
    [ValidateSet('Debug', 'Release')]
    [string]$Configuration = 'Release',

    [switch]$SkipBuild
)

$ErrorActionPreference = 'Stop'

$ProjectRoot = $PSScriptRoot
$BuildAll    = Join-Path $ProjectRoot 'build-all.ps1'

if (-not (Test-Path $BuildAll)) {
    throw "build-all.ps1 missing at $BuildAll"
}

if ($SkipBuild) {
    # 只跑装这一段；build-all.ps1 的 -SkipRust + -SkipCSharp 组合。
    & $BuildAll -Configuration $Configuration -SkipRust -SkipCSharp
}
else {
    & $BuildAll -Configuration $Configuration
}
