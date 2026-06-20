# ChiffonDB — Project Guide for Claude

## Overview

An embedded, schema-driven graph DBMS designed for AI-driven code implementation.
Written in pure Rust. See [ARCHITECTURE.md](ARCHITECTURE.md) for the storage design.

## Crate / package layout

```
chiffondb-core/   # Storage, schema, and traversal engine (Rust library)
chiffondb/        # Public crate: library + CLI binary (the `chiffon` command)
```

Dart bindings and the GUI browser live in separate repositories — this repo is pure Rust.

## Status

The whole engine's resident memory is bounded by the page-cache budget (all on-disk pages go
through a fixed-size LRU cache; there is no in-memory index that grows with the data). Known
limit: the topology segment is fixed-size, so a database holds at most ~2000 nodes (a
variable-length segment is future work).

## Development rules

### Tests
- **Always write tests; choose *when* by the nature of the code** (TDD is a tool, not a rule):
  - Pure logic with an obvious spec/API (parsers, serialization, capacity boundary values)
    → test-first.
  - APIs that solidify mid-implementation against the storage layer, and invariants that only
    become clear once the implementation settles (corruption injection, property tests,
    symmetry) → covered after the fact by the review session.
- Keep `cargo test` green at all times.
- proptest cases: `50` during development, `1000` in CI (`PROPTEST_CASES` env var).

### Review & progress workflow
- When an implementation reaches a stopping point, create and commit
  `docs/review-request-YYYY-MM-DDx.md`.
- When responding to a review request, follow [docs/review-process.md](docs/review-process.md)
  (separate the verifier from the fixer; back claims with the real code and throwaway probes;
  do not edit core logic — send it back to the implementation session). No need to read it
  when there is no review request.

### Code style
- `unwrap()` / `expect()` are banned. Propagate errors with the `?` operator.
- No `unsafe` (the codebase currently has none).
- Comment only non-obvious WHY. Do not write WHAT.
- **Write all code comments in English** (doc comments and inline comments alike).

### Storage-layer notes
- Page size is **fixed at 4096 bytes**.
- Byte manipulation is hand-written; mind alignment.
- Use `RecordId` (PageId + SlotId); raw pointers are forbidden.
- Durability is via the WAL (write-through), checkpointed on `flush()`. True copy-on-write /
  MVCC are planned but **not yet implemented** (see [ARCHITECTURE.md](ARCHITECTURE.md)).

### Error design
- Define error variants on `GraphError` with `thiserror`; propagate with `?`.

## Common commands

```bash
cargo test                          # Run all tests
cargo test -p chiffondb-core        # Test the core only
cargo test -- --nocapture           # Show test output
cargo clippy --all-targets -- -D warnings   # Lint
cargo fmt --check                   # Format check
```

## On-disk format

- Single-file database with the `.chiffon` extension (WAL: `.chiffon-wal`, lock:
  `.chiffon-lock`).
- Header magic is `CHIFFON\0`; format `VERSION` is bumped on any layout change, and a
  mismatch is rejected by `FileHeader::deserialize` rather than silently misread.
