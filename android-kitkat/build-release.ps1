# Builds the KitKat client and signs it with the owner's release key (v1 JAR + v2;
# Android 4.4 verifies only v1). Output: release/chernobyl-kitkat-<version>.apk.
#   powershell -NoProfile -ExecutionPolicy Bypass -File android-kitkat\build-release.ps1
# Env: JAVA_HOME (JDK 17), ANDROID_HOME (SDK), CHB_KEYSTORE (default G:\Secrets\Android\chernobyl-release.jks;
# the password is read by apksigner from the .pass file next to it, never by this script).
# native tools write warnings to stderr; exit codes are checked instead
$ErrorActionPreference = 'Continue'
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$root = Split-Path -Parent $here
if (-not $env:JAVA_HOME) { $env:JAVA_HOME = 'G:\Android\jdk' }
if (-not $env:GRADLE_USER_HOME) { $env:GRADLE_USER_HOME = 'G:\Android\gradle' }
$sdk = if ($env:ANDROID_HOME) { $env:ANDROID_HOME } else { 'G:\Android\sdk' }
$ks = if ($env:CHB_KEYSTORE) { $env:CHB_KEYSTORE } else { 'G:\Secrets\Android\chernobyl-release.jks' }
$pass = $ks -replace '\.jks$', '.pass'
if (-not (Test-Path $ks) -or -not (Test-Path $pass)) { throw "release keystore or its .pass file not found: $ks" }

Push-Location $here
try {
    & .\gradlew.bat testDebugUnitTest assembleRelease --console=plain
    if ($LASTEXITCODE -ne 0) { throw "gradle failed ($LASTEXITCODE)" }
} finally { Pop-Location }

$version = (Select-String -Path "$here\build.gradle" -Pattern "versionName '([^']+)'").Matches[0].Groups[1].Value
$tools = Get-ChildItem "$sdk\build-tools" | Where-Object { Test-Path "$($_.FullName)\apksigner.bat" } | Sort-Object { [version]$_.Name } | Select-Object -Last 1
$apksigner = "$($tools.FullName)\apksigner.bat"
New-Item -ItemType Directory -Force "$root\release" | Out-Null
$out = "$root\release\chernobyl-kitkat-$version.apk"
& $apksigner sign --ks $ks --ks-pass "file:$pass" --ks-key-alias chernobyl --min-sdk-version 19 `
    --v1-signing-enabled true --v2-signing-enabled true --v4-signing-enabled false --out $out "$here\build\outputs\apk\release\chernobyl-kitkat-release.apk"
if ($LASTEXITCODE -ne 0) { throw "apksigner sign failed" }
& $apksigner verify --min-sdk-version 19 --verbose --print-certs $out
if ($LASTEXITCODE -ne 0) { throw "apksigner verify failed" }
Write-Output "signed: $out ($((Get-Item $out).Length) bytes)"
