---
name: triad
description: Runs one triad session's side of the review⇄implementation loop against the triad server. With `register worker|reviewer <session-id>`, join a role and start long-polling for instructions. On each instruction, run the matching triad-plan subcommand (verify/result/fix/request/close), then report /done and wait again. This is the session-side loop of docs/design.md §6; the server (triad-server) owns handoff and command approval.
---

# triad

Drives **one session's** side of the triad loop (design §6). The server (`triad-server`,
127.0.0.1:8787) decides whose turn it is; this skill registers the session, waits for
its next instruction on a long-poll, runs the matching work, reports completion, and
waits again. It never decides the handoff itself — it obeys the server.

The actual per-action work (verify/result/fix/request/close) is the **triad-plan**
skill (design decision #12). This skill only orchestrates: it maps a server
instruction to a triad-plan subcommand.

## Roles (never mix them)

- **worker**: writes/fixes core logic. Runs `request` and `fix`.
- **reviewer**: verifies, records, closes. Runs `verify` / `result` / `close`. Never
  edits core logic — a bug goes *back* to the worker (design §3, §10).

The server enforces worker ≠ reviewer; `/register` returns 409 if a role is taken or a
session tries to hold both. Do not work around a 409 — it is the confirmation-bias guard.

## Command execution goes through `triad exec`

Every git/build command this session runs must go through the L2 gate, not directly:

```bash
triad exec --session "$SID" -- <cmd> [args...]
```

`triad exec` asks the server for approval and only then runs the command. `deny` prints
a reason + alternative (do that instead); `escalate` blocks until the user answers. Do
**not** bypass it by calling `git`/`cargo` directly — that defeats the safety gate.

Two helpers around the gate:

- **Dry-run**: `triad exec --check -- <cmd>` returns the verdict (allowed / denied /
  would-escalate) without running anything and without parking a question in the user's
  queue. Use it when unsure whether a command would escalate, and after editing
  `.triad.toml` to confirm the change actually took effect.
- **Withdraw a mistaken escalation**: if you escalated a command by mistake, take it
  back yourself instead of leaving it in the user's queue:
  ```bash
  curl -sS -X POST localhost:8787/cancel \
    -H 'content-type: application/json' -d "{\"session_id\":\"$SID\"}"
  ```
  (add `"id": N` to withdraw one specific escalation; ids are in `GET /state`). The
  blocked `triad exec` returns immediately with a deny.

`.triad.toml` edits take effect on the **next** `triad exec` automatically — the server
reloads the file when it changes; no restart is needed. While an edited config fails to
load (parse error, or an unsafe allow entry), the gate fails closed: every command is
denied with the load error until the file is fixed.

## `register <role> <session-id>` — join and start the loop

`<role>` is `worker` or `reviewer`. `<session-id>` is any stable string unique to this
session (e.g. `worker-1`).

1. Register:
   ```bash
   curl -sS -X POST localhost:8787/register \
     -H 'content-type: application/json' \
     -d "{\"role\":\"$ROLE\",\"session_id\":\"$SID\"}"
   ```
   A 409 means the role is taken (or this session holds the other role) — stop and tell
   the user; do not retry as the other role.
2. What to do next depends on the role and the cycle:
   - **reviewer** (and worker in any cycle after the first): enter the wait loop below.
   - **worker, at the very first cycle** (`GET /state` shows `implementing` with no prior
     request): do **not** long-poll yet. The first implementation task is not a triad
     instruction — the human seeds it directly into this session's prompt (design §9). The
     triad `session_id` is just an HTTP label, not a FleetView teammate, so a seed cannot
     arrive over `/next` or SendMessage; it comes from the human. Wait for it, do the work,
     run `request` + `/done`, and only *then* enter the wait loop for subsequent `fix`
     turns. Long-polling before the seed would block on empty 204s and could swallow the
     human's message.

## The wait loop (long-poll → act → done → repeat)

This is the core of design §6.1/§6.2. Run the long-poll as a **background** command so a
completed poll restarts the session with the instruction (design §2 method A):

```bash
# Blocks up to the server's wait window; server, not curl, ends an empty poll (204).
curl -sS --max-time 620 "http://127.0.0.1:8787/next?session_id=$SID&wait=600"
```

- **204 / empty body**: nothing was assigned within the window. Re-issue the long-poll.
- **200 with `{"action": "...", "note": "...", "message": "..."?, "sha": "..."?}`**: it
  is this session's turn. If a `message` field is present, it is a free-text handoff note
  the previous session left for you (e.g. why the working tree changed) — read it before
  acting. Then dispatch on `action`:

  | instruction `action` | what to run |
  |---|---|
  | `verify`   | triad-plan `verify` (reviewer) |
  | `result`   | triad-plan `result` (reviewer) |
  | `close`    | triad-plan `close` (reviewer) |
  | `fix`      | triad-plan `fix` (worker) |
  | `request` / `implement` | do the implementation work, then triad-plan `request` (worker) |
  | `rollback` | run `triad exec --session "$SID" -- git reset --hard <sha>` using the instruction's `sha` (the safety-net recovery, design §4.4) |
  | `await-human` | not actionable by a session; the cycle is at the human gate. Stop and wait. |

- After the work is done, report completion (next section), then long-poll again.

If an instruction's `action` is not in the table, do not guess — report the raw
instruction to the user and stop.

## Reporting completion — `/done`

When an action finishes, tell the server so it can advance the FSM and wake the next
role. Include the artifact you produced so the server can confirm it exists (design §7):

```bash
curl -sS -X POST localhost:8787/done \
  -H 'content-type: application/json' \
  -d "{\"session_id\":\"$SID\",\"action\":\"$ACTION\",\"artifact\":\"$ARTIFACT\",\"must_fix\":$MUST_FIX}"
```

- `action`: the triad-plan subcommand that completed (`request`/`verify`/`result`/`fix`/`close`).
- `artifact`: the file the action produced, e.g. `docs/review-request-2026-MM-DDx.md`
  for `request`, `docs/review-2026-MM-DDx.md` for `result`. Omit the field for actions
  with no file. A **422** means the file is not where you said — you have not actually
  produced it; fix that before retrying (the server rejects false "done").
- `must_fix`: for `result` only — `true` if the review found Must-fix items, else
  `false`. The **reviewer** decides this, not the server (design §5.1). It selects the
  branch: `true` → the worker is woken to `fix`; `false` → the reviewer is woken to `close`.
- `message` (optional): a free-text one-liner for whoever acts next, passed through
  verbatim onto their `/next` (design §5.1). Use it for a short heads-up that doesn't
  belong in an artifact file — e.g. the reviewer explaining a WIP snapshot commit so the
  worker isn't puzzled by the changed working tree. Omit it when there's nothing to say.

A **403** means this session does not hold the role the current state expects (e.g. a
worker reporting `verify`, which is the reviewer's action). You are acting out of turn —
stop; the other session should handle it.

A **409** means the event did not fit the current state (out of order, or it would break
the verifier≠fixer rule). Read the message; do not force it.

## A note without a turn — `/note`

When something is worth telling the human (or the other session) but there is no FSM
event to report — "done, but nothing to issue" one-liners like an environment quirk or
a config caveat — post it without moving any state:

```bash
curl -sS -X POST localhost:8787/note \
  -H 'content-type: application/json' \
  -d "{\"session_id\":\"$SID\",\"message\":\"...\"}"
```

It only updates `last_message` in `GET /state` (prefixed with your session id); it wakes
nobody. Anything that must reach a session's long-poll still goes through `/done`.

## WIP snapshot (reviewer, at verify start)

When `verify` starts, triad-plan takes the review snapshot
(`git add -A && git commit -m "wip: review snapshot ..."` via `triad exec`). Report that
SHA so rollback can target it (design §4.4):

```bash
curl -sS -X POST localhost:8787/snapshot \
  -H 'content-type: application/json' -d "{\"sha\":\"$SNAPSHOT_SHA\"}"
```

triad records the SHA; it never runs the commit or the reset itself — this session does
(design §10).

## Stopping

- `await-human`: the cycle reached the post-close human gate. The user approves it out
  of band (`POST /gate`); this session just waits (long-poll returns nothing until the
  next cycle). Do not try to advance it yourself. A note the reviewer attached to its
  `close` /done is carried across the gate to the next cycle's worker, so the reviewer
  can leave a heads-up for the next round even though close wakes no one directly. The
  user can also add/override it: `POST /gate` with `{"message":"..."}`.
- A 409/422 you cannot resolve, or an unknown instruction: stop and surface it to the
  user rather than looping.
