# ADR-001: Local-first backend (no cloud relay or account server)

**Status:** Accepted
**Date:** 2026-10-21
**Decided by:** Product owner during Phase 0 planning review
**Supersedes:** PRD §3.1-C, §3.2, §5.1 (cloud database), §10 (entire cloud backend section), §15.1, §16.1, §23.1

## Context

The PRD specified a three-layer architecture with a cloud relay and account backend hosting on a server (PostgreSQL, containerized services). The product owner evaluated the operational cost and infrastructure burden and directed: *"make it local, and the server will be online when desktop companion is online, and when its offline the client will see a normal system offline message"* — followed by explicit confirmation: *"Yes — fully local, tunnel for remote."*

## Decision

The entire backend (gateway, database, adapters, pairing) runs inside the desktop companion on the user's own computer:

- The phone reaches each companion directly — same LAN, or an **outbound tunnel URL** (cloudflared, Phase 5′) for remote access. No inbound ports, preserving the PRD's core NAT-friendly requirement (§3.3).
- The companion is the sole authority for its own state. There is **no hosted service of any kind**: no `services/cloud-api`, no hosted `services/relay`, no PostgreSQL.
- Multi-host means the phone pairs with each companion individually (each host = its own URL + paired credential) instead of a central account listing them.
- When a companion is offline, the phone shows the ordinary system offline state for that host; other paired hosts are unaffected.
- The notification worker becomes an in-companion service; push (FCM) is sent directly from the companion machine.

## Consequences

- PRD sections listed above are void; requirements that remain meaningful (no inbound ports, revocation, offline honesty, per-host authorization, data minimization) are re-homed onto the companion/gateway.
- Phases 4+5 of the roadmap collapse into one local identity + secure gateway phase (Phase 4′), then cross-network proof (Phase 5′).
- Data retention (PRD §16.3) is enforced in the companion's SQLite only.
- "Worldwide access" = tunnel-verified, never a claim about a hosted relay.
- Future re-introduction of a hosted relay remains possible but is out of scope and not designed for now.
