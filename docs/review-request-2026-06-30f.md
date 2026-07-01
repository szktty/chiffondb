# Review request 2026-06-30f — Phase 2b: wire topology through node/edge directories (code)

Code-diff review per `docs/review-process.md`. **Snapshot the uncommitted working tree first**
(`git add -A && git commit -m "wip: review snapshot 2026-06-30f"`) and review against that SHA —
see "Protecting the review target". Do not run destructive git against the implementer's tree.

## Header

- Branch: `feature/variable-topology` (forked from `e134249`)
- Source implement-request: `docs/implement-request-2026-06-30b.md`
- State: **uncommitted working tree** (impl session does not commit; `close` will).
- Files changed (all `chiffondb-core/src`):
  - `storage/file.rs` (+125): header node/edge dir roots, `TopologyKind`, topology page helpers.
  - `storage/topology.rs` (−183 net): `TopologyStore` becomes a stateless façade; `node_pid`/
    `edge_pid`/`*_capacity`/`ensure_page`/`topo_start`/`seg_end` removed.
  - `db.rs` (+151): drop segment pre-allocation; `TopologyStore::new()`; the old-cap test becomes
    a grows-past-cap test; + combined-rollback and interleave DoD tests.
  - `pathfinding/mod.rs`, `storage/index.rs`: test helpers no longer pre-allocate a segment.

## The change (and why)

Phase 2a made property RIDs logical; 2b does the same for topology so the ~2000-node cap is gone.

- **Header**: `node_dir_root`/`edge_dir_root` at offsets 56..64. The existing `node_page_count`/
  `edge_page_count` (40..48) are **reused** as the node/edge directory mapped lengths (single
  source of truth — no separate len fields). `FileHeader::new()` now starts counts at **0**
  (empty directories; the `.max(1)` in `deserialize` was removed since 0 is valid).
- **`DatabaseFile`**: `TopologyKind` enum; `alloc/resolve/read/write_topology_page` helpers
  mirroring the 2a property helpers (header is the single source of truth, persisted via WAL).
- **`TopologyStore`**: now a unit struct (`pub struct TopologyStore;`). `node_pid`/`edge_pid`
  (the interleaved `topo_start + i*2`) and `node_capacity`/`edge_capacity`/`ensure_page` are
  gone. `node_page_count`/`edge_page_count` now take `&DatabaseFile` and read the header.
  `alloc_node_slot`/`alloc_edge_slot` scan logical pages, else `alloc_topology_page` (no
  `CapacityExceeded`).
- **`db.rs`**: `create`/`open` drop the pages-1..prop_start pre-allocation; `open` no longer
  passes segment bounds.

## Tests (DoD)

- `insert_grows_past_the_old_fixed_node_cap` — inserts 2500 nodes (old cap ~2016), asserts
  count and that records across many logical node pages read back. (Replaces the old
  `insert_node_returns_error_when_node_capacity_exceeded`.)
- `rollback_unwinds_node_edge_and_property_growth_together` — one transaction grows node + edge +
  property pages, rolls back, all three revert; reopen reflects post-rollback state.
- `topology_and_properties_interleave_on_the_append_tail` — 200 nodes + edges + per-node
  properties interleaved; every property reads back (E-3 mixed-append check).
- Existing rollback tests (`rollback_across_new_topology_page`/`_edge_page`) still pass with the
  directory-backed counts.

## Points to look at in review

1. **Count/len reconciliation.** `node_page_count`/`edge_page_count` are now both the header
   field *and* the directory `len`. Confirm `store_topology_dir` keeps them in lockstep and no
   path updates one without the other. Starting at 0 — verify the first node/edge alloc maps
   logical page 0 correctly and nothing assumes a pre-existing page 0.
2. **Mixed append on one counter (E-3 / E-2 carried).** node, edge, property record pages **and**
   all three directories' pages share `append_page`. Probe that a directory page and the record
   page it maps don't get confused, and that resolution returns the record page. The interleave
   test covers the happy path — look for ordering edge cases.
3. **Rollback completeness.** Three directories' roots/lens all live in the header, reverted by
   the snapshot. Confirm `take_snapshot`/`restore_snapshot` capture the whole header (incl. the
   new 56..64 fields) and that appended directory pages are discarded by the WAL truncate.
4. **`TopologyStore` statelessness.** It's now `pub struct TopologyStore;`. Confirm no caller
   relied on its old per-instance counts (e.g. cloning a store and expecting independent state).
5. **Header layout / VERSION.** VERSION is already 4 (bumped in 2a); 2b only adds fields at
   56..64 within the same v4. Confirm serialize/deserialize round-trip and that no offset
   overlaps (40..48 counts, 48..56 property dir, 56..64 node/edge dir roots).
6. **`unwrap()`/`expect()`.** `db.rs` test uses `.expect(...)` (tests are exempt, but confirm no
   `expect`/`unwrap` crept into non-test code).

## Out of scope

- Vector segment boundary (Phase 3), indexes (4–6), ARCHITECTURE.md (Phase 7), cleanup/squash (§10).
- Free-list / CoW (§7 future). The `property_segment_start`/`topology_segment_start` header fields
  are now vestigial (no longer bound anything) — leaving them is fine for 2b; note for Phase 7.

## Verification (commands run, results)

```
cargo test -p chiffondb-core                 → 315 passed (was 313; +2 DoD tests, -1 old cap test, +1 grow test)
cargo test --workspace                       → core 315 / 18 / 6, all 0 failed
cargo clippy --all-targets -- -D warnings    → clean
cargo fmt --check                            → exit 0
PROPTEST_CASES=1000 cargo test -p chiffondb-core → 315 passed (19.8s)
```

## Deliverable

`docs/review-2026-06-30f.md` (untracked). Classify findings; bugs go back via `fix`. Do not edit
core logic in review.
