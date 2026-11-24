# Remote Access & Transport Guide

CHERNOBYL connects a phone app to a local desktop companion without any hosted cloud relay (ADR-011). Because security is established at pairing rather than transport, you can bring any network transport: home Wi-Fi, Tailscale, or a reverse proxy/tunnel.

## 1. Security Architecture: What is Protected

- **Protected everywhere (E2E AEAD):** Security lives in pairing, not transport (ADR-011). Scanning the pairing QR exchanges a 256-bit Pre-Shared Key (PSK) generated via OS CSPRNG (`main.rs`) and protected on disk with Windows DPAPI (`main.rs`). The PSK is **never transmitted on the wire**. The handshake (`hs_hello → hs_challenge → hs_proof → hs_ok`) proves mutual PSK possession using HMAC-SHA-256 and fresh nonces (`server.rs`). All subsequent WebSocket text frames are sealed with ChaCha20-Poly1305 AEAD (`server.rs`). Replay, tampering, or sequence desync tears down the socket immediately.
- **Connection metadata:** Transport intermediaries (routers, VPN relays, reverse proxies) can observe connection timing, frame lengths, and the HTTP upgrade request (including the `?token=` query string, unless the transport adds TLS).
- **Admission token:** The transport token (`--token` / `HOST_TOKEN`) is connection admission only (`server.rs`). It prevents unauthorized socket upgrades and port scanning.
- **Remote posture & Decision D9:** Beyond loopback, any `hello` sent before completing the E2E handshake is refused state (`ack_err("hello", "e2e session required (remote posture)")`, `server.rs`). An eavesdropper holding only the transport token cannot read session state or prompts.

## 2. Setup Recipes

### Recipe A: Same Wi-Fi / Local LAN
Direct connection over your local home or office network.

1. **Start the companion** (bind all interfaces or your LAN IP, requiring a token):
   ```powershell
   .\chernobyl-companion.exe --bind 0.0.0.0 --port 8787 --token <SECRET_TOKEN>
   ```
