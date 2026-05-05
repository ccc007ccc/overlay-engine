$paths = @(
  'C:\code\Rust\overlay-widget\install.ps1',
  'C:\code\Rust\overlay-widget\uninstall.ps1',
  'C:\code\Rust\overlay-widget\build-all.ps1',
  'C:\code\Rust\overlay-widget\csharp-shell\scripts\install-dev.ps1'
)
foreach ($p in $paths) {
  $bytes = [System.IO.File]::ReadAllBytes($p)
  $hasBom = $bytes.Length -ge 3 -and $bytes[0] -eq 0xEF -and $bytes[1] -eq 0xBB -and $bytes[2] -eq 0xBF
  if (-not $hasBom) {
    $content = Get-Content -LiteralPath $p -Raw -Encoding UTF8
    $utf8WithBom = New-Object System.Text.UTF8Encoding $true
    [System.IO.File]::WriteAllText($p, $content, $utf8WithBom)
    Write-Host "added BOM: $p"
  }
  # syntax check
  $err = $null
  [System.Management.Automation.Language.Parser]::ParseFile($p, [ref]$null, [ref]$err) | Out-Null
  if ($err) {
    Write-Host "PARSE ERROR in $p :" -ForegroundColor Red
    $err | ForEach-Object { Write-Host "  $_" -ForegroundColor Red }
  } else {
    Write-Host "ok: $p"
  }
}
