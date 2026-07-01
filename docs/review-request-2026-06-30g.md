# Review request 2026-06-30g — Phase 3: remove vestigial segment boundaries (code)

Code-diff review per `docs/review-process.md`. **Snapshot the uncommitted working tree first**
(`git add -A && git commit -m "wip: review snapshot 2026-06-30g"`), review against that SHA, and
do not run destructive git against the implementer's tree.

> Reviewing the diff: the Phase 3 code change is **`git diff HEAD -- chiffondb-core chiffondb`**
> (3 files). A `git diff` against the prior WIP snapshot also shows review-doc *deletions* — those
> are untracked review files that an earlier snapshot swept in; they are **not** part of Phase 3.
> Look at the three source files.

## Header

- Branch: `feature/variable-topology` (forked from `e134249`)
- Source implement-request: `docs/implement-request-2026-06-30c.md`
- State: **uncommitted working tree** (impl session does not commit; `close` will).
- Files changed:
  - `chiffondb-core/src/storage/file.rs` (±~83): drop 4 header fields + 3 consts, bump VERSION,
    update header-roundtrip test, add a v4-rejection test.
  - `chiffondb-core/src/error.rs` (−6): remove dead `CapacityExceeded`.
  - `chiffondb/src/commands/info.rs` (±~29): print directory-based facts instead of segments.

## The change (and why)

After Phase 2b everything resolves through page directories on the append tail, so the fixed
segment machinery is vestigial. This phase deletes it (cleanup the 2b review parked).

- **Removed header fields** (`file.rs`): `topology_segment_start`, `property_segment_start`,
  `vector_segment_start`, `page_directory_root` (struct fields + serialize/deserialize at offsets
  16..32 + `FileHeader::new` defaults + the three `*_SEGMENT_DEFAULT_START` consts).
- **Offsets 16..32 left reserved (zeroed)**, not compacted — keeping the surviving fields'
  offsets (32..64) stable so the diff stays small and serialize/deserialize is a pure deletion.
- **VERSION 4 → 5** (layout change; old v4 files rejected by the existing version guard).
- **`CapacityExceeded` removed** (`error.rs`): Phase 2b deleted its only construction sites; grep
  confirms zero remaining constructors/matches.
- **`info.rs`**: was printing `topology/property/vector_segment_start`-derived page spans. Now
  prints `Node pages` / `Edge pages` / `Property pages` from the directory counts
  (`node_page_count` / `edge_page_count` / `property_dir_len`) — the facts that actually bound
  data now.

## Decision on item 2 (`page_directory_root`) — REMOVED

The implement-request required stating this explicitly. `page_directory_root` (old offset 28..32)
was the MVCC-reserved single root from design §6. That design decided to *merge* with the
directory mechanism, and Phase 2a/2b realized it as **per-kind** `node_dir_root` / `edge_dir_root`
/ `property_dir_root` (offsets 48..64). The old single field was always 0 and has no reader, so it
is **removed**, not kept-reserved: future CoW will snapshot the per-kind roots, which already
exist. (The 16..32 range as a whole stays reserved for genuinely-future use.)

## Tests

- `create_and_reopen_reads_correct_header` — updated to assert the directory fields restore (empty
  dirs / zero counts on a fresh DB) instead of the deleted segment starts.
- `open_rejects_previous_version_v4` — **new**: a v4-magic file is rejected with
  `UnsupportedVersion { found: 4, supported: 5 }` (DoD: old-version file rejected).
- Existing v1-rejection test kept (its segment-start consts inlined as literals).

## Points to look at in review

1. **Offset integrity.** Confirm serialize/deserialize round-trips with 16..32 zeroed and the
   surviving fields unmoved (schema 32..40, counts 40..48, property dir 48..56, node/edge dir
   56..64). No field reads the reserved range.
2. **`CapacityExceeded` truly dead.** Re-grep core + `chiffondb` + `chiffondb-ffi` for any
   constructor/match. If a binding crate (Dart/FFI) maps the error enum by variant, flag whether
   removing a variant shifts discriminants in a way that matters.
3. **`info` output.** Confirm it compiles and is sensible on a fresh DB (all zeros) and after
   inserting nodes/edges/properties. `total_pages` still printed; the old "Vector pages
   (reserved)" line is gone — acceptable since there's no vector write path.
4. **VERSION guard.** Confirm v4 (and any non-5) is rejected, and that `VERSION` is the single
   place the number lives.
5. **No `unwrap`/`expect` in non-test code**; reserved-range comments are WHY-only.

## Out of scope

- Indexes (Phases 4–6), ARCHITECTURE.md / CHANGELOG / squash (Phase 7 / §10).
- Free-list / CoW (§7 future). This phase **completes** the Phase 7 cleanup items the 2026-06-30f
  review parked (dead `CapacityExceeded`, vestigial segment fields) — update the plan at `close`.

## Verification (commands run, results)

```
cargo test -p chiffondb-core                 → 317 passed (was 316; +open_rejects_previous_version_v4)
cargo test --workspace                       → core 317 / 18 / 6, all 0 failed
cargo clippy --all-targets -- -D warnings    → clean
cargo fmt --check                            → exit 0
PROPTEST_CASES=1000 cargo test -p chiffondb-core → 317 passed (19.7s)
grep CapacityExceeded / segment_start / page_directory_root in *.rs → only historical comments
```

## Deliverable

`docs/review-2026-06-30g.md` (untracked). Classify findings; bugs go back via `fix`. Do not edit
core logic in review.
