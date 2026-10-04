# taskr recovery

Load this when a taskr command fails, a `no_receipt`, `model_capacity`, quota or `stall` event arrives, a lane must be resumed, or a daemon needs a restart. Normal commands: [orchestrator.md](orchestrator.md). Output: [format.md](format.md).

## Exit 5 and request keys

- Client records (`got`, `ready`, `done`, `fail`, `decide`, `next`, `note`, `close`) print `qd1 <request key>` and exit 0 when unreachable: queued for later delivery, not a failure; do not retry. Other commands still exit 5 with the `retry with:` line and retry transient transport errors themselves: `wait` until its timeout; other commands for 60 s (or their longer RPC budget), with the same request key. One stderr line gives the retry deadline and non-wait retry command; no local fallback.
- Transport exit 5 is a transport failure that could not be retried (5xx, non-JSON, too large, unverified), the end of the window, or an interrupted write: `x1 5 ... server unreachable; retry with: taskr --request-key KEY ...`.
- If a write was killed or interrupted, rerun the `retry with:` command from its announce line or exit-5 line (same key) before anything else.
- For an ordinary command, rerun that exact line (same key) before anything else, including any wait. The server keeps each answer for 7 days by key and returns it again instead of running the command twice.
- A key still running is retried within the same window; exit 5 "outcome unknown" after it ends: rerun the same line later. The same key with other arguments, or from another host, exits 6.
- Reads and `wait` are never stored: rerun them plain.
- A worker's `ask` that exited 5 and was not retried is not in the ledger: no answer will come. Retry it before you wait.
- On a client host, the `prompt` relay uses fresh internal keys, ignores `--request-key` and is not retried automatically. On exit 5, keep any attempt id and check `taskr log` for the prompt event and the agent before resending.
- `--request-key` is client mode only (exit 2 locally).
- Errors can follow writes: keep returned ids and inspect `log`/`asks` and the agent before any resend. Never answer twice.

## After the server was unreachable

Check `taskr spool ls` on each client host, handle its states below, arm `wait` again, then run `taskr status --tree <root>`.
For each open lane whose agent is idle or done with no `ready`, `done` or `fail` after the outage began, read its report file and pane: the report command may never have reached the server.
Rerun a command that printed a `retry with: taskr --request-key …` line exactly as printed, from the lane's own pane.
Hook records also queue locally and are sent by the client daemon.

## Spool

The client daemon sends `<state dir>/spool/` in order after a successful pass. `taskr spool ls` lists queued/refused/bad files; `taskr spool send` tries now; `taskr spool rm SEQ|FILE` removes one after inspection.

- A stuck head (server 401/403/408/429) holds the whole queue in order. Fix the host's token or wait for the server to be free; it remains queued.
- A refused record marked `outcome unknown` had no final answer for 10 minutes. Look for it with `taskr log`; run the command again only if it is missing.
- Other refused records will not be sent again; the server's error says why. Resolve that error before issuing corrected work.
- A bad file could not be read. Inspect it, then remove it with `spool rm`.

## Receipt alarms

- `prompt_outcome` `no_receipt` (data `outcome`, `window_ms`, `async:true`): the lane did not run `got` within the deadline. It is written on a daemon pass or at your next `wait`. Run `taskr log ID` for that attempt's `got`, then `herdr agent read`. Resend only when the prompt is shown as never received, and never while the pane shows work.
- `late_receipt` (data `got_event_id`, `no_receipt_event_id`, `delay_ms`): the `got` arrived after the alarm; the lane is working. Ack it; send nothing.
- A `--confirm` timeout exits 5 `no_receipt`, `gok=false`, and writes the alarm to no inbox; a later `got` still adds `late_receipt` to the parent inbox.
- `rejected` or `delivery_unknown` (exit 5): keep the attempt/outcome ids and inspect `herdr agent get/read` before any resend. `rejected` disarms the deadline; `delivery_unknown` keeps it.
- A relaunch or close of the lane drops its armed deadlines.

## Bypass events

`herdr` events with `reason=model_capacity`, `reason=stall` or a `quota` field pass every `--for`/`--from` and replay until acked.

- `stall` (hooked lanes): the lane's turn ended while it owed a result (no done, fail or ask after the newest prompt). `last` is the newest report code (`r`, `d`, `f`, `q`); `error` gives the turn error code, or `unknown` if the harness gave none. Read the report path and the pane, then prompt or resume. One per attempt. Plain stalls are skipped for orchestrator and sub-orchestrator lanes; error stalls always emit. On the server host a Codex error turn also yields `stall` from its rollout tail; client-host lanes get none (see [agent hooks](../docs/install.md#6-agent-hooks)).
- `quota` (`--scan-quota`): `quota` is `limit` or `low`, with `percent`. Follow the host's quota policy for new work.
- `model_capacity` (Codex lanes): a historical observation. Verify the current launch, provider/name/pane/terminal/session and the pane tail, record a disposition, then ack. Recovered: ack it as stale. Still halted: report model capacity and take only owner-authorized next steps. Never automatically prompt, retry, switch model, rotate account or notify the phone. `model` is registered launch metadata and can be stale after a manual switch.

