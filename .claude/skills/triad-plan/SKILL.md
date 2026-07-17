---
name: triad-plan
description: Drives the project's review⇄implementation cycle. Five subcommands — verify (check), result (record), close (wrap up), request (ask for review), fix (address findings). Role separation between an implementation session and a review session, turned into runnable steps. With no argument, inspect repo state (presence of docs/review-request-* and docs/review-*) and propose which subcommand fits.
---

# triad-plan

Drives this project's "implementation session ⇄ review session" cycle. The authority
for what counts as done, what style to follow, and what a passing review requires is
the project's own docs (CLAUDE.md, README, or an equivalent process doc) — this skill
only turns that into runnable steps. When a judgment is unclear, read the project's
own conventions rather than guessing.

This skill is normally driven by the **triad** skill, which maps a triad-server
instruction (verify/result/fix/request/close) to the matching subcommand below. It can
also be run standalone by giving the subcommand as the first argument.

## Role separation (the overriding principle for every subcommand)

- **Implementation session (worker)**: writes/fixes core logic. Uses `request` at a
  stopping point and `fix` to address findings.
- **Review session (reviewer)**: verifies, adds tests, records. **Never edits core
  logic.** If it finds an implementation bug, it does not fix it — it sends it back
  (→ implementer runs `fix`). Owns `verify` / `result` / `close`.

To avoid confirmation bias, the verifier and the fixer are not the same session (the
triad server enforces this at the FSM level when this skill runs under `triad`).

## Cycle

```
            implement-request-*.md   (committed, reviewer→implementer)
                     │
        [impl]       ▼
            ─────────────────── request ──► review-request-*.md (untracked)
                                                  │
        [review]                                  ▼
            verify ─► result ──► review-2026-MM-DDx.md (untracked)
                                      │
                  ┌── Must fix ───────┘
                  ▼
            [impl] fix ─► (fix core) ─► send back via request
                  │
                  └── passes ─► [review] close ─► commit + update plan + next implement-request (commit)
```

## Subcommands

The first argument word is the subcommand. If none is given, see "When invoked with no argument".

### `verify` — verification (review session)

Take the review request and back its claims with the real code and live runs.

1. Read `docs/review-request-*.md`; note target commit / branch / changed files.
2. **Snapshot the review target first.** The implementer's diff is usually uncommitted,
   so take a read-only snapshot before touching anything: `git add -A && git commit -m
   "wip: review snapshot 2026-MM-DDx"` (via `triad exec` if this session is running
   under `triad`). Review against that SHA; if anything goes wrong, `git reset --hard
   <snapshot-sha>` restores it. `git stash` is **not** a substitute — it can be popped
   or dropped by mistake and is easy to lose track of mid-review. The snapshot is
   folded into the implementation commit at `close`.
3. **Read the diff against the real code.** Don't trust the summary; trace into related
   existing code the change touches (interfaces, data layout, invariants it depends on).
4. **Prove concerns with throwaway probes.** Add a temporary test/script to observe
   behavior directly; if needed, temporarily reproduce the pre-change state to decide
   "pre-existing bug or introduced by this change".
