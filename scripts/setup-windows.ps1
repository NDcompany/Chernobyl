# setup-windows.ps1: one-shot setup of the CHERNOBYL companion for daily use
# from a phone on the same Wi-Fi.
#
#   powershell -ExecutionPolicy Bypass -File scripts\setup-windows.ps1 -Workspaces 'G:\Projects\app','G:\Chernobyl'
#
# What it does (user level; the firewall rule needs an elevated prompt):
#   1. copies the release exe to %LOCALAPPDATA%\Programs\CHERNOBYL (so rebuilds don't fight a running exe)
#   2. writes %APPDATA%\CHERNOBYL\companion.json: demo data off, Claude Code on, your workspace
#      folders allow-listed, LAN bind (the old file is kept as companion.json.bak)
#   3. creates an access token once, stored in the user variable HOST_TOKEN (never in a file)
#   4. allows inbound TCP on the port for Private networks (if run as admin)
#   5. registers start-on-login and starts the companion in the background
#   6. prints the pairing QR for ws://<this PC's LAN IP>:<port> (and -TunnelUrl after it)
#
# Away from home: point a free tunnel (Cloudflare Tunnel, ngrok, loophole) at http://127.0.0.1:<port>
# and pass its public URL, e.g. -TunnelUrl wss://chernobyl.example.com. The phone tries the LAN first,
# then the tunnel. Tunnel traffic arrives from loopback, so this also turns on requireE2e: every
# connection must complete the paired E2E handshake (the tunnel only ever sees ciphertext).
param(
  [Parameter(Mandatory = $true)][string[]]$Workspaces,
  [int]$Port = 8787,
  [string]$Exe = (Join-Path $PSScriptRoot '..\services\companion-rs\target\release\chernobyl-companion.exe'),
  [switch]$NoAutostart,
  [string]$TunnelUrl
)
$ErrorActionPreference = 'Stop'
if ($TunnelUrl -and $TunnelUrl -notmatch '^wss://[^/\s]+/?$') { throw "-TunnelUrl must look like wss://host (got $TunnelUrl)" }

if (-not (Test-Path $Exe)) { throw "No companion exe at $Exe. Build it first: cargo build --release --manifest-path services\companion-rs\Cargo.toml" }
if (-not (Get-Command claude -ErrorAction SilentlyContinue)) { Write-Warning 'claude is not on PATH: install Claude Code and sign in (run "claude" once) before starting sessions.' }
foreach ($w in $Workspaces) { if (-not (Test-Path $w -PathType Container)) { throw "Workspace folder does not exist: $w" } }

# 1. install the exe
$bin = Join-Path $env:LOCALAPPDATA 'Programs\CHERNOBYL'
New-Item -ItemType Directory -Force $bin | Out-Null
Get-Process chernobyl-companion -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 500
$target = Join-Path $bin 'chernobyl-companion.exe'
Copy-Item $Exe $target -Force
Write-Host "Installed $target"

# 2. config
$dir = Join-Path $env:APPDATA 'CHERNOBYL'
New-Item -ItemType Directory -Force $dir | Out-Null
$cfgPath = Join-Path $dir 'companion.json'
if (Test-Path $cfgPath) { Copy-Item $cfgPath "$cfgPath.bak" -Force }
$cfg = [ordered]@{
  version = 1
  name = "$env:COMPUTERNAME"
  dataDir = $dir
  adapters = @(
    [ordered]@{ id = 'mock-1'; type = 'mock'; enabled = $false },
    [ordered]@{ id = 'claude-1'; type = 'claude'; enabled = $true; workspaces = @($Workspaces | ForEach-Object { (Resolve-Path $_).Path }) }
  )
  retention = [ordered]@{ events = 5000; eventsMaxAgeMs = 604800000 }
  diagnostics = [ordered]@{ port = 9487 }
  probeIntervalMs = 15000
  server = [ordered]@{ port = $Port; bind = '0.0.0.0'; requireE2e = [bool]$TunnelUrl }
}
[IO.File]::WriteAllText($cfgPath, ($cfg | ConvertTo-Json -Depth 6), (New-Object Text.UTF8Encoding $false))
Write-Host "Wrote $cfgPath"

# 3. token (created once; the companion and the login autostart read HOST_TOKEN)
$token = [Environment]::GetEnvironmentVariable('HOST_TOKEN', 'User')
if (-not $token) {
  $bytes = New-Object byte[] 24
  [Security.Cryptography.RandomNumberGenerator]::Create().GetBytes($bytes)
  $token = -join ($bytes | ForEach-Object { $_.ToString('x2') })
  [Environment]::SetEnvironmentVariable('HOST_TOKEN', $token, 'User')
  Write-Host 'Created a new access token (user variable HOST_TOKEN).'
}
$env:HOST_TOKEN = $token

# 4. firewall
$admin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if ($admin) {
  Get-NetFirewallRule -DisplayName 'CHERNOBYL companion' -ErrorAction SilentlyContinue | Remove-NetFirewallRule
  New-NetFirewallRule -DisplayName 'CHERNOBYL companion' -Direction Inbound -Protocol TCP -LocalPort $Port -Action Allow -Profile Private -Program $target | Out-Null
  Write-Host "Firewall: inbound TCP $Port allowed on Private networks."
} else {
  Write-Warning "Not elevated: no firewall rule added. If the phone can't connect, run this once in an admin PowerShell:`n  New-NetFirewallRule -DisplayName 'CHERNOBYL companion' -Direction Inbound -Protocol TCP -LocalPort $Port -Action Allow -Profile Private -Program '$target'"
}

# 5. autostart + start
if (-not $NoAutostart) { & $target --install-autostart | Out-Host }
Start-Process -FilePath $target -ArgumentList '--background' -WindowStyle Hidden
$up = $false
foreach ($i in 1..50) {
  try { Invoke-RestMethod -Uri 'http://127.0.0.1:9487/health' -TimeoutSec 1 | Out-Null; $up = $true; break } catch { Start-Sleep -Milliseconds 200 }
}
if (-not $up) { throw 'The companion did not come up. Run it in a terminal to see why: & "' + $target + '"' }
Write-Host 'Companion is running.'

# 6. pairing QR for the LAN address
$ip = (Get-NetIPConfiguration | Where-Object { $_.IPv4DefaultGateway -and $_.NetAdapter.Status -eq 'Up' } | Select-Object -First 1).IPv4Address.IPAddress
if (-not $ip) { throw 'No LAN IPv4 address with a default gateway found.' }
Write-Host ''
Write-Host "Phone setup: open CHERNOBYL > Pair with a host. Enter this access token in the token field, then scan the QR below."
Write-Host "Access token: $token" -ForegroundColor Yellow
Write-Host ''
$endpoints = @('--endpoint', "ws://${ip}:$Port")
if ($TunnelUrl) { $endpoints += @('--endpoint', $TunnelUrl.TrimEnd('/')) }
& $target --pair @endpoints
Write-Host ''
Write-Host "Each QR works once. For a new one later: & '$target' --pair $($endpoints -join ' ')"
