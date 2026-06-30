# Implement request 2026-06-30c — Phase 3: remove vestigial segment boundaries (cleanup)

Reviewer→implementer handoff after Phase 2b `close` (topology wired through node/edge
directories, `3cb0aa4`; review `docs/review-2026-06-30f.md` = approve).
Authority: `docs/design-variable-topology.md` (§9 Phase 3, §4) and `docs/plan-variable-topology.md`.

- Branch: `feature/variable-topology` (continue; forked from `e134249`).

## Context (read first — scope is smaller than the design's original Phase 3)

Design §9 framed Phase 3 as "remove the fixed boundary of the property / vector segments". But:

- **Property** boundaries were already removed in Phase 2a (property RID logicalized).
- **Vector** has **no data write path at all** — `grep` for `VectorStore`/`write_vector`/
  `alloc_vector` returns nothing; `vector_segment_start` is only *read* by
  `chiffondb/src/commands/info.rs:16` for display. Full-text/vector indexing is tier-4, explicitly
  out of scope for this branch (design §11 tier 4).
- After Phase 2b, **all** record/property/directory pages resolve through page directories on the
  append tail, so `topology_segment_start` / `property_segment_start` / `vector_segment_start`
  **no longer bound anything**.

So Phase 3's real content is **deleting vestigial fields and the dead capacity path**, not
logicalizing a live segment. This is the cleanup the Phase 2b review parked.

## Goal

Remove the now-meaningless fixed-segment machinery from the header and code, leaving the
directory-based layout as the single model. No backward compatibility (design §0): bump `VERSION`
and reject old files.

## Work

1. **Drop the three segment-start header fields** (`file.rs`): `topology_segment_start`,
   `property_segment_start`, `vector_segment_start` (struct fields, `serialize`/`deserialize` at
   offsets 16..28, `FileHeader::new` defaults, and the `*_SEGMENT_DEFAULT_START` consts). Decide
   the freed offsets: either leave them zeroed/reserved or compact the layout — **bump `VERSION`
   4 → 5** either way (layout change; old v4 files rejected by the existing version guard).
2. **`page_directory_root` (offset 28..32)**: this was the MVCC-reserved field (design §6, now
   superseded by the per-kind `*_dir_root` fields). Either remove it too or keep it explicitly
   reserved for future CoW with a comment — **state which in the review-request**. Don't leave it
   ambiguous.
3. **Remove the dead `CapacityExceeded` variant** (`error.rs`) — Phase 2b removed its only
   construction sites (the ceiling is now u32 logical page space). If anything still matches on it,
   that's a sign it wasn't fully dead — surface it.
4. **Update `commands/info.rs`**: it currently prints `topology/property/vector_segment_start`.
   Replace with the directory-based facts that actually exist now (e.g. node/edge/property
   directory roots + logical page counts), or drop the segment lines. Keep `info` meaningful.
5. **Sweep for other readers** of the removed fields (`grep` the three field names + the consts +
   `CapacityExceeded` across `chiffondb-core` and `chiffondb`) and update/remove each.

## Definition of done

- The three segment-start fields and `*_SEGMENT_DEFAULT_START` consts are gone; `VERSION` bumped
  (4 → 5) and an old-version file is rejected (extend/confirm the existing
  `open_unsupported_version_returns_error` style test).
- `serialize`/`deserialize` round-trip for the new header layout (no offset overlap with the
  surviving 40..64 fields: node/edge counts 40..48, property dir 48..56, node/edge dir 56..64).
- `CapacityExceeded` removed (or, if a real use remains, documented why it stays).
- `info` command compiles and prints directory-based layout facts; its output is sensible on a
  fresh DB and after inserting nodes.
- `cargo test --workspace`, `clippy --all-targets -- -D warnings`, `fmt --check` green;
  `PROPTEST_CASES=1000` for any touched proptests. No `unwrap()`/`expect()`/`unsafe`; English
  WHY-only comments.

## Approach is decided — do not re-deliberate

Capacity-over-compat (§0): no migration, bump VERSION, reject old files. Remove the vestigial
fields rather than keeping them "just in case". The one open call is item 2 (`page_directory_root`
remove vs. keep-reserved) — pick one, implement it, and **state the choice in the review-request**.
If the code reveals a field is not actually vestigial (a live reader you didn't expect), stop and
send it back via `request` rather than deleting blindly.

## Out of scope

- Vector data storage / vector index (tier 4 — no vector write path exists; not this branch).
- Indexes (Phases 4–6): tier-1 label index is the *next* phase after this.
- ARCHITECTURE.md rewrite + CHANGELOG + squash merge (Phase 7 / §10 — do not start until
  instructed). Note: this Phase 3 absorbs the "vestigial `*_segment_start`" and "dead
  `CapacityExceeded`" items the 2026-06-30f review had parked for Phase 7, so update
  `plan-variable-topology.md`'s Phase 7 section at `close` to reflect they're handled here.
- Free-list / CoW (§7 future).

## Stop point

When the segment-start fields + dead variant are gone, `VERSION` is bumped, `info` is updated, and
the DoD checks are green, stop and run `request` for the Phase 3 code review (state the
`page_directory_root` decision). Do not start Phase 4.
