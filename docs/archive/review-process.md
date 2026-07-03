# Review process

> Read this file only when responding to a review request. It is split out of CLAUDE.md so it
> is not loaded all the time. When you receive a `docs/review-request-*.md`, follow this guide.

## Role separation

Sessions fall into two kinds. **Separate the verifier from the fixer** so that assumptions the
implementer silently baked in are not waved through by their own tests (confirmation bias).

- **Implementation session**: writes the core logic against the spec. Writes its own regression
  tests TDD-style. Finalizes the concrete API (signatures, data layout) during implementation.
- **Review session (= the combined role of progress tracking, test writing, and review)**:
  objectively verifies and records the implemented diff. Does **not** modify the core logic
  (if a failing test indicates an implementation bug, send it back to the implementation
  session). Concretely, it owns these three consistently:
  1. **Review**: read the diff against the real code; assess correctness, regressions, design.
  2. **Test writing**: add the kinds of tests the implementer tends to miss (corruption
     injection, property tests, invariants, node/edge symmetry) and make them permanent. This
     is "filling coverage gaps", not changing core logic.
  3. **Progress update**: update the TODOs / open items / priorities in `docs/plan-*.md` to
     match reality.

## When to write tests (TDD is a tool, not a rule)

Do not rigidly apply "write the test first". A test is code that calls a concrete API, so it
cannot be written before the API exists. The API is often finalized during the implementation
session, and contradictions that the spec stage could not foresee (e.g. a trait's return type
clashing with a new `Result`-ification) only surface once implemented.

- **Test-first (implementer)**: pure logic with an obvious spec/API (parsers, serialization,
  capacity boundary values). I/O determined before the API is fixed.
- **Test-after (reviewer)**: things whose API solidifies mid-implementation against the storage
  layer, and where "what the invariant is" only becomes visible once the implementation settles
  (corruption injection, property tests, invariants, symmetry).

→ The principle is "**always write tests; choose *when* by the nature of the code**".

## How to review (empiricism)

Do not take "the tests pass, so it's correct" at face value. Real defects in this loop were
caught by **backing the request's claims with the real code and live runs**.

1. **Read the diff against the real code** (don't judge from the request's summary alone).
   Trace into related existing code (trait boundaries, header layout, snapshot/restore paths).
2. **Prove concerns with throwaway probes**. Insert a temporary test into the crate to observe
   behavior; if needed, **temporarily reproduce the pre-change state** to determine "was this a
   pre-existing bug / did this change fix it". (Example: temporarily removing the header restore
   in `restore` proved the schema-rollback drift existed before the change.)
3. **Always remove the probe — by hand, never with a destructive git command.** Delete exactly
   the lines you added (and revert exactly the lines you temporarily changed), then make
   `cargo fmt --check` pass. **Do not** use `git checkout -- <file>`, `git stash`, `git reset`,
   `git restore`, or `git clean` to "clean up": the review may run on a working tree that still
   holds the implementer's uncommitted code, and those commands would wipe it. See
   "Protecting the review target" below. Never dirty production code during review.
4. **Confirm green yourself**: `cargo test --workspace` / `cargo clippy --all-targets -- -D
   warnings` / `cargo fmt --check`. Also verify the request's test-count claims actually hold.

## Protecting the review target (uncommitted code)

The implementation session does not commit its own changes (that happens at `close`), so the
review often runs against an **uncommitted working tree**. A review session once destroyed that
work by running a `git checkout -- <file>` / `git stash` to remove a probe, and recovery was only
possible because another session still held the diff. To prevent a recurrence:

- **Snapshot before reviewing.** As the very first step of `verify`, the review session takes a
  read-only snapshot of the implementer's diff so it can always be restored:
  ```bash
  git add -A && git commit -m "wip: review snapshot 2026-MM-DDx"   # snapshot SHA = the review target
  ```
  Review against that SHA. If anything goes wrong, `git reset --hard <snapshot-sha>` restores it
  exactly. (`git stash` is **not** a substitute: popping a stash puts the code back into the same
  destructible uncommitted state, and a shared stash stack invites cross-session collisions.)
  At `close`, the WIP snapshot is folded into the real implementation commit (squash/amend) so it
  never lands in history as-is.
- **No destructive git in review.** The review session must never run `git checkout -- <path>`,
  `git restore`, `git stash`, `git reset --hard`, or `git clean` against anything but its own
  throwaway probe lines. Probe removal is line-level and manual (step 3 above).
- **Prefer isolating probes** so there is nothing in production to "restore": put a probe in its
  own `#[cfg(test)]` module or a scratch file you can delete outright, rather than editing a
  production function in place.

## Deliverables

- **Review result**: write it to `docs/review-2026-MM-DDx.md` (target commit, verification
  commands, summary, per-point findings, extra findings, conclusion). **Do not commit it
  (keep it untracked)** — per recent convention. The request file `docs/review-request-*.md`
  is also **not committed** (keep it untracked).
- **Added tests**: may be committed separately from the implementation diff (part of verification).
- **Progress update**: commit the TODO/priority changes in `docs/plan-*.md`.
- **Implementation commit**: the implementation session **does not commit its own changes**.
  The commit is made by the review session after the review passes, as part of closing out the
  review cycle. This keeps the commit history clean and ensures no unreviewed code lands in git.
- **Next-phase implementation request**: once the next task and its approach are settled in
  review/discussion, create and **commit** `docs/implement-request-YYYY-MM-DDx.md` so the
  implementation session can read it cold (state the goal, the work, the DoD, the approach, and
  what is out of scope; write the decided approach and do not ask for re-deliberation). This is
  the reviewer→implementer handoff; the implementer returns a `review-request-*` when done.
  **Important**: if a judgment written in the (untracked) review result file is later settled or
  changed in discussion, update the review result file to the latest decision *before* creating
  the implementation request (don't let the implementer read stale, unsettled info).

## Weighting findings

- **Must fix (before merge)**: functional bugs, or real regressions against required properties
  such as "lightweight / bounded".
- **Needs a test**: uncovered fragile paths (rollback, crash recovery, cross-page alloc, etc.).
- **Before release**: harmless alone but required before publishing (e.g. missing format version
  validation).
- **Record only / refactor candidate**: design improvements that don't affect merge-ability.
  Leave them in `docs/plan-*.md`.
