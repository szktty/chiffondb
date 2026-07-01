# Review request 2026-06-30c — Phase 1: PageDirectory (code)

> This is a **code-diff review** — `docs/review-process.md` applies in full: read the diff
> against the real code, back concerns with throwaway probes, confirm green yourself, and
> **do not edit the core logic** (a failing test that points at an implementation bug goes
> back to this implementation session). Add the kinds of tests an implementer tends to miss
> (corruption injection, property tests, invariants) as permanent tests.

## Target

- Branch: `feature/variable-topology`
- Commit under review: `676dfc7` ("feat(storage): add logical→physical page directory (Phase 1)")
- Diff to read: `git show 676dfc7`
- New file: `chiffondb-core/src/storage/page_directory.rs`; module registered in
  `chiffondb-core/src/storage/mod.rs`.
- Design: `docs/design-variable-topology.md` (§3.1 page directory, §9 Phase 1). Prior design
  reviews: `docs/review-2026-06-30a.md`, `docs/review-2026-06-30b.md` (both untracked).

## Scope of Phase 1

Only the standalone `PageDirectory` data structure: a persisted dense logical→physical page
map backed by a chain of directory pages, faulted through `DatabaseFile`'s bounded page cache.
**It is not yet wired into `topology.rs`** — that is Phase 2. So this review is about the
structure's correctness, on-disk layout, and invariants in isolation.

## On-disk layout (as implemented)

Each directory page:

```
[0..4]   next directory page id (u32, 0xFFFF_FFFF = end of chain)
[4..]    entries: physical page id (u32) per logical slot, little-endian
```

- `ENTRIES_PER_DIR_PAGE = (PAGE_SIZE - 4) / 4 = 1023`.
- Logical entry `i` → directory page `i / 1023`, slot `i % 1023`.
- `UNMAPPED = 0xFFFF_FFFF` is the sentinel for both an unmapped entry and end-of-chain.
- `PageDirectory` holds only `{ root, len }` (O(1)); directory pages are read/written on demand.

## What to verify

Back each judgment against the real code, not the prose above.

1. **Layout / arithmetic.** Confirm `ENTRIES_PER_DIR_PAGE`, `entry_offset`, and the
   `i / ENTRIES_PER_DIR_PAGE` / `i % ENTRIES_PER_DIR_PAGE` mapping never read/write outside the
   4096-byte page, including the last slot. CLAUDE.md requires hand-written byte work to mind
   alignment — check the `[off..off+4]` slices.

2. **Density invariant.** `set` rejects `logical > len` (`StorageCorrupted`) to keep the
   logical space dense. Is `push` the only mutator, and does the `len`-only bound hold? Probe
   whether any sequence can leave a hole (an `UNMAPPED` entry below `len`) that `resolve` would
   then return `None` for — and decide whether that is acceptable or a latent bug.

3. **Persistence / restore.** `with_root(root, len)` reconstructs from metadata alone. Verify a
   directory built, then reopened via `with_root`, resolves every entry — including across a
   directory-page boundary (the existing test only restores 2 entries; consider a multi-page
   restore).

4. **Cache/WAL behavior.** Directory pages go through `read_page`/`write_page`, so they ride the
   WAL write-through and rollback like any page. Sanity-check there's no in-memory directory
   state that could desync from disk on rollback (design §11.3 premise). `root`/`len` live in
   the struct, not on disk yet — note that persisting them is Phase 2/4's job (header fields),
   and flag if that hand-off is unclear.

5. **`UNMAPPED` collision.** `0xFFFF_FFFF` doubles as "unmapped entry" and "end of chain". Since
   it is also a plausible-looking physical page id, confirm a real physical pid can never equal
   `UNMAPPED` in practice (page counts are far below 2^32), or flag the aliasing risk.

6. **Error handling / style.** No `unwrap()`/`expect()`/`unsafe` (the `unwrap_or([0xFF;4])` on
   the infallible slice→array conversion is intentional — assess whether sentinel-on-impossible
   is the right choice vs. propagating). Confirm `cargo fmt --check`, `cargo clippy --all-targets
   -- -D warnings`, and `cargo test -p chiffondb-core` are green (claimed: 305 passed).

## Tests to consider adding (reviewer-owned)

- Corruption injection: a directory page whose `next` link points off the end of the file, or a
  cycle in the chain — does `resolve`/`ensure_dir_page` terminate / error cleanly rather than
  loop or panic?
- Property test: push N random physical ids, assert every logical i resolves to what was pushed,
  for N spanning several directory pages.
- Exact boundary: entry at slot `ENTRIES_PER_DIR_PAGE - 1` vs `ENTRIES_PER_DIR_PAGE`.

## Out of scope

- Wiring into `topology.rs`, capacity removal, header persistence of `{root, len}` — all Phase 2+.
- Free-list / reuse, CoW — future work (§7).

## Deliverable

Write `docs/review-2026-06-30c.md` (untracked, do not commit): target commit, the exact verify
commands you ran and their output, per-point findings, any tests you added (these are committed
by the implementation session, or noted for it), and a conclusion
(approve / approve-with-changes / needs-rework). Implementation-logic changes go back to this
session; do not edit `page_directory.rs` core logic in the review session.
