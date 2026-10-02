# taskr

taskr is a task ledger and a blocking `wait` for orchestrator agents that
delegate work to worker agents in [Herdr](https://herdr.dev) panes. Herdr is
required. taskr ships as a Go binary, stores work in SQLite, and includes a
Herdr plugin that runs its event bridge and dashboard.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/img/dashboard-dark.png">
  <img src="docs/img/dashboard-light.png" alt="Mock taskr dashboard with an owner question, a stalled lane, waiting work, and a five-lane release campaign across two hosts.">
</picture>

Mock data.

## Why use it?

An orchestrator that starts workers in other panes needs to learn when each
worker finishes, asks a question, fails, or stops without reporting. Without a
ledger, it polls terminal screens or asks the owner to relay messages.

taskr provides:

- Registration, brief paths, and report paths in a SQLite ledger.
- A blocking `wait` that wakes when a worker reports or an event arrives.
- Prompt receipts: confirmation that the worker acknowledged a prompt.
- Questions and answers, including blocking questions and questions for the owner.
- Stall signals from optional agent hooks when a turn ends without a result.
- A read-only dashboard for questions, campaign progress, and activity.
- Optional operation across machines using Tailscale.

If you run one agent at a time, you probably do not need taskr. If you do not
use Herdr, this workflow is not for you. taskr records and delivers coordination
state; your agents still do the work and write their reports to files.

## How it works

Herdr runs the agents; taskr records their coordination state and wakes the
orchestrator when there is something to handle. Hook receipts and stall signals
require the optional agent hooks.

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
    Herdr->>Worker: First taskr got ATTEMPT; read brief
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

## Quick start: one machine

You need macOS or Linux on ARM64 or x86_64, **Herdr 0.9.0 or later**, and `gh`
or `curl`. Release downloads also need `tar` and either `shasum` or `sha256sum`.
You do not need Go or Node to run a release binary.

### Install

From a normal terminal, install the latest public release without signing in:

Avoid piping a download to a shell: a partial installer could run before the transfer failure is reported.

```sh
curl -fsSL -o taskr-install.sh \
  https://github.com/olafurns7/herdr-taskr/releases/latest/download/install.sh &&
  test -s taskr-install.sh &&
  TASKR_LINK_SKILLS=1 sh taskr-install.sh &&
  PATH="$HOME/.local/bin:$PATH" taskr version
```

Alternatively, use GitHub CLI:

```sh
# Download the installer through GitHub CLI, then install and link agent skills.
gh api repos/olafurns7/herdr-taskr/contents/install.sh \
  -H 'Accept: application/vnd.github.raw+json' > taskr-install.sh &&
  test -s taskr-install.sh &&
  TASKR_LINK_SKILLS=1 sh taskr-install.sh &&
  PATH="$HOME/.local/bin:$PATH" taskr version
```

The installer verifies the binary, skill, and plugin against the release's
`SHA256SUMS`. It uses authenticated GitHub access when available and otherwise
downloads public release assets with `curl`. See [docs/install.md](docs/install.md)
for options, including selecting a release tag.

```sh
# Make the default binary directory available in this shell.
export PATH="$HOME/.local/bin:$PATH"
```

Add that PATH line to your shell configuration if it is not already there.
The default locations and installer options are in [docs/install.md](docs/install.md).

### Link and verify

Run the plugin link inside an already-running Herdr session:

```sh
# Link the installed plugin to Herdr.
herdr plugin link "$HOME/.local/share/taskr/plugin"
```

The installer may already have linked it when run inside Herdr. The plugin starts
the taskr daemon at Herdr startup or the next agent detection; it does not start
Herdr itself. Its launcher uses `TASKR_INSTALL_DIR` when set, otherwise
`$HOME/.local/bin/taskr`.

```sh
# Check the installed binary.
taskr version
# Inspect the daemon, its running version, and dashboard state.
taskr daemon --status
```

If the daemon is not running yet, start it in a separate terminal:

```sh
# Run the taskr event bridge and dashboard in the foreground.
taskr daemon
```

The dashboard defaults to <http://127.0.0.1:7788/>. It is read-only: answer owner
questions in the orchestrator's pane. A successful status command alone does
not mean the daemon is running; check its `running` and `dashboard` fields.

### Your first loop (about five minutes)

Use two human-operated terminals in Herdr, in the same working directory.
This exercises the ledger without starting an agent. It assumes neither shell
has `TASKR_TASK` or `TASKR_LAUNCH` set. Use the IDs printed by your commands;
the numbers below are examples for an empty ledger.

In terminal A, register an orchestrator and a worker:

```sh
# Register the parent; n1 1 means task ID 1.
taskr new first-loop --role orchestrator
# Save the returned parent ID (replace 1 if needed).
ORCH=1
# Register a worker and its report destination; n1 2 means task ID 2.
taskr new first-worker --role implementer --parent "$ORCH" --report report.md
# Save the returned worker ID (replace 2 if needed).
WORKER=2
# Block until a result or question arrives.
taskr wait --as "$ORCH" --for ready,ask,done,fail,herdr
```

While A waits, act as the worker in terminal B:

```sh
# Use the worker ID printed in terminal A.
WORKER=2
# Write the report file in the shared working directory.
printf 'First loop completed.\n' > report.md
# Tell the parent that the report is ready.
TASKR_TASK="$WORKER" taskr ready "first report" --report report.md
```

Terminal A wakes with an `e1` event. Its second field is the event ID. Read the
report, then acknowledge that event:

```sh
# Read the worker's evidence.
cat report.md
# Use the event ID from e1 (replace 1 if needed).
EVENT=1
# Mark that event handled; matching events replay until acknowledged.
taskr ack "$EVENT" --as "$ORCH"
```

Finish the worker in B, then collect completion and close the example in A:

```sh
# In B: record completion using the worker identity.
TASKR_TASK="$WORKER" taskr done "first loop complete"
# In A: collect the completion event and inspect the campaign.
taskr wait --as "$ORCH" --for done,fail,herdr
taskr status --tree "$ORCH"
# In A: acknowledge the completion event (replace 2 with its event ID).
EVENT=2
taskr ack "$EVENT" --as "$ORCH"
# In A: close the example tasks once you have read the result.
taskr close "$WORKER"
taskr close "$ORCH"
```

For real agents, register their Herdr pane locations, record a `launch`, and
pass its returned `TASKR_TASK` and `TASKR_LAUNCH` to the worker process. The
[orchestration reference](references/orchestrator.md) gives that workflow,
prompt delivery, asks and answers, and the wait/ack loop. `launch` records
identity; it does not start an agent.

## Agent hooks (optional)

Claude Code, Codex, and OpenCode hooks can bind a launch to its agent session,
acknowledge prompts beginning with `First taskr got N.`, and signal a stalled
turn. A receipt confirms acknowledgment, not successful completion. A stall
is a signal to investigate, not proof of a failed task.

**Neither `install.sh` nor the Herdr plugin configures these agent hooks.**
Enable them manually in your harness's hook configuration; supported events,
command templates, and a Claude Code configuration example are in
[docs/install.md](docs/install.md#agent-hooks).
Workers should still run `taskr got` and report through `ready`, `done`, or `fail`.

## Teaching your agents

[SKILL.md](SKILL.md) is the short agent-facing entry point; [references/](references/)
contains the detail. The installer copies both to `$HOME/.agents/skills/taskr`.
`TASKR_LINK_SKILLS=1` also links that directory into existing Claude Code, Codex,
and OpenCode configuration directories, leaving conflicts unchanged.

For an existing installation, expose the skill to Claude Code or Codex:

```sh
# Link the installed skill into Claude Code (only if taskr is not already there).
mkdir -p "$HOME/.claude/skills"
ln -s "$HOME/.agents/skills/taskr" "$HOME/.claude/skills/taskr"
# Or link it into Codex (only if taskr is not already there).
mkdir -p "$HOME/.codex/skills"
ln -s "$HOME/.agents/skills/taskr" "$HOME/.codex/skills/taskr"
```

Tell the agent to read the taskr skill before coordinating workers. Load only
the relevant reference: [orchestration](references/orchestrator.md),
[recovery](references/recovery.md), [review](references/review.md), or
[output format](references/format.md). CLI output is compact by default;
use `--json` when a script needs JSON.

## Multiple machines (optional)

A Tailscale hub can serve the dashboard and a shared ledger. Clients point
`server.url` at it, for example `http://hub.example.ts.net:7788`; without
`TASKR_DB`, their ledger commands go to the server. There is no local fallback
when that server is unreachable. Alternatively, peers can keep separate ledgers
and push read-only dashboard snapshots through `hub.url`.

See [multi-machine installation](docs/install.md#multiple-machines) for the
configuration, access rules, and the difference between clients and peers.
Start with one machine unless you need this.

## Uninstall

Unlink the plugin, stop the taskr daemon, and remove the installed binary,
plugin, and skill links. Keep your ledger and reports until you decide to
remove them. The [uninstall steps](docs/install.md#uninstall) preserve them.

## Status and license

Early software, used daily by one person. Interfaces and install details may
change. Licensed under [MIT](LICENSE). Source: [olafurns7/herdr-taskr](https://github.com/olafurns7/herdr-taskr).
