# make-signing-key.ps1: create the APK release key once (free; keytool ships with the JDK).
#
#   powershell -ExecutionPolicy Bypass -File scripts\make-signing-key.ps1
#
# Writes <Dir>\chernobyl-release.jks and <Dir>\chernobyl-release.pass (a random password, never
# printed). scripts/build-android.mjs signs every APK with it when CHB_KEYSTORE points at the
# .jks (the .pass file next to it is read by apksigner, not by the script).
#
# Keep both files backed up: an app signed with this key can only be updated by an APK signed
# with the same key. Lose it and the phone must uninstall the app (and re-pair) to install
# a build signed with a new key.
param(
  [string]$Dir = 'G:\Secrets\Android',
  [string]$Keytool = 'G:\Android\jdk\bin\keytool.exe'
)
$ErrorActionPreference = 'Stop'
$jks = Join-Path $Dir 'chernobyl-release.jks'
$pass = Join-Path $Dir 'chernobyl-release.pass'
if (Test-Path $jks) { throw "$jks already exists: never overwrite a release key (an installed app could no longer be updated)." }
if (-not (Test-Path $Keytool)) { throw "keytool not found at $Keytool (pass -Keytool)" }
New-Item -ItemType Directory -Force $Dir | Out-Null

$bytes = New-Object byte[] 24
[Security.Cryptography.RandomNumberGenerator]::Create().GetBytes($bytes)
$pw = -join ($bytes | ForEach-Object { $_.ToString('x2') })
[IO.File]::WriteAllText($pass, $pw, (New-Object Text.ASCIIEncoding))
Remove-Variable pw

& $Keytool -genkeypair -keystore $jks -storetype PKCS12 -alias chernobyl -keyalg RSA -keysize 4096 `
  -validity 10000 -dname 'CN=CHERNOBYL, O=CHERNOBYL' -storepass:file $pass -keypass:file $pass | Out-Null
if ($LASTEXITCODE -ne 0) { Remove-Item $jks, $pass -ErrorAction SilentlyContinue; throw 'keytool failed' }

Write-Host "Release key created: $jks"
Write-Host "Password file:       $pass (random; not shown)"
Write-Host "Build signed APKs with:  `$env:CHB_KEYSTORE = '$jks'; node scripts\build-android.mjs"
