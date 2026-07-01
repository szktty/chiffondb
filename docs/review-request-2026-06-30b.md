# Review request 2026-06-30b — re-review of design-review responses

> Follow-up to `docs/review-request-2026-06-30a.md` (result: approve-with-changes,
> recorded in `docs/review-2026-06-30a.md`). This asks you to confirm the **responses** to
> that review landed as intended. Still a **design-document review**, not a code-diff review.

## Target

- Branch: `feature/variable-topology`
- Document: `docs/design-variable-topology.md`
- **Change under re-review**: commit `d7c2e76` ("docs: address design review 2026-06-30a
  findings"). Diff to read: `git show d7c2e76`.
- Prior review result: `docs/review-2026-06-30a.md` (untracked).

## What changed in response to each finding

Confirm each response actually resolves the original finding (not just acknowledges it), and
that the **new tentative decisions** introduced while resolving them are sound.

- **C-1 (property RID asymmetry)** → new §3.4. Decision: property RID **stays physical (b)**,
  revisit at Phase 3. Check: is "physical RID survives removing the fixed boundary" actually
  true given `PropertyStore::write` / `read` and any code that assumes property pages live in a
  contiguous `[prop_start, …)` range (e.g. `storage/value.rs` scan loops `prop_start..page_count`)?
  If something still relies on that contiguity, (b) is not as free as stated.

- **C-2 (find takes &str, not PropertyPath)** → §11.2 now says the search API will be extended
  to `PropertyPath`. Check: does `PropertyPath` already impl `From<&str>` so existing flat-key
  callers keep compiling, and does the "indexed → B+tree, else scan fallback" plan cover all
  current `find`/`find_all` callers (`db.rs`, FFI)?

- **C-3 (label index = primary type only)** → §11 tier 1 now tentatively keys on **all labels
  (b)**. Check: does registering a node under every label id play correctly with set-normalized
  multi-labels and dynamic labels (the recent feature)? Specifically, does `node.node_type_id`
  + `label_ref` give a complete label set, and are there callers of `rids_of_type` that expect
  **primary-only** semantics and would break under (b)?

## New open items to sanity-check (§8 items 8, 9)

The responses added two open items (property RID logical-vs-physical; label index key scope).
Confirm these are genuinely deferrable to Phase 3 / Phase 4 and do not block Phase 1–2 (the
page-directory core), so implementation can start on Phase 1 without them resolved.

## Recorded notes to confirm

- §6: in-place→CoW update-discipline note — accurate?
- §11.3: the "indexes must be fully on-disk for WAL rollback to hold" premise (linked to §8
  open item 6) — agree it's a real constraint, not over-stated?
- §4: are the newly-listed test capacity-boundary sites (`db.rs:2867` area; `index.rs` /
  `pathfinding` `make` helpers pre-allocating pages 1..64) and `commands/info.rs` the complete
  set, or are there more boundary-assuming sites?

## Deliverable

Append to or write `docs/review-2026-06-30b.md` (untracked, do not commit). Conclusion:
approve (ready for Phase 1) / approve-with-changes / needs-rework. Anything to change goes back
to this (implementation) session; the review session does not edit the design doc directly.
