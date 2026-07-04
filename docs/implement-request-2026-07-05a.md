# Implement request 2026-07-05a — Phase 5: tier-2 property index (declared, B+tree)

Reviewer→implementer handoff after Phase 4 `close` (tier-1 label index, `0cc7a96`; review
`docs/review-2026-07-04a.md` = approve).
Authority: `docs/design-variable-topology.md` (§11 tier 2, §11.1 DSL, §11.2 PropertyPath, §11.3
consistency) and `docs/plan-variable-topology.md`.

- Branch: `feature/variable-topology` (continue; forked from `e134249`).

## Goal

Add the **tier-2 property index**: a *declared* `(type, property) → node` secondary index that
accelerates `find` / `find_all` from O(all-nodes-of-type) to O(matches). Unlike tier 1 (always-on,
label-only), tier 2 is opt-in via a schema annotation and keyed on a **property value**.

## Scope discipline — this is the expensive phase

Design §9「コスト見直しの方針」applies most here. A full on-disk B+tree is a lot of code. **If the
B+tree balloons, stop and send back via `request` before over-building** — the agreed fallback is
an **equality-only on-disk hash/bucket index** (per `(type, key)` a chain of `value → {RecordId}`
buckets, reusing the tier-1 page-chain machinery), which still gives O(matches) equality lookups.
Range/`OrderBy` acceleration is a *nice-to-have* that can slip to a follow-up. **Deliver equality
lookup first**; add range only if the B+tree lands cleanly.

## Decided approach — do not re-deliberate

- **Declared, not always-on** (design §11 tier 2): only `(type, property)` pairs annotated in the
  schema get an index. Everything else keeps the tier-1 / full-scan path.
- **Key = `PropertyPath`** (design §11.2, resolves the C-2 gap): the index key is a `PropertyPath`
  (`traversal/command.rs`), so both flat keys (`email`) and nested JSON scalar keys
  (`profile.city`) are indexable. Reuse `PropertyPath::resolve`'s semantics verbatim:
  scalar-terminal only, no arrays, nodes resolving to `None` are **excluded** (partial index).
- **Search API takes `PropertyPath`** (C-2): change `index::find` / `find_all` (and the `db.rs` /
  `GraphAccess` callers) so the key is `PropertyPath`. `From<&str>` already exists, so flat-key
  callers keep compiling. **Planner (minimal):** an indexed `(type, PropertyPath)` goes to the
  index; an unindexed one falls back to the existing full scan. Keep `GraphAccess::find_node`'s
  `&str` signature by converting internally if changing the trait is disruptive — reviewer's call,
  but state it.
- **On-disk, page-directory backed, rolls back via WAL** (design §11.3, same invariant tiers 1–4
  share): no in-memory index state; root in the header; mutations ride `read_page`/`write_page`.
- **Maintained on write**: insert / update-property / delete must keep the index consistent with
  the node's current resolved value for each indexed `(type, PropertyPath)`. Mirror the tier-1
  hook discipline (find the single lower write points for property changes and funnel there).

## Work

1. **Schema DSL** (§11.1): extend `field_def` in `schema.pest` from `ident ~ ":" ~ type_expr` to
   allow a trailing annotation (e.g. `name: String @index`). Add an `indexed: bool` (and leave
   room for `@unique` in Phase 6) to `FieldDef` (`ast.rs`), parse it (`parser.rs`), validate it
   (`validator.rs` — e.g. reject `@index` on a non-scalar type / on `List`/`Map`/`Vector`), and
   persist it (`schema/store.rs`) so a reopened DB knows which fields are indexed. **State the
   final annotation syntax in the review-request.**
2. **Index structure** (`storage/`, new module e.g. `property_index.rs`): per `(type_id,
   PropertyPath)` an on-disk map `value → {RecordId}`. Prefer the B+tree; fall back to the
   equality hash/bucket structure per the scope discipline above. Header gets a root (reserved
   20..32 range has space; **state the offset and bump VERSION 6→7**, no backward compat §0).
   Encode index values from the resolved `serde_json::Value` scalar (reuse `value.rs`'s scalar
   encoding so ordering/equality match query semantics).
3. **Maintenance hooks** (`db.rs`): on node insert, property update, and delete, for each indexed
   `(type, PropertyPath)` of the node's type(s): resolve the path against the node's properties and
   add/remove/update the `value → rid` entry. `None`-resolving nodes are simply absent (partial
   index). Find the shared property-write point (analogous to tier-1's `set_additional_labels`) and
   funnel there so no write path skips the index.
4. **Search routing** (`storage/index.rs`, `db.rs`): `find`/`find_all` take `PropertyPath`; when the
   `(type, path)` is indexed, look up via the index; else fall back to the current scan. Drop the
   now-vestigial `_topo` param from `rids_of_type` while here if it's a clean removal (plan Record-
   only E-2, review 2026-07-04a).

## Definition of done

- A `@index`-annotated field builds an index; `find`/`find_all` on it return correct results
  without a full scan (assert via a large dataset that scan and index agree, and — if practical —
  that the index path doesn't call `live_node_rids`).
- Nested-key index works: `profile.city` (a `PropertyPath::Path`) indexes nested scalars; array /
  object-terminal / `None` values are excluded (partial-index semantics matching `resolve`).
- **Consistency**: insert / update-property / delete keep the index correct — invariant/property
  test comparing the index against a brute-force `resolve`-filter over `live_node_rids`, for a
  random op sequence (PROPTEST_CASES=1000).
- **Rollback**: a tx that inserts/updates indexed properties then rolls back leaves the index and
  its header root exactly as before (same style as the Phase 4 rollback test).
- **Persistence**: reopen restores the index and the schema's `indexed` flags; queries still hit
  the index.
- Unindexed `find` still works (fallback path) and existing `find`/`find_node_by` callers compile
  (via `From<&str>`).
- `VERSION` bumped (6→7); old v6 files rejected (extend the version-guard test).
- `cargo test --workspace`, `clippy --all-targets -- -D warnings`, `fmt --check` green;
  `PROPTEST_CASES=1000` for new proptests. No `unwrap()`/`expect()`/`unsafe`; use `RecordId`;
  English WHY-only comments.

## Out of scope

- Tier-3 unique constraint (`@unique`) — Phase 6 (built as the *unique variant* of this index).
  Leave room in the DSL/`FieldDef` for it, but do not implement enforcement here.
- Tier-4 full-text/vector (design §11 tier 4).
- ARCHITECTURE.md / CHANGELOG / squash (Phase 7 / §10 — do not start until instructed).
- Free-list / CoW (§7 future).

## Stop point

When an `@index` field is parsed/persisted, its index is maintained on insert/update/delete,
`find`/`find_all` route through it (with `PropertyPath` keys and scan fallback), and
rollback/persistence/consistency DoD tests are green, stop and run `request` for the Phase 5
review — state the annotation syntax, the index structure actually shipped (B+tree vs equality
fallback), the header offset, and the `GraphAccess`/`_topo` signature decisions. Do not start
Phase 6.
