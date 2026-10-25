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

## Publish checksums (maintainer)

```powershell
node scripts/package-release.mjs
```

This hashes the release binary (and any `apps/mobile/dist-android/*.apk`) and
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
