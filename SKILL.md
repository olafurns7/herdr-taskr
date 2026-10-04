---
name: taskr
description: Report progress/questions/completion and orchestrate Herdr campaigns through the task ledger.
---

# taskr

One ledger: a host with ~/.local/state/taskr/server.url and no TASKR_DB is a client; every command goes to the server's ledger, none is local. A host without server.url keeps its own ledger until moved over; TASKR_DB forces local. Parent supplies TASKR_TASK/TASKR_LAUNCH; never replace them. Root uses --as ID. Compact default; --json or TASKR_FORMAT=json pins legacy JSON. Handover/adopt: Markdown.

Load: [format](references/format.md) before decoding output; [orchestrator](references/orchestrator.md) before orchestration, planning or handover/adopt; [recovery](references/recovery.md) when a command fails, an alarm arrives or a daemon needs a restart; [review](references/review.md) for a reviewer lane.

Worker (a HERDR-BRIEF prompt):
- `taskr got ATTEMPT` first, on every prompt that names one. A hook may record it first; your `got` then prints `dup`. Run it anyway.
- Report through the ledger: `ready` per finished slice, `ask --blocking` for a missing decision, `done` or `fail` at the end. Stop dependent work until the answer arrives. Honor the brief's Progress line.
- No commit, push, PR, issue-tracker write or agent start unless the brief grants it.
- Write the report to the brief's path, then reply with that path and three lines.
- Never `sleep` waiting for something. Use `taskr wait`, or `herdr pane wait-output` for a non-agent process.
```sh
taskr got ATTEMPT
taskr start                           # optional
taskr note "progress"
taskr ready "slice" --report PATH [--kv K=V]
taskr ask "question" --blocking [--owner]
taskr done "report"                   # or fail
```
Blocking ask: `taskr wait --for answer` (it uses TASKR_TASK/TASKR_LAUNCH; `--as` other than TASKR_TASK exits 6). Handle the answer to your ask, then `taskr ack EVENT --as $TASKR_TASK`. Timeout: wait again. Answers never reopen finished work. --key: idempotent; got: reserved.

Client outage: `got`, `ready`, `done`, `fail`, `decide`, `next`, `note` and `close` print `qd1 <request key>` and exit 0 when the server is unreachable; the client daemon sends the record when it returns. Do not retry or treat it as failure. Other commands still exit 5 with `retry with: taskr --request-key KEY ...`: rerun that exact line before any wait; never wait on an unstored ask. If killed, use the announce line's retry command.

`taskr spool ls` lists this host's queued, refused and bad files; `spool send` tries delivery now; `spool rm SEQ|FILE` removes one after inspection. A stuck head (401/403/408/429) holds the whole queue in order: fix this host's token or wait for the server to be free. A refused `outcome unknown` record: check `taskr log` and run again only if missing. Other refused records will not be sent again; the server's error says why. A bad file could not be read; inspect it, then `spool rm` it.

Documents are captured from the host with the file: briefs and `prompt --file` from the caller (the file must exist there), reports from the lane's host, goal/plan via `doc set --file` from the caller. No copy to the ledger host; `doc set` and `doc backfill` work from any host. See [Documents](references/orchestrator.md#documents).

Close a lane with `taskr close ID --outcome accepted|reworked|rejected|abandoned`: accepted = work taken as delivered; reworked = taken after a fix round; rejected = not taken; abandoned = stopped before a result. Omit the flag to record no outcome; `log` shows it in the closed event's data.

`taskr search QUERY [--root ID] [--kind K] [--limit N] [--raw]` searches latest captured documents and decision/ask/answer/note summaries (default 20, max 100; `--raw` uses FTS5 syntax):

```sh
taskr search "retry policy" --root 42
taskr search 'timeout OR outage' --kind decision --limit 10 --raw
```

Orchestrator loop: `taskr wait --as ID --for ready,ask,answer,owner_answer,done,fail,prompt_outcome,herdr --ack HANDLED`; omit `--ack` on the first wait; pass `--ack` only for a newly handled pending event, and after a timeout or an error that reports `acked_event_id`, do not repeat that successful ack. Matches replay until acked. One consumer per inbox. Herdr hints are never result proof: after `ready`, read the report and the diff, not the pane. Details: [orchestrator](references/orchestrator.md).

Exit 0 success or compact wait timeout (`w1 {...}` then `x1 3 timeout`); `x1 3 timeout unreachable` (no `w1`; JSON `"unreachable":true`) means the server was lost and counts are unknown: wait again, not `owed` 0. 2 usage; 3 interruption or legacy JSON wait timeout; 4 DB; 5 transport (server unreachable, Herdr delivery, `--confirm` no_receipt); 6 rejection (stale launch, closed task, launch on a root task, host mismatch, wait `--as` other than TASKR_TASK): stop. Errors may follow writes: keep ids, inspect `log`/`asks` and the agent before any resend; never answer twice. No server spawn/restart. Scratch: HOME + explicit TASKR_DB.
