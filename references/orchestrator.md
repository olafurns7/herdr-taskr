# taskr orchestration

Herdr owns agent panes and teardown; users choose agents, models and optional account wrappers. taskr owns the ledger. Every participant uses the same ledger. Reports and reasoning stay in files; plan, questions and handover stay in the ledger. Read [format.md](format.md) for output. On a failure or an alarm, load [recovery.md](recovery.md).

Register root once: `new NAME --role orchestrator` returns task id; use `--as ID` on wait/ack/note/ask/decide/handover. Root `--as` writes require no parent or launch and no different TASKR_TASK. Sub-orchestrator was registered by parent: use TASKR_TASK and TASKR_LAUNCH for writes. `launch` requires a task created with `--parent`.

Write the brief file first: a non-planned, non-gate `new` with `--brief` refuses a missing file (exit2). After creating a helper tab per Herdr policy, register `new NAME --parent ID --role ROLE --workspace W --tab T --pane P --cwd C --brief B --report R`, then `launch ID --provider P --model M --effort E`. Export returned TASKR_TASK/TASKR_LAUNCH in that pane before starting the user-selected agent with `herdr agent start`; account wrappers are optional. Role: orchestrator, sub-orchestrator, implementer, reviewer, researcher, gate. Names are not identity. Old launch writes are rejected.

Hosts: tasks and launches carry a machine. `new`/`launch --machine H` takes the server, the caller, or a client host with a fresh daemon. `prompt` reaches lanes on your host or the server host only; a lane on a third host exits 5 with no attempt: start a sub-orchestrator on that host. A lane whose host daemon has been silent for 30 s shows `unknown`, not `missing`.

Prompt:
- `prompt ID --file PATH` sends `First taskr got N; read PATH; execute exactly.`; `--text TEXT` sends `First taskr got N. TEXT`. It returns at once after Herdr sees activity: `p1 {...,"due":300000}`.
- It arms a 300000 ms receipt deadline. `--receipt-timeout MS` sets it: 0 disarms, 1-59999 exits 2. A root target arms none.
- No `got` by the deadline: a `prompt_outcome` `no_receipt` event arrives in your inbox. A `got` after it adds `late_receipt`. Handle both per [recovery.md](recovery.md).
- `--confirm [--confirm-timeout MS]` blocks for the `got` (default 60000; exit 5 `no_receipt` on timeout). Use it only when you must block. Never use a confirm window under 60000.
- `rejected` or `delivery_unknown` exits 5: keep the attempt id and inspect the agent before any resend. No reachable Herdr socket: no attempt, exit 5. Never start/restart Herdr from an agent. No automatic resend.

Collect: `wait --as ID --for ready,ask,answer,owner_answer,done,fail,prompt_outcome,herdr [--from NAME|ID]... [--ack HANDLED] [--timeout MS] [--scan-quota]`.
- Default timeout 540000 ms. Keep it. Never loop short waits, never `sleep`, never background a wait with `&`.
- Codex orchestrators too: Codex 0.159 runs a 540000 ms `taskr wait` to the end, yielding about every 30 s while the call stays alive; events arrive in about 1 s. A `&` job dies with its tool call.
- Pass `--ack EVENT` only after handling a newly returned pending event; the first wait has none. After a timeout, wait again without repeating the successful `--ack`. If a combined wait fails, keep `acked_event_id` and never repeat that successful acknowledgment. Standalone `ack EVENT --as ID` is idempotent.
- Kinds are long names, comma-separated or repeated; several `--from` are OR; kind and source are AND. Prefer ids to names.
- Nonmatches are acked, kept in the log, and counted in `sk1 N`. A filtered wait can skip an ask or fail you exclude: choose filters deliberately.
- These bypass `--for`/`--from` and replay until acked: `herdr` with `reason=model_capacity` (Codex lanes), with `quota` (`--scan-quota`), and with `reason=stall` (hooked lanes).
- A filtered wait also skips stale plain `herdr` hints (lane closed or relaunched, a newer hint, or a later ready/done/fail/ask from that launch) and a `no_receipt` whose `got` already exists, with its `late_receipt`. Unfiltered waits skip nothing.
- Timeout: compact prints `w1 {"owed":N,"due":K}` then `x1 3 timeout`, exit 0. `owed`: open children whose newest prompt has no later done/fail. `due`: armed receipt deadlines for you. With `owed` 0 you may end your turn. A compact timeout without `w1` means counts unknown (outage): wait again; it is not `owed` 0. Interruption exits 3.
- One consumer per inbox. Grandchildren report to their parent, not root.

