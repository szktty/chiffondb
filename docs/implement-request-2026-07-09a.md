# Implement request 2026-07-09a — Phase 6: tier-3 unique constraint (`@unique`)

Reviewer→implementer handoff after Phase 5 `close` (tier-2 property index, `4e48650`; review
`docs/review-2026-07-05a.md` = approve).
Authority: `docs/design-variable-topology.md` (§11 tier 3) and `docs/plan-variable-topology.md`.

- Branch: `feature/variable-topology` (continue; forked from `e134249`).

## Goal

Enforce the **tier-3 unique constraint**: a `@unique`-annotated `(type, property)` may hold at
most one node per value. Built as the **unique variant of the tier-2 index** (design §11 tier 3):
reuse the tier-2 machinery; the only new behavior is rejecting a second node with a duplicate
value.

## Already in place (Phase 5)

- **DSL**: `@unique` already parses into `FieldDef.unique` (`schema.pest`/`ast.rs`/`parser.rs`),
  and the validator already rejects `@unique` on non-scalar types (same `is_indexable_scalar`
  check as `@index`). `store.rs`/`unparse.rs` already persist/round-trip `unique`. **No DSL work
  needed** — just wire up enforcement.
- The tier-2 `PropertyIndex` (`storage/property_index.rs`) and the maintenance funnel
  (`db.rs::maintain_property_index`, hooked into insert/update/delete) are the foundation.

## Decided approach — do not re-deliberate

- **Unique = indexed + enforced**: a `@unique` field is also indexed (maintain it in the tier-2
  `PropertyIndex` exactly like `@index`). Treat `unique` as implying `indexed` for maintenance
  and lookup. So a field is index-maintained if `indexed || unique`.
- **Enforcement point = the write hooks**: on `insert_node` / `update_node_properties`, before
  committing the new value for a `@unique (type, path)`, check the index for an existing node
  with that value (confirming candidates against real values, like the planner) — if one exists
  and it is a *different* node, return a new `GraphError` variant (e.g. `UniqueViolation`).
- **Scalar / partial semantics**: same as tier-2. A `None`-resolving or non-scalar value is not
  constrained (nothing to be unique about) — skip enforcement for those, consistent with the
  partial index.
- **Primary-type-only** (same scope decision as tier-2, endorsed in review 2026-07-05a): enforce
  on the node's primary type's `@unique` fields.
- **On-disk / WAL**: enforcement reads the existing index (already on-disk, rolls back with the
  data). A rejected insert/update must leave the DB unchanged (return the error before writing;
  or rely on transaction rollback — state which you did).

## Work

1. **Error variant** (`error.rs`): add `UniqueViolation { type_name_or_id, field, value }` (or a
   concise form) with a `thiserror` message. Propagate with `?`.
2. **Maintain `@unique` fields in the index too** (`db.rs::indexed_paths_for_type`): include
   fields where `indexed || unique` (today it filters `f.indexed` only). Now `@unique` values are
   looked up-able.
3. **Enforce on write** (`db.rs`): a `check_unique` step in `insert_node` and
   `update_node_properties`, run **before** the value is persisted, for each `@unique (type,
   path)` whose resolved value is a scalar: if `find_node(type, path, value)` returns a *different*
   live node, return `UniqueViolation`. (On update, the node updating to a value it already holds
   must be allowed — exclude self.)
4. **`insert_node_with_dynamic_labels` / `_by_name`**: these funnel into `insert_node`, so
   enforcement should be automatic — confirm no bypass.

## Definition of done

- Inserting two nodes of a type with the same `@unique` value: the second fails with
  `UniqueViolation`, and the DB is unchanged (the first node and its index entry intact; no
  partial write from the rejected insert).
- Updating a node's `@unique` field to a value another node already has fails; updating to a fresh
  value succeeds; updating a node to *its own* current value is a no-op success (self-exclusion).
- Deleting a node frees its unique value: a subsequent insert with that value now succeeds.
- A `@unique` field is also queryable via the tier-2 index (find hits the index, not a scan).
- Non-scalar / `None` values are not constrained (partial semantics) — a test that two nodes with
  a `@unique` field left absent both insert fine.
- **Rollback**: a transaction that inserts a node then rolls back releases the unique value (a
  later insert with that value succeeds). Reuse the Phase 5 rollback test style.
- `cargo test --workspace`, `clippy --all-targets -- -D warnings`, `fmt --check` green;
  `PROPTEST_CASES=1000` for any new proptest. No `unwrap()`/`expect()`/`unsafe`; use `RecordId`;
  English WHY-only comments.

## Out of scope

- No new on-disk structure or VERSION bump expected (tier-3 reuses the tier-2 index). **If you
  find you need a VERSION bump or a new header field, stop and `request` — that would mean the
  design assumption "unique = tier-2 + enforcement" broke.**
- Multi-column / composite unique. Tier-4 (design §11). ARCHITECTURE.md / CHANGELOG / squash
  (Phase 7 / §10). Free-list / CoW (§7).
- The parked tier-2 Record-only items (E-2 dead `count`, E-3 schema re-parse) — leave them; do not
  fold cleanup into this phase unless it's directly in your way.

## Stop point

When `@unique` is enforced on insert/update (with self-exclusion), duplicate values are rejected
with `UniqueViolation`, delete/rollback release the value, and the DoD tests are green, stop and
run `request` for the Phase 6 review — state whether a rejected write is prevented pre-write or
via rollback, and confirm no VERSION bump was needed. Do not start Phase 7.
