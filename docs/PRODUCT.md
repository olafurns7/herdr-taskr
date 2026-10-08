# Product

## Platform

Terminal. `taskr` is a command line and a daemon; `taskr-tui` is a full-screen
terminal view. There is no web interface.

## Stack

- `taskr`: a Go binary at the repository root, with a Rust implementation of
  the same CLI, hub and daemon in `crates/taskr` held to the same behaviour by
  the contract tests in `tools/contract/`. State is one SQLite ledger.
- `taskr-tui`: Rust and ratatui, in `crates/taskr-tui`. It reads through the
  `taskr` command (`taskr --json glance`, `taskr campaign`, `taskr doc get`)
  and writes only through `taskr answer` and `taskr set`.

## Users

Developers coordinating several agent orchestrators and worker lanes in Herdr,
on one machine or across machines. They are the owner: they look at
`taskr-tui` between tasks to see what needs them, what reached a milestone,
and where each campaign stands. Their agents use the CLI.

## Product purpose

taskr is a task ledger for Herdr orchestrators: one SQLite ledger on the taskr
hub, a CLI, and a daemon shipped as a Herdr plugin. Client hosts send their
ledger commands to the hub. The ledger holds what orchestrators and workers
record as they work: registrations, prompt receipts, reports, asks and
answers, decisions, next steps, references, documents and handovers.

## Architecture

One ledger on one hub. The hub's daemon (`dashboard.addr` = `tailnet`; the
file name is historical) serves `/api/rpc` on loopback and the tailnet. A
client host (`~/.local/state/taskr/server.url`, no `TASKR_DB`) sends its
ledger commands there without opening a local ledger; `help`, `version` and
`daemon` run locally. A client's daemon reports its own Herdr panes to the hub
and shows owner-ask notifications; its `prompt` command delivers prompts to
its own lanes. With the hub unreachable, the client records
got/ready/done/fail/decide/next/note/close queue in the spool (`qd1`, exit 0);
other commands fail with exit 5 and a retry line; nothing falls back to a
local ledger. Details: [daemon.md](daemon.md).

## Terms

Orchestrator (a root task), lane (a worker task under it), campaign (an
orchestrator and its lanes), owner ask, ready/done/fail, next, decision,
handover/adopt, ref, hub, client host.

## Principles

1. What needs the owner comes first and is unmistakable. Red means an open
   owner ask and nothing else; when nothing needs the owner, the view says so.
2. Milestones over noise: each campaign shows its latest checkpoint; the raw
   event log is one key away.
3. Compact and readable at a glance: aligned rows, explicit status marks with
   words, restrained colour, a 46-column layout for a split pane.
4. Truthful: every signal comes from the ledger or Herdr's observed state.
   Missing or old data is shown as stale, never guessed.
5. Few writes: the view answers an ask and parks a campaign, each through the
   CLI; everything else happens where the agents are.

## Evidence on hand

Every screen is drawn from the synthetic fixture `crates/taskr-tui/fixture.json`
and compared with the golden frames in `crates/taskr-tui/tests/golden/`;
`frames/` holds the same frames as text, ANSI and PNG. Do not depend on a live
host or campaign.