Capacity scan: every positive-timeout parent wait scans active registered Codex children before backlog delivery, on a shared 15 s claim, even with a fresh daemon; `--timeout 0` is inbox-only. Direct local socket requests are capped at 500 ms/256 KiB with a 2 s scan budget and a persisted cursor. Gates, ready/done/failed/planned/closed tasks, waiting children and unregistered panes are excluded. Socket failure or ambiguous identity means no diagnosis, not recovery. Task, liveness and quota state is unchanged.

Capacity identity: bound before Codex prompt transport, else from the recorded native session or the exact latest taskr prompt marker in the current submitted user cell. Only `thread_id` and the Herdr `id` for the same nonempty native value are accepted; replacements, changed identities, stale launches and older revisions abstain. Positive later user/output activity clears the episode latch; blank, clipped or unreadable snapshots do not. Recovery and recurrence between two scans can be missed. Text cannot authenticate a byte-identical forged warning.

## Resume after a Herdr restart

Read the full log and recorded native home/session; verify the owned pane and topology. The user chooses the agent, model and optional account wrapper. `launch ID ... --workspace W --tab T --pane P` registers a new launch and location (omitted flags keep the location). Re-export TASKR_TASK/TASKR_LAUNCH, resume the user-selected agent in the recorded native home/session, then `prompt ID --text "Continue where you left off. Report path: PATH"`. Never adopt by pane name; a missing home or session blocks resume. Old launch writes are rejected. A Herdr handoff that keeps processes keeps launches: do not re-register.

## Daemon

The owner restarts daemons; a worker never does. `taskr daemon --status` reports freshness, version, pid, socket, dashboard URL/role, peer push state and `dashboard_usage`; `stale: true` means an older daemon still runs.

- `taskr daemon --restart` (local and client mode) stops only the daemon whose recorded pid, executable, argv, start time and uid match, waits ≤10 s, and starts this binary detached with only HOME/PATH/HERDR_SOCKET_PATH, preserving the recorded daemon arguments (including `--stay`). An unknown identity exits 6 and signals nothing.
- A local daemon is recorded as supervised only when `INVOCATION_ID` is set and `SYSTEMD_EXEC_PID` equals its own pid. `--restart` stops it, then waits up to 10 s for a verified replacement with a new process start time. If none appears and the lock is free, it starts a detached replacement and reports `started_detached: true`. `daemon --status` shows `stay` and `supervised`; the detached fallback is not supervised.
- With no lock holder, `--restart` starts plain `daemon`; a stopped stay daemon does not return with `--stay`.
- After `started_detached: true`, or while a plugin daemon holds the lock, the unit's daemon waits idle and the serving daemon is not supervised. `--restart` starts another detached daemon. To give the service back to the unit, get the lock holder's pid from `taskr daemon --status` and end it with `kill <pid>`; the unit's waiting daemon takes over.
- `daemon: none` or `stale` with `running: true, stay: true` is normal while Herdr is down. A stay daemon logs a missing `herdr` executable at startup and shows `herdr_missing: true` in `--status`; it serves the ledger but sends no owner notifications, writes no tokens and observes no panes until it is restarted with a PATH that holds `herdr`. Ensure its PATH also holds `tailscale` before restarting it.
- A stale local identity file can make `--restart` exit 6 while an older daemon holds the lock. The error names the file, normally `~/.local/state/taskr/daemon.json`. Check the holder's pid with `taskr daemon --status`, remove only that stale identity file (`rm ~/.local/state/taskr/daemon.json`), then run `taskr daemon --restart` again; the ledger's identity checks still apply.
- Client mode (server.url): `--status` shows `mode: client`, `server`, `last_call_at`, `last_error`, `pid`, `running_version`, `stale`.
- Upgrade from v0.10, once per client host: the old client daemon has no identity record, so `--restart` exits 6 and `--status` shows `running_version: unknown`. Stop it by its `pid` from `--status` (`kill PID`); the Herdr plugin starts the new one.
- The Herdr plugin restarts a killed daemon within seconds. To move a host to client mode, write `server.url` before you stop its local daemon (see [multiple machines](../docs/install.md#7-multiple-machines)).
- No Herdr server running: `wait` skips observations; `daemon --once` exits 5. Log: ~/.local/state/taskr/daemon.log, capped at 1 MB.
