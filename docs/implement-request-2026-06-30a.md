# Implement request 2026-06-30a — Phase 2: wire topology through the page directory

Reviewer→implementer handoff after Phase 1 `close` (PageDirectory approved,
`676dfc7` + `a9283d1`). Authority: `docs/design-variable-topology.md` (§3, §4, §9 Phase 2)
and `docs/plan-variable-topology.md`.

- Branch: `feature/variable-topology` (continue on it; forked from `e134249`).
- Source design: design §9 Phase 2.

## Goal

Replace the fixed, physically-interleaved topology layout with logical→physical resolution
through `PageDirectory`, removing the `~2000`-node cap. After this phase a database can grow its
topology beyond the old `[topology_segment_start, property_segment_start)` range.

## Work

1. **Give `TopologyStore` two `PageDirectory` instances** (decision §3.2: independent node/edge
   directories). Replace the interleaved physical mapping `node_pid(i) = topo_start + i*2` /
   `edge_pid(i) = topo_start + i*2 + 1` with a directory lookup: logical page `i` →
   `node_dir.resolve(file, i)` (resp. edge).

2. **`alloc_node_slot` / `alloc_edge_slot`** (`topology.rs:378`, `:410`): when all existing
   logical pages are full, instead of checking `node_capacity()` and computing a fixed physical
   pid, **append a fresh physical page (`append_page`) and `push` it into the directory**, using
   the returned logical number. Drop the `CapacityExceeded` check here (see §4: the capacity
   error narrows to u32 logical exhaustion only — practically unreachable; keep the variant but
   stop returning it from the fixed-segment path).

3. **`node_pid` / `edge_pid` / `node_capacity` / `edge_capacity`**: remove or replace. Physical
   resolution now goes through the directory; the `*_capacity` notion disappears.

4. **Header persistence.** Persist each directory's `{root, len}` so reopen restores via
   `PageDirectory::with_root`. Follow the exact discipline the current `alloc_*_slot` already
   uses for `node_page_count` (`topology.rs:404`): write the grown metadata through the WAL
   (`file.write_header()`) so it survives a crash without flush. Decide how `node_page_count` /
   `edge_page_count` relate to the new directory `len` (likely `len` supersedes them — keep one
   source of truth, don't double-count).

5. **Drop the segment pre-allocation** in `db.rs` (`create`/open paths around `db.rs:54-103`
   that zero-fill pages `1..prop_start`), since the topology segment is no longer a fixed range.

6. **Tests (test-first where the API is obvious):** node/edge alloc past the old ~2000 cap now
   succeeds; reopen-after-grow restores all records; node/edge symmetry. Keep `cargo test`
   green throughout.

## Definition of done

- A database can insert well beyond ~2000 nodes (add a test that previously hit
  `CapacityExceeded` and now passes).
- Reopen restores topology built across multiple directory pages.
- `cargo test --workspace`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check`
  all green; `PROPTEST_CASES=1000` for any new proptest.
- No `unwrap()`/`expect()`/`unsafe`; comments English, WHY-only.

## Must address (carried from Phase 1 review — plan E-2)

- **Mixed append ordering.** Directory pages and topology record pages now share the single
  `append_page` counter. Verify the interleaving is correct: a directory page and the record
  page it maps are both appended, and resolution returns the record page, not the directory
  page. Add a test that exercises growth where both kinds of pages are appended in sequence.

## Out of scope (do not do here)

- Property / vector segment boundary removal (Phase 3) — topology only.
- Any index work (Phases 4–6).
- Property RID logicalization (design §3.4 (a)) — property RID stays physical; do not touch
  `value.rs` blob-chain logic.
- `VERSION` bump / ARCHITECTURE.md (Phase 7) — though if the on-disk header layout changes here,
  note it for Phase 7.

## Approach is decided — do not re-deliberate

Independent node/edge directories (§3.2), capacity-over-compat (§0), header-persisted
`{root, len}` mirroring the existing `node_page_count` WAL discipline. Build it; raise questions
only if the code contradicts the design (then send back, don't silently diverge).

## Stop point

When alloc/resolve/reopen go through the directory and the DoD tests are green, stop and run
`request` to hand back for review (Phase 2 code review). Do not start Phase 3.
