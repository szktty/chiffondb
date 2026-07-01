# Review request 2026-06-30d — Phase 1 send-back: F-5 UNMAPPED guard

> Send-back after `fix`, addressing `docs/review-2026-06-30c.md` (conclusion: **approve**, with
> one **Before release** item F-5). Code-diff review per `docs/review-process.md`.

## Target

- Branch: `feature/variable-topology`
- Base: the Phase 1 commit `676dfc7` (not yet committed-over; `fix` does not commit — `close`
  will fold these changes into the Phase 1 commit).
- File changed: `chiffondb-core/src/storage/page_directory.rs` (working tree, uncommitted).
- Prior review: `docs/review-2026-06-30c.md` (untracked).

## Findings addressed

### F-5 (Before release) — `push` lacked an `UNMAPPED` sentinel guard → FIXED

The review noted that `push(UNMAPPED)` would advance `len` yet `resolve` to `None`
(mapped-but-invisible), because `0xFFFF_FFFF` doubles as the unmapped/end-of-chain sentinel.
Although unreachable today (a real physical pid that large needs a ~16 TB file), it is a latent
trap once a free-list / pid reuse lands.

Fix: `push` now rejects `physical == UNMAPPED` up front, returning
`GraphError::StorageCorrupted(physical)` — consistent with the existing density guard in `set`
(`logical > len` → `StorageCorrupted`). The failed push does **not** advance `len`.

Chose a real `Result` error over `debug_assert!` deliberately: this must hold in release builds
too, since the whole point is to stop a corrupt/reused pid from being smuggled in.

Permanent test added: `push_rejects_unmapped_sentinel` — asserts `push(0xFFFF_FFFF)` errors and
`len` stays 0.

## Not fixed (by design — Record only, parked for plan / Phase 2)

- **E-1 (cycle → silent misread)**: bounded `for _ in 0..dir_index` walk cannot hang/panic; a
  cycle only mislands on corrupt input. Record only; a "steps > expected pages →
  StorageCorrupted" check is a future hardening (goes to `docs/plan-*.md` at `close`).
- **E-2 (mixed append in Phase 2)**: directory vs topology/property physical allocation sharing
  the single `append_page` counter is a **Phase 2** review item, not Phase 1.

These are intentionally left untouched (`fix` only addresses Must fix / Needs a test; Record-only
stays parked).

## Reviewer-added tests (now in the working tree, to be committed at `close`)

From review 2026-06-30c (+91, tests only, core unchanged):
- `with_root_restores_across_directory_page_boundary`
- `exact_directory_page_slot_boundary`
- `corrupt_offend_next_link_errors_cleanly`
- `push_resolve_roundtrip_multipage` (proptest)

## Points to look at

1. Confirm the guard placement: `push` is the only public mutator, so guarding it (not `set`)
   covers every external path. Is there any other route that writes an entry?
2. Confirm the new test pins the invariant (`len` unchanged on rejection), and that the guard
   does not regress the proptest (its range `0..1_000_000` never hits `UNMAPPED`).
3. Confirm `StorageCorrupted(physical)` is the right variant vs. introducing a dedicated
   `InvalidArgument`-style variant. (The codebase uses `StorageCorrupted` for invariant breaks.)

## Verification (run by implementation session)

```
cargo test -p chiffondb-core page_directory   → 10 passed (was 9; +push_rejects_unmapped_sentinel)
cargo test --workspace                        → core 310 passed / 18 / 6, all 0 failed
cargo clippy --all-targets -- -D warnings     → clean
cargo fmt --check                             → exit 0
```

(Reviewer should independently confirm, incl. `PROPTEST_CASES=1000`.)

## Deliverable

Append to / update `docs/review-2026-06-30c.md` or write `docs/review-2026-06-30d.md` (untracked).
If F-5 is satisfied and nothing new surfaces, the cycle can proceed to `close` (commit the Phase 1
diff + reviewer tests + this guard as the implementation commit, update plan, issue the next
implement-request for Phase 2).
