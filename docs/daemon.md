# The daemon and the taskr hub

`taskr daemon` is a small background process, one per user on each machine.
The Herdr plugin starts it; you do not run it by hand. It does two jobs:

1. **Event bridge.** It listens to Herdr and wakes `taskr wait` when an
   agent's state changes, notifies you of blocking owner asks, and writes a few
   tokens on panes and workspaces so other Herdr plugins can show taskr's state.
2. **Hub server.** It serves the ledger to other machines' `taskr` commands
   over one HTTP route, `/api/rpc`. It serves no web page: you read the ledger
   with [`taskr-tui`](tui.md) and the [CLI](cli.md).

## Optional check-ins

`TASKR_CHECKIN=1` enables the Rust hub's check-in sweep, read once at daemon
start; unset or any other value leaves it off. `TASKR_FROZEN_NOW` fixes the
clock only in contract test builds; production builds ignore it.

The sweep nudges idle or done local leads to process lane results after 30
minutes (R1), or to say why with a root note, park, or close after 90 minutes
without a lane event or ordinary prompt (R2). R2 allows only closed, planned,
done or failed lanes; an open lane blocks it even when its agent is idle.
It also requires no open owner ask. Root replies and check-in prompts do
not restart the silence timer. A parked, waiting, working or blocked lead is
never prompted. Nudges identify themselves as automatic check-ins, not
owner instructions, and do not grant authority to park or close. Delivery
does not wait for activity or require a receipt: one nudge per pass and
anchor, with at least 60 minutes between a root's nudges.

The latest R1 nudge gets one owner notification after 60 minutes if its
anchor is still unacknowledged, even when newer results keep arriving. An
unchanged R2 anchor gets one notification after 60 minutes unless the lead
has replied with a root note or `next`. Remote leads get the notification at
the rule threshold, with no prompt, only when their host heartbeat is fresh
(younger than 30 seconds), with at least 60 minutes between a remote root's
escalation claims across both rules. R1 notification ages count from the
oldest unprocessed result.
The same switch also enables one notification for a non-blocking owner ask
open at least four hours on an asker that is not closed, including asks on
client hosts. Blocking owner asks always notify immediately. Check-ins never answer asks or change task
status.

## PR events

The ledger daemon (the hub, or a host with its own ledger) can follow linked
GitHub pull requests and tell the lead when one changes. It is off unless
`~/.local/state/taskr/watch.json` turns it on:

```json
{"github": {"enabled": true, "default_repo": "example-org/example-repo"}}
```

The file is read at each poll, so no restart is needed. Link a PR with
`taskr set TASK pr=31` (the default repo) or `pr=owner/repo#31`, and unlink it
with `pr=`. Without `default_repo`, only `owner/repo#N` links are followed.

- **Read-only, through `gh`.** Every 60 seconds one batched `gh api graphql`
  query (as the hub user, so taskr never holds a token) reads at most 30 linked
  PRs, rotating through the rest; past 20 PRs the interval stretches to 60 s
  per 20. `gh` runs in a worker thread with a 20-second deadline; the result is
  applied in one short ledger transaction, never across the network call.
- **What is followed:** PRs whose linked task is not closed, or that an
  unfired `taskr after pr:` subscription of an open root targets, until they
  merge or close. A task linked later to a merged or closed PR gets its refs from
  the cached state, with no query and no event.
- **Events:** kind `pr` (compact code `pu`), on change only, to the linked
  task's lead (a linked root gets its own); a closed lead's inbox is skipped.
  Subs: `checks_green`, `checks_failed` (each after two polls that agree, over
  the required checks, or all checks when none is required; more than 50
  checks, or a check GitHub could not return in full, is no verdict), `dirty`,
  `behind`, `blocked`, `thread_opened`, `threads_clear`, `merged`, `closed`.
  More than 50 review threads, or a thread GitHub could not return, keeps the
  last thread count.
  A PR GitHub reports as not found gets `closed` with a `reason` and is
  unlinked. Data: `pr`, `sub`, `head`, `checks`, `merge_state`,
  `threads_open`, plus `merge_commit` or `reason`. An unacked older `pr`
  event is skipped when a newer one for the same PR is in the same inbox.
  `pr` events have no launch, are not searchable, send no notification, and
  are not campaign activity (glance, park, R2) except `merged`.
- **Refs:** the poller keeps the task's `pr.state` (`open`, `merged`,
  `closed`) and `pr.ci` (`pass`, `fail`, `running`) refs current, so
  `taskr campaign` and the TUI's PR panel show live state. A new head or a
  newly linked PR clears `pr.ci` until it has a verdict. Its refs carry
  `"source":"github"` in their data and do not count toward a task's 20
  references.
- **Failures:** JSON on stdout is used whatever `gh`'s exit code. With no
  JSON or `data: null` the poller backs off 1, 2, 4, 8 then 15 minutes; `gh`
  exit 4 (not authenticated) retries every 15 minutes. `daemon.log` gets one
  line per change of state.

The last-seen state of each PR lives in the ledger's `meta` table as
`pr_state:owner/repo#N`. Wait for these events with `taskr wait --for pr`.
A root in another tree can follow a PR with `taskr after pr:owner/repo#N --as ROOT`
([cli.md](cli.md)); while that subscription is unfired, the PR stays polled
even after its linked task closes.

## Three kinds of host

