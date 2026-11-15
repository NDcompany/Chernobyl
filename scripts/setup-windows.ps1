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
#
# ngrok shortcut: -NgrokDomain <name>.ngrok-free.dev runs `ngrok http <port> --url <domain>` hidden,
# now and at every login (HKCU Run), and sets -TunnelUrl wss://<domain>. Run `ngrok config
# add-authtoken <token>` yourself once first; the token never passes through this script.
#
# -PhoneApprovals (ADR-018): also adds the companion's PermissionRequest hook to
# %USERPROFILE%\.claude\settings.json (`--install-hooks`, backup in settings.json.chernobyl-backup),
# so Claude Code sessions in a terminal or VS Code ask the phone too; the PC still asks if the
# phone does not answer in 120 s. Undo: & "$env:LOCALAPPDATA\Programs\CHERNOBYL\chernobyl-companion.exe" --uninstall-hooks
#
# OpenCode: -OpenCode adds an attach-only OpenCode adapter for an `opencode serve` at -OpenCodeUrl
# (default http://127.0.0.1:4096). No password is ever written to the file: if the server needs
# one, set the user variable OPENCODE_SERVER_PASSWORD (and OPENCODE_SERVER_USERNAME if it is not
# `opencode`); the companion reads it at start (`basicAuthEnv`). -StartOpenCode also runs
# `opencode serve --port <port> --hostname 127.0.0.1` hidden in the first workspace folder, now and
# at every login (HKCU Run).
#
# Antigravity (ADR-022): -Antigravity adds a Google Antigravity runtime (`agy acp`) for the same workspace
# folders. Install agy and sign in first (run `agy` once). The phone answers agy's own permission prompts
# (allow once or deny); nothing is ever written to agy's settings. -AntigravityModel sets the model new
# sessions start on (for example gemini-3.8-flash-low; `agy models` lists them).
#
# Terminal (ADR-020): -EnableTerminal lets the paired phone run any command line in an allow-listed
# workspace WITHOUT the agents' approval prompts. Off unless you pass it.
#
# -ConfigOnly writes companion.json (and the OpenCode launcher) and stops: no exe copy, token,
# firewall, autostart or process changes. For checking what setup would write.
param(
  [Parameter(Mandatory = $true)][string[]]$Workspaces,
  [int]$Port = 8787,
  [string]$Exe,
  [switch]$NoAutostart,
  [switch]$PhoneApprovals,
  [string]$TunnelUrl,
  [string]$NgrokDomain,
  [string]$Ngrok = 'C:\ngrok\ngrok.exe',
  [switch]$OpenCode,
  [string]$OpenCodeUrl = 'http://127.0.0.1:4096',
  [switch]$StartOpenCode,
  [switch]$EnableTerminal,
  [switch]$Antigravity,
  [string]$AntigravityModel,
  [switch]$ConfigOnly
)
$ErrorActionPreference = 'Stop'
# not a param default: Windows PowerShell 5.1 leaves $PSScriptRoot empty there under -File
if (-not $Exe) { $Exe = Join-Path $PSScriptRoot '..\services\companion-rs\target\release\chernobyl-companion.exe' }
if ($StartOpenCode) { $OpenCode = [switch]$true }
if ($OpenCode) {
  $ocUri = $null
  if (-not [Uri]::TryCreate($OpenCodeUrl, [UriKind]::Absolute, [ref]$ocUri) -or $ocUri.Scheme -ne 'http' -or $ocUri.PathAndQuery -ne '/') {
    throw "-OpenCodeUrl must look like http://127.0.0.1:4096 (got $OpenCodeUrl)"
  }
  $OpenCodeUrl = "http://$($ocUri.Authority)"
}
if ($StartOpenCode -and $ocUri.Host -notin @('127.0.0.1', 'localhost')) {
  throw "-StartOpenCode serves on this PC only: -OpenCodeUrl must be http://127.0.0.1:<port> (got $OpenCodeUrl)"
}
if ($NgrokDomain) {
  if ($NgrokDomain -notmatch '^[a-z0-9-]+(\.[a-z0-9-]+)+$') { throw "-NgrokDomain must be a bare host name (got $NgrokDomain)" }
  if (-not (Test-Path $Ngrok)) { throw "ngrok not found at $Ngrok (pass -Ngrok <path>)" }
  $TunnelUrl = "wss://$NgrokDomain"
}
if ($AntigravityModel) {
  if ($AntigravityModel -notmatch '^[a-z][a-z0-9]*([.-][a-z0-9]+)+$' -or $AntigravityModel.Length -gt 64) { throw "-AntigravityModel must be a model id such as gemini-3.8-flash-low (got $AntigravityModel)" }
  $Antigravity = [switch]$true
}
if ($Antigravity -and -not (Get-Command agy -ErrorAction SilentlyContinue) -and -not (Test-Path (Join-Path $env:LOCALAPPDATA 'agy\bin\agy.exe'))) {
  Write-Warning 'agy is not installed: install Google Antigravity CLI and sign in (run "agy" once) before starting sessions.'
}
if ($TunnelUrl -and $TunnelUrl -notmatch '^wss://[^/\s]+/?$') { throw "-TunnelUrl must look like wss://host (got $TunnelUrl)" }

