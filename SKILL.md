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

Exit 5 `server unreachable; retry with: taskr --request-key KEY ...` on any write (`ask`, `ready`, `done` ...): the write may not be stored. Rerun that exact line, never a fresh command, before you wait on anything. Never wait on an ask that was not stored. During an outage a client write may block up to 60 s; if killed, rerun the announce line's `retry with:` command.

Orchestrator loop: `taskr wait --as ID --for ready,ask,answer,owner_answer,done,fail,prompt_outcome,herdr --ack HANDLED`; omit `--ack` on the first wait; pass `--ack` only for a newly handled pending event, and after a timeout or an error that reports `acked_event_id`, do not repeat that successful ack. Matches replay until acked. One consumer per inbox. Herdr hints are never result proof: after `ready`, read the report and the diff, not the pane. Details: [orchestrator](references/orchestrator.md).

Exit 0 success or compact wait timeout (`w1 {...}` then `x1 3 timeout`); compact timeout without `w1` means counts unknown (outage): wait again, not `owed` 0. 2 usage; 3 interruption or legacy JSON wait timeout; 4 DB; 5 transport (server unreachable, Herdr delivery, `--confirm` no_receipt); 6 rejection (stale launch, closed task, launch on a root task, host mismatch, wait `--as` other than TASKR_TASK): stop. Errors may follow writes: keep ids, inspect `log`/`asks` and the agent before any resend; never answer twice. No server spawn/restart. Scratch: HOME + explicit TASKR_DB.