Hints: `blocked` always emits. Other hints require owed work: before the first prompt and after done/fail they are silent. After `ready`, only `missing` emits, even with an armed receipt. Otherwise an armed receipt without a `got` suppresses hints. Hooked lanes drop `idle`, `unknown`, and `done` and send `stall` instead; orchestrator and sub-orchestrator lanes only emit stalls for turn errors. Keep `herdr` in `--for` for unhooked lanes. Never infer a result from a hint: after `ready`, read the report and the diff, not the pane.

Answer:
- `answer ASK_ID TEXT` records once in the asker's inbox; a second answer exits 6. `w` (asker_waiting) is a snapshot, not proof of a live process.
- `w` true: the asker's wait gets it; no transport.
- `w` false and transport is needed: inspect the agent, then `answer ASK_ID TEXT --prompt` (or, after a plain answer, `prompt ID --text "ask ASK_ID: TEXT"`). Its receipt deadline alarms the asker's parent, or the `--as` task.
- Retry transport with `prompt`, never answer again. Answers do not reopen finished work.

Owner asks: `asks --owner --open` lists them. Ask the owner in your pane, then record it with `answer ASK_ID TEXT` there (`--as`, if given, must name the ask's recipient, else exit 6). The dashboard is read-only: it has no answer controls. `--blocking` means dependent work stops; independent work can continue. `owner_answer` is a legacy event; its answer is already recorded: if `asker_waiting` is false and transport is needed, inspect, then `prompt <asker_task_id> --text "taskr answer to ask <ask_id>: <summary>"`; otherwise ack. Answers to root's own owner asks arrive as `answer`.

Resume a lane: register a new launch (`launch ID ... --workspace W --tab T --pane P`), re-export both ids, start the agent, then `prompt ID --text "Continue where you left off. Report path: PATH"`. Full recipe: [recovery.md](recovery.md).

Plan:
```sh
taskr new NAME --parent ID --role ROLE --planned
taskr next ID "next step"              # --clear removes; keep current at plan changes
taskr decide --as ROOT "owner rule"    # --revoke EVENT retires
taskr set ID pr=123 commit=HASH        # key= deletes
```
Planned lanes have no launch/observation/inbox, refuse worker writes, prompt, wait/ack; children under a planned ancestor cannot launch/write/prompt. Launch top-down. Close drops an unused plan. next wakes nobody; latest wins, log keeps history. Answered owner asks already count as decisions; do not copy them. Refs: ≤20 keys/task, `[a-z][a-z0-9_.-]{0,31}`, values ≤200 runes without controls, pins and pointers rather than prose.

`handover --as ROOT [--note TEXT] [--out PATH]` before session end or low context: Markdown identity/cwd/inbox/next/refs, decisions, live/planned lanes, open asks, lanes closed since previous handover, note. `adopt ROOT [--workspace W --tab T --pane P]` only after the predecessor stops: defaults from HERDR_* caller location, rebinds root, clears the old waiting marker, re-renders the latest handover, preserves the pending inbox. Then `wait --as ROOT`. Sub-orchestrator is relaunched by parent. After owned-tab teardown per Herdr policy, `close ID` changes ledger only.

Reads: `status [--tree ID] [--all]`; `asks [--open] [--tree ID] [--owner] [--limit N]`; `log ID [--tree] [--since EVENT] [--before EVENT] [--limit N]`. Compact reads are bounded: `status --tree` omits closed lanes (`--all` adds them); `log` prints the newest 100 events (`--before` pages back, `--since` forward); `asks` prints open asks plus the latest 20 answered; `--limit 0` lifts limit and cap. A trailer `m1 {...}` names what was left out and the cursor. Strict input: unknown flags and invalid values exit 2 before any write; relative CLI paths resolve to absolute; `CMD --help` exits 0. Status changes only through new/prompt (open), planned registration, launch, ready/done/fail/close.

Gate: register role gate, launch provider shell/model none/effort none, export ids; script ends `ready` with exit/failure counters, or `fail`. Gate tasks have no liveness/quota events; process exit alone is not gate proof.
