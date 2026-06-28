# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

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

### Fixed

- A node's/edge's additional labels are now normalized to a set before persisting: the primary
  type id is excluded and duplicate ids are dropped (first occurrence wins, input order
  preserved). Centralized in the single write-through point so all insert/add/remove paths share
  the invariant.

## [0.1.0] - 2026-06-22

Initial release.
