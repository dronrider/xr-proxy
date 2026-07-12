# xr-share installer for Windows (XR-028/029). Downloads the agent, verifies its
# SHA-256, and, given a registration token, installs it as a service with a
# long-lived hub mandate so you can then share any number of paths. One command
# (run PowerShell as Administrator):
#
#   $env:XR_TOKEN="<REG-TOKEN-FROM-HUB>"
#   irm https://xr-hub.zoobr.top/share/install.ps1 | iex
#
# Generate <REG-TOKEN> in the hub admin (Shares, "Команда установки"). Then share
# any path:  xr-share share C:\photos   (a folder OR a single file). Set XR_DIR to
# also share one path right away. Without XR_TOKEN it just installs the binary;
# set up later with `xr-share install --token <reg-token>`.
# Optional: $env:XR_HUB, $env:XR_ADDR (advertised address), $env:XR_NAME.
$ErrorActionPreference = 'Stop'

$base = if ($env:XR_SHARE_BASE) { $env:XR_SHARE_BASE } else { 'https://xr-hub.zoobr.top/share' }
$hub  = if ($env:XR_HUB) { $env:XR_HUB } else { 'https://xr-hub.zoobr.top' }
$arch = 'x86_64'
$bin  = "xr-share-windows-$arch.exe"

$dir  = Join-Path $env:LOCALAPPDATA 'Programs\xr-share'
New-Item -ItemType Directory -Force -Path $dir | Out-Null
$dest = Join-Path $dir 'xr-share.exe'

# Stop a running agent first (XR-037): Windows locks a running .exe, so an update
# can't overwrite it. No-op on a fresh install. Needs an elevated shell.
schtasks /End /TN xr-share 2>$null | Out-Null
Get-Process -Name xr-share -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 800

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

# ── No-hands: install the service with a hub mandate ────────────────
if ($env:XR_TOKEN) {
    Write-Host "Installing the service and exchanging the token for a hub mandate ..."
    $installArgs = @('install', '--non-interactive', '--hub', $hub, '--token', $env:XR_TOKEN)
    & $dest @installArgs           # needs an elevated (Administrator) PowerShell
    if ($env:XR_DIR) {
        Write-Host "Sharing $($env:XR_DIR) ..."
        $shareArgs = @('share', $env:XR_DIR)
        if ($env:XR_ADDR)   { $shareArgs += @('--addr', $env:XR_ADDR) }
        if ($env:XR_NAME)   { $shareArgs += @('--name', $env:XR_NAME) }
        # Reachable through the hub's relay for a share behind NAT (LLD-23): the
        # binary must be a relay build; the relay descriptor came with the mandate
        # above, so no config editing is needed.
        if ($env:XR_RELAY)  { $shareArgs += @('--relay') }
        if ($env:XR_INVITE) { $shareArgs += @('--invite', $env:XR_INVITE) }
        & $dest @shareArgs
    }
    Write-Host ""
    Write-Host "Done. Share any path anytime (folder or file):"
    Write-Host "  xr-share share C:\photos"
    Write-Host "  xr-share list"
} else {
    Write-Host ""
    Write-Host "Next: install the service, then share paths"
    Write-Host "  xr-share install --hub $hub --token <reg-token-from-hub>"
    Write-Host "  xr-share share C:\path\to\share"
}