2. **Generate pairing QR** (specifying the LAN endpoint):
   ```powershell
   .\chernobyl-companion.exe --pair --endpoint ws://192.168.1.50:8787
   ```
   *(Replace `192.168.1.50` with your desktop's LAN IP. If the companion is already running, `--pair` asks it to create the offer, so pairing works without a restart.)*
3. On the phone, open **Connect a host**: type the token in the **Access token** field (it goes to the keystore), scan or paste the pairing QR, then tap **Pair with this host**. The token is saved with that pairing.
   Paired without it? Type the token in the same field with a name and address and tap **Connect host**: the app reconnects with it straight away. A token stays with the host it was entered for: pairing a different host drops it.
   *(App 0.1.0, the APK in releases v0.1.0 and v0.1.1, deletes a token typed before pairing. There, pair first, then save the token with **Connect host**, then force-stop and reopen the app. Fixed in the next app release.)*

### Recipe B: Tailscale (Recommended Mesh VPN)
Secure remote access across networks without opening router firewall ports.

1. Install Tailscale on your desktop and phone. Note your desktop's tailnet IP (e.g., `100.x.y.z`) or MagicDNS hostname.
2. **Start the companion** (bind your Tailscale IP or `0.0.0.0`):
   ```powershell
   .\chernobyl-companion.exe --bind 100.x.y.z --port 8787 --token <SECRET_TOKEN>
   ```
3. **Generate pairing QR**:
   ```powershell
   .\chernobyl-companion.exe --pair --endpoint ws://100.x.y.z:8787
   # or with MagicDNS:
   .\chernobyl-companion.exe --pair --endpoint ws://my-desktop.tailnet-name.ts.net:8787
   ```

### Recipe C: Cloudflare Tunnel or Reverse Proxy (Loopback wss://)
Expose a public TLS hostname forwarding to `ws://127.0.0.1:8787`.

1. Point your tunnel or reverse proxy (Cloudflare Tunnel, Caddy, or Nginx) to loopback `ws://127.0.0.1:8787`.
2. **Start the companion**:
   ```powershell
   .\chernobyl-companion.exe --bind 127.0.0.1 --port 8787 --token <SECRET_TOKEN> --require-e2e
   ```
3. **Generate pairing QR** (using your public `wss://` URL):
   ```powershell
   .\chernobyl-companion.exe --pair --endpoint wss://agents.example.com
   ```
> [!WARNING]
> **Always pass `--require-e2e` behind a tunnel.** The proxy connects from loopback, so without it the companion keeps loopback dev parity (threat model D2 residual (c)): anyone on the internet holding the token could run plaintext commands. With `--require-e2e` (or `"server": {"requireE2e": true}` in `companion.json`) every command and the state snapshot need the paired E2E session, exactly like a non-loopback bind.
>
> Since 2026-11-15 a request that carries a proxy header (`X-Forwarded-For/-Host/-Proto`, `Forwarded`, `ngrok-*`) is treated as remote for that connection even without the flag, and is refused outright when no token is set. That is a backstop for ngrok and most proxies, which add those headers; a proxy that strips them gets none of it, so keep `--require-e2e`. The token must be at least 32 characters (the companion will not start otherwise).

## 3. Multiple Endpoints & Failover (S10)

The `--endpoint` flag is repeatable (`main.rs`). The companion embeds them into the QR in the exact order specified:

```powershell
.\chernobyl-companion.exe --pair --endpoint ws://192.168.1.50:8787 --endpoint ws://100.x.y.z:8787
```

- **Dial order:** Advertise local LAN first, then Tailscale/remote second.
- **Failover behavior:** The phone dials endpoints in QR order (`transport.ts`). If LAN is unreachable (e.g., leaving home Wi-Fi), it automatically falls back to Tailscale. Once connected and handshaken, the index resets to primary for subsequent reconnections (`transport.ts`).
- **QR capacity:** If multiple endpoints exceed terminal QR capacity (213 bytes, QR v10-M), `companion pair` prints `payload: <json>` to stdout before exiting (`main.rs`). Paste this payload directly into the phone app.

## 4. Hardening Checklist

- [ ] **Enforce Token:** The companion refuses to bind beyond loopback without `--token` or `HOST_TOKEN`, and refuses any token shorter than 32 characters (`main.rs`).
- [ ] **Firewall Scope:** Restrict incoming traffic on port 8787 to your private subnet or Tailscale interface. Loopback diagnostics (port 9487) is strictly bound to `127.0.0.1` (`server.rs`).
- [ ] **Browser Protection (D1):** WebSockets with browser `Origin` headers are rejected with HTTP 403 (`server.rs`) to prevent CSWSH attacks.
- [ ] **Revoke Lost Devices (D4):**
  1. List active pairings: `.\chernobyl-companion.exe --devices` (`main.rs`).
  2. Revoke a device ID: `.\chernobyl-companion.exe --revoke <device-id>` (`main.rs`).
  If the companion is running, this terminates the live session with close code `4003` (ADR-014; `transport.ts`). Re-handshake attempts are refused.

## 5. Troubleshooting: Denied vs Reconnecting

The mobile app surfaces two distinct recovery states:

| State | Behavior | Causes & Action |
|---|---|---|
| **Reconnecting** | Automatic retry loop with jittered backoff (`transport.ts`). Rotates through `--endpoint` list. Pending commands are preserved and replayed on reconnect. | Network switch, Wi-Fi drop, server restart, or missed heartbeat pongs. Normal operation; wait for network to restore. |
| **Denied** | Terminal stop (`transport.ts`). Halts all reconnect attempts until explicit user intervention. | Permanent credential or protocol rejection. See specific close reasons below. |

### Denied Causes & Fixes

- **`hs_err` or `server-proof-mismatch`:** Handshake rejected by host (`transport.ts`). Pairing offer expired (>5m), already consumed, corrupted PSK, or host reinstalled. Re-pair device using `companion --pair`.
- **`4001` (Unauthorized):** Missing or incorrect transport token during WebSocket upgrade (`server.rs`, `transport.ts`). Check token in **Settings → Connect host**.
- **`4002` (Protocol Mismatch):** Protocol version drift between app and companion (`server.rs`, `transport.ts`). Update app and companion binary.
- **`4003` (Device Revoked):** Host operator revoked the device ID (`transport.ts`). Re-pair with a fresh QR if access should be restored.
