# Review request 2026-06-30e — Phase 2a: property RID logicalization (code)

Code-diff review per `docs/review-process.md` (read the diff against real code, back concerns
with throwaway probes, confirm green yourself, do not edit core logic — send bugs back).

## Header

- Branch: `feature/variable-topology` (forked from `e134249`)
- Source implement-request: `docs/implement-request-2026-06-30a.md` (revised: property RID
  logicalization folded into Phase 2; see its "⚠️ Revised" section) + design §3.4 (decided (a)).
- State: **uncommitted working tree** (impl session does not commit; `close` will).
- Files changed:
  - `chiffondb-core/src/storage/file.rs` (+90): header fields + property page-directory helpers.
  - `chiffondb-core/src/storage/value.rs` (~±290): property read/write rerouted through the
    directory (also a refactor that collapses `write`/`write_raw` and `read`/`read_raw` onto
    shared helpers).
  - `docs/plan-variable-topology.md`: ordering note (2a before 2b) — doc only.

## What was added (and why)

Phase 2 wiring forces property RIDs to be logical (design §3.4 (a)): once topology pages share
the file's append tail, the old `prop_start..page_count` property scan and the blob chain's
`first_pid + next` relative offset both break. 2a logicalizes property addressing first so that
2b (topology) can then append safely.

## Change

- **Header (`file.rs`)**: add `property_dir_root` / `property_dir_len` at offsets 48..56;
  `NO_DIR_ROOT = 0xFFFF_FFFF`. Bump `VERSION` 3 → 4 (layout change; old files rejected — no
  backward compat, per §0).
- **`DatabaseFile` property-directory helpers**: `property_dir()` builds a `PageDirectory` view
  from the header; `store_property_dir()` writes root/len back through `write_header()` (WAL
  write-through, crash-safe); `alloc_property_page` (append physical + `push` + persist),
  `resolve_property_page`, `read_property_page`, `write_property_page`, `property_page_count`.
  The header is the single source of truth; no in-memory directory state on `DatabaseFile`.
- **`value.rs`**: `write`/`write_raw` → `write_property_bytes`; `read`/`read_raw` →
  `read_property_bytes`. RID `page_id` is now a **logical** property page number.
  - Small value: scan logical pages `0..property_page_count` for a slot, else `alloc_property_page`.
  - Blob (> page): `write_blob_chain` builds the pages (its `next` is a relative index), each
    page is `alloc_property_page`-mapped, then `relink_chain_to_logical` rewrites each `next` to
    the *next page's logical number*; read follows logical `next` via the directory and
    reassembles with `read_blob_chain_dense` (order-preserving concat).
  - `CHAIN_HEAD_SLOT = 0xFFFF` unchanged as the chain-head RID marker.

## Test added (TDD: added after the API solidified against storage; intent below)

`value.rs` tests (4 new, all green):
- `large_blob_spans_chain_and_roundtrips` — multi-page chain round-trips; RID slot is
  `CHAIN_HEAD_SLOT`; `property_page_count >= 3`.
- `properties_survive_interleaved_physical_appends` — the core invariant: appending unrelated
  physical pages between property writes does not corrupt earlier properties (logical resolution).
- `large_blob_persists_across_reopen_with_interleaving` — chain resolves from header metadata
  after reopen, with interleaved appends.
- (existing `property_store_*`, `random_blob_roundtrip`, etc. still pass — regression check.)

## Points to look at in review

1. **Blob relink correctness.** `write_blob_chain` writes relative-index `next`, then
   `relink_chain_to_logical` overwrites it. Confirm no path reads the relative `next` after
   relink, and that `read_blob_chain_dense` (which ignores `next` and concats in collected order)
   matches the order the read loop collects pages in. Probe a chain of exactly 2 and ≥3 pages.
2. **Single source of truth.** `DatabaseFile` holds no directory struct — every op rebuilds from
   the header and stores back. Confirm there's no stale-metadata window (e.g. two writes in one
   logical op) and that `store_property_dir` always runs after a `push`.
3. **WAL/rollback.** `alloc_property_page` does `append_page` + `push` (which writes directory
   pages) + `write_header`. On rollback, do all three unwind together? Add/verify a rollback test
   if the existing WAL tests don't cover property growth.
4. **`is_valid()` reliance.** The small-value path still calls `SlottedPage::is_valid()` when
   scanning logical pages for free space. Now that only real property pages are in the property
   directory, is `is_valid()` still needed / correct? (It was originally a guard against reading
   non-property pages in the mixed `prop_start..page_count` scan.)
5. **`property_page_count` as a read limit.** `read_property_bytes` uses it to bound the chain
   walk. Confirm it's a safe upper bound (chain length ≤ total property pages).
6. **`unwrap_or` on slices.** `read_property_bytes` / `read_blob_chain_dense` use
   `try_into().unwrap_or(...)` on 4-byte slices (infallible) — same intentional pattern as
   `page_directory.rs`. Assess.

## Out of scope (Phase 2b / later)

- Topology wiring through node/edge directories, `*_capacity` removal — **Phase 2b**.
- `db.rs` segment pre-allocation removal — Phase 2b (still pre-allocates pages 1..64; property
  now lives past that via the directory, which is fine for 2a).
- ARCHITECTURE.md / CHANGELOG — Phase 7 / cleanup.

## Verification (commands run, results)

```
cargo test -p chiffondb-core value::       → 18 passed (4 new)
cargo test -p chiffondb-core file::        → 20 passed
cargo test --workspace                     → core 313 / 18 / 6, all 0 failed
cargo clippy --all-targets -- -D warnings  → clean
cargo fmt --check                          → exit 0
```

(Reviewer: please also run `PROPTEST_CASES=1000 cargo test --workspace`.)

## Deliverable

`docs/review-2026-06-30e.md` (untracked). Classify findings (Must fix / Needs a test / Before
release / Record only). Bugs go back to the implementation session via `fix`; do not edit core
logic in review.
