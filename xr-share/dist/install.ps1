# xr-share installer for Windows (XR-028). Downloads the agent, verifies its
# SHA-256, and — given a registration token — configures it and starts the
# service. One command, no hands (run PowerShell as Administrator):
#
#   $env:XR_DIR="C:\share"; $env:XR_TOKEN="<REG-TOKEN-FROM-HUB>"
#   irm https://xr-hub.zoobr.top/share/install.ps1 | iex
#
# Generate <REG-TOKEN> in the hub admin (Shares → "Команда установки"). Without
# XR_DIR/XR_TOKEN it just installs the binary; configure later with `xr-share init`.
# Optional: $env:XR_HUB, $env:XR_ADDR (advertised address), $env:XR_NAME.
$ErrorActionPreference = 'Stop'

$base = if ($env:XR_SHARE_BASE) { $env:XR_SHARE_BASE } else { 'https://xr-hub.zoobr.top/share' }
$hub  = if ($env:XR_HUB) { $env:XR_HUB } else { 'https://xr-hub.zoobr.top' }
$arch = 'x86_64'
$bin  = "xr-share-windows-$arch.exe"

$dir  = Join-Path $env:LOCALAPPDATA 'Programs\xr-share'
New-Item -ItemType Directory -Force -Path $dir | Out-Null
$dest = Join-Path $dir 'xr-share.exe'

Write-Host "Downloading $bin ..."
Invoke-WebRequest -Uri "$base/$bin" -OutFile $dest -UseBasicParsing

Write-Host "Verifying checksum ..."
$sums     = (Invoke-WebRequest -Uri "$base/SHA256SUMS" -UseBasicParsing).Content
$expected = ($sums -split "`n" |
    Where-Object { $_ -match "\s$([regex]::Escape($bin))\s*$" } |
    ForEach-Object { ($_ -split '\s+')[0] } | Select-Object -First 1)
if (-not $expected) { throw "no checksum listed for $bin" }
$actual = (Get-FileHash -Algorithm SHA256 $dest).Hash.ToLower()
if ($expected.ToLower() -ne $actual) { throw "checksum mismatch (expected $expected, got $actual)" }
Write-Host "  ok ($actual)"

# PATH: persist for future shells AND this session (so xr-share works right away).
$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if ($userPath -notlike "*$dir*") {
    [Environment]::SetEnvironmentVariable('Path', "$userPath;$dir", 'User')
}
if ($env:Path -notlike "*$dir*") { $env:Path = "$env:Path;$dir" }
Write-Host "Installed: $dest"

# ── No-hands: configure + start the service ─────────────────────────
if ($env:XR_DIR -and $env:XR_TOKEN) {
    Write-Host "Registering with the hub and starting the service ..."
    $initArgs = @('init', '--non-interactive', '--dir', $env:XR_DIR, '--hub', $hub, '--token', $env:XR_TOKEN)
    if ($env:XR_ADDR) { $initArgs += @('--addr', $env:XR_ADDR) }
    if ($env:XR_NAME) { $initArgs += @('--name', $env:XR_NAME) }
    & $dest @initArgs
    & $dest service install   # needs an elevated (Administrator) PowerShell
    Write-Host ""
    Write-Host "Done — xr-share is running and registered. Files in $($env:XR_DIR) are now shareable."
} else {
    Write-Host ""
    Write-Host "Next: configure + enable the service"
    Write-Host "  xr-share init --dir C:\path\to\share --token <reg-token-from-hub>"
    Write-Host "  xr-share service install      # run PowerShell as Administrator"
}
