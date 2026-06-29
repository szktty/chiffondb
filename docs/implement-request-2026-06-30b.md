# Implement request 2026-06-30b — Phase 2b: wire topology through node/edge directories

Reviewer→implementer handoff after Phase 2a `close` (property RID logicalized, `356ad00`).
Authority: `docs/design-variable-topology.md` (§3.2, §4, §9 Phase 2) and
`docs/plan-variable-topology.md`. Supersedes the topology half of
`docs/implement-request-2026-06-30a.md` (which was written pre-2a/2b split).

- Branch: `feature/variable-topology` (continue; forked from `e134249`).

## Goal

Replace the fixed, physically-interleaved topology layout with logical→physical resolution
through `PageDirectory`, removing the ~2000-node cap. Property is already logical (Phase 2a), so
topology can now safely share the file's append tail.

## Work

1. **Two `PageDirectory` instances on `TopologyStore`** (design §3.2: independent node/edge
   directories), mirroring how `DatabaseFile` holds the property directory in the header. Add
   header fields for node/edge directory root/len (the header already has `property_dir_root/len`
   at 48..56 from 2a — add the node/edge pair next, e.g. 56..64 and 64..72; keep the
   single-source-of-truth pattern, persisted through the WAL).

2. **Resolve `node_pid`/`edge_pid` through the directory.** Replace
   `node_pid(i) = topo_start + i*2` / `edge_pid(i) = topo_start + i*2 + 1` with a directory
   lookup. Remove `node_capacity`/`edge_capacity` and the interleaved physical mapping.

3. **`alloc_node_slot`/`alloc_edge_slot`** (`topology.rs:378`,`:410`): when all existing logical
   pages are full, `append_page` a fresh record page and `push` it into the node/edge directory
   (use the returned logical number), instead of checking `*_capacity` and computing a fixed pid.
   Drop the `CapacityExceeded` return from this path (keep the variant; it now only ever means
   u32 logical exhaustion, practically unreachable).

4. **Reconcile `node_page_count`/`edge_page_count` with directory `len`.** They likely become the
   directory `len` — keep one source of truth, don't double-count. Update `with_counts` / open /
   `db.rs` restore accordingly.

5. **Drop the segment pre-allocation in `db.rs`** (`create`/`open_in_memory` zero-filling pages
   `1..prop_start`, around `db.rs:54-83`) and the `topo_start`/`seg_end`/`prop_start` arguments
   to `TopologyStore` that only existed to bound the fixed segment. Topology pages are now mapped
   wherever `append_page` puts them.

## Definition of done

- Insert well beyond ~2000 nodes succeeds — convert the existing
  `insert_node_returns_error_when_node_capacity_exceeded` test (it asserts the old cap) into one
  that now inserts past it. (Search for current `CapacityExceeded` topology tests and update.)
- Reopen restores topology built across multiple node/edge directory pages.
- **Combined-rollback integration test (carried Needs-a-test from review 2026-06-30e):** one
  transaction that grows node + edge + property pages, then rolls back, must unwind all three
  directories (roots/lens) and every appended page together. Assert post-rollback state equals
  pre-transaction state.
- **E-3 mixed-append check (carried Record-only):** a test that grows topology and writes
  properties interleaved, then reads everything back — directory pages, topology pages, and
  property pages share the one `append_page` counter and must not collide.
- `cargo test --workspace`, `clippy --all-targets -- -D warnings`, `fmt --check` green;
  `PROPTEST_CASES=1000` for new proptests. No `unwrap()`/`expect()`/`unsafe`; English WHY-only
  comments.

## Approach is decided — do not re-deliberate

Independent node/edge directories (§3.2); header-persisted root/len mirroring the 2a property
directory; capacity-over-compat (§0). Build it; if the code contradicts the design, send back via
`request` rather than diverging silently.

## Out of scope

- Vector segment boundary (Phase 3 — no vector data written yet).
- Indexes (Phases 4–6), ARCHITECTURE.md (Phase 7), cleanup/squash (§10).
- Free-list / CoW (§7 future).

## Stop point

When alloc/resolve/reopen for node+edge go through their directories, the old cap is gone, and
the DoD tests (incl. combined rollback + mixed append) are green, stop and run `request` for the
Phase 2b code review. Do not start Phase 3.
