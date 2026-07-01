---
name: triad-orchestrate
description: Runs one triad session's side of ChiffonDB's review⇄implementation loop against the triad server. With `register worker|reviewer <session-id>`, join a role and start long-polling for instructions. On each instruction, run the matching chiffon-review subcommand (verify/result/fix/request/close), then report /done and wait again. The server (triad-poc) owns handoff and command approval.
---

# triad-orchestrate (ChiffonDB)

Drives **one session's** side of the triad loop (design §6). The server (`triad-poc`,
127.0.0.1:8787) decides whose turn it is; this skill registers the session, waits for
its next instruction on a long-poll, runs the matching work, reports completion, and
waits again. It never decides the handoff itself — it obeys the server.

The per-action work is the **chiffon-review** skill (verify/result/fix/request/close),
governed by docs/review-process.md. This skill only orchestrates: it maps a server
instruction to a chiffon-review subcommand.

## Prerequisites (start these once, before registering)

1. Build/install the triad binaries from the triad repo (`~/work/dev/products/ai/triad`):
   `cargo install --path .` → `triad-poc` and `triad-exec` land on PATH.
2. Start the server **from the ChiffonDB root** so it reads this project's `.triad.toml`
   allowlist (cargo test/clippy/fmt):
   ```bash
   cd ~/work/dev/products/rinne-graph/chiffondb/chiffondb
   triad-poc &      # logs "loaded 3 project allow rule(s)"
   ```
   Only one server for both sessions.

## Roles (never mix them)

- **worker** (implementation session): writes/fixes core logic. Runs `request` and `fix`.
- **reviewer** (review session): verifies, records, closes. Runs `verify`/`result`/`close`.
  **Never edits core logic** — a bug goes *back* to the worker (review-process.md).

The server enforces worker ≠ reviewer: `/register` returns 409 if a role is taken or a
session tries to hold both. Do not work around a 409 — it is the confirmation-bias guard.

## Command execution goes through triad-exec

Every git/cargo command must go through the L2 gate, not directly:

```bash
triad-exec --session "$SID" -- <cmd> [args...]
```

`triad-exec` asks the server for approval, then runs the command. `deny` prints a reason
+ alternative (do that instead); `escalate` blocks until the user answers
(`triad answer` / POST /answer). Do **not** bypass it by calling git/cargo directly —
that defeats the safety gate and the destructive-git ban (review-process.md).

## `register <role> <session-id>` — join and start the loop

`<role>` is `worker` or `reviewer`; `<session-id>` is a stable unique string (e.g.
`worker-1`).

```bash
curl -sS -X POST localhost:8787/register -H 'content-type: application/json' \
  -d "{\"role\":\"$ROLE\",\"session_id\":\"$SID\"}"
```

A 409 means the role is taken (or this session holds the other role) — stop and tell the
user; do not retry as the other role.

After a clean register, what to do next depends on the role and the cycle:

- **reviewer** (and worker in any cycle after the first): enter the wait loop below.
- **worker, at the very first cycle** (`GET /state` shows `implementing` with no prior
  request): do **not** long-poll yet. The first implementation task is not a triad
  instruction — the human seeds it directly into this session's prompt (design §9). Wait
  for that human instruction, do the work, run `request` + `/done`, and only *then* enter
  the wait loop for subsequent `fix` turns. Long-polling before the seed would just block
  on empty 204s and could swallow the human's message.

## The wait loop (long-poll → act → done → repeat)

Run the long-poll as a **background** command so a completed poll restarts the session
with the instruction (design §2 method A):

```bash
curl -sS --max-time 620 "http://127.0.0.1:8787/next?session_id=$SID&wait=600"
```

- **204 / empty**: nothing assigned in the window. Re-issue the long-poll.
- **200 with `{"action":"...","note":"...","sha":"..."?}`**: it is this session's turn.
  Dispatch on `action`:

  | instruction `action` | what to run |
  |---|---|
  | `verify`   | chiffon-review `verify` (reviewer) — snapshot first, then report the SHA (below) |
  | `result`   | chiffon-review `result` (reviewer) |
  | `close`    | chiffon-review `close` (reviewer) |
  | `fix`      | chiffon-review `fix` (worker) |
  | `request` / `implement` | do the implementation work, then chiffon-review `request` (worker) |
  | `rollback` | `triad-exec --session "$SID" -- git reset --hard <sha>` using the instruction's `sha` (recovery, design §4.4) |
  | `await-human` | not actionable; the cycle is at the human gate. Stop and wait. |

  If `action` is not in the table, do not guess — report the raw instruction and stop.
- After the work is done, report completion (below), then long-poll again.

## Reporting completion — `/done`

```bash
curl -sS -X POST localhost:8787/done -H 'content-type: application/json' \
  -d "{\"session_id\":\"$SID\",\"action\":\"$ACTION\",\"artifact\":\"$ARTIFACT\",\"must_fix\":$MUST_FIX}"
```

- `action`: the chiffon-review subcommand that completed.
- `artifact`: the file it produced, e.g. `docs/review-request-2026-MM-DDx.md` (request),
  `docs/review-2026-MM-DDx.md` (result). Omit for actions with no file. A **422** means
  the file is not where you said — you have not actually produced it (the server rejects
  false "done"); fix that before retrying.
- `must_fix`: for `result` only — `true` if the review found Must-fix items, else `false`.
  The **reviewer** decides this (design §5.1). It selects the branch: `true` → worker is
  woken to `fix`; `false` → reviewer is woken to `close`.

A **403** means this session does not hold the role the current state expects (e.g. a
worker reporting `verify`). You are acting out of turn — stop; the other session handles it.

A **409** means the event did not fit the current state (out of order, or it would break
verifier≠fixer). Read the message; do not force it.

## WIP snapshot (reviewer, at verify start)

chiffon-review `verify` takes the review snapshot
(`git add -A && git commit -m "wip: review snapshot ..."` via triad-exec). Report that SHA
so rollback can target it (design §4.4):

```bash
curl -sS -X POST localhost:8787/snapshot -H 'content-type: application/json' \
  -d "{\"sha\":\"$SNAPSHOT_SHA\"}"
```

triad records the SHA; it never runs the commit or the reset — this session does (§10).

## Stopping

- `await-human`: the cycle reached the post-close human gate. The user approves it out of
  band (`POST /gate`); this session just waits. Do not advance it yourself.
- A 403/409/422 you cannot resolve, or an unknown instruction: stop and surface it to the
  user rather than looping.
