# Install & verify (ADR-007)

CHERNOBYL ships the companion as an **unsigned binary with published SHA-256
checksums** (ADR-007). There is no code-signing certificate yet, so this document
is the operator's install + verify + troubleshooting guide — a Phase 8 exit
criterion.

## What you get

One zero-dependency binary: `services/companion-rs/target/release/chernobyl-companion.exe`
(Windows) / `chernobyl-companion` (Linux/macOS). It is the protocol-v2 WebSocket
server + pairing/E2E layer + adapters + loopback diagnostics.

## Build

```powershell
cargo build --release --manifest-path services/companion-rs/Cargo.toml
```

### The Android app (APK)

You need JDK 17 (`JAVA_HOME`) and the Android SDK (`ANDROID_HOME`) with platform 36,
build-tools 36.0.0, NDK 27.1.12297006 and CMake 3.31.6 (`sdkmanager "cmake;3.31.6"`).
On Windows, React Native's native build has object paths longer than 260 characters.
Long paths must be enabled (`LongPathsEnabled=1`), and only the ninja that ships with
CMake 3.31 or later handles them. The script selects that CMake for you.

```powershell
node scripts/build-android.mjs                 # arm64-v8a + x86_64
node scripts/build-android.mjs --abi x86_64    # emulator only
```

It compiles the shared packages, runs `expo prebuild` if `apps/mobile/android` is missing,
then builds the release APK into `release/`, where `package-release` hashes it. Gradle signs with the debug keystore from Expo's template; set `CHB_KEYSTORE` and the script
re-signs the APK with your own release key (free; `apksigner` from the SDK build-tools):

```powershell
powershell -ExecutionPolicy Bypass -File scripts\make-signing-key.ps1   # once: <dir>\chernobyl-release.jks + .pass (random, never printed)
$env:CHB_KEYSTORE = 'G:\Secrets\Android\chernobyl-release.jks'
node scripts/build-android.mjs                                        # or: --sign-only <apk>
```

Back up the `.jks` and `.pass`: updates must be signed with the same key, and a phone with the
app installed refuses an APK signed with a different one (uninstall and re-pair to switch keys).
It is not a Play Store identity (Play needs a developer account).

To test on an emulator, `scripts/device/validate.mjs` runs the phone checklist against the
APK and a real companion. It pairs, connects, restarts things, revokes the device and saves
evidence. The emulator reaches the PC's loopback as `10.0.2.2`.

### Android 4.4 tablets (KitKat client)

The main app needs Android 7.0. For an Android 4.4 tablet there is a separate native client,
`android-kitkat/` (ADR-019): same pairing, same E2E encryption, about 300 KB, no Google Play
services needed.

Build and sign it (JDK 17 + Android SDK; uses the cached Gradle 9.4.1 / AGP 9.2.1; runs the unit
tests first):

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File android-kitkat\build-release.ps1
# -> release\chernobyl-kitkat-0.1.0.apk, signed v1 + v2 with CHB_KEYSTORE (default G:\Secrets\Android\chernobyl-release.jks)
```

Install on the tablet:

1. Settings → Security → turn on **Unknown sources**.
2. Copy the APK over USB and open it in a file manager, or `adb install release\chernobyl-kitkat-0.1.0.apk`.
3. Open **CHERNOBYL**. It starts on **Pair**.

Pair:

1. On the PC, with the companion running, create an offer for an address the tablet can reach:
   `chernobyl-companion --pair --endpoint ws://<pc-lan-ip>:8787` (a LAN bind needs `--token`), or
   your tunnel's `wss://…ngrok-free.app` URL. Offers expire after 5 minutes and work once.
2. Paste the `payload: {...}` line into the first field (or tap **Scan QR** and point the camera at
   the terminal QR), type the access token, tap **Pair and connect**.
3. The rail shows **Online** and Home fills in. "Keys stored in" on the Pair screen says whether the
   Android Keystore protects the stored key (the normal case) or only app-private storage.

Good to know on a real 4.4 tablet:

- `wss://` works: the app turns on TLS 1.2 itself and carries the Let's Encrypt (ISRG) roots that
  Android 4.4 lacks, so ngrok URLs validate. A certificate error in "Status" means some other CA.
- A foreground notification keeps the connection while the app is in the background. When the
  screen is off, KitKat may sleep the CPU and Wi-Fi; alerts then arrive when it wakes. Tick
  **Keep the connection awake** on the Pair screen for instant alerts (costs battery).
- "Refused: unauthorized" = wrong or missing token: type the right one and leave the payload field
  empty to keep the pairing. "Refused: unknown device" / "offer expired" = make a new offer.
  "device revoked" = pair again.
- Removing or changing the lock screen can wipe KitKat's keystore; the app then asks to pair again.

## Publish checksums (maintainer)

```powershell
node scripts/package-release.mjs
```

This hashes the release binary (and any `release/*.apk`) and
writes `release/SHA256SUMS.txt` in the standard `<hex>  <path>` format. It also
prints an explicit note that these are **unsigned** binaries — checksums prove
integrity, not authorship.

## Verify a download (operator)

Before running a downloaded binary, re-hash it and compare against the published
sum:

```powershell
node scripts/package-release.mjs --verify          # cross-platform
# or, on Linux/macOS:
sha256sum -c release/SHA256SUMS.txt
```

