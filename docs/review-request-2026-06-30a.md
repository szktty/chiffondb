# Review request 2026-06-30a — variable-topology design document

> NOTE: This is a **design-document review**, not a code-diff review. The verification steps in
> `docs/review-process.md` that assume a code diff (live test runs, throwaway probes, green
> checks) do not apply. What is asked here is a critical read of the design for soundness,
> internal consistency, and missing risks — backed by the **real current code** where the design
> makes claims about it.

## Target

- Branch: `feature/variable-topology`
- Document under review: `docs/design-variable-topology.md`
- Commits: `f5ca6ec` (initial design), `8d300a7` (decisions + index chapter)
- Base: `e134249` on `main`/`develop`

## Context / goal

Today a database holds at most ~2000 nodes because the topology segment is a fixed range
`[topology_segment_start, property_segment_start)` (pages 1–63). The goal is to lift that cap by
introducing a **logical→physical page directory**, and — because raising capacity makes the
existing O(nodes) full-scan search unusable — to **co-design indexes** in the same effort.

Working constraints already decided by the user (do not re-litigate):

- Dedicated branch `feature/variable-topology`.
- **No backward compatibility.** Capacity over compatibility. Old v3 files are rejected, no
  upgrade path.
- Sonnet implements where quality allows; Opus handles storage-invariant-sensitive parts.
- Merge will be a squash merge, and only when explicitly instructed.

## Decisions baked into the design (please pressure-test these)

1. **Merge with `page_directory_root`** (the field ARCHITECTURE.md reserved for MVCC). The
   permanent logical→physical directory reuses that field and is meant to grow into the MVCC
   CoW root later (§6).
2. **Independent node/edge directories** rather than a bit-prefixed shared logical space (§3.2).
3. **Standard graph-DB index tiers** (§11): tier 1 implicit label index (always on), tier 2
   declared property B+tree index, tier 3 unique constraint as the unique variant of tier 2,
   tier 4 full-text/vector deferred.
4. **Index keys reuse `PropertyPath`** so flat and JSON-nested *scalar* keys are both indexable;
   arrays are out of scope; nodes that resolve to `None` are excluded (partial-index semantics)
   (§11.2).

## What to verify

Back each judgment against the actual code, not just the document's prose.

1. **Capacity claim.** §1/§2 derive "~2000 nodes" and "u32 logical space ≈ hundreds of millions
   to billions". Confirm the arithmetic against `storage/topology.rs`
   (`node_capacity`/`node_pid`), `storage/page.rs` (`compute_slot_count`,
   `NODE_RECORD_SIZE = 64`), and the segment bounds in `storage/file.rs`.

2. **`RecordId.page_id` is already logical.** The design leans heavily on the claim that the
   externally-exposed `RecordId.page_id` already holds a *logical* page number (not physical),
   so the directory indirection does not change record identity. Verify in
   `storage/topology.rs` (`alloc_node`/`read_node`/`node_pid`) and `storage/record.rs`.

3. **Impact list (§4) completeness.** Walk the codebase for every site that assumes the fixed
   segment boundaries — `property_segment_start` / `PROPERTY_SEGMENT_DEFAULT_START` (e.g.
   `storage/value.rs` blob page allocation, `db.rs` segment pre-allocation, `commands/info.rs`)
   — and flag any boundary-assuming code path the impact table misses.

4. **Index design soundness (§11).** In particular:
   - Does reusing `PropertyPath::resolve` (`traversal/command.rs`) as the index-key source hold
     up? Confirm the scalar-terminal / no-array / `None`-excluded semantics match what
     `resolve` actually does.
   - Are tier 1 (label index) and tier 3 (unique constraint) consistent with how the current
     `index.rs`, `db.rs` (`list_nodes`/`count_nodes`/`find`) and the schema layer
     (`schema/ast.rs`, `schema/schema.pest`, `apply_schema`) work? Flag any place the design
     would conflict with existing semantics (e.g. dynamic labels, multi-labels).

5. **MVCC merge risk (§6).** Reusing `page_directory_root` for a *permanent* mapping while it
   was reserved for a *per-transaction* MVCC root: does this foreclose any MVCC design option, or
   is the "permanent directory grows into the CoW root" story coherent? This is the highest-risk
   decision; scrutinize it.

6. **Consistency & WAL (§11.3, §8 open item 6).** Is "index updates ride the same WAL
   write-through and roll back together" actually guaranteed by the current write/rollback paths
   (`storage/file.rs`, `storage/buffer.rs`)? Note any gap.

## Out of scope

- Free-list / page reuse (§7), true CoW/MVCC, u64 page counts — all explicitly future work.
- Implementation details of the B+tree node layout (§8 open item 7) — only the *approach* is
  fixed here.

## Deliverable

Write the review result to `docs/review-2026-06-30a.md` (untracked, do not commit), following
`docs/review-process.md`'s deliverable format adapted for a design review: target, summary,
per-point findings, extra findings, conclusion (approve / approve-with-changes / needs-rework).
Anything that should change the design goes back to this (implementation) session — the review
session does not edit `docs/design-variable-topology.md` directly.
