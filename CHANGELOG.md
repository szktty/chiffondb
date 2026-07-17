# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.2.0] - 2026-07-13

### Added

- Variable-length topology: node/edge/property pages now grow on the file's append tail, resolved
  through per-kind page directories, removing the previous ~2000-node cap (the ceiling is now the
  u32 logical page space). See `ARCHITECTURE.md` for the on-disk format.
- Tier-1 label index: `list_nodes` / `count_nodes` by type are now O(matches) instead of a full
  scan; a node is indexed under every label it carries (primary + additional/dynamic).
- Tier-2 property index: fields declared `@index` in the schema get an on-disk equality index that
  accelerates `find` / `find_all` (flat and nested-scalar `PropertyPath` keys; partial index —
  scalars only). Details in `ARCHITECTURE.md` (Indexes).
- Tier-3 unique constraint: fields declared `@unique` reject duplicate values on insert/update
  (`UniqueViolation`), enforced pre-write; also queryable via the tier-2 index.
- On-disk format bumped to version 8; older files are rejected (no backward compatibility).
- Traversal: `any_key_contains` now recurses into `Json` objects and arrays, so keyword search
  reaches string values nested inside an aggregated `Json` property. Only string values are
  matched (object keys are not); top-level string matching is unchanged.
- Traversal: filter `property`, `OrderBy` `key`, property-reference `key`, group-by keys, and
  aggregate function properties accept a nested-path form `{ "path": ["props", "name"] }` in
  addition to a flat string key. Resolves to a nested scalar value (array indexing / JSON
  containment are out of scope). Existing flat-string keys are unchanged (backward compatible).
- Dynamic labels: `insert_node_with_dynamic_labels`, `add_node_label_dynamic`, and
  `add_edge_label_dynamic` register unknown label names on the fly (assigning a `u16` type id
  without `apply_schema`) and return a name → `{ id, created }` mapping. Dynamic labels share the
  schema id space and are preserved across later schema migrations.
- FFI: the dynamic-label API is exposed on `Connection` for the language bindings. Assignments
  are returned as JSON object strings (`{"id":<u16>,"created":<bool>}`), consistent with the
  existing label getters; the node insert returns a `DynamicInsertResult { rid, assignments_json }`.
- Insertion is now amortized O(1): per-kind free-slot hints in the header let node/edge/property
  inserts reuse freed slots without a linear scan.
- Property-store space reclaim: deleting or updating a node/edge now returns its property space
  to an intrusive free-page list for reuse; freed slots are tombstoned and a page rejoins the free
  list once it is fully empty (page-granular reclaim, not intra-page compaction).
- On-disk format bumped to version 10 (from 8); older files are rejected (no backward
  compatibility).

### Fixed

- Crash recovery: `open` now checkpoints the WAL before reading the header, so changes made since
  the last flush (held only in the WAL) are recovered instead of being silently orphaned.
- Corruption resistance: a corrupt/hostile `.chiffon` file now surfaces a `StorageCorrupted` error
  instead of panicking or looping — bounds-checked slotted-page/value reads and page-count-bounded
  chain walks throughout storage and the indexes.
- The tier-2 index hash is now a fixed FNV-1a instead of `std`'s `DefaultHasher`, whose output is
  not stable across Rust releases (a change would have silently broken lookups and `@unique`).
- `insert_edge` validates that its endpoints exist before writing, closing a silent-corruption
  path from freed/invalid `NodeRid`s.
- A node's/edge's additional labels are now normalized to a set before persisting: the primary
  type id is excluded and duplicate ids are dropped (first occurrence wins, input order
  preserved). Centralized in the single write-through point so all insert/add/remove paths share
  the invariant.

## [0.1.0] - 2026-06-22

Initial release.
