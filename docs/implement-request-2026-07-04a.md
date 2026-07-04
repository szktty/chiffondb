# Implement request 2026-07-04a — Phase 4: tier-1 label (type) index

Reviewer→implementer handoff after Phase 3 `close` (vestigial segment boundaries removed,
`a74cc96`; review `docs/review-2026-06-30g.md` = approve).
Authority: `docs/design-variable-topology.md` (§11 tier 1, §11.3) and `docs/plan-variable-topology.md`.

- Branch: `feature/variable-topology` (continue; forked from `e134249`).

## Goal

Add the **tier-1 label index**: an always-on, persisted `type_id → {node RecordId}` map so that
`list_nodes(type)` / `count_nodes(type)` / `index::rids_of_type` become **O(matches)** instead of
today's O(all-nodes) topology scan. This is the first index tier and the foundation the later
property index (Phase 5) scans from.

Capacity is now in the hundreds-of-millions range (Phases 1–3), so the O(nodes) full scan behind
`rids_of_type` is the next real bottleneck — this phase removes it.

## Decided approach — do not re-deliberate

- **Key = ALL labels (design §8 item 9 → decided (b)).** A node is registered under **every**
  `type_id` it carries: its primary `node_type_id` **plus** every additional/dynamic label
  (`get_node_type_ids`, `db.rs:707`, already returns the full set). `MATCH (n:Label)` must hit a
  node under any of its labels, matching standard property-graph semantics. This is a **semantics
  change** for `list_nodes`/`count_nodes` (today they filter by primary type only via
  `rids_of_type`) — make that change here and cover it with tests (see DoD).
- **Always-on, no declaration** (design §11 tier 1): every type is indexed; no schema opt-in.
- **On-disk, page-directory backed** (design §11.3): the index lives in pages resolved through a
  `PageDirectory` whose root/len persist in the header — same pattern as the property/topology
  directories (Phase 2a/2b). **No in-memory index state** that could desync on rollback: index
  writes ride `read_page`/`write_page` → WAL, so they roll back with the data (this is the §11.3
  premise the prior reviews established; keep it).
- **Structure**: `type_id → RecordId set`. A per-type page chain or an on-disk B-tree are both
  acceptable; pick the simpler one that keeps insert/delete near O(log n)/O(1) and resident memory
  bounded by the page cache. **State the chosen structure in the review-request.** (Design leaves
  the concrete layout open; §8 item 7 only fixes "approach".)

## Work

1. **Index structure** (`chiffondb-core/src/storage/`, new module, e.g. `label_index.rs`):
   persisted `type_id → {RecordId}`. Header gets a root/len (mirror the existing dir fields at
   56..64; add the label-index root in the reserved 16..32 range or a fresh offset — **state the
   offset choice and update VERSION 5→6** since it's a layout change, no backward compat §0).
2. **Maintain the index on every label mutation.** Hook all entry points so the index stays
   consistent with `get_node_type_ids`:
   - `insert_node` (`db.rs:134`) / `insert_node_with_dynamic_labels` (`:810`): add the node under
     each of its type_ids.
   - `delete_node` (`db.rs:217`): remove the node from every type_id set it was in.
   - `add_node_label`/`remove_node_label` (+ `_by_name`/`_dynamic` variants) and the shared
     `set_additional_labels` (`db.rs:850`): add/remove the node from the affected type_id set.
     `set_additional_labels` is the common lower write point — centralize the delta there where
     possible, but make sure primary-type registration (from `insert_node`) is covered too.
3. **Route `rids_of_type` through the index** (`storage/index.rs:28`): return the indexed set
   instead of scanning `live_node_rids`. `list_nodes`/`count_nodes` (`db.rs:350`,`:1300`) then
   become O(matches). With key=(b), a node with extra labels now appears under each label's query.
4. **Rebuild/consistency**: a freshly opened DB reconstructs the index purely from the header
   root/len (stateless, like the topology store). Decide whether a missing/legacy index triggers a
   one-time rebuild scan or is simply always-present-from-creation (given no backward compat, the
   latter is fine — **state which**).

## Definition of done

- `list_nodes(t)` / `count_nodes(t)` / `rids_of_type(t)` return results via the index, not a full
  scan. Add a test asserting a node with an **additional label** L is returned by `list_nodes(L)`
  and counted by `count_nodes(L)` (the (b) semantics change), and that primary-type queries still
  work.
- **Label mutation consistency**: tests that (a) adding a label makes the node appear under it,
  (b) removing a label makes it disappear, (c) deleting a node removes it from all its label sets,
  (d) dynamic-label insert registers all labels. Include an invariant/property test: for a random
  sequence of insert/add-label/remove-label/delete ops, the index for every type equals a
  brute-force `live_node_rids` filter by `get_node_type_ids`.
- **Rollback**: a transaction that inserts nodes + mutates labels, then rolls back, leaves the
  index exactly as before (rides the WAL; header root/len revert with the snapshot). Extend the
  combined-rollback style test from Phase 2b.
- **Persistence**: reopen restores the index from the header; queries return the same sets.
- `VERSION` bumped (5→6); old v5 files rejected (extend the version-guard test).
- `cargo test --workspace`, `clippy --all-targets -- -D warnings`, `fmt --check` green;
  `PROPTEST_CASES=1000` for new proptests. No `unwrap()`/`expect()`/`unsafe`; use `RecordId`;
  English WHY-only comments.

## Cost check (design §9 "コスト見直しの方針")

Tier 1 is the cheap tier (a set index, no B+tree). If the on-disk set structure balloons in
complexity, fall back to the simplest correct thing (per-type page chain) rather than a full
B-tree — the B-tree work belongs to Phase 5. If tier 1 itself turns out large, stop and send back
via `request` before over-building.

## Out of scope

- Tier-2 property index / schema `@index` DSL (Phase 5) and tier-3 unique (Phase 6).
- Tier-4 full-text/vector (design §11 tier 4 — not this branch).
- `PropertyPath`-based `find`/`find_all` extension (that's the Phase 5 C-2 work, driven by the
  property index — tier 1 is label-only).
- ARCHITECTURE.md / CHANGELOG / squash (Phase 7 / §10 — do not start until instructed).
- Free-list / CoW (§7 future).

## Stop point

When label mutations keep the index consistent, `list_nodes`/`count_nodes`/`rids_of_type` go
through it (with (b) all-label semantics), rollback/persistence hold, and the DoD tests are green,
stop and run `request` for the Phase 4 code review (state the index structure, the header offset,
and the rebuild decision). Do not start Phase 5.