if (-not $ConfigOnly -and -not (Test-Path $Exe)) { throw "No companion exe at $Exe. Build it first: cargo build --release --manifest-path services\companion-rs\Cargo.toml" }
if (-not (Get-Command claude -ErrorAction SilentlyContinue)) { Write-Warning 'claude is not on PATH: install Claude Code and sign in (run "claude" once) before starting sessions.' }
foreach ($w in $Workspaces) { if (-not (Test-Path $w -PathType Container)) { throw "Workspace folder does not exist: $w" } }

# 1. install the exe
$bin = Join-Path $env:LOCALAPPDATA 'Programs\CHERNOBYL'
New-Item -ItemType Directory -Force $bin | Out-Null
$target = Join-Path $bin 'chernobyl-companion.exe'
if (-not $ConfigOnly) {
  Get-Process chernobyl-companion -ErrorAction SilentlyContinue | Stop-Process -Force
  Start-Sleep -Milliseconds 500
  Copy-Item $Exe $target -Force
  Write-Host "Installed $target"
}

# 2. config
$dir = Join-Path $env:APPDATA 'CHERNOBYL'
New-Item -ItemType Directory -Force $dir | Out-Null
$cfgPath = Join-Path $dir 'companion.json'
if (Test-Path $cfgPath) { Copy-Item $cfgPath "$cfgPath.bak" -Force }
$adapters = @(
  [ordered]@{ id = 'mock-1'; type = 'mock'; enabled = $false },
  [ordered]@{ id = 'claude-1'; type = 'claude'; enabled = $true; workspaces = @($Workspaces | ForEach-Object { (Resolve-Path $_).Path }) }
)
if ($OpenCode) {
  # attach-only; the password (if any) stays in the user variable, never in this file
  $adapters += , ([ordered]@{ id = 'opencode-1'; type = 'opencode'; enabled = $true; endpoint = $OpenCodeUrl; basicAuthEnv = 'OPENCODE_SERVER_PASSWORD' })
}
if ($Antigravity) {
  # ADR-022: agy acp in the same allow-listed folders; the phone never picks a folder
  $agy = [ordered]@{ id = 'agy-1'; type = 'antigravity'; enabled = $true; workspaces = @($Workspaces | ForEach-Object { (Resolve-Path $_).Path }) }
  if ($AntigravityModel) { $agy.model = $AntigravityModel }
  $adapters += , $agy
}
$cfg = [ordered]@{
  version = 1
  name = "$env:COMPUTERNAME"
  dataDir = $dir
  adapters = $adapters
  retention = [ordered]@{ events = 5000; eventsMaxAgeMs = 604800000 }
  diagnostics = [ordered]@{ port = 9487 }
  probeIntervalMs = 15000
  server = [ordered]@{ port = $Port; bind = '0.0.0.0'; requireE2e = [bool]$TunnelUrl }
  # ADR-020: the paired phone may run shell commands without approval prompts only if this is true
  terminal = [ordered]@{ enabled = [bool]$EnableTerminal }
}
[IO.File]::WriteAllText($cfgPath, ($cfg | ConvertTo-Json -Depth 6), (New-Object Text.UTF8Encoding $false))
Write-Host "Wrote $cfgPath"
if ($EnableTerminal) { Write-Warning 'Terminal ON: the paired phone can run any command in your workspaces without approval prompts (ADR-020).' }

