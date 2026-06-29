# Progress: variable-topology (logical page directory)

Progress tracker for the work designed in [design-variable-topology.md](design-variable-topology.md).
Updated by the review session at each `close`.

- Branch: `feature/variable-topology`
- Design: `docs/design-variable-topology.md` (§9 phased plan, §11 index design)

## Phase status

| Phase | Scope | Status |
|-------|-------|--------|
| 1 | `PageDirectory` data structure + read/write (test-first) | ✅ done (`676dfc7`, `a9283d1`) |
| 2 | Wire topology through the directory; drop `*_capacity`; **+ logicalize property RID (folded in, §3.4 a)** | 🚧 in progress |
| 3 | Remove the fixed vector segment boundary (property folded into Phase 2) | todo |
| 4 | Tier-1 label index (`list`/`count`/type filter → O(matches)) | todo |
| 5 | Tier-2 property index (schema DSL `@index` + B+tree) | todo |
| 6 | Tier-3 unique constraint (unique variant of tier 2) | todo |
| 7 | `VERSION` bump + ARCHITECTURE.md update | todo |

Post-implementation cleanup & squash merge: design §10 (do **not** start until instructed).

## Open / parked items

### Carried from review (Record only — revisit later)

- **E-1 (cycle → silent misread)**: the directory chain walk is a bounded
  `for _ in 0..dir_index` loop, so a cyclic `next` link cannot hang or panic — it only
  mislands on already-corrupt input (the chain is built solely by `ensure_dir_page`; no external
  write path). Future hardening: "steps > expected page count → `StorageCorrupted`". Not needed
  for Phase 1 merge.
- **E-2 (mixed append in Phase 2)**: directory pages and topology/property pages share the
  single `append_page` counter for physical allocation. Verify the interleaving is correct when
  Phase 2 wires the directory into `topology.rs`. → **Phase 2 review item.**
- **`StorageCorrupted` payload reuse**: `push`'s `UNMAPPED` guard returns
  `StorageCorrupted(physical)`, repurposing a variant meant for a "corrupt page id" to carry the
  rejected value. Acceptable (unreachable in practice; useful diagnostic). Consider a dedicated
  variant if the error enum is ever tidied up.

### Deferred design decisions (from design §8)

- (5) directory cache strategy — page-cache only vs. pinning a hot root tier.
- (6) WAL/rollback consistency for directory growth and index updates (§11.3 premise: indexes
  must live fully on-disk).
- (7) B+tree node layout / split thresholds (only the approach is fixed in §11).
- (8) ~~property RID: logicalize (a) vs. keep physical (b)~~ → **decided (a) logicalize**, folded
  into Phase 2 (the (b) plan breaks once topology shares the append tail — design §3.4).
- (9) tier-1 label index key: primary type only (a) vs. all labels (b) — tentative (b),
  decide at Phase 4.
