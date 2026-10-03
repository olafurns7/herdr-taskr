# taskr

taskr is a task ledger and a blocking `wait` for orchestrator agents that delegate
work to workers in [Herdr](https://herdr.dev) panes. It ships as a Go binary,
stores coordination state in SQLite, and includes a read-only dashboard.
Herdr runs your agents; taskr tracks their questions, reports, and progress.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/img/dashboard-dark.png">
  <img src="docs/img/dashboard-light.png" alt="Mock taskr dashboard with an owner question, a stalled lane, waiting work, and a five-lane release campaign across two hosts.">
</picture>

Mock data.

## Why use it?

Keep parallel work moving without polling terminal screens or relaying messages:

- Register lanes with brief and report paths.
- Keep the text of briefs, prompts, reports, and handovers in the ledger, with a goal and a plan for each campaign.
- Block on `wait` until a report, question, or event arrives.
- Confirm prompt acknowledgment with receipts.
- Answer worker questions and route owner decisions.
- Catch turns that stop while work is owed, using optional hooks.
- Scan questions, campaign progress, and activity in the dashboard.
- Coordinate across machines with an optional Tailscale hub.
- Skip taskr if you run one agent at a time or do not use Herdr.

Agents still do the work and write reports to files. Receipts confirm
acknowledgment; stall signals prompt investigation rather than proving failure.

## How it works

Herdr runs the agents; taskr records coordination state and wakes the orchestrator.
Hook receipts and stall signals require the optional agent hooks.

```mermaid
sequenceDiagram
    participant Orchestrator
    participant Ledger as taskr (ledger)
    participant Herdr
    participant Worker
    Orchestrator->>Ledger: new worker --parent ORCH
    Orchestrator->>Ledger: launch WORKER (record identity)
    Orchestrator->>Herdr: herdr agent start (with launch IDs)
    Orchestrator->>Ledger: prompt WORKER --file brief.md
    Ledger->>Herdr: Deliver prompt with receipt attempt
    Herdr->>Worker: First taskr got ATTEMPT, read brief
    Worker->>Ledger: got ATTEMPT (prompt hook receipt)
    Orchestrator->>Ledger: wait --as ORCH --for ready,ask,done,herdr
    alt Worker reports
        Worker->>Ledger: ready / ask / done
        Ledger-->>Orchestrator: wait wakes with event
        Orchestrator->>Ledger: answer ASK_ID TEXT / ack EVENT --as ORCH
    else Turn ends while still owing work
        Worker->>Ledger: Stop hook records herdr event (reason=stall)
        Ledger-->>Orchestrator: wait wakes with stall signal
    end
```

## Install

Paste this into Claude Code or Codex running inside Herdr:

```text
Install taskr for Herdr: fetch https://raw.githubusercontent.com/olafurns7/herdr-taskr/master/docs/install.md and follow it step by step. Ask me before changing any hook or agent settings.
```

Prefer to do it by hand? The [runbook](docs/install.md) is plain shell.

## Status and license

Early software; interfaces and install details may change.
Licensed under [MIT](LICENSE).
Source: [olafurns7/herdr-taskr](https://github.com/olafurns7/herdr-taskr).
