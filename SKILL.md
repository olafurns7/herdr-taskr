---
name: taskr
description: Report progress/questions/completion and orchestrate Herdr campaigns through the task ledger.
---

# taskr

One ledger: a host with ~/.local/state/taskr/server.url and no TASKR_DB is a client; every command goes to the server's ledger, none is local. A host without server.url keeps its own ledger until moved over; TASKR_DB forces local. Parent supplies TASKR_TASK/TASKR_LAUNCH; never replace them. Root uses --as ID. Compact default; --json or TASKR_FORMAT=json pins legacy JSON. Handover/adopt: Markdown.

Load: [format](references/format.md) before decoding output; [orchestrator](references/orchestrator.md) before orchestration, planning or handover/adopt; [recovery](references/recovery.md) when a command fails, an alarm arrives or a daemon needs a restart; [review](references/review.md) for a reviewer lane.

Worker (a HERDR-BRIEF prompt): `prompt --file` to an implementer, researcher or reviewer lane appends the worker contract (got first, ledger reports, no unauthorized commit/push/PR/agent start, report path, no sleep, exit codes); workers follow it. Load this skill when the brief names it or a taskr command fails. Commands:
```sh
taskr got ATTEMPT
taskr start                           # optional
taskr note "progress"
taskr ready "slice" --report PATH [--kv K=V]
taskr ask "question" --blocking [--owner]
taskr done "report"                   # or fail
```
Blocking ask: `taskr wait --for answer` (it uses TASKR_TASK/TASKR_LAUNCH; `--as` other than TASKR_TASK exits 6). Handle the answer to your ask, then `taskr ack EVENT --as $TASKR_TASK`. Timeout: wait again. Answers never reopen finished work. --key: idempotent; got: reserved.

Root only: owner notes (`note --owner`) and `taskr notes`: see [orchestrator](references/orchestrator.md).
Every current owner action is an owner ask, blocking only when it stops work;
notes summarize context and link ask IDs. Glance red means open owner asks
only; sound is blocking-only, badges count all. `set ROOT glance.state=parked`
(and `glance.state=` to resume) requires TASKR_TASK and TASKR_LAUNCH unset, on the root's host, and a root target.
Parking suppresses coordination alarms, keeps asks red, and turns amber if
new tree events appear. The clear verdict is "no owner action".

Client records: `got`, `ready`, `done`, `fail`, `decide`, `next`, `note` and `close` print `qd1 <request key>` and exit 0 when the server is unreachable or earlier records wait in this host's spool. A full or unwritable spool: exit 5 with `retry with:`; rerun that line. Delivery: a running client daemon after a pass that reaches the server, or `taskr spool send` on that host; without a daemon they stay queued until manual send. Do not retry a queued record or treat it as failure. Other commands still exit 5 with `retry with: taskr --request-key KEY ...`: rerun that exact line before any wait; never wait on an unstored ask. If killed, use the announce line's retry command.

`taskr spool ls` lists this host's queued, refused and bad files; `spool send` tries delivery now on a client host only; `spool rm SEQ|FILE` removes one after inspection. A stuck head (401/403/408/429) holds the whole queue in order: fix this host's token or wait for the server to be free. A refused `outcome unknown` record: check `taskr log` and run again only if missing. Other refused records will not be sent again; the server's error says why. A bad file could not be read; inspect it, then `spool rm` it.

Documents are captured from the host with the file: briefs from the caller (a non-planned, non-gate `new` refuses a missing file), `prompt --file` from the caller (must exist there), reports from the lane's host when that host runs the command, goal/plan via `doc set --file` from the caller. No copy to the ledger host; `doc set` and `doc backfill` work from any host. See [Documents](references/orchestrator.md#documents).

Close a lane with `taskr close ID --outcome accepted|reworked|rejected|abandoned`: accepted = work taken as delivered; reworked = taken after a fix round; rejected = not taken; abandoned = stopped before a result. Omit the flag to record no outcome; `log` shows it in the closed event's data.

`taskr campaign ROOT [--page N] [--all]` reads goal, plan, lanes, asks, decisions, document metadata, stored PR refs and a 100-event log page. `--all` includes closed lanes; bodies load through `doc get`. Client reads are fresh RPC. Glance includes host/pane targets and sparklines; focus only when row host equals caller_host (both empty on the hub). A new orchestrator without a pane is told to adopt its root with `--pane` as its first act.

`taskr search QUERY [--root ID] [--kind K] [--limit N] [--raw]` searches latest captured documents and decision/ask/answer/note summaries (default 20, max 100; `--raw` uses FTS5 syntax):

```sh
taskr search "retry policy" --root 42
taskr search 'timeout OR outage' --kind decision --limit 10 --raw
```

Orchestrator loop: `taskr wait --as ID --for ready,ask,answer,owner_answer,done,fail,prompt_outcome,herdr --ack HANDLED`; omit `--ack` on the first wait; pass `--ack` only for a newly handled pending event, and after a timeout or an error that reports `acked_event_id`, do not repeat that successful ack. Matches replay until acked. One consumer per inbox. Herdr hints are never result proof: after `ready`, read the report and the diff, not the pane. Details: [orchestrator](references/orchestrator.md).

Exit 0 success or compact wait timeout (`w1 {...}` then `x1 3 timeout`); `x1 3 timeout unreachable` (no `w1`; JSON `"unreachable":true`) means the server was lost and counts are unknown: wait again, not `owed` 0. 2 usage; 3 interruption or legacy JSON wait timeout; 4 DB; 5 transport (server unreachable, Herdr delivery, `--confirm` no_receipt); 6 rejection (stale launch, closed task, launch on a root task, host mismatch, wait `--as` other than TASKR_TASK): stop. Errors may follow writes: keep ids, inspect `log`/`asks` and the agent before any resend; never answer twice. No server spawn/restart. Scratch: HOME + explicit TASKR_DB.
