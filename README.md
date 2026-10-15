# taskr

taskr is a task ledger and hub for agent fleets running in
[Herdr](https://herdr.dev). Orchestrator agents register their workers in it,
workers report progress and ask questions through it, and you, the owner, see
at a glance what needs you.

- **For agents:** a `taskr` command line that records briefs, reports,
  questions, answers and decisions, and a blocking `wait` that wakes the
  orchestrator when something arrives.
- **For you:** `taskr-tui`, a terminal view of every campaign, with the open
  questions first.
- **Across machines:** one host keeps the ledger (the taskr hub); the others
  send their commands to it over Tailscale.

The Rust hub daemon can check in with idle leads when started with
`TASKR_CHECKIN=1`; it is off by default. See [the daemon guide](docs/daemon.md).

Skip taskr if you run one agent at a time or do not use Herdr.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/images/glance-dark.png">
  <img src="docs/images/glance-light.png" alt="taskr-tui glance: two owner asks at the top, three things to check, then ten campaigns with their lanes and activity. The selected ask is shown in full on the right with its two options.">
</picture>

Open a campaign to see its goal, plan, lanes, questions, documents and log:

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/images/campaign-dark.png">
  <img src="docs/images/campaign-light.png" alt="taskr-tui campaign view: the goal and plan on top, the lane tree with role, model, host and age on the left, and asks, documents, pull requests and the log on the right.">
</picture>

The screenshots show invented data (`taskr-tui --demo`).

## Install

Paste this into Claude Code or Codex running inside Herdr:

```text
Install taskr for Herdr: fetch https://raw.githubusercontent.com/olafurns7/herdr-taskr/master/docs/install.md and follow it step by step. Ask me before changing any hook or agent settings.
```

Setting up a new machine from scratch, or not using Herdr yet? Follow
[AGENTS_SETUP.md](AGENTS_SETUP.md), a tested first-time setup guide.

Or install by hand on macOS or Linux. Pin the release: `taskr-tui` ships from
v0.21.0 on, and `releases/latest` skips prereleases, so it may lag behind the
newest prerelease (it has pointed at v0.18.0, which has no `taskr-tui`).

```sh
export TASKR_VERSION=v0.22.0   # or a newer tag
curl -fsSL -o taskr-install.sh \
  "https://github.com/olafurns7/herdr-taskr/releases/download/$TASKR_VERSION/install.sh" &&
  test -s taskr-install.sh &&
  TASKR_LINK_SKILLS=1 sh taskr-install.sh &&
  PATH="$HOME/.local/bin:$PATH" taskr version
rm taskr-install.sh
```

This installs `taskr`, `taskr-tui` (v0.21.0 and newer), the agent skill and
the Herdr plugin.
Set `TASKR_NO_TUI=1` to skip the terminal view. Installing an older release
without it preserves any existing `taskr-tui`. The [install guide](docs/install.md)
has every step, including more than one machine.

## Start the terminal view

`taskr-tui` is installed by the command above. To build it from a clone
instead, with [Rust](https://rustup.rs):

```sh
cargo install --path crates/taskr-tui
```

Then:

```sh
taskr-tui --demo   # invented data; reads no ledger and writes nothing
taskr-tui          # your ledger
```

Press `?` for the keys and `q` to quit. [docs/tui.md](docs/tui.md) explains
the screens.

## Documentation

| Page | What it covers |
| --- | --- |
| [docs/install.md](docs/install.md) | Installing, upgrading, hooks, several machines, uninstalling |
| [docs/tui.md](docs/tui.md) | `taskr-tui`: screens, keys, colours and marks |
| [docs/cli.md](docs/cli.md) | The `taskr` commands agents run, and how a campaign flows |
| [docs/daemon.md](docs/daemon.md) | The daemon, the taskr hub, client hosts and how they find each other |
| [docs/release.md](docs/release.md) | Building and publishing a release |
| [docs/troubleshooting.md](docs/troubleshooting.md) | Common problems and what to check |

Agents read [SKILL.md](SKILL.md) and [references/](references/) instead; the
installer puts them where agents load skills from.

## Status and license

Early software; interfaces and install details may change.
Licensed under [MIT](LICENSE).
Source: [olafurns7/herdr-taskr](https://github.com/olafurns7/herdr-taskr).