| Kind | Set up by | What it does |
| --- | --- | --- |
| Local | Nothing; this is the default. | Keeps its own ledger in `~/.local/state/taskr/taskr.db`. The server listens on `127.0.0.1:7788` only. |
| Hub | `tailnet` in `~/.local/state/taskr/dashboard.addr` | Keeps the one ledger and also listens on its Tailscale address, so client hosts can reach it. |
| Client | The hub's URL in `~/.local/state/taskr/server.url` | Keeps no ledger. Every `taskr` command goes to the hub. |

A single machine needs no setup. For several machines, pick one hub and make
the others clients: [install.md, step 7](install.md#7-multiple-machines).

## How a command finds the ledger

In this order:

1. `TASKR_DB` is set: use that file, locally.
2. `~/.local/state/taskr/server.url` exists: this is a client host; send the
   command to the hub named there. Nothing falls back to a local ledger when
   the hub is unreachable.
3. Otherwise: the local ledger, `~/.local/state/taskr/taskr.db`.

`help`, `version` and `daemon` always run locally. `taskr-tui` finds the
ledger the same way, because it runs `taskr` (from `PATH`, or the binary
`TASKR_BIN` names).

## The hub

The file is called `dashboard.addr` for historical reasons: the daemon once
served a web dashboard on this address. It now sets only where the hub server
listens.

| `dashboard.addr` | The server listens on |
| --- | --- |
| absent or empty | `127.0.0.1:7788` |
| `tailnet` | `127.0.0.1:7788` and this machine's Tailscale addresses, same port |
| `tailnet:PORT` | The same, on `PORT` |
| `127.0.0.1:PORT` or `[::1]:PORT` | That loopback address |
| `off` | Nothing |

Any other address is refused: the server binds only loopback and the tailnet.

Who may call the hub: a request from the tailnet is admitted only if
`tailscale whois` says the node belongs to the same Tailscale user as the hub
and has no tags. Other nodes get 403. Traffic is plain HTTP inside the
encrypted tailnet.

On Linux, run the hub's daemon at boot with `taskr daemon --stay`, which keeps
serving while Herdr is down:
[install.md, step 4](install.md#start-the-ledger-host-at-boot-linux).

## Client hosts

A client's `server.url` holds one line such as
`http://hub.example.ts.net:7788`. It must be a tailnet address or a MagicDNS
name of your tailnet.

The client's daemon sends its own Herdr panes to the hub every five seconds,
shows owner-ask notifications, and publishes the pane tokens the hub returns.
`taskr prompt` on a client delivers prompts to that client's own lanes.

When the hub cannot be reached, the report commands (`got`, `ready`, `done`,
`fail`, `decide`, `next`, `note`, `close`) are queued in
`~/.local/state/taskr/spool/` and sent when it returns. Other commands exit 5
with a line that says how to retry. `taskr spool ls` shows the queue.

Queued reports also stay queued when the hub replies `database is locked`.
The next send pass retries the same request key. Other database errors are
refused. `_hook` records remain best-effort: a busy hook can be dropped.

## Status and restart

```sh
taskr daemon --status    # what is running
taskr daemon --restart   # stop this host's daemon and start the installed binary
```

`--restart` restarts taskr, never Herdr. The most useful status fields:

| Field | Meaning |
| --- | --- |
| `running`, `pid` | Whether a daemon holds the lock. A successful exit alone does not mean one is running. |
| `running_version`, `stale` | The running daemon's version; `stale: true` means it is older than the installed binary, so restart it. |
| `daemon` | `fresh` when the daemon has a live Herdr connection; `stale` or `none` otherwise. |
| `role` | `local`, `hub`, or `peer`. On a client the field is `mode: client` with `server`, `last_call_at` and `last_error`. |
| `dashboard`, `dashboard_url` | The hub server on loopback: `up`, `down`, `off` or `refused`, and its URL. The names are historical. |
| `tailnet_url` | On a hub: the URL clients put in `server.url`. |
| `dashboard_usage` | Hourly request counts of the server. |
| `stay`, `supervised` | Whether it runs with `--stay`, and under systemd. |

`role: peer` and `hub.url` belong to an older setup in which each machine kept
its own ledger and pushed a copy to the hub's web page. The page is gone and
nothing is pushed. If a host still has a `hub.url`, make it a client with
`server.url` instead; [references/recovery.md](../references/recovery.md)
covers moving an existing ledger.

## Files

Everything lives in `~/.local/state/taskr/`:

| File | What |
| --- | --- |
| `taskr.db` | The ledger (SQLite). Absent on a client. |
| `daemon.log` | The daemon's log, capped at 1 MB. |
| `daemon.lock` | Keeps the daemon single; holds its pid. |
| `dashboard.addr` | Where the hub server listens (see above). |
| `server.url` | Makes this host a client. |
| `watch.json` | Turns on [PR events](#pr-events) on the ledger daemon. |
| `spool/` | A client's queued records. |

## Pane and workspace tokens

For authors of Herdr plugins. The daemon publishes:

- `taskr_state` and `taskr_round` on the hub's panes;
- `taskr_owner_ask=<n>` on a pane while its task has n unanswered owner asks
  (cleared at 0 or when the task closes), on every host;
- `taskr_campaign`, the campaign's name, on each task workspace, and
  `taskr_parent`, the ID of the workspace of the campaign's orchestrator, when
  that orchestrator's pane is open on the same host. The orchestrator's own
  workspace gets neither.

A client daemon publishes these from the hub's reply; an older or unreachable
hub leaves the client's tokens unchanged.