5. **Remove the probe by hand — never with a destructive git command.** Delete exactly
   the lines you added; do **not** run `git checkout <ref> <file>` (with or without
   `--` — both discard uncommitted work), `git stash`, `git reset`, `git restore`,
   `git rm`, or `git clean` (they would wipe the implementer's uncommitted code). The
   triad gate denies these forms builtin. Leave formatting/lint clean. Never dirty
   production code during review.
6. **Permanently add** tests covering the kinds of gaps the implementer tends to miss
   (edge cases, invariants, error paths). This is filling coverage gaps, not changing
   core logic. Added tests may be committed separately from the implementation diff.
7. **Confirm green yourself** using this project's standard build/test/lint commands
   (run them via `triad exec` if this session is running under `triad`). Also verify
   any test-count or coverage claims in the request actually hold.
8. **If the change under review touches `.triad.toml`**, verify the gate actually
   behaves as the new config claims: probe the tightened/loosened command with
   `triad exec --check -- <cmd>` (dry-run — returns the verdict without running or
   escalating). The server hot-reloads the file on change, so the diff being correct
   and the gate being correct are the same thing to check, with no restart in between.
9. If a failing test reveals a core bug, **do not fix it** — record it under Must fix
   in `result` and send it back.

### `result` — record the review result (review session)

Write `docs/review-2026-MM-DDx.md` (`x` is a same-day sequence a, b, c...).
**Do not commit it (keep it untracked).** Sections: target commit / verification
commands and results / summary / per-point findings / extra findings / conclusion.

Classify findings:
- **Must fix (before merge)**: functional bugs, or real regressions against the
  project's required properties.
- **Needs a test**: uncovered fragile paths.
- **Before release**: harmless alone but required before publishing.
- **Record only / refactor candidate**: design improvements that don't affect
  merge-ability → leave them in `docs/plan-*.md`.

Note: if a judgment is later settled or changed in discussion, update the review
result file to the latest decision *before* `close` creates the implement-request
(don't let the implementer read stale, unsettled info).

### `close` — wrap up the review cycle (review session)

Run only after the review passes.

1. **Implementation commit**: the implementation session does not commit its own
   changes, so commit the implementation diff here (keeps history clean; no
   unreviewed code lands in git). If `verify` took a `wip: review snapshot` commit,
   fold it into the real implementation commit now (`git commit --amend` / squash) so
   the WIP message never lands in history as-is.
2. **Progress update**: update TODOs / open items / priorities in `docs/plan-*.md` to
   match reality and **commit** it. Park `Record only / refactor candidate` findings
   here too.
3. **Next implement-request**: once the next task and approach are settled in
   review/discussion, create `docs/implement-request-YYYY-MM-DDx.md` and **commit** it
   (reviewer→implementer handoff). State goal / work / DoD / approach / out-of-scope,
   and **write the decided approach** (do not ask for re-deliberation).

### `request` — create a review request (implementation session)

When implementation reaches a stopping point, or as the send-back after `fix`, create
`docs/review-request-YYYY-MM-DDx.md`. **Do not commit it (keep it untracked).**
Include, at minimum:

- Header: title / Date / Branch (and what it forked from) / source implement-request / Files changed
- What was added (and why it was needed)
- Change (bulleted change points, the finalized API shape)
- Test added (TDD: did you confirm it fails before implementing; intent of added tests)
- Points to look at in review
- Out of scope (and why)
- Verification (commands run and their results)

For a send-back, state which Must fix items in the original `review-2026-MM-DDx.md`
were addressed.

### `fix` — address review findings (implementation session)

Take the findings in `docs/review-2026-MM-DDx.md` and fix the core logic. Only the
implementation side touches core.

1. Read the review result; extract the **Must fix** and **Needs a test** items as the
   work to do. Leave `Record only / refactor candidate` alone here (it stays parked in plan).
2. Resolve each Must fix by fixing core logic. The verification tests the reviewer
   added must keep passing.
3. Follow the project's own coding conventions (CLAUDE.md or equivalent).
4. Confirm green using this project's standard build/test/lint commands (via `triad
   exec` if running under `triad`).
5. If a Must-fix item had you edit `.triad.toml`, confirm the gate change is live with
   `triad exec --check -- <cmd>` (the server reloads the file on change — a correct
   diff that hasn't taken effect cannot happen, but a config that fails to load closes
   the gate entirely, and you'd see that here).
6. When fixed, send it back via `request` (do not commit yourself — committing happens
   in `close`).

## When invoked with no argument

Infer the fitting subcommand from repo state and confirm before acting.

- `docs/review-request-*.md` exists and no matching `docs/review-*.md` → propose `verify`.
- `verify` done but result not yet written → propose `result`.
- `docs/review-*.md` has unresolved Must fix items → propose `fix` (impl side), or
  `close` if the review already passed (review side).
- Implementation is at a stopping point with no review-request → propose `request`.

If none applies or the state is ambiguous, summarize the current state and ask the
user which subcommand to run.
