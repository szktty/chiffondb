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

The whole engine's resident memory is bounded by the page-cache budget (all on-disk pages,
including the indexes, go through a fixed-size LRU cache; no in-memory index grows with the data —
except `live_node_rids`, which materializes RecordIds during a full scan). Node/edge/property
pages grow on the file's append tail via per-kind page directories, so the old ~2000-node cap is
gone (ceiling is now the u32 logical page space). Insertion is amortized O(1) via per-kind
free-slot hints in the header. Deleting/updating a node or edge now reclaims its property space
(A-2): freed property pages go on an intrusive free-page list and are reused; freed slots are
tombstoned (page-granular reclaim — a page returns to the list once fully empty). On-disk `VERSION`
is 10. See `ARCHITECTURE.md` for the format, the label/property/unique indexes, and the remaining
known limitations (property space reclaim is page-granular, not intra-page compaction; schema-chain
space is still not reclaimed; crash atomicity is not guaranteed per API call).

## Development rules

### Branching (git flow)
- **`develop` is the integration branch.** Cut feature branches from `develop` and merge them
  back into `develop` (squash). Day-to-day work targets `develop`, never `main`.
- **`main` is the released, stable line.** It is updated *only at release time* by merging
  `develop` → `main` (and tagging). Do **not** merge feature branches into `main` directly.
- So: `feature/* → develop` (squash) for normal work; `develop → main` only on release.
- Merges are gated on explicit user instruction (see `CLAUDE.md` note on confirming
  hard-to-reverse actions); don't merge without being asked.

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
- Driven by triad (local tool, `~/work/dev/products/ai/triad`): each session registers as
  worker or reviewer and follows its instructions — see `.claude/skills/triad/SKILL.md`.
- The actual review/implement steps (verify/result/fix/request/close) are defined in
  `.claude/skills/triad-plan/SKILL.md`. Read it only when running one of those steps.
- Record in-progress work from AI sessions in `../chiffondb-private/docs/working.md`.

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
