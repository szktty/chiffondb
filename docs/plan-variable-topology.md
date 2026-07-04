# Progress: variable-topology (logical page directory)

Progress tracker for the work designed in [design-variable-topology.md](design-variable-topology.md).
Updated by the review session at each `close`.

- Branch: `feature/variable-topology`
- Design: `docs/design-variable-topology.md` (§9 phased plan, §11 index design)

## Phase status

| Phase | Scope | Status |
|-------|-------|--------|
| 1 | `PageDirectory` data structure + read/write (test-first) | ✅ done (`676dfc7`, `a9283d1`) |
| 2a | **Logicalize property RID** (property `PageDirectory`; blob-chain pages pushed into it) — done first so topology growth can't corrupt physically-contiguous properties | ✅ done (`356ad00`) |
| 2b | Wire topology through node/edge directories; drop `*_capacity` | ✅ done (`3cb0aa4`) |
| 3 | Remove vestigial segment boundaries (segment-start fields + `page_directory_root` + dead `CapacityExceeded`); VERSION 4→5; `info` → directory facts | ✅ done (`a74cc96`) |
| 4 | Tier-1 label index (`list`/`count`/type filter → O(matches)); key = all labels; per-type page chain; VERSION 5→6 | ✅ done (`0cc7a96`) |
| 5 | Tier-2 property index (schema DSL `@index` + B+tree) | ⏭️ next |
| 6 | Tier-3 unique constraint (unique variant of tier 2) | todo |
| 7 | ARCHITECTURE.md update (VERSION now 5; segment cleanup already done in Phase 3) + CHANGELOG + squash (§10) | todo |

> **Ordering note:** 2a precedes 2b deliberately. If topology were wired first, appending
> topology pages to the file tail would break the still-physical property contiguity (the very
> failure that forced §3.4 a). Logicalizing property first means topology can then share the
> append tail safely. blob-chain logicalization: each chain page is pushed into the property
> directory and linked by logical page number (decided).

Post-implementation cleanup & squash merge: design §10 (do **not** start until instructed).

## Open / parked items

### Carried from review (Record only — revisit later)

- **E-1 (cycle → silent misread)**: the directory chain walk is a bounded
  `for _ in 0..dir_index` loop, so a cyclic `next` link cannot hang or panic — it only
  mislands on already-corrupt input (the chain is built solely by `ensure_dir_page`; no external
  write path). Future hardening: "steps > expected page count → `StorageCorrupted`". Not needed
  for Phase 1 merge.
- **E-2 / E-3 (mixed append on the single `append_page` counter)**: ✅ **verified in Phase 2b
  review (2026-06-30f)**. node/edge/property record pages and all three directories' pages share
  `append_page`; a probe (200 nodes interleaved across 4 node + 4 edge pages) plus the DoD
  `topology_and_properties_interleave_on_the_append_tail` confirmed each kind resolves through its
  own directory regardless of physical interleaving (distinct dir roots; no confusion). Resolved.
- **`StorageCorrupted` payload reuse**: `push`'s `UNMAPPED` guard returns
  `StorageCorrupted(physical)`, repurposing a variant meant for a "corrupt page id" to carry the
  rejected value. Acceptable (unreachable in practice; useful diagnostic). Consider a dedicated
  variant if the error enum is ever tidied up.
- **(Phase 4, review 2026-07-04a E-2) `rids_of_type` unused `_topo` param**: routing
  `rids_of_type` through the label index made the `TopologyStore` argument unnecessary; it was kept
  as `_topo` to avoid churning callers (`db.rs:350`,`:1300`). Drop it when the index/search
  signatures are reworked in Phase 5 (tier-2 adds the `PropertyPath`-based `find`/`find_all` — a
  natural point to tidy the whole `index.rs` signature surface).

### Phase 7 cleanup — status

- **Dead `CapacityExceeded` variant**: ✅ **removed in Phase 3 (`a74cc96`)**. Grep confirmed zero
  constructors/matches across core/chiffondb/chiffondb-ffi; FFI does not map the enum by
  discriminant, so removal is safe (review 2026-06-30g).
- **Vestigial `*_segment_start` header fields**: ✅ **removed in Phase 3 (`a74cc96`)**. Together
  with the unused `page_directory_root` (MVCC single root, superseded by the per-kind dir roots),
  offsets 16..32 are now reserved (zeroed). VERSION 4→5; old v4 files rejected (review 2026-06-30g).
- **(new, Record only — review 2026-06-30g E-3)** Header offsets 16..32 are left *reserved* rather
  than compacted, to keep surviving offsets stable and the diff a pure deletion. A future header
  tidy-up could compact them (and reuse the 16 bytes for new fields). Not needed for merge.

> **Note (Phase 3 scope):** the design's "remove the vector segment boundary" turned out to be a
> no-op for data — there is no vector write path (`VectorStore`/`write_vector`/`alloc_vector` do
> not exist) and `vector_segment_start` was display-only. So Phase 3 became the vestigial-field
> cleanup above (which also absorbed the Phase 7 items the 2b review had parked). Vector/full-text
> indexing is tier 4, out of scope for this branch (design §11).

### Resolved Needs-a-test

- **Combined-rollback integration test** (from review 2026-06-30e): ✅ **done as a Phase 2b DoD
  test** `rollback_unwinds_node_edge_and_property_growth_together` — one transaction grows node +
  edge + property pages, rolls back, all three directory roots/lens and appended pages unwind
  together; reopen reflects post-rollback state. Plus reviewer-added
  `topology_and_properties_survive_reopen_after_multipage_growth` covers reopen persistence.

### Deferred design decisions (from design §8)

- (5) directory cache strategy — page-cache only vs. pinning a hot root tier.
- (6) WAL/rollback consistency for directory growth and index updates (§11.3 premise: indexes
  must live fully on-disk).
- (7) B+tree node layout / split thresholds (only the approach is fixed in §11).
- (8) ~~property RID: logicalize (a) vs. keep physical (b)~~ → **decided (a) logicalize**, folded
  into Phase 2 (the (b) plan breaks once topology shares the append tail — design §3.4).
- (9) ~~tier-1 label index key: primary type only (a) vs. all labels (b)~~ → **decided (b) all
  labels** (Phase 4, `implement-request-2026-07-04a.md`). A node is indexed under its primary
  `node_type_id` plus every additional/dynamic label; `MATCH (n:Label)` hits under any label. This
  changes `list_nodes`/`count_nodes` semantics (today primary-only via `rids_of_type`) — that
  change is part of Phase 4.