`--verify` re-hashes every artifact listed in `SHA256SUMS.txt` and exits non-zero
on any mismatch or missing file. Do this **before** you run an unsigned binary.

## Run

```powershell
.\chernobyl-companion.exe                 # loopback ws://127.0.0.1:8787 (+ diag 9487)
.\chernobyl-companion.exe --token <t>     # require a token (or set HOST_TOKEN)
.\chernobyl-companion.exe --pair          # print a single-use pairing QR
.\chernobyl-companion.exe --tui           # boot + terminal UI: sessions & approvals
                                          # (j/k move, a approve, x reject, r refresh,
                                          #  ? help, q quit) — loopback operator console
.\chernobyl-companion.exe --background    # windowless (used by autostart)
```

The default `companion.json` (in `%APPDATA%\CHERNOBYL`, or `CHERNOBYL_HOME`) enables a demo `mock` adapter whose hosts, agents, sessions and approvals the app marks **Simulated**; set `"enabled": false` on that adapter entry to boot without any demo data.

Start-on-login (user-level, no elevation):

```powershell
.\chernobyl-companion.exe --install-autostart    # HKCU Run key, launches --background
.\chernobyl-companion.exe --uninstall-autostart  # idempotent removal
```

### One-shot Windows setup (`scripts/setup-windows.ps1`)

From a checkout with a release build, one script installs the exe, writes `companion.json`
(Claude Code on, your folders allow-listed, LAN bind), creates the access token, adds the
firewall rule (elevated prompt), registers start-on-login and prints the pairing QR:

```powershell
powershell -ExecutionPolicy Bypass -File scripts\setup-windows.ps1 -Workspaces 'G:\Projects\app'
```

| Flag | Effect |
|---|---|
| `-TunnelUrl wss://host` / `-NgrokDomain <name>.ngrok-free.dev` | second endpoint for away from home; turns on `requireE2e` |
| `-OpenCode [-OpenCodeUrl http://127.0.0.1:4096]` | adds `{"id":"opencode-1","type":"opencode","endpoint":…,"basicAuthEnv":"OPENCODE_SERVER_PASSWORD"}` (attach-only) |
| `-StartOpenCode` | also runs `opencode serve --port <port> --hostname 127.0.0.1` hidden in the first workspace folder, now and at login (HKCU Run `CHERNOBYL OpenCode`); implies `-OpenCode`, loopback URLs only |
| `-EnableTerminal` | writes `"terminal": {"enabled": true}`: the paired phone can run shell commands in the workspaces **without approval prompts** (ADR-020). Off otherwise |
| `-ConfigOnly` | writes `companion.json` (and the OpenCode launcher) and stops: no install, token, firewall, autostart or process changes |
| `-NoAutostart`, `-Port`, `-Exe` | as named |

**OpenCode password.** No password is ever written to `companion.json`. If you start `opencode
serve` with `OPENCODE_SERVER_PASSWORD`, set it as a *user* environment variable (System
Properties > Environment Variables, or `[Environment]::SetEnvironmentVariable('OPENCODE_SERVER_PASSWORD', '<pw>', 'User')`
in a PowerShell you then close), plus `OPENCODE_SERVER_USERNAME` if it is not `opencode`. Open
a new PowerShell before running setup: the companion and the OpenCode launcher both read it at
start, from `basicAuthEnv`. Unset means no auth (the server's loopback default).

`scripts\test-setup-windows.ps1` checks the script without touching your real config, Run key
or variables (AST parse, then `-ConfigOnly` runs in a temp `APPDATA`, each file loaded by a debug
build of the companion).

## Backup / restore (same machine)

```powershell
.\chernobyl-companion.exe --backup  C:\backup\companion.journal
.\chernobyl-companion.exe --restore C:\backup\companion.journal
```

Restore refuses a checksum mismatch. PSKs inside the journal are **DPAPI-bound to
this machine + user**, so a restore onto a different machine requires re-pairing
the phone.

## Troubleshooting

### Windows SmartScreen ("Windows protected your PC")

Expected on first run — the binary is unsigned (ADR-007). To run it:

1. Click **More info**.
2. Click **Run anyway**.

Or unblock the file first (right-click → Properties → Unblock → Apply), or run it
from a terminal. This warning is a disclosed release limitation, not a bug.

### "connection refused" from the phone

- The companion binds loopback by default. For a phone on the same network, bind a
  LAN address and advertise it: `--bind 0.0.0.0 --endpoint ws://<lan-ip>:8787`
  (repeat `--endpoint` for LAN + tailnet + tunnel in one QR).
- A non-loopback bind **requires** `--token`; the companion refuses to start
  otherwise.
- For complete transport setup guides (LAN, Tailscale, tunnels), failover, and
  hardening, see [`REMOTE-ACCESS.md`](REMOTE-ACCESS.md).

### The phone connects but commands are refused

Beyond loopback a token alone is not enough (decision D2): the E2E session
handshake must complete first. Scan the pairing QR (`--pair`) so the phone holds a
per-device PSK, then the `hs_*` handshake seals every frame.

### "unauthorized" / socket closed 4001

Wrong or missing token. Set the same token on the companion (`--token` / `HOST_TOKEN`)
and in the app's **Connect host** modal.

### Console window flashes at login

Fixed: `--install-autostart` now writes `--background`, which detaches the console
after boot. Diagnostics remain on the loopback `/health` + `/metrics` routes.