# 2b. OpenCode serve: a hidden launcher in the first workspace folder, now and at login
if ($StartOpenCode) {
  $oc = Get-Command opencode -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
  $ocExe = if ($oc) { $oc.Source } else { Write-Warning 'opencode is not on PATH: install OpenCode; the launcher looks for it again at login.'; 'opencode' }
  $ocDir = (Resolve-Path $Workspaces[0]).Path
  $ocVbs = Join-Path $bin 'opencode-serve.vbs'
  $ocLine = '""' + $ocExe + '"" serve --port ' + $ocUri.Port + ' --hostname 127.0.0.1'
  [IO.File]::WriteAllText($ocVbs, "Set sh = CreateObject(""WScript.Shell"")`r`nsh.CurrentDirectory = ""$ocDir""`r`nsh.Run ""$ocLine"", 0, False`r`n", (New-Object Text.UTF8Encoding $false))
  Write-Host "Wrote $ocVbs"
}
if ($OpenCode) { Write-Host "OpenCode: run terminal tasks as 'opencode run --attach $OpenCodeUrl ""...""' so they stream live on the phone (a plain 'opencode run' shows up there only after it ends)." }
if ($ConfigOnly) { Write-Host 'Config only: nothing installed, started or registered.'; return }
if ($StartOpenCode) {
  if (-not $NoAutostart) {
    Set-ItemProperty 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run' -Name 'CHERNOBYL OpenCode' -Value "wscript.exe `"$ocVbs`""
  }
  if (Get-NetTCPConnection -LocalPort $ocUri.Port -State Listen -ErrorAction SilentlyContinue) {
    Write-Host "Port $($ocUri.Port) is already listening; not starting a second OpenCode server."
  } else {
    Start-Process wscript.exe -ArgumentList "`"$ocVbs`""
    Write-Host "OpenCode started: $OpenCodeUrl (in $ocDir)"
  }
}

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

# 5b. ngrok tunnel: hidden launcher, now and at login
if ($NgrokDomain) {
  $vbs = Join-Path $bin 'ngrok-tunnel.vbs'
  $line = '""' + $Ngrok + '"" http ' + $Port + ' --url ' + $NgrokDomain + ' --log=false'
  [IO.File]::WriteAllText($vbs, "CreateObject(""WScript.Shell"").Run ""$line"", 0, False`r`n", (New-Object Text.UTF8Encoding $false))
  if (-not $NoAutostart) {
    Set-ItemProperty 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run' -Name 'CHERNOBYL ngrok' -Value "wscript.exe `"$vbs`""
  }
  Get-Process ngrok -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq (Resolve-Path $Ngrok).Path } | Stop-Process -Force
  Start-Process wscript.exe -ArgumentList "`"$vbs`""
  Write-Host "ngrok tunnel started: $TunnelUrl -> 127.0.0.1:$Port"
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

# 5d. approvals for PC sessions (only on request: this edits Claude Code's own settings.json)
if ($PhoneApprovals) { & $target --install-hooks | Out-Host }

# 5c. prove the tunnel reaches the companion: /health without the token answers 401 from the
# companion itself (a non-browser user agent skips ngrok's browser warning page)
if ($TunnelUrl) {
  $probe = ($TunnelUrl -replace '^wss://', 'https://').TrimEnd('/') + '/health'
  $seen = $null
  foreach ($i in 1..15) {
    try { Invoke-WebRequest -Uri $probe -UseBasicParsing -UserAgent 'chernobyl-setup' -TimeoutSec 5 | Out-Null; $seen = 200 }
    catch { $seen = [int]$_.Exception.Response.StatusCode }
    if ($seen -eq 401) { break }
    Start-Sleep -Seconds 2
  }
  if ($seen -eq 401) { Write-Host "Tunnel OK: $TunnelUrl reaches the companion." }
  else { Write-Warning "Tunnel check failed (last HTTP status: $seen). If you use ngrok, run it once in a terminal to see why: & '$Ngrok' http $Port --url $NgrokDomain" }
}

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
