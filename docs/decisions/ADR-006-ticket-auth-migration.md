# ADR-006: Migrate query-string token auth to short-lived connection tickets

**Status:** Accepted (implementation scheduled Phase 4′)
**Date:** 2026-10-21
**Relates to:** PRD §11.4, §11.1; conflict register C1

## Context

The prototype's WebSocket authentication sends a long-lived token as a `?token=` query parameter (chosen because React Native's `WebSocket` cannot set headers). The PRD explicitly forbids long-lived query-string credentials for production: URLs leak into logs, proxies and history.

## Decision

- Keep the v1 query-token path **for dev/mock and existing tests** until Phase 4′ lands.
- Phase 4′ replaces it with a **short-lived, single-use connection ticket** flow: the phone authenticates to the gateway over an ordinary authenticated request, receives a ticket bound to (device, host, audience, short expiry), and presents it in the WebSocket handshake. Tickets are never logged; replayed or expired tickets are rejected.
- `PROTOCOL_VERSION` gates old clients, so the migration is clean: v1 token clients fail the version/ticket negotiation with an actionable error rather than silently connecting.

## Consequences

- Security tests for ticket expiry, single-use replay and binding mismatch are added with Phase 4′.
- The current integration tests asserting `?token=` → close 4001 keep passing until they are migrated alongside the implementation change.
