<#
.SYNOPSIS
    OverlayWidget 一键卸载（开发版）。

.DESCRIPTION
    三步干净卸：
      1. Remove-AppxPackage  干掉 MSIX（OS 自动回收：widget 注册、文件、注册表、Tile）
      2. 从 LocalMachine\TrustedPeople 移 dev cert（可选，默认不移，-RemoveCert 打开）
      3. 可选：从 AppPackages/* 清出过期签名输出

    MSIX 卸载 = OS 级，不需要自定义 Uninstall.exe / 注册表项 / self-delete。
    卸完可以在 Windows 设置 -> 应用 确认 "Overlay Widget" 已消失。

.PARAMETER RemoveCert
    同时把 "CN=OverlayWidget Dev" 从 LocalMachine\TrustedPeople 移掉。
    要 Admin。默认 off —— 再次开发安装就不用重导 cert。
    彻底清理机器（比如还给别人用）时才开。

.PARAMETER AllUsers
    卸载所有用户下的 OverlayWidget 包，而不仅当前用户。
    要 Admin。默认只卸当前用户（Add-AppxPackage 默认也是 per-user）。

.EXAMPLE
    .\uninstall.ps1
    当前用户卸载，保留 dev cert 方便下次重装。

.EXAMPLE
    .\uninstall.ps1 -RemoveCert -AllUsers
    彻底清理：所有用户 + 删 cert。

.NOTES
    等价命令（手动）：
      Get-AppxPackage *OverlayWidget* | Remove-AppxPackage
      Get-ChildItem Cert:\LocalMachine\TrustedPeople |
        Where-Object { $_.Subject -eq 'CN=OverlayWidget Dev' } |
        Remove-Item
#>

[CmdletBinding()]
param(
    [switch]$RemoveCert,
    [switch]$AllUsers
)

$ErrorActionPreference = 'Stop'

function Test-IsAdmin {
    $id = [System.Security.Principal.WindowsIdentity]::GetCurrent()
    $p  = New-Object System.Security.Principal.WindowsPrincipal($id)
    return $p.IsInRole([System.Security.Principal.WindowsBuiltInRole]::Administrator)
}

# ------------------------------------------------------------
# Step 1: 卸 MSIX
# ------------------------------------------------------------
Write-Host "[1/2] Removing OverlayWidget MSIX packages..." -ForegroundColor Cyan

$getParams = @{ Name = '*OverlayWidget*' ; ErrorAction = 'SilentlyContinue' }
if ($AllUsers) {
    if (-not (Test-IsAdmin)) {
        throw "-AllUsers requires Admin. Re-run from an elevated PowerShell."
    }
    $getParams.AllUsers = $true
}

$pkgs = @(Get-AppxPackage @getParams)

if ($pkgs.Count -eq 0) {
    Write-Host "  no OverlayWidget packages installed." -ForegroundColor DarkGray
}
else {
    foreach ($pkg in $pkgs) {
        Write-Host "  removing $($pkg.PackageFullName)" -ForegroundColor Yellow
        if ($AllUsers) {
            Remove-AppxPackage -Package $pkg.PackageFullName -AllUsers
        }
        else {
            Remove-AppxPackage -Package $pkg.PackageFullName
        }
    }
    Write-Host "  removed $($pkgs.Count) package(s)." -ForegroundColor Green
}

# ------------------------------------------------------------
# Step 2: 删 dev cert (opt-in)
# ------------------------------------------------------------
if ($RemoveCert) {
    Write-Host "[2/2] Removing dev cert from LocalMachine\TrustedPeople..." -ForegroundColor Cyan

    if (-not (Test-IsAdmin)) {
        throw "-RemoveCert requires Admin. Re-run from an elevated PowerShell."
    }

    $subject = 'CN=OverlayWidget Dev'
    $hits = @(Get-ChildItem -Path Cert:\LocalMachine\TrustedPeople -ErrorAction SilentlyContinue |
        Where-Object { $_.Subject -eq $subject })

    if ($hits.Count -eq 0) {
        Write-Host "  no matching cert." -ForegroundColor DarkGray
    }
    else {
        foreach ($c in $hits) {
            Write-Host "  removing $($c.Thumbprint)" -ForegroundColor Yellow
            Remove-Item -Path "Cert:\LocalMachine\TrustedPeople\$($c.Thumbprint)" -Force
        }
        Write-Host "  removed $($hits.Count) cert(s)." -ForegroundColor Green
    }
}
else {
    Write-Host "[2/2] Keeping dev cert (pass -RemoveCert to delete)." -ForegroundColor DarkGray
}

Write-Host ""
Write-Host "Uninstall complete." -ForegroundColor Green
Write-Host "Verify: Settings -> Apps -> search 'Overlay Widget' should return nothing." -ForegroundColor DarkGray
