# Checks setup-windows.ps1 without touching the real %APPDATA%\CHERNOBYL, HKCU Run or user env:
# parse it with the AST parser, then run -ConfigOnly in a child PowerShell whose APPDATA and
# LOCALAPPDATA point at a temp folder, and load each generated config with the companion.
#
#   cargo build --manifest-path services\companion-rs\Cargo.toml
#   powershell -NoProfile -ExecutionPolicy Bypass -File scripts\test-setup-windows.ps1
param([string]$Repo, [string]$Exe)
$ErrorActionPreference = 'Stop'
if (-not $Repo) { $Repo = Join-Path $PSScriptRoot '..' }
if (-not $Exe) { $Exe = Join-Path $Repo 'services\companion-rs\target\debug\chernobyl-companion.exe' }
$script = Join-Path $Repo 'scripts\setup-windows.ps1'
$errs = $null
[System.Management.Automation.Language.Parser]::ParseFile($script, [ref]$null, [ref]$errs) | Out-Null
if ($errs.Count) { $errs | ForEach-Object { Write-Host "PARSE: $_" }; exit 1 }
Write-Host 'parse ok'

$tmp = Join-Path ([IO.Path]::GetTempPath()) ("chb-setup-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
$ws = Join-Path $tmp 'ws'
New-Item -ItemType Directory -Force $ws | Out-Null
$runKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
# the key is missing on a fresh CI image: no value is a state too
function Get-RunValues { Get-ItemProperty $runKey -ErrorAction SilentlyContinue | Out-String }
$before = Get-RunValues
$envBefore = [Environment]::GetEnvironmentVariable('HOST_TOKEN', 'User')

function Run-Setup([string[]]$Extra) {
  $env:APPDATA = Join-Path $tmp 'roaming'
  $env:LOCALAPPDATA = Join-Path $tmp 'local'
  $out = & powershell -NoProfile -ExecutionPolicy Bypass -File $script -Workspaces $ws -ConfigOnly @Extra 2>&1
  if ($LASTEXITCODE) { $out | Out-Host; throw "setup failed: $Extra" }
  $out | Out-Host
  Get-Content -Raw (Join-Path $tmp 'roaming\CHERNOBYL\companion.json') | ConvertFrom-Json
}

$saved = @{ APPDATA = $env:APPDATA; LOCALAPPDATA = $env:LOCALAPPDATA }
try {
  $c = Run-Setup @()
  if ($c.terminal.enabled -ne $false) { throw 'terminal must default to off' }
  if (@($c.adapters | Where-Object type -eq 'opencode').Count) { throw 'no opencode without -OpenCode' }
  if (@($c.adapters | Where-Object type -eq 'antigravity').Count) { throw 'no antigravity without -Antigravity' }

  $c = Run-Setup @('-Antigravity')
  $ag = @($c.adapters | Where-Object type -eq 'antigravity')
  if ($ag.Count -ne 1 -or $ag[0].id -ne 'agy-1' -or -not $ag[0].enabled -or @($ag[0].workspaces)[0] -ne (Resolve-Path $ws).Path) { throw "bad antigravity entry: $($ag | ConvertTo-Json)" }
  if ($ag[0].PSObject.Properties.Name -contains 'model' -or $ag[0].PSObject.Properties.Name -contains 'args') { throw 'no model unless asked, never args' }
  $c = Run-Setup @('-AntigravityModel', 'gemini-3.8-flash-low')
  if (@($c.adapters | Where-Object type -eq 'antigravity')[0].model -ne 'gemini-3.8-flash-low') { throw '-AntigravityModel must set the model' }
  & $Exe --config (Join-Path $tmp 'roaming\CHERNOBYL\companion.json') --devices | Out-Host
  if ($LASTEXITCODE) { throw 'the companion refused the antigravity config' }

  if (@($c.adapters | Where-Object type -eq 'codex').Count) { throw 'no codex without -Codex' }

  $c = Run-Setup @('-Codex')
  $cx = @($c.adapters | Where-Object type -eq 'codex')
  if ($cx.Count -ne 1 -or $cx[0].id -ne 'codex-1' -or -not $cx[0].enabled -or @($cx[0].workspaces)[0] -ne (Resolve-Path $ws).Path) { throw "bad codex entry: $($cx | ConvertTo-Json)" }
  if ($cx[0].PSObject.Properties.Name -contains 'binary') { throw 'codex.exe is found at start, never written' }
  & $Exe --config (Join-Path $tmp 'roaming\CHERNOBYL\companion.json') --devices | Out-Host
  if ($LASTEXITCODE) { throw 'the companion refused the generated codex config' }

  $c = Run-Setup @('-OpenCode', '-EnableTerminal')
  $oc = @($c.adapters | Where-Object type -eq 'opencode')
  if ($oc.Count -ne 1 -or $oc[0].id -ne 'opencode-1' -or -not $oc[0].enabled -or $oc[0].endpoint -ne 'http://127.0.0.1:4096') { throw "bad opencode entry: $($oc | ConvertTo-Json)" }
  if ($oc[0].basicAuthEnv -ne 'OPENCODE_SERVER_PASSWORD' -or $oc[0].PSObject.Properties.Name -contains 'basicAuth') { throw 'the password must come from the env var only' }
  if ($c.terminal.enabled -ne $true) { throw '-EnableTerminal must set terminal.enabled' }
  # the companion itself accepts the file (fail-closed loader)
  & $Exe --config (Join-Path $tmp 'roaming\CHERNOBYL\companion.json') --devices | Out-Host
  if ($LASTEXITCODE) { throw 'the companion refused the generated config' }

  $c = Run-Setup @('-StartOpenCode', '-OpenCodeUrl', 'http://127.0.0.1:4097/')
  if (@($c.adapters | Where-Object type -eq 'opencode')[0].endpoint -ne 'http://127.0.0.1:4097') { throw 'endpoint not normalised' }
  $vbs = Get-Content -Raw (Join-Path $tmp 'local\Programs\CHERNOBYL\opencode-serve.vbs')
  Write-Host $vbs
  if ($vbs -notmatch 'serve --port 4097 --hostname 127\.0\.0\.1' -or $vbs -notmatch [regex]::Escape("sh.CurrentDirectory = ""$((Resolve-Path $ws).Path)""")) { throw 'bad launcher' }

  $env:APPDATA = Join-Path $tmp 'roaming'; $env:LOCALAPPDATA = Join-Path $tmp 'local'
  $ErrorActionPreference = 'Continue'
  $bad = & powershell -NoProfile -ExecutionPolicy Bypass -File $script -Workspaces $ws -ConfigOnly -StartOpenCode -OpenCodeUrl 'http://10.0.0.5:4096' 2>&1
  if (-not $LASTEXITCODE -or ($bad | Out-String) -notmatch 'serves on this PC only') { throw 'a remote -StartOpenCode must be refused' }
  $bad = & powershell -NoProfile -ExecutionPolicy Bypass -File $script -Workspaces $ws -ConfigOnly -OpenCode -OpenCodeUrl 'https://x/y' 2>&1
  if (-not $LASTEXITCODE -or ($bad | Out-String) -notmatch 'must look like') { throw 'a bad -OpenCodeUrl must be refused' }
  $bad = & powershell -NoProfile -ExecutionPolicy Bypass -File $script -Workspaces $ws -ConfigOnly -AntigravityModel '--dangerously-skip-permissions' 2>&1
  if (-not $LASTEXITCODE -or ($bad | Out-String) -notmatch 'must be a model id') { throw 'a flag as -AntigravityModel must be refused' }
  $ErrorActionPreference = 'Stop'
} finally {
  $env:APPDATA = $saved.APPDATA; $env:LOCALAPPDATA = $saved.LOCALAPPDATA
}
if ((Get-RunValues) -ne $before) { throw 'HKCU Run changed' }
if ([Environment]::GetEnvironmentVariable('HOST_TOKEN', 'User') -ne $envBefore) { throw 'HOST_TOKEN changed' }
Remove-Item -Recurse -Force $tmp
Write-Host 'setup checks ok'
