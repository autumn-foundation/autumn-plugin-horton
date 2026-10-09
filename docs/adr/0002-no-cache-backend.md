# ADR 0002: No cache backend in v0.1

## Status

Accepted.

## Context

Autumn's `Cache` trait is synchronous. A disk-backed cache must block the
caller until the store thread answers. Autumn's Redis backend does this
with `block_in_place`, which panics on a current-thread runtime.

## Decision

v0.1 does not implement `Cache`. Apps keep the in-memory `moka` cache.

## Consequences

- The plugin never blocks a Tokio worker.
- A persistent cache is out of scope. We can add it when Autumn has an
  async cache trait.
