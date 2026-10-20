# ADR-003: SQLite everywhere; no PostgreSQL

**Status:** Accepted
**Date:** 2026-10-21
**Decided by:** Product owner ("make it local"), confirmed in planning review
**Supersedes:** PRD §5.1 (cloud database), §16.1 (cloud database tables), §16.2 split, §22.4 (PostgreSQL migrations), §23.1

## Context

PRD mandated PostgreSQL for cloud metadata and SQLite for the companion. ADR-001 removes the cloud layer entirely, and the machine has neither Docker nor psql installed (low-disk constraint from the user).

## Decision

- **All durable state lives in the companion's SQLite database**, using the PRD §16.2 table set extended with the §16.1 tables that remain meaningful locally: `paired_devices`, `pairing_challenges`, `push_tokens`, `notification_preferences`, `audit` (renamed/merged per Phase 2 schema).
- There is no cloud database to operate, back up or migrate.
- Backups (Phase 8) = copying the companion SQLite file, with integrity checks.

## Consequences

- No new heavy dependencies are installed for storage; driver choice deferred to Phase 2 (`node:sqlite` builtin vs `better-sqlite3`), recorded in the Phase 2 ADR if contested.
- Retention enforcement (PRD §16.3) applies to one database.
- PRD security/integration tests mentioning PostgreSQL are dropped; equivalent SQLite migration/recovery tests replace them (PRD §22.4).
