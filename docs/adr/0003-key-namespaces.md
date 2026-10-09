# ADR 0003: Plugin keys start with byte 0x00

## Status

Accepted.

## Context

Sessions and app data share one horton keyspace. A collision would let app
code read or delete a session.

## Decision

Every plugin key starts with byte `0x00`. The app API refuses such keys.
App scans start at `[0x01]` or later. `verification/keys.rs` proves that
the two sets are disjoint and that every plugin key sorts below the floor.

## Consequences

- App code cannot touch sessions by accident.
- App keys cannot start with byte `0x00`.
